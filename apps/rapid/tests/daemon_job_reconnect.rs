//! SEAM-03 AC-02, end to end: a client of `rapid daemon` whose session runs
//! a background job is killed; a new client that reconnects rebuilds the
//! same `/jobs` rows from the ledger alone — the rows the TUI projects.
//!
//! The real binary serves a real Unix socket; the model is a scripted
//! loopback provider whose first answer starts `sleep 5` in the background.
//! Both clients fold what the daemon streams through the TUI's own reducer.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

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

fn spawn_capturing_server(
    responses: Vec<String>,
) -> (std::net::SocketAddr, std::sync::mpsc::Receiver<String>) {
    spawn_capturing_server_with_gate(responses, None)
}

fn spawn_child_resume_server() -> (
    std::net::SocketAddr,
    std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::{AtomicBool, Ordering};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let resumed = std::sync::Arc::new(AtomicBool::new(false));
    let observed = resumed.clone();
    thread::spawn(move || {
        let mut child_id: Option<String> = None;
        for mut stream in listener.incoming().flatten() {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = stream.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(end) = buf.windows(4).position(|v| v == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                    let length = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if buf.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let request = String::from_utf8_lossy(&buf);
            let body = request.split_once("\r\n\r\n").unwrap().1;
            let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
            let current = parsed["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|message| message["role"] == "user")
                .filter_map(|message| message["content"].as_str())
                .find(|text| text.contains("BEGIN_PARENT") || text.contains("CONTINUE_PARENT"));
            let tool = |id: &str, args: serde_json::Value| {
                serde_json::json!({
                "choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{
                    "id":id,"type":"function","function":{"name":"task_spawn","arguments":args.to_string()}
                }]},"finish_reason":"tool_calls"}],
                "usage":{"prompt_tokens":3,"completion_tokens":5}
            }).to_string()
            };
            let terminal = |text: &str| {
                serde_json::json!({
                "choices":[{"message":{"role":"assistant","content":text},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":3,"completion_tokens":2}
            }).to_string()
            };
            let answer = match current {
                Some(text) if text.contains("CONTINUE_PARENT") => {
                    if request.contains("continued_from=") && request.contains("FOLLOWUP_RESULT") {
                        observed.store(true, Ordering::SeqCst);
                        terminal("continued")
                    } else {
                        tool(
                            "resume-child",
                            serde_json::json!({"resume_agent_ids":{
                                child_id.as_ref().unwrap(): "FOLLOWUP_NEXT"
                            }}),
                        )
                    }
                }
                Some(_) if request.contains("\"tool_call_id\"") => {
                    let start = request.find("agent_id=").unwrap() + "agent_id=".len();
                    child_id = Some(request[start..start + 36].to_owned());
                    terminal("spawned")
                }
                Some(_) => tool(
                    "spawn-children",
                    serde_json::json!({
                        "prompt_template":"ORIGINAL_CHILD {{item}}","items":["a","b"],"type":"explore"
                    }),
                ),
                None if request.contains("FOLLOWUP_NEXT")
                    && request.contains("ORIGINAL_REPORT") =>
                {
                    terminal("FOLLOWUP_RESULT")
                }
                None => terminal("ORIGINAL_REPORT"),
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                answer.len(),
                answer
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.shutdown(Shutdown::Both);
        }
    });
    (addr, resumed)
}

fn spawn_capturing_server_with_gate(
    responses: Vec<String>,
    gate: Option<std::sync::mpsc::Receiver<()>>,
) -> (std::net::SocketAddr, std::sync::mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        for (index, body) in responses.into_iter().enumerate() {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = stream.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(end) = buf.windows(4).position(|v| v == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                    let length = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if buf.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
            if index == 2
                && let Some(gate) = &gate
            {
                gate.recv_timeout(Duration::from_secs(30))
                    .expect("release model response");
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.shutdown(Shutdown::Both);
        }
    });
    (addr, rx)
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
    // while to start on a host that scans new executables) — within a
    // deadline, so a daemon that never starts fails the test.
    let stdout = child.stdout.take().expect("stdout");
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(stdout).read_line(&mut line);
        let _ = tx.send(line);
    });
    let daemon = Daemon(child);
    let line = rx
        .recv_timeout(Duration::from_secs(180))
        .expect("the daemon's first line");
    assert!(line.contains("listening"), "{line}");
    daemon
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

    /// The events of `session` after `from`, through `through`, from a
    /// subscription left open (the daemon ends one only at a turn's terminal
    /// event): the caller is done with the connection.
    fn until(mut self, session: &str, from: u64, through: u64) -> Vec<ErasedEventEnvelope> {
        self.subscribe(session, from);
        let mut events: Vec<ErasedEventEnvelope> = Vec::new();
        while events.last().map_or(from, ErasedEventEnvelope::seq) < through {
            let frame = self.frame();
            assert_eq!(frame["kind"], "event", "{frame}");
            events.push(serde_json::from_value(frame["event"].clone()).expect("an event"));
        }
        events
    }

    fn subscribe(&mut self, session: &str, from: u64) {
        let id = self.next_id;
        self.next_id += 1;
        self.send(serde_json::json!({
            "schema": "rapidlm.sdk.rpc", "schema_version": 1, "kind": "request",
            "id": id, "method": "events.subscribe",
            "params": {"session_id": session, "from_seq": from},
        }));
        assert!(self.frame().get("error").is_none());
    }

    /// The events of `session` after `from`, through the next turn's
    /// terminal event — where the daemon ends a subscription.
    fn through_turn_end(&mut self, session: &str, from: u64) -> Vec<ErasedEventEnvelope> {
        self.subscribe(session, from);
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

/// Whatever the test leaves: the job's process group, the temp tree. Declared
/// before the daemon, so the daemon is stopped first.
struct Leftovers {
    home: PathBuf,
    group: Option<u64>,
}

impl Drop for Leftovers {
    fn drop(&mut self) {
        if let Some(group) = self.group {
            let _ = Command::new("kill")
                .args(["-KILL", &format!("-{group}")])
                .status();
        }
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

#[test]
fn a_reconnecting_client_rebuilds_the_jobs_rows_the_killed_one_had() {
    let home = temp_dir("jr");
    let mut leftovers = Leftovers {
        home: home.clone(),
        group: None,
    };
    let project = home.join("p");
    trusted_project(&home, &project);
    let sleep = test_fixtures::tool_str("sleep");
    // A job that outlives its turn and ends on its own a few seconds later,
    // while no client is connected.
    let addr = spawn_scripted_server(vec![
        tool_call_body(
            "call_1",
            "shell_exec",
            &format!(r#"{{\"argv\":[\"{sleep}\",\"5\"],\"background\":true}}"#),
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
    // The first client: a session, a turn that starts the job, and the rows
    // it saw streamed live.
    let mut first = Client::connect(&socket);
    let created = first.call("sessions.create", serde_json::json!({}));
    let session = created["id"].as_str().expect("session id").to_owned();
    let created_seq = created["seq"].as_u64().expect("seq");
    // The ledger the TUI would read, opened once the daemon has it in use
    // (it prints where it listens before opening it).
    let ledger =
        kernel::InProcessKernelClient::open(project.join(".rapidlm").join("sessions.sqlite"))
            .expect("the project's ledger");
    let on_record = |session: &str| -> Vec<ErasedEventEnvelope> {
        let id: protocol::SessionId = session.parse().expect("id");
        let tip = ledger.session_tip(id).expect("tip");
        (1..=tip)
            .map(|seq| ledger.read_event(id, seq).expect("event"))
            .collect()
    };
    let ended = |events: &[ErasedEventEnvelope]| {
        events
            .iter()
            .any(|event| event.kind() == event_ledger::event::EventKind::JobCompleted)
    };

    first.call(
        "turns.submit",
        serde_json::json!({"session_id": session, "expected_seq": created_seq,
            "prompt": "start the watcher"}),
    );
    let live = first.through_turn_end(&session, 0);
    assert_eq!(
        live.last().map(ErasedEventEnvelope::kind),
        Some(event_ledger::event::EventKind::TurnCompleted),
        "{:?}",
        live.iter()
            .map(ErasedEventEnvelope::kind)
            .collect::<Vec<_>>()
    );
    leftovers.group = live
        .iter()
        .find(|event| event.kind() == event_ledger::event::EventKind::JobStarted)
        .and_then(|event| event.payload()["process"]["process_group"].as_u64());
    let seen = job_rows(&live);
    assert_eq!(seen.len(), 1, "one job: {seen:?}");
    assert!(seen[0].contains("Started"), "running: {seen:?}");
    assert!(
        !ended(&on_record(&session)),
        "still running when the client dies"
    );

    // Killed while idle — the daemon sees that at once — and a second
    // client killed mid-stream, a live subscription open (the daemon sees
    // that only at its next write).
    drop(first);
    let mut watcher = Client::connect(&socket);
    watcher.subscribe(
        &session,
        live.last().map(ErasedEventEnvelope::seq).expect("seq"),
    );
    drop(watcher);
    thread::sleep(Duration::from_millis(200));
    assert!(
        !ended(&on_record(&session)),
        "still running after both clients are gone"
    );

    // The job ends on its own while no client is connected.
    let deadline = Instant::now() + Duration::from_secs(60);
    while !ended(&on_record(&session)) {
        assert!(Instant::now() < deadline, "the job never ended");
        thread::sleep(Duration::from_millis(100));
    }
    // Over: its group is no longer the test's to clean up.
    leftovers.group = None;
    let tip = on_record(&session)
        .last()
        .map(ErasedEventEnvelope::seq)
        .expect("tip");

    // A new client rebuilds the rows through the daemon from the session's
    // first event to its tip — past the turn's end, where one subscription
    // stops, to the job's end.
    let mut second = Client::connect(&socket);
    let mut replayed = second.through_turn_end(&session, 0);
    let from = replayed.last().map(ErasedEventEnvelope::seq).expect("seq");
    replayed.extend(second.until(&session, from, tip));
    // Through the first turn it is what the killed client had.
    let through = live.last().map(ErasedEventEnvelope::seq).expect("seq");
    let prefix: Vec<ErasedEventEnvelope> = replayed
        .iter()
        .filter(|event| event.seq() <= through)
        .cloned()
        .collect();
    assert_eq!(job_rows(&prefix), seen, "the killed client's rows");
    // Through the tip it is the job, completed on its own — not stopped
    // with the client — as the TUI projects the ledger.
    let rows = job_rows(&replayed);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(
        rows[0].contains("Completed") && rows[0].contains("Some(0)"),
        "ended on its own, exit 0: {rows:?}"
    );
    assert_eq!(rows, job_rows(&on_record(&session)), "the TUI's rows");
}

#[test]
fn daemon_recovers_before_model_or_refuses_conflicting_journal() {
    use std::os::unix::fs::PermissionsExt;
    use workspace::multi_edit_journal::JournalStore;
    use workspace::multi_edit_recovery::{FileImage, RecoveryEntry, RecoveryRecord, RecoveryState};
    for scenario in ["recover", "conflict"] {
        let home = temp_dir("preflight");
        let _leftovers = Leftovers {
            home: home.clone(),
            group: None,
        };
        let project = home.join("p");
        trusted_project(&home, &project);
        let original = if scenario == "recover" {
            "beta\n"
        } else {
            "external\n"
        };
        std::fs::write(project.join("notes.txt"), original).unwrap();
        std::fs::set_permissions(
            project.join("notes.txt"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let directory = project.join(".rapidlm/multi-edit-journal");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let journal = JournalStore::lock(&directory).unwrap();
        let image = |text: &str| {
            Some(FileImage {
                bytes: text.as_bytes().to_vec(),
                mode: Some(0o600),
            })
        };
        journal
            .write_intent(
                &RecoveryRecord::new(
                    RecoveryState::Intent,
                    vec![RecoveryEntry {
                        path: "notes.txt".into(),
                        before: image("alpha\n"),
                        after: image("beta\n"),
                    }],
                )
                .unwrap(),
            )
            .unwrap();
        drop(journal);
        let (addr, requests) = spawn_capturing_server(vec![
            tool_call_body("read-1", "repo_read", r#"{\"path\":\"notes.txt\"}"#),
            TERMINAL_BODY.to_owned(),
        ]);
        let config = home.join("config.toml");
        std::fs::write(&config,format!("[models]\ndefault = \"local\"\n\n[model.local]\nprovider = \"openai-compatible\"\nmodel = \"test-model\"\nbase_url = \"http://{addr}/v1\"\napi_key = \"k\"\n")).unwrap();
        let socket = home.join("d.sock");
        let _daemon = start_daemon(&project, &home, &config, &socket);
        let mut client = Client::connect(&socket);
        let created = client.call("sessions.create", serde_json::json!({}));
        let session = created["id"].as_str().unwrap().to_owned();
        client.call("turns.submit",serde_json::json!({"session_id":session,"expected_seq":created["seq"],"prompt":"read notes"}));
        let events = client.through_turn_end(&session, 0);
        if scenario == "recover" {
            assert_eq!(
                events.last().unwrap().kind(),
                event_ledger::event::EventKind::TurnCompleted,
                "{events:?}"
            );
            requests.recv_timeout(Duration::from_secs(30)).unwrap();
            let second = requests.recv_timeout(Duration::from_secs(30)).unwrap();
            let body = second.split_once("\r\n\r\n").unwrap().1;
            let wire: serde_json::Value = serde_json::from_str(body).unwrap();
            let content = wire["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|message| message["role"] == "tool")
                .unwrap()["content"]
                .as_str()
                .unwrap();
            assert!(
                content.contains("alpha") && !content.contains("beta"),
                "{content}"
            );
            assert_eq!(
                std::fs::read_to_string(project.join("notes.txt")).unwrap(),
                "alpha\n"
            );
            assert_eq!(
                JournalStore::lock(&directory).unwrap().load().unwrap(),
                None
            );
        } else {
            assert_eq!(
                events.last().unwrap().kind(),
                event_ledger::event::EventKind::TurnFailed,
                "{events:?}"
            );
            assert_eq!(
                std::fs::read_to_string(project.join("notes.txt")).unwrap(),
                original
            );
            assert!(
                JournalStore::lock(&directory)
                    .unwrap()
                    .load()
                    .unwrap()
                    .is_some()
            );
            assert!(requests.try_recv().is_err(), "model must not be called");
        }
    }
}

#[test]
fn daemon_invalidates_replayed_reads_when_another_process_compacts_mid_request() {
    daemon_mid_request_compaction(false);
}

#[test]
fn daemon_can_read_again_and_edit_after_mid_request_compaction() {
    daemon_mid_request_compaction(true);
}

fn daemon_mid_request_compaction(reread: bool) {
    let home = temp_dir("compact");
    let _leftovers = Leftovers {
        home: home.clone(),
        group: None,
    };
    let project = home.join("p");
    trusted_project(&home, &project);
    std::fs::write(project.join("notes.txt"), "before").unwrap();
    let (release, gate) = std::sync::mpsc::channel();
    let mut responses = vec![
        tool_call_body("read-1", "repo_read", r#"{\"path\":\"notes.txt\"}"#),
        TERMINAL_BODY.to_owned(),
        tool_call_body(
            "write-1",
            "workspace_write",
            r#"{\"path\":\"notes.txt\",\"content\":\"after\"}"#,
        ),
    ];
    if reread {
        responses.push(tool_call_body(
            "read-2",
            "repo_read",
            r#"{\"path\":\"notes.txt\"}"#,
        ));
        responses.push(tool_call_body(
            "write-2",
            "workspace_write",
            r#"{\"path\":\"notes.txt\",\"content\":\"after\"}"#,
        ));
    }
    responses.push(TERMINAL_BODY.to_owned());
    let (addr, requests) = spawn_capturing_server_with_gate(responses, Some(gate));
    let config = home.join("config.toml");
    std::fs::write(&config, format!("[models]\ndefault = \"local\"\n\n[model.local]\nprovider = \"openai-compatible\"\nmodel = \"test-model\"\nbase_url = \"http://{addr}/v1\"\napi_key = \"k\"\n")).unwrap();
    let socket = home.join("d.sock");
    let _daemon = start_daemon(&project, &home, &config, &socket);
    let mut first = Client::connect(&socket);
    let created = first.call("sessions.create", serde_json::json!({}));
    let session = created["id"].as_str().unwrap().to_owned();
    first.call("turns.submit", serde_json::json!({"session_id":session,"expected_seq":created["seq"],"prompt":"read notes"}));
    let first_events = first.through_turn_end(&session, 0);
    assert_eq!(
        first_events.last().map(ErasedEventEnvelope::kind),
        Some(event_ledger::event::EventKind::TurnCompleted)
    );
    let mut second = Client::connect(&socket);
    let tip = second.call("sessions.get", serde_json::json!({"session_id":session}))["seq"]
        .as_u64()
        .unwrap();
    second.call(
        "turns.submit",
        serde_json::json!({"session_id":session,"expected_seq":tip,"prompt":"edit notes"}),
    );
    for _ in 0..2 {
        requests.recv_timeout(Duration::from_secs(30)).unwrap();
    }
    let in_flight = requests.recv_timeout(Duration::from_secs(30)).unwrap();
    assert!(in_flight.contains("session-read/") && in_flight.contains("before"));
    let ledger =
        kernel::InProcessKernelClient::open(project.join(".rapidlm/sessions.sqlite")).unwrap();
    let id: protocol::SessionId = session.parse().unwrap();
    let actor = event_ledger::event::ActorRef::new(
        event_ledger::event::ActorKind::Human,
        &protocol::EventId::new().to_string(),
    )
    .unwrap();
    let through = ledger.session_tip(id).unwrap();
    ledger.append_turn_progress(id, &actor, protocol::TraceId::new(), event_ledger::event::EventKind::ContextCompacted,
        serde_json::json!({"summary":"a concurrent compaction", "through_seq":through, "turns":1})).unwrap();
    release.send(()).unwrap();
    let events = second.through_turn_end(&session, tip);
    assert!(
        events.iter().any(
            |event| event.kind() == event_ledger::event::EventKind::ToolRequested
                && event.payload().to_string().contains("write-1")
        ),
        "{events:?}"
    );
    let failed = events
        .iter()
        .position(|event| {
            matches!(
                event.kind(),
                event_ledger::event::EventKind::ToolFailed
                    | event_ledger::event::EventKind::ToolDenied
            ) && event.payload().to_string().contains("write-1")
        })
        .expect("stale write refused");
    if reread {
        let new_read = events
            .iter()
            .position(|event| {
                event.kind() == event_ledger::event::EventKind::ToolCompleted
                    && event.payload().to_string().contains("read-2")
            })
            .expect("new read completed");
        let new_write = events
            .iter()
            .position(|event| {
                event.kind() == event_ledger::event::EventKind::ToolCompleted
                    && event.payload().to_string().contains("write-2")
            })
            .expect("new write completed");
        assert!(failed < new_read && new_read < new_write, "{events:?}");
    }
    assert_eq!(
        std::fs::read_to_string(project.join("notes.txt")).unwrap(),
        if reread { "after" } else { "before" },
        "{events:?}"
    );
}

#[test]
fn daemon_replays_fresh_session_reads_across_client_connections_but_not_restart() {
    for scenario in ["fresh", "changed", "restart"] {
        let home = temp_dir("turns");
        let _leftovers = Leftovers {
            home: home.clone(),
            group: None,
        };
        let project = home.join("p");
        trusted_project(&home, &project);
        std::fs::write(project.join("notes.txt"), "before").unwrap();
        let (addr, requests) = spawn_capturing_server(vec![
            tool_call_body("read-1", "repo_read", r#"{\"path\":\"notes.txt\"}"#),
            TERMINAL_BODY.to_owned(),
            tool_call_body(
                "write-1",
                "workspace_write",
                r#"{\"path\":\"notes.txt\",\"content\":\"after\"}"#,
            ),
            TERMINAL_BODY.to_owned(),
        ]);
        let config = home.join("config.toml");
        std::fs::write(&config, format!("[models]\ndefault = \"local\"\n\n[model.local]\nprovider = \"openai-compatible\"\nmodel = \"test-model\"\nbase_url = \"http://{addr}/v1\"\napi_key = \"k\"\n")).unwrap();
        let socket = home.join("d.sock");
        let mut daemon = start_daemon(&project, &home, &config, &socket);
        let mut first = Client::connect(&socket);
        let created = first.call("sessions.create", serde_json::json!({}));
        let session = created["id"].as_str().unwrap().to_owned();
        first.call("turns.submit", serde_json::json!({"session_id":session,"expected_seq":created["seq"],"prompt":"read notes"}));
        let events = first.through_turn_end(&session, 0);
        assert_eq!(
            events.last().map(ErasedEventEnvelope::kind),
            Some(event_ledger::event::EventKind::TurnCompleted),
            "{events:?}"
        );
        if scenario == "changed" {
            std::fs::write(project.join("notes.txt"), "external").unwrap();
        }
        if scenario == "restart" {
            drop(daemon);
            daemon = start_daemon(&project, &home, &config, &socket);
        }
        let mut second = Client::connect(&socket);
        let tip = second.call("sessions.get", serde_json::json!({"session_id":session}))["seq"]
            .as_u64()
            .unwrap();
        second.call(
            "turns.submit",
            serde_json::json!({"session_id":session,"expected_seq":tip,"prompt":"edit notes"}),
        );
        let events = second.through_turn_end(&session, tip);
        assert!(
            events.iter().any(|event| event.kind()
                == event_ledger::event::EventKind::ToolRequested
                && event.payload().to_string().contains("write-1")),
            "{events:?}"
        );
        assert_eq!(
            std::fs::read_to_string(project.join("notes.txt")).unwrap(),
            match scenario {
                "fresh" => "after",
                "changed" => "external",
                _ => "before",
            },
            "{scenario}: {events:?}"
        );
        let captured: Vec<_> = (0..4)
            .map(|_| requests.recv_timeout(Duration::from_secs(30)).unwrap())
            .collect();
        assert_eq!(
            captured[2].contains("session-read/") && captured[2].contains("before"),
            scenario != "restart",
            "{scenario}: {}",
            captured[2]
        );
        drop(daemon);
    }
}

#[test]
fn daemon_retains_child_continuations_across_reconnect_but_not_restart() {
    for scenario in ["reconnect", "restart"] {
        let home = temp_dir("child-continuation");
        let _leftovers = Leftovers {
            home: home.clone(),
            group: None,
        };
        let project = home.join("p");
        trusted_project(&home, &project);
        let (addr, resumed) = spawn_child_resume_server();
        let config = home.join("config.toml");
        std::fs::write(&config, format!("[models]\ndefault = \"local\"\n\n[model.local]\nprovider = \"openai-compatible\"\nmodel = \"test-model\"\nbase_url = \"http://{addr}/v1\"\napi_key = \"k\"\n")).unwrap();
        let socket = home.join("d.sock");
        let mut daemon = start_daemon(&project, &home, &config, &socket);
        let mut first = Client::connect(&socket);
        let created = first.call("sessions.create", serde_json::json!({}));
        let session = created["id"].as_str().unwrap().to_owned();
        first.call("turns.submit", serde_json::json!({"session_id":session,"expected_seq":created["seq"],"prompt":"BEGIN_PARENT"}));
        let events = first.through_turn_end(&session, 0);
        assert!(
            events.iter().any(|event| event.kind()
                == event_ledger::event::EventKind::ToolCompleted
                && event.payload().to_string().contains("spawn-children")),
            "{events:?}"
        );
        drop(first);
        if scenario == "restart" {
            drop(daemon);
            daemon = start_daemon(&project, &home, &config, &socket);
        }
        let mut second = Client::connect(&socket);
        let tip = second.call("sessions.get", serde_json::json!({"session_id":session}))["seq"]
            .as_u64()
            .unwrap();
        second.call(
            "turns.submit",
            serde_json::json!({"session_id":session,"expected_seq":tip,"prompt":"CONTINUE_PARENT"}),
        );
        let events = second.through_turn_end(&session, tip);
        assert_eq!(
            resumed.load(std::sync::atomic::Ordering::SeqCst),
            scenario == "reconnect",
            "{scenario}: {events:?}"
        );
        drop(daemon);
    }
}

#[test]
fn approved_daemon_edit_keeps_the_live_suspended_read_across_connections() {
    for scenario in [
        "fresh",
        "changed",
        "restart",
        "compacted",
        "identical",
        "external-identical",
    ] {
        let home = temp_dir("read");
        let _leftovers = Leftovers {
            home: home.clone(),
            group: None,
        };
        let project = home.join("p");
        trusted_project(&home, &project);
        std::fs::write(project.join("notes.txt"), "before").unwrap();
        let hook = project.join("ask.sh");
        std::fs::write(
            &hook,
            "echo '{\"decision\":\"ask\",\"reason\":\"review\"}'\nexit 0\n",
        )
        .unwrap();
        std::fs::write(project.join(".rapidlm/settings.json"), serde_json::json!({
            "hooks":{"pre_tool_use":[{"command":format!("sh {}", hook.display()),"matcher":"workspace_write"}]}
        }).to_string()).unwrap();
        let mut responses = vec![
            tool_call_body("read-1", "repo_read", r#"{\"path\":\"notes.txt\"}"#),
            tool_call_body(
                "write-1",
                "workspace_write",
                r#"{\"path\":\"notes.txt\",\"content\":\"after\"}"#,
            ),
        ];
        if scenario == "identical" {
            responses.extend([
                tool_call_body("read-2", "repo_read", r#"{\"path\":\"notes.txt\"}"#),
                tool_call_body(
                    "write-2",
                    "workspace_write",
                    r#"{\"path\":\"notes.txt\",\"content\":\"before\"}"#,
                ),
                TERMINAL_BODY.to_owned(),
            ]);
        }
        responses.push(TERMINAL_BODY.to_owned());
        let addr = spawn_scripted_server(responses);
        let config = home.join("config.toml");
        std::fs::write(&config, format!("[models]\ndefault = \"local\"\n\n[model.local]\nprovider = \"openai-compatible\"\nmodel = \"test-model\"\nbase_url = \"http://{addr}/v1\"\napi_key = \"k\"\n")).unwrap();
        let socket = home.join("d.sock");
        let mut daemon = start_daemon(&project, &home, &config, &socket);
        let mut first = Client::connect(&socket);
        let created = first.call("sessions.create", serde_json::json!({}));
        let session = created["id"].as_str().unwrap().to_owned();
        first.call("turns.submit", serde_json::json!({"session_id":session,"expected_seq":created["seq"],"prompt":"read and update notes"}));
        let paused = first.through_turn_end(&session, 0);
        assert_eq!(
            paused.last().map(ErasedEventEnvelope::kind),
            Some(event_ledger::event::EventKind::TurnInterrupted),
            "{scenario}"
        );
        drop(first);
        if scenario == "changed" {
            std::fs::write(project.join("notes.txt"), "external").unwrap();
        }
        if scenario == "compacted" {
            let ledger =
                kernel::InProcessKernelClient::open(project.join(".rapidlm/sessions.sqlite"))
                    .unwrap();
            let id: protocol::SessionId = session.parse().unwrap();
            let actor = event_ledger::event::ActorRef::new(
                event_ledger::event::ActorKind::Human,
                &protocol::EventId::new().to_string(),
            )
            .unwrap();
            let tip = ledger.session_tip(id).unwrap();
            ledger.append_turn_progress(id, &actor, protocol::TraceId::new(), event_ledger::event::EventKind::ContextCompacted,
                serde_json::json!({"summary":"compacted while awaiting approval", "through_seq":tip, "turns":1})).unwrap();
        }
        if scenario == "external-identical" {
            let replacement = project.join("replacement.txt");
            std::fs::write(&replacement, "before").unwrap();
            std::fs::rename(replacement, project.join("notes.txt")).unwrap();
        }
        if scenario == "restart" {
            drop(daemon);
            daemon = start_daemon(&project, &home, &config, &socket);
        }
        if scenario == "identical" {
            let mut writer = Client::connect(&socket);
            let other = writer.call("sessions.create", serde_json::json!({}));
            let other_session = other["id"].as_str().unwrap().to_owned();
            writer.call("turns.submit", serde_json::json!({"session_id":other_session,"expected_seq":other["seq"],"prompt":"read and write the same bytes"}));
            let other_pause = writer.through_turn_end(&other_session, 0);
            assert_eq!(
                other_pause.last().map(ErasedEventEnvelope::kind),
                Some(event_ledger::event::EventKind::TurnInterrupted)
            );
            drop(writer);
            let mut resolver = Client::connect(&socket);
            let tip = resolver.call(
                "sessions.get",
                serde_json::json!({"session_id":other_session}),
            )["seq"]
                .as_u64()
                .unwrap();
            resolver.call("approvals.resolve", serde_json::json!({"session_id":other_session,"expected_seq":tip,"decision":"approved"}));
            drop(resolver);
            let other_done = Client::connect(&socket).through_turn_end(&other_session, tip);
            assert!(
                other_done.iter().any(|event| event.kind()
                    == event_ledger::event::EventKind::WorkspaceMutationDetected),
                "the identical-byte write must actually commit: {other_done:?}"
            );
            assert_eq!(
                std::fs::read_to_string(project.join("notes.txt")).unwrap(),
                "before"
            );
        }
        let mut second = Client::connect(&socket);
        let snapshot = second.call("sessions.get", serde_json::json!({"session_id":session}));
        let seq = snapshot["seq"].as_u64().unwrap();
        second.call(
            "approvals.resolve",
            serde_json::json!({"session_id":session,"expected_seq":seq,"decision":"approved"}),
        );
        drop(second);
        let mut events = Client::connect(&socket);
        let finished = events.through_turn_end(&session, seq);
        assert!(
            finished.iter().any(|event| matches!(
                event.kind(),
                event_ledger::event::EventKind::TurnCompleted
                    | event_ledger::event::EventKind::TurnFailed
            )),
            "{scenario}: {finished:?}"
        );
        if scenario == "identical" || scenario == "external-identical" {
            assert!(
                !finished.iter().any(|event| event.kind()
                    == event_ledger::event::EventKind::WorkspaceMutationDetected),
                "the earlier proof must be revoked: {finished:?}"
            );
        }
        let expected = match scenario {
            "fresh" => "after",
            "changed" => "external",
            _ => "before",
        };
        assert_eq!(
            std::fs::read_to_string(project.join("notes.txt")).unwrap(),
            expected,
            "{scenario}"
        );
        drop(daemon);
    }
}
