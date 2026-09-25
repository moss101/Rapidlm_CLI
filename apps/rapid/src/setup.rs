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
    /// Neither `--key-env` nor `--key-stdin`: a preset's key variable on
    /// the preset's own origin, otherwise whatever the profile names.
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
                      endpoint is taken to speak the openai-compatible dialect; a
                      preset's key variable goes only to the preset's own origin
                      (scheme, host and port; --key-env names one for another)
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
  11  verification: the key was refused or required, or it could not be sent
      (its variable is not set, or it is held where this build does not read it)
  12  verification: no quota or credit left, or rate-limited
  13  verification: the endpoint could not be reached, rapid does not dial it,
      or a proxy on the way refused its credentials
  14  verification: the endpoint failed on its side
  15  verification: the endpoint answered, but not usably
A failed verification changes no file.
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
            Some((flag, value)) if flag.starts_with('-') => (flag, Some(value)),
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
            // Nothing the user typed is echoed unless it is plainly an
            // option name: a key pasted in the wrong place must not land in
            // a terminal log.
            return Err(if !flag.starts_with('-') {
                "rapid setup: unexpected argument — setup takes only --options (a key is never \
given on the command line)"
                    .to_owned()
            } else if looks_like_an_option_name(flag) {
                format!("rapid setup: unknown option {flag}")
            } else {
                "rapid setup: unknown option".to_owned()
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
                    _ => {
                        return Err("rapid setup: --output takes text or json".to_owned());
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
            "rapid setup: unknown preset (one of: {})",
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

/// `-x` / `--word-word`: short enough and plain enough to be a typo, not a key.
fn looks_like_an_option_name(flag: &str) -> bool {
    let name = flag.trim_start_matches('-');
    (1..=2).contains(&(flag.len() - name.len()))
        && (1..=24).contains(&name.len())
        && name.starts_with(|ch: char| ch.is_ascii_lowercase())
        && name.chars().all(|ch| ch.is_ascii_lowercase() || ch == '-')
}

/// A flag whose value would be a secret: refused by name, with the reason.
fn is_key_bearing_flag(flag: &str) -> bool {
    matches!(
        flag,
        "--key" | "--api-key" | "--apikey" | "--token" | "--secret" | "--password"
    )
}

/// The run-time profile alphabet (`llm_router::ProfileId`): a profile setup
/// accepts is one every later run accepts.
fn validate_profile_id(raw: &str) -> Result<String, String> {
    llm_router::credentials::ProfileId::parse(raw)
        .map(|_| raw.to_owned())
        .map_err(|_| {
            "rapid setup: --profile must start with a lower-case letter and use only a-z, 0-9 \
and single '-' (not at the end)"
                .to_owned()
        })
}

/// The model client's own identifier rule (`llm_router::provider::ModelId`).
fn validate_model_id(raw: &str) -> Result<String, String> {
    llm_router::provider::ModelId::parse(raw)
        .map(|_| raw.to_owned())
        .map_err(|_| {
            "rapid setup: --model must be ASCII letters, digits and '-_./:', starting with a \
letter or digit"
                .to_owned()
        })
}

/// The conventional environment-variable form, upper case: a key pasted
/// here (they carry lower case, '-' or both) is refused, and never echoed.
fn validate_env_name(raw: &str) -> Result<String, String> {
    let mut chars = raw.chars();
    let valid_start = chars
        .next()
        .is_some_and(|ch| ch.is_ascii_uppercase() || ch == '_');
    if !valid_start
        || raw.len() > MAX_ENV_NAME_BYTES
        || !chars.all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
    {
        return Err(
            "rapid setup: --key-env takes the NAME of an environment variable (upper-case \
letters, digits, '_'), never a key"
                .to_owned(),
        );
    }
    Ok(raw.to_owned())
}

/// The model client's own `base_url` validation, so a URL setup accepts is
/// one a turn can use. The value is stored as given (without a trailing
/// `/`), and a rejected one is never echoed — it may carry a credential.
fn validate_base_url(raw: &str) -> Result<String, String> {
    OpenAiCompatibleEndpoint::new(raw, OpenAiApiStyle::ChatCompletions)
        .map(|_| raw.trim_end_matches('/').to_owned())
        .map_err(|_| {
            "rapid setup: --base-url is not usable: expected an http:// or https:// origin \
without userinfo or metadata hosts"
                .to_owned()
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
    /// The profile keeps the credential it names — but only for the origin
    /// it was set up for; on another origin it is removed (a custom endpoint,
    /// or a preset's `--base-url` on another origin, with no key flag: setup
    /// does not guess, and never sends a key for one host to another). A
    /// keyless preset on its own origin is `None` instead: its server takes
    /// no key.
    Unchanged,
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
        KeyFlag::Unspecified => match chosen {
            // A preset's key goes only to the preset's own endpoint: with a
            // `--base-url` on another origin the profile keeps what it
            // already names for that endpoint (the custom-endpoint rule),
            // never the provider's key variable.
            Some(preset) if !same_origin(preset.base_url, &base_url) => Credential::Unchanged,
            Some(Preset {
                key_env: Some(var), ..
            }) => Credential::Env {
                var: (*var).to_owned(),
            },
            Some(_) => Credential::None,
            None => Credential::Unchanged,
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
        .ok_or_else(|| format!("rapid setup: unknown preset (one of: {})", preset_ids()))
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
    /// Things the user should know before the file is written (a removed
    /// credential, an override from this shell, a key a run would ignore).
    pub notes: Vec<String>,
    /// Keys of the profile setup leaves as they are (`max_tokens`, …).
    pub kept: Vec<String>,
    /// For `Credential::Unchanged`: the credential key the profile keeps.
    pub kept_credential: Option<String>,
    /// Whether the credential's environment variable is set right now (for
    /// `Credential::Env`): the plan says so, never what it holds.
    pub env_key_present: Option<bool>,
    /// When a run would use another profile than the one written — the
    /// `RAPIDLM_MODEL` override or a managed locked default — which one and
    /// the layer that decides it.
    pub effective: Option<(String, &'static str)>,
    /// The symlink the config path is reached through, when it is one: the
    /// file behind it is the one written.
    pub via_symlink: Option<PathBuf>,
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
    policy: Option<&crate::managed_config::ManagedPolicy>,
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
    let mut notes = Vec::new();
    let kept: Vec<String>;
    let profile_keys: Vec<String>;
    let mut kept_credential = None;
    let mut changed = existing.is_none();
    {
        let (models, _) = table_at(
            doc.as_table_mut(),
            "models",
            false,
            false,
            config_path,
            &mut changed,
        )?;
        changed |= set_value(models, "default", &choice.profile, "models", &mut set);
    }
    {
        let (model, model_inline) = table_at(
            doc.as_table_mut(),
            "model",
            false,
            true,
            config_path,
            &mut changed,
        )?;
        let (profile, _) = table_at(
            model,
            &choice.profile,
            model_inline,
            false,
            config_path,
            &mut changed,
        )?;
        let prefix = format!("model.{}", choice.profile);
        // The endpoint the profile's existing credential was set up for.
        let previous_base_url = profile
            .get("base_url")
            .and_then(toml_edit::Item::as_str)
            .map(str::to_owned);
        changed |= set_value(
            profile,
            "provider",
            choice.dialect.as_str(),
            &prefix,
            &mut set,
        );
        changed |= set_value(profile, "model", &choice.model, &prefix, &mut set);
        changed |= set_value(profile, "base_url", &choice.base_url, &prefix, &mut set);
        let credential_keys = ["api_key", "env_key", "keychain"];
        let keep = match &choice.credential {
            Credential::Env { var } => {
                changed |= set_value(profile, "env_key", var, &prefix, &mut set);
                Some("env_key")
            }
            Credential::Keychain { alias } => {
                changed |= set_value(profile, "keychain", alias, &prefix, &mut set);
                Some("keychain")
            }
            Credential::None => None,
            Credential::Unchanged => {
                // The resolver's own precedence: an inline key wins, then
                // the variable, then the keychain.
                let existing_key = credential_keys
                    .iter()
                    .find(|key| profile.contains_key(key))
                    .map(|key| format!("{prefix}.{key}"));
                // A kept credential goes only to the endpoint it was set up
                // for: a key for one host is never sent to another.
                let same_origin = previous_base_url
                    .as_deref()
                    .is_some_and(|previous| same_origin(previous, &choice.base_url));
                match existing_key {
                    Some(_) if !same_origin => {
                        notes.push(
                            "the profile's key was set up for another endpoint and is removed; \
--key-env or --key-stdin adds one for this one"
                                .to_owned(),
                        );
                        None
                    }
                    key => {
                        kept_credential = key;
                        Some("*")
                    }
                }
            }
        };
        if let (
            Credential::Unchanged,
            None,
            Some(Preset {
                key_env: Some(var), ..
            }),
        ) = (&choice.credential, &kept_credential, choice.preset)
        {
            notes.push(format!(
                "{var} is sent only to the preset's own endpoint; --key-env {var} sends it to \
this --base-url"
            ));
        }
        // Credential keys other than the chosen one would shadow it (an
        // inline `api_key` wins over everything) or contradict it.
        if keep != Some("*") {
            for key in credential_keys {
                if Some(key) != keep && profile.remove(key).is_some() {
                    unset.push(format!("{prefix}.{key}"));
                    changed = true;
                }
            }
        }
        profile_keys = profile.iter().map(|(key, _)| key.to_owned()).collect();
        let written: Vec<&str> = ["provider", "model", "base_url"]
            .into_iter()
            .chain(credential_keys)
            .collect();
        kept = profile
            .iter()
            .map(|(key, _)| key.to_owned())
            .filter(|key| !written.contains(&key.as_str()))
            .map(|key| format!("{prefix}.{key}"))
            .collect();
    }
    // Nothing changed: the file stays byte for byte as it is — CRLF line
    // endings, a byte-order mark and a missing final newline included, which
    // re-serialising would otherwise normalise into an "update".
    let document = match existing {
        Some(previous) if !changed => previous.to_owned(),
        _ => with_original_line_endings(existing, doc.to_string()),
    };
    if document.len() > crate::user_config::MAX_USER_CONFIG_BYTES {
        return Err(format!(
            "rapid setup: {} would exceed {} bytes, the most a run reads; it is left untouched",
            config_path.display(),
            crate::user_config::MAX_USER_CONFIG_BYTES
        ));
    }
    // What a turn will read must parse — and resolve, through the same
    // managed gates a run applies, as the file decides it: setup never
    // leaves a config a run refuses (a provider outside a managed
    // allowlist, say). `RAPIDLM_MODEL` and the proxy settings
    // (`RAPIDLM_PROXY`, the proxy variables) are set aside for this: they are
    // this shell's choices, not the file's, and must neither refuse a good
    // plan nor let a bad one through.
    let parsed =
        parse_config_document(&document, &config_path.display().to_string()).map_err(|err| {
            format!(
                "rapid setup: {} would not parse after the change and is left untouched: {err}",
                config_path.display()
            )
        })?;
    let file_env: Vec<(String, String)> = env
        .iter()
        .filter(|(key, _)| {
            key != crate::user_config::DEFAULT_MODEL_ENV
                && !crate::user_config::is_shell_proxy_setting(key)
        })
        .cloned()
        .collect();
    let resolution =
        crate::managed_config::resolve_gated(&file_env, &parsed, policy).map_err(|err| {
            format!(
                "rapid setup: a run would refuse the configuration this writes, so {} is left \
untouched: {err}",
                config_path.display()
            )
        })?;
    // Under a lock the gate above judged the locked profile: the one this
    // writes becomes the file's default, so it must pass the allowlist too.
    if let Some(allowed) = policy.and_then(crate::managed_config::ManagedPolicy::allowed_providers)
        && !allowed.iter().any(|name| name == choice.dialect.as_str())
    {
        return Err(format!(
            "rapid setup: the managed policy allows only the providers {}, not {}, which this \
would make the default, so {} is left untouched",
            allowed.join(", "),
            choice.dialect.as_str(),
            config_path.display()
        ));
    }
    let effective = if resolution.active.profile_id != choice.profile {
        // A managed locked default.
        Some((
            resolution.active.profile_id.clone(),
            resolution.default_origin.as_str(),
        ))
    } else {
        match crate::managed_config::resolve_gated(env, &parsed, policy) {
            Ok(real) if real.active.profile_id != choice.profile => {
                Some((real.active.profile_id, real.default_origin.as_str()))
            }
            Ok(_) => None,
            Err(err) => {
                // Name the settings of this shell the file was judged
                // without: those are what make a run here differ.
                let mut set_aside = Vec::new();
                if env_value(env, crate::user_config::DEFAULT_MODEL_ENV).is_some() {
                    set_aside.push("RAPIDLM_MODEL");
                }
                if env
                    .iter()
                    .any(|(key, _)| crate::user_config::is_shell_proxy_setting(key))
                {
                    set_aside.push("proxy settings");
                }
                if set_aside.is_empty() {
                    set_aside.push("settings");
                }
                notes.push(format!(
                    "with this shell's {} a run would fail: {err}",
                    set_aside.join(" and ")
                ));
                None
            }
        }
    };
    // Keys of this profile a run would not read (a newer key than this
    // build's reader knows) are named, not silently written.
    let profile_prefix = format!("model.{}.", choice.profile);
    // Exactly this profile's own keys: a quoted id with a dot is another
    // profile, a quoted key with a dot is this one's.
    for key in profile_keys
        .iter()
        .map(|key| format!("{profile_prefix}{key}"))
        .filter(|key| parsed.unknown_keys.contains(key))
    {
        notes.push(format!(
            "a run would ignore {key}: this build does not read it"
        ));
    }
    let action = match existing {
        None => FileAction::Create,
        Some(_) if !changed => FileAction::Unchanged,
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
        notes,
        kept,
        kept_credential,
        env_key_present,
        effective,
        via_symlink: None,
        verify,
        document,
    })
}

/// A re-serialised document in the original's line-ending convention: CRLF
/// kept when the original used it, and its byte-order mark.
fn with_original_line_endings(original: Option<&str>, rendered: String) -> String {
    let Some(original) = original else {
        return rendered;
    };
    let crlf = original.matches("\r\n").count();
    let lines = original.matches('\n').count();
    let mut out = if crlf * 2 > lines {
        rendered.replace("\r\n", "\n").replace('\n', "\r\n")
    } else {
        rendered
    };
    if original.starts_with('\u{feff}') && !out.starts_with('\u{feff}') {
        out.insert(0, '\u{feff}');
    }
    out
}

/// The table at `key` in `parent` — a standard or an inline table, edited in
/// place either way (converting an inline table would drop its comments) —
/// created when absent, inline inside an inline parent. Any other value
/// there is an error naming it. Returns whether it is inline; `changed`
/// records a creation.
fn table_at<'a>(
    parent: &'a mut dyn toml_edit::TableLike,
    key: &str,
    parent_inline: bool,
    implicit: bool,
    config_path: &Path,
    changed: &mut bool,
) -> Result<(&'a mut dyn toml_edit::TableLike, bool), String> {
    if !parent.contains_key(key) {
        let item = if parent_inline {
            toml_edit::Item::Value(toml_edit::Value::InlineTable(toml_edit::InlineTable::new()))
        } else {
            let mut table = toml_edit::Table::new();
            table.set_implicit(implicit);
            toml_edit::Item::Table(table)
        };
        parent.insert(key, item);
        *changed = true;
    }
    let not_a_table = || {
        format!(
            "rapid setup: `{key}` in {} is not a table; fix it by hand",
            config_path.display()
        )
    };
    let item = parent.get_mut(key).ok_or_else(not_a_table)?;
    let inline = item.is_inline_table();
    let table = item.as_table_like_mut().ok_or_else(not_a_table)?;
    Ok((table, inline))
}

/// Set `table[key] = value` (a string) and record it. An existing entry is
/// replaced in place: the comment lines above the key and the comment after
/// the value survive. `true` when the file changes.
fn set_value(
    table: &mut dyn toml_edit::TableLike,
    key: &str,
    value: &str,
    prefix: &str,
    set: &mut Vec<(String, String)>,
) -> bool {
    let changed = match table.get_mut(key) {
        Some(item) if item.as_str() == Some(value) => false,
        Some(item) => {
            match item.as_value_mut() {
                Some(existing) => {
                    let decor = existing.decor().clone();
                    *existing = toml_edit::Value::from(value);
                    *existing.decor_mut() = decor;
                }
                None => *item = toml_edit::value(value),
            }
            true
        }
        None => {
            table.insert(key, toml_edit::value(value));
            true
        }
    };
    // Only what changes is listed: the plan names exactly what it writes.
    if changed {
        set.push((format!("{prefix}.{key}"), value.to_owned()));
    }
    changed
}

/// Whether two URLs name the same origin (scheme, host, port — default
/// ports filled in, case-insensitive host).
fn same_origin(left: &str, right: &str) -> bool {
    match (origin_of(left), origin_of(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

fn origin_of(url: &str) -> Option<(String, String, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit('@').next()?;
    let default_port = match scheme.as_str() {
        "https" => 443,
        "http" => 80,
        _ => return None,
    };
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let (host, after) = bracketed.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(port) => port.parse().ok()?,
            None => default_port,
        };
        (host.to_owned(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host.to_owned(), port.parse().ok()?),
            None => (authority.to_owned(), default_port),
        }
    };
    Some((scheme, host.to_ascii_lowercase(), port))
}

/// The plan as the text a person reads.
pub fn render_text(plan: &SetupPlan, dry_run: bool) -> String {
    let mut out = String::new();
    if dry_run {
        out.push_str("rapid setup — dry run: nothing was written and no request was made\n");
    }
    let path = match &plan.via_symlink {
        Some(link) => format!(
            "{} (through {})",
            plan.config_path.display(),
            link.display()
        ),
        None => plan.config_path.display().to_string(),
    };
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
    if !plan.kept.is_empty() {
        out.push_str(&format!("  kept     {}\n", plan.kept.join(", ")));
    }
    for note in &plan.notes {
        out.push_str(&format!("  note     {note}\n"));
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
        Credential::Unchanged => match &plan.kept_credential {
            Some(key) => format!("  key      {key} is kept as it is\n"),
            None => {
                "  key      none — the endpoint is called without credentials (--key-env or \
--key-stdin adds one)\n"
                    .to_owned()
            }
        },
    });
    if let Some((profile, origin)) = &plan.effective {
        out.push_str(&format!(
            "  note     runs will use profile '{profile}' (decided by the {origin} layer), not \
'{}'\n",
            plan.choice.profile
        ));
    }
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
        Credential::Unchanged => match &plan.kept_credential {
            Some(key) => serde_json::json!({ "source": "unchanged", "key": key }),
            None => serde_json::json!({ "source": "none" }),
        },
    };
    serde_json::json!({
        "schema": "rapidlm.setup_plan/v1",
        "dry_run": dry_run,
        "profile": plan.choice.profile,
        "preset": plan.choice.preset.map(|preset| preset.id),
        "files": [{
            "path": plan.config_path.display().to_string(),
            "via_symlink": plan.via_symlink.as_ref().map(|link| link.display().to_string()),
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
        "kept": plan.kept,
        "notes": plan.notes,
        "credential": credential,
        "effective_profile": plan.effective.as_ref().map(|(profile, _)| profile),
        "effective_origin": plan.effective.as_ref().map(|(_, origin)| origin),
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
    /// Reads the key `--key-stdin` names (stdin in production).
    pub read_key: fn() -> std::io::Result<String>,
}

impl SetupEnv {
    pub fn from_process() -> Self {
        use std::io::IsTerminal;
        Self {
            env: std::env::vars().collect(),
            stdin_is_tty: std::io::stdin().is_terminal(),
            read_key: read_stdin_key,
        }
    }
}

/// At most this much is read as a key from stdin.
const MAX_STDIN_KEY_BYTES: u64 = 8 * 1024;

/// The key `--key-stdin` supplies: stdin to its end, bounded, trimmed.
pub fn read_stdin_key() -> std::io::Result<String> {
    use std::io::Read;
    let mut key = String::new();
    std::io::stdin()
        .lock()
        .take(MAX_STDIN_KEY_BYTES)
        .read_to_string(&mut key)?;
    Ok(key.trim().to_owned())
}

/// Why the verification request failed. Each class is its own exit code and
/// hint, and every one is reported with "no files were changed".
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProbeFailure {
    /// The key's environment variable is not set: nothing was sent.
    NoKey { var: String },
    /// The endpoint refused a keyless request, and the profile names its key
    /// where this build does not read it (the `keychain` alias until the
    /// reader learns it).
    KeyNotRead { key: String },
    /// The endpoint refused a keyless request, and the variable the profile
    /// names for its key is not set.
    KeptKeyUnset { var: String },
    /// The key has characters no HTTP header can carry: nothing was sent.
    UnusableKey,
    /// The endpoint refused the key (401/403).
    Auth,
    /// The endpoint requires a key, and none was sent (401/403 to a request
    /// without one).
    AuthNoKey,
    /// No quota or credit left, or rate-limited (402/429).
    Quota,
    /// The endpoint could not be reached (DNS, connect, TLS, timeout).
    Network,
    /// The endpoint failed on its side (5xx).
    Server,
    /// Not sent: rapid does not dial this address (an unspecified,
    /// link-local or metadata address).
    Refused,
    /// The proxy the request goes through refused its own credentials.
    ProxyAuth,
    /// It answered, but not as this dialect's server would, or refused the
    /// request itself (unknown model, malformed body).
    Invalid,
}

impl ProbeFailure {
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::NoKey { .. }
            | Self::KeyNotRead { .. }
            | Self::KeptKeyUnset { .. }
            | Self::UnusableKey
            | Self::Auth
            | Self::AuthNoKey => 11,
            Self::Quota => 12,
            Self::Network | Self::Refused | Self::ProxyAuth => 13,
            Self::Server => 14,
            Self::Invalid => 15,
        }
    }

    pub const fn class(&self) -> &'static str {
        match self {
            Self::NoKey { .. }
            | Self::KeyNotRead { .. }
            | Self::KeptKeyUnset { .. }
            | Self::UnusableKey
            | Self::Auth
            | Self::AuthNoKey => "auth",
            Self::Quota => "quota",
            Self::Network | Self::Refused | Self::ProxyAuth => "network",
            Self::Server => "server",
            Self::Invalid => "invalid",
        }
    }

    /// The next step, naming the command. Never a credential or a response.
    pub fn hint(&self, plan: &SetupPlan) -> String {
        let endpoint = origin_of(&plan.choice.base_url)
            .map(|(scheme, host, port)| format!("{scheme}://{host}:{port}"))
            .unwrap_or_else(|| "the endpoint".to_owned());
        match self {
            Self::NoKey { var } => format!(
                "${var} is not set, so no request was made — export it, then run rapid setup again"
            ),
            Self::KeyNotRead { key } => format!(
                "{endpoint} requires a key, and the profile names it in {key}, which this build \
does not read — give the key with rapid setup --key-env <VAR>"
            ),
            Self::KeptKeyUnset { var } => format!(
                "{endpoint} requires a key, and ${var}, which the profile names for it, is not \
set — export it, then run rapid setup again"
            ),
            Self::UnusableKey => "the key cannot be sent: it has characters no HTTP header can \
carry (a line break from a file?) or is too long, so no request was made — check it, then run \
rapid setup again"
                .to_owned(),
            Self::Auth => format!(
                "{endpoint} refused the key — check it, or give another with rapid setup \
--key-env <VAR> or --key-stdin"
            ),
            Self::AuthNoKey => format!(
                "{endpoint} requires a key and none was sent — give one with rapid setup \
--key-env <VAR> or --key-stdin"
            ),
            Self::Quota => format!(
                "{endpoint} reports no quota or credit left, or a rate limit — check the account's \
billing and limits, then run rapid setup again"
            ),
            Self::Network => format!(
                "{endpoint} could not be reached — check --base-url and the network, then run \
rapid setup again"
            ),
            Self::Server => format!("{endpoint} failed on its side — run rapid setup again later"),
            Self::ProxyAuth => "the proxy on the way refused its credentials — check the user and \
password in the proxy variable (https_proxy or http_proxy), then run rapid setup again"
                .to_owned(),
            Self::Refused => format!(
                "rapid does not dial {endpoint} (an unspecified, broadcast, multicast, link-local \
or metadata address), so no request was made — check --base-url, then run rapid setup again"
            ),
            Self::Invalid => format!(
                "{endpoint} answered, but not as a {} server would, or refused the request — check \
--base-url and --model, then run rapid setup again",
                plan.choice.dialect.as_str()
            ),
        }
    }
}

/// The failure class of a provider error.
pub fn classify(err: &llm_router::provider::ProviderError) -> ProbeFailure {
    use llm_router::provider::ProviderError as E;
    match err {
        E::AuthFailed => ProbeFailure::Auth,
        E::QuotaExceeded | E::RateLimited { .. } => ProbeFailure::Quota,
        E::ProxyRefused => ProbeFailure::ProxyAuth,
        E::Connection | E::Cancelled => ProbeFailure::Network,
        E::Transient => ProbeFailure::Server,
        // Raised before anything is sent (the address guards).
        E::InvalidRequest => ProbeFailure::Refused,
        E::ContextTooLarge | E::Permanent | E::BoundExceeded | E::UnknownVariant => {
            ProbeFailure::Invalid
        }
    }
}

/// The egress gate the probe dials through: exactly the planned endpoint,
/// and the proxy a run would reach it through — the planned config's
/// `[network] proxy` (or `RAPIDLM_PROXY`) read as a run reads it.
pub fn probe_egress(
    plan: &SetupPlan,
    env: &[(String, String)],
) -> Result<crate::provider_egress::ProviderEgress, String> {
    let parsed = parse_config_document(&plan.document, "the planned config")
        .map_err(|err| err.to_string())?;
    let proxy = crate::user_config::resolve_proxy(env, &parsed).map_err(|err| err.to_string())?;
    endpoint_egress(&plan.choice.base_url, proxy.as_ref())
}

/// The egress gate for one endpoint: exactly its origin, and the proxy
/// `proxy` routes it through (if any).
pub fn endpoint_egress(
    base_url: &str,
    proxy: Option<&llm_router::providers::dial::ProxyConfig>,
) -> Result<crate::provider_egress::ProviderEgress, String> {
    let (scheme, host, port) =
        origin_of(base_url).ok_or_else(|| "the endpoint has no origin".to_owned())?;
    let https = scheme == "https";
    let via = proxy
        .and_then(|proxy| proxy.for_target(https, &host, port))
        .map(|proxy| (proxy.host(), proxy.port()));
    crate::provider_egress::ProviderEgress::for_endpoint(https, &host, port, via)
}

/// What one live probe of a configured model found (`rapid doctor --live`).
#[derive(Debug)]
pub struct LiveProbe {
    /// `scheme://host:port` of the endpoint probed.
    pub endpoint: String,
    pub result: Result<(), ProbeFailure>,
    /// The egress gate's receipt: one per dial asked for.
    pub receipts: Vec<crate::provider_egress::EgressReceipt>,
}

/// The live probe of a model as a run resolved it: the same bounded request
/// as `rapid setup`'s verification, through the client a run builds (its
/// key, its proxy), on an egress gate for exactly its endpoint. `Err` when
/// the probe could not be set up (nothing was dialled).
pub fn probe_resolved(
    active: &crate::user_config::ActiveModel,
    cancel: &llm_router::provider::CancellationToken,
) -> Result<LiveProbe, String> {
    let endpoint = origin_of(&active.entry.base_url)
        .map(|(scheme, host, port)| format!("{scheme}://{host}:{port}"))
        .ok_or_else(|| "base_url has no origin".to_owned())?;
    let egress = std::sync::Arc::new(endpoint_egress(
        &active.entry.base_url,
        active.proxy.as_ref(),
    )?);
    let mut active = active.clone();
    active.entry.max_tokens = Some(VERIFY_MAX_OUTPUT_TOKENS);
    let sends_key = active.credential.plaintext.is_some();
    let store = auth::InMemoryCredentialStore::new();
    let gate: std::sync::Arc<dyn llm_router::providers::dial::DialGate> = egress.clone();
    let result = match crate::model::ConfiguredModel::build_with_gate(&active, &store, Some(gate)) {
        Err(crate::model::ModelConfigError::Credential { .. }) => Err(ProbeFailure::UnusableKey),
        Err(crate::model::ModelConfigError::BaseUrl { .. }) => Err(ProbeFailure::Refused),
        Err(err) => return Err(err.to_string()),
        Ok(model) => model.probe(cancel).map_err(|err| match classify(&err) {
            ProbeFailure::Auth if !sends_key => ProbeFailure::AuthNoKey,
            failure => failure,
        }),
    };
    Ok(LiveProbe {
        endpoint,
        result,
        receipts: egress.receipts(),
    })
}

/// The live verification (SEAM-02): one request of at most
/// [`VERIFY_MAX_OUTPUT_TOKENS`] through the model client a run would build
/// for the planned profile — the config as the plan leaves it, the key as
/// the plan names it (`stdin_key` for `--key-stdin`). Nothing is written.
pub fn verify(
    plan: &SetupPlan,
    env: &[(String, String)],
    stdin_key: Option<&str>,
    policy: Option<&crate::managed_config::ManagedPolicy>,
    egress: &std::sync::Arc<crate::provider_egress::ProviderEgress>,
    cancel: &llm_router::provider::CancellationToken,
) -> Result<(), ProbeFailure> {
    if let Credential::Env { var } = &plan.choice.credential
        && env_value(env, var).is_none_or(str::is_empty)
    {
        return Err(ProbeFailure::NoKey { var: var.clone() });
    }
    let parsed = parse_config_document(&plan.document, "the planned config")
        .map_err(|_| ProbeFailure::Invalid)?;
    // The planned profile, whatever this shell's RAPIDLM_MODEL names.
    let mut probe_env: Vec<(String, String)> = env
        .iter()
        .filter(|(key, _)| key != crate::user_config::DEFAULT_MODEL_ENV)
        .cloned()
        .collect();
    probe_env.push((
        crate::user_config::DEFAULT_MODEL_ENV.to_owned(),
        plan.choice.profile.clone(),
    ));
    let mut active = crate::user_config::resolve_active(&probe_env, &parsed)
        .map_err(|_| ProbeFailure::Invalid)?;
    if let Credential::Keychain { .. } = &plan.choice.credential {
        active.credential = crate::user_config::ResolvedCredential {
            plaintext: stdin_key.map(str::to_owned),
            source: crate::user_config::CredentialSource::InlineApiKey,
        };
    }
    // The request a run would make: the managed effort floor applies.
    let mut active = crate::managed_config::apply_to_fallback_candidate(active, policy)
        .map_err(|_| ProbeFailure::Invalid)?;
    active.entry.max_tokens = Some(VERIFY_MAX_OUTPUT_TOKENS);
    // Keyless as a run would be (a local server needs no key, whatever its
    // profile names); only a refusal of that keyless request says why no key
    // was sent.
    let sends_key = active.credential.plaintext.is_some();
    let kept_keychain = plan
        .kept_credential
        .as_deref()
        .filter(|key| key.ends_with(".keychain"))
        .map(str::to_owned);
    let unset_var = active.entry.env_key.first().cloned();
    let store = auth::InMemoryCredentialStore::new();
    let gate: std::sync::Arc<dyn llm_router::providers::dial::DialGate> = egress.clone();
    let model = crate::model::ConfiguredModel::build_with_gate(&active, &store, Some(gate))
        .map_err(|err| match err {
            crate::model::ModelConfigError::Credential { .. } => ProbeFailure::UnusableKey,
            crate::model::ModelConfigError::BaseUrl { .. } => ProbeFailure::Refused,
            _ => ProbeFailure::Invalid,
        })?;
    model.probe(cancel).map_err(|err| match classify(&err) {
        ProbeFailure::Auth if !sends_key => match (kept_keychain, unset_var) {
            (Some(key), _) => ProbeFailure::KeyNotRead { key },
            (None, Some(var)) => ProbeFailure::KeptKeyUnset { var },
            // A preset on its own origin always sends its variable (or stops
            // before sending); here the key is deliberately not the preset's.
            (None, None) => ProbeFailure::AuthNoKey,
        },
        failure => failure,
    })
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
    // A key typed at a terminal would echo and stay in its scrollback.
    // (`--no-verify` reads no key until the key is stored — SEAM-02-3.)
    if parsed.key == KeyFlag::Stdin && env.stdin_is_tty && !parsed.dry_run && !parsed.no_verify {
        return usage_error(
            "rapid setup: --key-stdin reads the key from a pipe, and stdin is a terminal (the key \
would echo) — pipe it in: printf '%s' \"$KEY\" | rapid setup --key-stdin ...; no files were \
changed"
                .to_owned(),
        );
    }
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
    let failed = |message: String| SetupOutcome {
        stdout: String::new(),
        stderr: format!("{message}\n"),
        exit: 1,
    };
    // A symlinked config is edited where it points: the reader follows the
    // link, and replacing the link itself would detach it.
    let link = config_path
        .symlink_metadata()
        .is_ok_and(|meta| meta.file_type().is_symlink())
        .then(|| config_path.clone());
    let config_path = match &link {
        Some(link) => match resolve_symlinks(link) {
            Ok(target) => target,
            Err(message) => return failed(message),
        },
        None => config_path,
    };
    let existing = match read_existing(&config_path) {
        Ok(existing) => existing,
        Err(message) => return failed(message),
    };
    let policy = match crate::managed_config::load_policy(&env.env) {
        Ok(policy) => policy,
        Err(err) => {
            return failed(format!(
                "rapid setup: the managed policy could not be loaded, so nothing is planned: {err}"
            ));
        }
    };
    let now = compact_utc_now();
    let mut plan = match plan(
        choice,
        &config_path,
        existing.as_deref(),
        &env.env,
        policy.as_ref(),
        !parsed.no_verify,
        &now,
    ) {
        Ok(plan) => plan,
        Err(message) => return failed(message),
    };
    plan.via_symlink = link;
    if !parsed.dry_run {
        let stdin_key = match (&plan.choice.credential, parsed.no_verify) {
            (Credential::Keychain { .. }, false) => match (env.read_key)() {
                Ok(key) if !key.trim().is_empty() => Some(key.trim().to_owned()),
                _ => {
                    return usage_error(
                        "rapid setup: --key-stdin read no key from stdin; no files were changed"
                            .to_owned(),
                    );
                }
            },
            _ => None,
        };
        // What the plan says about the key and the profile — a dry run
        // prints it with the plan.
        let notes: String = plan
            .notes
            .iter()
            .map(|note| format!("note: {note}\n"))
            .collect();
        // The probe's receipt (S10): every dial decision its egress gate
        // made, allowed or refused — reported, never written (AC-02).
        let mut receipts = Vec::new();
        if !parsed.no_verify {
            let egress = match probe_egress(&plan, &env.env) {
                Ok(egress) => std::sync::Arc::new(egress),
                Err(message) => {
                    return SetupOutcome {
                        stdout: String::new(),
                        stderr: format!(
                            "rapid setup: the planned endpoint cannot be verified ({message}); \
no files were changed\n"
                        ),
                        exit: 2,
                    };
                }
            };
            let cancel = llm_router::provider::CancellationToken::new();
            let verified = verify(
                &plan,
                &env.env,
                stdin_key.as_deref(),
                policy.as_ref(),
                &egress,
                &cancel,
            );
            receipts = egress.receipts();
            if let Err(failure) = verified {
                let hint = failure.hint(&plan);
                let stdout = match parsed.output {
                    OutputFormat::Json => format!(
                        "{}\n",
                        serde_json::json!({
                            "schema": "rapidlm.setup_outcome/v1",
                            "verified": false,
                            "class": failure.class(),
                            "hint": hint,
                            "egress": receipts_json(&receipts),
                            "written": false,
                        })
                    ),
                    OutputFormat::Text => String::new(),
                };
                return SetupOutcome {
                    stdout,
                    stderr: format!(
                        "{notes}{}rapid setup: verification failed ({}): {hint}\nno files were \
changed\n",
                        receipts_text(&receipts),
                        failure.class()
                    ),
                    exit: failure.exit_code(),
                };
            }
        }
        // Writing is SEAM-02-3: until then a verified plan still writes
        // nothing, and says so.
        let verified = !parsed.no_verify;
        let stdout = match parsed.output {
            OutputFormat::Json => format!(
                "{}\n",
                serde_json::json!({
                    "schema": "rapidlm.setup_outcome/v1",
                    "verified": verified,
                    "egress": receipts_json(&receipts),
                    "written": false,
                })
            ),
            OutputFormat::Text => String::new(),
        };
        return SetupOutcome {
            stdout,
            stderr: format!(
                "{notes}{}rapid setup: {}writing the configuration is not in this build yet — no \
files were changed; --dry-run prints what would be written\n",
                receipts_text(&receipts),
                if verified {
                    format!("{} answered; ", plan.choice.base_url)
                } else {
                    String::new()
                }
            ),
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

/// The egress receipt as the outcome's JSON.
fn receipts_json(receipts: &[crate::provider_egress::EgressReceipt]) -> serde_json::Value {
    serde_json::Value::Array(
        receipts
            .iter()
            .map(|receipt| {
                serde_json::json!({
                    "allowed": receipt.allowed,
                    "dialled": receipt.dialled,
                    "reason": receipt.reason,
                })
            })
            .collect(),
    )
}

/// The egress receipt as lines of the outcome's text.
fn receipts_text(receipts: &[crate::provider_egress::EgressReceipt]) -> String {
    receipts
        .iter()
        .map(|receipt| match (&receipt.reason, receipt.allowed) {
            (_, true) => format!(
                "egress: allowed {} (policy: this endpoint only)\n",
                receipt.dialled
            ),
            (Some(reason), false) => format!("egress: refused {} ({reason})\n", receipt.dialled),
            (None, false) => format!("egress: refused {}\n", receipt.dialled),
        })
        .collect()
}

/// The file a symlink chain ends at — followed by hand, not canonicalised:
/// a link a dotfile manager made before its target exists points at exactly
/// the file a first setup creates.
fn follow_symlinks(link: &Path) -> Result<PathBuf, String> {
    let mut current = link.to_path_buf();
    // Up to 40 links are followed; the 41st read says whether that was the
    // file or yet another link.
    for _ in 0..=40 {
        match std::fs::read_link(&current) {
            Ok(target) => {
                current = if target.is_absolute() {
                    target
                } else {
                    current
                        .parent()
                        .map_or_else(|| target.clone(), |dir| dir.join(&target))
                };
            }
            // Not a link (or not there yet): this is the file.
            Err(_) => return Ok(current),
        }
    }
    Err(format!(
        "rapid setup: {} is a symlink loop, or a chain of more than 40 links",
        link.display()
    ))
}

/// [`follow_symlinks`], checked against the OS: a chain longer than the
/// platform follows (32 links on some) is refused rather than planned, so
/// the target always agrees with the file a run's reader opens.
fn resolve_symlinks(link: &Path) -> Result<PathBuf, String> {
    let target = follow_symlinks(link)?;
    // Refused unless the OS follows the link too: resolved, or ends at a
    // file not created yet (the target itself missing).
    match std::fs::metadata(link) {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && !target.exists() => {}
        // The OS's own reason ("too many levels of symbolic links", a
        // permission, …) — not a guess at one.
        Err(err) => {
            return Err(format!(
                "rapid setup: {} cannot be followed: {err}",
                link.display()
            ));
        }
    }
    Ok(target)
}

/// The config's current content: `None` when there is no file. Bounded like
/// the reader's own read, and only a regular file (a device or a pipe named
/// by `RAPIDLM_CONFIG` would block, or swallow a `--key-stdin` key).
fn read_existing(path: &Path) -> Result<Option<String>, String> {
    match path.metadata() {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(format!(
                "rapid setup: {} could not be read: {err}",
                path.display()
            ));
        }
        Ok(meta) if !meta.is_file() => {
            return Err(format!(
                "rapid setup: {} is not a regular file",
                path.display()
            ));
        }
        Ok(_) => {}
    }
    let bytes =
        crate::exec_tools::read_file_bounded(path, crate::user_config::MAX_USER_CONFIG_BYTES)
            .map_err(|err| match err {
                crate::exec_tools::BoundedReadError::TooLarge => format!(
                    "rapid setup: {} exceeds {} bytes, the most a run reads",
                    path.display(),
                    crate::user_config::MAX_USER_CONFIG_BYTES
                ),
                crate::exec_tools::BoundedReadError::Io(err) => {
                    format!("rapid setup: {} could not be read: {err}", path.display())
                }
            })?;
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| format!("rapid setup: {} is not UTF-8", path.display()))
}

/// `20260924T051500.123Z`: UTC now, compact, to the millisecond, for a
/// backup file's name (two runs in one second get two names).
fn compact_utc_now() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}.{:03}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        now.millisecond()
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
                read_key: || Ok("sk-test-from-stdin".to_owned()),
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
            let plan = plan(choice, Path::new("config.toml"), None, &[], None, true, "T")
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
                "--key-env",
                "OPENAI_API_KEY",
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
        let plan =
            plan(choice, &home.config(), Some(existing), &[], None, true, "T").expect("plan");
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
        let second = super::plan(
            again,
            &home.config(),
            Some(&plan.document),
            &[],
            None,
            true,
            "T",
        )
        .expect("plan");
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
        for key in [
            "sk-live-abc123",
            "gsk_0123456789abcdefABCDEF",
            "a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6",
        ] {
            let err = parse_args(&args(&["--key-env", key])).expect_err("refused");
            assert!(!err.contains(key), "a key is never echoed: {err}");
        }
        // Rejected values anywhere are never echoed: they may be keys.
        for list in [
            vec!["sk-live-abc123"],
            vec!["-k=sk-live-abc123"],
            vec!["--sk-live-abc123"],
            vec!["--base-url", "https://user:sk-live-abc123@gw.example/v1"],
            vec!["--preset", "sk-live-abc123"],
        ] {
            let err = parse_args(&args(&list)).expect_err("refused");
            assert!(!err.contains("sk-live-abc123"), "{list:?} echoed: {err}");
        }
    }

    #[test]
    fn the_argument_surface_is_closed_and_consistent() {
        for (list, needle) in [
            (vec!["--preset"], "needs a value"),
            (vec!["--preset", "nope"], "unknown preset (one of"),
            (vec!["--bogus"], "unknown option --bogus"),
            (
                vec!["stray"],
                "unexpected argument — setup takes only --options",
            ),
            (vec!["--dry-run", "--dry-run"], "given twice"),
            (vec!["--key-env", "A", "--key-stdin"], "alternatives"),
            (vec!["--output", "yaml"], "text or json"),
            (vec!["--profile", "has space"], "--profile must start with"),
            (vec!["--profile", "my_openai"], "--profile must start with"),
            (vec!["--model", "-x"], "--model must be"),
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
        let plan = plan(choice, Path::new("c.toml"), None, &[], None, false, "T").expect("plan");
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

    fn choice_for(list: &[&str]) -> Choice {
        resolve_choice(
            &parse_args(&args(list)).expect("args"),
            false,
            &mut Scripted(Vec::new()),
        )
        .expect("choice")
    }

    #[test]
    fn help_names_every_flag_and_every_preset() {
        let outcome = run(
            &args(&["--help"]),
            &Home::new("help").env(),
            &mut Scripted(Vec::new()),
        );
        assert_eq!(outcome.exit, 0);
        for needle in [
            "--preset",
            "--profile",
            "--model",
            "--base-url",
            "--key-env",
            "--key-stdin",
            "--dry-run",
            "--non-interactive",
            "--output",
            "--no-verify",
        ] {
            assert!(outcome.stdout.contains(needle), "{needle}");
        }
        for preset in PRESETS {
            assert!(outcome.stdout.contains(preset.id), "{}", preset.id);
        }
    }

    #[test]
    fn comments_on_a_changed_key_survive_and_an_unchanged_file_is_left_byte_for_byte() {
        let existing = "\
[models]
# keep local on the laptop
default = \"local\"  # the usual one

[model.local]
provider = \"openai-compatible\"
model = \"llama3.2\"
base_url = \"http://127.0.0.1:11434/v1\"
";
        let plan = plan(
            choice_for(&["--preset", "openai"]),
            Path::new("c.toml"),
            Some(existing),
            &[],
            None,
            true,
            "T",
        )
        .expect("plan");
        assert!(
            plan.document
                .contains("# keep local on the laptop\ndefault = \"openai\""),
            "{}",
            plan.document
        );
        assert!(
            plan.document.contains("# the usual one"),
            "{}",
            plan.document
        );
        // A file already holding exactly this profile, in CRLF with a BOM and
        // no final newline: `unchanged`, byte for byte, no backup.
        let written = plan.document.replace('\n', "\r\n");
        let odd = format!("\u{feff}{}", written.trim_end_matches("\r\n"));
        let again = super::plan(
            choice_for(&["--preset", "openai"]),
            Path::new("c.toml"),
            Some(&odd),
            &[],
            None,
            true,
            "T",
        )
        .expect("plan");
        assert_eq!(again.action, FileAction::Unchanged);
        assert_eq!(again.document, odd);
        assert_eq!(again.backup, None);
        // A change to such a file keeps its CRLF line endings and its BOM.
        let changed = super::plan(
            choice_for(&["--preset", "openai", "--model", "gpt-5-mini"]),
            Path::new("c.toml"),
            Some(&odd),
            &[],
            None,
            true,
            "T",
        )
        .expect("plan");
        assert_eq!(changed.action, FileAction::Update);
        assert!(changed.document.starts_with('\u{feff}'));
        assert!(
            !changed.document.replace("\r\n", "").contains('\n'),
            "only CRLF line endings"
        );
    }

    #[test]
    fn the_plan_says_when_a_run_would_use_another_profile_and_refuses_what_a_run_refuses() {
        // RAPIDLM_MODEL overrides `[models] default`: the plan says so.
        let existing = "\
[model.local]
provider = \"openai-compatible\"
model = \"llama3.2\"
base_url = \"http://127.0.0.1:11434/v1\"
";
        let env = vec![(
            crate::user_config::DEFAULT_MODEL_ENV.to_owned(),
            "local".to_owned(),
        )];
        let plan = plan(
            choice_for(&["--preset", "openai"]),
            Path::new("c.toml"),
            Some(existing),
            &env,
            None,
            true,
            "T",
        )
        .expect("plan");
        assert_eq!(plan.effective, Some(("local".to_owned(), "env")));
        assert!(render_text(&plan, true).contains("runs will use profile 'local'"));
        assert_eq!(render_json(&plan, true)["effective_profile"], "local");
        // A managed allowlist that refuses the dialect: nothing is planned.
        let policy = crate::managed_config::ManagedPolicy::parse(
            "schema = \"rapidlm.managed_config.v1\"\n[policy]\nallowed_providers = [\"openai-compatible\"]\n",
        )
        .expect("policy");
        let err = super::plan(
            choice_for(&["--preset", "anthropic"]),
            Path::new("c.toml"),
            None,
            &[],
            Some(&policy),
            true,
            "T",
        )
        .expect_err("a run would refuse it");
        assert!(err.contains("a run would refuse"), "{err}");
    }

    #[test]
    fn an_inline_table_config_is_edited_and_a_custom_endpoint_keeps_its_credential() {
        let existing = "\
models = { default = \"gw\" }

[model]
gw = { provider = \"openai-compatible\", model = \"m\", base_url = \"http://10.0.0.5:9000/v1\", env_key = \"GW_KEY\", max_tokens = 1024 }
";
        let plan = plan(
            choice_for(&[
                "--base-url",
                "http://10.0.0.5:9000/v2",
                "--model",
                "m2",
                "--profile",
                "gw",
            ]),
            Path::new("c.toml"),
            Some(existing),
            &[],
            None,
            true,
            "T",
        )
        .expect("an inline table is not refused");
        let parsed = parse_config_document(&plan.document, "c").expect("parses");
        let entry = parsed.models.entries.get("gw").expect("profile");
        assert_eq!(entry.model, "m2");
        assert_eq!(entry.base_url, "http://10.0.0.5:9000/v2");
        assert_eq!(
            entry.env_key,
            vec!["GW_KEY".to_owned()],
            "the credential is kept, not stripped"
        );
        assert!(plan.unset.is_empty(), "{:?}", plan.unset);
        assert_eq!(plan.kept, vec!["model.gw.max_tokens".to_owned()]);
        assert_eq!(plan.kept_credential.as_deref(), Some("model.gw.env_key"));
        let text = render_text(&plan, true);
        assert!(text.contains("model.gw.env_key is kept as it is"), "{text}");
        assert!(text.contains("kept     model.gw.max_tokens"), "{text}");
    }

    #[test]
    fn the_target_agrees_with_the_reader_once_the_file_exists() {
        let home = Home::new("reader");
        let env: Vec<(String, String)> = vec![
            (
                RAPIDLM_HOME_ENV.to_owned(),
                home.0.join("rh").display().to_string(),
            ),
            (HOME_ENV.to_owned(), home.0.display().to_string()),
        ];
        let target = config_target(&env).expect("target");
        std::fs::create_dir_all(target.parent().expect("dir")).expect("dir");
        std::fs::write(&target, "").expect("file");
        match crate::user_config::resolve_config_source(&env) {
            crate::user_config::ConfigSource::HomeFallback(read) => assert_eq!(read, target),
            other => panic!("the reader looks elsewhere: {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_config_is_planned_where_it_points() {
        let home = Home::new("symlink");
        let real = home.0.join("real.toml");
        std::fs::write(&real, "").expect("real");
        std::fs::create_dir_all(home.0.join(".rapidlm")).expect("dir");
        std::os::unix::fs::symlink(&real, home.config()).expect("link");
        let outcome = run(
            &args(&["--preset", "ollama", "--dry-run", "--output", "json"]),
            &home.env(),
            &mut Scripted(Vec::new()),
        );
        assert_eq!(outcome.exit, 0, "{}", outcome.stderr);
        let plan: serde_json::Value = serde_json::from_str(&outcome.stdout).expect("json");
        assert_eq!(plan["files"][0]["path"], real.display().to_string());
        assert_eq!(
            plan["files"][0]["via_symlink"],
            home.config().display().to_string()
        );
    }

    #[test]
    fn a_config_path_that_is_not_a_regular_file_is_refused() {
        let home = Home::new("fifo");
        let env = SetupEnv {
            env: vec![(CONFIG_PATH_ENV.to_owned(), home.0.display().to_string())],
            stdin_is_tty: false,
            read_key: || Ok(String::new()),
        };
        let outcome = run(
            &args(&["--preset", "ollama", "--dry-run"]),
            &env,
            &mut Scripted(Vec::new()),
        );
        assert_eq!(outcome.exit, 1);
        assert!(
            outcome.stderr.contains("not a regular file"),
            "{}",
            outcome.stderr
        );
    }

    #[test]
    fn a_kept_credential_goes_only_to_the_endpoint_it_was_set_up_for() {
        let existing = "\
[models]
default = \"default\"

[model.default]
provider = \"openai-compatible\"
model = \"m\"
base_url = \"https://gw-a.corp.example/v1\"
env_key = \"CORP_KEY\"
";
        // Another host, no key flag: the old key is removed, not sent there.
        let moved = plan(
            choice_for(&["--base-url", "http://10.0.0.9:8000/v1", "--model", "qwen3"]),
            Path::new("c.toml"),
            Some(existing),
            &[],
            None,
            true,
            "T",
        )
        .expect("plan");
        assert_eq!(moved.unset, vec!["model.default.env_key".to_owned()]);
        assert!(
            moved
                .notes
                .iter()
                .any(|note| note.contains("another endpoint")),
            "{:?}",
            moved.notes
        );
        let parsed = parse_config_document(&moved.document, "c").expect("parses");
        assert!(parsed.models.entries["default"].env_key.is_empty());
        // The same origin (another path, the default port spelled out): kept.
        let same = plan(
            choice_for(&[
                "--base-url",
                "https://GW-A.corp.example:443/v2",
                "--model",
                "m",
            ]),
            Path::new("c.toml"),
            Some(existing),
            &[],
            None,
            true,
            "T",
        )
        .expect("plan");
        assert!(same.unset.is_empty(), "{:?}", same.unset);
        assert_eq!(
            same.kept_credential.as_deref(),
            Some("model.default.env_key")
        );
        assert!(same_origin("http://[::1]:8080/v1", "http://[::1]:8080/x"));
        assert!(!same_origin("http://[::1]:8080/v1", "http://[::1]:8081/v1"));
    }

    #[test]
    fn this_shells_model_override_is_a_note_and_the_allowlist_judges_the_written_profile() {
        // RAPIDLM_MODEL naming a profile that does not exist: the plan still
        // stands, and says a run in this shell would fail.
        let env = vec![(
            crate::user_config::DEFAULT_MODEL_ENV.to_owned(),
            "work".to_owned(),
        )];
        let plan = plan(
            choice_for(&["--preset", "openai"]),
            Path::new("c.toml"),
            None,
            &env,
            None,
            true,
            "T",
        )
        .expect("a good plan is not refused");
        assert!(
            plan.notes.iter().any(|note| note.contains("RAPIDLM_MODEL")),
            "{:?}",
            plan.notes
        );
        // The allowlist judges the profile the file sets, not the one this
        // shell's RAPIDLM_MODEL happens to name.
        let existing = "\
[model.local]
provider = \"openai-compatible\"
model = \"llama3.2\"
base_url = \"http://127.0.0.1:11434/v1\"
";
        let policy = crate::managed_config::ManagedPolicy::parse(
            "schema = \"rapidlm.managed_config.v1\"\n[policy]\nallowed_providers = [\"openai-compatible\"]\n",
        )
        .expect("policy");
        let env = vec![(
            crate::user_config::DEFAULT_MODEL_ENV.to_owned(),
            "local".to_owned(),
        )];
        let err = super::plan(
            choice_for(&["--preset", "anthropic"]),
            Path::new("c.toml"),
            Some(existing),
            &env,
            Some(&policy),
            true,
            "T",
        )
        .expect_err("the written profile is refused");
        assert!(err.contains("a run would refuse"), "{err}");
    }

    #[test]
    fn under_a_lock_this_shells_missing_override_is_overruled_not_a_failure() {
        let existing = "\
[models]
default = \"corp\"

[model.corp]
provider = \"openai-compatible\"
model = \"m\"
base_url = \"http://10.0.0.5:9000/v1\"
";
        let policy = crate::managed_config::ManagedPolicy::parse(
            "schema = \"rapidlm.managed_config.v1\"\n[policy]\nlocked_default = \"corp\"\n",
        )
        .expect("policy");
        let env = vec![(
            crate::user_config::DEFAULT_MODEL_ENV.to_owned(),
            "nope".to_owned(),
        )];
        let plan = super::plan(
            choice_for(&[
                "--base-url",
                "http://10.0.0.5:9000/v1",
                "--model",
                "m",
                "--profile",
                "corp",
            ]),
            Path::new("c.toml"),
            Some(existing),
            &env,
            Some(&policy),
            true,
            "T",
        )
        .expect("plan");
        // The planned profile is the locked one, so the shell's override is
        // read — and overruled, not a failure.
        assert_eq!(plan.effective, None);
        assert!(
            !plan.notes.iter().any(|note| note.contains("would fail")),
            "{:?}",
            plan.notes
        );
        // The profile this writes is judged by the allowlist too, though the
        // lock picks another.
        let allowlisted = crate::managed_config::ManagedPolicy::parse(
            "schema = \"rapidlm.managed_config.v1\"\n[policy]\nlocked_default = \"corp\"\nallowed_providers = [\"openai-compatible\"]\n",
        )
        .expect("policy");
        let err = super::plan(
            choice_for(&["--preset", "anthropic", "--profile", "x"]),
            Path::new("c.toml"),
            Some(existing),
            &env,
            Some(&allowlisted),
            true,
            "T",
        )
        .expect_err("refused");
        assert!(
            err.contains("allows only the providers openai-compatible"),
            "{err}"
        );
    }

    #[test]
    fn an_inline_table_keeps_its_comments_and_an_identical_run_is_unchanged() {
        let existing = "\
# which profile runs
models = { default = \"gw\" }  # set by hand

[model]
gw = { provider = \"openai-compatible\", model = \"m\", base_url = \"http://10.0.0.5:9000/v1\", env_key = \"GW_KEY\" }
";
        let plan = plan(
            choice_for(&[
                "--base-url",
                "http://10.0.0.5:9000/v1",
                "--model",
                "m",
                "--profile",
                "gw",
                "--key-env",
                "GW_KEY",
            ]),
            Path::new("c.toml"),
            Some(existing),
            &[],
            None,
            true,
            "T",
        )
        .expect("plan");
        assert_eq!(plan.action, FileAction::Unchanged, "{}", plan.document);
        assert!(plan.set.is_empty(), "nothing is written: {:?}", plan.set);
        let changed = super::plan(
            choice_for(&[
                "--base-url",
                "http://10.0.0.5:9000/v1",
                "--model",
                "m2",
                "--profile",
                "gw",
            ]),
            Path::new("c.toml"),
            Some(existing),
            &[],
            None,
            true,
            "T",
        )
        .expect("plan");
        assert!(
            changed.document.contains("# which profile runs"),
            "{}",
            changed.document
        );
        assert!(
            changed.document.contains("# set by hand"),
            "{}",
            changed.document
        );
        assert_eq!(
            changed.set,
            vec![("model.gw.model".to_owned(), "m2".to_owned())]
        );
    }

    #[test]
    fn a_lf_file_with_one_crlf_line_stays_lf_and_output_never_echoes() {
        let existing = "[models]\ndefault = \"local\"\r\n\n[model.local]\nprovider = \"openai-compatible\"\nmodel = \"llama3.2\"\nbase_url = \"http://127.0.0.1:11434/v1\"\n";
        let plan = plan(
            choice_for(&["--preset", "openai"]),
            Path::new("c.toml"),
            Some(existing),
            &[],
            None,
            true,
            "T",
        )
        .expect("plan");
        assert!(
            plan.document.matches("\r\n").count() <= 1,
            "{:?}",
            plan.document
        );
        let err = parse_args(&args(&["--output", "sk-live-abc123"])).expect_err("refused");
        assert!(!err.contains("sk-live-abc123"), "{err}");
    }

    #[test]
    fn a_key_a_run_would_not_read_is_named_in_the_plan() {
        let plan = plan(
            choice_for(&[
                "--base-url",
                "http://10.0.0.5:9000/v1",
                "--model",
                "m",
                "--key-stdin",
            ]),
            Path::new("c.toml"),
            None,
            &[],
            None,
            true,
            "T",
        )
        .expect("plan");
        assert!(
            plan.notes
                .iter()
                .any(|note| note.contains("model.default.keychain")),
            "{:?}",
            plan.notes
        );
    }

    #[test]
    fn a_presets_key_goes_only_to_the_presets_own_endpoint() {
        let proxy = [
            "--preset",
            "openai",
            "--base-url",
            "http://10.0.0.9:8000/v1",
        ];
        let fresh = plan(
            choice_for(&proxy),
            Path::new("c.toml"),
            None,
            &[],
            None,
            true,
            "T",
        )
        .expect("plan");
        assert_eq!(fresh.choice.credential, Credential::Unchanged);
        let parsed = parse_config_document(&fresh.document, "c").expect("parses");
        assert!(parsed.models.entries["openai"].env_key.is_empty());
        assert!(
            fresh
                .notes
                .iter()
                .any(|note| note.contains("--key-env OPENAI_API_KEY")),
            "{:?}",
            fresh.notes
        );
        // The profile's key was for the preset's host: removed, not sent.
        let existing = "\
[model.openai]
provider = \"openai-compatible\"
model = \"gpt-5\"
base_url = \"https://api.openai.com/v1\"
env_key = \"OPENAI_API_KEY\"
";
        let moved = plan(
            choice_for(&proxy),
            Path::new("c.toml"),
            Some(existing),
            &[],
            None,
            true,
            "T",
        )
        .expect("plan");
        assert_eq!(moved.unset, vec!["model.openai.env_key".to_owned()]);
        assert!(
            moved
                .notes
                .iter()
                .any(|note| note.contains("--key-env OPENAI_API_KEY")),
            "{:?}",
            moved.notes
        );
        // A credential already set up for the proxy's own origin is kept,
        // and there is nothing to say about the preset's variable.
        let for_the_proxy = existing
            .replace("https://api.openai.com/v1", "http://10.0.0.9:8000/v1")
            .replace("OPENAI_API_KEY", "PROXY_KEY");
        let kept = plan(
            choice_for(&proxy),
            Path::new("c.toml"),
            Some(&for_the_proxy),
            &[],
            None,
            true,
            "T",
        )
        .expect("plan");
        assert_eq!(
            kept.kept_credential.as_deref(),
            Some("model.openai.env_key")
        );
        assert!(kept.unset.is_empty(), "{:?}", kept.unset);
        assert!(
            !kept
                .notes
                .iter()
                .any(|note| note.contains("OPENAI_API_KEY")),
            "{:?}",
            kept.notes
        );
        // A keyless preset: on its own origin its server takes no key; on
        // another it keeps what the profile names for that origin.
        assert_eq!(
            choice_for(&["--preset", "ollama"]).credential,
            Credential::None
        );
        assert_eq!(
            choice_for(&[
                "--preset",
                "ollama",
                "--base-url",
                "http://gpu.example.test:11434/v1"
            ])
            .credential,
            Credential::Unchanged
        );
        // The preset's own origin, another path: its key variable as before;
        // and a key variable named on the command line goes where it is told.
        assert_eq!(
            choice_for(&[
                "--preset",
                "openai",
                "--base-url",
                "https://api.openai.com/v2"
            ])
            .credential,
            Credential::Env {
                var: "OPENAI_API_KEY".to_owned()
            }
        );
        let mut explicit = proxy.to_vec();
        explicit.extend(["--key-env", "OPENAI_API_KEY"]);
        assert_eq!(
            choice_for(&explicit).credential,
            Credential::Env {
                var: "OPENAI_API_KEY".to_owned()
            }
        );
    }

    #[test]
    fn only_this_profiles_unknown_keys_are_named() {
        let existing = "\
[model.\"default.x\"]
provider = \"openai-compatible\"
model = \"m\"
base_url = \"http://10.0.0.5:9000/v1\"
future_knob = 1

[model.default]
provider = \"openai-compatible\"
model = \"m\"
base_url = \"http://10.0.0.5:9000/v1\"
own_knob = 2
\"dotted.knob\" = 3
";
        let plan = plan(
            choice_for(&["--base-url", "http://10.0.0.5:9000/v1", "--model", "m"]),
            Path::new("c.toml"),
            Some(existing),
            &[],
            None,
            true,
            "T",
        )
        .expect("plan");
        assert!(
            !plan.notes.iter().any(|note| note.contains("future_knob")),
            "another profile's key is not this one's: {:?}",
            plan.notes
        );
        for own in ["model.default.own_knob", "model.default.dotted.knob"] {
            assert!(
                plan.notes.iter().any(|note| note.contains(own)),
                "{own}: {:?}",
                plan.notes
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_chain_of_forty_links_is_followed_and_forty_one_is_refused() {
        let dir = std::env::temp_dir().join(format!(
            "rapid-setup-links-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        // No symlink in the directory's own path (a temp directory under a
        // symlinked root would add a hop the count below does not see).
        let dir = std::fs::canonicalize(&dir).expect("canonical dir");
        let target = dir.join("config.toml");
        // Relative links: each hop is exactly one link for the OS too (an
        // absolute path through a symlinked temp directory adds hops).
        let mut previous = std::path::PathBuf::from("config.toml");
        for hop in 1..=41 {
            let name = format!("link{hop}");
            std::os::unix::fs::symlink(&previous, dir.join(&name)).expect("symlink");
            previous = std::path::PathBuf::from(name);
        }
        assert_eq!(
            follow_symlinks(&dir.join("link40")).expect("forty links"),
            target
        );
        assert!(
            follow_symlinks(&dir.join("link41"))
                .expect_err("forty-one")
                .contains("symlink loop")
        );
        // A chain the system will not follow is refused, whether or not its
        // file exists yet. Each hop goes through a directory link, so the
        // system pays two links per hop: 21 hops are 42 for it (more than any
        // system follows), 21 for the count by hand.
        std::os::unix::fs::symlink(&dir, dir.join("alias")).expect("alias");
        let mut previous = dir.join("alias").join("config.toml");
        for hop in 1..=21 {
            let link = dir.join(format!("deep{hop}"));
            std::os::unix::fs::symlink(&previous, &link).expect("symlink");
            previous = dir.join("alias").join(format!("deep{hop}"));
        }
        for exists in [false, true] {
            if exists {
                std::fs::write(&target, "").expect("target");
            }
            assert!(
                resolve_symlinks(&dir.join("deep21"))
                    .expect_err("more than the system follows")
                    .contains("cannot be followed"),
                "exists={exists}"
            );
            assert!(
                resolve_symlinks(&dir.join("deep3")).is_ok(),
                "exists={exists}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_to_a_file_not_created_yet_plans_a_create_there() {
        let home = Home::new("dangling");
        let real = home.0.join("dotfiles").join("rapidlm.toml");
        std::fs::create_dir_all(real.parent().expect("dir")).expect("dir");
        std::fs::create_dir_all(home.0.join(".rapidlm")).expect("dir");
        std::os::unix::fs::symlink(&real, home.config()).expect("link");
        let outcome = run(
            &args(&["--preset", "ollama", "--dry-run", "--output", "json"]),
            &home.env(),
            &mut Scripted(Vec::new()),
        );
        assert_eq!(outcome.exit, 0, "{}", outcome.stderr);
        let plan: serde_json::Value = serde_json::from_str(&outcome.stdout).expect("json");
        assert_eq!(plan["files"][0]["path"], real.display().to_string());
        assert_eq!(plan["files"][0]["action"], "create");
        assert_eq!(
            plan["files"][0]["via_symlink"],
            home.config().display().to_string()
        );
    }

    /// A loopback endpoint answering every request with `status` and `body`;
    /// returns its `/v1` base URL and the requests it received.
    fn endpoint(
        status: u16,
        body: &'static str,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}/v1", listener.local_addr().expect("addr"));
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = std::sync::Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                while let Ok(read) = stream.read(&mut chunk) {
                    if read == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..read]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length = text[..end]
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|value| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                log.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(String::from_utf8_lossy(&buf).to_string());
                let reason = if status == 200 { "OK" } else { "Error" };
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            }
        });
        (url, seen)
    }

    const GOOD_BODY: &str = r#"{"choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1}}"#;

    fn verify_run(home: &Home, url: &str, extra: &[&str]) -> SetupOutcome {
        let mut list = vec!["--base-url", url, "--model", "m", "--non-interactive"];
        list.extend_from_slice(extra);
        let mut env = home.env();
        env.env
            .push(("TEST_KEY".to_owned(), "sk-test-env".to_owned()));
        run(&args(&list), &env, &mut Scripted(Vec::new()))
    }

    #[test]
    fn each_verification_failure_has_its_own_exit_code_and_changes_no_file() {
        // SEAM-02 AC-02 (and the validation list): 401 → auth, 402/429 →
        // quota, refused → network, 500 → server, garbage → invalid; the
        // home is byte-identical after each, and every message says no files
        // were changed.
        let refused = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            let url = format!("http://{}/v1", listener.local_addr().expect("addr"));
            drop(listener);
            url
        };
        let cases: Vec<(String, i32, &str)> = vec![
            (
                endpoint(401, r#"{"error":{"message":"bad key"}}"#).0,
                11,
                "auth",
            ),
            (
                endpoint(403, r#"{"error":{"message":"forbidden"}}"#).0,
                11,
                "auth",
            ),
            (
                endpoint(402, r#"{"error":{"message":"no credit"}}"#).0,
                12,
                "quota",
            ),
            (
                endpoint(429, r#"{"error":{"message":"slow down"}}"#).0,
                12,
                "quota",
            ),
            (refused, 13, "network"),
            (
                endpoint(500, r#"{"error":{"message":"oops"}}"#).0,
                14,
                "server",
            ),
            (endpoint(200, "definitely not json").0, 15, "invalid"),
            // Asked for proxy credentials on a direct connection: the
            // endpoint's own refusal, and no proxy variable is to blame.
            (endpoint(407, "").0, 15, "invalid"),
        ];
        for (url, code, class) in cases {
            let home = Home::new("verify-fail");
            let before = home.snapshot();
            let outcome = verify_run(&home, &url, &["--key-env", "TEST_KEY"]);
            assert_eq!(outcome.exit, code, "{class}: {}", outcome.stderr);
            assert!(
                outcome
                    .stderr
                    .contains(&format!("verification failed ({class})")),
                "{}",
                outcome.stderr
            );
            assert!(
                outcome.stderr.contains("no files were changed"),
                "{}",
                outcome.stderr
            );
            assert!(
                !outcome.stderr.contains("sk-test-env"),
                "the key is never printed"
            );
            assert!(
                !outcome.stderr.contains("proxy"),
                "no proxy was dialled: {}",
                outcome.stderr
            );
            assert_eq!(home.snapshot(), before, "{class}: nothing written");
        }
    }

    #[test]
    fn a_failed_verification_in_json_names_its_class_and_hint_and_writes_nothing() {
        let home = Home::new("verify-json");
        let (url, _) = endpoint(401, r#"{"error":{"message":"bad key"}}"#);
        let outcome = verify_run(&home, &url, &["--key-env", "TEST_KEY", "--output", "json"]);
        assert_eq!(outcome.exit, 11, "{}", outcome.stderr);
        let value: serde_json::Value = serde_json::from_str(&outcome.stdout).expect("json");
        assert_eq!(value["schema"], "rapidlm.setup_outcome/v1");
        assert_eq!(value["verified"], false);
        assert_eq!(value["class"], "auth");
        assert_eq!(value["written"], false);
        assert_eq!(
            value["egress"][0]["allowed"], true,
            "the refused key went out on the lease, and the receipt says so: {value}"
        );
        assert!(
            value["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("rapid setup")),
            "the hint names the next command: {value}"
        );
    }

    #[test]
    fn a_key_the_probe_would_not_send_is_named_instead_of_blamed_on_the_endpoint() {
        let config_with = |home: &Home, url: &str, key_line: &str| {
            std::fs::create_dir_all(home.config().parent().expect("dir")).expect("dir");
            std::fs::write(
                home.config(),
                format!(
                    "[models]\ndefault = \"default\"\n\n[model.default]\nprovider = \"openai-compatible\"\nmodel = \"m\"\nbase_url = \"{url}\"\n{key_line}\n"
                ),
            )
            .expect("config");
        };
        // A kept variable that is unset: the probe goes keyless, as a run
        // would — a local server that needs no key verifies.
        let (url, seen) = endpoint(200, GOOD_BODY);
        let home = Home::new("verify-kept-keyless");
        config_with(&home, &url, "env_key = \"LOCAL_KEY\"");
        let outcome = verify_run(&home, &url, &[]);
        assert_eq!(
            outcome.exit, 2,
            "verified, not yet written: {}",
            outcome.stderr
        );
        assert_eq!(seen.lock().expect("seen").len(), 1, "one keyless request");
        // Refused without a key: the message names why no key was sent.
        for (key_line, expected) in [
            (
                "env_key = \"GW_KEY\"",
                "$GW_KEY, which the profile names for it, is not set",
            ),
            (
                "keychain = \"rapidlm.default\"",
                "which this build does not read",
            ),
        ] {
            let (url, _) = endpoint(401, r#"{"error":{"message":"missing key"}}"#);
            let home = Home::new("verify-kept");
            config_with(&home, &url, key_line);
            let outcome = verify_run(&home, &url, &[]);
            assert_eq!(outcome.exit, 11, "{key_line}: {}", outcome.stderr);
            assert!(outcome.stderr.contains(expected), "{}", outcome.stderr);
        }
        // No key at all, and the endpoint wants one: said so — without
        // steering the preset's key to a host it is deliberately kept off —
        // and the plan's note on the key is shown.
        let (url, _) = endpoint(401, r#"{"error":{"message":"missing key"}}"#);
        let home = Home::new("verify-keyless");
        let outcome = verify_run(&home, &url, &["--preset", "openai"]);
        assert_eq!(outcome.exit, 11, "{}", outcome.stderr);
        assert!(
            outcome.stderr.contains(
                "requires a key and none was sent — give one with rapid setup --key-env <VAR>"
            ),
            "{}",
            outcome.stderr
        );
        assert!(
            outcome
                .stderr
                .contains("note: OPENAI_API_KEY is sent only to the preset's own endpoint"),
            "a real run shows the plan's notes too: {}",
            outcome.stderr
        );
    }

    #[test]
    fn what_is_refused_before_sending_is_not_reported_as_an_answer() {
        let home = Home::new("verify-local");
        // Only a proxy rapid dialled through reports this: a network failure
        // naming the variable it read.
        let proxy = classify(&llm_router::provider::ProviderError::ProxyRefused);
        assert_eq!((proxy.exit_code(), proxy.class()), (13, "network"));
        // A name that resolves to an address rapid does not dial is refused
        // by the transport's guard before anything is sent.
        let failure = classify(&llm_router::provider::ProviderError::InvalidRequest);
        assert_eq!(failure, ProbeFailure::Refused);
        assert_eq!((failure.exit_code(), failure.class()), (13, "network"));
        let plan = plan(
            choice_for(&["--base-url", "http://gw.example.test/v1", "--model", "m"]),
            Path::new("c.toml"),
            None,
            &[],
            None,
            true,
            "T",
        )
        .expect("plan");
        assert!(
            proxy.hint(&plan).contains("proxy variable"),
            "{}",
            proxy.hint(&plan)
        );
        assert!(
            failure
                .hint(&plan)
                .contains("rapid does not dial http://gw.example.test:80"),
            "{}",
            failure.hint(&plan)
        );
        // A key no header can carry.
        let (url, seen) = endpoint(200, GOOD_BODY);
        let mut env = home.env();
        env.env.push((
            "TWO_LINES".to_owned(),
            "sk-part-one\nsk-part-two".to_owned(),
        ));
        let outcome = run(
            &args(&[
                "--base-url",
                &url,
                "--model",
                "m",
                "--non-interactive",
                "--key-env",
                "TWO_LINES",
            ]),
            &env,
            &mut Scripted(Vec::new()),
        );
        assert_eq!(outcome.exit, 11, "{}", outcome.stderr);
        assert!(
            outcome.stderr.contains("no HTTP header can carry"),
            "{}",
            outcome.stderr
        );
        assert!(!outcome.stderr.contains("sk-part"), "never printed");
        assert!(seen.lock().expect("seen").is_empty(), "nothing was sent");
    }

    #[test]
    fn a_key_on_a_terminal_is_refused_before_anything_is_read() {
        let home = Home::new("stdin-tty");
        let mut env = home.env();
        env.stdin_is_tty = true;
        env.read_key = || panic!("a terminal key is never read");
        let outcome = run(
            &args(&["--preset", "openai", "--key-stdin"]),
            &env,
            &mut Scripted(Vec::new()),
        );
        assert_eq!(outcome.exit, 2, "{}", outcome.stderr);
        assert!(outcome.stderr.contains("pipe it in"), "{}", outcome.stderr);
        assert!(
            outcome.stderr.contains("no files were changed"),
            "{}",
            outcome.stderr
        );
        // A dry run, or one that does not verify, reads no key (yet), so it
        // may run on a terminal.
        let dry = run(
            &args(&["--preset", "openai", "--key-stdin", "--dry-run"]),
            &env,
            &mut Scripted(Vec::new()),
        );
        assert_eq!(dry.exit, 0, "{}", dry.stderr);
        let unverified = run(
            &args(&["--preset", "openai", "--key-stdin", "--no-verify"]),
            &env,
            &mut Scripted(Vec::new()),
        );
        assert_eq!(unverified.exit, 2, "{}", unverified.stderr);
        assert!(
            unverified.stderr.contains("not in this build yet"),
            "{}",
            unverified.stderr
        );
    }

    #[test]
    fn the_probe_carries_the_managed_effort_floor_a_run_would() {
        let home = Home::new("verify-floor");
        let policy = home.0.join("policy.toml");
        std::fs::write(
            &policy,
            format!(
                "schema = \"{}\"\n[policy]\nmin_reasoning_effort = \"high\"\n",
                crate::managed_config::MANAGED_SCHEMA
            ),
        )
        .expect("policy");
        let (url, seen) = endpoint(200, GOOD_BODY);
        let mut env = home.env();
        env.env
            .push(("TEST_KEY".to_owned(), "sk-test-env".to_owned()));
        env.env.push((
            crate::managed_config::MANAGED_CONFIG_ENV.to_owned(),
            policy.display().to_string(),
        ));
        let outcome = run(
            &args(&[
                "--base-url",
                &url,
                "--model",
                "m",
                "--non-interactive",
                "--key-env",
                "TEST_KEY",
            ]),
            &env,
            &mut Scripted(Vec::new()),
        );
        assert_eq!(
            outcome.exit, 2,
            "verified, not yet written: {}",
            outcome.stderr
        );
        let requests = seen.lock().expect("seen").clone();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0].contains(r#""reasoning_effort":"high""#),
            "{}",
            requests[0]
        );
    }

    #[test]
    fn no_verify_sends_nothing_and_says_nothing_was_written() {
        let home = Home::new("no-verify");
        let (url, seen) = endpoint(200, GOOD_BODY);
        let before = home.snapshot();
        let outcome = verify_run(&home, &url, &["--key-env", "TEST_KEY", "--no-verify"]);
        assert_eq!(outcome.exit, 2, "{}", outcome.stderr);
        assert!(
            outcome.stderr.contains("no files were changed"),
            "{}",
            outcome.stderr
        );
        assert!(seen.lock().expect("seen").is_empty(), "no request");
        assert_eq!(home.snapshot(), before);
    }

    #[test]
    fn a_verified_plan_sent_one_bounded_request_with_the_named_key() {
        let home = Home::new("verify-ok");
        let (url, seen) = endpoint(200, GOOD_BODY);
        let before = home.snapshot();
        let outcome = verify_run(&home, &url, &["--key-env", "TEST_KEY", "--output", "json"]);
        assert_eq!(outcome.exit, 2, "writing is SEAM-02-3: {}", outcome.stderr);
        assert!(outcome.stderr.contains("answered"), "{}", outcome.stderr);
        assert!(
            outcome.stderr.contains("no files were changed"),
            "{}",
            outcome.stderr
        );
        let json: serde_json::Value = serde_json::from_str(&outcome.stdout).expect("json");
        assert_eq!(json["verified"], true);
        assert_eq!(json["written"], false);
        // The egress receipt: one dial, on a lease for exactly this endpoint.
        let origin = url.trim_end_matches("/v1");
        assert_eq!(
            json["egress"],
            serde_json::json!([{ "allowed": true, "dialled": origin, "reason": null }]),
            "{}",
            outcome.stdout
        );
        assert!(
            outcome.stderr.contains(&format!(
                "egress: allowed {origin} (policy: this endpoint only)"
            )),
            "{}",
            outcome.stderr
        );
        assert_eq!(home.snapshot(), before);
        let requests = seen.lock().unwrap_or_else(|p| p.into_inner()).clone();
        assert_eq!(requests.len(), 1, "exactly one request");
        assert!(
            requests[0].contains("Bearer sk-test-env"),
            "the named key was sent"
        );
        assert!(requests[0].contains("\"max_tokens\":16"), "{}", requests[0]);
        // --key-stdin: the key read from stdin is the one sent.
        let (url, seen) = endpoint(200, GOOD_BODY);
        let outcome = verify_run(&home, &url, &["--key-stdin"]);
        assert_eq!(outcome.exit, 2, "{}", outcome.stderr);
        let requests = seen.lock().unwrap_or_else(|p| p.into_inner()).clone();
        assert!(
            requests[0].contains("Bearer sk-test-from-stdin"),
            "{}",
            requests[0]
        );
    }

    #[test]
    fn an_unset_key_variable_sends_nothing() {
        let home = Home::new("verify-nokey");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.set_nonblocking(true).expect("nonblocking");
        let url = format!("http://{}/v1", listener.local_addr().expect("addr"));
        let outcome = verify_run(&home, &url, &["--key-env", "NOT_SET_ANYWHERE"]);
        assert_eq!(outcome.exit, 11);
        assert!(
            outcome.stderr.contains("$NOT_SET_ANYWHERE is not set"),
            "{}",
            outcome.stderr
        );
        assert!(outcome.stderr.contains("no files were changed"));
        assert!(
            !outcome.stderr.contains("egress:"),
            "nothing dialled, nothing to receipt: {}",
            outcome.stderr
        );
        assert!(
            matches!(listener.accept(), Err(err) if err.kind() == std::io::ErrorKind::WouldBlock)
        );
    }

    #[test]
    fn the_probe_goes_through_the_proxy_a_run_would_and_only_when_opted_in() {
        let (proxy_url, seen) = endpoint(200, GOOD_BODY);
        let proxy_origin = proxy_url.trim_end_matches("/v1").to_owned();
        let target = "http://model.invalid:8080/v1";
        let run_with = |home: &Home, extra_env: &[(&str, &str)]| {
            let mut env = home.env();
            env.env
                .push(("TEST_KEY".to_owned(), "sk-test-env".to_owned()));
            env.env
                .push(("http_proxy".to_owned(), proxy_origin.clone()));
            for (key, value) in extra_env {
                env.env.push(((*key).to_owned(), (*value).to_owned()));
            }
            run(
                &args(&[
                    "--base-url",
                    target,
                    "--model",
                    "m",
                    "--non-interactive",
                    "--key-env",
                    "TEST_KEY",
                    "--output",
                    "json",
                ]),
                &env,
                &mut Scripted(Vec::new()),
            )
        };

        // Opted in by RAPIDLM_PROXY: the one request goes to the proxy, in
        // absolute form, and the receipt names the proxy as what was dialled.
        let home = Home::new("verify-proxy-env");
        let before = home.snapshot();
        let outcome = run_with(&home, &[("RAPIDLM_PROXY", "environment")]);
        let json: serde_json::Value = serde_json::from_str(&outcome.stdout).expect("json");
        assert_eq!(json["verified"], true, "{}", outcome.stderr);
        assert_eq!(
            json["egress"],
            serde_json::json!([{ "allowed": true, "dialled": proxy_origin, "reason": null }]),
            "{}",
            outcome.stdout
        );
        assert_eq!(home.snapshot(), before);
        {
            let requests = seen.lock().unwrap_or_else(|p| p.into_inner());
            assert_eq!(requests.len(), 1, "{requests:?}");
            assert!(
                requests[0]
                    .starts_with("POST http://model.invalid:8080/v1/chat/completions HTTP/1.1\r\n"),
                "{}",
                requests[0]
            );
        }

        // Opted in by the config the plan keeps: the same path.
        let home = Home::new("verify-proxy-file");
        std::fs::create_dir_all(home.config().parent().expect("dir")).expect("dir");
        std::fs::write(home.config(), "[network]\nproxy = \"environment\"\n").expect("config");
        let outcome = run_with(&home, &[]);
        let json: serde_json::Value = serde_json::from_str(&outcome.stdout).expect("json");
        assert_eq!(json["verified"], true, "{}", outcome.stderr);
        assert_eq!(seen.lock().unwrap_or_else(|p| p.into_inner()).len(), 2);

        // Not opted in (or opted out over the file): the variable is not read,
        // the proxy is sent nothing, and the unresolvable name fails as the
        // network failure it is.
        for (name, extra) in [
            ("verify-proxy-none", &[][..]),
            ("verify-proxy-off", &[("RAPIDLM_PROXY", "none")][..]),
        ] {
            let home = Home::new(name);
            if extra.is_empty() {
                // No opt-in anywhere.
            } else {
                std::fs::create_dir_all(home.config().parent().expect("dir")).expect("dir");
                std::fs::write(home.config(), "[network]\nproxy = \"environment\"\n")
                    .expect("config");
            }
            let outcome = run_with(&home, extra);
            assert_eq!(outcome.exit, 13, "{name}: {}", outcome.stderr);
            assert_eq!(
                seen.lock().unwrap_or_else(|p| p.into_inner()).len(),
                2,
                "{name}: the proxy was sent nothing"
            );
        }

        // An unusable proxy variable under this shell's opt-in: the file is
        // not blamed; the probe cannot dial as a run would, says why (the
        // opt-in's origin and the variable, never its value), and nothing is
        // dialled or written.
        let home = Home::new("verify-proxy-bad");
        let before = home.snapshot();
        let outcome = run_with(
            &home,
            &[
                ("RAPIDLM_PROXY", "environment"),
                ("https_proxy", "https://user:hunter2@proxy.example.test"),
            ],
        );
        assert_eq!(outcome.exit, 2, "{}", outcome.stderr);
        assert!(
            outcome.stderr.contains("cannot be verified")
                && outcome.stderr.contains("RAPIDLM_PROXY=environment")
                && outcome.stderr.contains("https_proxy"),
            "{}",
            outcome.stderr
        );
        assert!(
            !outcome.stderr.contains("left untouched"),
            "{}",
            outcome.stderr
        );
        assert!(!outcome.stderr.contains("hunter2"), "{}", outcome.stderr);
        assert!(!outcome.stderr.contains("egress:"), "{}", outcome.stderr);
        assert!(outcome.stderr.contains("no files were changed"));
        assert_eq!(home.snapshot(), before);
        assert_eq!(seen.lock().unwrap_or_else(|p| p.into_inner()).len(), 2);
        // The same shell without verification: the plan stands, and says a
        // run in this shell would fail.
        let mut env = home.env();
        env.env
            .push(("RAPIDLM_PROXY".to_owned(), "environment".to_owned()));
        env.env.push((
            "https_proxy".to_owned(),
            "https://user:hunter2@proxy.example.test".to_owned(),
        ));
        let outcome = run(
            &args(&[
                "--base-url",
                target,
                "--model",
                "m",
                "--non-interactive",
                "--no-verify",
                "--dry-run",
            ]),
            &env,
            &mut Scripted(Vec::new()),
        );
        assert_eq!(outcome.exit, 0, "{}", outcome.stderr);
        assert!(
            outcome
                .stdout
                .contains("with this shell's proxy settings a run would fail"),
            "{}",
            outcome.stdout
        );
        assert!(!outcome.stdout.contains("hunter2"), "{}", outcome.stdout);
    }

    #[test]
    fn an_https_probe_through_the_proxy_is_a_tunnel_to_the_planned_endpoint() {
        // The stand-in proxy refuses the tunnel: a network failure, after one
        // CONNECT for exactly the planned endpoint, on a lease for the proxy.
        let (proxy_url, seen) = endpoint(403, "");
        let proxy_origin = proxy_url.trim_end_matches("/v1").to_owned();
        let home = Home::new("verify-proxy-https");
        let before = home.snapshot();
        let mut env = home.env();
        env.env
            .push(("TEST_KEY".to_owned(), "sk-test-env".to_owned()));
        env.env
            .push(("RAPIDLM_PROXY".to_owned(), "environment".to_owned()));
        env.env
            .push(("https_proxy".to_owned(), proxy_origin.clone()));
        let outcome = run(
            &args(&[
                "--base-url",
                "https://model.invalid:8443/v1",
                "--model",
                "m",
                "--non-interactive",
                "--key-env",
                "TEST_KEY",
                "--output",
                "json",
            ]),
            &env,
            &mut Scripted(Vec::new()),
        );
        assert_eq!(outcome.exit, 13, "{}", outcome.stderr);
        let json: serde_json::Value = serde_json::from_str(&outcome.stdout).expect("json");
        assert_eq!(
            json["egress"],
            serde_json::json!([{ "allowed": true, "dialled": proxy_origin, "reason": null }]),
            "{}",
            outcome.stdout
        );
        let requests = seen.lock().unwrap_or_else(|p| p.into_inner()).clone();
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert!(
            requests[0].starts_with("CONNECT model.invalid:8443 HTTP/1.1\r\n"),
            "{}",
            requests[0]
        );
        assert!(
            !requests[0].contains("sk-test-env"),
            "the key never reaches the proxy: {}",
            requests[0]
        );
        assert_eq!(home.snapshot(), before);
    }
}
