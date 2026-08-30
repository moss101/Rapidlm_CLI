//! End-to-end coverage for the Grok-style configured model path.
//!
//! A scripted loopback HTTP server stands in for a local OpenAI-compatible
//! provider (Ollama/LM Studio style). The real `rapid` binary is driven
//! through `rapid exec` with `RAPIDLM_CONFIG` pointing at a temp config, and
//! the typed failure paths (no config, invalid config, unknown override) are
//! asserted against the actual process exit codes and stderr.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
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
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    thread::spawn(move || {
        for (status, body) in responses {
            let (mut stream, _) = match listener.accept() {
                Ok(accepted) => accepted,
                Err(_) => return,
            };
            let raw = read_request(&mut stream);
            captured.lock().expect("capture lock").push(raw);
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

fn scripted_model(
    doc: &str,
) -> ConfiguredModel<'static> {
    // Leak a per-test store: integration tests construct one model per test
    // process-lifetime; the store must outlive the returned adapter.
    let store: &'static InMemoryCredentialStore = Box::leak(Box::new(InMemoryCredentialStore::new()));
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
    assert!(request.contains("Authorization: Bearer scripted-key"), "{request}");
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
    let active = resolve_active(
        &[("GW_API_KEY".to_owned(), "gw-key".to_owned())],
        &config,
    )
    .expect("resolve");
    assert_eq!(active.entry.provider, ConfigProvider::Anthropic);
    assert_eq!(active.credential.source, CredentialSource::EnvVar("GW_API_KEY".to_owned()));
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

fn run_rapid(
    config_path: Option<&PathBuf>,
    model_override: Option<&str>,
    home: &PathBuf,
) -> (Option<i32>, String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rapid"));
    command
        .args(["exec", "ship the scripted feature"])
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

#[test]
fn binary_exec_uses_configured_model_end_to_end() {
    let server = spawn_scripted_server(vec![(200, NON_STREAMING_BODY.to_owned())]);
    let dir = temp_dir("exec-ok");
    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, config_doc(&format!("http://{}/v1", server.addr)))
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
    assert!(requests.is_empty(), "invalid config must not reach a provider");
}

#[test]
fn binary_exec_rejects_unknown_model_override_typed() {
    let server = spawn_scripted_server(vec![(200, NON_STREAMING_BODY.to_owned())]);
    let dir = temp_dir("exec-bad-override");
    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, config_doc(&format!("http://{}/v1", server.addr)))
        .expect("write config");
    let (code, _stdout, stderr) = run_rapid(Some(&config_path), Some("missing-model"), &dir);
    // JsonlExitCode::Usage: an unknown model override is a user-input error.
    assert_eq!(code, Some(2));
    assert!(
        stderr.contains("model configuration error") && stderr.contains("missing-model"),
        "typed override error missing: {stderr}"
    );
    let requests = server.requests.lock().expect("requests");
    assert!(requests.is_empty(), "bad override must not reach a provider");
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
        .step(&[], &agent_runtime::ModelStepInput::without_tools(1), &CancellationToken::new())
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

fn bypass_tools(workspace: &PathBuf) -> rapid::exec_tools::ExecTools {
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
    let mut model = ConfiguredModel::build(&active_from_doc(&doc), store).expect("build");
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
    let active = resolve_active(
        &[("GW_API_KEY".to_owned(), "gw-key".to_owned())],
        &config,
    )
    .expect("resolve");
    let store = InMemoryCredentialStore::new();
    let mut model = ConfiguredModel::build(&active, &store).expect("build");
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
    assert!(!second_request.contains("tool results:"), "{second_request}");
    let _ = std::fs::remove_dir_all(&workspace);
}

// ---------------------------------------------------------------------------
// Real-binary end-to-end: the actual `rapid` binary drives the new coding
// tools through a scripted loopback provider, with the six-mode lattice
// active (acceptEdits auto-approves the file edit; default mode denies it).
// ---------------------------------------------------------------------------

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
        let root = std::fs::canonicalize(&project).expect("canon");
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

#[test]
fn binary_exec_applies_workspace_patch_end_to_end_in_accept_edits_mode() {
    for round in 0..2 {
        let server = spawn_scripted_server(vec![
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
        let content = std::fs::read_to_string(env.project.join("notes.txt"))
            .expect("patched file exists");
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

#[test]
fn binary_exec_in_default_mode_denies_the_patch_and_keeps_disk_intact() {
    let server = spawn_scripted_server(vec![
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
    db_path: &PathBuf,
    args: &[&str],
) -> (Option<i32>, String, String) {
    let mut full_args = vec!["cron".to_owned(), "--db".to_owned(), db_path.display().to_string()];
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
        &["add", "--prompt", "patch notes.txt by replacing alpha with beta", "--schedule", "* * * * *"],
    );
    assert_eq!(add_code, Some(0), "cron add stderr: {add_stderr}");
    assert!(add_stdout.contains("status=active"), "add stdout: {add_stdout}");

    std::thread::sleep(std::time::Duration::from_secs(seconds_until_next_minute_boundary()));

    let (poll_code, poll_stdout, poll_stderr) =
        run_rapid_cron_in(&env.project, &env.home, &config_path, &db_path, &["poll"]);
    assert_eq!(poll_code, Some(0), "cron poll stderr: {poll_stderr}");
    assert!(poll_stdout.contains("fired=1"), "poll stdout: {poll_stdout}");
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
