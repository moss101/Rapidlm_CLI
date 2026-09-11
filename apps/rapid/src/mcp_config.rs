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
pub const MAX_ARGS: usize = 64;
pub const MAX_ARG_BYTES: usize = 4096;
pub const MAX_ENV_VARS: usize = 32;
pub const MAX_ENV_KEY_BYTES: usize = 128;
pub const MAX_ENV_VALUE_BYTES: usize = 4096;
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
    /// A server of this name was already admitted from an earlier settings
    /// file. First file wins, matching every other list-shaped setting's
    /// precedence.
    Shadowed {
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
            Self::RemoteTransport { kind } => write!(
                f,
                "remote MCP transport {kind:?} is not supported; this build speaks stdio only, \
so an entry needs a `command`"
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
            Self::Shadowed { by_file } => {
                write!(
                    f,
                    "a server of this name was already configured by {by_file}"
                )
            }
            Self::OverCapacity { limit } => {
                write!(f, "already at the {limit}-server limit for this project")
            }
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
        let accepted = parse_file(&value, file_name, &mut loaded.rejections);
        for entry in accepted {
            let name = entry.config.name.clone();
            if claimed.contains(&name) {
                let by_file = loaded
                    .servers
                    .iter()
                    .find(|server| server.config.name == name)
                    .map(|server| server.file.clone())
                    .unwrap_or_else(|| "an earlier settings file".to_owned());
                loaded.rejections.push(McpConfigRejection {
                    name: label(&name),
                    file: file_name.to_owned(),
                    issue: McpConfigIssue::Shadowed { by_file },
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
            Err(issue) => rejected.push(McpConfigRejection {
                name: label(name),
                file: file_name.to_owned(),
                issue,
            }),
        }
    }
    accepted
}

/// Validate one `mcpServers` entry into a spawn description.
pub fn parse_entry(
    name: &str,
    spec: &serde_json::Value,
) -> Result<McpServerConfig, McpConfigIssue> {
    validate_server_name(name)?;
    let Some(spec) = spec.as_object() else {
        return Err(McpConfigIssue::EntryNotAnObject);
    };
    let command = match spec.get("command").and_then(serde_json::Value::as_str) {
        Some(command) => command,
        None => {
            // A remote entry is the common shape of "no command"; naming the
            // real reason is the difference between a user fixing a typo and
            // a user discovering this build is stdio-only.
            let kind = spec
                .get("type")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .or_else(|| spec.get("url").map(|_| "url".to_owned()));
            return Err(match kind {
                Some(kind) if kind != "stdio" => {
                    McpConfigIssue::RemoteTransport { kind: label(&kind) }
                }
                _ => McpConfigIssue::MissingCommand,
            });
        }
    };
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
    Ok(McpServerConfig {
        name: name.to_owned(),
        command: command.to_owned(),
        args,
        env,
    })
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
    fn a_remote_entry_names_the_real_reason_not_just_a_missing_command() {
        // Reporting "entry has no `command`" for `{"type": "http", …}` is
        // true and useless: the user did not forget a key, this build does
        // not speak that transport.
        assert_eq!(
            parse_one(r#"{"type": "http", "url": "https://example.com/mcp"}"#),
            Err(McpConfigIssue::RemoteTransport {
                kind: "http".to_owned()
            })
        );
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
            McpConfigIssue::OverCapacity { limit: 8 },
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
}
