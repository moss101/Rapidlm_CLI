//! Agent definitions: a validated artifact declaring a named agent type —
//! compiled-in built-ins, the project's `<project>/.rapidlm/agents/*.toml`,
//! and the user's `~/.rapidlm/agents/*.toml`, in that order of precedence.
//!
//! A definition shapes the model-visible surface of a base role: it narrows
//! the role's tool surface, carries a description for delegation prompts,
//! and may name the child's instructions, model, reasoning effort and
//! declared inputs and outputs (ADR 0023 §5). It never grants authority — the registry surface, policy, and the capability
//! broker still enforce everything at request time. Validation rules are
//! fail-closed:
//!
//! * closed schema — unknown fields and unknown schema versions are rejected;
//! * sources must be regular files — symlinked definitions are refused so a
//!   project cannot make the loader follow a path outside the tree;
//! * a requested tool class must be inside the role's registry surface
//!   (definitions narrow, never widen) AND have a declared runtime
//!   implementation — granting a class the runtime cannot back is an error.

use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::agent::model::AgentRole;
use crate::role_profile::{RoleRegistry, RoleToolClass, RoleToolSurface};

/// Wire identity of the definition artifact format.
pub const AGENT_DEFS_SCHEMA: &str = "rapidlm.agent_defs.v1";

/// Maximum UTF-8 bytes in a definition file.
pub const MAX_DEF_FILE_BYTES: usize = 64 * 1024;
/// Maximum definition id length.
pub const MAX_DEF_ID_BYTES: usize = 64;
/// Maximum UTF-8 bytes in a definition description.
pub const MAX_DEF_DESCRIPTION_BYTES: usize = 512;
/// Upper bound on definition files loaded from one directory.
pub const MAX_DEFS_PER_DIRECTORY: usize = 128;
/// Project directory scanned for definitions, relative to the project root.
pub const PROJECT_DEFS_DIR: &str = ".rapidlm/agents";
/// User directory scanned for definitions, relative to the home directory.
pub const USER_DEFS_DIR: &str = ".rapidlm/agents";
/// Maximum UTF-8 bytes of a definition's `instructions`.
pub const MAX_DEF_INSTRUCTIONS_BYTES: usize = 8 * 1024;
/// Maximum bytes of a definition's `model` id.
pub const MAX_DEF_MODEL_BYTES: usize = 128;
/// Maximum declared `inputs` or `outputs` entries.
pub const MAX_DEF_IO_ENTRIES: usize = 16;
/// Reasoning-effort names a definition may request, in ladder order (the
/// model router's canonical names).
pub const DEF_REASONING_EFFORTS: [&str; 7] =
    ["none", "minimal", "low", "medium", "high", "xhigh", "ultra"];
/// Every `[agent]` field the closed schema accepts.
pub const DEF_AGENT_FIELDS: [&str; 10] = [
    "id",
    "description",
    "base_role",
    "role",
    "tools",
    "instructions",
    "model",
    "reasoning_effort",
    "inputs",
    "outputs",
];

/// Canonical definition id: lowercase letters, digits, single interior
/// dashes. Display never echoes a rejected raw id beyond its length.
#[derive(Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct AgentDefId(String);

/// Typed failures for definition parsing, validation, and loading.
#[derive(Debug)]
pub enum AgentDefError {
    /// The file is malformed TOML or violates the closed schema.
    Parse {
        path: PathBuf,
        reason: String,
    },
    IdInvalid,
    IdTooLong {
        limit: usize,
    },
    DescriptionTooLarge {
        limit: usize,
        observed: usize,
    },
    UnknownRole {
        name: String,
    },
    UnknownToolClass {
        name: String,
    },
    /// Source is a symlink or another non-regular file; loading must not
    /// follow it out of the project tree.
    NotARegularFile {
        path: PathBuf,
    },
    FileTooLarge {
        path: PathBuf,
        limit: usize,
    },
    /// Two project definitions claim the same id.
    DuplicateId {
        id: String,
        first: PathBuf,
        second: PathBuf,
    },
    /// A project definition collides with a compiled-in built-in id.
    BuiltinCollision {
        id: String,
    },
    /// Requested tool class is outside the role's registry surface.
    GrantExceedsRoleSurface {
        id: String,
        role: AgentRole,
        class: RoleToolClass,
    },
    /// A field's value is invalid; `remedy` says what would be accepted.
    FieldInvalid {
        field: &'static str,
        reason: String,
        remedy: String,
    },
    /// Requested tool class has no declared runtime implementation.
    GrantWithoutImplementation {
        id: String,
        class: RoleToolClass,
    },
    TooManyDefinitions {
        limit: usize,
    },
    Io(std::io::Error),
}

impl fmt::Display for AgentDefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse { path, reason } => {
                write!(
                    f,
                    "agent definition {} is invalid: {reason}",
                    path.display()
                )
            }
            Self::IdInvalid => write!(
                f,
                "definition id must be 1-{MAX_DEF_ID_BYTES} bytes of lowercase letters, digits, and single interior dashes"
            ),
            Self::IdTooLong { limit } => {
                write!(f, "definition id exceeds {limit} bytes")
            }
            Self::DescriptionTooLarge { limit, observed } => {
                write!(f, "description is {observed} bytes; limit is {limit} bytes")
            }
            Self::UnknownRole { name } => {
                let roles: Vec<&str> = AgentRole::ALL.iter().map(|role| role.as_str()).collect();
                write!(
                    f,
                    "field `base_role`: unknown role '{name}'; use one of: {}",
                    roles.join(", ")
                )
            }
            Self::UnknownToolClass { name } => {
                let classes: Vec<&str> = RoleToolClass::ALL
                    .iter()
                    .map(|class| class.as_str())
                    .collect();
                write!(
                    f,
                    "field `tools`: unknown tool class '{name}'; use: {}",
                    classes.join(", ")
                )
            }
            Self::NotARegularFile { path } => write!(
                f,
                "agent definition {} is not a regular file (symlinks are not followed)",
                path.display()
            ),
            Self::FileTooLarge { path, limit } => write!(
                f,
                "agent definition {} exceeds {limit} bytes",
                path.display()
            ),
            Self::DuplicateId { id, first, second } => write!(
                f,
                "duplicate definition id '{id}' in {} and {}",
                first.display(),
                second.display()
            ),
            Self::BuiltinCollision { id } => write!(
                f,
                "definition id '{id}' collides with a built-in agent; built-ins cannot be overridden"
            ),
            Self::GrantExceedsRoleSurface { id, role, class } => {
                let exposed: Vec<&str> = RoleRegistry::profile(*role)
                    .tool_surface()
                    .keys()
                    .map(RoleToolClass::as_str)
                    .collect();
                write!(
                    f,
                    "field `tools`: definition '{id}' asks for tool class '{}', which base role \
'{}' does not expose — definitions narrow a role, never widen it. Remove '{}' from `tools` \
(role '{}' exposes: {}), or choose a `base_role` that exposes it",
                    class.as_str(),
                    role.as_str(),
                    class.as_str(),
                    role.as_str(),
                    exposed.join(", ")
                )
            }
            Self::FieldInvalid {
                field,
                reason,
                remedy,
            } => write!(f, "field `{field}`: {reason}; {remedy}"),
            Self::GrantWithoutImplementation { id, class } => write!(
                f,
                "definition '{id}' grants tool class '{}' which has no declared runtime implementation",
                class.as_str()
            ),
            Self::TooManyDefinitions { limit } => {
                write!(f, "more than {limit} definition files in one directory")
            }
            Self::Io(err) => write!(f, "io error: {err}"),
        }
    }
}

impl std::error::Error for AgentDefError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for AgentDefError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl AgentDefId {
    /// Parse the canonical id alphabet. The raw input is never echoed back.
    pub fn parse(raw: &str) -> Result<Self, AgentDefError> {
        if raw.len() > MAX_DEF_ID_BYTES {
            return Err(AgentDefError::IdTooLong {
                limit: MAX_DEF_ID_BYTES,
            });
        }
        let bytes = raw.as_bytes();
        let valid = !bytes.is_empty()
            && bytes
                .iter()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
            && bytes.first() != Some(&b'-')
            && bytes.last() != Some(&b'-')
            && !bytes.windows(2).any(|pair| pair == b"--");
        if valid {
            Ok(Self(raw.to_string()))
        } else {
            Err(AgentDefError::IdInvalid)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AgentDefId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a definition came from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DefSource {
    /// Compiled into the binary; project files cannot override it.
    BuiltIn,
    /// Loaded from a project file (path is the resolved source).
    Project(PathBuf),
    /// Loaded from the user's own directory (`~/.rapidlm/agents`): below
    /// the project's definitions, never over a built-in.
    User(PathBuf),
}

impl DefSource {
    /// The file it came from; `None` for a built-in.
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::BuiltIn => None,
            Self::Project(path) | Self::User(path) => Some(path),
        }
    }

    /// `builtin`, `project` or `user`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::BuiltIn => "builtin",
            Self::Project(_) => "project",
            Self::User(_) => "user",
        }
    }
}

/// One validated agent definition.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentDefinition {
    pub id: AgentDefId,
    pub description: String,
    /// The base role (`base_role`, or the older `role`): the child runs as
    /// it, under its registry surface narrowed by `tool_surface`.
    pub role: AgentRole,
    /// Requested tool classes. Must be a subset of the role's registry
    /// surface; classes absent from the implementation registry fail
    /// validation.
    pub tool_surface: RoleToolSurface,
    /// The child's standing instructions, ahead of its task.
    pub instructions: Option<String>,
    /// A configured model id (`[model.<id>]`) the child runs on instead of
    /// its parent's; resolved, and refused if unknown, when it is spawned.
    pub model: Option<String>,
    /// Reasoning effort for the child's requests, one of
    /// [`DEF_REASONING_EFFORTS`].
    pub reasoning_effort: Option<String>,
    /// Names the parent supplies in the task envelope.
    pub inputs: Vec<String>,
    /// Names the child's result must carry.
    pub outputs: Vec<String>,
    pub source: DefSource,
}

/// Runtime implementation declarations: which tool classes this host can
/// actually back. Hosts declare what they provide; definitions requesting an
/// undeclared class are rejected instead of silently dead.
#[derive(Clone, Debug, Default)]
pub struct ImplementationRegistry {
    declared: Vec<(RoleToolClass, &'static str)>,
}

impl ImplementationRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare that `class` is implemented by the named runtime component.
    pub fn declare(mut self, class: RoleToolClass, implementation: &'static str) -> Self {
        if !self.declared.iter().any(|(c, _)| *c == class) {
            self.declared.push((class, implementation));
        }
        self
    }

    pub fn implements(&self, class: RoleToolClass) -> bool {
        self.declared.iter().any(|(c, _)| *c == class)
    }

    /// The implementation id for a class, when declared.
    pub fn implementation_of(&self, class: RoleToolClass) -> Option<&'static str> {
        self.declared
            .iter()
            .find(|(c, _)| *c == class)
            .map(|(_, id)| *id)
    }
}

impl AgentDefinition {
    /// Parse definition TOML. `source` records provenance; parsing itself is
    /// pure over `bytes`.
    pub fn parse(source: DefSource, bytes: &str) -> Result<Self, AgentDefError> {
        let path = source
            .path()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("<builtin>"));
        let value: toml::Value = toml::from_str(bytes).map_err(|err| AgentDefError::Parse {
            path: path.clone(),
            reason: err.to_string(),
        })?;
        let table = value.as_table().ok_or_else(|| AgentDefError::Parse {
            path: path.clone(),
            reason: "top level must be a table".to_string(),
        })?;
        // Closed schema: schema marker plus the single [agent] table.
        let schema = table
            .get("schema")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| AgentDefError::Parse {
                path: path.clone(),
                reason: "missing schema marker".to_string(),
            })?;
        if schema != AGENT_DEFS_SCHEMA {
            return Err(AgentDefError::Parse {
                path: path.clone(),
                reason: format!("unsupported schema '{schema}'"),
            });
        }
        let agent = table.get("agent").ok_or_else(|| AgentDefError::Parse {
            path: path.clone(),
            reason: "missing [agent] table".to_string(),
        })?;
        let agent = agent.as_table().ok_or_else(|| AgentDefError::Parse {
            path: path.clone(),
            reason: "[agent] must be a table".to_string(),
        })?;
        for key in table.keys() {
            if key != "schema" && key != "agent" {
                return Err(AgentDefError::Parse {
                    path: path.clone(),
                    reason: format!("unknown top-level field '{key}'"),
                });
            }
        }
        for key in agent.keys() {
            if !DEF_AGENT_FIELDS.contains(&key.as_str()) {
                return Err(AgentDefError::Parse {
                    path: path.clone(),
                    reason: format!(
                        "unknown [agent] field '{key}' (fields: {})",
                        DEF_AGENT_FIELDS.join(", ")
                    ),
                });
            }
        }
        let id_raw = agent
            .get("id")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| AgentDefError::Parse {
                path: path.clone(),
                reason: "[agent] id must be a string".to_string(),
            })?;
        let id = AgentDefId::parse(id_raw)?;
        let description = agent
            .get("description")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| AgentDefError::Parse {
                path: path.clone(),
                reason: "[agent] description must be a string".to_string(),
            })?;
        if description.len() > MAX_DEF_DESCRIPTION_BYTES {
            return Err(AgentDefError::DescriptionTooLarge {
                limit: MAX_DEF_DESCRIPTION_BYTES,
                observed: description.len(),
            });
        }
        // `base_role` names it; `role`, the older name, still does. One.
        let role_raw = match (agent.get("base_role"), agent.get("role")) {
            (Some(_), Some(_)) => {
                return Err(AgentDefError::FieldInvalid {
                    field: "base_role",
                    reason: "both `base_role` and `role` are set".to_owned(),
                    remedy: "keep `base_role` and remove `role`".to_owned(),
                });
            }
            (Some(value), None) | (None, Some(value)) => {
                value.as_str().ok_or_else(|| AgentDefError::Parse {
                    path: path.clone(),
                    reason: "[agent] base_role must be a string".to_string(),
                })?
            }
            (None, None) => {
                return Err(AgentDefError::Parse {
                    path: path.clone(),
                    reason: "[agent] base_role must be a string".to_string(),
                });
            }
        };
        let role = parse_role(role_raw).ok_or_else(|| AgentDefError::UnknownRole {
            name: role_raw.to_string(),
        })?;
        let mut tool_surface = RoleToolSurface::none();
        match agent.get("tools") {
            None => {}
            Some(tools) => {
                let names = tools.as_array().ok_or_else(|| AgentDefError::Parse {
                    path: path.clone(),
                    reason: "[agent] tools must be an array of strings".to_string(),
                })?;
                for name in names {
                    let name = name.as_str().ok_or_else(|| AgentDefError::Parse {
                        path: path.clone(),
                        reason: "[agent] tools entries must be strings".to_string(),
                    })?;
                    let class =
                        parse_tool_class(name).ok_or_else(|| AgentDefError::UnknownToolClass {
                            name: name.to_string(),
                        })?;
                    tool_surface = tool_surface.with(class);
                }
            }
        }
        let instructions = optional_text(agent, "instructions")?;
        if let Some(text) = &instructions
            && (text.is_empty() || text.len() > MAX_DEF_INSTRUCTIONS_BYTES || text.contains('\0'))
        {
            return Err(AgentDefError::FieldInvalid {
                field: "instructions",
                reason: format!("{} bytes", text.len()),
                remedy: format!("give 1-{MAX_DEF_INSTRUCTIONS_BYTES} bytes of text"),
            });
        }
        let model = optional_text(agent, "model")?;
        if let Some(model) = &model
            && (model.is_empty()
                || model.len() > MAX_DEF_MODEL_BYTES
                || !model
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._:/-".contains(&b)))
        {
            return Err(AgentDefError::FieldInvalid {
                field: "model",
                reason: "not a model id".to_owned(),
                remedy: format!(
                    "name a configured `[model.<id>]` id: 1-{MAX_DEF_MODEL_BYTES} bytes of \
letters, digits and . _ : / -"
                ),
            });
        }
        let reasoning_effort = optional_text(agent, "reasoning_effort")?;
        if let Some(effort) = &reasoning_effort
            && !DEF_REASONING_EFFORTS.contains(&effort.as_str())
        {
            return Err(AgentDefError::FieldInvalid {
                field: "reasoning_effort",
                reason: "not a reasoning effort".to_owned(),
                remedy: format!("use one of: {}", DEF_REASONING_EFFORTS.join(", ")),
            });
        }
        let inputs = name_list(agent, "inputs")?;
        let outputs = name_list(agent, "outputs")?;
        Ok(Self {
            id,
            description: description.to_string(),
            role,
            tool_surface,
            instructions,
            model,
            reasoning_effort,
            inputs,
            outputs,
            source,
        })
    }

    /// Validate grants against the role registry surface and the runtime
    /// implementation declarations.
    pub fn validate_grants(&self, impls: &ImplementationRegistry) -> Result<(), AgentDefError> {
        let role_surface = RoleRegistry::profile(self.role).tool_surface();
        for class in self.tool_surface.keys() {
            if !role_surface.allows(class) {
                return Err(AgentDefError::GrantExceedsRoleSurface {
                    id: self.id.as_str().to_string(),
                    role: self.role,
                    class,
                });
            }
            if !impls.implements(class) {
                return Err(AgentDefError::GrantWithoutImplementation {
                    id: self.id.as_str().to_string(),
                    class,
                });
            }
        }
        Ok(())
    }

    /// A valid starter definition for `id`, ready to be written to
    /// `<project>/.rapidlm/agents/<id>.toml`.
    pub fn scaffold(id: &AgentDefId) -> String {
        format!(
            "# Agent definition ({AGENT_DEFS_SCHEMA}).\n\
             # Definitions narrow a role's visible tool surface; they never\n\
             # widen it. See RoleRegistry for each role's surface.\n\
             schema = \"{AGENT_DEFS_SCHEMA}\"\n\
             [agent]\n\
             id = \"{id}\"\n\
             description = \"what this agent is for; shown in delegation prompts\"\n\
             base_role = \"explorer\"\n\
             # Optional. Subset of the role surface; classes without a declared\n\
             # runtime implementation are rejected at load.\n\
             tools = [\"read\"]\n\
             # Optional: standing instructions ahead of each task.\n\
             # instructions = \"Report file paths with line numbers.\"\n\
             # Optional: a configured [model.<id>] and a reasoning effort.\n\
             # model = \"fast\"\n\
             # reasoning_effort = \"low\"\n\
             # Optional: names the task supplies, and names the result carries.\n\
             # inputs = [\"question\"]\n\
             # outputs = [\"findings\"]\n"
        )
    }
}

/// Compiled-in definitions. Project files may add agents but never shadow
/// these ids.
pub fn builtin_definitions() -> Vec<AgentDefinition> {
    let builtin =
        |id: &str, description: &str, role: AgentRole, surface: RoleToolSurface| AgentDefinition {
            id: AgentDefId::parse(id).expect("builtin id is canonical"),
            description: description.to_string(),
            role,
            tool_surface: surface,
            instructions: None,
            model: None,
            reasoning_effort: None,
            inputs: Vec::new(),
            outputs: Vec::new(),
            source: DefSource::BuiltIn,
        };
    use RoleToolClass::{Exec, Git, Read, Write};
    vec![
        // The `task_spawn` types: every spawn resolves through this list.
        builtin(
            "general-purpose",
            "Any focused task, with the full tool surface",
            AgentRole::Coder,
            RoleToolSurface::none()
                .with(Read)
                .with(Write)
                .with(Exec)
                .with(Git),
        ),
        builtin(
            "plan",
            "Read-only investigation that returns a plan",
            AgentRole::Planner,
            RoleToolSurface::none().with(Read),
        ),
        builtin(
            "explore",
            "Read-only codebase exploration and summarization",
            AgentRole::Explorer,
            RoleToolSurface::none().with(Read),
        ),
        builtin(
            "patch",
            "Focused code edits with repository tooling",
            AgentRole::Coder,
            RoleToolSurface::none().with(Read).with(Write).with(Git),
        ),
    ]
}

/// Result of scanning a project definitions directory. Rejected files are
/// reported with reasons; they never abort the scan.
#[derive(Clone, Debug, Default)]
pub struct DefInventory {
    pub loaded: Vec<AgentDefinition>,
    pub rejected: Vec<RejectedDef>,
}

/// One file that failed to load, with the typed reason rendered.
#[derive(Clone, Debug, PartialEq)]
pub struct RejectedDef {
    pub path: PathBuf,
    pub reason: String,
}

/// Load every `*.toml` definition from `dir` (default
/// `<project>/.rapidlm/agents`). A missing directory is an empty inventory —
/// projects without custom agents are normal, not an error.
pub fn load_directory(
    dir: &Path,
    impls: &ImplementationRegistry,
) -> Result<DefInventory, AgentDefError> {
    load_directory_from(dir, impls, DefOrigin::Project)
}

/// Which directory a scan reads: the project's, or the user's.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DefOrigin {
    Project,
    User,
}

/// [`load_directory`] for either origin. A project file that collides with
/// a built-in fails the whole scan, as it always has; a user file that does
/// is rejected on its own, so one stray file in a home directory does not
/// hide every other.
pub fn load_directory_from(
    dir: &Path,
    impls: &ImplementationRegistry,
    origin: DefOrigin,
) -> Result<DefInventory, AgentDefError> {
    let mut inventory = DefInventory::default();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(inventory);
        }
        Err(err) => return Err(err.into()),
    };
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "toml") {
            paths.push(path);
        }
    }
    paths.sort();
    if paths.len() > MAX_DEFS_PER_DIRECTORY {
        return Err(AgentDefError::TooManyDefinitions {
            limit: MAX_DEFS_PER_DIRECTORY,
        });
    }
    let builtins = builtin_definitions();
    for path in paths {
        // Refuse symlinks and directories without following them.
        let meta = std::fs::symlink_metadata(&path)?;
        if !meta.is_file() {
            inventory.rejected.push(RejectedDef {
                path: path.clone(),
                reason: AgentDefError::NotARegularFile { path: path.clone() }.to_string(),
            });
            continue;
        }
        // Read through a bounded cap rather than trusting `meta.len()`: the
        // file can grow between the stat above and the read below, which
        // would otherwise buffer past MAX_DEF_FILE_BYTES despite the check
        // having passed.
        let mut bytes = String::new();
        std::fs::File::open(&path)?
            .take(MAX_DEF_FILE_BYTES as u64 + 1)
            .read_to_string(&mut bytes)?;
        if bytes.len() > MAX_DEF_FILE_BYTES {
            inventory.rejected.push(RejectedDef {
                path: path.clone(),
                reason: AgentDefError::FileTooLarge {
                    path: path.clone(),
                    limit: MAX_DEF_FILE_BYTES,
                }
                .to_string(),
            });
            continue;
        }
        let source = match origin {
            DefOrigin::Project => DefSource::Project(path.clone()),
            DefOrigin::User => DefSource::User(path.clone()),
        };
        let def = match AgentDefinition::parse(source, &bytes) {
            Ok(def) => def,
            Err(err) => {
                inventory.rejected.push(RejectedDef {
                    path: path.clone(),
                    reason: err.to_string(),
                });
                continue;
            }
        };
        if let Err(err) = def.validate_grants(impls) {
            inventory.rejected.push(RejectedDef {
                path: path.clone(),
                reason: err.to_string(),
            });
            continue;
        }
        if let Some(existing) = inventory
            .loaded
            .iter()
            .find(|loaded| loaded.id == def.id)
            .cloned()
        {
            return Err(AgentDefError::DuplicateId {
                id: def.id.as_str().to_string(),
                first: existing
                    .source
                    .path()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from("<builtin>")),
                second: path,
            });
        }
        if builtins.iter().any(|b| b.id == def.id) {
            let collision = AgentDefError::BuiltinCollision {
                id: def.id.as_str().to_string(),
            };
            if origin == DefOrigin::User {
                inventory.rejected.push(RejectedDef {
                    path: path.clone(),
                    reason: collision.to_string(),
                });
                continue;
            }
            return Err(collision);
        }
        inventory.loaded.push(def);
    }
    Ok(inventory)
}

/// Combined inventory: built-ins first (immutable), then project defs.
pub fn full_inventory(
    project_dir: &Path,
    impls: &ImplementationRegistry,
) -> Result<DefInventory, AgentDefError> {
    layered_inventory(Some(project_dir), None, impls)
}

/// Every definition a spawn may name: built-ins, then the project's
/// (`None` when the project is untrusted — its files are not read), then
/// the user's. A user definition never shadows a built-in or a project
/// definition of the same id: it is rejected, saying which it lost to. A
/// user directory that cannot be scanned at all is one rejection, not an
/// error — the project's and the built-ins still load.
pub fn layered_inventory(
    project_dir: Option<&Path>,
    user_dir: Option<&Path>,
    impls: &ImplementationRegistry,
) -> Result<DefInventory, AgentDefError> {
    let mut inventory = match project_dir {
        Some(dir) => load_directory(dir, impls)?,
        None => DefInventory::default(),
    };
    let mut loaded = builtin_definitions();
    loaded.append(&mut inventory.loaded);
    if let Some(dir) = user_dir {
        match load_directory_from(dir, impls, DefOrigin::User) {
            Ok(user) => {
                inventory.rejected.extend(user.rejected);
                for def in user.loaded {
                    if let Some(winner) = loaded.iter().find(|held| held.id == def.id) {
                        inventory.rejected.push(RejectedDef {
                            path: def.source.path().map(Path::to_path_buf).unwrap_or_default(),
                            reason: format!(
                                "definition id '{}' is already the {}'s; a user definition \
never shadows it",
                                def.id,
                                match winner.source {
                                    DefSource::BuiltIn => "built-in",
                                    _ => "project",
                                }
                            ),
                        });
                    } else {
                        loaded.push(def);
                    }
                }
            }
            Err(err) => inventory.rejected.push(RejectedDef {
                path: dir.to_path_buf(),
                reason: err.to_string(),
            }),
        }
    }
    inventory.loaded = loaded;
    Ok(inventory)
}

/// A definition by id, or why there is none: the ids there are.
pub fn resolve<'a>(
    inventory: &'a DefInventory,
    id: &str,
) -> Result<&'a AgentDefinition, UnknownAgentType> {
    inventory
        .loaded
        .iter()
        .find(|def| def.id.as_str() == id)
        .ok_or_else(|| UnknownAgentType {
            requested: id.chars().take(MAX_DEF_ID_BYTES).collect(),
            known: inventory
                .loaded
                .iter()
                .map(|def| def.id.as_str().to_owned())
                .collect(),
        })
}

/// A spawn named a type no definition has (ADR 0023 §5).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnknownAgentType {
    pub requested: String,
    pub known: Vec<String>,
}

impl fmt::Display for UnknownAgentType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "no agent type '{}'; the known types are: {}",
            self.requested,
            self.known.join(", ")
        )
    }
}

impl std::error::Error for UnknownAgentType {}

/// An optional string field, as a type error names the field when present
/// with another type.
fn optional_text(
    agent: &toml::map::Map<String, toml::Value>,
    field: &'static str,
) -> Result<Option<String>, AgentDefError> {
    match agent.get(field) {
        None => Ok(None),
        Some(value) => value
            .as_str()
            .map(|text| Some(text.to_owned()))
            .ok_or_else(|| AgentDefError::FieldInvalid {
                field,
                reason: "not a string".to_owned(),
                remedy: "give a string".to_owned(),
            }),
    }
}

/// `inputs` / `outputs`: at most [`MAX_DEF_IO_ENTRIES`] distinct names in
/// the id alphabet (lowercase letters, digits, `_` and interior `-`).
fn name_list(
    agent: &toml::map::Map<String, toml::Value>,
    field: &'static str,
) -> Result<Vec<String>, AgentDefError> {
    let invalid = |reason: String| AgentDefError::FieldInvalid {
        field,
        reason,
        remedy: format!(
            "give at most {MAX_DEF_IO_ENTRIES} distinct names of lowercase letters, digits, \
`_` and `-`"
        ),
    };
    let Some(value) = agent.get(field) else {
        return Ok(Vec::new());
    };
    let entries = value
        .as_array()
        .ok_or_else(|| invalid("not an array of names".to_owned()))?;
    if entries.len() > MAX_DEF_IO_ENTRIES {
        return Err(invalid(format!("{} entries", entries.len())));
    }
    let mut names: Vec<String> = Vec::new();
    for entry in entries {
        let name = entry
            .as_str()
            .ok_or_else(|| invalid("an entry is not a string".to_owned()))?;
        let valid = !name.is_empty()
            && name.len() <= MAX_DEF_ID_BYTES
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
            && !name.starts_with('-');
        if !valid {
            return Err(invalid("an entry is not a name".to_owned()));
        }
        if names.iter().any(|seen| seen == name) {
            return Err(invalid(format!("'{name}' is listed twice")));
        }
        names.push(name.to_owned());
    }
    Ok(names)
}

fn parse_role(raw: &str) -> Option<AgentRole> {
    AgentRole::ALL
        .iter()
        .copied()
        .find(|role| role.as_str().eq_ignore_ascii_case(raw))
}

fn parse_tool_class(raw: &str) -> Option<RoleToolClass> {
    RoleToolClass::ALL
        .iter()
        .copied()
        .find(|class| class.as_str().eq_ignore_ascii_case(raw))
}

#[cfg(test)]
mod tests {
    use super::*;
    use RoleToolClass::{Exec, Read};

    fn full_registry() -> ImplementationRegistry {
        let mut registry = ImplementationRegistry::new();
        for class in RoleToolClass::ALL {
            registry = registry.declare(*class, "rapidlm.impl.test.v1");
        }
        registry
    }

    fn def_toml(id: &str, role: &str, tools: &str) -> String {
        format!(
            "schema = \"{AGENT_DEFS_SCHEMA}\"\n\
             [agent]\n\
             id = \"{id}\"\n\
             description = \"test definition\"\n\
             role = \"{role}\"\n\
             tools = {tools}\n"
        )
    }

    #[test]
    fn parse_accepts_a_valid_definition_and_records_provenance() {
        let source = DefSource::Project(PathBuf::from("/p/.rapidlm/agents/x.toml"));
        let def = AgentDefinition::parse(
            source.clone(),
            &def_toml("auth-reviewer", "security_reviewer", "[\"read\"]"),
        )
        .expect("parse");
        assert_eq!(def.id.as_str(), "auth-reviewer");
        assert_eq!(def.role, AgentRole::SecurityReviewer);
        assert!(def.tool_surface.allows(Read));
        assert!(!def.tool_surface.allows(Exec));
        assert_eq!(def.source, source);
    }

    #[test]
    fn id_alphabet_is_canonical_and_rejections_do_not_echo_the_input() {
        assert!(AgentDefId::parse("valid-id-1").is_ok());
        assert!(AgentDefId::parse("-leading").is_err());
        assert!(AgentDefId::parse("trailing-").is_err());
        assert!(AgentDefId::parse("double--dash").is_err());
        assert!(AgentDefId::parse("Has-Upper").is_err());
        assert!(AgentDefId::parse("").is_err());
        let long = "a".repeat(MAX_DEF_ID_BYTES + 1);
        assert!(matches!(
            AgentDefId::parse(&long),
            Err(AgentDefError::IdTooLong { .. })
        ));
    }

    #[test]
    fn closed_schema_rejects_unknown_fields_and_wrong_schema_marker() {
        let wrong_schema = "schema = \"rapidlm.agent_defs.v0\"\n[agent]\nid = \"x\"\ndescription = \"d\"\nrole = \"explorer\"\n";
        let err = AgentDefinition::parse(DefSource::BuiltIn, wrong_schema).expect_err("schema");
        assert!(err.to_string().contains("unsupported schema"));
        let unknown_top = format!(
            "schema = \"{AGENT_DEFS_SCHEMA}\"\nextra = 1\n\n{}",
            def_toml("x", "explorer", "[]")
                .trim_start_matches(&format!("schema = \"{AGENT_DEFS_SCHEMA}\"\n"))
        );
        let err = AgentDefinition::parse(DefSource::BuiltIn, &unknown_top).expect_err("top field");
        assert!(err.to_string().contains("unknown top-level field 'extra'"));
        let unknown_agent = format!("{}\n", def_toml("x", "explorer", "[]"))
            .replace("tools = []", "tools = []\nstealth = true");
        let err =
            AgentDefinition::parse(DefSource::BuiltIn, &unknown_agent).expect_err("agent field");
        assert!(err.to_string().contains("unknown [agent] field 'stealth'"));
    }

    #[test]
    fn grants_must_stay_inside_the_role_surface() {
        let registry = full_registry();
        // Explorer's registry surface is read-only; exec exceeds it.
        let def = AgentDefinition::parse(
            DefSource::BuiltIn,
            &def_toml("greedy", "explorer", "[\"read\", \"exec\"]"),
        )
        .expect("parse");
        let err = def.validate_grants(&registry).expect_err("exceeds surface");
        assert!(err.to_string().contains("never widen it"));
        assert!(err.to_string().contains("explorer"));
    }

    #[test]
    fn grants_require_a_declared_runtime_implementation() {
        // Declare Read only; Git grants fail closed.
        let registry = ImplementationRegistry::new().declare(Read, "rapidlm.impl.fs.v1");
        let def = AgentDefinition::parse(
            DefSource::BuiltIn,
            &def_toml("needs-git", "coder", "[\"read\", \"git\"]"),
        )
        .expect("parse");
        let err = def
            .validate_grants(&registry)
            .expect_err("missing implementation");
        assert!(
            err.to_string()
                .contains("no declared runtime implementation")
        );
        // With Git declared the same definition validates.
        let registry = registry.declare(RoleToolClass::Git, "rapidlm.impl.git.v1");
        def.validate_grants(&registry).expect("declared");
    }

    #[test]
    fn builtins_are_valid_and_carry_unique_ids() {
        let registry = full_registry();
        let builtins = builtin_definitions();
        assert!(!builtins.is_empty());
        for def in &builtins {
            def.validate_grants(&registry)
                .unwrap_or_else(|err| panic!("builtin {} invalid: {err}", def.id));
        }
        for (i, a) in builtins.iter().enumerate() {
            for b in builtins.iter().skip(i + 1) {
                assert_ne!(a.id, b.id);
            }
        }
    }

    #[test]
    fn scaffold_output_parses_back_into_a_valid_definition() {
        let id = AgentDefId::parse("my-agent").expect("id");
        let text = AgentDefinition::scaffold(&id);
        let def = AgentDefinition::parse(DefSource::Project(PathBuf::from("scaffold")), &text)
            .expect("scaffold parses");
        assert_eq!(def.id.as_str(), "my-agent");
        let registry = full_registry();
        def.validate_grants(&registry).expect("scaffold validates");
    }

    #[test]
    fn load_directory_reports_rejections_and_loads_valid_files() {
        let seq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("rapidlm-agent-defs-{seq}"));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("good.toml"),
            def_toml("good-one", "explorer", "[\"read\"]"),
        )
        .expect("write good");
        std::fs::write(dir.join("bad.toml"), "schema = \"wrong\"\n").expect("write bad");
        // Not a regular file: rejected without following. The real target
        // lives in a subdirectory the scan does not read.
        std::fs::create_dir_all(dir.join("src")).expect("mkdir src");
        std::fs::write(
            dir.join("src/real.toml"),
            def_toml("target", "explorer", "[]"),
        )
        .expect("write target");
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.join("src/real.toml"), dir.join("link.toml"))
            .expect("symlink");
        let registry = full_registry();
        let inventory = load_directory(&dir, &registry).expect("scan");
        assert_eq!(inventory.loaded.len(), 1);
        assert_eq!(inventory.loaded[0].id.as_str(), "good-one");
        // bad.toml + link.toml rejected (symlink skipped only on unix).
        let rejected_reasons: Vec<&str> = inventory
            .rejected
            .iter()
            .map(|r| r.reason.as_str())
            .collect();
        assert!(
            rejected_reasons
                .iter()
                .any(|r| r.contains("unsupported schema"))
        );
        #[cfg(unix)]
        assert!(
            rejected_reasons
                .iter()
                .any(|r| r.contains("not a regular file"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_directory_rejects_a_file_over_the_byte_cap_without_buffering_it_in_full() {
        let seq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("rapidlm-agent-defs-oversized-{seq}"));
        std::fs::create_dir_all(&dir).expect("mkdir");
        // Far larger than MAX_DEF_FILE_BYTES: the bounded read must still
        // reject this cheaply via the post-read length check, not trust a
        // stat taken before the read (the TOCTOU this fix closes).
        std::fs::write(dir.join("huge.toml"), vec![b'#'; MAX_DEF_FILE_BYTES * 4]).expect("write");
        let registry = full_registry();
        let inventory = load_directory(&dir, &registry).expect("scan");
        assert!(inventory.loaded.is_empty());
        assert_eq!(inventory.rejected.len(), 1);
        assert!(inventory.rejected[0].reason.contains("exceeds"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_directory_is_an_empty_inventory_not_an_error() {
        let registry = ImplementationRegistry::new();
        let inventory =
            load_directory(&PathBuf::from("/nonexistent/rapidlm-agents"), &registry).expect("scan");
        assert!(inventory.loaded.is_empty());
        assert!(inventory.rejected.is_empty());
    }

    #[test]
    fn project_defs_colliding_with_builtins_are_a_hard_error() {
        let seq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("rapidlm-agent-defs-collide-{seq}"));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("shadow.toml"),
            def_toml("explore", "explorer", "[\"read\"]"),
        )
        .expect("write");
        let registry = full_registry();
        let err = load_directory(&dir, &registry).expect_err("collision");
        assert!(err.to_string().contains("cannot be overridden"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn full_inventory_lists_builtins_before_project_defs() {
        let seq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("rapidlm-agent-defs-full-{seq}"));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("z.toml"),
            def_toml("z-project", "tester", "[\"read\"]"),
        )
        .expect("write");
        let registry = full_registry();
        let inventory = full_inventory(&dir, &registry).expect("inventory");
        let ids: Vec<&str> = inventory.loaded.iter().map(|d| d.id.as_str()).collect();
        assert!(ids.first().is_some_and(|id| *id == "general-purpose"));
        assert!(ids.contains(&"z-project"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let seq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("rapidlm-agent-defs-{tag}-{seq}"));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn a_definition_names_its_instructions_model_effort_and_io() {
        let text = format!(
            "schema = \"{AGENT_DEFS_SCHEMA}\"\n\
             [agent]\n\
             id = \"reviewer-lite\"\n\
             description = \"reviews a diff\"\n\
             base_role = \"reviewer\"\n\
             tools = [\"read\"]\n\
             instructions = \"Cite file:line for every finding.\"\n\
             model = \"fast-model\"\n\
             reasoning_effort = \"low\"\n\
             inputs = [\"diff\"]\n\
             outputs = [\"findings\", \"verdict\"]\n"
        );
        let def =
            AgentDefinition::parse(DefSource::User(PathBuf::from("r.toml")), &text).expect("parse");
        assert_eq!(def.role, AgentRole::Reviewer);
        assert_eq!(
            def.instructions.as_deref(),
            Some("Cite file:line for every finding.")
        );
        assert_eq!(def.model.as_deref(), Some("fast-model"));
        assert_eq!(def.reasoning_effort.as_deref(), Some("low"));
        assert_eq!(def.inputs, vec!["diff".to_owned()]);
        assert_eq!(
            def.outputs,
            vec!["findings".to_owned(), "verdict".to_owned()]
        );
        assert_eq!(def.source.kind(), "user");
        // `role` still names the base role; both at once do not.
        assert!(
            AgentDefinition::parse(DefSource::BuiltIn, &def_toml("a", "explorer", "[]")).is_ok()
        );
        let both = text.replace(
            "base_role = \"reviewer\"",
            "base_role = \"reviewer\"\nrole = \"coder\"",
        );
        let err = AgentDefinition::parse(DefSource::BuiltIn, &both).expect_err("both");
        assert!(err.to_string().starts_with("field `base_role`:"), "{err}");
    }

    #[test]
    fn a_bad_field_says_which_and_what_would_do() {
        let with = |field: &str, value: &str| {
            format!(
                "{}{field} = {value}\n",
                def_toml("a", "explorer", "[\"read\"]")
            )
        };
        for (field, value) in [
            ("reasoning_effort", "\"extreme\""),
            ("model", "\"has space\""),
            ("instructions", "\"\""),
            ("inputs", "[\"Bad Name\"]"),
            ("outputs", "[\"a\", \"a\"]"),
            ("inputs", "\"not-a-list\""),
        ] {
            let err =
                AgentDefinition::parse(DefSource::BuiltIn, &with(field, value)).expect_err(field);
            let text = err.to_string();
            assert!(text.starts_with(&format!("field `{field}`:")), "{text}");
            assert!(text.contains("; "), "a remedy: {text}");
        }
        let err = AgentDefinition::parse(DefSource::BuiltIn, &with("reasoning_effort", "\"x\""))
            .expect_err("effort");
        assert!(err.to_string().contains("low, medium, high"), "{err}");
    }

    #[test]
    fn a_tool_outside_the_base_role_is_refused_naming_the_field_and_what_the_role_exposes() {
        let def = AgentDefinition::parse(
            DefSource::BuiltIn,
            &def_toml("widen", "explorer", "[\"read\", \"write\"]"),
        )
        .expect("parse");
        let err = def.validate_grants(&full_registry()).expect_err("widens");
        let text = err.to_string();
        assert!(text.starts_with("field `tools`:"), "{text}");
        assert!(text.contains("Remove 'write' from `tools`"), "{text}");
        assert!(
            text.contains("role 'explorer' exposes: read, net, mcp"),
            "{text}"
        );
    }

    #[test]
    fn the_user_directory_loads_below_the_project_and_never_shadows() {
        let project = temp_dir("project");
        let user = temp_dir("user");
        std::fs::write(
            project.join("p.toml"),
            def_toml("shared", "explorer", "[\"read\"]"),
        )
        .expect("write");
        std::fs::write(
            user.join("a.toml"),
            def_toml("mine", "explorer", "[\"read\"]"),
        )
        .expect("write");
        std::fs::write(
            user.join("b.toml"),
            def_toml("shared", "tester", "[\"read\"]"),
        )
        .expect("write");
        std::fs::write(
            user.join("c.toml"),
            def_toml("explore", "explorer", "[\"read\"]"),
        )
        .expect("write");
        let registry = full_registry();
        let inventory =
            layered_inventory(Some(&project), Some(&user), &registry).expect("inventory");
        let ids: Vec<&str> = inventory.loaded.iter().map(|d| d.id.as_str()).collect();
        let at = |id: &str| ids.iter().position(|seen| *seen == id).expect(id);
        assert!(
            at("general-purpose") < at("shared") && at("shared") < at("mine"),
            "{ids:?}"
        );
        let shared = inventory
            .loaded
            .iter()
            .find(|d| d.id.as_str() == "shared")
            .expect("shared");
        assert_eq!(shared.source.kind(), "project", "the project's wins");
        assert_eq!(inventory.rejected.len(), 2, "{:?}", inventory.rejected);
        assert!(
            inventory
                .rejected
                .iter()
                .any(|r| r.reason.contains("already the project's"))
        );
        assert!(
            inventory
                .rejected
                .iter()
                .any(|r| r.reason.contains("built-in"))
        );
        // An untrusted project's files are not read; the user's still are.
        let untrusted = layered_inventory(None, Some(&user), &registry).expect("inventory");
        let ids: Vec<&str> = untrusted.loaded.iter().map(|d| d.id.as_str()).collect();
        assert!(ids.contains(&"mine") && ids.contains(&"shared"), "{ids:?}");
        assert_eq!(
            untrusted
                .loaded
                .iter()
                .find(|d| d.id.as_str() == "shared")
                .map(|d| d.source.kind()),
            Some("user")
        );
        let _ = std::fs::remove_dir_all(&project);
        let _ = std::fs::remove_dir_all(&user);
    }

    #[test]
    fn every_spawn_type_is_a_builtin_and_an_unknown_one_lists_the_known() {
        let inventory = layered_inventory(None, None, &full_registry()).expect("inventory");
        for id in ["general-purpose", "explore", "plan"] {
            assert!(resolve(&inventory, id).is_ok(), "{id}");
        }
        let err = resolve(&inventory, "wizard").expect_err("unknown");
        assert_eq!(err.requested, "wizard");
        let text = err.to_string();
        assert!(
            text.contains("general-purpose, plan, explore, patch"),
            "{text}"
        );
    }
}
