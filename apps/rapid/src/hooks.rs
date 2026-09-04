//! Project-settings hooks (Claude hook parity, headless scope).
//!
//! `pre_tool_use` commands run before a matched tool executes: a non-zero
//! exit DENIES the call with the hook's stderr as the model-visible detail.
//! `post_tool_use` commands run after execution and their output is recorded
//! on the result. Hook commands are project settings (trusted-project gate),
//! receive the tool call as JSON on stdin, and are bounded by a timeout — a
//! hung hook denies rather than hangs the turn.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Default per-hook wall-clock budget (Claude: 5 s default).
pub const HOOK_TIMEOUT: Duration = Duration::from_secs(5);
/// Maximum hooks per stage.
pub const MAX_HOOKS_PER_STAGE: usize = 8;
/// Hard byte cap on captured hook stderr.
pub const MAX_HOOK_STDERR_BYTES: usize = 2048;

/// Hook commands parsed from project settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HooksConfig {
    pub pre_tool_use: Vec<String>,
    pub post_tool_use: Vec<String>,
    /// Fires once per `rapid exec` run, after hooks/settings load, before the
    /// turn starts. Notification-style: output is logged, never gates.
    pub session_start: Vec<String>,
    /// Fires once per `rapid exec` run, on every exit path (success, typed
    /// failure, or early return) — see `SessionEndHookGuard` in
    /// `interactive.rs`. Notification-style, same as `session_start`.
    pub session_end: Vec<String>,
    /// Fires when `task_spawn` is about to run a child agent.
    /// Notification-style, same as `session_start`.
    pub subagent_start: Vec<String>,
    /// Fires when a `task_spawn` child finishes (success or failure).
    /// Notification-style, same as `session_start`.
    pub subagent_stop: Vec<String>,
}

impl HooksConfig {
    /// Parse the `hooks` object from a settings document value.
    pub fn parse(value: &serde_json::Value) -> Option<HooksConfig> {
        let object = value.get("hooks")?.as_object()?;
        let mut config = HooksConfig::default();
        for (key, target) in [
            ("pre_tool_use", &mut config.pre_tool_use),
            ("post_tool_use", &mut config.post_tool_use),
            ("session_start", &mut config.session_start),
            ("session_end", &mut config.session_end),
            ("subagent_start", &mut config.subagent_start),
            ("subagent_stop", &mut config.subagent_stop),
        ] {
            let Some(entries) = object.get(key).and_then(serde_json::Value::as_array) else {
                continue;
            };
            for entry in entries {
                let Some(command) = entry.as_str() else {
                    continue;
                };
                if command.is_empty() || command.len() > 512 {
                    continue;
                }
                if target.len() >= MAX_HOOKS_PER_STAGE {
                    break;
                }
                target.push(command.to_owned());
            }
        }
        Some(config)
    }

    pub fn is_empty(&self) -> bool {
        self.pre_tool_use.is_empty()
            && self.post_tool_use.is_empty()
            && self.session_start.is_empty()
            && self.session_end.is_empty()
            && self.subagent_start.is_empty()
            && self.subagent_stop.is_empty()
    }
}

/// Outcome of a pre-tool-use hook stage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreHookOutcome {
    /// Every hook allowed the call.
    Allowed,
    /// A hook denied the call; carries its bounded stderr.
    Denied { reason: String },
}

/// Run one hook command with `input_json` on stdin; returns
/// `(exit_ok, stderr)`. A missing/failed spawn counts as failed with a
/// static reason (never a panic).
fn run_hook_once(command: &str, input_json: &str, timeout: Duration) -> (bool, String) {
    // Output goes to a temp file rather than our pipes: a hook that
    // backgrounds its own children (`sleep 30 &`) would otherwise hold the
    // pipe write-end open past the kill, blocking EOF collection.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let output_path = std::env::temp_dir().join(format!(
        "rapidlm-hook-out-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    let output_file = match std::fs::File::create(&output_path) {
        Ok(file) => file,
        Err(err) => return (false, format!("hook output file failed: {err}")),
    };
    #[cfg(unix)]
    let spawn = {
        let mut command_builder = Command::new("sh");
        command_builder.arg("-c").arg(command).env_clear();
        for key in ["PATH", "HOME", "LANG", "TMPDIR"] {
            if let Ok(value) = std::env::var(key) {
                let _ = command_builder.env(key, value);
            }
        }
        command_builder
            .stdin(Stdio::piped())
            .stdout(Stdio::from(output_file.try_clone().expect("clone")))
            .stderr(Stdio::from(output_file))
            .spawn()
    };
    #[cfg(not(unix))]
    let spawn = {
        let mut command_builder = Command::new("cmd");
        command_builder.arg("/C").arg(command).env_clear();
        for key in ["PATH", "USERPROFILE", "TEMP", "TMP", "SystemRoot"] {
            if let Ok(value) = std::env::var(key) {
                let _ = command_builder.env(key, value);
            }
        }
        command_builder.spawn()
    };
    let mut child = match spawn {
        Ok(child) => child,
        Err(err) => {
            let _ = std::fs::remove_file(&output_path);
            return (false, format!("hook spawn failed: {err}"));
        }
    };
    // Write stdin on its own thread rather than blocking here: `write_all`
    // on a piped child stdin has no timeout of its own, so a payload larger
    // than the OS pipe buffer combined with a hook that never reads stdin
    // (the overwhelmingly common case — most hooks only care about argv/the
    // command's own output) would otherwise block synchronously, before the
    // timeout clock below even starts, hanging the turn despite this
    // module's own "bounded by a timeout" contract (see the module doc).
    // The child's stdin handle is moved into the thread, so dropping it
    // there still gives the hook EOF; if the child is killed on timeout
    // while the write is still blocked, closing its stdin fd unblocks the
    // writer thread with a broken-pipe error, which is ignored below.
    if let Some(mut stdin) = child.stdin.take() {
        let input_json = input_json.to_owned();
        std::thread::spawn(move || {
            let _ = stdin.write_all(input_json.as_bytes());
            let _ = stdin.flush();
        });
    }
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = crate::exec_tools::read_capped_bytes(&output_path, MAX_HOOK_STDERR_BYTES);
                let _ = std::fs::remove_file(&output_path);
                let text = truncate(&output, MAX_HOOK_STDERR_BYTES);
                return (status.success(), text);
            }
            Ok(None) => {
                if started.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(err) => {
                let _ = std::fs::remove_file(&output_path);
                return (false, format!("hook wait failed: {err}"));
            }
        }
    }
    let output = crate::exec_tools::read_capped_bytes(&output_path, MAX_HOOK_STDERR_BYTES);
    let _ = std::fs::remove_file(&output_path);
    let text = truncate(&output, MAX_HOOK_STDERR_BYTES);
    if !text.is_empty() {
        (false, text)
    } else {
        (false, "hook timed out".to_owned())
    }
}

/// Run every `pre_tool_use` hook for one tool call. Input JSON:
/// `{"tool": name, "arguments": <raw arguments value>}`. The first denial
/// wins.
pub fn run_pre_tool_hooks(
    hooks: &[String],
    tool: &str,
    arguments: &str,
    timeout: Duration,
) -> PreHookOutcome {
    let input = format!(r#"{{"tool":"{tool}","arguments":{arguments}}}"#);
    for command in hooks {
        // Bounded stderr collection: the hook runs to completion (or timeout)
        // with stderr redirected to a temp buffer via the shell wrapper.
        let (ok, stderr) = run_hook_once(command, &input, timeout);
        if !ok {
            let reason = if stderr.is_empty() {
                // Re-run capturing stderr through the wrapper is already done;
                // a silent failure still denies with a static reason.
                "hook exited non-zero".to_owned()
            } else {
                stderr
            };
            return PreHookOutcome::Denied {
                reason: truncate(reason.as_bytes(), MAX_HOOK_STDERR_BYTES),
            };
        }
    }
    PreHookOutcome::Allowed
}

/// Run every `post_tool_use` hook; returns their combined output (bounded).
pub fn run_post_tool_hooks(
    hooks: &[String],
    tool: &str,
    summary: &str,
    timeout: Duration,
) -> String {
    // Proper JSON serialization: a hand-rolled `"{escaped}"` format only
    // escaped `\` and `"`, so a summary containing a raw newline (e.g.
    // `execute_shell`'s `"exit {code}\n{output}"`) produced invalid JSON.
    // `serde_json` escapes every control character RFC 8259 requires.
    let input = serde_json::json!({ "tool": tool, "summary": summary }).to_string();
    let mut combined = String::new();
    for command in hooks {
        let (_, output) = run_hook_once(command, &input, timeout);
        if !output.is_empty() && combined.len() < MAX_HOOK_STDERR_BYTES {
            if !combined.is_empty() {
                combined.push_str("; ");
            }
            combined.push_str(&output);
        }
    }
    let _ = MAX_HOOK_STDERR_BYTES;
    combined
}

/// Run every hook for a notification-style event (`session_start`,
/// `subagent_start`, `subagent_stop`, ...): fire-and-collect, never gates —
/// unlike `pre_tool_use`, a non-zero exit here is not a denial. `payload` is
/// serialized as the hook's stdin JSON (only `event` is added to it here, so
/// callers pass their own event-specific fields already, matching
/// `run_post_tool_hooks`'s `{"tool":...,"summary":...}` shape rather than a
/// generic wrapper).
pub fn run_notify_hooks(
    hooks: &[String],
    event: &str,
    payload: serde_json::Value,
    timeout: Duration,
) -> String {
    let mut input = match payload {
        serde_json::Value::Object(map) => map,
        other => {
            let mut map = serde_json::Map::new();
            map.insert("payload".to_owned(), other);
            map
        }
    };
    input.insert("event".to_owned(), serde_json::Value::String(event.to_owned()));
    let input = serde_json::Value::Object(input).to_string();
    let mut combined = String::new();
    for command in hooks {
        let (_, output) = run_hook_once(command, &input, timeout);
        if !output.is_empty() && combined.len() < MAX_HOOK_STDERR_BYTES {
            if !combined.is_empty() {
                combined.push_str("; ");
            }
            combined.push_str(&output);
        }
    }
    combined
}

fn truncate(bytes: &[u8], cap: usize) -> String {
    let mut end = bytes.len().min(cap);
    while end > 0 && std::str::from_utf8(&bytes[..end]).is_err() {
        end -= 1;
    }
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(dir: &std::path::Path, name: &str, body: &str) -> String {
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        format!("sh {}", path.display())
    }

    #[test]
    fn parse_settings_hooks_object() {
        let value: serde_json::Value = serde_json::from_str(
            r#"{"hooks": {"pre_tool_use": ["a", "b"], "post_tool_use": ["c"], "junk": 1}}"#,
        )
        .expect("json");
        let hooks = HooksConfig::parse(&value).expect("hooks");
        assert_eq!(hooks.pre_tool_use, vec!["a".to_owned(), "b".to_owned()]);
        assert_eq!(hooks.post_tool_use, vec!["c".to_owned()]);
        // No hooks key → None; empty object → empty config.
        assert!(HooksConfig::parse(&serde_json::json!({})).is_none());
        let empty = HooksConfig::parse(&serde_json::json!({"hooks": {}})).expect("hooks");
        assert!(empty.is_empty());
    }

    #[test]
    fn zero_exit_allows_and_nonzero_denies() {
        let dir = std::env::temp_dir().join(format!("hook-ok-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let ok = script(&dir, "ok.sh", "exit 0");
        assert_eq!(
            run_pre_tool_hooks(&[ok], "repo_read", "{}", HOOK_TIMEOUT),
            PreHookOutcome::Allowed
        );

        let deny = script(
            &dir,
            "deny.sh",
            r#"if [ "$1" = "check" ]; then true; fi
read line
case "$line" in
  *shell_exec*) echo "shell is not allowed here" >&2; exit 2 ;;
esac
exit 0"#,
        );
        match run_pre_tool_hooks(&[deny], "shell_exec", r#"{"argv":["ls"]}"#, HOOK_TIMEOUT) {
            PreHookOutcome::Denied { reason } => {
                assert!(reason.contains("shell is not allowed"), "{reason}");
            }
            PreHookOutcome::Allowed => panic!("denial expected"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hook_stdin_receives_the_tool_call_json() {
        let dir = std::env::temp_dir().join(format!("hook-stdin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let capture = dir.join("captured.json");
        let hook = script(
            &dir,
            "capture.sh",
            &format!("cat > {}", capture.display()),
        );
        let input = r#"{"tool":"repo_read","arguments":{"path":"a.txt"}}"#;
        assert_eq!(
            run_pre_tool_hooks(&[hook], "repo_read", r#"{"path":"a.txt"}"#, HOOK_TIMEOUT),
            PreHookOutcome::Allowed
        );
        let captured = std::fs::read_to_string(capture).expect("captured");
        assert_eq!(captured, input);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hung_hook_is_denied_after_the_bounded_timeout() {
        let dir = std::env::temp_dir().join(format!("hook-hang-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let hang = script(&dir, "hang.sh", "sleep 30");
        let started = std::time::Instant::now();
        match run_pre_tool_hooks(
            &[hang],
            "shell_exec",
            "{}",
            Duration::from_millis(250),
        ) {
            PreHookOutcome::Denied { reason } => {
                assert!(reason.contains("timed out"), "{reason}");
            }
            PreHookOutcome::Allowed => panic!("hung hook must deny"),
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the hook budget must bound the wait"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hook_subprocess_does_not_inherit_ambient_environment() {
        let dir = std::env::temp_dir().join(format!("hook-env-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let capture = dir.join("env.txt");
        let hook = format!("env > {}", capture.display());
        assert_eq!(
            run_pre_tool_hooks(&[hook], "repo_read", "{}", HOOK_TIMEOUT),
            PreHookOutcome::Allowed
        );
        let captured = std::fs::read_to_string(&capture).expect("captured env");
        // The four we deliberately forward, plus what `sh` itself injects
        // even under `env -i` (confirmed via `env -i PATH=/usr/bin:/bin sh
        // -c 'env'`: PWD, SHLVL, and `_`) — not something our own spawn
        // code passes through. Anything outside this set had to come from
        // the real process environment, which env_clear() must stop.
        const ALLOWED: &[&str] = &["PATH", "HOME", "LANG", "TMPDIR", "PWD", "SHLVL", "_"];
        for line in captured.lines() {
            let Some((key, _)) = line.split_once('=') else {
                continue;
            };
            assert!(
                ALLOWED.contains(&key),
                "hook subprocess must not inherit ambient env var {key:?}: {captured}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_large_stdin_payload_does_not_block_past_the_hook_timeout() {
        // `hang.sh` never touches stdin. If `run_hook_once` writes stdin
        // synchronously before starting its timeout clock, a payload larger
        // than the OS pipe buffer blocks the write until the child exits on
        // its own (here, after its full 30s sleep) rather than being bounded
        // by `timeout` — directly contradicting this module's doc comment
        // ("bounded by a timeout — a hung hook denies rather than hangs the
        // turn"). A small payload wouldn't reach the pipe buffer's capacity
        // and would pass either way, so this needs a payload comfortably
        // past any realistic OS pipe buffer size.
        let dir = std::env::temp_dir().join(format!("hook-bigstdin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let hang = script(&dir, "hang.sh", "sleep 30");
        let big_argument = format!("\"{}\"", "x".repeat(4_000_000));
        let started = std::time::Instant::now();
        match run_pre_tool_hooks(&[hang], "shell_exec", &big_argument, Duration::from_millis(250)) {
            PreHookOutcome::Denied { reason } => {
                assert!(reason.contains("timed out"), "{reason}");
            }
            PreHookOutcome::Allowed => panic!("hung hook must deny"),
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a large stdin payload must not block past the hook timeout, took {:?}",
            started.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn post_hooks_record_their_output() {
        let dir = std::env::temp_dir().join(format!("hook-post-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let note = script(&dir, "note.sh", "echo post-ran-ok");
        let output = run_post_tool_hooks(&[note], "repo_read", "the summary", HOOK_TIMEOUT);
        assert!(output.contains("post-ran-ok"), "{output}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hook_output_survives_a_trailing_invalid_utf8_byte() {
        // A `read_to_string`-based collection fails validity for the *whole*
        // captured file the instant any byte anywhere is invalid UTF-8,
        // discarding an otherwise perfectly good output rather than just the
        // offending tail. `read_capped_bytes` + the manual boundary-trimming
        // `truncate` above must instead preserve the valid prefix.
        let dir = std::env::temp_dir().join(format!("hook-badutf8-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let hook = script(&dir, "badutf8.sh", "printf 'ok-output'; printf '\\377'");
        let output = run_post_tool_hooks(&[hook], "repo_read", "the summary", HOOK_TIMEOUT);
        assert!(
            output.contains("ok-output"),
            "expected the valid prefix to survive a trailing invalid UTF-8 byte, got {output:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn notify_hooks_never_gate_and_receive_the_event_name() {
        let dir = std::env::temp_dir().join(format!("hook-notify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let capture = dir.join("captured.json");
        // Exits non-zero: notification hooks must not turn that into a
        // denial the way pre_tool_use does.
        let hook = script(
            &dir,
            "notify.sh",
            &format!("cat > {}\nexit 7", capture.display()),
        );
        let output = run_notify_hooks(
            &[hook],
            "subagent_start",
            serde_json::json!({"agent_type": "explore"}),
            HOOK_TIMEOUT,
        );
        // Non-gating: the caller gets logged output, not a Denied variant —
        // there is no such variant for this call at all, which is the point.
        let _ = output;
        let captured = std::fs::read_to_string(capture).expect("captured");
        let value: serde_json::Value = serde_json::from_str(&captured).expect("json");
        assert_eq!(value["event"], "subagent_start");
        assert_eq!(value["agent_type"], "explore");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_settings_reads_the_new_notification_hook_keys() {
        let value: serde_json::Value = serde_json::from_str(
            r#"{"hooks": {"session_start": ["a"], "session_end": ["d"], "subagent_start": ["b"], "subagent_stop": ["c"]}}"#,
        )
        .expect("json");
        let hooks = HooksConfig::parse(&value).expect("hooks");
        assert_eq!(hooks.session_start, vec!["a".to_owned()]);
        assert_eq!(hooks.session_end, vec!["d".to_owned()]);
        assert_eq!(hooks.subagent_start, vec!["b".to_owned()]);
        assert_eq!(hooks.subagent_stop, vec!["c".to_owned()]);
        assert!(!hooks.is_empty());
    }
}
