//! `rapid acp` end to end: the real binary serving an editor the test plays
//! over the process's stdio, with a scripted model on loopback.
//!
//! What the unit tests cannot show: that a prompt survives every approval
//! its turn raises — the editor's answers route to the waiting prompt, the
//! serve stays up, the prompt ends with its real stop reason, and a cancel
//! during an approval wait ends it.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
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
    json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": "workspace_write",
                        "arguments": json!({ "path": path, "content": "from the model" }).to_string(),
                    },
                }],
            },
            "finish_reason": "tool_calls",
        }],
        "usage": { "prompt_tokens": 3, "completion_tokens": 5 },
    })
    .to_string()
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

    /// Send `session/prompt` as request `id` and play the editor until its
    /// response: every `session/request_permission` is answered "allow once"
    /// in the protocol's response shape. Returns every frame the prompt
    /// streamed, its response last. Panics if the serve exits first.
    fn prompt(&mut self, id: i64, session: &str, text: &str) -> Vec<Value> {
        self.request(
            id,
            "session/prompt",
            json!({ "sessionId": session, "prompt": [{ "type": "text", "text": text }] }),
        );
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

#[test]
fn a_cancel_while_the_turn_waits_on_an_approval_ends_the_prompt_and_nothing_resumes() {
    fn model(request: &str) -> String {
        match tool_results(request) {
            0 => write_call("call_1", "first.txt"),
            _ => answer("resumed after the cancel"),
        }
    }
    let home = temp_dir("cancel-while-waiting");
    let (project, config) = trusted_project(&home.0, model);
    let mut editor = Editor::spawn(&project, &home.0, &config);
    let session = editor.open_session(&project);

    editor.request(
        3,
        "session/prompt",
        json!({ "sessionId": session, "prompt": [{ "type": "text", "text": "write it" }] }),
    );
    let deadline = Instant::now() + DEADLINE;
    let permission = loop {
        let frame = editor
            .next(deadline)
            .unwrap_or_else(|| panic!("no permission request; stderr: {}", editor.stderr_text()));
        if frame["method"] == "session/request_permission" {
            break frame;
        }
    };
    // Let the turn record its pause (milliseconds), so the cancel meets a
    // session with no live turn — the case the kernel cannot interrupt.
    thread::sleep(Duration::from_secs(1));
    // The protocol's order: cancel, then answer the outstanding request.
    editor.send(json!({
        "jsonrpc": "2.0",
        "method": "session/cancel",
        "params": { "sessionId": session },
    }));
    editor.send(json!({
        "jsonrpc": "2.0",
        "id": permission["id"].clone(),
        "result": { "outcome": { "outcome": "cancelled" } },
    }));
    let mut rest = Vec::new();
    let response = loop {
        let frame = editor.next(deadline).unwrap_or_else(|| {
            panic!(
                "the cancelled prompt never answered; frames: {rest:#?}; stderr: {}",
                editor.stderr_text()
            )
        });
        if frame["id"] == 3 && frame.get("method").is_none() {
            break frame;
        }
        rest.push(frame);
    };
    let stderr = editor.stderr_text();
    assert_eq!(
        response["result"]["stopReason"], "cancelled",
        "{rest:#?}\nstderr: {stderr}"
    );
    assert!(
        !rest
            .iter()
            .any(|frame| frame.to_string().contains("resumed after the cancel")),
        "no continuation ran: {rest:#?}"
    );
    assert!(!project.join("first.txt").exists(), "the write never ran");
    // The answer to the cancelled prompt's request is dropped, not fatal.
    assert_eq!(editor.finish(), Some(0), "stderr: {stderr}");
}
