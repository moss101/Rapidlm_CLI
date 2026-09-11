//! Scanner gate aggregator.
//!
//! Combines required scanner results by policy and blocks verification/apply
//! when a required scanner is failed, unavailable, or error. Unavailable and
//! error never become pass. Findings may proceed only with a typed policy
//! exception and audit ID. Threats: `T-014`, `T-012`, `T-001`, `T-016`.

use std::fmt::{self, Debug, Display, Formatter};

use crate::scanners::command::{CommandScanReport, CommandScanStatus, SCANNER_ID as COMMAND_ID};
use crate::scanners::external::{ExternalScanReport, ExternalScanStatus, MAX_SCANNER_ID_BYTES};
use crate::scanners::patch::{PatchScanReport, PatchScanStatus, SCANNER_ID as PATCH_ID};
use crate::scanners::secrets::{SCANNER_ID as SECRETS_ID, ScanReport, ScanStatus};

/// Maximum required scanners on one [`ScanGatePolicy`].
pub const MAX_REQUIRED_SCANNERS: usize = 32;

/// Maximum results accepted by [`evaluate_scan_gate`].
pub const MAX_GATE_RESULTS: usize = 64;

/// Maximum waivers on one [`ScanGatePolicy`].
pub const MAX_GATE_WAIVERS: usize = 32;

/// Maximum UTF-8 bytes accepted in a [`GateAuditId`].
pub const MAX_AUDIT_ID_BYTES: usize = 128;

/// Validated scanner identity used by policy and results.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct ScannerId(String);

/// Pipeline stage the gate is protecting.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum GatePhase {
    PreAction,
    Verification,
    Apply,
}

/// Normalized outcome consumed by the gate. Never upgraded from error/unavailable.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ScannerOutcome {
    Clean,
    Findings,
    Partial,
    Error,
    Unavailable,
}

/// Policy action when a required scanner reports findings.
///
/// Pass is not a findings action. Findings become pass only via
/// [`PolicyException`] plus [`GateAuditId`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum FindingsDisposition {
    Warn,
    Ask,
    Block,
}

/// Combined verdict class. Most restrictive reason wins.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub enum GateDisposition {
    Pass,
    Warn,
    Ask,
    Block,
}

/// Why a required scanner contributed to the verdict.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum GateReasonCode {
    Clean,
    Findings,
    FindingsWaived,
    PartialCoverage,
    ScannerError,
    ScannerUnavailable,
    MissingResult,
    ConflictingResults,
    BoundExceeded,
}

/// Typed exception that may waive findings only.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum PolicyException {
    AcceptedFindings,
}

/// Audit identity bound to a waiver. Empty or control-bearing values are rejected.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct GateAuditId(String);

/// One required scanner and the findings action for this gate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequiredScanner {
    id: ScannerId,
    on_findings: FindingsDisposition,
}

/// Findings waiver. Cannot make unavailable/error/partial into pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanWaiver {
    scanner_id: ScannerId,
    exception: PolicyException,
    audit_id: GateAuditId,
}

/// Policy: required scanners plus optional typed waivers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanGatePolicy {
    phase: GatePhase,
    required: Vec<RequiredScanner>,
    waivers: Vec<ScanWaiver>,
}

/// One scanner result supplied to the aggregator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanGateResult {
    scanner_id: ScannerId,
    outcome: ScannerOutcome,
    finding_count: usize,
}

/// Explicit reason recorded on a verdict. Finding payload bytes are never stored.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GateReason {
    scanner_id: Option<ScannerId>,
    code: GateReasonCode,
    disposition: GateDisposition,
    finding_count: usize,
    waiver_audit_id: Option<GateAuditId>,
}

/// Gate decision for verification/apply. Reasons are always populated on deny.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GateVerdict {
    disposition: GateDisposition,
    phase: GatePhase,
    reasons: Vec<GateReason>,
}

/// Policy/result construction failure. Evaluation itself always yields a verdict.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GateError {
    EmptyScannerId,
    InvalidScannerId,
    BoundExceeded { limit: usize, requested: usize },
    DuplicateRequiredScanner,
    EmptyAuditId,
    InvalidAuditId,
}

impl ScannerId {
    /// Parse a bounded scanner identity. Charset matches external scanner ids.
    pub fn parse(raw: impl AsRef<str>) -> Result<Self, GateError> {
        let raw = raw.as_ref();
        if raw.is_empty() {
            return Err(GateError::EmptyScannerId);
        }
        if raw.len() > MAX_SCANNER_ID_BYTES {
            return Err(GateError::BoundExceeded {
                limit: MAX_SCANNER_ID_BYTES,
                requested: raw.len(),
            });
        }
        if !raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        {
            return Err(GateError::InvalidScannerId);
        }
        Ok(Self(raw.to_owned()))
    }

    pub fn secrets() -> Self {
        Self(SECRETS_ID.to_owned())
    }

    pub fn command() -> Self {
        Self(COMMAND_ID.to_owned())
    }

    pub fn patch() -> Self {
        Self(PATCH_ID.to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for ScannerId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl GateAuditId {
    pub fn parse(raw: impl AsRef<str>) -> Result<Self, GateError> {
        let raw = raw.as_ref();
        if raw.is_empty() {
            return Err(GateError::EmptyAuditId);
        }
        if raw.len() > MAX_AUDIT_ID_BYTES {
            return Err(GateError::BoundExceeded {
                limit: MAX_AUDIT_ID_BYTES,
                requested: raw.len(),
            });
        }
        if !raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':'))
        {
            return Err(GateError::InvalidAuditId);
        }
        Ok(Self(raw.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for GateAuditId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl RequiredScanner {
    pub fn new(id: ScannerId, on_findings: FindingsDisposition) -> Self {
        Self { id, on_findings }
    }

    pub fn id(&self) -> &ScannerId {
        &self.id
    }

    pub fn on_findings(&self) -> FindingsDisposition {
        self.on_findings
    }
}

impl ScanWaiver {
    pub fn new(scanner_id: ScannerId, exception: PolicyException, audit_id: GateAuditId) -> Self {
        Self {
            scanner_id,
            exception,
            audit_id,
        }
    }

    pub fn scanner_id(&self) -> &ScannerId {
        &self.scanner_id
    }

    pub fn exception(&self) -> PolicyException {
        self.exception
    }

    pub fn audit_id(&self) -> &GateAuditId {
        &self.audit_id
    }
}

impl ScanGatePolicy {
    pub fn new(phase: GatePhase) -> Self {
        Self {
            phase,
            required: Vec::new(),
            waivers: Vec::new(),
        }
    }

    pub fn require(
        mut self,
        id: ScannerId,
        on_findings: FindingsDisposition,
    ) -> Result<Self, GateError> {
        if self.required.len() >= MAX_REQUIRED_SCANNERS {
            return Err(GateError::BoundExceeded {
                limit: MAX_REQUIRED_SCANNERS,
                requested: self.required.len().saturating_add(1),
            });
        }
        if self.required.iter().any(|item| item.id == id) {
            return Err(GateError::DuplicateRequiredScanner);
        }
        self.required.push(RequiredScanner::new(id, on_findings));
        Ok(self)
    }

    pub fn waive(mut self, waiver: ScanWaiver) -> Result<Self, GateError> {
        if self.waivers.len() >= MAX_GATE_WAIVERS {
            return Err(GateError::BoundExceeded {
                limit: MAX_GATE_WAIVERS,
                requested: self.waivers.len().saturating_add(1),
            });
        }
        self.waivers.push(waiver);
        Ok(self)
    }

    pub fn phase(&self) -> GatePhase {
        self.phase
    }

    pub fn required(&self) -> &[RequiredScanner] {
        &self.required
    }

    pub fn waivers(&self) -> &[ScanWaiver] {
        &self.waivers
    }
}

impl Default for ScanGatePolicy {
    fn default() -> Self {
        Self::new(GatePhase::Apply)
    }
}

impl ScanGateResult {
    pub fn new(scanner_id: ScannerId, outcome: ScannerOutcome) -> Self {
        Self {
            scanner_id,
            outcome,
            finding_count: 0,
        }
    }

    pub fn with_finding_count(mut self, finding_count: usize) -> Self {
        self.finding_count = finding_count;
        self
    }

    pub fn from_secret_report(report: &ScanReport) -> Self {
        Self::from_native(
            ScannerId::secrets(),
            map_native_status(
                report.status(),
                report.findings().len(),
                report.errors().len(),
            ),
            report.findings().len(),
        )
    }

    pub fn from_command_report(report: &CommandScanReport) -> Self {
        Self::from_native(
            ScannerId::command(),
            map_command_status(
                report.status(),
                report.findings().len(),
                report.errors().len(),
            ),
            report.findings().len(),
        )
    }

    pub fn from_patch_report(report: &PatchScanReport) -> Self {
        Self::from_native(
            ScannerId::patch(),
            map_patch_status(
                report.status(),
                report.findings().len(),
                report.errors().len(),
            ),
            report.findings().len(),
        )
    }

    pub fn from_external_report(scanner_id: ScannerId, report: &ExternalScanReport) -> Self {
        Self::from_native(
            scanner_id,
            map_external_status(
                report.status(),
                report.findings().len(),
                report.errors().len(),
            ),
            report.findings().len(),
        )
    }

    pub fn scanner_id(&self) -> &ScannerId {
        &self.scanner_id
    }

    pub fn outcome(&self) -> ScannerOutcome {
        self.outcome
    }

    pub fn finding_count(&self) -> usize {
        self.finding_count
    }

    fn from_native(scanner_id: ScannerId, outcome: ScannerOutcome, finding_count: usize) -> Self {
        Self {
            scanner_id,
            outcome,
            finding_count,
        }
    }
}

impl GateReason {
    pub fn scanner_id(&self) -> Option<&ScannerId> {
        self.scanner_id.as_ref()
    }

    pub fn code(&self) -> GateReasonCode {
        self.code
    }

    pub fn disposition(&self) -> GateDisposition {
        self.disposition
    }

    pub fn finding_count(&self) -> usize {
        self.finding_count
    }

    pub fn waiver_audit_id(&self) -> Option<&GateAuditId> {
        self.waiver_audit_id.as_ref()
    }
}

impl Display for GateReason {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.scanner_id {
            Some(id) => write!(f, "{id}: {}", self.code.as_str()),
            None => f.write_str(self.code.as_str()),
        }
    }
}

impl GateReasonCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Findings => "findings",
            Self::FindingsWaived => "findings-waived",
            Self::PartialCoverage => "partial-coverage",
            Self::ScannerError => "error",
            Self::ScannerUnavailable => "unavailable",
            Self::MissingResult => "missing-result",
            Self::ConflictingResults => "conflicting-results",
            Self::BoundExceeded => "bound-exceeded",
        }
    }
}

impl Display for GateReasonCode {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl GateVerdict {
    pub fn disposition(&self) -> GateDisposition {
        self.disposition
    }

    pub fn phase(&self) -> GatePhase {
        self.phase
    }

    pub fn reasons(&self) -> &[GateReason] {
        &self.reasons
    }

    /// Verification may complete only on pass/warn. Ask does not execute.
    pub fn allows_verification(&self) -> bool {
        matches!(
            self.disposition,
            GateDisposition::Pass | GateDisposition::Warn
        )
    }

    /// Apply may proceed only on pass/warn. Ask does not execute.
    pub fn allows_apply(&self) -> bool {
        self.allows_verification()
    }

    pub fn is_pass(&self) -> bool {
        self.disposition == GateDisposition::Pass
    }
}

impl Display for GateVerdict {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.disposition.as_str())?;
        if !self.reasons.is_empty() {
            f.write_str(" (")?;
            for (index, reason) in self.reasons.iter().enumerate() {
                if index > 0 {
                    f.write_str("; ")?;
                }
                Display::fmt(reason, f)?;
            }
            f.write_str(")")?;
        }
        Ok(())
    }
}

impl GateDisposition {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Warn => "warn",
            Self::Ask => "ask",
            Self::Block => "block",
        }
    }

    fn combine(self, other: Self) -> Self {
        self.max(other)
    }
}

impl Display for GateDisposition {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl GateError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::EmptyScannerId => "security.scan_gate_empty_scanner_id",
            Self::InvalidScannerId => "security.scan_gate_invalid_scanner_id",
            Self::BoundExceeded { .. } => "security.scan_gate_bound_exceeded",
            Self::DuplicateRequiredScanner => "security.scan_gate_duplicate_required",
            Self::EmptyAuditId => "security.scan_gate_empty_audit_id",
            Self::InvalidAuditId => "security.scan_gate_invalid_audit_id",
        }
    }

    pub fn retryable(&self) -> bool {
        false
    }
}

impl Display for GateError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyScannerId => f.write_str("scan gate scanner id must not be empty"),
            Self::InvalidScannerId => f.write_str("scan gate scanner id is invalid"),
            Self::BoundExceeded { limit, requested } => {
                write!(
                    f,
                    "scan gate exceeds bound ({requested} > {limit} bytes or items)"
                )
            }
            Self::DuplicateRequiredScanner => {
                f.write_str("scan gate required scanner is duplicated")
            }
            Self::EmptyAuditId => f.write_str("scan gate waiver audit id must not be empty"),
            Self::InvalidAuditId => f.write_str("scan gate waiver audit id is invalid"),
        }
    }
}

impl std::error::Error for GateError {}

/// Combine required scanners by policy. Required unavailable/error cannot pass.
pub fn evaluate_scan_gate(policy: &ScanGatePolicy, results: &[ScanGateResult]) -> GateVerdict {
    if results.len() > MAX_GATE_RESULTS {
        return GateVerdict {
            disposition: GateDisposition::Block,
            phase: policy.phase,
            reasons: vec![reason(
                None,
                GateReasonCode::BoundExceeded,
                GateDisposition::Block,
                0,
                None,
            )],
        };
    }

    let mut disposition = GateDisposition::Pass;
    let mut reasons = Vec::new();

    for required in &policy.required {
        let (code, outcome, finding_count) = lookup_required(&required.id, results);
        let (reason_disp, reason_code, audit) =
            decide_required(required, code, outcome, policy.waivers.as_slice());
        disposition = disposition.combine(reason_disp);
        reasons.push(reason(
            Some(required.id.clone()),
            reason_code,
            reason_disp,
            finding_count,
            audit,
        ));
    }

    GateVerdict {
        disposition,
        phase: policy.phase,
        reasons,
    }
}

fn lookup_required(
    id: &ScannerId,
    results: &[ScanGateResult],
) -> (GateReasonCode, ScannerOutcome, usize) {
    let mut found: Option<&ScanGateResult> = None;
    for result in results {
        if result.scanner_id != *id {
            continue;
        }
        match found {
            None => found = Some(result),
            Some(first)
                if first.outcome == result.outcome
                    && first.finding_count == result.finding_count => {}
            Some(_) => {
                return (GateReasonCode::ConflictingResults, ScannerOutcome::Error, 0);
            }
        }
    }
    match found {
        Some(result) => (
            reason_code_for(result.outcome),
            result.outcome,
            result.finding_count,
        ),
        None => (
            GateReasonCode::MissingResult,
            ScannerOutcome::Unavailable,
            0,
        ),
    }
}

fn decide_required(
    required: &RequiredScanner,
    code: GateReasonCode,
    outcome: ScannerOutcome,
    waivers: &[ScanWaiver],
) -> (GateDisposition, GateReasonCode, Option<GateAuditId>) {
    match outcome {
        ScannerOutcome::Clean => (GateDisposition::Pass, GateReasonCode::Clean, None),
        ScannerOutcome::Findings => match matching_findings_waiver(&required.id, waivers) {
            Some(waiver) => (
                GateDisposition::Pass,
                GateReasonCode::FindingsWaived,
                Some(waiver.audit_id.clone()),
            ),
            None => (
                disposition_for_findings(required.on_findings),
                GateReasonCode::Findings,
                None,
            ),
        },
        ScannerOutcome::Partial => (
            GateDisposition::Block,
            GateReasonCode::PartialCoverage,
            None,
        ),
        ScannerOutcome::Error => {
            let reason = if code == GateReasonCode::ConflictingResults {
                GateReasonCode::ConflictingResults
            } else {
                GateReasonCode::ScannerError
            };
            (GateDisposition::Block, reason, None)
        }
        ScannerOutcome::Unavailable => {
            let reason = if code == GateReasonCode::MissingResult {
                GateReasonCode::MissingResult
            } else {
                GateReasonCode::ScannerUnavailable
            };
            (GateDisposition::Block, reason, None)
        }
    }
}

fn matching_findings_waiver<'a>(
    scanner_id: &ScannerId,
    waivers: &'a [ScanWaiver],
) -> Option<&'a ScanWaiver> {
    waivers.iter().find(|waiver| {
        waiver.scanner_id == *scanner_id
            && matches!(waiver.exception, PolicyException::AcceptedFindings)
    })
}

fn disposition_for_findings(action: FindingsDisposition) -> GateDisposition {
    match action {
        FindingsDisposition::Warn => GateDisposition::Warn,
        FindingsDisposition::Ask => GateDisposition::Ask,
        FindingsDisposition::Block => GateDisposition::Block,
    }
}

fn reason_code_for(outcome: ScannerOutcome) -> GateReasonCode {
    match outcome {
        ScannerOutcome::Clean => GateReasonCode::Clean,
        ScannerOutcome::Findings => GateReasonCode::Findings,
        ScannerOutcome::Partial => GateReasonCode::PartialCoverage,
        ScannerOutcome::Error => GateReasonCode::ScannerError,
        ScannerOutcome::Unavailable => GateReasonCode::ScannerUnavailable,
    }
}

fn reason(
    scanner_id: Option<ScannerId>,
    code: GateReasonCode,
    disposition: GateDisposition,
    finding_count: usize,
    waiver_audit_id: Option<GateAuditId>,
) -> GateReason {
    GateReason {
        scanner_id,
        code,
        disposition,
        finding_count,
        waiver_audit_id,
    }
}

fn map_native_status(status: ScanStatus, findings: usize, errors: usize) -> ScannerOutcome {
    reconcile(
        match status {
            ScanStatus::Clean => ScannerOutcome::Clean,
            ScanStatus::Findings => ScannerOutcome::Findings,
            ScanStatus::Partial => ScannerOutcome::Partial,
            ScanStatus::Error => ScannerOutcome::Error,
        },
        findings,
        errors,
    )
}

fn map_command_status(status: CommandScanStatus, findings: usize, errors: usize) -> ScannerOutcome {
    reconcile(
        match status {
            CommandScanStatus::Clean => ScannerOutcome::Clean,
            CommandScanStatus::Findings => ScannerOutcome::Findings,
            CommandScanStatus::Partial => ScannerOutcome::Partial,
            CommandScanStatus::Error => ScannerOutcome::Error,
        },
        findings,
        errors,
    )
}

fn map_patch_status(status: PatchScanStatus, findings: usize, errors: usize) -> ScannerOutcome {
    reconcile(
        match status {
            PatchScanStatus::Clean => ScannerOutcome::Clean,
            PatchScanStatus::Findings => ScannerOutcome::Findings,
            PatchScanStatus::Partial => ScannerOutcome::Partial,
            PatchScanStatus::Error => ScannerOutcome::Error,
        },
        findings,
        errors,
    )
}

fn map_external_status(
    status: ExternalScanStatus,
    findings: usize,
    errors: usize,
) -> ScannerOutcome {
    reconcile(
        match status {
            ExternalScanStatus::Passed => ScannerOutcome::Clean,
            ExternalScanStatus::Failed => ScannerOutcome::Findings,
            ExternalScanStatus::Unavailable => ScannerOutcome::Unavailable,
            ExternalScanStatus::Error => ScannerOutcome::Error,
        },
        findings,
        errors,
    )
}

/// Inconsistent reports fail closed. Clean/pass never hides findings or errors.
fn reconcile(claimed: ScannerOutcome, findings: usize, errors: usize) -> ScannerOutcome {
    if errors > 0 && !matches!(claimed, ScannerOutcome::Error | ScannerOutcome::Unavailable) {
        return ScannerOutcome::Error;
    }
    if findings > 0 && matches!(claimed, ScannerOutcome::Clean) {
        return ScannerOutcome::Findings;
    }
    claimed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanners::secrets::{ScanCancellation, ScanRequest, SecretScanner};

    fn audit(id: &str) -> GateAuditId {
        GateAuditId::parse(id).expect("audit id")
    }

    fn waiver(scanner: ScannerId, id: &str) -> ScanWaiver {
        ScanWaiver::new(scanner, PolicyException::AcceptedFindings, audit(id))
    }

    fn apply_policy(scanners: &[(ScannerId, FindingsDisposition)]) -> ScanGatePolicy {
        let mut policy = ScanGatePolicy::new(GatePhase::Apply);
        for (id, action) in scanners {
            policy = policy
                .require(id.clone(), *action)
                .expect("require scanner");
        }
        policy
    }

    fn verify_policy(scanners: &[(ScannerId, FindingsDisposition)]) -> ScanGatePolicy {
        let mut policy = ScanGatePolicy::new(GatePhase::Verification);
        for (id, action) in scanners {
            policy = policy
                .require(id.clone(), *action)
                .expect("require scanner");
        }
        policy
    }

    fn clean(id: ScannerId) -> ScanGateResult {
        ScanGateResult::new(id, ScannerOutcome::Clean)
    }

    fn findings(id: ScannerId, count: usize) -> ScanGateResult {
        ScanGateResult::new(id, ScannerOutcome::Findings).with_finding_count(count)
    }

    #[test]
    fn all_required_clean_allows_apply_and_verification() {
        let policy = apply_policy(&[
            (ScannerId::secrets(), FindingsDisposition::Block),
            (ScannerId::patch(), FindingsDisposition::Block),
        ]);
        let verdict = evaluate_scan_gate(
            &policy,
            &[clean(ScannerId::secrets()), clean(ScannerId::patch())],
        );
        assert_eq!(verdict.disposition(), GateDisposition::Pass);
        assert!(verdict.is_pass());
        assert!(verdict.allows_apply());
        assert!(verdict.allows_verification());
        assert_eq!(verdict.phase(), GatePhase::Apply);
        assert!(
            verdict
                .reasons()
                .iter()
                .all(|reason| reason.code() == GateReasonCode::Clean)
        );
    }

    #[test]
    fn required_scanner_unavailable_cannot_yield_pass() {
        let policy = verify_policy(&[(ScannerId::secrets(), FindingsDisposition::Warn)]);
        let verdict = evaluate_scan_gate(
            &policy,
            &[ScanGateResult::new(
                ScannerId::secrets(),
                ScannerOutcome::Unavailable,
            )],
        );
        assert_eq!(verdict.disposition(), GateDisposition::Block);
        assert!(!verdict.allows_verification());
        assert!(!verdict.allows_apply());
        assert!(!verdict.is_pass());
        assert_eq!(
            verdict.reasons()[0].code(),
            GateReasonCode::ScannerUnavailable
        );
        assert!(format!("{verdict}").contains("unavailable"));
    }

    #[test]
    fn missing_required_result_is_unavailable_and_blocks() {
        let policy = apply_policy(&[
            (ScannerId::secrets(), FindingsDisposition::Block),
            (ScannerId::patch(), FindingsDisposition::Block),
        ]);
        let verdict = evaluate_scan_gate(&policy, &[clean(ScannerId::secrets())]);
        assert_eq!(verdict.disposition(), GateDisposition::Block);
        assert!(!verdict.allows_apply());
        let patch = verdict
            .reasons()
            .iter()
            .find(|reason| reason.scanner_id() == Some(&ScannerId::patch()))
            .expect("patch reason");
        assert_eq!(patch.code(), GateReasonCode::MissingResult);
    }

    #[test]
    fn required_scanner_error_cannot_yield_pass() {
        let policy = apply_policy(&[(ScannerId::command(), FindingsDisposition::Warn)]);
        let verdict = evaluate_scan_gate(
            &policy,
            &[ScanGateResult::new(
                ScannerId::command(),
                ScannerOutcome::Error,
            )],
        );
        assert_eq!(verdict.disposition(), GateDisposition::Block);
        assert_eq!(verdict.reasons()[0].code(), GateReasonCode::ScannerError);
        assert!(!verdict.allows_apply());
    }

    #[test]
    fn required_partial_coverage_blocks() {
        let policy = apply_policy(&[(ScannerId::patch(), FindingsDisposition::Ask)]);
        let verdict = evaluate_scan_gate(
            &policy,
            &[ScanGateResult::new(
                ScannerId::patch(),
                ScannerOutcome::Partial,
            )],
        );
        assert_eq!(verdict.disposition(), GateDisposition::Block);
        assert_eq!(verdict.reasons()[0].code(), GateReasonCode::PartialCoverage);
    }

    #[test]
    fn findings_without_waiver_follow_policy_disposition() {
        let block = evaluate_scan_gate(
            &apply_policy(&[(ScannerId::secrets(), FindingsDisposition::Block)]),
            &[findings(ScannerId::secrets(), 2)],
        );
        assert_eq!(block.disposition(), GateDisposition::Block);
        assert!(!block.allows_apply());
        assert_eq!(block.reasons()[0].finding_count(), 2);

        let ask = evaluate_scan_gate(
            &apply_policy(&[(ScannerId::secrets(), FindingsDisposition::Ask)]),
            &[findings(ScannerId::secrets(), 1)],
        );
        assert_eq!(ask.disposition(), GateDisposition::Ask);
        assert!(!ask.allows_apply());

        let warn = evaluate_scan_gate(
            &apply_policy(&[(ScannerId::secrets(), FindingsDisposition::Warn)]),
            &[findings(ScannerId::secrets(), 1)],
        );
        assert_eq!(warn.disposition(), GateDisposition::Warn);
        assert!(warn.allows_apply());
        assert!(warn.allows_verification());
    }

    #[test]
    fn waiver_requires_typed_exception_and_audit_id() {
        assert_eq!(GateAuditId::parse("").unwrap_err(), GateError::EmptyAuditId);
        assert_eq!(
            GateAuditId::parse("bad audit\nid").unwrap_err(),
            GateError::InvalidAuditId
        );
        assert_eq!(
            GateError::EmptyAuditId.code(),
            "security.scan_gate_empty_audit_id"
        );
        assert!(!GateError::EmptyAuditId.retryable());

        let exception = PolicyException::AcceptedFindings;
        let waiver = ScanWaiver::new(ScannerId::secrets(), exception, audit("AUD-018-001"));
        assert_eq!(waiver.exception(), PolicyException::AcceptedFindings);
        assert_eq!(waiver.audit_id().as_str(), "AUD-018-001");
    }

    #[test]
    fn typed_waiver_allows_findings_but_not_unavailable() {
        let policy = apply_policy(&[(ScannerId::secrets(), FindingsDisposition::Block)])
            .waive(waiver(ScannerId::secrets(), "AUD-018-FINDINGS"))
            .expect("waive");

        let waived = evaluate_scan_gate(&policy, &[findings(ScannerId::secrets(), 3)]);
        assert_eq!(waived.disposition(), GateDisposition::Pass);
        assert!(waived.allows_apply());
        assert_eq!(waived.reasons()[0].code(), GateReasonCode::FindingsWaived);
        assert_eq!(
            waived.reasons()[0]
                .waiver_audit_id()
                .map(GateAuditId::as_str),
            Some("AUD-018-FINDINGS")
        );

        let unavailable = evaluate_scan_gate(
            &policy,
            &[ScanGateResult::new(
                ScannerId::secrets(),
                ScannerOutcome::Unavailable,
            )],
        );
        assert_eq!(unavailable.disposition(), GateDisposition::Block);
        assert!(!unavailable.allows_apply());
        assert_eq!(
            unavailable.reasons()[0].code(),
            GateReasonCode::ScannerUnavailable
        );
        assert!(unavailable.reasons()[0].waiver_audit_id().is_none());

        let error = evaluate_scan_gate(
            &policy,
            &[ScanGateResult::new(
                ScannerId::secrets(),
                ScannerOutcome::Error,
            )],
        );
        assert_eq!(error.disposition(), GateDisposition::Block);
        assert_eq!(error.reasons()[0].code(), GateReasonCode::ScannerError);
    }

    #[test]
    fn waiver_for_other_scanner_does_not_bypass() {
        let policy = apply_policy(&[
            (ScannerId::secrets(), FindingsDisposition::Block),
            (ScannerId::patch(), FindingsDisposition::Block),
        ])
        .waive(waiver(ScannerId::patch(), "AUD-018-PATCH"))
        .expect("waive");
        let verdict = evaluate_scan_gate(
            &policy,
            &[
                findings(ScannerId::secrets(), 1),
                findings(ScannerId::patch(), 1),
            ],
        );
        assert_eq!(verdict.disposition(), GateDisposition::Block);
        assert!(!verdict.allows_apply());
        let secrets = verdict
            .reasons()
            .iter()
            .find(|reason| reason.scanner_id() == Some(&ScannerId::secrets()))
            .expect("secrets");
        assert_eq!(secrets.code(), GateReasonCode::Findings);
    }

    #[test]
    fn extra_clean_result_cannot_cover_missing_required() {
        let policy = apply_policy(&[(ScannerId::secrets(), FindingsDisposition::Block)]);
        let extra = ScannerId::parse("rapidlm.external.sast").expect("id");
        let verdict = evaluate_scan_gate(&policy, &[clean(extra), clean(ScannerId::command())]);
        assert_eq!(verdict.disposition(), GateDisposition::Block);
        assert_eq!(verdict.reasons()[0].code(), GateReasonCode::MissingResult);
    }

    #[test]
    fn one_required_failure_blocks_combined_gate() {
        let policy = apply_policy(&[
            (ScannerId::secrets(), FindingsDisposition::Block),
            (ScannerId::patch(), FindingsDisposition::Block),
            (ScannerId::command(), FindingsDisposition::Ask),
        ]);
        let verdict = evaluate_scan_gate(
            &policy,
            &[
                clean(ScannerId::secrets()),
                ScanGateResult::new(ScannerId::patch(), ScannerOutcome::Error),
                clean(ScannerId::command()),
            ],
        );
        assert_eq!(verdict.disposition(), GateDisposition::Block);
        assert!(!verdict.allows_verification());
    }

    #[test]
    fn most_restrictive_findings_disposition_wins() {
        let policy = apply_policy(&[
            (ScannerId::secrets(), FindingsDisposition::Warn),
            (ScannerId::patch(), FindingsDisposition::Ask),
        ]);
        let verdict = evaluate_scan_gate(
            &policy,
            &[
                findings(ScannerId::secrets(), 1),
                findings(ScannerId::patch(), 1),
            ],
        );
        assert_eq!(verdict.disposition(), GateDisposition::Ask);
        assert!(!verdict.allows_apply());
    }

    #[test]
    fn conflicting_results_fail_closed() {
        let policy = apply_policy(&[(ScannerId::secrets(), FindingsDisposition::Warn)]);
        let verdict = evaluate_scan_gate(
            &policy,
            &[
                clean(ScannerId::secrets()),
                findings(ScannerId::secrets(), 1),
            ],
        );
        assert_eq!(verdict.disposition(), GateDisposition::Block);
        assert_eq!(
            verdict.reasons()[0].code(),
            GateReasonCode::ConflictingResults
        );
        let waived = apply_policy(&[(ScannerId::secrets(), FindingsDisposition::Block)])
            .waive(waiver(ScannerId::secrets(), "AUD-018-CONFLICT"))
            .expect("waive");
        let still_blocked = evaluate_scan_gate(
            &waived,
            &[
                clean(ScannerId::secrets()),
                findings(ScannerId::secrets(), 1),
            ],
        );
        assert_eq!(still_blocked.disposition(), GateDisposition::Block);
    }

    #[test]
    fn oversized_result_set_blocks() {
        let policy = apply_policy(&[(ScannerId::secrets(), FindingsDisposition::Block)]);
        let extra = ScannerId::parse("rapidlm.external.extra").expect("id");
        let mut results = vec![clean(ScannerId::secrets())];
        for _ in 0..MAX_GATE_RESULTS {
            results.push(clean(extra.clone()));
        }
        let verdict = evaluate_scan_gate(&policy, &results);
        assert_eq!(verdict.disposition(), GateDisposition::Block);
        assert_eq!(verdict.reasons()[0].code(), GateReasonCode::BoundExceeded);
        assert!(!verdict.allows_apply());
    }

    #[test]
    fn invalid_scanner_id_and_duplicate_required_are_rejected() {
        assert_eq!(ScannerId::parse("").unwrap_err(), GateError::EmptyScannerId);
        assert_eq!(
            ScannerId::parse("has space").unwrap_err(),
            GateError::InvalidScannerId
        );
        assert_eq!(
            ScannerId::parse("x".repeat(MAX_SCANNER_ID_BYTES + 1)).unwrap_err(),
            GateError::BoundExceeded {
                limit: MAX_SCANNER_ID_BYTES,
                requested: MAX_SCANNER_ID_BYTES + 1,
            }
        );
        let err = ScanGatePolicy::new(GatePhase::Apply)
            .require(ScannerId::secrets(), FindingsDisposition::Block)
            .expect("first")
            .require(ScannerId::secrets(), FindingsDisposition::Warn)
            .expect_err("duplicate");
        assert_eq!(err, GateError::DuplicateRequiredScanner);
        assert!(!format!("{err}").contains("pass"));
    }

    #[test]
    fn native_and_external_status_maps_fail_closed() {
        assert_eq!(
            map_native_status(ScanStatus::Clean, 0, 0),
            ScannerOutcome::Clean
        );
        assert_eq!(
            map_native_status(ScanStatus::Clean, 1, 0),
            ScannerOutcome::Findings
        );
        assert_eq!(
            map_native_status(ScanStatus::Clean, 0, 1),
            ScannerOutcome::Error
        );
        assert_eq!(
            map_native_status(ScanStatus::Findings, 2, 0),
            ScannerOutcome::Findings
        );
        assert_eq!(
            map_native_status(ScanStatus::Partial, 0, 0),
            ScannerOutcome::Partial
        );
        assert_eq!(
            map_native_status(ScanStatus::Error, 0, 1),
            ScannerOutcome::Error
        );
        assert_eq!(
            map_command_status(CommandScanStatus::Error, 0, 0),
            ScannerOutcome::Error
        );
        assert_eq!(
            map_patch_status(PatchScanStatus::Partial, 0, 0),
            ScannerOutcome::Partial
        );
        assert_eq!(
            map_external_status(ExternalScanStatus::Passed, 0, 0),
            ScannerOutcome::Clean
        );
        assert_eq!(
            map_external_status(ExternalScanStatus::Failed, 4, 0),
            ScannerOutcome::Findings
        );
        assert_eq!(
            map_external_status(ExternalScanStatus::Unavailable, 0, 1),
            ScannerOutcome::Unavailable
        );
        assert_eq!(
            map_external_status(ExternalScanStatus::Error, 0, 1),
            ScannerOutcome::Error
        );
        assert_eq!(
            map_external_status(ExternalScanStatus::Passed, 0, 1),
            ScannerOutcome::Error
        );
    }

    #[test]
    fn secret_report_conversion_preserves_scanner_identity() {
        let report = SecretScanner::new()
            .scan(&ScanRequest::new(), &ScanCancellation::new())
            .expect("scan");
        assert_eq!(report.status(), ScanStatus::Clean);
        let result = ScanGateResult::from_secret_report(&report);
        assert_eq!(result.scanner_id(), &ScannerId::secrets());
        assert_eq!(result.outcome(), ScannerOutcome::Clean);

        let policy = apply_policy(&[(ScannerId::secrets(), FindingsDisposition::Block)]);
        let verdict = evaluate_scan_gate(&policy, &[result]);
        assert!(verdict.allows_apply());
    }

    #[test]
    fn command_findings_block_pre_action_without_waiver() {
        let result = findings(ScannerId::command(), 1);
        let policy = ScanGatePolicy::new(GatePhase::PreAction)
            .require(ScannerId::command(), FindingsDisposition::Block)
            .expect("require");
        let blocked = evaluate_scan_gate(&policy, std::slice::from_ref(&result));
        assert_eq!(blocked.phase(), GatePhase::PreAction);
        assert_eq!(blocked.disposition(), GateDisposition::Block);
        assert!(!blocked.allows_apply());

        let waived = policy
            .waive(waiver(ScannerId::command(), "AUD-018-CMD"))
            .expect("waive");
        let allowed = evaluate_scan_gate(&waived, &[result]);
        assert_eq!(allowed.disposition(), GateDisposition::Pass);
        assert!(allowed.allows_apply());
    }

    #[test]
    fn debug_and_display_omit_finding_payload() {
        let policy = apply_policy(&[(ScannerId::secrets(), FindingsDisposition::Block)]);
        let verdict = evaluate_scan_gate(&policy, &[findings(ScannerId::secrets(), 1)]);
        let rendered = format!("{verdict:?}{verdict}");
        assert!(!rendered.contains("AKIA"));
        assert!(!rendered.contains("sk_live"));
        assert!(!rendered.contains('\u{1b}'));
        assert!(rendered.contains("block"));
    }

    #[test]
    fn empty_required_set_passes_without_granting_from_results() {
        let policy = ScanGatePolicy::new(GatePhase::Apply);
        let verdict = evaluate_scan_gate(
            &policy,
            &[ScanGateResult::new(
                ScannerId::secrets(),
                ScannerOutcome::Error,
            )],
        );
        assert_eq!(verdict.disposition(), GateDisposition::Pass);
        assert!(verdict.reasons().is_empty());
    }
}
