//! `rapid skills list|show`: what the skills loader finds, offline and
//! read-only.
//!
//! Both commands read through [`crate::skills_runtime`], the same discovery a
//! turn uses, so this can never report a skill the model would not be offered
//! (or hide one it would). Nothing here activates a skill, records an event,
//! spawns a process or opens the network.

use std::path::{Path, PathBuf};

use kernel::{CancellationToken, ProjectIdentity, ProjectTrustStore, TrustStatus};

use crate::interactive::{TRUST_CATALOG_NAME, resolve_project_root, user_home_from};
use crate::mcp_config::label;
use crate::skills_runtime::{Catalog, Entry, Roots};

pub const SKILLS_USAGE: &str = "\
usage: rapid skills list
       rapid skills show <id>

list   every skill the loader finds, with the root it came from and who may
       invoke it, and where the roots are
show   one skill: its description, root, flags and instructions

Skills are found in the user's skills directory (<home>/skills) and, for a
trusted project, <project>/.rapidlm/skills. Both commands only read.
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
    Roots {
        home,
        project,
        trusted,
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
        [] => Err(SkillsUsageError(
            "rapid skills: a verb is required".to_owned(),
        )),
        [verb, ..] => Err(SkillsUsageError(format!(
            "rapid skills: unknown usage `{verb}`"
        ))),
    }
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
                label(&dir.display().to_string()),
                if dir.is_dir() { "" } else { " (absent)" }
            ));
        }
        None => text.push_str("  user     (no RapidLM home could be resolved)\n"),
    }
    match &roots.project {
        Some(project) => {
            let dir = project.join(plugin_host::skills::PROJECT_SKILLS_DIR);
            let note = match (dir.is_dir(), roots.trusted) {
                (false, _) => " (absent)",
                (true, true) => "",
                (true, false) => " (not loaded: this project is not trusted)",
            };
            text.push_str(&format!(
                "  project  {}{note}\n",
                label(&dir.display().to_string())
            ));
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
    text.push_str(&roots_text(roots, &catalog));
    SkillsOutcome {
        text,
        exit: i32::from(catalog.failure().is_some()),
    }
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
        label(&shown.dir.display().to_string()),
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
    text.push_str(&shown.instructions);
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
    let project_skipped = !roots.trusted
        && roots.project.as_deref().is_some_and(|project| {
            std::fs::read_dir(project.join(plugin_host::skills::PROJECT_SKILLS_DIR))
                .is_ok_and(|mut dir| dir.next().is_some())
        });
    if project_skipped {
        detail
            .push_str("; the project's .rapidlm/skills are not loaded: the project is not trusted");
        return (
            detail,
            Some("run `rapid trust grant` in the project if you trust its skills".to_owned()),
        );
    }
    (detail, None)
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
