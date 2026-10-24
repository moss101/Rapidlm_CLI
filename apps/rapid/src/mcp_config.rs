//! Canonical project MCP server configuration: one parser, one merge, one
//! set of bounds, shared by every consumer.
//!
//! Before this module existed the only reader was
//! `exec_tools::parse_mcp_servers`, called once per file from
//! [`crate::interactive::load_project_integrations`] and `extend`ed into a
//! flat list. That shape had four defects that all present the same way to a
//! user — a configured server that simply never appears, with no diagnostic
//! anywhere:
//!
//! * the "at most 8 servers" bound was applied *per file*, so a project with
//!   both `.rapidlm/settings.json` and `.claude/settings.json` could register
//!   16 — and adding a second settings file silently doubled the ceiling;
//! * duplicate names across files each spawned their own child process, but
//!   `ExecTools::execute_mcp_tool` resolves a server by `find`, so only the
//!   first was ever reachable: the second was a live, unreachable process and
//!   a set of duplicate `mcp__<server>__<tool>` entries on the model-facing
//!   surface;
//! * a name containing `__` produced tool names no call could ever route,
//!   because `execute_mcp_tool` splits on the *first* `__` — `mcp__a__b__tool`
//!   resolves server `a`, tool `b__tool`;
//! * every rejection was silent: a missing `command` (which is what a remote
//!   `{"type": "http", "url": …}` entry looks like to this parser), an
//!   over-long name, or an entry past the cap all vanished without a word.
//!
//! It also had no `env` support while `register_mcp_servers` calls
//! `env_clear()`, so an MCP server that needs an API key in its environment —
//! most real ones — could not be configured at all.
//!
//! Everything here is pure: it reads settings files and returns data. Spawning
//! is [`crate::exec_tools`]'s job, and this module never decides trust — the
//! caller does, exactly as it did before (`load_project_integrations` is only
//! consulted for a `TrustStatus::Trusted` project).

use std::collections::BTreeSet;
use std::path::Path;

use crate::exec_tools::McpServerConfig;
use crate::interactive::PROJECT_SETTINGS_FILES;

/// Servers admitted from a whole project, across every settings file.
///
/// Applied once after the merge rather than once per file: the bound exists
/// so a project cannot make Rapid spawn an unbounded number of child
/// processes at session start, and a per-file bound does not bound that.
///
/// Which servers survive an over-capacity project is deterministic but
/// arbitrary: settings files in [`PROJECT_SETTINGS_FILES`] order, and within
/// a file the `mcpServers` keys in ascending order (`serde_json`'s map is a
/// `BTreeMap` in this build — no `preserve_order` feature), *not* the order
/// they appear in the file. The ones that do not fit are reported as
/// [`McpConfigIssue::OverCapacity`] rather than dropped, so the choice is at
/// least visible.
pub const MAX_MCP_SERVERS: usize = 8;
/// A server name is embedded in every one of its tools' wire names
/// (`mcp__<name>__<tool>`), which the model sees and must reproduce exactly.
pub const MAX_SERVER_NAME_BYTES: usize = 32;
/// Bounds on the spawn description itself. These are generous — they exist to
/// keep a hostile or corrupt settings file from producing an unbounded argv,
/// not to constrain real configurations.
pub const MAX_COMMAND_BYTES: usize = 4096;
/// URL/header bounds for streamable-HTTP entries (mirrors the stdio bounds'
/// spirit: bounded config can never balloon a request).
pub const MAX_URL_BYTES: usize = 2048;
pub const MAX_HTTP_HEADERS: usize = 16;
pub const MAX_HTTP_HEADER_KEY_BYTES: usize = 128;
pub const MAX_HTTP_HEADER_VALUE_BYTES: usize = 4096;
pub const MAX_ARGS: usize = 64;
pub const MAX_ARG_BYTES: usize = 4096;
pub const MAX_ENV_VARS: usize = 32;
pub const MAX_ENV_KEY_BYTES: usize = 128;
pub const MAX_ENV_VALUE_BYTES: usize = 4096;
/// Patterns in one of a server's tool lists (`enabled_tools`, `disabled_tools`).
pub const MAX_TOOL_PATTERNS: usize = 64;
/// The longest tool pattern.
pub const MAX_TOOL_PATTERN_BYTES: usize = 128;
/// How many rejections are kept for reporting. Nothing bounds how many
/// entries a settings file declares, and every rejection becomes a stderr
/// warning on *every turn* — so an accidental (or hostile) file with
/// thousands of malformed entries must not turn each turn's startup into
/// thousands of lines. The count of the rest is still reported.
pub const MAX_REPORTED_REJECTIONS: usize = 32;
/// Cap on any settings-derived string interpolated into a report line.
/// Values here come from a project file, so they are attacker-controlled in
/// the same sense a repository is: bounded and flattened to one line before
/// they can appear in `rapid mcp list`, `rapid doctor`, or a stderr warning.
pub const MAX_LABEL_BYTES: usize = 96;

/// Truncate to [`MAX_LABEL_BYTES`] on a char boundary and replace every
/// control character with a space, so a settings-derived string can only ever
/// occupy the line it was given — the same row-forgery defense
/// `doctor::bounded` applies to its own details.
pub fn label(raw: &str) -> String {
    let mut end = raw.len().min(MAX_LABEL_BYTES);
    while end > 0 && !raw.is_char_boundary(end) {
        end -= 1;
    }
    let mut out: String = raw[..end]
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    if end < raw.len() {
        out.push_str("...");
    }
    out
}

/// Why one `mcpServers` entry (or one settings file) was not admitted.
///
/// Every variant is reported, never silently dropped: `rapid mcp list` prints
/// them and `rapid doctor`'s `mcp` row counts them. A rejection is always
/// about one named entry except [`Self::FileUnreadable`], which is about the
/// file as a whole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpConfigIssue {
    /// The settings file exists but its contents could not be used — either
    /// it is not parseable JSON, or reading it failed (no permission, a
    /// directory in its place, an I/O error). Non-fatal by long-standing
    /// precedent: a broken settings file has never failed a run, it just
    /// contributes nothing. What changes is that it is no longer silent.
    FileUnreadable {
        detail: String,
    },
    /// `mcpServers` is present but is not a JSON object.
    NotAnObject,
    /// The entry's value is not a JSON object.
    EntryNotAnObject,
    NameEmpty,
    NameTooLong {
        bytes: usize,
    },
    /// A name outside `[A-Za-z0-9._:-]` cannot appear verbatim in a
    /// model-facing tool name — that is `agent_runtime::turn::valid_ident`'s
    /// alphabet, which the composed `mcp__<server>__<tool>` has to satisfy.
    NameCharset,
    /// `__` is the `mcp__<server>__<tool>` separator; a name containing it
    /// makes every tool on that server unroutable.
    NameReservedSeparator,
    /// No `command` key. This is also what a remote (`type`/`url`) entry looks
    /// like, which is why `remote` is reported separately below.
    MissingCommand,
    MissingUrl,
    UrlTooLong {
        bytes: usize,
    },
    TooManyHttpHeaders {
        count: usize,
    },
    HttpHeaderNotAString {
        key: String,
    },
    HttpHeaderTooLong {
        key: String,
    },
    /// An entry of `headers_env` is not `{"Header-Name": "ENV_VAR"}` with a
    /// valid header name and an upper-case environment variable name, or
    /// repeats a name `headers` already sets.
    HeaderEnvInvalid {
        key: String,
    },
    /// `bearer_token_env` is not an upper-case environment variable name
    /// (`[A-Z_][A-Z0-9_]{0,63}`) or is not a string.
    BearerEnvInvalid,
    /// A `url` that is plain `http` to a host that is not on this machine or
    /// network, with a credential configured: the credential would cross the
    /// network unencrypted.
    PlainHttpCredential,
    /// An `Authorization` header and `bearer_token_env` are both configured:
    /// two credentials for one server, and no rule for which wins.
    CredentialConflict,
    /// `auth` is not `false` and not an object of the keys a project file may
    /// set, or one of them is out of its bounds. The reason names the key.
    AuthInvalid {
        reason: &'static str,
    },
    /// `auth` next to a static credential: the flow never engages for a server
    /// that has one, so the two cannot both be meant.
    AuthWithStaticCredential,
    /// `auth` on an entry that is not remote: authorizing a stdio server is not
    /// something this build does.
    AuthOnStdio,
    /// A `type`/`url` entry: a transport this build does not speak. Reported
    /// as its own issue rather than as "no command", which would be true but
    /// useless.
    RemoteTransport {
        kind: String,
    },
    CommandEmpty,
    CommandTooLong {
        bytes: usize,
    },
    ArgsNotAnArray,
    ArgNotAString {
        index: usize,
    },
    TooManyArgs {
        count: usize,
    },
    ArgTooLong {
        index: usize,
        bytes: usize,
    },
    EnvNotAnObject,
    TooManyEnvVars {
        count: usize,
    },
    /// An environment variable name outside `[A-Za-z_][A-Za-z0-9_]*`.
    EnvKeyInvalid {
        key: String,
    },
    EnvValueNotAString {
        key: String,
    },
    EnvValueTooLong {
        key: String,
        bytes: usize,
    },
    /// `enabled_tools` or `disabled_tools` is not a list of tool patterns:
    /// not a list, too many, an entry that is not a string, or a pattern that
    /// is empty, over its bound, or holds a character a tool name cannot.
    ToolListInvalid {
        key: &'static str,
        reason: ToolListFault,
    },
    /// `deferred` is not `true` or `false`.
    DeferredInvalid,
    /// `startup_timeout_ms` or `tool_timeout_ms` is not a whole number of
    /// milliseconds within its bounds.
    TimeoutInvalid {
        key: &'static str,
        min: u64,
        max: u64,
    },
    /// A server of this name was already admitted from an earlier settings
    /// file. First file wins, matching every other list-shaped setting's
    /// precedence.
    Shadowed {
        by_file: String,
    },
    /// An earlier settings file has an entry of this name that asks for fewer
    /// tools or its own limits and was rejected: this one does not run in its
    /// place.
    HeldByRejected {
        by_file: String,
    },
    /// Admitted entries already reached [`MAX_MCP_SERVERS`].
    OverCapacity {
        limit: usize,
    },
}

impl std::fmt::Display for McpConfigIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FileUnreadable { detail } => {
                write!(f, "settings file could not be read: {detail}")
            }
            Self::NotAnObject => f.write_str("`mcpServers` is not a JSON object"),
            Self::EntryNotAnObject => f.write_str("entry is not a JSON object"),
            Self::NameEmpty => f.write_str("server name is empty"),
            Self::NameTooLong { bytes } => {
                write!(
                    f,
                    "server name is {bytes} bytes, over the {MAX_SERVER_NAME_BYTES}-byte limit"
                )
            }
            Self::NameCharset => f.write_str(
                "server name may only contain letters, digits, '-', '_', '.' and ':' \
(it becomes part of every `mcp__<server>__<tool>` name the model calls)",
            ),
            Self::NameReservedSeparator => f.write_str(
                "server name contains '__', which separates server from tool in \
`mcp__<server>__<tool>`; every tool on this server would be unroutable",
            ),
            Self::MissingCommand => f.write_str("entry has no `command`"),
            Self::MissingUrl => f.write_str("an http entry needs a `url`"),
            Self::UrlTooLong { bytes } => f.write_str(&format!(
                "`url` is {bytes} bytes, over the {MAX_URL_BYTES}-byte limit"
            )),
            Self::TooManyHttpHeaders { count } => {
                f.write_str(&format!("`headers` has {count} entries, over the limit"))
            }
            Self::HttpHeaderNotAString { key } => {
                f.write_str(&format!("header `{key}` is not a string"))
            }
            Self::HttpHeaderTooLong { key } => {
                f.write_str(&format!("header `{key}` exceeds its byte bound"))
            }
            Self::HeaderEnvInvalid { key } => write!(
                f,
                "`headers_env` entry {key:?} must map a header name to an environment \
variable name ([A-Z_][A-Z0-9_]*) and must not repeat a name in `headers`"
            ),
            Self::BearerEnvInvalid => f.write_str(
                "`bearer_token_env` must name an environment variable \
([A-Z_][A-Z0-9_]*, at most 64 bytes)",
            ),
            Self::PlainHttpCredential => f.write_str(
                "a credential is configured for a plain `http://` URL that is not on this \
machine or network; use `https://`",
            ),
            Self::CredentialConflict => f.write_str(
                "an `Authorization` header and `bearer_token_env` are both set; configure one",
            ),
            Self::AuthInvalid { reason } => write!(
                f,
                "`auth` must be `false` or an object with only client_id, client_secret_env, \
scopes and callback_port ({reason})"
            ),
            Self::AuthWithStaticCredential => f.write_str(
                "`auth` is set beside a static credential (`bearer_token_env`, `headers_env` or \
an `Authorization` header); the authorization flow never starts for such a server",
            ),
            Self::AuthOnStdio => f.write_str(
                "`auth` applies to a remote (`\"type\": \"http\"`) entry; a stdio server is not \
authorized by this build",
            ),
            Self::RemoteTransport { kind } => write!(
                f,
                "remote MCP transport {kind:?} is not supported; this build speaks stdio \
(an entry with a `command`) and streamable HTTP (`\"type\": \"http\"` with a `url`)"
            ),
            Self::CommandEmpty => f.write_str("`command` is empty"),
            Self::CommandTooLong { bytes } => {
                write!(
                    f,
                    "`command` is {bytes} bytes, over the {MAX_COMMAND_BYTES}-byte limit"
                )
            }
            Self::ArgsNotAnArray => f.write_str("`args` is not a JSON array"),
            Self::ArgNotAString { index } => write!(f, "`args[{index}]` is not a string"),
            Self::TooManyArgs { count } => {
                write!(f, "{count} args, over the {MAX_ARGS} limit")
            }
            Self::ArgTooLong { index, bytes } => {
                write!(
                    f,
                    "`args[{index}]` is {bytes} bytes, over the {MAX_ARG_BYTES}-byte limit"
                )
            }
            Self::EnvNotAnObject => f.write_str("`env` is not a JSON object"),
            Self::TooManyEnvVars { count } => {
                write!(f, "{count} env vars, over the {MAX_ENV_VARS} limit")
            }
            Self::EnvKeyInvalid { key } => write!(
                f,
                "env key {key:?} is not a valid environment variable name"
            ),
            Self::EnvValueNotAString { key } => {
                write!(f, "env value for {key:?} is not a string")
            }
            Self::EnvValueTooLong { key, bytes } => write!(
                f,
                "env value for {key:?} is {bytes} bytes, over the {MAX_ENV_VALUE_BYTES}-byte limit"
            ),
            Self::ToolListInvalid { key, reason } => write!(f, "`{key}` {reason}"),
            Self::DeferredInvalid => f.write_str("`deferred` must be true or false"),
            Self::TimeoutInvalid { key, min, max } => write!(
                f,
                "`{key}` must be a whole number of milliseconds from {min} to {max}"
            ),
            Self::Shadowed { by_file } => {
                write!(
                    f,
                    "a server of this name was already configured by {by_file}"
                )
            }
            Self::HeldByRejected { by_file } => write!(
                f,
                "{by_file} has an entry of this name that limits its tools or its time and was rejected; this one is not run in its place"
            ),
            Self::OverCapacity { limit } => {
                write!(f, "already at the {limit}-server limit for this project")
            }
        }
    }
}

/// What is wrong with a tool list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolListFault {
    NotAList,
    TooMany { count: usize },
    NotAString { index: usize },
    Empty { index: usize },
    TooLong { index: usize, bytes: usize },
    Charset { index: usize },
}

impl std::fmt::Display for ToolListFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAList => f.write_str("is not a list of tool name patterns"),
            Self::TooMany { count } => {
                write!(
                    f,
                    "has {count} patterns, over the {MAX_TOOL_PATTERNS} limit"
                )
            }
            Self::NotAString { index } => write!(f, "[{index}] is not a string"),
            Self::Empty { index } => write!(f, "[{index}] is empty"),
            Self::TooLong { index, bytes } => write!(
                f,
                "[{index}] is {bytes} bytes, over the {MAX_TOOL_PATTERN_BYTES}-byte limit"
            ),
            Self::Charset { index } => write!(
                f,
                "[{index}] may only hold letters, digits, '.', '_', ':', '-', '*' and '?'"
            ),
        }
    }
}

/// A bounded collection of rejections plus a count of the ones dropped past
/// the bound.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RejectionLog {
    entries: Vec<McpConfigRejection>,
    omitted: usize,
}

impl RejectionLog {
    fn push(&mut self, rejection: McpConfigRejection) {
        if self.entries.len() < MAX_REPORTED_REJECTIONS {
            self.entries.push(rejection);
        } else {
            self.omitted += 1;
        }
    }

    fn total(&self) -> usize {
        self.entries.len() + self.omitted
    }
}

/// One entry that did not become a running server, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpConfigRejection {
    /// The `mcpServers` key, or `*` for a whole-file issue.
    pub name: String,
    /// The project-relative settings file the entry came from.
    pub file: String,
    pub issue: McpConfigIssue,
}

impl McpConfigRejection {
    /// Whole-file marker used when no individual entry can be blamed.
    pub const WHOLE_FILE: &'static str = "*";
}

/// One admitted server plus the settings file that defined it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedMcpServer {
    pub config: McpServerConfig,
    pub file: String,
}

/// Every `mcpServers` entry a project declares, split into what will run and
/// what will not.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct McpProjectConfig {
    servers: Vec<LoadedMcpServer>,
    rejections: RejectionLog,
    files_present: Vec<String>,
}

impl McpProjectConfig {
    pub fn servers(&self) -> &[LoadedMcpServer] {
        &self.servers
    }

    /// The rejections kept for reporting, at most
    /// [`MAX_REPORTED_REJECTIONS`] of them.
    pub fn rejections(&self) -> &[McpConfigRejection] {
        &self.rejections.entries
    }

    /// How many rejections were dropped past the reporting bound.
    pub fn rejections_omitted(&self) -> usize {
        self.rejections.omitted
    }

    /// Every rejected entry, reported or not.
    pub fn rejections_total(&self) -> usize {
        self.rejections.total()
    }

    /// Settings files that existed and were read (parseable or not).
    pub fn files_present(&self) -> &[String] {
        &self.files_present
    }

    /// The spawn descriptions, in registration order — the exact list handed
    /// to `ExecTools::register_mcp_servers`.
    /// Keep the servers `keep` accepts; how many were dropped.
    pub fn retain(&mut self, mut keep: impl FnMut(&McpServerConfig) -> bool) -> usize {
        let before = self.servers.len();
        self.servers.retain(|entry| keep(&entry.config));
        before - self.servers.len()
    }

    pub fn configs(&self) -> Vec<McpServerConfig> {
        self.servers
            .iter()
            .map(|entry| entry.config.clone())
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<&LoadedMcpServer> {
        self.servers.iter().find(|entry| entry.config.name == name)
    }
}

/// Read and merge every [`PROJECT_SETTINGS_FILES`] entry under `root`.
///
/// File order is precedence order: the first file to define a name wins and
/// later definitions are recorded as [`McpConfigIssue::Shadowed`]. The
/// [`MAX_MCP_SERVERS`] cap applies to the merged result, once.
pub fn load_project_mcp(root: &Path) -> McpProjectConfig {
    let mut loaded = McpProjectConfig::default();
    let mut claimed: BTreeSet<String> = BTreeSet::new();
    // The file whose rejected entry holds each name that no server has.
    let mut held_by: std::collections::BTreeMap<String, String> = Default::default();
    for file_name in PROJECT_SETTINGS_FILES {
        let text = match std::fs::read_to_string(root.join(file_name)) {
            Ok(text) => text,
            // Absent is the ordinary case. Anything else — no permission, a
            // directory in its place, an I/O error — is a settings file the
            // project *has* whose contents could not be read, which must not
            // be silently indistinguishable from not having one.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                loaded.files_present.push(file_name.to_owned());
                loaded.rejections.push(McpConfigRejection {
                    name: McpConfigRejection::WHOLE_FILE.to_owned(),
                    file: file_name.to_owned(),
                    issue: McpConfigIssue::FileUnreadable {
                        detail: label(&err.to_string()),
                    },
                });
                continue;
            }
        };
        loaded.files_present.push(file_name.to_owned());
        let value = match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(value) => value,
            Err(err) => {
                loaded.rejections.push(McpConfigRejection {
                    name: McpConfigRejection::WHOLE_FILE.to_owned(),
                    file: file_name.to_owned(),
                    issue: McpConfigIssue::FileUnreadable {
                        detail: label(&err.to_string()),
                    },
                });
                continue;
            }
        };
        let mut withheld = Vec::new();
        let accepted = parse_file(&value, file_name, &mut loaded.rejections, &mut withheld);
        // A name whose entry was rejected but asks for fewer tools or its own
        // limits is still this file's: a plain entry of the same name in a
        // later file would run without them, which is the opposite of what the
        // first file asked for.
        for name in withheld {
            if claimed.insert(name.clone()) {
                held_by.insert(name, file_name.to_owned());
            }
        }
        for entry in accepted {
            let name = entry.config.name.clone();
            if claimed.contains(&name) {
                let issue = match (
                    loaded
                        .servers
                        .iter()
                        .find(|server| server.config.name == name),
                    held_by.get(&name),
                ) {
                    (Some(server), _) => McpConfigIssue::Shadowed {
                        by_file: server.file.clone(),
                    },
                    (None, Some(by_file)) => McpConfigIssue::HeldByRejected {
                        by_file: by_file.clone(),
                    },
                    (None, None) => McpConfigIssue::Shadowed {
                        by_file: "an earlier settings file".to_owned(),
                    },
                };
                loaded.rejections.push(McpConfigRejection {
                    name: label(&name),
                    file: file_name.to_owned(),
                    issue,
                });
                continue;
            }
            if loaded.servers.len() >= MAX_MCP_SERVERS {
                loaded.rejections.push(McpConfigRejection {
                    name: label(&name),
                    file: file_name.to_owned(),
                    issue: McpConfigIssue::OverCapacity {
                        limit: MAX_MCP_SERVERS,
                    },
                });
                continue;
            }
            claimed.insert(name);
            loaded.servers.push(entry);
        }
    }
    loaded
}

/// Parse one settings document's `mcpServers` map. Per-entry validation only:
/// cross-file concerns (shadowing, the project-wide cap) belong to
/// [`load_project_mcp`], which is the only place that can see them.
fn parse_file(
    value: &serde_json::Value,
    file_name: &str,
    rejected: &mut RejectionLog,
    withheld: &mut Vec<String>,
) -> Vec<LoadedMcpServer> {
    let mut accepted = Vec::new();
    let Some(raw) = value.get("mcpServers") else {
        return accepted;
    };
    let Some(servers) = raw.as_object() else {
        rejected.push(McpConfigRejection {
            name: McpConfigRejection::WHOLE_FILE.to_owned(),
            file: file_name.to_owned(),
            issue: McpConfigIssue::NotAnObject,
        });
        return accepted;
    };
    for (name, spec) in servers {
        match parse_entry(name, spec) {
            Ok(config) => accepted.push(LoadedMcpServer {
                config,
                file: file_name.to_owned(),
            }),
            // The key is straight out of a project file: bound and flatten
            // it before it can become part of a report line.
            Err(issue) => {
                // An entry that asks for fewer tools or its own limits keeps
                // its name even when it is rejected, whatever the reason: a
                // plain entry of the same name in a later file would run
                // without what this one asked for.
                if spec
                    .as_object()
                    .is_some_and(|entry| NARROWING_KEYS.iter().any(|key| entry.contains_key(*key)))
                {
                    withheld.push(name.clone());
                }
                rejected.push(McpConfigRejection {
                    name: label(name),
                    file: file_name.to_owned(),
                    issue,
                });
            }
        }
    }
    accepted
}

/// The keys of an entry that restrict what a server may do or how long it may
/// take: an entry that has one and is rejected still holds its name.
const NARROWING_KEYS: [&str; 6] = [
    "enabled_tools",
    "disabled_tools",
    "startup_timeout_ms",
    "tool_timeout_ms",
    "deferred",
    // `auth: false` refuses authorization: a later file's entry of the same
    // name does not get to authorize in its place.
    "auth",
];

/// Validate one `mcpServers` entry into a spawn description.
pub fn parse_entry(
    name: &str,
    spec: &serde_json::Value,
) -> Result<McpServerConfig, McpConfigIssue> {
    validate_server_name(name)?;
    let Some(spec) = spec.as_object() else {
        return Err(McpConfigIssue::EntryNotAnObject);
    };
    // Streamable-HTTP remote entry (`{"type": "http", "url": …, "headers":
    // {…}}`): a transport this build now speaks. The URL host is the
    // egress-allowlist key; headers are the auth/tenancy boundary and are
    // attached verbatim at the io layer — never logged, never inherited.
    let mut http_endpoint: Option<crate::exec_tools::McpHttpEndpoint> = None;
    let http_kind = spec
        .get("type")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let command: String = match spec.get("command").and_then(serde_json::Value::as_str) {
        Some(command) => command.to_owned(),
        None if http_kind.as_deref() == Some("http") => {
            let url = spec
                .get("url")
                .and_then(serde_json::Value::as_str)
                .ok_or(McpConfigIssue::MissingUrl)?;
            if url.is_empty() || url.len() > MAX_URL_BYTES {
                return Err(McpConfigIssue::UrlTooLong { bytes: url.len() });
            }
            let mut headers = Vec::new();
            if let Some(map) = spec.get("headers").and_then(serde_json::Value::as_object) {
                if map.len() > MAX_HTTP_HEADERS {
                    return Err(McpConfigIssue::TooManyHttpHeaders { count: map.len() });
                }
                for (key, value) in map {
                    let Some(value) = value.as_str() else {
                        return Err(McpConfigIssue::HttpHeaderNotAString { key: label(key) });
                    };
                    if key.len() > MAX_HTTP_HEADER_KEY_BYTES
                        || value.len() > MAX_HTTP_HEADER_VALUE_BYTES
                    {
                        return Err(McpConfigIssue::HttpHeaderTooLong { key: label(key) });
                    }
                    headers.push((key.clone(), value.to_owned()));
                }
            }
            let mut headers_env = Vec::new();
            match spec.get("headers_env") {
                None => {}
                Some(serde_json::Value::Object(map)) => {
                    if headers.len() + map.len() > MAX_HTTP_HEADERS {
                        return Err(McpConfigIssue::TooManyHttpHeaders {
                            count: headers.len() + map.len(),
                        });
                    }
                    for (key, value) in map {
                        let valid = key.len() <= MAX_HTTP_HEADER_KEY_BYTES
                            && is_header_name(key)
                            && value.as_str().is_some_and(is_bearer_env_name)
                            && !headers.iter().any(|(set, _)| set.eq_ignore_ascii_case(key))
                            && !headers_env
                                .iter()
                                .any(|(set, _): &(String, String)| set.eq_ignore_ascii_case(key));
                        if !valid {
                            return Err(McpConfigIssue::HeaderEnvInvalid { key: label(key) });
                        }
                        headers_env
                            .push((key.clone(), value.as_str().unwrap_or_default().to_owned()));
                    }
                }
                Some(_) => {
                    return Err(McpConfigIssue::HeaderEnvInvalid {
                        key: "headers_env".to_owned(),
                    });
                }
            }
            let bearer_token_env = match spec.get("bearer_token_env") {
                None => None,
                Some(serde_json::Value::String(name)) if is_bearer_env_name(name) => {
                    Some(name.clone())
                }
                Some(_) => return Err(McpConfigIssue::BearerEnvInvalid),
            };
            let has_credential = bearer_token_env.is_some()
                || !headers_env.is_empty()
                || headers.iter().any(|(key, _)| {
                    let key = key.to_ascii_lowercase();
                    key == "authorization" || key.contains("key") || key.contains("token")
                });
            if has_credential && !url_is_encrypted_or_local(url) {
                return Err(McpConfigIssue::PlainHttpCredential);
            }
            let sets_authorization = headers
                .iter()
                .chain(headers_env.iter())
                .any(|(key, _)| key.eq_ignore_ascii_case("authorization"));
            if bearer_token_env.is_some() && sets_authorization {
                return Err(McpConfigIssue::CredentialConflict);
            }
            let auth = parse_auth(spec.get("auth"))?;
            if matches!(auth, crate::exec_tools::McpAuthMode::Settings(_)) {
                let is_static =
                    bearer_token_env.is_some() || !headers_env.is_empty() || sets_authorization;
                if is_static {
                    return Err(McpConfigIssue::AuthWithStaticCredential);
                }
                // Tokens the flow gets are credentials too: they do not cross
                // an unencrypted network.
                if !url_is_encrypted_or_local(url) {
                    return Err(McpConfigIssue::PlainHttpCredential);
                }
            }
            http_endpoint = Some(crate::exec_tools::McpHttpEndpoint {
                url: url.to_owned(),
                headers,
                headers_env,
                bearer_token_env,
                auth,
            });
            url.to_owned()
        }
        None => {
            // A remote entry with an unknown type is still "this shape is
            // not stdio"; name the real kind.
            let kind = http_kind.or_else(|| spec.get("url").map(|_| "url".to_owned()));
            return Err(match kind {
                Some(kind) if kind != "stdio" => {
                    McpConfigIssue::RemoteTransport { kind: label(&kind) }
                }
                _ => McpConfigIssue::MissingCommand,
            });
        }
    };
    if http_endpoint.is_none() && spec.contains_key("auth") {
        return Err(McpConfigIssue::AuthOnStdio);
    }
    if command.is_empty() {
        return Err(McpConfigIssue::CommandEmpty);
    }
    if command.len() > MAX_COMMAND_BYTES {
        return Err(McpConfigIssue::CommandTooLong {
            bytes: command.len(),
        });
    }
    let args = match spec.get("args") {
        None => Vec::new(),
        Some(serde_json::Value::Array(entries)) => {
            if entries.len() > MAX_ARGS {
                return Err(McpConfigIssue::TooManyArgs {
                    count: entries.len(),
                });
            }
            let mut args = Vec::with_capacity(entries.len());
            for (index, entry) in entries.iter().enumerate() {
                let Some(arg) = entry.as_str() else {
                    return Err(McpConfigIssue::ArgNotAString { index });
                };
                if arg.len() > MAX_ARG_BYTES {
                    return Err(McpConfigIssue::ArgTooLong {
                        index,
                        bytes: arg.len(),
                    });
                }
                args.push(arg.to_owned());
            }
            args
        }
        Some(_) => return Err(McpConfigIssue::ArgsNotAnArray),
    };
    let env = match spec.get("env") {
        None => Vec::new(),
        Some(serde_json::Value::Object(entries)) => {
            if entries.len() > MAX_ENV_VARS {
                return Err(McpConfigIssue::TooManyEnvVars {
                    count: entries.len(),
                });
            }
            let mut env = Vec::with_capacity(entries.len());
            for (key, value) in entries {
                if !is_env_key(key) || key.len() > MAX_ENV_KEY_BYTES {
                    return Err(McpConfigIssue::EnvKeyInvalid { key: label(key) });
                }
                let Some(value) = value.as_str() else {
                    return Err(McpConfigIssue::EnvValueNotAString { key: label(key) });
                };
                if value.len() > MAX_ENV_VALUE_BYTES {
                    return Err(McpConfigIssue::EnvValueTooLong {
                        key: label(key),
                        bytes: value.len(),
                    });
                }
                env.push((key.clone(), value.to_owned()));
            }
            env
        }
        Some(_) => return Err(McpConfigIssue::EnvNotAnObject),
    };
    let tools = crate::exec_tools::McpToolFilter {
        enabled: parse_tool_list(spec, "enabled_tools")?,
        disabled: parse_tool_list(spec, "disabled_tools")?.unwrap_or_default(),
    };
    let timeouts = crate::mcp_limits::Requested {
        startup_ms: parse_timeout(
            spec,
            "startup_timeout_ms",
            &crate::mcp_limits::STARTUP_RANGE_MS,
        )?,
        tool_ms: parse_timeout(spec, "tool_timeout_ms", &crate::mcp_limits::TOOL_RANGE_MS)?,
    };
    let deferred = match spec.get("deferred") {
        None => None,
        Some(serde_json::Value::Bool(deferred)) => Some(*deferred),
        Some(_) => return Err(McpConfigIssue::DeferredInvalid),
    };
    Ok(McpServerConfig {
        name: name.to_owned(),
        command,
        args,
        env,
        http: http_endpoint,
        tools,
        timeouts,
        deferred,
    })
}

/// `startup_timeout_ms` / `tool_timeout_ms`: absent is `None`; present is a
/// whole number of milliseconds within `range`. Anything else rejects the
/// entry: a limit that silently did not apply would leave the server on a
/// bound the person did not choose.
fn parse_timeout(
    spec: &serde_json::Map<String, serde_json::Value>,
    key: &'static str,
    range: &std::ops::RangeInclusive<u64>,
) -> Result<Option<u64>, McpConfigIssue> {
    let Some(value) = spec.get(key) else {
        return Ok(None);
    };
    check_timeout(key, value.as_u64(), range).map(Some)
}

/// A timeout already read as a number, held to its bounds: the loader's, and
/// `rapid mcp add`'s before it writes anything.
pub fn check_timeout(
    key: &'static str,
    value: Option<u64>,
    range: &std::ops::RangeInclusive<u64>,
) -> Result<u64, McpConfigIssue> {
    value
        .filter(|ms| range.contains(ms))
        .ok_or(McpConfigIssue::TimeoutInvalid {
            key,
            min: *range.start(),
            max: *range.end(),
        })
}

/// `enabled_tools` / `disabled_tools`: absent is `None`; present is a list of
/// at most [`MAX_TOOL_PATTERNS`] patterns of 1–[`MAX_TOOL_PATTERN_BYTES`] bytes
/// drawn from the characters of a tool name and `*` / `?`. A value that is
/// anything else rejects the entry: a filter that silently did not apply
/// would offer the model tools the person meant to hide.
fn parse_tool_list(
    spec: &serde_json::Map<String, serde_json::Value>,
    key: &'static str,
) -> Result<Option<Vec<String>>, McpConfigIssue> {
    let Some(value) = spec.get(key) else {
        return Ok(None);
    };
    let invalid = |reason| McpConfigIssue::ToolListInvalid { key, reason };
    let Some(items) = value.as_array() else {
        return Err(invalid(ToolListFault::NotAList));
    };
    if items.len() > MAX_TOOL_PATTERNS {
        return Err(invalid(ToolListFault::TooMany { count: items.len() }));
    }
    let mut patterns = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let Some(pattern) = item.as_str() else {
            return Err(invalid(ToolListFault::NotAString { index }));
        };
        patterns.push(pattern.to_owned());
    }
    validate_tool_patterns(key, &patterns)?;
    Ok(Some(patterns))
}

/// The rules a tool list obeys, applied to patterns already read as strings:
/// the loader's, and `rapid mcp add`'s before it writes anything.
pub fn validate_tool_patterns(
    key: &'static str,
    patterns: &[String],
) -> Result<(), McpConfigIssue> {
    let invalid = |reason| McpConfigIssue::ToolListInvalid { key, reason };
    if patterns.len() > MAX_TOOL_PATTERNS {
        return Err(invalid(ToolListFault::TooMany {
            count: patterns.len(),
        }));
    }
    for (index, pattern) in patterns.iter().enumerate() {
        if pattern.is_empty() {
            return Err(invalid(ToolListFault::Empty { index }));
        }
        if pattern.len() > MAX_TOOL_PATTERN_BYTES {
            return Err(invalid(ToolListFault::TooLong {
                index,
                bytes: pattern.len(),
            }));
        }
        if !pattern
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-' | '*' | '?'))
        {
            return Err(invalid(ToolListFault::Charset { index }));
        }
    }
    Ok(())
}

/// The name rules, in one place so `rapid mcp add` rejects at write time
/// exactly what the loader would reject at read time.
pub fn validate_server_name(name: &str) -> Result<(), McpConfigIssue> {
    if name.is_empty() {
        return Err(McpConfigIssue::NameEmpty);
    }
    if name.len() > MAX_SERVER_NAME_BYTES {
        return Err(McpConfigIssue::NameTooLong { bytes: name.len() });
    }
    // Exactly `agent_runtime::turn::valid_ident`'s alphabet, which is what
    // the composed `mcp__<server>__<tool>` name has to satisfy on the way
    // back in. Deliberately not narrower: the previous parser validated only
    // length, so a dotted or namespaced name (common in
    // `.claude/settings.json`, which this loader reads for compatibility)
    // did work, and tightening past what the runtime actually requires would
    // break those projects for no gain.
    if !name.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' || byte == b'.' || byte == b':'
    }) {
        return Err(McpConfigIssue::NameCharset);
    }
    if name.contains("__") {
        return Err(McpConfigIssue::NameReservedSeparator);
    }
    Ok(())
}

/// Whether a remote `url` is `https`, or plain `http` to a host on this machine
/// or network, so a credential sent to it is not exposed on the way.
pub fn url_is_encrypted_or_local(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    if scheme.eq_ignore_ascii_case("https") {
        return true;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority
        .rsplit_once(':')
        .map_or(authority, |(host, port)| {
            if port.bytes().all(|byte| byte.is_ascii_digit()) {
                host
            } else {
                authority
            }
        });
    scheme.eq_ignore_ascii_case("http") && crate::provider_egress::host_is_local_literal(host)
}

/// `url` for a report line: the query and fragment can carry a token, so
/// they are shown as `?…` instead of verbatim.
pub fn display_url(url: &str) -> String {
    match url.find(['?', '#']) {
        Some(at) => format!("{}?…", &url[..at]),
        None => url.to_owned(),
    }
}

/// A legal HTTP header field name, the subset the writer accepts.
/// The bounds on what a project file may say about the flow (EXT-02 §4.7).
pub const MAX_AUTH_CLIENT_ID_BYTES: usize = 256;
pub const MAX_AUTH_SCOPES: usize = 16;
pub const MAX_AUTH_SCOPE_BYTES: usize = 64;

/// The `auth` key of a remote entry. Absent engages the flow, `false` never
/// does, an object carries the four keys a project file may set and nothing
/// else — no endpoint, no issuer, no redirect host and no resource.
fn parse_auth(
    value: Option<&serde_json::Value>,
) -> Result<crate::exec_tools::McpAuthMode, McpConfigIssue> {
    use crate::exec_tools::{McpAuthMode, McpAuthSettings};
    let invalid = |reason| McpConfigIssue::AuthInvalid { reason };
    match value {
        None => Ok(McpAuthMode::Engage),
        Some(serde_json::Value::Bool(false)) => Ok(McpAuthMode::Never),
        Some(serde_json::Value::Object(map)) => {
            let mut settings = McpAuthSettings::default();
            for (key, value) in map {
                match key.as_str() {
                    "client_id" => {
                        let id = value
                            .as_str()
                            .filter(|id| {
                                !id.is_empty()
                                    && id.len() <= MAX_AUTH_CLIENT_ID_BYTES
                                    && id.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
                            })
                            .ok_or_else(|| invalid("client_id: 1-256 printable characters"))?;
                        settings.client_id = Some(id.to_owned());
                    }
                    "client_secret_env" => {
                        let name = value
                            .as_str()
                            .filter(|name| is_bearer_env_name(name))
                            .ok_or_else(|| {
                                invalid(
                                    "client_secret_env: an upper-case environment variable name",
                                )
                            })?;
                        settings.client_secret_env = Some(name.to_owned());
                    }
                    "scopes" => {
                        let list = value
                            .as_array()
                            .filter(|list| list.len() <= MAX_AUTH_SCOPES)
                            .ok_or_else(|| invalid("scopes: a list of at most 16"))?;
                        for scope in list {
                            let scope = scope
                                .as_str()
                                .filter(|scope| {
                                    !scope.is_empty()
                                        && scope.len() <= MAX_AUTH_SCOPE_BYTES
                                        && scope.bytes().all(|byte| {
                                            (0x21..=0x7e).contains(&byte)
                                                && byte != b'"'
                                                && byte != b'\\'
                                        })
                                })
                                .ok_or_else(|| {
                                    invalid("scopes: 1-64 printable characters, no quote or space")
                                })?;
                            settings.scopes.push(scope.to_owned());
                        }
                    }
                    "callback_port" => {
                        let port = value
                            .as_u64()
                            .and_then(|port| u16::try_from(port).ok())
                            .filter(|port| *port >= 1024)
                            .ok_or_else(|| invalid("callback_port: 1024-65535"))?;
                        settings.callback_port = Some(port);
                    }
                    _ => return Err(invalid("an unknown key")),
                }
            }
            Ok(McpAuthMode::Settings(settings))
        }
        Some(_) => Err(invalid("neither false nor an object")),
    }
}

pub fn is_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// The name an entry's `bearer_token_env` may take: upper-case, so a typo like
/// `path` or a lower-case word cannot silently name an unrelated variable.
pub fn is_bearer_env_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.as_bytes()[0].is_ascii_digit()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

/// POSIX-shaped environment variable name: `[A-Za-z_][A-Za-z0-9_]*`.
pub fn is_env_key(key: &str) -> bool {
    let mut bytes = key.bytes();
    match bytes.next() {
        Some(first) if first.is_ascii_alphabetic() || first == b'_' => {}
        _ => return false,
    }
    bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    struct TempProject(PathBuf);

    impl TempProject {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "rapidlm-mcpcfg-{name}-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join(".rapidlm")).expect("rapidlm dir");
            std::fs::create_dir_all(dir.join(".claude")).expect("claude dir");
            Self(dir)
        }

        fn write(&self, file: &str, body: &str) {
            std::fs::write(self.0.join(file), body).expect("write settings");
        }
    }

    impl Drop for TempProject {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn parse_one(json: &str) -> Result<McpServerConfig, McpConfigIssue> {
        let value: serde_json::Value = serde_json::from_str(json).expect("json");
        parse_entry("srv", &value)
    }

    #[test]
    fn a_minimal_entry_parses_to_a_spawn_description() {
        let config = parse_one(r#"{"command": "npx", "args": ["-y", "pkg"]}"#).expect("parse");
        assert_eq!(config.name, "srv");
        assert_eq!(config.command, "npx");
        assert_eq!(config.args, vec!["-y".to_owned(), "pkg".to_owned()]);
        assert!(config.env.is_empty());
    }

    #[test]
    fn env_is_parsed_rather_than_ignored() {
        // The whole point of the `env` field: `register_mcp_servers` calls
        // `env_clear()`, so before this a server needing an API key in its
        // environment could not be configured at all.
        let config = parse_one(r#"{"command": "srv", "env": {"API_KEY": "k", "REGION": "eu"}}"#)
            .expect("parse");
        assert_eq!(
            config.env,
            vec![
                ("API_KEY".to_owned(), "k".to_owned()),
                ("REGION".to_owned(), "eu".to_owned()),
            ]
        );
    }

    #[test]
    fn a_name_containing_the_wire_separator_is_rejected_not_silently_broken() {
        // `execute_mcp_tool` splits `mcp__a__b__tool` on the FIRST `__`, so a
        // server named `a__b` would resolve as server `a`, tool `b__tool` —
        // every one of its tools permanently unroutable, with no diagnostic.
        assert_eq!(
            validate_server_name("a__b"),
            Err(McpConfigIssue::NameReservedSeparator)
        );
        assert!(validate_server_name("a_b-c9").is_ok());
    }

    #[test]
    fn a_name_that_could_not_appear_in_a_tool_name_is_rejected() {
        assert_eq!(validate_server_name(""), Err(McpConfigIssue::NameEmpty));
        assert_eq!(
            validate_server_name("has space"),
            Err(McpConfigIssue::NameCharset)
        );
        assert_eq!(
            validate_server_name("slash/es"),
            Err(McpConfigIssue::NameCharset)
        );
        // Deliberately still accepted: the previous parser validated only
        // length, so a dotted or namespaced name did work, and it is inside
        // `agent_runtime::turn::valid_ident`'s alphabet. Rejecting it would
        // be a compatibility break with no gain.
        assert!(validate_server_name("my.server").is_ok());
        assert!(validate_server_name("vendor:tool-1").is_ok());
        let long = "x".repeat(MAX_SERVER_NAME_BYTES + 1);
        assert_eq!(
            validate_server_name(&long),
            Err(McpConfigIssue::NameTooLong {
                bytes: MAX_SERVER_NAME_BYTES + 1
            })
        );
    }

    #[test]
    fn http_entries_parse_and_unknown_remote_kinds_still_name_the_kind() {
        // The build now SPEAKS streamable HTTP: a `{type:"http", url:…}`
        // entry parses into the http endpoint (url + headers), and the
        // command field carries the url for display surfaces.
        let http = parse_one(
            r#"{"type": "http", "url": "https://example.com/mcp", "headers": {"Authorization": "Bearer tok"}}"#,
        )
        .expect("http parses");
        assert_eq!(http.command, "https://example.com/mcp");
        let endpoint = http.http.expect("http endpoint present");
        assert_eq!(endpoint.url, "https://example.com/mcp");
        assert_eq!(
            endpoint.headers,
            vec![("Authorization".to_owned(), "Bearer tok".to_owned())]
        );
        // A `url` WITHOUT the http type is still an unknown remote shape —
        // named by kind, not as a missing command.
        assert_eq!(
            parse_one(r#"{"url": "https://example.com/mcp"}"#),
            Err(McpConfigIssue::RemoteTransport {
                kind: "url".to_owned()
            })
        );
        // An explicit stdio entry that forgot its command is the ordinary
        // missing-key case.
        assert_eq!(
            parse_one(r#"{"type": "stdio"}"#),
            Err(McpConfigIssue::MissingCommand)
        );
        assert_eq!(parse_one("{}"), Err(McpConfigIssue::MissingCommand));
    }

    #[test]
    fn malformed_fields_are_rejected_with_the_field_named() {
        assert_eq!(
            parse_one(r#"{"command": "x", "args": "not-an-array"}"#),
            Err(McpConfigIssue::ArgsNotAnArray)
        );
        assert_eq!(
            parse_one(r#"{"command": "x", "args": ["ok", 7]}"#),
            Err(McpConfigIssue::ArgNotAString { index: 1 })
        );
        assert_eq!(
            parse_one(r#"{"command": "x", "env": []}"#),
            Err(McpConfigIssue::EnvNotAnObject)
        );
        assert_eq!(
            parse_one(r#"{"command": "x", "env": {"9BAD": "v"}}"#),
            Err(McpConfigIssue::EnvKeyInvalid {
                key: "9BAD".to_owned()
            })
        );
        assert_eq!(
            parse_one(r#"{"command": "x", "env": {"OK": 1}}"#),
            Err(McpConfigIssue::EnvValueNotAString {
                key: "OK".to_owned()
            })
        );
        assert_eq!(
            parse_one(r#"{"command": ""}"#),
            Err(McpConfigIssue::CommandEmpty)
        );
        assert_eq!(parse_one("7"), Err(McpConfigIssue::EntryNotAnObject));
    }

    #[test]
    fn bounds_are_enforced_on_argv_and_env() {
        let args: Vec<String> = (0..=MAX_ARGS).map(|i| i.to_string()).collect();
        let json = serde_json::json!({"command": "x", "args": args});
        assert_eq!(
            parse_entry("srv", &json),
            Err(McpConfigIssue::TooManyArgs {
                count: MAX_ARGS + 1
            })
        );
        let mut env = serde_json::Map::new();
        for index in 0..=MAX_ENV_VARS {
            env.insert(format!("K{index}"), serde_json::Value::String("v".into()));
        }
        let json = serde_json::json!({"command": "x", "env": env});
        assert_eq!(
            parse_entry("srv", &json),
            Err(McpConfigIssue::TooManyEnvVars {
                count: MAX_ENV_VARS + 1
            })
        );
    }

    #[test]
    fn the_server_cap_is_project_wide_not_per_file() {
        // The defect this replaces: the cap was applied inside a per-file
        // parse whose results were `extend`ed, so a project with both
        // settings files could register 2 * MAX_MCP_SERVERS children — and
        // *adding a settings file* silently doubled the ceiling.
        let project = TempProject::new("cap");
        let entries = |prefix: &str| {
            let mut map = serde_json::Map::new();
            for index in 0..MAX_MCP_SERVERS {
                map.insert(
                    format!("{prefix}{index}"),
                    serde_json::json!({"command": "true"}),
                );
            }
            serde_json::json!({"mcpServers": map}).to_string()
        };
        project.write(".rapidlm/settings.json", &entries("a"));
        project.write(".claude/settings.json", &entries("b"));

        let loaded = load_project_mcp(&project.0);
        assert_eq!(loaded.servers().len(), MAX_MCP_SERVERS);
        assert_eq!(loaded.rejections().len(), MAX_MCP_SERVERS);
        assert!(
            loaded.rejections().iter().all(|rejection| matches!(
                rejection.issue,
                McpConfigIssue::OverCapacity {
                    limit: MAX_MCP_SERVERS
                }
            )),
            "{:?}",
            loaded.rejections()
        );
        // Precedence: the first file's servers are the ones that survive.
        assert!(
            loaded
                .servers()
                .iter()
                .all(|entry| entry.file == ".rapidlm/settings.json")
        );
    }

    #[test]
    fn a_duplicate_name_in_a_second_file_is_shadowed_not_spawned_twice() {
        // Two entries of the same name each spawned a child, but
        // `execute_mcp_tool` resolves a server by `find` — so the second was
        // a live, unreachable process plus duplicate `mcp__x__*` names on the
        // model-facing surface.
        let project = TempProject::new("dupe");
        project.write(
            ".rapidlm/settings.json",
            r#"{"mcpServers": {"shared": {"command": "first"}}}"#,
        );
        project.write(
            ".claude/settings.json",
            r#"{"mcpServers": {"shared": {"command": "second"}}}"#,
        );

        let loaded = load_project_mcp(&project.0);
        assert_eq!(loaded.servers().len(), 1);
        assert_eq!(loaded.servers()[0].config.command, "first");
        assert_eq!(loaded.servers()[0].file, ".rapidlm/settings.json");
        assert_eq!(
            loaded.rejections(),
            &[McpConfigRejection {
                name: "shared".to_owned(),
                file: ".claude/settings.json".to_owned(),
                issue: McpConfigIssue::Shadowed {
                    by_file: ".rapidlm/settings.json".to_owned()
                },
            }]
        );
    }

    #[test]
    fn both_settings_files_contribute_distinct_servers() {
        let project = TempProject::new("merge");
        project.write(
            ".rapidlm/settings.json",
            r#"{"mcpServers": {"native": {"command": "a"}}}"#,
        );
        project.write(
            ".claude/settings.json",
            r#"{"mcpServers": {"compat": {"command": "b"}}}"#,
        );
        let loaded = load_project_mcp(&project.0);
        let names: Vec<&str> = loaded
            .servers()
            .iter()
            .map(|entry| entry.config.name.as_str())
            .collect();
        assert_eq!(names, vec!["native", "compat"]);
        assert!(loaded.rejections().is_empty());
        assert_eq!(
            loaded.files_present(),
            &[
                ".rapidlm/settings.json".to_owned(),
                ".claude/settings.json".to_owned()
            ]
        );
    }

    #[test]
    fn an_unparsable_settings_file_is_reported_and_the_other_still_loads() {
        // Long-standing precedent: a broken settings file never fails a run.
        // What changes is that it is no longer *silent*.
        let project = TempProject::new("broken");
        project.write(".rapidlm/settings.json", "{ this is not json");
        project.write(
            ".claude/settings.json",
            r#"{"mcpServers": {"ok": {"command": "a"}}}"#,
        );
        let loaded = load_project_mcp(&project.0);
        assert_eq!(loaded.servers().len(), 1);
        assert_eq!(loaded.servers()[0].config.name, "ok");
        assert_eq!(loaded.rejections().len(), 1);
        assert_eq!(loaded.rejections()[0].name, McpConfigRejection::WHOLE_FILE);
        assert!(matches!(
            loaded.rejections()[0].issue,
            McpConfigIssue::FileUnreadable { .. }
        ));
    }

    #[test]
    fn a_project_with_no_settings_yields_nothing_and_says_nothing() {
        let project = TempProject::new("empty");
        let loaded = load_project_mcp(&project.0);
        assert!(loaded.servers().is_empty());
        assert!(loaded.rejections().is_empty());
        assert!(loaded.files_present().is_empty());
    }

    #[test]
    fn mcp_servers_that_is_not_an_object_is_reported_once_for_the_file() {
        let project = TempProject::new("notobj");
        project.write(".rapidlm/settings.json", r#"{"mcpServers": ["a"]}"#);
        let loaded = load_project_mcp(&project.0);
        assert!(loaded.servers().is_empty());
        assert_eq!(loaded.rejections().len(), 1);
        assert_eq!(loaded.rejections()[0].issue, McpConfigIssue::NotAnObject);
    }

    #[test]
    fn a_settings_derived_string_cannot_forge_a_report_line() {
        // Every rejection reason is printed as one `reason=…` line by
        // `rapid mcp list`, one `warning: …` line by the turn that skipped
        // it, and inside `rapid doctor`'s mcp row. The `mcpServers` key and
        // the `type` value are both straight out of a project file, so a
        // newline in either would let a repository fabricate report lines.
        let project = TempProject::new("forge");
        project.write(
            ".rapidlm/settings.json",
            &serde_json::json!({"mcpServers": {
                "evil\nserver=fake file=nowhere command=totally-fine": {
                    "type": "http\nserver=also-fake"
                }
            }})
            .to_string(),
        );
        let loaded = load_project_mcp(&project.0);
        assert_eq!(loaded.rejections().len(), 1);
        let rejection = &loaded.rejections()[0];
        assert!(
            !rejection.name.contains('\n') && !rejection.name.contains('\r'),
            "name still spans lines: {:?}",
            rejection.name
        );
        let reason = rejection.issue.to_string();
        assert!(
            !reason.contains('\n') && !reason.contains('\r'),
            "reason still spans lines: {reason:?}"
        );
    }

    #[test]
    fn a_settings_derived_string_is_bounded() {
        let project = TempProject::new("huge");
        let huge = "z".repeat(50_000);
        project.write(
            ".rapidlm/settings.json",
            &serde_json::json!({"mcpServers": {"srv": {"type": huge}}}).to_string(),
        );
        let loaded = load_project_mcp(&project.0);
        let reason = loaded.rejections()[0].issue.to_string();
        assert!(
            reason.len() < 1024,
            "an unbounded settings value reached a report line ({} bytes)",
            reason.len()
        );
        assert!(
            reason.contains("..."),
            "truncation should be visible: {reason}"
        );
    }

    #[test]
    fn the_number_of_reported_rejections_is_bounded_and_the_rest_are_counted() {
        // Nothing bounds how many entries a settings file declares, and each
        // rejection becomes a stderr warning on *every* turn. A file with
        // thousands of malformed entries must not make every turn print
        // thousands of lines.
        let project = TempProject::new("manyrejects");
        let count = MAX_REPORTED_REJECTIONS * 3;
        let mut map = serde_json::Map::new();
        for index in 0..count {
            // No `command`: every one of these is rejected.
            map.insert(format!("srv{index:04}"), serde_json::json!({}));
        }
        project.write(
            ".rapidlm/settings.json",
            &serde_json::json!({"mcpServers": map}).to_string(),
        );
        let loaded = load_project_mcp(&project.0);
        assert!(loaded.servers().is_empty());
        assert_eq!(loaded.rejections().len(), MAX_REPORTED_REJECTIONS);
        assert_eq!(
            loaded.rejections_omitted(),
            count - MAX_REPORTED_REJECTIONS,
            "the rest must still be counted, not forgotten"
        );
        assert_eq!(loaded.rejections_total(), count);
    }

    #[test]
    fn every_issue_renders_a_non_empty_reason() {
        // The report is only useful if each variant says something; an empty
        // `reason=` line would be exactly the silence this module removes.
        for issue in [
            McpConfigIssue::FileUnreadable { detail: "x".into() },
            McpConfigIssue::NotAnObject,
            McpConfigIssue::EntryNotAnObject,
            McpConfigIssue::NameEmpty,
            McpConfigIssue::NameTooLong { bytes: 99 },
            McpConfigIssue::NameCharset,
            McpConfigIssue::NameReservedSeparator,
            McpConfigIssue::MissingCommand,
            McpConfigIssue::RemoteTransport { kind: "sse".into() },
            McpConfigIssue::CommandEmpty,
            McpConfigIssue::CommandTooLong { bytes: 99 },
            McpConfigIssue::ArgsNotAnArray,
            McpConfigIssue::ArgNotAString { index: 0 },
            McpConfigIssue::TooManyArgs { count: 99 },
            McpConfigIssue::ArgTooLong {
                index: 0,
                bytes: 99,
            },
            McpConfigIssue::EnvNotAnObject,
            McpConfigIssue::TooManyEnvVars { count: 99 },
            McpConfigIssue::EnvKeyInvalid { key: "9".into() },
            McpConfigIssue::EnvValueNotAString { key: "K".into() },
            McpConfigIssue::EnvValueTooLong {
                key: "K".into(),
                bytes: 99,
            },
            McpConfigIssue::Shadowed {
                by_file: "f".into(),
            },
            McpConfigIssue::HeldByRejected {
                by_file: "f".into(),
            },
            McpConfigIssue::OverCapacity { limit: 8 },
            McpConfigIssue::DeferredInvalid,
            McpConfigIssue::TimeoutInvalid {
                key: "tool_timeout_ms",
                min: 1_000,
                max: 600_000,
            },
            McpConfigIssue::ToolListInvalid {
                key: "enabled_tools",
                reason: ToolListFault::NotAList,
            },
            McpConfigIssue::ToolListInvalid {
                key: "disabled_tools",
                reason: ToolListFault::TooMany { count: 99 },
            },
            McpConfigIssue::ToolListInvalid {
                key: "enabled_tools",
                reason: ToolListFault::NotAString { index: 1 },
            },
            McpConfigIssue::ToolListInvalid {
                key: "enabled_tools",
                reason: ToolListFault::Empty { index: 1 },
            },
            McpConfigIssue::ToolListInvalid {
                key: "enabled_tools",
                reason: ToolListFault::TooLong {
                    index: 1,
                    bytes: 999,
                },
            },
            McpConfigIssue::ToolListInvalid {
                key: "enabled_tools",
                reason: ToolListFault::Charset { index: 1 },
            },
        ] {
            let rendered = issue.to_string();
            assert!(!rendered.trim().is_empty(), "{issue:?} rendered empty");
            assert!(
                !rendered.contains('\n'),
                "{issue:?} spans lines, which would break the one-per-line report"
            );
        }
    }

    #[test]
    fn an_env_value_is_never_part_of_an_issue_message() {
        // Issues are printed by `rapid mcp list`, by `rapid doctor`'s mcp
        // row, and on stderr by the turn that skipped the entry. A value is
        // very often a token, so only the *key* may appear — asserted by
        // parsing entries that really do carry a secret alongside the fault,
        // not by hand-building an issue that could not contain one.
        const SECRET: &str = "sk-live-must-never-be-rendered";
        for json in [
            // A bad key next to a good key holding the secret.
            format!(r#"{{"command": "x", "env": {{"API_KEY": "{SECRET}", "9BAD": "v"}}}}"#),
            // The secret's own key is the one at fault.
            format!(r#"{{"command": "x", "env": {{"9BAD": "{SECRET}"}}}}"#),
            // A non-string value beside the secret.
            format!(r#"{{"command": "x", "env": {{"API_KEY": "{SECRET}", "N": 1}}}}"#),
            // An over-long value: the *length* is reported, never the value.
            format!(
                r#"{{"command": "x", "env": {{"API_KEY": "{}"}}}}"#,
                "s".repeat(MAX_ENV_VALUE_BYTES + 1)
            ),
        ] {
            let issue = parse_one(&json).expect_err("entry should be rejected");
            let rendered = issue.to_string();
            assert!(
                !rendered.contains(SECRET) && !rendered.contains("ssss"),
                "issue rendered an env value: {rendered}"
            );
        }
    }

    #[test]
    fn env_key_shape_matches_posix() {
        assert!(is_env_key("A"));
        assert!(is_env_key("_a9"));
        assert!(!is_env_key(""));
        assert!(!is_env_key("9A"));
        assert!(!is_env_key("A-B"));
        assert!(!is_env_key("A B"));
    }

    fn stdio_with(extra: serde_json::Value) -> serde_json::Value {
        let mut entry = serde_json::json!({"command": "true"});
        for (key, value) in extra.as_object().expect("object") {
            entry[key] = value.clone();
        }
        entry
    }

    #[test]
    fn tool_lists_are_read_and_filter_by_the_servers_own_tool_name() {
        let config = parse_entry(
            "demo",
            &stdio_with(serde_json::json!({
                "enabled_tools": ["search_*", "get_?ssue", "create_issue"],
                "disabled_tools": ["get_audit_log", "search_private"],
            })),
        )
        .expect("parses");
        let filter = &config.tools;
        assert!(!filter.is_default());
        // `*` any run (even none), `?` exactly one, the rest literal, the
        // whole name, case-sensitive.
        for allowed in ["search_issues", "search_", "get_issue", "create_issue"] {
            assert!(filter.allows(allowed), "{allowed}");
        }
        for refused in [
            "get_audit_log",
            "search_private",
            "delete_issue",
            "get_issues",
            "Search_issues",
            "create_issue2",
            "xcreate_issue",
            "get_ssue",
            "",
        ] {
            assert!(!filter.allows(refused), "{refused:?}");
        }
        // No list at all is the default, and it allows everything.
        let plain = parse_entry("demo", &stdio_with(serde_json::json!({}))).expect("parses");
        assert!(plain.tools.is_default() && plain.tools.allows("anything"));
        // An empty allowlist allows nothing (it is not "absent").
        let none = parse_entry(
            "demo",
            &stdio_with(serde_json::json!({"enabled_tools": []})),
        )
        .expect("parses");
        assert!(!none.tools.allows("anything") && !none.tools.is_default());
        // A denylist alone removes from all.
        let only_off = parse_entry(
            "demo",
            &stdio_with(serde_json::json!({"disabled_tools": ["x*"]})),
        )
        .expect("parses");
        assert!(only_off.tools.allows("a") && !only_off.tools.allows("xyz"));
    }

    #[test]
    fn a_rejected_entry_that_refuses_authorization_still_holds_its_name() {
        // `auth: false` narrows: a later file's entry of the same name does not
        // get to authorize in its place.
        let project = TempProject::new("claim-auth");
        project.write(
            ".rapidlm/settings.json",
            r#"{"mcpServers": {
                 "tracker": {"type": "http", "url": "https://mcp.example.com/mcp",
                             "auth": false, "bearer_token_env": "lower"}
               }}"#,
        );
        project.write(
            ".claude/settings.json",
            r#"{"mcpServers": {
                 "tracker": {"type": "http", "url": "https://mcp.example.com/mcp"}
               }}"#,
        );
        let loaded = load_project_mcp(&project.0);
        assert!(loaded.servers().is_empty(), "{:?}", loaded.servers());
        assert!(
            loaded
                .rejections()
                .iter()
                .any(|r| matches!(r.issue, McpConfigIssue::HeldByRejected { .. })),
            "{:?}",
            loaded.rejections()
        );
    }

    #[test]
    fn a_rejected_tool_list_still_claims_its_name_so_a_later_file_cannot_run_it_unfiltered() {
        let project = TempProject::new("claim");
        project.write(
            ".rapidlm/settings.json",
            r#"{"mcpServers": {
                 "tracker": {"command": "true", "enabled_tools": ["a b"]},
                 "other": {"command": "true", "enabled_tools": "nope"}
               }}"#,
        );
        project.write(
            ".claude/settings.json",
            r#"{"mcpServers": {
                 "tracker": {"command": "true"},
                 "other": {"command": "true"},
                 "fresh": {"command": "true"}
               }}"#,
        );
        let loaded = load_project_mcp(&project.0);
        let names: Vec<&str> = loaded
            .servers()
            .iter()
            .map(|entry| entry.config.name.as_str())
            .collect();
        assert_eq!(names, ["fresh"], "neither plain twin runs");
        let held = loaded
            .rejections()
            .iter()
            .filter(|rejection| matches!(rejection.issue, McpConfigIssue::HeldByRejected { .. }))
            .count();
        assert_eq!(held, 2, "{:?}", loaded.rejections());
        // The claim does not depend on which fault got the entry rejected: one
        // that asks for a narrower tool set (or its own limits) and fails for
        // an earlier reason still holds its name, and says so.
        let project = TempProject::new("claim-any-fault");
        project.write(
            ".rapidlm/settings.json",
            r#"{"mcpServers": {
                 "tracker": {"command": "true", "args": "not-a-list", "enabled_tools": ["get_issue"]},
                 "timed": {"command": "", "tool_timeout_ms": 5000}
               }}"#,
        );
        project.write(
            ".claude/settings.json",
            r#"{"mcpServers": {"tracker": {"command": "true"}, "timed": {"command": "true"}}}"#,
        );
        let loaded = load_project_mcp(&project.0);
        assert!(loaded.servers().is_empty(), "neither plain twin runs");
        let held: Vec<String> = loaded
            .rejections()
            .iter()
            .filter(|r| matches!(r.issue, McpConfigIssue::HeldByRejected { .. }))
            .map(|r| r.issue.to_string())
            .collect();
        assert_eq!(held.len(), 2, "{:?}", loaded.rejections());
        assert!(
            held[0].starts_with(".rapidlm/settings.json has an entry of this name"),
            "the reason names the file that holds the name: {held:?}"
        );
        // An entry with none of those keys, rejected, leaves the name free.
        let project = TempProject::new("claim-other");
        project.write(
            ".rapidlm/settings.json",
            r#"{"mcpServers": {"tracker": {"command": ""}}}"#,
        );
        project.write(
            ".claude/settings.json",
            r#"{"mcpServers": {"tracker": {"command": "true"}}}"#,
        );
        assert_eq!(load_project_mcp(&project.0).servers().len(), 1);
    }

    #[test]
    fn the_timeout_keys_are_read_and_held_to_their_bounds() {
        let project = TempProject::new("timeouts");
        project.write(
            ".rapidlm/settings.json",
            r#"{"mcpServers": {
                 "plain": {"command": "true"},
                 "both": {"command": "true", "startup_timeout_ms": 5000, "tool_timeout_ms": 60000},
                 "low": {"command": "true", "startup_timeout_ms": 1000, "tool_timeout_ms": 1000},
                 "high": {"command": "true", "startup_timeout_ms": 120000, "tool_timeout_ms": 600000}
               }}"#,
        );
        let loaded = load_project_mcp(&project.0);
        let at = |name: &str| loaded.get(name).expect(name).config.timeouts;
        assert_eq!(at("plain"), crate::mcp_limits::Requested::default());
        assert_eq!(
            (at("both").startup_ms, at("both").tool_ms),
            (Some(5_000), Some(60_000))
        );
        assert_eq!(
            (at("low").startup_ms, at("low").tool_ms),
            (Some(1_000), Some(1_000))
        );
        assert_eq!(
            (at("high").startup_ms, at("high").tool_ms),
            (Some(120_000), Some(600_000))
        );
        assert!(loaded.rejections().is_empty(), "{:?}", loaded.rejections());
        // Anything else rejects the entry, with the key and the bounds in the reason.
        for (key, value) in [
            ("tool_timeout_ms", "999"),
            ("tool_timeout_ms", "600001"),
            ("startup_timeout_ms", "999"),
            ("startup_timeout_ms", "120001"),
            ("tool_timeout_ms", "0"),
            ("tool_timeout_ms", "-1"),
            ("tool_timeout_ms", "1.5"),
            ("tool_timeout_ms", "60000.0"),
            ("tool_timeout_ms", "\"60000\""),
            ("tool_timeout_ms", "null"),
            ("tool_timeout_ms", "true"),
            ("tool_timeout_ms", "[60000]"),
            ("startup_timeout_ms", "99999999999999999999"),
        ] {
            let project = TempProject::new("timeout-bad");
            project.write(
                ".rapidlm/settings.json",
                &format!(r#"{{"mcpServers": {{"x": {{"command": "true", "{key}": {value}}}}}}}"#),
            );
            let loaded = load_project_mcp(&project.0);
            assert!(
                loaded.get("x").is_none(),
                "{key}={value} must reject the entry"
            );
            let reason = loaded.rejections()[0].issue.to_string();
            assert!(
                reason.contains(key) && reason.contains("whole number of milliseconds"),
                "{key}={value}: {reason}"
            );
        }
        assert_eq!(
            McpConfigIssue::TimeoutInvalid {
                key: "startup_timeout_ms",
                min: 1_000,
                max: 120_000
            }
            .to_string(),
            "`startup_timeout_ms` must be a whole number of milliseconds from 1000 to 120000"
        );
    }

    #[test]
    fn the_deferred_key_is_a_boolean_and_nothing_else() {
        let project = TempProject::new("deferred");
        project.write(
            ".rapidlm/settings.json",
            r#"{"mcpServers": {
                 "plain": {"command": "true"},
                 "yes": {"command": "true", "deferred": true},
                 "no": {"command": "true", "deferred": false},
                 "remote": {"type": "http", "url": "https://example.com/mcp", "deferred": true}
               }}"#,
        );
        let loaded = load_project_mcp(&project.0);
        let at = |name: &str| loaded.get(name).expect(name).config.deferred;
        assert_eq!(
            (at("plain"), at("yes"), at("no"), at("remote")),
            (None, Some(true), Some(false), Some(true))
        );
        for bad in ["\"true\"", "1", "0", "null", "[true]", "{}"] {
            let project = TempProject::new("deferred-bad");
            project.write(
                ".rapidlm/settings.json",
                &format!(r#"{{"mcpServers": {{"x": {{"command": "true", "deferred": {bad}}}}}}}"#),
            );
            let loaded = load_project_mcp(&project.0);
            assert!(
                loaded.get("x").is_none(),
                "deferred={bad} must reject the entry"
            );
            assert_eq!(
                loaded.rejections()[0].issue.to_string(),
                "`deferred` must be true or false",
                "{bad}"
            );
        }
        // A rejected entry that asks to be deferred keeps its name.
        let project = TempProject::new("deferred-claim");
        project.write(
            ".rapidlm/settings.json",
            r#"{"mcpServers": {"x": {"command": "", "deferred": true}}}"#,
        );
        project.write(
            ".claude/settings.json",
            r#"{"mcpServers": {"x": {"command": "true"}}}"#,
        );
        assert!(load_project_mcp(&project.0).get("x").is_none());
    }

    #[test]
    fn a_remote_entry_takes_the_timeout_keys_too() {
        let project = TempProject::new("timeouts-remote");
        project.write(
            ".rapidlm/settings.json",
            r#"{"mcpServers": {"r": {"type": "http", "url": "https://example.com/mcp", "tool_timeout_ms": 45000}}}"#,
        );
        let loaded = load_project_mcp(&project.0);
        assert_eq!(
            loaded.get("r").expect("r").config.timeouts.tool_ms,
            Some(45_000)
        );
    }

    #[test]
    fn a_rejected_timeout_still_claims_its_name() {
        let project = TempProject::new("timeout-claim");
        project.write(
            ".rapidlm/settings.json",
            r#"{"mcpServers": {"tracker": {"command": "true", "tool_timeout_ms": 5}}}"#,
        );
        project.write(
            ".claude/settings.json",
            r#"{"mcpServers": {"tracker": {"command": "true"}}}"#,
        );
        let loaded = load_project_mcp(&project.0);
        assert!(
            loaded.get("tracker").is_none(),
            "the plain twin must not run"
        );
        assert!(
            loaded
                .rejections()
                .iter()
                .any(|r| matches!(r.issue, McpConfigIssue::HeldByRejected { .. })),
            "{:?}",
            loaded.rejections()
        );
    }

    #[test]
    fn a_pattern_that_matches_nothing_the_server_reports_is_named() {
        let filter = crate::exec_tools::McpToolFilter {
            enabled: Some(vec!["creat_issue".to_owned(), "get_*".to_owned()]),
            disabled: vec!["get_x".to_owned(), "nope".to_owned()],
        };
        assert_eq!(
            filter.matching_nothing(&["create_issue", "get_issue", "get_x"]),
            ["creat_issue", "nope"]
        );
        assert!(
            crate::exec_tools::McpToolFilter::default()
                .matching_nothing(&["a"])
                .is_empty()
        );
    }

    #[test]
    fn a_bad_tool_list_rejects_the_entry_and_says_which_pattern_and_why() {
        let issue = |extra: serde_json::Value| {
            parse_entry("demo", &stdio_with(extra))
                .expect_err("rejected")
                .to_string()
        };
        assert_eq!(
            issue(serde_json::json!({"enabled_tools": "search_*"})),
            "`enabled_tools` is not a list of tool name patterns"
        );
        assert_eq!(
            issue(serde_json::json!({"disabled_tools": [1]})),
            "`disabled_tools` [0] is not a string"
        );
        assert_eq!(
            issue(serde_json::json!({"enabled_tools": ["ok", ""]})),
            "`enabled_tools` [1] is empty"
        );
        assert_eq!(
            issue(serde_json::json!({"enabled_tools": ["a b"]})),
            "`enabled_tools` [0] may only hold letters, digits, '.', '_', ':', '-', '*' and '?'"
        );
        for bad in ["a/b", "a$", "a\u{e9}", "a\nb", "{a}", "a|b", "[a]"] {
            assert!(
                issue(serde_json::json!({"disabled_tools": [bad]})).contains("may only hold"),
                "{bad:?}"
            );
        }
        // The bounds: 64 patterns of 128 bytes pass, one more of either does not.
        let longest = "a".repeat(MAX_TOOL_PATTERN_BYTES);
        let many: Vec<String> = (0..MAX_TOOL_PATTERNS).map(|i| format!("t{i}")).collect();
        assert!(
            parse_entry(
                "demo",
                &stdio_with(
                    serde_json::json!({"enabled_tools": many, "disabled_tools": [longest]})
                )
            )
            .is_ok()
        );
        let mut too_many = many.clone();
        too_many.push("x".to_owned());
        assert_eq!(
            issue(serde_json::json!({"enabled_tools": too_many})),
            "`enabled_tools` has 65 patterns, over the 64 limit"
        );
        assert_eq!(
            issue(serde_json::json!({"disabled_tools": ["a".repeat(129)]})),
            "`disabled_tools` [0] is 129 bytes, over the 128-byte limit"
        );
        // Unknown keys stay ignored; a known key with a bad value does not.
        assert!(parse_entry("demo", &stdio_with(serde_json::json!({"enabled_tool": 5}))).is_ok());
    }

    fn http_entry(extra: serde_json::Value) -> serde_json::Value {
        let mut entry = serde_json::json!({"type": "http", "url": "https://mcp.example.com/mcp"});
        for (key, value) in extra.as_object().expect("object") {
            entry[key] = value.clone();
        }
        entry
    }

    #[test]
    fn a_remote_entry_may_name_the_variables_that_hold_its_credentials() {
        let config = parse_entry(
            "remote",
            &http_entry(serde_json::json!({
                "bearer_token_env": "MCP_REMOTE_TOKEN",
                "headers": {"X-Tenant": "t1"},
                "headers_env": {"X-Api-Key": "MCP_REMOTE_KEY"},
            })),
        )
        .expect("loads");
        let http = config.http.expect("remote");
        assert_eq!(http.bearer_token_env.as_deref(), Some("MCP_REMOTE_TOKEN"));
        assert_eq!(
            http.headers_env,
            vec![("X-Api-Key".to_owned(), "MCP_REMOTE_KEY".to_owned())]
        );
        assert_eq!(http.headers, vec![("X-Tenant".to_owned(), "t1".to_owned())]);
    }

    #[test]
    fn a_bad_bearer_variable_name_is_rejected_by_kind() {
        for bad in [
            serde_json::json!("lower_case"),
            serde_json::json!("1LEADING"),
            serde_json::json!("HAS SPACE"),
            serde_json::json!(""),
            serde_json::json!("A".repeat(65)),
            serde_json::json!(7),
            serde_json::json!(["X"]),
        ] {
            let outcome = parse_entry(
                "remote",
                &http_entry(serde_json::json!({"bearer_token_env": bad})),
            );
            assert_eq!(outcome, Err(McpConfigIssue::BearerEnvInvalid), "{bad}");
        }
        assert!(
            parse_entry(
                "remote",
                &http_entry(serde_json::json!({"bearer_token_env": "A".repeat(64)}))
            )
            .is_ok(),
            "64 bytes is the limit, inclusive"
        );
    }

    #[test]
    fn the_auth_key_is_absent_false_or_an_object_of_four_bounded_keys() {
        use crate::exec_tools::{McpAuthMode, McpAuthSettings};
        let mode = |extra: serde_json::Value| {
            parse_entry("remote", &http_entry(extra))
                .map(|config| config.http.expect("remote").auth)
        };
        assert_eq!(mode(serde_json::json!({})), Ok(McpAuthMode::Engage));
        assert_eq!(
            mode(serde_json::json!({"auth": false})),
            Ok(McpAuthMode::Never)
        );
        assert_eq!(
            mode(serde_json::json!({"auth": {
                "client_id": "rapidlm-cli",
                "client_secret_env": "MCP_REMOTE_SECRET",
                "scopes": ["read", "repo:write"],
                "callback_port": 4821,
            }})),
            Ok(McpAuthMode::Settings(McpAuthSettings {
                client_id: Some("rapidlm-cli".to_owned()),
                client_secret_env: Some("MCP_REMOTE_SECRET".to_owned()),
                scopes: vec!["read".to_owned(), "repo:write".to_owned()],
                callback_port: Some(4821),
            }))
        );
        assert_eq!(
            mode(serde_json::json!({"auth": {}})),
            Ok(McpAuthMode::Settings(McpAuthSettings::default()))
        );
        let many: Vec<String> = (0..17).map(|n| format!("s{n}")).collect();
        for (auth, why) in [
            (serde_json::json!(true), "neither false nor an object"),
            (serde_json::json!("yes"), "neither false nor an object"),
            (
                serde_json::json!({"issuer": "https://evil.example"}),
                "an unknown key",
            ),
            (
                serde_json::json!({"authorization_endpoint": "x"}),
                "an unknown key",
            ),
            (
                serde_json::json!({"redirect_uri": "http://evil/"}),
                "an unknown key",
            ),
            (serde_json::json!({"client_id": ""}), "client_id"),
            (
                serde_json::json!({"client_id": "x".repeat(257)}),
                "client_id",
            ),
            (serde_json::json!({"client_id": "has space"}), "client_id"),
            (serde_json::json!({"client_id": 5}), "client_id"),
            (
                serde_json::json!({"client_secret_env": "lower"}),
                "client_secret_env",
            ),
            (
                serde_json::json!({"client_secret_env": 5}),
                "client_secret_env",
            ),
            (serde_json::json!({"scopes": many}), "scopes"),
            (serde_json::json!({"scopes": ["has space"]}), "scopes"),
            (serde_json::json!({"scopes": ["a\"b"]}), "scopes"),
            (serde_json::json!({"scopes": ["x".repeat(65)]}), "scopes"),
            (serde_json::json!({"scopes": "read"}), "scopes"),
            (serde_json::json!({"callback_port": 80}), "callback_port"),
            (serde_json::json!({"callback_port": 65536}), "callback_port"),
            (
                serde_json::json!({"callback_port": "4821"}),
                "callback_port",
            ),
        ] {
            let got = mode(serde_json::json!({ "auth": auth.clone() }));
            let Err(McpConfigIssue::AuthInvalid { reason }) = got else {
                panic!("{auth}: {got:?}");
            };
            assert!(reason.contains(why), "{auth}: {reason}");
        }
        // The port bound is inclusive.
        assert!(mode(serde_json::json!({"auth": {"callback_port": 1024}})).is_ok());
        assert!(mode(serde_json::json!({"auth": {"callback_port": 65535}})).is_ok());
    }

    #[test]
    fn auth_sits_beside_no_static_credential_and_only_on_a_remote_entry_over_a_safe_url() {
        // An object beside a static credential: the flow never engages for such
        // a server, so the two cannot both be meant.
        for extra in [
            serde_json::json!({"auth": {"scopes": ["a"]}, "bearer_token_env": "MCP_T"}),
            serde_json::json!({"auth": {}, "headers_env": {"X-Key": "MCP_K"}}),
            serde_json::json!({"auth": {}, "headers": {"Authorization": "Bearer x"}}),
        ] {
            assert_eq!(
                parse_entry("remote", &http_entry(extra.clone())),
                Err(McpConfigIssue::AuthWithStaticCredential),
                "{extra}"
            );
        }
        // `auth: false` beside one is redundant, not wrong.
        assert!(
            parse_entry(
                "remote",
                &http_entry(serde_json::json!({"auth": false, "bearer_token_env": "MCP_T"}))
            )
            .is_ok()
        );
        // Tokens do not cross an unencrypted network.
        assert_eq!(
            parse_entry(
                "remote",
                &serde_json::json!({
                    "type": "http", "url": "http://mcp.example.com/mcp", "auth": {},
                })
            ),
            Err(McpConfigIssue::PlainHttpCredential)
        );
        assert!(
            parse_entry(
                "remote",
                &serde_json::json!({
                    "type": "http", "url": "http://127.0.0.1:8080/mcp", "auth": {},
                })
            )
            .is_ok()
        );
        // A stdio server is not authorized by this build.
        assert_eq!(
            parse_entry(
                "local",
                &serde_json::json!({"command": "true", "auth": false})
            ),
            Err(McpConfigIssue::AuthOnStdio)
        );
        assert!(
            McpConfigIssue::AuthOnStdio.to_string().contains("stdio"),
            "the refusal says why"
        );
    }

    #[test]
    fn two_credentials_for_one_server_are_refused() {
        for extra in [
            serde_json::json!({
                "bearer_token_env": "TOKEN_A",
                "headers": {"Authorization": "Bearer literal"},
            }),
            serde_json::json!({
                "bearer_token_env": "TOKEN_A",
                "headers": {"authorization": "Bearer literal"},
            }),
            serde_json::json!({
                "bearer_token_env": "TOKEN_A",
                "headers_env": {"Authorization": "TOKEN_B"},
            }),
        ] {
            assert_eq!(
                parse_entry("remote", &http_entry(extra.clone())),
                Err(McpConfigIssue::CredentialConflict),
                "{extra}"
            );
        }
        // An Authorization header alone, or a bearer variable alone, is fine.
        assert!(
            parse_entry(
                "remote",
                &http_entry(serde_json::json!({"headers": {"Authorization": "Basic abc"}}))
            )
            .is_ok()
        );
    }

    #[test]
    fn headers_env_is_validated_entry_by_entry() {
        for (headers_env, case) in [
            (serde_json::json!({"Bad Name": "TOKEN"}), "header name"),
            (serde_json::json!({"X-A": "lower"}), "variable name"),
            (serde_json::json!({"X-A": 3}), "non-string"),
            (serde_json::json!(["X-A"]), "not an object"),
        ] {
            let outcome = parse_entry(
                "remote",
                &http_entry(serde_json::json!({"headers_env": headers_env})),
            );
            assert!(
                matches!(outcome, Err(McpConfigIssue::HeaderEnvInvalid { .. })),
                "{case}: {outcome:?}"
            );
        }
        let repeated = parse_entry(
            "remote",
            &http_entry(serde_json::json!({
                "headers": {"X-A": "1"},
                "headers_env": {"x-a": "TOKEN"},
            })),
        );
        assert!(
            matches!(repeated, Err(McpConfigIssue::HeaderEnvInvalid { .. })),
            "a header set twice is ambiguous: {repeated:?}"
        );
        let mut many = serde_json::Map::new();
        for index in 0..=MAX_HTTP_HEADERS {
            many.insert(format!("X-H{index}"), "TOKEN".into());
        }
        assert!(matches!(
            parse_entry(
                "remote",
                &http_entry(serde_json::json!({"headers_env": many}))
            ),
            Err(McpConfigIssue::TooManyHttpHeaders { .. })
        ));
    }

    #[test]
    fn an_unsupported_remote_kind_names_both_supported_transports() {
        let issue = parse_entry(
            "remote",
            &serde_json::json!({"type": "websocket", "url": "wss://x.example"}),
        )
        .expect_err("rejected");
        let text = issue.to_string();
        assert!(text.contains("websocket"), "{text}");
        assert!(
            text.contains("stdio") && text.contains("streamable HTTP"),
            "{text}"
        );
        assert!(!text.contains("stdio only"), "{text}");
    }

    #[test]
    fn a_credential_for_a_plain_http_url_off_this_network_is_refused() {
        let with = |url: &str, extra: serde_json::Value| {
            let mut entry = http_entry(extra);
            entry["url"] = url.into();
            parse_entry("remote", &entry)
        };
        // Refused: any credential source, remote plain http.
        for extra in [
            serde_json::json!({"bearer_token_env": "MCP_A"}),
            serde_json::json!({"headers_env": {"X-Api-Key": "MCP_A"}}),
            serde_json::json!({"headers": {"Authorization": "Basic abcd"}}),
            serde_json::json!({"headers": {"X-Api-Key": "abcd"}}),
            serde_json::json!({"headers": {"X-Auth-Token": "abcd"}}),
        ] {
            assert_eq!(
                with("http://mcp.example.com/mcp", extra.clone()),
                Err(McpConfigIssue::PlainHttpCredential),
                "{extra}"
            );
        }
        // Allowed: https, or plain http on this machine or network, or no credential at all.
        let bearer = serde_json::json!({"bearer_token_env": "MCP_A"});
        for url in [
            "https://mcp.example.com/mcp",
            "HTTPS://mcp.example.com/mcp",
            "http://127.0.0.1:8080/mcp",
            "http://localhost/mcp",
            "http://10.1.2.3/mcp",
        ] {
            assert!(with(url, bearer.clone()).is_ok(), "{url}");
        }
        assert!(
            with(
                "http://mcp.example.com/mcp",
                serde_json::json!({"headers": {"X-Tenant": "t1"}})
            )
            .is_ok()
        );
        assert!(with("http://mcp.example.com/mcp", serde_json::json!({})).is_ok());
        // The look-alikes are not local.
        for url in [
            "http://localhost.evil.example/mcp",
            "http://127.0.0.1.evil.example/mcp",
            "http://foo.localhost/mcp",
            "http://[::1]/mcp",
        ] {
            assert_eq!(
                with(url, bearer.clone()),
                Err(McpConfigIssue::PlainHttpCredential),
                "{url}"
            );
        }
    }

    #[test]
    fn a_report_shows_a_url_without_the_part_that_can_carry_a_token() {
        assert_eq!(
            display_url("https://h.example/mcp"),
            "https://h.example/mcp"
        );
        assert_eq!(
            display_url("https://h.example/mcp?key=SECRET"),
            "https://h.example/mcp?…"
        );
        assert_eq!(
            display_url("https://h.example/mcp#access_token=SECRET"),
            "https://h.example/mcp?…"
        );
        assert_eq!(
            display_url("https://h.example/m?a=1#b"),
            "https://h.example/m?…"
        );
    }
}
