//! What the permission rules and the managed policy say about skills
//! (EXT-04 §3.3).
//!
//! Policy only narrows. A `skill(<glob>)` rule is written by the user, a
//! project or the organisation — never by a skill, whose frontmatter cannot
//! carry a permission. `deny` hides a skill and refuses it for the model and
//! for a typed `/skill` alike; `ask` makes a *model's* activation wait for an
//! approval (a person typing the name has already asked); `allow` is the
//! default. The managed policy adds `denied_ids` (id globs refused whatever
//! the rules say) and `project_skills = false` (a project's own skills are
//! not read at all). The user's config bounds how many skills the model may
//! load by itself.

use std::path::Path;

use crate::managed_config::ManagedPolicy;
use crate::permissions::{
    PermissionLattice, PermissionMode, ToolClass, glob_match, parse_settings,
};

/// The tool name a skill rule is written under: `skill(release*)`.
pub(crate) const RULE_TOOL: &str = "skill";

/// How many skills the model may load in one turn when the config says
/// nothing.
pub(crate) const DEFAULT_PER_TURN: u32 = 8;
/// How many skills the model may load in one session when the config says
/// nothing.
pub(crate) const DEFAULT_PER_SESSION: u32 = 64;
/// The largest value either bound may be given.
pub(crate) const MAX_BOUND: u32 = 1_000;

/// Why a skill is refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Denial {
    /// A `skill(...)` deny rule, a managed tool ban, or a standing "never
    /// allow" answer.
    Rule,
    /// The managed policy's `denied_ids`.
    Managed,
}

impl Denial {
    pub(crate) const fn describe(self) -> &'static str {
        match self {
            Self::Rule => "a skill(...) deny rule",
            Self::Managed => "the managed policy (skills.denied_ids)",
        }
    }
}

/// Which bound a model activation ran into.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Bound {
    Turn,
    Session,
}

/// The rules and limits one session's skills answer to.
#[derive(Clone, Debug)]
pub(crate) struct SkillPolicy {
    /// The turn's permission lattice, for the `skill(...)` rules, the
    /// managed tool bans and the user's standing refusals it holds.
    lattice: Option<PermissionLattice>,
    /// The managed policy's `denied_ids`, lower-cased.
    managed_ids: Vec<String>,
    /// Whether a project's own skills may be read at all.
    project_skills: bool,
    per_turn: u32,
    per_session: u32,
    /// What was wrong with a setting that was left out.
    problems: Vec<String>,
}

impl Default for SkillPolicy {
    /// No rules, no managed policy, the default bounds: what a session with
    /// nothing configured has.
    fn default() -> Self {
        Self {
            lattice: None,
            managed_ids: Vec::new(),
            project_skills: true,
            per_turn: DEFAULT_PER_TURN,
            per_session: DEFAULT_PER_SESSION,
            problems: Vec::new(),
        }
    }
}

impl SkillPolicy {
    /// The policy for a session in `project` (when it is in one) with
    /// `home` as the RapidLM home: the permission rules the project's
    /// settings files, the managed policy and the user's standing refusals
    /// give the `skill` tool, the managed `[skills]` table, and the bounds
    /// `config` names. Everything is read from where it is, never from the
    /// working directory, and nothing is written or made.
    pub(crate) fn load(
        project: Option<&Path>,
        trusted: bool,
        home: Option<&Path>,
        config: Option<&Path>,
        env: &[(String, String)],
    ) -> Self {
        let mut policy = Self::default();
        let managed = match crate::managed_config::load_policy(env) {
            Ok(managed) => managed,
            // A policy that cannot be read is no policy to rely on: a
            // project's skills are text the model would follow, so they are
            // not read.
            Err(error) => {
                policy.project_skills = false;
                policy.problems.push(format!(
                    "the managed policy could not be loaded ({error}): project skills are not read"
                ));
                None
            }
        };
        if let Some(managed) = &managed {
            let skills = managed.skills();
            policy.managed_ids = skills
                .denied_ids
                .iter()
                .map(|id| id.to_ascii_lowercase())
                .collect();
            policy.project_skills = skills.project_skills.unwrap_or(true);
        }
        policy.lattice = Some(policy.rules_lattice(project, trusted, home, managed.as_ref()));
        if let Some(config) = config {
            policy.bounds_from(config);
        }
        policy
    }

    /// A lattice holding only what can refuse a `skill` call: the project's
    /// deny rules (a trusted project's settings files), the managed tool
    /// bans, and the user's standing "never allow" answers.
    fn rules_lattice(
        &mut self,
        project: Option<&Path>,
        trusted: bool,
        home: Option<&Path>,
        managed: Option<&ManagedPolicy>,
    ) -> PermissionLattice {
        let mut loaded = Vec::new();
        if trusted && let Some(project) = project {
            for file in crate::interactive::PROJECT_SETTINGS_FILES {
                let Ok(text) = std::fs::read_to_string(project.join(file)) else {
                    continue;
                };
                match parse_settings(&text) {
                    Ok(settings) => loaded.push(settings),
                    Err(_) => self.problems.push(format!(
                        "{file} could not be read: its skill rules are not applied"
                    )),
                }
            }
        }
        let rules = crate::interactive::merge_settings_rules(&loaded);
        let mut lattice = PermissionLattice::new(PermissionMode::Default).with_rules(rules);
        if let Some(patterns) = managed.and_then(ManagedPolicy::denied_tools) {
            lattice = lattice.with_denied_tools(patterns.iter().cloned());
        }
        if let (Some(project), Some(home)) = (project, home) {
            lattice = match crate::interactive::persisted_denials_for(project, home) {
                Ok(denials) => lattice.with_denials(denials),
                Err(reason) => {
                    self.problems.push(format!(
                        "{reason}: every skill is refused until it is fixed"
                    ));
                    lattice.with_unreadable_denials()
                }
            };
        }
        lattice
    }

    /// `[skills] max_model_activations_per_turn` and `…_per_session` of the
    /// user config: whole numbers from 0 to [`MAX_BOUND`].
    fn bounds_from(&mut self, config: &Path) {
        let Ok(text) = std::fs::read_to_string(config) else {
            return;
        };
        let Ok(parsed) = toml::from_str::<toml::Value>(&text) else {
            return;
        };
        let Some(table) = parsed.get("skills").and_then(toml::Value::as_table) else {
            return;
        };
        for (key, slot) in [
            ("max_model_activations_per_turn", &mut self.per_turn),
            ("max_model_activations_per_session", &mut self.per_session),
        ] {
            let Some(value) = table.get(key) else {
                continue;
            };
            match value
                .as_integer()
                .and_then(|n| u32::try_from(n).ok())
                .filter(|n| *n <= MAX_BOUND)
            {
                Some(n) => *slot = n,
                None => self.problems.push(format!(
                    "[skills] {key} must be a whole number from 0 to {MAX_BOUND}; the default is used"
                )),
            }
        }
    }

    /// Why `id` is refused, when it is.
    pub(crate) fn denial(&self, id: &str) -> Option<Denial> {
        let lowered = id.to_ascii_lowercase();
        if self
            .managed_ids
            .iter()
            .any(|glob| glob_match(glob, &lowered))
        {
            return Some(Denial::Managed);
        }
        self.lattice
            .as_ref()?
            .evaluate(RULE_TOOL, id, ToolClass::ReadOnly)
            .is_denied()
            .then_some(Denial::Rule)
    }

    /// Whether a project's own skills may be read.
    pub(crate) fn project_skills(&self) -> bool {
        self.project_skills
    }

    pub(crate) fn bound(&self, which: Bound) -> u32 {
        match which {
            Bound::Turn => self.per_turn,
            Bound::Session => self.per_session,
        }
    }

    /// What was wrong with a setting that was left out.
    pub(crate) fn problems(&self) -> &[String] {
        &self.problems
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::{PermissionMode, parse_settings};

    fn lattice(settings: &str) -> PermissionLattice {
        let parsed = parse_settings(settings).expect("settings");
        PermissionLattice::new(PermissionMode::Default).with_rules(parsed.rules)
    }

    fn policy_with(settings: &str) -> SkillPolicy {
        SkillPolicy {
            lattice: Some(lattice(settings)),
            ..SkillPolicy::default()
        }
    }

    fn dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rapid-skills-policy-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    fn managed(dir: &Path, tables: &str) -> Vec<(String, String)> {
        let path = dir.join("managed.toml");
        std::fs::write(
            &path,
            format!("schema = \"rapidlm.managed_config.v1\"\n[policy]\n{tables}"),
        )
        .expect("policy");
        vec![(
            crate::managed_config::MANAGED_CONFIG_ENV.to_owned(),
            path.display().to_string(),
        )]
    }

    #[test]
    fn a_managed_tool_ban_naming_skills_refuses_them_and_the_managed_table_is_read() {
        let base = dir("managed");
        let env = managed(
            &base,
            "denied_tools = [\"skill(secret*)\"]\n[skills]\ndenied_ids = [\"deploy\"]\nproject_skills = false\n",
        );
        let policy = SkillPolicy::load(None, true, None, None, &env);
        assert_eq!(policy.denial("secret-sauce"), Some(Denial::Rule));
        assert_eq!(policy.denial("deploy"), Some(Denial::Managed));
        assert_eq!(policy.denial("release"), None);
        assert!(!policy.project_skills());
        assert!(policy.problems().is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_managed_policy_that_cannot_be_read_turns_project_skills_off_and_says_so() {
        let env = vec![(
            crate::managed_config::MANAGED_CONFIG_ENV.to_owned(),
            "/no/such/policy.toml".to_owned(),
        )];
        let policy = SkillPolicy::load(None, true, None, None, &env);
        assert!(!policy.project_skills(), "no policy to rely on");
        assert_eq!(policy.problems().len(), 1);
        assert!(policy.problems()[0].contains("managed policy could not be loaded"));
    }

    #[test]
    fn a_trusted_projects_settings_are_read_from_the_project_and_an_untrusted_ones_are_not() {
        let project = dir("project");
        std::fs::create_dir_all(project.join(".rapidlm")).expect("dir");
        std::fs::write(
            project.join(".rapidlm/settings.json"),
            r#"{"permissions":{"deny":["skill(deploy*)"]}}"#,
        )
        .expect("settings");
        let trusted = SkillPolicy::load(Some(&project), true, None, None, &[]);
        assert_eq!(trusted.denial("deploy-now"), Some(Denial::Rule));
        let untrusted = SkillPolicy::load(Some(&project), false, None, None, &[]);
        assert_eq!(untrusted.denial("deploy-now"), None);
        // A settings file that cannot be read is said, not skipped silently.
        std::fs::write(project.join(".rapidlm/settings.json"), "{ not json").expect("settings");
        let broken = SkillPolicy::load(Some(&project), true, None, None, &[]);
        assert_eq!(broken.problems().len(), 1, "{:?}", broken.problems());
        assert!(broken.problems()[0].contains("settings.json"));
        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn standing_refusals_the_user_gave_hide_the_skill_and_an_unreadable_store_refuses_all() {
        let project = dir("denials-project");
        let home = dir("denials-home");
        let canonical = std::fs::canonicalize(&project).expect("canonical");
        let store = serde_json::json!({
            "schema": 2,
            "projects": [{
                "root": canonical.to_string_lossy(),
                "allow": [],
                "deny": ["skill(never-me)"]
            }]
        });
        std::fs::write(
            home.join(crate::interactive::PERMISSIONS_STORE_NAME),
            store.to_string(),
        )
        .expect("store");
        let policy = SkillPolicy::load(Some(&project), false, Some(&home), None, &[]);
        assert_eq!(
            policy.denial("never-me"),
            Some(Denial::Rule),
            "{:?}",
            policy.problems()
        );
        assert_eq!(policy.denial("other"), None);
        // A store that exists but cannot be read refuses every skill.
        let unreadable = dir("denials-unreadable");
        std::fs::create_dir_all(unreadable.join(crate::interactive::PERMISSIONS_STORE_NAME))
            .expect("a directory where the store should be");
        let refused = SkillPolicy::load(Some(&project), false, Some(&unreadable), None, &[]);
        assert_eq!(refused.denial("anything"), Some(Denial::Rule));
        assert_eq!(refused.problems().len(), 1);
        for path in [&project, &home, &unreadable] {
            let _ = std::fs::remove_dir_all(path);
        }
    }

    #[test]
    fn a_deny_rule_refuses_the_skills_it_names_and_no_others() {
        let policy = policy_with(r#"{"permissions":{"deny":["skill(deploy*)"]}}"#);
        assert_eq!(policy.denial("deploy"), Some(Denial::Rule));
        assert_eq!(policy.denial("deploy-all"), Some(Denial::Rule));
        assert_eq!(policy.denial("release"), None);
        assert_eq!(policy.denial("redeploy"), None, "the glob is anchored");
    }

    #[test]
    fn an_ask_or_allow_rule_refuses_nothing() {
        let policy =
            policy_with(r#"{"permissions":{"ask":["skill(release)"],"allow":["skill(*)"]}}"#);
        assert_eq!(policy.denial("release"), None);
        assert_eq!(policy.denial("anything"), None);
    }

    #[test]
    fn a_rule_for_another_tool_is_not_a_skill_rule() {
        let policy = policy_with(r#"{"permissions":{"deny":["shell_exec(deploy*)"]}}"#);
        assert_eq!(policy.denial("deploy"), None);
        let whole = policy_with(r#"{"permissions":{"deny":["skill"]}}"#);
        assert_eq!(
            whole.denial("anything"),
            Some(Denial::Rule),
            "a rule with no glob names every skill"
        );
    }

    #[test]
    fn the_managed_ids_are_globs_and_ignore_case() {
        let policy = SkillPolicy {
            managed_ids: vec!["deploy*".to_owned(), "secret-?".to_owned()],
            ..SkillPolicy::default()
        };
        assert_eq!(policy.denial("Deploy-Prod"), Some(Denial::Managed));
        assert_eq!(policy.denial("SECRET-1"), Some(Denial::Managed));
        assert_eq!(policy.denial("secret-12"), None);
        assert_eq!(policy.denial("release"), None);
    }

    #[test]
    fn with_nothing_configured_nothing_is_refused_and_the_bounds_are_the_defaults() {
        let policy = SkillPolicy::default();
        assert_eq!(policy.denial("anything"), None);
        assert!(policy.project_skills());
        assert_eq!(policy.bound(Bound::Turn), 8);
        assert_eq!(policy.bound(Bound::Session), 64);
    }

    #[test]
    fn the_bounds_come_from_the_user_config_and_a_bad_one_is_named() {
        let dir = std::env::temp_dir().join(format!("rapid-skills-policy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let config = dir.join("config.toml");
        let read = |text: &str| {
            std::fs::write(&config, text).expect("config");
            let mut policy = SkillPolicy::default();
            policy.bounds_from(&config);
            policy
        };
        let good = read(
            "[skills]\nmax_model_activations_per_turn = 2\nmax_model_activations_per_session = 0\n",
        );
        assert_eq!(good.bound(Bound::Turn), 2);
        assert_eq!(good.bound(Bound::Session), 0, "zero is a bound: none");
        assert!(good.problems().is_empty());
        for bad in [
            "[skills]\nmax_model_activations_per_turn = -1\n",
            "[skills]\nmax_model_activations_per_turn = 1001\n",
            "[skills]\nmax_model_activations_per_turn = \"8\"\n",
            "[skills]\nmax_model_activations_per_turn = 2.5\n",
        ] {
            let policy = read(bad);
            assert_eq!(policy.bound(Bound::Turn), DEFAULT_PER_TURN, "{bad:?}");
            assert_eq!(policy.problems().len(), 1, "{bad:?}");
            assert!(
                policy.problems()[0].contains("max_model_activations_per_turn"),
                "{:?}",
                policy.problems()
            );
        }
        let top = read("[skills]\nmax_model_activations_per_turn = 1000\n");
        assert_eq!(top.bound(Bound::Turn), 1000);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
