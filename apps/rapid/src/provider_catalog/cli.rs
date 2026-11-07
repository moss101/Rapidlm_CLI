//! Explicit catalog browsing, refresh, diagnostics and reviewed imports.
use super::{
    DEFAULT_SOURCE,
    cache::{Cache, Freshness, Origin, ResolveOptions},
    import::{self, CredentialBinding, Request},
};
use crate::user_config::ConfigProvider;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

pub const USAGE: &str = "usage: rapid catalog sources\n       rapid catalog forget --source <url>\n       rapid catalog list [--provider <id>] [--source <url>] [--offline] [--limit <n>] [--offset <n>] [--json]\n       rapid catalog refresh [--source <url>]\n       rapid catalog check [--source <url>] [--online]\n       rapid catalog import <provider> <model> --profile <id> --wire <provider> --base-url <url> (--key-env <name> | --keychain <alias> | --account <id> | --keyless | --aws-env) [--option key=value] [--override-wire] [--allow-stale] [--set-default] [--apply] [--online] [--source <url>]\n\nImport prints a review plan unless --apply is given. It never overwrites an existing profile.\n";
#[derive(Debug, Eq, PartialEq)]
pub struct Outcome {
    pub text: String,
    pub exit: i32,
}
pub struct Context<'a> {
    pub env: &'a [(String, String)],
    pub now: u64,
    pub gate: Option<Arc<dyn llm_router::providers::dial::DialGate>>,
}

struct Args {
    action: String,
    positionals: Vec<String>,
    values: BTreeMap<String, String>,
    flags: BTreeSet<String>,
    native: BTreeMap<String, String>,
}
fn parse_args(args: &[String]) -> Result<Args, String> {
    let action = args
        .first()
        .ok_or_else(|| "missing catalog action".to_owned())?
        .clone();
    if !matches!(
        action.as_str(),
        "sources" | "forget" | "list" | "refresh" | "check" | "import"
    ) {
        return Err("unknown catalog action".into());
    }
    let mut parsed = Args {
        action,
        positionals: vec![],
        values: BTreeMap::new(),
        flags: BTreeSet::new(),
        native: BTreeMap::new(),
    };
    let mut index = 1;
    while index < args.len() {
        let token = &args[index];
        if !token.starts_with('-') {
            parsed.positionals.push(token.clone());
            index += 1;
            continue;
        }
        if [
            "--offline",
            "--online",
            "--json",
            "--apply",
            "--set-default",
            "--override-wire",
            "--allow-stale",
            "--keyless",
            "--aws-env",
        ]
        .contains(&token.as_str())
        {
            if !parsed.flags.insert(token.clone()) {
                return Err("duplicate catalog option".into());
            }
            index += 1;
            continue;
        }
        if ![
            "--source",
            "--provider",
            "--limit",
            "--offset",
            "--profile",
            "--wire",
            "--base-url",
            "--key-env",
            "--keychain",
            "--account",
            "--option",
        ]
        .contains(&token.as_str())
        {
            return Err("unknown catalog option".into());
        }
        let value = args
            .get(index + 1)
            .filter(|value| !value.is_empty() && !value.starts_with("--"))
            .ok_or_else(|| "catalog option requires a value".to_owned())?;
        if token == "--option" {
            let (key, value) = value
                .split_once('=')
                .filter(|(key, value)| !key.is_empty() && !value.is_empty())
                .ok_or_else(|| "provider option must be key=value".to_owned())?;
            if parsed
                .native
                .insert(key.to_owned(), value.to_owned())
                .is_some()
            {
                return Err("duplicate provider option".into());
            }
        } else if parsed.values.insert(token.clone(), value.clone()).is_some() {
            return Err("duplicate catalog option".into());
        }
        index += 2;
    }
    Ok(parsed)
}
fn has(args: &Args, flag: &str) -> bool {
    args.flags.contains(flag)
}
fn value<'a>(args: &'a Args, key: &str) -> Option<&'a str> {
    args.values.get(key).map(String::as_str)
}
fn required<'a>(args: &'a Args, key: &str) -> Result<&'a str, String> {
    value(args, key).ok_or_else(|| format!("{key} is required"))
}
fn count(args: &Args, key: &str, default: usize, max: usize) -> Result<usize, String> {
    let n = value(args, key).map_or(Ok(default), |value| {
        value
            .parse::<usize>()
            .map_err(|_| "invalid pagination value".to_owned())
    })?;
    if n > max || key == "--limit" && n == 0 {
        return Err("pagination value is outside its bound".into());
    }
    Ok(n)
}
fn import_request(parsed: &Args) -> Result<Request, String> {
    let wire = ConfigProvider::parse(required(parsed, "--wire")?)
        .ok_or_else(|| "unknown model wire".to_owned())?;
    let credentials = [
        value(parsed, "--key-env").map(|s| CredentialBinding::Env(s.into())),
        value(parsed, "--keychain").map(|s| CredentialBinding::Keychain(s.into())),
        value(parsed, "--account").map(|s| CredentialBinding::Account(s.into())),
        has(parsed, "--keyless").then_some(CredentialBinding::Keyless),
        has(parsed, "--aws-env").then_some(CredentialBinding::AwsEnvironment),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    if credentials.len() != 1 {
        return Err("choose exactly one explicit credential source".into());
    }
    Ok(Request {
        catalog_provider: parsed.positionals[0].clone(),
        model: parsed.positionals[1].clone(),
        profile: required(parsed, "--profile")?.to_owned(),
        wire,
        base_url: required(parsed, "--base-url")?.to_owned(),
        credential: credentials[0].clone(),
        override_wire: has(parsed, "--override-wire"),
        set_default: has(parsed, "--set-default"),
        native_fields: parsed.native.clone(),
    })
}

pub fn execute(
    args: &[String],
    context: &Context<'_>,
    cancel: &llm_router::CancellationToken,
) -> Result<Outcome, String> {
    if args.is_empty() || args == ["--help"] || args == ["-h"] {
        return Ok(Outcome {
            text: USAGE.into(),
            exit: 0,
        });
    }
    let parsed = parse_args(args)?;
    if parsed.action == "sources"
        && (!parsed.flags.is_empty() || !parsed.values.is_empty() || !parsed.native.is_empty())
    {
        return Err("sources takes no options".into());
    }
    if parsed.action == "forget" {
        required(&parsed, "--source")?;
        if parsed.values.len() != 1 || !parsed.flags.is_empty() || !parsed.native.is_empty() {
            return Err("forget takes only --source".into());
        }
    }
    if has(&parsed, "--offline") && has(&parsed, "--online") {
        return Err("choose offline or online, not both".into());
    }
    if parsed.action == "refresh" && (has(&parsed, "--offline") || has(&parsed, "--online")) {
        return Err("refresh is an explicit online operation".into());
    }
    let is_import = parsed.action == "import";
    if parsed.positionals.len() != if is_import { 2 } else { 0 } {
        return Err("wrong number of catalog arguments".into());
    }
    let import_only = [
        "--profile",
        "--wire",
        "--base-url",
        "--key-env",
        "--keychain",
        "--account",
    ];
    if !is_import
        && (import_only
            .iter()
            .any(|key| parsed.values.contains_key(*key))
            || !parsed.native.is_empty()
            || [
                "--apply",
                "--set-default",
                "--override-wire",
                "--allow-stale",
                "--keyless",
                "--aws-env",
            ]
            .iter()
            .any(|key| has(&parsed, key)))
    {
        return Err("import option used with another catalog action".into());
    }
    if parsed.action != "list"
        && (["--provider", "--limit", "--offset"]
            .iter()
            .any(|key| parsed.values.contains_key(*key))
            || has(&parsed, "--json"))
    {
        return Err("listing option used with another catalog action".into());
    }
    let request = if is_import {
        Some(import_request(&parsed)?)
    } else {
        None
    };
    if parsed.action == "list" {
        count(&parsed, "--limit", 40, 200)?;
        count(&parsed, "--offset", 0, super::MAX_MODELS_TOTAL)?;
    }
    let policy =
        crate::managed_config::load_policy(context.env).map_err(|error| error.to_string())?;
    let config_path = crate::setup::config_target(context.env)?;
    let existing = import::read_existing(&config_path).map_err(|error| error.to_string())?;
    let config = crate::user_config::parse_config_document(
        existing.as_deref().unwrap_or(""),
        "catalog configuration",
    )
    .map_err(|error| error.to_string())?;
    let proxy = crate::user_config::resolve_proxy(context.env, &config)
        .map_err(|error| error.to_string())?;
    let root = crate::user_config::provider_account_root(context.env)
        .and_then(|path| path.parent().map(|parent| parent.join("provider-catalog")))
        .ok_or_else(|| "catalog cache requires RAPIDLM_HOME, HOME or USERPROFILE".to_owned())?;
    if parsed.action == "sources" {
        let sources =
            Cache::sources(&root, context.now, 600, cancel).map_err(|error| error.to_string())?;
        let mut lines = vec![format!(
            "{} saved source(s) (maximum {})",
            sources.len(),
            super::cache::MAX_SOURCES
        )];
        for source in sources {
            lines.push(format!(
                "{}: {:?}, SHA-256 {}, fetched at {}, {} provider(s)",
                source.provenance.source,
                source.freshness,
                source.provenance.sha256,
                source.provenance.fetched_at,
                source.providers
            ));
        }
        return Ok(Outcome {
            text: format!("{}\n", lines.join("\n")),
            exit: 0,
        });
    }
    if parsed.action == "forget" {
        let source = required(&parsed, "--source")?;
        let cache = Cache::new(root, source).map_err(|error| error.to_string())?;
        let removed = cache.forget(cancel).map_err(|error| error.to_string())?;
        return Ok(Outcome {
            text: if removed {
                "saved snapshot removed; imported model profiles were preserved\n"
            } else {
                "no saved snapshot for this source\n"
            }
            .into(),
            exit: 0,
        });
    }
    let source = value(&parsed, "--source").unwrap_or(DEFAULT_SOURCE);
    let cache = Cache::new(root, source).map_err(|error| error.to_string())?;
    let offline = has(&parsed, "--offline")
        || matches!(parsed.action.as_str(), "import" | "check") && !has(&parsed, "--online");
    let resolved = cache
        .resolve(
            ResolveOptions {
                now: context.now,
                ttl: 600,
                offline,
                force: parsed.action == "refresh",
                proxy,
                gate: context.gate.clone(),
            },
            cancel,
        )
        .map_err(|error| error.to_string())?;
    let registry = &resolved.cached.registry;
    let freshness = match resolved.cached.freshness {
        Freshness::Fresh => "fresh",
        Freshness::Stale => "stale",
    };
    let mut notes = vec![format!(
        "catalog: {freshness}, source {}, SHA-256 {}, fetched at {}",
        registry.provenance.source, registry.provenance.sha256, registry.provenance.fetched_at
    )];
    if let Origin::Fallback { error } = &resolved.origin {
        notes.push(format!("refresh failed ({error}); using cached data"));
    }
    match parsed.action.as_str() {
        "refresh" => {
            notes.push(format!("{} provider(s) loaded", registry.providers.len()));
            Ok(Outcome {
                text: format!("{}\n", notes.join("\n")),
                exit: if matches!(resolved.origin, Origin::Fallback { .. }) {
                    1
                } else {
                    0
                },
            })
        }
        "list" => {
            let limit = count(&parsed, "--limit", 40, 200)?;
            let offset = count(&parsed, "--offset", 0, super::MAX_MODELS_TOTAL)?;
            let rows: Vec<serde_json::Value> = if let Some(provider) = value(&parsed, "--provider")
            {
                registry
                    .providers
                    .get(provider)
                    .ok_or_else(|| "catalog provider is missing".to_owned())?
                    .models
                    .values()
                    .skip(offset)
                    .take(limit)
                    .map(|model| serde_json::to_value(model).expect("catalog model serializes"))
                    .collect()
            } else {
                registry.providers.values().skip(offset).take(limit).map(|provider| serde_json::json!({"id":provider.id,"name":provider.name,"wire":provider.wire,"models":provider.models.len()})).collect()
            };
            if has(&parsed, "--json") {
                return Ok(Outcome {
                    text: format!(
                        "{}\n",
                        serde_json::json!({"provenance":registry.provenance,"freshness":freshness,"notes":notes,"offset":offset,"limit":limit,"rows":rows})
                    ),
                    exit: 0,
                });
            }
            for row in rows {
                notes.push(row.to_string());
            }
            Ok(Outcome {
                text: format!("{}\n", notes.join("\n")),
                exit: 0,
            })
        }
        "check" => {
            for (profile, diagnostics) in import::diagnose(&config, registry) {
                notes.push(format!("{profile}: {diagnostics:?}"));
            }
            if freshness == "stale" {
                notes.push(
                    "stale catalog findings may not reflect current provider availability".into(),
                );
            }
            Ok(Outcome {
                text: format!("{}\n", notes.join("\n")),
                exit: 0,
            })
        }
        "import" => {
            if resolved.cached.freshness == Freshness::Stale && !has(&parsed, "--allow-stale") {
                return Err(
                    "catalog snapshot is stale; refresh it or explicitly pass --allow-stale".into(),
                );
            }
            let request = request.as_ref().expect("import request was validated");
            let plan = import::plan(registry, request, existing.as_deref(), policy.as_ref())
                .map_err(|error| error.to_string())?;
            notes.push(format!(
                "profile {}: {} / {}",
                plan.profile,
                request.wire.as_str(),
                request.model
            ));
            notes.extend(plan.notes.clone());
            notes.push(format!(
                "credential source: {}",
                match &request.credential {
                    CredentialBinding::Env(name) => format!("environment variable {name}"),
                    CredentialBinding::Keychain(name) => format!("OS keychain {name}"),
                    CredentialBinding::Account(name) => format!("provider account {name}"),
                    CredentialBinding::Keyless => "explicit keyless".into(),
                    CredentialBinding::AwsEnvironment => "AWS environment".into(),
                }
            ));
            notes.push(if request.set_default {
                "default will be changed explicitly".into()
            } else {
                "existing default will be preserved".into()
            });
            if has(&parsed, "--apply") {
                let current_policy = crate::managed_config::load_policy(context.env)
                    .map_err(|error| error.to_string())?;
                if policy.as_ref().map(|p| p.policy_version())
                    != current_policy.as_ref().map(|p| p.policy_version())
                {
                    return Err("managed policy changed; review a new plan".into());
                }
                let backup = import::apply(&config_path, existing.as_deref(), &plan, cancel)
                    .map_err(|error| error.to_string())?;
                notes.push(format!("saved {}", config_path.display()));
                if let Some(backup) = backup {
                    notes.push(format!(
                        "previous configuration backed up to {}",
                        backup.display()
                    ));
                }
            } else {
                notes.push("preview only; pass --apply to save".into());
            }
            Ok(Outcome {
                text: format!("{}\n", notes.join("\n")),
                exit: 0,
            })
        }
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_router::providers::dial::{DialGate, DialTarget, ProxyTarget};
    use std::net::SocketAddr;
    struct Refuse;
    impl DialGate for Refuse {
        fn permit(
            &self,
            _: DialTarget<'_>,
            _: Option<&ProxyTarget>,
            _: &[SocketAddr],
        ) -> Result<Vec<SocketAddr>, llm_router::ProviderError> {
            Err(llm_router::ProviderError::Connection)
        }
    }
    fn words(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }
    #[test]
    fn catalog_commands_preview_apply_and_diagnose_without_exposing_existing_credentials() {
        let root = std::env::temp_dir().join(format!(
            "rapid-catalog-cli-{}",
            ::protocol::SessionId::new()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("config.toml");
        let old = "[models]\ndefault = \"local\"\n[model.local]\nprovider = \"openai-compatible\"\nmodel = \"local\"\nbase_url = \"http://127.0.0.1:8080/v1\"\napi_key = \"private-key-canary\"\n";
        std::fs::write(&path, old).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let source = format!("http://{}/catalog", listener.local_addr().unwrap());
        let cache = Cache::new(root.join("provider-catalog"), &source).unwrap();
        let raw = br#"{"vendor":{"id":"vendor","npm":"@ai-sdk/openai-compatible","api":"https://api.example/v1","models":{"m":{"id":"m","limit":{"context":10000,"output":1000},"tool_call":true}}}}"#;
        let cancel = llm_router::CancellationToken::new();
        cache.save(raw, 100, &cancel).unwrap();
        let env = vec![(
            "RAPIDLM_HOME".to_owned(),
            root.to_string_lossy().into_owned(),
        )];
        let context = Context {
            env: &env,
            now: 110,
            gate: Some(Arc::new(Refuse)),
        };
        let listing = execute(
            &words(&["list", "--source", &source, "--offline", "--json"]),
            &context,
            &cancel,
        )
        .unwrap();
        assert_eq!(listing.exit, 0);
        assert!(listing.text.contains("vendor"));
        assert!(!listing.text.contains("private-key-canary"));
        let refresh =
            execute(&words(&["refresh", "--source", &source]), &context, &cancel).unwrap();
        assert_eq!(refresh.exit, 1);
        assert!(refresh.text.contains("using cached data"));
        assert!(listener.accept().is_err());
        let args = [
            "import",
            "vendor",
            "m",
            "--source",
            source.as_str(),
            "--profile",
            "imported",
            "--wire",
            "openai-compatible",
            "--base-url",
            "https://api.example/v1",
            "--key-env",
            "VENDOR_KEY",
        ];
        let preview = execute(&words(&args), &context, &cancel).unwrap();
        assert!(preview.text.contains("preview only"));
        assert!(preview.text.contains("environment variable VENDOR_KEY"));
        assert!(!preview.text.contains("private-key-canary"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), old);
        let mut apply = words(&args);
        apply.push("--apply".into());
        assert_eq!(execute(&apply, &context, &cancel).unwrap().exit, 0);
        let written = std::fs::read_to_string(&path).unwrap();
        let config = crate::user_config::parse_config_document(&written, "fixture").unwrap();
        assert_eq!(config.models.default.as_deref(), Some("local"));
        assert_eq!(config.models.entries["imported"].env_key, ["VENDOR_KEY"]);
        let next = br#"{"vendor":{"id":"vendor","models":{"other":{"id":"other","limit":{"context":10000}}}}}"#;
        cache.save(next, 111, &cancel).unwrap();
        let later = Context {
            env: &env,
            now: 112,
            gate: Some(Arc::new(Refuse)),
        };
        let check = execute(&words(&["check", "--source", &source]), &later, &cancel).unwrap();
        assert!(check.text.contains("ModelMissing"));
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("[model.imported]")
        );
        let before_forget = std::fs::read_to_string(&path).unwrap();
        let forgotten = execute(&words(&["forget", "--source", &source]), &later, &cancel).unwrap();
        assert!(forgotten.text.contains("profiles were preserved"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before_forget);
        assert!(cache.load(112, 600, &cancel).unwrap().is_none());
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn catalog_argument_validation_rejects_ambiguous_credentials_and_flags() {
        assert!(parse_args(&words(&["list", "--source"])).is_err());
        assert!(
            parse_args(&words(&[
                "list",
                "--source",
                "https://example.com",
                "--source",
                "https://other.example"
            ]))
            .is_err()
        );
        let root = std::env::temp_dir().join(format!(
            "rapid-catalog-cli-empty-{}",
            ::protocol::SessionId::new()
        ));
        let env = vec![(
            "RAPIDLM_HOME".to_owned(),
            root.to_string_lossy().into_owned(),
        )];
        let context = Context {
            env: &env,
            now: 1,
            gate: None,
        };
        assert!(
            execute(
                &words(&["list", "--offline", "--online"]),
                &context,
                &llm_router::CancellationToken::new()
            )
            .is_err()
        );
        assert!(!root.exists());
        let bad = [
            "import",
            "provider",
            "model",
            "--online",
            "--wire",
            "openai-compatible",
            "--key-env",
            "KEY",
            "--keyless",
        ];
        assert!(
            execute(
                &words(&bad),
                &context,
                &llm_router::CancellationToken::new()
            )
            .unwrap_err()
            .contains("credential")
        );
        assert!(!root.exists());
    }
}
