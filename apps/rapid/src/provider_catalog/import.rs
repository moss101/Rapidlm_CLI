//! Reviewable catalog imports. Catalog metadata never chooses credentials or
//! authorizes an endpoint: both are explicit in the import request.
use super::{Endpoint, Lifecycle, Registry, Wire, WireChoice};
use crate::user_config::{ConfigProvider, UserConfig};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CredentialBinding {
    Env(String),
    Keychain(String),
    Account(String),
    Keyless,
    AwsEnvironment,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Request {
    pub catalog_provider: String,
    pub model: String,
    pub profile: String,
    pub wire: ConfigProvider,
    pub base_url: String,
    pub credential: CredentialBinding,
    /// Explicitly allow a wire that is guessed, unsupported, or differs from
    /// the catalog declaration. This never changes the chosen endpoint.
    pub override_wire: bool,
    pub set_default: bool,
    /// Provider-specific non-secret fields, checked by the normal config parser.
    pub native_fields: BTreeMap<String, String>,
}
#[derive(Clone, Eq, PartialEq)]
pub struct Plan {
    pub profile: String,
    pub document: String,
    pub notes: Vec<String>,
}
impl std::fmt::Debug for Plan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plan")
            .field("profile", &self.profile)
            .field("document_bytes", &self.document.len())
            .field("notes", &self.notes)
            .finish()
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportError(pub &'static str);
impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for ImportError {}

/// Read a bounded configuration without creating directories or following a
/// final-component symlink. The returned text is the apply precondition.
pub fn read_existing(path: &std::path::Path) -> Result<Option<String>, ImportError> {
    use std::io::Read;
    match path.symlink_metadata() {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(ImportError("configuration symlinks require manual import"));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(ImportError("configuration is not readable")),
        _ => {}
    }
    let file =
        std::fs::File::open(path).map_err(|_| ImportError("configuration is not readable"))?;
    let mut bytes = Vec::new();
    file.take((crate::user_config::MAX_USER_CONFIG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| ImportError("configuration is not readable"))?;
    if bytes.len() > crate::user_config::MAX_USER_CONFIG_BYTES {
        return Err(ImportError("configuration exceeds the size limit"));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| ImportError("configuration is not UTF-8"))
}

/// Apply a reviewed plan with a source-content precondition, an advisory
/// cross-process import lock, an owner-private backup and atomic replacement.
/// Editors that ignore the lock can still race the final rename; a backup is
/// retained whenever an existing document is replaced.
pub fn apply(
    path: &std::path::Path,
    expected: Option<&str>,
    plan: &Plan,
    cancel: &llm_router::CancellationToken,
) -> Result<Option<std::path::PathBuf>, ImportError> {
    use std::io::Write;
    cancel
        .check()
        .map_err(|_| ImportError("import cancelled"))?;
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|_| ImportError("configuration directory cannot be created"))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(ImportError("invalid configuration path"))?;
    let lock_path = parent.join(format!(".{name}.catalog-import.lock"));
    if lock_path
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(ImportError("import lock is a symlink"));
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options
        .open(&lock_path)
        .map_err(|_| ImportError("import lock cannot be opened"))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        cancel
            .check()
            .map_err(|_| ImportError("import cancelled"))?;
        match lock.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10))
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(ImportError("configuration is busy"));
            }
            Err(_) => return Err(ImportError("import lock failed")),
        }
    }
    let result = (|| {
        if read_existing(path)?.as_deref() != expected {
            return Err(ImportError(
                "configuration changed; review a new import plan",
            ));
        }
        crate::user_config::parse_config_document(&plan.document, "catalog import")
            .map_err(|_| ImportError("planned configuration is invalid"))?;
        let backup = if let Some(previous) = expected {
            let suffix = crate::mcp_oauth::random_token(12)
                .map_err(|_| ImportError("backup name could not be created"))?;
            let backup = parent.join(format!("{name}.catalog-{suffix}.bak"));
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options
                .open(&backup)
                .map_err(|_| ImportError("configuration backup cannot be created"))?;
            if file
                .write_all(previous.as_bytes())
                .and_then(|_| file.sync_all())
                .is_err()
            {
                let _ = std::fs::remove_file(&backup);
                return Err(ImportError("configuration backup cannot be written"));
            }
            Some(backup)
        } else {
            None
        };
        let committed = std::cell::Cell::new(false);
        let write = (|| {
            cancel
                .check()
                .map_err(|_| ImportError("import cancelled"))?;
            if read_existing(path)?.as_deref() != expected {
                return Err(ImportError(
                    "configuration changed; review a new import plan",
                ));
            }
            crate::exec_tools::atomic_write_with_mode(
                path,
                plan.document.as_bytes(),
                Some(crate::setup::CONFIG_FILE_MODE),
            )
            .map_err(|_| ImportError("configuration cannot be written"))?;
            committed.set(true);
            #[cfg(unix)]
            std::fs::File::open(parent)
                .and_then(|dir| dir.sync_all())
                .map_err(|_| ImportError("configuration directory cannot be synchronized"))?;
            Ok(())
        })();
        if let Err(error) = write {
            if !committed.get()
                && let Some(backup) = &backup
            {
                let _ = std::fs::remove_file(backup);
            }
            return Err(error);
        }
        Ok(backup)
    })();
    let _ = lock.unlock();
    result
}

/// Plan one model without changing the existing document or default. A caller
/// may show the resulting TOML and apply it only on an explicit user action.
pub fn plan(
    registry: &Registry,
    request: &Request,
    existing: Option<&str>,
    policy: Option<&crate::managed_config::ManagedPolicy>,
) -> Result<Plan, ImportError> {
    llm_router::ProfileId::parse(&request.profile)
        .map_err(|_| ImportError("invalid profile id"))?;
    crate::provider_accounts::endpoint(&request.base_url)
        .map_err(|_| ImportError("invalid explicit provider endpoint"))?;
    let provider = registry
        .providers
        .get(&request.catalog_provider)
        .ok_or(ImportError("catalog provider is missing"))?;
    let model = provider
        .models
        .get(&request.model)
        .ok_or(ImportError("catalog model is missing"))?;
    if model.lifecycle != Lifecycle::Active {
        return Err(ImportError("model is not active in the catalog"));
    }
    if policy
        .and_then(|policy| policy.allowed_providers())
        .is_some_and(|allowed| !allowed.iter().any(|name| name == request.wire.as_str()))
    {
        return Err(ImportError("provider is not allowed by managed policy"));
    }
    if matches!(request.credential, CredentialBinding::Account(_))
        && policy.is_some_and(|policy| !policy.allow_provider_accounts())
    {
        return Err(ImportError(
            "provider accounts are disabled by managed policy",
        ));
    }
    let advertised = model.wire_override.as_ref().unwrap_or(&provider.wire);
    let wire_matches = matches!(advertised, WireChoice::Declared { wire } if Some(*wire) == config_wire(request.wire));
    if !wire_matches && !request.override_wire {
        return Err(ImportError(
            "catalog wire is uncertain or differs; explicitly override it",
        ));
    }
    let mut notes = vec![
        format!(
            "catalog {} / {} at {}",
            provider.id, model.id, registry.provenance.source
        ),
        format!("snapshot SHA-256 {}", registry.provenance.sha256),
        format!("explicit endpoint {}", request.base_url),
    ];
    if !wire_matches {
        notes.push("wire interpretation explicitly overridden".to_owned());
    }
    let advertised_endpoint = model
        .endpoint_override
        .as_ref()
        .unwrap_or(&provider.endpoint);
    if !matches!(advertised_endpoint, Endpoint::Present(url) if crate::provider_accounts::endpoint(url).ok() == crate::provider_accounts::endpoint(&request.base_url).ok())
    {
        notes.push("explicit endpoint differs from or replaces the catalog endpoint".to_owned());
    }
    let mut doc = match existing {
        Some(text) => text
            .parse::<toml_edit::DocumentMut>()
            .map_err(|_| ImportError("existing configuration is invalid TOML"))?,
        None => toml_edit::DocumentMut::new(),
    };
    if !doc.as_table().contains_key("model") {
        doc["model"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    let models = doc["model"]
        .as_table_mut()
        .ok_or(ImportError("model section is not a table"))?;
    if models.contains_key(&request.profile) {
        return Err(ImportError(
            "profile already exists; import never overwrites it",
        ));
    }
    let mut table = toml_edit::Table::new();
    table.insert("provider", toml_edit::value(request.wire.as_str()));
    table.insert("model", toml_edit::value(&request.model));
    table.insert("base_url", toml_edit::value(&request.base_url));
    if let Some(name) = &model.name {
        table.insert("name", toml_edit::value(name));
    }
    let input_cap = model.max_input.unwrap_or(model.context).min(model.context);
    table.insert("context_window", toml_edit::value(i64::from(input_cap)));
    let max_output = model.max_output.unwrap_or(4096).min(input_cap);
    table.insert("max_tokens", toml_edit::value(i64::from(max_output)));
    if input_cap < model.context {
        notes.push("context window conservatively limited to catalog input cap".to_owned());
    }
    if model.max_output.is_none() {
        notes.push("output cap was absent; a bounded default was used".to_owned());
    }
    table.insert("tool_use", toml_edit::value(model.tools.unwrap_or(false)));
    table.insert("vision", toml_edit::value(model.image_input));
    table.insert(
        "reasoning",
        toml_edit::value(model.reasoning.unwrap_or(false)),
    );
    notes.push(format!(
        "capabilities: context {input_cap}, output {max_output}, tools {}, vision {}, reasoning {}",
        model.tools.unwrap_or(false),
        model.image_input,
        model.reasoning.unwrap_or(false)
    ));
    if model.tools.is_none() {
        notes.push("tool capability was absent; tool use defaults off".to_owned());
    }
    match &request.credential {
        CredentialBinding::Env(name) => {
            table.insert("env_key", toml_edit::value(name));
        }
        CredentialBinding::Keychain(alias) => {
            table.insert("keychain", toml_edit::value(alias));
        }
        CredentialBinding::Account(id) => {
            table.insert("account", toml_edit::value(id));
        }
        CredentialBinding::Keyless | CredentialBinding::AwsEnvironment => {}
    }
    const NATIVE_FIELDS: &[&str] = &[
        "project",
        "location",
        "region",
        "aws_auth",
        "api_version",
        "azure_auth",
        "azure_url_mode",
        "copilot_auth",
        "copilot_token_url",
        "copilot_integration_id",
    ];
    for (key, value) in &request.native_fields {
        if !NATIVE_FIELDS.contains(&key.as_str()) {
            return Err(ImportError("unsupported provider-specific option"));
        }
        table.insert(key, toml_edit::value(value));
        notes.push(format!(
            "provider option {key} = {}",
            toml_edit::value(value)
        ));
    }
    if matches!(request.credential, CredentialBinding::AwsEnvironment)
        && (request.wire != ConfigProvider::Bedrock
            || request
                .native_fields
                .get("aws_auth")
                .is_some_and(|value| value != "sigv4"))
    {
        return Err(ImportError(
            "AWS environment credentials require Bedrock SigV4",
        ));
    }
    if request.wire == ConfigProvider::Bedrock
        && request
            .native_fields
            .get("aws_auth")
            .is_none_or(|value| value == "sigv4")
        && !matches!(request.credential, CredentialBinding::AwsEnvironment)
    {
        return Err(ImportError(
            "Bedrock SigV4 requires explicit AWS environment credentials",
        ));
    }
    let mut binding = toml_edit::InlineTable::new();
    binding.insert(
        "source",
        toml_edit::Value::from(registry.provenance.source.as_str()),
    );
    binding.insert("provider", toml_edit::Value::from(provider.id.as_str()));
    binding.insert("model", toml_edit::Value::from(model.id.as_str()));
    binding.insert(
        "sha256",
        toml_edit::Value::from(registry.provenance.sha256.as_str()),
    );
    table.insert("catalog", toml_edit::value(binding));
    models.insert(&request.profile, toml_edit::Item::Table(table));
    if request.set_default {
        if !doc.as_table().contains_key("models") {
            doc["models"] = toml_edit::Item::Table(toml_edit::Table::new());
        }
        doc["models"]
            .as_table_mut()
            .ok_or(ImportError("models section is not a table"))?
            .insert("default", toml_edit::value(&request.profile));
    }
    let document = doc.to_string();
    if document.len() > crate::user_config::MAX_USER_CONFIG_BYTES {
        return Err(ImportError("result exceeds the configuration size limit"));
    }
    crate::user_config::parse_config_document(&document, "catalog import")
        .map_err(|_| ImportError("imported model fails configuration validation"))?;
    Ok(Plan {
        profile: request.profile.clone(),
        document,
        notes,
    })
}
fn config_wire(provider: ConfigProvider) -> Option<Wire> {
    match provider {
        ConfigProvider::OpenAiCompatible => Some(Wire::OpenAi),
        ConfigProvider::OpenAiResponses => Some(Wire::OpenAiResponses),
        ConfigProvider::Anthropic => Some(Wire::Anthropic),
        ConfigProvider::GoogleGenAi => Some(Wire::GoogleGenAi),
        ConfigProvider::VertexGenAi => Some(Wire::VertexGenAi),
        ConfigProvider::Bedrock => Some(Wire::Bedrock),
        ConfigProvider::AzureChat => Some(Wire::AzureChat),
        _ => None,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Diagnostic {
    ProviderMissing,
    ModelMissing,
    ModelRetired,
    SnapshotChanged,
    ConfigModelChanged,
}
/// Diagnose only imports bound to this source. No model or default is changed.
pub fn diagnose(config: &UserConfig, registry: &Registry) -> BTreeMap<String, Vec<Diagnostic>> {
    let mut diagnostics = BTreeMap::new();
    for (id, entry) in &config.models.entries {
        let Some(binding) = &entry.catalog else {
            continue;
        };
        if binding.source != registry.provenance.source {
            continue;
        }
        let mut rows = Vec::new();
        if binding.model != entry.model {
            rows.push(Diagnostic::ConfigModelChanged);
        }
        if binding.sha256 != registry.provenance.sha256 {
            rows.push(Diagnostic::SnapshotChanged);
        }
        match registry.providers.get(&binding.provider) {
            None => rows.push(Diagnostic::ProviderMissing),
            Some(provider) => match provider.models.get(&binding.model) {
                None => rows.push(Diagnostic::ModelMissing),
                Some(model) if model.lifecycle != Lifecycle::Active => {
                    rows.push(Diagnostic::ModelRetired)
                }
                _ => {}
            },
        }
        if !rows.is_empty() {
            diagnostics.insert(id.clone(), rows);
        }
    }
    diagnostics
}

#[cfg(test)]
mod tests {
    use super::*;
    fn registry() -> Registry {
        super::super::parse("https://models.example/api.json", br#"{"vendor":{"id":"vendor","npm":"@ai-sdk/openai-compatible","api":"https://api.example/v1","models":{"m":{"id":"m","limit":{"context":100000,"input":80000,"output":4000},"tool_call":false,"modalities":{"input":["text","image"],"output":["text"]}},"old":{"id":"old","status":"deprecated","limit":{"context":4096}}}}}"#, 100).unwrap()
    }
    fn request() -> Request {
        Request {
            catalog_provider: "vendor".into(),
            model: "m".into(),
            profile: "imported".into(),
            wire: ConfigProvider::OpenAiCompatible,
            base_url: "https://api.example/v1".into(),
            credential: CredentialBinding::Env("VENDOR_KEY".into()),
            override_wire: false,
            set_default: false,
            native_fields: BTreeMap::new(),
        }
    }
    const EXISTING: &str = "# Keep my comment\n[models]\ndefault = \"local\"\n[model.local]\nprovider = \"openai-compatible\"\nmodel = \"local\"\nbase_url = \"http://127.0.0.1:8080/v1\"\n";
    #[test]
    fn import_plan_requires_explicit_binding_and_preserves_existing_default_and_models() {
        let registry = registry();
        let result = plan(&registry, &request(), Some(EXISTING), None).unwrap();
        assert!(result.document.contains("# Keep my comment"));
        let config =
            crate::user_config::parse_config_document(&result.document, "fixture").unwrap();
        assert_eq!(config.models.default.as_deref(), Some("local"));
        assert!(config.models.entries.contains_key("local"));
        let imported = &config.models.entries["imported"];
        assert_eq!(imported.env_key, ["VENDOR_KEY"]);
        assert_eq!(imported.context_window, Some(80000));
        assert_eq!(imported.max_tokens, Some(4000));
        assert_eq!(imported.tool_use, Some(false));
        assert_eq!(imported.vision, Some(true));
        assert_eq!(
            imported.catalog.as_ref().unwrap().sha256,
            registry.provenance.sha256
        );
        assert!(result.notes.iter().any(|note| note.contains("input cap")));
        assert!(diagnose(&config, &registry).is_empty());
        let mut default_request = request();
        default_request.set_default = true;
        let changed = plan(&registry, &default_request, Some(EXISTING), None).unwrap();
        assert_eq!(
            crate::user_config::parse_config_document(&changed.document, "fixture")
                .unwrap()
                .models
                .default
                .as_deref(),
            Some("imported")
        );
        assert!(plan(&registry, &request(), Some(&result.document), None).is_err());
    }
    #[test]
    fn imports_reject_unreviewed_wires_bad_credentials_and_retired_models() {
        let mut registry = registry();
        registry.providers.get_mut("vendor").unwrap().wire = WireChoice::GuessedOpenAi;
        assert!(plan(&registry, &request(), Some(EXISTING), None).is_err());
        let mut overridden = request();
        overridden.override_wire = true;
        assert!(
            plan(&registry, &overridden, Some(EXISTING), None)
                .unwrap()
                .notes
                .iter()
                .any(|note| note.contains("overridden"))
        );
        overridden.credential = CredentialBinding::Env("NOT A VARIABLE".into());
        assert!(plan(&registry, &overridden, Some(EXISTING), None).is_err());
        overridden.credential = CredentialBinding::Account("unknown".into());
        assert!(plan(&registry, &overridden, Some(EXISTING), None).is_err());
        overridden.model = "old".into();
        overridden.credential = CredentialBinding::Keyless;
        assert!(plan(&registry, &overridden, Some(EXISTING), None).is_err());
        overridden.model = "m".into();
        overridden.base_url = "https://user:secret@example.com/v1".into();
        assert!(plan(&registry, &overridden, Some(EXISTING), None).is_err());
    }
    #[test]
    fn refresh_diagnostics_report_removed_or_changed_models_without_mutating_config() {
        let original = registry();
        let planned = plan(&original, &request(), Some(EXISTING), None).unwrap();
        let config =
            crate::user_config::parse_config_document(&planned.document, "fixture").unwrap();
        let mut updated = original.clone();
        updated
            .providers
            .get_mut("vendor")
            .unwrap()
            .models
            .remove("m");
        updated.provenance.sha256 = "a".repeat(64);
        assert_eq!(
            diagnose(&config, &updated)["imported"],
            [Diagnostic::SnapshotChanged, Diagnostic::ModelMissing]
        );
        assert_eq!(config.models.default.as_deref(), Some("local"));
        assert!(config.models.entries.contains_key("imported"));
        updated.providers.remove("vendor");
        assert!(diagnose(&config, &updated)["imported"].contains(&Diagnostic::ProviderMissing));
    }
    #[test]
    fn apply_keeps_a_backup_refuses_changed_files_and_redacts_existing_secrets() {
        let root = std::env::temp_dir().join(format!(
            "rapid-catalog-import-{}",
            ::protocol::SessionId::new()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("config.toml");
        let existing = format!("{EXISTING}api_key = \"private-existing-canary\"\n");
        std::fs::write(&path, &existing).unwrap();
        let plan = plan(&registry(), &request(), Some(&existing), None).unwrap();
        assert!(!format!("{plan:?}").contains("private-existing-canary"));
        let cancel = llm_router::CancellationToken::new();
        std::fs::write(&path, format!("{existing}# another edit\n")).unwrap();
        assert!(apply(&path, Some(&existing), &plan, &cancel).is_err());
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .ends_with("# another edit\n")
        );
        std::fs::write(&path, &existing).unwrap();
        let backup = apply(&path, Some(&existing), &plan, &cancel)
            .unwrap()
            .unwrap();
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), existing);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), plan.document);
        let before = std::fs::read(&path).unwrap();
        cancel.cancel();
        assert!(apply(&path, Some(&plan.document), &plan, &cancel).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let _ = std::fs::remove_dir_all(root);
    }
}
