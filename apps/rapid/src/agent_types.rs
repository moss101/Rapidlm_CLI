//! The agent types a spawn may name, and `rapid agents list` shows: one
//! inventory for both (ADR 0023 §5).
//!
//! Built-ins first, then the project's `.rapidlm/agents` — read only in a
//! trusted project — then the user's `<home>/agents` and the directories
//! `[agents] extra_dirs` names, none of which shadows what ranks above it.
//! A definition is a `.toml` or a `.md` file.

use std::path::{Path, PathBuf};

use agent_runtime::agent_defs::{
    DefInventory, ImplementationRegistry, PROJECT_DEFS_DIR, RejectedDef, USER_DEFS_DIR,
    builtin_definitions, layered_inventory_with, resolves_inside,
};

/// Tool-class implementations this composition root actually links: every
/// class is backed by a real subsystem crate in the binary.
pub(crate) fn implementation_registry() -> ImplementationRegistry {
    use agent_runtime::role_profile::RoleToolClass as Class;
    ImplementationRegistry::new()
        .with_rule_grammar(is_permission_rule)
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

/// The permission rules a definition adds to its child's calls (EXT-05 §3.2):
/// each can only refuse or ask. A key the host cannot read as a rule was
/// refused when the definition loaded, and is left out here.
pub(crate) fn child_rules(
    def: &agent_runtime::agent_defs::AgentDefinition,
) -> Vec<crate::permissions::ToolRule> {
    use agent_runtime::agent_defs::AgentRuleEffect;
    def.permission
        .iter()
        .filter_map(|(key, effect)| {
            Some(crate::permissions::ToolRule {
                effect: match effect {
                    AgentRuleEffect::Deny => crate::permissions::RuleEffect::Deny,
                    AgentRuleEffect::Ask => crate::permissions::RuleEffect::Ask,
                },
                pattern: crate::permissions::ToolPattern::parse(key)?,
            })
        })
        .collect()
}

/// The permission context a defined type's child runs under: the parent's,
/// narrowed for a subagent (and to `write_scope`, when it has one), plus the
/// definition's own rules. Each of those rules can only refuse or ask, so all
/// of them must hold: a definition whose rules do not fit beside the ones the
/// session already has is refused, not trimmed (the trim would drop whichever
/// sorted last, and that may be the `deny`).
pub(crate) fn child_permissions(
    parent: &crate::permissions::PermissionLattice,
    def: &agent_runtime::agent_defs::AgentDefinition,
    write_scope: Option<&str>,
) -> Result<crate::permissions::PermissionLattice, String> {
    let rules = child_rules(def);
    if rules.len() > parent.rule_room() {
        return Err(format!(
            "agent type '{}' adds {} permission rule(s) to the {} this session already holds, \
past the {} a session may hold; remove rules from the definition or from the settings",
            def.id,
            rules.len(),
            parent.rules().len(),
            crate::permissions::MAX_RULES
        ));
    }
    let mut lattice = parent.for_subagent();
    if let Some(scope) = write_scope {
        lattice = lattice.with_write_scope(scope);
    }
    Ok(lattice.with_rules(rules))
}

/// Whether `key` is a permission rule the host reads: `Tool` or `Tool(glob)`
/// with an exact tool name, as in the project settings.
fn is_permission_rule(key: &str) -> bool {
    crate::permissions::ToolPattern::parse(key).is_some()
}

/// The user's definitions directory under this environment's home
/// (`RAPIDLM_HOME`, else `~/.rapidlm`).
pub(crate) fn user_agents_dir(env: &[(String, String)]) -> Option<PathBuf> {
    crate::interactive::user_home_from(env).map(|home| home.join(USER_DEFS_DIR))
}

/// The directories `[agents] extra_dirs` names in the user's config, and
/// what was wrong with each entry left out. Only the user's own config is
/// read: a project cannot name a directory for its own definitions to come
/// from.
pub(crate) fn extra_dirs(env: &[(String, String)]) -> crate::skills_runtime::ExtraDirs {
    let home = crate::interactive::user_home_path_from(env);
    crate::skills_runtime::user_extra_of("agents", USER_DEFS_DIR, home.as_deref(), None, env)
}

/// Every type a spawn in the project at `root` may name. A project whose
/// definitions cannot be loaded at all (a duplicate id, a built-in
/// collision) still spawns built-ins and the user's: the failure is its
/// one rejection, not a refusal of every spawn.
pub(crate) fn spawn_inventory(root: &Path, trusted: bool) -> DefInventory {
    let env: Vec<(String, String)> = std::env::vars().collect();
    let project = trusted.then(|| root.join(PROJECT_DEFS_DIR));
    // A trusted project's directory is read where it is, in the project: a link
    // on the way to somewhere else is one refusal, not a way to read there.
    let (project, outside) = match project {
        Some(dir) if !resolves_inside(root, &dir) => (None, Some(dir)),
        other => (other, None),
    };
    let user = user_agents_dir(&env);
    let mut inventory = inventory_with(project.as_deref(), user.as_deref(), &extra_dirs(&env).dirs);
    if let Some(dir) = outside {
        inventory.rejected.push(RejectedDef {
            path: dir,
            reason: "resolves outside the project (a link on the way), so it is not read"
                .to_owned(),
        });
    }
    inventory
}

/// [`spawn_inventory`] over explicit directories, without `extra_dirs`.
#[cfg(test)]
pub(crate) fn inventory_of(project: Option<&Path>, user: Option<&Path>) -> DefInventory {
    inventory_with(project, user, &[])
}

/// [`inventory_of`] with the user's `extra_dirs` below the rest.
pub(crate) fn inventory_with(
    project: Option<&Path>,
    user: Option<&Path>,
    extras: &[PathBuf],
) -> DefInventory {
    let registry = implementation_registry();
    match layered_inventory_with(project, user, extras, &registry) {
        Ok(inventory) => inventory,
        Err(err) => {
            // The project's directory failed as a whole: keep the rest.
            let mut inventory = layered_inventory_with(None, user, extras, &registry)
                .unwrap_or_else(|_| DefInventory {
                    loaded: builtin_definitions(),
                    rejected: Vec::new(),
                });
            inventory.rejected.push(RejectedDef {
                path: project.map(Path::to_path_buf).unwrap_or_default(),
                reason: err.to_string(),
            });
            inventory
        }
    }
}
