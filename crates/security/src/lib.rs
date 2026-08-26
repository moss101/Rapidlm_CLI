#![forbid(unsafe_code)]

pub mod doctor;
pub mod hardening;
pub mod gate;
pub mod network_policy;
pub mod output_safety;
pub mod redaction;
pub mod scanners {
    pub mod command;
    pub mod external;
    pub mod patch;
    pub mod secrets;
}

pub use doctor::{
    CredentialObservation, DoctorCheck, DoctorCheckId, DoctorError, DoctorReport, DoctorRequest,
    DoctorStatus, ExecutableConfigClass, MAX_EXECUTABLE_SURFACES, MAX_METADATA_ENTRIES,
    MAX_METADATA_KEY_BYTES, MAX_METADATA_VALUE_BYTES, MAX_POLICY_DOCUMENTS,
    MAX_RELEASE_LABEL_BYTES, MAX_SANDBOX_TIERS, PolicyDocumentInput, ProjectConfigObservation,
    ProjectTrustState, ReleaseLabel, ReleaseSignatureObservation, SafeMetadata, SandboxObservation,
    SandboxTierSnapshot, evaluate_doctor,
};
pub use gate::{
    FindingsDisposition, GateAuditId, GateDisposition, GateError, GatePhase, GateReason,
    GateReasonCode, GateVerdict, MAX_AUDIT_ID_BYTES, MAX_GATE_RESULTS, MAX_GATE_WAIVERS,
    MAX_REQUIRED_SCANNERS, PolicyException, RequiredScanner, ScanGatePolicy, ScanGateResult,
    ScanWaiver, ScannerId, ScannerOutcome, evaluate_scan_gate,
};
pub use network_policy::{
    ConnectLease, ConnectLeaseId, ConsumedConnect, EgressAuditRecord, EgressDenial, EgressError,
    EgressHost, EgressMode, EgressOutcome, EgressPolicy, EgressProxy, EgressReason, EgressRule,
    MAX_AUDIT_RECORDS, MAX_EGRESS_RULES, NetworkClient, authorize_connect,
};
pub use output_safety::{
    MAX_ARTIFACT_METADATA_FIELD_BYTES, MAX_CONTROL_SEQUENCE_BYTES, MAX_LOG_EXPORT_BYTES,
    OutputChannel, OutputSafetyCancellation, OutputSafetyError, OutputSafetyStatus, SafeOutput,
    StreamingOutputFilter, filter_output, safe_artifact_metadata_field, safe_json_for_export,
    safe_text_for_terminal_or_json,
};
pub use redaction::{
    EncodingKind, MAX_DERIVED_VARIANT_BYTES, MAX_REDACT_CHUNK_BYTES, MAX_REGISTERED_SECRETS,
    PatternBuildReport, RedactedOutput, RedactionCancellation, RedactionError, RedactionSnapshot,
    RedactionStatus, SecretFingerprint, SecretRedactionRegistry, StreamingRedactor, TextSink,
};
pub use scanners::command::{
    CommandFinding, CommandFindingCategory, CommandFindingConfidence, CommandFindingFingerprint,
    CommandFindingSeverity, CommandRiskScanner, CommandRiskTag, CommandScanCancellation,
    CommandScanCoverage, CommandScanError, CommandScanReport, CommandScanStatus,
    MAX_COMMAND_FINDINGS, MAX_NESTING, MAX_SCAN_ARG_BYTES, MAX_SCAN_ARGV, MAX_SCAN_SCRIPT_BYTES,
    MAX_SCAN_TOKENS, MAX_UNWRAP, SCANNER_ID as COMMAND_SCANNER_ID,
    SCANNER_VERSION as COMMAND_SCANNER_VERSION,
};
pub use scanners::external::{
    ADAPTER_ID, ADAPTER_VERSION, ExternalByteRange, ExternalExecKind, ExternalExecResult,
    ExternalFinding, ExternalFindingCategory, ExternalFindingConfidence,
    ExternalFindingFingerprint, ExternalFindingSeverity, ExternalSandboxEvidence,
    ExternalSandboxPlan, ExternalScanCancellation, ExternalScanCoverage, ExternalScanError,
    ExternalScanReport, ExternalScanRequest, ExternalScanStatus, ExternalScannerAdapter,
    ExternalScannerConfig, ExternalScannerKind, MAX_EXTERNAL_FINDINGS, MAX_SARIF_BYTES,
    MAX_SCANNER_ARG_BYTES, MAX_SCANNER_ARGV, MAX_SCANNER_ID_BYTES, SCAN_OUT_MOUNT,
    SupervisedScannerExec, WORKSPACE_MOUNT,
};
pub use scanners::patch::{
    MAX_FINDINGS as MAX_PATCH_FINDINGS, MAX_SCAN_BYTES as MAX_PATCH_SCAN_BYTES,
    MAX_SCAN_TARGETS as MAX_PATCH_SCAN_TARGETS, MAX_TARGET_BYTES as MAX_PATCH_TARGET_BYTES,
    PatchByteRange, PatchChangeKind, PatchContentClass, PatchFinding, PatchFindingCategory,
    PatchFindingConfidence, PatchFindingFingerprint, PatchFindingSeverity, PatchScanCancellation,
    PatchScanCoverage, PatchScanError, PatchScanReport, PatchScanRequest, PatchScanStatus,
    PatchScanTarget, PatchScanner, SCANNER_ID as PATCH_SCANNER_ID,
    SCANNER_VERSION as PATCH_SCANNER_VERSION,
};
pub use scanners::secrets::{
    AppliedSuppression, ByteRange, CanaryFingerprint, Finding, FindingCategory, FindingConfidence,
    FindingFingerprint, FindingSeverity, MAX_CANARIES, MAX_FINDINGS, MAX_SCAN_BYTES,
    MAX_SCAN_TARGETS, MAX_SUPPRESSION_REASON_BYTES, MAX_SUPPRESSIONS, MAX_TARGET_BYTES, SCANNER_ID,
    SCANNER_VERSION, ScanCancellation, ScanCoverage, ScanError, ScanReport, ScanRequest,
    ScanStatus, ScanTarget, ScanTargetKind, SecretScanner, Suppression, SuppressionSource,
};
