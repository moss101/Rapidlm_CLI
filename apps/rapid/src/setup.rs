//! `rapid setup`: from nothing to a model configuration in one command
//! (SEAM-02).
//!
//! The command resolves a choice — a preset or an explicit endpoint, a
//! model, a profile name and where the key comes from — into a **plan**: the
//! file it would write, every key it would set or unset, where the credential
//! lives, and the verification request it would make first. `--dry-run`
//! prints exactly that plan and does nothing else: no network call, no disk
//! write. The plan is computed by the same code the writing run uses, so what
//! a dry run shows is what a real run writes.
//!
//! Boundaries:
//!
//! * A key never arrives on argv — it would be visible in the process list
//!   and the shell history. `--key-env <VAR>` names an environment variable
//!   the config then references (`env_key`); `--key-stdin` reads the key from
//!   stdin for the OS keychain, and the config names the keychain alias.
//!   Anything that looks like a key flag is a usage error that says so.
//! * The existing config is edited, not replaced: other profiles, comments and
//!   formatting survive (`toml_edit`); only the chosen profile's keys and
//!   `[models] default` change, and a credential key that would shadow the
//!   chosen one (`api_key` wins over `env_key` at resolution) is removed and
//!   listed as unset.
//! * An existing config that does not parse is never overwritten.
//!
//! Presets are data (rule 2.2: upstream providers are named only in the
//! preset rows and their tests).

use std::io::BufRead;
use std::path::{Path, PathBuf};

use llm_router::providers::openai_compatible::{OpenAiApiStyle, OpenAiCompatibleEndpoint};

use crate::user_config::{
    CONFIG_PATH_ENV, ConfigProvider, HOME_ENV, RAPIDLM_HOME_ENV, USERPROFILE_ENV, env_value,
    parse_config_document,
};

/// Output tokens the verification request may produce (SEAM-02: ≤16).
pub const VERIFY_MAX_OUTPUT_TOKENS: u32 = 16;
/// The mode the config file is written with: it may name a keychain alias or
/// an environment variable, and it is the user's alone.
pub const CONFIG_FILE_MODE: u32 = 0o600;
const MAX_PROFILE_ID_BYTES: usize = 64;
const MAX_MODEL_ID_BYTES: usize = 256;
const MAX_ENV_NAME_BYTES: usize = 128;

/// One provider preset — an endpoint, the wire dialect it speaks, a model to
/// start with, where its documentation is, whether it is a server on this
/// machine, and the environment variable its key is conventionally kept in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Preset {
    pub id: &'static str,
    pub dialect: ConfigProvider,
    pub base_url: &'static str,
    pub default_model: &'static str,
    pub docs: &'static str,
    pub local: bool,
    /// `None`: the server takes no key (a local server).
    pub key_env: Option<&'static str>,
}

/// The preset table. Adding a provider is adding a row (and its test row).
pub const PRESETS: &[Preset] = &[
    Preset {
        id: "openai",
        dialect: ConfigProvider::OpenAiCompatible,
        base_url: "https://api.openai.com/v1",
        default_model: "gpt-5",
        docs: "https://platform.openai.com/docs",
        local: false,
        key_env: Some("OPENAI_API_KEY"),
    },
    Preset {
        id: "anthropic",
        dialect: ConfigProvider::Anthropic,
        base_url: "https://api.anthropic.com/v1",
        default_model: "claude-sonnet-5",
        docs: "https://docs.anthropic.com",
        local: false,
        key_env: Some("ANTHROPIC_API_KEY"),
    },
    Preset {
        id: "openrouter",
        dialect: ConfigProvider::OpenAiCompatible,
        base_url: "https://openrouter.ai/api/v1",
        default_model: "openrouter/auto",
        docs: "https://openrouter.ai/docs",
        local: false,
        key_env: Some("OPENROUTER_API_KEY"),
    },
    Preset {
        id: "groq",
        dialect: ConfigProvider::OpenAiCompatible,
        base_url: "https://api.groq.com/openai/v1",
        default_model: "llama-3.3-70b-versatile",
        docs: "https://console.groq.com/docs",
        local: false,
        key_env: Some("GROQ_API_KEY"),
    },
    Preset {
        id: "together",
        dialect: ConfigProvider::OpenAiCompatible,
        base_url: "https://api.together.xyz/v1",
        default_model: "meta-llama/Llama-3.3-70B-Instruct-Turbo",
        docs: "https://docs.together.ai",
        local: false,
        key_env: Some("TOGETHER_API_KEY"),
    },
    Preset {
        id: "deepseek",
        dialect: ConfigProvider::OpenAiCompatible,
        base_url: "https://api.deepseek.com/v1",
        default_model: "deepseek-chat",
        docs: "https://api-docs.deepseek.com",
        local: false,
        key_env: Some("DEEPSEEK_API_KEY"),
    },
    Preset {
        id: "mistral",
        dialect: ConfigProvider::OpenAiCompatible,
        base_url: "https://api.mistral.ai/v1",
        default_model: "mistral-large-latest",
        docs: "https://docs.mistral.ai",
        local: false,
        key_env: Some("MISTRAL_API_KEY"),
    },
    Preset {
        id: "ollama",
        dialect: ConfigProvider::OpenAiCompatible,
        base_url: "http://127.0.0.1:11434/v1",
        default_model: "llama3.2",
        docs: "https://ollama.com",
        local: true,
        key_env: None,
    },
    Preset {
        id: "lm-studio",
        dialect: ConfigProvider::OpenAiCompatible,
        base_url: "http://127.0.0.1:1234/v1",
        default_model: "local-model",
        docs: "https://lmstudio.ai/docs",
        local: true,
        key_env: None,
    },
    Preset {
        id: "llama-cpp",
        dialect: ConfigProvider::OpenAiCompatible,
        base_url: "http://127.0.0.1:8080/v1",
        default_model: "local-model",
        docs: "https://github.com/ggml-org/llama.cpp",
        local: true,
        key_env: None,
    },
    Preset {
        id: "vllm",
        dialect: ConfigProvider::OpenAiCompatible,
        base_url: "http://127.0.0.1:8000/v1",
        default_model: "local-model",
        docs: "https://docs.vllm.ai",
        local: true,
        key_env: None,
    },
];

/// The preset with this id.
pub fn preset(id: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|preset| preset.id == id)
}

/// Where the key comes from, as the user asked.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum KeyFlag {
    /// Neither `--key-env` nor `--key-stdin`: the preset's convention.
    #[default]
    Unspecified,
    Env(String),
    Stdin,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
}

/// The parsed command line.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SetupArgs {
    pub preset: Option<String>,
    pub profile: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub key: KeyFlag,
    pub dry_run: bool,
    pub non_interactive: bool,
    pub output: OutputFormat,
    pub no_verify: bool,
}

/// `rapid setup --help`.
pub fn usage() -> String {
    let mut presets = String::new();
    for preset in PRESETS {
        presets.push_str(&format!(
            "  {:<11} {:<6} {}\n",
            preset.id,
            if preset.local { "local" } else { "cloud" },
            preset.default_model
        ));
    }
    format!(
        "\
usage: rapid setup [--preset <id>] [--profile <id>] [--model <id>] [--base-url <url>]
                   [--key-env <VAR> | --key-stdin] [--dry-run] [--non-interactive]
                   [--output text|json] [--no-verify]

Write a model configuration: a [model.<profile>] table and [models] default in
the user config (RAPIDLM_CONFIG, else RAPIDLM_HOME/config.toml, else
~/.rapidlm/config.toml). Other profiles, comments and formatting are kept.

  --preset <id>       start from a preset (below); --base-url and --model override it
  --profile <id>      the [model.<id>] name to write (default: the preset id, or \"default\")
  --model <id>        the model identifier to request (default: the preset's)
  --base-url <url>    the endpoint (http:// or https:// origin); without --preset the
                      endpoint is taken to speak the openai-compatible dialect
  --key-env <VAR>     the key is read from this environment variable at request time;
                      the config names the variable, never the value
  --key-stdin         read the key from stdin and keep it in the OS keychain; the
                      config names the keychain alias
  --dry-run           print exactly what would be written and requested, then stop:
                      no network call, no file written
  --non-interactive   never prompt; a missing choice is a usage error
  --output json       print the plan (or the outcome) as one JSON object
  --no-verify         skip the live verification request

A key is never accepted on the command line. On a terminal, choices you did not
give are asked for (unless --non-interactive or --key-stdin).

Presets:
{presets}
Exit codes:
  0   done (or the dry-run plan was printed)
  1   the existing config could not be read or does not parse
  2   usage error
"
    )
}

/// Parse `rapid setup`'s arguments (after the subcommand name).
pub fn parse_args(args: &[String]) -> Result<SetupArgs, String> {
    let mut parsed = SetupArgs::default();
    let mut index = 0;
    let mut seen: Vec<&str> = Vec::new();
    while index < args.len() {
        let raw = args[index].as_str();
        let (flag, inline) = match raw.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(value)),
            _ => (raw, None),
        };
        if is_key_bearing_flag(flag) {
            return Err(format!(
                "rapid setup: {flag} is not accepted — a key on the command line is visible in \
the process list and the shell history; use --key-env <VAR> or --key-stdin"
            ));
        }
        let takes_value = matches!(
            flag,
            "--preset" | "--profile" | "--model" | "--base-url" | "--key-env" | "--output"
        );
        let is_switch = matches!(
            flag,
            "--key-stdin" | "--dry-run" | "--non-interactive" | "--no-verify"
        );
        if !takes_value && !is_switch {
            return Err(if flag.starts_with('-') {
                format!("rapid setup: unknown option {flag}")
            } else {
                format!("rapid setup: unexpected argument '{raw}'")
            });
        }
        if seen.contains(&flag) {
            return Err(format!("rapid setup: {flag} given twice"));
        }
        seen.push(flag);
        let value = if takes_value {
            match inline {
                Some(value) => value.to_owned(),
                None => {
                    index += 1;
                    args.get(index)
                        .filter(|value| !value.starts_with("--"))
                        .cloned()
                        .ok_or_else(|| format!("rapid setup: {flag} needs a value"))?
                }
            }
        } else {
            if inline.is_some() {
                return Err(format!("rapid setup: {flag} takes no value"));
            }
            String::new()
        };
        match flag {
            "--preset" => parsed.preset = Some(value),
            "--profile" => parsed.profile = Some(validate_profile_id(&value)?),
            "--model" => parsed.model = Some(validate_model_id(&value)?),
            "--base-url" => parsed.base_url = Some(validate_base_url(&value)?),
            "--key-env" => parsed.key = KeyFlag::Env(validate_env_name(&value)?),
            "--output" => {
                parsed.output = match value.as_str() {
                    "json" => OutputFormat::Json,
                    "text" => OutputFormat::Text,
                    other => {
                        return Err(format!(
                            "rapid setup: --output takes text or json, not '{other}'"
                        ));
                    }
                }
            }
            "--key-stdin" => parsed.key = KeyFlag::Stdin,
            "--dry-run" => parsed.dry_run = true,
            "--non-interactive" => parsed.non_interactive = true,
            "--no-verify" => parsed.no_verify = true,
            _ => unreachable!("every accepted flag is matched"),
        }
        index += 1;
    }
    if seen.contains(&"--key-env") && seen.contains(&"--key-stdin") {
        return Err("rapid setup: --key-env and --key-stdin are alternatives; give one".to_owned());
    }
    if let Some(id) = &parsed.preset
        && preset(id).is_none()
    {
        return Err(format!(
            "rapid setup: unknown preset '{id}' (one of: {})",
            preset_ids()
        ));
    }
    Ok(parsed)
}

fn preset_ids() -> String {
    PRESETS
        .iter()
        .map(|preset| preset.id)
        .collect::<Vec<_>>()
        .join(", ")
}

/// A flag whose value would be a secret: refused by name, with the reason.
fn is_key_bearing_flag(flag: &str) -> bool {
    matches!(
        flag,
        "--key" | "--api-key" | "--apikey" | "--token" | "--secret" | "--password"
    )
}

fn validate_profile_id(raw: &str) -> Result<String, String> {
    if raw.is_empty()
        || raw.len() > MAX_PROFILE_ID_BYTES
        || !raw
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Err(format!(
            "rapid setup: --profile must be 1–{MAX_PROFILE_ID_BYTES} letters, digits, '-' or '_'"
        ));
    }
    Ok(raw.to_owned())
}

fn validate_model_id(raw: &str) -> Result<String, String> {
    if raw.is_empty()
        || raw.len() > MAX_MODEL_ID_BYTES
        || raw
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control() || ch == '"')
    {
        return Err(format!(
            "rapid setup: --model must be 1–{MAX_MODEL_ID_BYTES} bytes with no spaces, quotes \
or control characters"
        ));
    }
    Ok(raw.to_owned())
}

fn validate_env_name(raw: &str) -> Result<String, String> {
    let mut chars = raw.chars();
    let valid_start = chars
        .next()
        .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_');
    if !valid_start
        || raw.len() > MAX_ENV_NAME_BYTES
        || !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        return Err(
            "rapid setup: --key-env takes an environment variable name (letters, digits, '_'), \
not a key"
                .to_owned(),
        );
    }
    Ok(raw.to_owned())
}

/// The same validation the model client applies to `base_url`, so a URL
/// setup accepts is one a turn can use.
fn validate_base_url(raw: &str) -> Result<String, String> {
    OpenAiCompatibleEndpoint::new(raw, OpenAiApiStyle::ChatCompletions)
        .map(|endpoint| endpoint.base_url().to_owned())
        .map_err(|_| {
            format!(
                "rapid setup: --base-url '{raw}' is not usable: expected an http:// or https:// \
origin without userinfo or metadata hosts"
            )
        })
}

/// Asks for a choice the command line did not give (a terminal only).
/// `None`: the input ended.
pub trait Prompter {
    fn ask(&mut self, question: &str) -> Option<String>;
}

/// Asks on stderr, reads a line from stdin.
pub struct TerminalPrompter;

impl Prompter for TerminalPrompter {
    fn ask(&mut self, question: &str) -> Option<String> {
        eprint!("{question}");
        let mut line = String::new();
        match std::io::stdin().lock().read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line.trim().to_owned()),
        }
    }
}

/// Where the credential lives once set up.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Credential {
    /// The config names the environment variable (`env_key`).
    Env { var: String },
    /// The key is read from stdin into the OS keychain under `alias`; the
    /// config names the alias.
    Keychain { alias: String },
    /// No key: the endpoint is called without credentials.
    None,
}

/// A fully resolved choice.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Choice {
    pub preset: Option<&'static Preset>,
    pub profile: String,
    pub dialect: ConfigProvider,
    pub model: String,
    pub base_url: String,
    pub credential: Credential,
}

/// Resolve the command line (and, on a terminal, the answers to what it left
/// out) into a [`Choice`].
pub fn resolve_choice(
    args: &SetupArgs,
    interactive: bool,
    prompter: &mut dyn Prompter,
) -> Result<Choice, String> {
    let mut chosen = args.preset.as_deref().and_then(preset);
    if chosen.is_none() && args.base_url.is_none() {
        if !interactive {
            return Err(format!(
                "rapid setup: choose --preset <id> (one of: {}) or give --base-url and --model",
                preset_ids()
            ));
        }
        chosen = Some(ask_preset(prompter)?);
    }
    let base_url = match (&args.base_url, chosen) {
        (Some(url), _) => url.clone(),
        (None, Some(preset)) => preset.base_url.to_owned(),
        (None, None) => unreachable!("a preset or --base-url is resolved above"),
    };
    let model = match (&args.model, chosen) {
        (Some(model), _) => model.clone(),
        (None, Some(preset)) if !interactive || args.preset.is_some() => {
            preset.default_model.to_owned()
        }
        (None, Some(preset)) => {
            let answer = ask(
                prompter,
                &format!("model [{}]: ", preset.default_model),
                preset.default_model,
            )?;
            validate_model_id(&answer)?
        }
        (None, None) if interactive => validate_model_id(&ask(prompter, "model: ", "")?)?,
        (None, None) => {
            return Err(
                "rapid setup: --base-url needs --model (there is no preset default)".to_owned(),
            );
        }
    };
    let profile = match &args.profile {
        Some(profile) => profile.clone(),
        None => chosen.map_or("default", |preset| preset.id).to_owned(),
    };
    let credential = match &args.key {
        KeyFlag::Env(var) => Credential::Env { var: var.clone() },
        KeyFlag::Stdin => Credential::Keychain {
            alias: keychain_alias(&profile),
        },
        KeyFlag::Unspecified => match chosen.and_then(|preset| preset.key_env) {
            Some(var) => Credential::Env {
                var: var.to_owned(),
            },
            None => Credential::None,
        },
    };
    Ok(Choice {
        preset: chosen,
        profile,
        dialect: chosen.map_or(ConfigProvider::OpenAiCompatible, |preset| preset.dialect),
        model,
        base_url,
        credential,
    })
}

/// The keychain alias a profile's key is stored under.
pub fn keychain_alias(profile: &str) -> String {
    format!("rapidlm-model-{profile}")
}

fn ask_preset(prompter: &mut dyn Prompter) -> Result<&'static Preset, String> {
    let mut menu = String::from("presets:\n");
    for (index, preset) in PRESETS.iter().enumerate() {
        menu.push_str(&format!(
            "  {:>2}. {:<11} {}\n",
            index + 1,
            preset.id,
            if preset.local {
                "(a server on this machine)"
            } else {
                ""
            }
        ));
    }
    let answer = ask(prompter, &format!("{menu}preset (number or id): "), "")?;
    answer
        .parse::<usize>()
        .ok()
        .and_then(|number| number.checked_sub(1))
        .and_then(|index| PRESETS.get(index))
        .or_else(|| preset(&answer))
        .ok_or_else(|| {
            format!(
                "rapid setup: unknown preset '{answer}' (one of: {})",
                preset_ids()
            )
        })
}

fn ask(prompter: &mut dyn Prompter, question: &str, default: &str) -> Result<String, String> {
    let answer = prompter
        .ask(question)
        .ok_or_else(|| "rapid setup: input ended before every choice was made".to_owned())?;
    Ok(if answer.is_empty() {
        default.to_owned()
    } else {
        answer
    })
}

/// Where the config goes: the file a run would read, or, when there is none
/// yet, the first home convention the reader checks.
pub fn config_target(env: &[(String, String)]) -> Result<PathBuf, String> {
    let set = |name: &str| env_value(env, name).filter(|value| !value.trim().is_empty());
    if let Some(path) = set(CONFIG_PATH_ENV) {
        return Ok(PathBuf::from(path));
    }
    let candidates: Vec<PathBuf> = [
        set(RAPIDLM_HOME_ENV).map(|root| Path::new(root).join("config.toml")),
        set(HOME_ENV).map(|root| Path::new(root).join(".rapidlm").join("config.toml")),
        set(USERPROFILE_ENV).map(|root| Path::new(root).join(".rapidlm").join("config.toml")),
    ]
    .into_iter()
    .flatten()
    .collect();
    candidates
        .iter()
        .find(|path| path.is_file())
        .or_else(|| candidates.first())
        .cloned()
        .ok_or_else(|| {
            "rapid setup: no home directory — set HOME, RAPIDLM_HOME or RAPIDLM_CONFIG".to_owned()
        })
}

/// What happens to the config file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileAction {
    Create,
    Update,
    Unchanged,
}

impl FileAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Update => "update",
            Self::Unchanged => "unchanged",
        }
    }
}

/// Everything a run would do, computed without doing any of it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupPlan {
    pub choice: Choice,
    pub config_path: PathBuf,
    pub action: FileAction,
    /// The previous file's copy, written only when an existing file changes.
    pub backup: Option<PathBuf>,
    /// Dotted keys set, with their values (never a secret).
    pub set: Vec<(String, String)>,
    /// Dotted keys removed (a credential key that would shadow the chosen one).
    pub unset: Vec<String>,
    /// Whether the credential's environment variable is set right now (for
    /// `Credential::Env`): the plan says so, never what it holds.
    pub env_key_present: Option<bool>,
    pub verify: bool,
    /// The file's content after the run.
    pub document: String,
}

/// Compute the plan for `choice` against the config at `config_path`
/// (`existing`: its current content, if it exists).
pub fn plan(
    choice: Choice,
    config_path: &Path,
    existing: Option<&str>,
    env: &[(String, String)],
    verify: bool,
    now_compact: &str,
) -> Result<SetupPlan, String> {
    let mut doc = match existing {
        Some(text) => text.parse::<toml_edit::DocumentMut>().map_err(|err| {
            format!(
                "rapid setup: {} is not valid TOML and is left untouched: {}",
                config_path.display(),
                err.to_string().lines().next().unwrap_or_default()
            )
        })?,
        None => toml_edit::DocumentMut::new(),
    };
    let mut set = Vec::new();
    let mut unset = Vec::new();
    {
        let models = table_at(doc.as_table_mut(), "models", config_path, false)?;
        set_value(models, "default", &choice.profile, "models", &mut set);
    }
    {
        let model = table_at(doc.as_table_mut(), "model", config_path, true)?;
        let profile = table_at(model, &choice.profile, config_path, false)?;
        let prefix = format!("model.{}", choice.profile);
        set_value(
            profile,
            "provider",
            choice.dialect.as_str(),
            &prefix,
            &mut set,
        );
        set_value(profile, "model", &choice.model, &prefix, &mut set);
        set_value(profile, "base_url", &choice.base_url, &prefix, &mut set);
        let keep = match &choice.credential {
            Credential::Env { var } => {
                set_value(profile, "env_key", var, &prefix, &mut set);
                Some("env_key")
            }
            Credential::Keychain { alias } => {
                set_value(profile, "keychain", alias, &prefix, &mut set);
                Some("keychain")
            }
            Credential::None => None,
        };
        // Credential keys other than the chosen one would shadow it (an
        // inline `api_key` wins over everything) or contradict it.
        for key in ["api_key", "env_key", "keychain"] {
            if Some(key) != keep && profile.remove(key).is_some() {
                unset.push(format!("{prefix}.{key}"));
            }
        }
    }
    let document = doc.to_string();
    // What a turn will read must parse: setup never leaves a config the
    // reader rejects.
    parse_config_document(&document, &config_path.display().to_string()).map_err(|err| {
        format!(
            "rapid setup: {} would not parse after the change and is left untouched: {err}",
            config_path.display()
        )
    })?;
    let action = match existing {
        None => FileAction::Create,
        Some(previous) if previous == document => FileAction::Unchanged,
        Some(_) => FileAction::Update,
    };
    let backup = (action == FileAction::Update).then(|| {
        let name = config_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "config.toml".to_owned());
        config_path.with_file_name(format!("{name}.{now_compact}.bak"))
    });
    let env_key_present = match &choice.credential {
        Credential::Env { var } => Some(env_value(env, var).is_some_and(|value| !value.is_empty())),
        _ => None,
    };
    Ok(SetupPlan {
        choice,
        config_path: config_path.to_path_buf(),
        action,
        backup,
        set,
        unset,
        env_key_present,
        verify,
        document,
    })
}

/// The table at `key` in `parent`, created when absent; an existing
/// non-table value there is an error naming it.
fn table_at<'a>(
    parent: &'a mut toml_edit::Table,
    key: &str,
    config_path: &Path,
    implicit: bool,
) -> Result<&'a mut toml_edit::Table, String> {
    if !parent.contains_key(key) {
        let mut table = toml_edit::Table::new();
        table.set_implicit(implicit);
        parent.insert(key, toml_edit::Item::Table(table));
    }
    parent
        .get_mut(key)
        .and_then(toml_edit::Item::as_table_mut)
        .ok_or_else(|| {
            format!(
                "rapid setup: `{key}` in {} is not a table; fix it by hand",
                config_path.display()
            )
        })
}

/// Set `table[key] = value` (a string), keeping the entry's decoration when
/// it already holds that value, and record the change.
fn set_value(
    table: &mut toml_edit::Table,
    key: &str,
    value: &str,
    prefix: &str,
    set: &mut Vec<(String, String)>,
) {
    let current = table.get(key).and_then(toml_edit::Item::as_str);
    if current != Some(value) {
        table.insert(key, toml_edit::value(value));
    }
    set.push((format!("{prefix}.{key}"), value.to_owned()));
}

/// The plan as the text a person reads.
pub fn render_text(plan: &SetupPlan, dry_run: bool) -> String {
    let mut out = String::new();
    if dry_run {
        out.push_str("rapid setup — dry run: nothing was written and no request was made\n");
    }
    let path = plan.config_path.display();
    match plan.action {
        FileAction::Unchanged => out.push_str(&format!(
            "  config   {path}  unchanged — it already holds this profile\n"
        )),
        action => out.push_str(&format!(
            "  config   {path}  {} (mode {:o})\n",
            action.as_str(),
            CONFIG_FILE_MODE
        )),
    }
    if let Some(backup) = &plan.backup {
        out.push_str(&format!(
            "  backup   {} (the previous file)\n",
            backup.display()
        ));
    }
    for (key, value) in &plan.set {
        out.push_str(&format!("  set      {key} = \"{value}\"\n"));
    }
    for key in &plan.unset {
        out.push_str(&format!("  unset    {key}\n"));
    }
    out.push_str(&match &plan.choice.credential {
        Credential::Env { var } => format!(
            "  key      ${var} at request time ({}) — the config names the variable, never the value\n",
            if plan.env_key_present == Some(true) {
                "set now"
            } else {
                "not set now"
            }
        ),
        Credential::Keychain { alias } => format!(
            "  key      read from stdin into the OS keychain as '{alias}'; the config names the alias\n"
        ),
        Credential::None => {
            "  key      none — the endpoint is called without credentials\n".to_owned()
        }
    });
    out.push_str(&if plan.verify {
        format!(
            "  verify   one request of at most {VERIFY_MAX_OUTPUT_TOKENS} output tokens to {} before anything is written\n",
            plan.choice.base_url
        )
    } else {
        "  verify   skipped (--no-verify)\n".to_owned()
    });
    if let Some(preset) = plan.choice.preset {
        out.push_str(&format!("  docs     {}\n", preset.docs));
    }
    out
}

/// The plan as one JSON object.
pub fn render_json(plan: &SetupPlan, dry_run: bool) -> serde_json::Value {
    let credential = match &plan.choice.credential {
        Credential::Env { var } => serde_json::json!({
            "source": "env",
            "var": var,
            "set": plan.env_key_present == Some(true),
        }),
        Credential::Keychain { alias } => serde_json::json!({
            "source": "stdin",
            "stored": "keychain",
            "alias": alias,
        }),
        Credential::None => serde_json::json!({ "source": "none" }),
    };
    serde_json::json!({
        "schema": "rapidlm.setup_plan/v1",
        "dry_run": dry_run,
        "profile": plan.choice.profile,
        "preset": plan.choice.preset.map(|preset| preset.id),
        "files": [{
            "path": plan.config_path.display().to_string(),
            "action": plan.action.as_str(),
            "mode": format!("{:o}", CONFIG_FILE_MODE),
            "backup": plan.backup.as_ref().map(|path| path.display().to_string()),
        }],
        "keys": plan
            .set
            .iter()
            .map(|(key, value)| serde_json::json!({ "key": key, "value": value }))
            .collect::<Vec<_>>(),
        "unset": plan.unset,
        "credential": credential,
        "verify": {
            "planned": plan.verify,
            "endpoint": plan.choice.base_url,
            "max_output_tokens": VERIFY_MAX_OUTPUT_TOKENS,
        },
        "network": false,
        "written": false,
    })
}

/// Process inputs, injectable so the command runs in a test without touching
/// the test process's environment or terminal.
pub struct SetupEnv {
    pub env: Vec<(String, String)>,
    pub stdin_is_tty: bool,
}

impl SetupEnv {
    pub fn from_process() -> Self {
        use std::io::IsTerminal;
        Self {
            env: std::env::vars().collect(),
            stdin_is_tty: std::io::stdin().is_terminal(),
        }
    }
}

/// What a run printed and how it exited.
#[derive(Debug, PartialEq, Eq)]
pub struct SetupOutcome {
    pub stdout: String,
    pub stderr: String,
    pub exit: i32,
}

/// Run one `rapid setup` invocation.
pub fn run(args: &[String], env: &SetupEnv, prompter: &mut dyn Prompter) -> SetupOutcome {
    let usage_error = |message: String| SetupOutcome {
        stdout: String::new(),
        stderr: format!("{message}\n(see rapid setup --help)\n"),
        exit: 2,
    };
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        return SetupOutcome {
            stdout: usage(),
            stderr: String::new(),
            exit: 0,
        };
    }
    let parsed = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(message) => return usage_error(message),
    };
    // Stdin carries the key under --key-stdin, so it cannot also answer
    // questions.
    let interactive = env.stdin_is_tty && !parsed.non_interactive && parsed.key != KeyFlag::Stdin;
    let choice = match resolve_choice(&parsed, interactive, prompter) {
        Ok(choice) => choice,
        Err(message) => return usage_error(message),
    };
    let config_path = match config_target(&env.env) {
        Ok(path) => path,
        Err(message) => return usage_error(message),
    };
    let existing = match std::fs::read_to_string(&config_path) {
        Ok(text) => Some(text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            return SetupOutcome {
                stdout: String::new(),
                stderr: format!(
                    "rapid setup: {} could not be read: {err}\n",
                    config_path.display()
                ),
                exit: 1,
            };
        }
    };
    let now = compact_utc_now();
    let plan = match plan(
        choice,
        &config_path,
        existing.as_deref(),
        &env.env,
        !parsed.no_verify,
        &now,
    ) {
        Ok(plan) => plan,
        Err(message) => {
            return SetupOutcome {
                stdout: String::new(),
                stderr: format!("{message}\n"),
                exit: 1,
            };
        }
    };
    if !parsed.dry_run {
        return SetupOutcome {
            stdout: String::new(),
            stderr: "rapid setup: writing a configuration comes with live verification, which \
this build does not have yet; --dry-run prints what would be written\n"
                .to_owned(),
            exit: 2,
        };
    }
    let stdout = match parsed.output {
        OutputFormat::Text => render_text(&plan, true),
        OutputFormat::Json => format!("{}\n", render_json(&plan, true)),
    };
    SetupOutcome {
        stdout,
        stderr: String::new(),
        exit: 0,
    }
}

/// `20260924T051500Z`: UTC now, compact, for a backup file's name.
fn compact_utc_now() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| (*arg).to_owned()).collect()
    }

    /// Answers from a script; panics on a question beyond it.
    struct Scripted(Vec<&'static str>);

    impl Prompter for Scripted {
        fn ask(&mut self, question: &str) -> Option<String> {
            assert!(!self.0.is_empty(), "unexpected question: {question}");
            Some(self.0.remove(0).to_owned())
        }
    }

    struct Home(PathBuf);

    impl Home {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "rapidlm-setup-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or_default()
            ));
            std::fs::create_dir_all(&dir).expect("home");
            Self(dir)
        }

        fn env(&self) -> SetupEnv {
            SetupEnv {
                env: vec![
                    (HOME_ENV.to_owned(), self.0.display().to_string()),
                    (
                        "OPENAI_API_KEY".to_owned(),
                        "set-but-never-printed".to_owned(),
                    ),
                ],
                stdin_is_tty: false,
            }
        }

        fn config(&self) -> PathBuf {
            self.0.join(".rapidlm").join("config.toml")
        }

        /// Every path and its bytes, for before/after comparison.
        fn snapshot(&self) -> Vec<(PathBuf, Vec<u8>)> {
            fn walk(dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
                let Ok(entries) = std::fs::read_dir(dir) else {
                    return;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        out.push((path.clone(), Vec::new()));
                        walk(&path, out);
                    } else {
                        out.push((path.clone(), std::fs::read(&path).unwrap_or_default()));
                    }
                }
            }
            let mut out = Vec::new();
            walk(&self.0, &mut out);
            out.sort();
            out
        }
    }

    impl Drop for Home {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn every_preset_row_is_usable_as_written() {
        // Rule 2.2: providers are named here and in these rows only. Each row
        // must be something `rapid setup` can write and a turn can read.
        let mut ids = std::collections::BTreeSet::new();
        for preset in PRESETS {
            assert!(ids.insert(preset.id), "duplicate preset id {}", preset.id);
            assert!(validate_profile_id(preset.id).is_ok(), "{}", preset.id);
            assert!(
                validate_base_url(preset.base_url).is_ok(),
                "{}",
                preset.base_url
            );
            assert!(
                validate_model_id(preset.default_model).is_ok(),
                "{}",
                preset.id
            );
            assert!(preset.docs.starts_with("https://"), "{}", preset.docs);
            assert_eq!(
                preset.local,
                preset.base_url.starts_with("http://127.0.0.1"),
                "{}: local means a loopback server",
                preset.id
            );
            assert_eq!(
                preset.key_env.is_none(),
                preset.local,
                "{}: a cloud preset names its key variable, a local one takes none",
                preset.id
            );
            if let Some(var) = preset.key_env {
                assert!(validate_env_name(var).is_ok(), "{var}");
            }
            let choice = resolve_choice(
                &SetupArgs {
                    preset: Some(preset.id.to_owned()),
                    ..SetupArgs::default()
                },
                false,
                &mut Scripted(Vec::new()),
            )
            .expect("resolves");
            let plan = plan(choice, Path::new("config.toml"), None, &[], true, "T")
                .expect("plans a config the reader accepts");
            let config = parse_config_document(&plan.document, "config.toml").expect("parses");
            let entry = config.models.entries.get(preset.id).expect("the profile");
            assert_eq!(entry.provider, preset.dialect);
            assert_eq!(entry.model, preset.default_model);
        }
        assert_eq!(
            preset("anthropic").map(|p| p.dialect),
            Some(ConfigProvider::Anthropic)
        );
    }

    #[test]
    fn a_dry_run_prints_the_plan_and_writes_nothing_and_connects_nowhere() {
        // SEAM-02 AC-01: `--dry-run --non-interactive --output json` prints
        // exactly the files and keys it would write; the home is
        // byte-identical afterwards and the endpoint received no connection.
        let home = Home::new("dry-run");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.set_nonblocking(true).expect("nonblocking");
        let url = format!("http://{}/v1", listener.local_addr().expect("addr"));
        let before = home.snapshot();
        let outcome = run(
            &args(&[
                "--preset",
                "openai",
                "--base-url",
                &url,
                "--dry-run",
                "--non-interactive",
                "--output",
                "json",
            ]),
            &home.env(),
            &mut Scripted(Vec::new()),
        );
        assert_eq!(outcome.exit, 0, "{}", outcome.stderr);
        assert_eq!(home.snapshot(), before, "nothing was written");
        assert!(
            matches!(listener.accept(), Err(err) if err.kind() == std::io::ErrorKind::WouldBlock),
            "no connection was made"
        );
        let plan: serde_json::Value =
            serde_json::from_str(&outcome.stdout).expect("one JSON object");
        assert_eq!(plan["schema"], "rapidlm.setup_plan/v1");
        assert_eq!(plan["dry_run"], true);
        assert_eq!(plan["network"], false);
        assert_eq!(plan["written"], false);
        assert_eq!(
            plan["files"][0]["path"],
            home.config().display().to_string()
        );
        assert_eq!(plan["files"][0]["action"], "create");
        assert_eq!(plan["files"][0]["mode"], "600");
        let keys: Vec<(String, String)> = plan["keys"]
            .as_array()
            .expect("keys")
            .iter()
            .map(|entry| {
                (
                    entry["key"].as_str().unwrap_or_default().to_owned(),
                    entry["value"].as_str().unwrap_or_default().to_owned(),
                )
            })
            .collect();
        assert_eq!(
            keys,
            vec![
                ("models.default".to_owned(), "openai".to_owned()),
                (
                    "model.openai.provider".to_owned(),
                    "openai-compatible".to_owned()
                ),
                ("model.openai.model".to_owned(), "gpt-5".to_owned()),
                ("model.openai.base_url".to_owned(), url.clone()),
                (
                    "model.openai.env_key".to_owned(),
                    "OPENAI_API_KEY".to_owned()
                ),
            ]
        );
        assert_eq!(plan["credential"]["source"], "env");
        assert_eq!(plan["credential"]["set"], true);
        assert!(
            !outcome.stdout.contains("set-but-never-printed"),
            "a key's value is never printed"
        );
        assert_eq!(plan["verify"]["max_output_tokens"], 16);
    }

    #[test]
    fn an_existing_config_is_edited_not_replaced_and_a_shadowing_key_is_unset() {
        let home = Home::new("edit");
        std::fs::create_dir_all(home.config().parent().expect("dir")).expect("dir");
        let existing = "\
# my models
[models]
default = \"local\"

[model.local]
provider = \"openai-compatible\" # the laptop
model = \"llama3.2\"
base_url = \"http://127.0.0.1:11434/v1\"

[model.openai]
provider = \"openai-compatible\"
model = \"old\"
base_url = \"https://api.openai.com/v1\"
api_key = \"inline-secret\"
";
        std::fs::write(home.config(), existing).expect("config");
        let outcome = run(
            &args(&["--preset", "openai", "--dry-run", "--non-interactive"]),
            &home.env(),
            &mut Scripted(Vec::new()),
        );
        assert_eq!(outcome.exit, 0, "{}", outcome.stderr);
        assert!(
            outcome.stdout.contains("update (mode 600)"),
            "{}",
            outcome.stdout
        );
        assert!(
            outcome.stdout.contains(".bak (the previous file)"),
            "{}",
            outcome.stdout
        );
        assert!(
            outcome.stdout.contains("unset    model.openai.api_key"),
            "{}",
            outcome.stdout
        );
        assert!(
            !outcome.stdout.contains("inline-secret"),
            "{}",
            outcome.stdout
        );
        assert_eq!(
            std::fs::read_to_string(home.config()).expect("config"),
            existing
        );

        let choice = resolve_choice(
            &parse_args(&args(&["--preset", "openai"])).expect("args"),
            false,
            &mut Scripted(Vec::new()),
        )
        .expect("choice");
        let plan = plan(choice, &home.config(), Some(existing), &[], true, "T").expect("plan");
        assert!(
            plan.document.contains("# my models"),
            "comments kept:\n{}",
            plan.document
        );
        assert!(plan.document.contains("# the laptop"), "{}", plan.document);
        assert!(
            plan.document.contains("[model.local]"),
            "other profiles kept"
        );
        assert!(
            !plan.document.contains("inline-secret"),
            "{}",
            plan.document
        );
        let parsed = parse_config_document(&plan.document, "c").expect("parses");
        assert_eq!(parsed.models.default.as_deref(), Some("openai"));
        // The same plan against its own result is `unchanged`.
        let again = resolve_choice(
            &parse_args(&args(&["--preset", "openai"])).expect("args"),
            false,
            &mut Scripted(Vec::new()),
        )
        .expect("choice");
        let second =
            super::plan(again, &home.config(), Some(&plan.document), &[], true, "T").expect("plan");
        assert_eq!(second.action, FileAction::Unchanged);
        assert_eq!(second.backup, None);
    }

    #[test]
    fn an_existing_config_that_does_not_parse_is_left_untouched() {
        let home = Home::new("broken");
        std::fs::create_dir_all(home.config().parent().expect("dir")).expect("dir");
        std::fs::write(home.config(), "[models\n").expect("config");
        let outcome = run(
            &args(&["--preset", "ollama", "--dry-run"]),
            &home.env(),
            &mut Scripted(Vec::new()),
        );
        assert_eq!(outcome.exit, 1);
        assert!(
            outcome
                .stderr
                .contains("not valid TOML and is left untouched"),
            "{}",
            outcome.stderr
        );
    }

    #[test]
    fn a_key_on_the_command_line_is_refused_with_the_reason() {
        for flag in ["--key", "--api-key=sk-123", "--token", "--secret"] {
            let err =
                parse_args(&args(&["--preset", "openai", flag, "sk-123"])).expect_err("refused");
            assert!(err.contains("visible in the process list"), "{flag}: {err}");
            assert!(!err.contains("sk-123"), "the value is not echoed: {err}");
        }
        let err = parse_args(&args(&["--key-env", "sk-live-abc123"]));
        assert!(err.is_err(), "a key is not an environment variable name");
    }

    #[test]
    fn the_argument_surface_is_closed_and_consistent() {
        for (list, needle) in [
            (vec!["--preset"], "needs a value"),
            (vec!["--preset", "nope"], "unknown preset 'nope'"),
            (vec!["--bogus"], "unknown option --bogus"),
            (vec!["stray"], "unexpected argument 'stray'"),
            (vec!["--dry-run", "--dry-run"], "given twice"),
            (vec!["--key-env", "A", "--key-stdin"], "alternatives"),
            (vec!["--output", "yaml"], "text or json"),
            (vec!["--profile", "has space"], "--profile must be"),
            (
                vec!["--base-url", "http://user:pw@example.com"],
                "not usable",
            ),
            (vec!["--dry-run=yes"], "takes no value"),
        ] {
            let err = parse_args(&args(&list)).expect_err("refused");
            assert!(err.contains(needle), "{list:?}: {err}");
        }
        let parsed = parse_args(&args(&[
            "--preset=ollama",
            "--profile",
            "laptop",
            "--output=json",
            "--no-verify",
        ]))
        .expect("parses");
        assert_eq!(parsed.preset.as_deref(), Some("ollama"));
        assert_eq!(parsed.profile.as_deref(), Some("laptop"));
        assert_eq!(parsed.output, OutputFormat::Json);
        assert!(parsed.no_verify);
    }

    #[test]
    fn a_missing_choice_is_asked_on_a_terminal_and_a_usage_error_otherwise() {
        // Non-interactive: nothing to go on is a usage error naming the way out.
        let err = resolve_choice(&SetupArgs::default(), false, &mut Scripted(Vec::new()))
            .expect_err("refused");
        assert!(err.contains("choose --preset <id>"), "{err}");
        // On a terminal: the preset by number, the model by default.
        let choice = resolve_choice(&SetupArgs::default(), true, &mut Scripted(vec!["2", ""]))
            .expect("asked");
        assert_eq!(choice.preset.map(|p| p.id), Some("anthropic"));
        assert_eq!(choice.model, "claude-sonnet-5");
        assert_eq!(
            choice.credential,
            Credential::Env {
                var: "ANTHROPIC_API_KEY".to_owned()
            }
        );
        // By id, with a model of the user's own.
        let choice = resolve_choice(
            &SetupArgs::default(),
            true,
            &mut Scripted(vec!["ollama", "qwen3"]),
        )
        .expect("asked");
        assert_eq!(choice.base_url, "http://127.0.0.1:11434/v1");
        assert_eq!(choice.model, "qwen3");
        assert_eq!(choice.credential, Credential::None);
        // Input that ends is not a silent default.
        struct Ended;
        impl Prompter for Ended {
            fn ask(&mut self, _: &str) -> Option<String> {
                None
            }
        }
        assert!(resolve_choice(&SetupArgs::default(), true, &mut Ended).is_err());
        // --key-stdin: stdin carries the key, so nothing is asked.
        let home = Home::new("stdin");
        let outcome = run(
            &args(&["--key-stdin", "--dry-run"]),
            &SetupEnv {
                stdin_is_tty: true,
                ..home.env()
            },
            &mut Scripted(Vec::new()),
        );
        assert_eq!(outcome.exit, 2);
        assert!(
            outcome.stderr.contains("choose --preset"),
            "{}",
            outcome.stderr
        );
    }

    #[test]
    fn a_custom_endpoint_needs_a_model_and_a_stdin_key_names_its_keychain_alias() {
        let err = resolve_choice(
            &parse_args(&args(&["--base-url", "http://10.0.0.5:9000/v1"])).expect("args"),
            false,
            &mut Scripted(Vec::new()),
        )
        .expect_err("no model");
        assert!(err.contains("needs --model"), "{err}");
        let choice = resolve_choice(
            &parse_args(&args(&[
                "--base-url",
                "http://10.0.0.5:9000/v1",
                "--model",
                "m",
                "--key-stdin",
                "--profile",
                "gw",
            ]))
            .expect("args"),
            false,
            &mut Scripted(Vec::new()),
        )
        .expect("choice");
        assert_eq!(choice.dialect, ConfigProvider::OpenAiCompatible);
        assert_eq!(
            choice.credential,
            Credential::Keychain {
                alias: "rapidlm-model-gw".to_owned()
            }
        );
        let plan = plan(choice, Path::new("c.toml"), None, &[], false, "T").expect("plan");
        let text = render_text(&plan, true);
        assert!(text.contains("OS keychain as 'rapidlm-model-gw'"), "{text}");
        assert!(text.contains("verify   skipped (--no-verify)"), "{text}");
    }

    #[test]
    fn the_config_target_is_the_file_a_run_would_read() {
        let home = Home::new("target");
        let env = |pairs: &[(&str, String)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect()
        };
        let root = home.0.display().to_string();
        assert_eq!(
            config_target(&env(&[(CONFIG_PATH_ENV, "/x/c.toml".to_owned())])).expect("path"),
            PathBuf::from("/x/c.toml")
        );
        assert_eq!(
            config_target(&env(&[
                (RAPIDLM_HOME_ENV, root.clone()),
                (HOME_ENV, root.clone())
            ]))
            .expect("path"),
            home.0.join("config.toml"),
            "nothing exists yet: the first convention the reader checks"
        );
        std::fs::create_dir_all(home.0.join(".rapidlm")).expect("dir");
        std::fs::write(home.config(), "").expect("file");
        assert_eq!(
            config_target(&env(&[(RAPIDLM_HOME_ENV, root.clone()), (HOME_ENV, root)]))
                .expect("path"),
            home.config(),
            "an existing file is the one edited"
        );
        assert!(config_target(&[]).is_err());
    }

    #[test]
    fn a_run_without_dry_run_is_refused_until_verification_exists() {
        let home = Home::new("no-dry-run");
        let before = home.snapshot();
        let outcome = run(
            &args(&["--preset", "ollama", "--non-interactive"]),
            &home.env(),
            &mut Scripted(Vec::new()),
        );
        assert_eq!(outcome.exit, 2);
        assert!(
            outcome
                .stderr
                .contains("--dry-run prints what would be written"),
            "{}",
            outcome.stderr
        );
        assert_eq!(home.snapshot(), before);
    }
}
