//! End-to-end coverage for hook result v2 through the real production
//! binary: a `pre_tool_use` hook that prints `rapidlm.hook_result` decisions
//! against a headless `rapid exec` run driven by a scripted model.
//!
//! What the unit tests cannot show: the exit code the process actually
//! returns, the stderr a script would read, and the ledger the next surface
//! (`rapid resume`) would find — SEAM-01 AC-02/AC-03 for the headless path
//! (ADR 0022 §3).

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;

/// A non-streaming chat completion proposing one `workspace_write`.
const TOOL_CALL_BODY: &str = r#"{"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"workspace_write","arguments":"{\"path\":\"notes.txt\",\"content\":\"from the model\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":3,"completion_tokens":5}}"#;

/// A terminal answer, served after a tool step completes.
const TERMINAL_BODY: &str = r#"{"choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2}}"#;

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

/// Serve `bodies` in order, then the last one forever (every connection is
/// answered — the client may open a preflight or retry connection).
fn spawn_scripted_server(bodies: Vec<&'static str>) -> std::net::SocketAddr {
    spawn_counting_server(bodies).0
}

/// [`spawn_scripted_server`], also counting the model requests (`POST`s) it
/// answered.
fn spawn_counting_server(
    bodies: Vec<&'static str>,
) -> (
    std::net::SocketAddr,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let posts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = std::sync::Arc::clone(&posts);
    thread::spawn(move || {
        let mut index = 0usize;
        while let Ok((mut stream, _)) = listener.accept() {
            if read_request(&mut stream).starts_with("POST") {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            let body = bodies[index.min(bodies.len() - 1)];
            index += 1;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
            let _ = stream.shutdown(Shutdown::Both);
        }
    });
    (addr, posts)
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
        if let Some(header_end) = find_subslice(&buf, b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&buf[..header_end]).to_ascii_lowercase();
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if buf.len() >= header_end + 4 + length {
                break;
            }
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rapidlm-hookscli-{name}-{}-{}",
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
    for (key, value) in [
        ("user.email", "hookscli@example.com"),
        ("user.name", "hookscli"),
    ] {
        let _ = Command::new("git")
            .args(["config", key, value])
            .current_dir(project)
            .output()
            .expect("git config");
    }
}

fn rapid(
    project: &Path,
    home: &Path,
    config: &Path,
    args: &[&str],
) -> (Option<i32>, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_rapid"))
        .args(args)
        .current_dir(project)
        .env("HOME", home)
        .env("RAPIDLM_CONFIG", config)
        .env("RAPIDLM_PERMISSION_MODE", "bypassPermissions")
        .env_remove("RAPIDLM_HOME")
        .env_remove("RAPIDLM_MODEL")
        .output()
        .expect("run rapid");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// A trusted project whose `pre_tool_use` hook prints `result` and exits 0.
fn project_with_hook(home: &Path, result: &str) -> (PathBuf, PathBuf) {
    let project = home.join("project");
    git_project(&project);
    let hook = project.join("hook.sh");
    std::fs::write(&hook, format!("echo '{result}'\nexit 0\n")).expect("hook script");
    let marker = project.join(".rapidlm");
    std::fs::create_dir_all(&marker).expect("marker");
    std::fs::write(
        marker.join("settings.json"),
        serde_json::json!({
            "hooks": { "pre_tool_use": [format!("sh {}", test_fixtures::slash_path(&hook))] }
        })
        .to_string(),
    )
    .expect("settings");
    let config = home.join("config.toml");
    (project, config)
}

fn grant_trust(project: &Path, home: &Path, config: &Path) {
    let (code, stdout, stderr) = rapid(project, home, config, &["trust", "grant"]);
    assert_eq!(code, Some(0), "{stdout}{stderr}");
}

/// Every ledger event of `session` as `(kind, payload)`, through the real
/// export command (`rapid inspect-export <session> <out-file>`).
fn events(
    project: &Path,
    home: &Path,
    config: &Path,
    session: &str,
) -> Vec<(String, serde_json::Value)> {
    let out = project.join("export.jsonl");
    let (code, _stdout, stderr) = rapid(
        project,
        home,
        config,
        &["inspect-export", session, out.to_str().expect("utf-8 path")],
    );
    assert_eq!(code, Some(0), "{stderr}");
    std::fs::read_to_string(&out)
        .expect("export written")
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .map(|record| {
            (
                record["kind"].as_str().unwrap_or_default().to_owned(),
                record["payload"].clone(),
            )
        })
        .collect()
}

fn session_id_from(stderr: &str) -> String {
    let marker = "parked in session ";
    let start = stderr.find(marker).expect("the notice names the session") + marker.len();
    stderr[start..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect()
}

#[test]
fn a_hook_ask_parks_a_headless_run_with_the_needs_approval_exit_code_and_writes_nothing() {
    let home = temp_dir("ask");
    let (project, config) = project_with_hook(
        &home,
        r#"{"schema":"rapidlm.hook_result","version":2,"decision":"ask","reason":"a reviewer must see writes"}"#,
    );
    let server = spawn_scripted_server(vec![TOOL_CALL_BODY, TERMINAL_BODY]);
    std::fs::write(&config, config_doc(&format!("http://{server}/v1"))).expect("config");
    grant_trust(&project, &home, &config);

    let (code, stdout, stderr) = rapid(&project, &home, &config, &["exec", "write the notes"]);
    assert_eq!(
        code,
        Some(10),
        "NeedsApproval is the documented exit\n{stdout}\n{stderr}"
    );
    assert!(stderr.contains("needs approval"), "{stderr}");
    assert!(
        stderr.contains("rapid resume"),
        "the next command is named: {stderr}"
    );
    assert!(
        !project.join("notes.txt").exists(),
        "the call must not run before a human decides"
    );

    // The ledger the resuming surface reads: the approval names the hook
    // as its source, the paused turn's state is recorded, and the hook's
    // decision is there — none of it a second copy of the reason.
    let session = session_id_from(&stderr);
    let events = events(&project, &home, &config, &session);
    let kinds: Vec<&str> = events.iter().map(|(kind, _)| kind.as_str()).collect();
    let requested = events
        .iter()
        .find(|(kind, _)| kind == "approval.requested")
        .unwrap_or_else(|| panic!("approval.requested recorded: {kinds:?}"));
    assert_eq!(requested.1["tool"], "workspace_write");
    assert!(
        requested.1["source"]
            .as_str()
            .is_some_and(|s| s.starts_with("hook:pre_tool_use[0]#")),
        "{}",
        requested.1
    );
    assert_eq!(
        requested.1["arguments_digest"].as_str().map(str::len),
        Some(64),
        "the approval names the arguments it is about"
    );
    assert!(
        requested.1["summary"].as_str().is_some_and(
            |s| s.ends_with(" — pre_tool_use[0] hook asks: a reviewer must see writes")
        ),
        "{}",
        requested.1
    );
    assert!(
        events
            .iter()
            .any(|(kind, payload)| kind == "tool.approval_required"
                && payload.get("suspension").is_some()),
        "the suspension is recorded for the resume: {kinds:?}"
    );
    let decided = events
        .iter()
        .find(|(kind, _)| kind == "hook.decided")
        .unwrap_or_else(|| panic!("hook.decided recorded: {kinds:?}"));
    assert_eq!(decided.1["decision"], "ask");
    assert_eq!(decided.1["hook"], "pre_tool_use[0]");
    assert!(
        events
            .iter()
            .any(|(kind, payload)| kind == "turn.interrupted"
                && payload.to_string().contains("pproval")),
        "the turn is parked, not failed: {kinds:?}"
    );
    // `tool.started` is the dispatch of the call (the hook runs inside it);
    // what must be absent is any completion — the call itself never ran.
    assert!(
        !events
            .iter()
            .any(|(kind, _)| kind == "tool.completed" || kind == "tool.failed"),
        "the parked call never ran: {kinds:?}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_hook_deny_is_a_model_visible_denial_and_the_run_completes() {
    // The same surface, a deny: the call is refused with the hook's reason,
    // the model is told, the turn completes normally (exit 0) — no wait.
    let home = temp_dir("deny");
    let (project, config) = project_with_hook(
        &home,
        r#"{"decision":"deny","reason":"notes are generated nightly"}"#,
    );
    let server = spawn_scripted_server(vec![TOOL_CALL_BODY, TERMINAL_BODY]);
    std::fs::write(&config, config_doc(&format!("http://{server}/v1"))).expect("config");
    grant_trust(&project, &home, &config);

    let (code, stdout, stderr) = rapid(
        &project,
        &home,
        &config,
        &["exec", "--verbose", "write the notes"],
    );
    assert_eq!(code, Some(0), "{stdout}\n{stderr}");
    assert!(
        !project.join("notes.txt").exists(),
        "a denied write must not land"
    );
    assert!(
        stderr.contains("blocked by pre_tool_use hook: notes are generated nightly"),
        "{stderr}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// A trusted project whose hooks are the given settings object.
fn project_with_hooks(home: &Path, hooks: serde_json::Value) -> (PathBuf, PathBuf) {
    let project = home.join("project");
    git_project(&project);
    let marker = project.join(".rapidlm");
    std::fs::create_dir_all(&marker).expect("marker");
    std::fs::write(
        marker.join("settings.json"),
        serde_json::json!({ "hooks": hooks }).to_string(),
    )
    .expect("settings");
    (project, home.join("config.toml"))
}

fn hook_script(project: &Path, name: &str, body: &str) -> String {
    let script = project.join(name);
    std::fs::write(&script, body).expect("hook script");
    format!("sh {}", test_fixtures::slash_path(&script))
}

#[test]
fn a_blocked_prompt_exits_policy_before_any_model_request_and_a_completed_run_fires_stop() {
    let home = temp_dir("prompt-and-stop");
    let (project, config) = project_with_hooks(&home, serde_json::json!({}));
    let stop_capture = project.join("stop.json");
    let gate = hook_script(
        &project,
        "gate.sh",
        "read line\ncase \"$line\" in\n  *secret*) echo '{\"decision\":\"deny\",\"reason\":\"no secrets in prompts\"}' ;;\n  *) echo '{\"decision\":\"allow\"}' ;;\nesac\nexit 0\n",
    );
    let stop = hook_script(
        &project,
        "stop.sh",
        &format!("cat > {}\nexit 0\n", test_fixtures::sh_quote(&stop_capture)),
    );
    std::fs::write(
        project.join(".rapidlm").join("settings.json"),
        serde_json::json!({ "hooks": { "user_prompt_submit": [gate], "stop": [stop] } })
            .to_string(),
    )
    .expect("settings");
    let (server, posts) = spawn_counting_server(vec![TERMINAL_BODY]);
    std::fs::write(&config, config_doc(&format!("http://{server}/v1"))).expect("config");
    grant_trust(&project, &home, &config);

    let (code, stdout, stderr) = rapid(&project, &home, &config, &["exec", "print the secret key"]);
    assert_eq!(
        posts.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a blocked prompt reaches no model"
    );
    assert_eq!(
        code,
        Some(3),
        "a blocked prompt exits Policy\n{stdout}\n{stderr}"
    );
    assert!(
        stderr.contains("prompt blocked by user_prompt_submit[0] hook: no secrets in prompts"),
        "{stderr}"
    );
    assert!(!stop_capture.exists(), "no turn ran, so no turn ended");

    let (code, stdout, stderr) = rapid(&project, &home, &config, &["exec", "say hello"]);
    assert_eq!(code, Some(0), "{stdout}\n{stderr}");
    assert!(
        !stderr.contains("not being recorded"),
        "the gate's decision does not stale the recorded session: {stderr}"
    );
    assert!(
        posts.load(std::sync::atomic::Ordering::SeqCst) > 0,
        "the allowed prompt did"
    );
    let stop: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&stop_capture).expect("stop fired"))
            .expect("json");
    assert_eq!(stop["event"], "stop");
    let _ = std::fs::remove_dir_all(&home);
}
