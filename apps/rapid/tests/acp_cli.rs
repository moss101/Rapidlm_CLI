//! `rapid acp` end to end: the real binary serving an editor the test plays
//! over the process's stdio, with a scripted model on loopback.
//!
//! What the unit tests cannot show: that a prompt survives every approval
//! its turn raises — the editor's answers route to the waiting prompt, the
//! serve stays up, the prompt ends with its real stop reason, and a cancel
//! during an approval wait ends it — and that each prompt streams only its
//! own turn's events, never the previous turn's again.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Generous: a scripted turn finishes in well under a second; the bound only
/// keeps a regression from hanging the suite.
const DEADLINE: Duration = Duration::from_secs(120);

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

/// A non-streaming chat completion proposing one `workspace_write`.
fn write_call(id: &str, path: &str) -> String {
    write_calls(&[(id, path)])
}

/// A non-streaming chat completion proposing one `workspace_write` per
/// `(id, path)`, all in one model step.
fn write_calls(calls: &[(&str, &str)]) -> String {
    let tool_calls: Vec<Value> = calls
        .iter()
        .map(|(id, path)| {
            json!({
                "id": id,
                "type": "function",
                "function": {
                    "name": "workspace_write",
                    "arguments": json!({ "path": path, "content": "from the model" }).to_string(),
                },
            })
        })
        .collect();
    json!({
        "choices": [{
            "message": { "role": "assistant", "content": null, "tool_calls": tool_calls },
            "finish_reason": "tool_calls",
        }],
        "usage": { "prompt_tokens": 3, "completion_tokens": 5 },
    })
    .to_string()
}

/// A non-streaming chat completion proposing one call of `tool`.
#[cfg(unix)]
fn tool_call(id: &str, tool: &str, arguments: Value) -> String {
    json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": id,
                    "type": "function",
                    "function": { "name": tool, "arguments": arguments.to_string() },
                }],
            },
            "finish_reason": "tool_calls",
        }],
        "usage": { "prompt_tokens": 3, "completion_tokens": 5 },
    })
    .to_string()
}

/// A `sleep` duration for test `test` that no other process uses — not
/// another test here, not another run — so its command is found by argv.
#[cfg(unix)]
fn sleep_marker(test: u32) -> String {
    format!("3{test}.{}", std::process::id())
}

/// Whether a `sleep <seconds>` process is running.
#[cfg(unix)]
fn sleeping(seconds: &str) -> bool {
    Command::new("pgrep")
        .args(["-f", &format!("sleep {seconds}")])
        .output()
        .is_ok_and(|out| out.status.success())
}

/// A terminal answer.
fn answer(text: &str) -> String {
    json!({
        "choices": [{
            "message": { "role": "assistant", "content": text },
            "finish_reason": "stop",
        }],
        "usage": { "prompt_tokens": 3, "completion_tokens": 2 },
    })
    .to_string()
}

/// How many tool results the model has been shown so far.
fn tool_results(request: &str) -> usize {
    request.matches("\"tool_call_id\"").count()
}

/// Answer every connection with `respond(request)` — decided by what the
/// request carries, never by arrival order, so a preflight or retry
/// connection cannot shift the script.
fn spawn_model(respond: fn(&str) -> String) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    thread::spawn(move || {
        while let Ok((mut stream, _)) = listener.accept() {
            let body = respond(&read_request(&mut stream));
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
    addr
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = stream.read(&mut chunk).unwrap_or(0);
        if read == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..read]);
        if let Some(header_end) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
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

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn temp_dir(name: &str) -> Dir {
    let dir = std::env::temp_dir().join(format!(
        "rapidlm-acpcli-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    Dir(dir)
}

fn rapid(project: &Path, home: &Path, config: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rapid"));
    command
        .current_dir(project)
        .env("HOME", home)
        .env("RAPIDLM_CONFIG", config)
        .env_remove("RAPIDLM_HOME")
        .env_remove("RAPIDLM_MODEL")
        .env_remove("RAPIDLM_PERMISSION_MODE");
    command
}

/// A trusted git project whose model is `respond`, under the default
/// permission mode (a workspace write asks). Returns `(project, config)`.
fn trusted_project(home: &Path, respond: fn(&str) -> String) -> (PathBuf, PathBuf) {
    let project = home.join("project");
    std::fs::create_dir_all(project.join(".rapidlm")).expect("project");
    let init = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&project)
        .output()
        .expect("git init");
    assert!(init.status.success(), "git init failed");
    let config = home.join("config.toml");
    let addr = spawn_model(respond);
    std::fs::write(&config, config_doc(&format!("http://{addr}/v1"))).expect("config");
    let trust = rapid(&project, home, &config)
        .args(["trust", "grant"])
        .output()
        .expect("trust grant");
    assert!(
        trust.status.success(),
        "{}{}",
        String::from_utf8_lossy(&trust.stdout),
        String::from_utf8_lossy(&trust.stderr)
    );
    (project, config)
}

/// The editor: writes frames to `rapid acp`'s stdin, reads its stdout.
struct Editor {
    child: Child,
    stdin: Option<ChildStdin>,
    frames: Receiver<Value>,
    stderr: PathBuf,
}

impl Editor {
    fn spawn(project: &Path, home: &Path, config: &Path) -> Self {
        let stderr = home.join("acp.stderr");
        let mut child = rapid(project, home, config)
            .arg("acp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(&stderr).expect("stderr file"))
            .spawn()
            .expect("spawn rapid acp");
        let stdout = child.stdout.take().expect("stdout");
        let (tx, frames) = std::sync::mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                    return;
                };
                if tx.send(frame).is_err() {
                    return;
                }
            }
        });
        let stdin = child.stdin.take();
        Self {
            child,
            stdin,
            frames,
            stderr,
        }
    }

    fn send(&mut self, frame: Value) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        let mut line = frame.to_string();
        line.push('\n');
        stdin.write_all(line.as_bytes()).expect("write frame");
        stdin.flush().expect("flush frame");
    }

    fn request(&mut self, id: i64, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
    }

    /// The next frame, or `None` once the serve's stdout closed or the
    /// deadline passed.
    fn next(&self, deadline: Instant) -> Option<Value> {
        let left = deadline.checked_duration_since(Instant::now())?;
        self.frames.recv_timeout(left).ok()
    }

    /// Frames until the result of request `id`.
    fn until_result(&self, id: i64) -> Value {
        let deadline = Instant::now() + DEADLINE;
        while let Some(frame) = self.next(deadline) {
            if frame["id"] == id && frame.get("method").is_none() {
                return frame;
            }
        }
        panic!(
            "no response to request {id}; stderr: {}",
            self.stderr_text()
        );
    }

    fn stderr_text(&self) -> String {
        std::fs::read_to_string(&self.stderr).unwrap_or_default()
    }

    /// `initialize` + `session/new`; the new session's id.
    fn open_session(&mut self, project: &Path) -> String {
        self.request(1, "initialize", json!({ "protocolVersion": 1 }));
        let init = self.until_result(1);
        assert!(init.get("result").is_some(), "{init}");
        self.request(
            2,
            "session/new",
            json!({ "cwd": project.display().to_string(), "mcpServers": [] }),
        );
        let created = self.until_result(2);
        created["result"]["sessionId"]
            .as_str()
            .unwrap_or_else(|| panic!("no session id: {created}"))
            .to_owned()
    }

    /// Send `session/prompt` as request `id`.
    fn start_prompt(&mut self, id: i64, session: &str, text: &str) {
        self.request(
            id,
            "session/prompt",
            json!({ "sessionId": session, "prompt": [{ "type": "text", "text": text }] }),
        );
    }

    /// Frames until the next `session/request_permission`, which is
    /// returned (not answered).
    fn until_permission(&self) -> Value {
        let deadline = Instant::now() + DEADLINE;
        while let Some(frame) = self.next(deadline) {
            if frame["method"] == "session/request_permission" {
                return frame;
            }
        }
        panic!("no permission request; stderr: {}", self.stderr_text());
    }

    /// Answer the permission request `request` with `result`.
    fn answer_permission(&mut self, request: &Value, result: Value) {
        self.send(json!({ "jsonrpc": "2.0", "id": request["id"].clone(), "result": result }));
    }

    /// Send `session/prompt` as request `id` and play the editor until its
    /// response (see [`Editor::play`]).
    fn prompt(&mut self, id: i64, session: &str, text: &str) -> Vec<Value> {
        self.start_prompt(id, session, text);
        self.play(id)
    }

    /// Play the editor until the response to request `id`: every
    /// `session/request_permission` is answered "allow once" in the
    /// protocol's response shape. Returns every frame seen, the response
    /// last. Panics if the serve exits first.
    fn play(&mut self, id: i64) -> Vec<Value> {
        let deadline = Instant::now() + DEADLINE;
        let mut seen = Vec::new();
        while let Some(frame) = self.next(deadline) {
            seen.push(frame.clone());
            if frame["method"] == "session/request_permission" {
                self.send(json!({
                    "jsonrpc": "2.0",
                    "id": frame["id"].clone(),
                    "result": { "outcome": { "outcome": "selected", "optionId": "allow-once" } },
                }));
            } else if frame["id"] == id && frame.get("method").is_none() {
                return seen;
            }
        }
        panic!(
            "prompt {id} never answered; frames: {seen:#?}; stderr: {}",
            self.stderr_text()
        );
    }

    /// Disconnect (close stdin) and wait for the exit code.
    fn finish(mut self) -> Option<i32> {
        drop(self.stdin.take());
        let deadline = Instant::now() + DEADLINE;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return status.code(),
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(20));
                }
                _ => {
                    let _ = self.child.kill();
                    panic!("rapid acp did not exit after disconnect");
                }
            }
        }
    }
}

/// Every ledger event of `session` as `(kind, payload)`, through the real
/// export command, read once `rapid acp` has exited.
fn ledger(project: &Path, home: &Path, config: &Path, session: &str) -> Vec<(String, Value)> {
    let out = home.join(format!("export-{session}.jsonl"));
    let export = rapid(project, home, config)
        .args(["inspect-export", session, out.to_str().expect("utf-8 path")])
        .output()
        .expect("inspect-export");
    assert!(
        export.status.success(),
        "{}",
        String::from_utf8_lossy(&export.stderr)
    );
    std::fs::read_to_string(&out)
        .expect("export file")
        .lines()
        .map(|line| {
            let event: Value = serde_json::from_str(line).expect("export line");
            (
                event["kind"].as_str().unwrap_or_default().to_owned(),
                event["payload"].clone(),
            )
        })
        .collect()
}

fn count_kind(events: &[(String, Value)], kind: &str) -> usize {
    events.iter().filter(|(event, _)| event == kind).count()
}

fn permission_requests(frames: &[Value]) -> usize {
    frames
        .iter()
        .filter(|frame| frame["method"] == "session/request_permission")
        .count()
}

#[test]
fn every_permission_request_of_one_prompt_reaches_the_prompt_and_it_completes() {
    // Two writes, each asking: the second is raised by the continuation the
    // first answer resumed. Then the answer.
    fn model(request: &str) -> String {
        match tool_results(request) {
            0 => write_call("call_1", "first.txt"),
            1 => write_call("call_2", "second.txt"),
            _ => answer("both files handled"),
        }
    }
    let home = temp_dir("two-permissions");
    let (project, config) = trusted_project(&home.0, model);
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);

    let frames = editor.prompt(3, &session, "write both files");
    let stderr = editor.stderr_text();
    assert_eq!(
        permission_requests(&frames),
        2,
        "{frames:#?}\nstderr: {stderr}"
    );
    let response = frames.last().expect("the prompt's response");
    assert_eq!(
        response["result"]["stopReason"], "end_turn",
        "{frames:#?}\nstderr: {stderr}"
    );
    // The serve is still up: a clean disconnect exits 0, not the protocol
    // failure a misrouted answer causes.
    assert_eq!(editor.finish(), Some(0), "stderr: {stderr}");
}

/// One write that asks, then an answer — a prompt that pauses once.
fn one_write(request: &str) -> String {
    match tool_results(request) {
        0 => write_call("call_1", "first.txt"),
        _ => answer("resumed"),
    }
}

#[test]
fn a_cancel_while_the_turn_waits_on_an_approval_ends_the_prompt_and_nothing_resumes() {
    let home = temp_dir("cancel-while-waiting");
    let (project, config) = trusted_project(&home.0, one_write);
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);

    editor.start_prompt(3, &session, "write it");
    // Surfaced only once the turn's pause is recorded: the cancel meets a
    // session with no live turn — the case the kernel cannot interrupt.
    let permission = editor.until_permission();
    // The protocol's order: cancel, then answer the outstanding request.
    editor.send(json!({
        "jsonrpc": "2.0",
        "method": "session/cancel",
        "params": { "sessionId": session },
    }));
    editor.answer_permission(
        &permission,
        json!({ "outcome": { "outcome": "cancelled" } }),
    );
    let frames = editor.play(3);
    let stderr = editor.stderr_text();
    assert_eq!(
        frames.last().expect("response")["result"]["stopReason"],
        "cancelled",
        "{frames:#?}\nstderr: {stderr}"
    );
    // The answer to the cancelled prompt's request is dropped, not fatal.
    assert_eq!(editor.finish(), Some(0), "stderr: {stderr}");
    // Nothing was decided and nothing resumed: the approval stays pending.
    let events = ledger(&project, &home.0, &config, &session);
    assert_eq!(count_kind(&events, "approval.resolved"), 0, "{events:#?}");
    assert_eq!(count_kind(&events, "turn.started"), 1, "{events:#?}");
}

#[test]
fn a_disconnect_while_the_turn_waits_on_an_approval_lets_the_serve_exit() {
    let home = temp_dir("disconnect-while-waiting");
    let (project, config) = trusted_project(&home.0, one_write);
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);

    editor.start_prompt(3, &session, "write it");
    let _unanswered = editor.until_permission();
    let stderr = editor.stderr_text();
    assert_eq!(editor.finish(), Some(0), "stderr: {stderr}");
    let events = ledger(&project, &home.0, &config, &session);
    assert_eq!(count_kind(&events, "approval.resolved"), 0, "{events:#?}");
}

#[test]
fn an_error_answer_decides_nothing_and_ends_the_prompt() {
    let home = temp_dir("error-answer");
    let (project, config) = trusted_project(&home.0, one_write);
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);

    editor.start_prompt(3, &session, "write it");
    let permission = editor.until_permission();
    editor.send(json!({
        "jsonrpc": "2.0",
        "id": permission["id"].clone(),
        "error": { "code": -32601, "message": "Method not found" },
    }));
    let frames = editor.play(3);
    let stderr = editor.stderr_text();
    assert_eq!(
        frames.last().expect("response")["result"]["stopReason"],
        "refusal",
        "{frames:#?}\nstderr: {stderr}"
    );
    assert_eq!(editor.finish(), Some(0), "stderr: {stderr}");
    // No decision is recorded that nobody made, and nothing resumed.
    let events = ledger(&project, &home.0, &config, &session);
    assert_eq!(count_kind(&events, "approval.resolved"), 0, "{events:#?}");
    assert_eq!(count_kind(&events, "turn.started"), 1, "{events:#?}");
}

#[test]
fn a_second_prompt_while_the_first_waits_is_refused() {
    let home = temp_dir("overlapping-prompt");
    let (project, config) = trusted_project(&home.0, one_write);
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);

    editor.start_prompt(3, &session, "write it");
    let permission = editor.until_permission();
    editor.start_prompt(4, &session, "and meanwhile this");
    let refused = editor.until_result(4);
    let stderr = editor.stderr_text();
    assert_eq!(
        refused["error"]["code"], -32600,
        "{refused}\nstderr: {stderr}"
    );
    // The first prompt is untouched: answered, it resumes and completes.
    editor.answer_permission(
        &permission,
        json!({ "outcome": { "outcome": "selected", "optionId": "allow-once" } }),
    );
    let frames = editor.play(3);
    assert_eq!(
        frames.last().expect("response")["result"]["stopReason"],
        "end_turn",
        "{frames:#?}\nstderr: {stderr}"
    );
    // Its end frees the session: the next prompt is accepted at once.
    let next = editor.prompt(5, &session, "now this");
    assert_eq!(
        next.last().expect("response")["result"]["stopReason"],
        "end_turn",
        "{next:#?}"
    );
    assert_eq!(editor.finish(), Some(0), "stderr: {stderr}");
    let events = ledger(&project, &home.0, &config, &session);
    assert!(
        !events.iter().any(
            |(kind, payload)| kind == "turn.started" && payload["text"] == "and meanwhile this"
        ),
        "the refused prompt never became a turn: {events:#?}"
    );
}

#[test]
fn only_the_approval_the_turn_suspended_on_is_asked() {
    // One model step proposes two writes; both ask, and the turn suspends
    // on the first. Only that one can be resumed, so only it is surfaced.
    fn model(request: &str) -> String {
        match tool_results(request) {
            0 => write_calls(&[("call_a", "a.txt"), ("call_b", "b.txt")]),
            _ => answer("done"),
        }
    }
    let home = temp_dir("parallel-asks");
    let (project, config) = trusted_project(&home.0, model);
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);

    editor.start_prompt(3, &session, "write both");
    let permission = editor.until_permission();
    assert_eq!(
        permission["params"]["toolCall"]["toolCallId"], "call_a",
        "{permission}"
    );
    // Unanswered, nothing else is asked: the turn is paused on this one.
    let settle = Instant::now() + Duration::from_secs(1);
    let mut meanwhile = Vec::new();
    while let Some(frame) = editor.next(settle) {
        meanwhile.push(frame);
    }
    assert_eq!(permission_requests(&meanwhile), 0, "{meanwhile:#?}");
    editor.answer_permission(
        &permission,
        json!({ "outcome": { "outcome": "selected", "optionId": "allow-once" } }),
    );
    let frames = editor.play(3);
    let stderr = editor.stderr_text();
    assert_eq!(
        frames.last().expect("response")["result"]["stopReason"],
        "end_turn",
        "{frames:#?}\nstderr: {stderr}"
    );
    assert_eq!(editor.finish(), Some(0), "stderr: {stderr}");
}

/// How many of `frames` carry `needle` anywhere.
fn mentions(frames: &[Value], needle: &str) -> usize {
    frames
        .iter()
        .filter(|frame| frame.to_string().contains(needle))
        .count()
}

#[test]
fn the_next_prompt_streams_only_its_own_turn() {
    // Each prompt proposes one write of its own (one approval each), then
    // finishes. The second must stream its own tool activity and approval,
    // and none of the first's.
    fn model(request: &str) -> String {
        if request.contains("PROMPT-TWO") {
            if request.contains("call_second") {
                answer("second done")
            } else {
                write_call("call_second", "second.txt")
            }
        } else if tool_results(request) == 0 {
            write_call("call_first", "first.txt")
        } else {
            answer("first done")
        }
    }
    let home = temp_dir("two-prompts");
    let (project, config) = trusted_project(&home.0, model);
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);

    let first = editor.prompt(3, &session, "PROMPT-ONE write it");
    let stderr = editor.stderr_text();
    // What the second prompt must not repeat, seen once where it belongs.
    assert_eq!(
        permission_requests(&first),
        1,
        "{first:#?}\nstderr: {stderr}"
    );
    assert!(mentions(&first, "call_first") > 0, "{first:#?}");
    assert_eq!(
        first.last().expect("response")["result"]["stopReason"],
        "end_turn"
    );

    let second = editor.prompt(4, &session, "PROMPT-TWO write another");
    let stderr = editor.stderr_text();
    assert_eq!(
        mentions(&second, "call_first"),
        0,
        "the first turn's tool activity replayed: {second:#?}\nstderr: {stderr}"
    );
    let asked: Vec<&Value> = second
        .iter()
        .filter(|frame| frame["method"] == "session/request_permission")
        .collect();
    assert_eq!(
        asked.len(),
        1,
        "only the second turn's own approval: {second:#?}"
    );
    assert_eq!(asked[0]["params"]["toolCall"]["toolCallId"], "call_second");
    assert!(mentions(&second, "call_second") > 1, "{second:#?}");
    assert_eq!(
        second.last().expect("response")["result"]["stopReason"],
        "end_turn"
    );
    assert_eq!(editor.finish(), Some(0), "stderr: {stderr}");
}

/// The `reason` of every `turn.interrupted` in `events`, in order.
fn interruptions(events: &[(String, Value)]) -> Vec<String> {
    events
        .iter()
        .filter(|(kind, _)| kind == "turn.interrupted")
        .map(|(_, payload)| payload["reason"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[test]
fn a_disconnect_while_the_turn_runs_interrupts_it() {
    // The model takes its time; the editor leaves while it thinks. Left
    // running, the turn would go on to propose a write and ask.
    static THINKING: AtomicBool = AtomicBool::new(false);
    fn model(request: &str) -> String {
        match tool_results(request) {
            0 => {
                THINKING.store(true, Ordering::SeqCst);
                thread::sleep(Duration::from_secs(2));
                write_call("call_1", "first.txt")
            }
            _ => answer("done"),
        }
    }
    let home = temp_dir("disconnect-while-running");
    let (project, config) = trusted_project(&home.0, model);
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);

    editor.start_prompt(3, &session, "write it");
    let deadline = Instant::now() + DEADLINE;
    while !THINKING.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the model was never asked");
        thread::sleep(Duration::from_millis(10));
    }
    let stderr = editor.stderr_text();
    assert_eq!(editor.finish(), Some(0), "stderr: {stderr}");
    let events = ledger(&project, &home.0, &config, &session);
    assert_eq!(
        interruptions(&events),
        ["client_requested"],
        "interrupted at the disconnect: {events:#?}"
    );
    assert_eq!(count_kind(&events, "approval.requested"), 0, "{events:#?}");
}

#[test]
fn a_prompt_straight_after_a_cancel_is_accepted() {
    fn model(request: &str) -> String {
        if request.contains("PROMPT-TWO") {
            answer("second")
        } else {
            one_write(request)
        }
    }
    let home = temp_dir("prompt-after-cancel");
    let (project, config) = trusted_project(&home.0, model);
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);

    editor.start_prompt(3, &session, "write it");
    let permission = editor.until_permission();
    // Cancel and prompt again at once, without waiting for the cancelled
    // prompt's response; then answer its request as the protocol asks.
    editor.send(json!({
        "jsonrpc": "2.0",
        "method": "session/cancel",
        "params": { "sessionId": session },
    }));
    editor.start_prompt(4, &session, "PROMPT-TWO instead");
    editor.answer_permission(
        &permission,
        json!({ "outcome": { "outcome": "cancelled" } }),
    );
    let deadline = Instant::now() + DEADLINE;
    let (mut first, mut second) = (None, None);
    while first.is_none() || second.is_none() {
        let frame = editor
            .next(deadline)
            .unwrap_or_else(|| panic!("a prompt never answered; stderr: {}", editor.stderr_text()));
        if frame.get("method").is_none() && frame["id"] == 3 {
            first = Some(frame);
        } else if frame.get("method").is_none() && frame["id"] == 4 {
            second = Some(frame);
        }
    }
    let stderr = editor.stderr_text();
    let (first, second) = (first.expect("3"), second.expect("4"));
    assert_eq!(
        first["result"]["stopReason"], "cancelled",
        "{first}\nstderr: {stderr}"
    );
    assert_eq!(
        second["result"]["stopReason"], "end_turn",
        "{second}\nstderr: {stderr}"
    );
    assert_eq!(editor.finish(), Some(0), "stderr: {stderr}");
}

#[test]
fn a_call_id_reused_after_a_cancelled_approval_is_asked_again() {
    // The second prompt's model proposes a call under the same id as the
    // first prompt's, whose approval the cancel left pending. Its
    // continuation is the request that already carries that call.
    fn model(request: &str) -> String {
        if request.contains("PROMPT-TWO") {
            if request.contains("second.txt") {
                answer("second done")
            } else {
                write_call("call_1", "second.txt")
            }
        } else {
            one_write(request)
        }
    }
    let home = temp_dir("reused-call-id");
    let (project, config) = trusted_project(&home.0, model);
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);

    editor.start_prompt(3, &session, "write it");
    let permission = editor.until_permission();
    editor.send(json!({
        "jsonrpc": "2.0",
        "method": "session/cancel",
        "params": { "sessionId": session },
    }));
    editor.answer_permission(
        &permission,
        json!({ "outcome": { "outcome": "cancelled" } }),
    );
    let first = editor.play(3);
    assert_eq!(
        first.last().expect("response")["result"]["stopReason"],
        "cancelled"
    );

    let second = editor.prompt(4, &session, "PROMPT-TWO write again");
    let stderr = editor.stderr_text();
    assert_eq!(
        permission_requests(&second),
        1,
        "the reused call's own approval is asked: {second:#?}\nstderr: {stderr}"
    );
    assert_eq!(
        second.last().expect("response")["result"]["stopReason"],
        "end_turn",
        "{second:#?}\nstderr: {stderr}"
    );
    assert_eq!(editor.finish(), Some(0), "stderr: {stderr}");
}

#[cfg(unix)]
#[test]
fn a_disconnect_while_a_command_runs_stops_it_before_the_serve_exits() {
    fn model(request: &str) -> String {
        match tool_results(request) {
            0 => tool_call(
                "call_1",
                "shell_exec",
                json!({ "argv": ["sleep", sleep_marker(1)] }),
            ),
            _ => answer("done"),
        }
    }
    let marker = sleep_marker(1);
    let running = || sleeping(&marker);
    let home = temp_dir("disconnect-while-command-runs");
    let (project, config) = trusted_project(&home.0, model);
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);
    // The command runs without asking.
    editor.request(
        3,
        "session/set_mode",
        json!({ "sessionId": session, "mode": "bypassPermissions" }),
    );
    let set = editor.until_result(3);
    assert!(set.get("result").is_some(), "{set}");
    editor.start_prompt(4, &session, "run it");
    let deadline = Instant::now() + DEADLINE;
    while !running() {
        assert!(
            Instant::now() < deadline,
            "the command never started; stderr: {}",
            editor.stderr_text()
        );
        thread::sleep(Duration::from_millis(20));
    }
    let stderr = editor.stderr_text();
    let code = editor.finish();
    let orphaned = running();
    if orphaned {
        let _ = Command::new("pkill")
            .args(["-f", &format!("sleep {marker}")])
            .status();
    }
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(!orphaned, "the interrupted command outlived the serve");
}

#[cfg(unix)]
#[test]
fn a_cancel_while_a_command_runs_keeps_its_end_out_of_the_next_prompt() {
    // The cancelled turn's command is stopped and its end recorded after
    // the kernel records the interruption — here about two seconds after,
    // while a background child still holds the command's output open. The
    // next prompt, sent at once and still running then (its model takes
    // four), must not stream that end as its own.
    fn model(request: &str) -> String {
        if request.contains("PROMPT-TWO") {
            thread::sleep(Duration::from_secs(4));
            answer("second")
        } else {
            match tool_results(request) {
                0 => tool_call(
                    "call_old",
                    "shell_exec",
                    json!({ "argv": [
                        "sh",
                        "-c",
                        format!("sleep 2 & exec sleep {}", sleep_marker(2)),
                    ] }),
                ),
                _ => answer("done"),
            }
        }
    }
    let marker = sleep_marker(2);
    let home = temp_dir("cancel-while-command-runs");
    let (project, config) = trusted_project(&home.0, model);
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);
    editor.request(
        3,
        "session/set_mode",
        json!({ "sessionId": session, "mode": "bypassPermissions" }),
    );
    let set = editor.until_result(3);
    assert!(set.get("result").is_some(), "{set}");
    editor.start_prompt(4, &session, "run it");
    let deadline = Instant::now() + DEADLINE;
    while !sleeping(&marker) {
        assert!(
            Instant::now() < deadline,
            "the command never started; stderr: {}",
            editor.stderr_text()
        );
        thread::sleep(Duration::from_millis(20));
    }
    editor.send(json!({
        "jsonrpc": "2.0",
        "method": "session/cancel",
        "params": { "sessionId": session },
    }));
    editor.start_prompt(5, &session, "PROMPT-TWO now");
    let (mut first, mut second) = (None, None);
    let mut after_first = Vec::new();
    while second.is_none() {
        let frame = editor
            .next(deadline)
            .unwrap_or_else(|| panic!("a prompt never answered; stderr: {}", editor.stderr_text()));
        if frame.get("method").is_none() && frame["id"] == 4 {
            first = Some(frame);
        } else if frame.get("method").is_none() && frame["id"] == 5 {
            second = Some(frame);
        } else if first.is_some() {
            after_first.push(frame);
        }
    }
    let stderr = editor.stderr_text();
    let first = first.expect("the cancelled prompt answers before the next one ends");
    assert_eq!(
        first["result"]["stopReason"], "cancelled",
        "{first}\nstderr: {stderr}"
    );
    assert_eq!(
        second.expect("5")["result"]["stopReason"],
        "end_turn",
        "stderr: {stderr}"
    );
    assert_eq!(
        mentions(&after_first, "call_old"),
        0,
        "the cancelled turn's command end streamed into the next prompt: {after_first:#?}"
    );
    let stopped = !sleeping(&marker);
    if !stopped {
        let _ = Command::new("pkill")
            .args(["-f", &format!("sleep {marker}")])
            .status();
    }
    assert!(stopped, "the cancelled command was stopped");
    assert_eq!(editor.finish(), Some(0), "stderr: {stderr}");
}
