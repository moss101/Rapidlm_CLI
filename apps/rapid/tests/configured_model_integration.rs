//! End-to-end coverage for the Grok-style configured model path.
//!
//! A scripted loopback HTTP server stands in for a local OpenAI-compatible
//! provider (Ollama/LM Studio style). The real `rapid` binary is driven
//! through `rapid exec` with `RAPIDLM_CONFIG` pointing at a temp config, and
//! the typed failure paths (no config, invalid config, unknown override) are
//! asserted against the actual process exit codes and stderr.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;

use agent_runtime::{CancellationToken, FailureCause, ModelStepError, ModelStepOutput};
use auth::InMemoryCredentialStore;
use rapid::host::{LiveModelCall, PreservedLiveContext, UnconfiguredModel, build_packet};
use rapid::model::{ConfiguredModel, SelectedModel};
use rapid::user_config::{
    self, ConfigProvider, CredentialSource, ModelSelection, parse_config_document, resolve_active,
};

/// Non-streaming OpenAI chat-completion body (no `data:` blocks, so the
/// adapter's non-stream ingestion path handles it).
const NON_STREAMING_BODY: &str = r#"{"choices":[{"message":{"role":"assistant","content":"hello from scripted model"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":5}}"#;

/// SSE chat-completion stream: two text deltas, usage, then [DONE].
const SSE_BODY: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\
                        \n\
                        data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":2}}\n\
                        \n\
                        data: [DONE]\n\n";

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

/// One scripted response per accepted connection; bodies are captured.
struct ScriptedServer {
    addr: std::net::SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
}

fn spawn_scripted_server(responses: Vec<(u16, String)>) -> ScriptedServer {
    spawn_binary_server(
        responses
            .into_iter()
            .map(|(status, body)| (status, body.into_bytes()))
            .collect(),
        "application/json",
    )
}
fn spawn_binary_server(
    responses: Vec<(u16, Vec<u8>)>,
    content_type: &'static str,
) -> ScriptedServer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    thread::spawn(move || {
        for (status, body) in responses {
            let (mut socket, _) = match listener.accept() {
                Ok(s) => s,
                Err(_) => return,
            };
            let raw = read_request(&mut socket);
            captured.lock().unwrap().push(raw);
            let reason = if status == 200 { "OK" } else { "Error" };
            let headers = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(headers.as_bytes());
            let _ = socket.write_all(&body);
            let _ = socket.flush();
            let _ = socket.shutdown(Shutdown::Both);
        }
    });
    ScriptedServer { addr, requests }
}

/// Read one HTTP/1.1 request: headers plus the Content-Length-framed body.
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

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rapidlm-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn active_from_doc(doc: &str) -> rapid::user_config::ActiveModel {
    let config = parse_config_document(doc, "integration.toml").expect("parse config");
    resolve_active(&[], &config).expect("resolve active model")
}

fn scripted_model(doc: &str) -> ConfiguredModel<'static> {
    // Leak a per-test store: integration tests construct one model per test
    // process-lifetime; the store must outlive the returned adapter.
    let store: &'static InMemoryCredentialStore =
        Box::leak(Box::new(InMemoryCredentialStore::new()));
    let active = active_from_doc(doc);
    ConfiguredModel::build(&active, store).expect("build configured model")
}

#[test]
fn configured_model_step_reaches_loopback_openai_server() {
    let server = spawn_scripted_server(vec![(200, NON_STREAMING_BODY.to_owned())]);
    let doc = config_doc(&format!("http://{}/v1", server.addr));
    let mut model = scripted_model(&doc);
    let preserved =
        PreservedLiveContext::new("ship the scripted feature", Vec::new(), "", "", 1024, 64)
            .expect("preserved");
    let packet = build_packet(&preserved, None).expect("packet");
    let output = model
        .step(
            packet.blocks(),
            &agent_runtime::ModelStepInput::without_tools(1),
            &CancellationToken::new(),
        )
        .expect("step");
    match output {
        ModelStepOutput::Terminal { text, tokens, .. } => {
            assert_eq!(text, "hello from scripted model");
            assert_eq!(tokens, 8);
        }
        other => panic!("expected terminal output, got {other:?}"),
    }
    let requests = server.requests.lock().expect("requests");
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert!(request.contains("POST /v1/chat/completions"), "{request}");
    assert!(request.contains("\"model\":\"test-model\""), "{request}");
    assert!(request.contains("ship the scripted feature"), "{request}");
    assert!(
        request.contains("Authorization: Bearer scripted-key"),
        "{request}"
    );
}

#[test]
fn configured_model_step_parses_sse_stream() {
    let server = spawn_scripted_server(vec![(200, SSE_BODY.to_owned())]);
    let doc = config_doc(&format!("http://{}/v1", server.addr));
    let mut model = scripted_model(&doc);
    let preserved = PreservedLiveContext::new("goal", Vec::new(), "", "", 1024, 64).expect("p");
    let packet = build_packet(&preserved, None).expect("packet");
    let output = model
        .step(
            packet.blocks(),
            &agent_runtime::ModelStepInput::without_tools(1),
            &CancellationToken::new(),
        )
        .expect("step");
    match output {
        ModelStepOutput::Terminal { text, tokens, .. } => {
            assert_eq!(text, "hello");
            assert_eq!(tokens, 4);
        }
        other => panic!("expected terminal output, got {other:?}"),
    }
}

#[test]
fn binary_exec_selects_the_responses_protocol_from_its_configuration() {
    let body = serde_json::json!({"status":"completed","output":[{"type":"message","role":"assistant",
        "content":[{"type":"output_text","text":"hello from Responses"}]}],"usage":{"input_tokens":3,"output_tokens":5}}).to_string();
    let server = spawn_scripted_server(vec![(200, body)]);
    let dir = temp_dir("responses-protocol");
    let path = dir.join("config.toml");
    let doc = config_doc(&format!("http://{}/v1", server.addr))
        .replace("openai-compatible", "openai-responses");
    std::fs::write(&path, doc).unwrap();
    let (code, stdout, stderr) = run_rapid(Some(&path), None, &dir);
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(stdout.trim(), "hello from Responses");
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("POST /v1/responses "));
    let body: serde_json::Value =
        serde_json::from_str(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap();
    assert!(body["input"].is_array());
    assert!(body.get("messages").is_none());
    assert_eq!(body["store"], false);
    assert!(sessions_in_exec_project(&dir).contains("count=1"));
    drop(requests);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn provider_auth_failure_is_a_typed_step_failure() {
    let server = spawn_scripted_server(vec![(
        401,
        r#"{"error":{"message":"bad key","type":"invalid_request_error"}}"#.to_owned(),
    )]);
    let doc = config_doc(&format!("http://{}/v1", server.addr));
    let mut model = scripted_model(&doc);
    let preserved = PreservedLiveContext::new("goal", Vec::new(), "", "", 1024, 64).expect("p");
    let packet = build_packet(&preserved, None).expect("packet");
    let err = model
        .step(
            packet.blocks(),
            &agent_runtime::ModelStepInput::without_tools(1),
            &CancellationToken::new(),
        )
        .expect_err("typed failure");
    // A 401 keeps its cause class instead of collapsing into a bare failure.
    assert_eq!(
        err,
        ModelStepError::ProviderFailed {
            cause: FailureCause::Auth
        }
    );
}

#[test]
fn anthropic_provider_builds_and_reaches_the_loopback_server() {
    let server = spawn_scripted_server(vec![(
        200,
        r#"{"content":[{"type":"text","text":"bonjour"}],"usage":{"input_tokens":4,"output_tokens":2},"stop_reason":"end_turn"}"#.to_owned(),
    )]);
    let doc = format!(
        "[models]\ndefault = \"gw\"\n\
         \n\
         [model.gw]\n\
         provider = \"anthropic\"\n\
         model = \"claude-3-5-sonnet\"\n\
         base_url = \"http://{}\"\n\
         env_key = \"GW_API_KEY\"\n",
        server.addr
    );
    let config = parse_config_document(&doc, "gw.toml").expect("parse");
    // The credential comes from the env slice (pure core), not the process.
    let active = resolve_active(&[("GW_API_KEY".to_owned(), "gw-key".to_owned())], &config)
        .expect("resolve");
    assert_eq!(active.entry.provider, ConfigProvider::Anthropic);
    assert_eq!(
        active.credential.source,
        CredentialSource::EnvVar("GW_API_KEY".to_owned())
    );
    let store = InMemoryCredentialStore::new();
    let mut model = ConfiguredModel::build(&active, &store).expect("build");
    let preserved = PreservedLiveContext::new("goal", Vec::new(), "", "", 1024, 64).expect("p");
    let packet = build_packet(&preserved, None).expect("packet");
    let output = model
        .step(
            packet.blocks(),
            &agent_runtime::ModelStepInput::without_tools(1),
            &CancellationToken::new(),
        )
        .expect("step");
    match output {
        ModelStepOutput::Terminal { text, tokens, .. } => {
            assert_eq!(text, "bonjour");
            assert_eq!(tokens, 6);
        }
        other => panic!("expected terminal output, got {other:?}"),
    }
    let requests = server.requests.lock().expect("requests");
    assert!(requests[0].contains("anthropic-version"), "{}", requests[0]);
}

// ---------------------------------------------------------------------------
// Real-binary end-to-end: the actual `rapid` executable with RAPIDLM_CONFIG.
// ---------------------------------------------------------------------------

/// The project `run_rapid` runs in: a directory of its own under the test's
/// HOME, marked with `.rapidlm` so its root resolves there whatever encloses
/// the temp directory.
fn exec_project(home: &Path) -> PathBuf {
    home.join("project")
}

/// `rapid exec` in [`exec_project`], never in the test process's working
/// directory: that is this crate's directory, and a run there resolves the
/// repository as its project and accrues its turn to whatever goal the
/// repository's own `.rapidlm/goal.json` holds active.
fn run_rapid(
    config_path: Option<&PathBuf>,
    model_override: Option<&str>,
    home: &PathBuf,
) -> (Option<i32>, String, String) {
    let project = exec_project(home);
    std::fs::create_dir_all(project.join(".rapidlm")).expect("project marker");
    let mut command = Command::new(env!("CARGO_BIN_EXE_rapid"));
    command
        .args(["exec", "ship the scripted feature"])
        .current_dir(&project)
        .env("HOME", home)
        .env_remove("RAPIDLM_HOME")
        .env_remove("RAPIDLM_CONFIG")
        .env_remove("RAPIDLM_MODEL");
    if let Some(path) = config_path {
        command.env("RAPIDLM_CONFIG", path);
    }
    if let Some(model) = model_override {
        command.env("RAPIDLM_MODEL", model);
    }
    let output = command.output().expect("run rapid");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// `rapid sessions list` in [`exec_project`] under the same HOME: where a
/// `run_rapid` exec's session must be recorded.
fn sessions_in_exec_project(home: &Path) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_rapid"))
        .args(["sessions", "list"])
        .current_dir(exec_project(home))
        .env("HOME", home)
        .env_remove("RAPIDLM_HOME")
        .env_remove("RAPIDLM_CONFIG")
        .env_remove("RAPIDLM_MODEL")
        .output()
        .expect("run rapid sessions list");
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn binary_exec_uses_configured_model_end_to_end() {
    let server = spawn_scripted_server(vec![(200, NON_STREAMING_BODY.to_owned())]);
    let dir = temp_dir("exec-ok");
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        config_doc(&format!("http://{}/v1", server.addr)),
    )
    .expect("write config");
    let (code, stdout, stderr) = run_rapid(Some(&config_path), None, &dir);
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(
        stdout.contains("hello from scripted model"),
        "stdout did not carry the model text: {stdout}"
    );
    let requests = server.requests.lock().expect("requests");
    assert!(!requests.is_empty(), "no request reached the provider");
    assert!(requests[0].contains("\"model\":\"test-model\""));
    // The turn is recorded in its own project, not in the repository that
    // encloses this test's working directory.
    let listed = sessions_in_exec_project(&dir);
    assert_eq!(
        listed.lines().next(),
        Some("schema=rapidlm.sessions count=1"),
        "{listed}"
    );
    // The step's split is on its ledger record, and `rapid usage` reduces
    // it back: 3 prompt + 5 completion tokens, as the provider reported
    // them, and no cost — which it did not report — shown as unknown.
    let usage = Command::new(env!("CARGO_BIN_EXE_rapid"))
        .args(["usage", "--output", "json"])
        .current_dir(exec_project(&dir))
        .env("HOME", &dir)
        .env_remove("RAPIDLM_HOME")
        .env_remove("RAPIDLM_CONFIG")
        .env_remove("RAPIDLM_MODEL")
        .output()
        .expect("run rapid usage");
    assert_eq!(
        usage.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&usage.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&usage.stdout).expect("usage json");
    let total = &report["total"];
    assert_eq!(total["steps"], 1, "{report}");
    assert_eq!(total["tokens"], 8, "{report}");
    assert_eq!(total["input_tokens"], 3, "{report}");
    assert_eq!(total["output_tokens"], 5, "{report}");
    assert_eq!(total["tokens_estimated_steps"], 0, "{report}");
    assert_eq!(
        total["cost_usd_micros"],
        serde_json::Value::Null,
        "{report}"
    );
    assert_eq!(total["cost_unknown_steps"], 1, "{report}");
    assert_eq!(
        report["turns"].as_array().map(Vec::len),
        Some(1),
        "{report}"
    );
}

#[test]
fn binary_exec_exports_a_redacted_turn_record_only_when_the_user_opts_in() {
    let dir = temp_dir("exec-otlp");
    let config_path = dir.join("config.toml");
    let telemetry_dir = dir.join(".rapidlm").join("telemetry");
    // Off by default: nothing is sent and nothing is started.
    let server = spawn_scripted_server(vec![(200, NON_STREAMING_BODY.to_owned())]);
    std::fs::write(
        &config_path,
        config_doc(&format!("http://{}/v1", server.addr)),
    )
    .expect("write config");
    let collector = spawn_scripted_server(vec![(200, "{}".to_owned())]);
    let (code, _, stderr) = run_rapid(Some(&config_path), None, &dir);
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(collector.requests.lock().expect("requests").is_empty());
    assert!(!telemetry_dir.exists(), "telemetry started while off");

    // On: the user's own config names the collector.
    let server = spawn_scripted_server(vec![(200, NON_STREAMING_BODY.to_owned())]);
    std::fs::write(
        &config_path,
        config_doc(&format!("http://{}/v1", server.addr)),
    )
    .expect("write config");
    std::fs::create_dir_all(dir.join(".rapidlm")).expect("home");
    std::fs::write(
        dir.join(".rapidlm").join("config.toml"),
        format!(
            "[telemetry.otlp]\nendpoint = \"http://{}\"\n",
            collector.addr
        ),
    )
    .expect("telemetry config");
    let (code, _, stderr) = run_rapid(Some(&config_path), None, &dir);
    assert_eq!(code, Some(0), "stderr: {stderr}");
    let requests = collector.requests.lock().expect("requests").clone();
    assert_eq!(requests.len(), 1, "one turn, one record: {requests:?}");
    let body = &requests[0];
    assert!(body.contains("rapid.turn"), "{body}");
    assert!(!body.contains("scripted-key"), "the model key left: {body}");
    assert!(
        !body.contains("ship the scripted feature"),
        "the prompt left: {body}"
    );
    let receipts = std::fs::read_to_string(telemetry_dir.join("egress-receipts.jsonl"))
        .expect("an egress receipt");
    assert!(receipts.contains("\"allowed\":true"), "{receipts}");
    assert!(
        receipts.contains(&collector.addr.port().to_string()),
        "{receipts}"
    );
}

#[test]
fn binary_exec_without_config_takes_the_typed_fallback() {
    let dir = temp_dir("exec-unconfigured");
    let (code, stdout, stderr) = run_rapid(None, None, &dir);
    // JsonlExitCode::Runtime: an unconfigured model step fails with an
    // unclassified `FailureCause` (never provider-classified, since no
    // provider was ever dispatched to).
    assert_eq!(code, Some(5), "stdout: {stdout}");
    assert!(
        stderr.contains("no model configured"),
        "hint missing: {stderr}"
    );
    assert!(
        stderr.contains("agent turn failed"),
        "typed provider failure missing: {stderr}"
    );
    let listed = sessions_in_exec_project(&dir);
    assert_eq!(
        listed.lines().next(),
        Some("schema=rapidlm.sessions count=1"),
        "{listed}"
    );
}

#[test]
fn binary_exec_rejects_invalid_config_typed() {
    let server = spawn_scripted_server(vec![(200, NON_STREAMING_BODY.to_owned())]);
    let dir = temp_dir("exec-bad-config");
    let config_path = dir.join("config.toml");
    let doc = format!(
        "[models]\ndefault = \"local\"\n\
         \n\
         [model.local]\n\
         provider = \"vllm-ish\"\n\
         model = \"test-model\"\n\
         base_url = \"http://{}\"\n",
        server.addr
    );
    std::fs::write(&config_path, doc).expect("write config");
    let (code, stdout, stderr) = run_rapid(Some(&config_path), None, &dir);
    // JsonlExitCode::Usage: an invalid config is a user-input error.
    assert_eq!(code, Some(2), "stdout: {stdout}");
    assert!(
        stderr.contains("model configuration error") && stderr.contains("provider"),
        "typed config error missing: {stderr}"
    );
    let requests = server.requests.lock().expect("requests");
    assert!(
        requests.is_empty(),
        "invalid config must not reach a provider"
    );
}

#[test]
fn binary_exec_rejects_unknown_model_override_typed() {
    let server = spawn_scripted_server(vec![(200, NON_STREAMING_BODY.to_owned())]);
    let dir = temp_dir("exec-bad-override");
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        config_doc(&format!("http://{}/v1", server.addr)),
    )
    .expect("write config");
    let (code, _stdout, stderr) = run_rapid(Some(&config_path), Some("missing-model"), &dir);
    // JsonlExitCode::Usage: an unknown model override is a user-input error.
    assert_eq!(code, Some(2));
    assert!(
        stderr.contains("model configuration error") && stderr.contains("missing-model"),
        "typed override error missing: {stderr}"
    );
    let requests = server.requests.lock().expect("requests");
    assert!(
        requests.is_empty(),
        "bad override must not reach a provider"
    );
}

#[test]
fn selection_is_unconfigured_when_no_config_exists_anywhere() {
    let dir = temp_dir("selection-none");
    let env: Vec<(String, String)> = vec![("HOME".to_owned(), dir.display().to_string())];
    let selection = user_config::select_active_model(&env).expect("selection");
    assert!(matches!(selection, ModelSelection::Unconfigured { .. }));
    // The selected fallback still fails typed through the exec seam.
    let mut selected = match selection {
        ModelSelection::Unconfigured { .. } => SelectedModel::Unconfigured(UnconfiguredModel),
        ModelSelection::Configured { .. } => panic!("unexpected configured selection"),
    };
    let err = selected
        .step(
            &[],
            &agent_runtime::ModelStepInput::without_tools(1),
            &CancellationToken::new(),
        )
        .expect_err("typed fallback");
    assert_eq!(err, ModelStepError::Failed);
}

// ---------------------------------------------------------------------------
// Per-call tool-result channel: the request after a tool step carries one
// tool-role/`tool_result` entry per executed call, each with its own id and
// text, and the flat "tool results:" report is gone. Asserted on the wire for
// both provider encoders.
// ---------------------------------------------------------------------------

/// SSE chat-completions stream proposing two tool calls.
fn openai_tool_call_body() -> String {
    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"repo_read\",\"arguments\":\"{\\\"path\\\":\\\"a.txt\\\"}\"}}]}}]}\n\
     \n\
     data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_2\",\"type\":\"function\",\"function\":{\"name\":\"workspace_patch\",\"arguments\":\"{\\\"path\\\":\\\"b.txt\\\",\\\"old\\\":\\\"x\\\",\\\"new\\\":\\\"y\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\
     \n\
     data: [DONE]\n\n"
    .to_owned()
}

fn terminal_body(text: &str) -> String {
    format!(
        r#"{{"choices":[{{"message":{{"role":"assistant","content":"{text}"}},"finish_reason":"stop"}}],"usage":{{"prompt_tokens":3,"completion_tokens":4}}}}"#
    )
}

fn tool_workspace(tag: &str) -> PathBuf {
    let dir = temp_dir(tag);
    std::fs::write(dir.join("a.txt"), "alpha\n").expect("seed a");
    std::fs::write(dir.join("b.txt"), "x\n").expect("seed b");
    dir
}

fn exec_request_for(goal: &str) -> agent_runtime::AgentExecutionRequest {
    use protocol::{AgentId, SessionId, WorkspaceViewId};
    let spec = agent_runtime::AgentSpec::builder(
        AgentId::new(),
        agent_runtime::AgentRole::Coder,
        goal.to_owned(),
        WorkspaceViewId::new(),
    )
    .permissions_profile("work")
    .build()
    .expect("spec");
    agent_runtime::AgentExecutionRequest::new(spec, SessionId::new())
}

fn bypass_tools(workspace: &Path) -> rapid::exec_tools::ExecTools {
    rapid::exec_tools::ExecTools::workspace_with_permissions(
        workspace,
        rapid::permissions::PermissionLattice::new(
            rapid::permissions::PermissionMode::BypassPermissions,
        ),
    )
    .expect("tools")
}

#[test]
fn openai_tool_results_reach_the_provider_one_tool_message_per_call() {
    let server = spawn_scripted_server(vec![
        (200, openai_tool_call_body()),
        (200, terminal_body("patched and read")),
    ]);
    let doc = config_doc(&format!("http://{}/v1", server.addr));
    let store: &'static InMemoryCredentialStore =
        Box::leak(Box::new(InMemoryCredentialStore::new()));
    let model = ConfiguredModel::build(&active_from_doc(&doc), store).expect("build");
    let workspace = tool_workspace("tool-channel-openai");
    let mut tools = bypass_tools(&workspace);
    let preserved =
        PreservedLiveContext::new("patch the file", Vec::new(), "", "", 8192, 256).expect("p");
    let mut events = Vec::new();
    let outcome = rapid::host::run_live_exec(
        preserved,
        model,
        &exec_request_for("patch the file"),
        &mut tools,
        &mut events,
        &CancellationToken::new(),
        agent_runtime::ContextRetryPolicy::new(2),
        None,
    )
    .expect("execute");
    assert_eq!(outcome.result.summary(), "patched and read");

    let requests = server.requests.lock().expect("requests");
    assert_eq!(requests.len(), 2, "one request per model step");
    let second = &requests[1];
    // Per-call tool messages with ids and text content, in call order.
    let call_one = second.matches("\"tool_call_id\":\"call_1\"").count();
    let call_two = second.matches("\"tool_call_id\":\"call_2\"").count();
    assert_eq!(call_one, 1, "exactly one tool message per call: {second}");
    assert_eq!(call_two, 1, "exactly one tool message per call: {second}");
    assert!(
        second.contains("alpha\\n"),
        "the first call's text result is carried: {second}"
    );
    // The assistant message echoes the proposals.
    assert!(second.contains("\"tool_calls\""), "{second}");
    assert!(second.contains("workspace_patch"), "{second}");
    // The flat report is gone everywhere.
    assert!(!second.contains("tool results:"), "{second}");
    let _ = std::fs::remove_dir_all(&workspace);
}

#[test]
fn anthropic_tool_results_reach_the_provider_one_tool_result_per_call() {
    let first = r#"{"content":[{"type":"tool_use","id":"call_1","name":"repo_read","input":{"path":"a.txt"}}],"stop_reason":"tool_use","usage":{"input_tokens":4,"output_tokens":2}}"#.to_owned();
    let second = r#"{"content":[{"type":"text","text":"read done"}],"usage":{"input_tokens":9,"output_tokens":2},"stop_reason":"end_turn"}"#.to_owned();
    let server = spawn_scripted_server(vec![(200, first), (200, second)]);
    let doc = format!(
        "[models]\ndefault = \"gw\"\n\
         \n\
         [model.gw]\n\
         provider = \"anthropic\"\n\
         model = \"claude-3-5-sonnet\"\n\
         base_url = \"http://{}\"\n\
         env_key = \"GW_API_KEY\"\n",
        server.addr
    );
    let config = parse_config_document(&doc, "gw.toml").expect("parse");
    let active = resolve_active(&[("GW_API_KEY".to_owned(), "gw-key".to_owned())], &config)
        .expect("resolve");
    let store = InMemoryCredentialStore::new();
    let model = ConfiguredModel::build(&active, &store).expect("build");
    let workspace = tool_workspace("tool-channel-anthropic");
    let mut tools = bypass_tools(&workspace);
    let preserved = PreservedLiveContext::new("goal", Vec::new(), "", "", 8192, 256).expect("p");
    let mut events = Vec::new();
    let outcome = rapid::host::run_live_exec(
        preserved,
        model,
        &exec_request_for("goal"),
        &mut tools,
        &mut events,
        &CancellationToken::new(),
        agent_runtime::ContextRetryPolicy::new(2),
        None,
    )
    .expect("execute");
    assert_eq!(outcome.result.summary(), "read done");

    let requests = server.requests.lock().expect("requests");
    assert_eq!(requests.len(), 2);
    let second_request = &requests[1];
    assert!(
        second_request.contains("\"tool_use_id\":\"call_1\""),
        "per-call tool_result block missing: {second_request}"
    );
    assert!(
        second_request.contains("\"type\":\"tool_result\""),
        "{second_request}"
    );
    assert!(
        second_request.contains("\"tool_use\""),
        "assistant echo missing: {second_request}"
    );
    assert!(
        !second_request.contains("tool results:"),
        "{second_request}"
    );
    let _ = std::fs::remove_dir_all(&workspace);
}

// ---------------------------------------------------------------------------
// Real-binary end-to-end: the actual `rapid` binary drives the new coding
// tools through a scripted loopback provider, with the six-mode lattice
// active (acceptEdits auto-approves the file edit; default mode denies it).
// ---------------------------------------------------------------------------

fn read_notes_tool_call_body() -> String {
    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"read_1\",\"type\":\"function\",\"function\":{\"name\":\"repo_read\",\"arguments\":\"{\\\"path\\\":\\\"notes.txt\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n".to_owned()
}

fn patch_tool_call_body() -> String {
    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"workspace_patch\",\"arguments\":\"{\\\"path\\\":\\\"notes.txt\\\",\\\"old\\\":\\\"alpha\\\",\\\"new\\\":\\\"beta\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\
     \n\
     data: [DONE]\n\n"
    .to_owned()
}

/// One trusted project plus a HOME that grants it trust.
struct TrustedProject {
    home: PathBuf,
    project: PathBuf,
}

impl TrustedProject {
    fn new(tag: &str) -> Self {
        let home = temp_dir(&format!("{tag}-home"));
        let project = temp_dir(&format!("{tag}-proj"));
        std::fs::create_dir_all(project.join(".rapidlm")).expect("project marker");
        std::fs::write(project.join("notes.txt"), "alpha\n").expect("seed");
        let root = protocol::host_path::canonicalize(&project).expect("canon");
        let identity = kernel::ProjectIdentity::new(root, None).expect("identity");
        let store = kernel::ProjectTrustStore::open(home.join(".rapidlm/project-trust.json"));
        store
            .set(
                &identity,
                kernel::TrustStatus::Trusted,
                &kernel::CancellationToken::new(),
            )
            .expect("grant trust");
        Self { home, project }
    }
}

impl Drop for TrustedProject {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
        let _ = std::fs::remove_dir_all(&self.project);
    }
}

#[test]
fn binary_paged_repo_read_honors_the_file_specific_deny_rule() {
    let server = spawn_scripted_server(vec![
        (
            200,
            tool_call_body(
                "repo_read",
                &serde_json::json!({"path":"notes.txt","offset":-1,"limit":1}),
            ),
        ),
        (200, terminal_body("denied")),
    ]);
    let env = TrustedProject::new("bin-paged-read-deny");
    std::fs::write(env.project.join("notes.txt"), "SENSITIVE-WORKSPACE-PAGE\n").unwrap();
    let config_path = env.home.join("config.toml");
    std::fs::write(
        &config_path,
        config_doc(&format!("http://{}/v1", server.addr)),
    )
    .unwrap();
    let denied = Command::new(env!("CARGO_BIN_EXE_rapid"))
        .args(["permissions", "deny", "repo_read(notes.txt)"])
        .current_dir(&env.project)
        .env("HOME", &env.home)
        .env_remove("RAPIDLM_HOME")
        .output()
        .expect("set deny rule");
    assert_eq!(
        denied.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&denied.stderr)
    );
    let (code, _, stderr) = run_rapid_in(
        &env.project,
        &env.home,
        &config_path,
        Some("bypassPermissions"),
    );
    assert_eq!(code, Some(0), "{stderr}");
    let requests = server.requests.lock().unwrap();
    let follow_up = requests.last().unwrap();
    assert!(follow_up.contains("denied"), "{follow_up}");
    assert!(
        !follow_up.contains("SENSITIVE-WORKSPACE-PAGE"),
        "the denied file reached the provider"
    );
}

#[test]
fn binary_a_persisted_never_allow_blocks_the_next_run_without_asking() {
    let server = spawn_scripted_server(vec![
        (200, read_notes_tool_call_body()),
        (200, patch_tool_call_body()),
        (200, terminal_body("could not patch")),
    ]);
    let env = TrustedProject::new("bin-never");
    let config_path = env.home.join("config.toml");
    std::fs::write(
        &config_path,
        config_doc(&format!("http://{}/v1", server.addr)),
    )
    .expect("write config");
    let permissions = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_rapid"))
            .arg("permissions")
            .args(args)
            .current_dir(&env.project)
            .env("HOME", &env.home)
            .env_remove("RAPIDLM_HOME")
            .output()
            .expect("run rapid permissions")
    };
    let denied = permissions(&["deny", "workspace_patch(notes.txt)"]);
    assert_eq!(
        denied.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&denied.stdout)
    );
    // The most permissive mode, and still refused — no prompt, no patch.
    let (code, stdout, stderr) = run_rapid_in(
        &env.project,
        &env.home,
        &config_path,
        Some("bypassPermissions"),
    );
    assert_eq!(code, Some(0), "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(
        std::fs::read_to_string(env.project.join("notes.txt")).expect("notes"),
        "alpha\n",
        "{stderr}"
    );
    let listed = String::from_utf8_lossy(&permissions(&["list"]).stdout).into_owned();
    assert!(
        listed.contains("rule=deny workspace_patch(notes.txt) origin=persisted"),
        "{listed}"
    );
}

/// `rapid exec` of the scripted patch in `env`'s project with extra
/// arguments and environment; the permission mode comes only from what is
/// given.
fn exec_patch(
    env: &TrustedProject,
    config_path: &Path,
    args: &[&str],
    vars: &[(&str, &std::ffi::OsStr)],
) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rapid"));
    command
        .arg("exec")
        .args(args)
        .arg("patch notes.txt by replacing alpha with beta")
        .current_dir(&env.project)
        .env("HOME", &env.home)
        .env_remove("RAPIDLM_HOME")
        .env_remove("RAPIDLM_MODEL")
        .env_remove("RAPIDLM_PERMISSION_MODE")
        .env_remove("RAPIDLM_MANAGED_CONFIG")
        .env("RAPIDLM_CONFIG", config_path);
    for (key, value) in vars {
        command.env(key, value);
    }
    command.output().expect("run rapid")
}

fn notes(env: &TrustedProject) -> String {
    std::fs::read_to_string(env.project.join("notes.txt")).expect("notes")
}

#[test]
fn binary_exec_allow_grants_the_run_only_within_the_managed_ceiling() {
    let mut script = Vec::new();
    for _ in 0..4 {
        script.push((200, read_notes_tool_call_body()));
        script.push((200, patch_tool_call_body()));
        script.push((200, terminal_body("done")));
    }
    let server = spawn_scripted_server(script);
    let env = TrustedProject::new("bin-allow");
    let config_path = env.home.join("config.toml");
    std::fs::write(
        &config_path,
        config_doc(&format!("http://{}/v1", server.addr)),
    )
    .expect("write config");
    // A bad rule is a usage error, before anything runs.
    let bad = exec_patch(&env, &config_path, &["--allow", "not a rule"], &[]);
    assert_eq!(bad.status.code(), Some(2));
    // Default mode asks, and nothing here can answer: refused.
    let asked = exec_patch(&env, &config_path, &[], &[]);
    assert_eq!(
        asked.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&asked.stderr)
    );
    assert_eq!(notes(&env), "alpha\n");
    // `--allow` grants it for this run.
    let allowed = exec_patch(
        &env,
        &config_path,
        &["--allow", "workspace_patch(notes.txt)"],
        &[],
    );
    assert_eq!(
        allowed.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&allowed.stderr)
    );
    assert_eq!(notes(&env), "beta\n");
    std::fs::write(env.project.join("notes.txt"), "alpha\n").expect("reset");
    // …but never past managed policy: a banned tool, or a plan-mode ceiling.
    for (tag, policy) in [
        ("ban", "denied_tools = [\"workspace_patch\"]\n"),
        ("ceiling", "max_permission_mode = \"plan\"\n"),
    ] {
        let path = env.home.join(format!("managed-{tag}.toml"));
        std::fs::write(
            &path,
            format!("schema = \"rapidlm.managed_config.v1\"\n[policy]\n{policy}"),
        )
        .expect("policy");
        let out = exec_patch(
            &env,
            &config_path,
            &["--allow=workspace_patch(notes.txt)"],
            &[("RAPIDLM_MANAGED_CONFIG", path.as_os_str())],
        );
        assert_eq!(
            out.status.code(),
            Some(0),
            "{tag}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            notes(&env),
            "alpha\n",
            "{tag}: --allow widened past managed policy"
        );
    }
}

#[test]
fn binary_the_users_default_mode_starts_the_run_and_the_environment_overrides_it() {
    let mut script = Vec::new();
    for _ in 0..2 {
        script.push((200, read_notes_tool_call_body()));
        script.push((200, patch_tool_call_body()));
        script.push((200, terminal_body("done")));
    }
    let server = spawn_scripted_server(script);
    let env = TrustedProject::new("bin-user-mode");
    let config_path = env.home.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "{}\n[permissions]\ndefault_mode = \"acceptEdits\"\n",
            config_doc(&format!("http://{}/v1", server.addr))
        ),
    )
    .expect("write config");
    // The project ships plan mode; the user's own default ranks above it.
    std::fs::write(
        env.project.join(".rapidlm/settings.json"),
        r#"{"permissions":{"defaultMode":"plan"}}"#,
    )
    .expect("settings");
    let out = exec_patch(&env, &config_path, &[], &[]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        notes(&env),
        "beta\n",
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::write(env.project.join("notes.txt"), "alpha\n").expect("reset");
    // The environment overrides the user's default.
    let out = exec_patch(
        &env,
        &config_path,
        &[],
        &[("RAPIDLM_PERMISSION_MODE", std::ffi::OsStr::new("default"))],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(notes(&env), "alpha\n");
}

fn run_rapid_in(
    project: &PathBuf,
    home: &PathBuf,
    config_path: &PathBuf,
    permission_mode: Option<&str>,
) -> (Option<i32>, String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rapid"));
    command
        .args(["exec", "patch notes.txt by replacing alpha with beta"])
        .current_dir(project)
        .env("HOME", home)
        .env_remove("RAPIDLM_HOME")
        .env_remove("RAPIDLM_MODEL")
        .env("RAPIDLM_CONFIG", config_path);
    if let Some(mode) = permission_mode {
        command.env("RAPIDLM_PERMISSION_MODE", mode);
    } else {
        command.env_remove("RAPIDLM_PERMISSION_MODE");
    }
    let output = command.output().expect("run rapid");
    // Durable proof: persist the real binary's per-run output when the
    // evidence directory is provided (verification harness sets it).
    if let Ok(dir) = std::env::var("RAPIDLM_EVIDENCE_DIR") {
        let mode_tag = permission_mode.unwrap_or("default");
        let base = std::path::Path::new(&dir).join(format!("launch-{mode_tag}"));
        let _ = std::fs::write(
            format!("{}-{}.out.log", base.to_string_lossy(), std::process::id()),
            &output.stdout,
        );
        let _ = std::fs::write(
            format!("{}-{}.err.log", base.to_string_lossy(), std::process::id()),
            &output.stderr,
        );
    }
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn binary_du_measures_the_real_layout_and_its_plan_is_reclaims_dry_run() {
    let env = TrustedProject::new("bin-du");
    std::fs::write(env.project.join(".gitignore"), ".rapidlm/\n").expect("ignore");
    git(&env.project, &["init", "-q"]);
    git(
        &env.project,
        &["-c", "user.email=t@e", "-c", "user.name=t", "add", "."],
    );
    git(
        &env.project,
        &[
            "-c",
            "user.email=t@e",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "seed",
        ],
    );
    let rapid = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_rapid"))
            .args(args)
            .current_dir(&env.project)
            .env("HOME", &env.home)
            .env_remove("RAPIDLM_HOME")
            .env_remove("RAPIDLM_MODEL")
            .env_remove("RAPIDLM_CONFIG")
            .output()
            .expect("run rapid")
    };
    // A worktree no one holds any more: its goal made it, then was cancelled.
    let created = rapid(&["goal", "create", "tidy up", "--worktree=tidy"]);
    assert_eq!(
        created.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    assert_eq!(rapid(&["goal", "cancel"]).status.code(), Some(0));
    std::fs::write(env.project.join(".rapidlm/probe.bin"), vec![b'x'; 4321]).expect("probe");

    let du = rapid(&["du", "--output", "json"]);
    assert_eq!(
        du.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&du.stderr)
    );
    let doc: serde_json::Value = serde_json::from_slice(&du.stdout).expect("json");
    let project = doc["areas"]
        .as_array()
        .expect("areas")
        .iter()
        .find(|area| area["area"] == "project")
        .expect("project area");
    assert_eq!(
        project["path"].as_str().map(PathBuf::from),
        Some(
            env.project
                .canonicalize()
                .expect("project")
                .join(".rapidlm")
        )
    );
    let probe = project["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .find(|entry| entry["name"] == "probe.bin")
        .expect("probe listed");
    assert_eq!(probe["bytes"], 4321);
    let total: u64 = std::fs::read_dir(env.project.join(".rapidlm"))
        .expect("tree")
        .flatten()
        .map(|item| dir_bytes(&item.path()))
        .sum();
    assert_eq!(project["bytes"], total);
    assert!(
        doc["areas"]
            .as_array()
            .unwrap()
            .iter()
            .any(|area| area["area"] == "worktrees")
    );

    // The store's areas never overlap: together they are .git/rapidlm.
    let area_bytes = |name: &str| -> u64 {
        doc["areas"]
            .as_array()
            .unwrap()
            .iter()
            .find(|area| area["area"] == name)
            .and_then(|area| area["bytes"].as_u64())
            .unwrap_or_else(|| panic!("no {name} area"))
    };
    assert_eq!(
        area_bytes("worktrees") + area_bytes("worktree-store"),
        dir_bytes(&env.project.join(".git/rapidlm"))
    );
    // Bad arguments are usage errors.
    assert_eq!(rapid(&["du", "--output", "yaml"]).status.code(), Some(2));
    assert_eq!(rapid(&["du", "--bogus"]).status.code(), Some(2));

    // The plan is exactly reclaim's dry run, and it writes nothing.
    let snapshot = |dir: &Path| -> Vec<(PathBuf, u64)> {
        let mut files = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for item in std::fs::read_dir(&dir).expect("dir").flatten() {
                let meta = std::fs::symlink_metadata(item.path()).expect("meta");
                if meta.is_dir() {
                    stack.push(item.path());
                } else {
                    files.push((item.path(), meta.len()));
                }
            }
        }
        files.sort();
        files
    };
    let before = (
        snapshot(&env.project.join(".rapidlm")),
        snapshot(&env.project.join(".git/rapidlm")),
    );
    let plan = rapid(&["du", "--reclaim-plan"]);
    assert_eq!(
        (
            snapshot(&env.project.join(".rapidlm")),
            snapshot(&env.project.join(".git/rapidlm")),
        ),
        before,
        "the plan wrote to disk"
    );
    let dry = rapid(&["worktree", "reclaim", "--dry-run"]);
    let plan_lines: Vec<String> = String::from_utf8_lossy(&plan.stdout)
        .lines()
        .filter(|line| line.starts_with("would reclaim"))
        .map(str::to_owned)
        .collect();
    let dry_lines: Vec<String> = String::from_utf8_lossy(&dry.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(plan_lines.len(), 1, "{plan_lines:?}");
    assert_eq!(plan_lines, dry_lines);
    let listed = String::from_utf8_lossy(&rapid(&["worktree", "list"]).stdout).into_owned();
    assert!(listed.contains("tidy"), "{listed}");
}

fn dir_bytes(path: &Path) -> u64 {
    let meta = std::fs::symlink_metadata(path).expect("meta");
    if !meta.is_dir() {
        return meta.len();
    }
    std::fs::read_dir(path)
        .expect("dir")
        .flatten()
        .map(|item| dir_bytes(&item.path()))
        .sum()
}

#[test]
fn binary_worktree_reclaim_keeps_unpublished_work_until_it_is_abandoned() {
    let server = spawn_scripted_server(vec![
        (200, read_notes_tool_call_body()),
        (200, patch_tool_call_body()),
        (200, terminal_body("patched notes.txt")),
    ]);
    let env = TrustedProject::new("bin-worktree-reclaim");
    std::fs::write(env.project.join(".gitignore"), ".rapidlm/\n").expect("ignore");
    git(&env.project, &["init", "-q"]);
    git(
        &env.project,
        &["-c", "user.email=t@e", "-c", "user.name=t", "add", "."],
    );
    git(
        &env.project,
        &[
            "-c",
            "user.email=t@e",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "seed",
        ],
    );
    let config_path = env.home.join("config.toml");
    std::fs::write(
        &config_path,
        config_doc(&format!("http://{}/v1", server.addr)),
    )
    .expect("write config");
    let rapid = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_rapid"))
            .args(args)
            .current_dir(&env.project)
            .env("HOME", &env.home)
            .env_remove("RAPIDLM_HOME")
            .env_remove("RAPIDLM_MODEL")
            .env("RAPIDLM_CONFIG", &config_path)
            .env("RAPIDLM_PERMISSION_MODE", "acceptEdits")
            .output()
            .expect("run rapid")
    };
    let run = rapid(&["exec", "--worktree=wip", "patch notes.txt"]);
    let stderr = String::from_utf8_lossy(&run.stderr).into_owned();
    assert_eq!(run.status.code(), Some(0), "stderr: {stderr}");
    let worktree = stderr
        .lines()
        .find_map(|line| line.strip_prefix("worktree: "))
        .map(PathBuf::from)
        .expect("worktree named");
    // The run is over (its lease with it), but its patch is unpublished.
    let list = rapid(&["worktree", "list"]);
    let listed = String::from_utf8_lossy(&list.stdout).into_owned();
    assert!(
        listed.contains("kept: uncommitted or ignored files"),
        "{listed}"
    );
    let reclaim = rapid(&["worktree", "reclaim"]);
    assert_eq!(reclaim.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&reclaim.stdout).contains("nothing to reclaim"));
    assert!(worktree.join("notes.txt").exists());

    let abandon = rapid(&["worktree", "abandon", "wip"]);
    assert_eq!(
        abandon.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&abandon.stderr)
    );
    let dry = rapid(&["worktree", "reclaim", "--dry-run"]);
    let dry_out = String::from_utf8_lossy(&dry.stdout).into_owned();
    assert!(
        dry_out.contains("would reclaim") && dry_out.contains("abandoned"),
        "{dry_out}"
    );
    assert!(worktree.is_dir(), "a dry run removes nothing");

    let reclaim = rapid(&["worktree", "reclaim"]);
    let out = String::from_utf8_lossy(&reclaim.stdout).into_owned();
    assert_eq!(reclaim.status.code(), Some(0), "{out}");
    assert!(out.contains("reclaimed"), "{out}");
    assert!(!worktree.exists());
    assert!(String::from_utf8_lossy(&rapid(&["worktree", "list"]).stdout).contains("no worktrees"));
    // The project itself is untouched.
    assert_eq!(
        std::fs::read_to_string(env.project.join("notes.txt")).expect("notes"),
        "alpha\n"
    );
}

#[test]
fn binary_exec_worktree_runs_in_a_linked_worktree_and_leaves_the_project_untouched() {
    let server = spawn_scripted_server(vec![
        (200, read_notes_tool_call_body()),
        (200, patch_tool_call_body()),
        (200, terminal_body("patched notes.txt")),
    ]);
    let env = TrustedProject::new("bin-worktree");
    // A git project whose own state (the ledger) is ignored, as a real one's is.
    std::fs::write(env.project.join(".gitignore"), ".rapidlm/\n").expect("ignore");
    git(&env.project, &["init", "-q"]);
    git(
        &env.project,
        &["-c", "user.email=t@e", "-c", "user.name=t", "add", "."],
    );
    git(
        &env.project,
        &[
            "-c",
            "user.email=t@e",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "seed",
        ],
    );
    let status_before = git(&env.project, &["status", "--porcelain"]);
    let config_path = env.home.join("config.toml");
    std::fs::write(
        &config_path,
        config_doc(&format!("http://{}/v1", server.addr)),
    )
    .expect("write config");
    // No network: every proxy variable points at a closed port (the scripted
    // model is loopback and exempt), so making the worktree must need none.
    let output = Command::new(env!("CARGO_BIN_EXE_rapid"))
        .args([
            "exec",
            "--worktree=fix-notes",
            "patch notes.txt by replacing alpha with beta",
        ])
        .current_dir(&env.project)
        .env("HOME", &env.home)
        .env_remove("RAPIDLM_HOME")
        .env_remove("RAPIDLM_MODEL")
        .env("RAPIDLM_CONFIG", &config_path)
        .env("RAPIDLM_PERMISSION_MODE", "acceptEdits")
        .env("HTTP_PROXY", "http://127.0.0.1:1")
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .env("ALL_PROXY", "http://127.0.0.1:1")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .output()
        .expect("run rapid");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    // The project itself: untouched, file and git status alike.
    assert_eq!(
        std::fs::read_to_string(env.project.join("notes.txt")).expect("notes"),
        "alpha\n"
    );
    assert_eq!(git(&env.project, &["status", "--porcelain"]), status_before);
    // The worktree the run worked in: the patch landed there.
    let worktree = stderr
        .lines()
        .find_map(|line| line.strip_prefix("worktree: "))
        .map(PathBuf::from)
        .expect("the worktree is named on stderr");
    assert_eq!(
        std::fs::read_to_string(worktree.join("notes.txt")).expect("worktree notes"),
        "beta\n"
    );
    // Its metadata names the session and the name it was given.
    let views = env.project.join(".git").join("rapidlm").join("views");
    let metadata: Vec<String> = std::fs::read_dir(&views)
        .expect("views")
        .flatten()
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .collect();
    assert_eq!(metadata.len(), 1, "{metadata:?}");
    assert!(
        metadata[0].contains("\"name\":\"fix-notes\""),
        "{}",
        metadata[0]
    );
    assert!(metadata[0].contains("\"session_id\":\""), "{}", metadata[0]);
}

#[test]
fn binary_parent_and_worktree_child_cannot_borrow_each_others_read_authority() {
    for scenario in ["parent-read", "child-read"] {
        let script = if scenario == "parent-read" {
            vec![
                read_notes_tool_call_body(),
                spawn_call_body("helper"),
                patch_tool_call_body(),
                terminal_body("child done"),
                terminal_body("parent done"),
            ]
        } else {
            vec![
                spawn_call_body("helper"),
                read_notes_tool_call_body(),
                terminal_body("child done"),
                patch_tool_call_body(),
                terminal_body("parent done"),
            ]
        };
        let server = spawn_scripted_server(script.into_iter().map(|body| (200, body)).collect());
        let env = TrustedProject::new("bin-child-read-isolation");
        std::fs::write(env.project.join(".gitignore"), ".rapidlm/\n").unwrap();
        let defs = env.project.join(".rapidlm/agents");
        std::fs::create_dir_all(&defs).unwrap();
        std::fs::write(defs.join("helper.toml"), "schema = \"rapidlm.agent_defs.v1\"\n[agent]\nid = \"helper\"\ndescription = \"edits\"\nbase_role = \"coder\"\ntools = [\"read\", \"write\"]\n").unwrap();
        std::fs::write(
            env.project.join(".rapidlm/settings.json"),
            r#"{"permissions":{"allow":["task_spawn"]}}"#,
        )
        .unwrap();
        git(&env.project, &["init", "-q"]);
        git(
            &env.project,
            &["-c", "user.email=t@e", "-c", "user.name=t", "add", "."],
        );
        git(
            &env.project,
            &[
                "-c",
                "user.email=t@e",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "seed",
            ],
        );
        let config = env.home.join("config.toml");
        std::fs::write(&config, config_doc(&format!("http://{}/v1", server.addr))).unwrap();
        let (code, stdout, stderr) =
            run_rapid_in(&env.project, &env.home, &config, Some("acceptEdits"));
        assert_eq!(code, Some(0), "{scenario}: {stdout}\n{stderr}");
        assert_eq!(
            std::fs::read_to_string(env.project.join("notes.txt")).unwrap(),
            "alpha\n",
            "{scenario}"
        );
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 5, "{scenario}: {stdout}\n{stderr}");
        let refusal = if scenario == "parent-read" {
            &requests[3]
        } else {
            &requests[4]
        };
        assert!(
            refusal.contains("read this file in the current session"),
            "{scenario}: {refusal}"
        );
    }
}

#[test]
fn binary_exec_worktree_subagents_work_in_the_worktree_not_the_project() {
    // The parent spawns a write-capable child, which patches notes.txt; a
    // headless run integrates the child's patch into the parent's tree —
    // under `--worktree`, the worktree. The project is never touched.
    let server = spawn_scripted_server(vec![
        (200, spawn_call_body("helper")),
        (200, read_notes_tool_call_body()),
        (200, patch_tool_call_body()),
        (200, terminal_body("child patched it")),
        (200, terminal_body("all done")),
    ]);
    let env = TrustedProject::new("bin-worktree-child");
    // The child's type is the project's own definition, in its `.rapidlm`
    // (ignored by git, so absent from the worktree): it must still resolve.
    let defs = env.project.join(".rapidlm/agents");
    std::fs::create_dir_all(&defs).expect("defs dir");
    std::fs::write(
        defs.join("helper.toml"),
        "schema = \"rapidlm.agent_defs.v1\"\n[agent]\nid = \"helper\"\n\
         description = \"patches notes\"\nbase_role = \"coder\"\ntools = [\"read\", \"write\"]\n",
    )
    .expect("definition");
    std::fs::write(env.project.join(".gitignore"), ".rapidlm/\n").expect("ignore");
    std::fs::write(
        env.project.join(".rapidlm/settings.json"),
        r#"{"permissions": {"allow": ["task_spawn"]}}"#,
    )
    .expect("settings");
    git(&env.project, &["init", "-q"]);
    git(
        &env.project,
        &["-c", "user.email=t@e", "-c", "user.name=t", "add", "."],
    );
    git(
        &env.project,
        &[
            "-c",
            "user.email=t@e",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "seed",
        ],
    );
    let status_before = git(&env.project, &["status", "--porcelain"]);
    let config_path = env.home.join("config.toml");
    std::fs::write(
        &config_path,
        config_doc(&format!("http://{}/v1", server.addr)),
    )
    .expect("write config");
    let output = Command::new(env!("CARGO_BIN_EXE_rapid"))
        .args(["exec", "--worktree", "have a helper patch notes.txt"])
        .current_dir(&env.project)
        .env("HOME", &env.home)
        .env_remove("RAPIDLM_HOME")
        .env_remove("RAPIDLM_MODEL")
        .env("RAPIDLM_CONFIG", &config_path)
        .env("RAPIDLM_PERMISSION_MODE", "acceptEdits")
        .output()
        .expect("run rapid");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    // A refused spawn would leave the parent to run the scripted patch
    // itself and finish one reply early: the last reply proves a child ran.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("all done"),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(env.project.join("notes.txt")).expect("notes"),
        "alpha\n",
        "a child's patch reached the project: {stderr}"
    );
    assert_eq!(git(&env.project, &["status", "--porcelain"]), status_before);
    let worktree = stderr
        .lines()
        .find_map(|line| line.strip_prefix("worktree: "))
        .map(PathBuf::from)
        .expect("worktree named");
    assert_eq!(
        std::fs::read_to_string(worktree.join("notes.txt")).expect("worktree notes"),
        "beta\n",
        "{stderr}"
    );
}

#[test]
fn binary_goal_worktree_is_where_the_goals_exec_turns_work() {
    let server = spawn_scripted_server(vec![
        (200, read_notes_tool_call_body()),
        (200, patch_tool_call_body()),
        (200, terminal_body("patched notes.txt")),
    ]);
    let env = TrustedProject::new("bin-goal-worktree");
    std::fs::write(env.project.join(".gitignore"), ".rapidlm/\n").expect("ignore");
    git(&env.project, &["init", "-q"]);
    git(
        &env.project,
        &["-c", "user.email=t@e", "-c", "user.name=t", "add", "."],
    );
    git(
        &env.project,
        &[
            "-c",
            "user.email=t@e",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "seed",
        ],
    );
    let rapid = |args: &[&str], config: Option<&PathBuf>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rapid"));
        command
            .args(args)
            .current_dir(&env.project)
            .env("HOME", &env.home)
            .env_remove("RAPIDLM_HOME")
            .env_remove("RAPIDLM_MODEL")
            .env("RAPIDLM_PERMISSION_MODE", "acceptEdits");
        match config {
            Some(config) => command.env("RAPIDLM_CONFIG", config),
            None => command.env_remove("RAPIDLM_CONFIG"),
        };
        command.output().expect("run rapid")
    };
    let created = rapid(
        &["goal", "create", "patch the notes", "--worktree=g1"],
        None,
    );
    let stdout = String::from_utf8_lossy(&created.stdout);
    assert_eq!(
        created.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let worktree = stdout
        .lines()
        .find_map(|line| line.strip_prefix("worktree: "))
        .map(PathBuf::from)
        .expect("the goal's worktree is printed");
    let config_path = env.home.join("config.toml");
    std::fs::write(
        &config_path,
        config_doc(&format!("http://{}/v1", server.addr)),
    )
    .expect("write config");
    // No flag: the active goal's worktree is where the turn works.
    let ran = rapid(
        &["exec", "patch notes.txt by replacing alpha with beta"],
        Some(&config_path),
    );
    let stderr = String::from_utf8_lossy(&ran.stderr);
    assert_eq!(ran.status.code(), Some(0), "stderr: {stderr}");
    assert!(stderr.contains("(the goal's)"), "{stderr}");
    assert_eq!(
        std::fs::read_to_string(worktree.join("notes.txt")).expect("worktree notes"),
        "beta\n"
    );
    assert_eq!(
        std::fs::read_to_string(env.project.join("notes.txt")).expect("notes"),
        "alpha\n"
    );
}

#[test]
fn binary_exec_worktree_needs_a_trusted_project() {
    let dir = temp_dir("exec-worktree-untrusted");
    let (code, _, _) = {
        let project = exec_project(&dir);
        std::fs::create_dir_all(project.join(".rapidlm")).expect("project");
        // A real git project, so only its trust can refuse the worktree.
        std::fs::write(project.join(".gitignore"), ".rapidlm/\n").expect("ignore");
        git(&project, &["init", "-q"]);
        git(
            &project,
            &["-c", "user.email=t@e", "-c", "user.name=t", "add", "."],
        );
        git(
            &project,
            &[
                "-c",
                "user.email=t@e",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "seed",
            ],
        );
        let output = Command::new(env!("CARGO_BIN_EXE_rapid"))
            .args(["exec", "--worktree", "anything"])
            .current_dir(&project)
            .env("HOME", &dir)
            .env_remove("RAPIDLM_HOME")
            .env_remove("RAPIDLM_CONFIG")
            .env_remove("RAPIDLM_MODEL")
            .output()
            .expect("run");
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };
    assert_eq!(code, Some(2));
    assert!(
        !exec_project(&dir).join(".git").join("rapidlm").exists(),
        "a worktree was made for an untrusted project"
    );
}

#[test]
#[cfg(unix)]
fn binary_recovers_an_interrupted_edit_before_the_model_reads() {
    use std::os::unix::fs::PermissionsExt;
    use workspace::multi_edit_journal::JournalStore;
    use workspace::multi_edit_recovery::{FileImage, RecoveryEntry, RecoveryRecord, RecoveryState};
    let write = || {
        tool_call_body(
            "workspace_write",
            &serde_json::json!({"path":"notes.txt","content":"gamma\n"}),
        )
    };
    let server = spawn_scripted_server(vec![
        (200, read_notes_tool_call_body()),
        (200, write()),
        (200, terminal_body("done")),
    ]);
    let env = TrustedProject::new("bin-auto-recover");
    std::fs::write(env.project.join("notes.txt"), "beta\n").unwrap();
    std::fs::set_permissions(
        env.project.join("notes.txt"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let directory = env.project.join(".rapidlm/multi-edit-journal");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let journal = JournalStore::lock(&directory).unwrap();
    let image = |text: &str| {
        Some(FileImage {
            bytes: text.as_bytes().to_vec(),
            mode: Some(0o600),
        })
    };
    let record = RecoveryRecord::new(
        RecoveryState::Intent,
        vec![RecoveryEntry {
            path: "notes.txt".into(),
            before: image("alpha\n"),
            after: image("beta\n"),
        }],
    )
    .unwrap();
    journal.write_intent(&record).unwrap();
    drop(journal);
    let config = env.home.join("config.toml");
    std::fs::write(&config, config_doc(&format!("http://{}/v1", server.addr))).unwrap();
    let (code, _, stderr) = run_rapid_in(&env.project, &env.home, &config, Some("acceptEdits"));
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(notes(&env), "gamma\n");
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 3, "{requests:?}");
    let body = requests[1].split_once("\r\n\r\n").unwrap().1;
    let wire: serde_json::Value = serde_json::from_str(body).unwrap();
    let tool_result = wire["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(
        tool_result.contains("alpha") && !tool_result.contains("beta"),
        "{tool_result}"
    );
    assert!(requests[2].contains("wrote"), "{}", requests[2]);
    drop(requests);
    assert_eq!(
        JournalStore::lock(&directory).unwrap().load().unwrap(),
        None
    );
}

#[test]
#[cfg(unix)]
fn binary_refuses_edits_until_pending_or_corrupt_journal_is_recovered() {
    use std::os::unix::fs::PermissionsExt;
    use workspace::multi_edit_journal::JournalStore;
    use workspace::multi_edit_recovery::{FileImage, RecoveryEntry, RecoveryRecord, RecoveryState};
    for scenario in ["active", "corrupt", "clear"] {
        let server = spawn_scripted_server(vec![
            (200, read_notes_tool_call_body()),
            (200, patch_tool_call_body()),
            (200, terminal_body("done")),
        ]);
        let env = TrustedProject::new("bin-pending-journal");
        let directory = env.project.join(".rapidlm/multi-edit-journal");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        if scenario == "active" {
            let journal = JournalStore::lock(&directory).unwrap();
            let image = |text: &str| {
                Some(FileImage {
                    bytes: text.as_bytes().to_vec(),
                    mode: Some(0o600),
                })
            };
            let record = RecoveryRecord::new(
                RecoveryState::Intent,
                vec![RecoveryEntry {
                    path: "notes.txt".into(),
                    before: image("different\n"),
                    after: image("beta\n"),
                }],
            )
            .unwrap();
            journal.write_intent(&record).unwrap();
        } else if scenario == "corrupt" {
            std::fs::write(directory.join("active.json"), "broken").unwrap();
        }
        let config = env.home.join("config.toml");
        std::fs::write(&config, config_doc(&format!("http://{}/v1", server.addr))).unwrap();
        let (code, _, stderr) = run_rapid_in(&env.project, &env.home, &config, Some("acceptEdits"));
        if scenario == "clear" {
            assert_eq!(code, Some(0), "{scenario}: {stderr}");
        } else {
            assert_ne!(code, Some(0), "{scenario}: {stderr}");
            assert!(
                stderr.contains("workspace recovery could not complete"),
                "{scenario}: {stderr}"
            );
        }
        assert_eq!(
            notes(&env),
            if scenario == "clear" {
                "beta\n"
            } else {
                "alpha\n"
            },
            "{scenario}"
        );
        let requests = server.requests.lock().unwrap();
        if scenario != "clear" {
            assert!(requests.is_empty(), "{scenario}: {requests:?}");
        }
    }
}

#[test]
fn binary_multi_edit_is_single_file_atomic_and_requires_a_visible_read() {
    for scenario in ["fresh", "conflict", "unread"] {
        let edits = if scenario == "conflict" {
            serde_json::json!([{"old":"alpha","new":"beta"},{"old":"missing","new":"gamma"}])
        } else {
            serde_json::json!([{"old":"alpha","new":"beta"},{"old":"beta","new":"gamma"}])
        };
        let mut responses = Vec::new();
        if scenario != "unread" {
            responses.push((200, read_notes_tool_call_body()));
        }
        responses.push((
            200,
            tool_call_body(
                "workspace_patch",
                &serde_json::json!({"path":"notes.txt","edits":edits}),
            ),
        ));
        responses.push((200, terminal_body("done")));
        let server = spawn_scripted_server(responses);
        let env = TrustedProject::new("bin-multi-patch");
        let config = env.home.join("config.toml");
        std::fs::write(&config, config_doc(&format!("http://{}/v1", server.addr))).unwrap();
        let (code, _, stderr) = run_rapid_in(&env.project, &env.home, &config, Some("acceptEdits"));
        assert_eq!(code, Some(0), "{scenario}: {stderr}");
        assert_eq!(
            notes(&env),
            if scenario == "fresh" {
                "gamma\n"
            } else {
                "alpha\n"
            },
            "{scenario}"
        );
        let requests = server.requests.lock().unwrap();
        let last = requests.last().unwrap();
        let expected = match scenario {
            "fresh" => "applied 2 edits",
            "conflict" => "edit 2: old text not found",
            _ => "read this file in the current session",
        };
        assert!(last.contains(expected), "{scenario}: {last}");
    }
}

#[cfg(unix)]
#[test]
fn binary_multi_file_patch_requires_all_reads_and_commits_or_refuses_together() {
    for scenario in ["fresh", "conflict", "unread"] {
        let mut responses = vec![(200, read_notes_tool_call_body())];
        if scenario != "unread" {
            responses.push((
                200,
                tool_call_body("repo_read", &serde_json::json!({"path":"other.txt"})),
            ));
        }
        let old = if scenario == "conflict" {
            "missing"
        } else {
            "bravo"
        };
        responses.push((
            200,
            tool_call_body(
                "workspace_multi_patch",
                &serde_json::json!({"files":[
                    {"path":"notes.txt","edits":[{"old":"alpha","new":"beta"}]},
                    {"path":"other.txt","edits":[{"old":old,"new":"charlie"}]}
                ]}),
            )
            .replace("call_1", "multi_1"),
        ));
        responses.push((200, terminal_body("done")));
        let server = spawn_scripted_server(responses);
        let env = TrustedProject::new(&format!("bin-multi-file-{scenario}"));
        std::fs::write(env.project.join("other.txt"), "bravo\n").unwrap();
        let config = env.home.join("config.toml");
        std::fs::write(&config, config_doc(&format!("http://{}/v1", server.addr))).unwrap();
        let (code, _, stderr) = run_rapid_in(&env.project, &env.home, &config, Some("acceptEdits"));
        assert_eq!(code, Some(0), "{scenario}: {stderr}");
        assert_eq!(
            notes(&env),
            if scenario == "fresh" {
                "beta\n"
            } else {
                "alpha\n"
            }
        );
        assert_eq!(
            std::fs::read_to_string(env.project.join("other.txt")).unwrap(),
            if scenario == "fresh" {
                "charlie\n"
            } else {
                "bravo\n"
            }
        );
        let requests = server.requests.lock().unwrap();
        assert!(requests[0].contains("workspace_multi_patch"));
        let last = requests.last().unwrap();
        let expected = match scenario {
            "fresh" => "Multi-file patch committed",
            "conflict" => "old text not found",
            _ => "must be read in this session again",
        };
        assert!(last.contains(expected), "{scenario}: {last}");
    }
}

#[cfg(unix)]
#[test]
fn binary_multi_file_patch_creates_and_deletes_or_refuses_together() {
    use std::os::unix::fs::PermissionsExt;
    for scenario in ["fresh", "unread", "existing"] {
        let mut responses = Vec::new();
        if scenario != "unread" {
            responses.push((200, read_notes_tool_call_body()));
        }
        responses.push((
            200,
            tool_call_body(
                "workspace_multi_patch",
                &serde_json::json!({"files":[
                    {"path":"notes.txt","delete":true}, {"path":"new.txt","create":"new content"}
                ]}),
            )
            .replace("call_1", "multi_1"),
        ));
        responses.push((200, terminal_body("done")));
        let server = spawn_scripted_server(responses);
        let env = TrustedProject::new(&format!("bin-multi-create-{scenario}"));
        if scenario == "existing" {
            std::fs::write(env.project.join("new.txt"), "existing").unwrap();
        }
        let config = env.home.join("config.toml");
        std::fs::write(&config, config_doc(&format!("http://{}/v1", server.addr))).unwrap();
        let (code, _, stderr) = run_rapid_in(&env.project, &env.home, &config, Some("acceptEdits"));
        assert_eq!(code, Some(0), "{scenario}: {stderr}");
        if scenario == "fresh" {
            assert!(!env.project.join("notes.txt").exists());
            assert_eq!(
                std::fs::read_to_string(env.project.join("new.txt")).unwrap(),
                "new content"
            );
            assert_eq!(
                std::fs::metadata(env.project.join("new.txt"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        } else {
            assert_eq!(notes(&env), "alpha\n");
            if scenario == "existing" {
                assert_eq!(
                    std::fs::read_to_string(env.project.join("new.txt")).unwrap(),
                    "existing"
                );
            } else {
                assert!(!env.project.join("new.txt").exists());
            }
        }
        let requests = server.requests.lock().unwrap();
        let last = requests.last().unwrap();
        let expected = match scenario {
            "fresh" => "Multi-file patch committed",
            "existing" => "already exists",
            _ => "must be read in this session again",
        };
        assert!(last.contains(expected), "{scenario}: {last}");
    }
}

#[test]
fn binary_exec_refuses_an_unread_existing_file_even_when_edits_are_allowed() {
    let server = spawn_scripted_server(vec![
        (200, patch_tool_call_body()),
        (200, terminal_body("read required")),
    ]);
    let env = TrustedProject::new("bin-patch-unread");
    let config_path = env.home.join("config.toml");
    std::fs::write(
        &config_path,
        config_doc(&format!("http://{}/v1", server.addr)),
    )
    .expect("write config");
    let (code, _, stderr) =
        run_rapid_in(&env.project, &env.home, &config_path, Some("acceptEdits"));
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(notes(&env), "alpha\n");
    let requests = server.requests.lock().expect("requests");
    let follow_up = requests.last().expect("follow-up request");
    assert!(
        follow_up.contains("read this file in the current session"),
        "{follow_up}"
    );
}

#[test]
fn binary_image_crop_is_sent_only_to_vision_models() {
    for vision in [false, true] {
        let server = spawn_scripted_server(vec![
            (
                200,
                tool_call_body(
                    "workspace_read",
                    &serde_json::json!({"path":"image.png","region":{"x":1,"y":1,"width":2,"height":2},"full_resolution":true}),
                ),
            ),
            (200, terminal_body("done")),
        ]);
        let env = TrustedProject::new(&format!("bin-image-crop-{vision}"));
        let pixels = image::RgbaImage::from_pixel(4, 4, image::Rgba([10, 20, 30, 255]));
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(pixels)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        std::fs::write(env.project.join("image.png"), png.into_inner()).unwrap();
        let config_path = env.home.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "{}vision = {vision}\n",
                config_doc(&format!("http://{}/v1", server.addr))
            ),
        )
        .unwrap();
        let (code, _, stderr) = run_rapid_in(&env.project, &env.home, &config_path, None);
        assert_eq!(code, Some(0), "{stderr}");
        let requests = server.requests.lock().unwrap();
        let reply = requests.last().unwrap();
        assert_eq!(reply.contains("data:image/png;base64,"), vision, "{reply}");
        if vision {
            use base64::Engine as _;
            let data = reply
                .split_once("data:image/png;base64,")
                .unwrap()
                .1
                .split('"')
                .next()
                .unwrap();
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data)
                .unwrap();
            let image = image::load_from_memory(&bytes).unwrap();
            assert_eq!((image.width(), image.height()), (2, 2));
            assert_eq!(
                image.to_rgba8().get_pixel(0, 0),
                &image::Rgba([10, 20, 30, 255])
            );
        } else {
            assert!(reply.contains("image omitted"), "{reply}");
        }
    }
}

#[test]
fn binary_search_context_reaches_the_provider_with_surrounding_rows() {
    let server = spawn_scripted_server(vec![
        (
            200,
            tool_call_body(
                "repo_search",
                &serde_json::json!({"pattern":"needle","context":1}),
            ),
        ),
        (200, terminal_body("found")),
    ]);
    let env = TrustedProject::new("bin-search-context");
    std::fs::write(env.project.join("notes.txt"), "before\nneedle\nafter\n").unwrap();
    let config_path = env.home.join("config.toml");
    std::fs::write(
        &config_path,
        config_doc(&format!("http://{}/v1", server.addr)),
    )
    .unwrap();
    let (code, _, stderr) = run_rapid_in(&env.project, &env.home, &config_path, None);
    assert_eq!(code, Some(0), "{stderr}");
    let requests = server.requests.lock().unwrap();
    let reply = requests.last().unwrap();
    for row in [
        "notes.txt-1- before",
        "notes.txt:2: needle",
        "notes.txt-3- after",
    ] {
        assert!(reply.contains(row), "missing {row}: {reply}");
    }
}

#[test]
fn binary_exec_applies_workspace_patch_end_to_end_in_accept_edits_mode() {
    for round in 0..2 {
        let server = spawn_scripted_server(vec![
            (200, read_notes_tool_call_body()),
            (200, patch_tool_call_body()),
            (200, terminal_body("patched notes.txt")),
        ]);
        let env = TrustedProject::new(&format!("bin-patch-{round}"));
        let config_path = env.home.join("config.toml");
        std::fs::write(
            &config_path,
            config_doc(&format!("http://{}/v1", server.addr)),
        )
        .expect("write config");
        let (code, stdout, stderr) =
            run_rapid_in(&env.project, &env.home, &config_path, Some("acceptEdits"));
        assert_eq!(code, Some(0), "round {round} stderr: {stderr}");
        assert!(
            stdout.contains("patched notes.txt"),
            "round {round}: stdout must carry the final model text: {stdout}"
        );
        // The workspace effect actually occurred on disk.
        let content =
            std::fs::read_to_string(env.project.join("notes.txt")).expect("patched file exists");
        assert_eq!(content, "beta\n", "round {round}: patch must have landed");
        // The follow-up request carried the per-call tool result.
        let requests = server.requests.lock().expect("requests");
        assert!(requests.len() >= 2, "round {round}: expected a tool step");
        assert!(
            requests[requests.len() - 1].contains("\"tool_call_id\":\"call_1\""),
            "round {round}: per-call tool message missing"
        );
        assert!(
            !requests[requests.len() - 1].contains("tool results:"),
            "round {round}: flat report must be gone"
        );
    }
}

/// Run any `rapid` subcommand in the same project/home/config a
/// `run_rapid_in` exec used, so what that run recorded can be read back
/// through the binary rather than by opening its files.
fn run_rapid_args_in(
    project: &Path,
    home: &Path,
    config_path: &Path,
    args: &[&str],
) -> (Option<i32>, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_rapid"))
        .args(args)
        .current_dir(project)
        .env("HOME", home)
        .env_remove("RAPIDLM_HOME")
        .env_remove("RAPIDLM_MODEL")
        .env_remove("RAPIDLM_PERMISSION_MODE")
        .env("RAPIDLM_CONFIG", config_path)
        .output()
        .expect("run rapid");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn binary_exec_records_its_session_in_the_project_ledger() {
    // A headless run used to mint a `SessionId` for its execution request
    // and never open the ledger: a run that wrote a file was not in `rapid
    // sessions list`, could not be `rapid resume`d, and had no `/diff`. The
    // TUI recorded all of it. Everything asserted here is read back through
    // the binary's own commands, the way a user would.
    let server = spawn_scripted_server(vec![
        (200, read_notes_tool_call_body()),
        (200, patch_tool_call_body()),
        (200, terminal_body("patched notes.txt")),
    ]);
    let env = TrustedProject::new("bin-recorded");
    let config_path = env.home.join("config.toml");
    std::fs::write(
        &config_path,
        config_doc(&format!("http://{}/v1", server.addr)),
    )
    .expect("write config");
    let (code, stdout, stderr) =
        run_rapid_in(&env.project, &env.home, &config_path, Some("acceptEdits"));
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(stdout.contains("patched notes.txt"), "{stdout}");
    assert_eq!(
        std::fs::read_to_string(env.project.join("notes.txt")).expect("patched"),
        "beta\n"
    );

    // The run names the session it recorded, on stderr, so stdout stays
    // the answer.
    let session_id = stderr
        .lines()
        .find_map(|line| {
            line.strip_prefix("session ")
                .and_then(|rest| rest.split_whitespace().next())
        })
        .unwrap_or_else(|| panic!("the run must name the session it recorded:\n{stderr}"));
    assert!(
        stderr.contains(&format!("`rapid resume {session_id}` reopens it")),
        "{stderr}"
    );

    // It is in this project's session list.
    let (code, listed, err) =
        run_rapid_args_in(&env.project, &env.home, &config_path, &["sessions", "list"]);
    assert_eq!(code, Some(0), "{err}");
    assert!(listed.contains("count=1"), "one recorded session: {listed}");
    assert!(
        listed.contains(&format!("session={session_id}")),
        "the listed session is the one the run named: {listed}"
    );

    // And the record carries what the run did: the prompt, the write with
    // its hunk, and how the turn ended.
    let export = env.project.join("export.jsonl");
    let (code, _, err) = run_rapid_args_in(
        &env.project,
        &env.home,
        &config_path,
        &[
            "inspect-export",
            session_id,
            export.to_str().expect("utf-8 path"),
        ],
    );
    assert_eq!(code, Some(0), "{err}");
    let exported = std::fs::read_to_string(&export).expect("export written");
    assert!(
        exported.contains("turn.started")
            && exported.contains("patch notes.txt by replacing alpha with beta"),
        "the prompt must be in the record: {exported}"
    );
    assert!(
        exported.contains("workspace.mutation_detected")
            && exported.contains("notes.txt")
            && exported.contains("+beta"),
        "the write and its hunk must be in the record: {exported}"
    );
    assert!(
        exported.contains("turn.completed"),
        "the turn's end must be in the record: {exported}"
    );
    // And the turn's own progress — the model round trips and the tool
    // request — which reach the ledger only through the forwarding sink.
    assert!(
        exported.contains("model.completed") && exported.contains("tool.requested"),
        "the turn's progress events must be in the record: {exported}"
    );
}

#[test]
fn binary_exec_continue_runs_the_next_turn_of_the_recorded_session() {
    // `--continue` is a conversation across runs: the second run's model
    // request must carry what the first run asked and answered, and both
    // turns must land in one session, not two.
    let server = spawn_scripted_server(vec![
        (200, terminal_body("Noted: the codename is Nightjar.")),
        (200, terminal_body("It is Nightjar.")),
        (200, terminal_body("The tidy-up loop still runs.")),
    ]);
    let env = TrustedProject::new("bin-continue");
    let config_path = env.home.join("config.toml");
    std::fs::write(
        &config_path,
        config_doc(&format!("http://{}/v1", server.addr)),
    )
    .expect("write config");

    let (code, stdout, stderr) = run_rapid_args_in(
        &env.project,
        &env.home,
        &config_path,
        &["exec", "the codename is Nightjar; remember it"],
    );
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(
        stdout.contains("Noted: the codename is Nightjar."),
        "{stdout}"
    );
    let first_session = stderr
        .lines()
        .find_map(|line| {
            line.strip_prefix("session ")
                .and_then(|rest| rest.split_whitespace().next())
        })
        .unwrap_or_else(|| panic!("the first run must name its session:\n{stderr}"))
        .to_owned();

    let (code, stdout, stderr) = run_rapid_args_in(
        &env.project,
        &env.home,
        &config_path,
        &["exec", "--continue", "what is the codename?"],
    );
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(stdout.contains("It is Nightjar."), "{stdout}");
    assert!(
        stderr.contains(&format!("session {first_session} continued")),
        "the second run continues the first run's session:\n{stderr}"
    );

    {
        let requests = server.requests.lock().expect("requests");
        assert_eq!(requests.len(), 2, "one model call per run");
        let second = &requests[1];
        assert!(
            second.contains("the codename is Nightjar; remember it")
                && second.contains("Noted: the codename is Nightjar."),
            "the second run's model request must carry the first run's turn:\n{second}"
        );
        assert!(
            !requests[0].contains("what is the codename?"),
            "and the first run's request was its own prompt only"
        );
        assert!(
            !second.contains("Still running in this session"),
            "nothing runs, so the continued run is told nothing runs:\n{second}"
        );
    }

    // A loop of the session's outlives both runs: the next continued run
    // is told it still runs, derived from the loop store.
    let (code, _, stderr) = run_rapid_args_in(
        &env.project,
        &env.home,
        &config_path,
        &[
            "loop",
            "add",
            "1h",
            "tidy",
            "up",
            "--session",
            &first_session,
        ],
    );
    assert_eq!(code, Some(0), "stderr: {stderr}");
    let (code, _, stderr) = run_rapid_args_in(
        &env.project,
        &env.home,
        &config_path,
        &["exec", "--continue", "anything still going?"],
    );
    assert_eq!(code, Some(0), "stderr: {stderr}");
    {
        let requests = server.requests.lock().expect("requests");
        let third = &requests[2];
        assert!(
            third.contains("Still running in this session") && third.contains("tidy up"),
            "the continued run names the session's loop:\n{third}"
        );
    }

    // `--resume <id>` with an id this project never recorded refuses,
    // naming the ids it does have, rather than starting a fresh session
    // under a flag that promised a conversation.
    let (code, _, stderr) = run_rapid_args_in(
        &env.project,
        &env.home,
        &config_path,
        &[
            "exec",
            "--resume",
            "018f3c8a-7e2b-7a10-8c4d-0123456789ab",
            "anything",
        ],
    );
    assert_ne!(code, Some(0));
    assert!(
        stderr.contains("has not been recorded in this project") && stderr.contains(&first_session),
        "{stderr}"
    );
    assert_eq!(
        server.requests.lock().expect("requests").len(),
        3,
        "no model call was made"
    );
}

#[test]
fn binary_exec_in_a_bare_directory_records_there_like_the_tui_does() {
    // A directory with no `.rapidlm` or `.git` above it is its own project
    // root — the TUI's rule, and now the headless run's: the session goes
    // into that directory's own `.rapidlm/`, untrusted or not, so a run
    // made anywhere can be listed and resumed from there.
    let server = spawn_scripted_server(vec![(200, terminal_body("hello"))]);
    let home = temp_dir("bin-bare-home");
    let scratch = temp_dir("bin-bare-scratch");
    let config_path = home.join("config.toml");
    std::fs::write(
        &config_path,
        config_doc(&format!("http://{}/v1", server.addr)),
    )
    .expect("write config");
    let (code, stdout, stderr) =
        run_rapid_args_in(&scratch, &home, &config_path, &["exec", "say hello"]);
    assert_eq!(code, Some(0), "{stderr}");
    assert!(stdout.contains("hello"), "{stdout}");
    assert!(
        stderr.contains("recorded in this project"),
        "a run in a bare directory is recorded in it:\n{stderr}"
    );
    let (code, listed, err) =
        run_rapid_args_in(&scratch, &home, &config_path, &["sessions", "list"]);
    assert_eq!(code, Some(0), "{err}");
    assert!(listed.contains("count=1"), "{listed}");
}

#[test]
fn binary_exec_in_default_mode_denies_the_patch_and_keeps_disk_intact() {
    let server = spawn_scripted_server(vec![
        (200, read_notes_tool_call_body()),
        (200, patch_tool_call_body()),
        (200, terminal_body("nothing to change")),
    ]);
    let env = TrustedProject::new("bin-patch-denied");
    let config_path = env.home.join("config.toml");
    std::fs::write(
        &config_path,
        config_doc(&format!("http://{}/v1", server.addr)),
    )
    .expect("write config");
    let (code, stdout, _stderr) = run_rapid_in(&env.project, &env.home, &config_path, None);
    assert_eq!(code, Some(0));
    assert!(stdout.contains("nothing to change"));
    // default mode asks; headless exec denies the edit typed — the disk is
    // untouched and the model saw the denial.
    let content =
        std::fs::read_to_string(env.project.join("notes.txt")).expect("file still present");
    assert_eq!(content, "alpha\n", "a default-mode edit must be denied");
    let requests = server.requests.lock().expect("requests");
    let last = requests.last().expect("at least one request");
    assert!(
        last.contains("\"tool_call_id\":\"call_1\""),
        "the denial must be a model-visible tool result: {last}"
    );
}

fn run_rapid_cron_in(
    project: &PathBuf,
    home: &PathBuf,
    config_path: &PathBuf,
    db_path: &Path,
    args: &[&str],
) -> (Option<i32>, String, String) {
    let mut full_args = vec![
        "cron".to_owned(),
        "--db".to_owned(),
        db_path.display().to_string(),
    ];
    full_args.extend(args.iter().map(|s| (*s).to_owned()));
    let output = Command::new(env!("CARGO_BIN_EXE_rapid"))
        .args(&full_args)
        .current_dir(project)
        .env("HOME", home)
        .env_remove("RAPIDLM_HOME")
        .env_remove("RAPIDLM_MODEL")
        .env("RAPIDLM_CONFIG", config_path)
        // Set deliberately permissive — the whole point of this test is that
        // `rapid cron poll` must force `plan` mode regardless of what the
        // ambient environment says, not merely default to it.
        .env("RAPIDLM_PERMISSION_MODE", "acceptEdits")
        .output()
        .expect("run rapid cron");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Seconds until the top of the next minute, plus a one-second buffer —
/// `rapid cron`'s schedule grammar has a one-minute floor (Modbit `AGT-008`/
/// `newtask.md` §3.2), so there is no faster way to get a real, un-mocked
/// `rapid cron poll` invocation to see a genuinely due job.
fn seconds_until_next_minute_boundary() -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    (60 - (now % 60)) + 1
}

#[test]
#[ignore = "waits for a real minute boundary (rapid cron's schedule grammar has a one-minute \
            floor); run explicitly with `cargo test -- --ignored` to verify end to end"]
fn binary_cron_poll_runs_the_fired_job_in_plan_mode_and_denies_the_patch() {
    let server = spawn_scripted_server(vec![
        (200, read_notes_tool_call_body()),
        (200, patch_tool_call_body()),
        (200, terminal_body("nothing to change")),
    ]);
    let env = TrustedProject::new("cron-plan-mode");
    let config_path = env.home.join("config.toml");
    std::fs::write(
        &config_path,
        config_doc(&format!("http://{}/v1", server.addr)),
    )
    .expect("write config");
    let db_path = env.home.join("cron.sqlite");

    let (add_code, add_stdout, add_stderr) = run_rapid_cron_in(
        &env.project,
        &env.home,
        &config_path,
        &db_path,
        &[
            "add",
            "--prompt",
            "patch notes.txt by replacing alpha with beta",
            "--schedule",
            "* * * * *",
        ],
    );
    assert_eq!(add_code, Some(0), "cron add stderr: {add_stderr}");
    assert!(
        add_stdout.contains("status=active"),
        "add stdout: {add_stdout}"
    );

    std::thread::sleep(std::time::Duration::from_secs(
        seconds_until_next_minute_boundary(),
    ));

    let (poll_code, poll_stdout, poll_stderr) =
        run_rapid_cron_in(&env.project, &env.home, &config_path, &db_path, &["poll"]);
    assert_eq!(poll_code, Some(0), "cron poll stderr: {poll_stderr}");
    assert!(
        poll_stdout.contains("fired=1"),
        "poll stdout: {poll_stdout}"
    );
    assert!(
        poll_stdout.contains("outcome=exit:0"),
        "the fired job's turn must have actually run: {poll_stdout}"
    );

    // The real point of this test: even though the ambient environment set
    // `RAPIDLM_PERMISSION_MODE=acceptEdits` (which would apply the patch for
    // `rapid exec` directly — see the sibling `accept_edits` test above),
    // the cron-fired turn ran forced into `plan` mode and the patch was
    // denied, not applied.
    let content =
        std::fs::read_to_string(env.project.join("notes.txt")).expect("file still present");
    assert_eq!(
        content, "alpha\n",
        "a cron-fired turn must never apply a mutation, even in an acceptEdits environment"
    );
    let requests = server.requests.lock().expect("requests");
    assert!(
        requests.len() >= 2,
        "the model must have actually been called, not skipped: {}",
        requests.len()
    );
}

/// SSE chat-completions stream proposing one `task_spawn` of `agent_type`.
fn spawn_call_body(agent_type: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{{\"name\":\"task_spawn\",\"arguments\":\"{{\\\"prompt\\\":\\\"review notes.txt\\\",\\\"type\\\":\\\"{agent_type}\\\"}}\"}}}}]}},\"finish_reason\":\"tool_calls\"}}]}}\n\
         \n\
         data: [DONE]\n\n"
    )
}

#[test]
fn binary_a_defined_agent_type_sets_the_childs_model_effort_instructions_and_tools() {
    // ADR 0023 §5, end to end: a project definition names a model, an
    // effort and instructions and narrows its tools; the child's own model
    // request carries each. The user's definitions list beside the
    // project's, and an unknown type is refused naming the known ones.
    let server = spawn_scripted_server(vec![
        (200, spawn_call_body("reviewer")),
        (200, terminal_body("child saw it")),
        (200, terminal_body("all done")),
        (200, spawn_call_body("wizard")),
        (200, terminal_body("refused as expected")),
    ]);
    let env = TrustedProject::new("bin-agent-types");
    let config_path = env.home.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "{}\n[model.fast]\nprovider = \"openai-compatible\"\nmodel = \"fast-wire\"\n\
             base_url = \"http://{}/v1\"\napi_key = \"scripted-key\"\n",
            config_doc(&format!("http://{}/v1", server.addr)),
            server.addr
        ),
    )
    .expect("write config");
    // A headless run has no one to ask: spawning is pre-approved.
    std::fs::write(
        env.project.join(".rapidlm/settings.json"),
        r#"{"permissions": {"allow": ["task_spawn"]}}"#,
    )
    .expect("settings");
    let defs = env.project.join(".rapidlm/agents");
    std::fs::create_dir_all(&defs).expect("defs dir");
    std::fs::write(
        defs.join("reviewer.toml"),
        "schema = \"rapidlm.agent_defs.v1\"\n[agent]\nid = \"reviewer\"\n\
         description = \"reviews notes\"\nbase_role = \"explorer\"\ntools = [\"read\"]\n\
         instructions = \"STANDING-REVIEWER-MARKER: cite every line.\"\n\
         model = \"fast\"\nreasoning_effort = \"low\"\n",
    )
    .expect("project definition");
    let user_defs = env.home.join(".rapidlm/agents");
    std::fs::create_dir_all(&user_defs).expect("user defs dir");
    std::fs::write(
        user_defs.join("tidier.toml"),
        "schema = \"rapidlm.agent_defs.v1\"\n[agent]\nid = \"tidier\"\n\
         description = \"tidies\"\nbase_role = \"planner\"\ntools = [\"read\"]\n",
    )
    .expect("user definition");

    let (code, stdout, stderr) =
        run_rapid_args_in(&env.project, &env.home, &config_path, &["agents", "list"]);
    assert_eq!(code, Some(0), "stderr: {stderr}");
    let line = |id: &str| {
        stdout
            .lines()
            .find(|line| line.starts_with(&format!("id={id} ")))
            .unwrap_or_else(|| panic!("no {id} in:\n{stdout}"))
            .to_owned()
    };
    let reviewer = line("reviewer");
    assert!(
        reviewer.contains("role=explorer source=project:")
            && reviewer.contains("model=fast reasoning_effort=low"),
        "{reviewer}"
    );
    assert!(
        line("tidier").contains("role=planner source=user:"),
        "{stdout}"
    );

    let (code, stdout, stderr) = run_rapid_args_in(
        &env.project,
        &env.home,
        &config_path,
        &["exec", "delegate it"],
    );
    assert_eq!(code, Some(0), "stdout: {stdout}\nstderr: {stderr}");
    {
        let requests = server.requests.lock().expect("requests");
        assert!(
            requests.len() >= 3,
            "{} requests; the last: {}\nstdout: {stdout}\nstderr: {stderr}",
            requests.len(),
            requests
                .last()
                .map(|r| &r[r.len().saturating_sub(1500)..])
                .unwrap_or("")
        );
        let (parent, child, after) = (&requests[0], &requests[1], &requests[2]);
        assert!(parent.contains("\"model\":\"test-model\""), "{parent}");
        assert!(!parent.contains("\"reasoning_effort\""), "{parent}");
        // The child's own request: its type's model, effort, instructions,
        // and only the tools its surface allows.
        assert!(child.contains("\"model\":\"fast-wire\""), "{child}");
        assert!(child.contains("\"reasoning_effort\":\"low\""), "{child}");
        assert!(child.contains("STANDING-REVIEWER-MARKER"), "{child}");
        assert!(child.contains("\"repo_read\""), "{child}");
        for absent in ["\"shell_exec\"", "\"workspace_write\"", "\"web_fetch\""] {
            assert!(!child.contains(absent), "{absent} offered to the child");
        }
        assert!(after.contains("child saw it"), "{after}");
    }

    // An unknown type: refused, naming the known ones, and nothing spawned.
    let (code, _, stderr) = run_rapid_args_in(
        &env.project,
        &env.home,
        &config_path,
        &["exec", "delegate again"],
    );
    assert_eq!(code, Some(0), "stderr: {stderr}");
    let requests = server.requests.lock().expect("requests");
    assert_eq!(requests.len(), 5, "no child request for an unknown type");
    let refused = &requests[4];
    assert!(
        refused.contains("no agent type 'wizard'") && refused.contains("reviewer"),
        "{refused}"
    );
}

#[test]
fn binary_a_markdown_agent_definition_shapes_the_child_like_a_table_one() {
    // EXT-05 slice 1, end to end: the same definition written as Markdown (the
    // body is the instructions) in the project and, in a directory the user's
    // `[agents] extra_dirs` names, gives the child its model, effort,
    // instructions and narrowed tools; `rapid agents list` shows the format.
    let server = spawn_scripted_server(vec![
        (200, spawn_call_body("reviewer")),
        (200, terminal_body("child saw it")),
        (200, terminal_body("all done")),
        (200, spawn_call_body("shared-planner")),
        (200, terminal_body("planned")),
        (200, terminal_body("all done too")),
    ]);
    let env = TrustedProject::new("bin-agent-markdown");
    let shared = env.home.join("shared-agents");
    std::fs::create_dir_all(&shared).expect("shared dir");
    let config_path = env.home.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "{}\n[model.fast]\nprovider = \"openai-compatible\"\nmodel = \"fast-wire\"\n\
             base_url = \"http://{}/v1\"\napi_key = \"scripted-key\"\n\
             [agents]\nextra_dirs = [{:?}]\n",
            config_doc(&format!("http://{}/v1", server.addr)),
            server.addr,
            shared.display().to_string()
        ),
    )
    .expect("write config");
    std::fs::write(
        env.project.join(".rapidlm/settings.json"),
        r#"{"permissions": {"allow": ["task_spawn"]}}"#,
    )
    .expect("settings");
    let defs = env.project.join(".rapidlm/agents");
    std::fs::create_dir_all(&defs).expect("defs dir");
    std::fs::write(
        defs.join("reviewer.md"),
        "---\ndescription: reviews notes\nbase_role: explorer\ntool_classes: read\n\
         model: fast\nreasoning_effort: low\n---\n\
         MARKDOWN-REVIEWER-MARKER: cite every line.\n",
    )
    .expect("project definition");
    std::fs::write(
        shared.join("shared-planner.md"),
        "---\ndescription: plans from the shared directory\nbase_role: planner\n\
         tool_classes: read\n---\nSHARED-PLANNER-MARKER: plan in steps.\n",
    )
    .expect("extra definition");

    let (code, stdout, stderr) =
        run_rapid_args_in(&env.project, &env.home, &config_path, &["agents", "list"]);
    assert_eq!(code, Some(0), "stderr: {stderr}");
    let line = |id: &str| {
        stdout
            .lines()
            .find(|line| line.starts_with(&format!("id={id} ")))
            .unwrap_or_else(|| panic!("no {id} in:\n{stdout}"))
            .to_owned()
    };
    assert!(
        line("reviewer").contains("format=md role=explorer source=project:"),
        "{stdout}"
    );
    assert!(
        line("shared-planner").contains("format=md role=planner source=extra:"),
        "{stdout}"
    );

    for (task, marker) in [
        ("delegate it", "MARKDOWN-REVIEWER-MARKER"),
        ("delegate to the shared one", "SHARED-PLANNER-MARKER"),
    ] {
        let before = server.requests.lock().expect("requests").len();
        let (code, stdout, stderr) =
            run_rapid_args_in(&env.project, &env.home, &config_path, &["exec", task]);
        assert_eq!(code, Some(0), "stdout: {stdout}\nstderr: {stderr}");
        let requests = server.requests.lock().expect("requests");
        assert!(requests.len() >= before + 3, "{} requests", requests.len());
        let child = &requests[before + 1];
        assert!(
            child.contains(marker),
            "{marker} not in the child's request: {child}"
        );
        assert!(child.contains("\"repo_read\""), "{child}");
        for absent in ["\"shell_exec\"", "\"workspace_write\"", "\"web_fetch\""] {
            assert!(!child.contains(absent), "{absent} offered to the child");
        }
        if marker.starts_with("MARKDOWN") {
            assert!(child.contains("\"model\":\"fast-wire\""), "{child}");
            assert!(child.contains("\"reasoning_effort\":\"low\""), "{child}");
        }
    }
}

#[test]
fn binary_a_definitions_narrowing_holds_for_its_child_and_its_step_cap_ends_it_with_a_report() {
    // EXT-05 slice 2, end to end: the child sees only the tools its definition
    // keeps, a rule of the definition refuses a read of one file, a forced call
    // to a tool taken away is refused by name, and the cap ends it after exactly
    // that many model steps with a report of what it did.
    let server = spawn_scripted_server(vec![
        (200, spawn_call_body("guard")),
        (
            200,
            tool_call_body("repo_read", &serde_json::json!({"path": "secret.txt"})),
        ),
        (
            200,
            tool_call_body("repo_search", &serde_json::json!({"pattern": "alpha"})),
        ),
        (
            200,
            tool_call_body("repo_read", &serde_json::json!({"path": "notes.txt"})),
        ),
        (200, terminal_body("parent done")),
    ]);
    let env = TrustedProject::new("bin-agent-narrowing");
    let config_path = env.home.join("config.toml");
    std::fs::write(
        &config_path,
        config_doc(&format!("http://{}/v1", server.addr)),
    )
    .expect("write config");
    std::fs::write(
        env.project.join(".rapidlm/settings.json"),
        r#"{"permissions": {"allow": ["task_spawn"]}}"#,
    )
    .expect("settings");
    std::fs::write(env.project.join("notes.txt"), "alpha\n").expect("notes");
    std::fs::write(env.project.join("secret.txt"), "hunter2\n").expect("secret");
    let defs = env.project.join(".rapidlm/agents");
    std::fs::create_dir_all(&defs).expect("defs dir");
    std::fs::write(
        defs.join("guard.md"),
        "---\ndescription: reads the notes\nbase_role: explorer\ntool_classes: read\n\
         allow_tools: repo_read repo_search workspace_read\ndeny_tools: repo_search\n\
         permission:\n  \"repo_read(secret*)\": deny\nmax_steps: 3\n---\nGUARD-MARKER\n",
    )
    .expect("definition");

    let (code, stdout, stderr) = run_rapid_args_in(
        &env.project,
        &env.home,
        &config_path,
        &["agents", "validate"],
    );
    assert_eq!(code, Some(0), "stdout: {stdout}\nstderr: {stderr}");
    let (code, stdout, stderr) = run_rapid_args_in(
        &env.project,
        &env.home,
        &config_path,
        &["exec", "delegate it"],
    );
    assert_eq!(code, Some(0), "stdout: {stdout}\nstderr: {stderr}");
    let requests = server.requests.lock().expect("requests");
    let summary: Vec<String> = requests
        .iter()
        .map(|request| {
            let body = request.split("\r\n\r\n").nth(1).unwrap_or("");
            let tail: String = body
                .chars()
                .rev()
                .take(160)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            format!("{} bytes ...{tail}", body.len())
        })
        .collect();
    assert_eq!(
        requests.len(),
        5,
        "parent, three child steps (the cap), parent again:\n{}",
        summary.join("\n---\n")
    );
    let tools_of = |request: &str| -> Vec<&str> {
        [
            "repo_read",
            "repo_search",
            "workspace_read",
            "shell_exec",
            "workspace_write",
        ]
        .into_iter()
        .filter(|tool| request.contains(&format!("\"name\":\"{tool}\"")))
        .collect()
    };
    let child = &requests[1];
    assert!(child.contains("GUARD-MARKER"), "{child}");
    assert_eq!(
        tools_of(child),
        ["repo_read", "workspace_read"],
        "what the definition keeps of the explorer's tools"
    );
    // The rule refused the read of the one file; nothing of it reached the model.
    let after_secret = &requests[2];
    assert!(
        after_secret.contains("repo_read") && after_secret.to_lowercase().contains("denied"),
        "{after_secret}"
    );
    assert!(!after_secret.contains("hunter2"), "{after_secret}");
    // A forced call to the tool that was taken away is refused by name.
    let after_search = &requests[3];
    assert!(
        after_search.contains("tool_not_allowed_for_agent")
            && after_search.contains("repo_search")
            && after_search.contains("guard"),
        "{after_search}"
    );
    // The third step's call ran, then the cap: the parent is told, with what
    // the child did.
    let parent_again = &requests[4];
    assert!(
        parent_again.contains("Stopped at its step limit")
            && parent_again.contains("budget_exhausted")
            && parent_again.contains("repo_read")
            && parent_again.contains("tool_not_allowed_for_agent"),
        "{parent_again}"
    );
}

#[test]
fn binary_exec_runs_as_the_agent_it_names_and_a_resumed_session_keeps_it() {
    // EXT-05 slice 4, end to end: `--agent` narrows the main role (its tools, its
    // rule, its instructions), the session records which agent it is, a resume
    // runs as the recorded agent and says when its definition changed, a flag
    // beats the config's default, and what cannot be run as is refused before
    // any model request.
    let server = spawn_scripted_server(vec![
        (
            200,
            tool_call_body("repo_read", &serde_json::json!({"path": "secret.txt"})),
        ),
        (200, terminal_body("lead done")),
        (200, terminal_body("again done")),
        (200, terminal_body("third done")),
        (200, terminal_body("by config")),
        (200, terminal_body("by flag")),
        // A one-step agent: its model asks for a tool, and the run ends there.
        (
            200,
            tool_call_body("repo_read", &serde_json::json!({"path": "notes.txt"})),
        ),
    ]);
    // The model an agent names is its own server's.
    let review_server = spawn_scripted_server(vec![(200, terminal_body("by the review model"))]);
    let env = TrustedProject::new("bin-primary-agent");
    let config_path = env.home.join("config.toml");
    let base_config = format!(
        "{}\n[model.review]\nprovider = \"openai-compatible\"\nmodel = \"review-model\"\n\
         base_url = \"http://{}/v1\"\napi_key = \"scripted-key\"\n",
        config_doc(&format!("http://{}/v1", server.addr)),
        review_server.addr
    );
    std::fs::write(&config_path, &base_config).expect("write config");
    std::fs::write(env.project.join("secret.txt"), "hunter2\n").expect("secret");
    let defs = env.project.join(".rapidlm/agents");
    std::fs::create_dir_all(&defs).expect("defs dir");
    let lead = |description: &str| {
        format!(
            "---\ndescription: {description}\nbase_role: coder\ntool_classes: read\nmode: primary\n\
             allow_tools: repo_read repo_search task_spawn\nmax_steps: 5\nspawn: explore\n\
             permission:\n  \"repo_read(secret*)\": deny\n---\nLEAD-MARKER\n"
        )
    };
    std::fs::write(defs.join("lead.md"), lead("Lead")).expect("lead");
    std::fs::write(
        defs.join("brief.md"),
        "---\ndescription: Brief\nbase_role: coder\ntool_classes: read\nmode: primary\nmax_steps: 1\n---\n",
    )
    .expect("brief");
    std::fs::write(
        defs.join("rev.md"),
        "---\ndescription: Rev\nbase_role: reviewer\ntool_classes: read\nmode: primary\nmodel: review\n---\nREV-MARKER\n",
    )
    .expect("rev");
    std::fs::write(
        defs.join("mine.md"),
        "---\ndescription: Mine\nbase_role: coder\ntool_classes: read\nmode: both\n---\nMINE-MARKER\n",
    )
    .expect("mine");
    let rapid = |args: &[&str]| run_rapid_args_in(&env.project, &env.home, &config_path, args);
    let requests = || server.requests.lock().expect("requests").clone();
    let tools_of = |request: &str| -> Vec<&'static str> {
        [
            "repo_read",
            "repo_search",
            "workspace_read",
            "shell_exec",
            "workspace_write",
            "task_spawn",
        ]
        .into_iter()
        .filter(|tool| request.contains(&format!("\"name\":\"{tool}\"")))
        .collect()
    };

    // What cannot be run as is refused with nothing sent to the model.
    for (agent, code, said) in [
        (
            "nosuch",
            Some(2),
            "the agents a session can run as: brief, lead, mine, rev",
        ),
        ("explore", Some(2), "a subagent only"),
    ] {
        let (got, stdout, stderr) = rapid(&["exec", "--agent", agent, "go"]);
        assert_eq!(got, code, "{agent}: {stdout}\n{stderr}");
        assert!(
            stderr.contains(said) && stderr.contains("rapid agents list"),
            "{agent}: {stderr}"
        );
    }
    assert!(
        requests().is_empty(),
        "no model request for a refused agent"
    );

    // A new session as the agent.
    let (code, stdout, stderr) = rapid(&["exec", "--agent", "lead", "go"]);
    assert_eq!(code, Some(0), "stdout: {stdout}\nstderr: {stderr}");
    let session = stderr
        .split("session ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .expect("a session id")
        .to_owned();
    let seen = requests();
    assert_eq!(seen.len(), 2, "{seen:?}");
    let first = &seen[0];
    assert!(first.contains("LEAD-MARKER"), "its instructions: {first}");
    assert_eq!(
        tools_of(first),
        ["repo_read", "repo_search", "task_spawn"],
        "the main role's tools, less what the agent leaves out"
    );
    assert!(
        first.contains("\"enum\":[\"explore\"]"),
        "delegation is held to its spawn list: {first}"
    );
    let after_secret = &seen[1];
    assert!(
        after_secret.to_lowercase().contains("denied") && !after_secret.contains("hunter2"),
        "its rule refused the read: {after_secret}"
    );

    let export = |name: &str| -> Vec<serde_json::Value> {
        let out = env.home.join(name);
        let (code, _, stderr) = rapid(&["inspect-export", &session, out.to_str().expect("path")]);
        assert_eq!(code, Some(0), "{stderr}");
        std::fs::read_to_string(&out)
            .expect("export")
            .lines()
            .map(|line| serde_json::from_str(line).expect("line"))
            .filter(|event: &serde_json::Value| event["kind"] == "agent.selected")
            .collect()
    };
    let selected = export("export-1.jsonl");
    assert_eq!(selected.len(), 1, "{selected:?}");
    assert_eq!(selected[0]["payload"]["id"], "lead");
    assert_eq!(selected[0]["payload"]["mode"], "primary");
    let digest = selected[0]["payload"]["definition_digest"]
        .as_str()
        .expect("digest")
        .to_owned();
    assert_eq!(digest.len(), 64);

    // A resume with no flag runs as the recorded agent; nothing changed, so
    // nothing is said and nothing recorded again.
    let (code, _, stderr) = rapid(&["exec", "--continue", "again"]);
    assert_eq!(code, Some(0), "{stderr}");
    assert!(!stderr.contains("has changed"), "{stderr}");
    let seen = requests();
    assert_eq!(seen.len(), 3);
    assert!(seen[2].contains("LEAD-MARKER"), "{}", seen[2]);
    assert_eq!(
        tools_of(&seen[2]),
        ["repo_read", "repo_search", "task_spawn"]
    );
    assert_eq!(export("export-2.jsonl").len(), 1);

    // The definition changed: the current one runs, one line says so, and the
    // new fingerprint is recorded.
    std::fs::write(defs.join("lead.md"), lead("Lead, revised")).expect("lead");
    let (code, _, stderr) = rapid(&["exec", "--continue", "third"]);
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(
        stderr
            .lines()
            .filter(|line| line.contains("has changed since this session recorded it"))
            .count(),
        1,
        "{stderr}"
    );
    let selected = export("export-3.jsonl");
    assert_eq!(selected.len(), 2, "{selected:?}");
    assert_ne!(selected[1]["payload"]["definition_digest"], digest.as_str());
    assert_eq!(requests().len(), 4);

    // A session the ledger has never seen is reported as that, not as one that
    // began without an agent.
    let (code, _, stderr) = rapid(&[
        "exec",
        "--resume",
        "01234567-89ab-7cde-89ab-0123456789ab",
        "--agent",
        "lead",
        "go",
    ]);
    assert_eq!(code, Some(2), "{stderr}");
    assert!(
        stderr.contains("has not been recorded in this project")
            && !stderr.contains("began without an agent"),
        "{stderr}"
    );
    assert_eq!(requests().len(), 4);
    // A session keeps its agent: another is refused, and no request is sent.
    let (code, _, stderr) = rapid(&["exec", "--continue", "--agent", "mine", "x"]);
    assert_eq!(code, Some(2), "{stderr}");
    assert!(
        stderr.contains("runs as agent 'lead'") && stderr.contains("start a new session"),
        "{stderr}"
    );
    assert_eq!(requests().len(), 4);

    // The config's default applies to a new session; the flag beats it.
    std::fs::write(
        &config_path,
        format!("{base_config}\n[agents]\ndefault_primary = \"lead\"\n"),
    )
    .expect("config with a default");
    let (code, _, stderr) = rapid(&["exec", "plain"]);
    assert_eq!(code, Some(0), "{stderr}");
    let seen = requests();
    assert!(seen[4].contains("LEAD-MARKER"), "{}", seen[4]);
    let (code, _, stderr) = rapid(&["exec", "--agent", "mine", "flagged"]);
    assert_eq!(code, Some(0), "{stderr}");
    let seen = requests();
    assert!(
        seen[5].contains("MINE-MARKER") && !seen[5].contains("LEAD-MARKER"),
        "{}",
        seen[5]
    );
    // An agent that names a model runs on that model's server, and no other.
    let (code, _, stderr) = rapid(&["exec", "--agent", "rev", "reviewed"]);
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(
        requests().len(),
        6,
        "the default model's server was not asked"
    );
    let review_seen = review_server.requests.lock().expect("requests").clone();
    assert_eq!(review_seen.len(), 1);
    assert!(
        review_seen[0].contains("REV-MARKER") && review_seen[0].contains("review-model"),
        "{}",
        review_seen[0]
    );
    // `--agent` before a command is the terminal UI's flag: a command takes its own.
    let (code, _, stderr) = rapid(&["--agent", "lead", "exec", "go"]);
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("put it after the command"), "{stderr}");
    assert_eq!(requests().len(), 6);
    // An agent's step cap bounds the run: one step, then it stops, failed, with
    // nothing further asked of the model.
    let before = requests().len();
    let (code, stdout, stderr) = rapid(&["exec", "--agent", "brief", "look"]);
    assert_ne!(code, Some(0), "{stdout}\n{stderr}");
    assert!(
        stdout.contains("budget_exhausted") || stderr.contains("budget_exhausted"),
        "{stdout}\n{stderr}"
    );
    assert_eq!(requests().len(), before + 1, "one model step, no more");
    // And a default that names no agent is refused, not ignored.
    std::fs::write(
        &config_path,
        format!("{base_config}\n[agents]\ndefault_primary = \"ghost\"\n"),
    )
    .expect("config with a bad default");
    let (code, _, stderr) = rapid(&["exec", "plain again"]);
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("no agent 'ghost'"), "{stderr}");
    assert_eq!(requests().len(), before + 1);
}

#[test]
fn binary_a_definitions_rules_that_do_not_fit_beside_the_sessions_start_no_child() {
    // The session holds as many permission rules as it may; a definition that
    // adds one more is refused when its child would start, naming the numbers —
    // never trimmed to fit, which could have dropped its `deny`.
    let server = spawn_scripted_server(vec![
        (200, spawn_call_body("guard")),
        (200, terminal_body("parent done")),
    ]);
    let env = TrustedProject::new("bin-agent-rules-full");
    let config_path = env.home.join("config.toml");
    std::fs::write(
        &config_path,
        config_doc(&format!("http://{}/v1", server.addr)),
    )
    .expect("write config");
    let deny: Vec<String> = (0..128).map(|n| format!("shell_exec(rule-{n}*)")).collect();
    std::fs::write(
        env.project.join(".rapidlm/settings.json"),
        serde_json::json!({"permissions": {"deny": deny}}).to_string(),
    )
    .expect("settings");
    let defs = env.project.join(".rapidlm/agents");
    std::fs::create_dir_all(&defs).expect("defs dir");
    std::fs::write(
        defs.join("guard.md"),
        "---\ndescription: reads\nbase_role: explorer\ntool_classes: read\n\
         permission:\n  \"repo_read(secret*)\": deny\n---\nGUARD-MARKER\n",
    )
    .expect("definition");
    let (code, stdout, stderr) = run_rapid_args_in(
        &env.project,
        &env.home,
        &config_path,
        &["exec", "--allow", "task_spawn", "delegate it"],
    );
    assert_eq!(code, Some(0), "stdout: {stdout}\nstderr: {stderr}");
    let requests = server.requests.lock().expect("requests");
    assert_eq!(
        requests.len(),
        2,
        "the parent asked, the child never did, the parent was told"
    );
    assert!(
        requests[1].contains("adds 1 permission rule(s) to the 128 this session already holds"),
        "{}",
        requests[1]
    );
}

/// A stdio MCP server with two tools, that logs each call it is made.
#[cfg(unix)]
const MCP_STUB: &str = r#"#!/usr/bin/env python3
import sys, json, os
def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    method = req.get("method")
    rid = req.get("id")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": rid, "result": {
            "protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
            "serverInfo": {"name": "demo", "version": "1.0"}}})
    elif method == "tools/list":
        tools = [{"name": n, "description": "does " + n,
                  "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}}}}
                 for n in ["get_issue", "create_issue"]]
        send({"jsonrpc": "2.0", "id": rid, "result": {"tools": tools}})
    elif method == "tools/call":
        name = req.get("params", {}).get("name", "")
        with open(os.environ["CALL_LOG"], "a") as log:
            log.write(name + "\n")
        send({"jsonrpc": "2.0", "id": rid, "result": {"content": [
            {"type": "text", "text": "called " + name}]}})
"#;

#[test]
fn binary_a_read_only_main_agents_children_are_held_to_what_it_is() {
    // A definition narrows the agent that is run as it; the children it starts
    // are held to the same, or `tool_classes: read` would leave the way to a
    // child that writes.
    let server = spawn_scripted_server(vec![
        (200, spawn_call_body("general-purpose")),
        (200, terminal_body("the child looked")),
        (200, terminal_body("parent done")),
    ]);
    let env = TrustedProject::new("bin-agent-bound");
    let config_path = env.home.join("config.toml");
    std::fs::write(
        &config_path,
        config_doc(&format!("http://{}/v1", server.addr)),
    )
    .expect("write config");
    std::fs::write(
        env.project.join(".rapidlm/settings.json"),
        r#"{"permissions": {"allow": ["task_spawn"]}}"#,
    )
    .expect("settings");
    let defs = env.project.join(".rapidlm/agents");
    std::fs::create_dir_all(&defs).expect("defs dir");
    std::fs::write(
        defs.join("reader.md"),
        "---\ndescription: Reader\nbase_role: coder\ntool_classes: read\nmode: primary\n\
         deny_tools: repo_glob\n---\nREADER-MARKER\n",
    )
    .expect("reader");
    let (code, stdout, stderr) = run_rapid_args_in(
        &env.project,
        &env.home,
        &config_path,
        &["exec", "--agent", "reader", "delegate it"],
    );
    assert_eq!(code, Some(0), "stdout: {stdout}\nstderr: {stderr}");
    let seen = server.requests.lock().expect("requests").clone();
    assert_eq!(seen.len(), 3, "{seen:?}");
    let tools_of = |request: &str| -> Vec<&'static str> {
        [
            "repo_read",
            "repo_glob",
            "workspace_write",
            "shell_exec",
            "workspace_patch",
        ]
        .into_iter()
        .filter(|tool| request.contains(&format!("\"name\":\"{tool}\"")))
        .collect()
    };
    assert_eq!(tools_of(&seen[0]), ["repo_read"], "the agent: reads only");
    // The child is a general-purpose one, which has every tool of its own —
    // under this agent, it has the agent's: no write, no command, and not the
    // read the agent's own `deny_tools` takes away.
    assert!(
        !seen[1].contains("READER-MARKER"),
        "it is the child, not the agent"
    );
    assert_eq!(tools_of(&seen[1]), ["repo_read"], "{}", seen[1]);
}

#[test]
fn binary_a_prompt_a_hook_blocks_still_leaves_a_session_that_resumes_as_its_agent() {
    // The session says which agent it is as soon as it exists, not once a turn
    // has started: a prompt a hook blocks leaves a session behind, and the next
    // `--continue` picks it, so it must not come back as the plain main role.
    let server = spawn_scripted_server(vec![(200, terminal_body("resumed as the agent"))]);
    let env = TrustedProject::new("bin-agent-blocked");
    let config_path = env.home.join("config.toml");
    std::fs::write(
        &config_path,
        config_doc(&format!("http://{}/v1", server.addr)),
    )
    .expect("write config");
    let defs = env.project.join(".rapidlm/agents");
    std::fs::create_dir_all(&defs).expect("defs dir");
    std::fs::write(
        defs.join("lead.md"),
        "---\ndescription: Lead\nbase_role: coder\ntool_classes: read\nmode: primary\n---\nLEAD-MARKER\n",
    )
    .expect("lead");
    let hook = env.project.join("block.sh");
    std::fs::write(
        &hook,
        "echo '{\"decision\":\"deny\",\"reason\":\"blocked for the test\"}'\nexit 0\n",
    )
    .expect("hook");
    let settings = env.project.join(".rapidlm/settings.json");
    std::fs::write(
        &settings,
        serde_json::json!({"hooks": {"user_prompt_submit": [format!("sh {}", hook.display())]}})
            .to_string(),
    )
    .expect("settings");
    let (code, stdout, stderr) = run_rapid_args_in(
        &env.project,
        &env.home,
        &config_path,
        &["exec", "--agent", "lead", "go"],
    );
    assert_eq!(code, Some(3), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stderr.contains("blocked for the test"), "{stderr}");
    assert!(
        server.requests.lock().expect("requests").is_empty(),
        "the prompt never reached the model"
    );
    std::fs::write(&settings, "{}").expect("settings without the hook");
    let (code, stdout, stderr) = run_rapid_args_in(
        &env.project,
        &env.home,
        &config_path,
        &["exec", "--continue", "go on"],
    );
    assert_eq!(code, Some(0), "stdout: {stdout}\nstderr: {stderr}");
    let seen = server.requests.lock().expect("requests").clone();
    assert_eq!(seen.len(), 1);
    assert!(
        seen[0].contains("LEAD-MARKER"),
        "it resumed as the agent: {}",
        seen[0]
    );
}

#[cfg(unix)]
#[test]
fn binary_a_child_gets_the_mcp_tools_its_definition_names_and_a_child_that_names_none_gets_none() {
    // EXT-05 slice 5, end to end: the session's MCP server is shared with a
    // child only for the tools the child's definition names; the child calls
    // one through the same server, and a child that names none is offered none.
    let env = TrustedProject::new("bin-child-mcp");
    let script = env.project.join("mcp-stub.py");
    std::fs::write(&script, MCP_STUB).expect("stub");
    let log = env.project.join("mcp-calls.log");
    std::fs::write(
        env.project.join(".rapidlm/settings.json"),
        serde_json::json!({
            "permissions": {"allow": ["task_spawn", "mcp__demo__get_issue"]},
            "mcpServers": {"demo": {
                "command": "python3",
                "args": [script.display().to_string()],
                "env": {"CALL_LOG": log.display().to_string()}
            }}
        })
        .to_string(),
    )
    .expect("settings");
    let defs = env.project.join(".rapidlm/agents");
    std::fs::create_dir_all(&defs).expect("defs dir");
    std::fs::write(
        defs.join("docs.md"),
        "---\ndescription: reads issues\nbase_role: explorer\ntool_classes: read mcp\n\
         allow_tools: repo_read mcp__demo__get_issue\n---\nDOCS-MARKER\n",
    )
    .expect("docs");
    std::fs::write(
        defs.join("plain.md"),
        "---\ndescription: reads files\nbase_role: explorer\ntool_classes: read mcp\n---\nPLAIN-MARKER\n",
    )
    .expect("plain");
    let tools_of = |request: &str| -> Vec<&'static str> {
        [
            "repo_read",
            "mcp__demo__get_issue",
            "mcp__demo__create_issue",
        ]
        .into_iter()
        .filter(|tool| request.contains(&format!("\"name\":\"{tool}\"")))
        .collect()
    };

    let validate_config = env.home.join("validate.toml");
    std::fs::write(&validate_config, "").expect("config");
    let (code, stdout, stderr) = run_rapid_args_in(
        &env.project,
        &env.home,
        &validate_config,
        &["agents", "validate"],
    );
    assert_eq!(code, Some(0), "stdout: {stdout}\nstderr: {stderr}");
    // A child that names a tool is offered that tool, and calls it.
    let server = spawn_scripted_server(vec![
        (200, spawn_call_body("docs")),
        (
            200,
            tool_call_body("mcp__demo__get_issue", &serde_json::json!({"id": "7"})),
        ),
        (200, terminal_body("the child read issue 7")),
        (200, terminal_body("parent done")),
    ]);
    let config_path = env.home.join("config.toml");
    std::fs::write(
        &config_path,
        config_doc(&format!("http://{}/v1", server.addr)),
    )
    .expect("write config");
    let (code, stdout, stderr) = run_rapid_args_in(
        &env.project,
        &env.home,
        &config_path,
        &["exec", "delegate it"],
    );
    assert_eq!(code, Some(0), "stdout: {stdout}\nstderr: {stderr}");
    let seen = server.requests.lock().expect("requests").clone();
    assert_eq!(seen.len(), 4, "{seen:?}");
    assert!(
        tools_of(&seen[0]).contains(&"mcp__demo__create_issue"),
        "the parent has them all: {}",
        seen[0]
    );
    assert!(seen[1].contains("DOCS-MARKER"), "{}", seen[1]);
    assert_eq!(
        tools_of(&seen[1]),
        ["repo_read", "mcp__demo__get_issue"],
        "the child's tools: what it names, and no other MCP tool"
    );
    assert!(
        seen[2].contains("called get_issue"),
        "the call went through the session's server: {}",
        seen[2]
    );
    assert_eq!(
        std::fs::read_to_string(&log).expect("call log"),
        "get_issue\n"
    );

    // A child that names none gets none, though its classes would allow them.
    let server = spawn_scripted_server(vec![
        (200, spawn_call_body("plain")),
        (200, terminal_body("the child read files")),
        (200, terminal_body("parent done")),
    ]);
    std::fs::write(
        &config_path,
        config_doc(&format!("http://{}/v1", server.addr)),
    )
    .expect("write config");
    let (code, stdout, stderr) = run_rapid_args_in(
        &env.project,
        &env.home,
        &config_path,
        &["exec", "delegate it"],
    );
    assert_eq!(code, Some(0), "stdout: {stdout}\nstderr: {stderr}");
    let seen = server.requests.lock().expect("requests").clone();
    assert_eq!(seen.len(), 3, "{seen:?}");
    assert!(seen[1].contains("PLAIN-MARKER"), "{}", seen[1]);
    assert!(
        !seen[1].contains("mcp__demo__"),
        "a child gets no MCP tool it does not name: {}",
        seen[1]
    );
}

#[test]
fn binary_agents_help_is_the_commands_own_usage() {
    let env = TrustedProject::new("bin-agents-help");
    let config_path = env.home.join("config.toml");
    std::fs::write(&config_path, "").expect("config");
    for flag in ["--help", "-h"] {
        let (code, stdout, stderr) =
            run_rapid_args_in(&env.project, &env.home, &config_path, &["agents", flag]);
        assert_eq!(code, Some(0), "stderr: {stderr}");
        assert!(
            stdout.starts_with("usage: rapid agents list [--dir <path>]")
                && stdout.contains("scaffold <id> [--format md|toml]"),
            "{stdout}"
        );
    }
}

#[test]
fn binary_a_child_missing_a_declared_output_is_an_integration_failure() {
    // ADR 0023 §6: a type declares `verdict`; its child answers without
    // one. The parent is told it failed to integrate, naming the output —
    // never handed the report as a success.
    let server = spawn_scripted_server(vec![
        (200, spawn_call_body("judge")),
        (200, terminal_body("looked at it")),
        (200, terminal_body("noted")),
    ]);
    let env = TrustedProject::new("bin-declared-outputs");
    let config_path = env.home.join("config.toml");
    std::fs::write(
        &config_path,
        config_doc(&format!("http://{}/v1", server.addr)),
    )
    .expect("write config");
    std::fs::write(
        env.project.join(".rapidlm/settings.json"),
        r#"{"permissions": {"allow": ["task_spawn"]}}"#,
    )
    .expect("settings");
    let defs = env.project.join(".rapidlm/agents");
    std::fs::create_dir_all(&defs).expect("defs dir");
    std::fs::write(
        defs.join("judge.toml"),
        "schema = \"rapidlm.agent_defs.v1\"\n[agent]\nid = \"judge\"\n\
         description = \"judges notes\"\nbase_role = \"explorer\"\ntools = [\"read\"]\n\
         outputs = [\"verdict\"]\n",
    )
    .expect("definition");
    let (code, stdout, stderr) =
        run_rapid_args_in(&env.project, &env.home, &config_path, &["exec", "judge it"]);
    assert_eq!(code, Some(0), "stdout: {stdout}\nstderr: {stderr}");
    let requests = server.requests.lock().expect("requests");
    assert_eq!(requests.len(), 3, "parent, child, parent");
    // The child was told what to end with.
    assert!(
        requests[1].contains("## Declared outputs") && requests[1].contains("verdict"),
        "{}",
        requests[1]
    );
    // The parent hears a failure naming the missing output.
    assert!(
        requests[2].contains("integration_failed")
            && requests[2].contains("missing declared output: verdict"),
        "{}",
        requests[2]
    );
}

/// SSE chat-completions stream proposing one `plan_exit` with a proposal.
fn plan_exit_call_body() -> String {
    let arguments = serde_json::json!({
        "title": "Split the parser",
        "summary": "tokenising moves out",
        "steps": [{"key": "edit", "kind": "agent", "label": "split", "prompt": "split it"}]
    })
    .to_string();
    let call = serde_json::json!({
        "choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
            "function": {"name": "plan_exit", "arguments": arguments}}]},
            "finish_reason": "tool_calls"}]
    });
    format!("data: {call}\n\ndata: [DONE]\n\n")
}

#[test]
fn binary_exec_plan_prints_the_proposal_and_exits_needs_approval() {
    // ADR 0024 §3: a headless plan-mode run submits its proposal, prints
    // it, records the wait, and exits NeedsApproval (10).
    let server = spawn_scripted_server(vec![(200, plan_exit_call_body())]);
    let env = TrustedProject::new("bin-exec-plan");
    let config_path = env.home.join("config.toml");
    std::fs::write(
        &config_path,
        config_doc(&format!("http://{}/v1", server.addr)),
    )
    .expect("write config");
    let (code, stdout, stderr) = run_rapid_args_in(
        &env.project,
        &env.home,
        &config_path,
        &["exec", "--plan", "plan it"],
    );
    assert_eq!(code, Some(10), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("# Split the parser"), "{stdout}");
    assert!(stderr.contains("awaits a human decision"), "{stderr}");
    let plans = std::fs::read_dir(env.project.join(".rapidlm/plans"))
        .expect("plans dir")
        .count();
    assert_eq!(plans, 1, "the revision's markdown file");
}

#[test]
fn binary_a_headless_session_records_its_origin_and_sessions_list_filters_by_it() {
    let server = spawn_scripted_server(vec![(200, terminal_body("hello"))]);
    let env = TrustedProject::new("bin-origin");
    let config_path = env.home.join("config.toml");
    std::fs::write(
        &config_path,
        config_doc(&format!("http://{}/v1", server.addr)),
    )
    .expect("write config");
    let (code, _, stderr) = run_rapid_in(&env.project, &env.home, &config_path, None);
    assert_eq!(code, Some(0), "{stderr}");
    let list = |extra: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_rapid"))
            .arg("sessions")
            .arg("list")
            .args(extra)
            .current_dir(&env.project)
            .env("HOME", &env.home)
            .env_remove("RAPIDLM_HOME")
            .output()
            .expect("sessions");
        assert_eq!(
            out.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let all = list(&[]);
    assert!(
        all.contains("count=1") && all.contains("origin=headless"),
        "{all}"
    );
    assert!(list(&["--origin", "headless"]).contains("count=1"));
    assert!(list(&["--origin", "interactive"]).contains("count=0"));
    // A value that names no surface is a usage error, not an empty list.
    let bad = Command::new(env!("CARGO_BIN_EXE_rapid"))
        .args(["sessions", "list", "--origin", "tui"])
        .current_dir(&env.project)
        .env("HOME", &env.home)
        .output()
        .expect("sessions");
    assert_eq!(bad.status.code(), Some(2));
}

#[cfg(unix)]
fn tool_call_body(name: &str, arguments: &serde_json::Value) -> String {
    let call = serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1",
        "type":"function","function":{"name":name,"arguments":arguments.to_string()}}]},
        "finish_reason":"tool_calls"}]});
    format!("data: {call}\n\ndata: [DONE]\n\n")
}

#[cfg(unix)]
#[test]
fn binary_a_tools_command_and_a_hook_carry_the_session_and_turn_ids() {
    let server = spawn_scripted_server(vec![
        (
            200,
            tool_call_body(
                "shell_exec",
                &serde_json::json!({"argv": ["sh", "-c",
                    "printf '%s %s' \"$RAPIDLM_SESSION_ID\" \"$RAPIDLM_TURN_ID\" > tool-ids.txt"]}),
            ),
        ),
        (200, terminal_body("done")),
    ]);
    let env = TrustedProject::new("bin-run-ids");
    let hook = env.project.join("hook.sh");
    std::fs::write(
        &hook,
        "printf '%s %s' \"$RAPIDLM_SESSION_ID\" \"$RAPIDLM_TURN_ID\" > hook-ids.txt\nexit 0\n",
    )
    .expect("hook");
    std::fs::write(
        env.project.join(".rapidlm/settings.json"),
        serde_json::json!({"hooks": {"pre_tool_use": [format!("sh {}", hook.display())]}})
            .to_string(),
    )
    .expect("settings");
    let config_path = env.home.join("config.toml");
    std::fs::write(
        &config_path,
        config_doc(&format!("http://{}/v1", server.addr)),
    )
    .expect("write config");
    let (code, _, stderr) = run_rapid_in(
        &env.project,
        &env.home,
        &config_path,
        Some("bypassPermissions"),
    );
    assert_eq!(code, Some(0), "{stderr}");
    let sessions = Command::new(env!("CARGO_BIN_EXE_rapid"))
        .args(["sessions", "list"])
        .current_dir(&env.project)
        .env("HOME", &env.home)
        .output()
        .expect("sessions");
    let listed = String::from_utf8_lossy(&sessions.stdout).into_owned();
    let session = listed
        .split_whitespace()
        .find_map(|word| word.strip_prefix("session="))
        .expect("one session")
        .to_owned();
    for file in ["tool-ids.txt", "hook-ids.txt"] {
        let ids = std::fs::read_to_string(env.project.join(file)).expect(file);
        let mut parts = ids.split(' ');
        assert_eq!(parts.next(), Some(session.as_str()), "{file}: {ids}");
        let turn = parts.next().unwrap_or_default();
        assert!(turn.len() >= 32, "{file}: no turn id in {ids:?}");
    }
}

/// SEAM-06-1: with trusted keys configured, a managed policy that is unsigned,
/// edited after signing, or signed by another key refuses the run before any
/// model is asked; a good signature lets it proceed under the policy.
#[test]
fn binary_exec_refuses_an_unverified_managed_policy_before_any_model_request() {
    use ring::signature::{Ed25519KeyPair, KeyPair};
    let hex = |bytes: &[u8]| -> String { bytes.iter().map(|b| format!("{b:02x}")).collect() };
    let server = spawn_scripted_server(vec![
        (200, read_notes_tool_call_body()),
        (200, patch_tool_call_body()),
        (200, terminal_body("done")),
    ]);
    let env = TrustedProject::new("managed-signature");
    let config_path = env.home.join("config.toml");
    std::fs::write(
        &config_path,
        config_doc(&format!("http://{}/v1", server.addr)),
    )
    .expect("write config");
    let admin = Ed25519KeyPair::from_seed_unchecked(&[7; 32]).expect("key");
    let stranger = Ed25519KeyPair::from_seed_unchecked(&[8; 32]).expect("key");
    let keys = env.home.join("trusted.keys");
    std::fs::write(
        &keys,
        format!("ed25519:{}\n", hex(admin.public_key().as_ref())),
    )
    .expect("keys");
    let policy = env.home.join("managed.toml");
    let doc = "schema = \"rapidlm.managed_config.v1\"\n[policy]\n";
    std::fs::write(&policy, doc).expect("policy");
    let sig = env.home.join("managed.toml.sig");
    let run = || {
        exec_patch(
            &env,
            &config_path,
            &["--allow=workspace_patch(notes.txt)"],
            &[
                ("RAPIDLM_MANAGED_CONFIG", policy.as_os_str()),
                ("RAPIDLM_MANAGED_TRUSTED_KEYS", keys.as_os_str()),
            ],
        )
    };
    let assert_refused = |tag: &str| {
        let out = run();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_ne!(out.status.code(), Some(0), "{tag}: {stderr}");
        assert!(
            stderr.contains("managed policy signature check failed")
                && stderr.contains("remediation:"),
            "{tag}: {stderr}"
        );
        assert_eq!(notes(&env), "alpha\n", "{tag}");
        assert!(
            server.requests.lock().expect("lock").is_empty(),
            "{tag}: a model was asked"
        );
    };
    // Unsigned.
    assert_refused("unsigned");
    // Signed by a key that is not trusted.
    std::fs::write(
        &sig,
        format!("ed25519:{}\n", hex(stranger.sign(doc.as_bytes()).as_ref())),
    )
    .expect("sig");
    assert_refused("foreign");
    // Signed, then edited.
    std::fs::write(
        &sig,
        format!("ed25519:{}\n", hex(admin.sign(doc.as_bytes()).as_ref())),
    )
    .expect("sig");
    std::fs::write(&policy, format!("{doc}# loosened\n")).expect("tamper");
    assert_refused("tampered");
    // Restored: the good signature lets the run proceed.
    std::fs::write(&policy, doc).expect("restore");
    let out = run();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(notes(&env), "beta\n");
}

#[test]
fn responses_encrypted_reasoning_survives_a_fresh_model_and_serialized_tool_history() {
    assert_responses_private_restart(false);
}
#[test]
fn azure_responses_private_replay_survives_a_fresh_model_and_serialized_tool_history() {
    assert_responses_private_restart(true);
}
fn assert_responses_private_restart(azure: bool) {
    let first = serde_json::json!({"status":"completed", "output":[
        {"type":"reasoning","id":"rs-private","encrypted_content":"opaque-restart-canary"},
        {"type":"function_call","id":"fc1","call_id":"read1","name":"repo_read","arguments":"{\"path\":\"a.txt\"}"}
    ],"usage":{"input_tokens":3,"output_tokens":5}}).to_string();
    let last = serde_json::json!({"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"continued safely"}]}],"usage":{"input_tokens":4,"output_tokens":2}}).to_string();
    let server = spawn_scripted_server(vec![(200, first), (200, last)]);
    let doc = if azure {
        azure_doc("azure-responses", server.addr, "v1", "api-key")
    } else {
        config_doc(&format!("http://{}/v1", server.addr))
            .replace("openai-compatible", "openai-responses")
    };
    let mut model = scripted_model(&doc);
    let preserved = PreservedLiveContext::new("read a.txt", Vec::new(), "", "", 1024, 64).unwrap();
    let packet = build_packet(&preserved, None).unwrap();
    let cancel = CancellationToken::new();
    let first = model
        .step(
            packet.blocks(),
            &agent_runtime::ModelStepInput::without_tools(1),
            &cancel,
        )
        .unwrap();
    let ModelStepOutput::ToolCalls { calls, .. } = first else {
        panic!("expected call")
    };
    let replay = model.take_step_replay().expect("private replay");
    assert!(model.take_step_replay().is_none());
    let history = vec![
        agent_runtime::ToolStepExchange::new(
            calls,
            vec![agent_runtime::ToolStepResult::Succeeded {
                call_id: "read1".to_owned(),
                summary: "a.txt contents".to_owned(),
            }],
        )
        .with_provider_replay(Some(replay)),
    ];
    let raw =
        serde_json::to_string(&rapid::approvals::SuspendedHistory::from_agent(&history)).unwrap();
    drop(model);
    let restored: rapid::approvals::SuspendedHistory = serde_json::from_str(&raw).unwrap();
    let history = restored.to_agent();
    let mut restarted = scripted_model(&doc);
    let output = restarted
        .step(
            packet.blocks(),
            &agent_runtime::ModelStepInput::with_history(2, &history, &[]),
            &cancel,
        )
        .unwrap();
    assert!(matches!(output, ModelStepOutput::Terminal { text, .. } if text == "continued safely"));
    assert!(restarted.take_step_replay().is_none());
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let body: serde_json::Value =
        serde_json::from_str(requests[1].split_once("\r\n\r\n").unwrap().1).unwrap();
    let input = body["input"].as_array().unwrap();
    let at = input
        .iter()
        .position(|item| item["type"] == "reasoning")
        .unwrap();
    assert_eq!(input[at]["encrypted_content"], "opaque-restart-canary");
    assert_eq!(input[at + 1]["type"], "function_call");
    assert_eq!(input[at + 2]["type"], "function_call_output");
    assert!(
        !input
            .iter()
            .filter(|item| item["type"] == "message")
            .any(|item| item.to_string().contains("opaque-restart-canary"))
    );
}

#[test]
fn anthropic_private_thinking_replays_after_restart_and_explicit_controls_reach_wire() {
    let first = serde_json::json!({"type":"message","role":"assistant","content":[
        {"type":"thinking","thinking":"private-thinking-canary","signature":"signed-thinking-canary"},
        {"type":"redacted_thinking","data":"redacted-thinking-canary"},
        {"type":"tool_use","id":"read1","name":"repo_read","input":{"path":"a.txt"}}
    ],"stop_reason":"tool_use","usage":{"input_tokens":3,"output_tokens":5,"cache_read_input_tokens":2}}).to_string();
    let last = serde_json::json!({"type":"message","role":"assistant","content":[{"type":"text","text":"continued safely"}],"stop_reason":"end_turn","usage":{"input_tokens":4,"output_tokens":2}}).to_string();
    let server = spawn_scripted_server(vec![(200, first), (200, last)]);
    let doc = format!(
        "{}reasoning = true\ncaching = true\nthinking = \"adaptive\"\nreasoning_effort = \"high\"\n",
        config_doc(&format!("http://{}/v1", server.addr)).replace("openai-compatible", "anthropic")
    );
    let mut model = scripted_model(&doc);
    let preserved = PreservedLiveContext::new("read a.txt", Vec::new(), "", "", 1024, 64).unwrap();
    let packet = build_packet(&preserved, None).unwrap();
    let cancel = CancellationToken::new();
    let first = model
        .step(
            packet.blocks(),
            &agent_runtime::ModelStepInput::without_tools(1),
            &cancel,
        )
        .unwrap();
    let ModelStepOutput::ToolCalls { calls, .. } = first else {
        panic!("expected call")
    };
    let state = model.take_step_replay().unwrap();
    assert!(!format!("{state:?}").contains("private-thinking-canary"));
    let history = vec![
        agent_runtime::ToolStepExchange::new(
            calls,
            vec![agent_runtime::ToolStepResult::Succeeded {
                call_id: "read1".to_owned(),
                summary: "a.txt contents".to_owned(),
            }],
        )
        .with_provider_replay(Some(state)),
    ];
    let raw =
        serde_json::to_string(&rapid::approvals::SuspendedHistory::from_agent(&history)).unwrap();
    drop(model);
    let restored: rapid::approvals::SuspendedHistory = serde_json::from_str(&raw).unwrap();
    let history = restored.to_agent();
    let mut restarted = scripted_model(&doc);
    let output = restarted
        .step(
            packet.blocks(),
            &agent_runtime::ModelStepInput::with_history(2, &history, &[]),
            &cancel,
        )
        .unwrap();
    assert!(matches!(output, ModelStepOutput::Terminal { text, .. } if text == "continued safely"));
    let requests = server.requests.lock().unwrap();
    for request in requests.iter() {
        let body: serde_json::Value =
            serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["cache_control"]["type"], "ephemeral");
        assert_eq!(body["output_config"]["effort"], "high");
    }
    let body: serde_json::Value =
        serde_json::from_str(requests[1].split_once("\r\n\r\n").unwrap().1).unwrap();
    let assistant = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "assistant")
        .unwrap();
    assert_eq!(
        assistant["content"][0]["thinking"],
        "private-thinking-canary"
    );
    assert_eq!(
        assistant["content"][0]["signature"],
        "signed-thinking-canary"
    );
    assert_eq!(assistant["content"][1]["type"], "redacted_thinking");
    assert_eq!(assistant["content"][2]["type"], "tool_use");
}

fn google_body(parts: serde_json::Value) -> String {
    serde_json::json!({"candidates":[{"index":0,"content":{"role":"model","parts":parts},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":5,"thoughtsTokenCount":2,"totalTokenCount":10}}).to_string()
}
#[test]
fn binary_exec_selects_native_google_key_header_and_replays_signed_parallel_calls() {
    let first_parts = serde_json::json!([
        {"text":"private-google-canary","thought":true,"thoughtSignature":"private-signature"},
        {"functionCall":{"name":"repo_read","args":{"path":"a.txt"}},"thoughtSignature":"first-call-signature"},
        {"functionCall":{"id":"native-second","name":"repo_read","args":{"path":"b.txt"}}},
        {"thoughtSignature":"empty-tail-signature"}
    ]);
    let server = spawn_scripted_server(vec![
        (200, google_body(first_parts.clone())),
        (
            200,
            google_body(serde_json::json!([{"text":"native Google complete"}])),
        ),
    ]);
    let dir = temp_dir("google-native-protocol");
    let path = dir.join("config.toml");
    let project = exec_project(&dir);
    std::fs::create_dir_all(project.join(".rapidlm")).unwrap();
    let identity =
        kernel::ProjectIdentity::new(protocol::host_path::canonicalize(&project).unwrap(), None)
            .unwrap();
    kernel::ProjectTrustStore::open(dir.join(".rapidlm/project-trust.json"))
        .set(
            &identity,
            kernel::TrustStatus::Trusted,
            &kernel::CancellationToken::new(),
        )
        .unwrap();
    std::fs::write(project.join("a.txt"), "first file").unwrap();
    std::fs::write(project.join("b.txt"), "second file").unwrap();
    let doc = config_doc(&format!("http://{}/v1beta", server.addr))
        .replace("openai-compatible", "google-genai");
    std::fs::write(&path, doc).unwrap();
    let (code, stdout, stderr) = run_rapid(Some(&path), None, &dir);
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(stdout.trim(), "native Google complete");
    assert!(!format!("{stdout}{stderr}").contains("private-google-canary"));
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests.iter() {
        assert!(
            request.starts_with("POST /v1beta/models/test-model:streamGenerateContent?alt=sse ")
        );
        let headers = request
            .split_once("\r\n\r\n")
            .unwrap()
            .0
            .to_ascii_lowercase();
        assert!(headers.contains("x-goog-api-key: scripted-key"));
        assert!(!headers.contains("authorization:"));
        assert!(!request.lines().next().unwrap().contains("scripted-key"));
    }
    let body: serde_json::Value =
        serde_json::from_str(requests[1].split_once("\r\n\r\n").unwrap().1).unwrap();
    let contents = body["contents"].as_array().unwrap();
    let at = contents
        .iter()
        .position(|content| content["role"] == "model")
        .unwrap();
    assert_eq!(contents[at]["parts"], first_parts);
    let responses = &contents[at + 1]["parts"];
    assert_eq!(responses.as_array().unwrap().len(), 2);
    assert!(responses[0]["functionResponse"].get("id").is_none());
    assert_eq!(responses[1]["functionResponse"]["id"], "native-second");
    assert!(
        responses[0]["functionResponse"]["response"]["output"]
            .as_str()
            .unwrap()
            .contains("first file")
    );
    assert!(
        responses[1]["functionResponse"]["response"]["output"]
            .as_str()
            .unwrap()
            .contains("second file")
    );
    drop(requests);
    let _ = std::fs::remove_dir_all(dir);
}
#[test]
fn native_google_replays_signed_parts_after_serialization_and_a_fresh_model() {
    let parts = serde_json::json!([
        {"text":"private-restart","thought":true,"thoughtSignature":"restart-signature"},
        {"functionCall":{"name":"repo_read","args":{"path":"a.txt"}},"thoughtSignature":"restart-call-signature"},
        {"text":"","thoughtSignature":"restart-tail-signature"}
    ]);
    let server = spawn_scripted_server(vec![
        (200, google_body(parts.clone())),
        (
            200,
            google_body(serde_json::json!([{"text":"restarted Google"}])),
        ),
    ]);
    let doc = config_doc(&format!("http://{}/v1beta", server.addr))
        .replace("openai-compatible", "google-genai");
    let preserved = PreservedLiveContext::new("read a.txt", Vec::new(), "", "", 1024, 64).unwrap();
    let packet = build_packet(&preserved, None).unwrap();
    let cancel = CancellationToken::new();
    let mut model = scripted_model(&doc);
    let first = model
        .step(
            packet.blocks(),
            &agent_runtime::ModelStepInput::without_tools(1),
            &cancel,
        )
        .unwrap();
    let ModelStepOutput::ToolCalls { calls, .. } = first else {
        panic!("expected tool call")
    };
    let id = calls[0].call_id().to_owned();
    let replay = model.take_step_replay().unwrap();
    let history = vec![
        agent_runtime::ToolStepExchange::new(
            calls,
            vec![agent_runtime::ToolStepResult::Succeeded {
                call_id: id,
                summary: "read result".to_owned(),
            }],
        )
        .with_provider_replay(Some(replay)),
    ];
    let raw =
        serde_json::to_string(&rapid::approvals::SuspendedHistory::from_agent(&history)).unwrap();
    drop(model);
    let restored: rapid::approvals::SuspendedHistory = serde_json::from_str(&raw).unwrap();
    let history = restored.to_agent();
    let mut restarted = scripted_model(&doc);
    let output = restarted
        .step(
            packet.blocks(),
            &agent_runtime::ModelStepInput::with_history(2, &history, &[]),
            &cancel,
        )
        .unwrap();
    assert!(matches!(output,ModelStepOutput::Terminal {text,..} if text=="restarted Google"));
    let requests = server.requests.lock().unwrap();
    let body: serde_json::Value =
        serde_json::from_str(requests[1].split_once("\r\n\r\n").unwrap().1).unwrap();
    let assistant = body["contents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["role"] == "model")
        .unwrap();
    assert_eq!(assistant["parts"], parts);
}

#[test]
fn native_google_streams_before_completion_over_chunked_http_and_hides_thoughts() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (seen, delivered) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(3)))
            .unwrap();
        let request = read_request(&mut socket);
        assert!(request.contains("x-goog-api-key: scripted-key"));
        assert!(!request.contains("Authorization:"));
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").unwrap();
        let hidden = format!(
            "data: {}\r\n\r\n",
            serde_json::json!({"candidates":[{"content":{"role":"model","parts":[{"text":"hidden-thought-canary","thought":true,"thoughtSignature":"hidden-sig"}]}}]})
        );
        let visible = format!(
            "data: {}\r\n\r\n",
            serde_json::json!({"candidates":[{"content":{"role":"model","parts":[{"text":"🐈"}]}}]})
        );
        let split = visible.find('🐈').unwrap() + 2;
        for bytes in [
            hidden.as_bytes(),
            &visible.as_bytes()[..split],
            &visible.as_bytes()[split..],
        ] {
            write!(socket, "{:x}\r\n", bytes.len()).unwrap();
            socket.write_all(bytes).unwrap();
            socket.write_all(b"\r\n").unwrap();
            socket.flush().unwrap();
        }
        delivered
            .recv_timeout(std::time::Duration::from_secs(3))
            .expect("visible text must arrive before completion is sent");
        let tail = format!(
            "data: {}\r\n\r\n",
            serde_json::json!({"candidates":[{"content":{"role":"model","parts":[{"thoughtSignature":"empty-tail-signature"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":1,"thoughtsTokenCount":2}})
        );
        write!(socket, "{:x}\r\n", tail.len()).unwrap();
        socket.write_all(tail.as_bytes()).unwrap();
        socket.write_all(b"\r\n0\r\n\r\n").unwrap();
    });
    let doc =
        config_doc(&format!("http://{addr}/v1beta")).replace("openai-compatible", "google-genai");
    let mut model = scripted_model(&doc);
    let text = Arc::new(Mutex::new(String::new()));
    let copy = text.clone();
    model.set_delta_sink(Some(Arc::new(move |delta| {
        copy.lock().unwrap().push_str(delta);
        seen.send(()).unwrap();
    })));
    let preserved =
        PreservedLiveContext::new("test streaming", Vec::new(), "", "", 1024, 64).unwrap();
    let packet = build_packet(&preserved, None).unwrap();
    let output = model
        .step(
            packet.blocks(),
            &agent_runtime::ModelStepInput::without_tools(1),
            &CancellationToken::new(),
        )
        .unwrap();
    worker.join().unwrap();
    assert_eq!(*text.lock().unwrap(), "🐈");
    assert!(matches!(output,ModelStepOutput::Terminal {text,tokens,..} if text=="🐈"&&tokens==6));
    assert!(model.take_step_replay().is_some());
}

#[test]
fn native_google_http_errors_are_typed_and_never_echo_provider_error_content() {
    for (status, cause) in [
        (401, FailureCause::Auth),
        (403, FailureCause::Auth),
        (402, FailureCause::Quota),
        (
            429,
            FailureCause::Transient {
                retry_after_ms: None,
                rate_limited: true,
            },
        ),
        (
            503,
            FailureCause::Transient {
                retry_after_ms: None,
                rate_limited: false,
            },
        ),
    ] {
        let server = spawn_scripted_server(vec![(
            status,
            serde_json::json!({"error":{"code":status,"message":"private-error-canary"}})
                .to_string(),
        )]);
        let doc = config_doc(&format!("http://{}/v1beta", server.addr))
            .replace("openai-compatible", "google-genai");
        let mut model = scripted_model(&doc);
        let preserved =
            PreservedLiveContext::new("test errors", Vec::new(), "", "", 1024, 64).unwrap();
        let packet = build_packet(&preserved, None).unwrap();
        let error = model
            .step(
                packet.blocks(),
                &agent_runtime::ModelStepInput::without_tools(1),
                &CancellationToken::new(),
            )
            .unwrap_err();
        assert_eq!(error, ModelStepError::ProviderFailed { cause });
        assert!(!format!("{error:?}").contains("private-error-canary"));
    }
}

#[test]
fn native_google_cancellation_interrupts_an_inflight_hanging_provider() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (requested, received) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(3)))
            .unwrap();
        let _ = read_request(&mut socket);
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").unwrap();
        socket.flush().unwrap();
        requested.send(()).unwrap();
        let _ = socket.read(&mut [0u8; 1]);
    });
    let cancel = CancellationToken::new();
    let watched = cancel.clone();
    let canceller = thread::spawn(move || {
        received
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap();
        watched.cancel();
    });
    let doc =
        config_doc(&format!("http://{addr}/v1beta")).replace("openai-compatible", "google-genai");
    let mut model = scripted_model(&doc);
    let preserved =
        PreservedLiveContext::new("cancel request", Vec::new(), "", "", 1024, 64).unwrap();
    let packet = build_packet(&preserved, None).unwrap();
    let start = std::time::Instant::now();
    let error = model
        .step(
            packet.blocks(),
            &agent_runtime::ModelStepInput::without_tools(1),
            &cancel,
        )
        .unwrap_err();
    assert_eq!(error, ModelStepError::Cancelled);
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
    assert!(model.take_step_replay().is_none());
    canceller.join().unwrap();
    worker.join().unwrap();
}

#[test]
fn native_google_carries_inline_images_from_attachments_and_tool_results() {
    let server = spawn_scripted_server(vec![
        (
            200,
            google_body(
                serde_json::json!([{"functionCall":{"id":"image1","name":"workspace_read","args":{"path":"a.png"}},"thoughtSignature":"sig"}]),
            ),
        ),
        (
            200,
            google_body(serde_json::json!([{"text":"image received"}])),
        ),
    ]);
    let doc = format!(
        "{}vision = true\n",
        config_doc(&format!("http://{}/v1beta", server.addr))
            .replace("openai-compatible", "google-genai")
    );
    let mut model = scripted_model(&doc);
    let data = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/l9sAAAAASUVORK5CYII=";
    let data_url = format!("data:image/png;base64,{data}");
    assert!(
        model
            .set_image_attachments(vec![rapid::file_refs::ImageAttachment {
                path: "attached.png".to_owned(),
                data_url: data_url.clone(),
                bytes: 68
            }])
            .is_none()
    );
    let preserved =
        PreservedLiveContext::new("look at images", Vec::new(), "", "", 1024, 64).unwrap();
    let packet = build_packet(&preserved, None).unwrap();
    let cancel = CancellationToken::new();
    let first = model
        .step(
            packet.blocks(),
            &agent_runtime::ModelStepInput::without_tools(1),
            &cancel,
        )
        .unwrap();
    let ModelStepOutput::ToolCalls { calls, .. } = first else {
        panic!("expected tool call")
    };
    let state = model.take_step_replay();
    let history = vec![
        agent_runtime::ToolStepExchange::new(
            calls,
            vec![agent_runtime::ToolStepResult::Succeeded {
                call_id: "image1".to_owned(),
                summary: format!("image read\nDATA_URL:{data_url}"),
            }],
        )
        .with_provider_replay(state),
    ];
    model
        .step(
            packet.blocks(),
            &agent_runtime::ModelStepInput::with_history(2, &history, &[]),
            &cancel,
        )
        .unwrap();
    let requests = server.requests.lock().unwrap();
    let first: serde_json::Value =
        serde_json::from_str(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap();
    assert!(
        first["contents"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|content| content["parts"].as_array().unwrap())
            .any(|part| part["inlineData"]["data"] == data)
    );
    let second: serde_json::Value =
        serde_json::from_str(requests[1].split_once("\r\n\r\n").unwrap().1).unwrap();
    assert!(
        second["contents"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|content| content["parts"].as_array().unwrap())
            .any(|part| part["functionResponse"]["parts"][0]["inlineData"]["data"] == data)
    );
}

#[test]
fn native_google_uses_the_existing_egress_gate_before_any_connection() {
    use llm_router::providers::dial::{DialGate, DialTarget, ProxyTarget};
    struct Refuse(Arc<Mutex<Vec<(String, u16)>>>);
    impl DialGate for Refuse {
        fn permit(
            &self,
            target: DialTarget<'_>,
            _: Option<&ProxyTarget>,
            _: &[std::net::SocketAddr],
        ) -> Result<Vec<std::net::SocketAddr>, llm_router::ProviderError> {
            self.0
                .lock()
                .unwrap()
                .push((target.host.to_owned(), target.port));
            Err(llm_router::ProviderError::Connection)
        }
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let doc =
        config_doc(&format!("http://{addr}/v1beta")).replace("openai-compatible", "google-genai");
    let active = active_from_doc(&doc);
    let store = InMemoryCredentialStore::new();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut model =
        ConfiguredModel::build_with_gate(&active, &store, Some(Arc::new(Refuse(seen.clone()))))
            .unwrap();
    let preserved =
        PreservedLiveContext::new("check egress", Vec::new(), "", "", 1024, 64).unwrap();
    let packet = build_packet(&preserved, None).unwrap();
    let error = model
        .step(
            packet.blocks(),
            &agent_runtime::ModelStepInput::without_tools(1),
            &CancellationToken::new(),
        )
        .unwrap_err();
    assert_eq!(
        error,
        ModelStepError::ProviderFailed {
            cause: FailureCause::Connection
        }
    );
    assert_eq!(
        *seen.lock().unwrap(),
        vec![("127.0.0.1".to_owned(), addr.port())]
    );
    assert!(listener.accept().is_err());
}

fn vertex_doc(provider: &str, addr: std::net::SocketAddr) -> String {
    format!(
        "{}project = \"project-123\"\nlocation = \"us-central1\"\n",
        config_doc(&format!("http://{addr}/v1"))
            .replace("openai-compatible", provider)
            .replace("test-model", "fixture@20241022")
    )
}

#[cfg(unix)]
fn vertex_helper_doc(provider: &str, addr: std::net::SocketAddr, helper: &Path) -> String {
    let argv = serde_json::to_string(&vec![
        "/bin/sh".to_owned(),
        helper.to_string_lossy().into_owned(),
    ])
    .unwrap();
    vertex_doc(provider, addr).replace(
        "api_key = \"scripted-key\"",
        &format!("credential_process = {argv}\ncredential_timeout_ms = 1000"),
    )
}

#[cfg(unix)]
#[test]
fn binary_vertex_protocols_use_native_routes_bearer_and_lazy_refresh_without_token_output() {
    for provider in ["vertex-genai", "vertex-anthropic"] {
        let dir = temp_dir(provider);
        let helper = dir.join("credential.sh");
        // Plain tokens have no expiry and must be reread on the second tool turn.
        std::fs::write(&helper, "printf x >> \"$HOME/helper-runs\"\nprintf 'vertex-token-canary'\nprintf 'helper-stderr-canary' >&2\n").unwrap();
        let first = if provider == "vertex-genai" {
            google_body(
                serde_json::json!([{ "functionCall":{"name":"repo_read","args":{"path":"a.txt"}}, "thoughtSignature":"vertex-signature"}]),
            )
        } else {
            serde_json::json!({"type":"message","role":"assistant","content":[{"type":"thinking","thinking":"vertex-private-canary","signature":"vertex-signature"},{"type":"tool_use","id":"read1","name":"repo_read","input":{"path":"a.txt"}}],"stop_reason":"tool_use","usage":{"input_tokens":2,"output_tokens":3}}).to_string()
        };
        let last = if provider == "vertex-genai" {
            google_body(serde_json::json!([{"text":"Vertex completed"}]))
        } else {
            serde_json::json!({"type":"message","role":"assistant","content":[{"type":"text","text":"Vertex completed"}],"stop_reason":"end_turn","usage":{"input_tokens":2,"output_tokens":3}}).to_string()
        };
        let server = spawn_scripted_server(vec![(200, first), (200, last)]);
        let path = dir.join("config.toml");
        std::fs::write(&path, vertex_helper_doc(provider, server.addr, &helper)).unwrap();
        let project = exec_project(&dir);
        std::fs::create_dir_all(project.join(".rapidlm")).unwrap();
        let identity = kernel::ProjectIdentity::new(
            protocol::host_path::canonicalize(&project).unwrap(),
            None,
        )
        .unwrap();
        kernel::ProjectTrustStore::open(dir.join(".rapidlm/project-trust.json"))
            .set(
                &identity,
                kernel::TrustStatus::Trusted,
                &kernel::CancellationToken::new(),
            )
            .unwrap();
        std::fs::write(project.join("a.txt"), "Vertex read result").unwrap();
        let (code, stdout, stderr) = run_rapid(Some(&path), None, &dir);
        assert_eq!(code, Some(0), "{provider}: {stderr}");
        assert_eq!(stdout.trim(), "Vertex completed");
        for secret in [
            "vertex-token-canary",
            "helper-stderr-canary",
            "vertex-private-canary",
            "vertex-signature",
        ] {
            assert!(!format!("{stdout}{stderr}").contains(secret));
        }
        assert_eq!(
            std::fs::read_to_string(dir.join("helper-runs")).unwrap(),
            "xx"
        );
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests.iter() {
            let (headers, raw) = request.split_once("\r\n\r\n").unwrap();
            let headers = headers.to_ascii_lowercase();
            assert!(headers.contains("authorization: bearer vertex-token-canary"));
            assert!(!headers.contains("x-goog-api-key:"));
            assert!(!headers.contains("anthropic-version:"));
            let body: serde_json::Value = serde_json::from_str(raw).unwrap();
            let (publisher, method) = if provider == "vertex-genai" {
                ("google", "streamGenerateContent?alt=sse")
            } else {
                ("anthropic", "streamRawPredict")
            };
            assert!(request.starts_with(&format!("POST /v1/projects/project-123/locations/us-central1/publishers/{publisher}/models/fixture%4020241022:{method} ")));
            assert!(body.get("model").is_none());
            if provider == "vertex-anthropic" {
                assert_eq!(body["anthropic_version"], "vertex-2023-10-16");
            }
        }
        assert!(requests[1].contains("vertex-signature"));
        assert!(requests[1].contains("Vertex read result"));
        drop(requests);
        // Private session replay may retain signatures, but credentials must never enter the ledger.
        for entry in walk_files(&project.join(".rapidlm")) {
            let bytes = std::fs::read(entry).unwrap();
            assert!(!String::from_utf8_lossy(&bytes).contains("vertex-token-canary"));
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}

fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(walk_files(&path));
        } else {
            files.push(path);
        }
    }
    files
}

#[cfg(unix)]
#[test]
fn native_vertex_checks_egress_before_executing_a_credential_helper() {
    use llm_router::providers::dial::{DialGate, DialTarget, ProxyTarget};
    struct Refuse;
    impl DialGate for Refuse {
        fn permit(
            &self,
            _: DialTarget<'_>,
            _: Option<&ProxyTarget>,
            _: &[std::net::SocketAddr],
        ) -> Result<Vec<std::net::SocketAddr>, llm_router::ProviderError> {
            Err(llm_router::ProviderError::Connection)
        }
    }
    let dir = temp_dir("vertex-egress-helper");
    let marker = dir.join("helper-ran");
    let helper = dir.join("credential.sh");
    std::fs::write(
        &helper,
        format!("touch '{}'\nprintf token\n", marker.display()),
    )
    .unwrap();
    for provider in ["vertex-genai", "vertex-anthropic"] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let active = active_from_doc(&vertex_helper_doc(
            provider,
            listener.local_addr().unwrap(),
            &helper,
        ));
        let store = InMemoryCredentialStore::new();
        let mut model =
            ConfiguredModel::build_with_gate(&active, &store, Some(Arc::new(Refuse))).unwrap();
        let context =
            PreservedLiveContext::new("egress test", Vec::new(), "", "", 1024, 64).unwrap();
        let packet = build_packet(&context, None).unwrap();
        assert_eq!(
            model
                .step(
                    packet.blocks(),
                    &agent_runtime::ModelStepInput::without_tools(1),
                    &CancellationToken::new()
                )
                .unwrap_err(),
            ModelStepError::ProviderFailed {
                cause: FailureCause::Connection
            }
        );
        assert!(!marker.exists());
        assert!(listener.accept().is_err());
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn native_vertex_private_replay_survives_restart_and_stays_bound_to_the_cloud_resource() {
    for provider in ["vertex-genai", "vertex-anthropic"] {
        let first = if provider == "vertex-genai" {
            google_body(
                serde_json::json!([{"functionCall":{"name":"repo_read","args":{"path":"a.txt"}},"thoughtSignature":"vertex-restart-signature"}]),
            )
        } else {
            serde_json::json!({"type":"message","role":"assistant","content":[{"type":"thinking","thinking":"vertex-private-restart","signature":"vertex-restart-signature"},{"type":"tool_use","id":"read1","name":"repo_read","input":{"path":"a.txt"}}],"stop_reason":"tool_use","usage":{"input_tokens":2,"output_tokens":3}}).to_string()
        };
        let last = if provider == "vertex-genai" {
            google_body(serde_json::json!([{"text":"restarted Vertex"}]))
        } else {
            serde_json::json!({"type":"message","role":"assistant","content":[{"type":"text","text":"restarted Vertex"}],"stop_reason":"end_turn","usage":{"input_tokens":2,"output_tokens":3}}).to_string()
        };
        let server = spawn_scripted_server(vec![(200, first), (200, last.clone()), (200, last)]);
        let doc = vertex_doc(provider, server.addr);
        let context =
            PreservedLiveContext::new("read a.txt", Vec::new(), "", "", 1024, 64).unwrap();
        let packet = build_packet(&context, None).unwrap();
        let cancel = CancellationToken::new();
        let mut model = scripted_model(&doc);
        let first = model
            .step(
                packet.blocks(),
                &agent_runtime::ModelStepInput::without_tools(1),
                &cancel,
            )
            .unwrap();
        let ModelStepOutput::ToolCalls { calls, .. } = first else {
            panic!("expected tool call")
        };
        let id = calls[0].call_id().to_owned();
        let state = model.take_step_replay().unwrap();
        let history = vec![
            agent_runtime::ToolStepExchange::new(
                calls,
                vec![agent_runtime::ToolStepResult::Succeeded {
                    call_id: id,
                    summary: "restored result".to_owned(),
                }],
            )
            .with_provider_replay(Some(state)),
        ];
        let raw = serde_json::to_string(&rapid::approvals::SuspendedHistory::from_agent(&history))
            .unwrap();
        drop(model);
        let restored: rapid::approvals::SuspendedHistory = serde_json::from_str(&raw).unwrap();
        let history = restored.to_agent();
        for config in [&doc, &doc.replace("project-123", "project-456")] {
            let mut model = scripted_model(config);
            let output = model
                .step(
                    packet.blocks(),
                    &agent_runtime::ModelStepInput::with_history(2, &history, &[]),
                    &cancel,
                )
                .unwrap();
            assert!(
                matches!(output,ModelStepOutput::Terminal {text,..} if text == "restarted Vertex")
            );
        }
        let requests = server.requests.lock().unwrap();
        assert!(requests[1].contains("vertex-restart-signature"));
        assert!(!requests[2].contains("vertex-restart-signature"));
        assert!(requests[2].contains("/projects/project-456/"));
    }
}

#[cfg(unix)]
#[test]
fn vertex_offline_doctor_and_setup_preview_do_not_execute_credentials() {
    let dir = temp_dir("vertex-offline-configuration");
    let helper = dir.join("credential.sh");
    let marker = dir.join("helper-ran");
    std::fs::write(
        &helper,
        format!("touch '{}'\nprintf token\n", marker.display()),
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        vertex_helper_doc("vertex-genai", listener.local_addr().unwrap(), &helper),
    )
    .unwrap();
    let project = exec_project(&dir);
    std::fs::create_dir_all(project.join(".rapidlm")).unwrap();
    let doctor = Command::new(env!("CARGO_BIN_EXE_rapid"))
        .args(["doctor"])
        .current_dir(&project)
        .env("HOME", &dir)
        .env("RAPIDLM_CONFIG", &config)
        .env_remove("RAPIDLM_HOME")
        .env_remove("RAPIDLM_MODEL")
        .env_remove("RAPIDLM_MANAGED_CONFIG")
        .output()
        .unwrap();
    assert!(
        doctor.status.success(),
        "{}",
        String::from_utf8_lossy(&doctor.stderr)
    );
    assert!(!marker.exists());
    assert!(listener.accept().is_err());
    for provider in ["vertex-genai", "vertex-anthropic"] {
        let dest = dir.join(format!("{provider}.toml"));
        let output = Command::new(env!("CARGO_BIN_EXE_rapid"))
            .args([
                "setup",
                "--preset",
                provider,
                "--project",
                "project-123",
                "--location",
                "eu",
                "--dry-run",
                "--output",
                "json",
            ])
            .current_dir(&project)
            .env("HOME", &dir)
            .env("RAPIDLM_CONFIG", &dest)
            .env_remove("RAPIDLM_HOME")
            .env_remove("RAPIDLM_MODEL")
            .env_remove("RAPIDLM_MANAGED_CONFIG")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let body = String::from_utf8(output.stdout).unwrap();
        assert!(body.contains("aiplatform.eu.rep.googleapis.com"));
        assert!(body.contains("project-123"));
        assert!(!dest.exists());
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn native_vertex_cancellation_and_failed_helpers_never_dial_or_release_partial_credentials() {
    let dir = temp_dir("vertex-helper-cancel-failure");
    let helper = dir.join("credential.sh");
    for provider in ["vertex-genai", "vertex-anthropic"] {
        for cancelling in [false, true] {
            std::fs::write(
                &helper,
                if cancelling {
                    "printf partial-token; sleep 5\n"
                } else {
                    "printf partial-token; printf stderr-canary >&2; exit 1\n"
                },
            )
            .unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let doc = vertex_helper_doc(provider, listener.local_addr().unwrap(), &helper).replace(
                "credential_timeout_ms = 1000",
                "credential_timeout_ms = 10000",
            );
            let mut model = scripted_model(&doc);
            let context =
                PreservedLiveContext::new("credential boundary", Vec::new(), "", "", 1024, 64)
                    .unwrap();
            let packet = build_packet(&context, None).unwrap();
            let cancel = CancellationToken::new();
            let worker = if cancelling {
                let watched = cancel.clone();
                Some(thread::spawn(move || {
                    thread::sleep(std::time::Duration::from_millis(100));
                    watched.cancel();
                }))
            } else {
                None
            };
            let start = std::time::Instant::now();
            let error = model
                .step(
                    packet.blocks(),
                    &agent_runtime::ModelStepInput::without_tools(1),
                    &cancel,
                )
                .unwrap_err();
            if let Some(worker) = worker {
                worker.join().unwrap();
            }
            assert_eq!(
                error,
                if cancelling {
                    ModelStepError::Cancelled
                } else {
                    ModelStepError::ProviderFailed {
                        cause: FailureCause::Auth,
                    }
                },
                "{provider}"
            );
            assert!(start.elapsed() < std::time::Duration::from_secs(2));
            assert!(!format!("{error:?}").contains("partial-token"));
            assert!(listener.accept().is_err());
        }
    }
    let _ = std::fs::remove_dir_all(dir);
}

fn azure_doc(provider: &str, addr: std::net::SocketAddr, mode: &str, auth: &str) -> String {
    format!(
        "{}api_version = \"2025-04-01-preview\"\nazure_url_mode = \"{mode}\"\nazure_auth = \"{auth}\"\nmax_tokens = 1024\n",
        config_doc(&format!("http://{addr}/openai"))
            .replace("openai-compatible", provider)
            .replace("test-model", "my-deployment")
    )
}

#[test]
fn binary_azure_protocols_bind_deployment_mode_version_and_fixed_credential_headers() {
    for provider in ["azure-chat", "azure-responses"] {
        for mode in ["v1", "deployment"] {
            for auth in ["api-key", "entra"] {
                let body = if provider == "azure-chat" {
                    NON_STREAMING_BODY.to_owned()
                } else {
                    serde_json::json!({"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello from scripted model"}]}],"usage":{"input_tokens":3,"output_tokens":5}}).to_string()
                };
                let server = spawn_scripted_server(vec![(200, body)]);
                let dir = temp_dir(&format!("{provider}-{mode}-{auth}"));
                let config = dir.join("config.toml");
                std::fs::write(&config, azure_doc(provider, server.addr, mode, auth)).unwrap();
                let (code, stdout, stderr) = run_rapid(Some(&config), None, &dir);
                assert_eq!(code, Some(0), "{provider}/{mode}/{auth}: {stderr}");
                assert_eq!(stdout.trim(), "hello from scripted model");
                assert!(!format!("{stdout}{stderr}").contains("scripted-key"));
                let requests = server.requests.lock().unwrap();
                assert_eq!(requests.len(), 1);
                let request = &requests[0];
                let prefix = if mode == "v1" {
                    "/openai/v1"
                } else {
                    "/openai/deployments/my-deployment"
                };
                let suffix = if provider == "azure-chat" {
                    "chat/completions"
                } else {
                    "responses"
                };
                assert!(request.starts_with(&format!(
                    "POST {prefix}/{suffix}?api-version=2025-04-01-preview "
                )));
                let (headers, raw) = request.split_once("\r\n\r\n").unwrap();
                let headers = headers.to_ascii_lowercase();
                if auth == "api-key" {
                    assert!(headers.contains("\r\napi-key: scripted-key"));
                    assert!(!headers.contains("authorization:"));
                } else {
                    assert!(headers.contains("authorization: bearer scripted-key"));
                    assert!(!headers.contains("api-key:"));
                }
                assert!(!headers.contains("x-goog-api-key:"));
                let payload: serde_json::Value = serde_json::from_str(raw).unwrap();
                assert_eq!(payload["model"], "my-deployment");
                if provider == "azure-chat" {
                    assert!(payload["max_completion_tokens"].is_number());
                    assert!(payload.get("max_tokens").is_none());
                } else {
                    assert_eq!(payload["store"], false);
                    assert!(payload["input"].is_array());
                }
                drop(requests);
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn azure_entra_helpers_are_lazy_and_api_key_profiles_refuse_process_credentials() {
    let dir = temp_dir("azure-entra-helper");
    let helper = dir.join("credential.sh");
    std::fs::write(
        &helper,
        "printf 'entra-private-token'\nprintf 'entra-private-stderr' >&2\n",
    )
    .unwrap();
    let argv = serde_json::to_string(&vec![
        "/bin/sh".to_owned(),
        helper.to_string_lossy().into_owned(),
    ])
    .unwrap();
    let server = spawn_scripted_server(vec![(200, NON_STREAMING_BODY.to_owned())]);
    let doc = azure_doc("azure-chat", server.addr, "v1", "entra").replace(
        "api_key = \"scripted-key\"",
        &format!("credential_process = {argv}"),
    );
    let config = dir.join("config.toml");
    std::fs::write(&config, &doc).unwrap();
    let (code, stdout, stderr) = run_rapid(Some(&config), None, &dir);
    assert_eq!(code, Some(0), "{stderr}");
    assert!(!format!("{stdout}{stderr}").contains("entra-private"));
    assert!(
        server.requests.lock().unwrap()[0]
            .to_ascii_lowercase()
            .contains("authorization: bearer entra-private-token")
    );
    assert!(
        parse_config_document(
            &doc.replace("azure_auth = \"entra\"", "azure_auth = \"api-key\""),
            "fixture"
        )
        .is_err()
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn azure_setup_preview_binds_an_explicit_resource_deployment_and_api_options() {
    let dir = temp_dir("azure-setup-native");
    let project = exec_project(&dir);
    std::fs::create_dir_all(project.join(".rapidlm")).unwrap();
    let config = dir.join("config.toml");
    for provider in ["azure-chat", "azure-responses"] {
        let (code, stdout, stderr) = run_rapid_args_in(
            &project,
            &dir,
            &config,
            &[
                "setup",
                "--preset",
                provider,
                "--base-url",
                "https://owned.example/openai",
                "--model",
                "my-deployment",
                "--key-env",
                "CLOUD_TOKEN",
                "--azure-auth",
                "entra",
                "--azure-url-mode",
                "deployment",
                "--api-version",
                "2025-04-01-preview",
                "--dry-run",
                "--output",
                "json",
            ],
        );
        assert_eq!(code, Some(0), "{stderr}");
        let payload: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(payload["dry_run"], true);
        for field in [
            "azure_auth",
            "azure_url_mode",
            "api_version",
            "my-deployment",
        ] {
            assert!(stdout.contains(field), "{stdout}");
        }
        assert!(!config.exists());
    }
    let (code, _, stderr) = run_rapid_args_in(
        &project,
        &dir,
        &config,
        &["setup", "--preset", "azure-chat", "--dry-run"],
    );
    assert_eq!(code, Some(2));
    assert!(stderr.contains("Azure requires"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn native_azure_streams_split_utf8_before_completion_for_chat_and_responses() {
    for provider in ["azure-chat", "azure-responses"] {
        assert_native_openai_incremental(provider, false);
    }
}
#[test]
fn native_copilot_streams_split_utf8_before_completion_for_chat_and_responses() {
    for provider in ["copilot-chat", "copilot-responses"] {
        assert_native_openai_incremental(provider, true);
    }
}
fn assert_native_openai_incremental(provider: &'static str, copilot: bool) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (seen, delivered) = std::sync::mpsc::channel();
    let responses = provider.ends_with("responses");
    let worker = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(3)))
            .unwrap();
        let request = read_request(&mut socket);
        if copilot {
            assert!(
                request.contains("Authorization: Bearer tid=copilot-api-canary;exp=fixture:hmac")
            );
            assert!(request.contains("X-Initiator: user"));
        } else {
            assert!(request.contains("api-key: scripted-key"));
        }
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").unwrap();
        let first = if responses {
            serde_json::json!({"type":"response.output_text.delta","delta":"🐈"})
        } else {
            serde_json::json!({"choices":[{"delta":{"content":"🐈"}}]})
        };
        let first = format!("data: {first}\n\n");
        let split = first.find('🐈').unwrap() + 2;
        for bytes in [&first.as_bytes()[..split], &first.as_bytes()[split..]] {
            write!(socket, "{:x}\r\n", bytes.len()).unwrap();
            socket.write_all(bytes).unwrap();
            socket.write_all(b"\r\n").unwrap();
            socket.flush().unwrap();
        }
        delivered
            .recv_timeout(std::time::Duration::from_secs(3))
            .expect("delta before completion");
        let tail = if responses {
            format!(
                "data: {}\n\n",
                serde_json::json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"🐈"}]}],"usage":{"input_tokens":1,"output_tokens":1}}})
            )
        } else {
            format!(
                "data: {}\n\ndata: [DONE]\n\n",
                serde_json::json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}})
            )
        };
        write!(socket, "{:x}\r\n", tail.len()).unwrap();
        socket.write_all(tail.as_bytes()).unwrap();
        socket.write_all(b"\r\n0\r\n\r\n").unwrap();
    });
    let mut model = scripted_model(&if copilot {
        copilot_doc(provider, addr, None)
    } else {
        azure_doc(provider, addr, "v1", "api-key")
    });
    let deltas = Arc::new(Mutex::new(String::new()));
    let copy = deltas.clone();
    model.set_delta_sink(Some(Arc::new(move |text| {
        copy.lock().unwrap().push_str(text);
        seen.send(()).unwrap();
    })));
    let context = PreservedLiveContext::new("streaming", Vec::new(), "", "", 1024, 64).unwrap();
    let packet = build_packet(&context, None).unwrap();
    let output = model
        .step(
            packet.blocks(),
            &agent_runtime::ModelStepInput::without_tools(1),
            &CancellationToken::new(),
        )
        .unwrap();
    worker.join().unwrap();
    assert_eq!(*deltas.lock().unwrap(), "🐈");
    assert!(matches!(output,ModelStepOutput::Terminal {text,tokens,..} if text=="🐈" && tokens==2));
}

#[test]
fn azure_legacy_output_limit_is_explicit_and_does_not_depend_on_deployment_names() {
    let server = spawn_scripted_server(vec![(200, NON_STREAMING_BODY.to_owned())]);
    let doc = format!(
        "{}azure_legacy_max_tokens = true\n",
        azure_doc("azure-chat", server.addr, "deployment", "api-key")
    );
    let mut model = scripted_model(&doc);
    let context = PreservedLiveContext::new("bound", Vec::new(), "", "", 1024, 64).unwrap();
    let packet = build_packet(&context, None).unwrap();
    model
        .step(
            packet.blocks(),
            &agent_runtime::ModelStepInput::without_tools(1),
            &CancellationToken::new(),
        )
        .unwrap();
    let requests = server.requests.lock().unwrap();
    let payload: serde_json::Value =
        serde_json::from_str(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(payload["max_tokens"], 1024);
    assert!(payload.get("max_completion_tokens").is_none());
}

#[test]
fn native_cloud_profiles_refuse_unset_token_variables_before_connecting() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    for doc in [
        azure_doc("azure-chat", addr, "v1", "api-key"),
        azure_doc("azure-responses", addr, "v1", "entra"),
        vertex_doc("vertex-genai", addr),
        vertex_doc("vertex-anthropic", addr),
    ] {
        let doc = doc.replace(
            "api_key = \"scripted-key\"",
            "env_key = \"UNSET_NATIVE_CLOUD_FIXTURE\"",
        );
        let active = active_from_doc(&doc);
        let store = InMemoryCredentialStore::new();
        assert!(matches!(
            ConfiguredModel::build(&active, &store),
            Err(rapid::model::ModelConfigError::Credential { .. })
        ));
        assert!(listener.accept().is_err());
    }
}

fn aws_fixture_frame(kind: &str, value: serde_json::Value) -> Vec<u8> {
    // Independent fixture encoder: no production frame encoder or decoder calls.
    fn crc(bytes: &[u8]) -> u32 {
        let mut c = 0xffff_ffffu32;
        for b in bytes {
            c ^= u32::from(*b);
            for _ in 0..8 {
                c = (c >> 1) ^ if c & 1 != 0 { 0xedb8_8320 } else { 0 };
            }
        }
        !c
    }
    let mut headers = Vec::new();
    for (name, value) in [(":message-type", "event"), (":event-type", kind)] {
        headers.push(name.len() as u8);
        headers.extend_from_slice(name.as_bytes());
        headers.push(7);
        headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
        headers.extend_from_slice(value.as_bytes());
    }
    let payload = value.to_string().into_bytes();
    let mut frame = Vec::new();
    frame.extend_from_slice(&((16 + headers.len() + payload.len()) as u32).to_be_bytes());
    frame.extend_from_slice(&(headers.len() as u32).to_be_bytes());
    frame.extend_from_slice(&crc(&frame).to_be_bytes());
    frame.extend(headers);
    frame.extend(payload);
    frame.extend_from_slice(&crc(&frame).to_be_bytes());
    frame
}
fn bedrock_response(tool: bool) -> Vec<u8> {
    use serde_json::json;
    let mut events = vec![("messageStart", json!({"role":"assistant"}))];
    if tool {
        events.extend([
        ("contentBlockDelta",json!({"contentBlockIndex":0,"delta":{"reasoningContent":{"text":"bedrock-private-canary"}}})),
        ("contentBlockDelta",json!({"contentBlockIndex":0,"delta":{"reasoningContent":{"signature":"bedrock-signed-canary"}}})),
        ("contentBlockStop",json!({"contentBlockIndex":0})),
        ("contentBlockStart",json!({"contentBlockIndex":1,"start":{"toolUse":{"toolUseId":"read1","name":"repo_read"}}})),
        ("contentBlockDelta",json!({"contentBlockIndex":1,"delta":{"toolUse":{"input":"{ \"path\" : \"a.txt\" }"}}})),
        ("contentBlockStop",json!({"contentBlockIndex":1})),
        ("messageStop",json!({"stopReason":"tool_use"})),
    ]);
    } else {
        events.extend([
            (
                "contentBlockDelta",
                json!({"contentBlockIndex":0,"delta":{"text":"Bedrock completed"}}),
            ),
            ("contentBlockStop", json!({"contentBlockIndex":0})),
            ("messageStop", json!({"stopReason":"end_turn"})),
        ]);
    }
    events.push((
        "metadata",
        json!({"usage":{"inputTokens":2,"outputTokens":3,"totalTokens":5}}),
    ));
    events
        .into_iter()
        .flat_map(|(kind, value)| aws_fixture_frame(kind, value))
        .collect()
}
fn bedrock_doc(addr: std::net::SocketAddr, signed: bool) -> String {
    let doc = format!(
        "{}region = \"us-east-1\"\naws_auth = \"{}\"\n",
        config_doc(&format!("http://{addr}"))
            .replace("openai-compatible", "bedrock")
            .replace("test-model", "anthropic.fixture-v1:0"),
        if signed { "sigv4" } else { "bearer" }
    );
    if signed {
        doc.replace("api_key = \"scripted-key\"\n", "")
    } else {
        doc
    }
}
#[test]
fn binary_bedrock_bearer_and_sigv4_reach_native_converse_with_signed_tool_replay() {
    for signed in [false, true] {
        let server = spawn_binary_server(
            vec![
                (200, bedrock_response(true)),
                (200, bedrock_response(false)),
            ],
            "application/vnd.amazon.eventstream",
        );
        let dir = temp_dir(if signed {
            "bedrock-signed"
        } else {
            "bedrock-bearer"
        });
        let config = dir.join("config.toml");
        std::fs::write(&config, bedrock_doc(server.addr, signed)).unwrap();
        let project = exec_project(&dir);
        std::fs::create_dir_all(project.join(".rapidlm")).unwrap();
        let identity = kernel::ProjectIdentity::new(
            protocol::host_path::canonicalize(&project).unwrap(),
            None,
        )
        .unwrap();
        kernel::ProjectTrustStore::open(dir.join(".rapidlm/project-trust.json"))
            .set(
                &identity,
                kernel::TrustStatus::Trusted,
                &kernel::CancellationToken::new(),
            )
            .unwrap();
        std::fs::write(project.join("a.txt"), "native file result").unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_rapid"))
            .args(["exec", "read a.txt"])
            .current_dir(&project)
            .env("HOME", &dir)
            .env("RAPIDLM_CONFIG", &config)
            .env_remove("RAPIDLM_HOME")
            .env_remove("RAPIDLM_MODEL")
            .env("AWS_ACCESS_KEY_ID", "AKIDEXAMPLE")
            .env("AWS_SECRET_ACCESS_KEY", "bedrock-secret-canary")
            .env("AWS_SESSION_TOKEN", "bedrock-session-canary")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        assert_eq!(stdout.trim(), "Bedrock completed");
        for secret in [
            "bedrock-secret-canary",
            "bedrock-session-canary",
            "bedrock-private-canary",
            "bedrock-signed-canary",
        ] {
            assert!(!format!("{stdout}{stderr}").contains(secret));
        }
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests.iter() {
            assert!(request.starts_with("POST /model/anthropic.fixture-v1%3A0/converse-stream "));
            if signed {
                assert!(
                    request.contains("Authorization: AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/")
                );
                assert!(request.contains("/us-east-1/bedrock/aws4_request"));
                assert!(request.contains("x-amz-security-token: bedrock-session-canary"));
                assert!(!request.contains("bedrock-secret-canary"));
            } else {
                assert!(request.contains("Authorization: Bearer scripted-key"));
            }
            let payload: serde_json::Value =
                serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
            assert!(payload.get("model").is_none());
            assert!(payload["inferenceConfig"]["maxTokens"].is_number());
        }
        assert!(requests[1].contains("bedrock-signed-canary"));
        assert!(requests[1].contains("native file result"));
        drop(requests);
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(unix)]
fn bedrock_helper_doc(addr: std::net::SocketAddr, signed: bool, helper: &Path) -> String {
    let argv = serde_json::to_string(&vec![
        "/bin/sh".to_owned(),
        helper.to_string_lossy().into_owned(),
    ])
    .unwrap();
    let doc = bedrock_doc(addr, signed).replace("api_key = \"scripted-key\"\n", "");
    format!("{doc}credential_process = {argv}\ncredential_timeout_ms = 10000\n")
}

#[test]
fn native_bedrock_replay_survives_durable_history_and_omits_changed_resource_state() {
    let server = spawn_binary_server(
        vec![
            (200, bedrock_response(true)),
            (200, bedrock_response(false)),
            (200, bedrock_response(false)),
            (200, bedrock_response(false)),
        ],
        "application/vnd.amazon.eventstream",
    );
    let doc = bedrock_doc(server.addr, false);
    let context = PreservedLiveContext::new("read a.txt", Vec::new(), "", "", 1024, 64).unwrap();
    let packet = build_packet(&context, None).unwrap();
    let cancel = CancellationToken::new();
    let mut model = scripted_model(&doc);
    let output = model
        .step(
            packet.blocks(),
            &agent_runtime::ModelStepInput::without_tools(1),
            &cancel,
        )
        .unwrap();
    let ModelStepOutput::ToolCalls { calls, .. } = output else {
        panic!("expected tool call")
    };
    let id = calls[0].call_id().to_owned();
    let state = model.take_step_replay().unwrap();
    let history = vec![
        agent_runtime::ToolStepExchange::new(
            calls,
            vec![agent_runtime::ToolStepResult::Succeeded {
                call_id: id,
                summary: "restored Bedrock result".to_owned(),
            }],
        )
        .with_provider_replay(Some(state)),
    ];
    let raw =
        serde_json::to_string(&rapid::approvals::SuspendedHistory::from_agent(&history)).unwrap();
    drop(model);
    let restored: rapid::approvals::SuspendedHistory = serde_json::from_str(&raw).unwrap();
    let history = restored.to_agent();
    for config in [
        doc.clone(),
        doc.replace("us-east-1", "eu-west-1"),
        format!(
            "{doc}bedrock_resource = \"arn:aws:bedrock:us-east-1:123456789012:inference-profile/changed\"\n"
        ),
    ] {
        let mut model = scripted_model(&config);
        assert!(
            matches!(model.step(packet.blocks(), &agent_runtime::ModelStepInput::with_history(2, &history, &[]), &cancel).unwrap(), ModelStepOutput::Terminal { text, .. } if text == "Bedrock completed")
        );
    }
    let requests = server.requests.lock().unwrap();
    assert!(requests[1].contains("bedrock-signed-canary"));
    for request in &requests[2..] {
        assert!(!request.contains("bedrock-signed-canary"));
        assert!(request.contains("restored Bedrock result"));
    }
    assert!(requests[3].starts_with("POST /model/arn%3Aaws%3Abedrock%3Aus-east-1%3A123456789012%3Ainference-profile%2Fchanged/converse-stream "));
}

#[cfg(unix)]
#[test]
fn native_bedrock_checks_egress_before_resolving_either_credential_mode() {
    use llm_router::providers::dial::{DialGate, DialTarget, ProxyTarget};
    struct Refuse;
    impl DialGate for Refuse {
        fn permit(
            &self,
            _: DialTarget<'_>,
            _: Option<&ProxyTarget>,
            _: &[std::net::SocketAddr],
        ) -> Result<Vec<std::net::SocketAddr>, llm_router::ProviderError> {
            Err(llm_router::ProviderError::Connection)
        }
    }
    let dir = temp_dir("bedrock-egress-helper");
    let marker = dir.join("ran");
    let helper = dir.join("helper.sh");
    std::fs::write(
        &helper,
        format!("printf x > '{}'\nprintf invalid\n", marker.display()),
    )
    .unwrap();
    for signed in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let active = active_from_doc(&bedrock_helper_doc(
            listener.local_addr().unwrap(),
            signed,
            &helper,
        ));
        let store = InMemoryCredentialStore::new();
        let mut model =
            ConfiguredModel::build_with_gate(&active, &store, Some(Arc::new(Refuse))).unwrap();
        let context = PreservedLiveContext::new("test", Vec::new(), "", "", 1024, 64).unwrap();
        let packet = build_packet(&context, None).unwrap();
        assert_eq!(
            model
                .step(
                    packet.blocks(),
                    &agent_runtime::ModelStepInput::without_tools(1),
                    &CancellationToken::new()
                )
                .unwrap_err(),
            ModelStepError::ProviderFailed {
                cause: FailureCause::Connection
            }
        );
        assert!(!marker.exists());
        assert!(listener.accept().is_err());
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn native_bedrock_cancelled_or_failed_helpers_never_dial_or_expose_partial_keys() {
    let dir = temp_dir("bedrock-helper-cancel");
    let helper = dir.join("helper.sh");
    for signed in [false, true] {
        for cancelling in [false, true] {
            std::fs::write(
                &helper,
                if cancelling {
                    "printf partial-private-key; sleep 5\n"
                } else {
                    "printf partial-private-key; printf stderr-private-key >&2; exit 1\n"
                },
            )
            .unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let mut model = scripted_model(&bedrock_helper_doc(
                listener.local_addr().unwrap(),
                signed,
                &helper,
            ));
            let context = PreservedLiveContext::new("test", Vec::new(), "", "", 1024, 64).unwrap();
            let packet = build_packet(&context, None).unwrap();
            let cancel = CancellationToken::new();
            let worker = cancelling.then(|| {
                let cancel = cancel.clone();
                thread::spawn(move || {
                    thread::sleep(std::time::Duration::from_millis(100));
                    cancel.cancel();
                })
            });
            let start = std::time::Instant::now();
            let error = model
                .step(
                    packet.blocks(),
                    &agent_runtime::ModelStepInput::without_tools(1),
                    &cancel,
                )
                .unwrap_err();
            if let Some(worker) = worker {
                worker.join().unwrap();
            }
            assert_eq!(
                error,
                if cancelling {
                    ModelStepError::Cancelled
                } else {
                    ModelStepError::ProviderFailed {
                        cause: FailureCause::Auth,
                    }
                }
            );
            assert!(start.elapsed() < std::time::Duration::from_secs(2));
            assert!(listener.accept().is_err());
            assert!(!format!("{error:?}").contains("private-key"));
        }
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn binary_bedrock_helpers_refresh_without_expiry_and_cache_expiring_keys() {
    for (signed, expiring) in [(false, false), (true, false), (true, true)] {
        let dir = temp_dir("bedrock-process-source");
        let helper = dir.join("helper.sh");
        let mut aws = serde_json::json!({"Version":1,"AccessKeyId":"AKIDEXAMPLE","SecretAccessKey":"bedrock-helper-secret","SessionToken":"bedrock-helper-session"});
        if expiring {
            aws["Expiration"] = (time::OffsetDateTime::now_utc() + time::Duration::hours(1))
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap()
                .into();
        }
        let token = if signed {
            aws.to_string()
        } else {
            "bedrock-bearer-helper".to_owned()
        };
        std::fs::write(
            &helper,
            format!(
                "printf x >> \"$HOME/runs\"\nprintf '%s' '{}'\nprintf private-helper-stderr >&2\n",
                token
            ),
        )
        .unwrap();
        let server = spawn_binary_server(
            vec![
                (200, bedrock_response(true)),
                (200, bedrock_response(false)),
            ],
            "application/vnd.amazon.eventstream",
        );
        let config = dir.join("config.toml");
        std::fs::write(&config, bedrock_helper_doc(server.addr, signed, &helper)).unwrap();
        let project = exec_project(&dir);
        std::fs::create_dir_all(project.join(".rapidlm")).unwrap();
        let identity = kernel::ProjectIdentity::new(
            protocol::host_path::canonicalize(&project).unwrap(),
            None,
        )
        .unwrap();
        kernel::ProjectTrustStore::open(dir.join(".rapidlm/project-trust.json"))
            .set(
                &identity,
                kernel::TrustStatus::Trusted,
                &kernel::CancellationToken::new(),
            )
            .unwrap();
        std::fs::write(project.join("a.txt"), "helper result").unwrap();
        let (code, stdout, stderr) = run_rapid(Some(&config), None, &dir);
        assert_eq!(code, Some(0), "{stderr}");
        assert_eq!(stdout.trim(), "Bedrock completed");
        assert_eq!(
            std::fs::read_to_string(dir.join("runs")).unwrap(),
            if expiring { "x" } else { "xx" }
        );
        let secrets = [
            "bedrock-helper-secret",
            "bedrock-helper-session",
            "bedrock-bearer-helper",
            "private-helper-stderr",
        ];
        for secret in secrets {
            assert!(!format!("{stdout}{stderr}").contains(secret));
        }
        for file in walk_files(&project.join(".rapidlm")) {
            let content = std::fs::read(file).unwrap();
            for secret in secrets {
                assert!(!String::from_utf8_lossy(&content).contains(secret));
            }
        }
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|r| r.contains(if signed {
            "Authorization: AWS4-HMAC-SHA256"
        } else {
            "Authorization: Bearer bedrock-bearer-helper"
        })));
        drop(requests);
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(unix)]
#[test]
fn binary_bedrock_profile_delegates_fixed_cli_argv_and_stays_offline_in_preview() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp_dir("bedrock-cli-profile");
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let aws = bin.join("aws");
    std::fs::write(&aws, "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$HOME/aws-argv\"\nprintf '%s' '{\"Version\":1,\"AccessKeyId\":\"AKIDEXAMPLE\",\"SecretAccessKey\":\"profile-secret\"}'\n").unwrap();
    std::fs::set_permissions(&aws, std::fs::Permissions::from_mode(0o700)).unwrap();
    let server = spawn_binary_server(
        vec![(200, bedrock_response(false))],
        "application/vnd.amazon.eventstream",
    );
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        format!(
            "{}aws_profile = \"work profile\"\n",
            bedrock_doc(server.addr, true)
        ),
    )
    .unwrap();
    let project = exec_project(&dir);
    std::fs::create_dir_all(&project).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_rapid"))
            .args(args)
            .current_dir(&project)
            .env("HOME", &dir)
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("RAPIDLM_CONFIG", &config)
            .env_remove("RAPIDLM_HOME")
            .env_remove("RAPIDLM_MODEL")
            .env_remove("RAPIDLM_MANAGED_CONFIG")
            .output()
            .unwrap()
    };
    let doctor = run(&["doctor", "--no-sandbox-probe"]);
    assert!(!String::from_utf8_lossy(&doctor.stderr).contains("profile-secret"));
    assert!(!dir.join("aws-argv").exists());
    let preview = run(&[
        "setup",
        "--preset",
        "bedrock",
        "--region",
        "us-east-1",
        "--model",
        "fixture-v1:0",
        "--base-url",
        &format!("http://{}", server.addr),
        "--aws-profile",
        "work profile",
        "--dry-run",
        "--non-interactive",
        "--output",
        "json",
    ]);
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    assert!(!dir.join("aws-argv").exists());
    let output = run(&["exec", "say hello"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "Bedrock completed"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("aws-argv")).unwrap(),
        "configure\nexport-credentials\n--profile\nwork profile\n--format\nprocess\n--no-cli-pager\n--no-cli-auto-prompt\n"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn native_bedrock_live_probe_uses_shared_egress_and_preserves_bounded_effort() {
    let server = spawn_binary_server(
        vec![(200, bedrock_response(false))],
        "application/vnd.amazon.eventstream",
    );
    let doc = format!(
        "{}reasoning = true\nbedrock_effort = \"anthropic\"\nreasoning_effort = \"high\"\nthinking = \"enabled\"\nthinking_budget_tokens = 1024\n",
        bedrock_doc(server.addr, false)
    );
    let active = active_from_doc(&doc);
    let probe =
        rapid::setup::probe_resolved(&active, &llm_router::CancellationToken::new()).unwrap();
    assert_eq!(probe.result, Ok(()));
    assert!(!probe.receipts.is_empty());
    assert!(active.entry.thinking.is_some());
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let payload: serde_json::Value =
        serde_json::from_str(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(payload["inferenceConfig"]["maxTokens"], 16);
    assert_eq!(
        payload["additionalModelRequestFields"]["output_config"]["effort"],
        "high"
    );
    assert!(
        payload["additionalModelRequestFields"]
            .get("thinking")
            .is_none()
    );
}

fn copilot_doc(
    provider: &str,
    addr: std::net::SocketAddr,
    token_addr: Option<std::net::SocketAddr>,
) -> String {
    let doc = config_doc(&format!("http://{addr}")).replace("openai-compatible", provider);
    let doc = if let Some(token) = token_addr {
        format!(
            "{}copilot_auth = \"github-token\"\ncopilot_token_url = \"http://{token}/copilot_internal/v2/token\"\n",
            doc.replace("scripted-key", "github-source-canary")
        )
    } else {
        doc.replace("scripted-key", "tid=copilot-api-canary;exp=fixture:hmac")
    };
    format!("{doc}copilot_integration_id = \"rapidlm-test\"\nretry = {{ max_attempts = 1 }}\n")
}
fn copilot_body(provider: &str, tool: bool) -> String {
    if provider == "copilot-chat" {
        if tool {
            serde_json::json!({"choices":[{"message":{"role":"assistant","content":"checking file","tool_calls":[{"id":"read1","type":"function","function":{"name":"repo_read","arguments":"{\"path\":\"a.txt\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":3,"completion_tokens":5}}).to_string()
        } else {
            NON_STREAMING_BODY.replace("hello from scripted model", "Copilot completed")
        }
    } else if tool {
        serde_json::json!({"status":"completed","output":[{"type":"reasoning","id":"rs1","encrypted_content":"copilot-private-replay"},{"type":"function_call","id":"fc1","call_id":"read1","name":"repo_read","arguments":"{\"path\":\"a.txt\"}"}],"usage":{"input_tokens":3,"output_tokens":5}}).to_string()
    } else {
        serde_json::json!({"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Copilot completed"}]}],"usage":{"input_tokens":2,"output_tokens":3}}).to_string()
    }
}
fn trust_copilot_project(dir: &Path) -> PathBuf {
    let project = exec_project(dir);
    std::fs::create_dir_all(project.join(".rapidlm")).unwrap();
    let identity =
        kernel::ProjectIdentity::new(protocol::host_path::canonicalize(&project).unwrap(), None)
            .unwrap();
    kernel::ProjectTrustStore::open(dir.join(".rapidlm/project-trust.json"))
        .set(
            &identity,
            kernel::TrustStatus::Trusted,
            &kernel::CancellationToken::new(),
        )
        .unwrap();
    std::fs::write(project.join("a.txt"), "Copilot tool result").unwrap();
    project
}

#[test]
fn binary_copilot_chat_and_responses_keep_source_and_inference_tokens_separate_with_expiry_refresh()
{
    for provider in ["copilot-chat", "copilot-responses"] {
        for mode in ["direct", "cached", "refresh"] {
            let dir = temp_dir(&format!("{provider}-{mode}"));
            let project = trust_copilot_project(&dir);
            let server = spawn_scripted_server(vec![
                (200, copilot_body(provider, true)),
                (200, copilot_body(provider, false)),
            ]);
            let expires = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                + if mode == "refresh" { 15 } else { 3600 };
            let token = serde_json::json!({"token":"tid=copilot-api-canary;exp=fixture:hmac","expires_at":expires,"endpoints":{"api":"https://must-not-redirect.example"}}).to_string();
            let tokens = (mode != "direct").then(|| {
                spawn_scripted_server(vec![
                    (200, token.clone());
                    if mode == "refresh" { 2 } else { 1 }
                ])
            });
            let config = dir.join("config.toml");
            std::fs::write(
                &config,
                copilot_doc(provider, server.addr, tokens.as_ref().map(|s| s.addr)),
            )
            .unwrap();
            let (code, stdout, stderr) = run_rapid(Some(&config), None, &dir);
            assert_eq!(code, Some(0), "{provider}/{mode}: {stderr}");
            assert_eq!(stdout.trim(), "Copilot completed");
            for secret in [
                "github-source-canary",
                "copilot-api-canary",
                "copilot-private-replay",
            ] {
                assert!(!format!("{stdout}{stderr}").contains(secret));
            }
            let requests = server.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            for (index, request) in requests.iter().enumerate() {
                assert!(request.starts_with(if provider == "copilot-chat" {
                    "POST /chat/completions "
                } else {
                    "POST /responses "
                }));
                assert!(
                    request
                        .contains("Authorization: Bearer tid=copilot-api-canary;exp=fixture:hmac")
                );
                assert!(!request.contains("github-source-canary"));
                assert!(request.contains("Copilot-Integration-Id: rapidlm-test"));
                assert!(request.contains("User-Agent: RapidLM/"));
                assert!(!request.contains("vscode"));
                assert!(request.contains(if index == 0 {
                    "X-Initiator: user"
                } else {
                    "X-Initiator: agent"
                }));
            }
            assert!(requests[1].contains("Copilot tool result"));
            if provider == "copilot-responses" {
                assert!(requests[1].contains("copilot-private-replay"));
            }
            if let Some(tokens) = &tokens {
                let requests = tokens.requests.lock().unwrap();
                assert_eq!(requests.len(), if mode == "refresh" { 2 } else { 1 });
                for request in requests.iter() {
                    assert!(request.starts_with("GET /copilot_internal/v2/token "));
                    assert!(request.contains("Authorization: Bearer github-source-canary"));
                    assert!(!request.contains("copilot-api-canary"));
                    assert!(request.contains("Copilot-Integration-Id: rapidlm-test"));
                    assert_eq!(request.split_once("\r\n\r\n").unwrap().1, "");
                }
            }
            drop(requests);
            for file in walk_files(&project.join(".rapidlm")) {
                let bytes = std::fs::read(file).unwrap();
                for secret in ["github-source-canary", "copilot-api-canary"] {
                    assert!(!String::from_utf8_lossy(&bytes).contains(secret));
                }
            }
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

#[cfg(unix)]
#[test]
fn binary_copilot_opaque_token_helpers_and_github_source_helpers_respect_the_cache_boundary() {
    for github in [false, true] {
        let dir = temp_dir(if github {
            "copilot-source-helper"
        } else {
            "copilot-api-helper"
        });
        trust_copilot_project(&dir);
        let helper = dir.join("helper.sh");
        std::fs::write(
            &helper,
            format!(
                "printf x >> \"$HOME/runs\"\nprintf '%s' '{}'\nprintf private-copilot-stderr >&2\n",
                if github {
                    "github-source-canary"
                } else {
                    "tid=copilot-api-canary;exp=fixture:hmac"
                }
            ),
        )
        .unwrap();
        let server = spawn_scripted_server(vec![
            (200, copilot_body("copilot-chat", true)),
            (200, copilot_body("copilot-chat", false)),
        ]);
        let expiry = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        let tokens = github.then(|| spawn_scripted_server(vec![(200, serde_json::json!({"token":"tid=copilot-api-canary;exp=fixture:hmac","expires_at":expiry}).to_string())]));
        let argv = serde_json::to_string(&vec![
            "/bin/sh".to_owned(),
            helper.to_string_lossy().into_owned(),
        ])
        .unwrap();
        let doc = copilot_doc("copilot-chat", server.addr, tokens.as_ref().map(|s| s.addr));
        let key_line = doc.lines().find(|l| l.starts_with("api_key = ")).unwrap();
        let doc = doc.replace(key_line, &format!("credential_process = {argv}"));
        let config = dir.join("config.toml");
        std::fs::write(&config, doc).unwrap();
        let (code, stdout, stderr) = run_rapid(Some(&config), None, &dir);
        assert_eq!(code, Some(0), "{stderr}");
        assert_eq!(stdout.trim(), "Copilot completed");
        assert_eq!(
            std::fs::read_to_string(dir.join("runs")).unwrap(),
            if github { "x" } else { "xx" }
        );
        for secret in [
            "github-source-canary",
            "copilot-api-canary",
            "private-copilot-stderr",
        ] {
            assert!(!format!("{stdout}{stderr}").contains(secret));
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn copilot_exchange_egress_refusal_and_bad_responses_never_connect_inference() {
    use llm_router::providers::dial::{DialGate, DialTarget, ProxyTarget};
    struct RefusePort(u16);
    impl DialGate for RefusePort {
        fn permit(
            &self,
            target: DialTarget<'_>,
            _: Option<&ProxyTarget>,
            addrs: &[std::net::SocketAddr],
        ) -> Result<Vec<std::net::SocketAddr>, llm_router::ProviderError> {
            if target.port == self.0 {
                Err(llm_router::ProviderError::Connection)
            } else {
                Ok(addrs.to_vec())
            }
        }
    }
    let inference = TcpListener::bind("127.0.0.1:0").unwrap();
    inference.set_nonblocking(true).unwrap();
    let token_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    token_listener.set_nonblocking(true).unwrap();
    let doc = copilot_doc(
        "copilot-chat",
        inference.local_addr().unwrap(),
        Some(token_listener.local_addr().unwrap()),
    );
    let active = active_from_doc(&doc);
    let store = InMemoryCredentialStore::new();
    let context = PreservedLiveContext::new("test", Vec::new(), "", "", 1024, 64).unwrap();
    let packet = build_packet(&context, None).unwrap();
    for refused in [
        inference.local_addr().unwrap().port(),
        token_listener.local_addr().unwrap().port(),
    ] {
        let mut model =
            ConfiguredModel::build_with_gate(&active, &store, Some(Arc::new(RefusePort(refused))))
                .unwrap();
        assert_eq!(
            model
                .step(
                    packet.blocks(),
                    &agent_runtime::ModelStepInput::without_tools(1),
                    &CancellationToken::new()
                )
                .unwrap_err(),
            ModelStepError::ProviderFailed {
                cause: FailureCause::Connection
            }
        );
        assert!(inference.accept().is_err());
        assert!(token_listener.accept().is_err());
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    for (status, body) in [
        (
            200,
            serde_json::json!({"token":"expired-canary","expires_at":1}).to_string(),
        ),
        (
            200,
            serde_json::json!({"token":"bad\r\nheader","expires_at":now+3600}).to_string(),
        ),
        (200, "not JSON canary".to_owned()),
        (
            302,
            serde_json::json!({"token":"redirect-canary","expires_at":now+3600}).to_string(),
        ),
        (401, "private-auth-error".to_owned()),
    ] {
        let server = spawn_scripted_server(vec![(status, body)]);
        let mut model = scripted_model(&copilot_doc(
            "copilot-chat",
            inference.local_addr().unwrap(),
            Some(server.addr),
        ));
        assert!(
            model
                .step(
                    packet.blocks(),
                    &agent_runtime::ModelStepInput::without_tools(1),
                    &CancellationToken::new()
                )
                .is_err()
        );
        assert!(inference.accept().is_err());
    }
}

#[test]
fn copilot_live_probe_permits_only_its_configured_inference_and_token_endpoints() {
    let inference = spawn_scripted_server(vec![(200, copilot_body("copilot-responses", false))]);
    let expiry = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let tokens = spawn_scripted_server(vec![(
        200,
        serde_json::json!({"token":"opaque-api-token","expires_at":expiry}).to_string(),
    )]);
    let active = active_from_doc(&copilot_doc(
        "copilot-responses",
        inference.addr,
        Some(tokens.addr),
    ));
    let probe =
        rapid::setup::probe_resolved(&active, &llm_router::CancellationToken::new()).unwrap();
    assert_eq!(probe.result, Ok(()));
    assert_eq!(probe.receipts.len(), 2);
    assert!(probe.receipts.iter().all(|r| r.allowed));
    let requests = inference.requests.lock().unwrap();
    let body: serde_json::Value =
        serde_json::from_str(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(body["max_output_tokens"], 16);
}

#[test]
fn copilot_token_exchange_cancellation_closes_the_socket_without_releasing_partial_credentials() {
    let inference = TcpListener::bind("127.0.0.1:0").unwrap();
    inference.set_nonblocking(true).unwrap();
    let tokens = TcpListener::bind("127.0.0.1:0").unwrap();
    let token_addr = tokens.local_addr().unwrap();
    let (arrived, received) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        let (mut socket, _) = tokens.accept().unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(3)))
            .unwrap();
        let request = read_request(&mut socket);
        assert!(request.starts_with("GET /copilot_internal/v2/token "));
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n{\"token\":\"partial-private-token").unwrap();
        socket.flush().unwrap();
        arrived.send(()).unwrap();
        let mut bytes = [0; 1];
        assert_eq!(
            socket.read(&mut bytes).unwrap(),
            0,
            "cancellation closes refresh socket"
        );
    });
    let mut model = scripted_model(&copilot_doc(
        "copilot-chat",
        inference.local_addr().unwrap(),
        Some(token_addr),
    ));
    let context =
        PreservedLiveContext::new("cancel refresh", Vec::new(), "", "", 1024, 64).unwrap();
    let packet = build_packet(&context, None).unwrap();
    let cancel = CancellationToken::new();
    let watched = cancel.clone();
    let canceller = thread::spawn(move || {
        received
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap();
        watched.cancel();
    });
    assert_eq!(
        model
            .step(
                packet.blocks(),
                &agent_runtime::ModelStepInput::without_tools(1),
                &cancel
            )
            .unwrap_err(),
        ModelStepError::Cancelled
    );
    worker.join().unwrap();
    canceller.join().unwrap();
    assert!(inference.accept().is_err());
}

#[test]
fn copilot_responses_private_replay_survives_restart_and_rejects_changed_integration_binding() {
    let server = spawn_scripted_server(vec![
        (200, copilot_body("copilot-responses", true)),
        (200, copilot_body("copilot-responses", false)),
        (200, copilot_body("copilot-responses", false)),
    ]);
    let doc = copilot_doc("copilot-responses", server.addr, None);
    let context = PreservedLiveContext::new("read a.txt", Vec::new(), "", "", 1024, 64).unwrap();
    let packet = build_packet(&context, None).unwrap();
    let cancel = CancellationToken::new();
    let mut model = scripted_model(&doc);
    let ModelStepOutput::ToolCalls { calls, .. } = model
        .step(
            packet.blocks(),
            &agent_runtime::ModelStepInput::without_tools(1),
            &cancel,
        )
        .unwrap()
    else {
        panic!("tool call")
    };
    let replay = model.take_step_replay().unwrap();
    let history = vec![
        agent_runtime::ToolStepExchange::new(
            calls,
            vec![agent_runtime::ToolStepResult::Succeeded {
                call_id: "read1".to_owned(),
                summary: "restored Copilot tool result".to_owned(),
            }],
        )
        .with_provider_replay(Some(replay)),
    ];
    let serialized =
        serde_json::to_string(&rapid::approvals::SuspendedHistory::from_agent(&history)).unwrap();
    drop(model);
    let restored: rapid::approvals::SuspendedHistory = serde_json::from_str(&serialized).unwrap();
    let history = restored.to_agent();
    for config in [
        doc.clone(),
        doc.replace("rapidlm-test", "other-integration"),
    ] {
        let mut model = scripted_model(&config);
        assert!(
            matches!(model.step(packet.blocks(), &agent_runtime::ModelStepInput::with_history(2, &history, &[]), &cancel).unwrap(), ModelStepOutput::Terminal { text, .. } if text == "Copilot completed")
        );
    }
    let requests = server.requests.lock().unwrap();
    assert!(requests[1].contains("copilot-private-replay"));
    assert!(!requests[2].contains("copilot-private-replay"));
    assert!(requests[2].contains("restored Copilot tool result"));
}

#[cfg(unix)]
#[test]
fn copilot_offline_doctor_and_setup_never_execute_helpers_and_auth_egress_precedes_helper_resolution()
 {
    use llm_router::providers::dial::{DialGate, DialTarget, ProxyTarget};
    struct RefuseAuth(u16);
    impl DialGate for RefuseAuth {
        fn permit(
            &self,
            target: DialTarget<'_>,
            _: Option<&ProxyTarget>,
            addrs: &[std::net::SocketAddr],
        ) -> Result<Vec<std::net::SocketAddr>, llm_router::ProviderError> {
            if target.port == self.0 {
                Err(llm_router::ProviderError::Connection)
            } else {
                Ok(addrs.to_vec())
            }
        }
    }
    let dir = temp_dir("copilot-offline-helper");
    let project = trust_copilot_project(&dir);
    let helper = dir.join("helper.sh");
    let marker = dir.join("ran");
    std::fs::write(
        &helper,
        format!(
            "printf x > '{}'\nprintf github-source-canary\n",
            marker.display()
        ),
    )
    .unwrap();
    let inference = TcpListener::bind("127.0.0.1:0").unwrap();
    inference.set_nonblocking(true).unwrap();
    let tokens = TcpListener::bind("127.0.0.1:0").unwrap();
    tokens.set_nonblocking(true).unwrap();
    let argv = serde_json::to_string(&vec![
        "/bin/sh".to_owned(),
        helper.to_string_lossy().into_owned(),
    ])
    .unwrap();
    let config = dir.join("config.toml");
    let doc = copilot_doc(
        "copilot-chat",
        inference.local_addr().unwrap(),
        Some(tokens.local_addr().unwrap()),
    )
    .replace(
        "api_key = \"github-source-canary\"",
        &format!("credential_process = {argv}"),
    );
    std::fs::write(&config, &doc).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_rapid"))
        .args(["doctor", "--no-sandbox-probe"])
        .current_dir(&project)
        .env("HOME", &dir)
        .env("RAPIDLM_CONFIG", &config)
        .env_remove("RAPIDLM_MODEL")
        .env_remove("RAPIDLM_HOME")
        .env_remove("RAPIDLM_MANAGED_CONFIG")
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&output.stdout).contains("github-source-canary"));
    assert!(!marker.exists());
    assert!(inference.accept().is_err());
    assert!(tokens.accept().is_err());
    let active = active_from_doc(&doc);
    let store = InMemoryCredentialStore::new();
    let mut model = ConfiguredModel::build_with_gate(
        &active,
        &store,
        Some(Arc::new(RefuseAuth(tokens.local_addr().unwrap().port()))),
    )
    .unwrap();
    let context = PreservedLiveContext::new("test", Vec::new(), "", "", 1024, 64).unwrap();
    let packet = build_packet(&context, None).unwrap();
    assert!(
        model
            .step(
                packet.blocks(),
                &agent_runtime::ModelStepInput::without_tools(1),
                &CancellationToken::new()
            )
            .is_err()
    );
    assert!(!marker.exists());
    assert!(inference.accept().is_err());
    assert!(tokens.accept().is_err());
    let dest = dir.join("preview.toml");
    let output = Command::new(env!("CARGO_BIN_EXE_rapid"))
        .args([
            "setup",
            "--preset",
            "copilot-chat",
            "--model",
            "fixture",
            "--copilot-auth",
            "github-token",
            "--key-env",
            "GITHUB_SOURCE",
            "--base-url",
            &format!("http://{}", inference.local_addr().unwrap()),
            "--copilot-token-url",
            &format!(
                "http://{}/copilot_internal/v2/token",
                tokens.local_addr().unwrap()
            ),
            "--dry-run",
            "--non-interactive",
            "--output",
            "json",
        ])
        .current_dir(&project)
        .env("HOME", &dir)
        .env("RAPIDLM_CONFIG", &dest)
        .env_remove("RAPIDLM_MODEL")
        .env_remove("RAPIDLM_HOME")
        .env_remove("RAPIDLM_MANAGED_CONFIG")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!dest.exists());
    assert!(!marker.exists());
    assert!(tokens.accept().is_err());
    let _ = std::fs::remove_dir_all(dir);
}
