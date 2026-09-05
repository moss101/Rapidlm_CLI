//! `rapid scan`: run configured external SAST/SCA/container scanners
//! (`security::scanners::external::ExternalScannerAdapter`, previously
//! fully built and tested but with zero callers anywhere in this binary —
//! Modbit `VER-009`'s `ExternalFinding` half) inside the same
//! capability-broker-lease + sandbox ceremony `sandbox_exec.rs::
//! run_sandboxed` already uses for `shell_exec(sandbox: true)`, and combine
//! their results through `security::gate::evaluate_scan_gate` — the
//! crate's own generic multi-scanner gate aggregator, itself also dormant
//! until now.
//!
//! Deliberately its own opt-in command, not a hook on every write the way
//! the always-on, free, in-process secrets/patch/command scanners already
//! are in `exec_tools.rs`: an external scanner is a real subprocess that
//! may not even be installed (`ExternalScanStatus::Unavailable` is the
//! expected, common, non-error outcome on a machine without it configured)
//! and can be slow — invoking one on every tool call would be a real
//! latency cost most turns shouldn't pay.

use std::path::Path;
use std::time::Duration;

use capability_broker::{CancellationToken, LeaseIssuer, LeaseValidator};
use security::{
    ExternalExecKind, ExternalExecResult, ExternalFinding, ExternalSandboxPlan, ExternalScanError,
    ExternalScanReport, ExternalScanRequest, ExternalScanStatus, ExternalScannerAdapter,
    ExternalScannerConfig, ExternalScannerKind, FindingsDisposition, GatePhase, GateVerdict,
    ScanGatePolicy, ScanGateResult, ScannerOutcome, SupervisedScannerExec, evaluate_scan_gate,
};
use security::gate::ScannerId;

/// Workspace-relative path of the external-scanner configuration.
pub const SCANNERS_CONFIG_PATH: &str = ".rapidlm/scanners.json";

/// Read cap for `scanners.json`, matching `findings_store.rs`'s own bound
/// for the same "project-local advisory config" bucket — a handful of
/// scanner definitions never legitimately approaches this.
pub const MAX_SCANNERS_CONFIG_BYTES: usize = 256 * 1024;

/// One configured external scanner, parsed from `.rapidlm/scanners.json`.
#[derive(Clone, Debug)]
pub struct ScannerEntry {
    config: ExternalScannerConfig,
    on_findings: FindingsDisposition,
}

impl ScannerEntry {
    pub fn config(&self) -> &ExternalScannerConfig {
        &self.config
    }

    pub fn on_findings(&self) -> FindingsDisposition {
        self.on_findings
    }
}

/// Typed failure loading or parsing `.rapidlm/scanners.json`. A missing
/// file is not this error (see `load_scanners_config`); only a file that
/// exists but can't be read or doesn't parse reaches here.
#[derive(Debug)]
pub enum ScannersConfigError {
    Io(std::io::Error),
    Malformed(String),
}

impl std::fmt::Display for ScannersConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "could not read {SCANNERS_CONFIG_PATH}: {err}"),
            Self::Malformed(reason) => write!(f, "{SCANNERS_CONFIG_PATH} is malformed: {reason}"),
        }
    }
}

/// Load `.rapidlm/scanners.json`. A missing file is normal and common
/// (nothing configured yet) and returns an empty list, not an error —
/// but unlike `FindingsStore`'s fail-open convention for advisory state, a
/// file that exists but is malformed IS an error here: a user who wrote
/// this file meant for it to configure real scanning, and silently
/// treating a typo as "no scanners" would hide exactly the kind of
/// misconfiguration this command exists to catch.
pub fn load_scanners_config(root: &Path) -> Result<Vec<ScannerEntry>, ScannersConfigError> {
    let path = root.join(SCANNERS_CONFIG_PATH);
    let bytes = match crate::exec_tools::read_file_bounded(&path, MAX_SCANNERS_CONFIG_BYTES) {
        Ok(bytes) => bytes,
        Err(crate::exec_tools::BoundedReadError::Io(err))
            if err.kind() == std::io::ErrorKind::NotFound =>
        {
            return Ok(Vec::new());
        }
        Err(crate::exec_tools::BoundedReadError::Io(err)) => {
            return Err(ScannersConfigError::Io(err));
        }
        Err(crate::exec_tools::BoundedReadError::TooLarge) => {
            return Err(ScannersConfigError::Malformed(format!(
                "exceeds {MAX_SCANNERS_CONFIG_BYTES} bytes"
            )));
        }
    };
    parse_scanners_config(&bytes)
}

fn parse_scanners_config(bytes: &[u8]) -> Result<Vec<ScannerEntry>, ScannersConfigError> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|err| ScannersConfigError::Malformed(err.to_string()))?;
    let scanners = value
        .get("scanners")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| ScannersConfigError::Malformed("missing \"scanners\" array".to_owned()))?;
    let mut out = Vec::with_capacity(scanners.len());
    for (index, entry) in scanners.iter().enumerate() {
        out.push(
            parse_scanner_entry(entry)
                .map_err(|reason| ScannersConfigError::Malformed(format!("scanners[{index}]: {reason}")))?,
        );
    }
    Ok(out)
}

fn parse_scanner_entry(value: &serde_json::Value) -> Result<ScannerEntry, String> {
    let obj = value.as_object().ok_or("expected an object")?;
    let id = obj
        .get("id")
        .and_then(serde_json::Value::as_str)
        .ok_or("missing \"id\"")?;
    let kind = match obj.get("kind").and_then(serde_json::Value::as_str) {
        Some("sast") => ExternalScannerKind::Sast,
        Some("sca") => ExternalScannerKind::Sca,
        Some("container") => ExternalScannerKind::Container,
        Some(other) => return Err(format!("unknown \"kind\": {other:?}")),
        None => return Err("missing \"kind\"".to_owned()),
    };
    let argv: Vec<String> = obj
        .get("argv")
        .and_then(serde_json::Value::as_array)
        .ok_or("missing \"argv\" array")?
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| "argv entries must be strings".to_owned())
        })
        .collect::<Result<_, _>>()?;
    // Fixed to the only backend `sandbox_exec::build_manager` registers —
    // `ExternalScannerConfig::new`'s own default (`SandboxTier::Container`)
    // would otherwise fail closed with `Unavailable` on every real run,
    // since no container backend is registered in this binary.
    let mut config = ExternalScannerConfig::new(id, kind, argv)
        .map_err(|err| err.to_string())?
        .with_tier(protocol::SandboxTier::HostRestricted);
    if let Some(secs) = obj.get("timeout_secs").and_then(serde_json::Value::as_u64) {
        config = config
            .with_timeout(Duration::from_secs(secs))
            .map_err(|err| err.to_string())?;
    }
    if let Some(limit) = obj.get("output_limit_bytes").and_then(serde_json::Value::as_u64) {
        config = config.with_output_limit(limit).map_err(|err| err.to_string())?;
    }
    let on_findings = match obj.get("on_findings").and_then(serde_json::Value::as_str) {
        Some("warn") => FindingsDisposition::Warn,
        Some("ask") => FindingsDisposition::Ask,
        Some("block") | None => FindingsDisposition::Block,
        Some(other) => return Err(format!("unknown \"on_findings\": {other:?}")),
    };
    Ok(ScannerEntry { config, on_findings })
}

/// Runs a planned scan through the same capability-broker lease + sandbox
/// ceremony `sandbox_exec.rs::run_sandboxed` uses. Reads the scanner's
/// SARIF report from its combined stdout+stderr: `SCAN_OUT_MOUNT`
/// (`security::SCAN_OUT_MOUNT`) is a declared, least-privilege `Temp` mount
/// in the planned `SandboxSpec`, but `HostRestrictedBackend` — the only
/// backend registered here — never materializes a `Temp` mount to a real
/// host path (confirmed against `crates/sandbox`'s own source before
/// building this), so a configured scanner's argv must emit SARIF on
/// stdout (e.g. Semgrep's own default, or `--output -`), not to a file.
pub struct SandboxedScannerExec {
    root: std::path::PathBuf,
}

impl SandboxedScannerExec {
    pub fn new(root: std::path::PathBuf) -> Self {
        Self { root }
    }
}

impl SupervisedScannerExec for SandboxedScannerExec {
    fn run(
        &self,
        plan: &ExternalSandboxPlan,
        cancel: &CancellationToken,
    ) -> Result<ExternalExecResult, ExternalScanError> {
        let argv = plan.exec().argv();
        let program = argv.first().ok_or(ExternalScanError::EmptyArgv)?;
        // `HostRestrictedBackend` deliberately refuses a relative/unresolved
        // program the way `std::process::Command` would implicitly search
        // `$PATH` for — `resolve_program` is the exact same resolution
        // `sandbox_exec.rs::run_sandboxed` already applies before handing
        // argv to the sandbox, reused here rather than duplicated. A
        // scanner whose configured binary can't be found is `Unavailable`,
        // not a hard error — the same observable outcome as it not being
        // installed at all, which is the expected common case.
        let resolved_program = crate::sandbox_exec::resolve_program(&self.root, program)
            .map_err(|_| ExternalScanError::Unavailable)?;
        let resolved_argv: Vec<String> = std::iter::once(resolved_program)
            .chain(argv.iter().skip(1).cloned())
            .collect();
        let request = sandbox::SandboxExecRequest::new(
            resolved_argv,
            plan.exec().timeout(),
            plan.exec().output_limit(),
        )
        .map_err(map_sandbox_error)?;

        let manager = crate::sandbox_exec::build_manager();
        let issuer = LeaseIssuer::ephemeral();
        // The lease's `ProcessScope` names the *originally configured*
        // command (unresolved), matching `run_sandboxed`'s own precedent —
        // the resolved absolute path is an implementation detail of how
        // the command actually runs, not part of what was authorized.
        let (lease, revision) = crate::sandbox_exec::mint_proc_exec_lease(&issuer, program)
            .map_err(|_| ExternalScanError::Unavailable)?;
        let validator = LeaseValidator::new(issuer, revision);
        let handle = manager
            .prepare(plan.spec(), &lease, cancel)
            .map_err(map_sandbox_error)?;
        let result = manager.exec(plan.spec(), &handle, &request, &lease, &validator, cancel);
        // Best-effort cleanup, same rationale as `run_sandboxed`: a destroy
        // failure after a successful/failed exec is the backend's own
        // resource-leak concern, never something that should mask the
        // exec outcome the scan actually cares about.
        let _ = manager.destroy(&handle, cancel);
        let result = result.map_err(map_sandbox_error)?;
        let exit = result.exit();
        let kind = if exit.timed_out() {
            ExternalExecKind::TimedOut
        } else if let Some(exit_code) = exit.code() {
            ExternalExecKind::Completed { exit_code }
        } else {
            // Signalled, not a timeout: OOM, a pid-count kill, or any other
            // signal death all collapse to Crashed — `ExternalScanReport`
            // has no field for "why", only whether the run produced a
            // usable result at all.
            ExternalExecKind::Crashed
        };
        ExternalExecResult::new(kind, result.output().to_vec())
    }
}

fn map_sandbox_error(err: sandbox::SandboxError) -> ExternalScanError {
    match err {
        sandbox::SandboxError::Cancelled => ExternalScanError::Cancelled,
        _ => ExternalScanError::Unavailable,
    }
}

/// One scanner's outcome, findings already filtered against a dismissal
/// store the same way every other scanner in `exec_tools.rs` filters its
/// own advisory findings — a dismissed finding stays visible in the raw
/// report but never counts toward the gate's block decision.
pub struct ScanOutcome {
    pub scanner_id: String,
    pub report: ExternalScanReport,
    pub undismissed: Vec<ExternalFinding>,
}

/// Run every configured scanner and combine the results through
/// `evaluate_scan_gate`. `is_dismissed` is injected rather than taking a
/// `FindingsStore` directly so this stays testable without touching disk.
pub fn run_configured_scanners(
    entries: &[ScannerEntry],
    root: &Path,
    is_dismissed: impl Fn(&str) -> bool,
    cancel: &CancellationToken,
) -> Result<(GateVerdict, Vec<ScanOutcome>), ExternalScanError> {
    let host_str = root.to_str().ok_or(ExternalScanError::InvalidWorkspace)?;
    let workspace = capability_broker::CanonicalHostPath::from_resolved(host_str)
        .map_err(|_| ExternalScanError::InvalidWorkspace)?;
    let adapter = ExternalScannerAdapter::new();
    let exec = SandboxedScannerExec::new(root.to_path_buf());
    let mut policy = ScanGatePolicy::new(GatePhase::PreAction);
    let mut results = Vec::with_capacity(entries.len());
    let mut outcomes = Vec::with_capacity(entries.len());
    for entry in entries {
        let request = ExternalScanRequest::new(entry.config.clone(), workspace.clone());
        let report = adapter.scan(&request, Some(&exec), cancel)?;
        let scanner_id = ScannerId::parse(entry.config.id())
            .map_err(|_| ExternalScanError::InvalidScannerId)?;
        policy = policy
            .require(scanner_id.clone(), entry.on_findings)
            .map_err(|_| ExternalScanError::InvalidScannerId)?;

        let undismissed: Vec<ExternalFinding> = report
            .findings()
            .iter()
            .filter(|finding| !is_dismissed(finding.fingerprint().as_hex()))
            .cloned()
            .collect();
        // Findings existed, but every one of them is already dismissed:
        // the gate sees Clean, not Findings — a fully-triaged finding set
        // must not keep blocking the same way a resolved secrets/patch
        // advisory doesn't resurface after `rapid findings dismiss`.
        let result = if report.status() == ExternalScanStatus::Failed && undismissed.is_empty() {
            ScanGateResult::new(scanner_id.clone(), ScannerOutcome::Clean)
        } else {
            ScanGateResult::from_external_report(scanner_id.clone(), &report)
                .with_finding_count(undismissed.len())
        };
        results.push(result);
        outcomes.push(ScanOutcome {
            scanner_id: entry.config.id().to_owned(),
            report,
            undismissed,
        });
    }
    let verdict = evaluate_scan_gate(&policy, &results);
    Ok((verdict, outcomes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rejects_missing_scanners_array() {
        let err = parse_scanners_config(b"{}").expect_err("missing scanners array");
        assert!(matches!(err, ScannersConfigError::Malformed(_)));
    }

    #[test]
    fn parse_reads_a_full_entry_and_defaults_on_findings_to_block() {
        let json = br#"{
            "schema": 1,
            "scanners": [
                {"id": "semgrep", "kind": "sast", "argv": ["semgrep", "--sarif", "--quiet"]}
            ]
        }"#;
        let entries = parse_scanners_config(json).expect("parse");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].config().id(), "semgrep");
        assert_eq!(entries[0].config().kind(), ExternalScannerKind::Sast);
        assert_eq!(entries[0].on_findings(), FindingsDisposition::Block);
    }

    #[test]
    fn parse_reads_explicit_timeout_output_limit_and_disposition() {
        let json = br#"{
            "schema": 1,
            "scanners": [
                {
                    "id": "trivy",
                    "kind": "sca",
                    "argv": ["trivy", "fs", "--format", "sarif"],
                    "timeout_secs": 30,
                    "output_limit_bytes": 1048576,
                    "on_findings": "warn"
                }
            ]
        }"#;
        let entries = parse_scanners_config(json).expect("parse");
        assert_eq!(entries[0].config().timeout(), Duration::from_secs(30));
        assert_eq!(entries[0].config().output_limit(), 1_048_576);
        assert_eq!(entries[0].on_findings(), FindingsDisposition::Warn);
    }

    #[test]
    fn parse_rejects_unknown_kind_and_on_findings() {
        let bad_kind = br#"{"scanners":[{"id":"x","kind":"nope","argv":["x"]}]}"#;
        assert!(parse_scanners_config(bad_kind).is_err());
        let bad_disposition =
            br#"{"scanners":[{"id":"x","kind":"sast","argv":["x"],"on_findings":"nope"}]}"#;
        assert!(parse_scanners_config(bad_disposition).is_err());
    }

    #[test]
    fn load_missing_config_is_an_empty_list_not_an_error() {
        let root = std::env::temp_dir().join(format!(
            "rapidlm-scanners-config-missing-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let entries = load_scanners_config(&root).expect("missing file is not an error");
        assert!(entries.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn load_malformed_config_is_a_real_error() {
        let root = std::env::temp_dir().join(format!(
            "rapidlm-scanners-config-malformed-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".rapidlm")).expect("dir");
        std::fs::write(root.join(SCANNERS_CONFIG_PATH), b"not json").expect("write");
        let err = load_scanners_config(&root).expect_err("malformed file must error, not go silent");
        assert!(matches!(err, ScannersConfigError::Malformed(_)));
        let _ = std::fs::remove_dir_all(&root);
    }

    fn temp_workspace(tag: &str) -> std::path::PathBuf {
        let root =
            std::env::temp_dir().join(format!("rapidlm-external-scan-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        std::fs::canonicalize(&root).expect("canonicalize")
    }

    fn sh_scanner(id: &str, sarif_body: &str) -> ScannerEntry {
        let config = ExternalScannerConfig::new(
            id,
            ExternalScannerKind::Sast,
            ["sh", "-c", &format!("printf '%s' '{sarif_body}'")],
        )
        .expect("config")
        .with_tier(protocol::SandboxTier::HostRestricted);
        ScannerEntry {
            config,
            on_findings: FindingsDisposition::Block,
        }
    }

    const CLEAN_SARIF: &str =
        r#"{"version":"2.1.0","runs":[{"tool":{"driver":{"name":"fakescan"}},"results":[]}]}"#;

    fn finding_sarif() -> String {
        r#"{"version":"2.1.0","runs":[{"tool":{"driver":{"name":"fakescan"}},"results":[{"ruleId":"no-eval","level":"error","message":{"text":"eval is unsafe"},"locations":[{"physicalLocation":{"artifactLocation":{"uri":"src/app.rs"},"region":{"byteOffset":10,"byteLength":4}}}]}]}]}"#.to_owned()
    }

    #[test]
    fn a_real_sandboxed_scanner_with_no_findings_passes_the_gate() {
        let root = temp_workspace("clean");
        let entry = sh_scanner("fakescan", CLEAN_SARIF);
        let (verdict, outcomes) = run_configured_scanners(
            &[entry],
            &root,
            |_fp| false,
            &CancellationToken::new(),
        )
        .expect("scan");
        assert!(verdict.is_pass(), "{verdict}");
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].report.status(), ExternalScanStatus::Passed);
        assert!(outcomes[0].undismissed.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_real_sandboxed_scanner_with_a_finding_blocks_the_gate_by_default() {
        let root = temp_workspace("finding");
        let entry = sh_scanner("fakescan", &finding_sarif());
        let (verdict, outcomes) = run_configured_scanners(
            &[entry],
            &root,
            |_fp| false,
            &CancellationToken::new(),
        )
        .expect("scan");
        assert_eq!(verdict.disposition(), security::GateDisposition::Block, "{verdict}");
        assert_eq!(outcomes[0].report.status(), ExternalScanStatus::Failed);
        assert_eq!(outcomes[0].undismissed.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dismissing_the_only_finding_turns_the_gate_clean_again() {
        let root = temp_workspace("dismissed");
        let entry = sh_scanner("fakescan", &finding_sarif());
        let (verdict, outcomes) = run_configured_scanners(
            &[entry],
            &root,
            |_fp| true, // every fingerprint is already dismissed
            &CancellationToken::new(),
        )
        .expect("scan");
        assert!(
            verdict.is_pass(),
            "a fully-dismissed finding set must not keep blocking: {verdict}"
        );
        // The raw report still shows the finding — dismissal only affects
        // the gate decision, never hides what was actually found.
        assert_eq!(outcomes[0].report.status(), ExternalScanStatus::Failed);
        assert_eq!(outcomes[0].report.findings().len(), 1);
        assert!(outcomes[0].undismissed.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_warn_disposition_finds_but_does_not_block() {
        let root = temp_workspace("warn");
        let mut entry = sh_scanner("fakescan", &finding_sarif());
        entry.on_findings = FindingsDisposition::Warn;
        let (verdict, _outcomes) = run_configured_scanners(
            &[entry],
            &root,
            |_fp| false,
            &CancellationToken::new(),
        )
        .expect("scan");
        assert_eq!(verdict.disposition(), security::GateDisposition::Warn, "{verdict}");
        assert!(verdict.allows_apply());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_uninstalled_scanner_reports_unavailable_not_an_error() {
        let root = temp_workspace("missing-binary");
        let config = ExternalScannerConfig::new(
            "does-not-exist",
            ExternalScannerKind::Sast,
            ["rapidlm-definitely-not-a-real-binary-xyz"],
        )
        .expect("config")
        .with_tier(protocol::SandboxTier::HostRestricted);
        let entry = ScannerEntry {
            config,
            on_findings: FindingsDisposition::Block,
        };
        let (verdict, outcomes) = run_configured_scanners(
            &[entry],
            &root,
            |_fp| false,
            &CancellationToken::new(),
        )
        .expect("scan");
        assert_eq!(
            verdict.disposition(),
            security::GateDisposition::Block,
            "unavailable must never silently become pass: {verdict}"
        );
        assert_eq!(outcomes[0].report.status(), ExternalScanStatus::Unavailable);
        let _ = std::fs::remove_dir_all(&root);
    }
}
