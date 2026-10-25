//! The main agent of a session (EXT-05 slice 4): which agent definition a
//! session runs as, and what that does to the session's main role.
//!
//! A definition with `mode: primary` or `both` can be chosen with `--agent <id>`
//! or `[agents] default_primary`. It is an overlay on the main role and only
//! ever narrows it: its instructions follow the host's in a labelled section,
//! its tools are the main role's less what it leaves out, its permission rules
//! can refuse or ask, its step cap bounds each run, its `spawn` list holds
//! `task_spawn` to those agents, and its model and effort are used where the
//! managed policy allows. The choice is fixed for the session and recorded as
//! `agent.selected`; a resumed session runs as the agent it recorded.

use std::path::Path;

use agent_runtime::agent_defs::{AgentDefId, AgentDefinition};

use crate::headless::jsonl::JsonlExitCode;

/// The heading the overlay's instructions carry in the system prompt.
const OVERLAY_HEADING: &str = "Main agent";

/// A definition a session runs as.
#[derive(Clone, Debug)]
pub(crate) struct PrimaryAgent {
    def: AgentDefinition,
}

/// Why a session cannot run as the agent asked for. Each is said before any
/// model request, and names `rapid agents list`, which shows what can be chosen.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum PrimaryError {
    /// No agent has the id (or it is not even an id).
    Unknown {
        requested: String,
        selectable: Vec<String>,
    },
    /// The agent exists but is a subagent only.
    NotSelectable { id: String },
    /// The agent is defined by this project, which is not trusted.
    Untrusted { id: String },
    /// Its model or effort cannot be used (not configured, or the managed
    /// policy does not allow it).
    Model { id: String, reason: String },
    /// Its permission rules do not fit beside the session's.
    Rules { id: String, reason: String },
    /// A resumed session runs as the agent it recorded, not another.
    Fixed { recorded: String, requested: String },
    /// A resumed session that began without an agent does not acquire one.
    NoAgentYet { requested: String },
    /// The session's record of its agent could not be read.
    Ledger(String),
}

impl PrimaryError {
    /// The exit code of a run refused for this reason: a request that cannot
    /// be met is a usage error, a refusal of policy is a policy one.
    pub(crate) fn exit_code(&self) -> JsonlExitCode {
        match self {
            Self::Untrusted { .. } | Self::Model { .. } | Self::Rules { .. } => {
                JsonlExitCode::Policy
            }
            Self::Ledger(_) => JsonlExitCode::Runtime,
            _ => JsonlExitCode::Usage,
        }
    }
}

impl std::fmt::Display for PrimaryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown {
                requested,
                selectable,
            } => write!(
                f,
                "no agent '{}' to run as; the agents a session can run as: {} (rapid agents list)",
                shown(requested),
                if selectable.is_empty() {
                    "none".to_owned()
                } else {
                    selectable.join(", ")
                }
            ),
            Self::NotSelectable { id } => write!(
                f,
                "agent '{id}' is a subagent only (mode subagent); give it `mode: primary` or `both` to run a session as it (rapid agents list)"
            ),
            Self::Untrusted { id } => write!(
                f,
                "agent '{id}' is defined by this project, which is not trusted, so it is not read; review it, then run `rapid trust grant` (rapid agents list)"
            ),
            Self::Model { id, reason } => write!(f, "agent '{id}': {reason} (rapid agents list)"),
            Self::Rules { id, reason } => write!(f, "agent '{id}': {reason}"),
            Self::Fixed {
                recorded,
                requested,
            } => write!(
                f,
                "this session runs as agent '{}', and a session keeps its agent for its whole life; start a new session to run as '{}'",
                shown(recorded),
                shown(requested)
            ),
            Self::NoAgentYet { requested } => write!(
                f,
                "this session began without an agent, and a session keeps its agent for its whole life; start a new session to run as '{}'",
                shown(requested)
            ),
            Self::Ledger(reason) => write!(
                f,
                "the session's record of its agent could not be read: {reason}"
            ),
        }
    }
}

impl std::error::Error for PrimaryError {}

/// What a session recorded of its agent (`agent.selected`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Recorded {
    pub(crate) id: String,
    pub(crate) digest: String,
}

/// Which agent a run is to use, from what was asked and what the session
/// recorded: a new session takes the flag, else the configured default; a
/// resumed one keeps what it recorded, and is refused a different request
/// (the choice is fixed for a session's life, which keeps its prompt stable).
/// `None` is the plain main role.
pub(crate) fn choose(
    resuming: bool,
    flag: Option<&str>,
    configured: Option<&str>,
    recorded: Option<&Recorded>,
) -> Result<Option<String>, PrimaryError> {
    if !resuming {
        return Ok(flag.or(configured).map(str::to_owned));
    }
    match (recorded, flag) {
        (Some(recorded), Some(flag)) if flag != recorded.id => Err(PrimaryError::Fixed {
            recorded: recorded.id.clone(),
            requested: flag.to_owned(),
        }),
        (Some(recorded), _) => Ok(Some(recorded.id.clone())),
        (None, Some(flag)) => Err(PrimaryError::NoAgentYet {
            requested: flag.to_owned(),
        }),
        // The configured default is for sessions that begin after it was set.
        (None, None) => Ok(None),
    }
}

/// How far up a chain of forks a session's agent is looked for.
const MAX_FORK_DEPTH: usize = 8;

/// The agent a session recorded (`agent.selected`; the newest wins), if it
/// recorded one. A fork — `/fork`, `/undo`, `/rewind`, the daemon's — starts
/// with the record of the parent as it stood at the point it branched, so it
/// is the same agent: a session that branches from one that runs as an agent
/// would otherwise resume as the plain main role.
pub(crate) fn recorded_in(
    client: &kernel::InProcessKernelClient,
    session: protocol::SessionId,
) -> Result<Option<Recorded>, PrimaryError> {
    recorded_through(client, session, None, MAX_FORK_DEPTH)
}

/// [`recorded_in`] as of `through` (a fork's source point; `None`: now),
/// following at most `depth` forks up.
fn recorded_through(
    client: &kernel::InProcessKernelClient,
    session: protocol::SessionId,
    through: Option<u64>,
    depth: usize,
) -> Result<Option<Recorded>, PrimaryError> {
    fn ledger<E: std::fmt::Display>(err: E) -> PrimaryError {
        PrimaryError::Ledger(err.to_string())
    }
    let own = client
        .events_of_kind(session, "agent.selected")
        .map_err(ledger)?;
    if let Some(last) = own
        .iter()
        .rev()
        .find(|event| through.is_none_or(|point| event.seq() <= point))
    {
        let field = |name: &str| {
            last.payload()
                .get(name)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };
        return match (field("id"), field("definition_digest")) {
            (Some(id), Some(digest)) if AgentDefId::parse(&id).is_ok() => {
                Ok(Some(Recorded { id, digest }))
            }
            _ => Err(PrimaryError::Ledger(
                "an `agent.selected` record has no agent id or no digest".to_owned(),
            )),
        };
    }
    // Nothing of its own: a fork's record is its parent's up to the branch.
    let forks = client
        .events_of_kind(session, "session.forked")
        .map_err(ledger)?;
    let Some(fork) = forks.first() else {
        return Ok(None);
    };
    let parent = fork
        .payload()
        .get("parent_session_id")
        .and_then(serde_json::Value::as_str)
        .and_then(|raw| raw.parse::<protocol::SessionId>().ok());
    let source_seq = fork
        .payload()
        .get("source_seq")
        .and_then(serde_json::Value::as_u64);
    match (parent, source_seq) {
        (Some(parent), Some(source_seq)) if depth > 0 => {
            recorded_through(client, parent, Some(source_seq), depth - 1)
        }
        // A chain deeper than is followed, or a fork that names no parent: the
        // record cannot be read through, and a guess would be the main role.
        _ => Err(PrimaryError::Ledger(
            "the session's fork chain cannot be followed to its agent".to_owned(),
        )),
    }
}

/// The environment an agent is looked for in: the process's, with the
/// RapidLM home the session resolved.
pub(crate) fn session_env(user_home: &Path) -> Vec<(String, String)> {
    std::env::vars()
        .filter(|(key, _)| key != "RAPIDLM_HOME")
        .chain(std::iter::once((
            "RAPIDLM_HOME".to_owned(),
            user_home.display().to_string(),
        )))
        .collect()
}

/// The agent a session recorded, as it is now on disk, for a surface that
/// drives turns of a session it did not start (ACP, the daemon, a continuation
/// after an approval): every turn of a session that runs as an agent runs as
/// it. `None`: the session recorded none.
pub(crate) fn session_agent(
    client: &kernel::InProcessKernelClient,
    session: protocol::SessionId,
    root: &Path,
    trusted: bool,
    env: &[(String, String)],
) -> Result<Option<std::sync::Arc<PrimaryAgent>>, PrimaryError> {
    let Some(recorded) = recorded_in(client, session)? else {
        return Ok(None);
    };
    resolve(&recorded.id, Some(root), trusted, env).map(|agent| Some(std::sync::Arc::new(agent)))
}

/// An id as an error shows it: one clean line of at most 64 characters (it may
/// have come from a command line, a config or a ledger).
fn shown(text: &str) -> String {
    protocol::text::one_clean_line(text)
        .chars()
        .take(64)
        .collect()
}

/// The agent a run is to use, with what recording it takes.
#[derive(Debug)]
pub(crate) struct Selection {
    pub(crate) agent: PrimaryAgent,
    /// Whether the session is to record this as its agent: a new session does,
    /// and so does a resumed one whose agent's definition has changed since.
    pub(crate) record: bool,
    /// For a resumed session, the digest it recorded when that is not the
    /// definition's digest now (the current definition is used, with a warning).
    pub(crate) changed_from: Option<String>,
}

impl Selection {
    /// The one line a resume says when the agent's definition has changed since
    /// the session recorded it.
    pub(crate) fn changed_warning(&self) -> Option<String> {
        self.changed_from.as_ref().map(|_| {
            format!(
                "warning: agent '{}' has changed since this session recorded it; the current definition is used",
                self.agent.id()
            )
        })
    }
}

/// Settle which agent a run is to use. `flag` is `--agent`, `resuming` whether
/// the run continues a recorded session and `recorded` reads what that session
/// recorded (called only when resuming). `None`: the plain main role.
pub(crate) fn select(
    flag: Option<&str>,
    resuming: bool,
    recorded: impl FnOnce() -> Result<Option<Recorded>, PrimaryError>,
    root: Option<&Path>,
    trusted: bool,
    env: &[(String, String)],
) -> Result<Option<Selection>, PrimaryError> {
    let configured = if !resuming && flag.is_none() {
        crate::user_config::default_primary(env)
    } else {
        None
    };
    let recorded = if resuming { recorded()? } else { None };
    let Some(id) = choose(resuming, flag, configured.as_deref(), recorded.as_ref())? else {
        return Ok(None);
    };
    let agent = resolve(&id, root, trusted, env)?;
    let digest = agent.digest();
    let (record, changed_from) = match recorded {
        Some(recorded) if recorded.digest == digest => (false, None),
        Some(recorded) => (true, Some(recorded.digest)),
        None => (true, None),
    };
    Ok(Some(Selection {
        agent,
        record,
        changed_from,
    }))
}

/// Record on the session that it runs as `agent` (`agent.selected`). A session
/// that cannot say which agent it is would resume as the plain main role, so a
/// caller that cannot record this stops rather than carries on.
pub(crate) fn record_selected(
    client: &kernel::InProcessKernelClient,
    session: protocol::SessionId,
    actor: &event_ledger::event::ActorRef,
    agent: &PrimaryAgent,
) -> Result<(), String> {
    client
        .append_turn_progress(
            session,
            actor,
            protocol::TraceId::new(),
            event_ledger::event::EventKind::AgentSelected,
            agent.selected_payload(),
        )
        .map(|_| ())
        .map_err(|err| err.to_string())
}

/// The agent `requested` names, as it is now on disk. `root` is the project
/// (if the run has one) and `trusted` whether its definitions are read.
pub(crate) fn resolve(
    requested: &str,
    root: Option<&Path>,
    trusted: bool,
    env: &[(String, String)],
) -> Result<PrimaryAgent, PrimaryError> {
    let inventory = match root {
        Some(root) => crate::agent_types::spawn_inventory_in(root, trusted, env),
        None => crate::agent_types::inventory_with(
            None,
            crate::agent_types::user_agents_dir(env).as_deref(),
            &crate::agent_types::extra_dirs(env).dirs,
        ),
    };
    let selectable = || -> Vec<String> {
        inventory
            .loaded
            .iter()
            .filter(|def| def.mode.is_selectable())
            .map(|def| def.id.as_str().to_owned())
            .collect()
    };
    let shown_id: String = requested.chars().take(64).collect();
    if AgentDefId::parse(requested).is_err() {
        return Err(PrimaryError::Unknown {
            requested: shown_id,
            selectable: selectable(),
        });
    }
    let Some(def) = inventory
        .loaded
        .iter()
        .find(|def| def.id.as_str() == requested)
    else {
        // A project's own definition is not read until the project is trusted:
        // say that, rather than that there is no such agent.
        if let Some(root) = root.filter(|_| !trusted) {
            let dir = root.join(agent_runtime::agent_defs::PROJECT_DEFS_DIR);
            let named = ["md", "toml"]
                .iter()
                .any(|ext| dir.join(format!("{requested}.{ext}")).is_file());
            if named {
                return Err(PrimaryError::Untrusted {
                    id: requested.to_owned(),
                });
            }
        }
        return Err(PrimaryError::Unknown {
            requested: shown_id,
            selectable: selectable(),
        });
    };
    if !def.mode.is_selectable() {
        return Err(PrimaryError::NotSelectable {
            id: requested.to_owned(),
        });
    }
    let agent = PrimaryAgent { def: def.clone() };
    agent.check_model(env)?;
    Ok(agent)
}

impl PrimaryAgent {
    pub(crate) fn id(&self) -> &str {
        self.def.id.as_str()
    }

    /// A definition that says something else, to see a fingerprint move.
    #[cfg(test)]
    pub(crate) fn set_description_for_test(&mut self, text: &str) {
        self.def.description = text.to_owned();
    }

    pub(crate) fn digest(&self) -> String {
        self.def.digest()
    }

    /// The model profile the definition asks for, if it asks for one.
    pub(crate) fn model(&self) -> Option<&str> {
        self.def.model.as_deref()
    }

    /// The `agent.selected` payload.
    pub(crate) fn selected_payload(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.def.id.as_str(),
            "definition_digest": self.digest(),
            "mode": self.def.mode.as_str(),
        })
    }

    /// Whether the model and effort it names can be used in this environment
    /// (configured, and allowed by the managed policy), so a run that cannot
    /// have them is refused before it starts.
    fn check_model(&self, env: &[(String, String)]) -> Result<(), PrimaryError> {
        let refuse = |reason: String| PrimaryError::Model {
            id: self.id().to_owned(),
            reason,
        };
        if let Some(model) = &self.def.model {
            match crate::user_config::select_active_model_with_override(env, Some(model)) {
                Ok(crate::user_config::ModelSelection::Configured { .. }) => {}
                Ok(_) => {
                    return Err(refuse(format!(
                        "names model '{model}', but no model configuration is loaded"
                    )));
                }
                Err(err) => return Err(refuse(format!("names model '{model}': {err}"))),
            }
        }
        if let Some(name) = &self.def.reasoning_effort
            && llm_router::ReasoningEffort::parse(name).is_err()
        {
            return Err(refuse(format!(
                "names an unknown reasoning effort '{name}'"
            )));
        }
        crate::managed_config::load_policy(env)
            .map_err(|err| refuse(format!("the managed policy could not be read: {err}")))?;
        Ok(())
    }

    /// The session's permissions with the agent's rules on them. Each can only
    /// refuse or ask, and all must hold: if they do not fit beside the rules
    /// the session already has, the run is refused rather than trimmed (the
    /// trim could drop a `deny`).
    pub(crate) fn permissions(
        &self,
        lattice: crate::permissions::PermissionLattice,
    ) -> Result<crate::permissions::PermissionLattice, PrimaryError> {
        let rules = crate::agent_types::child_rules(&self.def);
        if rules.len() > lattice.rule_room() {
            return Err(PrimaryError::Rules {
                id: self.id().to_owned(),
                reason: format!(
                    "adds {} permission rule(s) to the {} this session already holds, past the {} a session may hold; remove rules from the definition or from the settings",
                    rules.len(),
                    lattice.rules().len(),
                    crate::permissions::MAX_RULES
                ),
            });
        }
        Ok(lattice.with_rules(rules))
    }

    /// Narrow the main role's tools to what the definition keeps, and hold
    /// `task_spawn` to its `spawn` list when it has one.
    pub(crate) fn narrow(&self, tools: &mut crate::exec_tools::ExecTools) {
        tools.narrow_to_agent(&self.def);
        if !self.def.spawn.is_empty() {
            tools.set_spawn_allowlist(self.def.spawn.clone());
        }
    }

    /// Its standing instructions after the host's, in a section of their own.
    /// The host's prompt is never replaced.
    pub(crate) fn overlay(
        &self,
        preserved: crate::host::PreservedLiveContext,
    ) -> Result<crate::host::PreservedLiveContext, String> {
        let Some(instructions) = &self.def.instructions else {
            return Ok(preserved);
        };
        let combined = format!(
            "{}\n\n## {OVERLAY_HEADING} '{}'\n{instructions}",
            preserved.system_prompt().unwrap_or_default(),
            self.def.id
        );
        if combined.len() > crate::host::MAX_SYSTEM_PROMPT_BLOCK_BYTES {
            return Err(format!(
                "agent '{}': its instructions do not fit the system prompt",
                self.def.id
            ));
        }
        Ok(preserved.with_system_prompt(Some(combined)))
    }

    /// The budget of each run: its step cap, if it has one.
    pub(crate) fn budget(&self) -> agent_runtime::AgentBudget {
        self.def
            .max_steps
            .map_or(agent_runtime::AgentBudget::unlimited(), |steps| {
                agent_runtime::AgentBudget::unlimited().with_max_steps(steps)
            })
    }

    /// Use the definition's reasoning effort on the plan's models, under the
    /// floors that still hold over it: the reminders' and the managed policy's.
    pub(crate) fn apply_effort(
        &self,
        plan: &mut crate::interactive::ModelPlan,
        reminder_floor: agent_runtime::reminders::ReminderFloor,
        env: &[(String, String)],
    ) -> Result<(), String> {
        let Some(name) = &self.def.reasoning_effort else {
            return Ok(());
        };
        let effort = llm_router::ReasoningEffort::parse(name)
            .map_err(|_| format!("agent '{}': unknown reasoning effort '{name}'", self.def.id))?;
        let policy_floor = crate::managed_config::load_policy(env)
            .map_err(|err| format!("agent '{}': managed policy: {err}", self.def.id))?
            .and_then(|policy| policy.min_reasoning_effort());
        let settle = |mut active: crate::user_config::ActiveModel, reminders: bool| {
            active.entry.reasoning_effort = Some(effort);
            if reminders {
                active = crate::interactive::apply_reminder_floor(active, reminder_floor);
            }
            if let Some(floor) = policy_floor
                && crate::managed_config::below_floor(active.entry.reasoning_effort, floor)
            {
                active.entry.reasoning_effort = Some(floor);
            }
            active
        };
        if let Some(first) = plan.models.first_mut() {
            *first = settle(first.clone(), true);
        }
        if let Some(config) = plan.primary_config.as_mut() {
            *config = settle(config.clone(), false);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A project, a RapidLM home and a configuration, in a directory of their own.
    struct World {
        root: PathBuf,
        project: PathBuf,
    }

    const CONFIG: &str = "[models]\ndefault = \"local\"\n\n\
        [model.local]\nprovider = \"openai-compatible\"\nmodel = \"local-model\"\n\
        base_url = \"http://127.0.0.1:9/v1\"\napi_key = \"k\"\n\n\
        [model.review]\nprovider = \"openai-compatible\"\nmodel = \"review-model\"\n\
        base_url = \"http://127.0.0.1:9/v1\"\napi_key = \"k\"\nreasoning_effort = \"low\"\n";

    impl World {
        fn new(tag: &str) -> Self {
            let seq = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let root = std::env::temp_dir().join(format!("rapidlm-primary-{tag}-{seq}"));
            std::fs::create_dir_all(root.join("project/.rapidlm/agents")).expect("project");
            std::fs::create_dir_all(root.join("home/agents")).expect("home");
            let root = protocol::host_path::canonicalize(&root).expect("canonical");
            Self {
                project: root.join("project"),
                root,
            }
        }

        fn env(&self, config: Option<&str>) -> Vec<(String, String)> {
            let mut env = vec![
                (
                    "RAPIDLM_HOME".to_owned(),
                    self.root.join("home").display().to_string(),
                ),
                ("HOME".to_owned(), self.root.display().to_string()),
            ];
            if let Some(text) = config {
                let path = self.root.join("config.toml");
                std::fs::write(&path, text).expect("config");
                env.push(("RAPIDLM_CONFIG".to_owned(), path.display().to_string()));
            }
            env
        }

        fn project_agent(&self, file: &str, text: &str) {
            std::fs::write(self.project.join(".rapidlm/agents").join(file), text).expect("agent");
        }

        fn user_agent(&self, file: &str, text: &str) {
            std::fs::write(self.root.join("home/agents").join(file), text).expect("agent");
        }
    }

    const LEAD: &str = "---\ndescription: Lead\nbase_role: coder\ntool_classes: read git\nmode: primary\n\
        allow_tools: repo_read repo_search task_spawn\nmax_steps: 5\nspawn: explore\n\
        permission:\n  \"repo_read(secret*)\": deny\n---\nLEAD-MARKER\n";

    fn recorded(id: &str, digest: &str) -> Recorded {
        Recorded {
            id: id.to_owned(),
            digest: digest.to_owned(),
        }
    }

    #[test]
    fn a_new_session_takes_the_flag_over_the_config_and_a_resumed_one_keeps_what_it_recorded() {
        // New: the flag, else the configured default, else nothing.
        assert_eq!(
            choose(false, Some("a"), Some("b"), None),
            Ok(Some("a".into()))
        );
        assert_eq!(choose(false, None, Some("b"), None), Ok(Some("b".into())));
        assert_eq!(choose(false, None, None, None), Ok(None));
        // Resumed: what it recorded, whatever the config says now.
        let rec = recorded("lead", "d1");
        assert_eq!(
            choose(true, None, Some("b"), Some(&rec)),
            Ok(Some("lead".into()))
        );
        assert_eq!(
            choose(true, Some("lead"), None, Some(&rec)),
            Ok(Some("lead".into()))
        );
        // A different request is refused: a session keeps its agent for its life.
        assert_eq!(
            choose(true, Some("other"), None, Some(&rec)),
            Err(PrimaryError::Fixed {
                recorded: "lead".into(),
                requested: "other".into()
            })
        );
        // A session that began without one does not acquire one, by flag or by config.
        assert_eq!(
            choose(true, Some("lead"), None, None),
            Err(PrimaryError::NoAgentYet {
                requested: "lead".into()
            })
        );
        assert_eq!(choose(true, None, Some("lead"), None), Ok(None));
    }

    #[test]
    fn an_agent_is_found_by_its_id_and_must_be_one_a_session_can_run_as() {
        let world = World::new("resolve");
        world.project_agent("lead.md", LEAD);
        world.project_agent("helper.md", "---\ndescription: H\nbase_role: coder\n---\n");
        world.user_agent(
            "either.md",
            "---\ndescription: E\nbase_role: coder\nmode: both\n---\n",
        );
        let env = world.env(None);
        let found = resolve("lead", Some(&world.project), true, &env).expect("lead");
        assert_eq!(found.id(), "lead");
        assert_eq!(found.digest().len(), 64);
        assert_eq!(found.selected_payload()["mode"], "primary");
        assert_eq!(found.selected_payload()["id"], "lead");
        assert_eq!(
            found.selected_payload()["definition_digest"],
            found.digest()
        );
        // `both` is a main agent too, wherever it is found.
        assert_eq!(
            resolve("either", Some(&world.project), true, &env)
                .expect("either")
                .selected_payload()["mode"],
            "both"
        );
        // A subagent only, and a built-in (a subagent), are not.
        for id in ["helper", "general-purpose", "explore"] {
            assert_eq!(
                resolve(id, Some(&world.project), true, &env).expect_err(id),
                PrimaryError::NotSelectable { id: id.into() },
                "{id}"
            );
        }
        // An id nobody has, or that is no id, names what can be chosen.
        for id in ["nosuch", "Not An Id", ""] {
            match resolve(id, Some(&world.project), true, &env).expect_err(id) {
                PrimaryError::Unknown {
                    requested,
                    selectable,
                } => {
                    assert_eq!(requested, id);
                    assert_eq!(selectable, ["lead", "either"], "in the order found: {id}");
                }
                other => panic!("{id}: {other:?}"),
            }
        }
        // Without a project the user's agents are what there are.
        assert!(resolve("either", None, false, &env).is_ok());
        assert!(matches!(
            resolve("lead", None, false, &env),
            Err(PrimaryError::Unknown { .. })
        ));
    }

    #[test]
    fn an_untrusted_projects_agent_is_named_as_such_and_never_read() {
        let world = World::new("untrusted");
        world.project_agent("lead.md", LEAD);
        world.project_agent("lead2.toml", "this is not even a definition");
        let env = world.env(None);
        // Not trusted: its definitions are not loaded, and saying so is the
        // answer, rather than that there is no such agent.
        assert_eq!(
            resolve("lead", Some(&world.project), false, &env).expect_err("untrusted"),
            PrimaryError::Untrusted { id: "lead".into() }
        );
        assert_eq!(
            resolve("lead2", Some(&world.project), false, &env).expect_err("untrusted"),
            PrimaryError::Untrusted { id: "lead2".into() }
        );
        // An id the project does not define is simply unknown.
        assert!(matches!(
            resolve("ghost", Some(&world.project), false, &env),
            Err(PrimaryError::Unknown { .. })
        ));
        // Trusted, it is found.
        assert!(resolve("lead", Some(&world.project), true, &env).is_ok());
        // The user's own agent is no one's to distrust.
        world.user_agent(
            "mine.md",
            "---\ndescription: M\nbase_role: coder\nmode: primary\n---\n",
        );
        assert!(resolve("mine", Some(&world.project), false, &env).is_ok());
    }

    #[test]
    fn the_model_and_effort_an_agent_names_must_be_usable_before_the_run_starts() {
        let world = World::new("model");
        world.project_agent(
            "reviewer.md",
            "---\ndescription: R\nbase_role: reviewer\nmode: primary\nmodel: review\n---\n",
        );
        world.project_agent(
            "lost.md",
            "---\ndescription: L\nbase_role: reviewer\nmode: primary\nmodel: nowhere\n---\n",
        );
        let env = world.env(Some(CONFIG));
        assert!(resolve("reviewer", Some(&world.project), true, &env).is_ok());
        match resolve("lost", Some(&world.project), true, &env).expect_err("lost") {
            PrimaryError::Model { id, reason } => {
                assert_eq!(id, "lost");
                assert!(reason.contains("nowhere"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
        // With no configuration at all, a named model cannot be had.
        let bare = world.env(None);
        match resolve("reviewer", Some(&world.project), true, &bare).expect_err("bare") {
            PrimaryError::Model { reason, .. } => {
                assert!(reason.contains("no model configuration"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
        // An agent that names none runs on whatever the session does.
        world.project_agent(
            "plain.md",
            "---\ndescription: P\nbase_role: coder\nmode: primary\n---\n",
        );
        assert!(resolve("plain", Some(&world.project), true, &bare).is_ok());
    }

    #[test]
    fn each_refusal_says_what_to_do_and_exits_as_a_usage_or_a_policy_error() {
        use crate::headless::jsonl::JsonlExitCode::{Policy, Runtime, Usage};
        let cases: Vec<(PrimaryError, crate::headless::jsonl::JsonlExitCode, &str)> = vec![
            (
                PrimaryError::Unknown {
                    requested: "x".into(),
                    selectable: vec!["a".into(), "b".into()],
                },
                Usage,
                "the agents a session can run as: a, b (rapid agents list)",
            ),
            (
                PrimaryError::Unknown {
                    requested: "x".into(),
                    selectable: vec![],
                },
                Usage,
                "the agents a session can run as: none (rapid agents list)",
            ),
            (
                PrimaryError::NotSelectable { id: "x".into() },
                Usage,
                "give it `mode: primary` or `both`",
            ),
            (
                PrimaryError::Untrusted { id: "x".into() },
                Policy,
                "rapid trust grant",
            ),
            (
                PrimaryError::Model {
                    id: "x".into(),
                    reason: "names model 'm'".into(),
                },
                Policy,
                "rapid agents list",
            ),
            (
                PrimaryError::Fixed {
                    recorded: "a".into(),
                    requested: "b".into(),
                },
                Usage,
                "start a new session to run as 'b'",
            ),
            (
                PrimaryError::NoAgentYet {
                    requested: "b".into(),
                },
                Usage,
                "began without an agent",
            ),
            (
                PrimaryError::Ledger("io".into()),
                Runtime,
                "could not be read",
            ),
        ];
        for (error, code, needle) in cases {
            assert_eq!(error.exit_code(), code, "{error}");
            assert!(error.to_string().contains(needle), "{error}");
            assert!(!error.to_string().contains('\n'), "one line: {error}");
        }
    }

    #[test]
    fn an_agents_rules_are_all_kept_beside_the_sessions_or_the_run_is_refused() {
        use crate::permissions::{
            MAX_RULES, PermissionLattice, PermissionMode, RuleEffect, ToolPattern, ToolRule,
        };
        let world = World::new("rules");
        world.project_agent("lead.md", LEAD);
        let agent = resolve("lead", Some(&world.project), true, &world.env(None)).expect("lead");
        let holding = |n: usize| {
            PermissionLattice::new(PermissionMode::Default).with_rules(
                (0..n)
                    .map(|k| ToolRule {
                        effect: RuleEffect::Ask,
                        pattern: ToolPattern::parse(&format!("repo_read(p{k}*)")).expect("rule"),
                    })
                    .collect(),
            )
        };
        let fitted = agent
            .permissions(holding(MAX_RULES - 1))
            .expect("one rule fits");
        assert_eq!(fitted.rules().len(), MAX_RULES);
        assert!(fitted.rules().iter().any(|r| r.effect == RuleEffect::Deny));
        match agent.permissions(holding(MAX_RULES)).expect_err("full") {
            PrimaryError::Rules { id, reason } => {
                assert_eq!(id, "lead");
                assert!(reason.contains("adds 1 permission rule(s)"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
        // The agent's rule refuses; the rest is as it was.
        let lattice = agent
            .permissions(PermissionLattice::new(PermissionMode::BypassPermissions))
            .expect("fits");
        assert!(matches!(
            lattice.evaluate(
                "repo_read",
                "secret.txt",
                crate::permissions::ToolClass::ReadOnly
            ),
            crate::permissions::Decision::Deny(_)
        ));
        assert!(matches!(
            lattice.evaluate(
                "repo_read",
                "a.txt",
                crate::permissions::ToolClass::ReadOnly
            ),
            crate::permissions::Decision::Allow(_)
        ));
    }

    #[test]
    fn an_id_from_outside_is_shown_as_one_clean_short_line() {
        let loud = "a\u{1b}[31m\nb\u{202e}c".to_owned() + &"x".repeat(100);
        for error in [
            PrimaryError::Unknown {
                requested: loud.clone(),
                selectable: vec![],
            },
            PrimaryError::Fixed {
                recorded: loud.clone(),
                requested: loud.clone(),
            },
            PrimaryError::NoAgentYet {
                requested: loud.clone(),
            },
        ] {
            let text = error.to_string();
            assert!(
                !text.contains('\u{1b}') && !text.contains('\n') && !text.contains('\u{202e}'),
                "{text:?}"
            );
            assert!(text.contains("a[31m bcx"), "{text}");
            assert!(text.len() < 400, "{}", text.len());
        }
    }

    #[test]
    fn plan_mode_still_wins_over_an_agent_that_can_write() {
        use crate::permissions::{Decision, PermissionLattice, PermissionMode, ToolClass};
        let world = World::new("plan");
        world.project_agent(
            "writer.md",
            "---\ndescription: W\nbase_role: coder\ntool_classes: read write\nmode: primary\n\
             allow_tools: workspace_write repo_read\npermission:\n  \"repo_read(secret*)\": deny\n---\n",
        );
        let agent =
            resolve("writer", Some(&world.project), true, &world.env(None)).expect("writer");
        let planning = agent
            .permissions(PermissionLattice::new(PermissionMode::Plan))
            .expect("fits");
        // The mode is the session's; the agent's rules came on top of it.
        assert_eq!(planning.mode(), PermissionMode::Plan);
        assert!(
            !matches!(
                planning.evaluate("workspace_write", "src/a.rs", ToolClass::FileEdit),
                Decision::Allow(_)
            ),
            "a write outside the plan is not allowed in plan mode, agent or not"
        );
        assert!(matches!(
            planning.evaluate("repo_read", "secret.txt", ToolClass::ReadOnly),
            Decision::Deny(_)
        ));
        // The same agent in a mode that allows the write: allowed, as the
        // definition would have it.
        let working = agent
            .permissions(PermissionLattice::new(PermissionMode::BypassPermissions))
            .expect("fits");
        assert!(matches!(
            working.evaluate("workspace_write", "src/a.rs", ToolClass::FileEdit),
            Decision::Allow(_)
        ));
    }

    #[test]
    fn an_agents_instructions_follow_the_hosts_prompt_and_its_budget_is_its_step_cap() {
        let world = World::new("overlay");
        world.project_agent("lead.md", LEAD);
        world.project_agent(
            "quiet.md",
            "---\ndescription: Q\nbase_role: coder\nmode: both\n---\n",
        );
        let env = world.env(None);
        let lead = resolve("lead", Some(&world.project), true, &env).expect("lead");
        let context = || {
            crate::host::PreservedLiveContext::new("goal", Vec::new(), "", "", 10_000, 1_000)
                .expect("context")
                .with_system_prompt(Some("HOST PROMPT".to_owned()))
        };
        let overlaid = lead.overlay(context()).expect("overlay");
        assert_eq!(
            overlaid.system_prompt(),
            Some("HOST PROMPT\n\n## Main agent 'lead'\nLEAD-MARKER")
        );
        // An agent with no instructions leaves the prompt alone.
        let quiet = resolve("quiet", Some(&world.project), true, &env).expect("quiet");
        assert_eq!(
            quiet.overlay(context()).expect("same").system_prompt(),
            Some("HOST PROMPT")
        );
        // Instructions that do not fit the prompt are refused, not cut.
        let mut big = lead.clone();
        big.def.instructions = Some("x".repeat(crate::host::MAX_SYSTEM_PROMPT_BLOCK_BYTES));
        assert!(
            big.overlay(context())
                .expect_err("too big")
                .contains("do not fit")
        );
        // The step cap is the budget; none is none.
        assert_eq!(lead.budget().max_steps(), Some(5));
        assert_eq!(quiet.budget().max_steps(), None);
    }

    #[test]
    fn an_agents_model_and_effort_are_used_under_the_floors_that_hold_over_them() {
        use agent_runtime::reminders::ReminderFloor;
        let world = World::new("effort");
        world.project_agent(
            "reviewer.md",
            "---\ndescription: R\nbase_role: reviewer\nmode: primary\nmodel: review\n\
             reasoning_effort: high\n---\n",
        );
        world.project_agent(
            "thrifty.md",
            "---\ndescription: T\nbase_role: reviewer\nmode: primary\nreasoning_effort: low\n---\n",
        );
        let env = world.env(Some(CONFIG));
        let plan_for = |agent: &PrimaryAgent, floor: ReminderFloor| {
            let mut plan = crate::interactive::resolve_model_plan_with_override(
                &env,
                floor,
                &mut |_| {},
                agent.model(),
            )
            .expect("plan");
            agent.apply_effort(&mut plan, floor, &env).expect("effort");
            plan
        };
        let reviewer = resolve("reviewer", Some(&world.project), true, &env).expect("reviewer");
        assert_eq!(reviewer.model(), Some("review"));
        let plan = plan_for(&reviewer, ReminderFloor::Baseline);
        // Its model, and its effort in place of the profile's own.
        assert_eq!(plan.models[0].entry.model, "review-model");
        assert_eq!(
            plan.models[0].entry.reasoning_effort,
            Some(llm_router::ReasoningEffort::High)
        );
        assert_eq!(
            plan.primary_config
                .as_ref()
                .expect("config")
                .entry
                .reasoning_effort,
            Some(llm_router::ReasoningEffort::High),
            "what a child inherits carries it too"
        );
        // A floor the reminders set still holds under a lower effort; the
        // config's copy, which has not had the reminders' floor, does not.
        let thrifty = resolve("thrifty", Some(&world.project), true, &env).expect("thrifty");
        let plan = plan_for(&thrifty, ReminderFloor::High);
        assert_eq!(
            plan.models[0].entry.model, "local-model",
            "it names no model"
        );
        assert_eq!(
            plan.models[0].entry.reasoning_effort,
            Some(llm_router::ReasoningEffort::High)
        );
        assert_eq!(
            plan.primary_config
                .as_ref()
                .expect("config")
                .entry
                .reasoning_effort,
            Some(llm_router::ReasoningEffort::Low)
        );
        // No effort named: the plan is as it was.
        world.project_agent(
            "plain.md",
            "---\ndescription: P\nbase_role: coder\nmode: primary\n---\n",
        );
        let plain = resolve("plain", Some(&world.project), true, &env).expect("plain");
        let mut plan = plan_for(&plain, ReminderFloor::Baseline);
        let before = plan.models[0].entry.reasoning_effort;
        plain
            .apply_effort(&mut plan, ReminderFloor::Baseline, &env)
            .expect("effort");
        assert_eq!(plan.models[0].entry.reasoning_effort, before);
    }

    #[test]
    fn a_choice_is_recorded_when_new_or_changed_and_a_changed_one_is_warned_of() {
        let world = World::new("select");
        world.project_agent("lead.md", LEAD);
        let env = world.env(None);
        let root = Some(world.project.as_path());
        let digest = resolve("lead", root, true, &env).expect("lead").digest();
        // Nothing asked, nothing configured: the plain main role.
        assert!(
            select(None, false, || panic!("not resumed"), root, true, &env)
                .expect("none")
                .is_none()
        );
        // A new session records its choice.
        let fresh = select(
            Some("lead"),
            false,
            || panic!("not resumed"),
            root,
            true,
            &env,
        )
        .expect("fresh")
        .expect("selected");
        assert!(fresh.record && fresh.changed_from.is_none() && fresh.changed_warning().is_none());
        // A resumed session whose agent is unchanged records nothing.
        let same = select(
            None,
            true,
            || Ok(Some(recorded("lead", &digest))),
            root,
            true,
            &env,
        )
        .expect("same")
        .expect("selected");
        assert!(!same.record && same.changed_warning().is_none());
        // One whose agent has changed uses the current definition, says so in
        // one line, and records the new fingerprint.
        let changed = select(
            None,
            true,
            || Ok(Some(recorded("lead", "0".repeat(64).as_str()))),
            root,
            true,
            &env,
        )
        .expect("changed")
        .expect("selected");
        assert!(changed.record);
        assert_eq!(
            changed.changed_from.as_deref(),
            Some("0".repeat(64).as_str())
        );
        let warning = changed.changed_warning().expect("warning");
        assert_eq!(
            warning,
            "warning: agent 'lead' has changed since this session recorded it; the current definition is used"
        );
        // A resumed session with no record and no request is the main role,
        // even when the config now names a default.
        let configured = world.env(Some(
            "[models]\ndefault = \"local\"\n[agents]\ndefault_primary = \"lead\"\n",
        ));
        assert!(
            select(None, true, || Ok(None), root, true, &configured)
                .expect("none")
                .is_none()
        );
        // A new session takes the configured default; the flag wins over it.
        let by_config = select(None, false, || panic!("no"), root, true, &configured)
            .expect("config")
            .expect("selected");
        assert_eq!(by_config.agent.id(), "lead");
        world.user_agent(
            "mine.md",
            "---\ndescription: M\nbase_role: coder\nmode: primary\n---\n",
        );
        let by_flag = select(
            Some("mine"),
            false,
            || panic!("no"),
            root,
            true,
            &configured,
        )
        .expect("flag")
        .expect("selected");
        assert_eq!(by_flag.agent.id(), "mine");
        // A recorded agent that is gone is an error, never the main role.
        let gone = select(
            None,
            true,
            || Ok(Some(recorded("vanished", &digest))),
            root,
            true,
            &env,
        )
        .expect_err("gone");
        assert!(matches!(gone, PrimaryError::Unknown { .. }), "{gone}");
    }
}
