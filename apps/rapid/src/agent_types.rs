//! The agent types a spawn may name, and `rapid agents list` shows: one
//! inventory for both (ADR 0023 §5).
//!
//! Built-ins first, then the project's `.rapidlm/agents` — read only in a
//! trusted project — then the user's `~/.rapidlm/agents`, which never
//! shadows either.

use std::path::{Path, PathBuf};

use agent_runtime::agent_defs::{
    DefInventory, ImplementationRegistry, PROJECT_DEFS_DIR, RejectedDef, builtin_definitions,
    layered_inventory,
};

/// Tool-class implementations this composition root actually links: every
/// class is backed by a real subsystem crate in the binary.
pub(crate) fn implementation_registry() -> ImplementationRegistry {
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

/// The user's definitions directory under this environment's home
/// (`RAPIDLM_HOME`, else `~/.rapidlm`).
pub(crate) fn user_agents_dir(env: &[(String, String)]) -> Option<PathBuf> {
    crate::interactive::user_home_from(env).map(|home| home.join("agents"))
}

/// Every type a spawn in the project at `root` may name. A project whose
/// definitions cannot be loaded at all (a duplicate id, a built-in
/// collision) still spawns built-ins and the user's: the failure is its
/// one rejection, not a refusal of every spawn.
pub(crate) fn spawn_inventory(root: &Path, trusted: bool) -> DefInventory {
    let env: Vec<(String, String)> = std::env::vars().collect();
    let project = trusted.then(|| root.join(PROJECT_DEFS_DIR));
    let user = user_agents_dir(&env);
    inventory_of(project.as_deref(), user.as_deref())
}

/// [`spawn_inventory`] over explicit directories.
pub(crate) fn inventory_of(project: Option<&Path>, user: Option<&Path>) -> DefInventory {
    let registry = implementation_registry();
    match layered_inventory(project, user, &registry) {
        Ok(inventory) => inventory,
        Err(err) => {
            // The project's directory failed as a whole: keep the rest.
            let mut inventory =
                layered_inventory(None, user, &registry).unwrap_or_else(|_| DefInventory {
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
