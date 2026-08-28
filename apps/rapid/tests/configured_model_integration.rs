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
        .step(packet.blocks(), &[], &[], &CancellationToken::new())
        .expect("step");
    match output {
        ModelStepOutput::Terminal { text, tokens } => {
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
        .step(packet.blocks(), &[], &[], &CancellationToken::new())
        .expect("step");
    match output {
        ModelStepOutput::Terminal { text, tokens } => {
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
        .step(packet.blocks(), &[], &[], &CancellationToken::new())
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
        .step(packet.blocks(), &[], &[], &CancellationToken::new())
        .expect("step");
    match output {
        ModelStepOutput::Terminal { text, tokens } => {
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
    assert_eq!(code, Some(1), "stdout: {stdout}");
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
    assert_eq!(code, Some(1), "stdout: {stdout}");
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
    assert_eq!(code, Some(1));
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
        .step(&[], &[], &[], &CancellationToken::new())
        .expect_err("typed fallback");
    assert_eq!(err, ModelStepError::Failed);
}
