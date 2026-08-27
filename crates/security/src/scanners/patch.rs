//! Native staged-patch security scanner.
//!
//! Inspects staged file mutations for permission broadening, credential
//! handling, CI/release changes, and executable hooks. Generated and binary
//! payloads are classified as metadata; text rules never run on them.
//! Sensitive-path binaries fail closed. Scanner failure is Error/unavailable,
//! never Clean. Threats: `T-014`, `T-001`, `T-012`, `T-015`, `T-016`.

use std::fmt::{self, Debug, Display, Formatter};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering, compiler_fence};

use protocol::{ArtifactId, RepoPath};
use sha2::{Digest, Sha256};

/// Scanner identity stored on every finding and report.
pub const SCANNER_ID: &str = "rapidlm.native.patch";

/// Pinned local rule-bundle version. Not a vendor engine version.
pub const SCANNER_VERSION: &str = "1";

/// Maximum staged patch targets accepted in one request.
pub const MAX_SCAN_TARGETS: usize = 4096;

/// Maximum bytes accepted on one target.
pub const MAX_TARGET_BYTES: usize = 8 * 1024 * 1024;

/// Maximum combined target bytes accepted in one request.
pub const MAX_SCAN_BYTES: usize = 32 * 1024 * 1024;

/// Maximum findings retained on one report.
pub const MAX_FINDINGS: usize = 1024;

const FINGERPRINT_HEX_LEN: usize = 16;
const CANCEL_STRIDE: usize = 4096;
const CLASSIFY_PREFIX: usize = 8192;
const TAG_FINDING: &[u8] = b"rapidlm.patch_finding.v1";
const RULE_COUNT: usize = 14;

/// Cooperative cancellation for classify/scan loops.
#[derive(Clone, Debug)]
pub struct PatchScanCancellation {
    cancelled: Arc<AtomicBool>,
}

/// How one staged path changed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum PatchChangeKind {
    Create,
    Replace,
    Delete,
    Move,
}

/// Content class used to choose text vs metadata rules.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum PatchContentClass {
    Text,
    Generated,
    Binary,
}

/// Finding category for the SARIF-compatible internal model.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum PatchFindingCategory {
    Permission,
    Credential,
    CiRelease,
    ExecutableHook,
}

/// Finding severity. Hook/sudoers/unreadable CI are critical.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub enum PatchFindingSeverity {
    Low,
    Medium,
    High,
    Critical,
}

/// Detector confidence. Path matches are high; content heuristics medium.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub enum PatchFindingConfidence {
    Low,
    Medium,
    High,
}

/// Half-open byte range within one target. `[start, end)`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct PatchByteRange {
    start: u64,
    end: u64,
}

/// Stable finding identity. Hex of a digest over rule/path/range/content-hash.
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct PatchFindingFingerprint {
    hex: [u8; FINGERPRINT_HEX_LEN],
}

/// Normalized finding. Attacker payload bytes are never a field.
#[derive(Clone, Eq, PartialEq)]
pub struct PatchFinding {
    id: PatchFindingFingerprint,
    rule_id: &'static str,
    category: PatchFindingCategory,
    severity: PatchFindingSeverity,
    confidence: PatchFindingConfidence,
    path: RepoPath,
    range: PatchByteRange,
    fingerprint: PatchFindingFingerprint,
    message: &'static str,
    remediation: &'static str,
    scanner: &'static str,
    evidence_ref: ArtifactId,
    content_class: PatchContentClass,
}

/// Outcome of a completed scan. Error/unavailable never become Clean.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum PatchScanStatus {
    Clean,
    Findings,
    Error,
    Partial,
}

/// Bounded coverage counters. Labels never include payload bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct PatchScanCoverage {
    targets: usize,
    bytes_scanned: usize,
    rules: usize,
    text_targets: usize,
    generated_targets: usize,
    binary_targets: usize,
}

/// Typed scanner failure. Display never echoes attacker-controlled input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PatchScanError {
    Cancelled,
    BoundExceeded { limit: usize, requested: usize },
    TooManyTargets { limit: usize },
    Unavailable,
}

/// One staged path mutation. Content is omitted from Debug and wiped on drop.
pub struct PatchScanTarget {
    path: RepoPath,
    kind: PatchChangeKind,
    content: Vec<u8>,
    executable: bool,
    previous_executable: bool,
    from: Option<RepoPath>,
}

/// Request: staged patch targets only. No suppressions (fail closed).
pub struct PatchScanRequest {
    targets: Vec<PatchScanTarget>,
    total_bytes: usize,
}

/// Normalized report. Debug omits target bytes.
#[derive(Clone, Eq, PartialEq)]
pub struct PatchScanReport {
    status: PatchScanStatus,
    findings: Vec<PatchFinding>,
    coverage: PatchScanCoverage,
    scanner_versions: Vec<String>,
    errors: Vec<PatchScanError>,
}

/// Native patch scanner. Deterministic; no vendor engine.
#[derive(Clone, Debug, Default)]
pub struct PatchScanner;

struct RawHit {
    rule_id: &'static str,
    category: PatchFindingCategory,
    severity: PatchFindingSeverity,
    confidence: PatchFindingConfidence,
    start: usize,
    end: usize,
    message: &'static str,
    remediation: &'static str,
}

impl PatchScanCancellation {
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

    fn check(&self) -> Result<(), PatchScanError> {
        if self.is_cancelled() {
            Err(PatchScanError::Cancelled)
        } else {
            Ok(())
        }
    }
}

impl Default for PatchScanCancellation {
    fn default() -> Self {
        Self::new()
    }
}

impl PatchByteRange {
    pub const fn new(start: u64, end: u64) -> Result<Self, PatchScanError> {
        if start > end {
            return Err(PatchScanError::Unavailable);
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

impl PatchFindingFingerprint {
    fn compute(
        rule_id: &str,
        path: &RepoPath,
        start: u64,
        end: u64,
        content_digest: &[u8; 32],
    ) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(TAG_FINDING);
        hasher.update(rule_id.as_bytes());
        hasher.update([0u8]);
        hasher.update(path.as_str().as_bytes());
        hasher.update([0u8]);
        hasher.update(start.to_be_bytes());
        hasher.update(end.to_be_bytes());
        hasher.update(content_digest);
        let digest = hasher.finalize();
        let mut hex = [0u8; FINGERPRINT_HEX_LEN];
        write_hex_lower(&digest[..8], &mut hex);
        Self { hex }
    }

    pub fn as_hex(&self) -> &str {
        std::str::from_utf8(&self.hex).unwrap_or("????????????????")
    }
}

impl Display for PatchFindingFingerprint {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_hex())
    }
}

impl Debug for PatchFindingFingerprint {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_tuple("PatchFindingFingerprint")
            .field(&self.as_hex())
            .finish()
    }
}

impl PatchFinding {
    pub fn id(&self) -> PatchFindingFingerprint {
        self.id
    }

    pub fn rule_id(&self) -> &'static str {
        self.rule_id
    }

    pub fn category(&self) -> PatchFindingCategory {
        self.category
    }

    pub fn severity(&self) -> PatchFindingSeverity {
        self.severity
    }

    pub fn confidence(&self) -> PatchFindingConfidence {
        self.confidence
    }

    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    pub fn range(&self) -> PatchByteRange {
        self.range
    }

    pub fn fingerprint(&self) -> PatchFindingFingerprint {
        self.fingerprint
    }

    pub fn message(&self) -> &'static str {
        self.message
    }

    pub fn remediation(&self) -> &'static str {
        self.remediation
    }

    pub fn scanner(&self) -> &'static str {
        self.scanner
    }

    pub fn evidence_ref(&self) -> ArtifactId {
        self.evidence_ref
    }

    pub fn content_class(&self) -> PatchContentClass {
        self.content_class
    }
}

impl Debug for PatchFinding {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("PatchFinding")
            .field("rule_id", &self.rule_id)
            .field("category", &self.category)
            .field("severity", &self.severity)
            .field("confidence", &self.confidence)
            .field("path", &self.path)
            .field("range", &self.range)
            .field("fingerprint", &self.fingerprint)
            .field("message", &self.message)
            .field("scanner", &self.scanner)
            .field("evidence_ref", &self.evidence_ref)
            .field("content_class", &self.content_class)
            .finish()
    }
}

impl Display for PatchFinding {
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

impl PatchScanCoverage {
    pub fn targets(self) -> usize {
        self.targets
    }

    pub fn bytes_scanned(self) -> usize {
        self.bytes_scanned
    }

    pub fn rules(self) -> usize {
        self.rules
    }

    pub fn text_targets(self) -> usize {
        self.text_targets
    }

    pub fn generated_targets(self) -> usize {
        self.generated_targets
    }

    pub fn binary_targets(self) -> usize {
        self.binary_targets
    }
}

impl PatchScanTarget {
    pub fn create(
        path: RepoPath,
        content: impl Into<Vec<u8>>,
        executable: bool,
    ) -> Result<Self, PatchScanError> {
        Self::from_parts(
            path,
            PatchChangeKind::Create,
            content.into(),
            executable,
            false,
            None,
        )
    }

    pub fn replace(
        path: RepoPath,
        content: impl Into<Vec<u8>>,
        executable: bool,
        previous_executable: bool,
    ) -> Result<Self, PatchScanError> {
        Self::from_parts(
            path,
            PatchChangeKind::Replace,
            content.into(),
            executable,
            previous_executable,
            None,
        )
    }

    pub fn delete(path: RepoPath, previous_executable: bool) -> Result<Self, PatchScanError> {
        Self::from_parts(
            path,
            PatchChangeKind::Delete,
            Vec::new(),
            previous_executable,
            previous_executable,
            None,
        )
    }

    pub fn move_file(
        from: RepoPath,
        to: RepoPath,
        content: impl Into<Vec<u8>>,
        executable: bool,
        previous_executable: bool,
    ) -> Result<Self, PatchScanError> {
        Self::from_parts(
            to,
            PatchChangeKind::Move,
            content.into(),
            executable,
            previous_executable,
            Some(from),
        )
    }

    fn from_parts(
        path: RepoPath,
        kind: PatchChangeKind,
        mut content: Vec<u8>,
        executable: bool,
        previous_executable: bool,
        from: Option<RepoPath>,
    ) -> Result<Self, PatchScanError> {
        if content.len() > MAX_TARGET_BYTES {
            let requested = content.len();
            wipe(&mut content);
            return Err(PatchScanError::BoundExceeded {
                limit: MAX_TARGET_BYTES,
                requested,
            });
        }
        Ok(Self {
            path,
            kind,
            content,
            executable,
            previous_executable,
            from,
        })
    }

    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    pub fn kind(&self) -> PatchChangeKind {
        self.kind
    }

    pub fn executable(&self) -> bool {
        self.executable
    }

    pub fn previous_executable(&self) -> bool {
        self.previous_executable
    }

    pub fn from_path(&self) -> Option<&RepoPath> {
        self.from.as_ref()
    }

    pub fn len(&self) -> usize {
        self.content.len()
    }

    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }
}

impl Debug for PatchScanTarget {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("PatchScanTarget")
            .field("path", &self.path)
            .field("kind", &self.kind)
            .field("bytes", &self.content.len())
            .field("executable", &self.executable)
            .field("previous_executable", &self.previous_executable)
            .field("from", &self.from)
            .finish()
    }
}

impl Drop for PatchScanTarget {
    fn drop(&mut self) {
        wipe(&mut self.content);
        self.content.clear();
    }
}

impl PatchScanRequest {
    pub fn new() -> Self {
        Self {
            targets: Vec::new(),
            total_bytes: 0,
        }
    }

    pub fn push_target(&mut self, target: PatchScanTarget) -> Result<(), PatchScanError> {
        if self.targets.len() >= MAX_SCAN_TARGETS {
            return Err(PatchScanError::TooManyTargets {
                limit: MAX_SCAN_TARGETS,
            });
        }
        let next = self.total_bytes.saturating_add(target.len());
        if next > MAX_SCAN_BYTES {
            return Err(PatchScanError::BoundExceeded {
                limit: MAX_SCAN_BYTES,
                requested: next,
            });
        }
        self.total_bytes = next;
        self.targets.push(target);
        Ok(())
    }

    pub fn target_count(&self) -> usize {
        self.targets.len()
    }
}

impl Default for PatchScanRequest {
    fn default() -> Self {
        Self::new()
    }
}

impl Debug for PatchScanRequest {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("PatchScanRequest")
            .field("targets", &self.targets.len())
            .field("bytes", &self.total_bytes)
            .finish()
    }
}

impl PatchScanReport {
    pub fn status(&self) -> PatchScanStatus {
        self.status
    }

    pub fn findings(&self) -> &[PatchFinding] {
        &self.findings
    }

    pub fn coverage(&self) -> PatchScanCoverage {
        self.coverage
    }

    pub fn scanner_versions(&self) -> &[String] {
        &self.scanner_versions
    }

    pub fn errors(&self) -> &[PatchScanError] {
        &self.errors
    }
}

impl Debug for PatchScanReport {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("PatchScanReport")
            .field("status", &self.status)
            .field("findings", &self.findings)
            .field("coverage", &self.coverage)
            .field("scanner_versions", &self.scanner_versions)
            .field("errors", &self.errors)
            .finish()
    }
}

impl PatchScanner {
    pub fn new() -> Self {
        Self
    }

    /// Scan a staged patch. Failures never become [`PatchScanStatus::Clean`].
    pub fn scan(
        &self,
        request: &PatchScanRequest,
        cancel: &PatchScanCancellation,
    ) -> Result<PatchScanReport, PatchScanError> {
        cancel.check()?;
        let mut findings = Vec::new();
        let mut bytes_scanned = 0usize;
        let mut text_targets = 0usize;
        let mut generated_targets = 0usize;
        let mut binary_targets = 0usize;

        for (index, target) in request.targets.iter().enumerate() {
            cancel.check()?;
            if index.is_multiple_of(CANCEL_STRIDE) {
                cancel.check()?;
            }
            bytes_scanned = bytes_scanned.saturating_add(target.content.len());
            let class = classify_content(target, cancel)?;
            match class {
                PatchContentClass::Text => text_targets += 1,
                PatchContentClass::Generated => generated_targets += 1,
                PatchContentClass::Binary => binary_targets += 1,
            }
            let evidence = ArtifactId::from_bytes(&target.content);
            let hits = scan_target(target, class, cancel)?;
            if findings.len().saturating_add(hits.len()) > MAX_FINDINGS {
                return Err(PatchScanError::BoundExceeded {
                    limit: MAX_FINDINGS,
                    requested: findings.len().saturating_add(hits.len()),
                });
            }
            for hit in hits {
                findings.push(materialize_finding(target, evidence, class, &hit));
            }
        }

        let status = if findings.is_empty() {
            PatchScanStatus::Clean
        } else {
            PatchScanStatus::Findings
        };
        Ok(PatchScanReport {
            status,
            findings,
            coverage: PatchScanCoverage {
                targets: request.targets.len(),
                bytes_scanned,
                rules: RULE_COUNT,
                text_targets,
                generated_targets,
                binary_targets,
            },
            scanner_versions: vec![format!("{SCANNER_ID}/{SCANNER_VERSION}")],
            errors: Vec::new(),
        })
    }
}

impl PatchScanError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Cancelled => "security.patch_scan_cancelled",
            Self::BoundExceeded { .. } => "security.patch_scan_bound_exceeded",
            Self::TooManyTargets { .. } => "security.patch_scan_target_capacity",
            Self::Unavailable => "security.patch_scan_unavailable",
        }
    }

    pub fn retryable(&self) -> bool {
        false
    }

    /// Fail-closed status for this error. Never [`PatchScanStatus::Clean`].
    pub fn as_status(self) -> PatchScanStatus {
        PatchScanStatus::Error
    }
}

impl Display for PatchScanError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("patch scan was cancelled"),
            Self::BoundExceeded { limit, requested } => {
                write!(
                    f,
                    "patch scan exceeds bound ({requested} > {limit} bytes or items)"
                )
            }
            Self::TooManyTargets { limit } => {
                write!(f, "patch scan target list is at capacity ({limit})")
            }
            Self::Unavailable => f.write_str("patch scanner is unavailable"),
        }
    }
}

impl std::error::Error for PatchScanError {}

const REMEDIATE_PERMISSION: &str =
    "Restore the previous permission posture and require explicit review before broadening access.";
const REMEDIATE_CREDENTIAL: &str =
    "Remove credential material from the patch, rotate exposed secrets, and use a secret manager.";
const REMEDIATE_CI: &str =
    "Review the CI/release change independently and keep privileges at least privilege.";
const REMEDIATE_HOOK: &str =
    "Remove the executable hook or lifecycle script until a trusted owner reviews it.";
const REMEDIATE_UNREADABLE: &str = "Replace the binary/unreadable payload on this security-sensitive path with inspectable text or drop the change.";

fn materialize_finding(
    target: &PatchScanTarget,
    evidence: ArtifactId,
    class: PatchContentClass,
    hit: &RawHit,
) -> PatchFinding {
    let start = hit.start as u64;
    let end = hit.end as u64;
    let digest = Sha256::digest(&target.content);
    let mut content_digest = [0u8; 32];
    content_digest.copy_from_slice(&digest);
    let fingerprint =
        PatchFindingFingerprint::compute(hit.rule_id, &target.path, start, end, &content_digest);
    PatchFinding {
        id: fingerprint,
        rule_id: hit.rule_id,
        category: hit.category,
        severity: hit.severity,
        confidence: hit.confidence,
        path: target.path.clone(),
        range: PatchByteRange { start, end },
        fingerprint,
        message: hit.message,
        remediation: hit.remediation,
        scanner: SCANNER_ID,
        evidence_ref: evidence,
        content_class: class,
    }
}

fn scan_target(
    target: &PatchScanTarget,
    class: PatchContentClass,
    cancel: &PatchScanCancellation,
) -> Result<Vec<RawHit>, PatchScanError> {
    cancel.check()?;
    let mut hits = Vec::new();
    let lower = path_lower(&target.path);
    let from_lower = target.from.as_ref().map(path_lower);
    let dest_hook = is_hook_path(&lower);
    let dest_ci = is_ci_release_path(&lower);
    let dest_cred = is_credential_path(&lower);
    let dest_policy = is_security_policy_path(&lower);
    let dest_sensitive = dest_hook || dest_ci || dest_cred || dest_policy;
    let source_hook = from_lower.as_deref().is_some_and(is_hook_path);
    let source_ci = from_lower.as_deref().is_some_and(is_ci_release_path);
    let source_cred = from_lower.as_deref().is_some_and(is_credential_path);
    let source_policy = from_lower.as_deref().is_some_and(is_security_policy_path);

    if dest_hook || source_hook {
        hits.push(path_hit(
            "patch.hook_path",
            PatchFindingCategory::ExecutableHook,
            PatchFindingSeverity::Critical,
            PatchFindingConfidence::High,
            "Staged change touches an executable hook path",
            REMEDIATE_HOOK,
        ));
    }
    if dest_ci || source_ci {
        hits.push(path_hit(
            "patch.ci_release_path",
            PatchFindingCategory::CiRelease,
            PatchFindingSeverity::Medium,
            PatchFindingConfidence::High,
            "Staged change touches CI or release configuration",
            REMEDIATE_CI,
        ));
    }
    if dest_cred || source_cred {
        hits.push(path_hit(
            "patch.credential_path",
            PatchFindingCategory::Credential,
            PatchFindingSeverity::High,
            PatchFindingConfidence::High,
            "Staged change touches a credential-bearing path",
            REMEDIATE_CREDENTIAL,
        ));
    }
    if (target.kind == PatchChangeKind::Delete && (dest_policy || dest_ci || dest_hook))
        || (target.kind == PatchChangeKind::Move && (source_policy || source_ci || source_hook))
    {
        hits.push(path_hit(
            "patch.security_policy_delete",
            PatchFindingCategory::Permission,
            PatchFindingSeverity::High,
            PatchFindingConfidence::High,
            "Staged delete or move removes a security, CI, or hook control file",
            REMEDIATE_PERMISSION,
        ));
    }

    let newly_executable = target.executable && !target.previous_executable;
    if newly_executable && target.kind != PatchChangeKind::Delete {
        let rule = if target.kind == PatchChangeKind::Create {
            "patch.executable_create"
        } else {
            "patch.permission_broaden_exec"
        };
        hits.push(path_hit(
            rule,
            if dest_hook {
                PatchFindingCategory::ExecutableHook
            } else {
                PatchFindingCategory::Permission
            },
            PatchFindingSeverity::High,
            PatchFindingConfidence::High,
            "Staged change introduces or broadens the executable bit",
            REMEDIATE_PERMISSION,
        ));
    }

    if class == PatchContentClass::Binary
        && dest_sensitive
        && target.kind != PatchChangeKind::Delete
    {
        hits.push(path_hit(
            "patch.sensitive_unreadable",
            if dest_hook {
                PatchFindingCategory::ExecutableHook
            } else if dest_cred {
                PatchFindingCategory::Credential
            } else {
                PatchFindingCategory::CiRelease
            },
            PatchFindingSeverity::Critical,
            PatchFindingConfidence::High,
            "Security-sensitive path is binary or otherwise unreadable as text",
            REMEDIATE_UNREADABLE,
        ));
    }

    // Content markers are attacker-controlled and cannot skip text rules.
    // Only well-known generated *paths* (lockfiles/vendor) are metadata-only.
    if target.kind != PatchChangeKind::Delete && class != PatchContentClass::Binary {
        if is_generated_path(&lower) {
            if dest_sensitive {
                collect_generated_sensitive_hits(&target.content, &mut hits, cancel)?;
            }
        } else {
            collect_text_hits(target, &lower, dest_ci, dest_sensitive, &mut hits, cancel)?;
        }
    }

    Ok(hits)
}

fn collect_text_hits(
    target: &PatchScanTarget,
    lower: &str,
    dest_ci: bool,
    dest_sensitive: bool,
    hits: &mut Vec<RawHit>,
    cancel: &PatchScanCancellation,
) -> Result<(), PatchScanError> {
    cancel.check()?;
    let hay = target.content.as_slice();
    if !is_doc_path(lower) {
        collect_world_writable(hay, hits, cancel)?;
        collect_sudoers(hay, hits, cancel)?;
        collect_network_broaden(hay, dest_ci || is_infra_path(lower), hits, cancel)?;
        collect_lifecycle(lower, hay, hits, cancel)?;
        collect_git_hooks_path(lower, hay, hits, cancel)?;
        if dest_ci {
            collect_ci_permissions(hay, hits, cancel)?;
        }
        if target.executable && hay.starts_with(b"#!") {
            hits.push(RawHit {
                rule_id: "patch.shebang_executable",
                category: PatchFindingCategory::ExecutableHook,
                severity: PatchFindingSeverity::Medium,
                confidence: PatchFindingConfidence::High,
                start: 0,
                end: shebang_end(hay),
                message: "Staged executable starts with a shebang",
                remediation: REMEDIATE_HOOK,
            });
        }
    }
    if dest_sensitive || !is_doc_path(lower) {
        collect_credential_material(hay, hits, cancel)?;
    }
    Ok(())
}

fn collect_generated_sensitive_hits(
    hay: &[u8],
    hits: &mut Vec<RawHit>,
    cancel: &PatchScanCancellation,
) -> Result<(), PatchScanError> {
    cancel.check()?;
    collect_credential_material(hay, hits, cancel)
}

fn collect_world_writable(
    hay: &[u8],
    hits: &mut Vec<RawHit>,
    cancel: &PatchScanCancellation,
) -> Result<(), PatchScanError> {
    cancel.check()?;
    for needle in [
        &b"chmod 777"[..],
        b"chmod 0777",
        b"chmod a+rwx",
        b"chmod a+w",
        b"chmod 666",
        b"chmod 0666",
        b"mode: 0777",
        b"mode: 0o777",
        b"0o777",
    ] {
        if let Some(start) = find_ascii_ignore_case(hay, needle) {
            hits.push(RawHit {
                rule_id: "patch.world_writable",
                category: PatchFindingCategory::Permission,
                severity: PatchFindingSeverity::High,
                confidence: PatchFindingConfidence::High,
                start,
                end: start + needle.len(),
                message: "Staged text broadens file mode to a world-writable or 0777 posture",
                remediation: REMEDIATE_PERMISSION,
            });
            break;
        }
    }
    Ok(())
}

fn collect_sudoers(
    hay: &[u8],
    hits: &mut Vec<RawHit>,
    cancel: &PatchScanCancellation,
) -> Result<(), PatchScanError> {
    cancel.check()?;
    for needle in [
        &b"nopasswd: all"[..],
        b"nopasswd:all",
        b"(all) all",
        b"(all:all) all",
        b"all=(all) nopasswd:all",
        b"all=(all:all) nopasswd:all",
    ] {
        if let Some(start) = find_ascii_ignore_case(hay, needle) {
            hits.push(RawHit {
                rule_id: "patch.sudoers_all",
                category: PatchFindingCategory::Permission,
                severity: PatchFindingSeverity::Critical,
                confidence: PatchFindingConfidence::High,
                start,
                end: start + needle.len(),
                message: "Staged text grants unrestricted sudoers or NOPASSWD ALL",
                remediation: REMEDIATE_PERMISSION,
            });
            break;
        }
    }
    Ok(())
}

fn collect_network_broaden(
    hay: &[u8],
    apply: bool,
    hits: &mut Vec<RawHit>,
    cancel: &PatchScanCancellation,
) -> Result<(), PatchScanError> {
    if !apply {
        return Ok(());
    }
    cancel.check()?;
    for needle in [
        &b"0.0.0.0/0"[..],
        b"::/0",
        b"network: host",
        b"privileged: true",
        b"allowedhosts: \"*\"",
        b"allowedhosts: '*'",
        b"cidr_blocks = [\"0.0.0.0/0\"]",
    ] {
        if let Some(start) = find_ascii_ignore_case(hay, needle) {
            hits.push(RawHit {
                rule_id: "patch.network_broaden",
                category: PatchFindingCategory::Permission,
                severity: PatchFindingSeverity::High,
                confidence: PatchFindingConfidence::High,
                start,
                end: start + needle.len(),
                message: "Staged text broadens network or container isolation",
                remediation: REMEDIATE_PERMISSION,
            });
            break;
        }
    }
    Ok(())
}

fn collect_ci_permissions(
    hay: &[u8],
    hits: &mut Vec<RawHit>,
    cancel: &PatchScanCancellation,
) -> Result<(), PatchScanError> {
    cancel.check()?;
    for needle in [
        &b"permissions: write-all"[..],
        b"permissions:write-all",
        b"permissions: \"write-all\"",
        b"permissions: 'write-all'",
        b"contents: write",
        b"id-token: write",
        b"pull-requests: write",
        b"actions: write",
        b"packages: write",
        b"secrets: inherit",
    ] {
        if let Some(start) = find_ascii_ignore_case(hay, needle) {
            hits.push(RawHit {
                rule_id: "patch.ci_permissions_broaden",
                category: PatchFindingCategory::Permission,
                severity: PatchFindingSeverity::High,
                confidence: PatchFindingConfidence::High,
                start,
                end: start + needle.len(),
                message: "Staged CI configuration broadens token or workflow permissions",
                remediation: REMEDIATE_PERMISSION,
            });
            break;
        }
    }
    Ok(())
}

fn collect_lifecycle(
    lower: &str,
    hay: &[u8],
    hits: &mut Vec<RawHit>,
    cancel: &PatchScanCancellation,
) -> Result<(), PatchScanError> {
    cancel.check()?;
    if !is_package_manifest(lower) {
        return Ok(());
    }
    for needle in [
        &b"\"postinstall\""[..],
        b"\"preinstall\"",
        b"\"prepare\"",
        b"\"prepublish\"",
        b"\"prepublishonly\"",
        b"\"install\"",
    ] {
        if let Some(start) = find_ascii_ignore_case(hay, needle) {
            hits.push(RawHit {
                rule_id: "patch.lifecycle_hook",
                category: PatchFindingCategory::ExecutableHook,
                severity: PatchFindingSeverity::High,
                confidence: PatchFindingConfidence::High,
                start,
                end: start + needle.len(),
                message: "Staged package manifest adds an install-time lifecycle hook",
                remediation: REMEDIATE_HOOK,
            });
            break;
        }
    }
    Ok(())
}

fn collect_git_hooks_path(
    lower: &str,
    hay: &[u8],
    hits: &mut Vec<RawHit>,
    cancel: &PatchScanCancellation,
) -> Result<(), PatchScanError> {
    cancel.check()?;
    if file_name(lower) != ".gitconfig" && !lower.ends_with(".git/config") {
        return Ok(());
    }
    if let Some(start) = find_ascii_ignore_case(hay, b"hookspath") {
        hits.push(RawHit {
            rule_id: "patch.hook_path",
            category: PatchFindingCategory::ExecutableHook,
            severity: PatchFindingSeverity::Critical,
            confidence: PatchFindingConfidence::High,
            start,
            end: start + b"hookspath".len(),
            message: "Staged git config redirects hooksPath",
            remediation: REMEDIATE_HOOK,
        });
    }
    Ok(())
}

fn collect_credential_material(
    hay: &[u8],
    hits: &mut Vec<RawHit>,
    cancel: &PatchScanCancellation,
) -> Result<(), PatchScanError> {
    cancel.check()?;
    const BEGIN: &[u8] = b"-----BEGIN ";
    const PRIVATE: &[u8] = b"PRIVATE KEY";
    if let Some(start) = find_bytes(hay, BEGIN) {
        let after = start + BEGIN.len();
        let window_end = hay.len().min(after.saturating_add(48));
        if find_bytes(&hay[after..window_end], PRIVATE).is_some() {
            let end = after
                .min(hay.len())
                .saturating_add(PRIVATE.len())
                .min(hay.len());
            hits.push(RawHit {
                rule_id: "patch.credential_material",
                category: PatchFindingCategory::Credential,
                severity: PatchFindingSeverity::Critical,
                confidence: PatchFindingConfidence::High,
                start,
                end,
                message: "Staged text includes private-key material",
                remediation: REMEDIATE_CREDENTIAL,
            });
            return Ok(());
        }
    }
    for needle in [
        &b"aws_secret_access_key="[..],
        b"aws_secret_access_key:",
        b"github_token=",
        b"npm_token=",
        b"private_key:",
    ] {
        if let Some(start) = find_ascii_ignore_case(hay, needle) {
            hits.push(RawHit {
                rule_id: "patch.credential_material",
                category: PatchFindingCategory::Credential,
                severity: PatchFindingSeverity::High,
                confidence: PatchFindingConfidence::High,
                start,
                end: start + needle.len(),
                message: "Staged text assigns credential material",
                remediation: REMEDIATE_CREDENTIAL,
            });
            break;
        }
    }
    Ok(())
}

fn path_hit(
    rule_id: &'static str,
    category: PatchFindingCategory,
    severity: PatchFindingSeverity,
    confidence: PatchFindingConfidence,
    message: &'static str,
    remediation: &'static str,
) -> RawHit {
    RawHit {
        rule_id,
        category,
        severity,
        confidence,
        start: 0,
        end: 0,
        message,
        remediation,
    }
}

fn classify_content(
    target: &PatchScanTarget,
    cancel: &PatchScanCancellation,
) -> Result<PatchContentClass, PatchScanError> {
    cancel.check()?;
    if target.kind == PatchChangeKind::Delete {
        return Ok(PatchContentClass::Text);
    }
    let lower = path_lower(&target.path);
    if is_binary_bytes(&target.content, cancel)? || is_binary_path(&lower) {
        return Ok(PatchContentClass::Binary);
    }
    if is_generated_path(&lower) || is_generated_marker(&target.content, cancel)? {
        return Ok(PatchContentClass::Generated);
    }
    Ok(PatchContentClass::Text)
}

fn is_binary_bytes(bytes: &[u8], cancel: &PatchScanCancellation) -> Result<bool, PatchScanError> {
    if bytes.is_empty() {
        return Ok(false);
    }
    if bytes.starts_with(b"\x7fELF")
        || bytes.starts_with(b"MZ")
        || bytes.starts_with(b"\0asm")
        || bytes.starts_with(&[0xfe, 0xed, 0xfa, 0xce])
        || bytes.starts_with(&[0xce, 0xfa, 0xed, 0xfe])
        || bytes.starts_with(&[0xcf, 0xfa, 0xed, 0xfe])
    {
        return Ok(true);
    }
    let prefix = bytes.len().min(CLASSIFY_PREFIX);
    let mut non_text = 0usize;
    for (index, &byte) in bytes[..prefix].iter().enumerate() {
        if index.is_multiple_of(CANCEL_STRIDE) {
            cancel.check()?;
        }
        if byte == 0 {
            return Ok(true);
        }
        if !is_text_byte(byte) {
            non_text += 1;
        }
    }
    Ok(non_text.saturating_mul(100) / prefix.max(1) > 30)
}

fn is_text_byte(byte: u8) -> bool {
    byte >= 0x20 || matches!(byte, b'\t' | b'\n' | b'\r')
}

fn is_generated_marker(
    bytes: &[u8],
    cancel: &PatchScanCancellation,
) -> Result<bool, PatchScanError> {
    cancel.check()?;
    let prefix = &bytes[..bytes.len().min(CLASSIFY_PREFIX)];
    for needle in [
        &b"@generated"[..],
        b"code generated by",
        b"this file is generated",
        b"do not edit",
        b"autogenerated file",
    ] {
        if find_ascii_ignore_case(prefix, needle).is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn path_lower(path: &RepoPath) -> String {
    path.as_str().to_ascii_lowercase()
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn is_hook_path(lower: &str) -> bool {
    let name = file_name(lower);
    lower.starts_with(".git/hooks/")
        || lower.contains("/.git/hooks/")
        || lower.starts_with(".husky/")
        || lower.contains("/.husky/")
        || lower.starts_with(".githooks/")
        || lower.contains("/.githooks/")
        || lower.starts_with(".lefthook/")
        || lower == "lefthook.yml"
        || lower == "lefthook.yaml"
        || lower == ".pre-commit-config.yaml"
        || lower == ".pre-commit-config.yml"
        || matches!(
            name,
            "pre-commit"
                | "pre-push"
                | "post-commit"
                | "post-merge"
                | "post-checkout"
                | "commit-msg"
                | "prepare-commit-msg"
        )
}

fn is_ci_release_path(lower: &str) -> bool {
    let name = file_name(lower);
    lower.starts_with(".github/workflows/")
        || lower.contains("/.github/workflows/")
        || lower.starts_with(".github/actions/")
        || lower == ".gitlab-ci.yml"
        || lower.starts_with(".gitlab/")
        || lower.starts_with(".circleci/")
        || lower.starts_with(".buildkite/")
        || lower.starts_with(".github/dependabot")
        || name == "jenkinsfile"
        || name.starts_with("azure-pipelines")
        || name == ".travis.yml"
        || name.starts_with(".goreleaser")
        || name.starts_with("dockerfile")
        || name.starts_with("docker-compose")
        || name == "cloudbuild.yaml"
        || name == "cloudbuild.yml"
}

fn is_credential_path(lower: &str) -> bool {
    let name = file_name(lower);
    if name.ends_with(".pub") {
        return false;
    }
    name == ".env"
        || name.starts_with(".env.")
        || name == "credentials"
        || name == "credentials.json"
        || name == "id_rsa"
        || name == "id_dsa"
        || name == "id_ecdsa"
        || name == "id_ed25519"
        || name.ends_with(".pem")
        || name.ends_with(".p12")
        || name.ends_with(".pfx")
        || name == "secrets.yaml"
        || name == "secrets.yml"
        || name == "secrets.json"
        || name == ".npmrc"
        || name == ".pypirc"
        || name == "netrc"
        || name == ".netrc"
        || name == "auth.json"
        || name.ends_with(".key")
        || lower.contains(".aws/credentials")
        || lower.contains("/.ssh/")
        || lower.starts_with(".ssh/")
}

fn is_security_policy_path(lower: &str) -> bool {
    let name = file_name(lower);
    name == "codeowners" || name == "security.md"
}

fn is_package_manifest(lower: &str) -> bool {
    matches!(file_name(lower), "package.json" | "package.json5")
}

fn is_doc_path(lower: &str) -> bool {
    let name = file_name(lower);
    lower.starts_with("docs/") || name.ends_with(".md") || name.starts_with("readme")
}

fn is_infra_path(lower: &str) -> bool {
    let name = file_name(lower);
    lower.starts_with("deploy/")
        || lower.starts_with("infra/")
        || lower.starts_with("terraform/")
        || lower.contains("/k8s/")
        || name.ends_with(".tf")
        || name.ends_with(".tfvars")
        || name.contains("security-group")
        || name.contains("firewall")
        || name.starts_with("dockerfile")
        || name.starts_with("docker-compose")
}

fn is_binary_path(lower: &str) -> bool {
    let name = file_name(lower);
    name.ends_with(".exe")
        || name.ends_with(".dll")
        || name.ends_with(".so")
        || name.ends_with(".dylib")
        || name.ends_with(".wasm")
        || name.ends_with(".png")
        || name.ends_with(".jpg")
        || name.ends_with(".jpeg")
        || name.ends_with(".gif")
        || name.ends_with(".woff")
        || name.ends_with(".woff2")
        || name.ends_with(".bin")
}

fn is_generated_path(lower: &str) -> bool {
    let name = file_name(lower);
    name == "cargo.lock"
        || name == "package-lock.json"
        || name == "pnpm-lock.yaml"
        || name == "yarn.lock"
        || name == "go.sum"
        || name == "poetry.lock"
        || name == "composer.lock"
        || name.contains(".generated.")
        || name.ends_with(".pb.go")
        || name.ends_with("_pb2.py")
        || name.ends_with(".min.js")
        || name.ends_with(".min.css")
        || lower.contains("/vendor/")
        || lower.starts_with("vendor/")
}

fn shebang_end(hay: &[u8]) -> usize {
    hay.iter()
        .position(|&b| b == b'\n')
        .map(|i| i + 1)
        .unwrap_or(hay.len().min(128))
}

fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len())
        .position(|window| window == needle)
}

fn find_ascii_ignore_case(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).position(|window| {
        window
            .iter()
            .zip(needle.iter())
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
    })
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

    const PRIVATE_KEY: &str =
        "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEAFAKE\n-----END RSA PRIVATE KEY-----\n";

    fn live() -> PatchScanCancellation {
        PatchScanCancellation::new()
    }

    fn path(raw: &str) -> RepoPath {
        RepoPath::parse(raw).expect("repo path")
    }

    fn scan_one(target: PatchScanTarget) -> PatchScanReport {
        let mut request = PatchScanRequest::new();
        request.push_target(target).expect("push");
        PatchScanner::new().scan(&request, &live()).expect("scan")
    }

    fn rules(report: &PatchScanReport) -> Vec<&'static str> {
        report
            .findings()
            .iter()
            .map(PatchFinding::rule_id)
            .collect()
    }

    fn assert_no_payload(label: &str, rendered: &str, payload: &str) {
        assert!(
            !rendered.contains(payload),
            "{label} echoed payload: {rendered}"
        );
    }

    #[test]
    fn ordinary_source_text_is_clean() {
        let report = scan_one(
            PatchScanTarget::replace(
                path("src/lib.rs"),
                b"pub fn add(a: i32, b: i32) -> i32 { a + b }\n".to_vec(),
                false,
                false,
            )
            .expect("target"),
        );
        assert_eq!(report.status(), PatchScanStatus::Clean);
        assert!(report.findings().is_empty());
        assert!(report.errors().is_empty());
        assert_eq!(report.coverage().text_targets(), 1);
        assert_eq!(report.coverage().binary_targets(), 0);
        assert_eq!(report.coverage().generated_targets(), 0);
        assert_eq!(
            report.scanner_versions(),
            &[format!("{SCANNER_ID}/{SCANNER_VERSION}")]
        );
    }

    #[test]
    fn executable_bit_broadening_emits_path_range_fingerprint_and_remediation() {
        let report = scan_one(
            PatchScanTarget::replace(path("scripts/tool.sh"), b"echo hi\n".to_vec(), true, false)
                .expect("target"),
        );
        assert_eq!(report.status(), PatchScanStatus::Findings);
        assert!(rules(&report).contains(&"patch.permission_broaden_exec"));
        let finding = report
            .findings()
            .iter()
            .find(|item| item.rule_id() == "patch.permission_broaden_exec")
            .expect("exec finding");
        assert_eq!(finding.path().as_str(), "scripts/tool.sh");
        assert_eq!(finding.range().start(), 0);
        assert_eq!(finding.range().end(), 0);
        assert!(!finding.fingerprint().as_hex().is_empty());
        assert!(!finding.remediation().is_empty());
        assert_eq!(finding.scanner(), SCANNER_ID);
        assert_eq!(finding.category(), PatchFindingCategory::Permission);
        assert_eq!(finding.severity(), PatchFindingSeverity::High);
    }

    #[test]
    fn existing_executable_replace_is_not_permission_broadening() {
        let report = scan_one(
            PatchScanTarget::replace(path("scripts/tool.sh"), b"echo hi\n".to_vec(), true, true)
                .expect("target"),
        );
        assert!(!rules(&report).contains(&"patch.permission_broaden_exec"));
        assert!(!rules(&report).contains(&"patch.executable_create"));
    }

    #[test]
    fn new_executable_create_is_flagged() {
        let report = scan_one(
            PatchScanTarget::create(path("bin/helper"), b"#!/bin/sh\necho hi\n".to_vec(), true)
                .expect("target"),
        );
        assert!(rules(&report).contains(&"patch.executable_create"));
        assert!(rules(&report).contains(&"patch.shebang_executable"));
        assert_eq!(report.status(), PatchScanStatus::Findings);
    }

    #[test]
    fn github_actions_write_all_is_permission_broadening() {
        let report = scan_one(
            PatchScanTarget::create(
                path(".github/workflows/release.yml"),
                b"name: release\npermissions: write-all\njobs: {}\n".to_vec(),
                false,
            )
            .expect("target"),
        );
        assert!(rules(&report).contains(&"patch.ci_release_path"));
        assert!(rules(&report).contains(&"patch.ci_permissions_broaden"));
        let finding = report
            .findings()
            .iter()
            .find(|item| item.rule_id() == "patch.ci_permissions_broaden")
            .expect("perm");
        assert_eq!(finding.category(), PatchFindingCategory::Permission);
        assert!(finding.range().end() > finding.range().start());
        assert!(!finding.remediation().is_empty());
    }

    #[test]
    fn ci_path_match_is_case_insensitive() {
        let report = scan_one(
            PatchScanTarget::create(
                path(".GitHub/workflows/ci.yml"),
                b"permissions: write-all\n".to_vec(),
                false,
            )
            .expect("target"),
        );
        assert!(rules(&report).contains(&"patch.ci_release_path"));
        assert!(rules(&report).contains(&"patch.ci_permissions_broaden"));
    }

    #[test]
    fn credential_path_and_private_key_are_flagged_without_echo() {
        let report = scan_one(
            PatchScanTarget::create(path(".ssh/id_rsa"), PRIVATE_KEY.as_bytes().to_vec(), false)
                .expect("target"),
        );
        assert!(rules(&report).contains(&"patch.credential_path"));
        assert!(rules(&report).contains(&"patch.credential_material"));
        let rendered = format!("{report:?}");
        assert_no_payload("report debug", &rendered, "MIIEowIBAAKCAQEAFAKE");
        for finding in report.findings() {
            assert_no_payload("message", finding.message(), "MIIEowIBAAKCAQEAFAKE");
            assert_no_payload("display", &finding.to_string(), "BEGIN RSA");
        }
    }

    #[test]
    fn husky_and_git_hooks_are_executable_hooks() {
        let husky = scan_one(
            PatchScanTarget::create(path(".husky/pre-commit"), b"npm test\n".to_vec(), true)
                .expect("target"),
        );
        assert!(rules(&husky).contains(&"patch.hook_path"));
        assert_eq!(
            husky
                .findings()
                .iter()
                .find(|item| item.rule_id() == "patch.hook_path")
                .map(PatchFinding::severity),
            Some(PatchFindingSeverity::Critical)
        );

        let hook = scan_one(
            PatchScanTarget::create(path(".git/hooks/pre-push"), b"exit 0\n".to_vec(), true)
                .expect("target"),
        );
        assert!(rules(&hook).contains(&"patch.hook_path"));
        assert!(rules(&hook).contains(&"patch.executable_create"));
    }

    #[test]
    fn generated_marker_cannot_hide_a_hook() {
        let report = scan_one(
            PatchScanTarget::create(
                path(".husky/pre-commit"),
                b"// @generated\nnpm test\n".to_vec(),
                true,
            )
            .expect("target"),
        );
        assert_eq!(report.coverage().generated_targets(), 1);
        assert!(rules(&report).contains(&"patch.hook_path"));
        assert_eq!(report.status(), PatchScanStatus::Findings);
    }

    #[test]
    fn generated_marker_cannot_skip_permission_text_rules() {
        let report = scan_one(
            PatchScanTarget::replace(
                path("scripts/setup.sh"),
                b"# DO NOT EDIT\nchmod 777 /tmp/work\n".to_vec(),
                false,
                false,
            )
            .expect("target"),
        );
        assert_eq!(report.coverage().generated_targets(), 1);
        assert!(rules(&report).contains(&"patch.world_writable"));
        assert_eq!(report.status(), PatchScanStatus::Findings);
    }

    #[test]
    fn package_lifecycle_hook_is_flagged() {
        let report = scan_one(
            PatchScanTarget::replace(
                path("package.json"),
                br#"{"scripts":{"postinstall":"node steal.js"}}"#.to_vec(),
                false,
                false,
            )
            .expect("target"),
        );
        assert!(rules(&report).contains(&"patch.lifecycle_hook"));
        assert_eq!(
            report
                .findings()
                .iter()
                .find(|item| item.rule_id() == "patch.lifecycle_hook")
                .map(PatchFinding::category),
            Some(PatchFindingCategory::ExecutableHook)
        );
    }

    #[test]
    fn generated_lockfile_is_metadata_not_text_heuristic() {
        let report = scan_one(
            PatchScanTarget::replace(
                path("Cargo.lock"),
                b"# This file is automatically @generated by Cargo.\nchecksum = \"0.0.0.0/0\"\n"
                    .to_vec(),
                false,
                false,
            )
            .expect("target"),
        );
        assert_eq!(report.coverage().generated_targets(), 1);
        assert_eq!(report.coverage().text_targets(), 0);
        assert!(!rules(&report).contains(&"patch.network_broaden"));
        assert_eq!(report.status(), PatchScanStatus::Clean);
    }

    #[test]
    fn binary_payload_is_not_scanned_as_text() {
        let mut blob = b"\x7fELF".to_vec();
        blob.extend_from_slice(b"permissions: write-all");
        blob.extend_from_slice(&[0u8; 32]);
        let report =
            scan_one(PatchScanTarget::create(path("bin/helper"), blob, false).expect("target"));
        assert_eq!(report.coverage().binary_targets(), 1);
        assert!(!rules(&report).contains(&"patch.ci_permissions_broaden"));
        assert_eq!(report.status(), PatchScanStatus::Clean);
        assert_eq!(
            format!(
                "{:?}",
                PatchScanTarget::create(path("bin/helper"), b"\x7fELF".to_vec(), false)
                    .expect("dbg")
            ),
            format!(
                "{:?}",
                PatchScanTarget::create(path("bin/helper"), b"\x7fELF".to_vec(), false)
                    .expect("dbg2")
            )
        );
    }

    #[test]
    fn nul_in_workflow_cannot_bypass_ci_rules() {
        let mut payload = b"name: evil\npermissions: write-all\n".to_vec();
        payload.push(0);
        let report = scan_one(
            PatchScanTarget::create(path(".github/workflows/evil.yml"), payload, false)
                .expect("target"),
        );
        assert_eq!(report.coverage().binary_targets(), 1);
        assert!(rules(&report).contains(&"patch.ci_release_path"));
        assert!(rules(&report).contains(&"patch.sensitive_unreadable"));
        assert_eq!(report.status(), PatchScanStatus::Findings);
        let finding = report
            .findings()
            .iter()
            .find(|item| item.rule_id() == "patch.sensitive_unreadable")
            .expect("unreadable");
        assert_eq!(finding.severity(), PatchFindingSeverity::Critical);
        assert_eq!(finding.content_class(), PatchContentClass::Binary);
    }

    #[test]
    fn world_writable_and_sudoers_and_network_are_flagged() {
        let chmod = scan_one(
            PatchScanTarget::replace(
                path("scripts/setup.sh"),
                b"chmod 777 /tmp/work\n".to_vec(),
                false,
                false,
            )
            .expect("target"),
        );
        assert!(rules(&chmod).contains(&"patch.world_writable"));

        let sudo = scan_one(
            PatchScanTarget::create(
                path("deploy/sudoers"),
                b"app ALL=(ALL) NOPASSWD:ALL\n".to_vec(),
                false,
            )
            .expect("target"),
        );
        assert!(rules(&sudo).contains(&"patch.sudoers_all"));

        let net = scan_one(
            PatchScanTarget::create(
                path("infra/sg.tf"),
                b"cidr_blocks = [\"0.0.0.0/0\"]\n".to_vec(),
                false,
            )
            .expect("target"),
        );
        assert!(rules(&net).contains(&"patch.network_broaden"));
    }

    #[test]
    fn docs_do_not_trigger_content_heuristics() {
        let report = scan_one(
            PatchScanTarget::replace(
                path("docs/security.md"),
                b"Never set permissions: write-all or chmod 777.\n".to_vec(),
                false,
                false,
            )
            .expect("target"),
        );
        assert_eq!(report.status(), PatchScanStatus::Clean);
    }

    #[test]
    fn delete_of_codeowners_is_permission_change() {
        let report = scan_one(PatchScanTarget::delete(path("CODEOWNERS"), false).expect("target"));
        assert!(rules(&report).contains(&"patch.security_policy_delete"));
        assert_eq!(report.status(), PatchScanStatus::Findings);
    }

    #[test]
    fn move_into_hook_path_is_flagged() {
        let report = scan_one(
            PatchScanTarget::move_file(
                path("scripts/helper.sh"),
                path(".husky/pre-commit"),
                b"#!/bin/sh\nexit 0\n".to_vec(),
                true,
                true,
            )
            .expect("target"),
        );
        assert!(rules(&report).contains(&"patch.hook_path"));
        assert_eq!(report.status(), PatchScanStatus::Findings);
    }

    #[test]
    fn dockerfile_privileged_is_ci_and_network() {
        let report = scan_one(
            PatchScanTarget::create(
                path("Dockerfile"),
                b"FROM alpine\n# privileged: true in compose\n".to_vec(),
                false,
            )
            .expect("target"),
        );
        assert!(rules(&report).contains(&"patch.ci_release_path"));
        let compose = scan_one(
            PatchScanTarget::create(
                path("docker-compose.yml"),
                b"services:\n  app:\n    privileged: true\n    network: host\n".to_vec(),
                false,
            )
            .expect("target"),
        );
        assert!(rules(&compose).contains(&"patch.ci_release_path"));
        assert!(rules(&compose).contains(&"patch.network_broaden"));
    }

    #[test]
    fn cancellation_and_bound_errors_are_not_clean() {
        let cancelled = PatchScanCancellation::new();
        cancelled.cancel();
        let mut request = PatchScanRequest::new();
        request
            .push_target(
                PatchScanTarget::create(path("src/lib.rs"), b"fn main() {}\n".to_vec(), false)
                    .expect("target"),
            )
            .expect("push");
        let err = PatchScanner::new()
            .scan(&request, &cancelled)
            .expect_err("cancelled");
        assert_eq!(err, PatchScanError::Cancelled);
        assert_eq!(err.as_status(), PatchScanStatus::Error);
        assert_ne!(err.as_status(), PatchScanStatus::Clean);
        assert_eq!(err.code(), "security.patch_scan_cancelled");
        assert!(!err.retryable());

        let bound =
            PatchScanTarget::create(path("huge.bin"), vec![b'a'; MAX_TARGET_BYTES + 1], false)
                .expect_err("bound");
        assert!(matches!(bound, PatchScanError::BoundExceeded { .. }));
        assert_eq!(bound.as_status(), PatchScanStatus::Error);
        assert_eq!(bound.code(), "security.patch_scan_bound_exceeded");
        assert!(!format!("{bound}").contains("aaaa"));

        assert_ne!(PatchScanStatus::Error, PatchScanStatus::Clean);
        assert_ne!(PatchScanStatus::Partial, PatchScanStatus::Clean);
        assert_eq!(
            PatchScanError::Unavailable.as_status(),
            PatchScanStatus::Error
        );
        assert_eq!(
            PatchScanError::Unavailable.to_string(),
            "patch scanner is unavailable"
        );
    }

    #[test]
    fn empty_request_is_clean_not_error() {
        let report = PatchScanner::new()
            .scan(&PatchScanRequest::new(), &live())
            .expect("scan");
        assert_eq!(report.status(), PatchScanStatus::Clean);
        assert_eq!(report.coverage().targets(), 0);
        assert_eq!(report.coverage().rules(), RULE_COUNT);
        assert!(report.errors().is_empty());
    }

    #[test]
    fn debug_omits_target_bytes() {
        let target = PatchScanTarget::create(
            path(".env"),
            b"AWS_SECRET_ACCESS_KEY=wJalrXUtn\n".to_vec(),
            false,
        )
        .expect("target");
        let shown = format!("{target:?}");
        assert!(!shown.contains("wJalrXUtn"));
        assert!(shown.contains("bytes"));
        let mut request = PatchScanRequest::new();
        request.push_target(target).expect("push");
        let report = PatchScanner::new().scan(&request, &live()).expect("scan");
        assert!(rules(&report).contains(&"patch.credential_path"));
        assert!(rules(&report).contains(&"patch.credential_material"));
        assert_no_payload("report", &format!("{report:?}"), "wJalrXUtn");
    }

    #[test]
    fn invalid_range_is_unavailable_not_clean() {
        let err = PatchByteRange::new(8, 2).expect_err("range");
        assert_eq!(err, PatchScanError::Unavailable);
        assert_eq!(err.as_status(), PatchScanStatus::Error);
    }
}
