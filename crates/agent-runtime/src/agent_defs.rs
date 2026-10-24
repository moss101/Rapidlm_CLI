//! Agent definitions: a validated artifact declaring a named agent type —
//! compiled-in built-ins, the project's `<project>/.rapidlm/agents/`, the
//! user's `<home>/agents/` and any `[agents] extra_dirs`, in that order of
//! precedence. A definition is a TOML file (`<id>.toml`) or a Markdown file
//! (`<id>.md`: front matter, then the instructions as the body); both are
//! turned into one table and run through one validator, so they cannot
//! disagree.
//!
//! A definition shapes the model-visible surface of a base role: it narrows
//! the role's tool surface, carries a description for delegation prompts,
//! and may name the child's instructions, model, reasoning effort and
//! declared inputs and outputs (ADR 0023 §5). It never grants authority — the registry surface, policy, and the capability
//! broker still enforce everything at request time. Validation rules are
//! fail-closed:
//!
//! * closed schema — unknown fields and unknown schema versions are rejected;
//! * sources must be regular files and the directory a real directory —
//!   symlinked definitions and a symlinked `agents/` are refused so a project
//!   cannot make the loader follow a path outside the tree;
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
/// The format with the narrowing keys (`allow_tools`, `deny_tools`,
/// `permission`, `max_steps`). A v1 file keeps parsing, and keeps its closed key
/// set: the new keys need this marker.
pub const AGENT_DEFS_SCHEMA_V2: &str = "rapidlm.agent_defs.v2";
/// The most tool patterns in `allow_tools` or `deny_tools`.
pub const MAX_DEF_TOOL_PATTERNS: usize = 64;
/// The most bytes of one tool pattern.
pub const MAX_DEF_TOOL_PATTERN_BYTES: usize = 128;
/// The most `permission` rules.
pub const MAX_DEF_RULES: usize = 32;
/// The most bytes of one `permission` rule.
pub const MAX_DEF_RULE_BYTES: usize = 256;
/// The most model steps a definition may allow: the turn's own ceiling.
pub const MAX_DEF_STEPS: u32 = 32;
/// The most bytes of `when_to_use`.
pub const MAX_DEF_WHEN_TO_USE_BYTES: usize = 256;
/// The most agent ids in `spawn`.
pub const MAX_DEF_SPAWN_IDS: usize = 32;

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
/// User directory scanned for definitions, relative to the RapidLM home
/// (`RAPIDLM_HOME`, else `~/.rapidlm`).
pub const USER_DEFS_DIR: &str = "agents";
/// How many `[agents] extra_dirs` are read.
pub const MAX_EXTRA_DEF_DIRS: usize = 8;
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
/// Every front matter key the Markdown form accepts. `name` is the id (the file
/// stem when left out); the body is the instructions, so there is no key for
/// them, and the tool classes are `tool_classes` as in the table form's
/// `tools`.
pub const DEF_MARKDOWN_FIELDS: [&str; 15] = [
    "name",
    "description",
    "base_role",
    "tool_classes",
    "model",
    "reasoning_effort",
    "inputs",
    "outputs",
    "allow_tools",
    "deny_tools",
    "permission",
    "max_steps",
    "mode",
    "when_to_use",
    "spawn",
];
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
/// Every `[agent]` field the v2 schema accepts: the v1 ones and the narrowing
/// keys.
pub const DEF_AGENT_FIELDS_V2: [&str; 17] = [
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
    "allow_tools",
    "deny_tools",
    "permission",
    "max_steps",
    "mode",
    "when_to_use",
    "spawn",
];

/// Canonical definition id: lowercase letters, digits, single interior
/// dashes. Display never echoes a rejected raw id beyond its length.
#[derive(Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct AgentDefId(String);

/// Which syntax a definition file is written in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DefFormat {
    Toml,
    Markdown,
}

impl DefFormat {
    /// `toml` or `md`: the file extension.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Toml => "toml",
            Self::Markdown => "md",
        }
    }

    /// The format a `--format` flag names.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "toml" => Some(Self::Toml),
            "md" | "markdown" => Some(Self::Markdown),
            _ => None,
        }
    }

    /// The format a file's extension says (`.md`, `.toml`), if either.
    pub fn of_path(path: &Path) -> Option<Self> {
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("toml") => Some(Self::Toml),
            Some("md") => Some(Self::Markdown),
            _ => None,
        }
    }

    /// The key that names the tool classes in this syntax.
    pub fn tools_key(self) -> &'static str {
        match self {
            Self::Toml => "tools",
            Self::Markdown => "tool_classes",
        }
    }

    /// What the instructions are called in this syntax.
    fn instructions_name(self) -> &'static str {
        match self {
            Self::Toml => "instructions",
            Self::Markdown => "body",
        }
    }

    /// What a type error calls the place the fields are in.
    fn place(self) -> &'static str {
        match self {
            Self::Toml => "[agent]",
            Self::Markdown => "front matter",
        }
    }
}

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
        field: &'static str,
        name: String,
    },
    /// `name` in the front matter is not the file's stem.
    NameMismatch {
        name: String,
        stem: String,
    },
    /// `<id>.md` and `<id>.toml` both exist in one directory.
    FormatCollision {
        first: PathBuf,
        second: PathBuf,
    },
    /// The definitions directory is a symlink.
    DirectorySymlink {
        path: PathBuf,
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
        field: &'static str,
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
                    "field `base_role`: unknown role '{}'; use one of: {}",
                    shown(name),
                    roles.join(", ")
                )
            }
            Self::UnknownToolClass { field, name } => {
                let classes: Vec<&str> = RoleToolClass::ALL
                    .iter()
                    .map(|class| class.as_str())
                    .collect();
                write!(
                    f,
                    "field `{field}`: unknown tool class '{}'; use: {}",
                    shown(name),
                    classes.join(", ")
                )
            }
            Self::NameMismatch { name, stem } => write!(
                f,
                "field `name`: '{}' is not the file name '{}'; leave `name` out, or make it the \
file's name without `.md`",
                shown(name),
                shown(stem)
            ),
            Self::FormatCollision { first, second } => write!(
                f,
                "{} and {} are both there; a definition is one file, so both are refused \
until one of them is removed",
                first.display(),
                second.display()
            ),
            Self::DirectorySymlink { path } => write!(
                f,
                "agent definitions directory {} is a symlink (symlinks are not followed)",
                path.display()
            ),
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
            Self::GrantExceedsRoleSurface {
                field,
                id,
                role,
                class,
            } => {
                let exposed: Vec<&str> = RoleRegistry::profile(*role)
                    .tool_surface()
                    .keys()
                    .map(RoleToolClass::as_str)
                    .collect();
                write!(
                    f,
                    "field `{field}`: definition '{id}' asks for tool class '{}', which base role \
'{}' does not expose — definitions narrow a role, never widen it. Remove '{}' from `{field}` \
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
    /// Loaded from the user's own directory (`<home>/agents`): below the
    /// project's definitions, never over a built-in.
    User(PathBuf),
    /// Loaded from a directory the user's `[agents] extra_dirs` names: below
    /// everything else.
    Extra(PathBuf),
}

impl DefSource {
    /// The file it came from; `None` for a built-in.
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::BuiltIn => None,
            Self::Project(path) | Self::User(path) | Self::Extra(path) => Some(path),
        }
    }

    /// `builtin`, `project`, `user` or `extra`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::BuiltIn => "builtin",
            Self::Project(_) => "project",
            Self::User(_) => "user",
            Self::Extra(_) => "extra",
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
    /// Tool-name patterns (`*` any run, `?` one character) the child's tools
    /// are held to, after the base role and `tool_surface`: `None` adds no
    /// restriction, `Some([])` leaves it no tool.
    pub allow_tools: Option<Vec<String>>,
    /// Tool-name patterns taken away last.
    pub deny_tools: Vec<String>,
    /// Permission rules for the child's calls, by rule pattern (`Tool` or
    /// `Tool(arg-glob)`), in pattern order. Only `deny` and `ask`: a
    /// definition can restrict, never pre-approve.
    pub permission: Vec<(String, AgentRuleEffect)>,
    /// The most model steps the child takes (1 to [`MAX_DEF_STEPS`]).
    pub max_steps: Option<u32>,
    /// Where the agent may be used: offered to `task_spawn` (`subagent`, the
    /// default), selectable as a session's main agent only (`primary`), or
    /// both.
    pub mode: AgentMode,
    /// When to reach for it, appended to its description in the list the model
    /// chooses a `task_spawn` type from.
    pub when_to_use: Option<String>,
    /// The agent ids a main agent of this definition may delegate to; empty is
    /// no restriction beyond the spawn path's own. Only an agent that can be a
    /// main agent has any.
    pub spawn: Vec<String>,
    pub source: DefSource,
}

/// Where an agent definition may be used.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AgentMode {
    /// Offered to `task_spawn` as a type; not a main agent.
    #[default]
    Subagent,
    /// A session's main agent only; not offered to `task_spawn`.
    Primary,
    Both,
}

impl AgentMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Subagent => "subagent",
            Self::Primary => "primary",
            Self::Both => "both",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "subagent" => Some(Self::Subagent),
            "primary" => Some(Self::Primary),
            "both" => Some(Self::Both),
            _ => None,
        }
    }

    /// Whether `task_spawn` may name it.
    pub fn is_spawnable(self) -> bool {
        !matches!(self, Self::Primary)
    }

    /// Whether it may be a session's main agent.
    pub fn is_selectable(self) -> bool {
        !matches!(self, Self::Subagent)
    }
}

/// What a `permission` rule of a definition does: it can only refuse or ask.
#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum AgentRuleEffect {
    Deny,
    Ask,
}

impl AgentRuleEffect {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deny => "deny",
            Self::Ask => "ask",
        }
    }
}

/// Runtime implementation declarations: which tool classes this host can
/// actually back. Hosts declare what they provide; definitions requesting an
/// undeclared class are rejected instead of silently dead.
#[derive(Clone, Debug, Default)]
pub struct ImplementationRegistry {
    declared: Vec<(RoleToolClass, &'static str)>,
    /// Whether a `permission` key is a rule the host can read (`Tool` or
    /// `Tool(glob)`); the grammar is the host's, not this crate's.
    rule_grammar: Option<fn(&str) -> bool>,
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

    /// Declare how a `permission` key is checked. Without one, a key is only
    /// held to the bounds every key has.
    #[must_use]
    pub fn with_rule_grammar(mut self, check: fn(&str) -> bool) -> Self {
        self.rule_grammar = Some(check);
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
        let fields: &[&str] = match schema {
            AGENT_DEFS_SCHEMA => &DEF_AGENT_FIELDS,
            AGENT_DEFS_SCHEMA_V2 => &DEF_AGENT_FIELDS_V2,
            _ => {
                return Err(AgentDefError::Parse {
                    path: path.clone(),
                    reason: format!("unsupported schema '{}'", shown(schema)),
                });
            }
        };
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
            if !fields.contains(&key.as_str()) {
                let hint =
                    if schema == AGENT_DEFS_SCHEMA && DEF_AGENT_FIELDS_V2.contains(&key.as_str()) {
                        format!("; it needs schema = \"{AGENT_DEFS_SCHEMA_V2}\"")
                    } else {
                        String::new()
                    };
                return Err(AgentDefError::Parse {
                    path: path.clone(),
                    reason: format!(
                        "unknown [agent] field '{}' (fields: {}){hint}",
                        shown(key),
                        fields.join(", ")
                    ),
                });
            }
        }
        Self::from_agent_table(path, source, agent, DefFormat::Toml)
    }

    /// Parse a Markdown definition: front matter between `---` lines, then the
    /// instructions as the body. The front matter is turned into the table
    /// the TOML form has and read by the same code, so the two syntaxes
    /// cannot accept different things.
    pub fn parse_markdown(source: DefSource, text: &str) -> Result<Self, AgentDefError> {
        let path = source
            .path()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("<builtin>"));
        // The name a file has is the id it is: one that cannot be read as text
        // cannot be compared with a `name`, so it is not a definition.
        let stem = match source.path() {
            None => None,
            Some(file) => match file.file_stem().and_then(|stem| stem.to_str()) {
                Some(stem) => Some(stem),
                None => {
                    return Err(AgentDefError::Parse {
                        path,
                        reason: "the file name is not valid UTF-8".to_owned(),
                    });
                }
            },
        };
        let agent = markdown_table(&path, stem, text)?;
        Self::from_agent_table(path, source, &agent, DefFormat::Markdown)
    }

    /// Parse `text` in the syntax `source`'s file name says: `.md` is
    /// Markdown, anything else TOML.
    pub fn parse_file(source: DefSource, text: &str) -> Result<Self, AgentDefError> {
        match source.path().and_then(DefFormat::of_path) {
            Some(DefFormat::Markdown) => Self::parse_markdown(source, text),
            _ => Self::parse(source, text),
        }
    }

    /// The syntax this definition was written in; `None` for a built-in.
    pub fn format(&self) -> Option<DefFormat> {
        let path = self.source.path()?;
        Some(DefFormat::of_path(path).unwrap_or(DefFormat::Toml))
    }

    /// The one validator: the `[agent]` table of either syntax.
    fn from_agent_table(
        path: PathBuf,
        source: DefSource,
        agent: &toml::map::Map<String, toml::Value>,
        format: DefFormat,
    ) -> Result<Self, AgentDefError> {
        let place = format.place();
        let id_raw = agent
            .get("id")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| AgentDefError::Parse {
                path: path.clone(),
                reason: format!("{place} id must be a string"),
            })?;
        let id = AgentDefId::parse(id_raw)?;
        let description = agent
            .get("description")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| AgentDefError::Parse {
                path: path.clone(),
                reason: format!("{place} description must be a string"),
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
                    reason: format!("{place} base_role must be a string"),
                })?
            }
            (None, None) => {
                return Err(AgentDefError::Parse {
                    path: path.clone(),
                    reason: format!("{place} base_role must be a string"),
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
                    reason: format!("{place} tools must be an array of strings"),
                })?;
                for name in names {
                    let name = name.as_str().ok_or_else(|| AgentDefError::Parse {
                        path: path.clone(),
                        reason: format!("{place} tools entries must be strings"),
                    })?;
                    let class =
                        parse_tool_class(name).ok_or_else(|| AgentDefError::UnknownToolClass {
                            field: format.tools_key(),
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
                field: format.instructions_name(),
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
        let allow_tools = tool_patterns(agent, "allow_tools")?;
        let deny_tools = tool_patterns(agent, "deny_tools")?.unwrap_or_default();
        let permission = permission_rules(agent)?;
        let max_steps = step_cap(agent)?;
        let mode = match optional_text(agent, "mode")? {
            None => AgentMode::default(),
            Some(raw) => AgentMode::parse(&raw).ok_or_else(|| AgentDefError::FieldInvalid {
                field: "mode",
                reason: format!("'{}' is not a mode", shown(&raw)),
                remedy: "use subagent, primary or both".to_owned(),
            })?,
        };
        let when_to_use = optional_text(agent, "when_to_use")?;
        if let Some(text) = &when_to_use
            && (text.trim().is_empty()
                || text.len() > MAX_DEF_WHEN_TO_USE_BYTES
                || text
                    .chars()
                    .any(|c| c.is_control() || protocol::text::changes_the_line(c)))
        {
            return Err(AgentDefError::FieldInvalid {
                field: "when_to_use",
                reason: format!("{} bytes", text.len()),
                remedy: format!(
                    "give one visible line of 1-{MAX_DEF_WHEN_TO_USE_BYTES} bytes, with no \
control, line-splitting or invisible characters"
                ),
            });
        }
        let spawn = spawn_ids(agent)?;
        if !spawn.is_empty() && !mode.is_selectable() {
            return Err(AgentDefError::FieldInvalid {
                field: "spawn",
                reason: "nested spawning is disabled: a subagent cannot delegate".to_owned(),
                remedy: "set `mode` to primary or both for an agent that delegates, or remove \
`spawn`"
                    .to_owned(),
            });
        }
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
            allow_tools,
            deny_tools,
            permission,
            max_steps,
            mode,
            when_to_use,
            spawn,
            source,
        })
    }

    /// A fingerprint of everything the definition says, whichever syntax it
    /// was written in and wherever it was found: SHA-256 (hex) of a canonical
    /// rendering. Two definitions with the same digest ask for the same thing.
    pub fn digest(&self) -> String {
        use sha2::Digest;
        let canonical = serde_json::json!({
            "schema": "rapidlm.agent_defs.digest/v1",
            "id": self.id.as_str(),
            "description": self.description,
            "base_role": self.role.as_str(),
            "tool_classes": self.tool_surface.keys().map(RoleToolClass::as_str).collect::<Vec<_>>(),
            "instructions": self.instructions,
            "model": self.model,
            "reasoning_effort": self.reasoning_effort,
            "inputs": self.inputs,
            "outputs": self.outputs,
            "allow_tools": self.allow_tools.as_deref().map(sorted_set),
            "deny_tools": sorted_set(&self.deny_tools),
            "permission": self.permission.iter().map(|(k, e)| [k.as_str(), e.as_str()]).collect::<Vec<_>>(),
            "max_steps": self.max_steps,
            "mode": self.mode.as_str(),
            "when_to_use": self.when_to_use,
            "spawn": sorted_set(&self.spawn),
        });
        sha2::Sha256::digest(canonical.to_string().as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// Validate grants against the role registry surface and the runtime
    /// implementation declarations.
    pub fn validate_grants(&self, impls: &ImplementationRegistry) -> Result<(), AgentDefError> {
        if let Some(check) = impls.rule_grammar
            && let Some((key, _)) = self.permission.iter().find(|(key, _)| !check(key))
        {
            return Err(AgentDefError::FieldInvalid {
                field: "permission",
                reason: format!("'{}' is not a permission rule", shown(key)),
                remedy: "write `Tool` or `Tool(glob)` with an exact tool name, as a rule in \
the project settings"
                    .to_owned(),
            });
        }
        let role_surface = RoleRegistry::profile(self.role).tool_surface();
        for class in self.tool_surface.keys() {
            if !role_surface.allows(class) {
                return Err(AgentDefError::GrantExceedsRoleSurface {
                    field: self.format().map_or("tools", DefFormat::tools_key),
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

    /// A valid starter definition for `id` in `format`, ready to be written to
    /// `<project>/.rapidlm/agents/<id>.<format>`.
    pub fn scaffold_as(id: &AgentDefId, format: DefFormat) -> String {
        match format {
            DefFormat::Toml => Self::scaffold(id),
            DefFormat::Markdown => Self::scaffold_markdown(id),
        }
    }

    /// [`Self::scaffold`] in the Markdown form.
    pub fn scaffold_markdown(id: &AgentDefId) -> String {
        format!(
            "---\n\
             name: {id}\n\
             description: what this agent is for; shown in delegation prompts\n\
             base_role: explorer\n\
             # Optional. Subset of the role surface; classes without a declared\n\
             # runtime implementation are rejected at load.\n\
             tool_classes: read\n\
             # Optional: a configured [model.<id>] and a reasoning effort.\n\
             # model: fast\n\
             # reasoning_effort: low\n\
             # Optional: names the task supplies, and names the result carries.\n\
             # inputs: question\n\
             # outputs: findings\n\
             # Optional narrowing: tool-name patterns (`*` any run, `?` one character)\n\
             # kept and then taken away, rules that can deny or ask but never allow,\n\
             # and the most model steps (1 to 32).\n\
             # allow_tools: repo_* workspace_read\n\
             # deny_tools: repo_glob\n\
             # permission:\n\
             #   \"workspace_patch(docs/*)\": ask\n\
             # max_steps: 12\n\
             ---\n\
             Standing instructions, added after the base role's own and ahead of each\n\
             task. Report file paths with line numbers.\n"
        )
    }

    /// A valid starter definition for `id`, ready to be written to
    /// `<project>/.rapidlm/agents/<id>.toml`.
    pub fn scaffold(id: &AgentDefId) -> String {
        format!(
            "# Agent definition ({AGENT_DEFS_SCHEMA_V2}).\n\
             # Definitions narrow a role's visible tool surface; they never\n\
             # widen it. See RoleRegistry for each role's surface.\n\
             schema = \"{AGENT_DEFS_SCHEMA_V2}\"\n\
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
             # outputs = [\"findings\"]\n\
             # Optional narrowing: tool-name patterns (`*` any run, `?` one character)\n\
             # kept and then taken away, the most model steps (1 to 32), and rules that\n\
             # can deny or ask but never allow (a table: [agent.permission]).\n\
             # allow_tools = [\"repo_*\", \"workspace_read\"]\n\
             # deny_tools = [\"repo_glob\"]\n\
             # max_steps = 12\n"
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
            allow_tools: None,
            deny_tools: Vec::new(),
            permission: Vec::new(),
            max_steps: None,
            mode: AgentMode::Subagent,
            when_to_use: None,
            spawn: Vec::new(),
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

/// Load every `*.toml` and `*.md` definition from `dir` (default
/// `<project>/.rapidlm/agents`). A missing directory is an empty inventory —
/// projects without custom agents are normal, not an error.
pub fn load_directory(
    dir: &Path,
    impls: &ImplementationRegistry,
) -> Result<DefInventory, AgentDefError> {
    load_directory_from(dir, impls, DefOrigin::Project)
}

/// Which directory a scan reads: the project's, the user's, or one of the
/// user's `extra_dirs`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DefOrigin {
    Project,
    User,
    Extra,
}

/// [`load_directory`] for any origin. A project file that collides with a
/// built-in fails the whole scan, as it always has; a user or extra file that
/// does is rejected on its own, so one stray file in a home directory does
/// not hide every other.
pub fn load_directory_from(
    dir: &Path,
    impls: &ImplementationRegistry,
    origin: DefOrigin,
) -> Result<DefInventory, AgentDefError> {
    let mut inventory = DefInventory::default();
    // A symlinked directory is not followed: it would let a project point the
    // loader at files outside its tree, as a symlinked file would.
    match std::fs::symlink_metadata(dir) {
        Ok(meta) if meta.file_type().is_symlink() => {
            inventory.rejected.push(RejectedDef {
                path: dir.to_path_buf(),
                reason: AgentDefError::DirectorySymlink {
                    path: dir.to_path_buf(),
                }
                .to_string(),
            });
            return Ok(inventory);
        }
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(inventory);
        }
        Err(err) => return Err(err.into()),
    }
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
        if DefFormat::of_path(&path).is_some() {
            paths.push(path);
        }
    }
    paths.sort();
    if paths.len() > MAX_DEFS_PER_DIRECTORY {
        return Err(AgentDefError::TooManyDefinitions {
            limit: MAX_DEFS_PER_DIRECTORY,
        });
    }
    // `<id>.md` and `<id>.toml` side by side: which one is meant is not for
    // the loader to guess, so neither is read. Grouped by name, not by who is
    // next in the sort (`x.old.toml` sorts between `x.md` and `x.toml`).
    let mut by_stem: std::collections::BTreeMap<&std::ffi::OsStr, [Option<&PathBuf>; 2]> =
        std::collections::BTreeMap::new();
    for path in &paths {
        let (Some(stem), Some(format)) = (path.file_stem(), DefFormat::of_path(path)) else {
            continue;
        };
        by_stem.entry(stem).or_default()[usize::from(format == DefFormat::Markdown)] = Some(path);
    }
    let mut doubled: Vec<PathBuf> = Vec::new();
    for [toml, markdown] in by_stem.values() {
        let (Some(toml), Some(markdown)) = (toml, markdown) else {
            continue;
        };
        for path in [markdown, toml] {
            inventory.rejected.push(RejectedDef {
                path: (*path).clone(),
                reason: AgentDefError::FormatCollision {
                    first: (*markdown).clone(),
                    second: (*toml).clone(),
                }
                .to_string(),
            });
        }
        doubled.push((*markdown).clone());
        doubled.push((*toml).clone());
    }
    paths.retain(|path| !doubled.contains(path));
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
        let mut bytes = Vec::new();
        std::fs::File::open(&path)?
            .take(MAX_DEF_FILE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
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
        let Ok(text) = String::from_utf8(bytes) else {
            inventory.rejected.push(RejectedDef {
                path: path.clone(),
                reason: AgentDefError::Parse {
                    path: path.clone(),
                    reason: "not valid UTF-8".to_owned(),
                }
                .to_string(),
            });
            continue;
        };
        let source = match origin {
            DefOrigin::Project => DefSource::Project(path.clone()),
            DefOrigin::User => DefSource::User(path.clone()),
            DefOrigin::Extra => DefSource::Extra(path.clone()),
        };
        let def = match AgentDefinition::parse_file(source, &text) {
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
            if origin != DefOrigin::Project {
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

/// Whether `dir`, with every link on the way followed, lies inside `root` (also
/// followed). A directory that is not there is inside: nothing is read from it.
/// A trusted project that commits `.rapidlm` as a link to somewhere else would
/// otherwise have the loader read there.
#[must_use]
pub fn resolves_inside(root: &Path, dir: &Path) -> bool {
    let Ok(real) = std::fs::canonicalize(dir) else {
        return true;
    };
    std::fs::canonicalize(root).is_ok_and(|base| real.starts_with(base))
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
    layered_inventory_with(project_dir, user_dir, &[], impls)
}

/// [`layered_inventory`] with the user's `extra_dirs` below everything else,
/// in the order written (at most [`MAX_EXTRA_DEF_DIRS`]). Each loses to what
/// ranks above it the way a user definition does.
pub fn layered_inventory_with(
    project_dir: Option<&Path>,
    user_dir: Option<&Path>,
    extra_dirs: &[PathBuf],
    impls: &ImplementationRegistry,
) -> Result<DefInventory, AgentDefError> {
    let mut inventory = match project_dir {
        Some(dir) => load_directory(dir, impls)?,
        None => DefInventory::default(),
    };
    let mut loaded = builtin_definitions();
    loaded.append(&mut inventory.loaded);
    let below = user_dir
        .map(|dir| (dir, DefOrigin::User))
        .into_iter()
        .chain(
            extra_dirs
                .iter()
                .take(MAX_EXTRA_DEF_DIRS)
                .map(|dir| (dir.as_path(), DefOrigin::Extra)),
        );
    for (dir, origin) in below {
        match load_directory_from(dir, impls, origin) {
            Ok(scanned) => {
                inventory.rejected.extend(scanned.rejected);
                for def in scanned.loaded {
                    if let Some(winner) = loaded.iter().find(|held| held.id == def.id) {
                        inventory.rejected.push(RejectedDef {
                            path: def.source.path().map(Path::to_path_buf).unwrap_or_default(),
                            reason: format!(
                                "definition id '{}' is already the {}'s; a {} definition \
never shadows it",
                                def.id,
                                match winner.source {
                                    DefSource::BuiltIn => "built-in",
                                    DefSource::Project(_) => "project",
                                    DefSource::User(_) => "user",
                                    DefSource::Extra(_) => "extra-directory",
                                },
                                match origin {
                                    DefOrigin::Extra => "extra-directory",
                                    _ => "user",
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
            if self.known.is_empty() {
                "none".to_owned()
            } else {
                self.known.join(", ")
            }
        )
    }
}

impl std::error::Error for UnknownAgentType {}

/// Text from a file as an error shows it: no more than an id's length, a
/// control character as `?`.
fn shown(text: &str) -> String {
    let mut out: String = text
        .chars()
        .take(MAX_DEF_ID_BYTES)
        .map(|ch| if ch.is_control() { '?' } else { ch })
        .collect();
    if text.chars().count() > MAX_DEF_ID_BYTES {
        out.push('…');
    }
    out
}

/// The `[agent]` table a Markdown definition stands for: its front matter,
/// with the file stem for a `name` left out, and the body as `instructions`.
/// Only the shape is settled here; what the values may be is the validator's.
fn markdown_table(
    path: &Path,
    stem: Option<&str>,
    text: &str,
) -> Result<toml::map::Map<String, toml::Value>, AgentDefError> {
    use protocol::frontmatter::{Rules, Stop, Syntax, Value, decode, split};
    let invalid = |reason: String| AgentDefError::Parse {
        path: path.to_path_buf(),
        reason,
    };
    if text.contains('\0') {
        return Err(invalid("contains a NUL byte".to_owned()));
    }
    // The opening fence is a line of its own, as the closing one is (the shared
    // reader, which the skills loader also uses, is lenient about it).
    let opening = text.strip_prefix('\u{feff}').unwrap_or(text);
    let fenced = opening.starts_with("---\n") || opening.starts_with("---\r\n");
    let Some((front, body)) = split(text).filter(|_| fenced) else {
        return Err(invalid(
            "no front matter: begin the file with a `---` line and close the block with another"
                .to_owned(),
        ));
    };
    let line = std::cell::Cell::new(0usize);
    let fields = decode(
        front,
        Rules {
            max_scalar_bytes: MAX_DEF_FILE_BYTES,
            // `permission` is a map of rules to what to do with them.
            nested_maps: true,
        },
        &mut |index| {
            line.set(index);
            Ok(())
        },
        &mut |key| {
            if DEF_MARKDOWN_FIELDS.contains(&key) {
                return Ok(());
            }
            let hint = match key {
                "instructions" => "; the body of the file is the instructions",
                "id" => "; the id is `name`",
                "role" => "; the role is `base_role`",
                "tools" => "; the tool classes are `tool_classes`",
                _ => "",
            };
            Err(invalid(format!(
                "unknown front matter key '{}' (keys: {}){hint}",
                shown(key),
                DEF_MARKDOWN_FIELDS.join(", ")
            )))
        },
    )
    .map_err(|stop| match stop {
        Stop::Caller(err) => err,
        // Line 1 is the opening fence.
        Stop::Syntax(Syntax::Invalid) => invalid(format!(
            "front matter line {} is outside the supported subset (`key: value`, a block \
list of `  - item` lines, `key: []`, `#` comments; a repeated key is not allowed)",
            line.get() + 2
        )),
        Stop::Syntax(Syntax::ScalarTooLarge) => {
            invalid(format!("front matter line {} is too long", line.get() + 2))
        }
    })?;

    let mut table = toml::map::Map::new();
    let single = |field: &'static str| -> Result<Option<String>, AgentDefError> {
        match fields.get(field) {
            None => Ok(None),
            Some(Value::Scalar(text)) => Ok(Some(text.clone())),
            Some(_) => Err(AgentDefError::FieldInvalid {
                field,
                reason: "not a single value".to_owned(),
                remedy: format!("write it on one line as `{field}: value`"),
            }),
        }
    };
    let several = |field: &'static str| -> Result<Option<Vec<String>>, AgentDefError> {
        match fields.get(field) {
            None => Ok(None),
            // Names are separated by spaces, tabs and line breaks, not by any
            // character Unicode calls white space (a no-break space is a
            // different, refused, name).
            Some(Value::Scalar(text)) => Ok(Some(
                text.split(|c: char| c.is_ascii_whitespace())
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>(),
            )),
            Some(Value::List(items)) => Ok(Some(items.clone())),
            Some(Value::Map(_)) => Err(AgentDefError::FieldInvalid {
                field,
                reason: "not a list of names".to_owned(),
                remedy: format!("write `{field}: one two`, or one `  - name` line each"),
            }),
        }
    };
    let name = single("name")?;
    let id = match (name, stem) {
        (Some(name), Some(stem)) if name != stem => {
            return Err(AgentDefError::NameMismatch {
                name,
                stem: stem.to_owned(),
            });
        }
        (Some(name), _) => name,
        (None, Some(stem)) => stem.to_owned(),
        (None, None) => return Err(invalid("front matter has no `name`".to_owned())),
    };
    table.insert("id".to_owned(), toml::Value::String(id));
    for field in [
        "description",
        "base_role",
        "model",
        "reasoning_effort",
        "mode",
        "when_to_use",
    ] {
        match single(field)? {
            Some(text) => {
                table.insert(field.to_owned(), toml::Value::String(text));
            }
            None if field == "description" || field == "base_role" => {
                return Err(invalid(format!("front matter has no `{field}`")));
            }
            None => {}
        }
    }
    for (key, field) in [
        ("tools", "tool_classes"),
        ("inputs", "inputs"),
        ("outputs", "outputs"),
        ("allow_tools", "allow_tools"),
        ("deny_tools", "deny_tools"),
        ("spawn", "spawn"),
    ] {
        if let Some(items) = several(field)? {
            table.insert(
                key.to_owned(),
                toml::Value::Array(items.into_iter().map(toml::Value::String).collect()),
            );
        }
    }
    // `max_steps` is a number: read as one when it is, so that the validator
    // that reads a table's integer reads this too.
    if let Some(text) = single("max_steps")? {
        table.insert(
            "max_steps".to_owned(),
            text.parse::<i64>()
                .map_or_else(|_| toml::Value::String(text), toml::Value::Integer),
        );
    }
    match fields.get("permission") {
        None => {}
        Some(Value::Map(rules)) => {
            let mut rule_table = toml::map::Map::new();
            for (pattern, effect) in rules {
                rule_table.insert(pattern.clone(), toml::Value::String(effect.clone()));
            }
            table.insert("permission".to_owned(), toml::Value::Table(rule_table));
        }
        // `permission:` and nothing under it: no rules.
        Some(Value::List(items)) if items.is_empty() => {
            table.insert(
                "permission".to_owned(),
                toml::Value::Table(toml::map::Map::new()),
            );
        }
        Some(_) => {
            return Err(AgentDefError::FieldInvalid {
                field: "permission",
                reason: "not a map of rules".to_owned(),
                remedy: "write each rule indented under `permission:`, as `\"Tool(glob)\": deny`"
                    .to_owned(),
            });
        }
    }
    let body = body.replace("\r\n", "\n");
    let body = body.trim();
    if !body.is_empty() {
        table.insert(
            "instructions".to_owned(),
            toml::Value::String(body.to_owned()),
        );
    }
    Ok(table)
}

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

/// `allow_tools` / `deny_tools`: at most [`MAX_DEF_TOOL_PATTERNS`] patterns of
/// at most [`MAX_DEF_TOOL_PATTERN_BYTES`] bytes in the alphabet of tool names
/// and the two wildcards (`*` any run, `?` one character). `None` when the
/// key is absent.
fn tool_patterns(
    agent: &toml::map::Map<String, toml::Value>,
    field: &'static str,
) -> Result<Option<Vec<String>>, AgentDefError> {
    let invalid = |reason: String| AgentDefError::FieldInvalid {
        field,
        reason,
        remedy: format!(
            "give at most {MAX_DEF_TOOL_PATTERNS} patterns of letters, digits, `_`, `-`, `*` \
and `?`, each up to {MAX_DEF_TOOL_PATTERN_BYTES} bytes, naming tools as `rapid tools` prints them"
        ),
    };
    let Some(value) = agent.get(field) else {
        return Ok(None);
    };
    let entries = value
        .as_array()
        .ok_or_else(|| invalid("not an array of patterns".to_owned()))?;
    if entries.len() > MAX_DEF_TOOL_PATTERNS {
        return Err(invalid(format!("{} patterns", entries.len())));
    }
    let mut patterns = Vec::with_capacity(entries.len());
    for entry in entries {
        let pattern = entry
            .as_str()
            .ok_or_else(|| invalid("an entry is not a string".to_owned()))?;
        let valid = !pattern.is_empty()
            && pattern.len() <= MAX_DEF_TOOL_PATTERN_BYTES
            && pattern
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'*' | b'?'));
        if !valid {
            return Err(invalid(format!("'{}' is not a pattern", shown(pattern))));
        }
        patterns.push(pattern.to_owned());
    }
    Ok(Some(patterns))
}

/// `permission`: at most [`MAX_DEF_RULES`] rules, each a key of at most
/// [`MAX_DEF_RULE_BYTES`] bytes with no control character, and `deny` or `ask`
/// for its value — never `allow`. In key order, whatever order they were
/// written in.
fn permission_rules(
    agent: &toml::map::Map<String, toml::Value>,
) -> Result<Vec<(String, AgentRuleEffect)>, AgentDefError> {
    let invalid = |reason: String, remedy: &str| AgentDefError::FieldInvalid {
        field: "permission",
        reason,
        remedy: remedy.to_owned(),
    };
    let Some(value) = agent.get("permission") else {
        return Ok(Vec::new());
    };
    let table = value.as_table().ok_or_else(|| {
        invalid(
            "not a table of rules".to_owned(),
            "map each rule (`Tool` or `Tool(glob)`) to deny or ask",
        )
    })?;
    if table.len() > MAX_DEF_RULES {
        return Err(invalid(
            format!("{} rules", table.len()),
            &format!("give at most {MAX_DEF_RULES}"),
        ));
    }
    let mut rules = Vec::with_capacity(table.len());
    for (key, value) in table {
        if key.is_empty() || key.len() > MAX_DEF_RULE_BYTES || key.chars().any(char::is_control) {
            return Err(invalid(
                format!("'{}' is not a rule", shown(key)),
                &format!("write `Tool` or `Tool(glob)` of up to {MAX_DEF_RULE_BYTES} bytes"),
            ));
        }
        let effect = match value.as_str() {
            Some("deny") => AgentRuleEffect::Deny,
            Some("ask") => AgentRuleEffect::Ask,
            Some("allow") => {
                return Err(invalid(
                    format!("'{}' is `allow`", shown(key)),
                    "an agent file can restrict, never pre-approve: use deny or ask",
                ));
            }
            _ => {
                return Err(invalid(
                    format!("the value of '{}' is not deny or ask", shown(key)),
                    "use deny or ask",
                ));
            }
        };
        rules.push((key.clone(), effect));
    }
    rules.sort();
    Ok(rules)
}

/// A list that is a set (tool patterns, spawn ids), once each and in order:
/// two definitions that name the same patterns in another order ask for the same
/// thing, so they have the same digest.
fn sorted_set(items: &[String]) -> Vec<&str> {
    let mut set: Vec<&str> = items.iter().map(String::as_str).collect();
    set.sort_unstable();
    set.dedup();
    set
}

/// `spawn`: at most [`MAX_DEF_SPAWN_IDS`] distinct agent ids.
fn spawn_ids(agent: &toml::map::Map<String, toml::Value>) -> Result<Vec<String>, AgentDefError> {
    let invalid = |reason: String| AgentDefError::FieldInvalid {
        field: "spawn",
        reason,
        remedy: format!(
            "give at most {MAX_DEF_SPAWN_IDS} distinct agent ids (lowercase letters, digits and \
single interior dashes)"
        ),
    };
    let Some(value) = agent.get("spawn") else {
        return Ok(Vec::new());
    };
    let entries = value
        .as_array()
        .ok_or_else(|| invalid("not an array of agent ids".to_owned()))?;
    if entries.len() > MAX_DEF_SPAWN_IDS {
        return Err(invalid(format!("{} ids", entries.len())));
    }
    let mut ids: Vec<String> = Vec::with_capacity(entries.len());
    for entry in entries {
        let raw = entry
            .as_str()
            .ok_or_else(|| invalid("an entry is not a string".to_owned()))?;
        AgentDefId::parse(raw)
            .map_err(|_| invalid(format!("'{}' is not an agent id", shown(raw))))?;
        if ids.iter().any(|seen| seen == raw) {
            return Err(invalid(format!("'{raw}' is listed twice")));
        }
        ids.push(raw.to_owned());
    }
    Ok(ids)
}

/// `max_steps`: a whole number from 1 to [`MAX_DEF_STEPS`].
fn step_cap(agent: &toml::map::Map<String, toml::Value>) -> Result<Option<u32>, AgentDefError> {
    let Some(value) = agent.get("max_steps") else {
        return Ok(None);
    };
    value
        .as_integer()
        .filter(|steps| (1..=i64::from(MAX_DEF_STEPS)).contains(steps))
        .map(|steps| Some(steps as u32))
        .ok_or_else(|| AgentDefError::FieldInvalid {
            field: "max_steps",
            reason: "not a whole number from 1 to the ceiling".to_owned(),
            remedy: format!("give a number from 1 to {MAX_DEF_STEPS}"),
        })
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

    // --- The Markdown form (EXT-05 slice 1) ---

    fn def_md(name_line: &str, extra: &str, body: &str) -> String {
        format!(
            "---\n{name_line}description: test definition\nbase_role: explorer\n\
             tool_classes: read\n{extra}---\n{body}"
        )
    }

    fn md_source(stem: &str) -> DefSource {
        DefSource::Project(PathBuf::from(format!("/p/.rapidlm/agents/{stem}.md")))
    }

    #[test]
    fn a_markdown_definition_loads_with_its_body_as_the_instructions() {
        let text = "---\n\
            name: reviewer\n\
            description: Read-only code reviewer\n\
            base_role: reviewer\n\
            tool_classes: read git\n\
            model: review\n\
            reasoning_effort: high\n\
            inputs: diff\n\
            outputs:\n  - findings\n  - verdict\n\
            ---\n\
            You review changes for correctness.\n\nReport findings only.\n";
        let def = AgentDefinition::parse_file(md_source("reviewer"), text).expect("parse");
        assert_eq!(def.id.as_str(), "reviewer");
        assert_eq!(def.role, AgentRole::Reviewer);
        assert!(def.tool_surface.allows(Read) && def.tool_surface.allows(RoleToolClass::Git));
        assert!(!def.tool_surface.allows(Exec));
        assert_eq!(def.model.as_deref(), Some("review"));
        assert_eq!(def.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(def.inputs, ["diff"]);
        assert_eq!(def.outputs, ["findings", "verdict"]);
        assert_eq!(
            def.instructions.as_deref(),
            Some("You review changes for correctness.\n\nReport findings only."),
            "the body, without the line breaks around it"
        );
        assert_eq!(def.format(), Some(DefFormat::Markdown));
        // No body, no instructions; and CRLF files read as LF ones do.
        let bare = AgentDefinition::parse_markdown(md_source("x"), &def_md("", "", "\n  \n"))
            .expect("parse");
        assert_eq!(bare.instructions, None);
        let crlf = def_md("", "", "one\ntwo\n").replace('\n', "\r\n");
        let lf = AgentDefinition::parse_markdown(md_source("x"), &def_md("", "", "one\ntwo\n"));
        assert_eq!(
            AgentDefinition::parse_markdown(md_source("x"), &crlf).expect("crlf"),
            lf.expect("lf")
        );
    }

    #[test]
    fn the_name_is_the_file_stem_when_left_out_and_must_equal_it_when_given() {
        let left_out = AgentDefinition::parse_markdown(md_source("stemmed"), &def_md("", "", ""))
            .expect("parse");
        assert_eq!(left_out.id.as_str(), "stemmed");
        let given = AgentDefinition::parse_markdown(
            md_source("stemmed"),
            &def_md("name: stemmed\n", "", ""),
        )
        .expect("parse");
        assert_eq!(given, left_out);
        let err =
            AgentDefinition::parse_markdown(md_source("stemmed"), &def_md("name: other\n", "", ""))
                .expect_err("mismatch");
        assert!(matches!(err, AgentDefError::NameMismatch { .. }), "{err}");
        assert!(err.to_string().contains("field `name`"), "{err}");
        // Without a file name there is nothing to default to.
        let err = AgentDefinition::parse_markdown(DefSource::BuiltIn, &def_md("", "", ""))
            .expect_err("no name");
        assert!(err.to_string().contains("no `name`"), "{err}");
        // A stem that is no id is the id's own error.
        let err = AgentDefinition::parse_markdown(md_source("README"), &def_md("", "", ""))
            .expect_err("not an id");
        assert!(matches!(err, AgentDefError::IdInvalid), "{err}");
    }

    #[test]
    fn the_front_matter_key_set_is_closed_and_the_body_cannot_replace_the_base_prompt() {
        for (key, hint) in [
            ("instructions: replace the prompt\n", "the body of the file"),
            ("tools: read\n", "`tool_classes`"),
            ("id: other\n", "`name`"),
            ("role: coder\n", "`base_role`"),
            ("system_prompt: all new\n", "keys:"),
            ("prompt: all new\n", "keys:"),
            ("extends: none\n", "keys:"),
            ("template: \"{{base}}\"\n", "keys:"),
            ("stealth: true\n", "keys:"),
        ] {
            let err = AgentDefinition::parse_markdown(md_source("x"), &def_md("", key, "body"))
                .expect_err(key);
            let text = err.to_string();
            assert!(text.contains("unknown front matter key"), "{key}: {text}");
            assert!(text.contains(hint), "{key}: {text}");
        }
    }

    #[test]
    fn front_matter_outside_the_subset_is_refused_with_its_line() {
        for (text, line) in [
            (
                "---\ndescription: a\nbase_role: explorer\ntool_classes: [read, git]\n---\n",
                4,
            ),
            (
                "---\ndescription: a\ndescription: b\nbase_role: explorer\n---\n",
                3,
            ),
            ("---\ndescription: 'single'\nbase_role: explorer\n---\n", 2),
            (
                "---\n  description: indented\nbase_role: explorer\n---\n",
                2,
            ),
        ] {
            let err = AgentDefinition::parse_markdown(md_source("x"), text).expect_err(text);
            let shown = err.to_string();
            assert!(
                shown.contains(&format!("front matter line {line} ")),
                "{text:?}: {shown}"
            );
        }
        for text in [
            "description: no fence\n",
            "---\ndescription: never closed\n",
            "",
        ] {
            let err = AgentDefinition::parse_markdown(md_source("x"), text).expect_err(text);
            assert!(err.to_string().contains("no front matter"), "{err}");
        }
        let err = AgentDefinition::parse_markdown(
            md_source("x"),
            "---\ndescription: a\nbase_role: explorer\n---\nnul \0 here\n",
        )
        .expect_err("nul");
        assert!(err.to_string().contains("NUL"), "{err}");
        // Required keys, and single-valued ones given a list.
        let err = AgentDefinition::parse_markdown(md_source("x"), "---\nbase_role: coder\n---\n")
            .expect_err("no description");
        assert!(err.to_string().contains("no `description`"), "{err}");
        let err = AgentDefinition::parse_markdown(
            md_source("x"),
            "---\ndescription:\n  - a\nbase_role: coder\n---\n",
        )
        .expect_err("list");
        assert!(err.to_string().starts_with("field `description`:"), "{err}");
    }

    #[test]
    fn the_opening_fence_is_a_line_of_its_own() {
        let body = "description: d\nbase_role: explorer\n---\nbody\n";
        for bad in [
            format!("---{body}"),
            format!("--- \n{body}"),
            format!("----\n{body}"),
            format!("\n---\n{body}"),
            format!("\u{feff}---{body}"),
        ] {
            let err = AgentDefinition::parse_markdown(md_source("x"), &bad).expect_err(&bad);
            assert!(
                err.to_string().contains("no front matter"),
                "{bad:?}: {err}"
            );
        }
        for good in [
            format!("---\n{body}"),
            format!("---\r\n{body}"),
            format!("\u{feff}---\n{body}"),
        ] {
            AgentDefinition::parse_markdown(md_source("x"), &good).expect(&good);
        }
    }

    #[test]
    fn names_in_a_list_are_split_on_ascii_white_space_only() {
        let with = |classes: &str| {
            AgentDefinition::parse_markdown(
                md_source("x"),
                &format!("---\ndescription: d\nbase_role: coder\ntool_classes: {classes}\n---\n"),
            )
        };
        // A tab can only be written as the escape, in quotes.
        assert!(with("\"read\\tgit\"").is_ok());
        assert!(with("  read   git ").is_ok());
        // A no-break space is part of a name, which is then no class.
        let err = with("read\u{a0}git").expect_err("not a class");
        assert!(
            matches!(err, AgentDefError::UnknownToolClass { .. }),
            "{err}"
        );
    }

    #[test]
    fn a_role_the_file_names_is_echoed_bounded() {
        let err = AgentDefinition::parse_markdown(
            md_source("x"),
            &format!(
                "---\ndescription: d\nbase_role: {}\n---\n",
                "r".repeat(5_000)
            ),
        )
        .expect_err("unknown role");
        assert!(
            err.to_string().len() < 300,
            "{} bytes",
            err.to_string().len()
        );
        assert!(err.to_string().contains("unknown role"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_file_name_that_is_not_text_is_not_a_definition() {
        use std::os::unix::ffi::OsStringExt;
        let name = std::ffi::OsString::from_vec(vec![0xff, 0xfe, b'.', b'm', b'd']);
        let source = DefSource::Project(PathBuf::from(name));
        let err = AgentDefinition::parse_markdown(source, &def_md("name: x\n", "", ""))
            .expect_err("no name to compare");
        assert!(err.to_string().contains("not valid UTF-8"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_reached_through_a_link_out_of_the_project_is_not_inside_it() {
        let outside = temp_dir("outside");
        std::fs::create_dir_all(outside.join("agents")).expect("agents");
        std::fs::write(outside.join("agents/x.md"), def_md("", "", "body")).expect("write");
        let project = temp_dir("project-root");
        // `.rapidlm` itself is the link, so `.rapidlm/agents` is a real directory
        // as far as the last component goes.
        std::os::unix::fs::symlink(&outside, project.join(".rapidlm")).expect("symlink");
        let through = project.join(".rapidlm/agents");
        assert!(!resolves_inside(&project, &through));
        // A real directory in the project, a link that stays in it, and a
        // directory that is not there are inside.
        std::fs::create_dir_all(project.join("real/agents")).expect("real");
        assert!(resolves_inside(&project, &project.join("real/agents")));
        std::os::unix::fs::symlink(project.join("real"), project.join("alias")).expect("alias");
        assert!(resolves_inside(&project, &project.join("alias/agents")));
        assert!(resolves_inside(&project, &project.join("missing/agents")));
        // The root is itself reached through a link.
        let holder = temp_dir("holder-of-link");
        std::os::unix::fs::symlink(&project, holder.join("p")).expect("link to root");
        assert!(resolves_inside(
            &holder.join("p"),
            &project.join("real/agents")
        ));
        for dir in [outside, project, holder] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    fn def_v2(agent_extra: &str, tail: &str) -> String {
        format!(
            "schema = \"{AGENT_DEFS_SCHEMA_V2}\"\n[agent]\nid = \"narrow\"\n\
             description = \"d\"\nbase_role = \"coder\"\ntools = [\"read\", \"exec\"]\n\
             {agent_extra}{tail}"
        )
    }

    #[test]
    fn the_narrowing_keys_are_read_in_both_syntaxes_and_mean_the_same() {
        let toml = def_v2(
            "allow_tools = [\"repo_*\", \"shell_exec\"]\ndeny_tools = [\"repo_glob\"]\nmax_steps = 12\n",
            "[agent.permission]\n\"workspace_patch(docs/*)\" = \"ask\"\n\"shell_exec(rm *)\" = \"deny\"\n",
        );
        let md = "---\ndescription: d\nbase_role: coder\ntool_classes: read exec\n\
                  allow_tools: repo_* shell_exec\ndeny_tools:\n  - repo_glob\nmax_steps: 12\n\
                  permission:\n  \"shell_exec(rm *)\": deny\n  \"workspace_patch(docs/*)\": ask\n---\n";
        let mut a = AgentDefinition::parse(DefSource::BuiltIn, &toml).expect("toml");
        let mut b = AgentDefinition::parse_markdown(md_source("narrow"), md).expect("md");
        a.source = DefSource::BuiltIn;
        b.source = DefSource::BuiltIn;
        assert_eq!(a, b);
        assert_eq!(
            a.allow_tools.as_deref(),
            Some(&["repo_*".to_owned(), "shell_exec".to_owned()][..])
        );
        assert_eq!(a.deny_tools, ["repo_glob"]);
        assert_eq!(a.max_steps, Some(12));
        assert_eq!(
            a.permission,
            [
                ("shell_exec(rm *)".to_owned(), AgentRuleEffect::Deny),
                ("workspace_patch(docs/*)".to_owned(), AgentRuleEffect::Ask),
            ],
            "in pattern order, however they were written"
        );
        // None of them said: no restriction beyond the classes.
        let plain = AgentDefinition::parse(DefSource::BuiltIn, &def_v2("", "")).expect("plain");
        assert_eq!(plain.allow_tools, None);
        assert!(plain.deny_tools.is_empty() && plain.permission.is_empty());
        assert_eq!(plain.max_steps, None);
        // An empty allow list is an allow list: no tool at all.
        let none = AgentDefinition::parse(DefSource::BuiltIn, &def_v2("allow_tools = []\n", ""))
            .expect("empty allow list");
        assert_eq!(none.allow_tools, Some(Vec::new()));
    }

    #[test]
    fn a_v1_table_keeps_its_closed_keys_and_is_told_where_the_new_ones_live() {
        let v1 = format!(
            "{}max_steps = 3\n",
            def_toml("old", "explorer", "[\"read\"]")
        );
        let err = AgentDefinition::parse(DefSource::BuiltIn, &v1).expect_err("v1 + new key");
        let text = err.to_string();
        assert!(
            text.contains("unknown [agent] field 'max_steps'")
                && text.contains("it needs schema = \"rapidlm.agent_defs.v2\""),
            "{text}"
        );
        // A key that is no one's has no such hint, and a v1 file without the
        // new keys is what it was.
        let err = AgentDefinition::parse(
            DefSource::BuiltIn,
            &format!("{}stealth = true\n", def_toml("old", "explorer", "[]")),
        )
        .expect_err("unknown");
        assert!(!err.to_string().contains("needs schema"), "{err}");
        assert!(
            AgentDefinition::parse(DefSource::BuiltIn, &def_toml("old", "explorer", "[]")).is_ok()
        );
        // v3 is nobody's.
        let err = AgentDefinition::parse(
            DefSource::BuiltIn,
            &def_toml("old", "explorer", "[]").replace("agent_defs.v1", "agent_defs.v3"),
        )
        .expect_err("v3");
        assert!(
            err.to_string()
                .contains("unsupported schema 'rapidlm.agent_defs.v3'"),
            "{err}"
        );
    }

    #[test]
    fn every_bad_narrowing_value_is_refused_naming_the_key_and_what_would_do() {
        let tail_rule = |key: &str, value: &str| format!("[agent.permission]\n{key} = {value}\n");
        let cases: Vec<(String, &str, &str)> = vec![
            (
                def_v2("allow_tools = \"repo_read\"\n", ""),
                "allow_tools",
                "not an array",
            ),
            (
                def_v2("allow_tools = [1]\n", ""),
                "allow_tools",
                "not a string",
            ),
            (
                def_v2("deny_tools = [\"has space\"]\n", ""),
                "deny_tools",
                "is not a pattern",
            ),
            (
                def_v2("deny_tools = [\"a.b\"]\n", ""),
                "deny_tools",
                "is not a pattern",
            ),
            (
                def_v2("deny_tools = [\"\"]\n", ""),
                "deny_tools",
                "is not a pattern",
            ),
            (
                def_v2(&format!("allow_tools = [\"{}\"]\n", "x".repeat(129)), ""),
                "allow_tools",
                "is not a pattern",
            ),
            (
                def_v2(
                    &format!(
                        "allow_tools = [{}]\n",
                        (0..=MAX_DEF_TOOL_PATTERNS)
                            .map(|n| format!("\"t{n}\""))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    "",
                ),
                "allow_tools",
                "65 patterns",
            ),
            (
                def_v2("", &tail_rule("\"repo_read\"", "\"allow\"")),
                "permission",
                "can restrict, never pre-approve",
            ),
            (
                def_v2("", &tail_rule("\"repo_read\"", "\"Deny\"")),
                "permission",
                "not deny or ask",
            ),
            (
                def_v2("", &tail_rule("\"repo_read\"", "1")),
                "permission",
                "not deny or ask",
            ),
            (
                def_v2("permission = [\"x\"]\n", ""),
                "permission",
                "not a table",
            ),
            (
                def_v2(
                    "",
                    &tail_rule(&format!("\"{}\"", "r".repeat(257)), "\"deny\""),
                ),
                "permission",
                "is not a rule",
            ),
            (def_v2("max_steps = 0\n", ""), "max_steps", "from 1 to"),
            (def_v2("max_steps = 33\n", ""), "max_steps", "from 1 to"),
            (def_v2("max_steps = -4\n", ""), "max_steps", "from 1 to"),
            (def_v2("max_steps = \"12\"\n", ""), "max_steps", "from 1 to"),
            (def_v2("max_steps = 2.5\n", ""), "max_steps", "from 1 to"),
        ];
        for (text, field, reason) in cases {
            let err = AgentDefinition::parse(DefSource::BuiltIn, &text).expect_err(field);
            let shown = err.to_string();
            assert!(
                shown.starts_with(&format!("field `{field}`:")) && shown.contains(reason),
                "{field}/{reason}: {shown}"
            );
            assert!(shown.contains("; "), "a remedy: {shown}");
        }
        // The boundaries themselves are fine.
        for ok in ["max_steps = 1\n", "max_steps = 32\n"] {
            assert!(
                AgentDefinition::parse(DefSource::BuiltIn, &def_v2(ok, "")).is_ok(),
                "{ok}"
            );
        }
        let many: Vec<String> = (0..MAX_DEF_RULES)
            .map(|n| format!("\"tool_{n}\" = \"ask\"\n"))
            .collect();
        assert!(
            AgentDefinition::parse(
                DefSource::BuiltIn,
                &def_v2("", &format!("[agent.permission]\n{}", many.concat()))
            )
            .is_ok()
        );
        let too_many = format!(
            "[agent.permission]\n{}\"one_more\" = \"ask\"\n",
            many.concat()
        );
        let err =
            AgentDefinition::parse(DefSource::BuiltIn, &def_v2("", &too_many)).expect_err("33");
        assert!(err.to_string().contains("33 rules"), "{err}");
    }

    #[test]
    fn the_same_refusals_come_from_the_markdown_form() {
        let with = |extra: &str| {
            AgentDefinition::parse_markdown(
                md_source("x"),
                &format!("---\ndescription: d\nbase_role: coder\n{extra}---\n"),
            )
        };
        for (extra, field) in [
            ("permission:\n  \"repo_read\": allow\n", "permission"),
            ("permission: ask\n", "permission"),
            ("permission:\n  - repo_read\n", "permission"),
            ("max_steps: 0\n", "max_steps"),
            ("max_steps: 33\n", "max_steps"),
            ("max_steps: twelve\n", "max_steps"),
            ("max_steps:\n  - 3\n", "max_steps"),
            ("deny_tools: has.dot\n", "deny_tools"),
        ] {
            let err = with(extra).expect_err(extra);
            assert!(
                err.to_string().starts_with(&format!("field `{field}`:")),
                "{extra}: {err}"
            );
        }
        // `permission:` with nothing under it is no rules.
        let none = with("permission:\n").expect("empty");
        assert!(none.permission.is_empty());
        // A rule key that is empty or has a control character cannot be written.
        assert!(with("permission:\n  \"\": deny\n").is_err());
    }

    #[test]
    fn the_host_can_hold_a_permission_key_to_its_own_rule_grammar() {
        fn grammar(key: &str) -> bool {
            let tool = key.split('(').next().unwrap_or("");
            !tool.is_empty() && tool.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        }
        let registry = full_registry().with_rule_grammar(grammar);
        let ok = AgentDefinition::parse(
            DefSource::BuiltIn,
            &def_v2("", "[agent.permission]\n\"shell_exec(rm *)\" = \"deny\"\n"),
        )
        .expect("parse");
        ok.validate_grants(&registry).expect("a rule");
        let bad = AgentDefinition::parse(
            DefSource::BuiltIn,
            &def_v2("", "[agent.permission]\n\"Tool With Spaces\" = \"deny\"\n"),
        )
        .expect("parse");
        let err = bad.validate_grants(&registry).expect_err("not a rule");
        assert!(
            err.to_string().starts_with("field `permission`:")
                && err.to_string().contains("is not a permission rule"),
            "{err}"
        );
        // Without a grammar the key is only held to its bounds.
        bad.validate_grants(&full_registry())
            .expect("no grammar declared");
    }

    #[test]
    fn a_definition_in_either_form_is_the_golden_one() {
        // The shared fixtures: one definition in the table form and in the
        // Markdown form, and what both must come to.
        let dir =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../protocol/tests/fixtures/agent_defs");
        let read = |name: &str| {
            std::fs::read_to_string(dir.join(name)).unwrap_or_else(|err| panic!("{name}: {err}"))
        };
        let from_toml = AgentDefinition::parse(
            DefSource::Project(PathBuf::from("agents/tester.toml")),
            &read("v2/tester.toml"),
        )
        .expect("toml fixture");
        let from_md = AgentDefinition::parse_markdown(
            DefSource::Project(PathBuf::from("agents/tester.md")),
            &read("v2/tester.md"),
        )
        .expect("md fixture");
        let expected: serde_json::Value =
            serde_json::from_str(&read("v2/tester.expected.json")).expect("expected");
        let as_json = |def: &AgentDefinition| {
            serde_json::json!({
                "id": def.id.as_str(),
                "description": def.description,
                "base_role": def.role.as_str(),
                "tool_classes": def.tool_surface.keys().map(RoleToolClass::as_str).collect::<Vec<_>>(),
                "instructions": def.instructions,
                "model": def.model,
                "reasoning_effort": def.reasoning_effort,
                "inputs": def.inputs,
                "outputs": def.outputs,
                "allow_tools": def.allow_tools,
                "deny_tools": def.deny_tools,
                "permission": def.permission.iter().map(|(k, e)| serde_json::json!([k, e.as_str()])).collect::<Vec<_>>(),
                "max_steps": def.max_steps,
                "mode": def.mode.as_str(),
                "when_to_use": def.when_to_use,
                "spawn": def.spawn,
                "digest": def.digest(),
            })
        };
        assert_eq!(as_json(&from_toml), expected, "the table form");
        assert_eq!(as_json(&from_md), expected, "the Markdown form");
        // A v1 table of the same agent, without the narrowing, is still a
        // definition.
        let v1 = AgentDefinition::parse(DefSource::BuiltIn, &read("v1/helper.toml"))
            .expect("v1 fixture");
        assert_eq!(v1.id.as_str(), "helper");
        assert!(v1.allow_tools.is_none() && v1.permission.is_empty() && v1.max_steps.is_none());
    }

    #[test]
    fn where_an_agent_may_be_used_is_a_mode_and_only_a_main_agent_delegates() {
        let with = |extra: &str| AgentDefinition::parse(DefSource::BuiltIn, &def_v2(extra, ""));
        // The default is a subagent, which the spawn path may name and which is
        // not a main agent.
        let plain = with("").expect("plain");
        assert_eq!(plain.mode, AgentMode::Subagent);
        assert!(plain.mode.is_spawnable() && !plain.mode.is_selectable());
        for (text, spawnable, selectable) in [
            ("subagent", true, false),
            ("primary", false, true),
            ("both", true, true),
        ] {
            let def = with(&format!("mode = \"{text}\"\n")).expect(text);
            assert_eq!(def.mode.as_str(), text);
            assert_eq!(
                (def.mode.is_spawnable(), def.mode.is_selectable()),
                (spawnable, selectable)
            );
        }
        for bad in ["\"Primary\"", "\"main\"", "\"\"", "3"] {
            let err = with(&format!("mode = {bad}\n")).expect_err(bad);
            assert!(err.to_string().starts_with("field `mode`:"), "{bad}: {err}");
        }
        // `spawn` is for an agent that can be a main agent: a subagent cannot
        // delegate, and the file says so when it loads.
        let err = with("spawn = [\"explore\"]\n").expect_err("subagent with spawn");
        let text = err.to_string();
        assert!(
            text.starts_with("field `spawn`:") && text.contains("nested spawning is disabled"),
            "{text}"
        );
        let err = with("mode = \"subagent\"\nspawn = [\"x\"]\n").expect_err("explicit subagent");
        assert!(
            err.to_string().contains("nested spawning is disabled"),
            "{err}"
        );
        for mode in ["primary", "both"] {
            let def = with(&format!(
                "mode = \"{mode}\"\nspawn = [\"explore\", \"plan\"]\n"
            ))
            .expect(mode);
            assert_eq!(def.spawn, ["explore", "plan"]);
        }
        // An empty `spawn` is no restriction, for any mode.
        assert!(with("spawn = []\n").expect("empty").spawn.is_empty());
        // The ids are agent ids, distinct, and not too many.
        for bad in [
            "[\"Bad Id\"]",
            "[\"a--b\"]",
            "[\"x\", \"x\"]",
            "[1]",
            "\"x\"",
        ] {
            let err = with(&format!("mode = \"both\"\nspawn = {bad}\n")).expect_err(bad);
            assert!(
                err.to_string().starts_with("field `spawn`:"),
                "{bad}: {err}"
            );
        }
        let ids = |n: usize| {
            (0..n)
                .map(|n| format!("\"a{n}\""))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let at_the_bound = with(&format!(
            "mode = \"both\"\nspawn = [{}]\n",
            ids(MAX_DEF_SPAWN_IDS)
        ))
        .expect("exactly the bound");
        assert_eq!(at_the_bound.spawn.len(), MAX_DEF_SPAWN_IDS);
        let many = ids(MAX_DEF_SPAWN_IDS + 1);
        let err = with(&format!("mode = \"both\"\nspawn = [{many}]\n")).expect_err("33 ids");
        assert!(err.to_string().contains("33 ids"), "{err}");
        // Both syntaxes say the same.
        let md = AgentDefinition::parse_markdown(
            md_source("x"),
            "---\ndescription: d\nbase_role: coder\nmode: primary\nspawn: explore\n---\n",
        )
        .expect("md");
        assert_eq!(
            (md.mode, md.spawn.as_slice()),
            (AgentMode::Primary, &["explore".to_owned()][..])
        );
        let err = AgentDefinition::parse_markdown(
            md_source("x"),
            "---\ndescription: d\nbase_role: coder\nspawn: explore\n---\n",
        )
        .expect_err("md subagent with spawn");
        assert!(
            err.to_string().contains("nested spawning is disabled"),
            "{err}"
        );
    }

    #[test]
    fn when_to_use_is_one_line_of_at_most_256_bytes() {
        let with = |text: &str| {
            AgentDefinition::parse(
                DefSource::BuiltIn,
                &def_v2(
                    &format!("when_to_use = {}\n", toml::Value::String(text.to_owned())),
                    "",
                ),
            )
        };
        assert_eq!(
            with("After a change").expect("ok").when_to_use.as_deref(),
            Some("After a change")
        );
        assert!(with(&"w".repeat(MAX_DEF_WHEN_TO_USE_BYTES)).is_ok());
        for bad in [
            String::new(),
            "w".repeat(MAX_DEF_WHEN_TO_USE_BYTES + 1),
            "two\nlines".to_owned(),
            "tab\there".to_owned(),
            "esc\u{1b}[0m".to_owned(),
            // Nothing a reader can see, or text no screen shows or that moves the line.
            "   ".to_owned(),
            "\u{200B}".to_owned(),
            "after a change\u{E0049}\u{E0067}\u{E006E}".to_owned(),
            "one\u{2028}two".to_owned(),
            "rtl\u{202E}override".to_owned(),
        ] {
            let err = with(&bad).expect_err(&format!("{bad:?}"));
            assert!(err.to_string().starts_with("field `when_to_use`:"), "{err}");
        }
        // A joiner is spelling, not a disguise.
        assert!(with("a\u{200D}b").is_ok());
        // The Markdown form refuses the same.
        let md = |text: &str| {
            AgentDefinition::parse_markdown(
                md_source("x"),
                &format!("---\ndescription: d\nbase_role: coder\nwhen_to_use: \"{text}\"\n---\n"),
            )
        };
        assert!(md("fine").is_ok());
        assert!(md("hidden\u{E0049}").is_err() && md("   ").is_err());
    }

    #[test]
    fn the_digest_is_the_same_for_the_same_request_whatever_it_was_written_in_and_changes_with_each_part()
     {
        let toml = def_v2(
            "allow_tools = [\"repo_*\"]\nmax_steps = 4\nmode = \"both\"\nwhen_to_use = \"w\"\nspawn = [\"explore\"]\n",
            "[agent.permission]\n\"repo_read\" = \"ask\"\n",
        );
        let md = "---\ndescription: d\nbase_role: coder\ntool_classes: read exec\n\
                  allow_tools: repo_*\nmax_steps: 4\nmode: both\nwhen_to_use: w\nspawn: explore\n\
                  permission:\n  \"repo_read\": ask\n---\n";
        let a = AgentDefinition::parse(
            DefSource::BuiltIn,
            &toml.replace("id = \"narrow\"", "id = \"x\""),
        )
        .expect("toml");
        let b = AgentDefinition::parse_markdown(md_source("x"), md).expect("md");
        assert_eq!(
            a.digest(),
            b.digest(),
            "syntax and source are not part of it"
        );
        assert_eq!(a.digest().len(), 64);
        assert!(a.digest().bytes().all(|c| c.is_ascii_hexdigit()));
        // Each part moves it.
        let digest_of = |edit: &dyn Fn(&mut AgentDefinition)| {
            let mut def = a.clone();
            edit(&mut def);
            def.digest()
        };
        let same = a.digest();
        type Edit = Box<dyn Fn(&mut AgentDefinition)>;
        let edits: Vec<Edit> = vec![
            Box::new(|d| d.description.push('x')),
            Box::new(|d| d.instructions = Some("i".to_owned())),
            Box::new(|d| d.model = Some("fast".to_owned())),
            Box::new(|d| d.reasoning_effort = Some("low".to_owned())),
            Box::new(|d| d.inputs.push("in".to_owned())),
            Box::new(|d| d.outputs.push("out".to_owned())),
            Box::new(|d| d.allow_tools = None),
            Box::new(|d| d.deny_tools.push("repo_glob".to_owned())),
            Box::new(|d| d.permission.clear()),
            Box::new(|d| d.max_steps = Some(5)),
            Box::new(|d| d.mode = AgentMode::Primary),
            Box::new(|d| d.when_to_use = None),
            Box::new(|d| d.spawn.push("plan".to_owned())),
            Box::new(|d| d.tool_surface = RoleToolSurface::none().with(Read)),
            Box::new(|d| d.role = AgentRole::Reviewer),
            Box::new(|d| d.id = AgentDefId::parse("y").expect("id")),
        ];
        let mut seen = vec![same.clone()];
        for (n, edit) in edits.iter().enumerate() {
            let moved = digest_of(&**edit);
            assert!(!seen.contains(&moved), "edit {n} did not move the digest");
            seen.push(moved);
        }
    }

    #[test]
    fn a_list_that_is_a_set_has_one_digest_in_any_order_and_a_changed_set_has_another() {
        let def = |allow: &str, deny: &str, spawn: &str| {
            AgentDefinition::parse(
                DefSource::BuiltIn,
                &def_v2(
                    &format!(
                        "allow_tools = [{allow}]\ndeny_tools = [{deny}]\nmode = \"both\"\nspawn = [{spawn}]\n"
                    ),
                    "",
                ),
            )
            .expect("definition")
        };
        let base = def(
            "\"repo_read\", \"repo_glob\"",
            "\"shell_exec\", \"web_fetch\"",
            "\"plan\", \"explore\"",
        );
        // Another order of the same patterns and ids asks for the same thing.
        let reordered = def(
            "\"repo_glob\", \"repo_read\"",
            "\"web_fetch\", \"shell_exec\"",
            "\"explore\", \"plan\"",
        );
        assert_eq!(base.digest(), reordered.digest());
        // Naming one twice is naming it.
        let repeated = def(
            "\"repo_read\", \"repo_read\", \"repo_glob\"",
            "\"shell_exec\", \"web_fetch\", \"shell_exec\"",
            "\"plan\", \"explore\"",
        );
        assert_eq!(base.digest(), repeated.digest());
        // A different set does not.
        for changed in [
            def(
                "\"repo_read\"",
                "\"shell_exec\", \"web_fetch\"",
                "\"plan\", \"explore\"",
            ),
            def(
                "\"repo_read\", \"repo_glob\"",
                "\"shell_exec\"",
                "\"plan\", \"explore\"",
            ),
            def(
                "\"repo_read\", \"repo_glob\"",
                "\"shell_exec\", \"web_fetch\"",
                "\"plan\"",
            ),
        ] {
            assert_ne!(base.digest(), changed.digest());
        }
        // No allow list is not an empty one.
        let none = AgentDefinition::parse(DefSource::BuiltIn, &def_v2("", "")).expect("none");
        let empty = AgentDefinition::parse(DefSource::BuiltIn, &def_v2("allow_tools = []\n", ""))
            .expect("empty");
        assert_ne!(none.digest(), empty.digest());
    }

    #[test]
    fn a_body_over_the_cap_is_refused_naming_the_body() {
        let at_cap = "x".repeat(MAX_DEF_INSTRUCTIONS_BYTES);
        let ok = AgentDefinition::parse_markdown(md_source("x"), &def_md("", "", &at_cap))
            .expect("at the cap");
        assert_eq!(
            ok.instructions.as_deref().map(str::len),
            Some(MAX_DEF_INSTRUCTIONS_BYTES)
        );
        let over = "x".repeat(MAX_DEF_INSTRUCTIONS_BYTES + 1);
        let err = AgentDefinition::parse_markdown(md_source("x"), &def_md("", "", &over))
            .expect_err("over the cap");
        assert!(err.to_string().starts_with("field `body`:"), "{err}");
        // The table form keeps calling it `instructions`.
        let toml = format!(
            "{}instructions = \"{over}\"\n",
            def_toml("x", "explorer", "[]")
        );
        let err = AgentDefinition::parse(DefSource::BuiltIn, &toml).expect_err("toml");
        assert!(
            err.to_string().starts_with("field `instructions`:"),
            "{err}"
        );
    }

    #[test]
    fn the_markdown_form_names_its_own_tool_key_when_a_class_is_wrong() {
        let err = AgentDefinition::parse_markdown(
            md_source("x"),
            "---\ndescription: a\nbase_role: explorer\ntool_classes: read bogus\n---\n",
        )
        .expect_err("bogus");
        assert!(
            err.to_string().starts_with("field `tool_classes`:"),
            "{err}"
        );
        let def = AgentDefinition::parse_markdown(
            md_source("x"),
            "---\ndescription: a\nbase_role: explorer\ntool_classes: read exec\n---\n",
        )
        .expect("parse");
        let err = def.validate_grants(&full_registry()).expect_err("widens");
        let text = err.to_string();
        assert!(text.starts_with("field `tool_classes`:"), "{text}");
        assert!(text.contains("Remove 'exec' from `tool_classes`"), "{text}");
    }

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn pick<'a, T>(&mut self, pool: &'a [T]) -> &'a T {
            &pool[(self.next() % pool.len() as u64) as usize]
        }

        fn chance(&mut self, one_in: u64) -> bool {
            self.next().is_multiple_of(one_in)
        }
    }

    /// One definition as data, to be written in either syntax.
    #[derive(Debug)]
    struct Spec {
        id: String,
        description: String,
        role: String,
        classes: Option<Vec<String>>,
        instructions: Option<String>,
        model: Option<String>,
        effort: Option<String>,
        inputs: Option<Vec<String>>,
        outputs: Option<Vec<String>>,
        allow_tools: Option<Vec<String>>,
        deny_tools: Option<Vec<String>>,
        permission: Option<Vec<(String, String)>>,
        max_steps: Option<i64>,
        mode: Option<String>,
        when_to_use: Option<String>,
        spawn: Option<Vec<String>>,
    }

    fn generated_spec(rng: &mut Rng) -> Spec {
        const DESCRIPTIONS: &[&str] = &[
            "",
            "plain text",
            "with: a colon",
            "quote \" and \\ slash",
            "hash # inside",
            "tab\there",
            "two\nlines",
            "ünïcode ✓ text",
            "- starts like a list item",
            "[starts like a flow list]",
        ];
        const ROLES: &[&str] = &[
            "explorer", "coder", "reviewer", "planner", "tester", "Reviewer", "nonsense",
        ];
        const CLASSES: &[&str] = &[
            "read", "write", "exec", "net", "browser", "mobile", "mcp", "plugin", "git", "secret",
            "bogus",
        ];
        const BODIES: &[&str] = &[
            "Be brief.",
            "line one\nline two\n\n  indented line",
            "has: a colon\n---\nand a fence-looking line",
            "# a heading\n- a list\n",
            "  padded  ",
            "",
            "   \n\n",
            "ünïcode ✓",
        ];
        const MODELS: &[&str] = &["fast", "a.b-c:d/e", "bad model!", "x", ""];
        const EFFORTS: &[&str] = &["none", "low", "high", "ultra", "extreme"];
        const NAMES: &[&str] = &["diff", "findings", "a_b", "x-y", "Bad Name", "", "-lead"];
        let list = |rng: &mut Rng, pool: &[&str], max: u64| -> Option<Vec<String>> {
            if rng.chance(3) {
                return None;
            }
            Some(
                (0..rng.next() % (max + 1))
                    .map(|_| (*rng.pick(pool)).to_owned())
                    .collect(),
            )
        };
        let description = match rng.next() % 12 {
            0 => "d".repeat(MAX_DEF_DESCRIPTION_BYTES + 1),
            1 => "d".repeat(MAX_DEF_DESCRIPTION_BYTES),
            _ => (*rng.pick(DESCRIPTIONS)).to_owned(),
        };
        let instructions = match rng.next() % 14 {
            0 => Some("i".repeat(MAX_DEF_INSTRUCTIONS_BYTES + 1)),
            1 => Some("i".repeat(MAX_DEF_INSTRUCTIONS_BYTES)),
            2 | 3 => None,
            _ => {
                let text = (*rng.pick(BODIES)).trim().to_owned();
                (!text.is_empty()).then_some(text)
            }
        };
        // The narrowing and steering keys are mostly valid, with a refusal now
        // and then, so the corpus reaches both ends.
        const PATTERNS: &[&str] = &[
            "repo_*",
            "workspace_read",
            "shell_exec",
            "mcp__docs__*",
            "r?po_read",
            "*",
        ];
        const BAD_PATTERNS: &[&str] = &["bad pattern", "a.b", "", "../x"];
        const RULES: &[&str] = &[
            "workspace_patch(docs/*)",
            "shell_exec(rm *)",
            "repo_read",
            "web_fetch(domain:example.com)",
            // The loader's own bounds are all a key is held to here; the host's
            // grammar is checked beside it.
            "Tool With Spaces",
            "\"quoted\" key",
        ];
        let permission = if rng.chance(2) {
            let mut seen: Vec<String> = Vec::new();
            let bad_effect = rng.chance(10);
            Some(
                (0..rng.next() % 4)
                    .filter_map(|n| {
                        let key = (*rng.pick(RULES)).to_owned();
                        if seen.contains(&key) {
                            return None;
                        }
                        seen.push(key.clone());
                        let effect = if bad_effect && n == 0 {
                            *rng.pick(&["allow", "Deny", "maybe"])
                        } else {
                            *rng.pick(&["deny", "ask"])
                        };
                        Some((key, effect.to_owned()))
                    })
                    .collect(),
            )
        } else {
            None
        };
        let patterns = |rng: &mut Rng| -> Option<Vec<String>> {
            if rng.chance(2) {
                return None;
            }
            let mut out: Vec<String> = Vec::new();
            for _ in 0..rng.next() % 5 {
                let pattern = (*rng.pick(PATTERNS)).to_owned();
                if !out.contains(&pattern) {
                    out.push(pattern);
                }
            }
            if rng.chance(10) {
                out.push((*rng.pick(BAD_PATTERNS)).to_owned());
            }
            if rng.chance(60) {
                out = (0..=MAX_DEF_TOOL_PATTERNS)
                    .map(|n| format!("t{n}"))
                    .collect();
            }
            Some(out)
        };
        let allow_tools = patterns(rng);
        let deny_tools = patterns(rng);
        let max_steps = if rng.chance(2) {
            Some(if rng.chance(8) {
                *rng.pick(&[0, 33, 1000, -1])
            } else {
                *rng.pick(&[1, 12, 32])
            })
        } else {
            None
        };
        const IDS: &[&str] = &["reviewer", "plan", "explore", "x"];
        let mut spawn = if rng.chance(2) {
            None
        } else {
            let mut ids: Vec<String> = Vec::new();
            for _ in 0..rng.next() % 4 {
                let id = (*rng.pick(IDS)).to_owned();
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
            if rng.chance(10) {
                ids.push((*rng.pick(&["Bad Id", "a--b"])).to_owned());
            }
            if rng.chance(50) {
                ids = (0..=MAX_DEF_SPAWN_IDS).map(|n| format!("a{n}")).collect();
            }
            Some(ids)
        };
        let mut mode = rng.chance(2).then(|| {
            if rng.chance(8) {
                (*rng.pick(&["Primary", "agent", ""])).to_owned()
            } else {
                (*rng.pick(&["subagent", "primary", "both"])).to_owned()
            }
        });
        // A definition that delegates is a main agent, nearly always.
        if spawn.as_ref().is_some_and(|ids| !ids.is_empty()) && !rng.chance(10) {
            mode = Some((*rng.pick(&["primary", "both"])).to_owned());
        }
        if mode.as_deref() == Some("subagent") && rng.chance(2) {
            spawn = spawn.map(|_| Vec::new());
        }
        let when_to_use = match rng.next() % 10 {
            0 => Some(String::new()),
            1 => Some("w".repeat(MAX_DEF_WHEN_TO_USE_BYTES + 1)),
            2 => Some("line\nbreak".to_owned()),
            3 => Some("w".repeat(MAX_DEF_WHEN_TO_USE_BYTES)),
            4..=6 => None,
            _ => Some(
                (*rng.pick(&[
                    "After a change is complete",
                    "to triage: a failing run",
                    "ünïcode ✓ use",
                ]))
                .to_owned(),
            ),
        };
        Spec {
            mode,
            when_to_use,
            spawn,
            allow_tools,
            deny_tools,
            permission,
            max_steps,
            id: if rng.chance(15) {
                "Not An Id".to_owned()
            } else {
                format!("agent-{}", rng.next() % 1000)
            },
            description,
            role: (*rng.pick(ROLES)).to_owned(),
            classes: list(rng, CLASSES, 5),
            instructions,
            model: rng.chance(2).then(|| (*rng.pick(MODELS)).to_owned()),
            effort: rng.chance(2).then(|| (*rng.pick(EFFORTS)).to_owned()),
            inputs: list(rng, NAMES, 4),
            outputs: list(rng, NAMES, 4),
        }
    }

    fn written_as_toml(spec: &Spec) -> String {
        let quoted = |text: &str| toml::Value::String(text.to_owned()).to_string();
        let array = |items: &[String]| {
            format!(
                "[{}]",
                items
                    .iter()
                    .map(|item| quoted(item))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let mut out = format!(
            "schema = \"{AGENT_DEFS_SCHEMA_V2}\"\n[agent]\nid = {}\ndescription = {}\nbase_role = {}\n",
            quoted(&spec.id),
            quoted(&spec.description),
            quoted(&spec.role),
        );
        if let Some(classes) = &spec.classes {
            out.push_str(&format!("tools = {}\n", array(classes)));
        }
        if let Some(text) = &spec.instructions {
            out.push_str(&format!("instructions = {}\n", quoted(text)));
        }
        if let Some(model) = &spec.model {
            out.push_str(&format!("model = {}\n", quoted(model)));
        }
        if let Some(effort) = &spec.effort {
            out.push_str(&format!("reasoning_effort = {}\n", quoted(effort)));
        }
        if let Some(inputs) = &spec.inputs {
            out.push_str(&format!("inputs = {}\n", array(inputs)));
        }
        if let Some(outputs) = &spec.outputs {
            out.push_str(&format!("outputs = {}\n", array(outputs)));
        }
        if let Some(allow) = &spec.allow_tools {
            out.push_str(&format!("allow_tools = {}\n", array(allow)));
        }
        if let Some(deny) = &spec.deny_tools {
            out.push_str(&format!("deny_tools = {}\n", array(deny)));
        }
        if let Some(steps) = spec.max_steps {
            out.push_str(&format!("max_steps = {steps}\n"));
        }
        if let Some(mode) = &spec.mode {
            out.push_str(&format!("mode = {}\n", quoted(mode)));
        }
        if let Some(text) = &spec.when_to_use {
            out.push_str(&format!("when_to_use = {}\n", quoted(text)));
        }
        if let Some(ids) = &spec.spawn {
            out.push_str(&format!("spawn = {}\n", array(ids)));
        }
        // A table goes last.
        if let Some(rules) = &spec.permission {
            out.push_str("[agent.permission]\n");
            for (pattern, effect) in rules {
                out.push_str(&format!("{} = {}\n", quoted(pattern), quoted(effect)));
            }
        }
        out
    }

    fn written_as_markdown(spec: &Spec, rng: &mut Rng) -> String {
        let quoted = |text: &str| {
            let mut out = String::from("\"");
            for ch in text.chars() {
                match ch {
                    '\\' => out.push_str("\\\\"),
                    '"' => out.push_str("\\\""),
                    '\n' => out.push_str("\\n"),
                    '\t' => out.push_str("\\t"),
                    other => out.push(other),
                }
            }
            out.push('"');
            out
        };
        let mut lines: Vec<String> = Vec::new();
        if rng.chance(2) {
            lines.push(format!("name: {}", quoted(&spec.id)));
        }
        lines.push(format!("description: {}", quoted(&spec.description)));
        lines.push(format!("base_role: {}", quoted(&spec.role)));
        let mut listed = |key: &str, items: &Option<Vec<String>>, rng: &mut Rng| {
            let Some(items) = items else { return };
            let spaced = items
                .iter()
                .any(|item| item.is_empty() || item.contains(' '));
            if items.is_empty() {
                lines.push(if rng.chance(2) {
                    format!("{key}: []")
                } else {
                    format!("{key}:")
                });
            } else if !spaced && rng.chance(2) {
                lines.push(format!("{key}: {}", items.join(" ")));
            } else {
                lines.push(format!("{key}:"));
                for item in items {
                    lines.push(format!("  - {item}"));
                }
            }
        };
        listed("tool_classes", &spec.classes, rng);
        listed("inputs", &spec.inputs, rng);
        listed("outputs", &spec.outputs, rng);
        listed("allow_tools", &spec.allow_tools, rng);
        listed("deny_tools", &spec.deny_tools, rng);
        listed("spawn", &spec.spawn, rng);
        if let Some(steps) = spec.max_steps {
            lines.push(format!("max_steps: {steps}"));
        }
        if let Some(mode) = &spec.mode {
            lines.push(format!("mode: {}", quoted(mode)));
        }
        if let Some(text) = &spec.when_to_use {
            lines.push(format!("when_to_use: {}", quoted(text)));
        }
        if let Some(rules) = &spec.permission {
            lines.push("permission:".to_owned());
            for (pattern, effect) in rules {
                lines.push(format!("  {}: {}", quoted(pattern), quoted(effect)));
            }
        }
        if let Some(model) = &spec.model {
            lines.push(format!("model: {}", quoted(model)));
        }
        if let Some(effort) = &spec.effort {
            lines.push(format!("reasoning_effort: {}", quoted(effort)));
        }
        // Block lists keep their items under their key; whole entries move.
        let mut entries: Vec<Vec<String>> = Vec::new();
        for line in lines {
            if line.starts_with("  ") {
                entries.last_mut().expect("a key first").push(line);
            } else {
                entries.push(vec![line]);
            }
        }
        for at in (1..entries.len()).rev() {
            let to = (rng.next() % (at as u64 + 1)) as usize;
            entries.swap(at, to);
        }
        let mut out = String::from("---\n");
        for entry in entries {
            if rng.chance(6) {
                out.push_str("# a comment\n\n");
            }
            for line in entry {
                out.push_str(&line);
                out.push('\n');
            }
        }
        out.push_str("---\n");
        if rng.chance(2) {
            out.push('\n');
        }
        if let Some(text) = &spec.instructions {
            out.push_str(text);
            out.push('\n');
        }
        out
    }

    fn outcome_kind(result: &Result<AgentDefinition, AgentDefError>) -> String {
        match result {
            Ok(_) => "ok".to_owned(),
            Err(AgentDefError::FieldInvalid { field, .. }) => match *field {
                "body" => "field instructions".to_owned(),
                other => format!("field {other}"),
            },
            Err(AgentDefError::UnknownToolClass { .. }) => "unknown tool class".to_owned(),
            Err(other) => format!("{:?}", std::mem::discriminant(other)),
        }
    }

    #[test]
    fn a_markdown_definition_and_its_table_form_mean_the_same_over_generated_definitions() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let (mut accepted, mut refused) = (0u32, 0u32);
        for case in 0..30_000u32 {
            let spec = generated_spec(&mut rng);
            let toml = written_as_toml(&spec);
            let markdown = written_as_markdown(&spec, &mut rng);
            // The stem is the id, as the loader requires of a Markdown file.
            let stem = spec.id.clone();
            let as_toml = AgentDefinition::parse(
                DefSource::Project(PathBuf::from(format!("agents/{stem}.toml"))),
                &toml,
            );
            let as_markdown = AgentDefinition::parse_file(
                DefSource::Project(PathBuf::from(format!("agents/{stem}.md"))),
                &markdown,
            );
            assert_eq!(
                outcome_kind(&as_toml),
                outcome_kind(&as_markdown),
                "case {case}: {spec:?}\n{toml}\n{markdown}\n{as_toml:?}\n{as_markdown:?}"
            );
            match (as_toml, as_markdown) {
                (Ok(mut a), Ok(mut b)) => {
                    accepted += 1;
                    a.source = DefSource::BuiltIn;
                    b.source = DefSource::BuiltIn;
                    assert_eq!(a, b, "case {case}: {spec:?}\n{toml}\n{markdown}");
                }
                _ => refused += 1,
            }
        }
        assert!(
            accepted > 1_000 && refused > 1_000,
            "the corpus must cover both: {accepted} accepted, {refused} refused"
        );
    }

    #[test]
    fn a_markdown_and_a_table_definition_of_one_id_are_both_refused_naming_each() {
        let dir = temp_dir("collision");
        std::fs::write(
            dir.join("twin.toml"),
            def_toml("twin", "explorer", "[\"read\"]"),
        )
        .expect("write");
        std::fs::write(dir.join("twin.md"), def_md("", "", "body")).expect("write");
        std::fs::write(dir.join("alone.md"), def_md("", "", "body")).expect("write");
        // A file that sorts between the twins (`twin.md` < `twin.old.toml` <
        // `twin.toml`) is a definition of its own and does not hide the clash.
        std::fs::write(
            dir.join("twin.old.toml"),
            def_toml("twin-old", "explorer", "[\"read\"]"),
        )
        .expect("write");
        let inventory = load_directory(&dir, &full_registry()).expect("scan");
        let mut ids: Vec<&str> = inventory.loaded.iter().map(|d| d.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            ["alone", "twin-old"],
            "neither twin is read, whatever sorts between them: {ids:?}"
        );
        assert_eq!(inventory.rejected.len(), 2, "{:?}", inventory.rejected);
        for rejected in &inventory.rejected {
            assert!(
                rejected.reason.contains("twin.md") && rejected.reason.contains("twin.toml"),
                "{}",
                rejected.reason
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_directory_loads_both_syntaxes_and_one_bad_file_does_not_hide_the_rest() {
        let dir = temp_dir("both");
        std::fs::write(dir.join("a-md.md"), def_md("", "", "from markdown")).expect("write");
        std::fs::write(
            dir.join("b-toml.toml"),
            def_toml("b-toml", "explorer", "[\"read\"]"),
        )
        .expect("write");
        std::fs::write(dir.join("c-bad.md"), "no front matter at all\n").expect("write");
        std::fs::write(dir.join("d-bytes.md"), b"---\ndescription: \xff\xfe\n---\n")
            .expect("write");
        std::fs::write(dir.join("notes.txt"), "not a definition").expect("write");
        let inventory = load_directory(&dir, &full_registry()).expect("scan");
        let formats: Vec<(&str, Option<DefFormat>)> = inventory
            .loaded
            .iter()
            .map(|d| (d.id.as_str(), d.format()))
            .collect();
        assert_eq!(
            formats,
            [
                ("a-md", Some(DefFormat::Markdown)),
                ("b-toml", Some(DefFormat::Toml))
            ]
        );
        let reasons: Vec<String> = inventory
            .rejected
            .iter()
            .map(|r| {
                format!(
                    "{}: {}",
                    r.path.file_name().unwrap().to_string_lossy(),
                    r.reason
                )
            })
            .collect();
        assert_eq!(reasons.len(), 2, "{reasons:?}");
        assert!(reasons[0].starts_with("c-bad.md") && reasons[0].contains("no front matter"));
        assert!(reasons[1].starts_with("d-bytes.md") && reasons[1].contains("not valid UTF-8"));
        // Over the byte cap, in either syntax.
        std::fs::write(dir.join("huge.md"), vec![b'#'; MAX_DEF_FILE_BYTES * 2]).expect("write");
        let inventory = load_directory(&dir, &full_registry()).expect("scan");
        assert!(
            inventory
                .rejected
                .iter()
                .any(|r| r.path.ends_with("huge.md") && r.reason.contains("exceeds")),
            "{:?}",
            inventory.rejected
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_definitions_directory_is_skipped_with_a_reason() {
        let real = temp_dir("real-agents");
        std::fs::write(real.join("inside.md"), def_md("", "", "body")).expect("write");
        let holder = temp_dir("holder");
        let link = holder.join("agents");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        for origin in [DefOrigin::Project, DefOrigin::User, DefOrigin::Extra] {
            let inventory = load_directory_from(&link, &full_registry(), origin).expect("scan");
            assert!(
                inventory.loaded.is_empty(),
                "{origin:?}: nothing is read through it"
            );
            assert_eq!(inventory.rejected.len(), 1, "{origin:?}");
            assert_eq!(inventory.rejected[0].path, link);
            assert!(inventory.rejected[0].reason.contains("symlink"));
        }
        // The same files, read where they really are.
        let direct = load_directory(&real, &full_registry()).expect("scan");
        assert_eq!(direct.loaded.len(), 1);
        // A layered inventory keeps the project's reason beside what else loads.
        let layered = layered_inventory(Some(&link), None, &full_registry()).expect("layered");
        assert!(
            layered
                .loaded
                .iter()
                .all(|d| d.source == DefSource::BuiltIn)
        );
        assert_eq!(layered.rejected.len(), 1);
        let _ = std::fs::remove_dir_all(&real);
        let _ = std::fs::remove_dir_all(&holder);
    }

    #[test]
    fn extra_directories_rank_below_the_user_in_the_order_written() {
        let project = temp_dir("x-project");
        let user = temp_dir("x-user");
        let first = temp_dir("x-first");
        let second = temp_dir("x-second");
        let write = |dir: &Path, file: &str, id: &str| {
            std::fs::write(dir.join(file), def_toml(id, "explorer", "[\"read\"]")).expect("write");
        };
        write(&project, "p.toml", "from-project");
        write(&user, "u.toml", "from-user");
        write(&user, "dup.toml", "contested");
        write(&first, "f.toml", "from-first");
        write(&first, "dup.toml", "contested");
        write(&second, "s.toml", "from-second");
        write(&second, "dup2.toml", "from-first");
        write(&second, "built.toml", "explore");
        let extras = [first.clone(), second.clone()];
        let inventory =
            layered_inventory_with(Some(&project), Some(&user), &extras, &full_registry())
                .expect("inventory");
        let kinds: Vec<(&str, &str)> = inventory
            .loaded
            .iter()
            .map(|d| (d.id.as_str(), d.source.kind()))
            .collect();
        for expected in [
            ("from-project", "project"),
            ("from-user", "user"),
            ("contested", "user"),
            ("from-first", "extra"),
            ("from-second", "extra"),
        ] {
            assert!(kinds.contains(&expected), "{expected:?} in {kinds:?}");
        }
        let at = |id: &str| kinds.iter().position(|(seen, _)| *seen == id).expect(id);
        assert!(
            at("from-project") < at("from-user")
                && at("from-user") < at("from-first")
                && at("from-first") < at("from-second"),
            "{kinds:?}"
        );
        let reasons: Vec<&str> = inventory
            .rejected
            .iter()
            .map(|r| r.reason.as_str())
            .collect();
        assert_eq!(reasons.len(), 3, "{reasons:?}");
        assert!(
            reasons
                .iter()
                .any(|r| r.contains("already the user's; a extra-directory")),
            "{reasons:?}"
        );
        assert!(
            reasons
                .iter()
                .any(|r| r.contains("already the extra-directory's")),
            "{reasons:?}"
        );
        assert!(
            reasons.iter().any(|r| r.contains("built-in")),
            "{reasons:?}"
        );
        // Past the cap, the rest are not read.
        let many: Vec<PathBuf> = (0..MAX_EXTRA_DEF_DIRS + 2)
            .map(|n| {
                let dir = temp_dir(&format!("x-many-{n}"));
                write(&dir, "one.toml", &format!("many-{n}"));
                dir
            })
            .collect();
        let inventory =
            layered_inventory_with(None, None, &many, &full_registry()).expect("inventory");
        let extras_loaded = inventory
            .loaded
            .iter()
            .filter(|d| d.source.kind() == "extra")
            .count();
        assert_eq!(extras_loaded, MAX_EXTRA_DEF_DIRS);
        for dir in [project, user, first, second].iter().chain(many.iter()) {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn both_scaffolds_parse_back_and_validate() {
        let id = AgentDefId::parse("starter").expect("id");
        for format in [DefFormat::Toml, DefFormat::Markdown] {
            let text = AgentDefinition::scaffold_as(&id, format);
            let source =
                DefSource::Project(PathBuf::from(format!("agents/starter.{}", format.as_str())));
            let def = AgentDefinition::parse_file(source, &text)
                .unwrap_or_else(|err| panic!("{format:?}: {err}\n{text}"));
            assert_eq!(def.id.as_str(), "starter");
            assert_eq!(def.format(), Some(format));
            def.validate_grants(&full_registry()).expect("validates");
        }
        // The two starters mean the same.
        let toml = AgentDefinition::parse_file(
            DefSource::BuiltIn,
            &AgentDefinition::scaffold_as(&id, DefFormat::Toml),
        )
        .expect("toml");
        let md = AgentDefinition::parse_markdown(
            DefSource::Project(PathBuf::from("agents/starter.md")),
            &AgentDefinition::scaffold_as(&id, DefFormat::Markdown),
        )
        .expect("md");
        assert_eq!(
            (toml.role, &toml.tool_surface, &toml.description),
            (md.role, &md.tool_surface, &md.description)
        );
    }

    #[test]
    fn the_user_directory_constant_is_the_one_the_loader_reads() {
        assert_eq!(USER_DEFS_DIR, "agents");
        assert_eq!(DefFormat::parse("md"), Some(DefFormat::Markdown));
        assert_eq!(DefFormat::parse("markdown"), Some(DefFormat::Markdown));
        assert_eq!(DefFormat::parse("toml"), Some(DefFormat::Toml));
        assert_eq!(DefFormat::parse("yaml"), None);
    }
}
