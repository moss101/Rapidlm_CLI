//! SEAM-03 AC-02, end to end: a client of `rapid daemon` whose session runs
//! a background job is killed; a new client that reconnects rebuilds the
//! same `/jobs` rows from the ledger alone — the rows the TUI projects.
//!
//! The real binary serves a real Unix socket; the model is a scripted
//! loopback provider whose first answer starts `sleep 30` in the background.
//! Both clients fold what the daemon streams through the TUI's own reducer.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

use event_ledger::event::ErasedEventEnvelope;
use tui::state::{AppState, UiEvent, reduce};

const TERMINAL_BODY: &str = r#"{"choices":[{"message":{"role":"assistant","content":"started it"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":5}}"#;

fn tool_call_body(id: &str, tool: &str, arguments_json: &str) -> String {
    format!(
        r#"{{"choices":[{{"message":{{"role":"assistant","content":null,"tool_calls":[{{"id":"{id}","type":"function","function":{{"name":"{tool}","arguments":"{arguments_json}"}}}}]}},"finish_reason":"tool_calls"}}],"usage":{{"prompt_tokens":3,"completion_tokens":5}}}}"#
    )
}

fn spawn_scripted_server(responses: Vec<String>) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    thread::spawn(move || {
        for body in responses {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                let read = stream.read(&mut chunk).unwrap_or(0);
                if read == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..read]);
                if String::from_utf8_lossy(&buf).contains("\r\n\r\n") {
                    break;
                }
            }
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

fn temp_dir(name: &str) -> PathBuf {
    // Short: a Unix socket path is bounded (104 bytes on macOS).
    let dir = std::env::temp_dir().join(format!(
        "rl-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
            % 1_000_000_000
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn trusted_project(home: &Path, project: &Path) {
    std::fs::create_dir_all(project.join(".rapidlm")).expect("project");
    let init = Command::new("git")
        .args(["init", "-q"])
        .current_dir(project)
        .output()
        .expect("git init");
    assert!(init.status.success(), "git init failed");
    let root = protocol::host_path::canonicalize(project).expect("canonical root");
    let trust_dir = home.join(".rapidlm");
    std::fs::create_dir_all(&trust_dir).expect("rapidlm home");
    let root_json = serde_json::to_string(root.to_str().expect("utf-8 root")).expect("json");
    std::fs::write(
        trust_dir.join("project-trust.json"),
        format!("{{\"schema\":1,\"records\":[{{\"canonical_root\":{root_json},\"status\":\"trusted\"}}]}}"),
    )
    .expect("trust catalog");
}

/// The daemon, killed (and its socket removed) when the test ends.
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_daemon(project: &Path, home: &Path, config: &Path, socket: &Path) -> Daemon {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rapid"))
        .args(["daemon", "--socket"])
        .arg(socket)
        .current_dir(project)
        .env("HOME", home)
        .env("RAPIDLM_CONFIG", config)
        .env("RAPIDLM_PERMISSION_MODE", "bypassPermissions")
        .env("RAPIDLM_RETRY_BASE_MS", "1")
        .env_remove("RAPIDLM_HOME")
        .env_remove("RAPIDLM_MODEL")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("rapid daemon");
    // It says where it listens once it does (a cold binary can take a
    // while to start on a host that scans new executables).
    let mut line = String::new();
    BufReader::new(child.stdout.take().expect("stdout"))
        .read_line(&mut line)
        .expect("the daemon's first line");
    assert!(line.contains("listening"), "{line}");
    Daemon(child)
}

/// One SDK client connection, speaking `rapidlm.sdk.rpc` v1.
struct Client {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: u64,
}

impl Client {
    fn connect(socket: &Path) -> Self {
        let stream = UnixStream::connect(socket).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .expect("timeout");
        let mut client = Self {
            writer: stream.try_clone().expect("clone"),
            reader: BufReader::new(stream),
            next_id: 1,
        };
        client.send(serde_json::json!({
            "schema": "rapidlm.sdk.rpc", "schema_version": 1, "kind": "hello", "id": "h",
            "auth": {"kind": "local", "handle": ""},
        }));
        assert_eq!(client.frame()["kind"], "hello_ok");
        client
    }

    fn send(&mut self, frame: serde_json::Value) {
        let mut line = frame.to_string();
        line.push('\n');
        self.writer.write_all(line.as_bytes()).expect("send");
    }

    fn frame(&mut self) -> serde_json::Value {
        let mut line = String::new();
        self.reader.read_line(&mut line).expect("read a frame");
        serde_json::from_str(&line).expect("a JSON frame")
    }

    fn call(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(serde_json::json!({
            "schema": "rapidlm.sdk.rpc", "schema_version": 1, "kind": "request",
            "id": id, "method": method, "params": params,
        }));
        let reply = self.frame();
        assert!(
            reply.get("error").is_none() && reply.get("result").is_some(),
            "{method}: {reply}"
        );
        reply["result"].clone()
    }

    /// The events of `session` after `from`, through the next turn's
    /// terminal event — where the daemon ends a subscription.
    fn through_turn_end(&mut self, session: &str, from: u64) -> Vec<ErasedEventEnvelope> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(serde_json::json!({
            "schema": "rapidlm.sdk.rpc", "schema_version": 1, "kind": "request",
            "id": id, "method": "events.subscribe",
            "params": {"session_id": session, "from_seq": from},
        }));
        assert!(self.frame().get("error").is_none());
        let mut events = Vec::new();
        loop {
            let frame = self.frame();
            match frame["kind"].as_str() {
                Some("event") => {
                    events.push(serde_json::from_value(frame["event"].clone()).expect("an event"))
                }
                Some("stream_end") => return events,
                other => panic!("unexpected frame {other:?}: {frame}"),
            }
        }
    }
}

/// `/jobs` rows as the TUI projects them from `events`.
fn job_rows(events: &[ErasedEventEnvelope]) -> Vec<String> {
    let state = events.iter().fold(AppState::new(), |state, event| {
        reduce(state, &UiEvent::Kernel(event.clone()))
    });
    state
        .jobs()
        .iter()
        .map(|(id, job)| {
            format!(
                "{id:?} {:?} {:?} {:?} {:?}",
                job.handle(),
                job.command(),
                job.state(),
                job.exit_status()
            )
        })
        .collect()
}

#[test]
fn a_reconnecting_client_rebuilds_the_jobs_rows_the_killed_one_had() {
    let home = temp_dir("jr");
    let project = home.join("p");
    trusted_project(&home, &project);
    let sleep = test_fixtures::tool_str("sleep");
    let addr = spawn_scripted_server(vec![
        tool_call_body(
            "call_1",
            "shell_exec",
            &format!(r#"{{\"argv\":[\"{sleep}\",\"30\"],\"background\":true}}"#),
        ),
        TERMINAL_BODY.to_owned(),
    ]);
    let config = home.join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[models]\ndefault = \"local\"\n\n[model.local]\nprovider = \"openai-compatible\"\n\
             model = \"test-model\"\nbase_url = \"http://{addr}/v1\"\napi_key = \"k\"\n"
        ),
    )
    .expect("config");
    let socket = home.join("d.sock");
    let _daemon = start_daemon(&project, &home, &config, &socket);

    // The first client: a session, a turn that starts a background job,
    // and the rows it saw streamed live.
    let mut first = Client::connect(&socket);
    let created = first.call("sessions.create", serde_json::json!({}));
    let session = created["id"].as_str().expect("session id").to_owned();
    let created_seq = created["seq"].as_u64().expect("seq");
    first.call(
        "turns.submit",
        serde_json::json!({"session_id": session, "expected_seq": created_seq,
            "prompt": "start the watcher"}),
    );
    // Everything through the turn's end, streamed live.
    let live = first.through_turn_end(&session, 0);
    assert_eq!(
        live.last().map(ErasedEventEnvelope::kind),
        Some(event_ledger::event::EventKind::TurnCompleted),
        "{:?}",
        live.iter()
            .map(ErasedEventEnvelope::kind)
            .collect::<Vec<_>>()
    );
    let seen = job_rows(&live);
    assert_eq!(seen.len(), 1, "one job, running: {seen:?}");
    assert!(seen[0].contains("Started"), "{seen:?}");
    let through = live.last().map(ErasedEventEnvelope::seq).expect("events");

    // Killed: the connection is gone mid-session.
    drop(first);

    // A new client rebuilds the rows from the ledger alone, through the
    // daemon, replaying from the session's first event.
    let mut second = Client::connect(&socket);
    let replayed = second.through_turn_end(&session, 0);
    assert_eq!(replayed.last().map(ErasedEventEnvelope::seq), Some(through));
    assert_eq!(job_rows(&replayed), seen, "the reconnecting client's rows");

    // And they are the rows the TUI projects from the same ledger.
    let ledger =
        kernel::InProcessKernelClient::open(project.join(".rapidlm").join("sessions.sqlite"))
            .expect("the project's ledger");
    let on_record: Vec<ErasedEventEnvelope> = (1..=through)
        .map(|seq| {
            ledger
                .read_event(session.parse().expect("id"), seq)
                .expect("event")
        })
        .collect();
    assert_eq!(job_rows(&on_record), seen, "the TUI's rows");

    // The daemon still runs the job (it outlives the client); stop its
    // group rather than leave it for 30 s.
    if let Some(group) = replayed
        .iter()
        .find(|event| event.kind() == event_ledger::event::EventKind::JobStarted)
        .and_then(|event| event.payload()["process"]["process_group"].as_u64())
    {
        let _ = Command::new("kill")
            .args(["-KILL", &format!("-{group}")])
            .status();
    }
    let _ = std::fs::remove_dir_all(&home);
}
