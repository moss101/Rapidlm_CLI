//! Canonical v1 model-visible gateway tool schemas.
//!
//! [`ToolCatalog::v1`] is the closed, ordered catalog from
//! `api-contracts/tool-gateway-api.md`. Provider adapters map native function
//! calling onto these names. Capability discovery stays behind the gateway.

use std::error::Error;
use std::fmt;
use std::str::FromStr;

use protocol::{ArtifactId, ErrorCode};
use serde::Serialize;
use serde_json::{Map, Value};

/// Wire schema name for [`ToolCatalog`].
pub const TOOL_CATALOG_SCHEMA: &str = "rapidlm.tool_catalog";

/// v1 schema version for the model-visible catalog.
pub const TOOL_CATALOG_SCHEMA_VERSION: u16 = 1;

/// Number of v1 model-visible gateway tools.
pub const V1_TOOL_COUNT: usize = 12;

/// SHA-256 of the canonical v1 catalog document. Order or schema drift fails.
pub const V1_CATALOG_HASH: &str =
    "sha256:c2127ba39ec170e735fce76c2bebbb430086f98e7dc0c36900dd78dd17b9b7a8";

/// Maximum UTF-8 bytes for one model-visible string argument.
pub const MAX_STRING_BYTES: usize = 4_096;

/// Maximum UTF-8 bytes for a semantic-patch payload (matches workspace crate).
pub const MAX_PATCH_CONTENT_BYTES: usize = 8 * 1024 * 1024;

/// Maximum operations accepted on one `workspace.patch` call.
pub const MAX_PATCH_OPS: usize = 4_096;

/// Maximum argv tokens including argv0 (matches command normalizer).
pub const MAX_ARGV: usize = 256;

/// Maximum UTF-8 bytes for one argv token.
pub const MAX_ARG_BYTES: usize = 4_096;

/// Maximum UTF-8 bytes for an explicit shell script.
pub const MAX_SHELL_SCRIPT_BYTES: usize = 64 * 1024;

/// Maximum environment variable names on `shell.exec`.
pub const MAX_ENV_NAMES: usize = 256;

/// Maximum UTF-8 bytes for one environment variable name.
pub const MAX_ENV_NAME_BYTES: usize = 256;

/// Maximum repositories accepted on `repo.search`.
pub const MAX_REPOS: usize = 32;

/// Maximum languages accepted on `repo.search`.
pub const MAX_LANGUAGES: usize = 16;

/// Hard search-hit cap. Callers may request less.
pub const MAX_SEARCH_LIMIT: u32 = 256;

/// Default `repo.search` page size (matches FTS default).
pub const DEFAULT_SEARCH_LIMIT: u32 = 20;

/// Maximum UTF-8 bytes for a bounded `repo.read` excerpt.
pub const MAX_READ_BYTES: u32 = 1_048_576;

/// Maximum UTF-8 bytes for an agent task or goal statement.
pub const MAX_TASK_BYTES: usize = 16 * 1024;

/// Maximum completion criteria on `goal.update`.
pub const MAX_CRITERIA: usize = 64;

/// Maximum properties on a proxied `external.call` arguments object.
pub const MAX_EXTERNAL_ARGUMENT_FIELDS: usize = 32;

/// Maximum UTF-8 bytes for one serialized `external.call` arguments object.
pub const MAX_EXTERNAL_ARGUMENTS_BYTES: usize = 16 * 1024;

/// Maximum preconditions or expected assertions on a computer-use action.
pub const MAX_UI_ASSERTIONS: usize = 16;

/// Maximum keys in a chord.
pub const MAX_CHORD_KEYS: usize = 8;

/// Maximum wall-clock milliseconds accepted on a tool timeout field.
pub const MAX_TIMEOUT_MS: u64 = 3_600_000;

/// Argument names that must never appear on a model-visible schema.
///
/// These are privilege, secret, or host-escape handles. Unknown extra
/// properties are also forbidden via `additionalProperties: false`.
pub const DENIED_ARGUMENT_NAMES: &[&str] = &[
    "api_key",
    "authorization",
    "capability_lease",
    "capability_token",
    "cookie",
    "credentials",
    "elevate",
    "force_complete",
    "grant_capability",
    "host_path",
    "lease",
    "override_policy",
    "parent_transcript",
    "password",
    "plaintext",
    "private_key",
    "privileged",
    "raw_adb",
    "secret",
    "secret_plaintext",
    "simctl",
    "skip_evidence",
    "sudo",
    "token",
    "write_owner",
];

const UUID_PATTERN: &str = "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$";
const ARTIFACT_ID_PATTERN: &str = "^sha256:[0-9a-f]{64}$";

const SEARCH_MODES: &[&str] = &["hybrid", "lexical", "structural"];
const LANGUAGES: &[&str] = &[
    "rust",
    "typescript",
    "javascript",
    "python",
    "go",
    "java",
    "c",
    "cpp",
    "csharp",
    "kotlin",
    "swift",
    "ruby",
    "bash",
    "json",
    "yaml",
    "toml",
    "markdown",
];
const PATCH_OPS: &[&str] = &["replace_range", "create_file", "delete_file", "move_file"];
const STATUS_INCLUDES: &[&str] = &["diff", "view", "checkpoint"];
const AGENT_ROLES: &[&str] = &[
    "planner",
    "coder",
    "explorer",
    "reviewer",
    "verifier",
    "security_reviewer",
    "context_curator",
];
const WORKSPACE_ACCESS: &[&str] = &["read_only", "write_isolated"];
const RESULT_ACTIONS: &[&str] = &["inspect", "merge"];
const GOAL_ACTIONS: &[&str] = &[
    "pause",
    "resume",
    "block",
    "complete",
    "cancel",
    "replace_criteria",
    "set_budget",
    "link_evidence",
];
const BROWSER_ACTIONS: &[&str] = &[
    "observe",
    "click",
    "drag",
    "scroll",
    "type_text",
    "key",
    "chord",
    "focus_window",
    "resize_window",
    "launch_app",
    "navigate",
    "upload_file",
    "download",
];
const MOBILE_ACTIONS: &[&str] = &[
    "observe",
    "tap",
    "swipe",
    "type_text",
    "key",
    "launch_app",
    "install_app",
    "screenshot",
];
const TARGET_KINDS: &[&str] = &["dom", "accessibility", "tui", "visual", "coordinate"];
const MOUSE_BUTTONS: &[&str] = &["left", "right", "middle"];
const EXTERNAL_KINDS: &[&str] = &["mcp", "plugin"];
const EVIDENCE_KINDS: &[&str] = &[
    "test",
    "build",
    "lint",
    "scan",
    "diff",
    "runtime_observation",
    "user_confirmation",
    "external_attestation",
    "manual_review",
    "status",
    "source",
    "artifact",
    "command",
];

/// Closed v1 model-visible tool name. Unknown names fail closed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum GatewayTool {
    RepoSearch,
    RepoRead,
    WorkspacePatch,
    WorkspaceStatus,
    ShellExec,
    AgentSpawn,
    AgentResult,
    GoalUpdate,
    BrowserAct,
    MobileAct,
    ExternalCall,
    EvidenceRecord,
}

/// One catalog row: stable name, purpose text, and JSON Schema parameters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolSchema {
    name: GatewayTool,
    description: &'static str,
    parameters: Value,
}

/// Ordered v1 gateway catalog. Mutations require a new schema version.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolCatalog {
    tools: Vec<ToolSchema>,
}

/// Typed catalog/schema failure. Display never echoes argument values.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum SchemaError {
    UnknownTool,
    DeniedArgument,
    InvalidSchema,
}

impl GatewayTool {
    /// Contract order from `api-contracts/tool-gateway-api.md`.
    pub const ALL: &'static [Self] = &[
        Self::RepoSearch,
        Self::RepoRead,
        Self::WorkspacePatch,
        Self::WorkspaceStatus,
        Self::ShellExec,
        Self::AgentSpawn,
        Self::AgentResult,
        Self::GoalUpdate,
        Self::BrowserAct,
        Self::MobileAct,
        Self::ExternalCall,
        Self::EvidenceRecord,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RepoSearch => "repo.search",
            Self::RepoRead => "repo.read",
            Self::WorkspacePatch => "workspace.patch",
            Self::WorkspaceStatus => "workspace.status",
            Self::ShellExec => "shell.exec",
            Self::AgentSpawn => "agent.spawn",
            Self::AgentResult => "agent.result",
            Self::GoalUpdate => "goal.update",
            Self::BrowserAct => "browser.act",
            Self::MobileAct => "mobile.act",
            Self::ExternalCall => "external.call",
            Self::EvidenceRecord => "evidence.record",
        }
    }

    /// Purpose text from the stable tool contract table.
    pub const fn description(self) -> &'static str {
        match self {
            Self::RepoSearch => "hybrid/lexical/structural search",
            Self::RepoRead => "bounded file/range/symbol read",
            Self::WorkspacePatch => "stage semantic patch",
            Self::WorkspaceStatus => "diff/view/checkpoint state",
            Self::ShellExec => "supervised command execution",
            Self::AgentSpawn => "create isolated subagent",
            Self::AgentResult => "inspect/merge typed subagent result",
            Self::GoalUpdate => "machine goal lifecycle/evidence linkage",
            Self::BrowserAct => "observe/act/verify browser",
            Self::MobileAct => "simulator actions",
            Self::ExternalCall => "MCP/plugin capability proxy",
            Self::EvidenceRecord => "attach verifiable evidence",
        }
    }

    fn parameters(self) -> Value {
        match self {
            Self::RepoSearch => repo_search_schema(),
            Self::RepoRead => repo_read_schema(),
            Self::WorkspacePatch => workspace_patch_schema(),
            Self::WorkspaceStatus => workspace_status_schema(),
            Self::ShellExec => shell_exec_schema(),
            Self::AgentSpawn => agent_spawn_schema(),
            Self::AgentResult => agent_result_schema(),
            Self::GoalUpdate => goal_update_schema(),
            Self::BrowserAct => browser_act_schema(),
            Self::MobileAct => mobile_act_schema(),
            Self::ExternalCall => external_call_schema(),
            Self::EvidenceRecord => evidence_record_schema(),
        }
    }
}

impl FromStr for GatewayTool {
    type Err = SchemaError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        for tool in Self::ALL {
            if tool.as_str() == s {
                return Ok(*tool);
            }
        }
        Err(SchemaError::UnknownTool)
    }
}

impl fmt::Display for GatewayTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ToolSchema {
    fn new(name: GatewayTool) -> Result<Self, SchemaError> {
        let parameters = name.parameters();
        if !parameters.is_object() {
            return Err(SchemaError::InvalidSchema);
        }
        if schema_allows_additional_properties(&parameters) {
            return Err(SchemaError::InvalidSchema);
        }
        if first_denied_schema_property(&parameters).is_some() {
            return Err(SchemaError::DeniedArgument);
        }
        Ok(Self {
            name,
            description: name.description(),
            parameters,
        })
    }

    pub const fn name(&self) -> GatewayTool {
        self.name
    }

    pub const fn description(&self) -> &'static str {
        self.description
    }

    pub fn parameters(&self) -> &Value {
        &self.parameters
    }
}

impl ToolCatalog {
    /// Stable ordered v1 catalog. Names, descriptions, and schemas match the contract.
    pub fn v1() -> Self {
        Self::try_v1().unwrap_or_else(|err| {
            unreachable!("v1 tool catalog is a compile-time invariant: {err}")
        })
    }

    fn try_v1() -> Result<Self, SchemaError> {
        let mut tools = Vec::with_capacity(V1_TOOL_COUNT);
        for name in GatewayTool::ALL {
            tools.push(ToolSchema::new(*name)?);
        }
        if tools.len() != V1_TOOL_COUNT {
            return Err(SchemaError::InvalidSchema);
        }
        Ok(Self { tools })
    }

    pub fn tools(&self) -> &[ToolSchema] {
        &self.tools
    }

    pub fn get(&self, name: GatewayTool) -> Option<&ToolSchema> {
        self.tools.iter().find(|tool| tool.name == name)
    }

    pub fn get_by_name(&self, name: &str) -> Result<&ToolSchema, SchemaError> {
        let tool = GatewayTool::from_str(name)?;
        self.get(tool).ok_or(SchemaError::UnknownTool)
    }

    /// Content hash of the canonical catalog document (`sha256:<hex>`).
    pub fn hash(&self) -> ArtifactId {
        let entries: Vec<CatalogHashEntry<'_>> = self
            .tools
            .iter()
            .map(|tool| CatalogHashEntry {
                name: tool.name.as_str(),
                description: tool.description,
                parameters: &tool.parameters,
            })
            .collect();
        let document = CatalogHashDocument {
            schema: TOOL_CATALOG_SCHEMA,
            schema_version: TOOL_CATALOG_SCHEMA_VERSION,
            tools: &entries,
        };
        let bytes = serde_json::to_vec(&document)
            .unwrap_or_else(|err| unreachable!("catalog hash document is serializable: {err}"));
        ArtifactId::from_bytes(&bytes)
    }
}

impl SchemaError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnknownTool => "unknown tool",
            Self::DeniedArgument => "denied argument",
            Self::InvalidSchema => "invalid schema",
        }
    }

    pub const fn error_code(self) -> ErrorCode {
        ErrorCode::ToolInvalidArguments
    }
}

impl fmt::Display for SchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for SchemaError {}

/// True when `name` is a reserved dangerous argument key.
pub fn denied_argument_name(name: &str) -> bool {
    DENIED_ARGUMENT_NAMES.binary_search(&name).is_ok()
}

#[derive(Serialize)]
struct CatalogHashDocument<'a> {
    schema: &'static str,
    schema_version: u16,
    tools: &'a [CatalogHashEntry<'a>],
}

#[derive(Serialize)]
struct CatalogHashEntry<'a> {
    name: &'static str,
    description: &'static str,
    parameters: &'a Value,
}

fn repo_search_schema() -> Value {
    object_schema(
        &["query"],
        vec![
            ("query", string_schema(1, MAX_STRING_BYTES as u64)),
            (
                "repos",
                array_schema(ident_schema(), 1, MAX_REPOS as u64, true),
            ),
            (
                "limit",
                integer_schema(
                    1,
                    i64::from(MAX_SEARCH_LIMIT),
                    Some(i64::from(DEFAULT_SEARCH_LIMIT)),
                ),
            ),
            ("mode", enum_schema(SEARCH_MODES)),
            ("path", repo_path_schema()),
            (
                "languages",
                array_schema(enum_schema(LANGUAGES), 1, MAX_LANGUAGES as u64, true),
            ),
        ],
    )
}

fn repo_read_schema() -> Value {
    object_schema(
        &["path"],
        vec![
            ("path", repo_path_schema()),
            ("repo", ident_schema()),
            ("start_line", integer_schema(1, i64::from(u32::MAX), None)),
            ("end_line", integer_schema(1, i64::from(u32::MAX), None)),
            ("start_byte", integer_schema(0, i64::from(u32::MAX), None)),
            ("end_byte", integer_schema(0, i64::from(u32::MAX), None)),
            ("symbol", string_schema(1, MAX_STRING_BYTES as u64)),
            (
                "max_bytes",
                integer_schema(1, i64::from(MAX_READ_BYTES), None),
            ),
        ],
    )
}

fn workspace_patch_schema() -> Value {
    object_schema(
        &["ops"],
        vec![(
            "ops",
            array_schema(patch_op_schema(), 1, MAX_PATCH_OPS as u64, false),
        )],
    )
}

fn workspace_status_schema() -> Value {
    object_schema(
        &[],
        vec![(
            "include",
            array_schema(
                enum_schema(STATUS_INCLUDES),
                1,
                STATUS_INCLUDES.len() as u64,
                true,
            ),
        )],
    )
}

fn shell_exec_schema() -> Value {
    let mut schema = object_schema(
        &[],
        vec![
            (
                "argv",
                array_schema(
                    string_schema(1, MAX_ARG_BYTES as u64),
                    1,
                    MAX_ARGV as u64,
                    false,
                ),
            ),
            (
                "shell",
                json_object(&[
                    ("type", Value::from("boolean")),
                    ("default", Value::from(false)),
                ]),
            ),
            ("script", string_schema(1, MAX_SHELL_SCRIPT_BYTES as u64)),
            ("cwd", repo_path_schema()),
            (
                "env_names",
                array_schema(
                    string_schema(1, MAX_ENV_NAME_BYTES as u64),
                    0,
                    MAX_ENV_NAMES as u64,
                    true,
                ),
            ),
            ("timeout_ms", integer_schema(1, MAX_TIMEOUT_MS as i64, None)),
        ],
    );
    // argv-first unless `shell` is explicitly true. Shell-string is never inferred.
    object_fields_mut(&mut schema).insert(
        "allOf".to_owned(),
        Value::Array(vec![json_object(&[
            (
                "if",
                json_object(&[
                    (
                        "properties",
                        json_object(&[("shell", json_object(&[("const", Value::from(true))]))]),
                    ),
                    ("required", Value::Array(vec![Value::from("shell")])),
                ]),
            ),
            (
                "then",
                json_object(&[("required", Value::Array(vec![Value::from("script")]))]),
            ),
            (
                "else",
                json_object(&[("required", Value::Array(vec![Value::from("argv")]))]),
            ),
        ])]),
    );
    schema
}

fn agent_spawn_schema() -> Value {
    object_schema(
        &["role", "task", "access"],
        vec![
            ("role", enum_schema(AGENT_ROLES)),
            ("task", string_schema(1, MAX_TASK_BYTES as u64)),
            ("access", enum_schema(WORKSPACE_ACCESS)),
            ("model_policy", ident_schema()),
            ("budget", budget_schema()),
            ("expected_result", string_schema(1, MAX_STRING_BYTES as u64)),
        ],
    )
}

fn agent_result_schema() -> Value {
    object_schema(
        &["agent_id", "action"],
        vec![
            ("agent_id", uuid_schema()),
            ("action", enum_schema(RESULT_ACTIONS)),
        ],
    )
}

fn goal_update_schema() -> Value {
    object_schema(
        &["action"],
        vec![
            ("action", enum_schema(GOAL_ACTIONS)),
            ("goal_id", uuid_schema()),
            ("statement", string_schema(1, MAX_TASK_BYTES as u64)),
            ("reason", string_schema(1, MAX_STRING_BYTES as u64)),
            (
                "criteria",
                array_schema(criterion_schema(), 1, MAX_CRITERIA as u64, true),
            ),
            ("budget", budget_schema()),
            ("evidence_id", uuid_schema()),
            ("criterion_id", string_schema(1, MAX_STRING_BYTES as u64)),
        ],
    )
}

fn browser_act_schema() -> Value {
    computer_act_schema(BROWSER_ACTIONS, true)
}

fn mobile_act_schema() -> Value {
    let mut schema = computer_act_schema(MOBILE_ACTIONS, false);
    properties_mut(&mut schema).insert("device_id".to_owned(), ident_schema());
    schema
}

fn external_call_schema() -> Value {
    let mut schema = object_schema(
        &["kind", "tool"],
        vec![
            ("kind", enum_schema(EXTERNAL_KINDS)),
            ("server", ident_schema()),
            ("plugin", ident_schema()),
            ("tool", ident_schema()),
            (
                "arguments",
                json_object(&[
                    ("type", Value::from("object")),
                    ("additionalProperties", Value::from(true)),
                    (
                        "maxProperties",
                        Value::from(MAX_EXTERNAL_ARGUMENT_FIELDS as u64),
                    ),
                ]),
            ),
        ],
    );
    object_fields_mut(&mut schema).insert(
        "allOf".to_owned(),
        Value::Array(vec![json_object(&[
            (
                "if",
                json_object(&[
                    (
                        "properties",
                        json_object(&[("kind", json_object(&[("const", Value::from("mcp"))]))]),
                    ),
                    ("required", Value::Array(vec![Value::from("kind")])),
                ]),
            ),
            (
                "then",
                json_object(&[("required", Value::Array(vec![Value::from("server")]))]),
            ),
            (
                "else",
                json_object(&[("required", Value::Array(vec![Value::from("plugin")]))]),
            ),
        ])]),
    );
    schema
}

fn evidence_record_schema() -> Value {
    object_schema(
        &["kind", "assertion", "subject_ref"],
        vec![
            ("kind", enum_schema(EVIDENCE_KINDS)),
            ("assertion", string_schema(1, MAX_STRING_BYTES as u64)),
            ("subject_ref", string_schema(1, MAX_STRING_BYTES as u64)),
            ("criterion_id", string_schema(1, MAX_STRING_BYTES as u64)),
            ("artifact_id", artifact_id_schema()),
            ("command", string_schema(1, MAX_STRING_BYTES as u64)),
        ],
    )
}

fn computer_act_schema(actions: &[&'static str], include_browser_fields: bool) -> Value {
    let mut properties = vec![
        ("action", enum_schema(actions)),
        ("observation_id", ident_schema()),
        ("target", target_schema()),
        (
            "preconditions",
            array_schema(
                string_schema(1, MAX_STRING_BYTES as u64),
                0,
                MAX_UI_ASSERTIONS as u64,
                true,
            ),
        ),
        (
            "expected",
            array_schema(
                string_schema(1, MAX_STRING_BYTES as u64),
                0,
                MAX_UI_ASSERTIONS as u64,
                true,
            ),
        ),
        ("timeout_ms", integer_schema(1, MAX_TIMEOUT_MS as i64, None)),
        ("button", enum_schema(MOUSE_BUTTONS)),
        ("count", integer_schema(1, 3, None)),
        (
            "dx",
            integer_schema(i64::from(i32::MIN), i64::from(i32::MAX), None),
        ),
        (
            "dy",
            integer_schema(i64::from(i32::MIN), i64::from(i32::MAX), None),
        ),
        ("text", string_schema(0, MAX_STRING_BYTES as u64)),
        ("secret_handle", ident_schema()),
        ("key", ident_schema()),
        (
            "keys",
            array_schema(ident_schema(), 1, MAX_CHORD_KEYS as u64, false),
        ),
        ("width", integer_schema(1, 16_384, None)),
        ("height", integer_schema(1, 16_384, None)),
        ("app", ident_schema()),
    ];
    if include_browser_fields {
        properties.push(("url", string_schema(1, MAX_STRING_BYTES as u64)));
        properties.push(("artifact_id", artifact_id_schema()));
    }
    let mut schema = object_schema(&["action"], properties);
    object_fields_mut(&mut schema).insert(
        "allOf".to_owned(),
        Value::Array(vec![json_object(&[
            (
                "if",
                json_object(&[
                    (
                        "properties",
                        json_object(&[(
                            "action",
                            json_object(&[("const", Value::from("observe"))]),
                        )]),
                    ),
                    ("required", Value::Array(vec![Value::from("action")])),
                ]),
            ),
            ("then", json_object(&[])),
            (
                "else",
                json_object(&[(
                    "required",
                    Value::Array(vec![Value::from("observation_id")]),
                )]),
            ),
        ])]),
    );
    schema
}

fn target_schema() -> Value {
    object_schema(
        &["kind"],
        vec![
            ("kind", enum_schema(TARGET_KINDS)),
            ("selector", string_schema(1, MAX_STRING_BYTES as u64)),
            ("node", string_schema(1, MAX_STRING_BYTES as u64)),
            ("region", string_schema(1, MAX_STRING_BYTES as u64)),
            ("label", string_schema(1, MAX_STRING_BYTES as u64)),
            ("observation_id", ident_schema()),
            ("x", integer_schema(0, 16_384, None)),
            ("y", integer_schema(0, 16_384, None)),
        ],
    )
}

fn patch_op_schema() -> Value {
    json_object(&[
        ("type", Value::from("object")),
        ("additionalProperties", Value::from(false)),
        ("required", Value::Array(vec![Value::from("op")])),
        (
            "properties",
            json_object(&[
                ("op", enum_schema(PATCH_OPS)),
                ("path", repo_path_schema()),
                ("from", repo_path_schema()),
                ("to", repo_path_schema()),
                ("preimage", artifact_id_schema()),
                ("start", integer_schema(0, i64::from(u32::MAX), None)),
                ("end", integer_schema(0, i64::from(u32::MAX), None)),
                ("content", string_schema(0, MAX_PATCH_CONTENT_BYTES as u64)),
                (
                    "executable",
                    Value::Object({
                        let mut map = Map::new();
                        map.insert("type".to_owned(), Value::from("boolean"));
                        map
                    }),
                ),
            ]),
        ),
        (
            "allOf",
            Value::Array(vec![
                op_branch(
                    "replace_range",
                    &["path", "preimage", "start", "end", "content"],
                ),
                op_branch("create_file", &["path", "content"]),
                op_branch("delete_file", &["path", "preimage"]),
                op_branch("move_file", &["from", "to", "preimage"]),
            ]),
        ),
    ])
}

fn op_branch(op: &str, required: &[&str]) -> Value {
    json_object(&[
        (
            "if",
            json_object(&[
                (
                    "properties",
                    json_object(&[("op", json_object(&[("const", Value::from(op))]))]),
                ),
                ("required", Value::Array(vec![Value::from("op")])),
            ]),
        ),
        (
            "then",
            json_object(&[(
                "required",
                Value::Array(required.iter().copied().map(Value::from).collect()),
            )]),
        ),
    ])
}

fn budget_schema() -> Value {
    object_schema(
        &[],
        vec![
            ("max_tokens", integer_schema(0, i64::MAX, None)),
            ("max_cost", integer_schema(0, i64::MAX, None)),
            ("max_active_ms", integer_schema(0, i64::MAX, None)),
            ("max_tool_calls", integer_schema(0, i64::MAX, None)),
        ],
    )
}

fn criterion_schema() -> Value {
    object_schema(
        &["id", "text"],
        vec![
            ("id", string_schema(1, MAX_STRING_BYTES as u64)),
            ("text", string_schema(1, MAX_STRING_BYTES as u64)),
        ],
    )
}

fn object_schema(required: &[&str], properties: Vec<(&str, Value)>) -> Value {
    let mut props = Map::new();
    for (name, schema) in properties {
        debug_assert!(
            !denied_argument_name(name),
            "schema property {name} is a denied argument"
        );
        props.insert(name.to_owned(), schema);
    }
    let mut fields: Vec<(&str, Value)> = vec![
        ("type", Value::from("object")),
        ("additionalProperties", Value::from(false)),
        ("properties", Value::Object(props)),
    ];
    if !required.is_empty() {
        fields.push((
            "required",
            Value::Array(required.iter().copied().map(Value::from).collect()),
        ));
    }
    json_object(&fields)
}

fn string_schema(min_length: u64, max_length: u64) -> Value {
    json_object(&[
        ("type", Value::from("string")),
        ("minLength", Value::from(min_length)),
        ("maxLength", Value::from(max_length)),
    ])
}

fn ident_schema() -> Value {
    json_object(&[
        ("type", Value::from("string")),
        ("minLength", Value::from(1)),
        ("maxLength", Value::from(256)),
        (
            "pattern",
            Value::from("^[A-Za-z0-9][A-Za-z0-9._:-]{0,254}$"),
        ),
    ])
}

fn repo_path_schema() -> Value {
    json_object(&[
        ("type", Value::from("string")),
        ("minLength", Value::from(1)),
        (
            "maxLength",
            Value::from(protocol::MAX_REPO_PATH_BYTES as u64),
        ),
        ("pattern", Value::from(r"^[^/\x00][^\\:\x00]*$")),
    ])
}

fn uuid_schema() -> Value {
    json_object(&[
        ("type", Value::from("string")),
        ("minLength", Value::from(36)),
        ("maxLength", Value::from(36)),
        ("pattern", Value::from(UUID_PATTERN)),
    ])
}

fn artifact_id_schema() -> Value {
    json_object(&[
        ("type", Value::from("string")),
        ("minLength", Value::from(71)),
        ("maxLength", Value::from(71)),
        ("pattern", Value::from(ARTIFACT_ID_PATTERN)),
    ])
}

fn enum_schema(values: &[&str]) -> Value {
    json_object(&[
        ("type", Value::from("string")),
        (
            "enum",
            Value::Array(values.iter().copied().map(Value::from).collect()),
        ),
    ])
}

fn integer_schema(minimum: i64, maximum: i64, default: Option<i64>) -> Value {
    let mut fields = vec![
        ("type", Value::from("integer")),
        ("minimum", Value::from(minimum)),
        ("maximum", Value::from(maximum)),
    ];
    if let Some(default) = default {
        fields.push(("default", Value::from(default)));
    }
    json_object(&fields)
}

fn array_schema(items: Value, min_items: u64, max_items: u64, unique: bool) -> Value {
    let mut fields = vec![
        ("type", Value::from("array")),
        ("items", items),
        ("minItems", Value::from(min_items)),
        ("maxItems", Value::from(max_items)),
    ];
    if unique {
        fields.push(("uniqueItems", Value::from(true)));
    }
    json_object(&fields)
}

fn json_object(fields: &[(&str, Value)]) -> Value {
    let mut map = Map::new();
    for (key, value) in fields {
        map.insert((*key).to_owned(), value.clone());
    }
    Value::Object(map)
}

fn object_fields_mut(schema: &mut Value) -> &mut Map<String, Value> {
    match schema {
        Value::Object(map) => map,
        _ => unreachable!("constructed JSON Schema root is an object"),
    }
}

fn properties_mut(schema: &mut Value) -> &mut Map<String, Value> {
    match object_fields_mut(schema).get_mut("properties") {
        Some(Value::Object(map)) => map,
        _ => unreachable!("constructed JSON Schema always has a properties object"),
    }
}

fn schema_allows_additional_properties(schema: &Value) -> bool {
    match schema {
        Value::Object(map) => {
            if map.get("additionalProperties") == Some(&Value::Bool(true))
                && map.get("type") == Some(&Value::String("object".to_owned()))
                && !map.contains_key("maxProperties")
            {
                return true;
            }
            map.values().any(schema_allows_additional_properties)
        }
        Value::Array(items) => items.iter().any(schema_allows_additional_properties),
        _ => false,
    }
}

fn first_denied_schema_property(schema: &Value) -> Option<&'static str> {
    walk_schema_keys(schema, &mut |key| denied_argument_name(key).then_some(key)).and_then(|key| {
        DENIED_ARGUMENT_NAMES
            .iter()
            .copied()
            .find(|name| *name == key)
    })
}

fn walk_schema_keys<'a, F, T>(schema: &'a Value, visit: &mut F) -> Option<T>
where
    F: FnMut(&'a str) -> Option<T>,
{
    let Value::Object(map) = schema else {
        return None;
    };
    if let Some(properties) = map.get("properties").and_then(Value::as_object) {
        for (key, child) in properties {
            if let Some(hit) = visit(key) {
                return Some(hit);
            }
            if let Some(hit) = walk_schema_keys(child, visit) {
                return Some(hit);
            }
        }
    }
    for (key, child) in map {
        if key == "properties" {
            continue;
        }
        if let Some(hit) = walk_schema_keys(child, visit) {
            return Some(hit);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn collect_property_names(schema: &Value, out: &mut BTreeSet<String>) {
        walk_schema_keys(schema, &mut |key| {
            out.insert(key.to_owned());
            None::<()>
        });
    }

    const CONTRACT_NAMES: &[&str] = &[
        "repo.search",
        "repo.read",
        "workspace.patch",
        "workspace.status",
        "shell.exec",
        "agent.spawn",
        "agent.result",
        "goal.update",
        "browser.act",
        "mobile.act",
        "external.call",
        "evidence.record",
    ];

    #[test]
    fn denied_names_are_sorted_unique() {
        let mut sorted = DENIED_ARGUMENT_NAMES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted, DENIED_ARGUMENT_NAMES);
    }

    #[test]
    fn v1_catalog_matches_contract_order_and_descriptions() {
        let catalog = ToolCatalog::v1();
        assert_eq!(catalog.tools().len(), V1_TOOL_COUNT);
        assert_eq!(GatewayTool::ALL.len(), V1_TOOL_COUNT);
        for (tool, expected) in catalog.tools().iter().zip(CONTRACT_NAMES) {
            assert_eq!(tool.name().as_str(), *expected);
            assert_eq!(tool.description(), tool.name().description());
        }
    }

    #[test]
    fn v1_catalog_hash_is_stable() {
        let catalog = ToolCatalog::v1();
        let hash = catalog.hash().to_string();
        assert_eq!(hash, V1_CATALOG_HASH);
        assert_eq!(ToolCatalog::v1().hash().to_string(), hash);
    }

    #[test]
    fn hash_detects_name_order_drift() {
        let catalog = ToolCatalog::v1();
        let mut swapped = catalog.tools.clone();
        swapped.swap(0, 1);
        let drifted = ToolCatalog { tools: swapped };
        assert_ne!(drifted.hash().to_string(), V1_CATALOG_HASH);
    }

    #[test]
    fn schemas_are_closed_objects_with_bounds() {
        let catalog = ToolCatalog::v1();
        for tool in catalog.tools() {
            let schema = tool.parameters();
            assert_eq!(schema["type"], "object");
            assert_eq!(schema["additionalProperties"], false);
            assert!(
                schema
                    .get("properties")
                    .and_then(Value::as_object)
                    .is_some()
            );
            let encoded = serde_json::to_vec(schema).expect("schema json");
            assert!(
                encoded.len() <= 16 * 1024,
                "{} parameters exceed provider bound ({})",
                tool.name(),
                encoded.len()
            );
            assert!(tool.description().len() <= 4096);
        }
    }

    #[test]
    fn schemas_reject_unknown_dangerous_arguments() {
        let catalog = ToolCatalog::v1();
        for tool in catalog.tools() {
            let mut names = BTreeSet::new();
            collect_property_names(tool.parameters(), &mut names);
            for denied in DENIED_ARGUMENT_NAMES {
                assert!(
                    !names.contains(*denied),
                    "{} advertises denied argument {denied}",
                    tool.name()
                );
            }
            assert!(!schema_allows_additional_properties(tool.parameters()));
        }
        assert!(denied_argument_name("capability_lease"));
        assert!(denied_argument_name("secret_plaintext"));
        assert!(denied_argument_name("host_path"));
        assert!(denied_argument_name("raw_adb"));
        assert!(!denied_argument_name("query"));
    }

    #[test]
    fn contract_example_arguments_are_declared() {
        let catalog = ToolCatalog::v1();
        let search = catalog.get(GatewayTool::RepoSearch).expect("repo.search");
        let properties = search.parameters()["properties"]
            .as_object()
            .expect("props");
        assert!(properties.contains_key("query"));
        assert!(properties.contains_key("repos"));
        assert!(properties.contains_key("limit"));
        assert_eq!(properties["mode"]["enum"][0], "hybrid");
        assert_eq!(properties["limit"]["default"], DEFAULT_SEARCH_LIMIT);
    }

    #[test]
    fn shell_exec_requires_explicit_shell_string() {
        let catalog = ToolCatalog::v1();
        let schema = catalog
            .get(GatewayTool::ShellExec)
            .expect("shell.exec")
            .parameters();
        assert_eq!(schema["properties"]["shell"]["default"], false);
        let all_of = schema["allOf"].as_array().expect("allOf");
        assert_eq!(all_of[0]["if"]["properties"]["shell"]["const"], true);
        assert_eq!(all_of[0]["then"]["required"][0], "script");
        assert_eq!(all_of[0]["else"]["required"][0], "argv");
        assert!(
            !schema["properties"]
                .as_object()
                .expect("props")
                .contains_key("env")
        );
    }

    #[test]
    fn spawn_and_goal_enums_exclude_privilege_escalation() {
        let catalog = ToolCatalog::v1();
        let spawn = catalog.get(GatewayTool::AgentSpawn).expect("spawn");
        let roles = spawn.parameters()["properties"]["role"]["enum"]
            .as_array()
            .expect("roles");
        assert!(!roles.iter().any(|role| role == "main"));
        let access = spawn.parameters()["properties"]["access"]["enum"]
            .as_array()
            .expect("access");
        assert!(access.iter().any(|item| item == "read_only"));
        assert!(access.iter().any(|item| item == "write_isolated"));

        let goal = catalog.get(GatewayTool::GoalUpdate).expect("goal");
        let actions = goal.parameters()["properties"]["action"]["enum"]
            .as_array()
            .expect("actions");
        assert!(actions.iter().any(|item| item == "complete"));
        assert!(!actions.iter().any(|item| item == "force_complete"));
    }

    #[test]
    fn computer_use_actions_require_observation_except_observe() {
        let catalog = ToolCatalog::v1();
        for tool in [GatewayTool::BrowserAct, GatewayTool::MobileAct] {
            let schema = catalog.get(tool).expect("tool").parameters();
            let all_of = schema["allOf"].as_array().expect("allOf");
            assert_eq!(all_of[0]["if"]["properties"]["action"]["const"], "observe");
            assert_eq!(all_of[0]["else"]["required"][0], "observation_id");
            let target_kinds = schema["properties"]["target"]["properties"]["kind"]["enum"]
                .as_array()
                .expect("kinds");
            assert!(target_kinds.iter().any(|item| item == "dom"));
            assert!(target_kinds.iter().any(|item| item == "coordinate"));
        }
    }

    #[test]
    fn unknown_tool_names_fail_closed() {
        assert_eq!(
            GatewayTool::from_str("repo.delete"),
            Err(SchemaError::UnknownTool)
        );
        assert_eq!(
            ToolCatalog::v1().get_by_name("sudo.exec"),
            Err(SchemaError::UnknownTool)
        );
        assert_eq!(
            SchemaError::UnknownTool.error_code(),
            ErrorCode::ToolInvalidArguments
        );
    }

    #[test]
    fn try_v1_succeeds_and_lookup_works() {
        let catalog = ToolCatalog::try_v1().expect("v1");
        assert!(catalog.get(GatewayTool::EvidenceRecord).is_some());
        assert_eq!(
            catalog.get_by_name("workspace.patch").map(|t| t.name()),
            Ok(GatewayTool::WorkspacePatch)
        );
    }
}
