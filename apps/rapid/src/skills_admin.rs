//! `rapid skills list|show|discover`: what the skills loader finds, offline
//! and read-only.
//!
//! Both commands read through [`crate::skills_runtime`], the same discovery a
//! turn uses, so this can never report a skill the model would not be offered
//! (or hide one it would). Nothing here activates a skill, records an event,
//! spawns a process or opens the network.

use std::path::{Path, PathBuf};

use kernel::{CancellationToken, ProjectIdentity, ProjectTrustStore, TrustStatus};

use crate::interactive::{TRUST_CATALOG_NAME, resolve_project_root, user_home_from};
use crate::mcp_config::label;
use crate::skills_runtime::{Catalog, Entry, RootClass, Roots};
use plugin_host::skills::SkillError;

pub const SKILLS_USAGE: &str = "\
usage: rapid skills list
       rapid skills show <id>
       rapid skills discover [--roots <path>...]

list      every skill the loader finds, with the root it came from and who
          may invoke it; the skills a higher-ranked root hides; the skills it
          refused, each with the reason; and where the roots are
show      one skill: its description, root, flags and instructions
discover  directories under your home and configuration directories that
          look like a skills directory (every folder in it holds a valid
          SKILL.md), with the `extra_dirs` line to add one. Nothing is
          changed. RAPIDLM_CONFIG_ROOTS (a path list) or --roots replaces
          the directories scanned.

Skills are found, highest rank first, in the user's skills directory
(<home>/skills), the directories `[skills] extra_dirs` names in the user
config, and, for a trusted project, <project>/.rapidlm/skills then
<project>/.agents/skills. A skill of the same id in a lower root is hidden.
All three commands only read.
";

/// Process inputs, injectable so the commands are testable without touching
/// the test process's working directory or environment.
pub struct SkillsEnv {
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
}

impl SkillsEnv {
    pub fn from_process() -> Self {
        Self {
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            env: std::env::vars().collect(),
        }
    }
}

/// What a command produced: text for stdout and the exit code.
#[derive(Debug, Eq, PartialEq)]
pub struct SkillsOutcome {
    pub text: String,
    pub exit: i32,
}

/// A malformed invocation; the caller prints [`SKILLS_USAGE`] and exits 2.
#[derive(Debug, Eq, PartialEq)]
pub struct SkillsUsageError(pub String);

/// The roots a command in `env` looks in, and whether the project's count.
fn roots_of(env: &SkillsEnv) -> Roots {
    let cancel = CancellationToken::new();
    let project = resolve_project_root(&env.cwd, &cancel).ok().map(|f| f.root);
    let home = user_home_from(&env.env);
    let trusted = match (&home, project.as_deref().and_then(identity_of)) {
        (Some(home), Some(identity)) => ProjectTrustStore::open(home.join(TRUST_CATALOG_NAME))
            .get(&identity, &cancel)
            .is_ok_and(|status| status == TrustStatus::Trusted),
        _ => false,
    };
    let extra = crate::skills_runtime::user_extra(home.as_deref(), None, &env.env);
    Roots {
        home,
        project,
        trusted,
        extra: extra.dirs,
        problems: extra.problems,
    }
}

fn identity_of(root: &Path) -> Option<ProjectIdentity> {
    ProjectIdentity::new(root, None).ok()
}

pub fn run(args: &[String], env: &SkillsEnv) -> Result<SkillsOutcome, SkillsUsageError> {
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["--help" | "-h"] => Ok(SkillsOutcome {
            text: SKILLS_USAGE.to_owned(),
            exit: 0,
        }),
        ["list"] => Ok(list(&roots_of(env))),
        ["show", id] => Ok(show(&roots_of(env), id)),
        ["discover", rest @ ..] => discover(rest, env),
        [] => Err(SkillsUsageError(
            "rapid skills: a verb is required".to_owned(),
        )),
        [verb, ..] => Err(SkillsUsageError(format!(
            "rapid skills: unknown usage `{verb}`"
        ))),
    }
}

/// A path as text: whole, with any control character shown as `?`.
fn plain(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { '?' } else { ch })
        .collect()
}

fn flags(entry: &Entry) -> &'static str {
    match (entry.user_invocable, entry.model_invocable) {
        (true, true) => "user+model",
        (true, false) => "user",
        (false, true) => "model",
        (false, false) => "none",
    }
}

fn roots_text(roots: &Roots, catalog: &Catalog) -> String {
    let mut text = String::from("roots\n");
    match &roots.home {
        Some(home) => {
            let dir = home.join(plugin_host::skills::USER_SKILLS_DIR);
            text.push_str(&format!(
                "  user     {}{}\n",
                plain(&dir.display().to_string()),
                if dir.is_dir() { "" } else { " (absent)" }
            ));
        }
        None => text.push_str("  user     (no RapidLM home could be resolved)\n"),
    }
    for dir in &roots.extra {
        text.push_str(&format!(
            "  extra    {}\n",
            plain(&dir.display().to_string())
        ));
    }
    match &roots.project {
        Some(project) => {
            for rel in [
                plugin_host::skills::PROJECT_SKILLS_DIR,
                plugin_host::skills::PROJECT_AGENTS_SKILLS_DIR,
            ] {
                let dir = project.join(rel);
                let note = match (dir.is_dir(), roots.trusted) {
                    (false, _) => " (absent)",
                    (true, true) => "",
                    (true, false) => " (not loaded: this project is not trusted)",
                };
                text.push_str(&format!(
                    "  project  {}{note}\n",
                    plain(&dir.display().to_string())
                ));
            }
        }
        None => text.push_str("  project  (not inside a project)\n"),
    }
    if let Some(failure) = catalog.failure() {
        text.push_str(&format!("discovery failed: {failure}\n"));
    }
    text
}

fn list(roots: &Roots) -> SkillsOutcome {
    let catalog = Catalog::discover(roots);
    let entries = catalog.entries();
    let mut text = format!(
        "skills: {} ({} offered to the model)\n",
        entries.len(),
        entries.iter().filter(|entry| entry.model_invocable).count()
    );
    for entry in &entries {
        text.push_str(&format!(
            "  {}  {}  [{}]  {}\n",
            label(&entry.id),
            entry.root_class.token(),
            flags(entry),
            label(&entry.description)
        ));
    }
    if !catalog.shadowed().is_empty() {
        text.push_str(&format!(
            "hidden by a higher-ranked root ({}):\n",
            catalog.shadowed().len()
        ));
        for hidden in catalog.shadowed() {
            text.push_str(&format!(
                "  {}  {}  {}  hidden by {}\n",
                label(hidden.id().as_str()),
                RootClass::of(hidden.origin()).token(),
                plain(&hidden.dir().display().to_string()),
                RootClass::of(hidden.shadowed_by()).token()
            ));
        }
    }
    if !catalog.rejected().is_empty() {
        text.push_str(&format!("not loaded ({}):\n", catalog.rejected().len()));
        for rejected in catalog.rejected() {
            let name = rejected
                .dir()
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            text.push_str(&format!(
                "  {}  {}  {}  {}: {}\n",
                label(name),
                RootClass::of(rejected.origin()).token(),
                plain(&rejected.dir().display().to_string()),
                reason_token(rejected.reason()),
                rejected.reason()
            ));
        }
    }
    if catalog.unlisted() > 0 {
        text.push_str(&format!(
            "note: {} more skill(s) than the most that are kept were left out, the lowest-ranked first\n",
            catalog.unlisted()
        ));
    }
    if catalog.unread() > 0 {
        text.push_str("note: a skills directory holds more entries than are read\n");
    }
    if !catalog.problems().is_empty() {
        text.push_str("extra_dirs:\n");
        for problem in catalog.problems() {
            text.push_str(&format!("  {}\n", label(problem)));
        }
    }
    text.push_str(&roots_text(roots, &catalog));
    SkillsOutcome {
        text,
        exit: i32::from(catalog.failure().is_some()),
    }
}

/// The word a refused skill's reason is shown under.
fn reason_token(reason: SkillError) -> &'static str {
    match reason {
        SkillError::NameMismatch => "name_mismatch",
        SkillError::PrivilegeInjection => "privilege_injection",
        SkillError::TooLarge => "too_large",
        SkillError::Symlink => "symlink",
        SkillError::InvalidIdent => "invalid_name",
        SkillError::Unreadable => "unreadable",
        _ => "malformed",
    }
}

/// How deep `discover` looks under a root, and how many entries per root.
const DISCOVER_DEPTH: usize = 3;
const DISCOVER_MAX_ENTRIES: usize = 20_000;
/// Entries of one directory looked at when judging its shape.
const SHAPE_MAX_ENTRIES: usize = 256;

/// `rapid skills discover [--roots <path>...]`: directories that look like a
/// skills directory, found by shape and not by name, and the line that adds
/// one. Reads, never writes.
fn discover(args: &[&str], env: &SkillsEnv) -> Result<SkillsOutcome, SkillsUsageError> {
    let scan: Vec<PathBuf> = match args {
        [] => crate::mcp_install::discover_roots(&env.env),
        ["--roots", rest @ ..] if !rest.is_empty() => rest.iter().map(PathBuf::from).collect(),
        _ => {
            return Err(SkillsUsageError(
                "rapid skills discover: expected nothing, or --roots <path>...".to_owned(),
            ));
        }
    };
    let roots = roots_of(env);
    let mut read: Vec<PathBuf> = roots
        .extra
        .iter()
        .cloned()
        .chain(
            roots
                .home
                .iter()
                .map(|home| home.join(plugin_host::skills::USER_SKILLS_DIR)),
        )
        .filter_map(|dir| std::fs::canonicalize(dir).ok())
        .collect();
    read.sort();
    let found = skill_dirs_by_shape(&scan);
    let mut text = String::new();
    if found.is_empty() {
        let shown: Vec<String> = scan
            .iter()
            .map(|p| plain(&p.display().to_string()))
            .collect();
        text.push_str(&format!(
            "no skills directory found by shape under: {}\n",
            shown.join(", ")
        ));
        return Ok(SkillsOutcome { text, exit: 0 });
    }
    text.push_str(&format!(
        "skills directories found by shape ({}):\n",
        found.len()
    ));
    let mut addable = Vec::new();
    for (dir, count) in &found {
        let already = std::fs::canonicalize(dir)
            .ok()
            .is_some_and(|canonical| read.iter().any(|known| canonical.starts_with(known)));
        text.push_str(&format!(
            "  {}  ({count} skill(s)){}\n",
            plain(&dir.display().to_string()),
            if already { "  already read" } else { "" }
        ));
        if !already {
            addable.push(dir.display().to_string());
        }
    }
    if !addable.is_empty() {
        let list: Vec<String> = addable.iter().map(|dir| format!("{dir:?}")).collect();
        text.push_str(&format!(
            "to read one, add it to your config:\n  [skills]\n  extra_dirs = [{}]\n",
            list.join(", ")
        ));
    }
    text.push_str("nothing was changed\n");
    Ok(SkillsOutcome { text, exit: 0 })
}

/// Directories under `roots` (to [`DISCOVER_DEPTH`]) whose every folder holds
/// a valid `SKILL.md` named for it, with how many folders that is. Symlinked
/// directories are not followed; a directory that is such a skills directory
/// is not looked into further.
fn skill_dirs_by_shape(roots: &[PathBuf]) -> Vec<(PathBuf, usize)> {
    let mut found = Vec::new();
    for root in roots {
        let mut visited = 0usize;
        walk_for_skills(root, 0, &mut visited, &mut found);
    }
    found.sort();
    found.dedup();
    found
}

fn walk_for_skills(
    dir: &Path,
    depth: usize,
    visited: &mut usize,
    found: &mut Vec<(PathBuf, usize)>,
) {
    if let Some(count) = shaped_like_skills(dir) {
        found.push((dir.to_path_buf(), count));
        return;
    }
    if depth >= DISCOVER_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        *visited += 1;
        if *visited > DISCOVER_MAX_ENTRIES {
            return;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name();
        let skip = matches!(
            name.to_str(),
            Some(".git" | "node_modules" | "target" | ".cache" | "Library" | ".Trash")
        );
        if kind.is_dir() && !skip {
            walk_for_skills(&entry.path(), depth + 1, visited, found);
        }
    }
}

/// How many folders `dir` holds, if every one of them (hidden ones apart)
/// holds a valid `SKILL.md` named for the folder; `None` otherwise or when it
/// holds none.
fn shaped_like_skills(dir: &Path) -> Option<usize> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut count = 0usize;
    for (seen, entry) in entries.flatten().enumerate() {
        if seen >= SHAPE_MAX_ENTRIES {
            return None;
        }
        let name = entry.file_name();
        let name = name.to_str()?;
        if name.starts_with('.') {
            continue;
        }
        let kind = entry.file_type().ok()?;
        if kind.is_file() {
            continue;
        }
        if !kind.is_dir() {
            return None;
        }
        let file = entry.path().join(plugin_host::skills::SKILL_FILE_NAME);
        let meta = std::fs::symlink_metadata(&file).ok()?;
        if !meta.is_file() || meta.len() > plugin_host::skills::DEFAULT_MAX_SKILL_BYTES as u64 {
            return None;
        }
        let bytes = std::fs::read(&file).ok()?;
        let (manifest, _) =
            plugin_host::skills::parse_skill(&bytes, &plugin_host::skills::SkillLimits::new())
                .ok()?;
        if manifest.name().as_str() != name {
            return None;
        }
        count += 1;
    }
    (count > 0).then_some(count)
}

fn show(roots: &Roots, id: &str) -> SkillsOutcome {
    let catalog = Catalog::discover(roots);
    let Some(shown) = catalog.show(id) else {
        let mut text = format!("no skill named `{}`\n", label(id));
        let near = catalog.near(id);
        if !near.is_empty() {
            text.push_str(&format!("near matches: {}\n", near.join(", ")));
        }
        if !roots.trusted
            && roots.project.as_deref().is_some_and(|project| {
                project
                    .join(plugin_host::skills::PROJECT_SKILLS_DIR)
                    .join(id)
                    .is_dir()
            })
        {
            text.push_str(
                "a skill of that name is in this project's .rapidlm/skills, which is not \
loaded until the project is trusted (`rapid trust grant`)\n",
            );
        }
        return SkillsOutcome { text, exit: 1 };
    };
    let entry = &shown.entry;
    let mut text = format!(
        "skill: {}\ndescription: {}\nroot: {} ({})\nuser_invocable: {}\nmodel_invocable: {}\n",
        label(&entry.id),
        label(&entry.description),
        entry.root_class.token(),
        plain(&shown.dir.display().to_string()),
        entry.user_invocable,
        entry.model_invocable
    );
    if !shown.resources.is_empty() {
        text.push_str(&format!("resources: {}\n", shown.resources.join(", ")));
    }
    if !shown.allowed_tools.is_empty() {
        text.push_str(&format!(
            "allowed-tools (descriptive only): {}\n",
            shown.allowed_tools.join(", ")
        ));
    }
    text.push_str("---\n");
    // The body is a file's text: no control character reaches the terminal.
    text.push_str(
        &shown
            .instructions
            .chars()
            .map(|ch| {
                if ch.is_control() && !matches!(ch, '\n' | '\t') {
                    '?'
                } else {
                    ch
                }
            })
            .collect::<String>(),
    );
    if !shown.instructions.ends_with('\n') {
        text.push('\n');
    }
    SkillsOutcome { text, exit: 0 }
}

/// The doctor's `skills` row: how many skills there are and why any root was
/// left out.
pub(crate) fn doctor_summary(roots: &Roots) -> (String, Option<String>) {
    let catalog = Catalog::discover(roots);
    if let Some(failure) = catalog.failure() {
        return (
            format!("skill discovery failed: {failure}"),
            Some("reduce the skills in the skills directories or fix the one named".to_owned()),
        );
    }
    let entries = catalog.entries();
    let offered = entries.iter().filter(|entry| entry.model_invocable).count();
    let mut detail = format!("{} skill(s), {offered} offered to the model", entries.len());
    let mut remedies: Vec<&str> = Vec::new();
    if !catalog.shadowed().is_empty() {
        detail.push_str(&format!(
            "; {} hidden by a higher-ranked root of the same id",
            catalog.shadowed().len()
        ));
    }
    if !catalog.rejected().is_empty() {
        detail.push_str(&format!("; {} not loaded", catalog.rejected().len()));
        remedies.push("`rapid skills list` says why each was refused; fix or remove it");
    }
    if catalog.unlisted() > 0 {
        detail.push_str(&format!(
            "; {} past the cap on how many are kept",
            catalog.unlisted()
        ));
        remedies.push("remove skills you do not use");
    }
    if !catalog.problems().is_empty() {
        detail.push_str(&format!(
            "; {} extra_dirs entry(ies) ignored: {}",
            catalog.problems().len(),
            catalog.problems().join("; ")
        ));
        remedies.push("fix the `[skills] extra_dirs` entries named");
    }
    let project_skipped = !roots.trusted
        && roots.project.as_deref().is_some_and(|project| {
            [
                plugin_host::skills::PROJECT_SKILLS_DIR,
                plugin_host::skills::PROJECT_AGENTS_SKILLS_DIR,
            ]
            .iter()
            .any(|rel| {
                std::fs::read_dir(project.join(rel)).is_ok_and(|mut dir| dir.next().is_some())
            })
        });
    if project_skipped {
        detail.push_str("; the project's skills are not loaded: the project is not trusted");
        remedies.push("run `rapid trust grant` in the project if you trust its skills");
    }
    (detail, (!remedies.is_empty()).then(|| remedies.join("; ")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    struct Dir(PathBuf);

    impl Dir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "rapid-skills-admin-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::SeqCst)
            ));
            fs::create_dir_all(&dir).expect("temp dir");
            Self(dir)
        }

        fn skill(&self, rel: &str, id: &str, frontmatter: &str, body: &str) {
            let dir = self.0.join(rel).join(id);
            fs::create_dir_all(&dir).expect("skill dir");
            fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {id}\n{frontmatter}---\n{body}"),
            )
            .expect("skill file");
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn roots(home: &Dir, project: &Dir, trusted: bool) -> Roots {
        Roots {
            home: Some(home.0.clone()),
            project: Some(project.0.clone()),
            trusted,
            ..Default::default()
        }
    }

    #[test]
    fn list_names_each_skill_its_root_and_who_may_invoke_it() {
        let home = Dir::new();
        let project = Dir::new();
        home.skill("skills", "release", "description: Cut a release\n", "x\n");
        home.skill(
            "skills",
            "people",
            "description: For people\nmodel_invocable: false\n",
            "x\n",
        );
        project.skill(
            ".rapidlm/skills",
            "local",
            "description: Here\nuser_invocable: false\n",
            "x\n",
        );
        let out = list(&roots(&home, &project, true));
        assert_eq!(out.exit, 0);
        assert!(
            out.text.starts_with("skills: 3 (2 offered to the model)\n"),
            "{}",
            out.text
        );
        assert!(
            out.text
                .contains("  release  user  [user+model]  Cut a release\n"),
            "{}",
            out.text
        );
        assert!(
            out.text.contains("  people  user  [user]  For people\n"),
            "{}",
            out.text
        );
        assert!(
            out.text.contains("  local  project  [model]  Here\n"),
            "{}",
            out.text
        );
        assert!(out.text.contains("roots\n  user     "), "{}", out.text);
    }

    #[test]
    fn an_untrusted_projects_skills_are_not_listed_and_the_listing_says_why() {
        let home = Dir::new();
        let project = Dir::new();
        project.skill(".rapidlm/skills", "local", "description: Here\n", "x\n");
        let out = list(&roots(&home, &project, false));
        assert!(
            out.text.starts_with("skills: 0 (0 offered to the model)\n"),
            "{}",
            out.text
        );
        assert!(
            out.text
                .contains("(not loaded: this project is not trusted)"),
            "{}",
            out.text
        );
        let shown = show(&roots(&home, &project, false), "local");
        assert_eq!(shown.exit, 1);
        assert!(shown.text.contains("rapid trust grant"), "{}", shown.text);
        let (detail, remediation) = doctor_summary(&roots(&home, &project, false));
        assert!(detail.contains("not trusted"), "{detail}");
        assert!(remediation.is_some());
    }

    #[test]
    fn show_prints_the_frontmatter_the_root_and_the_whole_body() {
        let home = Dir::new();
        let project = Dir::new();
        let body: String = (0..800).map(|i| format!("line {i}\n")).collect();
        home.skill(
            "skills",
            "big",
            "description: A long one\nresources:\n  - style.md\nallowed-tools:\n  - repo_read\n",
            &body,
        );
        fs::write(home.0.join("skills/big/style.md"), "style\n").expect("resource");
        let out = show(&roots(&home, &project, true), "big");
        assert_eq!(out.exit, 0);
        assert!(
            out.text
                .starts_with("skill: big\ndescription: A long one\nroot: user ("),
            "{}",
            out.text
        );
        assert!(out.text.contains("model_invocable: true\n"));
        assert!(out.text.contains("resources: style.md\n"), "{}", out.text);
        assert!(
            out.text
                .contains("allowed-tools (descriptive only): repo_read\n")
        );
        assert!(out.text.contains("---\nline 0\n"));
        assert!(out.text.contains("line 799\n"), "the body is whole");
    }

    #[test]
    fn list_says_what_a_higher_root_hides_and_what_it_refused_and_why() {
        let home = Dir::new();
        let project = Dir::new();
        let shared = Dir::new();
        home.skill("skills", "fmt", "description: user\n", "x\n");
        project.skill(".rapidlm/skills", "fmt", "description: project\n", "x\n");
        project.skill(".agents/skills", "fmt", "description: agents\n", "x\n");
        // Refused for each of the reasons the report names.
        let user_skills = home.0.join("skills");
        let write = |dir: &str, frontmatter: &str, body: &str| {
            let path = user_skills.join(dir);
            fs::create_dir_all(&path).expect("dir");
            fs::write(
                path.join("SKILL.md"),
                format!("---\n{frontmatter}---\n{body}"),
            )
            .expect("file");
        };
        write("mismatch", "name: other\ndescription: d\n", "x\n");
        write(
            "grabby",
            "name: grabby\ndescription: d\ntools: all\n",
            "x\n",
        );
        write("broken", "this is not frontmatter\n", "x\n");
        #[cfg(unix)]
        std::os::unix::fs::symlink(user_skills.join("fmt"), user_skills.join("linked"))
            .expect("link");
        let mut roots = roots(&home, &project, true);
        roots.extra = vec![shared.0.clone()];
        roots.problems = vec!["extra_dirs[1] \"/gone\": no such directory".to_owned()];
        let out = list(&roots);
        assert_eq!(out.exit, 0, "{}", out.text);
        assert!(
            out.text.starts_with("skills: 1 (1 offered to the model)\n"),
            "{}",
            out.text
        );
        assert!(
            out.text.contains("hidden by a higher-ranked root (2):\n"),
            "{}",
            out.text
        );
        assert_eq!(
            out.text.matches("hidden by user").count(),
            2,
            "{}",
            out.text
        );
        assert!(out.text.contains("not loaded ("), "{}", out.text);
        for expected in [
            "name_mismatch: skill name is not the name of its directory",
            "privilege_injection: ",
            "malformed: ",
        ] {
            assert!(out.text.contains(expected), "{expected:?} in {}", out.text);
        }
        #[cfg(unix)]
        assert!(out.text.contains("symlink: "), "{}", out.text);
        assert!(
            out.text
                .contains("extra_dirs:\n  extra_dirs[1] \"/gone\": no such directory"),
            "{}",
            out.text
        );
        // The roots, with the extra directory and both project ones.
        assert!(out.text.contains("  extra    "), "{}", out.text);
        assert!(out.text.contains(".rapidlm/skills") && out.text.contains(".agents/skills"));
    }

    /// Every path under `root` with its length, for "nothing was written".
    fn tree(root: &Path) -> Vec<(PathBuf, u64)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).expect("read").flatten() {
                let meta = entry.metadata().expect("meta");
                out.push((entry.path(), meta.len()));
                if meta.is_dir() {
                    stack.push(entry.path());
                }
            }
        }
        out.sort();
        out
    }

    #[test]
    fn discover_finds_a_skills_directory_by_its_shape_and_changes_nothing() {
        let base = Dir::new();
        // A neutral name, three levels down, every folder a valid skill.
        let found = base.0.join("stuff/things/collected");
        for id in ["alpha", "beta"] {
            let dir = found.join(id);
            fs::create_dir_all(&dir).expect("dir");
            fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {id}\ndescription: d\n---\nx\n"),
            )
            .expect("skill");
        }
        // Not skills directories: one folder without a SKILL.md; a name that
        // is not the folder's; one too deep; a hidden folder is ignored.
        fs::create_dir_all(base.0.join("stuff/decoy/one")).expect("decoy");
        fs::write(base.0.join("stuff/decoy/one/notes.md"), "x").expect("note");
        let mismatch = base.0.join("stuff/liar/real");
        fs::create_dir_all(&mismatch).expect("dir");
        fs::write(
            mismatch.join("SKILL.md"),
            "---\nname: other\ndescription: d\n---\nx\n",
        )
        .expect("skill");
        let deep = base.0.join("a/b/c/too-deep/one");
        fs::create_dir_all(&deep).expect("dir");
        fs::write(
            deep.join("SKILL.md"),
            "---\nname: one\ndescription: d\n---\nx\n",
        )
        .expect("skill");
        let before = tree(&base.0);
        let env = SkillsEnv {
            cwd: base.0.clone(),
            env: vec![(
                "RAPIDLM_CONFIG_ROOTS".to_owned(),
                base.0.display().to_string(),
            )],
        };
        let out = run(&["discover".to_owned()], &env).expect("discover");
        assert_eq!(out.exit, 0, "{}", out.text);
        assert!(
            out.text
                .starts_with("skills directories found by shape (1):\n"),
            "{}",
            out.text
        );
        assert!(
            out.text
                .contains(&format!("{}  (2 skill(s))", found.display())),
            "{}",
            out.text
        );
        assert!(
            out.text.contains(&format!(
                "[skills]\n  extra_dirs = [{:?}]",
                found.display().to_string()
            )),
            "the line to add: {}",
            out.text
        );
        assert!(out.text.ends_with("nothing was changed\n"));
        assert!(!out.text.contains("decoy") && !out.text.contains("liar"));
        assert!(!out.text.contains("too-deep"), "{}", out.text);
        assert_eq!(tree(&base.0), before, "nothing was written");

        // --roots names the places to look instead.
        let explicit = run(
            &[
                "discover".to_owned(),
                "--roots".to_owned(),
                base.0.join("stuff").display().to_string(),
            ],
            &SkillsEnv {
                cwd: base.0.clone(),
                env: Vec::new(),
            },
        )
        .expect("discover");
        assert!(explicit.text.contains("(2 skill(s))"), "{}", explicit.text);
        // A directory the loader reads already is marked, and no line is
        // offered to add it.
        let home_skills = base.0.join("home").join("skills");
        let mine = home_skills.join("mine");
        fs::create_dir_all(&mine).expect("dir");
        fs::write(
            mine.join("SKILL.md"),
            "---\nname: mine\ndescription: d\n---\nx\n",
        )
        .expect("skill");
        let already = run(
            &[
                "discover".to_owned(),
                "--roots".to_owned(),
                base.0.join("home").display().to_string(),
            ],
            &SkillsEnv {
                cwd: base.0.clone(),
                env: vec![(
                    "RAPIDLM_HOME".to_owned(),
                    base.0.join("home").display().to_string(),
                )],
            },
        )
        .expect("discover");
        assert!(
            already.text.contains("(1 skill(s))  already read"),
            "{}",
            already.text
        );
        assert!(!already.text.contains("extra_dirs = ["), "{}", already.text);
        // A usage error, not a guess.
        assert!(run(&["discover".to_owned(), "--roots".to_owned()], &env).is_err());
        assert!(run(&["discover".to_owned(), "bogus".to_owned()], &env).is_err());
        // Nothing found says where it looked.
        let none = run(
            &[
                "discover".to_owned(),
                "--roots".to_owned(),
                base.0.join("stuff/decoy").display().to_string(),
            ],
            &env,
        )
        .expect("discover");
        assert!(
            none.text
                .starts_with("no skills directory found by shape under: "),
            "{}",
            none.text
        );
    }

    #[test]
    fn past_the_cap_the_first_sixty_four_are_kept_and_the_rest_are_counted() {
        let home = Dir::new();
        let project = Dir::new();
        for i in 0..66 {
            home.skill("skills", &format!("s{i:02}"), "description: d\n", "x\n");
        }
        let out = list(&roots(&home, &project, true));
        assert_eq!(out.exit, 0, "a cap is a note, not a failure: {}", out.text);
        assert!(
            out.text
                .starts_with("skills: 64 (64 offered to the model)\n"),
            "{}",
            out.text
        );
        assert!(
            out.text
                .contains("note: 2 more skill(s) than the most that are kept were left out"),
            "{}",
            out.text
        );
        let (detail, remedy) = doctor_summary(&roots(&home, &project, true));
        assert!(
            detail.contains("2 past the cap on how many are kept"),
            "{detail}"
        );
        assert!(remedy.is_some());
    }

    #[test]
    fn show_names_the_directory_the_file_is_in_and_cleans_control_characters() {
        let home = Dir::new();
        let project = Dir::new();
        let shared = Dir::new();
        // A skill in an extra directory, with escape sequences in its text.
        let dir = shared.0.join("beta");
        fs::create_dir_all(&dir).expect("dir");
        fs::write(
            dir.join("SKILL.md"),
            "---\nname: beta\ndescription: d\n---\n\u{1b}[31mred\u{1b}[0m text\n",
        )
        .expect("skill");
        let mut roots = roots(&home, &project, true);
        roots.extra = vec![shared.0.clone()];
        let out = show(&roots, "beta");
        assert_eq!(out.exit, 0, "{}", out.text);
        assert!(
            out.text
                .contains(&format!("root: extra ({})", dir.display())),
            "the real directory, and the kind of root: {}",
            out.text
        );
        assert!(!out.text.contains('\u{1b}'), "{:?}", out.text);
        assert!(out.text.contains("?[31mred?[0m text"), "{}", out.text);
    }

    #[test]
    fn show_of_an_unknown_id_names_near_matches() {
        let home = Dir::new();
        let project = Dir::new();
        home.skill("skills", "release", "description: d\n", "x\n");
        let out = show(&roots(&home, &project, true), "realease");
        assert_eq!(out.exit, 1);
        assert!(out.text.contains("near matches: release"), "{}", out.text);
    }

    #[test]
    fn verbs_are_checked() {
        let env = SkillsEnv {
            cwd: std::env::temp_dir(),
            env: Vec::new(),
        };
        assert!(run(&[], &env).is_err());
        assert!(run(&["show".to_owned()], &env).is_err());
        assert!(run(&["enable".to_owned(), "x".to_owned()], &env).is_err());
    }
}
