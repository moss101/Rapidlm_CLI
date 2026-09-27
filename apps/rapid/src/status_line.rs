//! `[ui.status_line]` (SEAM-07 AC-03): what the TUI's status row shows —
//! the built-in items, a command's output, or nothing.
//!
//! A command is run under the sandbox with a timeout, handed a
//! [`protocol::status::StatusPayload`] as JSON on stdin and the session and
//! turn ids as `RAPIDLM_SESSION_ID` / `RAPIDLM_TURN_ID`. Up to
//! [`MAX_STATUS_LINES`] lines of its output are kept, each cut at
//! [`MAX_STATUS_LINE_CHARS`] characters; a run that fails keeps the last
//! output. A command a project's `.rapidlm/config.toml` names runs only in a
//! trusted project; the user's own config is the user's to trust.

use std::path::{Path, PathBuf};
use std::time::Duration;

use protocol::status::StatusPayload;

/// Most lines of a command's output the status row keeps.
pub const MAX_STATUS_LINES: usize = 5;
/// Each kept line is cut at this many characters.
pub const MAX_STATUS_LINE_CHARS: usize = 1024;
/// `refresh_interval` bounds, seconds.
pub const MIN_REFRESH_SECS: u64 = 1;
pub const MAX_REFRESH_SECS: u64 = 86_400;
/// How often a command reruns when `refresh_interval` is not set.
pub const DEFAULT_REFRESH_SECS: u64 = 60;
/// How long one run may take before it is stopped.
pub const STATUS_TIMEOUT: Duration = Duration::from_secs(5);
/// How much output one run may produce.
const STATUS_OUTPUT_LIMIT: u64 = 16 * 1024;

/// `[ui.status_line] type`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StatusLineKind {
    Builtin,
    Command(String),
    Disabled,
}

/// Where a `[ui.status_line]` came from: a project's command needs trust.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StatusOrigin {
    User,
    Project,
}

/// A parsed `[ui.status_line]`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusLineConfig {
    pub kind: StatusLineKind,
    /// The built-in items to show; empty shows them all.
    pub items: Vec<String>,
    pub refresh: Duration,
    pub origin: StatusOrigin,
}

impl StatusLineConfig {
    /// No `[ui.status_line]` anywhere: the built-in row, all items.
    pub fn builtin() -> Self {
        Self {
            kind: StatusLineKind::Builtin,
            items: Vec::new(),
            refresh: Duration::from_secs(DEFAULT_REFRESH_SECS),
            origin: StatusOrigin::User,
        }
    }
}

/// `[ui.status_line]` in `document`, if it has one. `file` names the
/// document in errors.
pub fn parse(
    document: &str,
    file: &str,
    origin: StatusOrigin,
) -> Result<Option<StatusLineConfig>, String> {
    let root: toml::Value = toml::from_str(document).map_err(|err| format!("{file}: {err}"))?;
    let Some(table) = root.get("ui").and_then(|ui| ui.get("status_line")) else {
        return Ok(None);
    };
    let table = table
        .as_table()
        .ok_or_else(|| format!("{file}: ui.status_line must be a table"))?;
    let key = |name: &str| format!("{file}: ui.status_line.{name}");
    for name in table.keys() {
        if !matches!(
            name.as_str(),
            "type" | "items" | "command" | "refresh_interval"
        ) {
            return Err(format!("{} is not a status-line key", key(name)));
        }
    }
    let command = match table.get("command") {
        None => None,
        Some(value) => Some(
            value
                .as_str()
                .filter(|command| !command.trim().is_empty())
                .filter(|command| !command.chars().any(char::is_control))
                .ok_or_else(|| {
                    format!(
                        "{} must be a non-empty, one-line string (put a longer script in a file)",
                        key("command")
                    )
                })?
                .to_owned(),
        ),
    };
    let kind = match table.get("type").map(toml::Value::as_str) {
        None | Some(Some("builtin")) => StatusLineKind::Builtin,
        Some(Some("command")) => StatusLineKind::Command(
            command
                .clone()
                .ok_or_else(|| format!("{} is `command`, but no command is named", key("type")))?,
        ),
        Some(Some("disabled")) => StatusLineKind::Disabled,
        _ => {
            return Err(format!(
                "{} must be \"builtin\", \"command\" or \"disabled\"",
                key("type")
            ));
        }
    };
    if command.is_some() && !matches!(kind, StatusLineKind::Command(_)) {
        return Err(format!(
            "{} is set, but type is not \"command\"",
            key("command")
        ));
    }
    let mut items = Vec::new();
    if let Some(value) = table.get("items") {
        let list = value
            .as_array()
            .ok_or_else(|| format!("{} must be a list of item names", key("items")))?;
        for item in list {
            let name = item
                .as_str()
                .filter(|name| tui::STATUS_ITEM_NAMES.contains(name))
                .ok_or_else(|| {
                    format!(
                        "{} names an unknown item (one of: {})",
                        key("items"),
                        tui::STATUS_ITEM_NAMES.join(", ")
                    )
                })?;
            items.push(name.to_owned());
        }
    }
    let refresh = match table.get("refresh_interval") {
        None => DEFAULT_REFRESH_SECS,
        Some(value) => value
            .as_integer()
            .and_then(|secs| u64::try_from(secs).ok())
            .filter(|secs| (MIN_REFRESH_SECS..=MAX_REFRESH_SECS).contains(secs))
            .ok_or_else(|| {
                format!(
                    "{} is seconds, {MIN_REFRESH_SECS} to {MAX_REFRESH_SECS}",
                    key("refresh_interval")
                )
            })?,
    };
    Ok(Some(StatusLineConfig {
        kind,
        items,
        refresh: Duration::from_secs(refresh),
        origin,
    }))
}

/// The status line in effect: the user's `[ui.status_line]` when it has
/// one (the user's config outranks the workspace's), else the project's,
/// else the built-in row. A file that cannot be read is no config; one
/// that does not parse is an error.
pub fn load(project_root: &Path, user_config: Option<&Path>) -> Result<StatusLineConfig, String> {
    let read = |path: &Path| std::fs::read_to_string(path).ok();
    if let Some(path) = user_config
        && let Some(text) = read(path)
        && let Some(config) = parse(&text, &path.display().to_string(), StatusOrigin::User)?
    {
        return Ok(config);
    }
    let project = project_root.join(".rapidlm").join("config.toml");
    if let Some(text) = read(&project)
        && let Some(config) = parse(&text, ".rapidlm/config.toml", StatusOrigin::Project)?
    {
        return Ok(config);
    }
    Ok(StatusLineConfig::builtin())
}

/// What one run of a status command came to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StatusRun {
    /// Its lines.
    Lines(Vec<String>),
    /// It did not produce lines: the reason. The last lines are kept.
    Failed(String),
    /// It was not run: a project's command in an untrusted project.
    Refused(String),
    /// No sandbox tier to run it in on this host.
    Unavailable(String),
}

/// The lines a status row keeps from `output`: the first
/// [`MAX_STATUS_LINES`], each cut at [`MAX_STATUS_LINE_CHARS`] characters.
pub fn keep_lines(output: &str) -> Vec<String> {
    output
        .lines()
        .take(MAX_STATUS_LINES)
        .map(|line| line.chars().take(MAX_STATUS_LINE_CHARS).collect())
        .collect()
}

/// The script that runs `$1` with the payload (`$2`) on stdin and the ids
/// (`$3`, `$4`) exported. One line, with nothing spliced into it: the
/// command, the payload and the ids are positional arguments, so the outer
/// shell neither expands nor parses them. The payload is one line of JSON,
/// control characters escaped, as the sandbox's argv rules require.
///
/// The sandbox refuses an empty argument, so an id the host does not have
/// is passed as `-` and stripped here: the variable is then empty.
const SCRIPT: &str = "RAPIDLM_SESSION_ID=\"${3#-}\"; RAPIDLM_TURN_ID=\"${4#-}\"; \
export RAPIDLM_SESSION_ID RAPIDLM_TURN_ID; printf '%s\\n' \"$2\" | sh -c \"$1\"";

/// Run `command` for `payload` in `root`, under the sandbox.
pub fn run_command(
    root: &Path,
    command: &str,
    origin: StatusOrigin,
    trusted: bool,
    payload: &StatusPayload,
) -> StatusRun {
    if origin == StatusOrigin::Project && !trusted {
        return StatusRun::Refused(
            "the project's status-line command runs only in a trusted project; run `rapid trust grant`"
                .to_owned(),
        );
    }
    let json = match serde_json::to_string(payload) {
        Ok(json) => json,
        Err(err) => return StatusRun::Failed(err.to_string()),
    };
    let argv = vec![
        "sh".to_owned(),
        "-c".to_owned(),
        SCRIPT.to_owned(),
        "rapid-status".to_owned(),
        command.to_owned(),
        json,
        payload.session_id.clone().unwrap_or_else(|| "-".to_owned()),
        payload.turn_id.clone().unwrap_or_else(|| "-".to_owned()),
    ];
    match crate::sandbox_exec::run_sandboxed(root, &argv, STATUS_TIMEOUT, STATUS_OUTPUT_LIMIT) {
        Ok(outcome) if outcome.timed_out => {
            StatusRun::Failed(format!("timed out after {}s", STATUS_TIMEOUT.as_secs()))
        }
        Ok(outcome) if outcome.exit_code == Some(0) => {
            StatusRun::Lines(keep_lines(&String::from_utf8_lossy(&outcome.output)))
        }
        Ok(outcome) => StatusRun::Failed(match outcome.exit_code {
            Some(code) => format!("exited {code}"),
            None => "was stopped".to_owned(),
        }),
        // Only a missing or unhealthy tier is "unavailable"; anything else
        // the sandbox refused is this run's failure, said as it is.
        Err(crate::sandbox_exec::SandboxRunError::Sandbox(
            err @ (sandbox::SandboxError::TierUnavailable
            | sandbox::SandboxError::HealthFailed
            | sandbox::SandboxError::ResourceLimit),
        )) => StatusRun::Unavailable(format!("no sandbox to run it in: {err}")),
        Err(crate::sandbox_exec::SandboxRunError::Sandbox(err)) => {
            StatusRun::Failed(format!("the sandbox refused the run: {err}"))
        }
        Err(err) => StatusRun::Failed(err.to_string()),
    }
}

/// The user's config file, as the status line reads it.
pub fn user_config_path(user_home: Option<&Path>) -> Option<PathBuf> {
    user_home.map(|home| home.join("config.toml"))
}

/// The session facts a payload is built from that the TUI state does not
/// hold.
#[derive(Clone, Debug, Default)]
pub struct SessionFacts {
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub cwd: PathBuf,
    pub repo: Option<PathBuf>,
    pub worktree: Option<PathBuf>,
}

/// A payload from the TUI's projection and the session's facts.
pub fn payload(
    ui: &tui::state::AppState,
    facts: &SessionFacts,
    trigger: protocol::status::StatusTrigger,
) -> StatusPayload {
    use protocol::status::*;
    let context = ui.context_usage();
    let usage = ui.session_usage();
    let basis = if usage.steps == 0 {
        "none"
    } else if usage.cost_unknown_steps > 0 {
        "unknown"
    } else if usage.tokens_estimated_steps > 0 {
        "estimated"
    } else {
        "reported"
    };
    let goal = ui
        .goals()
        .values()
        .find(|goal| goal.lifecycle() == tui::state::GoalLifecycle::Active)
        .or_else(|| ui.goals().values().next_back())
        .map(|goal| StatusGoal {
            id: goal.id().to_string(),
            state: format!("{:?}", goal.lifecycle()).to_lowercase(),
        });
    StatusPayload {
        schema: STATUS_PAYLOAD_SCHEMA.to_owned(),
        version: STATUS_PAYLOAD_VERSION,
        session_id: facts.session_id.clone(),
        turn_id: facts.turn_id.clone(),
        model: facts.model.clone(),
        effort: facts.effort.clone(),
        context: StatusContext {
            used_tokens: context.map(|(used, _)| used),
            limit_tokens: context.map(|(_, limit)| limit),
            used_percent: context.and_then(|(used, limit)| {
                (limit > 0).then(|| u8::try_from((used.min(limit) * 100) / limit).unwrap_or(100))
            }),
        },
        cost: StatusCost {
            usd_micros: if usage.steps == 0 {
                Some(0)
            } else {
                usage.cost_usd_micros()
            },
            basis: basis.to_owned(),
        },
        goal,
        worktree: facts
            .worktree
            .as_ref()
            .map(|path| path.display().to_string()),
        workspace: StatusWorkspace {
            cwd: facts.cwd.display().to_string(),
            repo: facts.repo.as_ref().map(|path| path.display().to_string()),
        },
        trigger,
    }
}

/// Drives a `command` status line from the TUI loop: one run at a time on
/// its own thread (a run can take up to [`STATUS_TIMEOUT`], and the loop
/// must not wait on it), rerun when `refresh_interval` elapses or what the
/// payload carries changes.
#[derive(Default)]
pub struct StatusRunner {
    config: Option<StatusLineConfig>,
    root: PathBuf,
    trusted: bool,
    in_flight: Option<std::sync::mpsc::Receiver<StatusRun>>,
    last_run: Option<std::time::Instant>,
    /// The payload last run for, `trigger` aside: a change reruns.
    last_state: Option<String>,
    lines: Vec<String>,
}

impl StatusRunner {
    /// Set the status line; the mode the row starts in.
    pub fn configure(
        &mut self,
        config: StatusLineConfig,
        root: &Path,
        trusted: bool,
    ) -> tui::state::StatusMode {
        let mode = match &config.kind {
            StatusLineKind::Builtin => tui::state::StatusMode::Builtin {
                items: config.items.clone(),
            },
            StatusLineKind::Disabled => tui::state::StatusMode::Disabled,
            StatusLineKind::Command(_) => tui::state::StatusMode::Command { lines: Vec::new() },
        };
        *self = Self {
            config: Some(config),
            root: root.to_path_buf(),
            trusted,
            ..Self::default()
        };
        mode
    }

    /// One loop tick: start a run when one is due, take one that finished.
    /// The mode to show when it changed.
    pub fn tick(
        &mut self,
        build: impl Fn(protocol::status::StatusTrigger) -> StatusPayload,
    ) -> Option<tui::state::StatusMode> {
        let Some(config) = &self.config else {
            return None;
        };
        let StatusLineKind::Command(command) = &config.kind else {
            return None;
        };
        if let Some(receiver) = &self.in_flight {
            let run = match receiver.try_recv() {
                Ok(run) => run,
                Err(std::sync::mpsc::TryRecvError::Empty) => return None,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    StatusRun::Failed("the status command's runner stopped".to_owned())
                }
            };
            self.in_flight = None;
            return match run {
                StatusRun::Lines(lines) => {
                    self.lines = lines;
                    Some(tui::state::StatusMode::Command {
                        lines: self.lines.clone(),
                    })
                }
                // The last output stands; with none yet, the row says why.
                StatusRun::Failed(reason)
                | StatusRun::Refused(reason)
                | StatusRun::Unavailable(reason) => {
                    self.lines
                        .is_empty()
                        .then(|| tui::state::StatusMode::Command {
                            lines: vec![format!("status line: {reason}")],
                        })
                }
            };
        }
        let state_payload = build(protocol::status::StatusTrigger::State);
        let mut fingerprint = serde_json::to_value(&state_payload).unwrap_or_default();
        fingerprint["trigger"] = serde_json::Value::Null;
        let fingerprint = fingerprint.to_string();
        let changed = self.last_state.as_ref() != Some(&fingerprint);
        let due = self
            .last_run
            .is_none_or(|last| last.elapsed() >= config.refresh);
        if !changed && !due {
            return None;
        }
        let payload = if changed {
            state_payload
        } else {
            build(protocol::status::StatusTrigger::RefreshInterval)
        };
        self.last_state = Some(fingerprint);
        self.last_run = Some(std::time::Instant::now());
        let (sender, receiver) = std::sync::mpsc::channel();
        let (root, command, origin, trusted) = (
            self.root.clone(),
            command.clone(),
            config.origin,
            self.trusted,
        );
        std::thread::spawn(move || {
            let _ = sender.send(run_command(&root, &command, origin, trusted, &payload));
        });
        self.in_flight = Some(receiver);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::status::*;

    fn payload() -> StatusPayload {
        StatusPayload {
            schema: STATUS_PAYLOAD_SCHEMA.to_owned(),
            version: STATUS_PAYLOAD_VERSION,
            session_id: Some("019c0000-0000-7000-8000-000000000010".to_owned()),
            turn_id: Some("019c0000-0000-7000-8000-000000000011".to_owned()),
            model: Some("local".to_owned()),
            effort: None,
            context: StatusContext {
                used_tokens: Some(10),
                limit_tokens: Some(100),
                used_percent: Some(10),
            },
            cost: StatusCost {
                usd_micros: None,
                basis: "none".to_owned(),
            },
            goal: None,
            worktree: None,
            workspace: StatusWorkspace {
                cwd: "/w".to_owned(),
                repo: None,
            },
            trigger: StatusTrigger::State,
        }
    }

    fn root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "rapidlm-status-line-{tag}-{}",
            protocol::TraceId::new()
        ));
        std::fs::create_dir_all(&root).expect("root");
        protocol::host_path::canonicalize(&root).expect("canonical")
    }

    #[test]
    fn the_config_is_parsed_closed_and_bounded() {
        let parsed = parse(
            "[ui.status_line]\ntype = \"command\"\ncommand = \"echo hi\"\nrefresh_interval = 5\n",
            "f",
            StatusOrigin::User,
        )
        .expect("parse")
        .expect("present");
        assert_eq!(parsed.kind, StatusLineKind::Command("echo hi".to_owned()));
        assert_eq!(parsed.refresh, Duration::from_secs(5));
        assert_eq!(parse("a = 1\n", "f", StatusOrigin::User), Ok(None));
        let items = parse(
            "[ui.status_line]\nitems = [\"model\", \"cost\"]\n",
            "f",
            StatusOrigin::User,
        )
        .expect("parse")
        .expect("present");
        assert_eq!(items.kind, StatusLineKind::Builtin);
        assert_eq!(items.items, ["model", "cost"]);
        for bad in [
            "[ui.status_line]\ntype = \"command\"\n",
            "[ui.status_line]\ncommand = \"x\"\n",
            "[ui.status_line]\ntype = \"fancy\"\n",
            "[ui.status_line]\nrefresh_interval = 0\n",
            "[ui.status_line]\nrefresh_interval = 86401\n",
            "[ui.status_line]\nitems = [\"weather\"]\n",
            "[ui.status_line]\ncolour = 1\n",
            "[ui.status_line]\ntype = \"command\"\ncommand = \" \"\n",
            "[ui.status_line]\ntype = \"command\"\ncommand = \"a\\nb\"\n",
        ] {
            assert!(parse(bad, "f", StatusOrigin::User).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_users_config_outranks_the_projects() {
        let root = root("load");
        std::fs::create_dir_all(root.join(".rapidlm")).expect("marker");
        std::fs::write(
            root.join(".rapidlm/config.toml"),
            "[ui.status_line]\ntype = \"disabled\"\n",
        )
        .expect("project");
        let loaded = load(&root, None).expect("load");
        assert_eq!(
            (loaded.kind, loaded.origin),
            (StatusLineKind::Disabled, StatusOrigin::Project)
        );
        let user = root.join("user.toml");
        std::fs::write(&user, "[ui.status_line]\nitems = [\"cost\"]\n").expect("user");
        let loaded = load(&root, Some(&user)).expect("load");
        assert_eq!(loaded.origin, StatusOrigin::User);
        assert_eq!(loaded.items, ["cost"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn output_is_kept_to_five_lines_of_1024_characters() {
        let long = "x".repeat(2000);
        let output = format!("{long}\n2\n3\n4\n5\n6\n");
        let kept = keep_lines(&output);
        assert_eq!(kept.len(), MAX_STATUS_LINES);
        assert_eq!(kept[0].chars().count(), MAX_STATUS_LINE_CHARS);
        assert_eq!(kept[4], "5");
    }

    #[test]
    fn a_projects_command_does_not_run_in_an_untrusted_project() {
        let root = root("untrusted");
        let run = run_command(&root, "touch ran", StatusOrigin::Project, false, &payload());
        assert!(matches!(run, StatusRun::Refused(_)), "{run:?}");
        assert!(!root.join("ran").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_command_reads_the_payload_on_stdin_and_the_ids_from_its_environment() {
        let root = root("run");
        // The command reads its payload and names the ids it was given; a
        // `$` in the payload is not expanded.
        let mut given = payload();
        given.model = Some("$(touch expanded)".to_owned());
        let run = run_command(
            &root,
            "cat; echo \"$RAPIDLM_SESSION_ID\"; echo \"$RAPIDLM_TURN_ID\"",
            StatusOrigin::User,
            false,
            &given,
        );
        let StatusRun::Lines(lines) = run else {
            panic!("{run:?}");
        };
        let read: StatusPayload = serde_json::from_str(&lines[0]).expect("payload json");
        read.validate().expect("valid");
        assert_eq!(read, given);
        assert_eq!(lines[1], "019c0000-0000-7000-8000-000000000010");
        assert_eq!(lines[2], "019c0000-0000-7000-8000-000000000011");
        assert!(!root.join("expanded").exists(), "the payload was expanded");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn the_runner_reruns_on_a_changed_state_or_an_elapsed_interval_and_keeps_the_last_lines() {
        let root = root("runner");
        let mut runner = StatusRunner::default();
        let config = StatusLineConfig {
            kind: StatusLineKind::Command(
                "read line; test -e fail && exit 3; printf 'run %s\\n' \"$(ls runs | wc -l | tr -d ' ')\"; touch runs/$(date +%s%N)".to_owned(),
            ),
            items: Vec::new(),
            refresh: Duration::from_secs(3600),
            origin: StatusOrigin::User,
        };
        std::fs::create_dir_all(root.join("runs")).expect("runs");
        assert_eq!(
            runner.configure(config, &root, true),
            tui::state::StatusMode::Command { lines: Vec::new() }
        );
        let wait =
            |runner: &mut StatusRunner,
             build: &dyn Fn(protocol::status::StatusTrigger) -> StatusPayload| {
                let deadline = std::time::Instant::now() + Duration::from_secs(20);
                loop {
                    if let Some(mode) = runner.tick(build) {
                        return mode;
                    }
                    assert!(std::time::Instant::now() < deadline, "no run finished");
                    std::thread::sleep(Duration::from_millis(20));
                }
            };
        let first = |trigger| StatusPayload {
            trigger,
            ..payload()
        };
        let lines = |mode| match mode {
            tui::state::StatusMode::Command { lines } => lines,
            other => panic!("{other:?}"),
        };
        assert_eq!(lines(wait(&mut runner, &first)), ["run 0"]);
        // Same state, interval not elapsed: no run.
        assert_eq!(runner.tick(first), None);
        std::thread::sleep(Duration::from_millis(50));
        assert!(runner.in_flight.is_none());
        // A changed state reruns.
        let second = |trigger| StatusPayload {
            model: Some("other".to_owned()),
            trigger,
            ..payload()
        };
        assert_eq!(lines(wait(&mut runner, &second)), ["run 1"]);
        // A failed run keeps the last lines.
        std::fs::write(root.join("fail"), b"").expect("fail");
        runner.last_run = Some(std::time::Instant::now() - Duration::from_secs(7200));
        assert_eq!(runner.tick(second), None, "the run starts");
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while runner.in_flight.is_some() {
            assert_eq!(runner.tick(second), None, "a failure changes nothing shown");
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(runner.lines, ["run 1"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    // Off unix there is no sandbox tier to run a status command in: the run
    // is typed unavailable, never a silent success or an unsandboxed run.
    #[cfg(not(unix))]
    #[test]
    fn a_host_without_a_sandbox_tier_reports_the_command_unavailable() {
        let root = root("unavailable");
        let run = run_command(&root, "echo hi", StatusOrigin::User, true, &payload());
        assert!(matches!(run, StatusRun::Unavailable(_)), "{run:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_command_that_outruns_its_timeout_is_stopped_and_fails() {
        let root = root("timeout");
        let started = std::time::Instant::now();
        let run = run_command(&root, "sleep 30", StatusOrigin::User, true, &payload());
        assert!(
            matches!(&run, StatusRun::Failed(reason) if reason.contains("timed out")),
            "{run:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(20));
        let _ = std::fs::remove_dir_all(&root);
    }
}
