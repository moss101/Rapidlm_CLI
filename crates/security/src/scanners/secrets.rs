//! Native secret scanner for staged diffs and artifacts.
//!
//! Detects high-confidence credential formats and configured canaries.
//! Findings omit matched secret bytes. False-positive suppressions are
//! recorded on the report and never apply to canary hits. Threats: `T-012`,
//! `T-001`, `T-014`, `T-TRJ-01`.

use std::fmt::{self, Debug, Display, Formatter};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering, compiler_fence};

use auth::MAX_SECRET_BYTES;
use protocol::{ArtifactId, RepoPath};
use sha2::{Digest, Sha256};

/// Scanner identity stored on every finding and report.
pub const SCANNER_ID: &str = "rapidlm.native.secrets";

/// Pinned local rule-bundle version. Not a vendor engine version.
pub const SCANNER_VERSION: &str = "1";

/// Maximum staged/artifact targets accepted in one request.
pub const MAX_SCAN_TARGETS: usize = 4096;

/// Maximum bytes accepted on one target.
pub const MAX_TARGET_BYTES: usize = 8 * 1024 * 1024;

/// Maximum combined target bytes accepted in one request.
pub const MAX_SCAN_BYTES: usize = 32 * 1024 * 1024;

/// Maximum findings retained after suppression.
pub const MAX_FINDINGS: usize = 1024;

/// Maximum configured canaries on one scanner.
pub const MAX_CANARIES: usize = 1024;

/// Maximum request suppressions accepted in one request.
pub const MAX_SUPPRESSIONS: usize = 256;

/// Maximum UTF-8 bytes accepted in a suppression reason.
pub const MAX_SUPPRESSION_REASON_BYTES: usize = 512;

const MAX_RULE_ID_BYTES: usize = 64;
const FINGERPRINT_HEX_LEN: usize = 16;
const CANCEL_STRIDE: usize = 4096;
const TAG_FINDING: &[u8] = b"rapidlm.secret_finding.v1";
const TAG_CANARY: &[u8] = b"rapidlm.secret_canary.v1";
const RULE_CANARY: &str = "secrets.canary";

/// Cooperative cancellation for register/scan loops.
#[derive(Clone, Debug)]
pub struct ScanCancellation {
    cancelled: Arc<AtomicBool>,
}

/// Configured canary identity. Hex only; never the canary bytes.
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct CanaryFingerprint {
    hex: [u8; FINGERPRINT_HEX_LEN],
}

/// Stable finding identity. Hex of a digest over rule/path/range/match-hash.
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct FindingFingerprint {
    hex: [u8; FINGERPRINT_HEX_LEN],
}

/// Half-open byte range within one target. `[start, end)`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ByteRange {
    start: u64,
    end: u64,
}

/// How the scanned bytes were produced.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ScanTargetKind {
    StagedDiff,
    Artifact,
}

/// One staged diff hunk or artifact payload. Content is omitted from Debug.
pub struct ScanTarget {
    path: RepoPath,
    kind: ScanTargetKind,
    artifact_id: Option<ArtifactId>,
    content: Vec<u8>,
}

/// Origin of an applied suppression. Built-in rules are deterministic.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum SuppressionSource {
    BuiltIn,
    Request,
}

/// Explicit, reason-bearing suppression. Empty reasons are rejected.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Suppression {
    rule_id: String,
    path: Option<RepoPath>,
    fingerprint: Option<FindingFingerprint>,
    reason: String,
}

/// Record that a hit was withheld. Never includes matched secret bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppliedSuppression {
    rule_id: String,
    path: RepoPath,
    fingerprint: FindingFingerprint,
    source: SuppressionSource,
    reason: String,
}

/// Finding category for the SARIF-compatible internal model.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum FindingCategory {
    Secret,
}

/// Finding severity. Canaries and private keys are critical.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub enum FindingSeverity {
    Low,
    Medium,
    High,
    Critical,
}

/// Detector confidence. Format matches are high; entropy heuristics medium.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub enum FindingConfidence {
    Low,
    Medium,
    High,
}

/// Normalized finding. Matched secret bytes are never a field.
#[derive(Clone, Eq, PartialEq)]
pub struct Finding {
    id: FindingFingerprint,
    rule_id: String,
    category: FindingCategory,
    severity: FindingSeverity,
    confidence: FindingConfidence,
    path: RepoPath,
    range: ByteRange,
    fingerprint: FindingFingerprint,
    message: String,
    remediation: String,
    scanner: &'static str,
    evidence_ref: ArtifactId,
    canary: Option<CanaryFingerprint>,
}

/// Outcome of a completed scan. Error/unavailable never become [`ScanStatus::Clean`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ScanStatus {
    Clean,
    Findings,
    Error,
    Partial,
}

/// Bounded coverage counters. Labels never include secret material.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ScanCoverage {
    targets: usize,
    bytes_scanned: usize,
    rules: usize,
    canaries: usize,
    suppressed: usize,
}

/// Typed scanner failure. Display never includes secret material.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScanError {
    Cancelled,
    BoundExceeded { limit: usize, requested: usize },
    EmptyCanary,
    EmptyRuleId,
    EmptyReason,
    InvalidRuleId,
    TooManyCanaries { limit: usize },
    TooManyTargets { limit: usize },
    TooManySuppressions { limit: usize },
    Unavailable,
}

/// Request: staged/artifact targets plus auditable suppressions.
pub struct ScanRequest {
    targets: Vec<ScanTarget>,
    suppressions: Vec<Suppression>,
    total_bytes: usize,
}

/// Normalized report. Debug omits target bytes and matched secrets.
#[derive(Clone, Eq, PartialEq)]
pub struct ScanReport {
    status: ScanStatus,
    findings: Vec<Finding>,
    coverage: ScanCoverage,
    scanner_versions: Vec<String>,
    errors: Vec<ScanError>,
    applied_suppressions: Vec<AppliedSuppression>,
}

/// Native secret scanner. Holds configured canaries only.
pub struct SecretScanner {
    canaries: Vec<CanaryNeedle>,
}

struct CanaryNeedle {
    fingerprint: CanaryFingerprint,
    bytes: Vec<u8>,
}

struct RawHit {
    rule_id: &'static str,
    severity: FindingSeverity,
    confidence: FindingConfidence,
    start: usize,
    end: usize,
    message: &'static str,
    remediation: &'static str,
    canary: Option<CanaryFingerprint>,
}

impl ScanCancellation {
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    fn check(&self) -> Result<(), ScanError> {
        if self.is_cancelled() {
            Err(ScanError::Cancelled)
        } else {
            Ok(())
        }
    }
}

impl Default for ScanCancellation {
    fn default() -> Self {
        Self::new()
    }
}

impl CanaryFingerprint {
    fn from_bytes(bytes: &[u8]) -> Self {
        Self {
            hex: digest_hex_prefix(TAG_CANARY, bytes),
        }
    }

    pub fn as_hex(&self) -> &str {
        std::str::from_utf8(&self.hex).unwrap_or("????????????????")
    }
}

impl Display for CanaryFingerprint {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_hex())
    }
}

impl Debug for CanaryFingerprint {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CanaryFingerprint")
            .field(&self.as_hex())
            .finish()
    }
}

impl FindingFingerprint {
    fn compute(
        rule_id: &str,
        path: &RepoPath,
        start: u64,
        end: u64,
        match_digest: &[u8; 32],
    ) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(TAG_FINDING);
        hasher.update(rule_id.as_bytes());
        hasher.update([0u8]);
        hasher.update(path.as_str().as_bytes());
        hasher.update([0u8]);
        hasher.update(start.to_be_bytes());
        hasher.update(end.to_be_bytes());
        hasher.update(match_digest);
        let digest = hasher.finalize();
        let mut hex = [0u8; FINGERPRINT_HEX_LEN];
        write_hex_lower(&digest[..8], &mut hex);
        Self { hex }
    }

    pub fn as_hex(&self) -> &str {
        std::str::from_utf8(&self.hex).unwrap_or("????????????????")
    }
}

impl Display for FindingFingerprint {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_hex())
    }
}

impl Debug for FindingFingerprint {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_tuple("FindingFingerprint")
            .field(&self.as_hex())
            .finish()
    }
}

impl ByteRange {
    pub const fn new(start: u64, end: u64) -> Result<Self, ScanError> {
        if start > end {
            return Err(ScanError::Unavailable);
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

impl ScanTarget {
    pub fn staged_diff(path: RepoPath, content: impl Into<Vec<u8>>) -> Result<Self, ScanError> {
        Self::from_parts(path, ScanTargetKind::StagedDiff, None, content.into())
    }

    pub fn artifact(
        path: RepoPath,
        artifact_id: ArtifactId,
        content: impl Into<Vec<u8>>,
    ) -> Result<Self, ScanError> {
        Self::from_parts(
            path,
            ScanTargetKind::Artifact,
            Some(artifact_id),
            content.into(),
        )
    }

    fn from_parts(
        path: RepoPath,
        kind: ScanTargetKind,
        artifact_id: Option<ArtifactId>,
        mut content: Vec<u8>,
    ) -> Result<Self, ScanError> {
        if content.len() > MAX_TARGET_BYTES {
            let requested = content.len();
            wipe(&mut content);
            return Err(ScanError::BoundExceeded {
                limit: MAX_TARGET_BYTES,
                requested,
            });
        }
        Ok(Self {
            path,
            kind,
            artifact_id,
            content,
        })
    }

    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    pub fn kind(&self) -> ScanTargetKind {
        self.kind
    }

    pub fn artifact_id(&self) -> Option<ArtifactId> {
        self.artifact_id
    }

    pub fn len(&self) -> usize {
        self.content.len()
    }

    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }
}

impl Debug for ScanTarget {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScanTarget")
            .field("path", &self.path)
            .field("kind", &self.kind)
            .field("artifact_id", &self.artifact_id)
            .field("bytes", &self.content.len())
            .finish()
    }
}

impl Drop for ScanTarget {
    fn drop(&mut self) {
        wipe(&mut self.content);
        self.content.clear();
    }
}

impl Suppression {
    pub fn new(rule_id: impl Into<String>, reason: impl Into<String>) -> Result<Self, ScanError> {
        let rule_id = rule_id.into();
        let reason = reason.into();
        validate_rule_id(&rule_id)?;
        if reason.is_empty() {
            return Err(ScanError::EmptyReason);
        }
        if reason.len() > MAX_SUPPRESSION_REASON_BYTES {
            return Err(ScanError::BoundExceeded {
                limit: MAX_SUPPRESSION_REASON_BYTES,
                requested: reason.len(),
            });
        }
        if reason.chars().any(char::is_control) {
            return Err(ScanError::EmptyReason);
        }
        Ok(Self {
            rule_id,
            path: None,
            fingerprint: None,
            reason,
        })
    }

    pub fn with_path(mut self, path: RepoPath) -> Self {
        self.path = Some(path);
        self
    }

    pub fn with_fingerprint(mut self, fingerprint: FindingFingerprint) -> Self {
        self.fingerprint = Some(fingerprint);
        self
    }

    pub fn rule_id(&self) -> &str {
        &self.rule_id
    }

    pub fn path(&self) -> Option<&RepoPath> {
        self.path.as_ref()
    }

    pub fn fingerprint(&self) -> Option<FindingFingerprint> {
        self.fingerprint
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }

    fn matches(&self, rule_id: &str, path: &RepoPath, fingerprint: FindingFingerprint) -> bool {
        if self.rule_id != rule_id {
            return false;
        }
        if let Some(expected) = &self.path
            && expected != path
        {
            return false;
        }
        if let Some(expected) = self.fingerprint
            && expected != fingerprint
        {
            return false;
        }
        true
    }
}

impl AppliedSuppression {
    pub fn rule_id(&self) -> &str {
        &self.rule_id
    }

    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    pub fn fingerprint(&self) -> FindingFingerprint {
        self.fingerprint
    }

    pub fn source(&self) -> SuppressionSource {
        self.source
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl Finding {
    pub fn id(&self) -> FindingFingerprint {
        self.id
    }

    pub fn rule_id(&self) -> &str {
        &self.rule_id
    }

    pub fn category(&self) -> FindingCategory {
        self.category
    }

    pub fn severity(&self) -> FindingSeverity {
        self.severity
    }

    pub fn confidence(&self) -> FindingConfidence {
        self.confidence
    }

    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    pub fn range(&self) -> ByteRange {
        self.range
    }

    pub fn fingerprint(&self) -> FindingFingerprint {
        self.fingerprint
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn remediation(&self) -> &str {
        &self.remediation
    }

    pub fn scanner(&self) -> &'static str {
        self.scanner
    }

    pub fn evidence_ref(&self) -> ArtifactId {
        self.evidence_ref
    }

    pub fn canary_fingerprint(&self) -> Option<CanaryFingerprint> {
        self.canary
    }
}

impl Debug for Finding {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("Finding")
            .field("rule_id", &self.rule_id)
            .field("severity", &self.severity)
            .field("confidence", &self.confidence)
            .field("path", &self.path)
            .field("range", &self.range)
            .field("fingerprint", &self.fingerprint)
            .field("message", &self.message)
            .field("scanner", &self.scanner)
            .field("evidence_ref", &self.evidence_ref)
            .field("canary", &self.canary)
            .finish()
    }
}

impl Display for Finding {
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

impl ScanCoverage {
    pub fn targets(self) -> usize {
        self.targets
    }

    pub fn bytes_scanned(self) -> usize {
        self.bytes_scanned
    }

    pub fn rules(self) -> usize {
        self.rules
    }

    pub fn canaries(self) -> usize {
        self.canaries
    }

    pub fn suppressed(self) -> usize {
        self.suppressed
    }
}

impl ScanRequest {
    pub fn new() -> Self {
        Self {
            targets: Vec::new(),
            suppressions: Vec::new(),
            total_bytes: 0,
        }
    }

    pub fn push_target(&mut self, target: ScanTarget) -> Result<(), ScanError> {
        if self.targets.len() >= MAX_SCAN_TARGETS {
            return Err(ScanError::TooManyTargets {
                limit: MAX_SCAN_TARGETS,
            });
        }
        let next = self.total_bytes.saturating_add(target.len());
        if next > MAX_SCAN_BYTES {
            return Err(ScanError::BoundExceeded {
                limit: MAX_SCAN_BYTES,
                requested: next,
            });
        }
        self.total_bytes = next;
        self.targets.push(target);
        Ok(())
    }

    pub fn push_suppression(&mut self, suppression: Suppression) -> Result<(), ScanError> {
        if self.suppressions.len() >= MAX_SUPPRESSIONS {
            return Err(ScanError::TooManySuppressions {
                limit: MAX_SUPPRESSIONS,
            });
        }
        self.suppressions.push(suppression);
        Ok(())
    }

    pub fn target_count(&self) -> usize {
        self.targets.len()
    }

    pub fn suppression_count(&self) -> usize {
        self.suppressions.len()
    }
}

impl Default for ScanRequest {
    fn default() -> Self {
        Self::new()
    }
}

impl Debug for ScanRequest {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScanRequest")
            .field("targets", &self.targets.len())
            .field("bytes", &self.total_bytes)
            .field("suppressions", &self.suppressions.len())
            .finish()
    }
}

impl ScanReport {
    pub fn status(&self) -> ScanStatus {
        self.status
    }

    pub fn findings(&self) -> &[Finding] {
        &self.findings
    }

    pub fn coverage(&self) -> ScanCoverage {
        self.coverage
    }

    pub fn scanner_versions(&self) -> &[String] {
        &self.scanner_versions
    }

    pub fn errors(&self) -> &[ScanError] {
        &self.errors
    }

    pub fn applied_suppressions(&self) -> &[AppliedSuppression] {
        &self.applied_suppressions
    }
}

impl Debug for ScanReport {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScanReport")
            .field("status", &self.status)
            .field("findings", &self.findings)
            .field("coverage", &self.coverage)
            .field("scanner_versions", &self.scanner_versions)
            .field("errors", &self.errors)
            .field("applied_suppressions", &self.applied_suppressions)
            .finish()
    }
}

impl SecretScanner {
    pub fn new() -> Self {
        Self {
            canaries: Vec::new(),
        }
    }

    pub fn canary_count(&self) -> usize {
        self.canaries.len()
    }

    /// Register a configured canary. Idempotent on exact bytes.
    ///
    /// Privileged: the caller already holds plaintext (policy/injector).
    /// The returned fingerprint and Debug never include `plaintext`.
    pub fn register_canary(
        &mut self,
        plaintext: &[u8],
        cancel: &ScanCancellation,
    ) -> Result<CanaryFingerprint, ScanError> {
        cancel.check()?;
        if plaintext.is_empty() {
            return Err(ScanError::EmptyCanary);
        }
        if plaintext.len() > MAX_SECRET_BYTES {
            return Err(ScanError::BoundExceeded {
                limit: MAX_SECRET_BYTES,
                requested: plaintext.len(),
            });
        }
        if let Some(existing) = self
            .canaries
            .iter()
            .find(|needle| ct_eq(&needle.bytes, plaintext))
        {
            return Ok(existing.fingerprint);
        }
        if self.canaries.len() >= MAX_CANARIES {
            return Err(ScanError::TooManyCanaries {
                limit: MAX_CANARIES,
            });
        }
        let fingerprint = CanaryFingerprint::from_bytes(plaintext);
        self.canaries.push(CanaryNeedle {
            fingerprint,
            bytes: plaintext.to_vec(),
        });
        Ok(fingerprint)
    }

    /// Scan staged diffs/artifacts. Failures never become [`ScanStatus::Clean`].
    pub fn scan(
        &self,
        request: &ScanRequest,
        cancel: &ScanCancellation,
    ) -> Result<ScanReport, ScanError> {
        cancel.check()?;
        let mut findings = Vec::new();
        let mut applied = Vec::new();
        let mut bytes_scanned = 0usize;

        for (index, target) in request.targets.iter().enumerate() {
            cancel.check()?;
            if index.is_multiple_of(CANCEL_STRIDE) {
                cancel.check()?;
            }
            bytes_scanned = bytes_scanned.saturating_add(target.content.len());
            let evidence = target
                .artifact_id
                .unwrap_or_else(|| ArtifactId::from_bytes(&target.content));
            let hits = scan_target(target, &self.canaries, cancel)?;
            if findings.len().saturating_add(hits.len()) > MAX_FINDINGS {
                return Err(ScanError::BoundExceeded {
                    limit: MAX_FINDINGS,
                    requested: findings.len().saturating_add(hits.len()),
                });
            }
            for hit in hits {
                let finding = materialize_finding(target, evidence, &hit);
                if let Some(suppression) =
                    decide_suppression(&finding, &hit, &request.suppressions, &target.content)
                {
                    applied.push(suppression);
                    continue;
                }
                findings.push(finding);
            }
        }

        let status = if findings.is_empty() {
            ScanStatus::Clean
        } else {
            ScanStatus::Findings
        };
        Ok(ScanReport {
            status,
            findings,
            coverage: ScanCoverage {
                targets: request.targets.len(),
                bytes_scanned,
                rules: RULE_COUNT,
                canaries: self.canaries.len(),
                suppressed: applied.len(),
            },
            scanner_versions: vec![format!("{SCANNER_ID}/{SCANNER_VERSION}")],
            errors: Vec::new(),
            applied_suppressions: applied,
        })
    }
}

impl Default for SecretScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl Debug for SecretScanner {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretScanner")
            .field("canaries", &self.canaries.len())
            .finish()
    }
}

impl Drop for SecretScanner {
    fn drop(&mut self) {
        for needle in &mut self.canaries {
            wipe(&mut needle.bytes);
            needle.bytes.clear();
        }
        self.canaries.clear();
    }
}

impl Drop for CanaryNeedle {
    fn drop(&mut self) {
        wipe(&mut self.bytes);
        self.bytes.clear();
    }
}

impl ScanError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Cancelled => "security.secret_scan_cancelled",
            Self::BoundExceeded { .. } => "security.secret_scan_bound_exceeded",
            Self::EmptyCanary => "security.secret_scan_empty_canary",
            Self::EmptyRuleId => "security.secret_scan_empty_rule",
            Self::EmptyReason => "security.secret_scan_empty_reason",
            Self::InvalidRuleId => "security.secret_scan_invalid_rule",
            Self::TooManyCanaries { .. } => "security.secret_scan_canary_capacity",
            Self::TooManyTargets { .. } => "security.secret_scan_target_capacity",
            Self::TooManySuppressions { .. } => "security.secret_scan_suppression_capacity",
            Self::Unavailable => "security.secret_scan_unavailable",
        }
    }

    pub fn retryable(&self) -> bool {
        false
    }
}

impl Display for ScanError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("secret scan was cancelled"),
            Self::BoundExceeded { limit, requested } => {
                write!(
                    f,
                    "secret scan exceeds bound ({requested} > {limit} bytes or items)"
                )
            }
            Self::EmptyCanary => f.write_str("canary value must not be empty"),
            Self::EmptyRuleId => f.write_str("suppression rule id must not be empty"),
            Self::EmptyReason => f.write_str("suppression reason must be explicit"),
            Self::InvalidRuleId => f.write_str("suppression rule id is invalid"),
            Self::TooManyCanaries { limit } => {
                write!(f, "secret scanner canary registry is at capacity ({limit})")
            }
            Self::TooManyTargets { limit } => {
                write!(f, "secret scan target list is at capacity ({limit})")
            }
            Self::TooManySuppressions { limit } => {
                write!(f, "secret scan suppression list is at capacity ({limit})")
            }
            Self::Unavailable => f.write_str("secret scanner is unavailable"),
        }
    }
}

impl std::error::Error for ScanError {}

const REMEDIATE_ROTATE: &str =
    "Remove the credential from the staged change, rotate it, and store it in a secret manager.";
const REMEDIATE_CANARY: &str = "Treat the canary as a confirmed leak: rotate the associated secret and purge the staged change.";

const RULE_COUNT: usize = 14;

fn scan_target(
    target: &ScanTarget,
    canaries: &[CanaryNeedle],
    cancel: &ScanCancellation,
) -> Result<Vec<RawHit>, ScanError> {
    let hay = target.content.as_slice();
    let mut hits = Vec::new();
    collect_canaries(hay, canaries, &mut hits, cancel)?;
    collect_private_keys(hay, &mut hits, cancel)?;
    collect_prefixed_tokens(hay, &mut hits, cancel)?;
    collect_aws_secret_assignments(hay, &mut hits, cancel)?;
    collect_connection_strings(hay, &mut hits, cancel)?;
    collect_high_entropy_assignments(hay, &mut hits, cancel)?;
    dedupe_hits(&mut hits);
    Ok(hits)
}

fn collect_canaries(
    hay: &[u8],
    canaries: &[CanaryNeedle],
    hits: &mut Vec<RawHit>,
    cancel: &ScanCancellation,
) -> Result<(), ScanError> {
    if canaries.is_empty() {
        return Ok(());
    }
    let mut pos = 0;
    while pos < hay.len() {
        cancel.check()?;
        if pos.is_multiple_of(CANCEL_STRIDE) {
            cancel.check()?;
        }
        let mut best: Option<(usize, CanaryFingerprint)> = None;
        for needle in canaries {
            let nlen = needle.bytes.len();
            if nlen == 0 || pos + nlen > hay.len() {
                continue;
            }
            if &hay[pos..pos + nlen] == needle.bytes.as_slice() {
                match best {
                    Some((best_end, _)) if best_end >= pos + nlen => {}
                    _ => best = Some((pos + nlen, needle.fingerprint)),
                }
            }
        }
        if let Some((end, fingerprint)) = best {
            hits.push(RawHit {
                rule_id: RULE_CANARY,
                severity: FindingSeverity::Critical,
                confidence: FindingConfidence::High,
                start: pos,
                end,
                message: "Configured secret canary detected in staged content",
                remediation: REMEDIATE_CANARY,
                canary: Some(fingerprint),
            });
            pos = end;
        } else {
            pos += 1;
        }
    }
    Ok(())
}

fn collect_private_keys(
    hay: &[u8],
    hits: &mut Vec<RawHit>,
    cancel: &ScanCancellation,
) -> Result<(), ScanError> {
    const BEGIN: &[u8] = b"-----BEGIN ";
    const PRIVATE: &[u8] = b"PRIVATE KEY-----";
    let mut pos = 0;
    while let Some(rel) = find_bytes(&hay[pos..], BEGIN) {
        cancel.check()?;
        let start = pos + rel;
        let after = start + BEGIN.len();
        let window_end = hay.len().min(after.saturating_add(48));
        if let Some(priv_rel) = find_bytes(&hay[after..window_end], PRIVATE) {
            let end = after + priv_rel + PRIVATE.len();
            hits.push(RawHit {
                rule_id: "secrets.private_key",
                severity: FindingSeverity::Critical,
                confidence: FindingConfidence::High,
                start,
                end,
                message: "PEM/OpenSSH private key header detected in staged content",
                remediation: REMEDIATE_ROTATE,
                canary: None,
            });
            pos = end;
        } else {
            pos = after;
        }
    }
    Ok(())
}

fn collect_prefixed_tokens(
    hay: &[u8],
    hits: &mut Vec<RawHit>,
    cancel: &ScanCancellation,
) -> Result<(), ScanError> {
    for spec in PREFIXED_RULES {
        cancel.check()?;
        let mut pos = 0;
        while let Some(rel) = find_bytes(&hay[pos..], spec.prefix) {
            cancel.check()?;
            let start = pos + rel;
            if !is_left_boundary(hay, start) {
                pos = start + spec.prefix.len();
                continue;
            }
            let body_start = start + spec.prefix.len();
            let mut end = body_start;
            while end < hay.len() && (spec.body)(hay[end]) {
                end += 1;
            }
            let body_len = end - body_start;
            if body_len >= spec.min_body && body_len <= spec.max_body && is_right_boundary(hay, end)
            {
                hits.push(RawHit {
                    rule_id: spec.rule_id,
                    severity: spec.severity,
                    confidence: FindingConfidence::High,
                    start,
                    end,
                    message: spec.message,
                    remediation: REMEDIATE_ROTATE,
                    canary: None,
                });
                pos = end;
            } else {
                pos = start + spec.prefix.len();
            }
        }
    }
    Ok(())
}

fn collect_aws_secret_assignments(
    hay: &[u8],
    hits: &mut Vec<RawHit>,
    cancel: &ScanCancellation,
) -> Result<(), ScanError> {
    const KEYS: &[&[u8]] = &[
        b"aws_secret_access_key",
        b"AWS_SECRET_ACCESS_KEY",
        b"awsSecretAccessKey",
    ];
    for key in KEYS {
        cancel.check()?;
        let mut pos = 0;
        while let Some(rel) = find_bytes(&hay[pos..], key) {
            let key_at = pos + rel;
            if !is_left_boundary(hay, key_at) {
                pos = key_at + key.len();
                continue;
            }
            let after_key = key_at + key.len();
            if let Some((start, end)) = read_assigned_value(hay, after_key, 40, 40, is_aws_secret) {
                hits.push(RawHit {
                    rule_id: "secrets.aws_secret_access_key",
                    severity: FindingSeverity::High,
                    confidence: FindingConfidence::High,
                    start,
                    end,
                    message: "AWS secret access key assignment detected in staged content",
                    remediation: REMEDIATE_ROTATE,
                    canary: None,
                });
                pos = end;
            } else {
                pos = after_key;
            }
        }
    }
    Ok(())
}

fn collect_connection_strings(
    hay: &[u8],
    hits: &mut Vec<RawHit>,
    cancel: &ScanCancellation,
) -> Result<(), ScanError> {
    const SCHEMES: &[&[u8]] = &[
        b"postgres://",
        b"postgresql://",
        b"mysql://",
        b"mongodb://",
        b"redis://",
        b"amqp://",
        b"amqps://",
    ];
    for scheme in SCHEMES {
        cancel.check()?;
        let mut pos = 0;
        while let Some(rel) = find_bytes(&hay[pos..], scheme) {
            let start = pos + rel;
            if !is_left_boundary(hay, start) {
                pos = start + scheme.len();
                continue;
            }
            if let Some(end) = read_userinfo_url(hay, start + scheme.len()) {
                hits.push(RawHit {
                    rule_id: "secrets.connection_string",
                    severity: FindingSeverity::High,
                    confidence: FindingConfidence::High,
                    start,
                    end,
                    message: "Connection string with embedded credentials detected",
                    remediation: REMEDIATE_ROTATE,
                    canary: None,
                });
                pos = end;
            } else {
                pos = start + scheme.len();
            }
        }
    }
    Ok(())
}

fn collect_high_entropy_assignments(
    hay: &[u8],
    hits: &mut Vec<RawHit>,
    cancel: &ScanCancellation,
) -> Result<(), ScanError> {
    const KEYS: &[&[u8]] = &[
        b"password",
        b"passwd",
        b"secret",
        b"token",
        b"api_key",
        b"api-key",
        b"apikey",
        b"access_key",
        b"access-key",
        b"private_key",
        b"client_secret",
        b"client-secret",
        b"auth_token",
        b"credential",
        b"credentials",
        b"bearer",
    ];
    for key in KEYS {
        cancel.check()?;
        let mut pos = 0;
        while let Some(rel) = find_ignore_ascii_case(&hay[pos..], key) {
            cancel.check()?;
            let key_at = pos + rel;
            if !is_left_boundary(hay, key_at) || !is_right_boundary(hay, key_at + key.len()) {
                pos = key_at + key.len();
                continue;
            }
            let after_key = key_at + key.len();
            if let Some((start, end)) =
                read_assigned_value(hay, after_key, 20, 256, is_assignment_value)
            {
                let value = &hay[start..end];
                if unique_bytes(value) >= 10 && shannon_entropy(value) >= 3.5 {
                    hits.push(RawHit {
                        rule_id: "secrets.high_entropy_assignment",
                        severity: FindingSeverity::Medium,
                        confidence: FindingConfidence::Medium,
                        start,
                        end,
                        message: "High-entropy secret assignment detected in staged content",
                        remediation: REMEDIATE_ROTATE,
                        canary: None,
                    });
                    pos = end;
                    continue;
                }
            }
            pos = after_key;
        }
    }
    Ok(())
}

struct PrefixRule {
    rule_id: &'static str,
    prefix: &'static [u8],
    min_body: usize,
    max_body: usize,
    body: fn(u8) -> bool,
    severity: FindingSeverity,
    message: &'static str,
}

const PREFIXED_RULES: &[PrefixRule] = &[
    PrefixRule {
        rule_id: "secrets.aws_access_key_id",
        prefix: b"AKIA",
        min_body: 16,
        max_body: 16,
        body: is_aws_key_body,
        severity: FindingSeverity::High,
        message: "AWS access key id detected in staged content",
    },
    PrefixRule {
        rule_id: "secrets.github_token",
        prefix: b"github_pat_",
        min_body: 22,
        max_body: 255,
        body: is_alnum_underscore,
        severity: FindingSeverity::High,
        message: "GitHub fine-grained personal access token detected",
    },
    PrefixRule {
        rule_id: "secrets.github_token",
        prefix: b"ghp_",
        min_body: 36,
        max_body: 36,
        body: is_alnum,
        severity: FindingSeverity::High,
        message: "GitHub personal access token detected in staged content",
    },
    PrefixRule {
        rule_id: "secrets.github_token",
        prefix: b"gho_",
        min_body: 36,
        max_body: 36,
        body: is_alnum,
        severity: FindingSeverity::High,
        message: "GitHub OAuth access token detected in staged content",
    },
    PrefixRule {
        rule_id: "secrets.github_token",
        prefix: b"ghs_",
        min_body: 36,
        max_body: 36,
        body: is_alnum,
        severity: FindingSeverity::High,
        message: "GitHub server-to-server token detected in staged content",
    },
    PrefixRule {
        rule_id: "secrets.github_token",
        prefix: b"ghu_",
        min_body: 36,
        max_body: 36,
        body: is_alnum,
        severity: FindingSeverity::High,
        message: "GitHub user-to-server token detected in staged content",
    },
    PrefixRule {
        rule_id: "secrets.slack_token",
        prefix: b"xoxb-",
        min_body: 10,
        max_body: 128,
        body: is_slack_body,
        severity: FindingSeverity::High,
        message: "Slack bot token detected in staged content",
    },
    PrefixRule {
        rule_id: "secrets.slack_token",
        prefix: b"xoxp-",
        min_body: 10,
        max_body: 128,
        body: is_slack_body,
        severity: FindingSeverity::High,
        message: "Slack user token detected in staged content",
    },
    PrefixRule {
        rule_id: "secrets.google_api_key",
        prefix: b"AIza",
        min_body: 35,
        max_body: 35,
        body: is_google_body,
        severity: FindingSeverity::High,
        message: "Google API key detected in staged content",
    },
    PrefixRule {
        rule_id: "secrets.stripe_live_key",
        prefix: b"sk_live_",
        min_body: 24,
        max_body: 128,
        body: is_alnum,
        severity: FindingSeverity::High,
        message: "Stripe live secret key detected in staged content",
    },
    PrefixRule {
        rule_id: "secrets.stripe_live_key",
        prefix: b"rk_live_",
        min_body: 24,
        max_body: 128,
        body: is_alnum,
        severity: FindingSeverity::High,
        message: "Stripe live restricted key detected in staged content",
    },
    PrefixRule {
        rule_id: "secrets.anthropic_key",
        prefix: b"sk-ant-",
        min_body: 20,
        max_body: 256,
        body: is_alnum_dash,
        severity: FindingSeverity::High,
        message: "Anthropic API key detected in staged content",
    },
    PrefixRule {
        rule_id: "secrets.openai_key",
        prefix: b"sk-proj-",
        min_body: 20,
        max_body: 256,
        body: is_alnum_dash,
        severity: FindingSeverity::High,
        message: "OpenAI project API key detected in staged content",
    },
    PrefixRule {
        rule_id: "secrets.npm_token",
        prefix: b"npm_",
        min_body: 36,
        max_body: 36,
        body: is_alnum,
        severity: FindingSeverity::High,
        message: "npm access token detected in staged content",
    },
    PrefixRule {
        rule_id: "secrets.pypi_token",
        prefix: b"pypi-",
        min_body: 20,
        max_body: 256,
        body: is_alnum_dash,
        severity: FindingSeverity::High,
        message: "PyPI API token detected in staged content",
    },
    PrefixRule {
        rule_id: "secrets.huggingface_token",
        prefix: b"hf_",
        min_body: 34,
        max_body: 64,
        body: is_alnum,
        severity: FindingSeverity::High,
        message: "Hugging Face access token detected in staged content",
    },
];

fn materialize_finding(target: &ScanTarget, evidence: ArtifactId, hit: &RawHit) -> Finding {
    let start = hit.start as u64;
    let end = hit.end as u64;
    let matched = &target.content[hit.start..hit.end];
    let match_digest = Sha256::digest(matched);
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&match_digest);
    let fingerprint = FindingFingerprint::compute(hit.rule_id, &target.path, start, end, &digest);
    Finding {
        id: fingerprint,
        rule_id: hit.rule_id.to_owned(),
        category: FindingCategory::Secret,
        severity: hit.severity,
        confidence: hit.confidence,
        path: target.path.clone(),
        range: ByteRange { start, end },
        fingerprint,
        message: hit.message.to_owned(),
        remediation: hit.remediation.to_owned(),
        scanner: SCANNER_ID,
        evidence_ref: evidence,
        canary: hit.canary,
    }
}

fn decide_suppression(
    finding: &Finding,
    hit: &RawHit,
    requested: &[Suppression],
    content: &[u8],
) -> Option<AppliedSuppression> {
    if finding.rule_id == RULE_CANARY {
        return None;
    }
    if let Some(reason) = builtin_suppression_reason(&content[hit.start..hit.end]) {
        return Some(AppliedSuppression {
            rule_id: finding.rule_id.clone(),
            path: finding.path.clone(),
            fingerprint: finding.fingerprint,
            source: SuppressionSource::BuiltIn,
            reason: reason.to_owned(),
        });
    }
    for suppression in requested {
        if suppression.matches(&finding.rule_id, &finding.path, finding.fingerprint) {
            return Some(AppliedSuppression {
                rule_id: finding.rule_id.clone(),
                path: finding.path.clone(),
                fingerprint: finding.fingerprint,
                source: SuppressionSource::Request,
                reason: suppression.reason.clone(),
            });
        }
    }
    None
}

fn builtin_suppression_reason(matched: &[u8]) -> Option<&'static str> {
    if matched
        .windows(7)
        .any(|w| w.eq_ignore_ascii_case(b"EXAMPLE"))
    {
        return Some("documentation example credential (contains EXAMPLE)");
    }
    if matched
        .iter()
        .all(|b| matches!(b, b'x' | b'X' | b'*' | b'.' | b'-' | b'_'))
    {
        return Some("repeated placeholder character");
    }
    if starts_with_interpolation(matched) {
        return Some("template interpolation, not a literal secret");
    }
    const PLACEHOLDERS: &[&[u8]] = &[
        b"changeme",
        b"redacted",
        b"placeholder",
        b"your_api_key",
        b"your-api-key",
        b"insert_token",
        b"dummysecret",
    ];
    for needle in PLACEHOLDERS {
        if contains_ignore_ascii_case(matched, needle) {
            return Some("explicit placeholder token");
        }
    }
    None
}

fn is_canary_hit(hit: &RawHit) -> bool {
    hit.canary.is_some() || hit.rule_id == RULE_CANARY
}

fn dedupe_hits(hits: &mut Vec<RawHit>) {
    hits.sort_by(|a, b| {
        a.start
            .cmp(&b.start)
            .then(b.end.cmp(&a.end))
            .then(priority(b).cmp(&priority(a)))
    });
    let mut kept: Vec<RawHit> = Vec::new();
    for hit in hits.drain(..) {
        if is_canary_hit(&hit) {
            // Canaries are never dropped for an earlier-start overlap.
            kept.retain(|existing| !ranges_overlap(existing, &hit) || is_canary_hit(existing));
            kept.push(hit);
            continue;
        }
        let dominated = kept.iter().any(|existing| {
            ranges_overlap(existing, &hit)
                && (is_canary_hit(existing) || priority(existing) >= priority(&hit))
        });
        if dominated {
            continue;
        }
        kept.retain(|existing| {
            !ranges_overlap(existing, &hit) || priority(existing) > priority(&hit)
        });
        kept.push(hit);
    }
    *hits = kept;
}

fn ranges_overlap(a: &RawHit, b: &RawHit) -> bool {
    a.start < b.end && b.start < a.end
}

fn priority(hit: &RawHit) -> u8 {
    let class: u8 = if hit.rule_id == RULE_CANARY {
        4
    } else if hit.rule_id == "secrets.private_key" {
        3
    } else if hit.rule_id == "secrets.high_entropy_assignment" {
        1
    } else {
        2
    };
    class.saturating_mul(4).saturating_add(match hit.severity {
        FindingSeverity::Critical => 3,
        FindingSeverity::High => 2,
        FindingSeverity::Medium => 1,
        FindingSeverity::Low => 0,
    })
}

fn read_assigned_value(
    hay: &[u8],
    after_key: usize,
    min_len: usize,
    max_len: usize,
    pred: fn(u8) -> bool,
) -> Option<(usize, usize)> {
    let mut i = after_key;
    while i < hay.len() && matches!(hay[i], b' ' | b'\t' | b'"' | b'\'') {
        i += 1;
    }
    if i >= hay.len() || !matches!(hay[i], b'=' | b':') {
        return None;
    }
    i += 1;
    while i < hay.len() && matches!(hay[i], b' ' | b'\t') {
        i += 1;
    }
    let quote = if i < hay.len() && matches!(hay[i], b'"' | b'\'') {
        let q = hay[i];
        i += 1;
        Some(q)
    } else {
        None
    };
    let start = i;
    while i < hay.len() && pred(hay[i]) {
        i += 1;
        if i - start > max_len {
            return None;
        }
    }
    let end = i;
    if end - start < min_len || end - start > max_len {
        return None;
    }
    if let Some(q) = quote {
        if end >= hay.len() || hay[end] != q {
            return None;
        }
    } else if !is_right_boundary(hay, end) {
        return None;
    }
    Some((start, end))
}

fn read_userinfo_url(hay: &[u8], after_scheme: usize) -> Option<usize> {
    let mut i = after_scheme;
    if i >= hay.len() || !is_url_user(hay[i]) {
        return None;
    }
    while i < hay.len() && is_url_user(hay[i]) {
        i += 1;
    }
    if i >= hay.len() || hay[i] != b':' {
        return None;
    }
    i += 1;
    let pass_start = i;
    while i < hay.len() && is_url_pass(hay[i]) {
        i += 1;
    }
    if i == pass_start || i >= hay.len() || hay[i] != b'@' {
        return None;
    }
    i += 1;
    if i >= hay.len() || !is_url_host(hay[i]) {
        return None;
    }
    while i < hay.len() && is_url_host(hay[i]) {
        i += 1;
    }
    Some(i)
}

fn validate_rule_id(rule_id: &str) -> Result<(), ScanError> {
    if rule_id.is_empty() {
        return Err(ScanError::EmptyRuleId);
    }
    if rule_id == "*" || rule_id == "all" {
        return Err(ScanError::InvalidRuleId);
    }
    if rule_id.len() > MAX_RULE_ID_BYTES {
        return Err(ScanError::BoundExceeded {
            limit: MAX_RULE_ID_BYTES,
            requested: rule_id.len(),
        });
    }
    if !rule_id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(ScanError::InvalidRuleId);
    }
    Ok(())
}

fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

fn find_ignore_ascii_case(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle))
}

fn contains_ignore_ascii_case(hay: &[u8], needle: &[u8]) -> bool {
    find_ignore_ascii_case(hay, needle).is_some()
}

fn starts_with_interpolation(bytes: &[u8]) -> bool {
    bytes.starts_with(b"${") || bytes.starts_with(b"{{") || bytes.starts_with(b"$(")
}

fn is_left_boundary(hay: &[u8], pos: usize) -> bool {
    pos == 0 || !is_token_char(hay[pos - 1])
}

fn is_right_boundary(hay: &[u8], pos: usize) -> bool {
    pos >= hay.len() || !is_token_char(hay[pos])
}

fn is_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')
}

fn is_alnum(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}

fn is_alnum_underscore(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn is_alnum_dash(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')
}

fn is_aws_key_body(b: u8) -> bool {
    b.is_ascii_uppercase() || b.is_ascii_digit()
}

fn is_aws_secret(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'/' | b'+')
}

fn is_google_body(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')
}

fn is_slack_body(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-'
}

fn is_assignment_value(b: u8) -> bool {
    b.is_ascii_graphic() && !matches!(b, b'"' | b'\'' | b' ' | b',' | b';')
}

fn is_url_user(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-' | b'+' | b'%')
}

fn is_url_pass(b: u8) -> bool {
    b.is_ascii_graphic() && b != b'@' && b != b'/'
}

fn is_url_host(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']')
}

fn unique_bytes(bytes: &[u8]) -> usize {
    let mut seen = [false; 256];
    let mut n = 0;
    for &b in bytes {
        if !seen[b as usize] {
            seen[b as usize] = true;
            n += 1;
        }
    }
    n
}

fn shannon_entropy(bytes: &[u8]) -> f64 {
    if bytes.is_empty() {
        return 0.0;
    }
    let mut counts = [0u32; 256];
    for &b in bytes {
        counts[b as usize] += 1;
    }
    let n = bytes.len() as f64;
    let mut h = 0.0;
    for count in counts {
        if count > 0 {
            let p = f64::from(count) / n;
            h -= p * p.log2();
        }
    }
    h
}

fn digest_hex_prefix(tag: &[u8], bytes: &[u8]) -> [u8; FINGERPRINT_HEX_LEN] {
    let mut hasher = Sha256::new();
    hasher.update(tag);
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut hex = [0u8; FINGERPRINT_HEX_LEN];
    write_hex_lower(&digest[..8], &mut hex);
    hex
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

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc = 0u8;
    for (l, r) in a.iter().zip(b.iter()) {
        acc |= l ^ r;
    }
    acc == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const CANARY: &str = "canary-secret-PLAINTEXT-do-not-leak-scan-014";
    const AWS_LIVE: &str = "AKIAJSIE6T5YJX3ZZZZZ";
    const AWS_DOC: &str = "AKIAIOSFODNN7EXAMPLE";
    const GHP: &str = "ghp_abcdefghijklmnopqrstuvwxyzABCDEFGHIJ";

    fn live() -> ScanCancellation {
        ScanCancellation::new()
    }

    fn path(raw: &str) -> RepoPath {
        RepoPath::parse(raw).expect("repo path")
    }

    fn scan_text(scanner: &SecretScanner, file: &str, text: &str) -> ScanReport {
        let mut request = ScanRequest::new();
        request
            .push_target(
                ScanTarget::staged_diff(path(file), text.as_bytes().to_vec()).expect("target"),
            )
            .expect("push");
        scanner.scan(&request, &live()).expect("scan")
    }

    fn assert_no_canary(label: &str, rendered: &str) {
        assert!(
            !rendered.contains(CANARY),
            "{label} echoed canary: {rendered}"
        );
        assert!(
            !rendered.contains("PLAINTEXT-do-not-leak"),
            "{label} echoed canary fragment: {rendered}"
        );
    }

    fn assert_no_secret(label: &str, rendered: &str, secret: &str) {
        assert!(
            !rendered.contains(secret),
            "{label} echoed secret: {rendered}"
        );
    }

    #[test]
    fn canary_fixture_is_detected_without_echoing_secret() {
        let mut scanner = SecretScanner::new();
        let fingerprint = scanner
            .register_canary(CANARY.as_bytes(), &live())
            .expect("register");
        let report = scan_text(&scanner, "src/config.rs", &format!("token = \"{CANARY}\""));
        assert_eq!(report.status(), ScanStatus::Findings);
        assert_eq!(report.findings().len(), 1);
        let finding = &report.findings()[0];
        assert_eq!(finding.rule_id(), RULE_CANARY);
        assert_eq!(finding.severity(), FindingSeverity::Critical);
        assert_eq!(finding.canary_fingerprint(), Some(fingerprint));
        assert_eq!(finding.path().as_str(), "src/config.rs");
        assert_eq!(finding.scanner(), SCANNER_ID);
        assert_no_canary("message", finding.message());
        assert_no_canary("display", &finding.to_string());
        assert_no_canary("finding debug", &format!("{finding:?}"));
        assert_no_canary("report debug", &format!("{report:?}"));
        assert_no_canary("scanner debug", &format!("{scanner:?}"));
    }

    #[test]
    fn canary_cannot_be_suppressed_by_request_or_placeholder_rules() {
        let mut scanner = SecretScanner::new();
        scanner
            .register_canary(CANARY.as_bytes(), &live())
            .expect("register");
        let mut request = ScanRequest::new();
        request
            .push_target(
                ScanTarget::staged_diff(path("leak.env"), format!("CANARY={CANARY}"))
                    .expect("target"),
            )
            .expect("push target");
        request
            .push_suppression(
                Suppression::new(RULE_CANARY, "ignore canary in fixture")
                    .expect("suppression")
                    .with_path(path("leak.env")),
            )
            .expect("push suppression");
        let report = scanner.scan(&request, &live()).expect("scan");
        assert_eq!(report.status(), ScanStatus::Findings);
        assert_eq!(report.findings()[0].rule_id(), RULE_CANARY);
        assert!(report.applied_suppressions().is_empty());
        assert_no_canary("suppressed report", &format!("{report:?}"));
    }

    #[test]
    fn nested_canary_survives_overlapping_heuristic_and_suppression() {
        let mut scanner = SecretScanner::new();
        scanner
            .register_canary(CANARY.as_bytes(), &live())
            .expect("register");

        // T-012 fail-closed: an earlier-start EXAMPLE assignment must not
        // drop the nested canary, then BuiltIn-suppress into Clean.
        let example = scan_text(
            &scanner,
            "app.env",
            &format!("password = \"EXAMPLE{CANARY}padding-value-XXXX\"\n"),
        );
        assert_eq!(example.status(), ScanStatus::Findings);
        assert!(
            example
                .findings()
                .iter()
                .any(|finding| finding.rule_id() == RULE_CANARY),
            "canary nested in EXAMPLE high-entropy assignment must remain a finding"
        );
        assert!(
            example
                .applied_suppressions()
                .iter()
                .all(|applied| applied.rule_id() != RULE_CANARY)
        );
        assert_no_canary("example wrap report", &format!("{example:?}"));

        // Earlier-start connection string wrapping the same canary.
        let conn = scan_text(
            &scanner,
            "docker-compose.yml",
            &format!("url: postgres://app:{CANARY}@db.internal:5432/app\n"),
        );
        assert_eq!(conn.status(), ScanStatus::Findings);
        assert!(
            conn.findings()
                .iter()
                .any(|finding| finding.rule_id() == RULE_CANARY),
            "canary nested in a connection string must remain a finding"
        );
        assert_no_canary("connection wrap report", &format!("{conn:?}"));

        // Request suppression of the overlapping heuristic must not yield Clean.
        let mut request = ScanRequest::new();
        request
            .push_target(
                ScanTarget::staged_diff(
                    path("wrap.env"),
                    format!("password = \"prefix-{CANARY}-suffix-padding-xx\""),
                )
                .expect("target"),
            )
            .expect("push target");
        request
            .push_suppression(
                Suppression::new(
                    "secrets.high_entropy_assignment",
                    "test fixture assignment tracked in SEC-014",
                )
                .expect("suppression"),
            )
            .expect("push suppression");
        let suppressed = scanner.scan(&request, &live()).expect("scan");
        assert_eq!(suppressed.status(), ScanStatus::Findings);
        assert!(
            suppressed
                .findings()
                .iter()
                .any(|finding| finding.rule_id() == RULE_CANARY)
        );
        assert!(
            suppressed
                .applied_suppressions()
                .iter()
                .all(|applied| applied.rule_id() != RULE_CANARY)
        );
        assert_no_canary("request wrap report", &format!("{suppressed:?}"));
    }

    #[test]
    fn aws_and_github_tokens_emit_normalized_findings() {
        let scanner = SecretScanner::new();
        let report = scan_text(
            &scanner,
            "deploy.env",
            &format!("AWS_ACCESS_KEY_ID={AWS_LIVE}\nGITHUB_TOKEN={GHP}\n"),
        );
        assert_eq!(report.status(), ScanStatus::Findings);
        let rules: Vec<&str> = report.findings().iter().map(Finding::rule_id).collect();
        assert!(rules.contains(&"secrets.aws_access_key_id"));
        assert!(rules.contains(&"secrets.github_token"));
        for finding in report.findings() {
            assert_no_secret("message", finding.message(), AWS_LIVE);
            assert_no_secret("message", finding.message(), GHP);
            assert_no_secret("debug", &format!("{finding:?}"), AWS_LIVE);
            assert_eq!(finding.category(), FindingCategory::Secret);
            assert!(!finding.fingerprint().as_hex().is_empty());
        }
    }

    #[test]
    fn private_key_detected_public_key_is_not() {
        let scanner = SecretScanner::new();
        let private = scan_text(
            &scanner,
            "id_rsa",
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA\n-----END RSA PRIVATE KEY-----\n",
        );
        assert_eq!(private.status(), ScanStatus::Findings);
        assert_eq!(private.findings()[0].rule_id(), "secrets.private_key");
        assert_eq!(private.findings()[0].severity(), FindingSeverity::Critical);

        let public = scan_text(
            &scanner,
            "id_rsa.pub",
            "-----BEGIN PUBLIC KEY-----\nMIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8A\n-----END PUBLIC KEY-----\n",
        );
        assert_eq!(public.status(), ScanStatus::Clean);
        assert!(public.findings().is_empty());
    }

    #[test]
    fn documentation_example_is_auditable_suppression_not_silent() {
        let scanner = SecretScanner::new();
        let report = scan_text(
            &scanner,
            "docs/aws.md",
            &format!("Use {AWS_DOC} in the AWS docs only.\n"),
        );
        assert_eq!(report.status(), ScanStatus::Clean);
        assert!(report.findings().is_empty());
        assert_eq!(report.applied_suppressions().len(), 1);
        let applied = &report.applied_suppressions()[0];
        assert_eq!(applied.source(), SuppressionSource::BuiltIn);
        assert_eq!(applied.rule_id(), "secrets.aws_access_key_id");
        assert!(applied.reason().contains("EXAMPLE"));
        assert_no_secret("suppression debug", &format!("{applied:?}"), AWS_DOC);
    }

    #[test]
    fn request_suppression_requires_reason_and_is_recorded() {
        let scanner = SecretScanner::new();
        assert_eq!(
            Suppression::new("secrets.aws_access_key_id", "").unwrap_err(),
            ScanError::EmptyReason
        );
        assert_eq!(
            Suppression::new("*", "suppress everything").unwrap_err(),
            ScanError::InvalidRuleId
        );

        let mut request = ScanRequest::new();
        request
            .push_target(
                ScanTarget::staged_diff(path("ci/secrets.env"), format!("KEY={AWS_LIVE}"))
                    .expect("target"),
            )
            .expect("push target");
        request
            .push_suppression(
                Suppression::new(
                    "secrets.aws_access_key_id",
                    "rotated test fixture key tracked in SEC-014",
                )
                .expect("suppression")
                .with_path(path("ci/secrets.env")),
            )
            .expect("push suppression");
        let report = scanner.scan(&request, &live()).expect("scan");
        assert_eq!(report.status(), ScanStatus::Clean);
        assert_eq!(report.applied_suppressions().len(), 1);
        let applied = &report.applied_suppressions()[0];
        assert_eq!(applied.source(), SuppressionSource::Request);
        assert_eq!(
            applied.reason(),
            "rotated test fixture key tracked in SEC-014"
        );
        assert_no_secret("request suppression", &format!("{report:?}"), AWS_LIVE);
    }

    #[test]
    fn artifact_scan_detects_stripe_live_key() {
        let scanner = SecretScanner::new();
        let payload = b"{\"sk\":\"sk_live_abcdefghijklmnopqrstuvwx\"}";
        let artifact = ArtifactId::from_bytes(payload);
        let mut request = ScanRequest::new();
        request
            .push_target(
                ScanTarget::artifact(path("artifacts/ci.json"), artifact, payload.to_vec())
                    .expect("target"),
            )
            .expect("push");
        let report = scanner.scan(&request, &live()).expect("scan");
        assert_eq!(report.status(), ScanStatus::Findings);
        assert_eq!(report.findings()[0].rule_id(), "secrets.stripe_live_key");
        assert_eq!(report.findings()[0].evidence_ref(), artifact);
        assert_no_secret(
            "stripe finding",
            report.findings()[0].message(),
            "sk_live_abcdefghijklmnopqrstuvwx",
        );
    }

    #[test]
    fn low_entropy_password_is_not_a_finding() {
        let scanner = SecretScanner::new();
        let report = scan_text(&scanner, "app.toml", "password = \"hunter2\"\n");
        assert_eq!(report.status(), ScanStatus::Clean);
        assert!(report.findings().is_empty());
        assert!(report.applied_suppressions().is_empty());
    }

    #[test]
    fn connection_string_with_password_is_detected() {
        let scanner = SecretScanner::new();
        let report = scan_text(
            &scanner,
            "docker-compose.yml",
            "url: postgres://app:s3cret-pass@db.internal:5432/app\n",
        );
        assert_eq!(report.status(), ScanStatus::Findings);
        assert_eq!(report.findings()[0].rule_id(), "secrets.connection_string");
        assert_no_secret(
            "connection string",
            report.findings()[0].message(),
            "s3cret-pass",
        );
        let clean = scan_text(
            &scanner,
            "docker-compose.yml",
            "url: postgres://localhost/app\n",
        );
        assert_eq!(clean.status(), ScanStatus::Clean);
    }

    #[test]
    fn cancellation_and_bound_errors_are_not_clean() {
        let mut scanner = SecretScanner::new();
        let cancelled = ScanCancellation::new();
        cancelled.cancel();
        let err = scanner
            .register_canary(CANARY.as_bytes(), &cancelled)
            .expect_err("cancelled register");
        assert_eq!(err, ScanError::Cancelled);
        assert!(!format!("{err}").contains(CANARY));

        let err = ScanTarget::staged_diff(path("huge.bin"), vec![b'a'; MAX_TARGET_BYTES + 1])
            .expect_err("bound");
        assert!(matches!(err, ScanError::BoundExceeded { .. }));
        assert_ne!(
            ScanStatus::Clean,
            ScanStatus::Error,
            "error and clean stay distinct"
        );
        assert_eq!(err.retryable(), false);
        assert_eq!(err.code(), "security.secret_scan_bound_exceeded");
    }

    #[test]
    fn empty_and_clean_targets_are_clean_not_error() {
        let scanner = SecretScanner::new();
        let report = scan_text(&scanner, "README.md", "hello world, no credentials here\n");
        assert_eq!(report.status(), ScanStatus::Clean);
        assert!(report.errors().is_empty());
        assert_eq!(report.coverage().targets(), 1);
        assert_eq!(
            report.scanner_versions(),
            &[format!("{SCANNER_ID}/{SCANNER_VERSION}")]
        );
    }
}
