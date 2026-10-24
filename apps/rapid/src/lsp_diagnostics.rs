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
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
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
/// Longest diagnostic code, server name or path shown.
const MAX_CODE_CHARS: usize = 60;
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

/// Where `program` is, if it is an executable file: as given when it names a
/// path, else the first match on `PATH` (with the platform's executable
/// extensions on Windows).
fn find_program(program: &str) -> Option<PathBuf> {
    let executable = |path: &Path| {
        let Ok(meta) = std::fs::metadata(path) else {
            return false;
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            meta.is_file() && meta.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        {
            meta.is_file()
        }
    };
    if program.contains(['/', '\\']) {
        return executable(Path::new(program)).then(|| PathBuf::from(program));
    }
    let paths = std::env::var_os("PATH")?;
    let extensions: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT".to_owned())
            .split(';')
            .map(str::to_owned)
            .collect()
    } else {
        vec![String::new()]
    };
    std::env::split_paths(&paths).find_map(|dir| {
        extensions
            .iter()
            .map(|ext| dir.join(format!("{program}{ext}")))
            .find(|candidate| executable(candidate))
    })
}

/// Whether `program` is an executable on `PATH`.
fn on_path(program: &str) -> bool {
    find_program(program).is_some()
}

/// What to run for `program`, and the words to put before its arguments. On
/// Windows an npm-installed server is a `.cmd` shim, which only a shell runs.
fn resolve_program(program: &str) -> Result<(PathBuf, Vec<String>), String> {
    let found = find_program(program)
        .ok_or_else(|| format!("could not start `{program}`: not found on PATH"))?;
    #[cfg(windows)]
    if found
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"))
    {
        return Ok((
            PathBuf::from("cmd"),
            vec!["/C".to_owned(), found.to_string_lossy().into_owned()],
        ));
    }
    Ok((found, Vec::new()))
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
                        .map(|code| plain(code, MAX_CODE_CHARS))
                        .or_else(|| item["code"].as_i64().map(|n| n.to_string()))
                        .filter(|code| !code.is_empty()),
                    message: item["message"].as_str().unwrap_or("").to_owned(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `text` as one plain run: no control, bidirectional or zero-width characters,
/// cut at `max` characters. Text from a server is shown to the model, and a
/// server is a program the project chose, not one to trust with the layout of a
/// tool result.
fn plain(text: &str, max: usize) -> String {
    text.chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(
                    c,
                    '\u{200b}'..='\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2066}'..='\u{2069}'
                        | '\u{2028}'
                        | '\u{2029}'
                        | '\u{feff}'
                )
        })
        .take(max)
        .collect()
}

/// The text added to a tool result.
fn render(server: &str, relative: &str, found: &[Diagnostic], min: Severity) -> String {
    let mut shown: Vec<&Diagnostic> = found.iter().filter(|d| d.severity <= min).collect();
    let server = plain(server, MAX_CODE_CHARS);
    let relative = plain(relative, 200);
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
        let message = plain(d.message.lines().next().unwrap_or(""), MAX_MESSAGE_CHARS);
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
    /// Bumped on every publish; orders publishes against one another.
    tick: u64,
    /// Keyed by [`normalize_uri`], so the spelling a server uses does not matter.
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

impl Shared {
    /// The server is gone: wake every waiter, and fail every request still
    /// waiting for an answer that will never come.
    fn mark_dead(&self) {
        self.dead.store(true, Ordering::SeqCst);
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.changed.notify_all();
    }
}

/// Bytes queued for a server that is not reading, past which it is treated as
/// hung. A server that has stopped reading its input is not going to answer.
const MAX_QUEUED_BYTES: usize = 64 * 1024 * 1024;

/// Messages to a server, written by a thread of their own.
///
/// A caller never writes to the pipe itself: a server that stops reading (busy
/// indexing, stuck) would fill the pipe and block the writer for good, and with
/// it whatever lock the writer held. Queuing cannot block, and the thread that
/// can is the only one that suffers.
struct Outbox {
    queue: Sender<Vec<u8>>,
    queued: Arc<AtomicUsize>,
}

impl Outbox {
    fn start(mut pipe: ChildStdin, shared: Arc<Shared>) -> Self {
        let (queue, receiver): (Sender<Vec<u8>>, Receiver<Vec<u8>>) = channel();
        let queued = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&queued);
        std::thread::spawn(move || {
            while let Ok(bytes) = receiver.recv() {
                counter.fetch_sub(bytes.len(), Ordering::SeqCst);
                if pipe.write_all(&bytes).and_then(|()| pipe.flush()).is_err() {
                    shared.mark_dead();
                    return;
                }
            }
        });
        Self { queue, queued }
    }

    fn send(&self, message: &Value) -> Result<(), String> {
        let body = serde_json::to_vec(message).map_err(|e| e.to_string())?;
        let mut framed = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        framed.extend_from_slice(&body);
        if self.queued.fetch_add(framed.len(), Ordering::SeqCst) + framed.len() > MAX_QUEUED_BYTES {
            return Err("the server is not reading its input".to_owned());
        }
        self.queue
            .send(framed)
            .map_err(|_| "the server closed its input".to_owned())
    }
}

/// What one read from a server produced.
enum Incoming {
    Message(Value),
    /// A message that could not be used (not JSON, or oversized): skipped.
    Skipped,
    /// The server closed its output.
    End,
}

/// Longest header line accepted.
const MAX_HEADER_BYTES: u64 = 8 * 1024;

fn read_message(reader: &mut impl BufRead) -> Incoming {
    let mut length: Option<usize> = None;
    loop {
        let mut line = String::new();
        match reader.by_ref().take(MAX_HEADER_BYTES).read_line(&mut line) {
            Ok(0) | Err(_) => return Incoming::End,
            Ok(_) => {}
        }
        if !line.ends_with('\n') {
            // A header with no end within the limit: the stream is not LSP.
            return Incoming::End;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().ok();
        }
    }
    let Some(length) = length else {
        return Incoming::Skipped;
    };
    if length > MAX_BODY_BYTES {
        // Read and drop it, so the stream stays in step.
        let mut remaining = length as u64;
        let mut sink = [0u8; 8192];
        while remaining > 0 {
            let want = usize::try_from(remaining.min(8192)).unwrap_or(8192);
            match reader.read(&mut sink[..want]) {
                Ok(0) | Err(_) => return Incoming::End,
                Ok(n) => remaining -= n as u64,
            }
        }
        return Incoming::Skipped;
    }
    let mut body = vec![0u8; length];
    if reader.read_exact(&mut body).is_err() {
        return Incoming::End;
    }
    serde_json::from_slice(&body).map_or(Incoming::Skipped, Incoming::Message)
}

/// The reply to a request the server made of us. Empty, but the shape the
/// protocol asks for.
fn answer_server_request(method: &str, params: &Value, root_uri: &str) -> Value {
    match method {
        // One entry per requested section, none of them configured.
        "workspace/configuration" => {
            let count = params["items"].as_array().map_or(0, Vec::len);
            Value::Array(vec![Value::Null; count])
        }
        "workspace/workspaceFolders" => json!([{ "uri": root_uri, "name": "project" }]),
        // The client does not apply edits or show documents on a server's behalf.
        "workspace/applyEdit" => json!({ "applied": false }),
        "window/showDocument" => json!({ "success": false }),
        _ => Value::Null,
    }
}

fn reader_loop(stdout: impl Read, shared: &Arc<Shared>, outbox: &Outbox, root_uri: &str) {
    let mut reader = BufReader::new(stdout);
    loop {
        let message = match read_message(&mut reader) {
            Incoming::Message(message) => message,
            Incoming::Skipped => continue,
            Incoming::End => break,
        };
        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id");
        match (method, id) {
            (Some(method), Some(id)) => {
                let reply = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": answer_server_request(method, &message["params"], root_uri),
                });
                let _ = outbox.send(&reply);
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
                        normalize_uri(uri),
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
    shared.mark_dead();
}

/// A URI in a form that two spellings of one file share: percent escapes
/// decoded, separators forward, a Windows drive letter lower-cased, no
/// trailing slash. Servers differ in what they escape (`@`, `(`, `[`, a drive
/// colon), and an exact comparison would never match them.
fn normalize_uri(uri: &str) -> String {
    let rest = uri.strip_prefix("file://").unwrap_or(uri);
    let bytes = rest.as_bytes();
    let mut decoded: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let escaped = (bytes[i] == b'%' && i + 3 <= bytes.len())
            .then(|| std::str::from_utf8(&bytes[i + 1..i + 3]).ok())
            .flatten()
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        if let Some(byte) = escaped {
            decoded.push(byte);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    let mut text = String::from_utf8_lossy(&decoded).replace('\\', "/");
    // `/C:/x` and `/c:/x` are one file.
    if text.len() > 2
        && text.as_bytes()[0] == b'/'
        && text.as_bytes()[1].is_ascii_alphabetic()
        && text.as_bytes()[2] == b':'
    {
        let drive = text.as_bytes()[1].to_ascii_lowercase() as char;
        text.replace_range(1..2, &drive.to_string());
    }
    while text.len() > 1 && text.ends_with('/') {
        text.pop();
    }
    text
}

struct Session {
    child: Mutex<Child>,
    outbox: Outbox,
    shared: Arc<Shared>,
    next_id: AtomicU64,
    /// Document versions only ever rise, across every file and every open.
    next_version: AtomicI64,
    misses: AtomicU32,
}

impl Session {
    fn start(spec: &ServerSpec, root: &Path) -> Result<Self, String> {
        let (program, leading) = resolve_program(&spec.command[0])?;
        let mut command = Command::new(program);
        command
            .args(leading)
            .args(&spec.command[1..])
            .current_dir(root)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (key, value) in std::env::vars_os() {
            if env_kept(&key.to_string_lossy()) {
                command.env(key, value);
            }
        }
        for (key, value) in protocol::host_env::platform_base_env() {
            command.env(key, value);
        }
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
        let shared = Arc::new(Shared {
            diagnostics: Mutex::new(DiagTable::default()),
            changed: Condvar::new(),
            pending: Mutex::new(HashMap::new()),
            dead: AtomicBool::new(false),
        });
        let outbox = Outbox::start(stdin, Arc::clone(&shared));
        let root_uri = path_to_uri(root);
        {
            let shared = Arc::clone(&shared);
            let reader_outbox = Outbox {
                queue: outbox.queue.clone(),
                queued: Arc::clone(&outbox.queued),
            };
            let root_uri = root_uri.clone();
            std::thread::spawn(move || reader_loop(stdout, &shared, &reader_outbox, &root_uri));
        }
        let session = Self {
            child: Mutex::new(child),
            outbox,
            shared,
            next_id: AtomicU64::new(1),
            next_version: AtomicI64::new(1),
            misses: AtomicU32::new(0),
        };
        let reply = session.request(
            "initialize",
            &json!({
                "processId": std::process::id(),
                "rootUri": root_uri,
                "workspaceFolders": [{ "uri": root_uri, "name": root.file_name().map_or_else(|| "project".to_owned(), |n| n.to_string_lossy().into_owned()) }],
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
        {
            let mut pending = self
                .shared
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Checked under the lock `mark_dead` takes, so a server that has
            // just gone is seen here and not waited on.
            if self.is_dead() {
                return Err("the server exited".to_owned());
            }
            pending.insert(id, sender);
        }
        if let Err(e) = self
            .outbox
            .send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
        {
            self.shared.mark_dead();
            return Err(e);
        }
        receiver.recv_timeout(timeout).map_err(|e| match e {
            RecvTimeoutError::Disconnected => "the server exited before it answered".to_owned(),
            RecvTimeoutError::Timeout => {
                format!("`{method}` got no answer within {} ms", timeout.as_millis())
            }
        })
    }

    fn notify(&self, method: &str, params: &Value) -> Result<(), String> {
        self.outbox
            .send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .inspect_err(|_| self.shared.mark_dead())
    }

    fn is_dead(&self) -> bool {
        self.shared.dead.load(Ordering::SeqCst)
    }

    /// Tell the server the file's content, wait for what it says, and let go of
    /// the file again. `None` when it said nothing in time.
    ///
    /// The file is closed afterwards. It is on disk exactly as sent, so the
    /// server loses nothing by reading it from there, and an open buffer would
    /// otherwise outlive a later change made by another route (a shell
    /// command, an undo, an editor) and mislead the next check of another file.
    fn check(
        &self,
        path: &Path,
        language: &str,
        text: &str,
        wait: Duration,
    ) -> Option<Vec<Diagnostic>> {
        let uri = path_to_uri(path);
        let key = normalize_uri(&uri);
        let version = self.next_version.fetch_add(1, Ordering::SeqCst);
        // What this file's last answer was, so only a newer one counts. Per file,
        // so another file's traffic neither hides nor fakes this one's answer.
        let baseline = self
            .shared
            .diagnostics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .by_uri
            .get(&key)
            .map_or(0, |p| p.tick);
        let sent = self
            .notify(
                "textDocument/didOpen",
                &json!({ "textDocument": { "uri": uri, "languageId": language, "version": version, "text": text } }),
            )
            // Servers that run their checker on save (rust-analyzer's cargo
            // check) report type errors only after this.
            .and_then(|()| self.notify("textDocument/didSave", &json!({ "textDocument": { "uri": uri } })));
        if sent.is_err() {
            return None;
        }
        let wait = if self.misses.load(Ordering::SeqCst) >= 2 {
            wait.min(PROBE_WAIT)
        } else {
            wait
        };
        let deadline = Instant::now() + wait;
        let result = self.wait_for(&key, baseline, version, deadline);
        let _ = self.notify(
            "textDocument/didClose",
            &json!({ "textDocument": { "uri": uri } }),
        );
        match &result {
            Some(_) => self.misses.store(0, Ordering::SeqCst),
            None => {
                self.misses.fetch_add(1, Ordering::SeqCst);
            }
        }
        result
    }

    fn wait_for(
        &self,
        key: &str,
        baseline: u64,
        version: i64,
        deadline: Instant,
    ) -> Option<Vec<Diagnostic>> {
        let mut table = self
            .shared
            .diagnostics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let fresh = |table: &DiagTable| {
            table
                .by_uri
                .get(key)
                .filter(|p| p.tick > baseline && p.version.is_none_or(|v| v >= version))
                .map(|p| p.tick)
        };
        // The first answer for this change.
        let mut seen = loop {
            if let Some(tick) = fresh(&table) {
                break tick;
            }
            let now = Instant::now();
            if now >= deadline || self.is_dead() {
                return None;
            }
            table = self
                .shared
                .changed
                .wait_timeout(table, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        };
        // Then let a quick follow-up replace it. The quiet period is measured
        // from this file's last answer, so a server busy with other files
        // cannot stretch it.
        let mut last_change = Instant::now();
        loop {
            let now = Instant::now();
            if now >= deadline || now.duration_since(last_change) >= QUIET {
                break;
            }
            let until = (last_change + QUIET).min(deadline);
            table = self
                .shared
                .changed
                .wait_timeout(table, until.saturating_duration_since(now))
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
            match fresh(&table) {
                Some(tick) if tick != seen => {
                    seen = tick;
                    last_change = Instant::now();
                }
                _ => {}
            }
        }
        table.by_uri.get(key).map(|p| p.items.clone())
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

/// Environment variables a language server is given: what it needs to find its
/// toolchain, caches and configuration, and nothing that looks like a
/// credential. The process's own environment holds provider keys and tokens.
fn env_kept(name: &str) -> bool {
    const EXACT: &[&str] = &[
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "LANG",
        "TMPDIR",
        "TEMP",
        "TMP",
        "SHELL",
        "TERM",
        "VIRTUAL_ENV",
        "PYTHONPATH",
        "GOPATH",
        "GOROOT",
        "GOFLAGS",
        "GOCACHE",
        "GOMODCACHE",
        "JAVA_HOME",
        "NVM_DIR",
        "VOLTA_HOME",
        "PNPM_HOME",
        "SDKROOT",
        "DEVELOPER_DIR",
    ];
    const PREFIXES: &[&str] = &[
        "LC_",
        "XDG_",
        "CARGO_",
        "RUSTUP_",
        "RUST_",
        "NODE_",
        "NPM_CONFIG_",
    ];
    let upper = name.to_ascii_uppercase();
    if upper.contains("TOKEN")
        || upper.contains("SECRET")
        || upper.contains("PASSWORD")
        || upper.contains("API_KEY")
    {
        return false;
    }
    EXACT.contains(&upper.as_str()) || PREFIXES.iter().any(|p| upper.starts_with(p))
}

// ---------------------------------------------------------------------------
// The pool
// ---------------------------------------------------------------------------

enum Slot {
    Running(Arc<Session>),
    Failed { at: Instant },
}

/// The language servers of one session, started on demand.
pub struct LspPool {
    config: LspConfig,
    /// The project root with links resolved: what servers are told, and what
    /// every file URI is built from.
    root: PathBuf,
    slots: Mutex<HashMap<String, Slot>>,
    /// One lock per server name, held while that server starts, so a slow start
    /// blocks writes of its own language and no other.
    starting: Mutex<HashMap<String, Arc<Mutex<()>>>>,
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
            root: std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()),
            slots: Mutex::new(HashMap::new()),
            starting: Mutex::new(HashMap::new()),
        }
    }

    #[must_use]
    pub fn config(&self) -> &LspConfig {
        &self.config
    }

    /// The text for a tool result about the file `relative` under the tools'
    /// root `tool_root`, whose content is now `content`. `None` when no server
    /// claims the file or none answered.
    ///
    /// Tools rooted somewhere other than the pool's root (a subagent working in
    /// its own worktree) get nothing: the server watches the pool's project, and
    /// a draft from another tree would be checked against the wrong files. The
    /// file's URI is built from the pool's own root and the cleaned relative
    /// path, never from the model's spelling of it (`./a.rs`, `src//a.rs`).
    #[must_use]
    pub fn diagnostics(&self, tool_root: &Path, relative: &str, content: &[u8]) -> Option<String> {
        let tool_root =
            std::fs::canonicalize(tool_root).unwrap_or_else(|_| tool_root.to_path_buf());
        if tool_root != self.root {
            return None;
        }
        let relative_path = Path::new(relative);
        let extension = relative_path.extension()?.to_str()?;
        let spec = self.config.server_for(extension)?;
        let text = std::str::from_utf8(content).ok()?;
        let clean: PathBuf = relative_path
            .components()
            .filter(|c| matches!(c, std::path::Component::Normal(_)))
            .collect();
        let shown = clean
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        let session = match self.session_for(spec) {
            Ok(session) => session,
            Err(note) => return note,
        };
        let language = spec
            .language_id
            .clone()
            .unwrap_or_else(|| language_id(extension).to_owned());
        let found = session.check(&self.root.join(&clean), &language, text, self.config.wait)?;
        Some(render(&spec.name, &shown, &found, self.config.min_severity))
    }

    /// The running session for `spec`, starting it if needed. A failure is
    /// returned as the note to show, once; later attempts within the retry
    /// interval return no note.
    fn session_for(&self, spec: &ServerSpec) -> Result<Arc<Session>, Option<String>> {
        let lock = {
            let mut starting = self
                .starting
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Arc::clone(starting.entry(spec.name.clone()).or_default())
        };
        let _starting = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut note = None;
        {
            let mut slots = self
                .slots
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match slots.get(&spec.name) {
                Some(Slot::Running(session)) if !session.is_dead() => {
                    return Ok(Arc::clone(session));
                }
                Some(Slot::Running(_)) => {
                    // It was running and has gone. Leave it alone for the retry
                    // interval rather than starting it again on every write.
                    slots.insert(spec.name.clone(), Slot::Failed { at: Instant::now() });
                    return Err(Some(format!(
                        "lsp ({}): the server exited; it will be started again in {} s",
                        spec.name,
                        RETRY_AFTER.as_secs()
                    )));
                }
                Some(Slot::Failed { at }) if at.elapsed() < RETRY_AFTER => return Err(None),
                _ => {}
            }
        }
        // Started without the slots lock: a server that takes its time to
        // start holds up only its own language.
        let started = Session::start(spec, &self.root);
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match started {
            Ok(session) => {
                let session = Arc::new(session);
                slots.insert(spec.name.clone(), Slot::Running(Arc::clone(&session)));
                Ok(session)
            }
            Err(why) => {
                let was_failed = matches!(slots.get(&spec.name), Some(Slot::Failed { .. }));
                slots.insert(spec.name.clone(), Slot::Failed { at: Instant::now() });
                if !was_failed {
                    note = Some(format!("lsp ({}): not available — {why}", spec.name));
                }
                Err(note)
            }
        }
    }
}

#[cfg(test)]
/// A tiny language server: it reports an error on any line containing
/// `ERROR` and a warning on any containing `WARN`. `mode` changes how it
/// behaves; see the script.
pub(crate) const FAKE_SERVER: &str = r#"
import json, os, sys, time, urllib.parse

mode = os.environ.get("FAKE_LSP_MODE", "normal")
log_path = os.environ.get("FAKE_LSP_LOG")

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
    if log_path and method:
        with open(log_path, "a") as f:
            f.write(method + "\n")
    if method == "initialize":
        if mode == "die_early":
            sys.exit(0)
        if mode == "slow_start":
            time.sleep(2)
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
        if mode == "deaf":
            time.sleep(120)
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
        shown_uri = urllib.parse.unquote(td["uri"]) if mode == "reencode" else td["uri"]
        publish(shown_uri, diagnose(td["uri"], text, td.get("version")), td.get("version"))
        if mode == "exit_after_first":
            sys.exit(0)
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": None})
    elif method == "exit":
        break
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// `bytes` as the content of `relative` under the pool's own root.
    fn check(pool: &LspPool, relative: &str, bytes: &[u8]) -> Option<String> {
        pool.diagnostics(&pool.root, relative, bytes)
    }

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
        let text =
            check(&pool, "a.txt", b"fine\nthis has an ERROR here\nWARN me\n").expect("an answer");
        assert!(
            text.contains("2 problem(s) in a.txt (1 error(s))"),
            "{text}"
        );
        assert!(text.contains("a.txt:2:13 error [E1] bad token"), "{text}");
        assert!(text.contains("a.txt:3:1 warning suspicious"), "{text}");
        // The same server answers a second change to the same file, and a clean
        // file is reported as clean rather than as silence.
        let clean = check(&pool, "a.txt", b"all good\n").expect("an answer");
        assert!(clean.contains("no problems reported in a.txt"), "{clean}");
    }

    #[cfg(unix)]
    #[test]
    fn a_stale_first_publish_is_replaced_by_the_one_that_follows() {
        let dir = Dir::new("double");
        let Some(pool) = pool(&dir, "double", 5000) else {
            return;
        };
        let text = check(&pool, "a.txt", b"ERROR\n").expect("an answer");
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
        let text = check(&pool, "a.txt", b"ERROR\n").expect("an answer");
        assert!(
            text.contains("1 problem(s)"),
            "diagnostics, not a start-up failure note: {text}"
        );
        assert_eq!(
            answer_server_request(
                "workspace/configuration",
                &json!({ "items": [1, 2, 3] }),
                "file:///r"
            ),
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
        assert_eq!(check(&pool, "a.txt", b"x\n"), None);
        assert_eq!(check(&pool, "a.txt", b"y\n"), None);
        let after_two = started.elapsed();
        assert!(
            after_two >= Duration::from_millis(1200),
            "two full waits: {after_two:?}"
        );
        let before = Instant::now();
        assert_eq!(check(&pool, "a.txt", b"z\n"), None);
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
        assert!(check(&pool, "a.txt", b"ERROR\n").is_some());
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
        let first = check(&pool, "a.txt", b"ERROR\n");
        let second = check(&pool, "a.txt", b"ERROR\n");
        let third = check(&pool, "a.txt", b"ERROR\n");
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
        let first = check(&pool, "a.txt", b"x").expect("a note the first time");
        assert!(
            first.contains("not available") && first.contains("could not start"),
            "{first}"
        );
        assert_eq!(check(&pool, "a.txt", b"x"), None, "and silence after");
    }

    #[test]
    fn a_file_no_server_claims_or_that_is_not_text_is_ignored() {
        let (config, _) = parse_with(
            &json!({ "lsp": { "defaults": false, "servers": { "g": { "command": ["x"], "extensions": ["rs"] } } } }),
            &|_| true,
        );
        let pool = LspPool::new(config.expect("config"), &std::env::temp_dir());
        assert_eq!(check(&pool, "a.py", b"x"), None);
        assert_eq!(check(&pool, "noextension", b"x"), None);
        assert_eq!(check(&pool, "a.rs", &[0xff, 0xfe]), None);
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
        match read_message(&mut reader) {
            Incoming::Message(message) => assert_eq!(message["method"], "x"),
            _ => panic!("expected a message"),
        }
        assert!(matches!(
            read_message(&mut BufReader::new(&b""[..])),
            Incoming::End
        ));
    }

    #[test]
    fn headers_match_in_any_case_and_a_bad_body_is_skipped_not_fatal() {
        let good = br#"{"jsonrpc":"2.0","method":"next"}"#;
        let stream = [
            b"content-length: 5\r\nContent-Type: x\r\n\r\nnot{j".to_vec(),
            format!("CONTENT-LENGTH: {}\r\n\r\n", good.len()).into_bytes(),
            good.to_vec(),
        ]
        .concat();
        let mut reader = BufReader::new(&stream[..]);
        assert!(matches!(read_message(&mut reader), Incoming::Skipped));
        match read_message(&mut reader) {
            Incoming::Message(message) => assert_eq!(message["method"], "next"),
            _ => panic!("the stream stayed in step"),
        }
    }

    #[test]
    fn a_header_with_no_end_is_not_read_without_limit() {
        let endless = vec![b'a'; (MAX_HEADER_BYTES as usize) * 2];
        assert!(matches!(
            read_message(&mut BufReader::new(&endless[..])),
            Incoming::End
        ));
    }

    #[test]
    fn requests_from_the_server_get_the_reply_shape_the_protocol_asks_for() {
        let reply = |method: &str| answer_server_request(method, &json!({}), "file:///r");
        assert_eq!(reply("workspace/applyEdit"), json!({ "applied": false }));
        assert_eq!(reply("window/showDocument"), json!({ "success": false }));
        assert_eq!(
            reply("workspace/workspaceFolders"),
            json!([{ "uri": "file:///r", "name": "project" }])
        );
        assert_eq!(reply("client/registerCapability"), Value::Null);
        assert_eq!(reply("window/workDoneProgress/create"), Value::Null);
    }

    #[test]
    fn two_spellings_of_one_file_normalise_to_the_same_key() {
        let same = |a: &str, b: &str| assert_eq!(normalize_uri(a), normalize_uri(b), "{a} / {b}");
        same("file:///a/b%20c/d%40.rs", "file:///a/b c/d@.rs");
        same(
            "file:///proj/app/%28group%29/%5Bid%5D/page.tsx",
            "file:///proj/app/(group)/[id]/page.tsx",
        );
        same("file:///C%3A/Work/X.rs", "file:///c:/Work/X.rs");
        same("file:///a/%C3%BCber.rs", "file:///a/\u{fc}ber.rs");
        same("file:///a/dir/", "file:///a/dir");
        assert_eq!(
            normalize_uri("file:///a/%zz/%4"),
            "/a/%zz/%4",
            "a bad escape is kept literally"
        );
        assert_ne!(
            normalize_uri("file:///a/b.rs"),
            normalize_uri("file:///a/B.rs")
        );
    }

    #[test]
    fn a_diagnostic_code_or_name_cannot_forge_lines_or_run_long() {
        let found = [Diagnostic {
            line: 1,
            column: 1,
            severity: Severity::Error,
            code: Some(plain(
                &format!(
                    "E1\n  fake.rs:9:9 error injected\u{1b}[2J{}",
                    "x".repeat(500)
                ),
                MAX_CODE_CHARS,
            )),
            message: "m".to_owned(),
        }];
        let text = render("srv\nname", "a\u{202e}.rs", &found, Severity::Error);
        assert_eq!(text.lines().count(), 2, "{text}");
        assert!(
            !text.contains('\u{1b}') && !text.contains('\u{202e}'),
            "{text:?}"
        );
        assert!(text.lines().nth(1).unwrap().len() < 200, "{text}");
    }

    #[test]
    fn a_server_is_given_the_toolchain_environment_and_no_credentials() {
        for kept in [
            "PATH",
            "HOME",
            "LC_ALL",
            "CARGO_HOME",
            "RUSTUP_TOOLCHAIN",
            "XDG_CONFIG_HOME",
            "NODE_OPTIONS",
            "GOPATH",
        ] {
            assert!(env_kept(kept), "{kept}");
        }
        for dropped in [
            "ANTHROPIC_API_KEY",
            "GITHUB_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "CARGO_REGISTRY_TOKEN",
            "DB_PASSWORD",
            "RAPIDLM_PROVIDER_KEY",
            "SSH_AUTH_SOCK",
            "OPENAI_API_KEY",
        ] {
            assert!(!env_kept(dropped), "{dropped}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_file_on_the_path_that_is_not_executable_is_not_a_server() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Dir::new("exec-bit");
        let program = dir.0.join("fake-server");
        std::fs::write(&program, "#!/bin/sh\n").expect("write");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        assert_eq!(find_program(&program.to_string_lossy()), None);
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert_eq!(find_program(&program.to_string_lossy()), Some(program));
    }

    // --- regressions for the review ---------------------------------------

    #[cfg(unix)]
    #[test]
    fn a_server_that_dies_before_it_answers_fails_the_start_at_once() {
        let dir = Dir::new("die-early");
        let Some(pool) = pool(&dir, "die_early", 5000) else {
            return;
        };
        let started = Instant::now();
        let note = check(&pool, "a.txt", b"x\n").expect("a note");
        assert!(note.contains("not available"), "{note}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "waited {:?} for a server that was already gone",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_slow_start_in_one_language_does_not_hold_up_another() {
        let dir = Dir::new("two-langs");
        let Some(python) = python() else { return };
        let script = dir.0.join("fake_lsp.py");
        let (config, _) = parse_with(
            &json!({ "lsp": { "defaults": false, "wait_ms": 3000, "servers": {
                "slow": { "command": ["env", "FAKE_LSP_MODE=slow_start", python, script.to_string_lossy()], "extensions": ["slow"] },
                "fast": { "command": ["env", "FAKE_LSP_MODE=normal", python, script.to_string_lossy()], "extensions": ["txt"] },
            } } }),
            &|_| true,
        );
        let pool = Arc::new(LspPool::new(config.expect("config"), &dir.0));
        let slow = {
            let pool = Arc::clone(&pool);
            std::thread::spawn(move || check(&pool, "a.slow", b"ERROR\n"))
        };
        std::thread::sleep(Duration::from_millis(300));
        let started = Instant::now();
        let fast = check(&pool, "a.txt", b"ERROR\n").expect("an answer");
        assert!(fast.contains("1 problem(s)"), "{fast}");
        assert!(
            started.elapsed() < Duration::from_millis(1500),
            "the txt server waited behind the slow one: {:?}",
            started.elapsed()
        );
        let _ = slow.join();
    }

    #[cfg(unix)]
    #[test]
    fn tools_rooted_elsewhere_get_nothing_and_start_nothing() {
        let dir = Dir::new("other-root");
        let Some(pool) = pool(&dir, "normal", 2000) else {
            return;
        };
        let elsewhere = std::env::temp_dir().join("a-subagent-worktree");
        assert_eq!(pool.diagnostics(&elsewhere, "a.txt", b"ERROR\n"), None);
        assert!(
            pool.slots.lock().unwrap().is_empty(),
            "no server was started for a tree it does not watch"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_server_that_spells_the_uri_differently_is_still_matched() {
        let dir = Dir::new("reencode");
        let Some(pool) = pool(&dir, "reencode", 5000) else {
            return;
        };
        let text = check(&pool, "app/(group)/[id] x@y.txt", b"ERROR\n").expect("an answer");
        assert!(
            text.contains("1 problem(s)"),
            "the published URI did not match: {text}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_server_that_stops_reading_cannot_hang_a_write_or_the_pool() {
        let dir = Dir::new("deaf");
        let Some(pool) = pool(&dir, "deaf", 500) else {
            return;
        };
        let big = "x".repeat(8 * 1024 * 1024);
        let started = Instant::now();
        assert_eq!(check(&pool, "a.txt", big.as_bytes()), None);
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "the write blocked on a pipe nobody reads: {:?}",
            started.elapsed()
        );
        let dropping = Instant::now();
        drop(pool);
        assert!(
            dropping.elapsed() < Duration::from_secs(4),
            "ending the session hung: {:?}",
            dropping.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_closed_after_each_check_so_no_stale_buffer_outlives_it() {
        let dir = Dir::new("closes");
        let Some(python) = python() else { return };
        let log = dir.0.join("methods.log");
        let script = dir.0.join("fake_lsp.py");
        let (config, _) = parse_with(
            &json!({ "lsp": { "defaults": false, "wait_ms": 5000, "servers": { "fake": {
                "command": ["env", "FAKE_LSP_MODE=normal", format!("FAKE_LSP_LOG={}", log.display()),
                            python, script.to_string_lossy()],
                "extensions": ["txt"] } } } }),
            &|_| true,
        );
        let pool = LspPool::new(config.expect("config"), &dir.0);
        assert!(check(&pool, "a.txt", b"ERROR\n").is_some());
        assert!(check(&pool, "a.txt", b"fine\n").is_some());
        drop(pool);
        let methods: Vec<String> = std::fs::read_to_string(&log)
            .expect("log")
            .lines()
            .map(str::to_owned)
            .collect();
        let doc: Vec<&str> = methods
            .iter()
            .map(String::as_str)
            .filter(|m| m.starts_with("textDocument/"))
            .collect();
        assert_eq!(
            doc,
            [
                "textDocument/didOpen",
                "textDocument/didSave",
                "textDocument/didClose",
                "textDocument/didOpen",
                "textDocument/didSave",
                "textDocument/didClose",
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_server_that_exits_mid_session_is_reported_and_not_restarted_on_every_write() {
        let dir = Dir::new("exits-later");
        let Some(pool) = pool(&dir, "exit_after_first", 1500) else {
            return;
        };
        assert!(
            check(&pool, "a.txt", b"ERROR\n")
                .expect("first answer")
                .contains("problem(s)")
        );
        // It has gone now. The next write says so, once, and later ones leave it be.
        std::thread::sleep(Duration::from_millis(300));
        let second = check(&pool, "a.txt", b"ERROR\n");
        let third = check(&pool, "a.txt", b"ERROR\n");
        let fourth = check(&pool, "a.txt", b"ERROR\n");
        let said: Vec<_> = [second, third, fourth]
            .into_iter()
            .flatten()
            .filter(|n| n.contains("exited"))
            .collect();
        assert_eq!(said.len(), 1, "{said:?}");
    }
}
