//! `rapid agents list|validate|scaffold`: the agent definitions the loader
//! finds, offline and read-only (`scaffold` only prints a starter).
//!
//! `list` reads through [`crate::agent_types`], the loader a spawn uses, so it
//! can never show a type a spawn would not offer (or hide one it would).
//! Nothing here runs an agent, records an event, spawns a process or opens the
//! network. The project's own directory is read only when the project is
//! trusted, as a spawn reads it; naming a directory with `--dir` is the one
//! way to read another, and then it is read because it was named.

use std::path::{Path, PathBuf};

use agent_runtime::agent_defs::{
    AGENT_DEFS_SCHEMA, AgentDefId, AgentDefinition, DefFormat, DefInventory, PROJECT_DEFS_DIR,
    USER_DEFS_DIR, load_directory,
};
use kernel::{CancellationToken, ProjectIdentity, ProjectTrustStore, TrustStatus};

use crate::headless::jsonl::JsonlExitCode;
use crate::interactive::{TRUST_CATALOG_NAME, resolve_project_root, user_home_path_from};

pub const AGENTS_USAGE: &str = "\
usage: rapid agents list [--dir <path>]
       rapid agents validate [--dir <path>]
       rapid agents scaffold <id> [--format md|toml]

list      the agent types a spawn may name: the built-ins, the project's
          definitions, the user's (<home>/agents) and the directories
          `[agents] extra_dirs` names in the user config; each with its file
          format, base role, tool classes and source, then the files that were
          refused, each with the reason
validate  every definition in the project's directory (or --dir), the reason
          for each refused file, and an exit status of 2 if there is one
scaffold  a starter definition for <id>, in the table (toml, the default) or
          Markdown form

A definition is <id>.toml, or <id>.md: front matter between `---` lines, then
the agent's instructions as the body. The project's directory,
<project>/.rapidlm/agents, is read only when the project is trusted
(`rapid trust grant`); --dir names a directory to read whatever its project's
trust. Other Markdown files in a definitions directory (a README) are reported
as refused: keep notes elsewhere.
";

/// Process inputs, injectable so the commands are testable without touching
/// the test process's working directory or environment.
pub struct AgentsEnv {
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
}

impl AgentsEnv {
    pub fn from_process() -> Self {
        Self {
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            env: std::env::vars().collect(),
        }
    }
}

/// What a command produced: text for stdout and the exit code.
#[derive(Debug, Eq, PartialEq)]
pub struct AgentsOutcome {
    pub text: String,
    pub exit: i32,
}

/// A malformed invocation; the caller prints [`AGENTS_USAGE`] and exits 2.
#[derive(Debug, Eq, PartialEq)]
pub struct AgentsUsageError(pub String);

/// Where the commands look.
#[derive(Debug, Default)]
pub(crate) struct Scope {
    /// The project's definitions directory, when it is to be read.
    project: Option<PathBuf>,
    /// Why it is not, when it is not.
    project_note: Option<String>,
    /// The directory came from `--dir`.
    named: bool,
    user: Option<PathBuf>,
    extras: Vec<PathBuf>,
    problems: Vec<String>,
    /// Where the tool names are read from, for the patterns that name none.
    names_root: PathBuf,
}

/// The scope of `env`, with `--dir` standing for the project's directory.
pub(crate) fn scope_of(env: &AgentsEnv, dir: Option<PathBuf>) -> Scope {
    let home = user_home_path_from(&env.env);
    let extras = crate::agent_types::extra_dirs(&env.env);
    let mut scope = Scope {
        user: home.map(|home| home.join(USER_DEFS_DIR)),
        extras: extras.dirs,
        problems: extras.problems,
        names_root: env.cwd.clone(),
        ..Scope::default()
    };
    if let Some(dir) = dir {
        scope.project = Some(dir);
        scope.named = true;
        return scope;
    }
    let cancel = CancellationToken::new();
    let Some(root) = resolve_project_root(&env.cwd, &cancel)
        .ok()
        .map(|found| found.root)
    else {
        scope.project_note = Some("not inside a project".to_owned());
        return scope;
    };
    let trusted = match (
        user_home_path_from(&env.env),
        ProjectIdentity::new(&root, None).ok(),
    ) {
        (Some(home), Some(identity)) => ProjectTrustStore::open(home.join(TRUST_CATALOG_NAME))
            .get(&identity, &cancel)
            .is_ok_and(|status| status == TrustStatus::Trusted),
        _ => false,
    };
    let dir = root.join(PROJECT_DEFS_DIR);
    if !trusted {
        scope.project_note = Some(format!(
            "not loaded: untrusted project ({}); run `rapid trust grant` in it if you trust \
its definitions",
            plain(&dir.display().to_string())
        ));
    } else if !agent_runtime::agent_defs::resolves_inside(&root, &dir) {
        scope.project_note = Some(format!(
            "not loaded: {} resolves outside the project (a link on the way)",
            plain(&dir.display().to_string())
        ));
    } else {
        scope.project = Some(dir);
    }
    scope
}

pub fn run(args: &[String], env: &AgentsEnv) -> Result<AgentsOutcome, AgentsUsageError> {
    let mut dir: Option<PathBuf> = None;
    let mut format: Option<String> = None;
    let mut rest: Vec<&str> = Vec::new();
    let mut at = 0;
    while at < args.len() {
        match args[at].as_str() {
            "--help" | "-h" => {
                return Ok(AgentsOutcome {
                    text: AGENTS_USAGE.to_owned(),
                    exit: 0,
                });
            }
            "--dir" => {
                at += 1;
                match args.get(at) {
                    Some(path) => dir = Some(PathBuf::from(path)),
                    None => {
                        return Err(AgentsUsageError(
                            "rapid agents: --dir needs a path".to_owned(),
                        ));
                    }
                }
            }
            "--format" => {
                at += 1;
                match args.get(at) {
                    Some(value) => format = Some(value.clone()),
                    None => {
                        return Err(AgentsUsageError(
                            "rapid agents: --format needs md or toml".to_owned(),
                        ));
                    }
                }
            }
            other => rest.push(other),
        }
        at += 1;
    }
    match rest.as_slice() {
        ["list"] if format.is_none() => Ok(list(&scope_of(env, dir))),
        ["validate"] if format.is_none() => Ok(validate(&scope_of(env, dir))),
        ["scaffold", id] if dir.is_none() => scaffold(id, format.as_deref()),
        [] => Err(AgentsUsageError(
            "rapid agents: a verb is required".to_owned(),
        )),
        [verb, ..] => Err(AgentsUsageError(format!(
            "rapid agents: unknown usage `{}`",
            plain(verb)
        ))),
    }
}

/// `; closest: a, b` for a pattern that matches no tool: the known tool names
/// nearest to what the pattern spells with its wildcards taken out, or nothing.
fn closest(pattern: &str, known: &[String]) -> String {
    let spelled: Vec<char> = pattern
        .chars()
        .filter(|c| !matches!(c, '*' | '?'))
        .collect();
    if spelled.is_empty() {
        return String::new();
    }
    let distance = |name: &str| -> usize {
        let name: Vec<char> = name.chars().collect();
        let mut row: Vec<usize> = (0..=name.len()).collect();
        for (i, a) in spelled.iter().enumerate() {
            let mut previous = row[0];
            row[0] = i + 1;
            for (j, b) in name.iter().enumerate() {
                let next = row[j + 1];
                row[j + 1] = (previous + usize::from(a != b))
                    .min(row[j] + 1)
                    .min(row[j + 1] + 1);
                previous = next;
            }
        }
        row[name.len()]
    };
    let limit = (spelled.len() / 2).max(3);
    let mut near: Vec<(usize, &String)> = known
        .iter()
        .map(|name| (distance(name), name))
        .filter(|(d, _)| *d <= limit)
        .collect();
    near.sort();
    near.truncate(3);
    if near.is_empty() {
        String::new()
    } else {
        format!(
            "; closest: {}",
            near.iter()
                .map(|(_, name)| name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// A value as one line of output: a control character, and anything that
/// would split the line or reorder it on a screen, shown as `?`.
pub(crate) fn plain(text: &str) -> String {
    text.chars()
        .map(|ch| {
            if ch.is_control() || crate::mcp_deferred::changes_the_line(ch) {
                '?'
            } else {
                ch
            }
        })
        .collect()
}

/// The most of a refusal's reason one line of output carries: what a file said
/// can run to the size of the file.
const MAX_REASON_CHARS: usize = 400;

/// [`plain`] and no longer than `max` characters.
pub(crate) fn bounded(text: &str, max: usize) -> String {
    let text = plain(text);
    if text.chars().count() <= max {
        return text;
    }
    let head: String = text.chars().take(max).collect();
    format!("{head}…")
}

/// `text` cut to a line's worth, on one line.
fn brief(text: &str) -> String {
    bounded(text, 80)
}

fn list(scope: &Scope) -> AgentsOutcome {
    let inventory = crate::agent_types::inventory_with(
        scope.project.as_deref(),
        scope.user.as_deref(),
        &scope.extras,
    );
    let mut text = format!(
        "schema={} dir={} user_dir={} extra_dirs={} loaded={} rejected={}\n",
        AGENT_DEFS_SCHEMA,
        shown_dir(scope.project.as_deref()),
        shown_dir(scope.user.as_deref()),
        scope.extras.len(),
        inventory.loaded.len(),
        inventory.rejected.len()
    );
    if let Some(note) = &scope.project_note {
        text.push_str(&format!("project: {note}\n"));
    }
    for problem in &scope.problems {
        text.push_str(&format!("extra_dirs: {}\n", plain(problem)));
    }
    for def in &inventory.loaded {
        text.push_str(&definition_line(def));
        text.push('\n');
    }
    for rejected in &inventory.rejected {
        text.push_str(&format!(
            "rejected path={} reason={}\n",
            plain(&rejected.path.display().to_string()),
            bounded(&rejected.reason, MAX_REASON_CHARS)
        ));
    }
    AgentsOutcome { text, exit: 0 }
}

fn shown_dir(dir: Option<&Path>) -> String {
    dir.map_or_else(|| "-".to_owned(), |dir| plain(&dir.display().to_string()))
}

/// One loaded definition as a line.
fn definition_line(def: &AgentDefinition) -> String {
    let source = match def.source.path() {
        None => "builtin".to_owned(),
        Some(path) => format!(
            "{}:{}",
            def.source.kind(),
            plain(&path.display().to_string())
        ),
    };
    let format = def.format().map_or("builtin", DefFormat::as_str);
    let classes: Vec<&str> = def
        .tool_surface
        .keys()
        .map(agent_runtime::role_profile::RoleToolClass::as_str)
        .collect();
    let mut line = format!(
        "id={} format={format} role={} source={source} tools={}",
        def.id,
        def.role.as_str(),
        if classes.is_empty() {
            "-".to_owned()
        } else {
            classes.join(",")
        }
    );
    // What narrows it further, when anything does: the patterns it keeps and
    // takes away, how many rules it adds, how many steps it may take.
    if let Some(allow) = &def.allow_tools {
        line.push_str(&format!(
            " allow_tools={}",
            if allow.is_empty() {
                "-".to_owned()
            } else {
                allow.join(",")
            }
        ));
    }
    if !def.deny_tools.is_empty() {
        line.push_str(&format!(" deny_tools={}", def.deny_tools.join(",")));
    }
    if !def.permission.is_empty() {
        line.push_str(&format!(" rules={}", def.permission.len()));
    }
    if let Some(steps) = def.max_steps {
        line.push_str(&format!(" max_steps={steps}"));
    }
    // Where it may be used, when that is not the default, and who a main agent
    // of it may delegate to.
    if def.mode != agent_runtime::agent_defs::AgentMode::Subagent {
        line.push_str(&format!(" mode={}", def.mode.as_str()));
    }
    if !def.spawn.is_empty() {
        line.push_str(&format!(" spawn={}", def.spawn.join(",")));
    }
    if let Some(model) = &def.model {
        line.push_str(&format!(" model={}", plain(model)));
    }
    if let Some(effort) = &def.reasoning_effort {
        line.push_str(&format!(" reasoning_effort={}", plain(effort)));
    }
    if !def.inputs.is_empty() {
        line.push_str(&format!(" inputs={}", def.inputs.join(",")));
    }
    if !def.outputs.is_empty() {
        line.push_str(&format!(" outputs={}", def.outputs.join(",")));
    }
    line.push_str(&format!(" description={}", brief(&def.description)));
    line
}

fn validate(scope: &Scope) -> AgentsOutcome {
    let Some(dir) = &scope.project else {
        let note = scope
            .project_note
            .clone()
            .unwrap_or_else(|| "no definitions directory".to_owned());
        return AgentsOutcome {
            text: format!(
                "project: {note}\nnothing was validated; name a directory with --dir to read \
one\n"
            ),
            exit: JsonlExitCode::Policy.as_i32(),
        };
    };
    // A directory that was named and is not one has nothing to validate, which
    // is not the same as nothing being wrong.
    if scope.named && !dir.is_dir() && std::fs::symlink_metadata(dir).is_err() {
        return AgentsOutcome {
            text: format!(
                "rejected path={} reason=no such directory\n",
                plain(&dir.display().to_string())
            ),
            exit: JsonlExitCode::Usage.as_i32(),
        };
    }
    let registry = crate::agent_types::implementation_registry();
    let inventory = match load_directory(dir, &registry) {
        Ok(inventory) => inventory,
        // A directory that fails as a whole (a repeated id, a built-in's id, too
        // many files) is one refusal, as the spawn path counts it.
        Err(err) => DefInventory {
            loaded: Vec::new(),
            rejected: vec![agent_runtime::agent_defs::RejectedDef {
                path: dir.clone(),
                reason: err.to_string(),
            }],
        },
    };
    let mut text = String::new();
    let known = crate::exec_tools::builtin_tool_names(&scope.names_root);
    // The agent ids a `spawn` list can name: the built-ins, this directory's and
    // the user's.
    let agents: Vec<String> = crate::agent_types::inventory_with(
        scope.project.as_deref(),
        scope.user.as_deref(),
        &scope.extras,
    )
    .loaded
    .iter()
    .chain(&inventory.loaded)
    .map(|def| def.id.as_str().to_owned())
    .collect();
    for def in &inventory.loaded {
        if let Some(path) = def.source.path() {
            text.push_str(&format!(
                "ok path={} id={} format={}\n",
                plain(&path.display().to_string()),
                def.id,
                def.format().map_or("toml", DefFormat::as_str)
            ));
            // An id in `spawn` that no agent has is a typo or an agent that is
            // not there: said, not refused.
            for id in &def.spawn {
                if !agents.contains(id) {
                    text.push_str(&format!(
                        "warning path={} spawn names no agent `{}`\n",
                        plain(&path.display().to_string()),
                        plain(id)
                    ));
                }
            }
            // A pattern that names no tool is a typo or a tool that is not
            // connected: said, not refused (an MCP tool may not be there yet).
            for (key, patterns) in [
                ("allow_tools", def.allow_tools.as_deref().unwrap_or(&[])),
                ("deny_tools", def.deny_tools.as_slice()),
            ] {
                for pattern in patterns {
                    if let Some(known) = &known
                        && !pattern.starts_with("mcp__")
                        && !known
                            .iter()
                            .any(|name| crate::permissions::glob_match(pattern, name))
                    {
                        text.push_str(&format!(
                            "warning path={} {key} pattern `{}` matches no tool{}\n",
                            plain(&path.display().to_string()),
                            plain(pattern),
                            closest(pattern, known)
                        ));
                    }
                }
            }
        }
    }
    for rejected in &inventory.rejected {
        text.push_str(&format!(
            "rejected path={} reason={}\n",
            plain(&rejected.path.display().to_string()),
            bounded(&rejected.reason, MAX_REASON_CHARS)
        ));
    }
    text.push_str(&format!(
        "summary ok={} refused={}\n",
        inventory.loaded.len(),
        inventory.rejected.len()
    ));
    let exit = if inventory.rejected.is_empty() {
        0
    } else {
        JsonlExitCode::Usage.as_i32()
    };
    AgentsOutcome { text, exit }
}

fn scaffold(id: &str, format: Option<&str>) -> Result<AgentsOutcome, AgentsUsageError> {
    let format = match format {
        None => DefFormat::Toml,
        Some(raw) => DefFormat::parse(raw).ok_or_else(|| {
            AgentsUsageError(format!(
                "rapid agents: --format is md or toml, not `{}`",
                plain(raw)
            ))
        })?,
    };
    let id =
        AgentDefId::parse(id).map_err(|err| AgentsUsageError(format!("rapid agents: {err}")))?;
    Ok(AgentsOutcome {
        text: AgentDefinition::scaffold_as(&id, format),
        exit: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A project, a RapidLM home and an environment pointing at them.
    struct Fixture {
        root: PathBuf,
        project: PathBuf,
        home: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let seq = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let root = std::env::temp_dir().join(format!("rapidlm-agents-admin-{tag}-{seq}"));
            let project = root.join("project");
            let home = root.join("home");
            std::fs::create_dir_all(project.join(".rapidlm/agents")).expect("project");
            std::fs::create_dir_all(&home).expect("home");
            let root = protocol::host_path::canonicalize(&root).expect("canonical");
            Self {
                project: root.join("project"),
                home: root.join("home"),
                root,
            }
        }

        fn env(&self) -> AgentsEnv {
            AgentsEnv {
                cwd: self.project.clone(),
                env: vec![
                    ("RAPIDLM_HOME".to_owned(), self.home.display().to_string()),
                    ("HOME".to_owned(), self.root.display().to_string()),
                ],
            }
        }

        fn defs(&self) -> PathBuf {
            self.project.join(".rapidlm/agents")
        }

        fn trust(&self) {
            let identity = ProjectIdentity::new(&self.project, None).expect("identity");
            ProjectTrustStore::open(self.home.join(TRUST_CATALOG_NAME))
                .set(&identity, TrustStatus::Trusted, &CancellationToken::new())
                .expect("grant trust");
        }

        fn write(&self, dir: &Path, file: &str, text: &str) {
            std::fs::create_dir_all(dir).expect("dir");
            std::fs::write(dir.join(file), text).expect("write");
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    const MD: &str = "---\ndescription: \"Reads\\nthings\"\nbase_role: reviewer\n\
                      tool_classes: read git\nmodel: review\n---\nBe careful.\n";
    const TOML: &str = "schema = \"rapidlm.agent_defs.v1\"\n[agent]\nid = \"tabled\"\n\
                        description = \"from a table\"\nbase_role = \"explorer\"\n\
                        tools = [\"read\"]\n";

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    fn line_of<'a>(text: &'a str, id: &str) -> &'a str {
        text.lines()
            .find(|line| line.starts_with(&format!("id={id} ")))
            .unwrap_or_else(|| panic!("no {id} in:\n{text}"))
    }

    #[test]
    fn a_trusted_projects_two_syntaxes_are_listed_with_their_format_and_source() {
        let fx = Fixture::new("list");
        fx.write(&fx.defs(), "reviewer.md", MD);
        fx.write(&fx.defs(), "tabled.toml", TOML);
        fx.write(&fx.defs(), "README.md", "# notes, not an agent\n");
        fx.trust();
        let out = run(&args(&["list"]), &fx.env()).expect("list");
        assert_eq!(out.exit, 0);
        let reviewer = line_of(&out.text, "reviewer");
        assert!(
            reviewer.starts_with("id=reviewer format=md role=reviewer source=project:")
                && reviewer.contains(" tools=read,git model=review ")
                && reviewer.ends_with("description=Reads?things"),
            "{reviewer}"
        );
        assert!(
            line_of(&out.text, "tabled").starts_with("id=tabled format=toml role=explorer "),
            "{}",
            out.text
        );
        assert!(
            line_of(&out.text, "explore").starts_with("id=explore format=builtin "),
            "{}",
            out.text
        );
        let refused: Vec<&str> = out
            .text
            .lines()
            .filter(|line| line.starts_with("rejected "))
            .collect();
        assert_eq!(refused.len(), 1, "{}", out.text);
        assert!(refused[0].contains("README.md"), "{}", refused[0]);
        assert!(
            out.text
                .lines()
                .next()
                .is_some_and(|header| header.contains("loaded=6 rejected=1")),
            "{}",
            out.text
        );
    }

    #[test]
    fn an_untrusted_projects_directory_is_not_read_and_the_command_says_why() {
        let fx = Fixture::new("untrusted");
        fx.write(&fx.defs(), "reviewer.md", MD);
        // A file that would be refused, and so show, if the directory were read.
        fx.write(&fx.defs(), "bad.md", "---\nstealth: true\n---\n");
        let out = run(&args(&["list"]), &fx.env()).expect("list");
        assert_eq!(out.exit, 0);
        assert!(!out.text.contains("id=reviewer"), "{}", out.text);
        assert!(
            out.text.contains("project: not loaded: untrusted project")
                && out.text.contains("rapid trust grant"),
            "{}",
            out.text
        );
        // Nothing but the built-ins, and no refusal of the unread directory.
        assert!(
            out.text
                .lines()
                .next()
                .is_some_and(|header| header.contains("loaded=4 rejected=0")),
            "{}",
            out.text
        );
        let checked = run(&args(&["validate"]), &fx.env()).expect("validate");
        assert_eq!(checked.exit, JsonlExitCode::Policy.as_i32());
        assert!(
            checked.text.contains("untrusted project")
                && checked.text.contains("nothing was validated"),
            "{}",
            checked.text
        );
        assert!(!checked.text.contains("ok path"), "{}", checked.text);
        // Trusting it changes both.
        fx.trust();
        let out = run(&args(&["list"]), &fx.env()).expect("list");
        assert!(out.text.contains("id=reviewer "), "{}", out.text);
        assert!(out.text.contains("rejected path=") && out.text.contains("bad.md"));
        let checked = run(&args(&["validate"]), &fx.env()).expect("validate");
        assert_eq!(
            checked.exit,
            JsonlExitCode::Usage.as_i32(),
            "{}",
            checked.text
        );
        assert!(checked.text.contains("ok path="), "{}", checked.text);
        assert!(
            checked.text.ends_with("summary ok=1 refused=1\n"),
            "{}",
            checked.text
        );
    }

    #[test]
    fn a_directory_named_with_dir_is_read_whatever_its_projects_trust() {
        let fx = Fixture::new("named");
        fx.write(&fx.defs(), "reviewer.md", MD);
        fx.write(&fx.defs(), "bad.md", "---\nstealth: true\n---\n");
        let dir = fx.defs().display().to_string();
        let checked = run(&args(&["validate", "--dir", &dir]), &fx.env()).expect("validate");
        assert_eq!(
            checked.exit,
            JsonlExitCode::Usage.as_i32(),
            "{}",
            checked.text
        );
        assert!(
            checked.text.contains("ok path=") && checked.text.contains("id=reviewer format=md")
        );
        assert!(
            checked.text.contains("rejected path=")
                && checked.text.contains("bad.md")
                && checked.text.contains("unknown front matter key 'stealth'"),
            "{}",
            checked.text
        );
        let out = run(&args(&["list", "--dir", &dir]), &fx.env()).expect("list");
        assert!(out.text.contains("id=reviewer "), "{}", out.text);
        assert!(!out.text.contains("untrusted"), "{}", out.text);
        // A directory that fails as a whole is one refusal.
        fx.write(&fx.defs(), "dupe.toml", &TOML.replace("tabled", "reviewer"));
        let checked = run(&args(&["validate", "--dir", &dir]), &fx.env()).expect("validate");
        assert_eq!(checked.exit, JsonlExitCode::Usage.as_i32());
        assert!(
            checked.text.contains("duplicate definition id 'reviewer'"),
            "{}",
            checked.text
        );
    }

    #[test]
    fn extra_directories_come_from_the_user_config_below_the_user_directory() {
        let fx = Fixture::new("extra");
        let shared = fx.root.join("shared-agents");
        fx.write(
            &shared,
            "shared.md",
            "---\ndescription: Shared\nbase_role: explorer\ntool_classes: read\n---\n",
        );
        fx.write(
            &fx.home.join("agents"),
            "mine.toml",
            &TOML.replace("tabled", "mine"),
        );
        fx.write(
            &fx.home,
            "config.toml",
            &format!(
                "[agents]\nextra_dirs = [{:?}, \"relative/dir\", \"~/nowhere\"]\n",
                shared.display().to_string()
            ),
        );
        let out = run(&args(&["list"]), &fx.env()).expect("list");
        let shared_line = line_of(&out.text, "shared");
        assert!(shared_line.contains("source=extra:"), "{shared_line}");
        assert!(
            line_of(&out.text, "mine").contains("source=user:"),
            "{}",
            out.text
        );
        let problems: Vec<&str> = out
            .text
            .lines()
            .filter(|line| line.starts_with("extra_dirs: "))
            .collect();
        assert_eq!(problems.len(), 2, "{}", out.text);
        assert!(problems[0].contains("not an absolute path"), "{problems:?}");
        assert!(problems[1].contains("no such directory"), "{problems:?}");
        assert!(out.text.contains("extra_dirs=1 "), "{}", out.text);
    }

    #[test]
    fn only_an_absolute_user_config_names_extra_directories() {
        let fx = Fixture::new("extra-relative");
        let shared = fx.root.join("shared-agents");
        std::fs::create_dir_all(&shared).expect("shared");
        let config = fx.root.join("named.toml");
        std::fs::write(
            &config,
            format!(
                "[agents]\nextra_dirs = [{:?}]\n",
                shared.display().to_string()
            ),
        )
        .expect("config");
        let mut env = fx.env();
        env.env.push((
            crate::user_config::CONFIG_PATH_ENV.to_owned(),
            config.display().to_string(),
        ));
        let named = crate::agent_types::extra_dirs(&env.env);
        assert_eq!(named.dirs.len(), 1, "{:?}", named.problems);
        // The same name, relative: it would mean whatever the working directory
        // holds, a project's own file included, so it is not read at all.
        let mut relative = fx.env();
        relative.env.push((
            crate::user_config::CONFIG_PATH_ENV.to_owned(),
            "named.toml".to_owned(),
        ));
        let none = crate::agent_types::extra_dirs(&relative.env);
        assert!(none.dirs.is_empty() && none.problems.is_empty(), "{none:?}");
        // The guard itself: no path at all, where the file the name would mean
        // depends on where the process happens to be.
        assert_eq!(
            crate::skills_runtime::config_path(None, &env.env),
            Some(config.clone())
        );
        assert_eq!(
            crate::skills_runtime::config_path(None, &relative.env),
            None
        );
    }

    #[test]
    fn a_bad_extra_dirs_entry_is_named_with_the_table_it_is_in() {
        let fx = Fixture::new("extra-problems");
        fx.write(
            &fx.home,
            "config.toml",
            "[agents]\nextra_dirs = \"not a list\"\n",
        );
        let out = run(&args(&["list"]), &fx.env()).expect("list");
        assert!(
            out.text
                .contains("extra_dirs: [agents] extra_dirs must be a list of directory paths"),
            "{}",
            out.text
        );
        // The user's own directory is read already.
        std::fs::create_dir_all(fx.home.join("agents")).expect("user dir");
        fx.write(
            &fx.home,
            "config.toml",
            &format!(
                "[agents]\nextra_dirs = [{:?}]\n",
                fx.home.join("agents").display().to_string()
            ),
        );
        let out = run(&args(&["list"]), &fx.env()).expect("list");
        assert!(
            out.text
                .contains("the user agents directory, which is read already"),
            "{}",
            out.text
        );
        assert!(out.text.contains("extra_dirs=0 "), "{}", out.text);
    }

    #[cfg(unix)]
    #[test]
    fn a_trusted_projects_link_out_of_the_project_is_not_followed() {
        let fx = Fixture::new("outside");
        let elsewhere = fx.root.join("elsewhere");
        fx.write(&elsewhere.join("agents"), "intruder.md", MD);
        std::fs::remove_dir_all(fx.project.join(".rapidlm")).expect("remove");
        // `.rapidlm` is the link: `.rapidlm/agents` is a real directory.
        std::os::unix::fs::symlink(&elsewhere, fx.project.join(".rapidlm")).expect("link");
        fx.trust();
        let out = run(&args(&["list"]), &fx.env()).expect("list");
        assert!(
            !out.text.contains("id=intruder") && !out.text.contains("id=reviewer"),
            "{}",
            out.text
        );
        assert!(
            out.text.contains("resolves outside the project"),
            "{}",
            out.text
        );
        let checked = run(&args(&["validate"]), &fx.env()).expect("validate");
        assert_eq!(
            checked.exit,
            JsonlExitCode::Policy.as_i32(),
            "{}",
            checked.text
        );
        assert!(checked.text.contains("resolves outside the project"));
        // What a spawn resolves against says the same, once.
        let inventory = crate::agent_types::spawn_inventory(&fx.project, true);
        assert!(
            inventory.loaded.iter().all(|d| d.source.path().is_none()),
            "builtins only"
        );
        assert_eq!(inventory.rejected.len(), 1, "{:?}", inventory.rejected);
        assert!(
            inventory.rejected[0]
                .reason
                .contains("resolves outside the project")
        );
    }

    #[test]
    fn a_named_directory_that_is_not_there_is_refused_and_an_empty_one_says_so() {
        let fx = Fixture::new("named-missing");
        let missing = fx.root.join("typo").display().to_string();
        let checked = run(&args(&["validate", "--dir", &missing]), &fx.env()).expect("validate");
        assert_eq!(
            checked.exit,
            JsonlExitCode::Usage.as_i32(),
            "{}",
            checked.text
        );
        assert!(
            checked.text.contains("no such directory"),
            "{}",
            checked.text
        );
        let empty = fx.root.join("empty");
        std::fs::create_dir_all(&empty).expect("empty");
        let checked = run(
            &args(&["validate", "--dir", &empty.display().to_string()]),
            &fx.env(),
        )
        .expect("validate");
        assert_eq!(checked.exit, 0);
        assert_eq!(checked.text, "summary ok=0 refused=0\n");
    }

    #[test]
    fn what_a_file_says_is_one_clean_bounded_line() {
        let fx = Fixture::new("clean");
        fx.write(
            &fx.defs(),
            "wide.toml",
            &TOML.replace("tabled", "wide").replace(
                "from a table",
                "bidi \u{202e}override and line\u{2028}break",
            ),
        );
        fx.write(
            &fx.defs(),
            "huge.md",
            &format!(
                "---\ndescription: d\nbase_role: {}\n---\n",
                "r".repeat(30_000)
            ),
        );
        // A table that does not parse: the parser's message repeats the line.
        fx.write(
            &fx.defs(),
            "broken.toml",
            &format!("schema = \"{}\n", "x".repeat(30_000)),
        );
        fx.trust();
        let out = run(&args(&["list"]), &fx.env()).expect("list");
        let wide = line_of(&out.text, "wide");
        assert!(wide.contains("bidi ?override and line?break"), "{wide}");
        for file in ["huge.md", "broken.toml"] {
            let line = out
                .text
                .lines()
                .find(|line| line.contains(file))
                .unwrap_or_else(|| panic!("{file} is refused, in {} bytes", out.text.len()));
            assert!(
                line.chars().count() < 1_500,
                "{file}: {} chars",
                line.chars().count()
            );
        }
        let checked = run(&args(&["validate"]), &fx.env()).expect("validate");
        assert!(
            checked
                .text
                .lines()
                .all(|line| line.chars().count() < 1_500),
            "{}",
            checked.text.len()
        );
    }

    const NARROW_MD: &str = "---\ndescription: Narrow\nbase_role: coder\ntool_classes: read exec\n\
         allow_tools: repo_* shell_exec\ndeny_tools: repo_glob\nmax_steps: 12\n\
         permission:\n  \"shell_exec(rm *)\": deny\n  \"repo_read(secret*)\": ask\n---\n";

    #[test]
    fn the_listing_shows_what_a_definition_narrows_and_nothing_where_it_does_not() {
        let fx = Fixture::new("narrow-list");
        fx.write(&fx.defs(), "narrow.md", NARROW_MD);
        fx.write(&fx.defs(), "plain.md", MD);
        fx.write(
            &fx.defs(),
            "nothing.md",
            "---\ndescription: d\nbase_role: coder\nallow_tools: []\n---\n",
        );
        fx.trust();
        let out = run(&args(&["list"]), &fx.env()).expect("list");
        let narrow = line_of(&out.text, "narrow");
        assert!(
            narrow.contains(" tools=read,exec allow_tools=repo_*,shell_exec deny_tools=repo_glob rules=2 max_steps=12 "),
            "{narrow}"
        );
        let plain = line_of(&out.text, "plain");
        assert!(
            !plain.contains("allow_tools")
                && !plain.contains("deny_tools")
                && !plain.contains("rules=")
                && !plain.contains("max_steps"),
            "no new columns where nothing narrows: {plain}"
        );
        assert!(
            line_of(&out.text, "nothing").contains("allow_tools=-"),
            "an empty allow list is shown as no tool: {}",
            out.text
        );
    }

    #[test]
    fn the_listing_shows_the_mode_and_the_spawn_list_and_validate_warns_about_an_agent_that_is_not_there()
     {
        let fx = Fixture::new("steer-list");
        fx.write(
            &fx.defs(),
            "lead.md",
            "---\ndescription: Lead\nbase_role: coder\nmode: primary\nwhen_to_use: when asked to plan\n\
             spawn: explore ghost\n---\n",
        );
        fx.write(
            &fx.defs(),
            "either.md",
            "---\ndescription: E\nbase_role: coder\nmode: both\n---\n",
        );
        fx.write(&fx.defs(), "plain.md", MD);
        fx.trust();
        let out = run(&args(&["list"]), &fx.env()).expect("list");
        let lead = line_of(&out.text, "lead");
        assert!(
            lead.contains(" mode=primary spawn=explore,ghost "),
            "{lead}"
        );
        assert!(
            line_of(&out.text, "either").contains(" mode=both "),
            "{}",
            out.text
        );
        assert!(
            !line_of(&out.text, "plain").contains("mode="),
            "the default says nothing"
        );
        let checked = run(&args(&["validate"]), &fx.env()).expect("validate");
        assert_eq!(checked.exit, 0, "{}", checked.text);
        let warnings: Vec<&str> = checked
            .text
            .lines()
            .filter(|l| l.starts_with("warning "))
            .collect();
        assert_eq!(warnings.len(), 1, "{}", checked.text);
        assert!(
            warnings[0].contains("spawn names no agent `ghost`"),
            "{}",
            warnings[0]
        );
    }

    #[test]
    fn a_pattern_that_names_no_tool_is_a_warning_with_the_closest_names_and_not_a_refusal() {
        let fx = Fixture::new("narrow-warn");
        fx.write(
            &fx.defs(),
            "typo.md",
            "---\ndescription: d\nbase_role: coder\ntool_classes: read exec\n\
             allow_tools: repo_raed shell_exec mcp__docs__* repo_?ead zzzzzzzzzz\n\
             deny_tools: workspce_write\n---\n",
        );
        let dir = fx.defs().display().to_string();
        let out = run(&args(&["validate", "--dir", &dir]), &fx.env()).expect("validate");
        assert_eq!(out.exit, 0, "a warning is not a refusal: {}", out.text);
        let warnings: Vec<&str> = out
            .text
            .lines()
            .filter(|l| l.starts_with("warning "))
            .collect();
        assert_eq!(warnings.len(), 3, "{}", out.text);
        assert!(
            warnings[0]
                .contains("allow_tools pattern `repo_raed` matches no tool; closest: repo_read"),
            "{}",
            warnings[0]
        );
        assert!(
            warnings[1].contains("allow_tools pattern `zzzzzzzzzz` matches no tool")
                && !warnings[1].contains("closest"),
            "{}",
            warnings[1]
        );
        assert!(
            warnings[2].contains(
                "deny_tools pattern `workspce_write` matches no tool; closest: workspace_write"
            ),
            "{}",
            warnings[2]
        );
        // A real name, a wildcard that matches, and an MCP pattern (that tool may
        // not be connected yet) say nothing.
        assert!(!out.text.contains("`shell_exec`") && !out.text.contains("`repo_?ead`"));
        assert!(!out.text.contains("mcp__docs__"), "{}", out.text);
        assert!(
            out.text.ends_with("summary ok=1 refused=0\n"),
            "{}",
            out.text
        );
    }

    #[test]
    fn a_permission_key_that_is_no_rule_refuses_the_definition_when_it_loads() {
        let fx = Fixture::new("narrow-rule");
        fx.write(
            &fx.defs(),
            "badrule.md",
            "---\ndescription: d\nbase_role: coder\npermission:\n  \"Tool With Spaces\": deny\n---\n",
        );
        fx.write(
            &fx.defs(),
            "goodrule.md",
            "---\ndescription: d\nbase_role: coder\npermission:\n  \"shell_exec(rm *)\": deny\n---\n",
        );
        let dir = fx.defs().display().to_string();
        let out = run(&args(&["validate", "--dir", &dir]), &fx.env()).expect("validate");
        assert_eq!(out.exit, JsonlExitCode::Usage.as_i32(), "{}", out.text);
        assert!(
            out.text.contains("rejected path=")
                && out.text.contains("badrule.md")
                && out.text.contains("is not a permission rule"),
            "{}",
            out.text
        );
        assert!(out.text.contains("id=goodrule"), "{}", out.text);
        assert!(!out.text.contains("id=badrule"), "{}", out.text);
        // And a spawn does not resolve it.
        let inventory = crate::agent_types::inventory_of(Some(&fx.defs()), None);
        assert!(inventory.loaded.iter().all(|d| d.id.as_str() != "badrule"));
    }

    #[test]
    fn listing_makes_nothing_not_even_a_missing_home() {
        let fx = Fixture::new("readonly");
        let env = AgentsEnv {
            cwd: fx.project.clone(),
            env: vec![(
                "RAPIDLM_HOME".to_owned(),
                fx.root.join("not-yet").display().to_string(),
            )],
        };
        let out = run(&args(&["list"]), &env).expect("list");
        assert_eq!(out.exit, 0);
        assert!(!fx.root.join("not-yet").exists());
    }

    #[test]
    fn both_scaffolds_validate_as_written_to_a_directory() {
        let fx = Fixture::new("scaffold");
        let dir = fx.root.join("scaffolded");
        for (format, extension) in [(None, "toml"), (Some("toml"), "toml"), (Some("md"), "md")] {
            let mut words = vec!["scaffold", "starter"];
            if let Some(format) = format {
                words.extend(["--format", format]);
            }
            let out = run(&args(&words), &fx.env()).expect("scaffold");
            assert_eq!(out.exit, 0);
            let name = format!(
                "starter-{}-{extension}.{extension}",
                format.unwrap_or("default")
            );
            // The Markdown form's name is its file stem, so write it as that.
            let text = out.text.replace(
                "name: starter",
                &format!("name: {}", name.trim_end_matches(".md")),
            );
            let text = text.replace(
                "id = \"starter\"",
                &format!("id = \"{}\"", name.trim_end_matches(".toml")),
            );
            fx.write(&dir, &name, &text);
        }
        let checked = run(
            &args(&["validate", "--dir", &dir.display().to_string()]),
            &fx.env(),
        )
        .expect("validate");
        assert_eq!(checked.exit, 0, "{}", checked.text);
        assert_eq!(
            checked.text.matches("ok path=").count(),
            3,
            "{}",
            checked.text
        );
    }

    #[test]
    fn a_malformed_invocation_is_a_usage_error_naming_the_problem() {
        let fx = Fixture::new("usage");
        for (words, expected) in [
            (vec![], "a verb is required"),
            (vec!["frobnicate"], "unknown usage `frobnicate`"),
            (vec!["list", "--format", "md"], "unknown usage `list`"),
            (vec!["scaffold"], "unknown usage `scaffold`"),
            (
                vec!["scaffold", "x", "--dir", "d"],
                "unknown usage `scaffold`",
            ),
            (
                vec!["scaffold", "x", "--format", "yaml"],
                "--format is md or toml",
            ),
            (vec!["scaffold", "Bad Id"], "definition id must be"),
            (vec!["list", "--dir"], "--dir needs a path"),
            (vec!["scaffold", "x", "--format"], "--format needs"),
        ] {
            let err = run(&args(&words), &fx.env()).expect_err("usage error");
            assert!(err.0.contains(expected), "{words:?}: {}", err.0);
        }
        let help = run(&args(&["--help"]), &fx.env()).expect("help");
        assert_eq!(help.text, AGENTS_USAGE);
    }
}
