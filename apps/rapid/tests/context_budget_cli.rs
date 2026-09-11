//! End-to-end coverage for the model-derived context budget: proves the
//! production `rapid exec` path derives `(context_limit, output_reserve)`
//! from the resolved model's actual configured capabilities, not the
//! previous hard-coded `8192`/`256` constants, and that the derived numbers
//! actually reach the wire — captured from the real outbound HTTP request
//! to a scripted provider, via the same `Content-Length`-framed capture
//! `configured_model_integration.rs` already established, not inferred
//! indirectly.
//!
//! The rendered system prompt's own "## Token budget" section
//! (`crates/agent-runtime/src/prompt_stack.rs::render_system_prompt`) is the
//! observable: "Context window: {context_limit} tokens. Reserve
//! {output_reserve} tokens" — a literal, already-existing wire artifact, not
//! something added for this test.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;

const TERMINAL_BODY: &str = r#"{"choices":[{"message":{"role":"assistant","content":"hello from scripted model"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":5}}"#;

fn config_doc(base_url: &str, extra: &str) -> String {
    format!(
        "[models]\ndefault = \"local\"\n\
         \n\
         [model.local]\n\
         provider = \"openai-compatible\"\n\
         model = \"test-model\"\n\
         base_url = \"{base_url}\"\n\
         api_key = \"scripted-key\"\n\
         {extra}\n"
    )
}

/// One scripted response per accepted connection; full request bodies
/// (headers + `Content-Length`-framed body) are captured in order.
struct ScriptedServer {
    addr: std::net::SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
}

fn spawn_scripted_server(responses: Vec<(u16, String)>) -> ScriptedServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
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

/// Read one HTTP/1.1 request: headers plus the `Content-Length`-framed body.
fn read_request(stream: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
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
    let dir = std::env::temp_dir().join(format!(
        "rapidlm-ctxbudget-{name}-{}-{}",
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

fn git_project(project: &Path) {
    std::fs::create_dir_all(project).expect("project");
    let init = Command::new("git")
        .arg("init")
        .arg("-q")
        .current_dir(project)
        .output()
        .expect("git init");
    assert!(init.status.success(), "git init failed");
}

fn write_config(home: &Path, addr: std::net::SocketAddr, extra: &str) -> PathBuf {
    let config = home.join("config.toml");
    std::fs::write(&config, config_doc(&format!("http://{addr}/v1"), extra)).expect("config");
    config
}

fn run_exec(
    project: &Path,
    home: &Path,
    config: &Path,
    prompt: &str,
) -> (Option<i32>, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_rapid"))
        .args(["exec", "--verbose", prompt])
        .current_dir(project)
        .env("HOME", home)
        .env("RAPIDLM_CONFIG", config)
        .env("RAPIDLM_PERMISSION_MODE", "bypassPermissions")
        .env_remove("RAPIDLM_HOME")
        .env_remove("RAPIDLM_MODEL")
        .output()
        .expect("run rapid exec");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn configured_context_window_reaches_the_actual_wire_request() {
    let home = temp_dir("configured");
    let project = home.join("project");
    git_project(&project);
    let server = spawn_scripted_server(vec![(200, TERMINAL_BODY.to_owned())]);
    let config = write_config(
        &home,
        server.addr,
        "context_window = 200000\nmax_tokens = 8000",
    );

    let (code, _stdout, stderr) = run_exec(&project, &home, &config, "say hi");
    assert_eq!(code, Some(0), "{stderr}");
    assert!(
        stderr.contains(
            "context budget: context_window=200000 output_reserve=8000 source=configured"
        ),
        "verbose diagnostic must report the real configured budget: {stderr}"
    );

    let requests = server.requests.lock().expect("requests");
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].contains("Context window: 200000 tokens. Reserve 8000 tokens"),
        "the configured budget must reach the real outbound request, not just an \
         internal struct: {}",
        requests[0]
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn unconfigured_model_context_window_uses_the_conservative_default_on_the_wire() {
    let home = temp_dir("unconfigured");
    let project = home.join("project");
    git_project(&project);
    let server = spawn_scripted_server(vec![(200, TERMINAL_BODY.to_owned())]);
    let config = write_config(&home, server.addr, "");

    let (code, _stdout, stderr) = run_exec(&project, &home, &config, "say hi");
    assert_eq!(code, Some(0), "{stderr}");
    assert!(
        stderr.contains(
            "context budget: context_window=32768 output_reserve=4096 \
             source=default (context_window/max_tokens unset in model config)"
        ),
        "{stderr}"
    );

    let requests = server.requests.lock().expect("requests");
    assert!(
        requests[0].contains("Context window: 32768 tokens. Reserve 4096 tokens"),
        "an unconfigured context_window must fall back to the conservative built-in \
         default, never the old hard-coded 8192/256: {}",
        requests[0]
    );
    assert!(
        !requests[0].contains("Context window: 8192 tokens"),
        "must never regress to the old hard-coded literal: {}",
        requests[0]
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// The overflow-shaping proof the task calls for: the same oversized
/// mandatory content (the prompt itself — `ContextSource::Goal`, mandatory —
/// see `context-engine::compile::is_mandatory`) is rejected by a small
/// configured budget and accepted by a large one, proving the fix is
/// genuinely model-derived rather than merely a bigger constant. Also
/// exercises the inverse framing the task asks for: a model *smaller* than
/// the old 8192 default now constrains context *before* ever reaching the
/// provider, rather than only the provider ever having a say.
#[test]
fn overflow_regression_small_budget_rejects_what_a_large_budget_accepts() {
    let home = temp_dir("overflow");
    let project = home.join("project");
    git_project(&project);
    // ~12,000 bytes: comfortably under PreservedLiveContext's own
    // MAX_GOAL_BYTES (16 KiB) structural cap, but large enough in token
    // terms to exceed a deliberately tiny model's usable input budget while
    // still fitting trivially under the default (32768) one.
    let long_prompt = "word ".repeat(2400);

    // Small model: context_window well under what the prompt alone needs.
    let small_config = home.join("config-small.toml");
    std::fs::write(
        &small_config,
        config_doc(
            "http://127.0.0.1:1/v1",
            "context_window = 200\nmax_tokens = 50",
        ),
    )
    .expect("small config");
    let (code, _stdout, stderr) = run_exec(&project, &home, &small_config, &long_prompt);
    assert_ne!(
        code,
        Some(0),
        "a model whose usable input budget the prompt alone exceeds must fail, not \
         silently truncate mandatory content: {stderr}"
    );

    // Large (default) model: the identical prompt must actually reach the
    // provider and succeed.
    let server = spawn_scripted_server(vec![(200, TERMINAL_BODY.to_owned())]);
    let large_config = write_config(&home, server.addr, "");
    let (code, _stdout, stderr) = run_exec(&project, &home, &large_config, &long_prompt);
    assert_eq!(
        code,
        Some(0),
        "the identical content must succeed once the model's real budget has room \
         for it: {stderr}"
    );
    let requests = server.requests.lock().expect("requests");
    assert_eq!(
        requests.len(),
        1,
        "the large-budget run must have actually reached the provider"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// The router/fallback architectural question the task calls out
/// explicitly: is context built before or after the effective model is
/// known? Here it is built before (a fallback chain's effective model
/// isn't chosen until request time) — so the pre-built budget must combine
/// the *minimum* context_limit with the *maximum* max_output across every
/// candidate the chain could dispatch to (not the minimum of both — see
/// `context_budget_for`'s own doc comment for why the minimum of max_output
/// is actually unsafe: each backend still sends its own real per-request
/// output cap independently, so under-reserving headroom for a backend
/// with a *larger* real cap risks exactly the overflow this budget exists
/// to prevent). The two backends are deliberately crossed (primary: large
/// context, large output; alternate: small context, small output) so the
/// correct result is a genuine chimera matching neither backend's own real
/// pair — the only fixture shape that can actually tell "correct" apart
/// from "minimum of both" and from "just use the primary."
#[test]
fn fallback_chain_budget_combines_min_context_and_max_output_not_just_the_primary() {
    let home = temp_dir("fallback-budget");
    let project = home.join("project");
    git_project(&project);
    let server = spawn_scripted_server(vec![(200, TERMINAL_BODY.to_owned())]);
    let config = home.join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[models]\n\
             default = \"primary\"\n\
             fallback = [\"alternate\"]\n\
             \n\
             [model.primary]\n\
             provider = \"openai-compatible\"\n\
             model = \"big-context-big-output\"\n\
             base_url = \"http://{}/v1\"\n\
             api_key = \"scripted-key\"\n\
             context_window = 50000\n\
             max_tokens = 6000\n\
             \n\
             [model.alternate]\n\
             provider = \"openai-compatible\"\n\
             model = \"small-context-small-output\"\n\
             base_url = \"http://127.0.0.1:1/v1\"\n\
             api_key = \"scripted-key\"\n\
             context_window = 8000\n\
             max_tokens = 500\n",
            server.addr
        ),
    )
    .expect("config");

    let (code, _stdout, stderr) = run_exec(&project, &home, &config, "say hi");
    assert_eq!(
        code,
        Some(0),
        "the primary must have served this turn successfully: {stderr}"
    );
    assert!(
        stderr.contains("context budget: context_window=8000 output_reserve=6000"),
        "the diagnostic must report the chain-wide (min context_limit, max \
         max_output) pairing — 8000 from the alternate, 6000 from the primary — \
         never the primary's own pair (50000, 6000) or the min/min pair \
         (8000, 500): {stderr}"
    );

    let requests = server.requests.lock().expect("requests");
    assert_eq!(
        requests.len(),
        1,
        "the primary alone must have served this turn"
    );
    assert!(
        requests[0].contains("Context window: 8000 tokens. Reserve 6000 tokens"),
        "the primary's own real request must carry the chain-wide (min context, max \
         output) budget, since the effective model was not yet known when context \
         was built: {}",
        requests[0]
    );
    assert!(
        !requests[0].contains("Context window: 50000 tokens"),
        "the primary must never receive a context_limit sized only for itself, \
         ignoring a smaller alternate the router could still have fallen back to: {}",
        requests[0]
    );
    assert!(
        !requests[0].contains("Reserve 500 tokens"),
        "the primary must never receive an output_reserve smaller than its own real \
         output cap just because an alternate's is smaller — that would under-\
         reserve headroom for the primary's own real, larger request: {}",
        requests[0]
    );
    let _ = std::fs::remove_dir_all(&home);
}
