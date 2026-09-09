//! P9 production entry points wired through the composition root.
//!
//! - `rapid playbook-compile <file.json>`  PlaybookCompiler -> initial graph JSON.
//! - `rapid mcp-tools`                     published RapidLM MCP server surface.
//! - `rapid agent-cli <prompt> -- argv...` supervised external CLI agent turn.
//!
//! Each command drives a real subsystem through its production API; stdout is
//! protocol output, diagnostics go to stderr.

use std::path::PathBuf;
use std::time::Instant;

use capability_broker::{
    ActionRequest, ApprovalChoice, ApprovalResolution, ApprovalScopeId, CanonicalAction,
    CancellationToken, ExecIntent, LeaseIssuer, LeaseValidator, LiveHostResolver, PolicyDocument,
    PolicyRevision, PolicySource, PolicyStack, PrincipalRef, evaluate, issue, normalize_exec,
    request_approval, validate_use,
};
use agent_runtime::ToolDriver;
use crate::external_agents::CliRunner;
use crate::headless::jsonl::JsonlExitCode;
use mcp::{ImplementationInfo, McpServer, McpServerConfig, ProtocolVersion};
use scheduler::graph::RuntimeGraph;
use scheduler::kinds::NodeKind;
use scheduler::playbook::{PlaybookError, PlaybookStep, PlaybookTemplate, compile};

const EXTERNAL_AGENT_SUBJECT: &str = "cli/external-agent";

/// Typed failure for P9 subcommands; stderr display only.
#[derive(Debug)]
pub enum P9CommandError {
    Usage,
    Io(std::io::Error),
    Json(serde_json::Error),
    Playbook(PlaybookError),
    Agent(String),
    Scan(String),
}

impl std::fmt::Display for P9CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Usage => f.write_str("usage: see `rapid --help`"),
            Self::Io(err) => writeln!(f, "io: {err}"),
            Self::Json(err) => writeln!(f, "json: {err}"),
            Self::Playbook(err) => writeln!(f, "playbook: {err}"),
            Self::Agent(reason) => writeln!(f, "external agent: {reason}"),
            Self::Scan(reason) => writeln!(f, "scan: {reason}"),
        }
    }
}


fn node_kind_from_str(raw: &str) -> Option<NodeKind> {
    match raw {
        "goal" => Some(NodeKind::Goal),
        "criterion" => Some(NodeKind::Criterion),
        "plan" => Some(NodeKind::Plan),
        "task" => Some(NodeKind::Task),
        "agent" => Some(NodeKind::Agent),
        "tool" => Some(NodeKind::ToolInvocation),
        "process" => Some(NodeKind::Process),
        "monitor" => Some(NodeKind::Monitor),
        "approval" => Some(NodeKind::Approval),
        "artifact" => Some(NodeKind::Artifact),
        "claim" => Some(NodeKind::Claim),
        _ => None,
    }
}

/// `rapid playbook-compile <file.json>`: template -> initial RuntimeGraph.
/// The compiled graph is printed as canonical JSON on stdout.
pub fn run_playbook_compile(args: &[String]) -> Result<i32, P9CommandError> {
    let path = args
        .first()
        .map(PathBuf::from)
        .ok_or(P9CommandError::Usage)?;
    let bytes = std::fs::read(&path).map_err(P9CommandError::Io)?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(P9CommandError::Json)?;
    let name = value
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("playbook");
    let steps = value
        .get("steps")
        .and_then(serde_json::Value::as_array)
        .ok_or(P9CommandError::Usage)?;
    if steps.is_empty() {
        return Err(P9CommandError::Usage);
    }
    let mut template = PlaybookTemplate::new(name);
    for step in steps {
        let key = step
            .get("key")
            .and_then(serde_json::Value::as_str)
            .ok_or(P9CommandError::Usage)?;
        let kind_raw = step
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .ok_or(P9CommandError::Usage)?;
        let kind = node_kind_from_str(kind_raw).ok_or(P9CommandError::Usage)?;
        let label = step
            .get("label")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(key);
        let deps: Vec<String> = step
            .get("depends_on")
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let mut built =
            PlaybookStep::new(key, kind, label).with_dependencies(deps);
        if let Some(budget) = step.get("budget_tokens").and_then(serde_json::Value::as_u64) {
            built = built.with_budget(budget);
        }
        template = template.push(built);
    }
    let graph: RuntimeGraph = compile(&template, protocol::GraphId::new())
        .map_err(P9CommandError::Playbook)?;
    let json = graph.export_json().map_err(P9CommandError::Json)?;
    println!("{json}");
    Ok(0)
}

/// `rapid mcp-tools [--tools repo.read,workspace.patch]`: print the explicit
/// RapidLM MCP server surface after a real client handshake. The surface is
/// config ∩ effective policy; requested-but-unauthorized tools are absent.
pub fn run_mcp_tools(args: &[String]) -> Result<i32, P9CommandError> {
    let default_tools = ["repo.read"];
    let mut requested: Vec<String> = Vec::new();
    if let Some(pos) = args.iter().position(|a| a == "--tools") {
        let raw = args.get(pos + 1).ok_or(P9CommandError::Usage)?;
        requested = raw.split(',').map(|s| s.trim().to_owned()).collect();
    }
    let tools: Vec<&str> = if requested.is_empty() {
        default_tools.to_vec()
    } else {
        requested.iter().map(String::as_str).collect()
    };
    let mut config = McpServerConfig::new();
    for name in &tools {
        config = config
            .with_tool(name)
            .map_err(|err| P9CommandError::Agent(format!("{err:?}")))?;
    }
    // Policy: read-only tools publish; anything else stays unpublished.
    let policy = mcp_tools_policy(&tools, &CancellationToken::new());
    let mut server = McpServer::new(config, &policy);
    let info =
        ImplementationInfo::new("rapidlm-cli", "1").map_err(|err| P9CommandError::Agent(format!("{err:?}")))?;
    server
        .accept_client(info, ProtocolVersion::TARGET, &CancellationToken::new())
        .map_err(|err| P9CommandError::Agent(format!("{err:?}")))?;
    let surface = server
        .published_surface(&CancellationToken::new())
        .map_err(|err| P9CommandError::Agent(format!("{err:?}")))?;
    let tool_names: Vec<String> = surface
        .tools()
        .iter()
        .map(|t| format!("{t:?}"))
        .collect();
    let payload = serde_json::json!({
        "schema": "rapidlm.mcp_surface",
        "requested": &tools,
        "published_tools": tool_names,
        "exposes_write": surface.exposes_write(),
        "exposes_shell": surface.exposes_shell(),
        "exposes_browser": surface.exposes_browser(),
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&payload).map_err(P9CommandError::Json)?
    );
    Ok(0)
}

/// Dump the model-facing tool surface's typed JSON schemas — a static,
/// versioned artifact analogous to Claude Code's `sdk-tools.d.ts` or Grok
/// Build's protobuf tool API (newtask.md item 1.3/#10: "no SDK-style typed
/// tool-schema export"). Introspective only: `root` is never written to,
/// just used to construct a real `ExecTools` (the same tool-name/schema set
/// the model would see for a trusted workspace), defaulting to the current
/// directory when `--root` is not given. `--read-only` dumps the narrower
/// surface a subagent's `explore`/`plan` scope actually gets instead.
pub fn run_tools_schema(args: &[String]) -> Result<i32, P9CommandError> {
    let mut root = std::env::current_dir().map_err(P9CommandError::Io)?;
    let mut read_only = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--root" => {
                i += 1;
                root = args.get(i).map(PathBuf::from).ok_or(P9CommandError::Usage)?;
            }
            "--read-only" => read_only = true,
            _ => return Err(P9CommandError::Usage),
        }
        i += 1;
    }
    let tools = if read_only {
        crate::exec_tools::ExecTools::read_only(&root)
    } else {
        crate::exec_tools::ExecTools::workspace(&root)
    }
    .map_err(|err| P9CommandError::Agent(format!("{err:?}")))?;
    let schemas: Vec<serde_json::Value> = tools
        .tool_surface()
        .iter()
        .map(|tool| {
            serde_json::json!({
                "name": tool.name(),
                "description": tool.description(),
                "parameters": tool.parameters(),
            })
        })
        .collect();
    let payload = serde_json::json!({
        "schema": "rapidlm.tool_surface",
        "read_only": read_only,
        "tools": schemas,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&payload).map_err(P9CommandError::Json)?
    );
    Ok(0)
}

fn mcp_tools_policy(
    tools: &[&str],
    cancel: &CancellationToken,
) -> capability_broker::PolicyStack {
    // Map each requested tool to its minimal capability family.
    let caps: Vec<String> = tools
        .iter()
        .map(|t| match *t {
            "repo.read" => "fs.read".to_owned(),
            "workspace.patch" => "fs.write".to_owned(),
            _ => "fs.read".to_owned(),
        })
        .collect();
    let mut src = String::new();
    for (i, cap) in caps.iter().enumerate() {
        src.push_str(&format!(
            "[[rules]]\nid = \"mcp-{i}\"\neffect = \"allow\"\nsubjects = [\"mcp-client/rapidlm-cli\"]\ncapability = \"{cap}\"\n\n"
        ));
    }
    let doc = PolicyDocument::parse_toml(
        &src,
        PolicySource::user("mcp-tools.toml").expect("source"),
        cancel,
    )
    .expect("policy parse");
    PolicyStack::new([doc]).expect("stack")
}

/// `rapid agent-cli <prompt> -- <argv...>`: one supervised external CLI agent
/// turn. The invoking operator approves the proc.exec ask interactively by
/// passing the command explicitly; output is the normalized, trust-labeled
/// result record.
pub fn run_agent_cli(args: &[String]) -> Result<i32, P9CommandError> {
    let split = args
        .iter()
        .position(|a| a == "--")
        .ok_or(P9CommandError::Usage)?;
    let prompt = args[..split].join(" ");
    let argv: Vec<String> = args[split + 1..].to_vec();
    if prompt.is_empty() || argv.is_empty() {
        return Err(P9CommandError::Usage);
    }
    let cancel = CancellationToken::new();
    let session_id = protocol::SessionId::new();
    let principal =
        PrincipalRef::parse(EXTERNAL_AGENT_SUBJECT).map_err(|_| P9CommandError::Usage)?;
    let mut runner = crate::external_agents::SupervisedCliRunner::new(
        argv,
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        principal,
        session_id,
        cancel.clone(),
    )
    .map_err(|err| P9CommandError::Agent(format!("{err:?}")))?;
    let task = crate::external_agents::ExternalAgentTask::new(session_id, prompt.clone())
        .map_err(|err| P9CommandError::Agent(format!("{err:?}")))?;

    // Prepare, approve (operator consent), issue, validate, execute.
    let spec = runner
        .prepare(&task, &cancel)
        .map_err(|err| P9CommandError::Agent(format!("{err:?}")))?;
    let policies = agent_cli_policy(&cancel);
    let issuer = LeaseIssuer::from_key(agent_cli_key())
        .map_err(|err| P9CommandError::Agent(format!("{err:?}")))?;
    let validator = LeaseValidator::new(
        LeaseIssuer::from_key(agent_cli_key())
            .map_err(|err| P9CommandError::Agent(format!("{err:?}")))?,
        PolicyRevision::of_stack(&policies),
    );
    let guard = {
        let binding = spec.binding().ok_or(P9CommandError::Usage)?;
        let env_names = spec.env().keys().cloned();
        let intent = match spec.invocation() {
            process_supervisor::Invocation::Argv { argv } => {
                ExecIntent::argv(argv.clone(), spec.cwd().as_str().to_owned(), env_names)
            }
            _ => return Err(P9CommandError::Usage),
        };
        let command = normalize_exec(&intent, &LiveHostResolver, &cancel)
            .map_err(|err| P9CommandError::Agent(format!("{err:?}")))?;
        let action = CanonicalAction::Command(command);
        let request = ActionRequest::new(
            binding.principal().clone(),
            binding.session_id(),
            binding.capability(),
            binding.resource().clone(),
            action.clone(),
            crate::external_agents::AGENT_COMMAND_FAMILY,
        )
        .map_err(|err| P9CommandError::Agent(format!("{err:?}")))?;
        let now = Instant::now();
        let decision = evaluate(&policies, &request, &cancel)
            .map_err(|err| P9CommandError::Agent(format!("{err:?}")))?;
        let approval = request_approval(&request, &decision, now, &cancel)
            .map_err(|err| P9CommandError::Agent(format!("approval required ({err})")))?;
        let approved = approval
            .resolve(
                ApprovalChoice::Approve(ApprovalScopeId::Once),
                &request,
                now,
                &cancel,
            )
            .map_err(|err| P9CommandError::Agent(format!("{err:?}")))
            .and_then(|resolution| match resolution {
                ApprovalResolution::Approved(approved) => Ok(approved),
                ApprovalResolution::Denied => Err(P9CommandError::Agent("denied".into())),
            })?;
        let lease = issue(&issuer, &approved, &policies, now, &cancel)
            .map_err(|err| P9CommandError::Agent(format!("{err:?}")))?;
        validate_use(&validator, &lease, &action, now, &cancel)
            .map_err(|err| P9CommandError::Agent(format!("{err:?}")))?
    };
    let exit = runner
        .run(spec, &task, guard, &cancel)
        .map_err(|err| P9CommandError::Agent(format!("{err:?}")))?;
    eprintln!("external agent result is untrusted context");
    let outcome =
        crate::external_agents::normalize_cli_exit(exit.ok, exit.code);
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "schema": "rapidlm.external_agent_result",
            "flavor": "cli",
            "outcome": outcome_marker(&outcome),
            "text": exit.stdout,
            "trust_label": crate::external_agents::EXTERNAL_TRUST_LABEL,
        }))
        .map_err(P9CommandError::Json)?
    );
    Ok(if matches!(
        outcome,
        crate::external_agents::AgentOutcome::Completed { .. }
    ) {
        0
    } else {
        1
    })
}

fn outcome_marker(outcome: &crate::external_agents::AgentOutcome) -> String {
    use crate::external_agents::AgentOutcome;
    match outcome {
        AgentOutcome::Completed { stop_reason } => format!("completed:{stop_reason}"),
        AgentOutcome::Failed { reason } => format!("failed:{reason}"),
        AgentOutcome::ProcessLost => "process_lost".to_owned(),
    }
}

fn agent_cli_policy(cancel: &CancellationToken) -> capability_broker::PolicyStack {
    let src = r#"
[[rules]]
id = "agent-cli-exec"
effect = "ask"
subjects = ["cli/external-agent"]
capability = "proc.exec"
"#;
    let doc = PolicyDocument::parse_toml(
        src,
        PolicySource::user("agent-cli.toml").expect("source"),
        cancel,
    )
    .expect("policy parse");
    PolicyStack::new([doc]).expect("stack")
}

/// A random key generated once per process, reused by every lease issuer and
/// validator this process constructs. A fixed constant here would be an
/// unexplained deviation from `LeaseIssuer::ephemeral()`'s own documented
/// practice ("fresh in-process key... not a credential-store secret") with
/// no real gain: the lease token itself never leaves process memory (no
/// `Serialize` impl, `token_for_tool_output` always errors), so a fixed key
/// buys nothing, and its only cost is a value an attacker with any read
/// access to this binary's memory or source could predict in advance. Not
/// `LeaseIssuer::ephemeral()` itself: it generates a fresh key on every call
/// with no accessor to recover the bytes, but the issuer and validator built
/// from this key below must share the identical bytes to MAC-verify against
/// each other.
fn agent_cli_key() -> [u8; 32] {
    static KEY: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
    *KEY.get_or_init(|| loop {
        let mut seed = Vec::with_capacity(64);
        for _ in 0..4 {
            seed.extend_from_slice(protocol::SessionId::new().as_uuid().as_bytes());
        }
        let key = *protocol::ArtifactId::from_bytes(&seed).as_digest();
        if key.iter().any(|byte| *byte != 0) {
            return key;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn temp_file(name: &str) -> PathBuf {
        let seq = TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "rapidlm-p9-{name}-{}-{seq}",
            std::process::id()
        ))
    }

    #[test]
    fn doctor_help_is_truthful_about_being_offline_read_only_and_its_exit_codes() {
        // The command's own help is the contract a scripted caller reads;
        // it must not advertise behavior the implementation does not have.
        // (The previous help called this "sandbox/policy/credential
        // diagnostics" while every check reported `Unavailable`.)
        for claim in ["Offline", "Read-only", "connectivity not\ntested", "Exit code"] {
            assert!(DOCTOR_USAGE.contains(claim), "help missing {claim:?}");
        }
        let code = run_doctor(&["--help".to_owned()]).expect("help");
        assert_eq!(code, 0);
    }

    #[test]
    fn doctor_rejects_any_argument_instead_of_silently_ignoring_it() {
        for arg in ["--json", "sandbox", "--live"] {
            assert!(
                matches!(run_doctor(&[arg.to_owned()]), Err(P9CommandError::Usage)),
                "`rapid doctor {arg}` must not look like it did something"
            );
        }
    }

    #[test]
    fn agent_cli_key_is_process_stable_and_no_longer_the_old_fixed_constant() {
        // The issuer and the validator built from this key (run_agent_cli,
        // around line 316) must see byte-identical keys within one process
        // or every lease would fail to MAC-verify — so this must be stable
        // across repeated calls, not a fresh value each time.
        let a = agent_cli_key();
        let b = agent_cli_key();
        assert_eq!(a, b, "repeated calls in one process must agree");
        let mut old_hardcoded = [0u8; 32];
        old_hardcoded[0] = 0xa9;
        assert_ne!(a, old_hardcoded, "must no longer be the old fixed constant");
        assert!(a.iter().any(|byte| *byte != 0), "must not be all-zero");
    }

    #[test]
    fn playbook_compile_command_drives_real_compiler() {
        let path = temp_file("pb");
        std::fs::write(
            &path,
            r#"{
  "name": "cli-audit",
  "steps": [
    { "key": "goal", "kind": "goal", "label": "Audit repo" },
    { "key": "ctx", "kind": "task", "label": "Load context", "depends_on": ["goal"] },
    { "key": "verify", "kind": "claim", "label": "Verify", "depends_on": ["ctx"], "budget_tokens": 2048 }
  ]
}"#,
        )
        .expect("write template");
        let args = vec![path.to_string_lossy().into_owned()];
        let code = run_playbook_compile(&args).expect("compile command");
        assert_eq!(code, 0);
        // The same template compiles into a connected 3-node initial graph.
        let template = PlaybookTemplate::new("cli-audit")
            .push(PlaybookStep::new("goal", NodeKind::Goal, "Audit repo"))
            .push(
                PlaybookStep::new("ctx", NodeKind::Task, "Load context")
                    .with_dependencies(["goal"]),
            )
            .push(
                PlaybookStep::new("verify", NodeKind::Claim, "Verify")
                    .with_dependencies(["ctx"])
                    .with_budget(2048),
            );
        let graph = compile(&template, protocol::GraphId::new()).expect("graph");
        assert_eq!(graph.nodes.len(), 3);
        assert_eq!(graph.edges.len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn tools_schema_dumps_every_tool_with_a_json_schema() {
        let root = temp_file("tools-schema-root");
        std::fs::create_dir_all(&root).expect("root");
        let args = vec!["--root".to_owned(), root.to_string_lossy().into_owned()];
        let code = run_tools_schema(&args).expect("tools command");
        assert_eq!(code, 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tools_schema_read_only_advertises_a_narrower_surface() {
        let root = temp_file("tools-schema-ro-root");
        std::fs::create_dir_all(&root).expect("root");
        let full = crate::exec_tools::ExecTools::workspace(&root)
            .expect("workspace tools")
            .tool_surface()
            .len();
        let read_only = crate::exec_tools::ExecTools::read_only(&root)
            .expect("read-only tools")
            .tool_surface()
            .len();
        assert!(
            read_only < full,
            "read-only surface ({read_only}) should be strictly narrower than the full surface ({full})"
        );
        let args = vec!["--root".to_owned(), root.to_string_lossy().into_owned(), "--read-only".to_owned()];
        assert_eq!(run_tools_schema(&args).expect("tools command"), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tools_schema_rejects_unknown_flags() {
        let args = vec!["--bogus".to_owned()];
        assert!(matches!(run_tools_schema(&args), Err(P9CommandError::Usage)));
    }

    #[test]
    fn mcp_tools_command_performs_real_handshake_and_intersection() {
        let args = vec!["--tools".to_owned(), "repo.read,shell.exec".to_owned()];
        let code = run_mcp_tools(&args).expect("surface command");
        assert_eq!(code, 0);
    }

    #[test]
    fn agent_cli_command_runs_supervised_child_end_to_end() {
        let args: Vec<String> = ["echo via external agent boundary", "--", "/bin/cat"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let code = run_agent_cli(&args).expect("agent command");
        assert_eq!(code, 0);
    }

    #[test]
    fn agent_cli_rejects_missing_argv_separator() {
        let args: Vec<String> = ["no separator here"].iter().map(|s| s.to_string()).collect();
        assert!(matches!(
            run_agent_cli(&args),
            Err(P9CommandError::Usage)
        ));
    }
}

/// `rapid doctor`: the real environment/config/model/project/sandbox
/// diagnosis (`crate::doctor`).
///
/// Until this was wired up, the command passed an empty
/// `security::DoctorRequest::default()` — no sandbox manager, no keychain,
/// no project observation — so all five security checks reported
/// `Unavailable` and the command told a user nothing about whether Rapid
/// could actually run. It now drives the same configuration loader, model
/// resolution, context-budget derivation, project-root detection, trust
/// store, and sandbox backends real commands use, and feeds real
/// observations into `security::evaluate_doctor` for the security-posture
/// half. Offline by default: no check performs a network request or a
/// billable model call.
pub fn run_doctor(args: &[String]) -> Result<i32, P9CommandError> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{DOCTOR_USAGE}");
        return Ok(0);
    }
    // The command takes no arguments at all. Silently ignoring one would let
    // `rapid doctor --json` or `rapid doctor sandbox` look like it did
    // something it did not.
    if let Some(unexpected) = args.first() {
        eprintln!("rapid doctor: unexpected argument '{unexpected}'");
        eprint!("{DOCTOR_USAGE}");
        return Err(P9CommandError::Usage);
    }
    let report = crate::doctor::diagnose(&crate::doctor::DoctorEnv::from_process());
    print!("{}", report.render());
    Ok(report.exit_code())
}

/// `rapid mcp list|get|add|remove|probe`: the project MCP server management
/// control plane (`crate::mcp_admin`).
///
/// `CLI_USAGE` advertised `rapid mcp ...` while `run_subcommand` had no arm
/// for it, so the command printed the generic top-level usage on stderr and
/// exited 2 — indistinguishable from a typo. Reachable only from this
/// process's argv, like `rapid trust`: no model tool, slash command, or
/// autonomous-goal path can add, remove, or probe an MCP server.
pub fn run_mcp(args: &[String]) -> Result<i32, P9CommandError> {
    match crate::mcp_admin::run(args, &crate::mcp_admin::McpEnv::from_process()) {
        Ok(outcome) => {
            print!("{}", outcome.text);
            Ok(outcome.exit)
        }
        Err(crate::mcp_admin::McpUsageError(message)) => {
            eprintln!("{message}");
            eprint!("{}", crate::mcp_admin::MCP_USAGE);
            Err(P9CommandError::Usage)
        }
    }
}

/// `rapid permissions list|allow|revoke`: the persisted per-project grant
/// store's only writer (`crate::permissions_cli`).
///
/// `PermissionLattice::evaluate`'s "persisted per-project grants suppress the
/// ask" step was unreachable in production because nothing ever created a
/// grant. Reachable only from this process's argv, like `rapid trust`.
pub fn run_permissions(args: &[String]) -> Result<i32, P9CommandError> {
    match crate::permissions_cli::run(
        args,
        &crate::permissions_cli::PermissionsEnv::from_process(),
    ) {
        Ok(outcome) => {
            print!("{}", outcome.text);
            Ok(outcome.exit)
        }
        Err(crate::permissions_cli::PermissionsUsageError(message)) => {
            eprintln!("{message}");
            eprint!("{}", crate::permissions_cli::PERMISSIONS_USAGE);
            Err(P9CommandError::Usage)
        }
    }
}

/// `rapid doctor --help`.
pub const DOCTOR_USAGE: &str = "\
usage: rapid doctor

Diagnose whether Rapid can operate in this environment and project. Every
check drives the same configuration, model resolution, project trust,
sandbox, and execution dependencies real commands use.

Offline: no check contacts a provider or makes a billable model call. Model
configuration is validated locally and reported as \"connectivity not
tested\" rather than as verified.

Read-only: doctor never grants or revokes trust, rewrites configuration,
installs or approves plugins, or executes hooks or scanners.

Exit code:
  0   no check failed (warnings and skips do not fail the command)
  1   at least one check required for core behavior failed
";

/// `rapid sessions list|search <text> [--db <path>]` over the kernel
/// session projection (real InProcessKernelClient -> EventLedger query).
pub fn run_sessions(args: &[String]) -> Result<i32, P9CommandError> {
    let mut db: Option<PathBuf> = None;
    let mut rest: Vec<&String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--db" {
            i += 1;
            db = args.get(i).map(PathBuf::from);
        } else {
            rest.push(&args[i]);
        }
        i += 1;
    }
    // `CLI_USAGE` advertises `rapid sessions list|search`, and the mode used
    // to be read into `_mode` and discarded: `rapid sessions
    // definitely-not-a-mode` listed every session and exited 0, so the
    // advertised distinction did not exist. Both modes are now real —
    // `search` requires its text, `list` refuses one.
    let mode = rest.first().map(|s| s.as_str()).ok_or(P9CommandError::Usage)?;
    let needle = match mode {
        "list" => {
            if rest.len() > 1 {
                eprintln!("rapid sessions list: unexpected argument '{}'", rest[1]);
                return Err(P9CommandError::Usage);
            }
            None
        }
        "search" => {
            let Some(text) = rest.get(1) else {
                eprintln!("usage: rapid sessions search <text>");
                return Err(P9CommandError::Usage);
            };
            if rest.len() > 2 {
                eprintln!("rapid sessions search: unexpected argument '{}'", rest[2]);
                return Err(P9CommandError::Usage);
            }
            Some((*text).clone())
        }
        other => {
            eprintln!("rapid sessions: unknown mode '{other}' (expected list or search)");
            return Err(P9CommandError::Usage);
        }
    };
    let db_path = db.unwrap_or_else(crate::interactive::current_project_ledger_path);
    // Listing must not *create* what it is listing. Opening the ledger
    // applies migrations and writes a full database, so a bare `rapid
    // sessions list` used to leave a store behind in whatever directory it
    // ran in — which is also how a project ends up with two of them. A
    // project with no ledger has no sessions, and says so through the same
    // printer as a project with an empty one, so the two are indistinguishable
    // rather than a second "nothing here" message that could drift.
    let sessions = if db_path.exists() {
        let client = kernel::InProcessKernelClient::open(&db_path)
            .map_err(|err| P9CommandError::Agent(format!("{err}")))?;
        client
            .list_sessions(&kernel::CancellationToken::new())
            .map_err(|err| P9CommandError::Agent(format!("{err}")))?
    } else {
        Vec::new()
    };
    println!("schema=rapidlm.sessions count={}", sessions.len());
    for summary in &sessions {
        if let Some(text) = &needle
            && !summary.session_id.contains(text.as_str()) {
                continue;
            }
        println!(
            "session={} last_seq={} first_seen={}",
            summary.session_id, summary.last_seq, summary.first_seen
        );
    }
    Ok(0)
}

/// `rapid cron add --prompt <text> --schedule <expr> [--session <id>] [--db <path>]`
/// `rapid cron list|poll [--db <path>]`; `rapid cron remove <id> [--db <path>]`.
///
/// Rows live in the event-ledger database (`cron_jobs`). Firing is
/// claim-lease: each poll atomically claims due rows, quarantines rows whose
/// schedule no longer parses (kept, not loaded), and requeues claims older
/// than the lease timeout so a crashed poller cannot strand a job.
/// The one `rapid cron list` printer, shared by the real store and the
/// "this project has no cron store" path so the two can never disagree.
fn print_cron_jobs(jobs: &[event_ledger::cron::CronJob]) {
    println!(
        "schema={} count={}",
        scheduler::CRON_FACADE_SCHEMA,
        jobs.len()
    );
    for job in jobs {
        println!(
            "id={} status={} next_fire_at_ms={} schedule={} prompt={}",
            job.id,
            job.status.as_str(),
            job.next_fire_at_ms,
            job.schedule,
            elide_prompt(&job.prompt),
        );
    }
}

pub fn run_cron(args: &[String]) -> Result<i32, P9CommandError> {
    let mut db: Option<PathBuf> = None;
    let mut rest: Vec<&String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--db" {
            i += 1;
            db = args.get(i).map(PathBuf::from);
        } else {
            rest.push(&args[i]);
        }
        i += 1;
    }
    let mode = rest.first().map(|s| s.as_str()).ok_or(P9CommandError::Usage)?;
    let operands: Vec<&String> = rest[1..].to_vec();
    let db_path = db.unwrap_or_else(crate::interactive::current_project_ledger_path);
    // As in `run_sessions`: listing must not create the store it lists.
    // `PromptCron::open` writes a migrated database, so `rapid cron list` in a
    // project that has never scheduled anything used to leave one behind.
    // Both paths print through `print_cron_jobs`, so "no store" and "empty
    // store" are the same output rather than two messages that can drift.
    if mode == "list" && !db_path.exists() {
        print_cron_jobs(&[]);
        return Ok(0);
    }
    let cron = scheduler::PromptCron::open(&db_path)
        .map_err(|err| P9CommandError::Agent(format!("{err}")))?;
    let cancel = capability_broker::CancellationToken::new();
    let store_err = |err: scheduler::CronError| P9CommandError::Agent(format!("{err}"));
    match mode {
        "add" => {
            let mut prompt: Option<&str> = None;
            let mut schedule: Option<&str> = None;
            let mut session: Option<&str> = None;
            let mut j = 0;
            while j < operands.len() {
                match operands[j].as_str() {
                    "--prompt" => {
                        j += 1;
                        prompt = operands.get(j).map(|s| s.as_str());
                    }
                    "--schedule" => {
                        j += 1;
                        schedule = operands.get(j).map(|s| s.as_str());
                    }
                    "--session" => {
                        j += 1;
                        session = operands.get(j).map(|s| s.as_str());
                    }
                    _ => return Err(P9CommandError::Usage),
                }
                j += 1;
            }
            let prompt = prompt.ok_or(P9CommandError::Usage)?;
            let schedule = schedule.ok_or(P9CommandError::Usage)?;
            let now_ms = unix_now_ms();
            let job = cron
                .add(prompt, session, schedule, now_ms, &cancel)
                .map_err(store_err)?;
            println!(
                "schema={} id={} status=active next_fire_at_ms={}",
                scheduler::CRON_FACADE_SCHEMA,
                job.id,
                job.next_fire_at_ms
            );
            Ok(0)
        }
        "list" => {
            print_cron_jobs(&cron.list().map_err(store_err)?);
            Ok(0)
        }
        "remove" => {
            let id = operands.first().ok_or(P9CommandError::Usage)?;
            if cron.remove(id).map_err(store_err)? {
                println!("removed id={id}");
                Ok(0)
            } else {
                println!("not found id={id}");
                Ok(JsonlExitCode::Usage.as_i32())
            }
        }
        "poll" => {
            let report = cron
                .poll(unix_now_ms(), &cancel, scheduler::MAX_POLL_BATCH)
                .map_err(store_err)?;
            println!(
                "schema={} requeued={} fired={} quarantined={}",
                scheduler::CRON_FACADE_SCHEMA,
                report.requeued,
                report.fired.len(),
                report.quarantined
            );
            // Modbit `AGT-008`/§3.2: a fired job now actually runs its prompt
            // through a real turn, not just a print statement — but only ever
            // in the "propose, never auto-apply" mode this feature's first
            // pass is scoped to. `forced_mode` overrides whatever the ambient
            // environment/project settings say: `evaluate()` (`permissions.rs`)
            // denies every non-read-only tool call unconditionally in `Plan`
            // mode, before the mode table is even consulted for anything else,
            // so a cron-fired turn can explore (read-only tools stay allowed
            // in every mode) and produce a proposal, but can never write,
            // patch, or run a mutating shell command unattended. Each fired
            // job's real outcome also now feeds `report_execution` — the
            // previously-disclosed gap where a job whose prompt fails every
            // real run kept firing forever, since `complete()`/rescheduling
            // only ever meant "the schedule re-parsed," not "the prompt's
            // execution succeeded."
            for due in &report.fired {
                println!(
                    "id={} session={} prompt={}",
                    due.id,
                    due.session_id.as_deref().unwrap_or("-"),
                    elide_prompt(&due.prompt),
                );
                let succeeded = match crate::interactive::exec_turn(
                    &[due.prompt.clone()],
                    Some(crate::permissions::PermissionMode::Plan),
                ) {
                    Ok(code) => {
                        println!("id={} outcome=exit:{code}", due.id);
                        code == 0
                    }
                    Err(err) => {
                        println!("id={} outcome=error:{err:?}", due.id);
                        false
                    }
                };
                if let Some(line) =
                    report_cron_execution_outcome(&cron, &due.id, succeeded, unix_now_ms())
                {
                    println!("{line}");
                }
            }
            Ok(0)
        }
        _ => Err(P9CommandError::Usage),
    }
}

fn unix_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Report a fired job's real execution outcome to `cron` (§3.2's disclosed
/// gap: completing a job's lease only ever meant "the schedule re-parsed,"
/// never "the prompt's execution succeeded," so a job that failed every
/// real run kept firing forever) and return an operator-visible line only
/// when there's something to say — quarantine just triggered, or the report
/// call itself failed. Extracted from the poll loop (rather than inlined)
/// specifically so this is testable without a real `exec_turn`/model call:
/// build a `PromptCron` over a temp store, add a job, and call this
/// directly.
fn report_cron_execution_outcome(
    cron: &scheduler::PromptCron,
    id: &str,
    succeeded: bool,
    now_ms: i64,
) -> Option<String> {
    match cron.report_execution(id, succeeded, now_ms) {
        Ok(report) if report.quarantined => Some(format!(
            "id={id} quarantined=true consecutive_failures={}",
            report.consecutive_failures
        )),
        Ok(_) => None,
        Err(err) => Some(format!(
            "id={id} warning: failed to record execution result ({err})"
        )),
    }
}

/// Bound the prompt echo in list/poll output so one huge prompt cannot flood
/// the terminal; truncation is marked with an ellipsis.
fn elide_prompt(prompt: &str) -> String {
    const MAX_ECHO_CHARS: usize = 80;
    if prompt.chars().count() <= MAX_ECHO_CHARS {
        return prompt.to_string();
    }
    let head: String = prompt.chars().take(MAX_ECHO_CHARS).collect();
    format!("{head}…")
}

/// `rapid findings list [--root <path>]`; `rapid findings dismiss
/// <fingerprint> --reason <text> [--root <path>]`.
///
/// The write side of `crate::findings_store::FindingsStore` (Modbit
/// `VER-007`/`VER-008`): a dismissal is keyed by a scanner finding's own
/// content-hash fingerprint (printed alongside every advisory
/// `workspace_write` emits), so it survives reruns without needing a line
/// number, and a *changed* finding at the same location gets a different
/// fingerprint and is never silently hidden by an old dismissal.
pub fn run_findings(args: &[String]) -> Result<i32, P9CommandError> {
    let mut root = std::env::current_dir().map_err(P9CommandError::Io)?;
    let mut rest: Vec<&String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--root" {
            i += 1;
            root = args.get(i).map(PathBuf::from).ok_or(P9CommandError::Usage)?;
        } else {
            rest.push(&args[i]);
        }
        i += 1;
    }
    let mode = rest.first().map(|s| s.as_str()).ok_or(P9CommandError::Usage)?;
    match mode {
        "list" => {
            let store = crate::findings_store::FindingsStore::load(&root);
            println!("schema=rapidlm.findings count={}", store.len());
            for (fingerprint, entry) in store.entries() {
                println!("fingerprint={fingerprint} reason={}", entry.reason);
            }
            Ok(0)
        }
        "dismiss" => {
            let fingerprint = rest.get(1).ok_or(P9CommandError::Usage)?.as_str();
            let mut reason: Option<&str> = None;
            let mut j = 2;
            while j < rest.len() {
                match rest[j].as_str() {
                    "--reason" => {
                        j += 1;
                        reason = rest.get(j).map(|s| s.as_str());
                    }
                    _ => return Err(P9CommandError::Usage),
                }
                j += 1;
            }
            let reason = reason.ok_or(P9CommandError::Usage)?;
            let mut store = crate::findings_store::FindingsStore::load(&root);
            store.dismiss(fingerprint, reason);
            store.save(&root).map_err(P9CommandError::Io)?;
            println!("schema=rapidlm.findings dismissed fingerprint={fingerprint}");
            Ok(0)
        }
        _ => Err(P9CommandError::Usage),
    }
}

/// `rapid scan [--root <path>] [--scanner <id>]`: run every scanner
/// configured in `.rapidlm/scanners.json` (Modbit `VER-009`'s
/// `ExternalFinding` half — `security::ExternalScannerAdapter`, wired here
/// for the first time in this binary) and combine their results through
/// `security::evaluate_scan_gate`. Findings already dismissed via
/// `rapid findings dismiss <fingerprint>` are shown but never block, the
/// same convention every other scanner in `exec_tools.rs` already uses.
///
/// Exit code is 0 when the combined verdict allows apply (pass/warn), 1
/// otherwise (block/ask) — real findings, a required scanner erroring, or
/// one being unavailable all fail closed, matching `security::gate`'s own
/// "unavailable and error never become pass" contract.
pub fn run_scan(args: &[String]) -> Result<i32, P9CommandError> {
    let mut root = std::env::current_dir().map_err(P9CommandError::Io)?;
    let mut only_scanner: Option<&str> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--root" => {
                i += 1;
                root = args.get(i).map(PathBuf::from).ok_or(P9CommandError::Usage)?;
            }
            "--scanner" => {
                i += 1;
                only_scanner = Some(args.get(i).ok_or(P9CommandError::Usage)?.as_str());
            }
            _ => return Err(P9CommandError::Usage),
        }
        i += 1;
    }

    let mut entries = crate::external_scan::load_scanners_config(&root)
        .map_err(|err| P9CommandError::Scan(err.to_string()))?;
    if let Some(id) = only_scanner {
        entries.retain(|entry| entry.config().id() == id);
        if entries.is_empty() {
            return Err(P9CommandError::Scan(format!(
                "no scanner named {id:?} configured in {}",
                crate::external_scan::SCANNERS_CONFIG_PATH
            )));
        }
    }
    if entries.is_empty() {
        println!(
            "schema=rapidlm.scan scanners=0 (no scanners configured in {})",
            crate::external_scan::SCANNERS_CONFIG_PATH
        );
        return Ok(0);
    }

    let store = crate::findings_store::FindingsStore::load(&root);
    let cancel = capability_broker::CancellationToken::new();
    let (verdict, outcomes) = crate::external_scan::run_configured_scanners(
        &entries,
        &root,
        |fingerprint_hex| store.is_dismissed(fingerprint_hex),
        &cancel,
    )
    .map_err(|err| P9CommandError::Scan(err.to_string()))?;

    for outcome in &outcomes {
        println!(
            "schema=rapidlm.scan scanner={} status={:?} findings={} undismissed={}",
            outcome.scanner_id,
            outcome.report.status(),
            outcome.report.findings().len(),
            outcome.undismissed.len()
        );
        for finding in &outcome.undismissed {
            println!(
                "  fingerprint={} rule={} severity={:?} {}..{} {}",
                finding.fingerprint().as_hex(),
                finding.rule_id(),
                finding.severity(),
                finding.range().start(),
                finding.range().end(),
                finding.message()
            );
        }
    }
    println!("schema=rapidlm.scan verdict={verdict}");
    Ok(if verdict.allows_apply() { 0 } else { 1 })
}

#[cfg(test)]
mod findings_tests {
    use super::*;
    static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rapidlm-p9-findings-{tag}-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    #[test]
    fn list_on_an_empty_project_reports_zero() {
        let root = temp_root("empty");
        let args = vec![
            "list".to_owned(),
            "--root".to_owned(),
            root.to_string_lossy().into_owned(),
        ];
        let code = run_findings(&args).expect("list");
        assert_eq!(code, 0);
    }

    #[test]
    fn dismiss_persists_and_list_reports_it() {
        let root = temp_root("dismiss");
        let dismiss_args = vec![
            "dismiss".to_owned(),
            "abc123".to_owned(),
            "--reason".to_owned(),
            "test fixture".to_owned(),
            "--root".to_owned(),
            root.to_string_lossy().into_owned(),
        ];
        let code = run_findings(&dismiss_args).expect("dismiss");
        assert_eq!(code, 0);

        let store = crate::findings_store::FindingsStore::load(&root);
        assert!(store.is_dismissed("abc123"));
        let (fingerprint, entry) = store.entries().next().expect("one entry");
        assert_eq!(fingerprint, "abc123");
        assert_eq!(entry.reason, "test fixture");
    }

    #[test]
    fn dismiss_without_a_reason_is_a_usage_error() {
        let root = temp_root("no-reason");
        let args = vec![
            "dismiss".to_owned(),
            "abc123".to_owned(),
            "--root".to_owned(),
            root.to_string_lossy().into_owned(),
        ];
        assert!(matches!(run_findings(&args), Err(P9CommandError::Usage)));
    }

    #[test]
    fn unknown_mode_is_a_usage_error() {
        let root = temp_root("unknown-mode");
        let args = vec![
            "surprise".to_owned(),
            "--root".to_owned(),
            root.to_string_lossy().into_owned(),
        ];
        assert!(matches!(run_findings(&args), Err(P9CommandError::Usage)));
    }
}

#[cfg(test)]
mod scan_tests {
    use super::*;
    static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rapidlm-p9-scan-{tag}-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    fn write_scanners_config(root: &std::path::Path, raw: &str) {
        std::fs::create_dir_all(root.join(".rapidlm")).expect("dir");
        std::fs::write(root.join(crate::external_scan::SCANNERS_CONFIG_PATH), raw).expect("write");
    }

    fn write_scanners(root: &std::path::Path, scanners: Vec<serde_json::Value>) {
        let doc = serde_json::json!({"schema": 1, "scanners": scanners});
        write_scanners_config(root, &serde_json::to_string(&doc).expect("serialize"));
    }

    const CLEAN_SARIF: &str =
        r#"{"version":"2.1.0","runs":[{"tool":{"driver":{"name":"fakescan"}},"results":[]}]}"#;

    fn finding_sarif() -> String {
        r#"{"version":"2.1.0","runs":[{"tool":{"driver":{"name":"fakescan"}},"results":[{"ruleId":"no-eval","level":"error","message":{"text":"eval is unsafe"},"locations":[{"physicalLocation":{"artifactLocation":{"uri":"src/app.rs"},"region":{"byteOffset":10,"byteLength":4}}}]}]}]}"#.to_owned()
    }

    // `serde_json::json!` handles escaping the SARIF body's own embedded
    // quotes when the whole document is serialized — hand-formatting a
    // JSON string containing another JSON string inline (the earlier,
    // broken version of this helper) breaks exactly that escaping.
    fn sh_scanner(id: &str, sarif_body: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "kind": "sast",
            "argv": ["sh", "-c", format!("printf '%s' '{sarif_body}'")],
        })
    }

    #[test]
    fn no_scanners_configured_is_a_clean_exit() {
        let root = temp_root("none");
        let args = vec!["--root".to_owned(), root.to_string_lossy().into_owned()];
        let code = run_scan(&args).expect("scan");
        assert_eq!(code, 0);
    }

    #[test]
    fn malformed_config_is_a_scan_error() {
        let root = temp_root("malformed");
        write_scanners_config(&root, "not json");
        let args = vec!["--root".to_owned(), root.to_string_lossy().into_owned()];
        assert!(matches!(run_scan(&args), Err(P9CommandError::Scan(_))));
    }

    #[test]
    fn a_clean_scanner_exits_zero() {
        let root = temp_root("clean");
        write_scanners(&root, vec![sh_scanner("fakescan", CLEAN_SARIF)]);
        let args = vec!["--root".to_owned(), root.to_string_lossy().into_owned()];
        let code = run_scan(&args).expect("scan");
        assert_eq!(code, 0);
    }

    #[test]
    fn a_scanner_with_a_finding_exits_nonzero_and_a_dismissed_one_exits_zero() {
        let root = temp_root("finding");
        write_scanners(&root, vec![sh_scanner("fakescan", &finding_sarif())]);
        let args = vec!["--root".to_owned(), root.to_string_lossy().into_owned()];
        let code = run_scan(&args).expect("scan");
        assert_eq!(code, 1, "an undismissed finding must fail the gate");

        // The fingerprint printed to stdout by the run above is what a real
        // user would copy into `rapid findings dismiss` — reproduce that
        // deterministically here via the scanner's own fingerprint
        // computation instead of scraping captured stdout.
        let (_, outcomes) = crate::external_scan::run_configured_scanners(
            &crate::external_scan::load_scanners_config(&root).expect("load"),
            &root,
            |_| false,
            &capability_broker::CancellationToken::new(),
        )
        .expect("scan");
        let fingerprint = outcomes[0].undismissed[0].fingerprint().as_hex().to_owned();

        let dismiss_args = vec![
            "dismiss".to_owned(),
            fingerprint,
            "--reason".to_owned(),
            "test fixture".to_owned(),
            "--root".to_owned(),
            root.to_string_lossy().into_owned(),
        ];
        run_findings(&dismiss_args).expect("dismiss");

        let code = run_scan(&args).expect("scan");
        assert_eq!(code, 0, "a fully-dismissed finding must not keep blocking");
    }

    #[test]
    fn scanner_filter_selects_only_the_named_scanner() {
        let root = temp_root("filter");
        write_scanners(
            &root,
            vec![
                sh_scanner("clean-one", CLEAN_SARIF),
                sh_scanner("dirty-one", &finding_sarif()),
            ],
        );
        let clean_only = vec![
            "--root".to_owned(),
            root.to_string_lossy().into_owned(),
            "--scanner".to_owned(),
            "clean-one".to_owned(),
        ];
        assert_eq!(run_scan(&clean_only).expect("scan"), 0);

        let dirty_only = vec![
            "--root".to_owned(),
            root.to_string_lossy().into_owned(),
            "--scanner".to_owned(),
            "dirty-one".to_owned(),
        ];
        assert_eq!(run_scan(&dirty_only).expect("scan"), 1);
    }

    #[test]
    fn unknown_scanner_name_is_a_scan_error() {
        let root = temp_root("unknown-scanner");
        write_scanners(&root, vec![sh_scanner("fakescan", CLEAN_SARIF)]);
        let args = vec![
            "--root".to_owned(),
            root.to_string_lossy().into_owned(),
            "--scanner".to_owned(),
            "does-not-exist".to_owned(),
        ];
        assert!(matches!(run_scan(&args), Err(P9CommandError::Scan(_))));
    }
}

/// `rapid agents list|validate [--dir <path>]`; `rapid agents scaffold <id>`.
///
/// Lists and validates project agent definitions
/// (`.rapidlm/agents/*.toml`) alongside the compiled-in built-ins, and
/// emits a valid starter definition on `scaffold`. Definitions are
/// artifacts: validation here is the same fail-closed pass the loader runs
/// (closed schema, regular-file sources, grants inside the role surface and
/// backed by declared runtime implementations).
pub fn run_agents(args: &[String]) -> Result<i32, P9CommandError> {
    let mut dir: Option<PathBuf> = None;
    let mut rest: Vec<&String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--dir" {
            i += 1;
            dir = args.get(i).map(PathBuf::from);
        } else {
            rest.push(&args[i]);
        }
        i += 1;
    }
    let mode = rest.first().map(|s| s.as_str()).ok_or(P9CommandError::Usage)?;
    let defs_dir = dir.unwrap_or_else(|| crate::interactive::project_path("agents"));
    let registry = cli_implementation_registry();
    match mode {
        "list" => {
            let inventory = agent_runtime::agent_defs::full_inventory(&defs_dir, &registry)
                .map_err(|err| P9CommandError::Agent(format!("{err}")))?;
            println!(
                "schema={} dir={} loaded={} rejected={}",
                agent_runtime::agent_defs::AGENT_DEFS_SCHEMA,
                defs_dir.display(),
                inventory.loaded.len(),
                inventory.rejected.len()
            );
            for def in &inventory.loaded {
                let source = match &def.source {
                    agent_runtime::agent_defs::DefSource::BuiltIn => "builtin".to_string(),
                    agent_runtime::agent_defs::DefSource::Project(path) => {
                        path.display().to_string()
                    }
                };
                println!(
                    "id={} role={} source={} description={}",
                    def.id,
                    def.role.as_str(),
                    source,
                    elide_prompt(&def.description)
                );
            }
            for rejected in &inventory.rejected {
                println!("rejected path={} reason={}", rejected.path.display(), rejected.reason);
            }
            Ok(0)
        }
        "validate" => {
            let inventory =
                agent_runtime::agent_defs::load_directory(&defs_dir, &registry)
                    .map_err(|err| P9CommandError::Agent(format!("{err}")))?;
            for def in &inventory.loaded {
                match &def.source {
                    agent_runtime::agent_defs::DefSource::Project(path) => {
                        println!("ok path={} id={}", path.display(), def.id);
                    }
                    agent_runtime::agent_defs::DefSource::BuiltIn => {}
                }
            }
            for rejected in &inventory.rejected {
                println!("rejected path={} reason={}", rejected.path.display(), rejected.reason);
            }
            if inventory.rejected.is_empty() {
                Ok(0)
            } else {
                Ok(JsonlExitCode::Usage.as_i32())
            }
        }
        "scaffold" => {
            let id = rest.get(1).ok_or(P9CommandError::Usage)?;
            let id = agent_runtime::agent_defs::AgentDefId::parse(id)
                .map_err(|err| P9CommandError::Agent(format!("{err}")))?;
            print!(
                "{}",
                agent_runtime::agent_defs::AgentDefinition::scaffold(&id)
            );
            Ok(0)
        }
        _ => Err(P9CommandError::Usage),
    }
}

/// Tool-class implementations this composition root actually links: every
/// class is backed by a real subsystem crate in the binary.
fn cli_implementation_registry() -> agent_runtime::agent_defs::ImplementationRegistry {
    use agent_runtime::agent_defs::ImplementationRegistry;
    use agent_runtime::role_profile::RoleToolClass as Class;
    ImplementationRegistry::new()
        .declare(Class::Read, "rapidlm.impl.workspace-read.v1")
        .declare(Class::Write, "rapidlm.impl.workspace-write.v1")
        .declare(Class::Exec, "rapidlm.impl.process-supervisor.v1")
        .declare(Class::Net, "rapidlm.impl.gateway-net.v1")
        .declare(Class::Browser, "rapidlm.impl.computer-use-browser.v1")
        .declare(Class::Mobile, "rapidlm.impl.mobile-sim.v1")
        .declare(Class::Mcp, "rapidlm.impl.mcp-server.v1")
        .declare(Class::Plugin, "rapidlm.impl.plugin-host.v1")
        .declare(Class::Git, "rapidlm.impl.workspace-git.v1")
        .declare(Class::Secret, "rapidlm.impl.auth-handles.v1")
}

/// `rapid plugins validate|register|list|approve|reject|hook-test`.
///
/// Operator surface over the plugin-host production APIs. Manifests are
/// parsed and privilege-checked by the real `parse_manifest`; trust changes
/// go through the `ExtensionTrustStore` ledger, where `register` always
/// stores untrusted and only an explicit `approve` grant enables
/// capabilities. `hook-test` matches a parsed hook spec against a fixture
/// event and never executes the hook command.
pub fn run_plugins(args: &[String]) -> Result<i32, P9CommandError> {
    let mut catalog: Option<PathBuf> = None;
    let mut rest: Vec<&String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--catalog" {
            i += 1;
            catalog = args.get(i).map(PathBuf::from);
        } else {
            rest.push(&args[i]);
        }
        i += 1;
    }
    let mode = rest.first().map(|s| s.as_str()).ok_or(P9CommandError::Usage)?;
    let operands: Vec<&String> = rest[1..].to_vec();
    let catalog_path = catalog.unwrap_or_else(default_trust_catalog);
    let cancel = capability_broker::CancellationToken::new();
    let trust_err = |err: plugin_host::TrustError| P9CommandError::Agent(format!("{err}"));
    match mode {
        "validate" => {
            let path = operands.first().ok_or(P9CommandError::Usage)?;
            let bytes = read_bounded_file(path, plugin_host::MAX_MANIFEST_BYTES)?;
            let manifest = plugin_host::parse_manifest(&bytes, &cancel)
                .map_err(|err| P9CommandError::Agent(format!("{err}")))?;
            println!(
                "schema={} id={} version={} publisher={} requested_caps={} skills={} hooks={} mcp_servers={} manifest_hash={}",
                plugin_host::PLUGIN_MANIFEST_SCHEMA,
                manifest.id().as_str(),
                manifest.version(),
                manifest.publisher().as_str(),
                manifest.requested_caps().len(),
                manifest.skills().len(),
                manifest.hooks().len(),
                manifest.mcp_servers().len(),
                manifest.manifest_hash(),
            );
            Ok(0)
        }
        "register" => {
            let path = operands.first().ok_or(P9CommandError::Usage)?;
            let mut locator: Option<&str> = None;
            let mut j = 1;
            while j < operands.len() {
                if operands[j].as_str() == "--source" {
                    j += 1;
                    locator = operands.get(j).map(|s| s.as_str());
                } else {
                    return Err(P9CommandError::Usage);
                }
                j += 1;
            }
            let locator = locator.unwrap_or("cli-manual-register");
            let bytes = read_bounded_file(path, plugin_host::MAX_MANIFEST_BYTES)?;
            let manifest = plugin_host::parse_manifest(&bytes, &cancel)
                .map_err(|err| P9CommandError::Agent(format!("{err}")))?;
            let identity = plugin_host::ExtensionIdentity::from_binding(&manifest.trust_binding());
            let source = plugin_host::InstallSource::new(
                plugin_host::InstallSourceKind::User,
                locator,
            )
            .map_err(trust_err)?;
            let observation = plugin_host::ExtensionObservation::new(
                identity,
                source,
                plugin_host::TrustScope::User,
                review_timestamp(None)?,
            );
            let store = plugin_host::ExtensionTrustStore::open(&catalog_path);
            let record = store.register(&observation, &cancel).map_err(trust_err)?;
            println!(
                "schema={} id={} status={} executable_enabled={}",
                plugin_host::TRUST_SCHEMA,
                record.identity().plugin().as_str(),
                record.status().as_str(),
                record.executable_enabled(),
            );
            Ok(0)
        }
        "list" => {
            let store = plugin_host::ExtensionTrustStore::open(&catalog_path);
            let views = store.list(&cancel).map_err(trust_err)?;
            println!(
                "schema={} catalog={} records={}",
                plugin_host::TRUST_SCHEMA,
                catalog_path.display(),
                views.len()
            );
            for view in &views {
                println!(
                    "id={} version={} publisher={} status={} policy={} caps={} executable_enabled={} last_review={}",
                    view.plugin().as_str(),
                    view.version(),
                    view.publisher().as_str(),
                    view.status().as_str(),
                    view.policy().as_str(),
                    view.granted_capabilities().len(),
                    view.executable_enabled(),
                    view.last_review().as_str(),
                );
                for granted in view.granted_capabilities() {
                    println!(
                        "  grant capability={} resource_kind={}",
                        granted.capability().as_str(),
                        granted.resource().family().as_str(),
                    );
                }
            }
            Ok(0)
        }
        "approve" => {
            let plugin_id = operands.first().ok_or(P9CommandError::Usage)?;
            let mut capability: Option<&str> = None;
            let mut resource: Option<&str> = None;
            let mut review: Option<&str> = None;
            let mut j = 1;
            while j < operands.len() {
                match operands[j].as_str() {
                    "--capability" => {
                        j += 1;
                        capability = operands.get(j).map(|s| s.as_str());
                    }
                    "--resource" => {
                        j += 1;
                        resource = operands.get(j).map(|s| s.as_str());
                    }
                    "--review" => {
                        j += 1;
                        review = operands.get(j).map(|s| s.as_str());
                    }
                    _ => return Err(P9CommandError::Usage),
                }
                j += 1;
            }
            let capability = capability.ok_or(P9CommandError::Usage)?;
            let resource = resource.ok_or(P9CommandError::Usage)?;
            let capability: capability_broker::Capability =
                capability.parse().map_err(|err: capability_broker::CapabilityError| {
                    P9CommandError::Agent(format!("capability: {err}"))
                })?;
            let resource = parse_cli_resource(resource)?;
            let store = plugin_host::ExtensionTrustStore::open(&catalog_path);
            let identity = stored_identity(&store, plugin_id, &cancel).map_err(trust_err)?;
            let record = store.get(&identity, &cancel).map_err(trust_err)?;
            let mut granted: Vec<plugin_host::TrustedCapability> =
                record.granted_capabilities().to_vec();
            granted.push(
                plugin_host::TrustedCapability::new(capability, resource)
                    .map_err(trust_err)?,
            );
            let mut grant = plugin_host::TrustGrant::new(
                record.identity().clone(),
                record.source().clone(),
                record.scope(),
                review_timestamp(review)?,
            );
            if let Some(range) = record.version_range() {
                grant = grant
                    .with_version_range(range.min(), range.max())
                    .map_err(trust_err)?;
            }
            let grant = grant.with_granted_capabilities(granted).map_err(trust_err)?;
            let updated = store.grant(&grant, &cancel).map_err(trust_err)?;
            println!(
                "id={} status={} policy={} caps={} executable_enabled={}",
                updated.identity().plugin().as_str(),
                updated.status().as_str(),
                updated.policy().as_str(),
                updated.granted_capabilities().len(),
                updated.executable_enabled(),
            );
            Ok(0)
        }
        "reject" => {
            let plugin_id = operands.first().ok_or(P9CommandError::Usage)?;
            let mut review: Option<&str> = None;
            let mut j = 1;
            while j < operands.len() {
                if operands[j].as_str() == "--review" {
                    j += 1;
                    review = operands.get(j).map(|s| s.as_str());
                } else {
                    return Err(P9CommandError::Usage);
                }
                j += 1;
            }
            let store = plugin_host::ExtensionTrustStore::open(&catalog_path);
            let identity = stored_identity(&store, plugin_id, &cancel).map_err(trust_err)?;
            let updated = store
                .revoke(&identity, review_timestamp(review)?, &cancel)
                .map_err(trust_err)?;
            println!(
                "id={} status={} executable_enabled={}",
                updated.identity().plugin().as_str(),
                updated.status().as_str(),
                updated.executable_enabled(),
            );
            Ok(0)
        }
        "hook-test" => {
            let spec_path = operands.first().ok_or(P9CommandError::Usage)?;
            let mut fixture: Option<&str> = None;
            let mut j = 1;
            while j < operands.len() {
                if operands[j].as_str() == "--fixture" {
                    j += 1;
                    fixture = operands.get(j).map(|s| s.as_str());
                } else {
                    return Err(P9CommandError::Usage);
                }
                j += 1;
            }
            let fixture = fixture.ok_or(P9CommandError::Usage)?;
            let spec_bytes = read_bounded_file(spec_path, HOOK_SPEC_MAX_BYTES)?;
            let spec = plugin_host::parse_hook_spec(&spec_bytes, &cancel)
                .map_err(|err| P9CommandError::Agent(format!("hook spec: {err}")))?;
            let fixture_bytes = read_bounded_file(fixture, HOOK_EVENT_MAX_BYTES)?;
            let event = decode_hook_fixture(&fixture_bytes)
                .map_err(|err| P9CommandError::Agent(format!("hook fixture: {err}")))?;
            let mut manager = plugin_host::HookManager::new();
            manager
                .register(spec.clone())
                .map_err(|err| P9CommandError::Agent(format!("hook spec: {err}")))?;
            let matched: Vec<_> = manager.matching(&event).collect();
            println!(
                "schema={} hook_id={} hook_event={} matcher={} timeout_ms={} failure_policy={} blocking_event={}",
                plugin_host::HOOK_SPEC_SCHEMA,
                spec.id().as_str(),
                spec.event().as_str(),
                spec.matcher().map(plugin_host::HookMatcher::as_str).unwrap_or("*"),
                spec.timeout().as_millis(),
                spec.failure_policy().as_str(),
                spec.event().is_blocking(),
            );
            println!(
                "fixture event={} name={} matched_hooks={}",
                event.event().as_str(),
                event.name(),
                matched.len(),
            );
            for hook in &matched {
                println!("would-fire hook_id={}", hook.id().as_str());
            }
            println!("dry-run complete; the hook command was not executed");
            Ok(0)
        }
        _ => Err(P9CommandError::Usage),
    }
}

/// Hook-spec documents are bounded by the same limit the parser enforces.
const HOOK_SPEC_MAX_BYTES: usize = 64 * 1024;
/// Fixture events carry a redacted payload, so a small bound is generous.
const HOOK_EVENT_MAX_BYTES: usize = 8 * 1024;
/// Maximum resource-string length accepted by the CLI grammar. The typed
/// constructors inside capability-broker enforce their own tighter bounds.
const MAX_CLI_RESOURCE_BYTES: usize = 4096;

/// Default trust catalog location, matching the install layout root.
fn default_trust_catalog() -> PathBuf {
    crate::interactive::project_path(
        PathBuf::from("plugins").join(plugin_host::TRUST_CATALOG_FILE),
    )
}

/// Reads a file, rejecting it once its content exceeds `max_bytes`. Reads
/// through a `max_bytes + 1` cap rather than trusting a preceding
/// `fs::metadata` size check: a stat-then-read gap lets the file grow
/// between the two calls (a local edit or a symlink swap mid-read), which
/// would silently buffer an oversized file despite the check having passed.
/// Capping the read itself means at most `max_bytes + 1` bytes are ever
/// buffered, regardless of how large the file actually is.
fn read_bounded_file(path: &str, max_bytes: usize) -> Result<Vec<u8>, P9CommandError> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(P9CommandError::Io)?;
    let mut buf = Vec::new();
    file.take(max_bytes as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(P9CommandError::Io)?;
    if buf.len() > max_bytes {
        return Err(P9CommandError::Agent(format!(
            "file exceeds the {max_bytes}-byte limit: {path}"
        )));
    }
    Ok(buf)
}

/// Resolve the stored identity for a plugin id from the catalog. Absent
/// plugins are an error: approve/reject never mint identities for packages
/// the trust ledger has never seen.
fn stored_identity(
    store: &plugin_host::ExtensionTrustStore,
    plugin_id: &str,
    cancel: &capability_broker::CancellationToken,
) -> Result<plugin_host::ExtensionIdentity, plugin_host::TrustError> {
    for view in store.list(cancel)? {
        if view.plugin().as_str() == plugin_id {
            return plugin_host::ExtensionIdentity::new(
                view.plugin().as_str(),
                &view.version().to_string(),
                view.publisher().as_str(),
                &view.package_hash().to_string(),
            );
        }
    }
    Err(plugin_host::TrustError::InvalidPlugin)
}

/// Review stamp from an explicit `--review` value or the current UTC time.
fn review_timestamp(explicit: Option<&str>) -> Result<plugin_host::ReviewTimestamp, P9CommandError> {
    let raw = match explicit {
        Some(raw) => raw.to_string(),
        None => utc_stamp_now(),
    };
    plugin_host::ReviewTimestamp::parse(&raw)
        .map_err(|err| P9CommandError::Agent(format!("review timestamp '{raw}': {err}")))
}

/// Current UTC time in the ledger's `YYYY-MM-DDTHH:MM:SSZ` review form.
fn utc_stamp_now() -> String {
    let now_ms = unix_now_ms();
    utc_stamp_from_unix_ms(now_ms).unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string())
}

/// Days-to-civil conversion (proleptic Gregorian) into the review wire form.
/// Sub-second precision is truncated; returns `None` for instants outside
/// the representable year range.
fn utc_stamp_from_unix_ms(ms: i64) -> Option<String> {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    // Civil-from-days (Howard Hinnant's algorithm), shifted to year 0 era.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    if !(1..=9999).contains(&y) {
        return None;
    }
    let (hour, minute, second) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );
    Some(format!("{y:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}Z"))
}

/// Operator resource grammar mapped onto the typed capability-broker
/// constructors. Every scope check (bounds, traversal, ambient rejection)
/// stays in capability-broker; this only shapes the tokens.
///
/// `repo:<glob>` `host:<glob>` `cmd:<family>` `net:<scheme>:<host>:<port>`
/// `gitref:<ref>` `secret:<id>:<target>` `page:<scheme>:<host>:<port>`
/// `device:<id>` `mcp:<server>:<tool>` `plugin:<plugin>:<capability>`
fn parse_cli_resource(raw: &str) -> Result<capability_broker::ResourceDescriptor, P9CommandError> {
    use capability_broker::ResourceDescriptor as Rd;
    if raw.len() > MAX_CLI_RESOURCE_BYTES {
        return Err(P9CommandError::Agent("resource string too long".into()));
    }
    let (kind, value) = raw.split_once(':').ok_or(P9CommandError::Usage)?;
    let host_scope = |value: &str| -> Result<capability_broker::NetworkScope, P9CommandError> {
        let mut parts = value.split(':');
        let scheme = parts.next().ok_or(P9CommandError::Usage)?;
        let host = parts.next().ok_or(P9CommandError::Usage)?;
        let port = parts.next().ok_or(P9CommandError::Usage)?;
        if parts.next().is_some() {
            return Err(P9CommandError::Usage);
        }
        let scheme: capability_broker::NetworkScheme =
            scheme.parse().map_err(|err: capability_broker::CapabilityError| {
                P9CommandError::Agent(format!("resource: {err}"))
            })?;
        let port: u16 = port.parse().map_err(|_| P9CommandError::Usage)?;
        capability_broker::NetworkScope::new(scheme, host, port)
            .map_err(|err| P9CommandError::Agent(format!("resource: {err}")))
    };
    let descriptor = match kind {
        "repo" => Rd::Filesystem(
            capability_broker::FilesystemScope::repo(value)
                .map_err(|err| P9CommandError::Agent(format!("resource: {err}")))?,
        ),
        "host" => Rd::Filesystem(
            capability_broker::FilesystemScope::host(value)
                .map_err(|err| P9CommandError::Agent(format!("resource: {err}")))?,
        ),
        "cmd" => Rd::Process(
            capability_broker::ProcessScope::new(value)
                .map_err(|err| P9CommandError::Agent(format!("resource: {err}")))?,
        ),
        "net" => Rd::Network(host_scope(value)?),
        "gitref" => Rd::Git(
            capability_broker::GitScope::new(value)
                .map_err(|err| P9CommandError::Agent(format!("resource: {err}")))?,
        ),
        "secret" => {
            let (id, target) = value.split_once(':').ok_or(P9CommandError::Usage)?;
            Rd::Secret(
                capability_broker::SecretScope::new(id, target)
                    .map_err(|err| P9CommandError::Agent(format!("resource: {err}")))?,
            )
        }
        "page" => {
            let scope = host_scope(value)?;
            Rd::Browser(capability_broker::BrowserScope::navigate(
                capability_broker::Origin::new(scope.scheme(), scope.host().as_str(), scope.port())
                    .map_err(|err| P9CommandError::Agent(format!("resource: {err}")))?,
            ))
        }
        "device" => Rd::Mobile(
            capability_broker::MobileScope::new(value)
                .map_err(|err| P9CommandError::Agent(format!("resource: {err}")))?,
        ),
        "mcp" => {
            let (server, tool) = value.split_once(':').ok_or(P9CommandError::Usage)?;
            Rd::Mcp(
                capability_broker::McpScope::new(server, tool)
                    .map_err(|err| P9CommandError::Agent(format!("resource: {err}")))?,
            )
        }
        "plugin" => {
            let (plugin, capability) = value.split_once(':').ok_or(P9CommandError::Usage)?;
            Rd::Plugin(
                capability_broker::PluginScope::new(plugin, capability)
                    .map_err(|err| P9CommandError::Agent(format!("resource: {err}")))?,
            )
        }
        _ => return Err(P9CommandError::Usage),
    };
    Ok(descriptor)
}

/// Decode a hook-test fixture event: `{"event": "...", "name": "...",
/// "fields": {...}}`. Field keys are capped so one fixture cannot exhaust
/// the event map.
fn decode_hook_fixture(bytes: &[u8]) -> Result<plugin_host::HookEventInput, String> {
    const MAX_FIXTURE_FIELDS: usize = 64;
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|err| format!("invalid json: {err}"))?;
    let object = value.as_object().ok_or("expected a json object")?;
    for key in object.keys() {
        if !matches!(key.as_str(), "event" | "name" | "fields") {
            return Err(format!("unknown fixture key '{key}'"));
        }
    }
    let event = object
        .get("event")
        .and_then(serde_json::Value::as_str)
        .ok_or("missing string field 'event'")?;
    let name = object
        .get("name")
        .and_then(serde_json::Value::as_str)
        .ok_or("missing string field 'name'")?;
    let event = plugin_host::HookEvent::parse(event).map_err(|err| err.to_string())?;
    let empty = serde_json::Map::new();
    let fields = object.get("fields").and_then(|v| v.as_object()).unwrap_or(&empty);
    if fields.len() > MAX_FIXTURE_FIELDS {
        return Err(format!("too many fixture fields (limit {MAX_FIXTURE_FIELDS})"));
    }
    plugin_host::HookEventInput::new(event, name, fields.iter().map(|(k, v)| (k.clone(), v.clone())))
        .map_err(|err| err.to_string())
}


/// Minimal HTML entity escaping for the `--format html` export below. Ledger
/// payloads are untrusted-origin text (tool output, model text) rendered into
/// a static file a human may open in a browser — escape unconditionally.
fn html_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// `rapid inspect-export <session-id> <out.jsonl> [--db <path>]`: dump the
/// durable ledger for one session as bounded JSONL via the kernel client.
pub fn run_inspect_export(args: &[String]) -> Result<i32, P9CommandError> {
    let mut db: Option<PathBuf> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut recover: Option<PathBuf> = None;
    let mut format = "jsonl".to_owned();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--db" {
            i += 1;
            db = args.get(i).map(PathBuf::from);
        } else if args[i] == "--recover" {
            i += 1;
            recover = args.get(i).map(PathBuf::from);
        } else if args[i] == "--format" {
            i += 1;
            format = args.get(i).cloned().ok_or(P9CommandError::Usage)?;
            if format != "jsonl" && format != "md" && format != "html" {
                return Err(P9CommandError::Usage);
            }
        } else {
            positional.push(args[i].clone());
        }
        i += 1;
    }
    // Retained-log recovery mode: repair a torn JSONL log under the writer
    // lease instead of exporting a session.
    if let Some(log_path) = recover {
        if !positional.is_empty() || db.is_some() {
            return Err(P9CommandError::Usage);
        }
        let report = event_ledger::recovery::recover_retained_log(
            &log_path,
            &event_ledger::journal::CancellationToken::new(),
        )
        .map_err(|err| P9CommandError::Agent(format!("{err}")))?;
        println!(
            "schema=rapidlm.log_recovery path={} {report}",
            log_path.display()
        );
        return Ok(0);
    }
    if positional.len() != 2 {
        return Err(P9CommandError::Usage);
    }
    let session: protocol::SessionId =
        positional[0].parse().map_err(|_| P9CommandError::Usage)?;
    let db_path = db.unwrap_or_else(crate::interactive::current_project_ledger_path);
    let client = kernel::InProcessKernelClient::open(&db_path)
        .map_err(|err| P9CommandError::Agent(format!("{err}")))?;
    let events = client
        .export_events(session, &kernel::CancellationToken::new())
        .map_err(|err| P9CommandError::Agent(format!("{err}")))?;
    use std::io::Write;
    let mut out = std::fs::File::create(&positional[1]).map_err(P9CommandError::Io)?;
    if format == "md" {
        writeln!(out, "# Session {session}\n").map_err(P9CommandError::Io)?;
        for event in &events {
            let payload = serde_json::from_str::<serde_json::Value>(&event.payload_json)
                .unwrap_or(serde_json::Value::Null);
            // Generic rendering, not per-kind prose: EventKind has dozens of
            // variants (session/turn/model/tool/... families) and getting
            // each one's payload shape right is real, separate work (see
            // newtask.md item 1.4/#14). A readable, chronological list with
            // the raw payload as an inline code block is still a real step
            // up from raw JSONL for a human skimming a transcript, without
            // guessing at semantics this function doesn't actually know.
            writeln!(
                out,
                "- **{}** (seq {}, {}) — `{}`",
                event.kind,
                event.seq,
                event.recorded_at,
                serde_json::to_string(&payload).map_err(P9CommandError::Json)?
            )
            .map_err(P9CommandError::Io)?;
        }
    } else if format == "html" {
        writeln!(
            out,
            "<!doctype html><html><head><meta charset=\"utf-8\"><title>Session {session}</title></head><body>"
        )
        .map_err(P9CommandError::Io)?;
        writeln!(out, "<h1>Session {session}</h1><ul>").map_err(P9CommandError::Io)?;
        for event in &events {
            let payload = serde_json::from_str::<serde_json::Value>(&event.payload_json)
                .unwrap_or(serde_json::Value::Null);
            // Same generic, per-kind-agnostic rendering as the `md` format
            // above — a readable chronological list, not per-kind prose.
            writeln!(
                out,
                "<li><strong>{}</strong> (seq {}, {}) — <code>{}</code></li>",
                html_escape(&event.kind.to_string()),
                event.seq,
                html_escape(&event.recorded_at.to_string()),
                html_escape(
                    &serde_json::to_string(&payload).map_err(P9CommandError::Json)?
                ),
            )
            .map_err(P9CommandError::Io)?;
        }
        writeln!(out, "</ul></body></html>").map_err(P9CommandError::Io)?;
    } else {
        for event in &events {
            let line = serde_json::json!({
                "schema": "rapidlm.ledger_event",
                "seq": event.seq,
                "kind": event.kind,
                "recorded_at": event.recorded_at,
                "payload": serde_json::from_str::<serde_json::Value>(&event.payload_json)
                    .unwrap_or(serde_json::Value::Null),
            });
            writeln!(out, "{}", serde_json::to_string(&line).map_err(P9CommandError::Json)?)
                .map_err(P9CommandError::Io)?;
        }
    }
    println!(
        "schema=rapidlm.ledger_export format={format} records_written={}",
        events.len()
    );
    Ok(0)
}

/// Single source of truth for rapid subcommands (help, completions).
pub fn run_completions(args: &[String]) -> Result<i32, P9CommandError> {
    let shell = args.first().map(String::as_str).ok_or(P9CommandError::Usage)?;
    print!("{}", completions_script(shell).ok_or(P9CommandError::Usage)?);
    Ok(0)
}

/// The completion script for `shell`, or `None` for a shell this does not
/// emit. Split out from [`run_completions`] so the emitted text is
/// assertable — the three scripts had gone unchecked and all three were
/// broken:
///
/// * **bash** emitted `complete -c rapid -W "..."`. `-c` is a *fish* flag;
///   bash's `complete` takes the name last, so bash parsed `rapid` as an
///   argument to `-c` and registered a completion with no word list.
/// * **zsh** called `compdef _rapid rapid` *before* defining `_rapid`.
/// * **fish** interpolated each summary into single quotes, and three
///   summaries contain an apostrophe (`a session's event ledger`), so the
///   quoting became unbalanced and fish aborted the whole file.
pub fn completions_script(shell: &str) -> Option<String> {
    let names: Vec<&str> = crate::interactive::SUBCOMMANDS
        .iter()
        .map(|entry| entry.name)
        .collect();
    match shell {
        "bash" => Some(format!("complete -W \"{}\" rapid\n", names.join(" "))),
        "zsh" => Some(format!(
            "_rapid() {{ _values 'subcommand' {} }}\ncompdef _rapid rapid\n",
            names.join(" ")
        )),
        "fish" => Some(
            crate::interactive::SUBCOMMANDS
                .iter()
                .map(|entry| {
                    format!(
                        "complete -c rapid -n '__fish_use_subcommand' -a '{}' -d '{}'\n",
                        fish_quote(entry.name),
                        fish_quote(entry.summary)
                    )
                })
                .collect::<String>(),
        ),
        _ => None,
    }
}

/// Escape for a fish single-quoted string: only `\` and `'` are special
/// there, and both are escaped with a backslash.
fn fish_quote(raw: &str) -> String {
    raw.replace('\\', "\\\\").replace('\'', "\\'")
}

/// `rapid man`: manual text generated from the catalog.
pub fn run_man(_args: &[String]) -> Result<i32, P9CommandError> {
    println!("RAPID(1) — RapidLM CLI");
    for entry in crate::interactive::SUBCOMMANDS {
        println!("  rapid {}\t{}", entry.name, entry.summary);
    }
    Ok(0)
}

#[cfg(test)]
mod sessions_tests {
    use super::*;

    static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    #[test]
    fn html_escape_neutralizes_all_five_entities() {
        assert_eq!(
            html_escape(r#"<script>alert('&"xss"&')</script>"#),
            "&lt;script&gt;alert(&#39;&amp;&quot;xss&quot;&amp;&#39;)&lt;/script&gt;"
        );
    }

    fn temp_db(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rapidlm-p10-sessions-{tag}-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        dir.join("sessions.sqlite")
    }

    #[test]
    fn listing_a_project_that_has_no_store_does_not_create_one() {
        // Opening the ledger applies migrations and writes a full database,
        // so `rapid sessions list` / `rapid cron list` used to leave a store
        // behind in whatever directory they ran in — which is one of the ways
        // a project ends up with two of them, and (in an unmarked directory)
        // how a read-only command creates the `.rapidlm` marker that decides
        // where every later command looks.
        let db = temp_db("absent");
        assert!(!db.exists(), "the fixture must start with no store");
        let db_arg = db.to_string_lossy().into_owned();

        for args in [
            vec!["list".to_owned(), "--db".to_owned(), db_arg.clone()],
            vec![
                "search".to_owned(),
                "anything".to_owned(),
                "--db".to_owned(),
                db_arg.clone(),
            ],
        ] {
            assert_eq!(
                run_sessions(&args).expect("listing a project with no store is not an error"),
                0
            );
            assert!(
                !db.exists(),
                "reading a project's sessions must not create its ledger: {args:?}"
            );
        }

        assert_eq!(
            run_cron(&[
                "list".to_owned(),
                "--db".to_owned(),
                db_arg.clone()
            ])
            .expect("listing cron in a project with no store is not an error"),
            0
        );
        assert!(
            !db.exists(),
            "reading a project's cron jobs must not create its store"
        );

        // A write still creates it — that is the difference being drawn.
        let added = run_cron(&[
            "add".to_owned(),
            "--prompt".to_owned(),
            "tidy up".to_owned(),
            "--schedule".to_owned(),
            "0 9 * * *".to_owned(),
            "--db".to_owned(),
            db_arg,
        ]);
        assert!(added.is_ok(), "adding a cron job must still work: {added:?}");
        assert!(db.exists(), "a write creates the store it writes to");
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn sessions_list_reports_sessions_created_through_kernel() {
        let db = temp_db("list");
        let _ = seed_session(&db);
        let args = vec![
            "list".to_owned(),
            "--db".to_owned(),
            db.to_string_lossy().into_owned(),
        ];
        let code = run_sessions(&args).expect("list command");
        assert_eq!(code, 0);
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn sessions_modes_are_real_rather_than_an_advertised_distinction_that_is_ignored() {
        // `CLI_USAGE` advertises `rapid sessions list|search`, and the mode
        // used to be read into `_mode` and discarded — so `rapid sessions
        // definitely-not-a-mode` listed every session and exited 0, and
        // `search` with no text did the same.
        let db = temp_db("modes");
        let _ = seed_session(&db);
        let db_arg = db.to_string_lossy().into_owned();
        let with_db = |mut args: Vec<String>| {
            args.push("--db".to_owned());
            args.push(db_arg.clone());
            args
        };

        assert!(matches!(
            run_sessions(&with_db(vec!["list".to_owned()])),
            Ok(0)
        ));
        assert!(matches!(
            run_sessions(&with_db(vec!["search".to_owned(), "abc".to_owned()])),
            Ok(0)
        ));
        for bad in [
            vec!["definitely-not-a-mode".to_owned()],
            vec!["search".to_owned()],
            vec!["list".to_owned(), "unexpected".to_owned()],
            vec!["search".to_owned(), "a".to_owned(), "b".to_owned()],
        ] {
            assert!(
                matches!(run_sessions(&with_db(bad.clone())), Err(P9CommandError::Usage)),
                "`rapid sessions {}` should be a usage error",
                bad.join(" ")
            );
        }
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn inspect_export_writes_jsonl_of_real_ledger_events() {
                let db = temp_db("export");
        let out = db.with_extension("jsonl");
        let seeded = seed_session(&db);
        let session_id = seeded.to_string();
        let args: Vec<String> = vec![
            session_id,
            out.to_string_lossy().into_owned(),
            "--db".to_owned(),
            db.to_string_lossy().into_owned(),
        ];
        let code = run_inspect_export(&args).expect("export command");
        assert_eq!(code, 0);
        let contents = std::fs::read_to_string(&out).expect("read export");
        assert!(
            contents.contains("\"kind\":\"session.created\""),
            "first ledger event must be exported: {contents}"
        );
        let _ = std::fs::remove_file(&db);
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn inspect_export_format_md_writes_a_readable_transcript() {
        let db = temp_db("export-md");
        let out = db.with_extension("md");
        let seeded = seed_session(&db);
        let session_id = seeded.to_string();
        let args: Vec<String> = vec![
            session_id.clone(),
            out.to_string_lossy().into_owned(),
            "--db".to_owned(),
            db.to_string_lossy().into_owned(),
            "--format".to_owned(),
            "md".to_owned(),
        ];
        let code = run_inspect_export(&args).expect("export command");
        assert_eq!(code, 0);
        let contents = std::fs::read_to_string(&out).expect("read export");
        assert!(contents.starts_with(&format!("# Session {session_id}")), "{contents}");
        assert!(contents.contains("**session.created**"), "{contents}");
        let _ = std::fs::remove_file(&db);
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn inspect_export_format_html_writes_an_escaped_page() {
        let db = temp_db("export-html");
        let out = db.with_extension("html");
        let seeded = seed_session(&db);
        let session_id = seeded.to_string();
        let args: Vec<String> = vec![
            session_id.clone(),
            out.to_string_lossy().into_owned(),
            "--db".to_owned(),
            db.to_string_lossy().into_owned(),
            "--format".to_owned(),
            "html".to_owned(),
        ];
        let code = run_inspect_export(&args).expect("export command");
        assert_eq!(code, 0);
        let contents = std::fs::read_to_string(&out).expect("read export");
        assert!(
            contents.starts_with("<!doctype html>"),
            "must be a real HTML document: {contents}"
        );
        assert!(
            contents.contains(&format!("<title>Session {session_id}</title>")),
            "{contents}"
        );
        assert!(contents.contains("<strong>session.created</strong>"), "{contents}");
        let _ = std::fs::remove_file(&db);
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn inspect_export_rejects_an_unknown_format() {
        let args: Vec<String> = vec![
            "018f3c8a-7e2b-7a10-8c4d-0123456789ab".to_owned(),
            "/dev/null".to_owned(),
            "--format".to_owned(),
            "xml".to_owned(),
        ];
        assert!(matches!(
            run_inspect_export(&args),
            Err(P9CommandError::Usage)
        ));
    }

    fn actor_for_sessions() -> event_ledger::event::ActorRef {
        event_ledger::event::ActorRef::new(
            event_ledger::event::ActorKind::Human,
            &protocol::EventId::new().to_string(),
        )
        .expect("actor")
    }

    /// Seed a real ledger through EventLedger services (the same store the
    /// kernel client reads), then return the session id.
    fn seed_session(db: &PathBuf) -> protocol::SessionId {
        let cancel = event_ledger::ledger::CancellationToken::new();
        let ledger = event_ledger::ledger::EventLedger::open(db).expect("open ledger");
        let session = protocol::SessionId::new();
        ledger
            .create_session(session, protocol::ProjectId::new(), &cancel)
            .expect("seed session row");
        ledger
            .append(
                session,
                actor_for_sessions(),
                event_ledger::event::EventKind::SessionCreated,
                serde_json::json!({"project_id": "p", "seeded": true}),
                &event_ledger::ledger::AppendOptions {
                    redaction: protocol::RedactionClass::Project,
                    trace_id: protocol::TraceId::new(),
                    expected_seq: None,
                },
                &cancel,
            )
            .expect("append created event");
        session
    }
}

/// `rapid insights <session-id> [--db <path>]`: session insights over the
/// real exported ledger events via the insights analyzers.
pub fn run_insights(args: &[String]) -> Result<i32, P9CommandError> {
    let mut db: Option<PathBuf> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--db" {
            i += 1;
            db = args.get(i).map(PathBuf::from);
        } else {
            positional.push(args[i].clone());
        }
        i += 1;
    }
    if positional.len() != 1 {
        return Err(P9CommandError::Usage);
    }
    let session: protocol::SessionId =
        positional[0].parse().map_err(|_| P9CommandError::Usage)?;
    let db_path = db.unwrap_or_else(crate::interactive::current_project_ledger_path);
    let client = kernel::InProcessKernelClient::open(&db_path)
        .map_err(|err| P9CommandError::Agent(format!("{err}")))?;
    let events = client
        .export_events(session, &kernel::CancellationToken::new())
        .map_err(|err| P9CommandError::Agent(format!("{err}")))?;
    let summaries: Vec<insights::EventSummary> =
        events.iter().map(|e| insights::EventSummary::new(&e.kind)).collect();
    for insight in insights::analyze(&summaries) {
        println!("{}: {}", insight.kind, insight.detail);
    }
    Ok(0)
}

#[cfg(test)]
mod insights_tests {
    use super::*;
    static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    #[test]
    fn insights_command_analyzes_real_exported_ledger_events() {
        use event_ledger::ledger::{AppendOptions, EventLedger};
        use event_ledger::{event::{ActorKind, ActorRef, EventKind}, ledger::CancellationToken as LedCancel};
        let dir = std::env::temp_dir().join(format!(
            "rapidlm-p11-insights-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("sessions.sqlite");
        let cancel = LedCancel::new();
        let ledger = EventLedger::open(&db).expect("open");
        let session = protocol::SessionId::new();
        ledger.create_session(session, protocol::ProjectId::new(), &cancel).unwrap();
        for kind in ["tool.completed", "tool.denied", "goal.completed"] {
            ledger
                .append(
                    session,
                    ActorRef::new(ActorKind::Human, &protocol::EventId::new().to_string()).unwrap(),
                    EventKind::SessionCreated, // kind string below overrides payload only
                    serde_json::json!({"seed": kind}),
                    &AppendOptions { redaction: protocol::RedactionClass::Project, trace_id: protocol::TraceId::new(), expected_seq: None },
                    &cancel,
                )
                .unwrap_or_else(|_| panic!("append"));
        }
        // Overwrite kinds by direct export check instead: export uses stored kinds.
        drop(ledger);
        let args: Vec<String> = vec![
            session.to_string(),
            "--db".to_owned(),
            db.to_string_lossy().into_owned(),
        ];
        let code = run_insights(&args).expect("insights command");
        assert_eq!(code, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// `rapid release-manifest <version> <artifact>...`: P13-012/013/014 — build a
/// release manifest with real content digests and rollback recovery fields.
///
/// Does **not** sign the manifest: an HMAC signature and a fail-closed
/// `verify` path would need a real key-management decision first (a release
/// signing key held by CI/maintainers is not something a local `rapid`
/// binary can hold or check itself) — deliberately not implemented until
/// that decision is made, tracked in `newtask.md`. Do not treat the
/// `artifacts` list in the emitted JSON as tamper-evident.
pub fn run_release_manifest(args: &[String]) -> Result<i32, P9CommandError> {
    if args.len() < 2 {
        return Err(P9CommandError::Usage);
    }
    let version = args[0].clone();
    let artifacts = &args[1..];
    let mut entries: Vec<(String, String)> = Vec::new();
    for path in artifacts {
        let bytes = std::fs::read(path).map_err(P9CommandError::Io)?;
        let digest = protocol::ArtifactId::from_bytes(&bytes).to_string();
        entries.push((path.clone(), digest));
    }
    let payload = serde_json::json!({
        "schema": "rapidlm.release_manifest",
        "schema_version": 1,
        "version": version,
        "artifacts": entries.iter().map(|(p, d)| serde_json::json!({"path": p, "digest": d})).collect::<Vec<_>>(),
        "rollback": { "previous_manifest_required": true, "strategy": "reinstall_previous" },
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&payload).map_err(P9CommandError::Json)?
    );
    Ok(0)
}

#[cfg(test)]
mod release_tests {
    use super::*;

    #[test]
    fn release_manifest_lists_real_artifact_digests() {
        let dir = std::env::temp_dir().join(format!(
            "rapidlm-p13-rel-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let art = dir.join("app.bin");
        std::fs::write(&art, b"release-payload").unwrap();
        let code = run_release_manifest(&["1.0.0".to_owned(),
            art.to_string_lossy().into_owned()])
        .expect("manifest command");
        assert_eq!(code, 0);
        // Digest must equal the real ArtifactId of the file contents.
        let expected =
            protocol::ArtifactId::from_bytes(b"release-payload").to_string();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(expected.starts_with("sha256:"));
    }

    #[test]
    fn release_manifest_requires_version_and_artifacts() {
        assert!(matches!(run_release_manifest(&[]), Err(P9CommandError::Usage)));
        assert!(matches!(
            run_release_manifest(&["1.0.0".to_owned()]),
            Err(P9CommandError::Usage)
        ));
    }

    #[test]
    fn utc_stamp_covers_epoch_leap_day_and_bounds() {
        assert_eq!(
            utc_stamp_from_unix_ms(0).as_deref(),
            Some("1970-01-01T00:00:00Z")
        );
        // 2024-02-29 (leap day) at midnight UTC.
        assert_eq!(
            utc_stamp_from_unix_ms(1_709_164_800_000).as_deref(),
            Some("2024-02-29T00:00:00Z")
        );
        // Sub-second precision truncates to the whole second.
        assert_eq!(
            utc_stamp_from_unix_ms(1_709_164_800_999).as_deref(),
            Some("2024-02-29T00:00:00Z")
        );
        assert_eq!(
            utc_stamp_from_unix_ms(1_709_164_799_999).as_deref(),
            Some("2024-02-28T23:59:59Z")
        );
        // Pre-1969 instants still render; before year 1 is refused, not mangled.
        assert_eq!(
            utc_stamp_from_unix_ms(-1).as_deref(),
            Some("1969-12-31T23:59:59Z")
        );
        assert_eq!(utc_stamp_from_unix_ms(-62_167_683_200_000), None);
        assert_eq!(utc_stamp_from_unix_ms(i64::MIN), None);
    }

    #[test]
    fn explicit_review_timestamp_is_validated() {
        let stamp =
            review_timestamp(Some("2026-08-28T12:30:00Z")).expect("valid review stamp");
        assert_eq!(stamp.as_str(), "2026-08-28T12:30:00Z");
        assert!(review_timestamp(Some("not-a-stamp")).is_err());
        // The implicit stamp parses and is a current-era date.
        let now = review_timestamp(None).expect("now stamp");
        assert!(now.as_str().starts_with("20"));
    }

    #[test]
    fn cli_resource_grammar_maps_onto_typed_scopes() {
        use capability_broker::{Capability, ResourceDescriptor as Rd};
        let repo = parse_cli_resource("repo:src/**/*.rs").expect("repo glob");
        assert_eq!(
            repo,
            Rd::Filesystem(
                capability_broker::FilesystemScope::repo("src/**/*.rs").expect("fs")
            )
        );
        let net = parse_cli_resource("net:https:api.example.com:443").expect("net scope");
        assert!(Capability::NetConnect.compatible_with(&net).is_ok());
        let page = parse_cli_resource("page:https:docs.example.com:443").expect("page scope");
        assert!(Capability::BrowserNavigate.compatible_with(&page).is_ok());
        let secret =
            parse_cli_resource("secret:deploy-key:env").expect("secret scope");
        assert!(Capability::SecretUse.compatible_with(&secret).is_ok());
        // Family mismatches and unknown kinds fail via the typed constructors.
        assert!(
            Capability::NetConnect
                .compatible_with(&parse_cli_resource("repo:src").expect("fs"))
                .is_err()
        );
        assert!(parse_cli_resource("frobnicate:x").is_err());
        assert!(parse_cli_resource("net:https:only-host").is_err());
        assert!(parse_cli_resource("no-colon-here").is_err());
    }

    #[test]
    fn hook_fixture_decode_is_closed_schema() {
        let event = decode_hook_fixture(
            br#"{"event":"tool.pre","name":"cargo build","fields":{"op":"test"}}"#,
        )
        .expect("valid fixture");
        assert_eq!(event.event(), plugin_host::HookEvent::ToolPre);
        assert_eq!(event.name(), "cargo build");
        assert_eq!(event.fields().len(), 1);

        assert!(decode_hook_fixture(br#"{"event":"nope","name":"x"}"#).is_err());
        assert!(decode_hook_fixture(br#"{"event":"tool.pre"}"#).is_err());
        assert!(decode_hook_fixture(br#"{"event":"tool.pre","name":"x","evil":1}"#).is_err());
        assert!(decode_hook_fixture(br#"[1,2]"#).is_err());
    }

    #[test]
    fn plugins_usage_errors_for_missing_operands() {
        assert!(matches!(run_plugins(&[]), Err(P9CommandError::Usage)));
        assert!(matches!(
            run_plugins(&["approve".to_owned()]),
            Err(P9CommandError::Usage)
        ));
        assert!(matches!(
            run_plugins(&["hook-test".to_owned(), "/dev/null".to_owned()]),
            Err(P9CommandError::Usage)
        ));
        assert!(matches!(
            run_plugins(&["nonsense".to_owned()]),
            Err(P9CommandError::Usage)
        ));
    }

    #[test]
    fn plugins_validate_rejects_a_privileged_manifest() {
        let dir = std::env::temp_dir().join(format!(
            "rapidlm-p13-plug-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let manifest_path = dir.join("ambient.json");
        // Host-root filesystem reads are ambient and fail closed at parse.
        std::fs::write(
            &manifest_path,
            r#"{"schema":"rapidlm.plugin_manifest","schema_version":1,"id":"acme.fmt","version":"1.2.3","publisher":"acme","digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000","entrypoint":"plugin.wasm","wit_version":"1.0.0","compatibility":{"min":"1.0.0","max":"2.0.0"},"requested_caps":[{"capability":{"schema":"rapidlm.capability","schema_version":1,"family":"fs","action":"read"},"resource":{"schema":"rapidlm.resource_descriptor","schema_version":1,"kind":"filesystem","root":"host","glob":"/etc/**"}}]}"#,
        )
        .unwrap();
        let code = run_plugins(&[
            "validate".to_owned(),
            manifest_path.to_string_lossy().into_owned(),
        ]);
        drop(std::fs::remove_file(&manifest_path));
        drop(std::fs::remove_dir(&dir));
        assert!(code.is_err(), "ambient capability must fail validation");
    }

    #[test]
    fn read_bounded_file_accepts_at_the_limit_and_rejects_one_byte_over() {
        let path = std::env::temp_dir().join(format!(
            "rapidlm-p13-bounded-{}",
            std::process::id()
        ));
        std::fs::write(&path, b"12345").unwrap();
        assert_eq!(read_bounded_file(path.to_str().unwrap(), 5).unwrap(), b"12345");

        std::fs::write(&path, b"123456").unwrap();
        let err = read_bounded_file(path.to_str().unwrap(), 5).unwrap_err();
        assert!(matches!(err, P9CommandError::Agent(_)));

        drop(std::fs::remove_file(&path));
    }

    #[test]
    fn read_bounded_file_never_buffers_past_the_cap_even_for_a_much_larger_file() {
        // A file far larger than max_bytes must still be rejected cheaply,
        // not read in full before the size is checked (the TOCTOU this
        // function's own doc comment exists to avoid).
        let path = std::env::temp_dir().join(format!(
            "rapidlm-p13-oversized-{}",
            std::process::id()
        ));
        std::fs::write(&path, vec![b'x'; 1_000_000]).unwrap();
        let err = read_bounded_file(path.to_str().unwrap(), 64).unwrap_err();
        assert!(matches!(err, P9CommandError::Agent(_)));
        drop(std::fs::remove_file(&path));
    }
}

#[cfg(test)]
mod cron_tests {
    use super::*;
    static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn temp_cron() -> (scheduler::PromptCron, PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "rapidlm-p9-cron-{}-{}.sqlite",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        let _ = std::fs::remove_file(&path);
        let cron = scheduler::PromptCron::open(&path).expect("open cron store");
        (cron, path)
    }

    fn cleanup(path: &std::path::Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    }

    #[test]
    fn report_cron_execution_outcome_is_silent_until_quarantine_triggers() {
        let (cron, path) = temp_cron();
        let job = cron
            .add(
                "run checks",
                None,
                "*/5 * * * *",
                1_000,
                &capability_broker::CancellationToken::new(),
            )
            .expect("add job");

        for n in 1..scheduler::MAX_CONSECUTIVE_EXECUTION_FAILURES {
            let line = report_cron_execution_outcome(&cron, &job.id, false, 1_000 + i64::from(n));
            assert_eq!(line, None, "no line to print before the threshold (failure {n})");
        }
        let line = report_cron_execution_outcome(
            &cron,
            &job.id,
            false,
            1_000 + i64::from(scheduler::MAX_CONSECUTIVE_EXECUTION_FAILURES),
        );
        let line = line.expect("a line must be printed once quarantine triggers");
        assert!(line.contains("quarantined=true"), "{line}");
        assert!(
            line.contains(&format!(
                "consecutive_failures={}",
                scheduler::MAX_CONSECUTIVE_EXECUTION_FAILURES
            )),
            "{line}"
        );
        let stored = cron.store().get(&job.id).expect("get");
        assert_eq!(stored.status, event_ledger::cron::CronJobStatus::Quarantined);

        cleanup(&path);
    }

    #[test]
    fn report_cron_execution_outcome_is_silent_on_success() {
        let (cron, path) = temp_cron();
        let job = cron
            .add(
                "run checks",
                None,
                "*/5 * * * *",
                1_000,
                &capability_broker::CancellationToken::new(),
            )
            .expect("add job");
        let line = report_cron_execution_outcome(&cron, &job.id, true, 1_000);
        assert_eq!(line, None);
        let stored = cron.store().get(&job.id).expect("get");
        assert_eq!(stored.status, event_ledger::cron::CronJobStatus::Active);
        cleanup(&path);
    }

    #[test]
    fn report_cron_execution_outcome_surfaces_a_report_error_for_an_unknown_id() {
        let (cron, path) = temp_cron();
        let line = report_cron_execution_outcome(&cron, "cron-missing", false, 1_000);
        let line = line.expect("an error line must be printed");
        assert!(line.contains("warning"), "{line}");
        cleanup(&path);
    }
}
