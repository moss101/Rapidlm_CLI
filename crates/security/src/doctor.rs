//! Read-only security doctor checks.
//!
//! Reports sandbox availability, policy parse status, credential-store health,
//! dangerous project-controlled config, and release signature state. Unavailable
//! and error never become pass. Secrets, plaintext, and attacker payload bytes
//! are omitted from reports. Threats: `T-009`, `T-012`, `T-015`, `T-001`.

use std::collections::BTreeMap;
use std::fmt::{self, Debug, Display, Formatter};

use auth::{KeychainProbe, PlatformKeychain, PlatformKeychainKind};
use capability_broker::{
    CancellationToken, MAX_POLICY_DOCUMENT_BYTES, PolicyDocument, PolicyParseError, PolicySource,
};
use protocol::SandboxTier;
use sandbox::{BackendDoctorReport, HealthReason, IsolationStrength, SandboxError, SandboxManager};

/// Maximum policy documents accepted by one doctor run.
pub const MAX_POLICY_DOCUMENTS: usize = 16;

/// Maximum sandbox-tier snapshots accepted by one doctor run.
pub const MAX_SANDBOX_TIERS: usize = 8;

/// Maximum executable-config surfaces on one project observation.
pub const MAX_EXECUTABLE_SURFACES: usize = 32;

/// Maximum safe metadata entries on one check.
pub const MAX_METADATA_ENTRIES: usize = 16;

/// Maximum UTF-8 bytes accepted in a metadata key.
pub const MAX_METADATA_KEY_BYTES: usize = 64;

/// Maximum UTF-8 bytes accepted in a metadata value.
pub const MAX_METADATA_VALUE_BYTES: usize = 256;

/// Maximum UTF-8 bytes accepted in a release version or channel label.
pub const MAX_RELEASE_LABEL_BYTES: usize = 64;

/// Stable check identity. Order is the report order.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum DoctorCheckId {
    SandboxAvailability,
    PolicyParse,
    CredentialStore,
    DangerousProjectConfig,
    ReleaseSignature,
}

/// Check outcome. Unavailable and error are never pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub enum DoctorStatus {
    Pass,
    Warn,
    Unavailable,
    Fail,
    Error,
}

/// Project trust used only to classify executable config. Not a grant.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ProjectTrustState {
    Trusted,
    Untrusted,
    Unknown,
}

/// Project-controlled executable surface. Names only; no file bodies.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum ExecutableConfigClass {
    Hooks,
    ProjectMcp,
    Plugins,
    SkillsWithHelpers,
    ShellStartup,
}

/// Typed doctor construction or evaluation failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum DoctorError {
    Cancelled,
    BoundExceeded { limit: usize, requested: usize },
    InvalidLabel,
    PrivilegeUncertainty,
}

/// Bounded key/value bag. Values are labels and counts only.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SafeMetadata {
    entries: BTreeMap<String, String>,
}

/// One check row: identity, status, static remediation, safe metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoctorCheck {
    id: DoctorCheckId,
    status: DoctorStatus,
    remediation: Option<&'static str>,
    metadata: SafeMetadata,
}

/// Aggregate of the five default checks. Never a clean pass when any check is not pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoctorReport {
    checks: Vec<DoctorCheck>,
}

/// Local sandbox availability snapshot. Does not execute a backend.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SandboxTierSnapshot {
    tier: SandboxTier,
    isolation: IsolationStrength,
    available: bool,
    health_reason: Option<HealthReason>,
    version: Option<String>,
    warning: Option<&'static str>,
}

/// How sandbox availability is observed. Manager probe is read-only health.
pub enum SandboxObservation<'a> {
    NotProvided,
    Manager(&'a SandboxManager),
    Tiers(Vec<SandboxTierSnapshot>),
    ProbeFailed,
}

/// TOML policy document plus caller-attested source trust. Debug omits the body.
#[derive(Clone, Eq, PartialEq)]
pub struct PolicyDocumentInput {
    source: PolicySource,
    body: String,
}

/// Credential-store health input. Never carries secret bytes.
pub enum CredentialObservation<'a> {
    NotProvided,
    Probe(KeychainProbe),
    Keychain { keychain: &'a dyn PlatformKeychain },
}

/// Project-controlled executable config observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectConfigObservation {
    trust: ProjectTrustState,
    surfaces: Vec<ExecutableConfigClass>,
    workspace_policy_attempts_grant: bool,
}

/// Bounded release version or channel. Control characters are rejected.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ReleaseLabel(String);

/// Local release-signature observation. No network fetch is performed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReleaseSignatureObservation {
    NotProvided,
    Verified {
        version: ReleaseLabel,
        channel: ReleaseLabel,
    },
    Unsigned {
        version: Option<ReleaseLabel>,
    },
    InvalidSignature,
    Unavailable,
    VerificationError,
}

/// Read-only doctor inputs. Default observations require no provider credentials
/// or network. Repair/write actions are not part of this API.
pub struct DoctorRequest<'a> {
    sandbox: SandboxObservation<'a>,
    policies: Vec<PolicyDocumentInput>,
    credentials: CredentialObservation<'a>,
    project: Option<ProjectConfigObservation>,
    release: ReleaseSignatureObservation,
}

const REMEDIATE_SANDBOX_UNAVAILABLE: &str =
    "install or enable a supported sandbox backend; do not treat a missing backend as clean";
const REMEDIATE_SANDBOX_WEAK: &str = "host-restricted is not a strong malicious-code boundary; prefer container, gvisor, or remote-worker";
const REMEDIATE_SANDBOX_ERROR: &str =
    "sandbox health failed; fail closed until availability is known";
const REMEDIATE_SANDBOX_MISSING: &str =
    "provide sandbox manager or tier snapshots; missing availability is not pass";
const REMEDIATE_POLICY_FAIL: &str =
    "fix policy syntax or remove workspace allow/grant rules; lower-trust layers cannot broaden";
const REMEDIATE_POLICY_MISSING: &str =
    "provide a policy document to evaluate parse status; missing policy is not pass";
const REMEDIATE_POLICY_ERROR: &str =
    "policy parse failed closed; do not treat the document as clean";
const REMEDIATE_CREDENTIAL_UNAVAILABLE: &str = "configure a platform keychain or an explicit encrypted fallback; unavailable store is not pass";
const REMEDIATE_CREDENTIAL_MISSING: &str =
    "provide a keychain probe; missing credential-store health is not pass";
const REMEDIATE_PROJECT_DANGEROUS: &str =
    "project-controlled executable config stays inactive until trust is granted";
const REMEDIATE_PROJECT_TRUST_UNKNOWN: &str =
    "establish project trust before treating executable config as safe";
const REMEDIATE_PROJECT_GRANT: &str =
    "workspace policy cannot grant or allow beyond higher-trust layers";
const REMEDIATE_PROJECT_MISSING: &str =
    "provide a project-config observation; missing trust state is not pass";
const REMEDIATE_PROJECT_ACTIVE: &str =
    "trusted project executable config is active; review hooks, MCP, plugins, and skills";
const REMEDIATE_RELEASE_UNSIGNED: &str =
    "refuse unsigned release artifacts; verify a signed manifest before install";
const REMEDIATE_RELEASE_INVALID: &str =
    "quarantine the artifact; invalid signatures must not install";
const REMEDIATE_RELEASE_UNAVAILABLE: &str =
    "release signature state is unavailable; do not treat the artifact as verified";
const REMEDIATE_RELEASE_ERROR: &str =
    "release signature verification failed; fail closed until state is known";
const REMEDIATE_RELEASE_MISSING: &str =
    "provide a local release-signature observation; missing state is not pass";

impl DoctorCheckId {
    pub const ALL: [Self; 5] = [
        Self::SandboxAvailability,
        Self::PolicyParse,
        Self::CredentialStore,
        Self::DangerousProjectConfig,
        Self::ReleaseSignature,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SandboxAvailability => "sandbox_availability",
            Self::PolicyParse => "policy_parse",
            Self::CredentialStore => "credential_store",
            Self::DangerousProjectConfig => "dangerous_project_config",
            Self::ReleaseSignature => "release_signature",
        }
    }
}

impl DoctorStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Warn => "warn",
            Self::Unavailable => "unavailable",
            Self::Fail => "fail",
            Self::Error => "error",
        }
    }

    pub const fn is_pass(self) -> bool {
        matches!(self, Self::Pass)
    }

    /// Clean/pass only. Warn, unavailable, fail, and error are distinct.
    pub const fn is_clean(self) -> bool {
        self.is_pass()
    }
}

impl ProjectTrustState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trusted => "trusted",
            Self::Untrusted => "untrusted",
            Self::Unknown => "unknown",
        }
    }
}

impl ExecutableConfigClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hooks => "hooks",
            Self::ProjectMcp => "project_mcp",
            Self::Plugins => "plugins",
            Self::SkillsWithHelpers => "skills_with_helpers",
            Self::ShellStartup => "shell_startup",
        }
    }
}

impl DoctorError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Cancelled => "security.doctor_cancelled",
            Self::BoundExceeded { .. } => "security.doctor_bound_exceeded",
            Self::InvalidLabel => "security.doctor_invalid_label",
            Self::PrivilegeUncertainty => "security.doctor_privilege_uncertainty",
        }
    }

    pub fn retryable(&self) -> bool {
        false
    }
}

impl Display for DoctorCheckId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Display for DoctorStatus {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Display for DoctorError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("security doctor was cancelled"),
            Self::BoundExceeded { limit, requested } => {
                write!(
                    f,
                    "security doctor exceeds bound ({requested} > {limit} bytes or items)"
                )
            }
            Self::InvalidLabel => f.write_str("security doctor label is invalid"),
            Self::PrivilegeUncertainty => {
                f.write_str("security doctor failed closed on privilege uncertainty")
            }
        }
    }
}

impl std::error::Error for DoctorError {}

impl SafeMetadata {
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries.get(key).map(String::as_str)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn insert(&mut self, key: &str, value: &str) -> Result<(), DoctorError> {
        if self.entries.len() >= MAX_METADATA_ENTRIES && !self.entries.contains_key(key) {
            return Err(DoctorError::BoundExceeded {
                limit: MAX_METADATA_ENTRIES,
                requested: self.entries.len().saturating_add(1),
            });
        }
        let key = parse_meta_token(key, MAX_METADATA_KEY_BYTES)?;
        let value = parse_meta_token(value, MAX_METADATA_VALUE_BYTES)?;
        self.entries.insert(key, value);
        Ok(())
    }
}

impl DoctorCheck {
    pub fn id(&self) -> DoctorCheckId {
        self.id
    }

    pub fn status(&self) -> DoctorStatus {
        self.status
    }

    pub fn remediation(&self) -> Option<&'static str> {
        self.remediation
    }

    pub fn metadata(&self) -> &SafeMetadata {
        &self.metadata
    }

    pub fn is_pass(&self) -> bool {
        self.status.is_pass()
    }
}

impl DoctorReport {
    pub fn checks(&self) -> &[DoctorCheck] {
        &self.checks
    }

    pub fn check(&self, id: DoctorCheckId) -> Option<&DoctorCheck> {
        self.checks.iter().find(|check| check.id == id)
    }

    /// Most severe status across checks. Missing rows fail closed to error.
    pub fn status(&self) -> DoctorStatus {
        if self.checks.len() != DoctorCheckId::ALL.len() {
            return DoctorStatus::Error;
        }
        self.checks
            .iter()
            .map(|check| check.status)
            .max()
            .unwrap_or(DoctorStatus::Error)
    }

    pub fn is_pass(&self) -> bool {
        self.checks.len() == DoctorCheckId::ALL.len()
            && self.checks.iter().all(DoctorCheck::is_pass)
    }
}

impl Display for DoctorReport {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.status().as_str())?;
        for check in &self.checks {
            write!(f, " {}={}", check.id.as_str(), check.status.as_str())?;
        }
        Ok(())
    }
}

impl SandboxTierSnapshot {
    pub fn available(tier: SandboxTier) -> Result<Self, DoctorError> {
        let isolation =
            IsolationStrength::of_tier(tier).ok_or(DoctorError::PrivilegeUncertainty)?;
        Ok(Self {
            tier,
            isolation,
            available: true,
            health_reason: None,
            version: None,
            warning: isolation.doctor_warning(),
        })
    }

    pub fn unavailable(tier: SandboxTier, reason: HealthReason) -> Result<Self, DoctorError> {
        let isolation =
            IsolationStrength::of_tier(tier).ok_or(DoctorError::PrivilegeUncertainty)?;
        Ok(Self {
            tier,
            isolation,
            available: false,
            health_reason: Some(reason),
            version: None,
            warning: isolation.doctor_warning(),
        })
    }

    pub fn from_report(report: &BackendDoctorReport) -> Self {
        Self {
            tier: report.tier(),
            isolation: report.isolation(),
            available: report.health().is_available(),
            health_reason: report.health().reason(),
            version: report.health().version().map(str::to_owned),
            warning: report.warning(),
        }
    }

    pub const fn tier(&self) -> SandboxTier {
        self.tier
    }

    pub const fn isolation(&self) -> IsolationStrength {
        self.isolation
    }

    pub const fn is_available(&self) -> bool {
        self.available
    }

    pub const fn health_reason(&self) -> Option<HealthReason> {
        self.health_reason
    }

    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    pub const fn warning(&self) -> Option<&'static str> {
        self.warning
    }
}

impl PolicyDocumentInput {
    pub fn new(source: PolicySource, body: impl Into<String>) -> Result<Self, DoctorError> {
        let body = body.into();
        if body.len() > MAX_POLICY_DOCUMENT_BYTES {
            return Err(DoctorError::BoundExceeded {
                limit: MAX_POLICY_DOCUMENT_BYTES,
                requested: body.len(),
            });
        }
        Ok(Self { source, body })
    }

    pub fn source(&self) -> &PolicySource {
        &self.source
    }

    pub fn len(&self) -> usize {
        self.body.len()
    }

    pub fn is_empty(&self) -> bool {
        self.body.is_empty()
    }
}

impl Debug for PolicyDocumentInput {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("PolicyDocumentInput")
            .field("layer", &self.source.layer())
            .field("bytes", &self.body.len())
            .finish()
    }
}

impl ProjectConfigObservation {
    pub fn new(
        trust: ProjectTrustState,
        surfaces: impl IntoIterator<Item = ExecutableConfigClass>,
    ) -> Result<Self, DoctorError> {
        let mut seen = Vec::new();
        for surface in surfaces {
            if !seen.contains(&surface) {
                seen.push(surface);
            }
            if seen.len() > MAX_EXECUTABLE_SURFACES {
                return Err(DoctorError::BoundExceeded {
                    limit: MAX_EXECUTABLE_SURFACES,
                    requested: seen.len(),
                });
            }
        }
        seen.sort_unstable();
        Ok(Self {
            trust,
            surfaces: seen,
            workspace_policy_attempts_grant: false,
        })
    }

    pub fn with_workspace_policy_grant_attempt(mut self, attempted: bool) -> Self {
        self.workspace_policy_attempts_grant = attempted;
        self
    }

    pub const fn trust(&self) -> ProjectTrustState {
        self.trust
    }

    pub fn surfaces(&self) -> &[ExecutableConfigClass] {
        &self.surfaces
    }

    pub const fn workspace_policy_attempts_grant(&self) -> bool {
        self.workspace_policy_attempts_grant
    }
}

impl ReleaseLabel {
    pub fn parse(raw: &str) -> Result<Self, DoctorError> {
        if raw.is_empty() || raw.len() > MAX_RELEASE_LABEL_BYTES {
            return Err(DoctorError::InvalidLabel);
        }
        if raw.contains('\0') || raw.chars().any(char::is_control) {
            return Err(DoctorError::InvalidLabel);
        }
        Ok(Self(raw.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'a> DoctorRequest<'a> {
    pub fn new() -> Self {
        Self {
            sandbox: SandboxObservation::NotProvided,
            policies: Vec::new(),
            credentials: CredentialObservation::NotProvided,
            project: None,
            release: ReleaseSignatureObservation::NotProvided,
        }
    }

    pub fn with_sandbox_manager(mut self, manager: &'a SandboxManager) -> Self {
        self.sandbox = SandboxObservation::Manager(manager);
        self
    }

    pub fn with_sandbox_tiers(
        mut self,
        tiers: Vec<SandboxTierSnapshot>,
    ) -> Result<Self, DoctorError> {
        if tiers.len() > MAX_SANDBOX_TIERS {
            return Err(DoctorError::BoundExceeded {
                limit: MAX_SANDBOX_TIERS,
                requested: tiers.len(),
            });
        }
        self.sandbox = SandboxObservation::Tiers(tiers);
        Ok(self)
    }

    pub fn with_sandbox_probe_failed(mut self) -> Self {
        self.sandbox = SandboxObservation::ProbeFailed;
        self
    }

    pub fn with_policy(mut self, document: PolicyDocumentInput) -> Result<Self, DoctorError> {
        let requested = self.policies.len().saturating_add(1);
        if requested > MAX_POLICY_DOCUMENTS {
            return Err(DoctorError::BoundExceeded {
                limit: MAX_POLICY_DOCUMENTS,
                requested,
            });
        }
        self.policies.push(document);
        Ok(self)
    }

    pub fn with_keychain(mut self, keychain: &'a dyn PlatformKeychain) -> Self {
        self.credentials = CredentialObservation::Keychain { keychain };
        self
    }

    pub fn with_keychain_probe(mut self, probe: KeychainProbe) -> Self {
        self.credentials = CredentialObservation::Probe(probe);
        self
    }

    pub fn with_project(mut self, project: ProjectConfigObservation) -> Self {
        self.project = Some(project);
        self
    }

    pub fn with_release(mut self, release: ReleaseSignatureObservation) -> Self {
        self.release = release;
        self
    }

    pub fn sandbox(&self) -> &SandboxObservation<'a> {
        &self.sandbox
    }

    pub fn policies(&self) -> &[PolicyDocumentInput] {
        &self.policies
    }

    pub fn project(&self) -> Option<&ProjectConfigObservation> {
        self.project.as_ref()
    }

    pub fn release(&self) -> &ReleaseSignatureObservation {
        &self.release
    }
}

impl Default for DoctorRequest<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl Debug for DoctorRequest<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("DoctorRequest")
            .field("sandbox", &sandbox_debug(&self.sandbox))
            .field("policy_documents", &self.policies.len())
            .field("credentials", &credential_debug(&self.credentials))
            .field("project", &self.project)
            .field("release", &self.release)
            .finish()
    }
}

/// Evaluate the five default security doctor checks. Read-only: no store
/// mutation, no sandbox prepare/exec, no provider credential or network use.
pub fn evaluate_doctor(
    request: &DoctorRequest<'_>,
    cancel: &CancellationToken,
) -> Result<DoctorReport, DoctorError> {
    check_cancel(cancel)?;
    let sandbox = check_sandbox(request, cancel)?;
    check_cancel(cancel)?;
    let policy = check_policy(request, cancel)?;
    check_cancel(cancel)?;
    let credentials = check_credentials(request, cancel)?;
    check_cancel(cancel)?;
    let project = check_project(request, cancel)?;
    check_cancel(cancel)?;
    let release = check_release(request, cancel)?;
    Ok(DoctorReport {
        checks: vec![sandbox, policy, credentials, project, release],
    })
}

fn check_sandbox(
    request: &DoctorRequest<'_>,
    cancel: &CancellationToken,
) -> Result<DoctorCheck, DoctorError> {
    check_cancel(cancel)?;
    match &request.sandbox {
        SandboxObservation::NotProvided => Ok(check(
            DoctorCheckId::SandboxAvailability,
            DoctorStatus::Unavailable,
            Some(REMEDIATE_SANDBOX_MISSING),
            SafeMetadata::new(),
        )),
        SandboxObservation::ProbeFailed => {
            let mut metadata = SafeMetadata::new();
            metadata.insert("probe", "failed")?;
            Ok(check(
                DoctorCheckId::SandboxAvailability,
                DoctorStatus::Error,
                Some(REMEDIATE_SANDBOX_ERROR),
                metadata,
            ))
        }
        SandboxObservation::Tiers(tiers) => sandbox_from_tiers(tiers),
        SandboxObservation::Manager(manager) => match manager.doctor(cancel) {
            Ok(reports) => {
                let tiers: Vec<SandboxTierSnapshot> = reports
                    .iter()
                    .map(SandboxTierSnapshot::from_report)
                    .collect();
                sandbox_from_tiers(&tiers)
            }
            Err(SandboxError::Cancelled) => Err(DoctorError::Cancelled),
            Err(_) => {
                let mut metadata = SafeMetadata::new();
                metadata.insert("probe", "health_failed")?;
                Ok(check(
                    DoctorCheckId::SandboxAvailability,
                    DoctorStatus::Error,
                    Some(REMEDIATE_SANDBOX_ERROR),
                    metadata,
                ))
            }
        },
    }
}

fn sandbox_from_tiers(tiers: &[SandboxTierSnapshot]) -> Result<DoctorCheck, DoctorError> {
    if tiers.len() > MAX_SANDBOX_TIERS {
        return Err(DoctorError::BoundExceeded {
            limit: MAX_SANDBOX_TIERS,
            requested: tiers.len(),
        });
    }
    let mut metadata = SafeMetadata::new();
    metadata.insert("tier_count", &tiers.len().to_string())?;
    if tiers.is_empty() {
        return Ok(check(
            DoctorCheckId::SandboxAvailability,
            DoctorStatus::Unavailable,
            Some(REMEDIATE_SANDBOX_UNAVAILABLE),
            metadata,
        ));
    }

    let mut available = 0usize;
    let mut strong = 0usize;
    let mut available_ids = Vec::new();
    let mut unavailable_ids = Vec::new();
    let mut weak_warning = None;
    for tier in tiers {
        if IsolationStrength::of_tier(tier.tier).is_none() {
            metadata.insert("probe", "unknown_tier")?;
            return Ok(check(
                DoctorCheckId::SandboxAvailability,
                DoctorStatus::Error,
                Some(REMEDIATE_SANDBOX_ERROR),
                metadata,
            ));
        }
        if let Some(warning) = tier.warning {
            weak_warning = Some(warning);
        }
        if tier.available {
            available += 1;
            available_ids.push(tier.tier.as_str());
            if tier.isolation.is_strong_isolation() {
                strong += 1;
            }
        } else {
            unavailable_ids.push(tier.tier.as_str());
            if let Some(reason) = tier.health_reason {
                metadata.insert("unavailable_reason", health_reason_label(reason))?;
            }
        }
    }
    if !available_ids.is_empty() {
        metadata.insert("available_tiers", &available_ids.join(","))?;
    }
    if !unavailable_ids.is_empty() {
        metadata.insert("unavailable_tiers", &unavailable_ids.join(","))?;
    }
    if let Some(warning) = weak_warning {
        metadata.insert("isolation_warning", warning)?;
    }
    metadata.insert("available_count", &available.to_string())?;
    metadata.insert("strong_available_count", &strong.to_string())?;

    let (status, remediation) = if available == 0 {
        (
            DoctorStatus::Unavailable,
            Some(REMEDIATE_SANDBOX_UNAVAILABLE),
        )
    } else if strong == 0 {
        (DoctorStatus::Warn, Some(REMEDIATE_SANDBOX_WEAK))
    } else {
        (DoctorStatus::Pass, None)
    };
    Ok(check(
        DoctorCheckId::SandboxAvailability,
        status,
        remediation,
        metadata,
    ))
}

fn check_policy(
    request: &DoctorRequest<'_>,
    cancel: &CancellationToken,
) -> Result<DoctorCheck, DoctorError> {
    check_cancel(cancel)?;
    if request.policies.is_empty() {
        return Ok(check(
            DoctorCheckId::PolicyParse,
            DoctorStatus::Unavailable,
            Some(REMEDIATE_POLICY_MISSING),
            SafeMetadata::new(),
        ));
    }

    let mut metadata = SafeMetadata::new();
    metadata.insert("document_count", &request.policies.len().to_string())?;
    let mut total_rules = 0usize;
    let mut worst = DoctorStatus::Pass;
    for (index, document) in request.policies.iter().enumerate() {
        check_cancel(cancel)?;
        let prefix = format!("doc_{index}");
        metadata.insert(&format!("{prefix}_layer"), document.source.layer().as_str())?;
        match PolicyDocument::parse_toml(&document.body, document.source.clone(), cancel) {
            Ok(parsed) => {
                total_rules = total_rules.saturating_add(parsed.rules().len());
                metadata.insert(
                    &format!("{prefix}_rule_count"),
                    &parsed.rules().len().to_string(),
                )?;
                metadata.insert(&format!("{prefix}_status"), "parsed")?;
            }
            Err(PolicyParseError::Cancelled) => return Err(DoctorError::Cancelled),
            Err(err) => {
                metadata.insert(&format!("{prefix}_status"), "error")?;
                metadata.insert(&format!("{prefix}_error"), policy_error_code(&err))?;
                worst = worst.max(status_for_policy_error(&err));
            }
        }
    }
    metadata.insert("rule_count", &total_rules.to_string())?;
    let remediation = match worst {
        DoctorStatus::Pass => None,
        DoctorStatus::Error => Some(REMEDIATE_POLICY_ERROR),
        _ => Some(REMEDIATE_POLICY_FAIL),
    };
    Ok(check(
        DoctorCheckId::PolicyParse,
        worst,
        remediation,
        metadata,
    ))
}

fn check_credentials(
    request: &DoctorRequest<'_>,
    cancel: &CancellationToken,
) -> Result<DoctorCheck, DoctorError> {
    check_cancel(cancel)?;
    let probe = match &request.credentials {
        CredentialObservation::NotProvided => {
            return Ok(check(
                DoctorCheckId::CredentialStore,
                DoctorStatus::Unavailable,
                Some(REMEDIATE_CREDENTIAL_MISSING),
                SafeMetadata::new(),
            ));
        }
        CredentialObservation::Probe(probe) => *probe,
        CredentialObservation::Keychain { keychain } => {
            check_cancel(cancel)?;
            keychain.probe()
        }
    };
    let mut metadata = SafeMetadata::new();
    metadata.insert("keychain_kind", keychain_kind_label(probe.kind()))?;
    metadata.insert(
        "keychain_available",
        if probe.is_available() {
            "true"
        } else {
            "false"
        },
    )?;
    let (status, remediation) = if probe.is_available() {
        (DoctorStatus::Pass, None)
    } else {
        (
            DoctorStatus::Unavailable,
            Some(REMEDIATE_CREDENTIAL_UNAVAILABLE),
        )
    };
    Ok(check(
        DoctorCheckId::CredentialStore,
        status,
        remediation,
        metadata,
    ))
}

fn check_project(
    request: &DoctorRequest<'_>,
    cancel: &CancellationToken,
) -> Result<DoctorCheck, DoctorError> {
    check_cancel(cancel)?;
    let Some(project) = request.project.as_ref() else {
        return Ok(check(
            DoctorCheckId::DangerousProjectConfig,
            DoctorStatus::Unavailable,
            Some(REMEDIATE_PROJECT_MISSING),
            SafeMetadata::new(),
        ));
    };
    if project.surfaces.len() > MAX_EXECUTABLE_SURFACES {
        return Err(DoctorError::BoundExceeded {
            limit: MAX_EXECUTABLE_SURFACES,
            requested: project.surfaces.len(),
        });
    }

    let mut metadata = SafeMetadata::new();
    metadata.insert("trust", project.trust.as_str())?;
    metadata.insert("surface_count", &project.surfaces.len().to_string())?;
    if !project.surfaces.is_empty() {
        let names: Vec<&str> = project.surfaces.iter().map(|s| s.as_str()).collect();
        metadata.insert("surfaces", &names.join(","))?;
    }
    metadata.insert(
        "workspace_policy_grant_attempt",
        if project.workspace_policy_attempts_grant {
            "true"
        } else {
            "false"
        },
    )?;

    if project.trust == ProjectTrustState::Unknown {
        return Ok(check(
            DoctorCheckId::DangerousProjectConfig,
            DoctorStatus::Error,
            Some(REMEDIATE_PROJECT_TRUST_UNKNOWN),
            metadata,
        ));
    }
    if project.workspace_policy_attempts_grant {
        return Ok(check(
            DoctorCheckId::DangerousProjectConfig,
            DoctorStatus::Fail,
            Some(REMEDIATE_PROJECT_GRANT),
            metadata,
        ));
    }
    if !project.surfaces.is_empty() {
        let (status, remediation) = match project.trust {
            ProjectTrustState::Untrusted => (DoctorStatus::Fail, Some(REMEDIATE_PROJECT_DANGEROUS)),
            ProjectTrustState::Trusted => (DoctorStatus::Warn, Some(REMEDIATE_PROJECT_ACTIVE)),
            ProjectTrustState::Unknown => {
                (DoctorStatus::Error, Some(REMEDIATE_PROJECT_TRUST_UNKNOWN))
            }
        };
        return Ok(check(
            DoctorCheckId::DangerousProjectConfig,
            status,
            remediation,
            metadata,
        ));
    }
    Ok(check(
        DoctorCheckId::DangerousProjectConfig,
        DoctorStatus::Pass,
        None,
        metadata,
    ))
}

fn check_release(
    request: &DoctorRequest<'_>,
    cancel: &CancellationToken,
) -> Result<DoctorCheck, DoctorError> {
    check_cancel(cancel)?;
    let mut metadata = SafeMetadata::new();
    let (status, remediation, state) = match &request.release {
        ReleaseSignatureObservation::NotProvided => (
            DoctorStatus::Unavailable,
            Some(REMEDIATE_RELEASE_MISSING),
            "not_provided",
        ),
        ReleaseSignatureObservation::Verified { version, channel } => {
            metadata.insert("version", version.as_str())?;
            metadata.insert("channel", channel.as_str())?;
            (DoctorStatus::Pass, None, "verified")
        }
        ReleaseSignatureObservation::Unsigned { version } => {
            if let Some(version) = version {
                metadata.insert("version", version.as_str())?;
            }
            (
                DoctorStatus::Fail,
                Some(REMEDIATE_RELEASE_UNSIGNED),
                "unsigned",
            )
        }
        ReleaseSignatureObservation::InvalidSignature => (
            DoctorStatus::Fail,
            Some(REMEDIATE_RELEASE_INVALID),
            "invalid",
        ),
        ReleaseSignatureObservation::Unavailable => (
            DoctorStatus::Unavailable,
            Some(REMEDIATE_RELEASE_UNAVAILABLE),
            "unavailable",
        ),
        ReleaseSignatureObservation::VerificationError => {
            (DoctorStatus::Error, Some(REMEDIATE_RELEASE_ERROR), "error")
        }
    };
    metadata.insert("signature_state", state)?;
    Ok(check(
        DoctorCheckId::ReleaseSignature,
        status,
        remediation,
        metadata,
    ))
}

fn check(
    id: DoctorCheckId,
    status: DoctorStatus,
    remediation: Option<&'static str>,
    metadata: SafeMetadata,
) -> DoctorCheck {
    DoctorCheck {
        id,
        status,
        remediation,
        metadata,
    }
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), DoctorError> {
    if cancel.is_cancelled() {
        Err(DoctorError::Cancelled)
    } else {
        Ok(())
    }
}

fn parse_meta_token(raw: &str, limit: usize) -> Result<String, DoctorError> {
    if raw.is_empty() {
        return Err(DoctorError::InvalidLabel);
    }
    if raw.len() > limit {
        return Err(DoctorError::BoundExceeded {
            limit,
            requested: raw.len(),
        });
    }
    if raw.contains('\0') || raw.chars().any(char::is_control) {
        return Err(DoctorError::InvalidLabel);
    }
    Ok(raw.to_owned())
}

fn health_reason_label(reason: HealthReason) -> &'static str {
    match reason {
        HealthReason::RuntimeMissing => "runtime_missing",
        HealthReason::PlatformUnsupported => "platform_unsupported",
        HealthReason::FeatureMissing => "feature_missing",
    }
}

fn keychain_kind_label(kind: PlatformKeychainKind) -> &'static str {
    match kind {
        PlatformKeychainKind::MacosKeychain => "macos_keychain",
        PlatformKeychainKind::WindowsCredentialManager => "windows_credential_manager",
        PlatformKeychainKind::FreedesktopSecretService => "freedesktop_secret_service",
        PlatformKeychainKind::File => "file_backed",
        PlatformKeychainKind::Unsupported => "unsupported",
    }
}

fn policy_error_code(err: &PolicyParseError) -> &'static str {
    match err {
        PolicyParseError::Cancelled => "cancelled",
        PolicyParseError::SourceTooLarge => "source_too_large",
        PolicyParseError::InvalidSyntax => "invalid_syntax",
        PolicyParseError::UnsupportedSchema { .. } => "unsupported_schema",
        PolicyParseError::MissingField { .. } => "missing_field",
        PolicyParseError::UnknownPrivilegedField { .. } => "unknown_privileged_field",
        PolicyParseError::InvalidValue { .. } => "invalid_value",
        PolicyParseError::DuplicateRuleId => "duplicate_rule_id",
        PolicyParseError::TooManyRules => "too_many_rules",
        PolicyParseError::TooManySubjects => "too_many_subjects",
        PolicyParseError::Broadening { .. } => "broadening",
        PolicyParseError::Capability(_) => "capability",
    }
}

fn status_for_policy_error(err: &PolicyParseError) -> DoctorStatus {
    match err {
        PolicyParseError::Cancelled => DoctorStatus::Error,
        PolicyParseError::SourceTooLarge
        | PolicyParseError::TooManyRules
        | PolicyParseError::TooManySubjects => DoctorStatus::Error,
        _ => DoctorStatus::Fail,
    }
}

fn sandbox_debug(observation: &SandboxObservation<'_>) -> &'static str {
    match observation {
        SandboxObservation::NotProvided => "not_provided",
        SandboxObservation::Manager(_) => "manager",
        SandboxObservation::Tiers(_) => "tiers",
        SandboxObservation::ProbeFailed => "probe_failed",
    }
}

fn credential_debug(observation: &CredentialObservation<'_>) -> &'static str {
    match observation {
        CredentialObservation::NotProvided => "not_provided",
        CredentialObservation::Probe(_) => "probe",
        CredentialObservation::Keychain { .. } => "keychain",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use std::sync::Arc;

    use auth::{KeychainItemMeta, StoreError, UnimplementedPlatformKeychain};
    use capability_broker::CapabilityLease;
    use sandbox::{
        BackendHealth, HostRestrictedBackend, MountCapability, NetworkCapability,
        ResourceCapability, SandboxBackend, SandboxCapabilities, SandboxExecRequest,
        SandboxExecResult, SandboxHandle, SandboxSpec,
    };

    const CANARY: &str = "super-secret-password-doctor-canary";

    struct CountingBackend {
        caps: SandboxCapabilities,
        health: Result<BackendHealth, SandboxError>,
        prepare: Arc<AtomicUsize>,
        exec: Arc<AtomicUsize>,
        destroy: Arc<AtomicUsize>,
    }

    struct CountingKeychain {
        probe: KeychainProbe,
        gets: AtomicUsize,
        puts: AtomicUsize,
        deletes: AtomicUsize,
    }

    impl CountingBackend {
        fn host_available() -> Self {
            Self::new(
                SandboxTier::HostRestricted,
                NetworkCapability::allowlist(),
                BackendHealth::available(Some("host-1")),
            )
        }

        fn container_available() -> Self {
            Self::new(
                SandboxTier::Container,
                NetworkCapability::allowlist(),
                BackendHealth::available(Some("container-1")),
            )
        }

        fn health_error(tier: SandboxTier) -> Self {
            Self::new(
                tier,
                NetworkCapability::none_only(),
                Err(SandboxError::HealthFailed),
            )
        }

        fn new(
            tier: SandboxTier,
            network: NetworkCapability,
            health: Result<BackendHealth, SandboxError>,
        ) -> Self {
            Self {
                caps: SandboxCapabilities::new(
                    tier,
                    network,
                    MountCapability::workspace_temp(),
                    ResourceCapability::bounded(),
                )
                .expect("caps"),
                health,
                prepare: Arc::new(AtomicUsize::new(0)),
                exec: Arc::new(AtomicUsize::new(0)),
                destroy: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl SandboxBackend for CountingBackend {
        fn capabilities(&self) -> SandboxCapabilities {
            self.caps
        }

        fn health(
            &self,
            cancel: &CancellationToken,
        ) -> Result<sandbox::BackendHealth, SandboxError> {
            if cancel.is_cancelled() {
                return Err(SandboxError::Cancelled);
            }
            self.health.clone()
        }

        fn prepare(
            &self,
            _spec: &SandboxSpec,
            _lease: &CapabilityLease,
            _cancel: &CancellationToken,
        ) -> Result<SandboxHandle, SandboxError> {
            self.prepare.fetch_add(1, Ordering::SeqCst);
            Err(SandboxError::InvalidSpec)
        }

        fn exec(
            &self,
            _handle: &SandboxHandle,
            _request: &SandboxExecRequest,
            _lease: &CapabilityLease,
            _cancel: &CancellationToken,
        ) -> Result<SandboxExecResult, SandboxError> {
            self.exec.fetch_add(1, Ordering::SeqCst);
            Err(SandboxError::InvalidSpec)
        }

        fn destroy(
            &self,
            _handle: &SandboxHandle,
            _cancel: &CancellationToken,
        ) -> Result<(), SandboxError> {
            self.destroy.fetch_add(1, Ordering::SeqCst);
            Err(SandboxError::UnknownHandle)
        }
    }

    impl PlatformKeychain for CountingKeychain {
        fn probe(&self) -> KeychainProbe {
            self.probe
        }

        fn put(
            &self,
            _item: &KeychainItemMeta,
            _secret: &[u8],
            cancel: &auth::CancellationToken,
        ) -> Result<(), StoreError> {
            self.puts.fetch_add(1, Ordering::SeqCst);
            cancel.check()?;
            Ok(())
        }

        fn get(
            &self,
            _item: &KeychainItemMeta,
            cancel: &auth::CancellationToken,
        ) -> Result<Vec<u8>, StoreError> {
            self.gets.fetch_add(1, Ordering::SeqCst);
            cancel.check()?;
            Ok(CANARY.as_bytes().to_vec())
        }

        fn delete(
            &self,
            _item: &KeychainItemMeta,
            cancel: &auth::CancellationToken,
        ) -> Result<(), StoreError> {
            self.deletes.fetch_add(1, Ordering::SeqCst);
            cancel.check()?;
            Ok(())
        }

        fn list(
            &self,
            cancel: &auth::CancellationToken,
        ) -> Result<Vec<KeychainItemMeta>, StoreError> {
            cancel.check()?;
            Ok(Vec::new())
        }
    }

    fn user_source() -> PolicySource {
        PolicySource::user("user-policy.toml").expect("user origin")
    }

    fn workspace_source() -> PolicySource {
        PolicySource::trusted_project(".rapidlm/policy.toml").expect("workspace origin")
    }

    fn allow_doc() -> &'static str {
        r#"
schema = 1

[[rules]]
id = "repo-read"
effect = "allow"
subjects = ["*"]
capability = "fs.read"
resource = { root = "repo", glob = "src/**" }
"#
    }

    fn deny_doc() -> &'static str {
        r#"
schema = 1

[[rules]]
id = "net-deny"
effect = "deny"
capability = "net.connect"
"#
    }

    fn healthy_request<'a>(
        keychain: &'a dyn PlatformKeychain,
        sandbox: SandboxObservation<'a>,
    ) -> DoctorRequest<'a> {
        let mut request = DoctorRequest::new()
            .with_keychain(keychain)
            .with_project(
                ProjectConfigObservation::new(ProjectTrustState::Untrusted, []).expect("project"),
            )
            .with_release(ReleaseSignatureObservation::Verified {
                version: ReleaseLabel::parse("1.0.0").expect("version"),
                channel: ReleaseLabel::parse("stable").expect("channel"),
            });
        request.sandbox = sandbox;
        request
            .with_policy(PolicyDocumentInput::new(user_source(), allow_doc()).expect("policy"))
            .expect("add policy")
    }

    fn status_of(report: &DoctorReport, id: DoctorCheckId) -> DoctorStatus {
        report.check(id).expect("check").status()
    }

    #[test]
    fn empty_request_is_unavailable_not_pass() {
        let report =
            evaluate_doctor(&DoctorRequest::new(), &CancellationToken::new()).expect("run");
        assert!(!report.is_pass());
        assert_eq!(report.status(), DoctorStatus::Unavailable);
        for id in DoctorCheckId::ALL {
            assert_eq!(status_of(&report, id), DoctorStatus::Unavailable);
            assert!(!report.check(id).expect("row").is_pass());
        }
    }

    #[test]
    fn host_restricted_only_warns_and_does_not_prepare() {
        let backend = CountingBackend::host_available();
        let mut manager = SandboxManager::new();
        manager.register(Box::new(backend)).expect("register");
        let keychain = UnimplementedPlatformKeychain::new();
        let request = healthy_request(&keychain, SandboxObservation::Manager(&manager));
        let report = evaluate_doctor(&request, &CancellationToken::new()).expect("run");
        assert_eq!(
            status_of(&report, DoctorCheckId::SandboxAvailability),
            DoctorStatus::Warn
        );
        let sandbox = report
            .check(DoctorCheckId::SandboxAvailability)
            .expect("sandbox");
        assert!(
            sandbox
                .remediation()
                .expect("remediation")
                .contains("not a strong")
        );
        assert_eq!(sandbox.metadata().get("strong_available_count"), Some("0"));
        assert!(!report.is_pass());
    }

    #[test]
    fn strong_sandbox_and_healthy_inputs_pass() {
        let keychain = CountingKeychain {
            probe: KeychainProbe::available(PlatformKeychainKind::MacosKeychain),
            gets: AtomicUsize::new(0),
            puts: AtomicUsize::new(0),
            deletes: AtomicUsize::new(0),
        };
        let request = healthy_request(&keychain, SandboxObservation::NotProvided)
            .with_sandbox_tiers(vec![
                SandboxTierSnapshot::available(SandboxTier::Container).expect("container"),
            ])
            .expect("tiers")
            .with_keychain(&keychain);
        let report = evaluate_doctor(&request, &CancellationToken::new()).expect("run");
        assert!(report.is_pass());
        assert_eq!(report.status(), DoctorStatus::Pass);
        assert_eq!(
            status_of(&report, DoctorCheckId::SandboxAvailability),
            DoctorStatus::Pass
        );
        assert_eq!(
            status_of(&report, DoctorCheckId::PolicyParse),
            DoctorStatus::Pass
        );
        assert_eq!(
            status_of(&report, DoctorCheckId::CredentialStore),
            DoctorStatus::Pass
        );
        assert_eq!(
            status_of(&report, DoctorCheckId::DangerousProjectConfig),
            DoctorStatus::Pass
        );
        assert_eq!(
            status_of(&report, DoctorCheckId::ReleaseSignature),
            DoctorStatus::Pass
        );
        assert_eq!(keychain.gets.load(Ordering::SeqCst), 0);
        assert_eq!(keychain.puts.load(Ordering::SeqCst), 0);
        assert_eq!(keychain.deletes.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn doctor_does_not_call_sandbox_side_effects() {
        let backend = CountingBackend::container_available();
        let prepare = Arc::clone(&backend.prepare);
        let exec = Arc::clone(&backend.exec);
        let destroy = Arc::clone(&backend.destroy);
        let mut manager = SandboxManager::new();
        manager.register(Box::new(backend)).expect("register");
        let keychain = CountingKeychain {
            probe: KeychainProbe::available(PlatformKeychainKind::MacosKeychain),
            gets: AtomicUsize::new(0),
            puts: AtomicUsize::new(0),
            deletes: AtomicUsize::new(0),
        };
        let request = healthy_request(&keychain, SandboxObservation::Manager(&manager));
        let report = evaluate_doctor(&request, &CancellationToken::new()).expect("run");
        assert_eq!(
            status_of(&report, DoctorCheckId::SandboxAvailability),
            DoctorStatus::Pass
        );
        assert_eq!(prepare.load(Ordering::SeqCst), 0);
        assert_eq!(exec.load(Ordering::SeqCst), 0);
        assert_eq!(destroy.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn live_host_restricted_backend_warns() {
        let mut manager = SandboxManager::new();
        manager
            .register(Box::new(HostRestrictedBackend::new()))
            .expect("register");
        let keychain = UnimplementedPlatformKeychain::new();
        let request = healthy_request(&keychain, SandboxObservation::Manager(&manager));
        let report = evaluate_doctor(&request, &CancellationToken::new()).expect("run");
        // A live host-restricted tier is weak isolation (Warn). Where the
        // host has no POSIX governance for it (Windows) the tier reports
        // itself unavailable, and the doctor says so rather than warning
        // about a tier that cannot run.
        let expected = if cfg!(unix) {
            DoctorStatus::Warn
        } else {
            DoctorStatus::Unavailable
        };
        assert_eq!(
            status_of(&report, DoctorCheckId::SandboxAvailability),
            expected
        );
    }

    #[test]
    fn sandbox_health_error_is_error_not_pass() {
        let backend = CountingBackend::health_error(SandboxTier::Gvisor);
        let mut manager = SandboxManager::new();
        manager.register(Box::new(backend)).expect("register");
        let keychain = UnimplementedPlatformKeychain::new();
        let request = healthy_request(&keychain, SandboxObservation::Manager(&manager));
        let report = evaluate_doctor(&request, &CancellationToken::new()).expect("run");
        assert_eq!(
            status_of(&report, DoctorCheckId::SandboxAvailability),
            DoctorStatus::Error
        );
        assert!(!report.is_pass());
        assert_eq!(report.status(), DoctorStatus::Error);
    }

    #[test]
    fn unavailable_sandbox_tiers_are_not_pass() {
        let keychain = UnimplementedPlatformKeychain::new();
        let request = healthy_request(&keychain, SandboxObservation::NotProvided)
            .with_sandbox_tiers(vec![
                SandboxTierSnapshot::unavailable(SandboxTier::Gvisor, HealthReason::RuntimeMissing)
                    .expect("tier"),
            ])
            .expect("tiers");
        let report = evaluate_doctor(&request, &CancellationToken::new()).expect("run");
        assert_eq!(
            status_of(&report, DoctorCheckId::SandboxAvailability),
            DoctorStatus::Unavailable
        );
        assert!(
            !report
                .check(DoctorCheckId::SandboxAvailability)
                .unwrap()
                .is_pass()
        );
    }

    #[test]
    fn workspace_allow_policy_is_fail_not_pass() {
        let keychain = CountingKeychain {
            probe: KeychainProbe::available(PlatformKeychainKind::MacosKeychain),
            gets: AtomicUsize::new(0),
            puts: AtomicUsize::new(0),
            deletes: AtomicUsize::new(0),
        };
        let request = DoctorRequest::new()
            .with_sandbox_tiers(vec![
                SandboxTierSnapshot::available(SandboxTier::Container).expect("tier"),
            ])
            .expect("tiers")
            .with_policy(PolicyDocumentInput::new(workspace_source(), allow_doc()).expect("doc"))
            .expect("policy")
            .with_keychain(&keychain)
            .with_project(
                ProjectConfigObservation::new(ProjectTrustState::Trusted, []).expect("project"),
            )
            .with_release(ReleaseSignatureObservation::Verified {
                version: ReleaseLabel::parse("1.0.0").expect("version"),
                channel: ReleaseLabel::parse("stable").expect("channel"),
            });
        let report = evaluate_doctor(&request, &CancellationToken::new()).expect("run");
        let policy = report.check(DoctorCheckId::PolicyParse).expect("policy");
        assert_eq!(policy.status(), DoctorStatus::Fail);
        assert_eq!(policy.metadata().get("doc_0_error"), Some("broadening"));
        assert!(!report.is_pass());
    }

    #[test]
    fn malformed_policy_is_fail_and_omits_canary() {
        let keychain = CountingKeychain {
            probe: KeychainProbe::available(PlatformKeychainKind::MacosKeychain),
            gets: AtomicUsize::new(0),
            puts: AtomicUsize::new(0),
            deletes: AtomicUsize::new(0),
        };
        let body = format!("schema = 1\nsecret = \"{CANARY}\"\n[[[not-toml");
        let request = DoctorRequest::new()
            .with_sandbox_tiers(vec![
                SandboxTierSnapshot::available(SandboxTier::Container).expect("tier"),
            ])
            .expect("tiers")
            .with_policy(PolicyDocumentInput::new(user_source(), body).expect("doc"))
            .expect("policy")
            .with_keychain(&keychain)
            .with_project(
                ProjectConfigObservation::new(ProjectTrustState::Untrusted, []).expect("project"),
            )
            .with_release(ReleaseSignatureObservation::Verified {
                version: ReleaseLabel::parse("1.0.0").expect("version"),
                channel: ReleaseLabel::parse("stable").expect("channel"),
            });
        let report = evaluate_doctor(&request, &CancellationToken::new()).expect("run");
        assert_eq!(
            status_of(&report, DoctorCheckId::PolicyParse),
            DoctorStatus::Fail
        );
        let rendered = format!("{report:?} {report} {:?}", request.policies()[0]);
        assert!(!rendered.contains(CANARY));
        let policy = report.check(DoctorCheckId::PolicyParse).expect("policy");
        for (_, value) in policy.metadata().iter() {
            assert!(!value.contains(CANARY));
        }
    }

    #[test]
    fn workspace_deny_policy_parses() {
        let keychain = CountingKeychain {
            probe: KeychainProbe::available(PlatformKeychainKind::MacosKeychain),
            gets: AtomicUsize::new(0),
            puts: AtomicUsize::new(0),
            deletes: AtomicUsize::new(0),
        };
        let request = DoctorRequest::new()
            .with_sandbox_tiers(vec![
                SandboxTierSnapshot::available(SandboxTier::Container).expect("tier"),
            ])
            .expect("tiers")
            .with_policy(PolicyDocumentInput::new(workspace_source(), deny_doc()).expect("doc"))
            .expect("policy")
            .with_keychain(&keychain)
            .with_project(
                ProjectConfigObservation::new(ProjectTrustState::Untrusted, []).expect("project"),
            )
            .with_release(ReleaseSignatureObservation::Verified {
                version: ReleaseLabel::parse("1.0.0").expect("version"),
                channel: ReleaseLabel::parse("stable").expect("channel"),
            });
        let report = evaluate_doctor(&request, &CancellationToken::new()).expect("run");
        assert_eq!(
            status_of(&report, DoctorCheckId::PolicyParse),
            DoctorStatus::Pass
        );
        assert_eq!(
            report
                .check(DoctorCheckId::PolicyParse)
                .unwrap()
                .metadata()
                .get("rule_count"),
            Some("1")
        );
    }

    #[test]
    fn unimplemented_keychain_is_unavailable_not_pass() {
        let keychain = UnimplementedPlatformKeychain::new();
        let request = DoctorRequest::new()
            .with_sandbox_tiers(vec![
                SandboxTierSnapshot::available(SandboxTier::Container).expect("tier"),
            ])
            .expect("tiers")
            .with_policy(PolicyDocumentInput::new(user_source(), allow_doc()).expect("doc"))
            .expect("policy")
            .with_keychain(&keychain)
            .with_project(
                ProjectConfigObservation::new(ProjectTrustState::Untrusted, []).expect("project"),
            )
            .with_release(ReleaseSignatureObservation::Verified {
                version: ReleaseLabel::parse("1.0.0").expect("version"),
                channel: ReleaseLabel::parse("stable").expect("channel"),
            });
        let report = evaluate_doctor(&request, &CancellationToken::new()).expect("run");
        assert_eq!(
            status_of(&report, DoctorCheckId::CredentialStore),
            DoctorStatus::Unavailable
        );
        assert!(!report.is_pass());
    }

    #[test]
    fn keychain_probe_never_reads_secret_bytes() {
        let keychain = CountingKeychain {
            probe: KeychainProbe::available(PlatformKeychainKind::MacosKeychain),
            gets: AtomicUsize::new(0),
            puts: AtomicUsize::new(0),
            deletes: AtomicUsize::new(0),
        };
        let request = DoctorRequest::new()
            .with_sandbox_tiers(vec![
                SandboxTierSnapshot::available(SandboxTier::Container).expect("tier"),
            ])
            .expect("tiers")
            .with_policy(PolicyDocumentInput::new(user_source(), allow_doc()).expect("doc"))
            .expect("policy")
            .with_keychain(&keychain)
            .with_project(
                ProjectConfigObservation::new(ProjectTrustState::Untrusted, []).expect("project"),
            )
            .with_release(ReleaseSignatureObservation::Verified {
                version: ReleaseLabel::parse("1.0.0").expect("version"),
                channel: ReleaseLabel::parse("stable").expect("channel"),
            });
        let report = evaluate_doctor(&request, &CancellationToken::new()).expect("run");
        assert_eq!(
            status_of(&report, DoctorCheckId::CredentialStore),
            DoctorStatus::Pass
        );
        assert_eq!(keychain.gets.load(Ordering::SeqCst), 0);
        let rendered = format!("{report:?}");
        assert!(!rendered.contains(CANARY));
    }

    #[test]
    fn untrusted_executable_config_cannot_pass() {
        let keychain = CountingKeychain {
            probe: KeychainProbe::available(PlatformKeychainKind::MacosKeychain),
            gets: AtomicUsize::new(0),
            puts: AtomicUsize::new(0),
            deletes: AtomicUsize::new(0),
        };
        let project = ProjectConfigObservation::new(
            ProjectTrustState::Untrusted,
            [
                ExecutableConfigClass::Hooks,
                ExecutableConfigClass::ProjectMcp,
            ],
        )
        .expect("project");
        let request = DoctorRequest::new()
            .with_sandbox_tiers(vec![
                SandboxTierSnapshot::available(SandboxTier::Container).expect("tier"),
            ])
            .expect("tiers")
            .with_policy(PolicyDocumentInput::new(user_source(), allow_doc()).expect("doc"))
            .expect("policy")
            .with_keychain(&keychain)
            .with_project(project)
            .with_release(ReleaseSignatureObservation::Verified {
                version: ReleaseLabel::parse("1.0.0").expect("version"),
                channel: ReleaseLabel::parse("stable").expect("channel"),
            });
        let report = evaluate_doctor(&request, &CancellationToken::new()).expect("run");
        let project = report
            .check(DoctorCheckId::DangerousProjectConfig)
            .expect("project");
        assert_eq!(project.status(), DoctorStatus::Fail);
        assert!(
            project
                .remediation()
                .unwrap()
                .contains("inactive until trust")
        );
        assert_eq!(
            project.metadata().get("surfaces"),
            Some("hooks,project_mcp")
        );
        assert!(!report.is_pass());
    }

    #[test]
    fn unknown_trust_is_error_not_pass() {
        let keychain = CountingKeychain {
            probe: KeychainProbe::available(PlatformKeychainKind::MacosKeychain),
            gets: AtomicUsize::new(0),
            puts: AtomicUsize::new(0),
            deletes: AtomicUsize::new(0),
        };
        let request = DoctorRequest::new()
            .with_sandbox_tiers(vec![
                SandboxTierSnapshot::available(SandboxTier::Container).expect("tier"),
            ])
            .expect("tiers")
            .with_policy(PolicyDocumentInput::new(user_source(), allow_doc()).expect("doc"))
            .expect("policy")
            .with_keychain(&keychain)
            .with_project(
                ProjectConfigObservation::new(ProjectTrustState::Unknown, []).expect("project"),
            )
            .with_release(ReleaseSignatureObservation::Verified {
                version: ReleaseLabel::parse("1.0.0").expect("version"),
                channel: ReleaseLabel::parse("stable").expect("channel"),
            });
        let report = evaluate_doctor(&request, &CancellationToken::new()).expect("run");
        assert_eq!(
            status_of(&report, DoctorCheckId::DangerousProjectConfig),
            DoctorStatus::Error
        );
        assert_eq!(report.status(), DoctorStatus::Error);
        assert!(!report.is_pass());
    }

    #[test]
    fn workspace_grant_attempt_fails_even_when_trusted() {
        let keychain = CountingKeychain {
            probe: KeychainProbe::available(PlatformKeychainKind::MacosKeychain),
            gets: AtomicUsize::new(0),
            puts: AtomicUsize::new(0),
            deletes: AtomicUsize::new(0),
        };
        let project = ProjectConfigObservation::new(ProjectTrustState::Trusted, [])
            .expect("project")
            .with_workspace_policy_grant_attempt(true);
        let request = DoctorRequest::new()
            .with_sandbox_tiers(vec![
                SandboxTierSnapshot::available(SandboxTier::Container).expect("tier"),
            ])
            .expect("tiers")
            .with_policy(PolicyDocumentInput::new(user_source(), allow_doc()).expect("doc"))
            .expect("policy")
            .with_keychain(&keychain)
            .with_project(project)
            .with_release(ReleaseSignatureObservation::Verified {
                version: ReleaseLabel::parse("1.0.0").expect("version"),
                channel: ReleaseLabel::parse("stable").expect("channel"),
            });
        let report = evaluate_doctor(&request, &CancellationToken::new()).expect("run");
        assert_eq!(
            status_of(&report, DoctorCheckId::DangerousProjectConfig),
            DoctorStatus::Fail
        );
    }

    #[test]
    fn unsigned_release_cannot_pass() {
        let keychain = CountingKeychain {
            probe: KeychainProbe::available(PlatformKeychainKind::MacosKeychain),
            gets: AtomicUsize::new(0),
            puts: AtomicUsize::new(0),
            deletes: AtomicUsize::new(0),
        };
        let request = DoctorRequest::new()
            .with_sandbox_tiers(vec![
                SandboxTierSnapshot::available(SandboxTier::Container).expect("tier"),
            ])
            .expect("tiers")
            .with_policy(PolicyDocumentInput::new(user_source(), allow_doc()).expect("doc"))
            .expect("policy")
            .with_keychain(&keychain)
            .with_project(
                ProjectConfigObservation::new(ProjectTrustState::Untrusted, []).expect("project"),
            )
            .with_release(ReleaseSignatureObservation::Unsigned {
                version: Some(ReleaseLabel::parse("1.0.0").expect("version")),
            });
        let report = evaluate_doctor(&request, &CancellationToken::new()).expect("run");
        assert_eq!(
            status_of(&report, DoctorCheckId::ReleaseSignature),
            DoctorStatus::Fail
        );
        assert_eq!(
            report
                .check(DoctorCheckId::ReleaseSignature)
                .unwrap()
                .metadata()
                .get("signature_state"),
            Some("unsigned")
        );
        assert!(!report.is_pass());
    }

    #[test]
    fn invalid_release_signature_is_fail() {
        let keychain = CountingKeychain {
            probe: KeychainProbe::available(PlatformKeychainKind::MacosKeychain),
            gets: AtomicUsize::new(0),
            puts: AtomicUsize::new(0),
            deletes: AtomicUsize::new(0),
        };
        let request = DoctorRequest::new()
            .with_sandbox_tiers(vec![
                SandboxTierSnapshot::available(SandboxTier::Container).expect("tier"),
            ])
            .expect("tiers")
            .with_policy(PolicyDocumentInput::new(user_source(), allow_doc()).expect("doc"))
            .expect("policy")
            .with_keychain(&keychain)
            .with_project(
                ProjectConfigObservation::new(ProjectTrustState::Untrusted, []).expect("project"),
            )
            .with_release(ReleaseSignatureObservation::InvalidSignature);
        let report = evaluate_doctor(&request, &CancellationToken::new()).expect("run");
        assert_eq!(
            status_of(&report, DoctorCheckId::ReleaseSignature),
            DoctorStatus::Fail
        );
    }

    #[test]
    fn release_verification_error_is_error_not_pass() {
        let keychain = CountingKeychain {
            probe: KeychainProbe::available(PlatformKeychainKind::MacosKeychain),
            gets: AtomicUsize::new(0),
            puts: AtomicUsize::new(0),
            deletes: AtomicUsize::new(0),
        };
        let request = DoctorRequest::new()
            .with_sandbox_tiers(vec![
                SandboxTierSnapshot::available(SandboxTier::Container).expect("tier"),
            ])
            .expect("tiers")
            .with_policy(PolicyDocumentInput::new(user_source(), allow_doc()).expect("doc"))
            .expect("policy")
            .with_keychain(&keychain)
            .with_project(
                ProjectConfigObservation::new(ProjectTrustState::Untrusted, []).expect("project"),
            )
            .with_release(ReleaseSignatureObservation::VerificationError);
        let report = evaluate_doctor(&request, &CancellationToken::new()).expect("run");
        assert_eq!(
            status_of(&report, DoctorCheckId::ReleaseSignature),
            DoctorStatus::Error
        );
        assert!(!report.is_pass());
    }

    #[test]
    fn cancelled_run_does_not_yield_pass() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = evaluate_doctor(&DoctorRequest::new(), &cancel).expect_err("cancelled");
        assert_eq!(err, DoctorError::Cancelled);
        assert_eq!(err.code(), "security.doctor_cancelled");
    }

    #[test]
    fn oversized_policy_is_rejected_before_parse() {
        let body = "x".repeat(MAX_POLICY_DOCUMENT_BYTES + 1);
        let err = PolicyDocumentInput::new(user_source(), body).expect_err("bound");
        assert!(matches!(err, DoctorError::BoundExceeded { .. }));
    }

    #[test]
    fn control_characters_are_rejected_in_release_labels() {
        assert_eq!(
            ReleaseLabel::parse("1.0.0\u{001b}secret"),
            Err(DoctorError::InvalidLabel)
        );
        assert_eq!(ReleaseLabel::parse(""), Err(DoctorError::InvalidLabel));
    }

    #[test]
    fn default_request_debug_omits_provider_credentials() {
        let request = DoctorRequest::new();
        let debug = format!("{request:?}");
        assert!(debug.contains("credentials: \"not_provided\""));
        assert!(!debug.to_lowercase().contains("token"));
        assert!(!debug.contains(CANARY));
    }
}
