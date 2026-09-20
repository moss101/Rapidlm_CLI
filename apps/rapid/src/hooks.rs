//! Project-settings hooks (headless scope).
//!
//! `pre_tool_use` commands run before a matched tool executes. A hook that
//! prints nothing structured keeps the v1 contract: a non-zero exit DENIES
//! the call with the hook's stderr as the model-visible detail. A hook that
//! prints a [`protocol::HookResult`] (`rapidlm.hook_result` v2) on stdout
//! decides in words — `allow`, `deny`, `ask`, `defer` — and each such
//! decision is recorded as a [`HookDecisionRecord`] for the ledger (ADR
//! 0022). `post_tool_use` commands run after execution and their output is
//! recorded on the result. Hook commands are project settings
//! (trusted-project gate), receive the tool call as JSON on stdin, and are
//! bounded by a timeout — a hung hook denies rather than hangs the turn.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use protocol::{HookDecision, HookResult, HookResultError, MAX_HOOK_RESULT_BYTES};

/// Default per-hook wall-clock budget.
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
    /// Fires before a `/compact` asks the model for its summary, with how
    /// many turns are about to be folded. Notification-style: it observes,
    /// it never gates.
    pub pre_compact: Vec<String>,
    /// Fires after a `/compact` recorded its summary, with the turns folded
    /// and the summary's size. Notification-style, same as `pre_compact`.
    pub post_compact: Vec<String>,
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
            ("pre_compact", &mut config.pre_compact),
            ("post_compact", &mut config.post_compact),
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

    /// Every stage, in declaration order — the one list a merge or a cap
    /// walks, so a stage added to the struct cannot be left out of either
    /// (which is how `pre_compact`/`post_compact` parsed and then vanished
    /// in the project-settings merge).
    pub fn stages_mut(&mut self) -> [&mut Vec<String>; 8] {
        [
            &mut self.pre_tool_use,
            &mut self.post_tool_use,
            &mut self.session_start,
            &mut self.session_end,
            &mut self.subagent_start,
            &mut self.subagent_stop,
            &mut self.pre_compact,
            &mut self.post_compact,
        ]
    }

    /// Append every stage of `other` to this config.
    pub fn extend(&mut self, mut other: HooksConfig) {
        for (stage, more) in self.stages_mut().into_iter().zip(other.stages_mut()) {
            stage.append(more);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.pre_tool_use.is_empty()
            && self.post_tool_use.is_empty()
            && self.session_start.is_empty()
            && self.session_end.is_empty()
            && self.subagent_start.is_empty()
            && self.subagent_stop.is_empty()
            && self.pre_compact.is_empty()
            && self.post_compact.is_empty()
    }
}

/// Outcome of a pre-tool-use hook stage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreHookOutcome {
    /// Every hook allowed (or deferred on) the call.
    Allowed,
    /// A hook denied the call; carries its bounded reason (a v2 `reason`,
    /// or the v1 stderr).
    Denied { reason: String },
    /// A v2 hook asked for a human decision and no hook denied. `hook`
    /// names the asking hook (`pre_tool_use[<index>]`).
    Ask { hook: String, reason: String },
}

/// One v2 decision a hook made, in the shape the ledger records
/// (`hook.decided`, `rapidlm.hook.decision/v1`). v1 hooks — exit code only,
/// nothing structured on stdout — produce no record, so a project whose
/// hooks never print a result has a ledger identical to before.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookDecisionRecord {
    /// `pre_tool_use[<index>]` — the stage and the hook's position in it.
    pub hook: String,
    /// The stage that ran (`pre_tool_use`).
    pub event: &'static str,
    /// First twelve hex digits of the SHA-256 of the hook command line, so
    /// a reader can tell which command a position referred to after the
    /// settings file changed.
    pub command_digest: String,
    pub decision: HookDecision,
    pub reason: Option<String>,
    /// The result carried a grant-shaped key (ignored; recorded).
    pub grant_attempted: bool,
}

/// A pre-tool stage's outcome together with every v2 decision made on the
/// way to it, for the caller to record, and the input rewrite that survived
/// the stage (ADR 0022 §5): the last rewriting hook wins, a denial discards
/// every rewrite, and an `ask` keeps the rewrite so the human is shown the
/// call that would actually run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreHookReport {
    pub outcome: PreHookOutcome,
    pub decisions: Vec<HookDecisionRecord>,
    pub rewrite: Option<HookRewrite>,
}

/// An `updated_input` a hook returned: the hook that returned it and the
/// object that replaces the call's arguments. Validation against the tool's
/// own argument parser is the caller's (the stage does not know the tools).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookRewrite {
    /// `pre_tool_use[<index>]`.
    pub hook: String,
    pub command_digest: String,
    pub input: serde_json::Map<String, serde_json::Value>,
}

/// What one hook run produced: whether it exited zero, its stdout (the
/// result channel, bounded by [`MAX_HOOK_RESULT_BYTES`]) and its stderr
/// (the detail channel, bounded by [`MAX_HOOK_STDERR_BYTES`]).
#[derive(Clone, Debug, PartialEq, Eq)]
struct HookRun {
    ok: bool,
    /// The hook was killed at the timeout. Whatever it printed is at best
    /// partial: never a decision, still a diagnostic.
    timed_out: bool,
    stdout: Vec<u8>,
    stderr: String,
}

impl HookRun {
    fn failed(reason: String) -> Self {
        Self {
            ok: false,
            timed_out: false,
            stdout: Vec::new(),
            stderr: reason,
        }
    }

    /// The v1 detail: stderr, or stdout when the hook wrote its reason there
    /// (both streams used to land in one capture, so a hook that spoke on
    /// stdout keeps being heard).
    fn detail(&self) -> String {
        if !self.stderr.is_empty() {
            return self.stderr.clone();
        }
        truncate(&self.stdout, MAX_HOOK_STDERR_BYTES)
    }

    /// Both streams as one bounded text, stdout first — the shape the
    /// notification and post-tool stages have always recorded.
    fn combined(&self) -> String {
        let mut text = truncate(&self.stdout, MAX_HOOK_STDERR_BYTES);
        if !self.stderr.is_empty() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&self.stderr);
        }
        truncate(text.as_bytes(), MAX_HOOK_STDERR_BYTES)
    }
}

/// `pre_tool_use[<index>]`.
fn hook_name(stage: &str, index: usize) -> String {
    format!("{stage}[{index}]")
}

/// First twelve hex digits of the SHA-256 of the hook command line.
fn command_digest(command: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(command.as_bytes());
    let mut hex = String::with_capacity(12);
    for byte in &digest[..6] {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// The shell a hook line runs under, with the environment cleared down to
/// what a shell needs to find programs and a temp directory. Which shell
/// and which variables is the only platform difference in running a hook.
fn hook_shell(command: &str) -> Command {
    #[cfg(unix)]
    let (shell, flag, keep): (&str, &str, &[&str]) =
        ("sh", "-c", &["PATH", "HOME", "LANG", "TMPDIR"]);
    #[cfg(not(unix))]
    let (shell, flag, keep): (&str, &str, &[&str]) = ("cmd", "/C", &["PATH"]);
    let mut builder = Command::new(shell);
    builder.arg(flag).arg(command).env_clear();
    for key in keep {
        if let Ok(value) = std::env::var(key) {
            let _ = builder.env(key, value);
        }
    }
    // What the OS itself needs to start a child (`SystemRoot`, `COMSPEC`,
    // `TEMP`, … on Windows; nothing on Unix), stated once in `host_env`.
    for (key, value) in protocol::host_env::platform_base_env() {
        let _ = builder.env(key, value);
    }
    builder
}

/// Run one hook command with `input_json` on stdin. A missing/failed spawn
/// counts as failed with a static reason (never a panic).
fn run_hook_once(command: &str, input_json: &str, timeout: Duration) -> HookRun {
    // Output goes to temp files rather than our pipes: a hook that
    // backgrounds its own children (`sleep 30 &`) would otherwise hold the
    // pipe write-end open past the kill, blocking EOF collection. Two files,
    // because stdout is the result channel and stderr the detail channel
    // (a v2 result printed beside a warning must still parse).
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let stdout_path =
        std::env::temp_dir().join(format!("rapidlm-hook-out-{}-{seq}", std::process::id()));
    let stderr_path =
        std::env::temp_dir().join(format!("rapidlm-hook-err-{}-{seq}", std::process::id()));
    let cleanup = || {
        let _ = std::fs::remove_file(&stdout_path);
        let _ = std::fs::remove_file(&stderr_path);
    };
    let stdout_file = match std::fs::File::create(&stdout_path) {
        Ok(file) => file,
        Err(err) => return HookRun::failed(format!("hook output file failed: {err}")),
    };
    let stderr_file = match std::fs::File::create(&stderr_path) {
        Ok(file) => file,
        Err(err) => {
            cleanup();
            return HookRun::failed(format!("hook output file failed: {err}"));
        }
    };
    // The stdio wiring is the contract — input JSON on stdin, everything the
    // hook prints into the files — and is the same on every platform. Only
    // the shell differs. (The Windows arm used to spawn bare: no stdin, so
    // the hook never received its payload, and no redirection, so its
    // output went to the TUI's own terminal and the file read back empty.)
    let spawn = hook_shell(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .spawn();
    let mut child = match spawn {
        Ok(child) => child,
        Err(err) => {
            cleanup();
            return HookRun::failed(format!("hook spawn failed: {err}"));
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
    let collect = |ok: bool| {
        let stdout = crate::exec_tools::read_capped_bytes(&stdout_path, MAX_HOOK_RESULT_BYTES + 1);
        let stderr = crate::exec_tools::read_capped_bytes(&stderr_path, MAX_HOOK_STDERR_BYTES);
        cleanup();
        HookRun {
            ok,
            timed_out: false,
            stdout,
            stderr: truncate(&stderr, MAX_HOOK_STDERR_BYTES),
        }
    };
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return collect(status.success()),
            Ok(None) => {
                if started.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(err) => {
                cleanup();
                return HookRun::failed(format!("hook wait failed: {err}"));
            }
        }
    }
    let mut run = collect(false);
    run.timed_out = true;
    if run.stderr.is_empty() && run.stdout.is_empty() {
        run.stderr = "hook timed out".to_owned();
    }
    run
}

/// Run every `pre_tool_use` hook for one tool call. Input JSON:
/// `{"tool": name, "arguments": <raw arguments value>}`. The first denial
/// wins. Kept for callers that only need the outcome; the stage's decision
/// records are in [`run_pre_tool_stage`].
pub fn run_pre_tool_hooks(
    hooks: &[String],
    tool: &str,
    arguments: &str,
    timeout: Duration,
) -> PreHookOutcome {
    run_pre_tool_stage(hooks, tool, arguments, timeout).outcome
}

/// Run every `pre_tool_use` hook for one tool call and report every v2
/// decision made on the way (ADR 0022 §1–4).
///
/// Per hook, in declaration order:
/// - a non-zero exit, a crash or a timeout **denies** with the hook's stderr
///   (v1 contract, unchanged), whatever its stdout says;
/// - stdout with no structured result is v1: exit zero allows;
/// - a v2 `deny` denies with its `reason`; a v2 result the binary cannot read
///   (unknown decision, wrong schema, newer version) denies naming the hook —
///   a hook speaking an unknown contract cannot be assumed to have allowed;
/// - `allow` and `defer` continue to the next hook (`defer` states no
///   opinion: the normal permission flow decides);
/// - `ask` is remembered and reported only if no later hook denies.
///
/// A denial ends the stage: later hooks do not run (they never did).
pub fn run_pre_tool_stage(
    hooks: &[String],
    tool: &str,
    arguments: &str,
    timeout: Duration,
) -> PreHookReport {
    const STAGE: &str = "pre_tool_use";
    let input = format!(r#"{{"tool":"{tool}","arguments":{arguments}}}"#);
    let mut decisions = Vec::new();
    let mut ask: Option<(String, String)> = None;
    let mut rewrite: Option<HookRewrite> = None;
    for (index, command) in hooks.iter().enumerate() {
        let name = hook_name(STAGE, index);
        let run = run_hook_once(command, &input, timeout);
        if !run.ok {
            // Fail-closed whatever stdout says. A v2 result printed beside
            // the non-zero exit is still a decision the hook made, so it is
            // recorded — and a `deny` lends its reason — but only the exit
            // code decides the outcome, and it says deny. A timed-out hook
            // printed nothing that counts as a decision; its stdout is a
            // diagnostic like its stderr.
            let result = if run.timed_out {
                None
            } else {
                HookResult::from_stdout(&run.stdout).ok().flatten()
            };
            let mut reason = None;
            if let Some(result) = result {
                if result.decision == HookDecision::Deny {
                    reason = result.reason.clone().filter(|r| !r.is_empty());
                } else {
                    reason = Some(format!(
                        "hook exited non-zero (its result said {}; the exit code decides)",
                        result.decision
                    ));
                }
                decisions.push(HookDecisionRecord {
                    hook: name.clone(),
                    event: STAGE,
                    command_digest: command_digest(command),
                    decision: result.decision,
                    reason: result.reason.filter(|r| !r.is_empty()),
                    grant_attempted: result.grant_attempted,
                });
            }
            let reason = reason.unwrap_or_else(|| {
                let detail = run.detail();
                if detail.is_empty() {
                    // A silent failure still denies with a static reason.
                    "hook exited non-zero".to_owned()
                } else if run.timed_out {
                    format!("hook timed out: {detail}")
                } else {
                    detail
                }
            });
            // A denial discards every rewrite: nothing runs.
            return PreHookReport {
                outcome: PreHookOutcome::Denied {
                    reason: truncate(reason.as_bytes(), MAX_HOOK_STDERR_BYTES),
                },
                decisions,
                rewrite: None,
            };
        }
        let result = match HookResult::from_stdout(&run.stdout) {
            Ok(None) => continue,
            Ok(Some(result)) => result,
            Err(err) => {
                return PreHookReport {
                    outcome: PreHookOutcome::Denied {
                        reason: unreadable_result_reason(&name, &err),
                    },
                    decisions,
                    rewrite: None,
                };
            }
        };
        decisions.push(HookDecisionRecord {
            hook: name.clone(),
            event: STAGE,
            command_digest: command_digest(command),
            decision: result.decision,
            reason: result.reason.clone().filter(|r| !r.is_empty()),
            grant_attempted: result.grant_attempted,
        });
        // The last rewriting hook wins (a later hook that rewrites nothing
        // leaves an earlier rewrite standing); a `deny` below discards it.
        if let Some(input) = result.updated_input.clone() {
            rewrite = Some(HookRewrite {
                hook: name.clone(),
                command_digest: command_digest(command),
                input,
            });
        }
        match result.decision {
            HookDecision::Allow | HookDecision::Defer => {}
            HookDecision::Deny => {
                let reason = result
                    .reason
                    .filter(|r| !r.is_empty())
                    .unwrap_or_else(|| format!("denied by {name} hook"));
                return PreHookReport {
                    outcome: PreHookOutcome::Denied { reason },
                    decisions,
                    rewrite: None,
                };
            }
            HookDecision::Ask => {
                if ask.is_none() {
                    let reason = result
                        .reason
                        .filter(|r| !r.is_empty())
                        .unwrap_or_else(|| format!("{name} hook asked for approval"));
                    ask = Some((name, reason));
                }
            }
        }
    }
    let outcome = match ask {
        Some((hook, reason)) => PreHookOutcome::Ask { hook, reason },
        None => PreHookOutcome::Allowed,
    };
    PreHookReport {
        outcome,
        decisions,
        rewrite,
    }
}

/// The denial reason for a hook whose stdout declared itself a result this
/// binary cannot read.
fn unreadable_result_reason(hook: &str, err: &HookResultError) -> String {
    truncate(
        format!("{hook} hook printed an unreadable result ({err}); the call is denied").as_bytes(),
        MAX_HOOK_STDERR_BYTES,
    )
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
        let output = run_hook_once(command, &input, timeout).combined();
        if !output.is_empty() && combined.len() < MAX_HOOK_STDERR_BYTES {
            if !combined.is_empty() {
                combined.push_str("; ");
            }
            combined.push_str(&output);
        }
    }
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
    input.insert(
        "event".to_owned(),
        serde_json::Value::String(event.to_owned()),
    );
    let input = serde_json::Value::Object(input).to_string();
    let mut combined = String::new();
    for command in hooks {
        let output = run_hook_once(command, &input, timeout).combined();
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
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        // The hook line is handed to `sh -c` on Unix and `cmd /C` on
        // Windows. Unquoted: `cmd /C` re-escapes double quotes on their
        // way into `sh` (Windows CI, 2026-09-17: `sh: \C:/…/ok.sh": No such
        // file`), and the CI temp path has no spaces. Forward slashes so
        // `sh` on Windows does not read the backslashes as escapes. A
        // profile with a space in its temp path is a known limitation of
        // these fixtures, not of hooks (a user's hook line is their own).
        format!("sh {}", test_fixtures::slash_path(&path))
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
            other => panic!("denial expected, got {other:?}"),
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
            &format!("cat > {}", test_fixtures::sh_quote(&capture)),
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
        match run_pre_tool_hooks(&[hang], "shell_exec", "{}", Duration::from_millis(250)) {
            PreHookOutcome::Denied { reason } => {
                assert!(reason.contains("timed out"), "{reason}");
            }
            other => panic!("hung hook must deny, got {other:?}"),
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
        // Cargo sets `CARGO_MANIFEST_DIR` (and the rest of `CARGO_*`) in
        // every test binary's environment: a variable this process is
        // guaranteed to have and no hook is ever forwarded. If it reaches
        // the hook, the ambient environment leaked — the same assertion on
        // every platform.
        const CANARY_NAME: &str = "CARGO_MANIFEST_DIR";
        assert!(
            std::env::var_os(CANARY_NAME).is_some(),
            "the test binary itself carries the canary"
        );
        // The redirect lives inside a script `sh` parses on every host;
        // `cmd /C` would parse a bare `env > file` line itself on Windows
        // and does not understand POSIX quoting.
        let hook = script(
            &dir,
            "env.sh",
            &format!(
                "{} > {}",
                test_fixtures::sh_quote(&test_fixtures::tool("env")),
                test_fixtures::sh_quote(&capture)
            ),
        );
        assert_eq!(
            run_pre_tool_hooks(&[hook], "repo_read", "{}", HOOK_TIMEOUT),
            PreHookOutcome::Allowed
        );
        let captured = std::fs::read_to_string(&capture).expect("captured env");
        assert!(
            !captured.lines().any(|line| line.starts_with(CANARY_NAME)),
            "hook subprocess must not inherit the ambient environment: {captured}"
        );
        // On Unix the exact set is known: the four we deliberately forward,
        // plus what `sh` itself injects even under `env -i` (confirmed via
        // `env -i PATH=/usr/bin:/bin sh -c 'env'`: PWD, SHLVL, and `_`) —
        // not something our own spawn code passes through. Anything outside
        // this set had to come from the real process environment, which
        // env_clear() must stop. (`cmd.exe` and the MSYS runtime add their
        // own set on Windows — COMSPEC, PATHEXT, the `=D:` drive cwd
        // pseudo-variables, … — which is why the canary is the assertion
        // that holds everywhere.)
        #[cfg(unix)]
        {
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
        match run_pre_tool_hooks(
            &[hang],
            "shell_exec",
            &big_argument,
            Duration::from_millis(250),
        ) {
            PreHookOutcome::Denied { reason } => {
                assert!(reason.contains("timed out"), "{reason}");
            }
            other => panic!("hung hook must deny, got {other:?}"),
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
            &format!("cat > {}\nexit 7", test_fixtures::sh_quote(&capture)),
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

    /// A hook script whose stdout is exactly `json` (single-quoted for `sh`,
    /// so the braces and double quotes survive) and whose exit code is
    /// `exit_code`.
    fn result_script(dir: &std::path::Path, name: &str, json: &str, exit_code: u8) -> String {
        script(dir, name, &format!("echo '{json}'\nexit {exit_code}"))
    }

    fn temp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("hook-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    #[test]
    fn a_v2_deny_result_denies_with_its_reason_and_is_recorded() {
        let dir = temp("v2-deny");
        let deny = result_script(
            &dir,
            "deny.sh",
            r#"{"schema":"rapidlm.hook_result","version":2,"decision":"deny","reason":"writes to docs/ are reviewed"}"#,
            0,
        );
        let report = run_pre_tool_stage(
            std::slice::from_ref(&deny),
            "workspace_write",
            "{}",
            HOOK_TIMEOUT,
        );
        assert_eq!(
            report.outcome,
            PreHookOutcome::Denied {
                reason: "writes to docs/ are reviewed".to_owned()
            }
        );
        assert_eq!(report.decisions.len(), 1);
        let record = &report.decisions[0];
        assert_eq!(record.hook, "pre_tool_use[0]");
        assert_eq!(record.event, "pre_tool_use");
        assert_eq!(record.decision, HookDecision::Deny);
        assert_eq!(
            record.reason.as_deref(),
            Some("writes to docs/ are reviewed")
        );
        assert_eq!(record.command_digest, command_digest(&deny));
        assert_eq!(record.command_digest.len(), 12);
        assert!(!record.grant_attempted);
        // The thin wrapper agrees with the stage.
        assert_eq!(
            run_pre_tool_hooks(&[deny], "workspace_write", "{}", HOOK_TIMEOUT),
            report.outcome
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn v2_allow_and_defer_continue_to_the_next_hook_and_are_recorded() {
        let dir = temp("v2-allow-defer");
        let allow = result_script(&dir, "allow.sh", r#"{"decision":"allow"}"#, 0);
        let defer = result_script(
            &dir,
            "defer.sh",
            r#"{"decision":"defer","reason":"not my call","Capabilities":["fs.write"]}"#,
            0,
        );
        let report = run_pre_tool_stage(&[allow, defer], "repo_read", "{}", HOOK_TIMEOUT);
        assert_eq!(report.outcome, PreHookOutcome::Allowed);
        let decisions: Vec<(String, HookDecision, bool)> = report
            .decisions
            .iter()
            .map(|d| (d.hook.clone(), d.decision, d.grant_attempted))
            .collect();
        assert_eq!(
            decisions,
            vec![
                ("pre_tool_use[0]".to_owned(), HookDecision::Allow, false),
                ("pre_tool_use[1]".to_owned(), HookDecision::Defer, true),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_v2_ask_is_reported_unless_a_later_hook_denies() {
        let dir = temp("v2-ask");
        let ask = result_script(
            &dir,
            "ask.sh",
            r#"{"decision":"ask","reason":"a human should look at this"}"#,
            0,
        );
        let deny = result_script(&dir, "deny.sh", r#"{"decision":"deny","reason":"no"}"#, 0);
        let alone =
            run_pre_tool_stage(std::slice::from_ref(&ask), "shell_exec", "{}", HOOK_TIMEOUT);
        assert_eq!(
            alone.outcome,
            PreHookOutcome::Ask {
                hook: "pre_tool_use[0]".to_owned(),
                reason: "a human should look at this".to_owned()
            }
        );
        assert_eq!(alone.decisions.len(), 1);
        assert_eq!(alone.decisions[0].decision, HookDecision::Ask);
        // A later denial wins over an earlier ask; both are recorded.
        let denied = run_pre_tool_stage(&[ask, deny], "shell_exec", "{}", HOOK_TIMEOUT);
        assert_eq!(
            denied.outcome,
            PreHookOutcome::Denied {
                reason: "no".to_owned()
            }
        );
        assert_eq!(
            denied
                .decisions
                .iter()
                .map(|d| d.decision)
                .collect::<Vec<_>>(),
            vec![HookDecision::Ask, HookDecision::Deny]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_v2_result_denies_naming_the_hook() {
        let dir = temp("v2-unreadable");
        let unknown = result_script(&dir, "maybe.sh", r#"{"decision":"maybe"}"#, 0);
        let future = result_script(
            &dir,
            "v9.sh",
            r#"{"schema":"rapidlm.hook_result","version":9,"decision":"allow"}"#,
            0,
        );
        for (hook, expected) in [
            (unknown, "is not one of allow, deny, ask, defer"),
            (future, "version 9 is not supported"),
        ] {
            let report = run_pre_tool_stage(&[hook], "repo_read", "{}", HOOK_TIMEOUT);
            match &report.outcome {
                PreHookOutcome::Denied { reason } => {
                    assert!(
                        reason.starts_with("pre_tool_use[0] hook printed an unreadable result"),
                        "{reason}"
                    );
                    assert!(reason.contains(expected), "{reason}");
                }
                other => panic!("an unreadable result must deny, got {other:?}"),
            }
            // Nothing readable was decided, so nothing is recorded.
            assert!(report.decisions.is_empty());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plain_stdout_text_keeps_v1_semantics_and_records_nothing() {
        let dir = temp("v1-text");
        // A v1 hook that chats on stdout and exits zero: allowed, silent.
        let chatty = script(&dir, "chatty.sh", "echo checking the call\nexit 0");
        let report = run_pre_tool_stage(&[chatty], "repo_read", "{}", HOOK_TIMEOUT);
        assert_eq!(report.outcome, PreHookOutcome::Allowed);
        assert!(report.decisions.is_empty());
        // A v1 hook that prints its reason on stdout (not stderr) and exits
        // non-zero: the reason is still the detail, as it was when both
        // streams shared one capture.
        let stdout_deny = script(&dir, "stdout-deny.sh", "echo not here\nexit 3");
        let report = run_pre_tool_stage(&[stdout_deny], "repo_read", "{}", HOOK_TIMEOUT);
        assert_eq!(
            report.outcome,
            PreHookOutcome::Denied {
                reason: "not here\n".to_owned()
            }
        );
        assert!(report.decisions.is_empty());
        // stderr wins over stdout as the v1 detail when both are written.
        let both = script(&dir, "both.sh", "echo progress\necho refused >&2\nexit 1");
        let report = run_pre_tool_stage(&[both], "repo_read", "{}", HOOK_TIMEOUT);
        assert_eq!(
            report.outcome,
            PreHookOutcome::Denied {
                reason: "refused\n".to_owned()
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_v2_deny_beside_a_nonzero_exit_lends_its_reason_and_a_v2_allow_does_not_rescue_it() {
        let dir = temp("v2-nonzero");
        let deny = result_script(
            &dir,
            "deny1.sh",
            r#"{"decision":"deny","reason":"typed refusal"}"#,
            1,
        );
        let report = run_pre_tool_stage(&[deny], "repo_read", "{}", HOOK_TIMEOUT);
        assert_eq!(
            report.outcome,
            PreHookOutcome::Denied {
                reason: "typed refusal".to_owned()
            }
        );
        assert_eq!(report.decisions.len(), 1);
        assert_eq!(report.decisions[0].decision, HookDecision::Deny);
        // Exit code still rules: `allow` printed by a failing hook denies
        // (fail-closed) with a reason that says so — not the echoed JSON,
        // which would read as an allow — and the decision is still recorded.
        let allow = result_script(&dir, "allow1.sh", r#"{"decision":"allow"}"#, 1);
        let report = run_pre_tool_stage(&[allow], "repo_read", "{}", HOOK_TIMEOUT);
        assert_eq!(
            report.outcome,
            PreHookOutcome::Denied {
                reason: "hook exited non-zero (its result said allow; the exit code decides)"
                    .to_owned()
            }
        );
        assert_eq!(report.decisions.len(), 1);
        assert_eq!(report.decisions[0].decision, HookDecision::Allow);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_timed_out_hooks_partial_stdout_is_not_a_decision() {
        let dir = temp("v2-timeout");
        // Prints a deny with a distinctive reason, then hangs. If the
        // timed-out run's stdout were parsed, that deny would be recorded
        // and its reason would surface; instead the timeout is the reason
        // and nothing is recorded — what a killed hook printed is a
        // diagnostic, never a decision.
        let hang = script(
            &dir,
            "hang.sh",
            "echo '{\"decision\":\"deny\",\"reason\":\"partial-verdict\"}'\nsleep 30",
        );
        let report = run_pre_tool_stage(&[hang], "repo_read", "{}", Duration::from_millis(250));
        match &report.outcome {
            PreHookOutcome::Denied { reason } => {
                assert!(reason.starts_with("hook timed out"), "{reason}");
                // The stdout diagnostic still reaches the detail.
                assert!(reason.contains("partial-verdict"), "{reason}");
            }
            other => panic!("a hung hook must deny, got {other:?}"),
        }
        assert!(report.decisions.is_empty(), "{:?}", report.decisions);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_malformed_result_denies_and_a_chatty_v1_hook_of_any_size_allows() {
        let dir = temp("v2-malformed");
        // A deny whose reason broke the JSON: denied, naming the hook — not
        // silently read as a v1 hook that allowed.
        let broken = script(
            &dir,
            "broken.sh",
            "echo '{\"decision\":\"deny\",\"reason\":\"a \"quoted\" word\"}'\nexit 0",
        );
        let report = run_pre_tool_stage(&[broken], "repo_read", "{}", HOOK_TIMEOUT);
        match &report.outcome {
            PreHookOutcome::Denied { reason } => {
                assert!(
                    reason.starts_with("pre_tool_use[0] hook printed an unreadable result"),
                    "{reason}"
                );
                assert!(reason.contains("not exactly one JSON object"), "{reason}");
            }
            other => panic!("a malformed result must deny, got {other:?}"),
        }
        // A result followed by a log line is malformed too.
        let trailing = script(
            &dir,
            "trailing.sh",
            "echo '{\"decision\":\"deny\"}'\necho checked 3 files\nexit 0",
        );
        assert!(matches!(
            run_pre_tool_stage(&[trailing], "repo_read", "{}", HOOK_TIMEOUT).outcome,
            PreHookOutcome::Denied { .. }
        ));
        // A v1 hook that prints far more than the result ceiling and exits
        // zero is still allowed: the ceiling is for results, not for text.
        let chatty = script(
            &dir,
            "chatty.sh",
            "i=0\nwhile [ $i -lt 1200 ]; do echo 'lint: file ok ...................................................'; i=$((i+1)); done\nexit 0",
        );
        let report = run_pre_tool_stage(&[chatty], "repo_read", "{}", HOOK_TIMEOUT);
        assert_eq!(report.outcome, PreHookOutcome::Allowed);
        assert!(report.decisions.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn post_hooks_record_both_streams_stdout_first() {
        let dir = temp("post-both");
        let both = script(&dir, "both.sh", "echo out-line\necho err-line >&2");
        let output = run_post_tool_hooks(&[both], "repo_read", "s", HOOK_TIMEOUT);
        let out = output.find("out-line").expect("stdout recorded");
        let err = output.find("err-line").expect("stderr recorded");
        assert!(out < err, "{output}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_last_rewrite_wins_a_deny_discards_them_and_an_ask_keeps_one() {
        let dir = temp("v2-rewrite");
        let first = result_script(
            &dir,
            "first.sh",
            r#"{"decision":"allow","updated_input":{"path":"first.txt","content":"a"}}"#,
            0,
        );
        let second = result_script(
            &dir,
            "second.sh",
            r#"{"decision":"defer","updated_input":{"path":"second.txt","content":"b"}}"#,
            0,
        );
        let plain_allow = result_script(&dir, "plain.sh", r#"{"decision":"allow"}"#, 0);
        let deny = result_script(&dir, "deny.sh", r#"{"decision":"deny","reason":"no"}"#, 0);
        let ask = result_script(
            &dir,
            "ask.sh",
            r#"{"decision":"ask","reason":"look","updated_input":{"path":"asked.txt","content":"c"}}"#,
            0,
        );
        // Two rewriters, then a hook that rewrites nothing: the second
        // rewrite stands (the last *rewriting* hook wins).
        let report = run_pre_tool_stage(
            &[first.clone(), second.clone(), plain_allow],
            "workspace_write",
            r#"{"path":"orig.txt","content":"x"}"#,
            HOOK_TIMEOUT,
        );
        assert_eq!(report.outcome, PreHookOutcome::Allowed);
        let rewrite = report.rewrite.expect("a rewrite survives");
        assert_eq!(rewrite.hook, "pre_tool_use[1]");
        assert_eq!(rewrite.command_digest, command_digest(&second));
        assert_eq!(rewrite.input["path"], "second.txt");
        // A denial discards every rewrite.
        let report = run_pre_tool_stage(
            &[first.clone(), deny],
            "workspace_write",
            "{}",
            HOOK_TIMEOUT,
        );
        assert!(matches!(report.outcome, PreHookOutcome::Denied { .. }));
        assert_eq!(report.rewrite, None);
        // An ask keeps the rewrite: the human is shown what would run.
        let report = run_pre_tool_stage(&[first, ask], "workspace_write", "{}", HOOK_TIMEOUT);
        assert!(matches!(report.outcome, PreHookOutcome::Ask { .. }));
        assert_eq!(report.rewrite.expect("kept").input["path"], "asked.txt");
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
