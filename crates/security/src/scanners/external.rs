//! External SAST/SCA scanner adapter.
//!
//! Runs configured scanner argv through a supervised sandbox with least
//! mounts and no network, then normalizes SARIF 2.1.0 into RapidLM findings.
//! Malformed SARIF, crash, timeout, and unavailability are never Passed.
//! Threats: `T-009`, `T-001`, `T-012`, `T-016`, `T-014`.

use std::collections::BTreeMap;
use std::fmt::{self, Debug, Display, Formatter};
use std::sync::atomic::{Ordering, compiler_fence};
use std::time::Duration;

use capability_broker::{CancellationToken, CanonicalHostPath};
use protocol::{ArtifactId, RepoPath, SandboxTier};
use sandbox::{
    MountMode, SandboxError, SandboxExecRequest, SandboxMount, SandboxNetwork, SandboxSpec,
};
use sha2::{Digest, Sha256};

/// Adapter identity stored on every report.
pub const ADAPTER_ID: &str = "rapidlm.external.sarif";

/// Pinned adapter contract version. Not a vendor engine version.
pub const ADAPTER_VERSION: &str = "1";

/// In-sandbox workspace mount target (read-only).
pub const WORKSPACE_MOUNT: &str = "workspace";

/// In-sandbox temp mount for scanner output.
pub const SCAN_OUT_MOUNT: &str = "scan-out";

/// Maximum UTF-8 bytes accepted for a configured scanner id.
pub const MAX_SCANNER_ID_BYTES: usize = 64;

/// Maximum argv tokens accepted for one scanner command.
pub const MAX_SCANNER_ARGV: usize = 256;

/// Maximum UTF-8 bytes accepted for one argv token.
pub const MAX_SCANNER_ARG_BYTES: usize = 4096;

/// Maximum SARIF bytes accepted from a runner or artifact.
pub const MAX_SARIF_BYTES: usize = 8 * 1024 * 1024;

/// Maximum findings retained on one report.
pub const MAX_EXTERNAL_FINDINGS: usize = 1024;

/// Default child timeout when the config omits one.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Default captured-output ceiling when the config omits one.
pub const DEFAULT_OUTPUT_LIMIT: u64 = 2 * 1024 * 1024;

/// Matches [`sandbox`] child timeout ceiling (`MAX_TIMEOUT`).
const MAX_TIMEOUT: Duration = Duration::from_secs(3_600);

const FINGERPRINT_HEX_LEN: usize = 16;
const CANCEL_STRIDE: usize = 32;
const MAX_RULE_ID_BYTES: usize = 128;
const MAX_MESSAGE_BYTES: usize = 256;
const MAX_REMEDIATION_BYTES: usize = 256;
const TAG_FINDING: &[u8] = b"rapidlm.external_finding.v1";
const UNRESOLVED_PATH: &str = "external/unresolved";
const SARIF_VERSION: &str = "2.1.0";

/// Cooperative cancellation alias. Checks map to [`ExternalScanError::Cancelled`].
pub type ExternalScanCancellation = CancellationToken;

/// Configured scanner class. Adapters share one SARIF normalizer.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ExternalScannerKind {
    Sast,
    Sca,
    Container,
}

/// Outcome consumed by the security gate. Never upgraded from error/unavailable.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ExternalScanStatus {
    Passed,
    Failed,
    Unavailable,
    Error,
}

/// Finding category mirrors the configured scanner kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ExternalFindingCategory {
    Sast,
    Sca,
    Container,
}

/// Finding severity mapped from SARIF `level`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub enum ExternalFindingSeverity {
    Low,
    Medium,
    High,
    Critical,
}

/// Detector confidence mapped from SARIF `rank` when present.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub enum ExternalFindingConfidence {
    Low,
    Medium,
    High,
}

/// Half-open byte range within one location. `[start, end)`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ExternalByteRange {
    start: u64,
    end: u64,
}

/// Stable finding identity. Hex of a digest over rule/path/range/message-hash.
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct ExternalFindingFingerprint {
    hex: [u8; FINGERPRINT_HEX_LEN],
}

/// Normalized finding. Raw SARIF / attacker payload bytes are never a field.
#[derive(Clone, Eq, PartialEq)]
pub struct ExternalFinding {
    id: ExternalFindingFingerprint,
    rule_id: String,
    category: ExternalFindingCategory,
    severity: ExternalFindingSeverity,
    confidence: ExternalFindingConfidence,
    path: RepoPath,
    range: ExternalByteRange,
    fingerprint: ExternalFindingFingerprint,
    message: String,
    remediation: String,
    scanner: String,
    evidence_ref: ArtifactId,
}

/// How a supervised child finished. Distinct from SARIF parse status.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ExternalExecKind {
    Completed { exit_code: i32 },
    TimedOut,
    Crashed,
    Cancelled,
    Unavailable,
}

/// Bounded runner output. Debug omits SARIF bytes.
pub struct ExternalExecResult {
    kind: ExternalExecKind,
    sarif: Vec<u8>,
}

/// Least-privilege sandbox + argv-first exec planned for one scan.
pub struct ExternalSandboxPlan {
    spec: SandboxSpec,
    exec: SandboxExecRequest,
}

/// Audit view of the planned sandbox. Never includes host paths as authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ExternalSandboxEvidence {
    network: SandboxNetwork,
    mount_count: usize,
    read_only_mounts: usize,
    temp_mounts: usize,
    read_write_mounts: usize,
    secrets: usize,
    env_names: usize,
    timeout: Duration,
    output_limit: u64,
    tier: SandboxTier,
}

/// Bounded coverage counters. Labels never include SARIF payload bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ExternalScanCoverage {
    runs: usize,
    results: usize,
    bytes_scanned: usize,
    findings: usize,
}

/// Typed adapter failure. Display never echoes SARIF, argv, or path bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExternalScanError {
    Cancelled,
    BoundExceeded { limit: usize, requested: usize },
    EmptyScannerId,
    InvalidScannerId,
    EmptyArgv,
    InvalidTimeout,
    InvalidOutputLimit,
    InvalidWorkspace,
    ForbiddenMount,
    MalformedSarif,
    TimedOut,
    Crashed,
    Unavailable,
}

/// Command/tool config for one external scanner. No network or extra mounts.
#[derive(Clone, Eq, PartialEq)]
pub struct ExternalScannerConfig {
    id: String,
    kind: ExternalScannerKind,
    argv: Vec<String>,
    timeout: Duration,
    output_limit: u64,
    tier: SandboxTier,
}

/// Adapter request: config plus optional already-captured SARIF artifact.
pub struct ExternalScanRequest {
    config: ExternalScannerConfig,
    workspace: CanonicalHostPath,
    sarif: Option<SarifArtifact>,
}

struct SarifArtifact {
    bytes: Vec<u8>,
    artifact_id: ArtifactId,
}

/// Normalized report. Debug omits raw SARIF bytes.
#[derive(Clone, Eq, PartialEq)]
pub struct ExternalScanReport {
    status: ExternalScanStatus,
    findings: Vec<ExternalFinding>,
    coverage: ExternalScanCoverage,
    scanner_versions: Vec<String>,
    errors: Vec<ExternalScanError>,
    raw_sarif: Option<ArtifactId>,
    sandbox: ExternalSandboxEvidence,
}

/// External SAST/SCA/container adapter. Holds no vendor state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ExternalScannerAdapter;

/// Supervised Process/Sandbox execution seam. Implementations re-check leases.
pub trait SupervisedScannerExec {
    fn run(
        &self,
        plan: &ExternalSandboxPlan,
        cancel: &CancellationToken,
    ) -> Result<ExternalExecResult, ExternalScanError>;
}

struct ParsedSarif {
    runs: usize,
    results: usize,
    tool_label: Option<String>,
    findings: Vec<RawFinding>,
}

struct RawFinding {
    rule_id: String,
    severity: ExternalFindingSeverity,
    confidence: ExternalFindingConfidence,
    path: RepoPath,
    range: ExternalByteRange,
    message: String,
    remediation: String,
}

impl ExternalByteRange {
    pub const fn new(start: u64, end: u64) -> Result<Self, ExternalScanError> {
        if start > end {
            return Err(ExternalScanError::MalformedSarif);
        }
        Ok(Self { start, end })
    }

    pub const fn start(self) -> u64 {
        self.start
    }

    pub const fn end(self) -> u64 {
        self.end
    }
}

impl ExternalFindingFingerprint {
    fn compute(
        scanner: &str,
        rule_id: &str,
        path: &RepoPath,
        start: u64,
        end: u64,
        message: &str,
    ) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(TAG_FINDING);
        hasher.update(scanner.as_bytes());
        hasher.update([0u8]);
        hasher.update(rule_id.as_bytes());
        hasher.update([0u8]);
        hasher.update(path.as_str().as_bytes());
        hasher.update([0u8]);
        hasher.update(start.to_be_bytes());
        hasher.update(end.to_be_bytes());
        hasher.update(message.as_bytes());
        let digest = hasher.finalize();
        let mut hex = [0u8; FINGERPRINT_HEX_LEN];
        write_hex_lower(&digest[..8], &mut hex);
        Self { hex }
    }

    pub fn as_hex(&self) -> &str {
        std::str::from_utf8(&self.hex).unwrap_or("????????????????")
    }
}

impl Display for ExternalFindingFingerprint {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_hex())
    }
}

impl Debug for ExternalFindingFingerprint {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ExternalFindingFingerprint")
            .field(&self.as_hex())
            .finish()
    }
}

impl ExternalFinding {
    pub fn id(&self) -> ExternalFindingFingerprint {
        self.id
    }

    pub fn rule_id(&self) -> &str {
        &self.rule_id
    }

    pub fn category(&self) -> ExternalFindingCategory {
        self.category
    }

    pub fn severity(&self) -> ExternalFindingSeverity {
        self.severity
    }

    pub fn confidence(&self) -> ExternalFindingConfidence {
        self.confidence
    }

    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    pub fn range(&self) -> ExternalByteRange {
        self.range
    }

    pub fn fingerprint(&self) -> ExternalFindingFingerprint {
        self.fingerprint
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn remediation(&self) -> &str {
        &self.remediation
    }

    pub fn scanner(&self) -> &str {
        &self.scanner
    }

    pub fn evidence_ref(&self) -> ArtifactId {
        self.evidence_ref
    }
}

impl Debug for ExternalFinding {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExternalFinding")
            .field("rule_id", &self.rule_id)
            .field("severity", &self.severity)
            .field("confidence", &self.confidence)
            .field("path", &self.path)
            .field("range", &self.range)
            .field("fingerprint", &self.fingerprint)
            .field("scanner", &self.scanner)
            .field("evidence_ref", &self.evidence_ref)
            .finish()
    }
}

impl Display for ExternalFinding {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {:?} {} {}..{} fp={}",
            self.rule_id,
            self.severity,
            self.path.as_str(),
            self.range.start,
            self.range.end,
            self.fingerprint.as_hex()
        )
    }
}

impl ExternalExecResult {
    pub fn new(
        kind: ExternalExecKind,
        sarif: impl Into<Vec<u8>>,
    ) -> Result<Self, ExternalScanError> {
        let mut sarif = sarif.into();
        if sarif.len() > MAX_SARIF_BYTES {
            let requested = sarif.len();
            wipe(&mut sarif);
            return Err(ExternalScanError::BoundExceeded {
                limit: MAX_SARIF_BYTES,
                requested,
            });
        }
        Ok(Self { kind, sarif })
    }

    pub const fn kind(&self) -> ExternalExecKind {
        self.kind
    }

    pub fn sarif(&self) -> &[u8] {
        &self.sarif
    }
}

impl Debug for ExternalExecResult {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExternalExecResult")
            .field("kind", &self.kind)
            .field("bytes", &self.sarif.len())
            .finish()
    }
}

impl Drop for ExternalExecResult {
    fn drop(&mut self) {
        wipe(&mut self.sarif);
        self.sarif.clear();
    }
}

impl ExternalSandboxPlan {
    pub fn spec(&self) -> &SandboxSpec {
        &self.spec
    }

    pub fn exec(&self) -> &SandboxExecRequest {
        &self.exec
    }

    pub fn evidence(&self) -> ExternalSandboxEvidence {
        let mut read_only = 0usize;
        let mut temp = 0usize;
        let mut read_write = 0usize;
        for mount in self.spec.mounts() {
            match mount.mode() {
                MountMode::ReadOnly => read_only = read_only.saturating_add(1),
                MountMode::Temp => temp = temp.saturating_add(1),
                MountMode::ReadWrite => read_write = read_write.saturating_add(1),
            }
        }
        ExternalSandboxEvidence {
            network: self.spec.network(),
            mount_count: self.spec.mounts().len(),
            read_only_mounts: read_only,
            temp_mounts: temp,
            read_write_mounts: read_write,
            secrets: self.spec.secrets().len(),
            env_names: self.spec.env_allowlist().len(),
            timeout: self.spec.timeout(),
            output_limit: self.spec.output_limit(),
            tier: self.spec.tier(),
        }
    }
}

impl Debug for ExternalSandboxPlan {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExternalSandboxPlan")
            .field("evidence", &self.evidence())
            .field("argv_len", &self.exec.argv().len())
            .finish()
    }
}

impl ExternalSandboxEvidence {
    pub const fn network(self) -> SandboxNetwork {
        self.network
    }

    pub const fn mount_count(self) -> usize {
        self.mount_count
    }

    pub const fn read_only_mounts(self) -> usize {
        self.read_only_mounts
    }

    pub const fn temp_mounts(self) -> usize {
        self.temp_mounts
    }

    pub const fn read_write_mounts(self) -> usize {
        self.read_write_mounts
    }

    pub const fn secrets(self) -> usize {
        self.secrets
    }

    pub const fn env_names(self) -> usize {
        self.env_names
    }

    pub const fn timeout(self) -> Duration {
        self.timeout
    }

    pub const fn output_limit(self) -> u64 {
        self.output_limit
    }

    pub const fn tier(self) -> SandboxTier {
        self.tier
    }

    pub fn is_least_privilege(self) -> bool {
        self.network == SandboxNetwork::None
            && self.read_write_mounts == 0
            && self.secrets == 0
            && self.env_names == 0
            && self.read_only_mounts == 1
            && self.temp_mounts == 1
            && self.mount_count == 2
    }
}

impl ExternalScanCoverage {
    pub const fn runs(self) -> usize {
        self.runs
    }

    pub const fn results(self) -> usize {
        self.results
    }

    pub const fn bytes_scanned(self) -> usize {
        self.bytes_scanned
    }

    pub const fn findings(self) -> usize {
        self.findings
    }
}

impl ExternalScanError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Cancelled => "security.external_scan_cancelled",
            Self::BoundExceeded { .. } => "security.external_scan_bound_exceeded",
            Self::EmptyScannerId => "security.external_scan_empty_id",
            Self::InvalidScannerId => "security.external_scan_invalid_id",
            Self::EmptyArgv => "security.external_scan_empty_argv",
            Self::InvalidTimeout => "security.external_scan_invalid_timeout",
            Self::InvalidOutputLimit => "security.external_scan_invalid_output",
            Self::InvalidWorkspace => "security.external_scan_invalid_workspace",
            Self::ForbiddenMount => "security.external_scan_forbidden_mount",
            Self::MalformedSarif => "security.external_scan_malformed_sarif",
            Self::TimedOut => "security.external_scan_timeout",
            Self::Crashed => "security.external_scan_crashed",
            Self::Unavailable => "security.external_scan_unavailable",
        }
    }

    pub fn retryable(&self) -> bool {
        false
    }
}

impl Display for ExternalScanError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("external scan was cancelled"),
            Self::BoundExceeded { limit, requested } => {
                write!(
                    f,
                    "external scan exceeds bound ({requested} > {limit} bytes or items)"
                )
            }
            Self::EmptyScannerId => f.write_str("external scanner id must not be empty"),
            Self::InvalidScannerId => f.write_str("external scanner id is invalid"),
            Self::EmptyArgv => f.write_str("external scanner argv must not be empty"),
            Self::InvalidTimeout => f.write_str("external scanner timeout is invalid"),
            Self::InvalidOutputLimit => f.write_str("external scanner output limit is invalid"),
            Self::InvalidWorkspace => f.write_str("external scanner workspace is invalid"),
            Self::ForbiddenMount => f.write_str("external scanner mount is forbidden"),
            Self::MalformedSarif => f.write_str("external scanner produced malformed SARIF"),
            Self::TimedOut => f.write_str("external scanner timed out"),
            Self::Crashed => f.write_str("external scanner crashed"),
            Self::Unavailable => f.write_str("external scanner is unavailable"),
        }
    }
}

impl std::error::Error for ExternalScanError {}

impl ExternalScannerConfig {
    pub fn new(
        id: impl Into<String>,
        kind: ExternalScannerKind,
        argv: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self, ExternalScanError> {
        let id = id.into();
        validate_scanner_id(&id)?;
        let argv: Vec<String> = argv.into_iter().map(Into::into).collect();
        validate_argv(&argv)?;
        Ok(Self {
            id,
            kind,
            argv,
            timeout: DEFAULT_TIMEOUT,
            output_limit: DEFAULT_OUTPUT_LIMIT,
            tier: SandboxTier::Container,
        })
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Result<Self, ExternalScanError> {
        validate_timeout(timeout)?;
        self.timeout = timeout;
        Ok(self)
    }

    pub fn with_output_limit(mut self, output_limit: u64) -> Result<Self, ExternalScanError> {
        validate_output_limit(output_limit)?;
        self.output_limit = output_limit;
        Ok(self)
    }

    pub fn with_tier(mut self, tier: SandboxTier) -> Self {
        self.tier = tier;
        self
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn kind(&self) -> ExternalScannerKind {
        self.kind
    }

    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn output_limit(&self) -> u64 {
        self.output_limit
    }

    pub fn tier(&self) -> SandboxTier {
        self.tier
    }
}

impl Debug for ExternalScannerConfig {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExternalScannerConfig")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("argv_len", &self.argv.len())
            .field("timeout", &self.timeout)
            .field("output_limit", &self.output_limit)
            .field("tier", &self.tier)
            .finish()
    }
}

impl ExternalScanRequest {
    pub fn new(config: ExternalScannerConfig, workspace: CanonicalHostPath) -> Self {
        Self {
            config,
            workspace,
            sarif: None,
        }
    }

    pub fn with_sarif_bytes(
        mut self,
        bytes: impl Into<Vec<u8>>,
    ) -> Result<Self, ExternalScanError> {
        let mut bytes = bytes.into();
        if bytes.len() > MAX_SARIF_BYTES {
            let requested = bytes.len();
            wipe(&mut bytes);
            return Err(ExternalScanError::BoundExceeded {
                limit: MAX_SARIF_BYTES,
                requested,
            });
        }
        let artifact_id = ArtifactId::from_bytes(&bytes);
        self.sarif = Some(SarifArtifact { bytes, artifact_id });
        Ok(self)
    }

    pub fn config(&self) -> &ExternalScannerConfig {
        &self.config
    }

    pub fn workspace(&self) -> &CanonicalHostPath {
        &self.workspace
    }

    pub fn sarif_artifact_id(&self) -> Option<ArtifactId> {
        self.sarif.as_ref().map(|item| item.artifact_id)
    }

    fn sarif_bytes(&self) -> Option<&[u8]> {
        self.sarif.as_ref().map(|item| item.bytes.as_slice())
    }
}

impl Debug for ExternalScanRequest {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExternalScanRequest")
            .field("config", &self.config)
            .field("has_sarif", &self.sarif.is_some())
            .field(
                "sarif_bytes",
                &self.sarif.as_ref().map(|item| item.bytes.len()),
            )
            .finish()
    }
}

impl Drop for SarifArtifact {
    fn drop(&mut self) {
        wipe(&mut self.bytes);
        self.bytes.clear();
    }
}

impl ExternalScanReport {
    pub fn status(&self) -> ExternalScanStatus {
        self.status
    }

    pub fn findings(&self) -> &[ExternalFinding] {
        &self.findings
    }

    pub fn coverage(&self) -> ExternalScanCoverage {
        self.coverage
    }

    pub fn scanner_versions(&self) -> &[String] {
        &self.scanner_versions
    }

    pub fn errors(&self) -> &[ExternalScanError] {
        &self.errors
    }

    pub fn raw_sarif(&self) -> Option<ArtifactId> {
        self.raw_sarif
    }

    pub fn sandbox(&self) -> ExternalSandboxEvidence {
        self.sandbox
    }
}

impl Debug for ExternalScanReport {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExternalScanReport")
            .field("status", &self.status)
            .field("findings", &self.findings)
            .field("coverage", &self.coverage)
            .field("scanner_versions", &self.scanner_versions)
            .field("errors", &self.errors)
            .field("raw_sarif", &self.raw_sarif)
            .field("sandbox", &self.sandbox)
            .finish()
    }
}

impl ExternalScannerAdapter {
    pub fn new() -> Self {
        Self
    }

    /// Plan the least-privilege sandbox for this request. Network is always none.
    pub fn plan(
        &self,
        request: &ExternalScanRequest,
    ) -> Result<ExternalSandboxPlan, ExternalScanError> {
        let workspace_target =
            RepoPath::parse(WORKSPACE_MOUNT).map_err(|_| ExternalScanError::InvalidWorkspace)?;
        let out_target =
            RepoPath::parse(SCAN_OUT_MOUNT).map_err(|_| ExternalScanError::InvalidWorkspace)?;
        let workspace = SandboxMount::bind(
            request.workspace.clone(),
            workspace_target.clone(),
            MountMode::ReadOnly,
        )
        .map_err(map_sandbox_error)?;
        let scan_out = SandboxMount::temp(out_target).map_err(map_sandbox_error)?;
        let spec = SandboxSpec::builder(request.config.tier)
            .mount(workspace)
            .mount(scan_out)
            .cwd(workspace_target)
            .network(SandboxNetwork::None)
            .timeout(request.config.timeout)
            .output_limit(request.config.output_limit)
            .build()
            .map_err(map_sandbox_error)?;
        let exec = SandboxExecRequest::new(
            request.config.argv.clone(),
            request.config.timeout,
            request.config.output_limit,
        )
        .map_err(map_sandbox_error)?;
        Ok(ExternalSandboxPlan { spec, exec })
    }

    /// Run the configured scanner (or normalize a supplied SARIF artifact).
    pub fn scan(
        &self,
        request: &ExternalScanRequest,
        exec: Option<&dyn SupervisedScannerExec>,
        cancel: &CancellationToken,
    ) -> Result<ExternalScanReport, ExternalScanError> {
        check_cancel(cancel)?;
        let plan = self.plan(request)?;
        let evidence = plan.evidence();
        if !evidence.is_least_privilege() {
            return Ok(status_report(
                ExternalScanStatus::Error,
                evidence,
                Vec::new(),
                0,
                0,
                0,
                None,
                vec![ExternalScanError::ForbiddenMount],
                &request.config,
                None,
            ));
        }

        if let Some(bytes) = request.sarif_bytes() {
            return self.normalize_completed(
                request,
                evidence,
                ExternalExecKind::Completed { exit_code: 0 },
                bytes,
                cancel,
            );
        }

        let Some(exec) = exec else {
            return Ok(status_report(
                ExternalScanStatus::Unavailable,
                evidence,
                Vec::new(),
                0,
                0,
                0,
                None,
                vec![ExternalScanError::Unavailable],
                &request.config,
                None,
            ));
        };

        check_cancel(cancel)?;
        let mut outcome = match exec.run(&plan, cancel) {
            Ok(outcome) => outcome,
            Err(ExternalScanError::Cancelled) => return Err(ExternalScanError::Cancelled),
            Err(ExternalScanError::Unavailable) => {
                return Ok(status_report(
                    ExternalScanStatus::Unavailable,
                    evidence,
                    Vec::new(),
                    0,
                    0,
                    0,
                    None,
                    vec![ExternalScanError::Unavailable],
                    &request.config,
                    None,
                ));
            }
            Err(err) => {
                return Ok(status_report(
                    ExternalScanStatus::Error,
                    evidence,
                    Vec::new(),
                    0,
                    0,
                    0,
                    None,
                    vec![err],
                    &request.config,
                    None,
                ));
            }
        };
        let result =
            self.normalize_completed(request, evidence, outcome.kind, &outcome.sarif, cancel);
        wipe(&mut outcome.sarif);
        result
    }
}

impl ExternalScannerAdapter {
    fn normalize_completed(
        &self,
        request: &ExternalScanRequest,
        evidence: ExternalSandboxEvidence,
        kind: ExternalExecKind,
        sarif: &[u8],
        cancel: &CancellationToken,
    ) -> Result<ExternalScanReport, ExternalScanError> {
        check_cancel(cancel)?;
        let raw = if sarif.is_empty() {
            None
        } else {
            Some(ArtifactId::from_bytes(sarif))
        };

        match kind {
            ExternalExecKind::Cancelled => Err(ExternalScanError::Cancelled),
            ExternalExecKind::Unavailable => {
                Ok(status_report(
                    ExternalScanStatus::Unavailable,
                    evidence,
                    Vec::new(),
                    0,
                    0,
                    sarif.len(),
                    raw,
                    vec![ExternalScanError::Unavailable],
                    &request.config,
                    None,
                ))
            }
            ExternalExecKind::TimedOut => {
                Ok(status_report(
                    ExternalScanStatus::Error,
                    evidence,
                    Vec::new(),
                    0,
                    0,
                    sarif.len(),
                    raw,
                    vec![ExternalScanError::TimedOut],
                    &request.config,
                    None,
                ))
            }
            ExternalExecKind::Crashed => {
                Ok(status_report(
                    ExternalScanStatus::Error,
                    evidence,
                    Vec::new(),
                    0,
                    0,
                    sarif.len(),
                    raw,
                    vec![ExternalScanError::Crashed],
                    &request.config,
                    None,
                ))
            }
            ExternalExecKind::Completed { exit_code } => {
                let parsed = match parse_sarif(sarif, request.config.kind, cancel) {
                    Ok(parsed) => parsed,
                    Err(ExternalScanError::Cancelled) => return Err(ExternalScanError::Cancelled),
                    Err(err) => {
                        return Ok(status_report(
                            ExternalScanStatus::Error,
                            evidence,
                            Vec::new(),
                            0,
                            0,
                            sarif.len(),
                            raw,
                            vec![err],
                            &request.config,
                            None,
                        ));
                    }
                };
                if parsed.findings.len() > MAX_EXTERNAL_FINDINGS {
                    return Ok(status_report(
                        ExternalScanStatus::Error,
                        evidence,
                        Vec::new(),
                        parsed.runs,
                        parsed.results,
                        sarif.len(),
                        raw,
                        vec![ExternalScanError::BoundExceeded {
                            limit: MAX_EXTERNAL_FINDINGS,
                            requested: parsed.findings.len(),
                        }],
                        &request.config,
                        parsed.tool_label.as_deref(),
                    ));
                }
                let evidence_ref = raw.unwrap_or_else(|| ArtifactId::from_bytes(sarif));
                let findings =
                    materialize_findings(&request.config, evidence_ref, parsed.findings, cancel)?;
                let (status, errors) = if findings.is_empty() {
                    if exit_code == 0 {
                        (ExternalScanStatus::Passed, Vec::new())
                    } else {
                        (ExternalScanStatus::Error, vec![ExternalScanError::Crashed])
                    }
                } else {
                    (ExternalScanStatus::Failed, Vec::new())
                };
                Ok(status_report(
                    status,
                    evidence,
                    findings,
                    parsed.runs,
                    parsed.results,
                    sarif.len(),
                    raw,
                    errors,
                    &request.config,
                    parsed.tool_label.as_deref(),
                ))
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn status_report(
    status: ExternalScanStatus,
    sandbox: ExternalSandboxEvidence,
    findings: Vec<ExternalFinding>,
    runs: usize,
    results: usize,
    bytes_scanned: usize,
    raw_sarif: Option<ArtifactId>,
    errors: Vec<ExternalScanError>,
    config: &ExternalScannerConfig,
    tool_label: Option<&str>,
) -> ExternalScanReport {
    let mut scanner_versions = vec![format!("{ADAPTER_ID}/{ADAPTER_VERSION}/{}", config.id)];
    if let Some(tool) = tool_label {
        scanner_versions.push(tool.to_owned());
    }
    ExternalScanReport {
        status,
        coverage: ExternalScanCoverage {
            runs,
            results,
            bytes_scanned,
            findings: findings.len(),
        },
        findings,
        scanner_versions,
        errors,
        raw_sarif,
        sandbox,
    }
}

fn materialize_findings(
    config: &ExternalScannerConfig,
    evidence_ref: ArtifactId,
    raw: Vec<RawFinding>,
    cancel: &CancellationToken,
) -> Result<Vec<ExternalFinding>, ExternalScanError> {
    let mut findings = Vec::with_capacity(raw.len());
    for (index, item) in raw.into_iter().enumerate() {
        if index.is_multiple_of(CANCEL_STRIDE) {
            check_cancel(cancel)?;
        }
        let fingerprint = ExternalFindingFingerprint::compute(
            &config.id,
            &item.rule_id,
            &item.path,
            item.range.start,
            item.range.end,
            &item.message,
        );
        findings.push(ExternalFinding {
            id: fingerprint,
            rule_id: item.rule_id,
            category: category_of(config.kind),
            severity: item.severity,
            confidence: item.confidence,
            path: item.path,
            range: item.range,
            fingerprint,
            message: item.message,
            remediation: item.remediation,
            scanner: config.id.clone(),
            evidence_ref,
        });
    }
    Ok(findings)
}

fn parse_sarif(
    bytes: &[u8],
    kind: ExternalScannerKind,
    cancel: &CancellationToken,
) -> Result<ParsedSarif, ExternalScanError> {
    check_cancel(cancel)?;
    if bytes.is_empty() {
        return Err(ExternalScanError::MalformedSarif);
    }
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| ExternalScanError::MalformedSarif)?;
    check_cancel(cancel)?;
    let obj = value.as_object().ok_or(ExternalScanError::MalformedSarif)?;
    let version = obj
        .get("version")
        .and_then(|item| item.as_str())
        .ok_or(ExternalScanError::MalformedSarif)?;
    if version != SARIF_VERSION {
        return Err(ExternalScanError::MalformedSarif);
    }
    let runs = obj
        .get("runs")
        .and_then(|item| item.as_array())
        .ok_or(ExternalScanError::MalformedSarif)?;

    let mut parsed = ParsedSarif {
        runs: runs.len(),
        results: 0,
        tool_label: None,
        findings: Vec::new(),
    };

    for (run_index, run) in runs.iter().enumerate() {
        if run_index.is_multiple_of(CANCEL_STRIDE) {
            check_cancel(cancel)?;
        }
        let run = run.as_object().ok_or(ExternalScanError::MalformedSarif)?;
        if parsed.tool_label.is_none() {
            parsed.tool_label = tool_label(run);
        }
        let remediations = rule_remediations(run);
        match run.get("results") {
            None => {}
            Some(results) => {
                let results = results
                    .as_array()
                    .ok_or(ExternalScanError::MalformedSarif)?;
                parsed.results = parsed.results.saturating_add(results.len());
                for (result_index, result) in results.iter().enumerate() {
                    if result_index.is_multiple_of(CANCEL_STRIDE) {
                        check_cancel(cancel)?;
                    }
                    let result = result
                        .as_object()
                        .ok_or(ExternalScanError::MalformedSarif)?;
                    parsed
                        .findings
                        .push(normalize_result(result, kind, &remediations)?);
                }
            }
        }
    }
    Ok(parsed)
}

fn normalize_result(
    result: &serde_json::Map<String, serde_json::Value>,
    kind: ExternalScannerKind,
    remediations: &BTreeMap<String, String>,
) -> Result<RawFinding, ExternalScanError> {
    let rule_id = result
        .get("ruleId")
        .and_then(|item| item.as_str())
        .map(sanitize_rule_id)
        .filter(|item| !item.is_empty())
        .unwrap_or_else(|| "sarif.unknown".to_owned());
    let level = result.get("level").and_then(|item| item.as_str());
    let severity = map_severity(level, kind);
    let confidence = result
        .get("rank")
        .and_then(|item| item.as_f64())
        .map(map_rank)
        .unwrap_or(ExternalFindingConfidence::Medium);
    let message = result
        .get("message")
        .and_then(|item| item.get("text"))
        .and_then(|item| item.as_str())
        .map(sanitize_text)
        .unwrap_or_default();
    let remediation = remediations
        .get(&rule_id)
        .cloned()
        .unwrap_or_else(|| default_remediation(kind).to_owned());
    let (path, range) = location_of(result);
    Ok(RawFinding {
        rule_id,
        severity,
        confidence,
        path,
        range,
        message,
        remediation,
    })
}

fn unresolved_path() -> RepoPath {
    // Static repo-relative identifier: no `..`, absolute, or control bytes.
    RepoPath::parse(UNRESOLVED_PATH).expect("UNRESOLVED_PATH is a static repo-relative identifier")
}

fn location_of(
    result: &serde_json::Map<String, serde_json::Value>,
) -> (RepoPath, ExternalByteRange) {
    let fallback = (unresolved_path(), ExternalByteRange { start: 0, end: 0 });
    let Some(locations) = result.get("locations").and_then(|item| item.as_array()) else {
        return fallback;
    };
    let Some(first) = locations.first().and_then(|item| item.as_object()) else {
        return fallback;
    };
    let Some(physical) = first
        .get("physicalLocation")
        .and_then(|item| item.as_object())
    else {
        return fallback;
    };
    let uri = physical
        .get("artifactLocation")
        .and_then(|item| item.get("uri"))
        .and_then(|item| item.as_str())
        .unwrap_or("");
    let path = repo_path_from_uri(uri).unwrap_or_else(|| fallback.0.clone());
    let range = physical
        .get("region")
        .and_then(|item| item.as_object())
        .map(range_from_region)
        .unwrap_or(fallback.1);
    (path, range)
}

fn range_from_region(region: &serde_json::Map<String, serde_json::Value>) -> ExternalByteRange {
    if let (Some(start), Some(len)) = (
        as_u64(region.get("byteOffset")),
        as_u64(region.get("byteLength")),
    ) {
        return ExternalByteRange {
            start,
            end: start.saturating_add(len),
        };
    }
    ExternalByteRange { start: 0, end: 0 }
}

fn as_u64(value: Option<&serde_json::Value>) -> Option<u64> {
    value.and_then(|item| {
        item.as_u64()
            .or_else(|| item.as_i64().and_then(|n| u64::try_from(n).ok()))
    })
}

fn repo_path_from_uri(uri: &str) -> Option<RepoPath> {
    let trimmed = uri.trim();
    if trimmed.is_empty() {
        return None;
    }
    let without_scheme = trimmed
        .strip_prefix("file://")
        .map(|rest| rest.trim_start_matches('/'))
        .unwrap_or(trimmed);
    RepoPath::parse(without_scheme).ok()
}

fn tool_label(run: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    let driver = run
        .get("tool")
        .and_then(|item| item.get("driver"))
        .and_then(|item| item.as_object())?;
    let name = driver.get("name").and_then(|item| item.as_str())?;
    let name = sanitize_ident(name);
    if name.is_empty() {
        return None;
    }
    match driver
        .get("version")
        .and_then(|item| item.as_str())
        .map(sanitize_ident)
    {
        Some(version) if !version.is_empty() => Some(format!("{name}/{version}")),
        _ => Some(name),
    }
}

fn rule_remediations(run: &serde_json::Map<String, serde_json::Value>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Some(rules) = run
        .get("tool")
        .and_then(|item| item.get("driver"))
        .and_then(|item| item.get("rules"))
        .and_then(|item| item.as_array())
    else {
        return out;
    };
    for rule in rules {
        let Some(rule) = rule.as_object() else {
            continue;
        };
        let Some(id) = rule.get("id").and_then(|item| item.as_str()) else {
            continue;
        };
        let id = sanitize_rule_id(id);
        if id.is_empty() {
            continue;
        }
        let help = rule
            .get("help")
            .and_then(|item| item.get("text"))
            .and_then(|item| item.as_str())
            .or_else(|| {
                rule.get("shortDescription")
                    .and_then(|item| item.get("text"))
                    .and_then(|item| item.as_str())
            })
            .map(sanitize_text)
            .filter(|item| !item.is_empty());
        if let Some(help) = help {
            out.insert(id, help);
        }
    }
    out
}

fn category_of(kind: ExternalScannerKind) -> ExternalFindingCategory {
    match kind {
        ExternalScannerKind::Sast => ExternalFindingCategory::Sast,
        ExternalScannerKind::Sca => ExternalFindingCategory::Sca,
        ExternalScannerKind::Container => ExternalFindingCategory::Container,
    }
}

fn map_severity(level: Option<&str>, kind: ExternalScannerKind) -> ExternalFindingSeverity {
    match level {
        Some("error")
            if matches!(
                kind,
                ExternalScannerKind::Sca | ExternalScannerKind::Container
            ) =>
        {
            ExternalFindingSeverity::Critical
        }
        Some("error") => ExternalFindingSeverity::High,
        Some("warning") => ExternalFindingSeverity::Medium,
        Some("note") | Some("none") => ExternalFindingSeverity::Low,
        _ => ExternalFindingSeverity::Medium,
    }
}

fn map_rank(rank: f64) -> ExternalFindingConfidence {
    if rank >= 0.8 {
        ExternalFindingConfidence::High
    } else if rank >= 0.4 {
        ExternalFindingConfidence::Medium
    } else {
        ExternalFindingConfidence::Low
    }
}

fn default_remediation(kind: ExternalScannerKind) -> &'static str {
    match kind {
        ExternalScannerKind::Sast => {
            "Review the reported code location and remediate the SAST rule before apply."
        }
        ExternalScannerKind::Sca => {
            "Pin or replace the reported dependency and re-run the SCA scanner."
        }
        ExternalScannerKind::Container => {
            "Rebuild the image without the reported package or configuration weakness."
        }
    }
}

fn validate_scanner_id(id: &str) -> Result<(), ExternalScanError> {
    if id.is_empty() {
        return Err(ExternalScanError::EmptyScannerId);
    }
    if id.len() > MAX_SCANNER_ID_BYTES {
        return Err(ExternalScanError::BoundExceeded {
            limit: MAX_SCANNER_ID_BYTES,
            requested: id.len(),
        });
    }
    if !id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
    {
        return Err(ExternalScanError::InvalidScannerId);
    }
    Ok(())
}

fn validate_argv(argv: &[String]) -> Result<(), ExternalScanError> {
    if argv.is_empty() || argv.iter().any(|arg| arg.is_empty()) {
        return Err(ExternalScanError::EmptyArgv);
    }
    if argv.len() > MAX_SCANNER_ARGV {
        return Err(ExternalScanError::BoundExceeded {
            limit: MAX_SCANNER_ARGV,
            requested: argv.len(),
        });
    }
    for arg in argv {
        if arg.len() > MAX_SCANNER_ARG_BYTES {
            return Err(ExternalScanError::BoundExceeded {
                limit: MAX_SCANNER_ARG_BYTES,
                requested: arg.len(),
            });
        }
        if arg.contains('\0') || arg.chars().any(char::is_control) {
            return Err(ExternalScanError::EmptyArgv);
        }
    }
    Ok(())
}

fn validate_timeout(timeout: Duration) -> Result<(), ExternalScanError> {
    if timeout.is_zero() || timeout > MAX_TIMEOUT {
        return Err(ExternalScanError::InvalidTimeout);
    }
    Ok(())
}

fn validate_output_limit(output_limit: u64) -> Result<(), ExternalScanError> {
    if output_limit == 0 || output_limit > MAX_SARIF_BYTES as u64 {
        return Err(ExternalScanError::InvalidOutputLimit);
    }
    Ok(())
}

fn map_sandbox_error(err: SandboxError) -> ExternalScanError {
    match err {
        SandboxError::Cancelled => ExternalScanError::Cancelled,
        SandboxError::ForbiddenMount => ExternalScanError::ForbiddenMount,
        SandboxError::TierUnavailable
        | SandboxError::HealthFailed
        | SandboxError::UnknownHandle
        | SandboxError::DuplicateBackend => ExternalScanError::Unavailable,
        SandboxError::TimeoutInvalid => ExternalScanError::InvalidTimeout,
        SandboxError::OutputLimitInvalid => ExternalScanError::InvalidOutputLimit,
        SandboxError::TooManyMounts
        | SandboxError::TooManyEnvNames
        | SandboxError::TooManyArgs
        | SandboxError::TooLong
        | SandboxError::ResourceLimit => ExternalScanError::BoundExceeded {
            limit: 0,
            requested: 1,
        },
        SandboxError::Empty
        | SandboxError::Nul
        | SandboxError::Control
        | SandboxError::InvalidEnvName
        | SandboxError::InvalidSpec
        | SandboxError::UnsupportedNetwork
        | SandboxError::UnsupportedMount
        | SandboxError::LeaseInvalid => ExternalScanError::InvalidWorkspace,
    }
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), ExternalScanError> {
    if cancel.is_cancelled() {
        Err(ExternalScanError::Cancelled)
    } else {
        Ok(())
    }
}

fn sanitize_rule_id(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.chars() {
        if out.len() >= MAX_RULE_ID_BYTES {
            break;
        }
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | '/') {
            out.push(ch);
        }
    }
    out
}

fn sanitize_ident(raw: &str) -> String {
    sanitize_text(raw)
}

fn sanitize_text(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.chars() {
        if out.len() >= MAX_MESSAGE_BYTES {
            break;
        }
        if ch.is_control() {
            continue;
        }
        out.push(ch);
    }
    if out.len() > MAX_REMEDIATION_BYTES {
        out.truncate(MAX_REMEDIATION_BYTES);
    }
    out
}

fn write_hex_lower(bytes: &[u8], out: &mut [u8]) {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    for (i, byte) in bytes.iter().copied().enumerate() {
        let at = i * 2;
        if at + 1 < out.len() {
            out[at] = TABLE[(byte >> 4) as usize];
            out[at + 1] = TABLE[(byte & 0x0f) as usize];
        }
    }
}

fn wipe(buf: &mut [u8]) {
    for byte in buf.iter_mut() {
        *byte = 0;
    }
    compiler_fence(Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    const CANARY: &str = "canary-secret-PLAINTEXT-do-not-leak-ext-017";

    struct ScriptedExec {
        kind: ExternalExecKind,
        sarif: Vec<u8>,
        last_network: Mutex<Option<SandboxNetwork>>,
        last_rw: Mutex<Option<usize>>,
        last_secrets: Mutex<Option<usize>>,
        last_env: Mutex<Option<usize>>,
    }

    impl ScriptedExec {
        fn sarif(kind: ExternalExecKind, body: &str) -> Self {
            Self {
                kind,
                sarif: body.as_bytes().to_vec(),
                last_network: Mutex::new(None),
                last_rw: Mutex::new(None),
                last_secrets: Mutex::new(None),
                last_env: Mutex::new(None),
            }
        }
    }

    impl SupervisedScannerExec for ScriptedExec {
        fn run(
            &self,
            plan: &ExternalSandboxPlan,
            cancel: &CancellationToken,
        ) -> Result<ExternalExecResult, ExternalScanError> {
            check_cancel(cancel)?;
            *self.last_network.lock().expect("network") = Some(plan.spec().network());
            *self.last_rw.lock().expect("rw") = Some(
                plan.spec()
                    .mounts()
                    .iter()
                    .filter(|mount| mount.mode() == MountMode::ReadWrite)
                    .count(),
            );
            *self.last_secrets.lock().expect("secrets") = Some(plan.spec().secrets().len());
            *self.last_env.lock().expect("env") = Some(plan.spec().env_allowlist().len());
            ExternalExecResult::new(self.kind, self.sarif.clone())
        }
    }

    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    fn workspace() -> CanonicalHostPath {
        CanonicalHostPath::from_resolved("/workspace/project").expect("workspace")
    }

    fn config() -> ExternalScannerConfig {
        ExternalScannerConfig::new(
            "semgrep",
            ExternalScannerKind::Sast,
            ["semgrep", "--sarif", "--quiet"],
        )
        .expect("config")
    }

    fn request() -> ExternalScanRequest {
        ExternalScanRequest::new(config(), workspace())
    }

    fn clean_sarif() -> &'static str {
        r#"{"version":"2.1.0","runs":[{"tool":{"driver":{"name":"semgrep","version":"1.0.0"}},"results":[]}]}"#
    }

    fn finding_sarif() -> String {
        r#"{
            "version":"2.1.0",
            "runs":[{
                "tool":{"driver":{"name":"semgrep","version":"1.2.3","rules":[
                    {"id":"no-eval","help":{"text":"Remove eval and use a safe parser."}}
                ]}},
                "results":[{
                    "ruleId":"no-eval",
                    "level":"error",
                    "rank":0.9,
                    "message":{"text":"eval is unsafe"},
                    "locations":[{
                        "physicalLocation":{
                            "artifactLocation":{"uri":"src/app.rs"},
                            "region":{"byteOffset":10,"byteLength":4}
                        }
                    }]
                }]
            }]
        }"#
        .to_owned()
    }

    fn scan_with(exec: &ScriptedExec) -> ExternalScanReport {
        ExternalScannerAdapter::new()
            .scan(&request(), Some(exec), &live())
            .expect("scan")
    }

    #[test]
    fn least_privilege_plan_has_no_network_or_rw_mounts() {
        let adapter = ExternalScannerAdapter::new();
        let plan = adapter.plan(&request()).expect("plan");
        let evidence = plan.evidence();
        assert_eq!(evidence.network(), SandboxNetwork::None);
        assert_eq!(evidence.read_write_mounts(), 0);
        assert_eq!(evidence.secrets(), 0);
        assert_eq!(evidence.env_names(), 0);
        assert_eq!(evidence.read_only_mounts(), 1);
        assert_eq!(evidence.temp_mounts(), 1);
        assert_eq!(evidence.mount_count(), 2);
        assert!(evidence.is_least_privilege());
        assert_eq!(plan.spec().network(), SandboxNetwork::None);
        assert!(plan.spec().secrets().is_empty());
        assert!(plan.spec().env_allowlist().is_empty());
        assert_eq!(plan.spec().cwd().as_str(), WORKSPACE_MOUNT);
        let modes: Vec<MountMode> = plan.spec().mounts().iter().map(|m| m.mode()).collect();
        assert_eq!(modes, vec![MountMode::ReadOnly, MountMode::Temp]);
        assert_eq!(plan.spec().mounts()[0].target().as_str(), WORKSPACE_MOUNT);
        assert_eq!(plan.spec().mounts()[1].target().as_str(), SCAN_OUT_MOUNT);
        assert_eq!(plan.exec().argv(), &["semgrep", "--sarif", "--quiet"]);
    }

    #[test]
    fn runner_receives_the_least_privilege_plan() {
        let exec = ScriptedExec::sarif(ExternalExecKind::Completed { exit_code: 0 }, clean_sarif());
        let report = scan_with(&exec);
        assert_eq!(report.status(), ExternalScanStatus::Passed);
        assert_eq!(
            *exec.last_network.lock().expect("net"),
            Some(SandboxNetwork::None)
        );
        assert_eq!(*exec.last_rw.lock().expect("rw"), Some(0));
        assert_eq!(*exec.last_secrets.lock().expect("sec"), Some(0));
        assert_eq!(*exec.last_env.lock().expect("env"), Some(0));
        assert!(report.sandbox().is_least_privilege());
    }

    #[test]
    fn clean_sarif_roundtrip_is_passed() {
        let report = scan_with(&ScriptedExec::sarif(
            ExternalExecKind::Completed { exit_code: 0 },
            clean_sarif(),
        ));
        assert_eq!(report.status(), ExternalScanStatus::Passed);
        assert!(report.findings().is_empty());
        assert!(report.errors().is_empty());
        assert_eq!(report.coverage().runs(), 1);
        assert_eq!(report.coverage().results(), 0);
        assert!(
            report
                .scanner_versions()
                .iter()
                .any(|item| item == "semgrep/1.0.0")
        );
        assert_eq!(
            report.raw_sarif(),
            Some(ArtifactId::from_bytes(clean_sarif().as_bytes()))
        );
    }

    #[test]
    fn finding_sarif_roundtrip_is_failed_with_normalized_fields() {
        let body = finding_sarif();
        let report = scan_with(&ScriptedExec::sarif(
            ExternalExecKind::Completed { exit_code: 1 },
            &body,
        ));
        assert_eq!(report.status(), ExternalScanStatus::Failed);
        assert_eq!(report.findings().len(), 1);
        let finding = &report.findings()[0];
        assert_eq!(finding.rule_id(), "no-eval");
        assert_eq!(finding.category(), ExternalFindingCategory::Sast);
        assert_eq!(finding.severity(), ExternalFindingSeverity::High);
        assert_eq!(finding.confidence(), ExternalFindingConfidence::High);
        assert_eq!(finding.path().as_str(), "src/app.rs");
        assert_eq!(finding.range().start(), 10);
        assert_eq!(finding.range().end(), 14);
        assert_eq!(finding.message(), "eval is unsafe");
        assert_eq!(finding.remediation(), "Remove eval and use a safe parser.");
        assert_eq!(finding.scanner(), "semgrep");
        assert_eq!(
            finding.evidence_ref(),
            ArtifactId::from_bytes(body.as_bytes())
        );
        assert!(!finding.fingerprint().as_hex().is_empty());
        assert_eq!(finding.id(), finding.fingerprint());
    }

    #[test]
    fn malformed_sarif_is_error_not_pass() {
        for body in [
            "",
            "not-json",
            "{}",
            r#"{"version":"2.1.0"}"#,
            r#"{"version":"2.0.0","runs":[]}"#,
            r#"{"version":2.1,"runs":[]}"#,
            r#"{"version":"2.1.0","runs":{}}"#,
            r#"{"version":"2.1.0","runs":[{"results":{}}]}"#,
            r#"{"version":"2.1.0","runs":[null]}"#,
        ] {
            let report = scan_with(&ScriptedExec::sarif(
                ExternalExecKind::Completed { exit_code: 0 },
                body,
            ));
            assert_eq!(
                report.status(),
                ExternalScanStatus::Error,
                "treated as pass: {body:?}"
            );
            assert_ne!(report.status(), ExternalScanStatus::Passed);
            assert!(
                report
                    .errors().contains(&ExternalScanError::MalformedSarif),
                "missing malformed error for {body:?}"
            );
            if !body.is_empty() {
                assert_eq!(
                    report.raw_sarif(),
                    Some(ArtifactId::from_bytes(body.as_bytes()))
                );
            }
        }
    }

    #[test]
    fn supplied_malformed_artifact_is_error_and_preserves_digest() {
        let garbage = b"{\"version\":\"nope\"}";
        let request = request()
            .with_sarif_bytes(garbage.to_vec())
            .expect("artifact");
        let report = ExternalScannerAdapter::new()
            .scan(&request, None, &live())
            .expect("scan");
        assert_eq!(report.status(), ExternalScanStatus::Error);
        assert_eq!(report.raw_sarif(), Some(ArtifactId::from_bytes(garbage)));
        assert!(report.sandbox().is_least_privilege());
    }

    #[test]
    fn scanner_crash_or_timeout_cannot_yield_pass() {
        let crashed = scan_with(&ScriptedExec::sarif(
            ExternalExecKind::Crashed,
            clean_sarif(),
        ));
        assert_eq!(crashed.status(), ExternalScanStatus::Error);
        assert_ne!(crashed.status(), ExternalScanStatus::Passed);
        assert!(crashed.errors().contains(&ExternalScanError::Crashed));

        let timed_out = scan_with(&ScriptedExec::sarif(
            ExternalExecKind::TimedOut,
            clean_sarif(),
        ));
        assert_eq!(timed_out.status(), ExternalScanStatus::Error);
        assert_ne!(timed_out.status(), ExternalScanStatus::Passed);
        assert!(timed_out.errors().contains(&ExternalScanError::TimedOut));
    }

    #[test]
    fn unavailable_scanner_cannot_yield_pass() {
        let report = scan_with(&ScriptedExec::sarif(
            ExternalExecKind::Unavailable,
            clean_sarif(),
        ));
        assert_eq!(report.status(), ExternalScanStatus::Unavailable);
        assert_ne!(report.status(), ExternalScanStatus::Passed);

        let missing = ExternalScannerAdapter::new()
            .scan(&request(), None, &live())
            .expect("scan");
        assert_eq!(missing.status(), ExternalScanStatus::Unavailable);
        assert_ne!(missing.status(), ExternalScanStatus::Passed);
    }

    #[test]
    fn nonzero_exit_without_findings_is_error_not_pass() {
        let report = scan_with(&ScriptedExec::sarif(
            ExternalExecKind::Completed { exit_code: 2 },
            clean_sarif(),
        ));
        assert_eq!(report.status(), ExternalScanStatus::Error);
        assert_ne!(report.status(), ExternalScanStatus::Passed);
    }

    #[test]
    fn cancellation_is_typed_and_not_pass() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = ExternalScannerAdapter::new()
            .scan(&request(), None, &cancel)
            .expect_err("cancelled");
        assert_eq!(err, ExternalScanError::Cancelled);
    }

    #[test]
    fn oversized_sarif_artifact_fails_closed() {
        let err = request()
            .with_sarif_bytes(vec![b'{'; MAX_SARIF_BYTES + 1])
            .expect_err("bound");
        assert_eq!(
            err,
            ExternalScanError::BoundExceeded {
                limit: MAX_SARIF_BYTES,
                requested: MAX_SARIF_BYTES + 1,
            }
        );
    }

    #[test]
    fn traversal_uri_is_data_not_authority_and_does_not_pass() {
        let body = r#"{
            "version":"2.1.0",
            "runs":[{
                "tool":{"driver":{"name":"semgrep"}},
                "results":[{
                    "ruleId":"escape",
                    "level":"error",
                    "message":{"text":"../etc/passwd"},
                    "locations":[{
                        "physicalLocation":{
                            "artifactLocation":{"uri":"../../etc/passwd"}
                        }
                    }]
                }]
            }]
        }"#;
        let report = scan_with(&ScriptedExec::sarif(
            ExternalExecKind::Completed { exit_code: 0 },
            body,
        ));
        assert_eq!(report.status(), ExternalScanStatus::Failed);
        assert_eq!(report.findings()[0].path().as_str(), UNRESOLVED_PATH);
    }

    #[test]
    fn attacker_strings_are_data_and_control_chars_are_stripped() {
        let body = format!(
            r#"{{
                "version":"2.1.0",
                "runs":[{{
                    "results":[{{
                        "ruleId":"x",
                        "message":{{"text":"leak {CANARY} \u001b[31mOSC"}}
                    }}]
                }}]
            }}"#
        );
        let report = scan_with(&ScriptedExec::sarif(
            ExternalExecKind::Completed { exit_code: 0 },
            &body,
        ));
        assert_eq!(report.status(), ExternalScanStatus::Failed);
        let finding = &report.findings()[0];
        assert!(!finding.message().contains('\u{1b}'));
        let debug = format!("{report:?}");
        let request_debug = format!(
            "{:?}",
            request().with_sarif_bytes(body.into_bytes()).unwrap()
        );
        assert!(
            !request_debug.contains(CANARY),
            "request debug leaked canary"
        );
        assert!(
            !debug.contains("\u{1b}"),
            "report debug kept control sequence"
        );
    }

    #[test]
    fn empty_or_invalid_config_fails_closed() {
        assert_eq!(
            ExternalScannerConfig::new("", ExternalScannerKind::Sast, ["semgrep"]).expect_err("id"),
            ExternalScanError::EmptyScannerId
        );
        assert_eq!(
            ExternalScannerConfig::new("sem grep", ExternalScannerKind::Sast, ["semgrep"])
                .expect_err("id"),
            ExternalScanError::InvalidScannerId
        );
        assert_eq!(
            ExternalScannerConfig::new("semgrep", ExternalScannerKind::Sca, Vec::<String>::new())
                .expect_err("argv"),
            ExternalScanError::EmptyArgv
        );
        assert_eq!(
            config().with_timeout(Duration::ZERO).expect_err("timeout"),
            ExternalScanError::InvalidTimeout
        );
        assert_eq!(
            config().with_output_limit(0).expect_err("output"),
            ExternalScanError::InvalidOutputLimit
        );
    }

    #[test]
    fn docker_socket_workspace_is_forbidden() {
        let request = ExternalScanRequest::new(
            config(),
            CanonicalHostPath::from_resolved("/var/run/docker.sock").expect("sock"),
        );
        let err = ExternalScannerAdapter::new()
            .plan(&request)
            .expect_err("forbidden");
        assert_eq!(err, ExternalScanError::ForbiddenMount);
    }

    #[test]
    fn error_display_does_not_echo_sarif() {
        let rendered = ExternalScanError::MalformedSarif.to_string();
        assert!(!rendered.contains("version"));
        assert!(!rendered.contains(CANARY));
        assert_eq!(
            ExternalScanError::MalformedSarif.code(),
            "security.external_scan_malformed_sarif"
        );
        assert!(!ExternalScanError::MalformedSarif.retryable());
    }
}
