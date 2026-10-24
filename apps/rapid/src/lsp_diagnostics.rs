//! Language-server diagnostics for files the agent writes.
//!
//! After a file is written, the language server for its type is told about the
//! new content and the problems it reports are returned for the model to see,
//! so a type error or a missing import is found in the same step that made it
//! rather than a build later.
//!
//! Opt-in, in `.rapidlm/settings.json`:
//!
//! ```json
//! { "lsp": true }
//! ```
//!
//! turns on the built-in servers whose programs are on `PATH` (rust-analyzer,
//! typescript-language-server, pyright-langserver, gopls, clangd). An object
//! configures more:
//!
//! ```json
//! { "lsp": {
//!     "defaults": true,
//!     "servers": {
//!       "rust": { "command": ["rust-analyzer"], "extensions": ["rs"] },
//!       "pyright": false
//!     },
//!     "wait_ms": 3000,
//!     "min_severity": "error"
//! } }
//! ```
//!
//! A server is started the first time a file of its type is written and kept for
//! the session, shared by the turn's subagents, and stopped when the session
//! ends. Everything here is advisory: a server that is missing, slow, crashed
//! or silent costs a bounded wait at most and never fails a write. After two
//! consecutive writes with no answer a server is probed with a short wait
//! instead of a full one until it answers again.
//!
//! The transport is the Language Server Protocol over standard input/output:
//! `Content-Length` framed JSON-RPC. Requests the server makes of the client
//! (configuration, work-done progress, capability registration) are answered
//! with empty results so a server never stalls waiting on us.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Time to wait for a server's diagnostics after a write, unless configured.
pub const DEFAULT_WAIT: Duration = Duration::from_millis(3000);
/// Longest `wait_ms` accepted.
pub const MAX_WAIT: Duration = Duration::from_secs(30);
/// Time a server has to answer `initialize`.
pub const START_TIMEOUT: Duration = Duration::from_secs(20);
/// After the first answer, how long to wait for a newer one. Servers often
/// publish a stale or partial set first and the real one a moment later.
const QUIET: Duration = Duration::from_millis(150);
/// Wait used once a server has missed twice in a row.
const PROBE_WAIT: Duration = Duration::from_millis(400);
/// How long a failed server is left alone before it is started again.
const RETRY_AFTER: Duration = Duration::from_secs(60);
/// Most diagnostics shown for one file.
const MAX_SHOWN: usize = 10;
/// Longest message shown.
const MAX_MESSAGE_CHARS: usize = 240;
/// Most servers one settings file may define.
const MAX_SERVERS: usize = 16;
/// Largest message body accepted from a server.
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

/// How serious a diagnostic is, most serious first.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Severity {
    Error = 1,
    Warning = 2,
    Information = 3,
    Hint = 4,
}

impl Severity {
    fn from_lsp(code: Option<i64>) -> Self {
        match code {
            Some(2) => Self::Warning,
            Some(3) => Self::Information,
            Some(4) => Self::Hint,
            _ => Self::Error,
        }
    }

    fn word(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Information => "info",
            Self::Hint => "hint",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text.to_ascii_lowercase().as_str() {
            "error" => Some(Self::Error),
            "warning" => Some(Self::Warning),
            "information" | "info" => Some(Self::Information),
            "hint" => Some(Self::Hint),
            _ => None,
        }
    }
}

/// One language server the pool may run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerSpec {
    name: String,
    command: Vec<String>,
    extensions: Vec<String>,
    language_id: Option<String>,
    init_options: Option<Value>,
}

/// The settings: which servers, how long to wait, how serious to report.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LspConfig {
    servers: Vec<ServerSpec>,
    wait: Duration,
    min_severity: Severity,
}

impl LspConfig {
    /// A stable text form, used to tell whether a settings change needs a new
    /// pool.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        format!("{self:?}")
    }

    /// The server that claims `extension`.
    fn server_for(&self, extension: &str) -> Option<&ServerSpec> {
        self.servers.iter().find(|s| {
            s.extensions
                .iter()
                .any(|e| e.eq_ignore_ascii_case(extension))
        })
    }
}

fn builtin(name: &str, command: &[&str], extensions: &[&str]) -> ServerSpec {
    ServerSpec {
        name: name.to_owned(),
        command: command.iter().map(|s| (*s).to_owned()).collect(),
        extensions: extensions.iter().map(|s| (*s).to_owned()).collect(),
        language_id: None,
        init_options: None,
    }
}

fn builtin_servers() -> Vec<ServerSpec> {
    vec![
        builtin("rust", &["rust-analyzer"], &["rs"]),
        builtin(
            "typescript",
            &["typescript-language-server", "--stdio"],
            &["ts", "tsx", "js", "jsx", "mjs", "cjs"],
        ),
        builtin("pyright", &["pyright-langserver", "--stdio"], &["py"]),
        builtin("go", &["gopls"], &["go"]),
        builtin(
            "clangd",
            &["clangd"],
            &["c", "cc", "cpp", "cxx", "h", "hh", "hpp"],
        ),
    ]
}

/// Parse the `lsp` key of a settings document. `None` when LSP is not asked
/// for; the second value lists entries that were dropped and why.
#[must_use]
pub fn parse(settings: &Value) -> (Option<LspConfig>, Vec<String>) {
    parse_with(settings, &|program| on_path(program))
}

fn parse_with(
    settings: &Value,
    available: &dyn Fn(&str) -> bool,
) -> (Option<LspConfig>, Vec<String>) {
    let mut problems = Vec::new();
    let (use_defaults, overrides, wait, min_severity) = match settings.get("lsp") {
        None | Some(Value::Bool(false)) | Some(Value::Null) => return (None, problems),
        Some(Value::Bool(true)) => (true, serde_json::Map::new(), DEFAULT_WAIT, Severity::Error),
        Some(Value::Object(object)) => {
            let wait = object
                .get("wait_ms")
                .and_then(Value::as_u64)
                .map_or(DEFAULT_WAIT, Duration::from_millis)
                .min(MAX_WAIT);
            let min_severity = match object.get("min_severity").and_then(Value::as_str) {
                Some(text) => Severity::parse(text).unwrap_or_else(|| {
                    problems.push(format!(
                        "lsp.min_severity: `{text}` is not error, warning, information or hint"
                    ));
                    Severity::Error
                }),
                None => Severity::Error,
            };
            let use_defaults = object
                .get("defaults")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let overrides = object
                .get("servers")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            (use_defaults, overrides, wait, min_severity)
        }
        Some(_) => {
            problems.push("lsp: expected true, false or an object".to_owned());
            return (None, problems);
        }
    };
    let mut servers: Vec<ServerSpec> = Vec::new();
    for (name, entry) in &overrides {
        if servers.len() >= MAX_SERVERS {
            problems.push(format!(
                "lsp.servers: more than {MAX_SERVERS}; the rest are ignored"
            ));
            break;
        }
        if entry.as_bool() == Some(false) {
            continue;
        }
        match parse_server(name, entry) {
            Ok(spec) => servers.push(spec),
            Err(why) => problems.push(format!("lsp.servers.{name}: {why}")),
        }
    }
    if use_defaults {
        for spec in builtin_servers() {
            let disabled = overrides.get(&spec.name).and_then(Value::as_bool) == Some(false);
            let overridden = servers.iter().any(|s| s.name == spec.name);
            if !disabled && !overridden && available(&spec.command[0]) {
                servers.push(spec);
            }
        }
    }
    (
        (!servers.is_empty()).then_some(LspConfig {
            servers,
            wait,
            min_severity,
        }),
        problems,
    )
}

fn parse_server(name: &str, entry: &Value) -> Result<ServerSpec, String> {
    let object = entry.as_object().ok_or("expected an object or false")?;
    let command: Vec<String> = object
        .get("command")
        .and_then(Value::as_array)
        .ok_or("`command` must be an array of strings")?
        .iter()
        .map(|w| {
            w.as_str()
                .map(str::to_owned)
                .ok_or("`command` holds a non-string")
        })
        .collect::<Result<_, _>>()?;
    if command.is_empty() || command.iter().any(String::is_empty) || command.len() > 16 {
        return Err("`command` is empty or too long".to_owned());
    }
    let extensions: Vec<String> = object
        .get("extensions")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|e| e.as_str())
                .map(|e| e.trim_start_matches('.').to_owned())
                .filter(|e| !e.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if extensions.is_empty() {
        return Err("`extensions` is required".to_owned());
    }
    Ok(ServerSpec {
        name: name.to_owned(),
        command,
        extensions,
        language_id: object
            .get("language_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        init_options: object.get("initialization_options").cloned(),
    })
}

/// Whether `program` is an executable on `PATH`.
fn on_path(program: &str) -> bool {
    if program.contains(['/', '\\']) {
        return Path::new(program).is_file();
    }
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    let extensions: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT".to_owned())
            .split(';')
            .map(str::to_owned)
            .collect()
    } else {
        vec![String::new()]
    };
    std::env::split_paths(&paths).any(|dir| {
        extensions
            .iter()
            .any(|ext| dir.join(format!("{program}{ext}")).is_file())
    })
}

fn language_id(extension: &str) -> &'static str {
    match extension.to_ascii_lowercase().as_str() {
        "rs" => "rust",
        "ts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "py" => "python",
        "go" => "go",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hh" | "hpp" => "cpp",
        "json" => "json",
        "java" => "java",
        "rb" => "ruby",
        _ => "plaintext",
    }
}

/// `file://` URI for an absolute path, percent-encoding what a URI cannot hold.
fn path_to_uri(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let mut out = String::from("file://");
    if !text.starts_with('/') {
        out.push('/');
    }
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~' | b':') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Eq, PartialEq)]
struct Diagnostic {
    line: u64,
    column: u64,
    severity: Severity,
    code: Option<String>,
    message: String,
}

fn parse_diagnostics(list: &Value) -> Vec<Diagnostic> {
    list.as_array()
        .map(|items| {
            items
                .iter()
                .map(|item| Diagnostic {
                    line: item["range"]["start"]["line"].as_u64().unwrap_or(0) + 1,
                    column: item["range"]["start"]["character"].as_u64().unwrap_or(0) + 1,
                    severity: Severity::from_lsp(item["severity"].as_i64()),
                    code: item["code"]
                        .as_str()
                        .map(str::to_owned)
                        .or_else(|| item["code"].as_i64().map(|n| n.to_string())),
                    message: item["message"].as_str().unwrap_or("").to_owned(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The text added to a tool result.
fn render(server: &str, relative: &str, found: &[Diagnostic], min: Severity) -> String {
    let mut shown: Vec<&Diagnostic> = found.iter().filter(|d| d.severity <= min).collect();
    if shown.is_empty() {
        return format!("lsp ({server}): no problems reported in {relative}");
    }
    shown.sort_by_key(|d| (d.severity, d.line, d.column));
    let errors = shown
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .count();
    let mut out = format!(
        "lsp ({server}): {} problem(s) in {relative} ({errors} error(s)); fix them or say why they are expected",
        shown.len()
    );
    for d in shown.iter().take(MAX_SHOWN) {
        let message: String = d
            .message
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .filter(|c| !c.is_control())
            .take(MAX_MESSAGE_CHARS)
            .collect();
        let code = d
            .code
            .as_deref()
            .map(|c| format!(" [{c}]"))
            .unwrap_or_default();
        out.push_str(&format!(
            "\n  {relative}:{}:{} {}{code} {message}",
            d.line,
            d.column,
            d.severity.word()
        ));
    }
    if shown.len() > MAX_SHOWN {
        out.push_str(&format!("\n  ... and {} more", shown.len() - MAX_SHOWN));
    }
    out
}

// ---------------------------------------------------------------------------
// One running server
// ---------------------------------------------------------------------------

#[derive(Default)]
struct DiagTable {
    /// Bumped on every publish; lets a waiter tell "newer than my change".
    tick: u64,
    by_uri: HashMap<String, Published>,
}

struct Published {
    tick: u64,
    version: Option<i64>,
    items: Vec<Diagnostic>,
}

struct Shared {
    diagnostics: Mutex<DiagTable>,
    changed: Condvar,
    pending: Mutex<HashMap<u64, Sender<Value>>>,
    dead: AtomicBool,
}

struct Session {
    child: Mutex<Child>,
    writer: Arc<Mutex<ChildStdin>>,
    shared: Arc<Shared>,
    docs: Mutex<HashMap<String, i64>>,
    next_id: AtomicU64,
    misses: AtomicU32,
    /// Serialises "tell the server, then wait" so two writes do not read each
    /// other's answers.
    sync: Mutex<()>,
}

fn write_message(writer: &Mutex<ChildStdin>, message: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(message)?;
    let mut pipe = writer
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    write!(pipe, "Content-Length: {}\r\n\r\n", body.len())?;
    pipe.write_all(&body)?;
    pipe.flush()
}

fn read_message(reader: &mut impl BufRead) -> Option<Value> {
    let mut length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            length = value.trim().parse().ok();
        }
    }
    let length = length.filter(|n| *n <= MAX_BODY_BYTES)?;
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

/// The reply to a request the server made of us. Empty, but well-formed.
fn answer_server_request(method: &str, params: &Value) -> Value {
    match method {
        // One entry per requested section, none of them configured.
        "workspace/configuration" => {
            let count = params["items"].as_array().map_or(0, Vec::len);
            Value::Array(vec![Value::Null; count])
        }
        "workspace/workspaceFolders" => json!([]),
        _ => Value::Null,
    }
}

fn reader_loop(stdout: impl Read, shared: &Arc<Shared>, writer: &Arc<Mutex<ChildStdin>>) {
    let mut reader = BufReader::new(stdout);
    while let Some(message) = read_message(&mut reader) {
        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id");
        match (method, id) {
            (Some(method), Some(id)) => {
                let reply = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": answer_server_request(method, &message["params"]),
                });
                let _ = write_message(writer, &reply);
            }
            (Some("textDocument/publishDiagnostics"), None) => {
                let params = &message["params"];
                if let Some(uri) = params["uri"].as_str() {
                    let mut table = shared
                        .diagnostics
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    table.tick += 1;
                    let tick = table.tick;
                    table.by_uri.insert(
                        uri.to_owned(),
                        Published {
                            tick,
                            version: params["version"].as_i64(),
                            items: parse_diagnostics(&params["diagnostics"]),
                        },
                    );
                    shared.changed.notify_all();
                }
            }
            (None, Some(id)) => {
                if let Some(id) = id.as_u64()
                    && let Some(waiter) = shared
                        .pending
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&id)
                {
                    let _ = waiter.send(message);
                }
            }
            _ => {}
        }
    }
    shared.dead.store(true, Ordering::SeqCst);
    shared.changed.notify_all();
}

impl Session {
    fn start(spec: &ServerSpec, root: &Path) -> Result<Self, String> {
        let mut command = Command::new(&spec.command[0]);
        command
            .args(&spec.command[1..])
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // Its own process group: a language server starts helpers of its own
        // (rust-analyzer's proc-macro server, tsserver), and stopping it must
        // stop them.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let mut child = command
            .spawn()
            .map_err(|e| format!("could not start `{}`: {e}", spec.command[0]))?;
        let stdin = child.stdin.take().ok_or("no standard input")?;
        let stdout = child.stdout.take().ok_or("no standard output")?;
        let writer = Arc::new(Mutex::new(stdin));
        let shared = Arc::new(Shared {
            diagnostics: Mutex::new(DiagTable::default()),
            changed: Condvar::new(),
            pending: Mutex::new(HashMap::new()),
            dead: AtomicBool::new(false),
        });
        {
            let shared = Arc::clone(&shared);
            let writer = Arc::clone(&writer);
            std::thread::spawn(move || reader_loop(stdout, &shared, &writer));
        }
        let session = Self {
            child: Mutex::new(child),
            writer,
            shared,
            docs: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            misses: AtomicU32::new(0),
            sync: Mutex::new(()),
        };
        let uri = path_to_uri(root);
        let reply = session.request(
            "initialize",
            &json!({
                "processId": std::process::id(),
                "rootUri": uri,
                "workspaceFolders": [{ "uri": uri, "name": root.file_name().map_or_else(|| "project".to_owned(), |n| n.to_string_lossy().into_owned()) }],
                "initializationOptions": spec.init_options,
                "capabilities": {
                    "general": { "positionEncodings": ["utf-16"] },
                    "workspace": { "configuration": true, "workspaceFolders": true },
                    "window": { "workDoneProgress": true },
                    "textDocument": {
                        "synchronization": { "didSave": true, "dynamicRegistration": false },
                        "publishDiagnostics": { "versionSupport": true, "relatedInformation": false },
                    },
                },
            }),
            START_TIMEOUT,
        )?;
        if reply.get("error").is_some() {
            return Err(format!(
                "the server refused to initialize: {}",
                reply["error"]["message"]
            ));
        }
        session.notify("initialized", &json!({}))?;
        Ok(session)
    }

    fn request(&self, method: &str, params: &Value, timeout: Duration) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver): (Sender<Value>, Receiver<Value>) = channel();
        self.shared
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, sender);
        let sent = write_message(
            &self.writer,
            &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
        );
        if let Err(e) = sent {
            self.shared.dead.store(true, Ordering::SeqCst);
            return Err(format!("the server closed its input: {e}"));
        }
        receiver
            .recv_timeout(timeout)
            .map_err(|_| format!("`{method}` got no answer within {} ms", timeout.as_millis()))
    }

    fn notify(&self, method: &str, params: &Value) -> Result<(), String> {
        write_message(
            &self.writer,
            &json!({ "jsonrpc": "2.0", "method": method, "params": params }),
        )
        .map_err(|e| {
            self.shared.dead.store(true, Ordering::SeqCst);
            format!("the server closed its input: {e}")
        })
    }

    fn is_dead(&self) -> bool {
        self.shared.dead.load(Ordering::SeqCst)
    }

    /// Tell the server the file's new content, then wait for what it says.
    /// `None` when it said nothing in time.
    fn check(
        &self,
        path: &Path,
        language: &str,
        text: &str,
        wait: Duration,
    ) -> Option<Vec<Diagnostic>> {
        let _turn = self
            .sync
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let uri = path_to_uri(path);
        let baseline = self
            .shared
            .diagnostics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tick;
        let version = {
            let mut docs = self
                .docs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let version = docs.get(&uri).map_or(1, |v| v + 1);
            docs.insert(uri.clone(), version);
            version
        };
        let sent = if version == 1 {
            self.notify(
                "textDocument/didOpen",
                &json!({ "textDocument": { "uri": uri, "languageId": language, "version": version, "text": text } }),
            )
        } else {
            self.notify(
                "textDocument/didChange",
                &json!({
                    "textDocument": { "uri": uri, "version": version },
                    "contentChanges": [{ "text": text }],
                }),
            )
        };
        // Servers that run their checker on save (rust-analyzer's cargo check)
        // only report type errors after this.
        let sent = sent.and_then(|()| {
            self.notify(
                "textDocument/didSave",
                &json!({ "textDocument": { "uri": uri } }),
            )
        });
        if sent.is_err() {
            return None;
        }
        let wait = if self.misses.load(Ordering::SeqCst) >= 2 {
            wait.min(PROBE_WAIT)
        } else {
            wait
        };
        let deadline = Instant::now() + wait;
        let mut table = self
            .shared
            .diagnostics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let fresh = |table: &DiagTable| {
            table
                .by_uri
                .get(&uri)
                .filter(|p| p.tick > baseline && p.version.is_none_or(|v| v >= version))
                .map(|p| p.tick)
        };
        // First answer for this change.
        let mut seen = loop {
            if let Some(tick) = fresh(&table) {
                break tick;
            }
            let now = Instant::now();
            if now >= deadline || self.is_dead() {
                self.misses.fetch_add(1, Ordering::SeqCst);
                return None;
            }
            table = self
                .shared
                .changed
                .wait_timeout(table, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        };
        // Then let a quick follow-up replace it.
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let (guard, timed_out) = self
                .shared
                .changed
                .wait_timeout(table, QUIET.min(deadline - now))
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            table = guard;
            match fresh(&table) {
                Some(tick) if tick != seen => seen = tick,
                _ if timed_out.timed_out() => break,
                _ => {}
            }
        }
        self.misses.store(0, Ordering::SeqCst);
        table.by_uri.get(&uri).map(|p| p.items.clone())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // A polite goodbye, briefly; then make sure the process is gone.
        if !self.is_dead() {
            let _ = self.request("shutdown", &Value::Null, Duration::from_millis(500));
            let _ = self.notify("exit", &Value::Null);
        }
        let mut child = self
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let pid = child.id();
        #[cfg(unix)]
        {
            let _ = Command::new("kill")
                .args(["-KILL", "--", &format!("-{pid}")])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        #[cfg(windows)]
        {
            let _ = Command::new("taskkill")
                .args(["/T", "/F", "/PID", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

// ---------------------------------------------------------------------------
// The pool
// ---------------------------------------------------------------------------

enum Slot {
    Running(Arc<Session>),
    Failed { at: Instant, told: bool },
}

/// The language servers of one session, started on demand.
pub struct LspPool {
    config: LspConfig,
    root: PathBuf,
    slots: Mutex<HashMap<String, Slot>>,
}

impl std::fmt::Debug for LspPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspPool")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl LspPool {
    #[must_use]
    pub fn new(config: LspConfig, root: &Path) -> Self {
        Self {
            config,
            root: root.to_path_buf(),
            slots: Mutex::new(HashMap::new()),
        }
    }

    #[must_use]
    pub fn config(&self) -> &LspConfig {
        &self.config
    }

    /// The text for a tool result about `relative`, whose content is now
    /// `content`. `None` when no server claims the file or none answered.
    #[must_use]
    pub fn diagnostics(&self, relative: &str, content: &[u8]) -> Option<String> {
        let extension = Path::new(relative).extension()?.to_str()?;
        let spec = self.config.server_for(extension)?;
        let text = std::str::from_utf8(content).ok()?;
        let session = match self.session_for(spec) {
            Ok(session) => session,
            Err(note) => return note,
        };
        let language = spec
            .language_id
            .clone()
            .unwrap_or_else(|| language_id(extension).to_owned());
        let absolute = self.root.join(relative);
        let found = session.check(&absolute, &language, text, self.config.wait)?;
        Some(render(
            &spec.name,
            relative,
            &found,
            self.config.min_severity,
        ))
    }

    /// The running session for `spec`, starting it if needed. A failure is
    /// returned as the note to show, once; later attempts within the retry
    /// interval return no note.
    fn session_for(&self, spec: &ServerSpec) -> Result<Arc<Session>, Option<String>> {
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match slots.get_mut(&spec.name) {
            Some(Slot::Running(session)) if !session.is_dead() => return Ok(Arc::clone(session)),
            Some(Slot::Failed { at, .. }) if at.elapsed() < RETRY_AFTER => return Err(None),
            _ => {}
        }
        // Started with the lock held: a second write waits for the first to
        // finish starting the server instead of starting another.
        match Session::start(spec, &self.root) {
            Ok(session) => {
                let session = Arc::new(session);
                slots.insert(spec.name.clone(), Slot::Running(Arc::clone(&session)));
                Ok(session)
            }
            Err(why) => {
                let told = matches!(slots.get(&spec.name), Some(Slot::Failed { told: true, .. }));
                slots.insert(
                    spec.name.clone(),
                    Slot::Failed {
                        at: Instant::now(),
                        told: true,
                    },
                );
                Err((!told).then(|| format!("lsp ({}): not available — {why}", spec.name)))
            }
        }
    }
}

#[cfg(test)]
/// A tiny language server: it reports an error on any line containing
/// `ERROR` and a warning on any containing `WARN`. `mode` changes how it
/// behaves; see the script.
pub(crate) const FAKE_SERVER: &str = r#"
import json, os, sys, time

mode = os.environ.get("FAKE_LSP_MODE", "normal")

def read():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        line = line.decode().strip()
        if not line:
            break
        k, _, v = line.partition(":")
        headers[k.lower()] = v.strip()
    body = sys.stdin.buffer.read(int(headers["content-length"]))
    return json.loads(body)

def send(msg):
    data = json.dumps(msg).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(data) + data)
    sys.stdout.buffer.flush()

def diagnose(uri, text, version):
    items = []
    for n, line in enumerate(text.split("\n")):
        if "ERROR" in line:
            items.append({"range": {"start": {"line": n, "character": line.index("ERROR")}, "end": {"line": n, "character": 5}},
                          "severity": 1, "code": "E1", "message": "bad token\nsecond line"})
        if "WARN" in line:
            items.append({"range": {"start": {"line": n, "character": 0}, "end": {"line": n, "character": 1}},
                          "severity": 2, "message": "suspicious"})
    return items

def publish(uri, items, version):
    send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
          "params": {"uri": uri, "version": version, "diagnostics": items}})

while True:
    msg = read()
    if msg is None:
        break
    method = msg.get("method")
    if method == "initialize":
        if mode == "child":
            import subprocess
            helper = subprocess.Popen(["sleep", "60"])
            with open(os.environ["FAKE_LSP_PIDFILE"], "w") as f:
                f.write(str(helper.pid))
        if mode == "requests":
            # The server asks things of the client before it will proceed.
            send({"jsonrpc": "2.0", "id": 900, "method": "workspace/configuration", "params": {"items": [{"section": "a"}, {"section": "b"}]}})
            send({"jsonrpc": "2.0", "id": 901, "method": "window/workDoneProgress/create", "params": {"token": "t"}})
            # It will not finish initialising until both are answered properly.
            answered = {}
            while len(answered) < 2:
                reply = read()
                if reply is None:
                    sys.exit(3)
                if "result" in reply:
                    answered[reply["id"]] = reply["result"]
            if answered.get(900) != [None, None]:
                sys.exit(4)
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"capabilities": {"textDocumentSync": 1}}})
        if mode == "crash":
            sys.exit(0)
    elif method in ("textDocument/didOpen", "textDocument/didChange"):
        td = msg["params"]["textDocument"]
        text = td["text"] if method == "textDocument/didOpen" else msg["params"]["contentChanges"][0]["text"]
        if mode == "silent":
            continue
        if mode == "slow":
            time.sleep(0.3)
        if mode == "double":
            publish(td["uri"], [], td.get("version"))
            time.sleep(0.05)
        publish(td["uri"], diagnose(td["uri"], text, td.get("version")), td.get("version"))
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": None})
    elif method == "exit":
        break
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn python() -> Option<&'static str> {
        ["python3", "python"].into_iter().find(|p| {
            Command::new(p)
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        })
    }

    struct Dir(PathBuf);

    impl Dir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "rapid-lsp-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("dir");
            std::fs::write(path.join("fake_lsp.py"), FAKE_SERVER).expect("script");
            Self(path)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    fn pool(dir: &Dir, mode: &str, wait_ms: u64) -> Option<LspPool> {
        let python = python()?;
        let script = dir.0.join("fake_lsp.py");
        // `env` carries the mode to the child without touching this process's
        // own environment, which other tests share.
        let (config, problems) = parse_with(
            &json!({ "lsp": {
                "defaults": false,
                "wait_ms": wait_ms,
                "min_severity": "warning",
                "servers": { "fake": {
                    "command": ["env", format!("FAKE_LSP_MODE={mode}"), python, script.to_string_lossy()],
                    "extensions": ["txt"],
                } },
            } }),
            &|_| true,
        );
        assert!(problems.is_empty(), "{problems:?}");
        Some(LspPool::new(config.expect("config"), &dir.0))
    }

    #[test]
    fn settings_default_to_off_and_true_means_the_servers_found_on_path() {
        assert_eq!(parse(&json!({})).0, None);
        assert_eq!(parse(&json!({ "lsp": false })).0, None);
        let (config, problems) = parse_with(&json!({ "lsp": true }), &|p| {
            p == "gopls" || p == "rust-analyzer"
        });
        assert!(problems.is_empty());
        let names: Vec<_> = config
            .expect("config")
            .servers
            .iter()
            .map(|s| s.name.clone())
            .collect();
        assert_eq!(names, ["rust", "go"]);
        assert_eq!(parse_with(&json!({ "lsp": true }), &|_| false).0, None);
    }

    #[test]
    fn an_object_configures_overrides_disables_and_reports_what_it_drops() {
        let (config, problems) = parse_with(
            &json!({ "lsp": {
                "wait_ms": 99999999,
                "min_severity": "loud",
                "servers": {
                    "rust": { "command": ["my-ra"], "extensions": [".rs"] },
                    "go": false,
                    "broken": { "command": [] },
                    "noext": { "command": ["x"] },
                }
            } }),
            &|_| true,
        );
        let config = config.expect("config");
        assert_eq!(config.wait, MAX_WAIT);
        let rust = config
            .servers
            .iter()
            .find(|s| s.name == "rust")
            .expect("rust");
        assert_eq!(rust.command, ["my-ra"], "an override replaces the built-in");
        assert_eq!(rust.extensions, ["rs"]);
        assert!(
            !config.servers.iter().any(|s| s.name == "go"),
            "disabled with false"
        );
        assert!(
            config.servers.iter().any(|s| s.name == "pyright"),
            "other built-ins remain"
        );
        assert_eq!(problems.len(), 3, "{problems:?}");
    }

    #[test]
    fn uris_are_percent_encoded_and_use_forward_slashes() {
        assert_eq!(
            path_to_uri(Path::new("/a/b c/d#e.rs")),
            "file:///a/b%20c/d%23e.rs"
        );
        assert_eq!(
            path_to_uri(Path::new("C:\\proj\\a.rs")),
            "file:///C:/proj/a.rs"
        );
    }

    #[test]
    fn diagnostics_are_ranked_capped_and_stripped_of_control_characters() {
        let found: Vec<Diagnostic> = (1..=12)
            .map(|n| Diagnostic {
                line: n,
                column: 1,
                severity: if n % 2 == 0 {
                    Severity::Warning
                } else {
                    Severity::Error
                },
                code: Some("X".to_owned()),
                message: format!("m{n}\u{1b}[31m\nsecond line"),
            })
            .collect();
        let text = render("s", "a.rs", &found, Severity::Warning);
        assert!(
            text.starts_with("lsp (s): 12 problem(s) in a.rs (6 error(s))"),
            "{text}"
        );
        assert_eq!(text.lines().count(), 1 + MAX_SHOWN + 1, "{text}");
        assert!(text.contains("a.rs:1:1 error [X] m1[31m"), "{text}");
        assert!(!text.contains("second line") && !text.contains('\u{1b}'));
        assert!(text.ends_with("... and 2 more"));
        // Below the threshold, a warning is not shown.
        let only_warning = [found[1].clone()];
        assert!(render("s", "a.rs", &only_warning, Severity::Error).contains("no problems"));
    }

    #[cfg(unix)]
    #[test]
    fn a_server_reports_the_problems_in_a_written_file() {
        let dir = Dir::new("report");
        let Some(pool) = pool(&dir, "normal", 5000) else {
            return;
        };
        let text = pool
            .diagnostics("a.txt", b"fine\nthis has an ERROR here\nWARN me\n")
            .expect("an answer");
        assert!(
            text.contains("2 problem(s) in a.txt (1 error(s))"),
            "{text}"
        );
        assert!(text.contains("a.txt:2:13 error [E1] bad token"), "{text}");
        assert!(text.contains("a.txt:3:1 warning suspicious"), "{text}");
        // The same server answers a second change to the same file, and a clean
        // file is reported as clean rather than as silence.
        let clean = pool.diagnostics("a.txt", b"all good\n").expect("an answer");
        assert!(clean.contains("no problems reported in a.txt"), "{clean}");
    }

    #[cfg(unix)]
    #[test]
    fn a_stale_first_publish_is_replaced_by_the_one_that_follows() {
        let dir = Dir::new("double");
        let Some(pool) = pool(&dir, "double", 5000) else {
            return;
        };
        let text = pool.diagnostics("a.txt", b"ERROR\n").expect("an answer");
        assert!(
            text.contains("1 problem(s)"),
            "the empty first set must not win: {text}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn requests_the_server_makes_of_the_client_are_answered_so_it_never_stalls() {
        let dir = Dir::new("requests");
        let Some(pool) = pool(&dir, "requests", 5000) else {
            return;
        };
        let text = pool.diagnostics("a.txt", b"ERROR\n").expect("an answer");
        assert!(
            text.contains("1 problem(s)"),
            "diagnostics, not a start-up failure note: {text}"
        );
        assert_eq!(
            answer_server_request("workspace/configuration", &json!({ "items": [1, 2, 3] })),
            json!([null, null, null])
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_silent_server_costs_a_bounded_wait_then_only_a_probe() {
        let dir = Dir::new("silent");
        let Some(pool) = pool(&dir, "silent", 600) else {
            return;
        };
        let started = Instant::now();
        assert_eq!(pool.diagnostics("a.txt", b"x\n"), None);
        assert_eq!(pool.diagnostics("a.txt", b"y\n"), None);
        let after_two = started.elapsed();
        assert!(
            after_two >= Duration::from_millis(1200),
            "two full waits: {after_two:?}"
        );
        let before = Instant::now();
        assert_eq!(pool.diagnostics("a.txt", b"z\n"), None);
        assert!(
            before.elapsed() < Duration::from_millis(550),
            "after two misses the wait is a probe: {:?}",
            before.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn stopping_the_pool_stops_the_helpers_a_server_started() {
        let dir = Dir::new("helpers");
        let Some(python) = python() else { return };
        let pidfile = dir.0.join("helper.pid");
        let script = dir.0.join("fake_lsp.py");
        let (config, _) = parse_with(
            &json!({ "lsp": { "defaults": false, "wait_ms": 5000, "servers": { "fake": {
                "command": ["env", "FAKE_LSP_MODE=child",
                            format!("FAKE_LSP_PIDFILE={}", pidfile.display()),
                            python, script.to_string_lossy()],
                "extensions": ["txt"] } } } }),
            &|_| true,
        );
        let pool = LspPool::new(config.expect("config"), &dir.0);
        assert!(pool.diagnostics("a.txt", b"ERROR\n").is_some());
        let helper: u32 = std::fs::read_to_string(&pidfile)
            .expect("the server started a helper")
            .trim()
            .parse()
            .expect("pid");
        let alive = |pid: u32| {
            Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };
        assert!(alive(helper), "the helper runs while the pool does");
        drop(pool);
        let mut gone = false;
        for _ in 0..100 {
            if !alive(helper) {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if !gone {
            let _ = Command::new("kill")
                .args(["-KILL", &helper.to_string()])
                .status();
        }
        assert!(gone, "the helper outlived the pool");
    }

    #[cfg(unix)]
    #[test]
    fn a_server_that_exits_is_reported_once_and_not_retried_at_once() {
        let dir = Dir::new("crash");
        let Some(pool) = pool(&dir, "crash", 1500) else {
            return;
        };
        // The first write may find the process already gone while starting.
        let first = pool.diagnostics("a.txt", b"ERROR\n");
        let second = pool.diagnostics("a.txt", b"ERROR\n");
        let third = pool.diagnostics("a.txt", b"ERROR\n");
        let notes: Vec<_> = [first, second, third]
            .into_iter()
            .flatten()
            .filter(|n| n.contains("not available"))
            .collect();
        assert!(notes.len() <= 1, "said at most once: {notes:?}");
    }

    #[test]
    fn a_missing_program_is_reported_once_as_unavailable() {
        let (config, _) = parse_with(
            &json!({ "lsp": { "defaults": false, "servers": { "ghost": {
                "command": ["definitely-not-a-language-server-xyz"], "extensions": ["txt"] } } } }),
            &|_| true,
        );
        let pool = LspPool::new(config.expect("config"), &std::env::temp_dir());
        let first = pool
            .diagnostics("a.txt", b"x")
            .expect("a note the first time");
        assert!(
            first.contains("not available") && first.contains("could not start"),
            "{first}"
        );
        assert_eq!(pool.diagnostics("a.txt", b"x"), None, "and silence after");
    }

    #[test]
    fn a_file_no_server_claims_or_that_is_not_text_is_ignored() {
        let (config, _) = parse_with(
            &json!({ "lsp": { "defaults": false, "servers": { "g": { "command": ["x"], "extensions": ["rs"] } } } }),
            &|_| true,
        );
        let pool = LspPool::new(config.expect("config"), &std::env::temp_dir());
        assert_eq!(pool.diagnostics("a.py", b"x"), None);
        assert_eq!(pool.diagnostics("noextension", b"x"), None);
        assert_eq!(pool.diagnostics("a.rs", &[0xff, 0xfe]), None);
    }

    #[test]
    fn message_framing_round_trips_and_rejects_oversized_bodies() {
        let body = br#"{"jsonrpc":"2.0","method":"x"}"#;
        let framed = [
            format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes(),
            body.to_vec(),
        ]
        .concat();
        let mut reader = BufReader::new(&framed[..]);
        assert_eq!(read_message(&mut reader).expect("message")["method"], "x");
        let huge = format!("Content-Length: {}\r\n\r\n", MAX_BODY_BYTES + 1);
        assert!(read_message(&mut BufReader::new(huge.as_bytes())).is_none());
        assert!(read_message(&mut BufReader::new(&b""[..])).is_none());
    }
}
