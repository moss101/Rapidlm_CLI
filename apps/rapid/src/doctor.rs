//! `rapid doctor`: a deterministic, read-only diagnosis of whether Rapid can
//! actually operate in this environment and project.
//!
//! Every check drives the *same* production code path the real commands do —
//! `user_config`'s config resolution, `interactive::resolve_model_plan`/
//! `build_backing_model`'s layered model selection, `context_budget_for`'s
//! model-derived budget, `canonicalize_dir`/`detect_project_root`/
//! `ProjectTrustStore` for project identity and trust, the tiered `sandbox`
//! crate's real backends, and `security::evaluate_doctor` for the five
//! security-posture checks — rather than a second, doctor-local
//! interpretation of "healthy". Nothing here parses config, resolves a
//! model, or derives a context budget on its own.
//!
//! Guarantees this module holds to, each covered by a test below or in
//! `apps/rapid/tests/doctor_cli.rs`:
//!
//!   - **Offline.** No check performs a network request or a billable model
//!     call. Provider configuration is validated locally (endpoint parsing,
//!     capability pinning, credential seeding into a process-local store);
//!     connectivity is reported as untested, never as verified.
//!   - **Read-only.** No check grants or revokes trust, rewrites config,
//!     installs or approves a plugin, executes a hook or a scanner, or
//!     touches project source, `goal.json`, or evidence. The one write any
//!     check performs is a uniquely-named temp file used to prove a
//!     directory is writable, removed immediately.
//!   - **Secret-safe.** The rendered report is passed through
//!     `security::SecretRedactionRegistry` seeded with every credential the
//!     resolved model config actually produced, so even a provider error
//!     that quoted a key could not print it.
//!   - **Deterministic.** Checks run in a fixed sequence and render in that
//!     order; no hash-map iteration or completion order is observable.
//!
//! Exit semantics: `0` unless at least one check is [`DoctorStatus::Fail`].
//! Warnings and skips never fail the command — an absent optional
//! integration is not a broken installation.

use std::fmt::{self, Display, Formatter};
use std::path::{Path, PathBuf};
use std::time::Duration;

use kernel::{CancellationToken, ProjectIdentity, ProjectTrustStore, TrustStatus};

use crate::interactive::{
    FoundProject, GIT_MARKER, PROJECT_MARKER, TRUST_CATALOG_NAME, build_backing_model,
    context_budget_for, context_budget_source, load_project_integrations, resolve_model_plan,
    resolve_project_root, user_home_from,
};
use crate::user_config::{ConfigSource, CredentialSource};

/// Wall-clock ceiling for the sandbox smoke probe. Generous enough for a
/// cold backend start on slow hardware, short enough that `rapid doctor`
/// stays a few-second command even when the probe cannot complete.
const SANDBOX_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Output ceiling for the sandbox smoke probe. It runs one `echo`.
const SANDBOX_PROBE_OUTPUT_LIMIT: u64 = 4096;

/// The token the sandbox smoke probe echoes and then looks for.
const SANDBOX_PROBE_TOKEN: &str = "rapidlm-doctor-sandbox-probe";

/// Wall-clock ceiling for `git --version`.
const GIT_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Poll interval while waiting for `git --version` to exit.
const GIT_PROBE_POLL: Duration = Duration::from_millis(10);

/// Byte ceiling on what `git --version` may write before its pipe is closed.
const GIT_PROBE_OUTPUT_LIMIT: u64 = 4096;

/// Wall-clock ceiling for the platform keychain probe. The Linux backend
/// shells out to `secret-tool lookup --unlock`, which can block on a keyring
/// unlock prompt; on a headless box that would hang the whole command.
const KEYCHAIN_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Cap on how many credential canaries are seeded into the redaction
/// registry. Comfortably above the reachable maximum — a plan holds at most
/// `1 + user_config::MAX_FALLBACK_MODELS` entries, each contributing up to
/// three values (resolved key, `base_url` userinfo, its password half) — and
/// far below `security::MAX_REGISTERED_SECRETS` (1024). Values past the cap
/// are still scrubbed by `finalize`'s literal backstop; only their encoded
/// variants would be missed.
const MAX_REDACTION_CANARIES: usize = 128;

/// Shortest `base_url` userinfo with **no** password component that is still
/// treated as an opaque token (`http://ghp_…@host`) rather than a username.
/// See [`base_url_userinfo_secrets`] for why a floor is needed at all.
const MIN_BARE_USERINFO_TOKEN: usize = 8;

/// Shortest `base_url` password half registered as a secret on its own.
/// Anything shorter is still scrubbed through the full `user:password` form.
const MIN_USERINFO_PASSWORD: usize = 4;

/// Replacement text for the literal backstop in [`finalize`].
const REDACTED: &str = "<redacted>";

/// Cap on any single rendered detail line, so a pathological config value
/// cannot turn one check row into an unbounded dump.
const MAX_DETAIL_BYTES: usize = 512;

/// Check outcome.
///
/// The `Fail`/`Warn` split is the whole point of the model and is decided by
/// one question only: *is the thing being checked required for Rapid's core
/// behavior?* A malformed config or a model that cannot be constructed is a
/// `Fail` — no turn can run. A missing optional integration (no external
/// scanner, no MCP server, no platform keychain, an untrusted project) is a
/// `Warn`: Rapid runs, with a named capability unavailable. `Skipped` means
/// the check does not apply here at all, either because a prerequisite
/// failed or because the environment has nothing for it to look at.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum DoctorStatus {
    /// Healthy.
    Pass,
    /// Not applicable, or a prerequisite was unavailable. Never a failure.
    Skipped,
    /// Rapid can operate; an optional or degraded capability is unavailable.
    Warn,
    /// Something required for core behavior is invalid or unusable.
    Fail,
}

impl DoctorStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Skipped => "SKIP",
            Self::Warn => "WARN",
            Self::Fail => "FAIL",
        }
    }

    pub const fn is_fail(self) -> bool {
        matches!(self, Self::Fail)
    }
}

impl Display for DoctorStatus {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One diagnostic row: stable identity, status, a concise explanation, and a
/// remediation when there is a real action to take.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoctorCheck {
    id: &'static str,
    status: DoctorStatus,
    detail: String,
    remediation: Option<String>,
}

impl DoctorCheck {
    // Detail and remediation are stored **raw**: bounding and control-
    // character sanitization happen in `finalize`, after redaction, because
    // truncating first would split a long secret across the cut and leave the
    // surviving prefix unmatched by the redaction registry's full-length
    // needle — a real leak, found in review.
    fn new(
        id: &'static str,
        status: DoctorStatus,
        detail: impl Into<String>,
        remediation: Option<String>,
    ) -> Self {
        Self {
            id,
            status,
            detail: detail.into(),
            remediation,
        }
    }

    fn pass(id: &'static str, detail: impl Into<String>) -> Self {
        Self::new(id, DoctorStatus::Pass, detail, None)
    }

    fn skipped(id: &'static str, detail: impl Into<String>) -> Self {
        Self::new(id, DoctorStatus::Skipped, detail, None)
    }

    fn warn(id: &'static str, detail: impl Into<String>, remediation: impl Into<String>) -> Self {
        Self::new(id, DoctorStatus::Warn, detail, Some(remediation.into()))
    }

    fn fail(id: &'static str, detail: impl Into<String>, remediation: impl Into<String>) -> Self {
        Self::new(id, DoctorStatus::Fail, detail, Some(remediation.into()))
    }

    pub fn id(&self) -> &'static str {
        self.id
    }

    pub fn status(&self) -> DoctorStatus {
        self.status
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }

    pub fn remediation(&self) -> Option<&str> {
        self.remediation.as_deref()
    }
}

/// The full diagnosis, as structured data. Rendering is a separate step
/// ([`DoctorReport::render`]) so no check ever writes to stdout itself.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DoctorReport {
    checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    pub fn checks(&self) -> &[DoctorCheck] {
        &self.checks
    }

    pub fn check(&self, id: &str) -> Option<&DoctorCheck> {
        self.checks.iter().find(|check| check.id == id)
    }

    pub fn failures(&self) -> usize {
        self.count(DoctorStatus::Fail)
    }

    pub fn warnings(&self) -> usize {
        self.count(DoctorStatus::Warn)
    }

    fn count(&self, status: DoctorStatus) -> usize {
        self.checks
            .iter()
            .filter(|check| check.status == status)
            .count()
    }

    /// `0` when nothing failed. Warnings alone never fail the command: an
    /// absent optional integration must not break a CI gate that only wants
    /// to know whether Rapid can run at all.
    pub fn exit_code(&self) -> i32 {
        if self.checks.iter().any(|check| check.status.is_fail()) {
            1
        } else {
            0
        }
    }

    /// Concise, scannable, colour-free rendering. Rows appear in the fixed
    /// order the checks ran in; a remediation is indented under its row.
    pub fn render(&self) -> String {
        let width = self
            .checks
            .iter()
            .map(|check| check.id.len())
            .max()
            .unwrap_or(0);
        let mut out = String::new();
        for check in &self.checks {
            out.push_str(&format!(
                "{}  {:<width$}  {}\n",
                check.status.as_str(),
                check.id,
                check.detail,
                width = width
            ));
            if let Some(remediation) = &check.remediation {
                out.push_str(&format!(
                    "      {:<width$}  -> {remediation}\n",
                    "",
                    width = width
                ));
            }
        }
        out.push_str(&format!(
            "\n{} checks: {} failed, {} warnings\n",
            self.checks.len(),
            self.failures(),
            self.warnings()
        ));
        out
    }
}

/// Everything a diagnosis needs from the outside world, so tests can drive
/// the real checks against an isolated home/project/env instead of the
/// machine's own.
pub struct DoctorEnv {
    /// Process working directory the diagnosis is rooted at.
    pub cwd: PathBuf,
    /// Process environment, as `user_config`'s own resolution expects it.
    pub env: Vec<(String, String)>,
    /// Run the real sandbox smoke probe. Off only for tests that must stay
    /// hermetic on hosts where spawning a sandboxed child is not meaningful.
    pub sandbox_probe: bool,
}

impl DoctorEnv {
    /// The real process environment.
    pub fn from_process() -> Self {
        Self {
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            env: std::env::vars().collect(),
            sandbox_probe: true,
        }
    }
}

/// Run every check, in order, and return the structured report.
///
/// Checks are independent by default: a failing one does not stop the rest.
/// Where a genuine prerequisite is missing (unparsable config, no resolvable
/// project directory) the dependent checks are marked `Skipped` with the
/// reason rather than emitting cascading fake failures.
pub fn diagnose(env: &DoctorEnv) -> DoctorReport {
    let mut checks: Vec<DoctorCheck> = Vec::new();
    let cancel = CancellationToken::new();

    checks.push(check_environment());

    let home = user_home_from(&env.env);
    checks.push(check_home(home.as_deref()));

    // --- configuration / model / budget ---------------------------------
    let config_source = crate::user_config::resolve_config_source(&env.env);
    let config_path = match &config_source {
        ConfigSource::ExplicitPath(path) | ConfigSource::HomeFallback(path) => path.clone(),
    };
    let loaded = crate::user_config::load_config(&config_source);
    checks.push(check_config(&config_path, &loaded));

    // The one place the whole model half is resolved: the same
    // `resolve_model_plan` + `build_backing_model` pair `rapid exec` runs.
    // Lines it would have printed to stderr are collected and folded into
    // the `model` row instead of becoming a variable extra row, so the
    // report always carries the same checks in the same order.
    // `NOT_CONFIGURED_HINT` is dropped here: the unconfigured branch below
    // states the same fact with a real remediation.
    let mut notes: Vec<String> = Vec::new();
    let mut sink = |line: &str| {
        if line != crate::interactive::NOT_CONFIGURED_HINT {
            notes.push(line.to_owned());
        }
    };
    let plan = resolve_model_plan(
        &env.env,
        agent_runtime::reminders::ReminderFloor::Baseline,
        &mut sink,
    );

    // Credentials are read off the resolved plan (never the raw file), so
    // the precedence reported is production's own: inline `api_key`, then
    // the first set non-empty `env_key`, then keyless.
    let mut secrets: Vec<String> = Vec::new();
    if let Ok(plan) = &plan {
        for model in &plan.models {
            if let Some(plaintext) = model.credential.plaintext.as_deref() {
                secrets.push(plaintext.to_owned());
            }
            // Credentials embedded in a `base_url`'s userinfo are not the
            // *resolved* credential (a keyless entry has none at all), yet
            // the URL is printed on the model row and quoted verbatim by
            // `ModelConfigError::BaseUrl` when construction rejects it — so
            // register those bytes too rather than assuming the resolved
            // credential is the only secret that can reach the report.
            secrets.extend(base_url_userinfo_secrets(&model.entry.base_url));
        }
    }

    match &plan {
        Err(err) => {
            let detail = format!("{err}");
            checks.push(DoctorCheck::fail(
                "model",
                detail,
                "fix the model configuration named above; no turn can run until it resolves",
            ));
            checks.push(DoctorCheck::skipped("credentials", "model unresolved"));
            checks.push(DoctorCheck::skipped("context-budget", "model unresolved"));
        }
        Ok(plan) if plan.unconfigured => {
            checks.push(DoctorCheck::warn(
                "model",
                format!("no model configured (searched {})", config_path.display()),
                "add a [models] default and a [model.<id>] entry to the config file above",
            ));
            checks.push(DoctorCheck::skipped("credentials", "no model configured"));
            // The typed fallback still has a real, production-derived budget
            // (`context_budget_for`'s Unconfigured arm), so this stays a
            // genuine reading rather than a skip.
            let stores: Vec<auth::InMemoryCredentialStore> = Vec::new();
            let mut discard = |_: &str| {};
            match build_backing_model(&[], &stores, true, None, None, &mut discard) {
                Ok((backing, _)) => checks.push(check_context_budget(&backing, 0)),
                Err(err) => checks.push(DoctorCheck::fail(
                    "context-budget",
                    format!("{err}"),
                    "the typed no-model fallback failed to construct; this is an internal fault",
                )),
            }
        }
        Ok(plan) => {
            let stores: Vec<auth::InMemoryCredentialStore> = plan
                .models
                .iter()
                .map(|_| auth::InMemoryCredentialStore::new())
                .collect();
            // Only notes that actually describe the *model* resolution
            // downgrade the model row. `unknown config key '…'` is already
            // reported, with its own correct remediation, by the `config`
            // row; repeating it here cost the model row its `PASS` and
            // pointed the user at fallback entries that may not exist.
            let mut build_notes: Vec<String> = notes
                .iter()
                .filter(|note| !note.contains("unknown config key"))
                .cloned()
                .collect();
            let mut build_sink = |line: &str| build_notes.push(line.to_owned());
            match build_backing_model(&plan.models, &stores, false, None, None, &mut build_sink) {
                Ok((backing, _)) => {
                    checks.push(check_model(plan, &build_notes));
                    checks.push(check_credentials(plan));
                    checks.push(check_context_budget(&backing, plan.models.len()));
                }
                Err(err) => {
                    checks.push(DoctorCheck::fail(
                        "model",
                        format!("{err}"),
                        "fix the provider entry named above (provider, model, base_url, api key)",
                    ));
                    checks.push(check_credentials(plan));
                    checks.push(DoctorCheck::skipped(
                        "context-budget",
                        "model could not be constructed",
                    ));
                }
            }
        }
    }

    // --- project / trust -------------------------------------------------
    let project = resolve_project_root(&env.cwd, &cancel);
    checks.push(check_project(&project));

    let root = project.as_ref().ok().map(|found| found.root.clone());
    let identity = root
        .as_deref()
        .and_then(|root| ProjectIdentity::new(root, None).ok());
    let trust = match (&home, &identity) {
        (Some(home), Some(identity)) => {
            let store = ProjectTrustStore::open(home.join(TRUST_CATALOG_NAME));
            Some((
                store.catalog_path().to_path_buf(),
                store.get(identity, &cancel),
            ))
        }
        _ => None,
    };
    checks.push(check_project_trust(trust.as_ref().map(|(_, got)| got)));
    checks.push(check_trust_store(trust.as_ref()));
    checks.push(check_workspace_tools(trust.as_ref().map(|(_, got)| got)));

    // --- sandbox ---------------------------------------------------------
    // The manager whose backend this platform's real `shell_exec --sandbox`
    // would select, so both the availability row (via `security::
    // evaluate_doctor`) and the smoke probe describe the production path.
    let seatbelt = crate::exec_tools::find_sandbox_exec().is_some();
    let manager = if seatbelt {
        crate::sandbox_exec::build_manager_seatbelt()
    } else {
        crate::sandbox_exec::build_manager()
    };

    // The five security-posture checks, driven with real observations rather
    // than the empty `DoctorRequest::default()` the previous `rapid doctor`
    // passed (which made every one of them report `Unavailable`).
    // Read once, used by both the security engine's executable-surface
    // observation and the hooks/MCP rows below: two independent reads could
    // observe different files if project settings changed mid-run, making
    // `project-config-exposure` describe something `hooks`/`mcp` did not.
    let integrations = root.as_deref().map(load_project_integrations);
    let security_report = evaluate_security(
        &manager,
        trust
            .as_ref()
            .and_then(|(_, got)| got.as_ref().ok().copied()),
        root.as_deref(),
        integrations.as_ref(),
    );
    checks.push(check_sandbox_availability(&security_report));
    checks.push(check_sandbox_probe(env.sandbox_probe, &manager, seatbelt));

    // --- project-scoped tooling -----------------------------------------
    let marked = project
        .as_ref()
        .map(|found| found.marker.is_some())
        .unwrap_or(false);
    checks.push(check_git(root.as_deref()));
    match root
        .as_deref()
        .filter(|_| marked)
        .zip(integrations.as_ref())
    {
        Some((root, integrations)) => {
            checks.push(check_scanner(root));
            checks.push(check_hooks(root, integrations));
            checks.push(check_mcp(integrations, trust.as_ref().map(|(_, got)| got)));
            checks.push(check_plugins(root));
        }
        None => {
            for id in ["scanner", "hooks", "mcp", "plugins"] {
                checks.push(DoctorCheck::skipped(
                    id,
                    "not inside a RapidLM or git project",
                ));
            }
        }
    }

    // --- remaining security-posture rows ---------------------------------
    checks.push(check_credential_store(&security_report));
    checks.push(check_project_config_exposure(&security_report));
    checks.push(check_security_policy(&security_report));
    checks.push(check_release_signature(&security_report));

    finalize(DoctorReport { checks }, &secrets)
}

// -------------------------------------------------------------------------
// individual checks
// -------------------------------------------------------------------------

fn check_environment() -> DoctorCheck {
    DoctorCheck::pass(
        "environment",
        format!(
            "rapid {} on {} {}",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH
        ),
    )
}

fn check_home(home: Option<&Path>) -> DoctorCheck {
    let Some(home) = home else {
        return DoctorCheck::fail(
            "home",
            "no RapidLM home: neither RAPIDLM_HOME nor HOME/USERPROFILE is set",
            "set HOME (or RAPIDLM_HOME) so config, trust, and session state have a location",
        );
    };
    match writable_probe(home) {
        Ok(()) => DoctorCheck::pass("home", format!("{} (readable, writable)", home.display())),
        Err(reason) => DoctorCheck::fail(
            "home",
            format!("{} is not writable: {reason}", home.display()),
            "fix the permissions on the RapidLM home directory; trust and session state need it",
        ),
    }
}

fn check_config(
    path: &Path,
    loaded: &Result<Option<crate::user_config::UserConfig>, crate::user_config::UserConfigError>,
) -> DoctorCheck {
    match loaded {
        Ok(Some(config)) => {
            let default = config
                .models
                .default
                .as_deref()
                .unwrap_or("<unset>")
                .to_owned();
            let mut detail = format!(
                "{} parsed: {} model(s), default '{}'",
                path.display(),
                config.models.entries.len(),
                default
            );
            if !config.models.fallback.is_empty() {
                detail.push_str(&format!(
                    ", fallback chain of {}",
                    config.models.fallback.len()
                ));
            }
            if config.unknown_keys.is_empty() {
                DoctorCheck::pass("config", detail)
            } else {
                DoctorCheck::warn(
                    "config",
                    format!("{detail}; {} unknown key(s)", config.unknown_keys.len()),
                    "remove or correct the unrecognized keys; they are ignored at runtime",
                )
            }
        }
        // Absent config at the *default* path is a supported, documented
        // state: Rapid starts and reports the typed no-model fallback rather
        // than failing. (`load_config` only ever returns `Ok(None)` for the
        // home fallback — an absent explicit path is the typed error below.)
        Ok(None) => DoctorCheck::warn(
            "config",
            format!("no config file at {}", path.display()),
            "create the config file above with a [models] default and a [model.<id>] entry",
        ),
        // An *explicitly named* config file that is absent is different: the
        // user pointed `RAPIDLM_CONFIG` at something that is not there, so
        // say that plainly instead of calling a missing file "invalid".
        Err(crate::user_config::UserConfigError::ExplicitConfigMissing { .. }) => {
            DoctorCheck::fail(
                "config",
                format!(
                    "{} does not exist (named by RAPIDLM_CONFIG)",
                    path.display()
                ),
                "point RAPIDLM_CONFIG at an existing config file, or unset it to use the default",
            )
        }
        Err(err) => DoctorCheck::fail(
            "config",
            format!("{} is invalid: {err}", path.display()),
            "fix the configuration error above; no model can resolve until it parses",
        ),
    }
}

fn check_model(plan: &crate::interactive::ModelPlan, build_notes: &[String]) -> DoctorCheck {
    let Some(primary) = plan.models.first() else {
        return DoctorCheck::skipped("model", "no model entries resolved");
    };
    let mut detail = format!(
        "{} '{}' via profile '{}' at {} (constructed; connectivity not tested)",
        primary.entry.provider.as_str(),
        primary.entry.model,
        primary.profile_id,
        primary.entry.base_url
    );
    if plan.models.len() > 1 {
        let chain: Vec<&str> = plan
            .models
            .iter()
            .map(|model| model.profile_id.as_str())
            .collect();
        detail.push_str(&format!("; fallback chain {}", chain.join(" -> ")));
    }
    if build_notes.is_empty() {
        DoctorCheck::pass("model", detail)
    } else {
        DoctorCheck::warn(
            "model",
            format!("{detail}; {}", build_notes.join("; ")),
            "review the model-resolution notes above (fallback entries and managed-policy \
             gating); the primary model itself resolved",
        )
    }
}

fn check_credentials(plan: &crate::interactive::ModelPlan) -> DoctorCheck {
    let Some(primary) = plan.models.first() else {
        return DoctorCheck::skipped("credentials", "no model entries resolved");
    };
    // Never the value: only which source production's own
    // `resolve_credential` precedence selected, and whether it produced
    // anything at all.
    match &primary.credential.source {
        CredentialSource::InlineApiKey => DoctorCheck::pass(
            "credentials",
            format!(
                "profile '{}': inline api_key configured",
                primary.profile_id
            ),
        ),
        CredentialSource::EnvVar(name) => DoctorCheck::pass(
            "credentials",
            format!("profile '{}': {name} configured", primary.profile_id),
        ),
        // Keyless is a real, supported configuration (a local server needs no
        // key), and is also what a set-but-empty `env_key` degrades to — so
        // this reports the fact and names both readings rather than guessing.
        CredentialSource::Keyless => {
            if primary.entry.env_key.is_empty() {
                DoctorCheck::pass(
                    "credentials",
                    format!(
                        "profile '{}': keyless (no api_key or env_key configured)",
                        primary.profile_id
                    ),
                )
            } else {
                DoctorCheck::warn(
                    "credentials",
                    format!(
                        "profile '{}': env_key names {} but none is set and non-empty; \
                         falling back to keyless",
                        primary.profile_id,
                        primary.entry.env_key.join(", ")
                    ),
                    "export one of the environment variables above, or remove env_key if the \
                     provider needs no credential",
                )
            }
        }
    }
}

fn check_context_budget(
    backing: &crate::model::SelectedModel<'_>,
    candidates: usize,
) -> DoctorCheck {
    // Both numbers come from `context_budget_for` — the single production
    // derivation (a chain uses the minimum context_limit with the maximum
    // max_output across candidates). Nothing is recomputed here.
    let (context_limit, output_reserve) = context_budget_for(backing);
    let source = context_budget_source(backing, candidates);
    let input_budget = context_limit.saturating_sub(output_reserve);
    let detail = format!(
        "context_window={context_limit} output_reserve={output_reserve} \
         input_budget={input_budget} source={source}"
    );
    if input_budget == 0 {
        return DoctorCheck::fail(
            "context-budget",
            detail,
            "the reserved output leaves no room for input; lower max_tokens or raise \
             context_window on the model entry",
        );
    }
    DoctorCheck::pass("context-budget", detail)
}

fn check_project(project: &Result<FoundProject, String>) -> DoctorCheck {
    match project {
        Ok(found) => match found.marker {
            Some(marker) => DoctorCheck::pass(
                "project",
                format!("{} (marker {marker})", found.root.display()),
            ),
            None => DoctorCheck::pass(
                "project",
                format!(
                    "{} (no .rapidlm or .git marker above the working directory; \
                     project-scoped checks skipped)",
                    found.root.display()
                ),
            ),
        },
        Err(reason) => DoctorCheck::fail(
            "project",
            format!("working directory could not be resolved: {reason}"),
            "run rapid from an existing, readable directory",
        ),
    }
}

fn check_project_trust(
    trust: Option<&Result<TrustStatus, kernel::ProjectTrustError>>,
) -> DoctorCheck {
    match trust {
        None => DoctorCheck::skipped("project-trust", "no project or RapidLM home resolved"),
        Some(Ok(TrustStatus::Trusted)) => DoctorCheck::pass("project-trust", "trusted"),
        Some(Ok(TrustStatus::Untrusted)) => DoctorCheck::warn(
            "project-trust",
            "untrusted (workspace file/shell tools, retrieval, hooks, and MCP stay disabled)",
            "run `rapid trust grant` in this project to approve it",
        ),
        // Fail closed and say so: an unreadable or corrupt trust catalog must
        // never be reported as "untrusted, all fine".
        Some(Err(err)) => DoctorCheck::fail(
            "project-trust",
            format!("trust state could not be read: {err}"),
            "repair or remove the project-trust catalog, then run `rapid trust grant` again",
        ),
    }
}

fn check_trust_store(
    trust: Option<&(PathBuf, Result<TrustStatus, kernel::ProjectTrustError>)>,
) -> DoctorCheck {
    let Some((path, result)) = trust else {
        return DoctorCheck::skipped("trust-store", "no project or RapidLM home resolved");
    };
    let exists = path.exists();
    match result {
        // A successful read is proof the catalog parsed under the real
        // store's own schema/bounds checks; an absent catalog is the
        // ordinary empty-untrusted state, not a fault.
        Ok(_) if exists => DoctorCheck::pass(
            "trust-store",
            format!("{} readable and parsed", path.display()),
        ),
        Ok(_) => DoctorCheck::pass(
            "trust-store",
            format!("{} not created yet (empty, untrusted)", path.display()),
        ),
        Err(err) => DoctorCheck::fail(
            "trust-store",
            format!("{} is unusable: {err}", path.display()),
            "repair or remove the trust catalog above; trust decisions cannot be read",
        ),
    }
}

fn check_workspace_tools(
    trust: Option<&Result<TrustStatus, kernel::ProjectTrustError>>,
) -> DoctorCheck {
    match trust {
        None => DoctorCheck::skipped("workspace-tools", "no project or RapidLM home resolved"),
        Some(Ok(TrustStatus::Trusted)) => {
            DoctorCheck::pass("workspace-tools", "enabled (project is trusted)")
        }
        // An intentional trust denial is not a software fault: it is the
        // security boundary working. Warn with the action, never fail.
        Some(Ok(TrustStatus::Untrusted)) => DoctorCheck::warn(
            "workspace-tools",
            "disabled by design while the project is untrusted",
            "run `rapid trust grant` in this project to enable them",
        ),
        Some(Err(_)) => DoctorCheck::skipped("workspace-tools", "trust state unavailable"),
    }
}

fn check_git(root: Option<&Path>) -> DoctorCheck {
    let version = match git_version() {
        Some(version) => version,
        // git backs commit/merge gating and `.git` project detection, but no
        // core turn requires it — a missing git degrades, it does not break.
        None => {
            return DoctorCheck::warn(
                "git",
                "git executable not found, not runnable, or did not respond in time",
                "install git to enable repository-backed features (commit/merge gating)",
            );
        }
    };
    match root {
        Some(root) if root.join(GIT_MARKER).exists() => {
            DoctorCheck::pass("git", format!("{version}; project is a git repository"))
        }
        _ => DoctorCheck::pass(
            "git",
            format!("{version}; this directory is not a git repository"),
        ),
    }
}

/// `git --version`, bounded in both time and bytes.
///
/// Not `Command::output()`: that blocks to EOF with no deadline and buffers
/// the whole stream, so a `git` shim earlier on `$PATH` that never exits (or
/// that streams forever) would hang or exhaust `rapid doctor` — the one check
/// here that runs an arbitrary `$PATH` program, and the only one that was
/// unbounded. The reader thread stops at `GIT_PROBE_OUTPUT_LIMIT` bytes and
/// drops the pipe, which kills a chatty child; the deadline loop kills a
/// silent one, which in turn releases the reader.
fn git_version() -> Option<String> {
    use std::io::Read;

    let mut child = std::process::Command::new("git")
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout
            .by_ref()
            .take(GIT_PROBE_OUTPUT_LIMIT)
            .read_to_end(&mut buffer);
        buffer
    });

    let deadline = std::time::Instant::now() + GIT_PROBE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(GIT_PROBE_POLL),
            Err(_) => break None,
        }
    };
    let buffer = reader.join().ok()?;
    if !status.is_some_and(|status| status.success()) {
        return None;
    }
    let text = String::from_utf8_lossy(&buffer).trim().to_owned();
    if text.is_empty() { None } else { Some(text) }
}

fn check_scanner(root: &Path) -> DoctorCheck {
    match crate::external_scan::load_scanners_config(root) {
        Ok(entries) if entries.is_empty() => DoctorCheck::skipped(
            "scanner",
            format!(
                "no {} (scanner-gated commit/merge automation is off)",
                crate::external_scan::SCANNERS_CONFIG_PATH
            ),
        ),
        Ok(entries) => {
            // Availability only: the scanner is never executed here. A
            // configured-but-missing executable degrades the optional
            // commit/merge gate, so it warns rather than fails.
            let mut missing = Vec::new();
            let mut present = Vec::new();
            for entry in &entries {
                let program = entry.config().argv().first().cloned().unwrap_or_default();
                if crate::sandbox_exec::resolve_program(root, &program).is_ok() {
                    present.push(entry.config().id().to_owned());
                } else {
                    missing.push(format!("{} ({program})", entry.config().id()));
                }
            }
            if missing.is_empty() {
                DoctorCheck::pass(
                    "scanner",
                    format!(
                        "{} configured and present: {}",
                        present.len(),
                        present.join(", ")
                    ),
                )
            } else {
                DoctorCheck::warn(
                    "scanner",
                    format!("executable not found for: {}", missing.join(", ")),
                    "install the scanners above, or remove them from .rapidlm/scanners.json; \
                     scanner-gated commit/merge will refuse until they resolve",
                )
            }
        }
        Err(err) => DoctorCheck::warn(
            "scanner",
            format!("{err}"),
            "fix or remove .rapidlm/scanners.json; scanner-gated commit/merge cannot run",
        ),
    }
}

fn check_hooks(root: &Path, integrations: &crate::interactive::ProjectIntegrations) -> DoctorCheck {
    let hooks = &integrations.hooks;
    if hooks.is_empty() {
        return DoctorCheck::skipped("hooks", "no hooks configured in project settings");
    }
    // Every stage the config carries — the list used to name six of them,
    // so a `pre_compact`/`post_compact` hook (and every stage added since)
    // was never counted or path-checked here.
    let stages = hooks.named_stages();
    let mut total = 0usize;
    let mut unresolved = Vec::new();
    for (stage, commands) in stages {
        for command in commands {
            total += 1;
            // A hook is never executed by doctor: running arbitrary
            // project-controlled commands as a side effect of a diagnosis
            // would be exactly the surprise this command must not create.
            //
            // `hooks::run_hook` passes the whole entry to `sh -c`, so it is
            // a shell line, not an argv. Only a first token in *path form*
            // (`./scripts/x.sh`, `/usr/local/bin/y`) is checked: a bare name
            // could legitimately be a shell builtin, a function, or a
            // keyword, and flagging those would be a false alarm rather than
            // a finding.
            let program = command.split_whitespace().next().unwrap_or_default();
            if program.contains('/') && crate::sandbox_exec::resolve_program(root, program).is_err()
            {
                unresolved.push(format!("{stage}:{program}"));
            }
        }
    }
    if unresolved.is_empty() {
        DoctorCheck::pass(
            "hooks",
            format!("{total} hook(s) configured; run through `sh -c`, not executed here"),
        )
    } else {
        DoctorCheck::warn(
            "hooks",
            format!(
                "{total} hook(s) configured; path not found for {}",
                unresolved.join(", ")
            ),
            "create the hook scripts above or remove them from the project settings",
        )
    }
}

fn check_mcp(
    integrations: &crate::interactive::ProjectIntegrations,
    trust: Option<&Result<TrustStatus, kernel::ProjectTrustError>>,
) -> DoctorCheck {
    let loaded = &integrations.mcp;
    let servers = loaded.servers();
    let rejections = loaded.rejections();
    if servers.is_empty() && rejections.is_empty() {
        return DoctorCheck::skipped("mcp", "no mcpServers configured in project settings");
    }
    let names: Vec<&str> = servers
        .iter()
        .map(|entry| entry.config.name.as_str())
        .collect();
    let trusted = matches!(trust, Some(Ok(TrustStatus::Trusted)));
    // A rejected entry is a configured server that will never run — the
    // single most common reason a user's MCP server "just does not show up"
    // — so it warns whatever the trust state. Both facts go in one detail:
    // an earlier version returned on the rejection branch alone and dropped
    // the untrusted signal, which made an accepted-but-unregistered server
    // read as "usable".
    if !rejections.is_empty() || !trusted {
        let accepted = if names.is_empty() {
            "none".to_owned()
        } else {
            names.join(", ")
        };
        let mut detail = if trusted {
            format!("{} server(s) registered ({accepted})", servers.len())
        } else {
            format!(
                "{} server(s) configured ({accepted}) but not registered: the project is untrusted",
                servers.len()
            )
        };
        if !rejections.is_empty() {
            detail.push_str(&format!(
                "; {} entry/entries rejected — {}",
                loaded.rejections_total(),
                rejections
                    .iter()
                    .map(|rejection| format!(
                        "{} in {}: {}",
                        rejection.name, rejection.file, rejection.issue
                    ))
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }
        let remediation = match (trusted, rejections.is_empty()) {
            (true, _) => {
                "run `rapid mcp list` for the full report, then fix or remove the rejected entries"
            }
            (false, true) => {
                "run `rapid trust grant` in this project to let configured MCP servers register"
            }
            (false, false) => {
                "run `rapid trust grant` to register the usable servers, and `rapid mcp list` \
for the rejected ones"
            }
        };
        return DoctorCheck::warn("mcp", detail, remediation);
    }

    DoctorCheck::pass(
        "mcp",
        format!(
            "{} server(s) configured ({}); registered on a trusted project, not contacted here",
            servers.len(),
            names.join(", ")
        ),
    )
}

fn check_plugins(root: &Path) -> DoctorCheck {
    let catalog = root
        .join(PROJECT_MARKER)
        .join("plugins")
        .join(plugin_host::TRUST_CATALOG_FILE);
    if !catalog.exists() {
        return DoctorCheck::skipped("plugins", "no plugin trust catalog in this project");
    }
    let store = plugin_host::ExtensionTrustStore::open(&catalog);
    let cancel = capability_broker::CancellationToken::new();
    match store.list(&cancel) {
        Ok(views) if views.is_empty() => {
            DoctorCheck::skipped("plugins", "plugin trust catalog is empty")
        }
        Ok(views) => {
            let enabled = views
                .iter()
                .filter(|view| view.executable_enabled())
                .count();
            DoctorCheck::pass(
                "plugins",
                format!(
                    "{} plugin record(s), {enabled} with executable code enabled",
                    views.len()
                ),
            )
        }
        Err(err) => DoctorCheck::warn(
            "plugins",
            format!("{} is unusable: {err}", catalog.display()),
            "repair or remove the plugin trust catalog above",
        ),
    }
}

// -------------------------------------------------------------------------
// sandbox
// -------------------------------------------------------------------------

fn check_sandbox_availability(report: &Result<security::DoctorReport, String>) -> DoctorCheck {
    let Some(check) = security_check(report, security::DoctorCheckId::SandboxAvailability) else {
        return sandbox_engine_error("sandbox", report);
    };
    let detail = security_detail(&check);
    match check.status() {
        security::DoctorStatus::Pass => DoctorCheck::pass("sandbox", detail),
        // Sandboxed execution is per-call opt-in (`shell_exec`'s `sandbox`
        // argument); a weak or absent backend degrades that one capability
        // and never stops a turn, so it is a warning here even where the
        // security engine's own stricter posture model calls it worse.
        _ => DoctorCheck::warn(
            "sandbox",
            detail,
            check
                .remediation()
                .unwrap_or("no sandbox backend is available for sandboxed shell_exec"),
        ),
    }
}

fn check_sandbox_probe(
    enabled: bool,
    manager: &sandbox::SandboxManager,
    seatbelt: bool,
) -> DoctorCheck {
    if !enabled {
        return DoctorCheck::skipped("sandbox-probe", "live probe disabled for this run");
    }
    let backend = if seatbelt {
        "seatbelt"
    } else {
        "host-restricted"
    };
    // A scratch directory, never the project: the probe mounts its root
    // read-write, so it must not be pointed at anything real.
    let Some(root) = probe_dir() else {
        return DoctorCheck::warn(
            "sandbox-probe",
            "could not create a scratch directory for the probe",
            "ensure the system temp directory is writable",
        );
    };
    let argv = vec!["echo".to_owned(), SANDBOX_PROBE_TOKEN.to_owned()];
    let outcome = crate::sandbox_exec::run_sandboxed_with(
        manager,
        &root,
        &argv,
        SANDBOX_PROBE_TIMEOUT,
        SANDBOX_PROBE_OUTPUT_LIMIT,
    );
    let _ = std::fs::remove_dir_all(&root);
    match outcome {
        Ok(result)
            if result.exit_code == Some(0)
                && String::from_utf8_lossy(&result.output).contains(SANDBOX_PROBE_TOKEN) =>
        {
            DoctorCheck::pass(
                "sandbox-probe",
                format!("{backend} backend executed a scratch command successfully"),
            )
        }
        Ok(result) => DoctorCheck::warn(
            "sandbox-probe",
            format!(
                "{backend} backend ran but did not produce the expected output \
                 (exit={:?} timed_out={} oom={})",
                result.exit_code, result.timed_out, result.oom
            ),
            "sandboxed shell_exec will not behave as expected on this host",
        ),
        Err(err) => DoctorCheck::warn(
            "sandbox-probe",
            format!("{backend} backend could not execute: {err}"),
            "sandboxed shell_exec is unavailable on this host; unsandboxed calls still run",
        ),
    }
}

fn probe_dir() -> Option<PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "rapidlm-doctor-probe-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).ok()?;
    match protocol::host_path::canonicalize(&dir) {
        Ok(canonical) => Some(canonical),
        Err(_) => {
            // The directory was created; do not leave it behind just because
            // it could not be canonicalized.
            let _ = std::fs::remove_dir_all(&dir);
            None
        }
    }
}

// -------------------------------------------------------------------------
// security-posture rows
// -------------------------------------------------------------------------

/// Drive `security::evaluate_doctor` with observations taken from this host
/// and project, rather than the empty default request the previous
/// `rapid doctor` passed.
fn evaluate_security(
    manager: &sandbox::SandboxManager,
    trust: Option<TrustStatus>,
    root: Option<&Path>,
    integrations: Option<&crate::interactive::ProjectIntegrations>,
) -> Result<security::DoctorReport, String> {
    let cancel = capability_broker::CancellationToken::new();
    let mut request = security::DoctorRequest::new().with_sandbox_manager(manager);

    // The probe itself, not the keychain handle, is what goes into the
    // request: `PlatformKeychain::probe` shells out to a local tool, and the
    // Linux backend's `secret-tool lookup --unlock` can block on an unlock
    // prompt. Running it behind a deadline keeps `rapid doctor` bounded on a
    // headless host; a timeout reports "unavailable", which is a warning, not
    // a failure.
    request = request.with_keychain_probe(probe_keychain());

    let trust_state = match trust {
        Some(TrustStatus::Trusted) => security::ProjectTrustState::Trusted,
        Some(TrustStatus::Untrusted) => security::ProjectTrustState::Untrusted,
        None => security::ProjectTrustState::Unknown,
    };
    if trust.is_some() {
        let surfaces = executable_surfaces(root, integrations);
        match security::ProjectConfigObservation::new(trust_state, surfaces) {
            Ok(observation) => request = request.with_project(observation),
            Err(err) => return Err(format!("{err}")),
        }
    }

    // Policy documents and release-signature material: this build ships no
    // user-facing policy file and no locally verifiable signed manifest, so
    // both stay `NotProvided` and are reported as not applicable rather than
    // fabricated. See `check_security_policy`/`check_release_signature`.
    security::evaluate_doctor(&request, &cancel).map_err(|err| format!("{err}"))
}

/// Which project-controlled executable-config surfaces actually exist under
/// `root`, by inspecting the same files production reads. Names only — no
/// file bodies ever reach the security engine.
fn executable_surfaces(
    root: Option<&Path>,
    integrations: Option<&crate::interactive::ProjectIntegrations>,
) -> Vec<security::ExecutableConfigClass> {
    let (Some(root), Some(integrations)) = (root, integrations) else {
        return Vec::new();
    };
    let mut surfaces = Vec::new();
    if !integrations.hooks.is_empty() {
        surfaces.push(security::ExecutableConfigClass::Hooks);
    }
    // Any `mcpServers` entry at all is project-controlled executable
    // configuration, including one this build rejects: the security
    // engine's question is what the settings file *declares*, not what
    // Rapid would run.
    if !integrations.mcp.servers().is_empty() || !integrations.mcp.rejections().is_empty() {
        surfaces.push(security::ExecutableConfigClass::ProjectMcp);
    }
    if root
        .join(PROJECT_MARKER)
        .join("plugins")
        .join(plugin_host::TRUST_CATALOG_FILE)
        .exists()
    {
        surfaces.push(security::ExecutableConfigClass::Plugins);
    }
    surfaces
}

/// [`auth::PlatformKeychain::probe`] behind [`KEYCHAIN_PROBE_TIMEOUT`].
/// A probe that has not answered by the deadline is reported as unavailable
/// and its thread is abandoned (it holds no lock this process needs, and the
/// command exits shortly afterwards).
fn probe_keychain() -> auth::KeychainProbe {
    let unavailable = auth::KeychainProbe::unavailable(auth::platform_keychain_kind());
    let Ok(keychain) = auth::os_keychain_select::production_backend(std::env::consts::OS) else {
        return unavailable;
    };
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(keychain.probe());
    });
    receiver
        .recv_timeout(KEYCHAIN_PROBE_TIMEOUT)
        .unwrap_or(unavailable)
}

fn security_check(
    report: &Result<security::DoctorReport, String>,
    id: security::DoctorCheckId,
) -> Option<security::DoctorCheck> {
    report
        .as_ref()
        .ok()
        .and_then(|report| report.check(id).cloned())
}

fn security_detail(check: &security::DoctorCheck) -> String {
    let mut detail = format!("security status={}", check.status().as_str());
    let metadata: Vec<String> = check
        .metadata()
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    if !metadata.is_empty() {
        detail.push_str(&format!(" ({})", metadata.join(" ")));
    }
    detail
}

fn sandbox_engine_error(
    id: &'static str,
    report: &Result<security::DoctorReport, String>,
) -> DoctorCheck {
    let reason = report
        .as_ref()
        .err()
        .cloned()
        .unwrap_or_else(|| "check missing from the security report".to_owned());
    DoctorCheck::warn(
        id,
        format!("security doctor could not evaluate this check: {reason}"),
        "report this as an internal diagnostic fault",
    )
}

fn check_credential_store(report: &Result<security::DoctorReport, String>) -> DoctorCheck {
    let Some(check) = security_check(report, security::DoctorCheckId::CredentialStore) else {
        return sandbox_engine_error("credential-store", report);
    };
    let detail = security_detail(&check);
    match check.status() {
        security::DoctorStatus::Pass => DoctorCheck::pass("credential-store", detail),
        // Provider credentials come from config/env (`resolve_credential`),
        // never from the platform keychain, so an unavailable keychain does
        // not stop Rapid from running a turn — it only removes durable
        // secure storage for anything that would use it.
        _ => DoctorCheck::warn(
            "credential-store",
            detail,
            check.remediation().unwrap_or(
                "platform keychain is unavailable; model keys still come from config/env",
            ),
        ),
    }
}

fn check_project_config_exposure(report: &Result<security::DoctorReport, String>) -> DoctorCheck {
    let Some(check) = security_check(report, security::DoctorCheckId::DangerousProjectConfig)
    else {
        return sandbox_engine_error("project-config-exposure", report);
    };
    if check.status() == security::DoctorStatus::Unavailable {
        return DoctorCheck::skipped(
            "project-config-exposure",
            "no project trust state to classify against",
        );
    }
    let detail = security_detail(&check);
    match check.status() {
        security::DoctorStatus::Pass => DoctorCheck::pass("project-config-exposure", detail),
        // The security engine grades this from a pure security-posture
        // stance, where "an untrusted project carrying executable config" is
        // a `Fail`. From `rapid doctor`'s stance that is the boundary
        // working as designed and Rapid runs fine, so the app-level status
        // never exceeds a warning — the engine's own verdict is printed
        // verbatim in the detail so nothing is hidden by the softening.
        _ => DoctorCheck::warn(
            "project-config-exposure",
            detail,
            check
                .remediation()
                .unwrap_or("review the project-controlled executable config named above"),
        ),
    }
}

fn check_security_policy(report: &Result<security::DoctorReport, String>) -> DoctorCheck {
    let Some(check) = security_check(report, security::DoctorCheckId::PolicyParse) else {
        return sandbox_engine_error("security-policy", report);
    };
    if check.status() == security::DoctorStatus::Unavailable {
        // Truthful, not a pretend pass: this build has no user-authored
        // capability-policy document to parse. The only policy stacks that
        // exist are fixed in-process constants minted per call site.
        return DoctorCheck::skipped(
            "security-policy",
            "no user-authored capability-policy document exists in this build",
        );
    }
    let detail = security_detail(&check);
    match check.status() {
        security::DoctorStatus::Pass => DoctorCheck::pass("security-policy", detail),
        _ => DoctorCheck::fail(
            "security-policy",
            detail,
            check
                .remediation()
                .unwrap_or("fix the capability-policy document"),
        ),
    }
}

fn check_release_signature(report: &Result<security::DoctorReport, String>) -> DoctorCheck {
    let Some(check) = security_check(report, security::DoctorCheckId::ReleaseSignature) else {
        return sandbox_engine_error("release-signature", report);
    };
    if check.status() == security::DoctorStatus::Unavailable {
        return DoctorCheck::skipped(
            "release-signature",
            "no signed release manifest is installed alongside this binary",
        );
    }
    let detail = security_detail(&check);
    match check.status() {
        security::DoctorStatus::Pass => DoctorCheck::pass("release-signature", detail),
        _ => DoctorCheck::warn(
            "release-signature",
            detail,
            check
                .remediation()
                .unwrap_or("verify the installed release artifact's signature"),
        ),
    }
}

// -------------------------------------------------------------------------
// helpers
// -------------------------------------------------------------------------

/// One rendered line's worth of text: control characters neutralized, then
/// truncated on a character boundary.
///
/// Sanitization is not cosmetic. `check_git` puts an external program's
/// stdout into a detail, and `render` writes details straight into a
/// `STATUS  id  detail` line — so an embedded newline would let that program
/// forge additional report rows (a fabricated `PASS  model  ...` above the
/// summary). Every control character, `\n` and `\r` included, becomes a
/// space so a detail can only ever occupy the one line it was given.
fn bounded(text: &str) -> String {
    let mut clean: String = text
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    if clean.len() <= MAX_DETAIL_BYTES {
        return clean;
    }
    let mut end = MAX_DETAIL_BYTES;
    while end > 0 && !clean.is_char_boundary(end) {
        end -= 1;
    }
    clean.truncate(end);
    clean.push_str("...");
    clean
}

/// Every credential-bearing substring of a `base_url`'s userinfo component
/// (`scheme://user:password@host/...`): the whole userinfo, and the password
/// half on its own, so a report quoting either form is scrubbed. Empty when
/// the URL carries no userinfo, which is the normal case.
///
/// Two length floors keep scrubbing from destroying the report itself, since
/// redaction is exact-substring replacement over the whole rendered text: a
/// bare username with no password (`http://user@host`) is only treated as a
/// token at [`MIN_BARE_USERINFO_TOKEN`] characters or more — a short one is a
/// *name*, and registering `user` would blank that word out of every
/// unrelated row — and a password under [`MIN_USERINFO_PASSWORD`] characters
/// is covered only through the full `user:password` form, which contains a
/// colon and so cannot collide with ordinary prose. That full form is the one
/// that matters anyway: `ModelConfigError::BaseUrl` quotes the entire URL.
fn base_url_userinfo_secrets(base_url: &str) -> Vec<String> {
    let Some(after_scheme) = base_url.split_once("://").map(|(_, rest)| rest) else {
        return Vec::new();
    };
    // Authority ends at the first '/', '?' or '#'; userinfo is what precedes
    // the last '@' inside it.
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    let Some((userinfo, _)) = authority.rsplit_once('@') else {
        return Vec::new();
    };
    if userinfo.is_empty() {
        return Vec::new();
    }
    let Some((_, password)) = userinfo.split_once(':') else {
        // No password component: a bare `user@host`. Only token-shaped.
        return if userinfo.chars().count() >= MIN_BARE_USERINFO_TOKEN {
            vec![userinfo.to_owned()]
        } else {
            Vec::new()
        };
    };
    let mut secrets = vec![userinfo.to_owned()];
    if password.chars().count() >= MIN_USERINFO_PASSWORD {
        secrets.push(password.to_owned());
    }
    secrets
}

/// Prove `dir` is writable with a uniquely named temp file, removed
/// immediately. Never touches an existing file.
fn writable_probe(dir: &Path) -> Result<(), String> {
    let probe = dir.join(format!(
        ".rapidlm-doctor-write-probe-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ));
    std::fs::write(&probe, b"").map_err(|err| format!("{err}"))?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// Final, mandatory pass over the whole report: every rendered string goes
/// through the same `security::SecretRedactionRegistry` the live turn path
/// already uses for captured tool output, seeded with each credential the
/// resolved model configuration actually produced. A provider or config
/// error that quoted a key verbatim therefore still cannot print it.
fn finalize(report: DoctorReport, secrets: &[String]) -> DoctorReport {
    let mut registry = security::SecretRedactionRegistry::new();
    let cancel = security::RedactionCancellation::new();
    for (index, secret) in secrets.iter().take(MAX_REDACTION_CANARIES).enumerate() {
        let Ok(refer) = auth::SecretRef::from_alias(&format!("doctor-model-credential-{index}"))
        else {
            continue;
        };
        // A registration failure is not fatal here: the literal backstop
        // below covers the exact bytes unconditionally. The registry adds
        // the encoded variants (base64 and friends) on top of that.
        let _ = registry.register_canary(&refer, secret.as_bytes(), &cancel);
    }
    let snapshot = registry.snapshot();
    let scrub = |text: &str| -> String {
        let cancel = security::RedactionCancellation::new();
        let scrubbed =
            match snapshot.redact_text(security::TextSink::ProcessStdout, text, &cancel) {
                Ok(output) => output.as_text().map(str::to_owned).unwrap_or_else(|_| {
                    "<redacted: diagnostic text was not valid UTF-8>".to_owned()
                }),
                // A redaction failure must never let raw text through: replace
                // the row's text rather than printing something unscrubbed.
                Err(_) => "<redacted: diagnostic text could not be scrubbed>".to_owned(),
            };
        // Literal backstop, applied to *every* secret including any the
        // registry refused (over its own size bound, or past its registered-
        // secret limit). Without it a single failed registration would leak
        // that one value silently while the rest of the report looked clean.
        let scrubbed = secrets
            .iter()
            .filter(|secret| !secret.is_empty())
            .fold(scrubbed, |acc, secret| {
                acc.replace(secret.as_str(), REDACTED)
            });
        // Bounding and control-character sanitization come last, so a secret
        // can never survive by being split across the truncation point.
        bounded(&scrubbed)
    };
    DoctorReport {
        checks: report
            .checks
            .into_iter()
            .map(|check| DoctorCheck {
                id: check.id,
                status: check.status,
                detail: scrub(&check.detail),
                remediation: check.remediation.as_deref().map(scrub),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(rows: Vec<DoctorCheck>) -> DoctorReport {
        DoctorReport { checks: rows }
    }

    #[test]
    fn exit_code_is_zero_when_nothing_failed_and_one_when_something_did() {
        // The whole contract a CI gate depends on: warnings and skips are
        // not failures, a single failure is.
        assert_eq!(report(Vec::new()).exit_code(), 0);
        assert_eq!(
            report(vec![
                DoctorCheck::pass("a", "ok"),
                DoctorCheck::skipped("b", "n/a"),
                DoctorCheck::warn("c", "degraded", "do something"),
            ])
            .exit_code(),
            0,
            "warnings alone must never fail the command"
        );
        assert_eq!(
            report(vec![
                DoctorCheck::pass("a", "ok"),
                DoctorCheck::fail("b", "broken", "fix it"),
            ])
            .exit_code(),
            1
        );
    }

    #[test]
    fn counts_report_failures_and_warnings_separately() {
        let report = report(vec![
            DoctorCheck::fail("a", "broken", "fix"),
            DoctorCheck::warn("b", "degraded", "fix"),
            DoctorCheck::warn("c", "degraded", "fix"),
            DoctorCheck::skipped("d", "n/a"),
            DoctorCheck::pass("e", "ok"),
        ]);
        assert_eq!(report.failures(), 1);
        assert_eq!(report.warnings(), 2);
        assert_eq!(report.exit_code(), 1);
    }

    #[test]
    fn render_preserves_insertion_order_and_shows_every_status_label() {
        let rendered = report(vec![
            DoctorCheck::pass("zeta", "ok"),
            DoctorCheck::fail("alpha", "broken", "fix alpha"),
            DoctorCheck::warn("mid", "degraded", "fix mid"),
            DoctorCheck::skipped("omega", "n/a"),
        ])
        .render();
        let order: Vec<&str> = rendered
            .lines()
            .filter(|line| {
                ["PASS", "FAIL", "WARN", "SKIP"]
                    .iter()
                    .any(|label| line.starts_with(label))
            })
            .filter_map(|line| line.split_whitespace().nth(1))
            .collect();
        // Insertion order, not sorted and not hash order.
        assert_eq!(&order[..4], &["zeta", "alpha", "mid", "omega"]);
        for label in ["PASS", "FAIL", "WARN", "SKIP"] {
            assert!(rendered.contains(label), "missing {label} in:\n{rendered}");
        }
        assert!(rendered.contains("-> fix alpha"));
        assert!(rendered.contains("4 checks: 1 failed, 1 warnings"));
    }

    #[test]
    fn render_is_byte_identical_across_repeated_calls() {
        let report = report(vec![
            DoctorCheck::pass("a", "ok"),
            DoctorCheck::warn("b", "degraded", "fix"),
        ]);
        assert_eq!(report.render(), report.render());
    }

    #[test]
    fn a_detail_longer_than_the_bound_is_truncated_on_a_char_boundary() {
        // A pathological config value must not turn one row into a dump,
        // and truncation must not split a multi-byte character.
        let long = "é".repeat(MAX_DETAIL_BYTES);
        let report = finalize(report(vec![DoctorCheck::pass("x", long)]), &[]);
        let detail = report.check("x").expect("row").detail().to_owned();
        assert!(detail.len() <= MAX_DETAIL_BYTES + 3);
        assert!(detail.ends_with("..."));
        assert!(std::str::from_utf8(detail.as_bytes()).is_ok());
    }

    #[test]
    fn a_secret_long_enough_to_be_truncated_is_still_scrubbed() {
        // Regression: bounding used to run at row construction, *before*
        // redaction. A secret straddling the cut was split, the registry's
        // full-length needle no longer matched, and the surviving prefix
        // printed in the clear. Redaction must therefore see the untruncated
        // text.
        let secret = format!("LEAKMARKER{}", "A".repeat(MAX_DETAIL_BYTES));
        let scrubbed = finalize(
            report(vec![DoctorCheck::fail(
                "model",
                format!("config base_url 'http://u:{secret}@example.invalid/v1' is invalid"),
                "fix the base_url",
            )]),
            std::slice::from_ref(&secret),
        );
        let rendered = scrubbed.render();
        assert!(
            !rendered.contains("LEAKMARKER"),
            "a truncated secret leaked:\n{rendered}"
        );
    }

    #[test]
    fn a_detail_carrying_newlines_cannot_forge_extra_report_rows() {
        // `check_git` puts an external program's stdout into a detail, and
        // `render` writes details straight into a `STATUS  id  detail` line,
        // so an embedded newline would let that program fabricate rows.
        let forged = "git version 9.9.9
PASS  model  everything is perfectly fine";
        let rendered = finalize(
            report(vec![
                DoctorCheck::fail("model", "broken", "fix it"),
                DoctorCheck::pass("git", forged),
            ]),
            &[],
        )
        .render();
        let row_lines: Vec<&str> = rendered
            .lines()
            .filter(|line| {
                ["PASS", "FAIL", "WARN", "SKIP"]
                    .iter()
                    .any(|label| line.starts_with(label))
            })
            .collect();
        assert_eq!(row_lines.len(), 2, "forged rows appeared:\n{rendered}");
        assert!(
            rendered.contains("everything is perfectly fine"),
            "text is kept, just flattened"
        );
        assert!(!rendered.contains("\nPASS  model"));
    }

    #[test]
    fn redaction_scrubs_a_credential_that_leaked_into_a_detail_or_remediation() {
        // The mandatory guarantee for a diagnostic command: even if a
        // provider or config error quoted the key verbatim, the rendered
        // report cannot contain it.
        let secret = "sk-doctor-unit-test-secret-value";
        let scrubbed = finalize(
            report(vec![
                DoctorCheck::fail(
                    "model",
                    format!("provider rejected key {secret}"),
                    format!("check {secret} in the config"),
                ),
                DoctorCheck::pass("other", "nothing sensitive"),
            ]),
            &[secret.to_owned()],
        );
        let rendered = scrubbed.render();
        assert!(
            !rendered.contains(secret),
            "the credential survived redaction:\n{rendered}"
        );
        assert!(rendered.contains("other"), "unrelated rows must survive");
    }

    #[test]
    fn base_url_userinfo_is_treated_as_a_secret_even_when_the_entry_is_keyless() {
        // A keyless entry resolves no credential at all, so the resolved-key
        // list is empty — but the URL is still printed on the model row and
        // quoted verbatim by `ModelConfigError::BaseUrl`.
        let found = base_url_userinfo_secrets("http://svc:hunter2@example.invalid/v1");
        assert!(found.contains(&"svc:hunter2".to_owned()));
        assert!(found.contains(&"hunter2".to_owned()));
        // A bare short username is a name, not a token: registering it would
        // blank that word out of every unrelated row.
        assert!(base_url_userinfo_secrets("http://user@example.invalid/v1").is_empty());
        assert_eq!(
            base_url_userinfo_secrets("http://ghp1234567890abcdef@example.invalid/v1"),
            vec!["ghp1234567890abcdef".to_owned()],
            "a token-shaped bare userinfo is still a credential"
        );
        // A pathologically short password is covered only through the full
        // `user:password` form, which cannot collide with ordinary prose.
        assert_eq!(
            base_url_userinfo_secrets("http://svc:a@example.invalid/v1"),
            vec!["svc:a".to_owned()]
        );
        // A URL with a path containing '@' must not be mistaken for userinfo.
        assert!(base_url_userinfo_secrets("http://example.invalid/v1/a@b").is_empty());
        assert!(base_url_userinfo_secrets("https://api.example.com/v1").is_empty());
        assert!(base_url_userinfo_secrets("not-a-url").is_empty());
        assert!(base_url_userinfo_secrets("http://@example.invalid/v1").is_empty());

        let scrubbed = finalize(
            report(vec![DoctorCheck::fail(
                "model",
                "config base_url 'http://svc:hunter2@example.invalid/v1' is invalid",
                "fix the base_url",
            )]),
            &found,
        );
        assert!(
            !scrubbed.render().contains("hunter2"),
            "{}",
            scrubbed.render()
        );
    }

    #[test]
    fn redaction_is_a_no_op_when_no_credential_was_resolved() {
        let original = report(vec![DoctorCheck::pass("a", "ok")]);
        assert_eq!(finalize(original.clone(), &[]), original);
    }

    #[test]
    fn writable_probe_leaves_no_file_behind() {
        let dir = std::env::temp_dir().join(format!(
            "rapidlm-doctor-writable-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        let before: Vec<_> = std::fs::read_dir(&dir).expect("read").collect();
        writable_probe(&dir).expect("writable");
        let after: Vec<_> = std::fs::read_dir(&dir).expect("read").collect();
        assert_eq!(before.len(), after.len(), "probe file must be removed");
        assert!(after.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn writable_probe_fails_on_a_directory_that_does_not_exist() {
        let missing = std::env::temp_dir().join("rapidlm-doctor-absent-directory-xyz");
        let _ = std::fs::remove_dir_all(&missing);
        assert!(writable_probe(&missing).is_err());
    }

    #[test]
    fn an_untrusted_project_is_a_warning_with_the_real_grant_command_never_a_failure() {
        // Intentional trust denial is the security boundary working, not a
        // broken installation — and the remediation must name the command
        // that actually exists (`rapid trust grant`), not an invented one.
        let check = check_project_trust(Some(&Ok(TrustStatus::Untrusted)));
        assert_eq!(check.status(), DoctorStatus::Warn);
        assert!(check.remediation().unwrap().contains("rapid trust grant"));
        let tools = check_workspace_tools(Some(&Ok(TrustStatus::Untrusted)));
        assert_eq!(tools.status(), DoctorStatus::Warn);
        assert!(tools.remediation().unwrap().contains("rapid trust grant"));
    }

    #[test]
    fn a_trusted_project_passes_both_trust_rows() {
        assert_eq!(
            check_project_trust(Some(&Ok(TrustStatus::Trusted))).status(),
            DoctorStatus::Pass
        );
        assert_eq!(
            check_workspace_tools(Some(&Ok(TrustStatus::Trusted))).status(),
            DoctorStatus::Pass
        );
    }

    #[test]
    fn an_unreadable_trust_catalog_fails_closed_rather_than_reading_as_untrusted() {
        let check = check_project_trust(Some(&Err(kernel::ProjectTrustError::CatalogCorrupt)));
        assert_eq!(check.status(), DoctorStatus::Fail);
        let store = check_trust_store(Some(&(
            PathBuf::from("/nowhere/project-trust.json"),
            Err(kernel::ProjectTrustError::CatalogCorrupt),
        )));
        assert_eq!(store.status(), DoctorStatus::Fail);
        // Workspace tools cannot be classified without a trust answer, and
        // must not be reported as "enabled" or "broken software".
        assert_eq!(
            check_workspace_tools(Some(&Err(kernel::ProjectTrustError::CatalogCorrupt))).status(),
            DoctorStatus::Skipped
        );
    }

    #[test]
    fn outside_a_project_the_trust_rows_skip_instead_of_failing() {
        assert_eq!(check_project_trust(None).status(), DoctorStatus::Skipped);
        assert_eq!(check_trust_store(None).status(), DoctorStatus::Skipped);
        assert_eq!(check_workspace_tools(None).status(), DoctorStatus::Skipped);
    }

    #[test]
    fn an_absent_default_config_warns_but_an_explicitly_named_missing_one_fails() {
        // A default-path config that isn't there is a supported state Rapid
        // starts in; a `RAPIDLM_CONFIG` pointing at nothing is a real
        // misconfiguration the user asked for explicitly. `load_config`
        // signals the two differently — `Ok(None)` only ever comes from the
        // home fallback — so this drives the states it actually produces,
        // not an unreachable pair.
        let path = PathBuf::from("/nowhere/config.toml");
        assert_eq!(check_config(&path, &Ok(None)).status(), DoctorStatus::Warn);
        let explicit = check_config(
            &path,
            &Err(crate::user_config::UserConfigError::ExplicitConfigMissing {
                path: path.display().to_string(),
            }),
        );
        assert_eq!(explicit.status(), DoctorStatus::Fail);
        assert!(
            explicit.detail().contains("does not exist"),
            "a missing file must not be described as invalid: {}",
            explicit.detail()
        );
        assert!(explicit.remediation().unwrap().contains("RAPIDLM_CONFIG"));
    }

    #[test]
    fn a_malformed_config_fails_and_names_the_real_path() {
        let path = PathBuf::from("/tmp/rapidlm-doctor/config.toml");
        let check = check_config(
            &path,
            &Err(crate::user_config::UserConfigError::NoModelsDefined),
        );
        assert_eq!(check.status(), DoctorStatus::Fail);
        assert!(check.detail().contains("/tmp/rapidlm-doctor/config.toml"));
    }

    #[test]
    fn a_missing_rapidlm_home_is_a_failure_because_trust_and_state_need_one() {
        let check = check_home(None);
        assert_eq!(check.status(), DoctorStatus::Fail);
        assert!(check.remediation().is_some());
    }

    #[test]
    fn context_budget_reports_the_production_pair_and_the_derived_input_budget() {
        // Everything here comes from `context_budget_for`; this asserts the
        // reporting layer does not invent or re-derive a number.
        let backing = crate::model::SelectedModel::Unconfigured(crate::host::UnconfiguredModel);
        let check = check_context_budget(&backing, 0);
        let (limit, reserve) = context_budget_for(&backing);
        assert_eq!(limit, crate::user_config::DEFAULT_CONTEXT_WINDOW);
        assert_eq!(reserve, crate::user_config::DEFAULT_MAX_OUTPUT_TOKENS);
        assert_eq!(check.status(), DoctorStatus::Pass);
        assert!(check.detail().contains(&format!("context_window={limit}")));
        assert!(
            check
                .detail()
                .contains(&format!("output_reserve={reserve}"))
        );
        assert!(
            check
                .detail()
                .contains(&format!("input_budget={}", limit - reserve))
        );
        assert!(
            check
                .detail()
                .contains("source=default (no model configured)")
        );
    }

    #[test]
    fn a_security_row_that_the_engine_calls_fail_never_exceeds_a_warning_but_shows_its_verdict() {
        // `dangerous_project_config` is graded from a pure security-posture
        // stance where "untrusted project carrying executable config" is a
        // Fail. For `rapid doctor` that is the boundary working, so the
        // app status softens to Warn — but the engine's own verdict must
        // still be printed verbatim, not hidden by the softening.
        let observation = security::ProjectConfigObservation::new(
            security::ProjectTrustState::Untrusted,
            [security::ExecutableConfigClass::Hooks],
        )
        .expect("observation");
        let request = security::DoctorRequest::new().with_project(observation);
        let cancel = capability_broker::CancellationToken::new();
        let report = Ok(security::evaluate_doctor(&request, &cancel).expect("evaluate"));
        let engine = report
            .as_ref()
            .expect("report")
            .check(security::DoctorCheckId::DangerousProjectConfig)
            .expect("row");
        assert_eq!(engine.status(), security::DoctorStatus::Fail);
        let check = check_project_config_exposure(&report);
        assert_eq!(check.status(), DoctorStatus::Warn);
        assert!(
            check.detail().contains("security status=fail"),
            "the engine verdict must survive: {}",
            check.detail()
        );
    }

    #[test]
    fn absent_policy_and_release_material_are_skipped_not_failed_or_faked() {
        // The previous `rapid doctor` reported both as `Unavailable` (a
        // non-pass, non-zero exit) purely because it passed an empty
        // request. Neither has anything to observe in this build, and
        // neither blocks Rapid from operating.
        let request = security::DoctorRequest::new();
        let cancel = capability_broker::CancellationToken::new();
        let report = Ok(security::evaluate_doctor(&request, &cancel).expect("evaluate"));
        assert_eq!(
            check_security_policy(&report).status(),
            DoctorStatus::Skipped
        );
        assert_eq!(
            check_release_signature(&report).status(),
            DoctorStatus::Skipped
        );
        assert_eq!(
            check_project_config_exposure(&report).status(),
            DoctorStatus::Skipped
        );
    }

    #[test]
    fn an_unavailable_keychain_is_a_warning_because_model_keys_come_from_config_or_env() {
        let request = security::DoctorRequest::new().with_keychain_probe(
            auth::KeychainProbe::unavailable(auth::PlatformKeychainKind::Unsupported),
        );
        let cancel = capability_broker::CancellationToken::new();
        let report = Ok(security::evaluate_doctor(&request, &cancel).expect("evaluate"));
        let check = check_credential_store(&report);
        assert_eq!(check.status(), DoctorStatus::Warn);
        assert!(check.detail().contains("keychain_available=false"));
    }

    #[test]
    fn an_unavailable_sandbox_backend_warns_rather_than_failing_the_command() {
        // Sandboxed `shell_exec` is per-call opt-in; a host without a
        // backend still runs every turn.
        let request = security::DoctorRequest::new().with_sandbox_probe_failed();
        let cancel = capability_broker::CancellationToken::new();
        let report = Ok(security::evaluate_doctor(&request, &cancel).expect("evaluate"));
        assert_eq!(
            check_sandbox_availability(&report).status(),
            DoctorStatus::Warn
        );
    }

    #[test]
    fn the_sandbox_probe_runs_the_real_backend_and_leaves_no_scratch_directory() {
        // A real execution through the production sandbox abstraction, in a
        // scratch directory — not an "is the binary on disk?" check. Counting
        // the probe directories *this* call is responsible for, rather than
        // every `rapidlm-doctor-probe-*` in the shared temp dir, so a sibling
        // test running concurrently cannot make this flake.
        let seatbelt = crate::exec_tools::find_sandbox_exec().is_some();
        let manager = if seatbelt {
            crate::sandbox_exec::build_manager_seatbelt()
        } else {
            crate::sandbox_exec::build_manager()
        };
        let before = probe_dirs();
        let check = check_sandbox_probe(true, &manager, seatbelt);
        let after = probe_dirs();
        assert!(
            matches!(check.status(), DoctorStatus::Pass | DoctorStatus::Warn),
            "the probe must never fail the command outright: {check:?}"
        );
        let expected = if seatbelt {
            "seatbelt"
        } else {
            "host-restricted"
        };
        assert!(check.detail().contains(expected), "{}", check.detail());
        // Nothing this call created may survive it. (Anything a *concurrent*
        // sibling created is in both sets, so it cancels out.)
        let leaked: Vec<_> = after.difference(&before).collect();
        assert!(
            leaked.is_empty(),
            "probe left scratch dirs behind: {leaked:?}"
        );
    }

    fn probe_dirs() -> std::collections::BTreeSet<std::ffi::OsString> {
        std::fs::read_dir(std::env::temp_dir())
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.file_name())
                    .filter(|name| name.to_string_lossy().starts_with("rapidlm-doctor-probe-"))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn the_probe_can_be_disabled_and_then_skips_rather_than_warning() {
        let manager = crate::sandbox_exec::build_manager();
        assert_eq!(
            check_sandbox_probe(false, &manager, false).status(),
            DoctorStatus::Skipped
        );
    }
}
