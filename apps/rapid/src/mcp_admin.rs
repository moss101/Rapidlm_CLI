//! `rapid mcp list|get|add|remove|probe`: the management control plane for a
//! project's MCP servers.
//!
//! `CLI_USAGE` has advertised `rapid mcp …` since the CLI existed, but
//! `run_subcommand` had no `mcp` arm at all: the command printed the generic
//! top-level usage on stderr and exited 2, with nothing to say that the
//! feature was unimplemented rather than mistyped. Meanwhile the *runtime*
//! half was real — `load_project_integrations` -> `register_mcp_servers` ->
//! `execute_mcp_tool` — so a user's only way to find out whether their
//! `mcpServers` entry worked was to start a turn and see whether a tool
//! showed up.
//!
//! Every command here reads through [`crate::mcp_config`], the same loader
//! the turn path uses, so this can never report a server the turn would not
//! register (or hide one it would). `probe` goes further and runs
//! [`crate::exec_tools::connect_mcp_server`] — literally the function that
//! brings a server up for a real turn — rather than a management-only
//! imitation of it.
//!
//! Boundaries:
//!
//! * `probe` executes a project-declared command, so it requires the project
//!   to be trusted, exactly as registration does. It fails closed on an
//!   unreadable trust catalog.
//! * `list`/`get` are read-only and never spawn anything.
//! * `add`/`remove` edit project settings files and nothing else. They are
//!   reachable only from this process's argv — no model tool, slash command,
//!   or autonomous-goal path dispatches a subcommand — which is what makes
//!   it acceptable for them to write executable configuration at all.

use std::path::{Path, PathBuf};

use kernel::{CancellationToken, ProjectIdentity, ProjectTrustStore, TrustStatus};

use crate::exec_tools::McpServerConfig;
use crate::interactive::{
    PROJECT_SETTINGS_FILES, TRUST_CATALOG_NAME, resolve_project_root, user_home_from,
};
use crate::mcp_config::{
    MAX_ARG_BYTES, MAX_ARGS, MAX_COMMAND_BYTES, MAX_ENV_VALUE_BYTES, MAX_ENV_VARS,
    McpProjectConfig, label, load_project_mcp, validate_server_name,
};

/// The settings file `rapid mcp add` creates entries in. `.claude/settings.json`
/// is read for compatibility and never *added to* — it belongs to another
/// tool — but `remove` does delete from it, because a `remove` that left the
/// server running would be worse than one that edits a shared file.
pub const WRITE_TARGET: &str = PROJECT_SETTINGS_FILES[0];

/// Cap on how many of a probed server's tools are named on its report line.
/// The count is always exact; only the listing is bounded.
const MAX_PROBE_TOOLS_LISTED: usize = 24;

/// Mode for a settings file this command creates. It can carry an MCP
/// server's `env`, i.e. API tokens, so a newly created one is owner-only.
/// An existing file keeps whatever mode it already had — tightening a file
/// the user or another tool deliberately made group-readable is not this
/// command's call.
#[cfg(unix)]
const NEW_SETTINGS_MODE: u32 = 0o600;

/// Process inputs, injectable so the whole command is testable without
/// changing the test process's working directory or environment — the same
/// shape [`crate::doctor::DoctorEnv`] uses.
pub struct McpEnv {
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
}

impl McpEnv {
    pub fn from_process() -> Self {
        Self {
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            env: std::env::vars().collect(),
        }
    }
}

/// What a command produced: text for stdout and the process exit code.
///
/// Returned rather than printed so every behavior below is assertable
/// without capturing a stream, following `doctor::DoctorReport`'s split
/// between deciding and rendering.
#[derive(Debug, PartialEq, Eq)]
pub struct McpOutcome {
    pub text: String,
    pub exit: i32,
}

/// A malformed invocation. The caller prints [`MCP_USAGE`] and exits 2, so
/// this never reaches stdout.
#[derive(Debug, PartialEq, Eq)]
pub struct McpUsageError(pub String);

pub const MCP_USAGE: &str = "\
usage: rapid mcp list|get|add|remove|probe

Manage the MCP servers this project configures under `mcpServers` in
.rapidlm/settings.json (and .claude/settings.json, which is read for
compatibility; `add` only ever creates entries in .rapidlm/settings.json,
but `remove` deletes from whichever files define the server).

At most 8 servers run per project. When more are configured, the ones that
fit are chosen by settings-file order and then by ascending server name --
not by the order they appear in the file -- and the rest are listed as
rejected rather than dropped.

Commands:
  list                    Every server this project configures, plus every
                          entry that was rejected and the reason. Read-only:
                          no server is started.
  get <name>              One server's full configuration. Environment
                          variable names are shown; values never are.
  add <name> --command <program> [--arg <value>]... [--env KEY=VALUE]...
                          Add a stdio server to .rapidlm/settings.json.
                          Refuses to overwrite an existing entry without
                          --force. Repeat --arg and --env per value; --arg
                          order is argv order.
  remove <name>           Remove a server from every project settings file
                          that defines it, including .claude/settings.json:
                          a remove that left the server running would be
                          worse than one that edits a shared file.
  probe [<name>]          Start the configured server(s) for real, run the
                          MCP initialize handshake, and list the tools they
                          advertise — the same spawn, environment, and
                          handshake a turn performs. Requires a trusted
                          project, because it executes project-declared
                          commands. Servers are probed one at a time and
                          one that never answers costs up to 30 seconds
                          each, so a fully unresponsive project takes
                          minutes.

  -h, --help              Print this help

Only stdio servers are supported: an entry with `type`/`url` and no
`command` is reported as an unsupported remote transport rather than being
silently ignored.

`add` and `remove` rewrite the settings file as pretty-printed JSON. Every
other key's value is preserved, but object key order and the original
indentation are not; a file this command cannot parse is refused rather than
rewritten, so edit that one by hand. A settings file this command creates is
owner-only (0600), because an `env` value is usually a token; one that
already exists keeps the mode it has.

Exit code:
  0   the command succeeded (`list` succeeds even when entries were rejected)
  1   the requested server does not exist, nothing was removed, a write
      failed, or a probed server did not come up. A settings file that could
      not be read is a warning, not a failure, when the command otherwise
      did what was asked.
  2   usage error
";

/// Dispatch one `rapid mcp` invocation.
pub fn run(args: &[String], env: &McpEnv) -> Result<McpOutcome, McpUsageError> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        return Ok(McpOutcome {
            text: MCP_USAGE.to_owned(),
            exit: 0,
        });
    }
    let Some(sub) = args.first().map(String::as_str) else {
        return Err(McpUsageError("rapid mcp: no command given".to_owned()));
    };
    let rest = &args[1..];
    let project = resolve(env)?;
    match sub {
        "list" => {
            expect_no_args("list", rest)?;
            Ok(list(&project))
        }
        "get" => {
            let name = one_name("get", rest)?;
            Ok(get(&project, &name))
        }
        "add" => add(&project, rest),
        "remove" => {
            let name = one_name("remove", rest)?;
            Ok(remove(&project, &name))
        }
        "probe" => {
            if rest.len() > 1 {
                return Err(McpUsageError(
                    "rapid mcp probe: expected at most one server name".to_owned(),
                ));
            }
            if let Some(flag) = rest.iter().find(|arg| arg.starts_with('-')) {
                return Err(McpUsageError(format!(
                    "rapid mcp probe: unexpected option '{flag}'"
                )));
            }
            Ok(probe(&project, rest.first().map(String::as_str)))
        }
        other => Err(McpUsageError(format!("rapid mcp: unknown command '{other}'"))),
    }
}

/// The project this command operates on, resolved exactly the way every
/// trust check and `rapid doctor` resolve it.
struct Project {
    root: PathBuf,
    trust: Result<TrustStatus, String>,
    config: McpProjectConfig,
}

fn resolve(env: &McpEnv) -> Result<Project, McpUsageError> {
    let cancel = CancellationToken::new();
    let found = resolve_project_root(&env.cwd, &cancel)
        .map_err(|reason| McpUsageError(format!("rapid mcp: {reason}")))?;
    let trust = trust_of(&found.root, &env.env, &cancel);
    let config = load_project_mcp(&found.root);
    Ok(Project {
        root: found.root,
        trust,
        config,
    })
}

fn trust_of(
    root: &Path,
    env: &[(String, String)],
    cancel: &CancellationToken,
) -> Result<TrustStatus, String> {
    let Some(home) = user_home_from(env) else {
        return Err("no RapidLM home directory could be resolved".to_owned());
    };
    let identity = ProjectIdentity::new(root, None)
        .map_err(|err| format!("project identity could not be derived: {err}"))?;
    ProjectTrustStore::open(home.join(TRUST_CATALOG_NAME))
        .get(&identity, cancel)
        .map_err(|err| format!("trust state could not be read: {err}"))
}

fn trust_word(trust: &Result<TrustStatus, String>) -> String {
    match trust {
        Ok(status) => status.as_str().to_owned(),
        Err(reason) => format!("unreadable ({})", label(reason)),
    }
}

// --- list ---------------------------------------------------------------

fn list(project: &Project) -> McpOutcome {
    let mut text = String::new();
    text.push_str(&header(project));
    let servers = project.config.servers();
    let rejections = project.config.rejections();
    text.push_str(&format!(
        "servers={} rejected={}\n",
        servers.len(),
        project.config.rejections_total()
    ));
    for entry in servers {
        text.push_str(&format!(
            "server={} file={} command={} args={} env={}\n",
            entry.config.name,
            entry.file,
            quoted(&entry.config.command),
            entry.config.args.len(),
            env_keys(&entry.config),
        ));
    }
    for rejection in rejections {
        text.push_str(&format!(
            "rejected={} file={} reason={}\n",
            rejection.name, rejection.file, rejection.issue
        ));
    }
    if project.config.rejections_omitted() > 0 {
        text.push_str(&format!(
            "note: {} further rejected entry/entries not listed (report is capped at {})\n",
            project.config.rejections_omitted(),
            crate::mcp_config::MAX_REPORTED_REJECTIONS
        ));
    }
    if servers.is_empty() && rejections.is_empty() {
        text.push_str(&format!(
            "note: no `mcpServers` entry in {}\n",
            PROJECT_SETTINGS_FILES.join(" or ")
        ));
    }
    if !servers.is_empty() && !matches!(project.trust, Ok(TrustStatus::Trusted)) {
        text.push_str(
            "note: this project is not trusted, so no configured server is registered for a \
turn; run `rapid trust grant` here to enable them\n",
        );
    }
    // Rejections are reported, not fatal: `list` answering the question it
    // was asked is a success even when the answer is "three of these are
    // broken". `rapid mcp probe` is the command that fails on a bad server.
    McpOutcome { text, exit: 0 }
}

fn header(project: &Project) -> String {
    let present = project.config.files_present();
    format!(
        "project={} trust={}\nsettings={}\n",
        project.root.display(),
        trust_word(&project.trust),
        if present.is_empty() {
            "none".to_owned()
        } else {
            present.join(",")
        },
    )
}

/// Env *names* only. A value is very often an API token, and a management
/// command must not be the thing that prints it to a terminal or a CI log.
fn env_keys(config: &McpServerConfig) -> String {
    if config.env.is_empty() {
        return "-".to_owned();
    }
    config
        .env
        .iter()
        .map(|(key, _)| key.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

/// Quote anything whose spaces or control characters would otherwise make a
/// `key=value` line ambiguous.
fn quoted(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|ch| !ch.is_whitespace() && !ch.is_control())
    {
        return value.to_owned();
    }
    format!("{value:?}")
}

// --- get ----------------------------------------------------------------

fn get(project: &Project, name: &str) -> McpOutcome {
    let mut text = header(project);
    if let Some(entry) = project.config.get(name) {
        text.push_str(&format!("server={} file={}\n", entry.config.name, entry.file));
        text.push_str(&format!("command={}\n", quoted(&entry.config.command)));
        for (index, arg) in entry.config.args.iter().enumerate() {
            text.push_str(&format!("arg[{index}]={}\n", quoted(arg)));
        }
        for (key, _) in &entry.config.env {
            text.push_str(&format!("env={key}\n"));
        }
        for tool in wire_names(&entry.config) {
            text.push_str(&format!("tool-prefix={tool}\n"));
        }
        return McpOutcome { text, exit: 0 };
    }
    text.push_str(&unknown_server_line(project, name));
    McpOutcome { text, exit: 1 }
}

/// What to say about a name that is not a usable server.
///
/// A name the user typed that was *rejected* is the interesting case: saying
/// "not configured" would be actively misleading, because it is configured —
/// it just will not run.
fn unknown_server_line(project: &Project, name: &str) -> String {
    match project
        .config
        .rejections()
        .iter()
        .find(|rejection| rejection.name == name)
    {
        Some(rejection) => format!(
            "rejected={} file={} reason={}\n",
            rejection.name, rejection.file, rejection.issue
        ),
        None => format!("error: no MCP server named {name:?} in this project\n"),
    }
}

/// The `mcp__<server>__` prefix every tool of this server is exposed under.
fn wire_names(config: &McpServerConfig) -> Vec<String> {
    vec![format!("mcp__{}__*", config.name)]
}

// --- add / remove -------------------------------------------------------

fn add(project: &Project, args: &[String]) -> Result<McpOutcome, McpUsageError> {
    let mut name: Option<String> = None;
    let mut command: Option<String> = None;
    let mut argv: Vec<String> = Vec::new();
    let mut env: Vec<(String, String)> = Vec::new();
    let mut force = false;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--command" | "--arg" | "--env" => {
                let Some(value) = args.get(index + 1) else {
                    return Err(McpUsageError(format!("rapid mcp add: {arg} needs a value")));
                };
                match arg {
                    "--command" => {
                        if command.is_some() {
                            return Err(McpUsageError(
                                "rapid mcp add: --command given more than once".to_owned(),
                            ));
                        }
                        command = Some(value.clone());
                    }
                    "--arg" => argv.push(value.clone()),
                    _ => {
                        // Deliberately does not echo the operand: the typo
                        // this catches — a bare value with no `KEY=` — is
                        // exactly the case where the operand is the secret,
                        // and an error line lands in shell history and CI
                        // logs.
                        let Some((key, val)) = value.split_once('=') else {
                            return Err(McpUsageError(
                                "rapid mcp add: --env expects KEY=VALUE".to_owned(),
                            ));
                        };
                        if !crate::mcp_config::is_env_key(key) {
                            return Err(McpUsageError(format!(
                                "rapid mcp add: {key:?} is not a valid environment variable name"
                            )));
                        }
                        if val.len() > MAX_ENV_VALUE_BYTES {
                            return Err(McpUsageError(format!(
                                "rapid mcp add: value for {key:?} is over the \
{MAX_ENV_VALUE_BYTES}-byte limit"
                            )));
                        }
                        env.push((key.to_owned(), val.to_owned()));
                    }
                }
                index += 2;
            }
            "--force" => {
                force = true;
                index += 1;
            }
            other if other.starts_with('-') => {
                return Err(McpUsageError(format!(
                    "rapid mcp add: unexpected option '{other}'"
                )));
            }
            other => {
                if name.is_some() {
                    return Err(McpUsageError(format!(
                        "rapid mcp add: unexpected argument '{other}'"
                    )));
                }
                name = Some(other.to_owned());
                index += 1;
            }
        }
    }
    let Some(name) = name else {
        return Err(McpUsageError("rapid mcp add: no server name given".to_owned()));
    };
    let Some(command) = command else {
        return Err(McpUsageError(
            "rapid mcp add: --command <program> is required".to_owned(),
        ));
    };
    // The same rules the loader applies, so a write can never produce an
    // entry `rapid mcp list` would immediately report as rejected.
    if let Err(issue) = validate_server_name(&name) {
        return Err(McpUsageError(format!("rapid mcp add: {issue}")));
    }
    if command.is_empty() {
        return Err(McpUsageError("rapid mcp add: --command is empty".to_owned()));
    }
    if command.len() > MAX_COMMAND_BYTES {
        return Err(McpUsageError(format!(
            "rapid mcp add: --command is over the {MAX_COMMAND_BYTES}-byte limit"
        )));
    }
    if argv.len() > MAX_ARGS {
        return Err(McpUsageError(format!(
            "rapid mcp add: {} args, over the {MAX_ARGS} limit",
            argv.len()
        )));
    }
    if let Some(index) = argv.iter().position(|arg| arg.len() > MAX_ARG_BYTES) {
        return Err(McpUsageError(format!(
            "rapid mcp add: --arg #{index} is over the {MAX_ARG_BYTES}-byte limit"
        )));
    }
    if env.len() > MAX_ENV_VARS {
        return Err(McpUsageError(format!(
            "rapid mcp add: {} env vars, over the {MAX_ENV_VARS} limit",
            env.len()
        )));
    }

    let target = project.root.join(WRITE_TARGET);
    let mut document = match read_settings(&target) {
        Ok(document) => document,
        Err(reason) => {
            return Ok(McpOutcome {
                text: format!("error: {reason}\n"),
                exit: 1,
            });
        }
    };
    let servers = match ensure_servers_object(&mut document) {
        Ok(servers) => servers,
        Err(reason) => {
            return Ok(McpOutcome {
                text: format!("error: {} {reason}\n", target.display()),
                exit: 1,
            });
        }
    };
    if servers.contains_key(&name) && !force {
        return Ok(McpOutcome {
            text: format!(
                "error: {} already configures a server named {name:?}; pass --force to replace it\n",
                target.display()
            ),
            exit: 1,
        });
    }
    let replaced = servers.contains_key(&name);
    let mut entry = serde_json::Map::new();
    entry.insert("command".to_owned(), serde_json::Value::String(command));
    if !argv.is_empty() {
        entry.insert(
            "args".to_owned(),
            serde_json::Value::Array(argv.into_iter().map(serde_json::Value::String).collect()),
        );
    }
    if !env.is_empty() {
        let mut map = serde_json::Map::new();
        for (key, value) in env {
            map.insert(key, serde_json::Value::String(value));
        }
        entry.insert("env".to_owned(), serde_json::Value::Object(map));
    }
    servers.insert(name.clone(), serde_json::Value::Object(entry));

    if let Err(reason) = write_settings(&target, &document) {
        return Ok(McpOutcome {
            text: format!("error: {reason}\n"),
            exit: 1,
        });
    }
    let mut text = format!(
        "{} {name} in {}\n",
        if replaced { "replaced" } else { "added" },
        target.display()
    );
    // Re-read through the real loader instead of asserting success: if the
    // merged, capped, deduplicated view still would not run this server, the
    // user needs to hear it now, not at the next turn.
    let reloaded = load_project_mcp(&project.root);
    // Keyed on "did this name end up usable", not on "is there a rejection
    // mentioning this name". A name the *other* settings file also defines
    // produces a `Shadowed` rejection for that file even though the entry
    // just written is the one that wins — matching by name alone reported a
    // perfectly working server as broken.
    if reloaded.get(&name).is_none()
        && let Some(rejection) = reloaded
            .rejections()
            .iter()
            .find(|rejection| rejection.name == name)
    {
        text.push_str(&format!(
            "warning: {name} will still not run: {}\n",
            rejection.issue
        ));
    }
    if !matches!(project.trust, Ok(TrustStatus::Trusted)) {
        text.push_str(
            "note: this project is not trusted, so no configured server is registered for a \
turn; run `rapid trust grant` here to enable them\n",
        );
    }
    Ok(McpOutcome { text, exit: 0 })
}

fn remove(project: &Project, name: &str) -> McpOutcome {
    let mut text = String::new();
    let mut removed_from: Vec<String> = Vec::new();
    // A file this command could not read or understand is a different thing
    // from a write that failed. A commented `.claude/settings.json` is legal
    // for the tool that owns it and usually has nothing to do with the
    // server being removed, so it must not turn a successful removal into a
    // non-zero exit — it is reported, and only stands in for a real failure
    // when nothing was removed anywhere.
    let mut unusable: Vec<String> = Vec::new();
    let mut write_failures: Vec<String> = Vec::new();
    for file_name in PROJECT_SETTINGS_FILES {
        let target = project.root.join(file_name);
        if !target.exists() {
            continue;
        }
        let mut document = match read_settings(&target) {
            Ok(document) => document,
            Err(reason) => {
                unusable.push(reason);
                continue;
            }
        };
        let servers = match document.as_object_mut() {
            None => {
                unusable.push(format!(
                    "{} is not a JSON object at the top level",
                    target.display()
                ));
                continue;
            }
            Some(object) => match object.get_mut("mcpServers") {
                None => continue,
                Some(value) => match value.as_object_mut() {
                    Some(servers) => servers,
                    None => {
                        unusable.push(format!(
                            "{} has an `mcpServers` value that is not a JSON object",
                            target.display()
                        ));
                        continue;
                    }
                },
            },
        };
        if servers.remove(name).is_none() {
            continue;
        }
        match write_settings(&target, &document) {
            Ok(()) => removed_from.push(file_name.to_owned()),
            Err(reason) => write_failures.push(reason),
        }
    }
    for reason in &unusable {
        text.push_str(&format!("warning: {reason}\n"));
    }
    for reason in &write_failures {
        text.push_str(&format!("error: {reason}\n"));
    }
    if removed_from.is_empty() {
        if write_failures.is_empty() {
            text.push_str(&format!(
                "error: no MCP server named {name:?} in {}\n",
                PROJECT_SETTINGS_FILES.join(" or ")
            ));
        }
        return McpOutcome { text, exit: 1 };
    }
    text.push_str(&format!(
        "removed {} from {}\n",
        label(name),
        removed_from.join(", ")
    ));
    McpOutcome {
        text,
        exit: if write_failures.is_empty() { 0 } else { 1 },
    }
}

/// Read a settings document, or the empty object if it does not exist.
///
/// A file that exists but does not parse is an error, never an overwrite: a
/// management command must not silently replace a settings file it could not
/// understand (a JSON-with-comments `.claude/settings.json`, say).
fn read_settings(path: &Path) -> Result<serde_json::Value, String> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(serde_json::Value::Object(Default::default())),
        Ok(text) => serde_json::from_str(&text).map_err(|err| {
            format!(
                "{} is not valid JSON ({err}); fix it by hand rather than letting this command \
overwrite it",
                path.display()
            )
        }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Ok(serde_json::Value::Object(Default::default()))
        }
        Err(err) => Err(format!("{} could not be read: {err}", path.display())),
    }
}

fn ensure_servers_object(
    document: &mut serde_json::Value,
) -> Result<&mut serde_json::Map<String, serde_json::Value>, &'static str> {
    let object = document
        .as_object_mut()
        .ok_or("is not a JSON object at the top level")?;
    let entry = object
        .entry("mcpServers")
        .or_insert_with(|| serde_json::Value::Object(Default::default()));
    entry
        .as_object_mut()
        .ok_or("has an `mcpServers` value that is not a JSON object")
}

fn write_settings(path: &Path, document: &serde_json::Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("{} could not be created: {err}", parent.display()))?;
    }
    let mut bytes = serde_json::to_vec_pretty(document)
        .map_err(|err| format!("{} could not be encoded: {err}", path.display()))?;
    bytes.push(b'\n');
    // Temp-file-then-rename, the same primitive goal/evidence persistence
    // uses: a settings file must never be observed truncated. The rename
    // replaces the file, so the mode has to be carried across explicitly —
    // otherwise writing an `env` secret would relax a 0600 settings file to
    // whatever the umask allows.
    crate::exec_tools::atomic_write_with_mode(path, &bytes, settings_mode(path))
        .map_err(|err| format!("{} could not be written: {err}", path.display()))
}

/// The mode [`write_settings`] must land on: whatever the file already had,
/// or [`NEW_SETTINGS_MODE`] for one this command is creating.
#[cfg(unix)]
fn settings_mode(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(metadata) => Some(metadata.permissions().mode() & 0o7777),
        Err(_) => Some(NEW_SETTINGS_MODE),
    }
}

#[cfg(not(unix))]
fn settings_mode(_path: &Path) -> Option<u32> {
    None
}

// --- probe --------------------------------------------------------------

fn probe(project: &Project, only: Option<&str>) -> McpOutcome {
    let mut text = header(project);
    // Fail closed. Probing runs whatever `command` the project declares, so
    // it is gated on exactly the trust decision that gates registration —
    // and an unreadable catalog is a refusal, never an implied "untrusted".
    match &project.trust {
        Ok(TrustStatus::Trusted) => {}
        Ok(TrustStatus::Untrusted) => {
            text.push_str(
                "error: probing starts the configured servers, which this project is not \
trusted to do; run `rapid trust grant` here first\n",
            );
            return McpOutcome { text, exit: 1 };
        }
        Err(reason) => {
            text.push_str(&format!("error: {reason}\n"));
            return McpOutcome { text, exit: 1 };
        }
    }
    let selected: Vec<&crate::mcp_config::LoadedMcpServer> = match only {
        Some(name) => match project.config.get(name) {
            Some(entry) => vec![entry],
            None => {
                text.push_str(&unknown_server_line(project, name));
                return McpOutcome { text, exit: 1 };
            }
        },
        None => project.config.servers().iter().collect(),
    };
    if selected.is_empty() {
        text.push_str("note: no MCP server is configured in this project\n");
        return McpOutcome { text, exit: 0 };
    }
    let mut failed = 0usize;
    for entry in selected {
        match crate::exec_tools::connect_mcp_server(&entry.config) {
            Ok(connected) => {
                let mut names: Vec<&str> = connected
                    .tools
                    .iter()
                    .map(|tool| tool.name.as_str())
                    .collect();
                names.sort_unstable();
                let total = names.len();
                // A tool name is chosen by the *server*, not by the settings
                // file, so it gets the same treatment every other foreign
                // string on a report line gets: bounded, control characters
                // flattened, quoted if it still could not sit in a
                // whitespace-separated list. Listing is capped too — a
                // server advertising thousands of tools must not produce one
                // unbounded line.
                let listed: Vec<String> = names
                    .iter()
                    .take(MAX_PROBE_TOOLS_LISTED)
                    .map(|name| quoted(&format!("mcp__{}__{}", entry.config.name, label(name))))
                    .collect();
                let mut line = format!(
                    "ok={} file={} tools={total} ",
                    entry.config.name, entry.file
                );
                if listed.is_empty() {
                    line.push_str("(server advertises none)");
                } else {
                    line.push_str(&listed.join(" "));
                    if total > listed.len() {
                        line.push_str(&format!(" (+{} more)", total - listed.len()));
                    }
                }
                line.push('\n');
                text.push_str(&line);
                connected.shutdown();
            }
            Err(err) => {
                failed += 1;
                // The error text embeds the configured `command`, which came
                // from a project file: bound and flatten it so it cannot
                // forge a report row.
                text.push_str(&format!(
                    "failed={} file={} reason={}\n",
                    entry.config.name,
                    entry.file,
                    label(&err.to_string())
                ));
            }
        }
    }
    McpOutcome {
        text,
        exit: if failed == 0 { 0 } else { 1 },
    }
}

// --- shared argument helpers -------------------------------------------

fn expect_no_args(sub: &str, args: &[String]) -> Result<(), McpUsageError> {
    match args.first() {
        None => Ok(()),
        Some(unexpected) => Err(McpUsageError(format!(
            "rapid mcp {sub}: unexpected argument '{unexpected}'"
        ))),
    }
}

fn one_name(sub: &str, args: &[String]) -> Result<String, McpUsageError> {
    let Some(name) = args.first() else {
        return Err(McpUsageError(format!("rapid mcp {sub}: no server name given")));
    };
    if args.len() > 1 {
        return Err(McpUsageError(format!(
            "rapid mcp {sub}: unexpected argument '{}'",
            args[1]
        )));
    }
    if name.starts_with('-') {
        return Err(McpUsageError(format!(
            "rapid mcp {sub}: unexpected option '{name}'"
        )));
    }
    Ok(name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    /// An isolated project plus its own RapidLM home, so trust decisions in
    /// a test never touch the developer's real catalog.
    struct Fixture {
        root: PathBuf,
        project: PathBuf,
        home: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "rapidlm-mcpadmin-{name}-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = std::fs::remove_dir_all(&root);
            let project = root.join("project");
            let home = root.join("home");
            std::fs::create_dir_all(project.join(".rapidlm")).expect("project");
            std::fs::create_dir_all(&home).expect("home");
            Self {
                root,
                project,
                home,
            }
        }

        fn env(&self) -> McpEnv {
            McpEnv {
                cwd: self.project.clone(),
                env: vec![(
                    "RAPIDLM_HOME".to_owned(),
                    self.home.display().to_string(),
                )],
            }
        }

        fn settings(&self, file: &str, body: &str) {
            let path = self.project.join(file);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
            std::fs::write(path, body).expect("write settings");
        }

        fn read(&self, file: &str) -> String {
            std::fs::read_to_string(self.project.join(file)).expect("read settings")
        }

        fn set_trust(&self, status: TrustStatus) {
            let cancel = CancellationToken::new();
            let canonical = std::fs::canonicalize(&self.project).expect("canonicalize");
            let identity = ProjectIdentity::new(&canonical, None).expect("identity");
            ProjectTrustStore::open(self.home.join(TRUST_CATALOG_NAME))
                .set(&identity, status, &cancel)
                .expect("set trust");
        }

        fn run(&self, args: &[&str]) -> McpOutcome {
            run(
                &args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>(),
                &self.env(),
            )
            .expect("not a usage error")
        }

        fn usage(&self, args: &[&str]) -> McpUsageError {
            run(
                &args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>(),
                &self.env(),
            )
            .expect_err("expected a usage error")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn list_reports_usable_servers_and_every_rejected_entry() {
        let fixture = Fixture::new("list");
        fixture.settings(
            ".rapidlm/settings.json",
            r#"{"mcpServers": {
                 "ok": {"command": "true"},
                 "remote": {"type": "http", "url": "https://example.com"},
                 "bad__name": {"command": "true"}
               }}"#,
        );
        let outcome = fixture.run(&["list"]);
        assert_eq!(outcome.exit, 0, "listing is not a diagnosis: {}", outcome.text);
        assert!(outcome.text.contains("servers=1 rejected=2"), "{}", outcome.text);
        assert!(
            outcome.text.contains("server=ok file=.rapidlm/settings.json"),
            "{}",
            outcome.text
        );
        assert!(outcome.text.contains("rejected=remote"), "{}", outcome.text);
        assert!(
            outcome.text.contains("rejected=bad__name"),
            "{}",
            outcome.text
        );
        // Every rejection carries a reason: the whole point is that a
        // dropped entry stops being invisible.
        for line in outcome.text.lines().filter(|line| line.starts_with("rejected=")) {
            assert!(line.contains(" reason="), "no reason on: {line}");
        }
    }

    #[test]
    fn an_env_value_is_never_printed_by_any_read_command() {
        // An `env` value is very often an API token. `list`/`get` show the
        // key so a user can confirm the wiring, and never the value.
        const TOKEN: &str = "tok-do-not-print-me";
        let fixture = Fixture::new("secret");
        fixture.settings(
            ".rapidlm/settings.json",
            &format!(
                r#"{{"mcpServers": {{"srv": {{"command": "true", "env": {{"API_KEY": "{TOKEN}"}}}}}}}}"#
            ),
        );
        for args in [vec!["list"], vec!["get", "srv"]] {
            let outcome = fixture.run(&args);
            assert!(
                !outcome.text.contains(TOKEN),
                "{args:?} leaked the env value:\n{}",
                outcome.text
            );
            assert!(
                outcome.text.contains("API_KEY"),
                "{args:?} should still name the key:\n{}",
                outcome.text
            );
        }
    }

    #[test]
    fn get_on_a_rejected_entry_explains_it_rather_than_claiming_it_is_absent() {
        let fixture = Fixture::new("getrejected");
        fixture.settings(
            ".rapidlm/settings.json",
            r#"{"mcpServers": {"remote": {"type": "sse", "url": "https://example.com"}}}"#,
        );
        let outcome = fixture.run(&["get", "remote"]);
        assert_eq!(outcome.exit, 1);
        assert!(outcome.text.contains("rejected=remote"), "{}", outcome.text);
        assert!(
            outcome.text.contains("stdio only"),
            "the reason must be the real one: {}",
            outcome.text
        );
        assert!(
            !outcome.text.contains("no MCP server named"),
            "a configured-but-rejected entry is not absent: {}",
            outcome.text
        );
    }

    #[test]
    fn get_on_an_unknown_name_fails() {
        let fixture = Fixture::new("getmissing");
        let outcome = fixture.run(&["get", "nope"]);
        assert_eq!(outcome.exit, 1);
        assert!(
            outcome.text.contains("no MCP server named \"nope\""),
            "{}",
            outcome.text
        );
    }

    #[test]
    fn add_writes_a_loadable_entry_and_preserves_unrelated_settings() {
        let fixture = Fixture::new("add");
        fixture.settings(
            ".rapidlm/settings.json",
            r#"{"fetch_allowlist": ["example.com"]}"#,
        );
        let outcome = fixture.run(&[
            "add", "srv", "--command", "npx", "--arg", "-y", "--arg", "pkg", "--env",
            "API_KEY=abc",
        ]);
        assert_eq!(outcome.exit, 0, "{}", outcome.text);
        // Key order and indentation are explicitly *not* preserved (the
        // document is re-serialized), so this checks what is actually
        // promised: every unrelated key keeps its value.
        let written = fixture.read(".rapidlm/settings.json");
        let parsed: serde_json::Value =
            serde_json::from_str(&written).expect("the rewritten file is valid JSON");
        assert_eq!(
            parsed.get("fetch_allowlist"),
            Some(&serde_json::json!(["example.com"])),
            "unrelated settings were dropped or altered:\n{written}"
        );
        // Round-trips through the real loader: an `add` that produced an
        // entry the turn path would reject would be worse than no command.
        let loaded = fixture.run(&["get", "srv"]);
        assert_eq!(loaded.exit, 0, "{}", loaded.text);
        assert!(loaded.text.contains("command=npx"), "{}", loaded.text);
        assert!(loaded.text.contains("arg[0]=-y"), "{}", loaded.text);
        assert!(loaded.text.contains("arg[1]=pkg"), "{}", loaded.text);
        assert!(loaded.text.contains("env=API_KEY"), "{}", loaded.text);
    }

    #[test]
    fn add_refuses_a_name_the_loader_would_reject() {
        // Otherwise `rapid mcp add` would happily write an entry that
        // `rapid mcp list` reports as broken one line later.
        let fixture = Fixture::new("addbadname");
        let err = fixture.usage(&["add", "bad__name", "--command", "true"]);
        assert!(err.0.contains("'__'"), "{}", err.0);
        assert!(
            !fixture.project.join(".rapidlm/settings.json").exists(),
            "a rejected add must not create a settings file"
        );
    }

    #[test]
    fn add_will_not_replace_an_existing_entry_without_force() {
        let fixture = Fixture::new("addforce");
        fixture.settings(
            ".rapidlm/settings.json",
            r#"{"mcpServers": {"srv": {"command": "original"}}}"#,
        );
        let outcome = fixture.run(&["add", "srv", "--command", "replacement"]);
        assert_eq!(outcome.exit, 1);
        assert!(outcome.text.contains("--force"), "{}", outcome.text);
        assert!(
            fixture.read(".rapidlm/settings.json").contains("original"),
            "the existing entry must survive a refused add"
        );

        let forced = fixture.run(&["add", "srv", "--command", "replacement", "--force"]);
        assert_eq!(forced.exit, 0, "{}", forced.text);
        assert!(forced.text.starts_with("replaced srv"), "{}", forced.text);
        assert!(fixture.read(".rapidlm/settings.json").contains("replacement"));
    }

    #[test]
    fn add_refuses_to_overwrite_a_settings_file_it_could_not_parse() {
        // A `.claude/settings.json` with comments, a half-written file, a
        // file another tool owns: none of them may be silently replaced by a
        // document built from `{}`.
        let fixture = Fixture::new("addbroken");
        fixture.settings(".rapidlm/settings.json", "{ // a comment\n  \"a\": 1 }");
        let outcome = fixture.run(&["add", "srv", "--command", "true"]);
        assert_eq!(outcome.exit, 1);
        assert!(outcome.text.contains("not valid JSON"), "{}", outcome.text);
        assert!(
            fixture.read(".rapidlm/settings.json").contains("// a comment"),
            "the unparsable file must be left exactly as it was"
        );
    }

    #[test]
    fn add_warns_when_the_written_entry_still_would_not_run() {
        // A syntactically perfect entry, written to the right file, that a
        // project-wide rule still keeps from running. Saying only "added" —
        // then having it never appear — is the exact silence this whole
        // change removes, so `add` re-reads through the real loader and says
        // so.
        let fixture = Fixture::new("addcapped");
        let mut existing = serde_json::Map::new();
        for index in 0..crate::mcp_config::MAX_MCP_SERVERS {
            existing.insert(
                format!("filler{index}"),
                serde_json::json!({"command": "true"}),
            );
        }
        fixture.settings(
            ".rapidlm/settings.json",
            &serde_json::json!({"mcpServers": existing}).to_string(),
        );

        let outcome = fixture.run(&["add", "onemore", "--command", "true"]);
        assert_eq!(outcome.exit, 0, "the write itself succeeded: {}", outcome.text);
        assert!(outcome.text.starts_with("added onemore"), "{}", outcome.text);
        assert!(
            outcome.text.contains("warning: onemore will still not run"),
            "{}",
            outcome.text
        );
        assert!(
            outcome
                .text
                .contains(&format!("{}-server limit", crate::mcp_config::MAX_MCP_SERVERS)),
            "the warning must name the real reason: {}",
            outcome.text
        );
    }

    #[test]
    fn add_to_an_untrusted_project_says_the_server_will_not_register() {
        let fixture = Fixture::new("adduntrusted");
        let outcome = fixture.run(&["add", "srv", "--command", "true"]);
        assert_eq!(outcome.exit, 0, "{}", outcome.text);
        assert!(
            outcome.text.contains("rapid trust grant"),
            "an added server that cannot register must say so: {}",
            outcome.text
        );

        // Granted, the note is gone: nothing stands between the entry and a
        // real turn any more.
        fixture.set_trust(TrustStatus::Trusted);
        let outcome = fixture.run(&["add", "other", "--command", "true"]);
        assert!(
            !outcome.text.contains("rapid trust grant"),
            "{}",
            outcome.text
        );
    }

    #[test]
    fn remove_deletes_from_every_file_that_defines_it() {
        let fixture = Fixture::new("remove");
        fixture.settings(
            ".rapidlm/settings.json",
            r#"{"mcpServers": {"srv": {"command": "a"}, "keep": {"command": "b"}}}"#,
        );
        fixture.settings(
            ".claude/settings.json",
            r#"{"mcpServers": {"srv": {"command": "c"}}}"#,
        );
        let outcome = fixture.run(&["remove", "srv"]);
        assert_eq!(outcome.exit, 0, "{}", outcome.text);
        assert!(
            outcome.text.contains(".rapidlm/settings.json")
                && outcome.text.contains(".claude/settings.json"),
            "both files must be named: {}",
            outcome.text
        );
        assert!(!fixture.read(".rapidlm/settings.json").contains("\"srv\""));
        assert!(fixture.read(".rapidlm/settings.json").contains("\"keep\""));
        assert!(!fixture.read(".claude/settings.json").contains("\"srv\""));
    }

    #[test]
    #[cfg(unix)]
    fn a_settings_file_carrying_a_secret_is_not_left_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        // `atomic_write` renames a fresh temp file into place, so the
        // target's mode is replaced, not preserved. For the one command
        // whose documented purpose includes persisting API tokens that
        // meant a new file defaulted to the umask and an existing 0600 file
        // was silently relaxed to 0644.
        let fixture = Fixture::new("mode");
        let outcome = fixture.run(&["add", "srv", "--command", "true", "--env", "API_KEY=tok"]);
        assert_eq!(outcome.exit, 0, "{}", outcome.text);
        let path = fixture.project.join(".rapidlm/settings.json");
        let mode = std::fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a settings file this command created is 0o{mode:o}");

        // An existing mode is carried across rather than replaced: this
        // command does not get to loosen (or tighten) a decision the user or
        // another tool already made.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640))
            .expect("set mode");
        let outcome = fixture.run(&["add", "other", "--command", "true"]);
        assert_eq!(outcome.exit, 0, "{}", outcome.text);
        let mode = std::fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode, 0o640, "an existing mode was not preserved (0o{mode:o})");
    }

    #[test]
    fn add_does_not_call_a_server_broken_because_another_file_shadows_the_name() {
        // The post-write check used to match rejections by name alone. A
        // name also defined in `.claude/settings.json` produces a `Shadowed`
        // rejection for *that* file even though the entry just written to
        // `.rapidlm/settings.json` is the one that wins — so `add` reported
        // a perfectly working server as broken, and the `else` chain
        // swallowed the untrusted note as well.
        let fixture = Fixture::new("addshadowed");
        fixture.settings(
            ".claude/settings.json",
            r#"{"mcpServers": {"srv": {"command": "from-claude"}}}"#,
        );
        let outcome = fixture.run(&["add", "srv", "--command", "from-rapidlm"]);
        assert_eq!(outcome.exit, 0, "{}", outcome.text);
        assert!(
            !outcome.text.contains("will still not run"),
            "the entry just written is the one that runs:\n{}",
            outcome.text
        );
        assert!(
            outcome.text.contains("rapid trust grant"),
            "the untrusted note must not be swallowed:\n{}",
            outcome.text
        );
        // And it really is the winner.
        let got = fixture.run(&["get", "srv"]);
        assert!(got.text.contains("command=from-rapidlm"), "{}", got.text);
    }

    #[test]
    fn remove_succeeds_even_when_an_unrelated_settings_file_cannot_be_parsed() {
        // A commented `.claude/settings.json` is legal for the tool that
        // owns it and usually has nothing to do with the server being
        // removed. Failing the command on it broke `rapid mcp remove x`
        // for any such project.
        let fixture = Fixture::new("removejsonc");
        fixture.settings(
            ".rapidlm/settings.json",
            r#"{"mcpServers": {"srv": {"command": "a"}}}"#,
        );
        fixture.settings(".claude/settings.json", "{ // a comment\n  \"other\": 1 }");
        let outcome = fixture.run(&["remove", "srv"]);
        assert_eq!(
            outcome.exit, 0,
            "the removal did what was asked:\n{}",
            outcome.text
        );
        assert!(outcome.text.contains("removed srv"), "{}", outcome.text);
        assert!(
            outcome.text.contains("warning:") && outcome.text.contains(".claude/settings.json"),
            "the file it could not read is still reported:\n{}",
            outcome.text
        );
        assert!(
            fixture.read(".claude/settings.json").contains("// a comment"),
            "the unparsable file must be left exactly as it was"
        );
    }

    #[test]
    fn remove_of_an_absent_server_fails_rather_than_reporting_success() {
        let fixture = Fixture::new("removemissing");
        fixture.settings(".rapidlm/settings.json", r#"{"mcpServers": {}}"#);
        let outcome = fixture.run(&["remove", "srv"]);
        assert_eq!(outcome.exit, 1);
        assert!(outcome.text.contains("no MCP server named"), "{}", outcome.text);
    }

    #[test]
    fn probe_refuses_an_untrusted_project_and_starts_nothing() {
        // Probing executes whatever `command` the project declares, so it is
        // gated on exactly the trust decision that gates registration.
        let fixture = Fixture::new("probeuntrusted");
        let marker = fixture.root.join("probe-ran");
        fixture.settings(
            ".rapidlm/settings.json",
            &format!(
                r#"{{"mcpServers": {{"srv": {{"command": "/usr/bin/touch", "args": ["{}"]}}}}}}"#,
                marker.display()
            ),
        );
        let outcome = fixture.run(&["probe"]);
        assert!(
            !marker.exists(),
            "an untrusted project's configured command must not be executed"
        );
        assert_eq!(outcome.exit, 1);
        assert!(outcome.text.contains("not trusted"), "{}", outcome.text);
    }

    #[test]
    fn probe_fails_closed_when_the_trust_catalog_cannot_be_read() {
        let fixture = Fixture::new("probecorrupt");
        fixture.settings(
            ".rapidlm/settings.json",
            r#"{"mcpServers": {"srv": {"command": "true"}}}"#,
        );
        std::fs::write(fixture.home.join(TRUST_CATALOG_NAME), "not a trust catalog")
            .expect("corrupt catalog");
        let outcome = fixture.run(&["probe"]);
        assert_eq!(
            outcome.exit, 1,
            "an unreadable catalog is a refusal, never an implied untrusted-but-fine: {}",
            outcome.text
        );
        // Specifically the *unreadable* wording, not the ordinary untrusted
        // refusal: both exit 1 and both print `error:`, so asserting only
        // that would still pass if an unreadable catalog were silently
        // downgraded to "untrusted".
        assert!(
            outcome.text.contains("trust=unreadable")
                && outcome.text.contains("trust state could not be read"),
            "{}",
            outcome.text
        );
        assert!(
            !outcome.text.contains("not trusted to do"),
            "an unreadable catalog must not be reported as a plain untrusted project: {}",
            outcome.text
        );
    }

    #[test]
    fn probe_on_a_trusted_project_with_nothing_configured_succeeds_quietly() {
        let fixture = Fixture::new("probeempty");
        fixture.set_trust(TrustStatus::Trusted);
        let outcome = fixture.run(&["probe"]);
        assert_eq!(outcome.exit, 0, "{}", outcome.text);
        assert!(outcome.text.contains("no MCP server is configured"), "{}", outcome.text);
    }

    #[test]
    fn probe_of_an_unknown_name_fails_without_starting_anything() {
        let fixture = Fixture::new("probeunknown");
        fixture.set_trust(TrustStatus::Trusted);
        let outcome = fixture.run(&["probe", "nope"]);
        assert_eq!(outcome.exit, 1);
        assert!(outcome.text.contains("no MCP server named"), "{}", outcome.text);
    }

    #[test]
    fn a_server_that_cannot_start_is_reported_as_a_failure() {
        let fixture = Fixture::new("probedead");
        fixture.set_trust(TrustStatus::Trusted);
        fixture.settings(
            ".rapidlm/settings.json",
            r#"{"mcpServers": {"dead": {"command": "/nonexistent/mcp-server-binary"}}}"#,
        );
        let outcome = fixture.run(&["probe"]);
        assert_eq!(outcome.exit, 1, "{}", outcome.text);
        assert!(outcome.text.contains("failed=dead"), "{}", outcome.text);
        assert!(outcome.text.contains("failed to start"), "{}", outcome.text);
    }

    #[test]
    fn every_command_reports_the_project_and_its_trust_state() {
        let fixture = Fixture::new("header");
        fixture.set_trust(TrustStatus::Trusted);
        for args in [vec!["list"], vec!["get", "x"], vec!["probe"]] {
            let outcome = fixture.run(&args);
            assert!(
                outcome.text.contains("trust=trusted"),
                "{args:?} omitted the trust state:\n{}",
                outcome.text
            );
        }
    }

    #[test]
    fn add_refuses_argv_and_env_the_loader_would_reject() {
        // Same rule as the name check: a write must never produce an entry
        // `rapid mcp list` reports as broken one line later.
        let fixture = Fixture::new("addbounds");
        let mut args: Vec<String> =
            vec!["add".into(), "srv".into(), "--command".into(), "true".into()];
        for index in 0..=crate::mcp_config::MAX_ARGS {
            args.push("--arg".into());
            args.push(index.to_string());
        }
        let err = run(&args, &fixture.env()).expect_err("over the arg limit");
        assert!(err.0.contains("over the"), "{}", err.0);
        assert!(
            !fixture.project.join(".rapidlm/settings.json").exists(),
            "a rejected add must not create a settings file"
        );

        let mut args: Vec<String> =
            vec!["add".into(), "srv".into(), "--command".into(), "true".into()];
        for index in 0..=crate::mcp_config::MAX_ENV_VARS {
            args.push("--env".into());
            args.push(format!("K{index}=v"));
        }
        let err = run(&args, &fixture.env()).expect_err("over the env limit");
        assert!(err.0.contains("env vars"), "{}", err.0);
    }

    #[test]
    fn malformed_invocations_are_usage_errors_not_silent_successes() {
        let fixture = Fixture::new("usage");
        for args in [
            vec!["bogus"],
            vec!["list", "extra"],
            vec!["get"],
            vec!["get", "a", "b"],
            vec!["remove"],
            vec!["add"],
            vec!["add", "srv"],
            vec!["add", "srv", "--command"],
            vec!["add", "srv", "--command", "a", "--command", "b"],
            vec!["add", "srv", "--command", "a", "--env", "novalue"],
            vec!["add", "srv", "--command", "a", "--env", "9BAD=v"],
            vec!["add", "srv", "--command", "a", "--nope"],
            vec!["probe", "a", "b"],
        ] {
            let _ = fixture.usage(&args);
        }
    }

    #[test]
    fn help_is_available_and_exits_zero() {
        let fixture = Fixture::new("help");
        let outcome = fixture.run(&["--help"]);
        assert_eq!(outcome.exit, 0);
        assert_eq!(outcome.text, MCP_USAGE);
        // Whether every advertised command actually dispatches is the job of
        // `every_subcommand_the_top_level_help_advertises_is_accepted` in
        // `tests/mcp_cli.rs`; asserting `MCP_USAGE.contains("list")` here
        // would only restate the constant.
    }
}
