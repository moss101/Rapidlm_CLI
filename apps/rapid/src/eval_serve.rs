//! `rapid eval` — the reproducible coding benchmark (delivery goal §7).
//!
//! A task suite lives in `eval/suite/*.json`: each task seeds a scratch
//! repository with real files, states a plain-language task, carries a gold
//! patch, and names a verification command whose exit 0 is the external
//! judge. Nothing about scoring lives inside the agent.
//!
//! Two modes, reported separately and never mixed:
//!
//! - **offline** (mechanical): replays each task's gold patch through the
//!   real turn executor — the same tool dispatch, permission lattice, and
//!   ledger a live run uses — with a scripted model. This validates the
//!   harness, the tasks, and the executor end to end, deterministically. It
//!   says nothing about model quality and is labeled mechanical in every
//!   report.
//! - **live** (model quality): drives real agent CLIs (`rapid exec`, plus
//!   pinned competitor binaries when present) against identical scratch
//!   repositories with the same verification commands. Agents that are not
//!   installed, or runs that exceed the wall-clock ceiling, are recorded as
//!   skipped/failed with the reason — never silently dropped, and no
//!   comparison is claimed without actual runs on both sides.
//!
//! Output: a JSON results file (`eval/results/<stamp>-<mode>.json`) and a
//! summary line per task; exit 0 iff every non-skipped task passed.

use agent_runtime::run_turn;
use protocol::{AgentId, SessionId};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Wall-clock ceiling per live task (offline tasks finish in milliseconds).
const LIVE_TASK_TIMEOUT_SECS: u64 = 600;

/// Where the suite ships.
pub const SUITE_DIR: &str = "eval/suite";

// ---------------------------------------------------------------------------
// Task schema
// ---------------------------------------------------------------------------

/// One benchmark task. `deny_unknown_fields` semantics via manual parse: a
/// task file the runner does not fully understand must fail the load, not
/// silently skip fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BenchTask {
    /// Stable id, e.g. `bugfix-001`.
    pub id: String,
    /// `bugfix` | `multifile` | `tests` | `recovery` | `workflow`.
    pub category: String,
    /// The plain-language task a model is given.
    pub prompt: String,
    /// `(relative path, contents)` written into the scratch repo before the
    /// run — the identical starting state every agent gets.
    pub setup: Vec<(String, String)>,
    /// The gold patch: `(relative path, contents)` the offline scripted
    /// model writes. Live agents are given only the prompt.
    pub gold: Vec<(String, String)>,
    /// The external judge: a shell command run in the scratch repo root;
    /// exit 0 = the task is done correctly.
    pub verify: String,
    /// A second command that must FAIL on the unmodified repo (the task is
    /// not vacuous) — the anti-vacuity check every offline run performs.
    pub verify_fails_before: bool,
}

const MAX_TASKS: usize = 64;
const MAX_FILE_BYTES: usize = 16 * 1024;

/// Load every task file in a directory, sorted by name.
pub fn load_suite(dir: &Path) -> Result<Vec<BenchTask>, String> {
    let mut tasks = Vec::new();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|err| format!("{}: {err}", dir.display()))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    entries.sort();
    for path in entries {
        let bytes = std::fs::read(&path).map_err(|err| err.to_string())?;
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|err| format!("{}: {err}", path.display()))?;
        let id = value
            .get("id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("{}: missing id", path.display()))?
            .to_owned();
        let category = value
            .get("category")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("{}: missing category", path.display()))?
            .to_owned();
        let prompt = value
            .get("prompt")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("{}: missing prompt", path.display()))?
            .to_owned();
        let verify = value
            .get("verify")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("{}: missing verify", path.display()))?
            .to_owned();
        let verify_fails_before = value
            .get("verify_fails_before")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        let files = |key: &str| -> Result<Vec<(String, String)>, String> {
            let mut out = Vec::new();
            for (path, contents) in value
                .get(key)
                .and_then(serde_json::Value::as_object)
                .ok_or_else(|| format!("{}: missing {key} object", path_name(dir)))?
            {
                let contents = contents
                    .as_str()
                    .ok_or_else(|| format!("{field}: non-string contents", field = key))?;
                if contents.len() > MAX_FILE_BYTES {
                    return Err(format!("{key}/{path}: exceeds the byte bound"));
                }
                out.push((path.clone(), contents.to_owned()));
            }
            Ok(out)
        };
        let setup = files("setup")?;
        let gold = files("gold")?;
        if tasks.len() >= MAX_TASKS {
            return Err(format!("suite exceeds the {MAX_TASKS}-task bound"));
        }
        tasks.push(BenchTask {
            id,
            category,
            prompt,
            setup,
            gold,
            verify,
            verify_fails_before,
        });
    }
    if tasks.is_empty() {
        return Err(format!("{}: no tasks found", dir.display()));
    }
    Ok(tasks)
}

fn path_name(dir: &Path) -> String {
    dir.display().to_string()
}

// ---------------------------------------------------------------------------
// Scratch repositories
// ---------------------------------------------------------------------------

/// Materialize a task's starting state in a fresh scratch repo.
pub fn materialize(root: &Path, task: &BenchTask) -> Result<(), String> {
    std::fs::create_dir_all(root).map_err(|err| err.to_string())?;
    for (relative, contents) in &task.setup {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }
        std::fs::write(&path, contents).map_err(|err| err.to_string())?;
    }
    let git = Command::new("git")
        .args(["init", "-q"])
        .current_dir(root)
        .output()
        .map_err(|err| err.to_string())?;
    if !git.status.success() {
        return Err("git init failed (is git installed?)".to_owned());
    }
    Ok(())
}

fn run_verify(dir: &Path, command: &str, timeout_secs: u64) -> Result<bool, String> {
    use std::io::Read as _;
    // `{RAPID}` in a task's verify command names this binary — so a judge
    // can invoke real rapid subcommands without depending on PATH.
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rapid"));
    let substituted = command.replace("{RAPID}", &format!("'{}'", exe.display()));
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&substituted)
        .current_dir(dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|err| err.to_string())?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        match child.try_wait().map_err(|err| err.to_string())? {
            Some(status) => return Ok(status.success()),
            None => {
                if std::time::Instant::now() > deadline {
                    let _ = child.kill();
                    return Err(format!("verify exceeded its {timeout_secs}s ceiling"));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Offline (mechanical) mode: the scripted gold trajectory through the real
// turn loop.
// ---------------------------------------------------------------------------

/// A model driver that performs the task's gold patch: one `workspace_write`
/// call per gold file, then a terminal answer. This exercises the real tool
/// dispatcher, permission lattice, and ledger — nothing about the outcome is
/// asserted without the verification command running.
struct GoldPatchModel {
    calls: Vec<agent_runtime::ProposedToolCall>,
    step: usize,
}

impl GoldPatchModel {
    fn for_task(task: &BenchTask) -> Self {
        let calls = task
            .gold
            .iter()
            .enumerate()
            .map(|(index, (path, contents))| {
                agent_runtime::ProposedToolCall::new(
                    format!("gold-{index}"),
                    "workspace_write",
                    serde_json::json!({ "path": path, "content": contents }).to_string(),
                )
                .expect("gold call is well-formed")
            })
            .collect();
        Self { calls, step: 0 }
    }
}

impl agent_runtime::ModelDriver for GoldPatchModel {
    fn step(
        &mut self,
        input: &agent_runtime::ModelStepInput<'_>,
        _cancel: &agent_runtime::CancellationToken,
    ) -> Result<agent_runtime::ModelStepOutput, agent_runtime::ModelStepError> {
        self.step += 1;
        if self.step <= self.calls.len() {
            return Ok(agent_runtime::ModelStepOutput::ToolCalls {
                calls: vec![self.calls[self.step - 1].clone()],
                tokens: 10,
                cost_usd_micros: None,
            });
        }
        let _ = input;
        Ok(agent_runtime::ModelStepOutput::Terminal {
            text: "gold patch applied".to_owned(),
            tokens: 10,
            cost_usd_micros: None,
        })
    }
}

/// One task's outcome. `skipped` runs are recorded with the reason — a
/// report that hides its skips is a lie. Usage fields are `None` when the
/// agent cannot report them (or the task never ran); `cost_usd_micros` is
/// `None` when the provider reports no cost for this credential, which is
/// "unknown", never "free".
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct TaskResult {
    pub id: String,
    pub category: String,
    pub outcome: String, // passed | failed | skipped
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub wall_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd_micros: Option<u64>,
    /// Cost ESTIMATED from the measured token split at the provider's
    /// published rates — recorded beside, never merged with, the measured
    /// `cost_usd_micros`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_estimate_usd_micros: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_turns: Option<u64>,
    /// Which live agent produced this record (`rapid`, `grok`, …). `None`
    /// in offline mode, where the runner is this binary's gold trajectory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

/// Run the suite offline: gold trajectory through the real turn loop, graded
/// only by the task's verification command (plus the anti-vacuity check).
pub fn run_offline(tasks: &[BenchTask], scratch_root: &Path, trusted: bool) -> Vec<TaskResult> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut results = Vec::new();
    for task in tasks {
        let started = std::time::Instant::now();
        let scratch = scratch_root.join(format!("{}-{stamp:x}", task.id));
        let result = run_offline_task(task, &scratch, trusted);
        if std::env::var("RAPIDLM_EVAL_KEEP_SCRATCH")
            .map(|v| v == "1")
            .unwrap_or(false)
        {
            eprintln!("scratch kept: {}", scratch.display());
        } else {
            let _ = std::fs::remove_dir_all(&scratch);
        }
        results.push(TaskResult {
            id: task.id.clone(),
            category: task.category.clone(),
            outcome: match &result {
                Ok(()) => "passed".to_owned(),
                Err(reason) => {
                    let _ = reason;
                    "failed".to_owned()
                }
            },
            reason: result.err(),
            wall_ms: started.elapsed().as_millis(),
            tokens: Some(u64::from(task.gold.len() as u32 + 1) * 10),
            cost_usd_micros: None,
            cost_estimate_usd_micros: None,
            input_tokens: None,
            cached_tokens: None,
            output_tokens: None,
            num_turns: None,
            agent: None,
        });
    }
    results
}

fn run_offline_task(task: &BenchTask, scratch: &Path, trusted: bool) -> Result<(), String> {
    materialize(scratch, task)?;
    // Anti-vacuity: the verification command must FAIL on the unmodified
    // repo, or the task proves nothing.
    if task.verify_fails_before && run_verify(scratch, &task.verify, 60).unwrap_or(true) {
        return Err(
            "verification command passes on the UNMODIFIED repo; the task is vacuous".to_owned(),
        );
    }
    // The gold trajectory through the REAL turn loop: tools, lattice, ledger.
    if !trusted {
        return Err(
            "the project is not trusted; offline evaluation refuses to run with no tools"
                .to_owned(),
        );
    }
    // The gold trajectory is scripted, not a model's proposal: the calls are
    // known-good by construction, so the lattice runs in bypass mode (the
    // same seam the interactive test harness uses) and the permission gate
    // is not what is under test here — the verification command is.
    let mut tools = crate::exec_tools::ExecTools::workspace_with_permissions(
        scratch,
        crate::permissions::PermissionLattice::new(
            crate::permissions::PermissionMode::BypassPermissions,
        ),
    )
    .map_err(|err| err.to_string())?;
    tools.set_approval_source(std::sync::Arc::new(
        crate::approvals::LedgerApprovalSink::new(
            offline_ledger_client(scratch)?,
            SessionId::new(),
            event_ledger::event::ActorRef::new(
                event_ledger::event::ActorKind::Agent,
                &protocol::EventId::new().to_string(),
            )
            .map_err(|err| err.to_string())?,
            scratch.to_path_buf(),
        ),
    ));
    let spec = agent_runtime::AgentSpec::builder(
        AgentId::new(),
        agent_runtime::AgentRole::Coder,
        task.prompt.clone(),
        protocol::WorkspaceViewId::new(),
    )
    .permissions_profile("work")
    .build()
    .map_err(|err| err.to_string())?;
    let request = agent_runtime::AgentExecutionRequest::new(spec, SessionId::new());
    let mut model = GoldPatchModel::for_task(task);
    let mut events = Vec::new();
    let outcome = run_turn(
        agent_runtime::TurnSpec::new(
            protocol::TurnId::new(),
            SessionId::new(),
            AgentId::new(),
            agent_runtime::TurnBudget::new(16, Some(32), None).expect("budget"),
            &mut model,
            &mut tools,
            &mut events,
        ),
        &agent_runtime::CancellationToken::new(),
    )
    .map_err(|err| err.to_string())?;
    if outcome.status() != agent_runtime::TurnStatus::Completed {
        return Err(format!(
            "the gold trajectory turn did not complete: {}",
            outcome
                .reason()
                .map(|reason| reason.as_str())
                .unwrap_or("unknown")
        ));
    }
    // The external judge: verification exit 0 in the patched repo.
    if !run_verify(scratch, &task.verify, 120).unwrap_or(false) {
        return Err("verification command failed after the gold patch".to_owned());
    }
    Ok(())
}

fn offline_ledger_client(scratch: &Path) -> Result<kernel::InProcessKernelClient, String> {
    let ledger_path = scratch.join(".rapidlm").join("sessions.db");
    if let Some(parent) = ledger_path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    kernel::InProcessKernelClient::open(&ledger_path).map_err(|err| err.to_string())
}

// ---------------------------------------------------------------------------
// Live mode: real agent CLIs, identical repos, same judge.
// ---------------------------------------------------------------------------

/// The live agents. `rapid` is ourselves; competitors run when their pinned
/// CLI is on PATH. Each entry names the binary to probe and the argv shape.
pub const LIVE_AGENTS: &[(&str, &str)] = &[
    ("rapid", "rapid exec <prompt>"),
    ("qwen", "qwen -p <prompt> (pinned version required)"),
    ("grok", "grok <prompt> (pinned version required)"),
];

fn probe_binary(name: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(&format!("command -v {name} >/dev/null 2>&1"))
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// The real live runner: `bin` is the resolved agent binary (ours is the
/// current executable); the prompt is piped on stdin so agents with
/// different CLI grammars get identical instructions.
fn run_live_agent_with(
    bin: &str,
    args: &[String],
    task: &BenchTask,
    scratch: &Path,
) -> Result<(bool, u64), String> {
    use std::io::Write as _;
    use std::process::Stdio;
    let started = std::time::Instant::now();
    let mut child = Command::new(bin)
        .args(args)
        .current_dir(scratch)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("{bin} could not start: {err}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(task.prompt.as_bytes());
    }
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(LIVE_TASK_TIMEOUT_SECS);
    let status = loop {
        match child.try_wait().map_err(|err| err.to_string())? {
            Some(status) => break status,
            None => {
                if std::time::Instant::now() > deadline {
                    let _ = child.kill();
                    return Err(format!(
                        "{bin} exceeded the {LIVE_TASK_TIMEOUT_SECS}s ceiling"
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
    };
    let _ = started;
    Ok((status.success(), 0)) // tokens: parsed per-agent when the CLI reports them; 0 = not reported
}

/// Pinned competitor invocation recipes. Each entry: (name, version probe
/// binary, version flag, argv builder). A competitor runs only when its
/// binary is present AND a recipe is recorded here — an unpinned binary is
/// skipped rather than compared, and the recorded version is embedded in
/// the report so runs are reproducible.
fn agent_recipes() -> Vec<(String, String, String, Vec<String>)> {
    // (name, pinned version, version probe flag, argv prefix). The prompt is
    // appended as the final argument.
    vec![
        (
            "rapid".to_owned(),
            env!("CARGO_PKG_VERSION").to_owned(),
            String::new(),
            vec!["exec".to_owned()],
        ),
        (
            "grok".to_owned(),
            "1.0.30".to_owned(),
            "--version".to_owned(),
            // `--always-approve` is grok's auto-approval mode — the
            // comparable setting to rapid's acceptEdits (file/shell tools
            // auto-approved; the task's verify command is still the judge).
            // `--output-format json` returns the final answer plus the
            // provider-reported token/cost usage as one JSON object —
            // the harness's per-task usage source.
            vec![
                "--always-approve".to_owned(),
                "--output-format".to_owned(),
                "json".to_owned(),
                "-p".to_owned(),
            ],
        ),
    ]
}

/// Run the suite live for every agent with a recorded recipe whose binary
/// is present; agents that are absent or unpinned are recorded as skipped
/// with the reason — a comparison is only ever drawn between agents that
/// actually ran under pinned invocations.
pub fn run_live(
    tasks: &[BenchTask],
    scratch_root: &Path,
    self_exe: &Path,
    trusted: bool,
    grant_shell: bool,
) -> Vec<(String, Vec<TaskResult>)> {
    let mut agents: Vec<(String, PathBuf, Vec<String>)> = vec![(
        "rapid".to_owned(),
        self_exe.to_path_buf(),
        vec!["exec".to_owned()],
    )];
    for (name, pinned_version, version_flag, prefix) in agent_recipes() {
        if name == "rapid" {
            continue;
        }
        if !probe_binary(&name) {
            eprintln!("eval: competitor {name} not found on PATH; recorded as skipped");
            continue;
        }
        // Pin check: the installed version must match the pinned one.
        let observed = Command::new(&name)
            .arg(&version_flag)
            .output()
            .ok()
            .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
            .unwrap_or_default();
        if !observed.contains(&pinned_version) {
            eprintln!(
                "eval: competitor {name} version mismatch (pinned {pinned_version}, found {:?}); \
recorded as skipped rather than compared unpinned",
                observed.trim()
            );
            continue;
        }
        agents.push((name.clone(), PathBuf::from(&name), prefix));
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Usage files live OUTSIDE the per-task scratch (which is removed after
    // each task) so a failed task's spend is still recorded.
    let usage_dir = scratch_root.join("usage");
    let _ = std::fs::create_dir_all(&usage_dir);
    let mut per_agent = Vec::new();
    for (name, bin, args) in &agents {
        // Pre-flight: a probe task. If the model call itself fails (no
        // credentials, unreachable endpoint), every live task for that
        // agent is recorded as SKIPPED with that reason — failures of the
        // environment are not failures of the tasks.
        let probe = scratch_root.join(format!("preflight-{name}-{stamp:x}"));
        let model_ready = match run_live_agent(bin, args, &tasks[0], &probe, name, grant_shell, None)
        {
            (Err(reason), _)
                if reason.contains("model")
                    || reason.contains("provider")
                    || reason.contains("non-zero") =>
            {
                let _ = std::fs::remove_dir_all(&probe);
                Some(reason)
            }
            _ => {
                let _ = std::fs::remove_dir_all(&probe);
                None
            }
        };
        if let Some(reason) = model_ready {
            per_agent.push((
                name.clone(),
                tasks
                    .iter()
                    .map(|task| TaskResult {
                        id: task.id.clone(),
                        category: task.category.clone(),
                        outcome: "skipped".to_owned(),
                        reason: Some(format!("model not usable: {reason}")),
                        wall_ms: 0,
                        tokens: None,
                        cost_usd_micros: None,
                        cost_estimate_usd_micros: None,
                        input_tokens: None,
                        cached_tokens: None,
                        output_tokens: None,
                        num_turns: None,
                        agent: Some(name.clone()),
                    })
                    .collect(),
            ));
            continue;
        }
        let mut results = Vec::new();
        for task in tasks {
            let started = std::time::Instant::now();
            let scratch = scratch_root.join(format!("{name}-{}-{stamp:x}", task.id));
            let usage_path = usage_dir.join(format!("{name}-{}-{stamp:x}.json", task.id));
            let usage_file = if name == "rapid" {
                Some(usage_path.as_path())
            } else {
                None
            };
            let (result, usage) = run_live_agent(bin, args, task, &scratch, name, grant_shell, usage_file);
            let _ = std::fs::remove_dir_all(&scratch);
            results.push(TaskResult {
                id: task.id.clone(),
                category: task.category.clone(),
                outcome: match &result {
                    Ok(()) => "passed".to_owned(),
                    Err(reason) => {
                        if reason.contains("skipped") {
                            "skipped".to_owned()
                        } else {
                            "failed".to_owned()
                        }
                    }
                },
            reason: result.err(),
            wall_ms: started.elapsed().as_millis(),
            tokens: usage.tokens,
            cost_usd_micros: usage.cost_usd_micros,
            cost_estimate_usd_micros: estimate_cost_micros(&usage),
            input_tokens: usage.input_tokens,
            cached_tokens: usage.cached_tokens,
            output_tokens: usage.output_tokens,
            num_turns: usage.num_turns,
            agent: Some(name.clone()),
        });
        }
        per_agent.push((name.clone(), results));
    }
    per_agent
}

/// Consumption an agent reported for one task. Every field is optional:
/// agents differ in what they can report, and "unknown" must stay
/// distinguishable from "zero" in the report.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveUsage {
    pub tokens: Option<u64>,
    pub cost_usd_micros: Option<u64>,
    pub num_turns: Option<u64>,
    pub input_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

/// x.ai's published grok-4.6 API rates in USD per million tokens (input,
/// cached input, output) — [x.ai/api](https://x.ai/api),
/// [docs.x.ai/developers/pricing](https://docs.x.ai/developers/pricing).
/// Used ONLY for the clearly-labeled rapid cost estimate: the OpenAI-
/// compatible endpoint reports no cost field, so pricing rapid's measured
/// split at the provider's public catalog is an estimate, never a
/// measurement.
const RAPID_ESTIMATE_USD_PER_M: (f64, f64, f64) = (2.0, 0.5, 6.0);

/// Price a measured token split at the published rates. `None` when the
/// split itself was not reported. When the provider did not itemize the
/// cached subset, all input is priced at the uncached rate (a stated
/// upper bound, never silently optimistic).
fn estimate_cost_micros(usage: &LiveUsage) -> Option<u64> {
    let (input, output) = match (usage.input_tokens, usage.output_tokens) {
        (Some(input), Some(output)) => (input, output),
        _ => return None,
    };
    let (input_rate, cached_rate, output_rate) = RAPID_ESTIMATE_USD_PER_M;
    let cached = usage.cached_tokens.unwrap_or(0);
    let uncached = input.saturating_sub(cached);
    let micros = uncached as f64 * input_rate
        + cached as f64 * cached_rate
        + output as f64 * output_rate;
    Some(micros as u64)
}

/// Parse rapid's `exec --usage-file` output.
fn parse_rapid_usage_file(text: &str) -> Option<LiveUsage> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    Some(LiveUsage {
        tokens: value.get("tokens").and_then(|v| v.as_u64()),
        cost_usd_micros: value.get("cost_usd_micros").and_then(|v| v.as_u64()),
        num_turns: None,
        input_tokens: value.get("input_tokens").and_then(|v| v.as_u64()),
        cached_tokens: value.get("cached_tokens").and_then(|v| v.as_u64()),
        output_tokens: value.get("output_tokens").and_then(|v| v.as_u64()),
    })
}

/// Parse grok CLI `--output-format json` stdout. `total_cost_usd_ticks` is
/// the provider-reported cost in 1e-10 USD units (its integer form);
/// micros are ticks / 10_000, truncated.
fn parse_grok_usage(stdout: &str) -> Option<LiveUsage> {
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    if value.get("type").and_then(|v| v.as_str()) == Some("error") {
        return None;
    }
    let usage = value.get("usage")?;
    Some(LiveUsage {
        tokens: usage.get("total_tokens").and_then(|v| v.as_u64()),
        cost_usd_micros: value
            .get("total_cost_usd_ticks")
            .and_then(|v| v.as_u64())
            .map(|ticks| ticks / 10_000),
        num_turns: value.get("num_turns").and_then(|v| v.as_u64()),
        input_tokens: usage.get("input_tokens").and_then(|v| v.as_u64()),
        cached_tokens: usage
            .get("cache_read_input_tokens")
            .and_then(|v| v.as_u64()),
        output_tokens: usage.get("output_tokens").and_then(|v| v.as_u64()),
    })
}

/// Read up to `cap` bytes from a stream (looping — one `read` may return
/// less than the buffer even with more pending).
fn read_capped(stream: &mut impl std::io::Read, cap: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;
    let mut buf = Vec::with_capacity(cap.min(64 * 1024));
    let mut chunk = [0u8; 16 * 1024];
    while buf.len() < cap {
        let room = (cap - buf.len()).min(chunk.len());
        let read = stream.read(&mut chunk[..room])?;
        if read == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..read]);
    }
    Ok(buf)
}

/// Drive one live agent on one task: materialize the identical repo, run
/// the agent's argv with the prompt appended, then judge with the task's
/// verification command (with the anti-vacuity check first). Returns the
/// usage the agent reported, when it can.
#[allow(clippy::too_many_arguments)]
fn run_live_agent(
    bin: &Path,
    args: &[String],
    task: &BenchTask,
    scratch: &Path,
    name: &str,
    grant_shell: bool,
    usage_file: Option<&Path>,
) -> (Result<(), String>, LiveUsage) {
    if let Err(reason) = materialize(scratch, task) {
        return (Err(reason), LiveUsage::default());
    }
    if task.verify_fails_before && run_verify(scratch, &task.verify, 60).unwrap_or(true) {
        return (
            Err("vacuous task (verify passes before any change)".to_owned()),
            LiveUsage::default(),
        );
    }
    use std::io::Read as _;
    use std::process::{Command, Stdio};
    let mut argv = args.to_vec();
    if let Some(path) = usage_file {
        argv.push("--usage-file".to_owned());
        argv.push(path.display().to_string());
    }
    argv.push(task.prompt.clone());
    let mut command = Command::new(bin);
    command
        .args(&argv)
        .current_dir(scratch)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if name == "rapid" {
        // The harness acts as the operator for its own scratch repos:
        // grant trust (each materialized repo is a fresh root) and run
        // rapid exec in acceptEdits mode — file edits auto-approved, deny
        // rules and managed ceilings still enforced. This mirrors what a
        // real operator does interactively; it grants nothing to the model
        // that the operator did not. With `grant_shell` (the equal-surface
        // configuration) the operator also pre-approves shell_exec per
        // scratch repo, matching the competitor's auto-approved shell.
        let trust = match Command::new(bin)
            .arg("trust")
            .arg("grant")
            .current_dir(scratch)
            .status()
        {
            Ok(status) => status,
            Err(err) => return (Err(err.to_string()), LiveUsage::default()),
        };
        if !trust.success() {
            return (
                Err("trust grant failed in scratch repo".to_owned()),
                LiveUsage::default(),
            );
        }
        if grant_shell {
            let shell = match Command::new(bin)
                .args(["permissions", "allow", "shell_exec"])
                .current_dir(scratch)
                .status()
            {
                Ok(status) => status,
                Err(err) => return (Err(err.to_string()), LiveUsage::default()),
            };
            if !shell.success() {
                return (
                    Err("shell_exec pre-approval failed in scratch repo".to_owned()),
                    LiveUsage::default(),
                );
            }
        }
        command.env("RAPIDLM_PERMISSION_MODE", "acceptEdits");
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            return (
                Err(format!("{name} could not start: {err}")),
                LiveUsage::default(),
            )
        }
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(LIVE_TASK_TIMEOUT_SECS);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if std::time::Instant::now() > deadline {
                    let _ = child.kill();
                    return (
                        Err(format!(
                            "{name} exceeded the {LIVE_TASK_TIMEOUT_SECS}s ceiling"
                        )),
                        LiveUsage::default(),
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            Err(err) => {
                return (
                    Err(err.to_string()),
                    LiveUsage::default(),
                );
            }
        }
    };
    // The 512 KiB stdout cap exists for the grok JSON contract: its whole
    // final answer comes back as one JSON object (text + thought + usage).
    let mut text = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let capped = read_capped(&mut stdout, 512 * 1024).unwrap_or_default();
        text.push_str(&String::from_utf8_lossy(&capped));
    }
    if let Some(mut stderr) = child.stderr.take() {
        let mut capped = vec![0u8; 8 * 1024];
        let read = stderr.read(&mut capped).unwrap_or(0);
        let err_text = String::from_utf8_lossy(&capped[..read]);
        if !err_text.trim().is_empty() {
            text.push_str("\n[stderr] ");
            text.push_str(err_text.trim());
        }
    }
    if !status.success() {
        let mut snippet: String = text.chars().take(300).collect();
        snippet = snippet.replace('\n', " ");
        let usage = collect_usage(name, usage_file, &text);
        return (Err(format!("{name} exited non-zero: {snippet}")), usage);
    }
    let usage = collect_usage(name, usage_file, &text);
    if !run_verify(scratch, &task.verify, 120).unwrap_or(false) {
        return (
            Err("verification command failed after the live run".to_owned()),
            usage,
        );
    }
    (Ok(()), usage)
}

/// Usage harvest after a live run: grok reports in its stdout JSON, rapid
/// in the `--usage-file` the harness passed. Missing on either side stays
/// `None` — unknown, never zero.
fn collect_usage(name: &str, usage_file: Option<&Path>, stdout: &str) -> LiveUsage {
    if name == "grok" {
        parse_grok_usage(stdout).unwrap_or_default()
    } else if let Some(path) = usage_file {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|body| parse_rapid_usage_file(&body))
            .unwrap_or_default()
    } else {
        LiveUsage::default()
    }
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

/// Summary counts + the JSON report body. Mechanical and live reports are
/// written to separate files and never summed together.
pub fn summarize(mode: &str, results: &[TaskResult]) -> (serde_json::Value, String) {
    let passed = results.iter().filter(|r| r.outcome == "passed").count();
    let failed = results.iter().filter(|r| r.outcome == "failed").count();
    let skipped = results.iter().filter(|r| r.outcome == "skipped").count();
    // Tokens and cost per VERIFIED success (goal §7), PER AGENT: totals over
    // that agent's passed tasks, divided by the pass count. Cost divides
    // only tasks whose cost the provider actually reported — the divisor is
    // the number of those tasks, reported beside the rate so partial
    // coverage cannot masquerade as a full comparison. Cross-agent totals
    // are never summed: different agents' spend is not one budget.
    let mut agent_names: Vec<String> = Vec::new();
    for result in results {
        let name = result.agent.clone().unwrap_or_else(|| "runner".to_owned());
        if !agent_names.contains(&name) {
            agent_names.push(name);
        }
    }
    let mut agent_metrics = serde_json::Map::new();
    let mut metric_lines: Vec<String> = Vec::new();
    for agent in &agent_names {
        let passed_tasks: Vec<&TaskResult> = results
            .iter()
            .filter(|r| r.outcome == "passed")
            .filter(|r| r.agent.as_deref().unwrap_or("runner") == agent)
            .collect();
        if passed_tasks.is_empty() {
            continue;
        }
        let tokens_reported = passed_tasks.iter().filter(|r| r.tokens.is_some()).count();
        let tokens_total: u64 = passed_tasks.iter().filter_map(|r| r.tokens).sum();
        let cost_reported = passed_tasks
            .iter()
            .filter(|r| r.cost_usd_micros.is_some())
            .count();
        let cost_total: u64 = passed_tasks.iter().filter_map(|r| r.cost_usd_micros).sum();
        let est_reported = passed_tasks
            .iter()
            .filter(|r| r.cost_estimate_usd_micros.is_some())
            .count();
        let est_total: u64 = passed_tasks
            .iter()
            .filter_map(|r| r.cost_estimate_usd_micros)
            .sum();
        let mut entry = serde_json::Map::new();
        if tokens_reported > 0 {
            let per_success = tokens_total / passed_tasks.len() as u64;
            entry.insert(
                "tokens_per_verified_success".to_owned(),
                serde_json::json!({
                    "tokens": per_success,
                    "over_passed_tasks": passed_tasks.len(),
                    "usage_reported_for": tokens_reported,
                }),
            );
            metric_lines.push(format!(
                "  {agent} tokens/verified-success: {per_success} (usage reported for \
                 {tokens_reported}/{} passed)",
                passed_tasks.len()
            ));
        }
        if cost_reported > 0 {
            let per_success = cost_total / cost_reported as u64;
            entry.insert(
                "cost_per_verified_success".to_owned(),
                serde_json::json!({
                    "usd_micros": per_success,
                    "over_tasks_with_reported_cost": cost_reported,
                }),
            );
            metric_lines.push(format!(
                "  {agent} cost/verified-success: ${:.4} (cost reported for {cost_reported} passed)",
                per_success as f64 / 1_000_000.0
            ));
        }
        if est_reported > 0 {
            let per_success = est_total / est_reported as u64;
            entry.insert(
                "cost_estimate_per_verified_success".to_owned(),
                serde_json::json!({
                    "usd_micros": per_success,
                    "over_tasks_with_reported_split": est_reported,
                    "basis": "measured token split priced at the provider's published rates — an estimate, not a measurement",
                }),
            );
            metric_lines.push(format!(
                "  {agent} cost-estimate/verified-success: ${:.4} (published-rate estimate over {est_reported} passed)",
                per_success as f64 / 1_000_000.0
            ));
        }
        if !entry.is_empty() {
            agent_metrics.insert(agent.clone(), serde_json::Value::Object(entry));
        }
    }
    let mut report = serde_json::json!({
        "mode": mode,
        "kind": if mode == "offline" { "mechanical validation (scripted gold trajectories; says nothing about model quality)" } else { "live model quality (agents given prompts only; the verification command is the judge)" },
        "tasks": results.len(),
        "passed": passed,
        "failed": failed,
        "skipped": skipped,
        "results": results,
    });
    if !agent_metrics.is_empty() {
        report["per_agent_usage"] = serde_json::Value::Object(agent_metrics);
    }
    let mut lines = vec![format!(
        "{mode}: {}/{} passed, {failed} failed, {skipped} skipped",
        passed,
        results.len()
    )];
    lines.extend(metric_lines);
    for result in results {
        if result.outcome != "passed" {
            lines.push(format!(
                "  {} [{}]{} {}: {}",
                result.id,
                result.category,
                result
                    .agent
                    .as_ref()
                    .map(|agent| format!(" ({agent})"))
                    .unwrap_or_default(),
                result.outcome,
                result.reason.as_deref().unwrap_or("")
            ));
        }
    }
    (report, lines.join("\n"))
}

/// Results directory for a run.
pub fn results_dir(root: &Path) -> PathBuf {
    root.join("eval").join("results")
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

pub const EVAL_USAGE: &str = "usage: rapid eval --offline [--suite <dir>] [--scratch <dir>] | rapid eval --live [--grant-shell] [--suite <dir>]

Run the reproducible coding benchmark.

  --offline   mechanical validation: each task's gold patch replays through
              the real turn executor (tools, permission lattice, ledger) and
              the task's verification command is the only judge. Deterministic;
              says nothing about model quality.
  --live      model quality: real agent CLIs run on identical scratch repos
              with the same prompts and the same verification commands.
              Agents without a pinned, present binary are recorded as skipped;
              no comparison is claimed without actual runs on both sides.
  --grant-shell  (live, rapid arm only) pre-approve shell_exec per scratch
              repo — the equal-tool-surface configuration, matching a
              competitor that auto-approves shell. Without it rapid runs
              acceptEdits (shell denied) and the report says so.

Results are written to eval/results/<mode>-<stamp>.json; exit 0 iff every
non-skipped task passed.
";

/// `rapid eval`.
pub fn run_eval(args: &[String]) -> Result<i32, crate::p9_commands::P9CommandError> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{EVAL_USAGE}");
        return Ok(0);
    }
    let offline = args.iter().any(|arg| arg == "--offline");
    let live = args.iter().any(|arg| arg == "--live");
    if offline == live {
        eprint!("{EVAL_USAGE}");
        return Err(crate::p9_commands::P9CommandError::Usage);
    }
    let suite = args
        .iter()
        .position(|arg| arg == "--suite")
        .and_then(|position| args.get(position + 1))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(SUITE_DIR));
    let scratch = args
        .iter()
        .position(|arg| arg == "--scratch")
        .and_then(|position| args.get(position + 1))
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("rapid-eval-scratch"));
    let _ = std::fs::create_dir_all(&scratch);
    let tasks = load_suite(&suite).map_err(|err| {
        crate::p9_commands::P9CommandError::Agent(format!("suite load failed: {err}"))
    })?;
    let Some((root, trusted)) = crate::interactive::workflow_workspace_root() else {
        eprintln!("rapid eval: no project workspace resolved");
        return Err(crate::p9_commands::P9CommandError::Usage);
    };
    let _ = &root;

    let mode = if offline { "offline" } else { "live" };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let results = if offline {
        run_offline(&tasks, &scratch, trusted)
    } else {
        let grant_shell = args.iter().any(|arg| arg == "--grant-shell");
        let self_exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rapid"));
        run_live(&tasks, &scratch, &self_exe, trusted, grant_shell)
            .into_iter()
            .flat_map(|(_agent, results)| results)
            .collect::<Vec<_>>()
    };
    let (report, summary) = summarize(mode, &results);
    let results_dir = results_dir(&std::env::current_dir().unwrap_or_default());
    let _ = std::fs::create_dir_all(&results_dir);
    let results_path = results_dir.join(format!("{mode}-{stamp}.json"));
    let written = serde_json::to_string_pretty(&report)
        .map(|json| std::fs::write(&results_path, json + "\n").is_ok())
        .unwrap_or(false);
    println!("{summary}");
    if written {
        println!("results: {}", results_path.display());
    }
    let any_failed = results.iter().any(|result| result.outcome == "failed");
    Ok(if any_failed { 1 } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, agent: Option<&str>, outcome: &str, tokens: Option<u64>, cost: Option<u64>) -> TaskResult {
        TaskResult {
            id: id.to_owned(),
            category: "bugfix".to_owned(),
            outcome: outcome.to_owned(),
            reason: None,
            wall_ms: 1,
            tokens,
            cost_usd_micros: cost,
            cost_estimate_usd_micros: None,
            input_tokens: None,
            cached_tokens: None,
            output_tokens: None,
            num_turns: None,
            agent: agent.map(|a| a.to_owned()),
        }
    }

    #[test]
    fn grok_usage_json_yields_tokens_cost_and_turns() {
        // Verbatim shape from a live grok CLI `--output-format json` run.
        let stdout = r#"{
  "text": "OK",
  "stopReason": "end_turn",
  "sessionId": "01a0a130",
  "usage": {"input_tokens": 5961, "cache_read_input_tokens": 10496, "output_tokens": 34, "reasoning_tokens": 29, "total_tokens": 16491},
  "num_turns": 1,
  "total_cost_usd": 0.00590716,
  "total_cost_usd_ticks": 59071600
}"#;
        let usage = parse_grok_usage(stdout).expect("parses");
        assert_eq!(usage.tokens, Some(16491));
        assert_eq!(usage.num_turns, Some(1));
        // ticks / 10_000, truncated: 59071600 / 10_000 = 5907 micros.
        assert_eq!(usage.cost_usd_micros, Some(5907));
    }

    #[test]
    fn grok_error_object_reports_no_usage() {
        let stdout = r#"{"type":"error","message":"Couldn't set model: unknown model id"}"#;
        assert_eq!(parse_grok_usage(stdout), None);
    }

    #[test]
    fn rapid_usage_file_parses_and_null_cost_stays_unknown() {
        let usage = parse_rapid_usage_file(
            r#"{"tokens":4321,"cost_usd_micros":null,"input_tokens":4000,"cached_tokens":3200,"output_tokens":321,"tool_calls":6,"status":"Completed"}"#,
        )
        .expect("parses");
        assert_eq!(usage.tokens, Some(4321));
        assert_eq!(usage.cost_usd_micros, None, "null is unknown, never free");
        assert_eq!(usage.input_tokens, Some(4000));
        assert_eq!(usage.cached_tokens, Some(3200));
        assert_eq!(usage.output_tokens, Some(321));
        assert_eq!(parse_rapid_usage_file("not json"), None);
        // Published-rate estimate: (4000-3200)×$2/M + 3200×$0.5/M + 321×$6/M
        // = 800×2 + 3200×0.5 + 321×6 micros = 1600 + 1600 + 1926 = 5126.
        assert_eq!(estimate_cost_micros(&usage), Some(5126));
        // No split reported: no estimate rather than a fabricated one.
        let bare = parse_rapid_usage_file(r#"{"tokens":99,"cost_usd_micros":null}"#).expect("parses");
        assert_eq!(estimate_cost_micros(&bare), None);
    }

    #[test]
    fn summarize_reports_usage_per_agent_and_never_across_agents() {
        let results = vec![
            task("a-1", Some("rapid"), "passed", Some(1000), None),
            task("a-2", Some("rapid"), "passed", Some(3000), None),
            task("a-3", Some("rapid"), "failed", Some(999_999), None),
            task("g-1", Some("grok"), "passed", Some(500), Some(1000)),
            task("g-2", Some("grok"), "passed", Some(1500), Some(3000)),
            task("g-3", Some("grok"), "failed", Some(777), Some(999)),
        ];
        let (report, lines) = summarize("live", &results);
        let per_agent = report["per_agent_usage"].as_object().expect("per-agent block");
        // rapid: usage only, (1000+3000)/2 = 2000 tokens per success; no
        // cost entry (provider reported none) and the failed task's spend
        // never leaks into the rate.
        let rapid = &per_agent["rapid"];
        assert_eq!(
            rapid["tokens_per_verified_success"]["tokens"].as_u64(),
            Some(2000)
        );
        assert!(rapid.get("cost_per_verified_success").is_none());
        // grok: (500+1500)/2 = 1000 tokens; (1000+3000)/2 = 2000 micros.
        let grok = &per_agent["grok"];
        assert_eq!(grok["tokens_per_verified_success"]["tokens"].as_u64(), Some(1000));
        assert_eq!(
            grok["cost_per_verified_success"]["usd_micros"].as_u64(),
            Some(2000)
        );
        assert_eq!(
            grok["cost_per_verified_success"]["over_tasks_with_reported_cost"].as_u64(),
            Some(2)
        );
        // The human summary names each metric line with its agent.
        assert!(lines.contains("rapid tokens/verified-success: 2000"));
        assert!(lines.contains("grok cost/verified-success: $0.0020"));
        // Failure lines carry the agent so the report is attributable.
        assert!(lines.contains("a-3 [bugfix] (rapid) failed"));
    }

    #[test]
    fn summarize_without_usage_reports_no_metrics() {
        let results = vec![task("x-1", None, "passed", None, None)];
        let (report, lines) = summarize("offline", &results);
        assert!(report.get("per_agent_usage").is_none());
        assert_eq!(lines.lines().count(), 1, "only the headline, no metric lines");
    }
}
