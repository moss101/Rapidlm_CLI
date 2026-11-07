//! Staged workspace adapter for the multi-file publication coordinator.
//! Host path locks and the journal coordinate participating
//! RapidLM writers; external filesystem races remain outside that guarantee.
use super::recovery::{JOURNAL_PATH, WorkspaceRecovery};
use super::*;
use workspace::multi_edit_journal::JournalStore;
use workspace::multi_edit_publish::{
    self, JournaledPrepareError, JournaledPublishIo, PublishFailure, PublishIo, PublishOutcome,
};
use workspace::multi_edit_recover::RecoveryIo;
use workspace::multi_edit_recovery::{
    FileImage, RecoveryEntry, RecoveryRecord, RecoveryState, StageIdentity,
};

#[cfg(all(test, unix))]
pub(super) fn pause_at_crash_boundary(root: &Path, boundary: &str) {
    if std::env::var("RAPID_TEST_MULTI_CRASH_POINT").as_deref() != Ok(boundary) {
        return;
    }
    fs::write(root.join("child-ready"), boundary).unwrap();
    std::thread::sleep(Duration::from_secs(60));
    panic!("crash test was not terminated at {boundary}");
}

#[derive(Debug, Eq, PartialEq)]
enum MultiPatchError {
    Invalid,
    Cancelled,
    FormatterUncertain,
    File {
        index: usize,
        error: WorkspaceRecoveryError,
    },
    Patch {
        index: usize,
        error: OrderedPatchError,
    },
}

#[derive(Debug, Eq, PartialEq)]
enum MultiPatchRunError {
    Prepare(MultiPatchError),
    Workspace(WorkspaceRecoveryError),
}

#[derive(Debug, Eq, PartialEq)]
#[allow(dead_code)]
enum MultiApprovalProgress {
    Waiting,
    Ready(MultiApprovalGrant),
}

/// A short-lived capability constructed only after durable per-path checks.
/// Publication revalidates the exact call and candidate before using it.
#[derive(Debug, Eq, PartialEq)]
struct MultiApprovalGrant {
    candidate_digest: String,
    call_id: String,
    arguments_digest: String,
    approved_paths: std::collections::BTreeSet<usize>,
}

impl MultiApprovalGrant {
    fn new(
        record: &RecoveryRecord,
        call: &ValidatedToolCall,
        approved_paths: std::collections::BTreeSet<usize>,
    ) -> Result<Self, MultiApprovalError> {
        let encoded = record
            .encode()
            .map_err(|_| MultiApprovalError::InvalidApproval)?;
        Ok(Self {
            candidate_digest: sha256_hex(&encoded),
            call_id: call.call_id().to_owned(),
            arguments_digest: crate::approvals::arguments_digest(call.arguments()),
            approved_paths,
        })
    }
    fn matches(&self, record: &RecoveryRecord, call: &ValidatedToolCall) -> bool {
        call.tool() == "workspace_multi_patch"
            && self.call_id == call.call_id()
            && self.arguments_digest == crate::approvals::arguments_digest(call.arguments())
            && record
                .encode()
                .is_ok_and(|bytes| self.candidate_digest == sha256_hex(&bytes))
            && self
                .approved_paths
                .iter()
                .all(|index| *index < record.entries().len())
    }
}

#[derive(Debug, Eq, PartialEq)]
#[allow(dead_code)]
enum MultiApprovalError {
    Policy(WorkspaceRecoveryError),
    Workspace(WorkspaceRecoveryError),
    Prepare(MultiPatchError),
    Unavailable,
    InvalidApproval,
    Checkpoint,
    ShadowFailed(String),
    ShadowUncertain(String),
    PublicationFailed(String),
}

#[cfg(unix)]
enum FormattedProgress {
    Waiting,
    NoChanges(String),
    Ready(Box<FormattedReady>),
}

#[cfg(unix)]
struct FormattedReady {
    record: RecoveryRecord,
    grant: MultiApprovalGrant,
    notes: String,
    chain: super::multi_approval::PathApprovalChain,
}

/// A rewritten multi-file call is only a candidate: each new path still
/// needs its own approval before the hook coordinator can publish it.
#[cfg(unix)]
#[allow(dead_code)] // Used by the pending pre-hook coordinator.
struct MultiHookRewrite {
    call: ValidatedToolCall,
    candidate: RecoveryRecord,
    asks: std::collections::BTreeSet<usize>,
    marker: String,
    provenance: HookRewriteRecord,
}

#[cfg(unix)]
#[allow(dead_code)] // The public dispatcher remains gated until lifecycle integration.
enum MultiHookProgress {
    Waiting,
    Denied(String),
    Ready(Box<MultiHookReady>),
}

#[cfg(unix)]
#[allow(dead_code)]
struct MultiHookReady {
    original: RecoveryRecord,
    candidate: RecoveryRecord,
    grant: MultiApprovalGrant,
    chain: super::multi_approval::PathApprovalChain,
    report: crate::hooks::PreHookReport,
    event_binding: String,
    rewrite: Option<MultiHookRewrite>,
    latest_matches: bool,
}

#[cfg(unix)]
#[derive(Debug, Eq, PartialEq)]
enum MultiHookRewriteError {
    Invalid,
    Prepare(MultiPatchError),
    Policy(WorkspaceRecoveryError),
}

#[derive(Debug, Eq, PartialEq)]
#[allow(dead_code)]
enum ApprovedMultiPatchOutcome {
    Waiting,
    Denied(String),
    NoChanges(String),
    Published(PublishOutcome<WorkspaceRecoveryError>, String),
}

/// A formatter can undo an edit. Preserve its original index for the approval
/// flow, while excluding the no-op from the publication journal.
#[allow(dead_code)] // Consumed by the pending formatter approval orchestration.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct FormattedMultiCandidate {
    pub(super) record: Option<RecoveryRecord>,
    pub(super) original_indices: Vec<usize>,
    pub(super) notes: String,
}

pub(super) struct MultiFileChange {
    path: String,
    operation: MultiFileOperation,
}

enum MultiFileOperation {
    Patch(Vec<PatchEdit>),
    Create(String),
    Delete,
}

pub(super) fn parse_multi_patch_args(raw: &str) -> Result<Vec<MultiFileChange>, ()> {
    if raw.len() > agent_runtime::MAX_ARGUMENT_BYTES {
        return Err(());
    }
    let input: serde_json::Value = serde_json::from_str(raw).map_err(|_| ())?;
    let object = input
        .as_object()
        .filter(|object| object.len() == 1)
        .ok_or(())?;
    let files = object
        .get("files")
        .and_then(serde_json::Value::as_array)
        .filter(|files| {
            !files.is_empty() && files.len() <= workspace::multi_edit_recovery::MAX_RECOVERY_FILES
        })
        .ok_or(())?;
    files
        .iter()
        .map(|file| {
            let object = file.as_object().ok_or(())?;
            let path = object
                .get("path")
                .and_then(serde_json::Value::as_str)
                .ok_or(())?;
            checked_relative(path).map_err(|_| ())?;
            let operation = if object.contains_key("edits") {
                let patch = parse_patch_args(&file.to_string()).map_err(|_| ())?;
                if patch.edits.is_empty() {
                    return Err(());
                }
                MultiFileOperation::Patch(patch.edits)
            } else if object.contains_key("create") && object.len() == 2 {
                let content = object
                    .get("create")
                    .and_then(serde_json::Value::as_str)
                    .ok_or(())?;
                if content.len() > MAX_WRITE_BYTES {
                    return Err(());
                }
                MultiFileOperation::Create(content.to_owned())
            } else if object.len() == 2
                && object.get("delete").and_then(serde_json::Value::as_bool) == Some(true)
            {
                MultiFileOperation::Delete
            } else {
                return Err(());
            };
            Ok(MultiFileChange {
                path: path.to_owned(),
                operation,
            })
        })
        .collect()
}

/// Reconstruct the documented operations from recorded preimages without
/// consulting or changing the filesystem. Used to bind durable hook questions
/// to the exact candidate those arguments describe.
pub(super) fn candidate_matches_multi_patch(raw: &str, candidate: &RecoveryRecord) -> bool {
    let Ok(changes) = parse_multi_patch_args(raw) else {
        return false;
    };
    if candidate.state() != RecoveryState::Intent
        || candidate.has_staging_plan()
        || changes.len() != candidate.entries().len()
    {
        return false;
    }
    changes
        .into_iter()
        .zip(candidate.entries())
        .all(|(change, entry)| {
            if change.path != entry.path {
                return false;
            }
            match (&change.operation, &entry.before, &entry.after) {
                (MultiFileOperation::Create(content), None, Some(after)) => {
                    after.mode == Some(0o600) && after.bytes == content.as_bytes()
                }
                (MultiFileOperation::Delete, Some(_), None) => true,
                (MultiFileOperation::Patch(edits), Some(before), Some(after)) => {
                    let Ok(text) = String::from_utf8(before.bytes.clone()) else {
                        return false;
                    };
                    let Ok((updated, _)) =
                        apply_ordered_patches(text, edits, &CancellationToken::new())
                    else {
                        return false;
                    };
                    after.mode == before.mode && after.bytes == updated.as_bytes()
                }
                _ => false,
            }
        })
}

impl WorkspaceTools {
    #[cfg(unix)]
    fn multi_hook_result_detail(&self, detail: String, ready: &MultiHookReady) -> String {
        let mut detail = self.with_hook_context(detail, &ready.report.context);
        if let Some(rewrite) = &ready.rewrite {
            detail.push('\n');
            detail.push_str(&rewrite.marker);
        }
        detail
    }

    #[cfg(unix)]
    fn observe_multi_hook_rewrite(
        &self,
        call: &ValidatedToolCall,
        ready: &MultiHookReady,
    ) -> Result<(), MultiApprovalError> {
        if let (Some(rewrite), Some(events)) = (&ready.rewrite, &self.hook_events) {
            let key = sha256_hex(
                format!("rapidlm.multi-hook.rewrite.v1:{}", ready.event_binding).as_bytes(),
            );
            events
                .rewritten_once(call.tool(), call.call_id(), &rewrite.provenance, &key)
                .map_err(|_| MultiApprovalError::Unavailable)?;
        }
        Ok(())
    }

    #[cfg(unix)]
    fn retire_failed_hook_format(
        &self,
        call: &ValidatedToolCall,
        ready: &MultiHookReady,
    ) -> Result<(), MultiApprovalError> {
        use super::multi_format_checkpoint::{CheckpointState, FormatCheckpoint};
        let binding = self
            .write_steps
            .as_ref()
            .and_then(crate::write_steps::WriteSteps::formatter_approval_binding)
            .ok_or(MultiApprovalError::Checkpoint)?;
        let chain = ready
            .chain
            .for_formatter(&ready.candidate, call.arguments(), &binding)
            .map_err(|_| MultiApprovalError::Checkpoint)?;
        let key = chain
            .formatter_checkpoint_key()
            .map_err(|_| MultiApprovalError::Checkpoint)?;
        let journal = recovery::lock_journal_for_publication(self.root())
            .map_err(MultiApprovalError::Workspace)?;
        let checkpoint =
            FormatCheckpoint::new(&journal, &key).map_err(|_| MultiApprovalError::Checkpoint)?;
        match checkpoint
            .load(&ready.candidate)
            .map_err(|_| MultiApprovalError::Checkpoint)?
        {
            CheckpointState::Missing | CheckpointState::Retired => {}
            CheckpointState::Ready(result) => checkpoint
                .retire_ready(&ready.candidate, &result)
                .map_err(|_| MultiApprovalError::Checkpoint)?,
            CheckpointState::Running => return Err(MultiApprovalError::Checkpoint),
        }
        super::multi_hook_checkpoint::HookCheckpoint::new(
            &journal,
            &ready.chain,
            &ready.original,
            call.arguments(),
            &self.hooks.pre_tool_use,
        )
        .and_then(|checkpoint| checkpoint.retire(&ready.report))
        .map_err(|_| MultiApprovalError::Checkpoint)
    }

    #[cfg(unix)]
    fn retire_cancelled_candidates(
        &self,
        call: &ValidatedToolCall,
        effective: &RecoveryRecord,
        format_chain: Option<&super::multi_approval::PathApprovalChain>,
        hook: Option<&MultiHookReady>,
    ) -> Result<(), MultiApprovalError> {
        use super::multi_format_checkpoint::{CheckpointState, FormatCheckpoint};
        if format_chain.is_none() && hook.is_none() {
            return Ok(());
        }
        let journal = recovery::lock_journal_for_publication(self.root())
            .map_err(MultiApprovalError::Workspace)?;
        if let Some(chain) = format_chain {
            let key = chain
                .formatter_checkpoint_key()
                .map_err(|_| MultiApprovalError::Checkpoint)?;
            let checkpoint = FormatCheckpoint::new(&journal, &key)
                .map_err(|_| MultiApprovalError::Checkpoint)?;
            match checkpoint
                .load(effective)
                .map_err(|_| MultiApprovalError::Checkpoint)?
            {
                CheckpointState::Ready(result) => checkpoint
                    .retire_ready(effective, &result)
                    .map_err(|_| MultiApprovalError::Checkpoint)?,
                CheckpointState::Retired => {}
                CheckpointState::Missing | CheckpointState::Running => {
                    return Err(MultiApprovalError::Checkpoint);
                }
            }
        }
        if let Some(ready) = hook {
            super::multi_hook_checkpoint::HookCheckpoint::new(
                &journal,
                &ready.chain,
                &ready.original,
                call.arguments(),
                &self.hooks.pre_tool_use,
            )
            .and_then(|checkpoint| checkpoint.retire(&ready.report))
            .map_err(|_| MultiApprovalError::Checkpoint)?;
        }
        Ok(())
    }

    #[cfg(unix)]
    fn bind_multi_hook_pending(
        &self,
        raw: &RecoveryRecord,
        call: &ValidatedToolCall,
        chain: &super::multi_approval::PathApprovalChain,
        report: &crate::hooks::PreHookReport,
        request: &crate::approvals::ApprovalRequest,
    ) -> Result<(), MultiApprovalError> {
        let journal = recovery::lock_journal_for_publication(self.root())
            .map_err(MultiApprovalError::Workspace)?;
        super::multi_hook_checkpoint::HookCheckpoint::new(
            &journal,
            chain,
            raw,
            call.arguments(),
            &self.hooks.pre_tool_use,
        )
        .and_then(|checkpoint| checkpoint.bind_pending(report, request))
        .map_err(|_| MultiApprovalError::Checkpoint)
    }
    /// Advance all pre-hook decisions without publishing. Each resume is
    /// checked against exact durable requests, and the synced marker prevents
    /// repeating external hooks after an interrupted execution.
    #[cfg(unix)]
    #[allow(dead_code)] // Not exposed until formatter and checkpoint lifecycle integration.
    fn advance_multi_hook_approval(
        &self,
        raw: &RecoveryRecord,
        call: &ValidatedToolCall,
        latest: Option<&crate::approvals::ApprovedAsk>,
        cancel: &CancellationToken,
    ) -> Result<MultiHookProgress, MultiApprovalError> {
        self.advance_multi_hook_approval_inner(raw, call, latest, cancel, false)
    }

    /// A later formatter phase may identify the chain but must itself verify
    /// an unmatched latest answer before starting any formatter or granting.
    #[cfg(unix)]
    #[allow(dead_code)]
    fn advance_multi_hook_approval_inner(
        &self,
        raw: &RecoveryRecord,
        call: &ValidatedToolCall,
        latest: Option<&crate::approvals::ApprovedAsk>,
        cancel: &CancellationToken,
        allow_formatter_answer: bool,
    ) -> Result<MultiHookProgress, MultiApprovalError> {
        use super::multi_approval::{ApprovalPhase, MultiHookQuestion, PathApprovalChain};
        use super::multi_hook_checkpoint::{HookCheckpoint, State};

        if call.tool() != WORKSPACE_MULTI_PATCH_TOOL
            || self.hooks.pre_tool_use.is_empty()
            || !candidate_matches_multi_patch(call.arguments(), raw)
        {
            return Err(MultiApprovalError::InvalidApproval);
        }
        let mut raw_asks = std::collections::BTreeSet::new();
        for (index, entry) in raw.entries().iter().enumerate() {
            match self.check_multi_edit_permission(&entry.path) {
                Ok(()) => {}
                Err(WorkspaceRecoveryError::ApprovalRequired) => {
                    raw_asks.insert(index);
                }
                Err(error) => return Err(MultiApprovalError::Policy(error)),
            }
        }
        let chain = match latest {
            Some(answer) => PathApprovalChain::from_resolved_for_phase(
                raw,
                call.call_id(),
                call.arguments(),
                answer,
                ApprovalPhase::Plain,
                "",
            ),
            None => PathApprovalChain::new(raw, call.call_id(), call.arguments()),
        }
        .map_err(|_| MultiApprovalError::InvalidApproval)?;
        let sink = self.approval_sink.as_ref();
        if !raw_asks.is_empty() && sink.is_none() {
            return Err(MultiApprovalError::Unavailable);
        }
        let raw_approved = match sink {
            Some(sink) => chain
                .approved_indices(raw, call.arguments(), sink.as_ref())
                .map_err(|_| MultiApprovalError::Unavailable)?,
            None => Default::default(),
        };
        let matches_raw = latest.is_some_and(|answer| {
            raw_approved
                .iter()
                .any(|&index| chain.authorizes(index, answer))
        });
        if let Some(&index) = raw_asks.iter().find(|index| !raw_approved.contains(index)) {
            if latest.is_some() && !matches_raw {
                return Err(MultiApprovalError::InvalidApproval);
            }
            let request = chain
                .request(raw, index, call.arguments())
                .map_err(|_| MultiApprovalError::InvalidApproval)?;
            let sink = sink.ok_or(MultiApprovalError::Unavailable)?;
            let token = sink
                .request(&request)
                .map_err(|_| MultiApprovalError::Unavailable)?;
            self.observe_approval_request(&request, &token);
            return Ok(MultiHookProgress::Waiting);
        }
        cancel
            .check()
            .map_err(|_| MultiApprovalError::Prepare(MultiPatchError::Cancelled))?;
        let (saved, command_token) = {
            let journal = recovery::lock_journal_for_publication(self.root())
                .map_err(MultiApprovalError::Workspace)?;
            let checkpoint = HookCheckpoint::new(
                &journal,
                &chain,
                raw,
                call.arguments(),
                &self.hooks.pre_tool_use,
            )
            .map_err(|_| MultiApprovalError::Checkpoint)?;
            match checkpoint
                .load()
                .map_err(|_| MultiApprovalError::Checkpoint)?
            {
                State::Missing => {
                    if latest.is_some() && !matches_raw {
                        return Err(MultiApprovalError::InvalidApproval);
                    }
                    recovery::ensure_external_can_start(&journal)
                        .map_err(MultiApprovalError::Workspace)?;
                    checkpoint
                        .start()
                        .map_err(|_| MultiApprovalError::Checkpoint)?;
                    let token = recovery::begin_external(&journal, "hook")
                        .map_err(MultiApprovalError::Workspace)?;
                    (None, Some(token))
                }
                State::Ready(report) => (Some(*report), None),
                State::Running | State::Retired => return Err(MultiApprovalError::Checkpoint),
            }
        };
        let report = match saved {
            Some(report) => report,
            None => {
                let token = command_token
                    .as_deref()
                    .ok_or(MultiApprovalError::Checkpoint)?;
                let report = crate::hooks::run_pre_tool_stage_until(
                    &self.hooks.pre_tool_use,
                    call.tool(),
                    call.arguments(),
                    crate::hooks::HOOK_TIMEOUT,
                    Some(cancel),
                );
                let journal = recovery::lock_journal_for_publication(self.root())
                    .map_err(MultiApprovalError::Workspace)?;
                let checkpoint = HookCheckpoint::new(
                    &journal,
                    &chain,
                    raw,
                    call.arguments(),
                    &self.hooks.pre_tool_use,
                )
                .map_err(|_| MultiApprovalError::Checkpoint)?;
                match report {
                    Ok(report) => {
                        checkpoint
                            .finish(&report)
                            .map_err(|_| MultiApprovalError::Checkpoint)?;
                        recovery::finish_external(&journal, token)
                            .map_err(MultiApprovalError::Workspace)?;
                        report
                    }
                    Err(interruption) => {
                        if let Some(events) = self.hook_events.as_ref() {
                            for decision in &interruption.decisions {
                                events.decided(call.tool(), call.call_id(), decision);
                            }
                        }
                        if interruption.termination_uncertain {
                            return Err(MultiApprovalError::Checkpoint);
                        }
                        checkpoint
                            .retire_after_cancelled_execution()
                            .map_err(|_| MultiApprovalError::Checkpoint)?;
                        recovery::finish_external(&journal, token)
                            .map_err(MultiApprovalError::Workspace)?;
                        return Err(MultiApprovalError::Prepare(MultiPatchError::Cancelled));
                    }
                }
            }
        };
        let report_binding = {
            let journal = recovery::lock_journal_for_publication(self.root())
                .map_err(MultiApprovalError::Workspace)?;
            HookCheckpoint::new(
                &journal,
                &chain,
                raw,
                call.arguments(),
                &self.hooks.pre_tool_use,
            )
            .and_then(|checkpoint| checkpoint.approval_binding(&report))
            .map_err(|_| MultiApprovalError::Checkpoint)?
        };
        if let Some(events) = self.hook_events.as_ref() {
            for (index, decision) in report.decisions.iter().enumerate() {
                let key = sha256_hex(
                    format!("rapidlm.multi-hook.decision.v1:{report_binding}:{index}").as_bytes(),
                );
                events
                    .decided_once(call.tool(), call.call_id(), decision, &key)
                    .map_err(|_| MultiApprovalError::Unavailable)?;
            }
        }
        if cancel.is_cancelled() {
            // Only retire here when this invocation's authority was already
            // checked before the hook ran. A later-phase answer has not yet
            // been verified and must not be able to retire another result.
            if latest.is_none() || matches_raw {
                let journal = recovery::lock_journal_for_publication(self.root())
                    .map_err(MultiApprovalError::Workspace)?;
                HookCheckpoint::new(
                    &journal,
                    &chain,
                    raw,
                    call.arguments(),
                    &self.hooks.pre_tool_use,
                )
                .and_then(|checkpoint| checkpoint.retire(&report))
                .map_err(|_| MultiApprovalError::Checkpoint)?;
            }
            return Err(MultiApprovalError::Prepare(MultiPatchError::Cancelled));
        }
        if let crate::hooks::PreHookOutcome::Denied { reason } = &report.outcome {
            if latest.is_some() && !matches_raw {
                return Err(MultiApprovalError::InvalidApproval);
            }
            let journal = recovery::lock_journal_for_publication(self.root())
                .map_err(MultiApprovalError::Workspace)?;
            HookCheckpoint::new(
                &journal,
                &chain,
                raw,
                call.arguments(),
                &self.hooks.pre_tool_use,
            )
            .and_then(|checkpoint| checkpoint.retire(&report))
            .map_err(|_| MultiApprovalError::Checkpoint)?;
            return Ok(MultiHookProgress::Denied(
                self.redact_output(bounded_detail(reason)),
            ));
        }
        let rewrite = report
            .rewrite
            .clone()
            .map(|rewrite| self.prepare_multi_hook_rewrite(call, rewrite, cancel))
            .transpose()
            .map_err(|error| match error {
                MultiHookRewriteError::Invalid => MultiApprovalError::InvalidApproval,
                MultiHookRewriteError::Prepare(error) => MultiApprovalError::Prepare(error),
                MultiHookRewriteError::Policy(error) => MultiApprovalError::Policy(error),
            })?;
        let effective = rewrite.as_ref().map_or(raw, |rewrite| &rewrite.candidate);
        let effective_arguments = rewrite
            .as_ref()
            .map_or(call.arguments(), |rewrite| rewrite.call.arguments());
        let mut matches_question = false;
        if let crate::hooks::PreHookOutcome::Ask { reason, .. } = &report.outcome {
            let reason = self.redact_output(bounded_detail(reason));
            let reason = bounded_text(reason.as_bytes(), 256);
            let question = MultiHookQuestion::new(
                &chain,
                raw,
                call.arguments(),
                effective,
                effective_arguments,
                &self.hooks.pre_tool_use,
                &report,
                &reason,
            )
            .map_err(|_| MultiApprovalError::InvalidApproval)?;
            let sink = sink.ok_or(MultiApprovalError::Unavailable)?;
            let approved = question
                .is_approved(sink.as_ref())
                .map_err(|_| MultiApprovalError::Unavailable)?;
            matches_question = approved
                && latest.is_some_and(|answer| {
                    answer.source == question.request().source
                        && answer.arguments_digest == question.request().arguments_digest
                });
            if !approved {
                if latest.is_some() && !matches_raw {
                    return Err(MultiApprovalError::InvalidApproval);
                }
                self.bind_multi_hook_pending(raw, call, &chain, &report, question.request())?;
                let token = sink
                    .request(question.request())
                    .map_err(|_| MultiApprovalError::Unavailable)?;
                self.observe_approval_request(question.request(), &token);
                return Ok(MultiHookProgress::Waiting);
            }
        }
        let mut matches_hook = matches_raw || matches_question;
        let approved_paths = if let Some(rewrite) = &rewrite {
            let after = chain
                .for_hook_rewrite(effective, call.arguments(), &report_binding)
                .map_err(|_| MultiApprovalError::InvalidApproval)?;
            if !rewrite.asks.is_empty() && sink.is_none() {
                return Err(MultiApprovalError::Unavailable);
            }
            let approved = match sink {
                Some(sink) => after
                    .approved_indices(effective, call.arguments(), sink.as_ref())
                    .map_err(|_| MultiApprovalError::Unavailable)?,
                None => Default::default(),
            };
            let matches_after = latest.is_some_and(|answer| {
                approved
                    .iter()
                    .any(|&index| after.authorizes(index, answer))
            });
            matches_hook |= matches_after;
            if let Some(&index) = rewrite.asks.iter().find(|index| !approved.contains(index)) {
                if latest.is_some() && !matches_hook {
                    return Err(MultiApprovalError::InvalidApproval);
                }
                let request = after
                    .request(effective, index, call.arguments())
                    .map_err(|_| MultiApprovalError::InvalidApproval)?;
                self.bind_multi_hook_pending(raw, call, &chain, &report, &request)?;
                let sink = sink.ok_or(MultiApprovalError::Unavailable)?;
                let token = sink
                    .request(&request)
                    .map_err(|_| MultiApprovalError::Unavailable)?;
                self.observe_approval_request(&request, &token);
                return Ok(MultiHookProgress::Waiting);
            }
            approved
        } else {
            raw_approved
        };
        if latest.is_some() && !matches_hook && !allow_formatter_answer {
            return Err(MultiApprovalError::InvalidApproval);
        }
        Ok(MultiHookProgress::Ready(Box::new(MultiHookReady {
            original: raw.clone(),
            candidate: effective.clone(),
            grant: MultiApprovalGrant::new(effective, call, approved_paths)?,
            chain,
            report,
            event_binding: report_binding,
            rewrite,
            latest_matches: latest.is_none() || matches_hook,
        })))
    }
    /// Validate a hook rewrite as a fresh multi-file call, including the
    /// rewritten paths' read proofs and all denials before returning asks.
    /// This prepares bytes only; no hook, formatter, or write is performed.
    #[cfg(unix)]
    #[allow(dead_code)] // Used by the pending pre-hook coordinator.
    fn prepare_multi_hook_rewrite(
        &self,
        original: &ValidatedToolCall,
        rewrite: crate::hooks::HookRewrite,
        cancel: &CancellationToken,
    ) -> Result<MultiHookRewrite, MultiHookRewriteError> {
        if original.tool() != WORKSPACE_MULTI_PATCH_TOOL {
            return Err(MultiHookRewriteError::Invalid);
        }
        let after = serde_json::Value::Object(rewrite.input).to_string();
        let proposed = ProposedToolCall::new(original.call_id(), original.tool(), &after)
            .map_err(|_| MultiHookRewriteError::Invalid)?;
        let call = ValidatedToolCall::from_proposed(&proposed);
        let candidate = self
            .prepare_multi_patch(call.arguments(), cancel)
            .map_err(MultiHookRewriteError::Prepare)?;
        let mut asks = std::collections::BTreeSet::new();
        for (index, entry) in candidate.entries().iter().enumerate() {
            match self.check_multi_edit_permission(&entry.path) {
                Ok(()) => {}
                Err(WorkspaceRecoveryError::ApprovalRequired) => {
                    asks.insert(index);
                }
                Err(error) => return Err(MultiHookRewriteError::Policy(error)),
            }
        }
        let marker = format!(
            "[hook {} rewrote {} input]",
            rewrite.contributors.join(", "),
            original.tool()
        );
        let provenance = HookRewriteRecord {
            hook: rewrite.hook,
            contributors: rewrite.contributors,
            command_digest: rewrite.command_digest,
            before: original.arguments().to_owned(),
            after,
            before_digest: sha256_hex(original.arguments().as_bytes()),
            after_digest: sha256_hex(call.arguments().as_bytes()),
        };
        Ok(MultiHookRewrite {
            call,
            candidate,
            asks,
            marker,
            provenance,
        })
    }

    pub(super) fn execute_multi_patch(
        &self,
        call: &ValidatedToolCall,
        approved: Option<&crate::approvals::ApprovedAsk>,
        cancel: &CancellationToken,
    ) -> Result<ToolStepResult, ToolStepError> {
        #[cfg(not(unix))]
        {
            let _ = (approved, cancel);
            return Ok(ToolStepResult::Failed {
                call_id: call.call_id().to_owned(),
                handled: true,
                detail: Some("multi-file editing is unavailable on this platform".into()),
            });
        }
        #[cfg(unix)]
        {
            let result = self.run_multi_patch_approved(call, approved, cancel);
            let failed = |detail: String, handled| ToolStepResult::Failed {
                call_id: call.call_id().to_owned(),
                handled,
                detail: Some(bounded_detail(&detail)),
            };
            match result {
                Ok(ApprovedMultiPatchOutcome::Waiting) => Ok(ToolStepResult::ApprovalRequired { call_id: call.call_id().to_owned() }),
                Ok(ApprovedMultiPatchOutcome::Denied(detail)) => Ok(ToolStepResult::Denied { call_id: call.call_id().to_owned(), detail: Some(detail) }),
                Ok(ApprovedMultiPatchOutcome::NoChanges(notes)) => Ok(ToolStepResult::Succeeded { call_id: call.call_id().to_owned(), summary: format!("Formatter left no file changes.{notes}") }),
                Ok(ApprovedMultiPatchOutcome::Published(PublishOutcome::Committed, diagnostics)) => Ok(ToolStepResult::Succeeded { call_id: call.call_id().to_owned(), summary: format!("Multi-file patch committed.{diagnostics}") }),
                Ok(ApprovedMultiPatchOutcome::Published(PublishOutcome::CommittedCleanupPending { .. }, diagnostics)) => Ok(ToolStepResult::Succeeded { call_id: call.call_id().to_owned(), summary: format!("Multi-file patch committed; workspace recovery must finish cleanup before further edits.{diagnostics}") }),
                Ok(ApprovedMultiPatchOutcome::Published(PublishOutcome::RecoveryPending { .. }, diagnostics)) => Ok(ToolStepResult::Failed { call_id: call.call_id().to_owned(), handled: false, detail: Some(format!("Multi-file patch could not be fully rolled back; workspace recovery is required before further edits.{diagnostics}")) }),
                Ok(ApprovedMultiPatchOutcome::Published(PublishOutcome::CommitUncertain(_), diagnostics)) => Ok(ToolStepResult::Failed { call_id: call.call_id().to_owned(), handled: false, detail: Some(format!("Multi-file patch commit status is uncertain; inspect workspace recovery before retrying.{diagnostics}")) }),
                Ok(ApprovedMultiPatchOutcome::Published(PublishOutcome::Rejected(PublishFailure::Cancelled) | PublishOutcome::RolledBack(PublishFailure::Cancelled), _)) | Err(MultiApprovalError::Prepare(MultiPatchError::Cancelled)) => Err(ToolStepError::Cancelled),
                Ok(ApprovedMultiPatchOutcome::Published(PublishOutcome::Rejected(reason) | PublishOutcome::RolledBack(reason), diagnostics)) => {
                    let detail = match reason {
                        PublishFailure::Conflict { path } => format!("Multi-file patch was not committed: {path} changed."),
                        _ => "Multi-file patch was not committed; inspect the workspace and try again.".to_owned(),
                    };
                    Ok(ToolStepResult::Failed { call_id: call.call_id().to_owned(), handled: true, detail: Some(format!("{}{diagnostics}", bounded_detail(&detail))) })
                }
                Err(MultiApprovalError::Policy(_)) => Ok(ToolStepResult::Denied { call_id: call.call_id().to_owned(), detail: Some("Multi-file patch denied by workspace permissions or path scope.".into()) }),
                Err(MultiApprovalError::Unavailable) => Ok(ToolStepResult::Denied { call_id: call.call_id().to_owned(), detail: Some("Per-file approval or its durable history is unavailable.".into()) }),
                Err(MultiApprovalError::InvalidApproval) => Ok(failed("Approval no longer matches the requested files or their contents; read the files and request the edit again.".into(), true)),
                Err(MultiApprovalError::Checkpoint) => Ok(failed("Formatter execution or its saved result is uncertain; inspect workspace recovery before retrying.".into(), false)),
                Err(MultiApprovalError::ShadowFailed(detail)) => Ok(ToolStepResult::Failed { call_id: call.call_id().to_owned(), handled: true, detail: Some(detail) }),
                Err(MultiApprovalError::ShadowUncertain(detail)) => Ok(ToolStepResult::Failed { call_id: call.call_id().to_owned(), handled: false, detail: Some(detail) }),
                Err(MultiApprovalError::PublicationFailed(detail)) => Ok(ToolStepResult::Failed { call_id: call.call_id().to_owned(), handled: true, detail: Some(detail) }),
                Err(MultiApprovalError::Workspace(WorkspaceRecoveryError::HooksRequireIntegration)) => Ok(failed("Multi-file patch is unavailable while pre-use tool hooks are configured.".into(), true)),
                Err(MultiApprovalError::Workspace(WorkspaceRecoveryError::WriteStepsRequireIntegration)) => Ok(failed("Multi-file patch cannot bypass configured write steps.".into(), true)),
                Err(MultiApprovalError::Workspace(WorkspaceRecoveryError::ShadowUncertain)) => Ok(failed("A shadow diagnostic may still be running; inspect the workspace recovery journal before editing.".into(), false)),
                Err(MultiApprovalError::Workspace(WorkspaceRecoveryError::ExternalUncertain)) => Ok(failed("A hook or formatter may still be running; inspect the workspace recovery journal before editing.".into(), false)),
                Err(MultiApprovalError::Workspace(_)) => Ok(failed("Multi-file patch is unavailable; workspace recovery or coordination is required.".into(), true)),
                Err(MultiApprovalError::Prepare(MultiPatchError::File { index, error: WorkspaceRecoveryError::ReadRequired })) => Ok(failed(format!("File {} must be read in this session again before editing.", index + 1), true)),
                Err(MultiApprovalError::Prepare(MultiPatchError::File { index, error: WorkspaceRecoveryError::AlreadyExists })) => Ok(failed(format!("File {} already exists; creation was refused.", index + 1), true)),
                Err(MultiApprovalError::Prepare(MultiPatchError::File { index, error: WorkspaceRecoveryError::MissingFile })) => Ok(failed(format!("File {} does not exist; the operation was refused.", index + 1), true)),
                Err(MultiApprovalError::Prepare(MultiPatchError::Patch { index, error: OrderedPatchError::Conflict { index: edit, detail } })) => Ok(failed(format!("File {}, edit {}: {detail}.", index + 1, edit + 1), true)),
                Err(MultiApprovalError::Prepare(MultiPatchError::Patch { error: OrderedPatchError::Limit(detail), .. })) => Ok(failed(detail.to_owned(), true)),
                Err(MultiApprovalError::Prepare(_)) => Ok(failed("Multi-file patch could not be prepared; check paths, file contents and size limits.".into(), true)),
            }
        }
    }

    /// Runs the durable, per-path approval sequence for one tool call.
    #[cfg(unix)]
    #[allow(dead_code)]
    fn run_multi_patch_approved(
        &self,
        call: &ValidatedToolCall,
        latest: Option<&crate::approvals::ApprovedAsk>,
        cancel: &CancellationToken,
    ) -> Result<ApprovedMultiPatchOutcome, MultiApprovalError> {
        self.run_multi_patch_composed(call, latest, cancel, true)
    }

    /// Coordinate the optional hook, formatter and publication phases.
    #[cfg(unix)]
    fn run_multi_patch_composed(
        &self,
        call: &ValidatedToolCall,
        latest: Option<&crate::approvals::ApprovedAsk>,
        cancel: &CancellationToken,
        enable_hooks: bool,
    ) -> Result<ApprovedMultiPatchOutcome, MultiApprovalError> {
        cancel
            .check()
            .map_err(|_| MultiApprovalError::Prepare(MultiPatchError::Cancelled))?;
        if !enable_hooks {
            self.check_multi_edit_integrations()
                .map_err(MultiApprovalError::Workspace)?;
        }
        self.recover_before_edit()
            .map_err(MultiApprovalError::Workspace)?;
        let raw = self
            .prepare_multi_patch(call.arguments(), cancel)
            .map_err(MultiApprovalError::Prepare)?;
        let has_formatter = self
            .write_steps
            .as_ref()
            .is_some_and(crate::write_steps::WriteSteps::has_formatters);
        let hook_ready = if enable_hooks && !self.hooks.pre_tool_use.is_empty() {
            match self.advance_multi_hook_approval_inner(
                &raw,
                call,
                latest,
                cancel,
                has_formatter,
            )? {
                MultiHookProgress::Waiting => return Ok(ApprovedMultiPatchOutcome::Waiting),
                MultiHookProgress::Denied(reason) => {
                    return Ok(ApprovedMultiPatchOutcome::Denied(reason));
                }
                MultiHookProgress::Ready(ready) => Some(ready),
            }
        } else {
            None
        };
        let effective = hook_ready.as_ref().map_or(&raw, |ready| &ready.candidate);
        let (candidate, grant, mut diagnostics, checkpoint_chain) = if has_formatter {
            let progress = if let Some(ready) = hook_ready.as_deref() {
                self.advance_formatted_multi_approval_inner(
                    effective,
                    call,
                    latest,
                    cancel,
                    Some(ready),
                )
            } else {
                self.advance_formatted_multi_approval(effective, call, latest, cancel)
            };
            let progress = match progress {
                Ok(progress) => progress,
                Err(MultiApprovalError::Prepare(error)) => {
                    if let Some(ready) = hook_ready.as_deref() {
                        self.retire_failed_hook_format(call, ready)?;
                    }
                    return Err(MultiApprovalError::Prepare(error));
                }
                Err(error) => return Err(error),
            };
            match progress {
                FormattedProgress::Waiting => return Ok(ApprovedMultiPatchOutcome::Waiting),
                FormattedProgress::NoChanges(notes) => {
                    let notes = if let Some(ready) = hook_ready.as_ref() {
                        self.observe_multi_hook_rewrite(call, ready)?;
                        let journal = recovery::lock_journal_for_publication(self.root())
                            .map_err(MultiApprovalError::Workspace)?;
                        super::multi_hook_checkpoint::HookCheckpoint::new(
                            &journal,
                            &ready.chain,
                            &raw,
                            call.arguments(),
                            &self.hooks.pre_tool_use,
                        )
                        .and_then(|checkpoint| checkpoint.retire(&ready.report))
                        .map_err(|_| MultiApprovalError::Checkpoint)?;
                        self.multi_hook_result_detail(notes, ready)
                    } else {
                        notes
                    };
                    return Ok(ApprovedMultiPatchOutcome::NoChanges(notes));
                }
                FormattedProgress::Ready(ready) => {
                    let FormattedReady {
                        record,
                        grant,
                        notes,
                        chain,
                    } = *ready;
                    (record, grant, notes, Some(chain))
                }
            }
        } else if let Some(ready) = hook_ready.as_ref() {
            (
                ready.candidate.clone(),
                MultiApprovalGrant::new(
                    &ready.candidate,
                    call,
                    ready.grant.approved_paths.clone(),
                )?,
                String::new(),
                None,
            )
        } else {
            match self.advance_multi_approval(&raw, call, latest)? {
                MultiApprovalProgress::Waiting => return Ok(ApprovedMultiPatchOutcome::Waiting),
                MultiApprovalProgress::Ready(grant) => (raw.clone(), grant, String::new(), None),
            }
        };
        if cancel.is_cancelled() {
            self.retire_cancelled_candidates(
                call,
                effective,
                checkpoint_chain.as_ref(),
                hook_ready.as_deref(),
            )?;
            return Err(MultiApprovalError::Prepare(MultiPatchError::Cancelled));
        }
        if let Some(ready) = hook_ready.as_ref() {
            self.observe_multi_hook_rewrite(call, ready)?;
        }
        if let Some(config) = self.shadow_diagnostics.as_ref()
            && candidate
                .entries()
                .iter()
                .any(|entry| config.matches(&entry.path, glob_path_match))
        {
            let shadow_token =
                recovery::begin_shadow(self.root()).map_err(MultiApprovalError::Workspace)?;
            let outcome = crate::shadow_diagnostics::verify_multi_candidate(
                self.root(),
                &candidate,
                config,
                || cancel.is_cancelled(),
            );
            if let crate::shadow_diagnostics::ShadowVerifyOutcome::Uncertain { reason } = &outcome {
                let detail = self.redact_output(bounded_detail(&format!("shadow diagnostics termination is uncertain; the multi-file patch was not applied: {reason}")));
                return Err(MultiApprovalError::ShadowUncertain(detail));
            }
            recovery::finish_shadow(self.root(), &shadow_token)
                .map_err(MultiApprovalError::Workspace)?;
            if cancel.is_cancelled() {
                self.retire_cancelled_candidates(
                    call,
                    effective,
                    checkpoint_chain.as_ref(),
                    hook_ready.as_deref(),
                )?;
                return Err(MultiApprovalError::Prepare(MultiPatchError::Cancelled));
            }
            match outcome {
                crate::shadow_diagnostics::ShadowVerifyOutcome::Uncertain { .. } => {
                    unreachable!("handled before cancellation")
                }
                crate::shadow_diagnostics::ShadowVerifyOutcome::Failed { diagnostics_tail } => {
                    let mut cleanup_note = "";
                    if let Some(chain) = checkpoint_chain.as_ref() {
                        use super::multi_format_checkpoint::{CheckpointState, FormatCheckpoint};
                        let retired = recovery::lock_journal_for_publication(self.root())
                            .ok()
                            .and_then(|journal| {
                                let key = chain.formatter_checkpoint_key().ok()?;
                                let checkpoint = FormatCheckpoint::new(&journal, &key).ok()?;
                                match checkpoint.load(effective) {
                                    Ok(CheckpointState::Ready(result)) => {
                                        Some(checkpoint.retire_ready(effective, &result).is_ok())
                                    }
                                    _ => None,
                                }
                            })
                            .unwrap_or(false);
                        if !retired {
                            cleanup_note = "; formatter checkpoint cleanup is pending";
                        }
                    }
                    if let Some(ready) = hook_ready.as_ref() {
                        let retired = recovery::lock_journal_for_publication(self.root())
                            .ok()
                            .is_some_and(|journal| {
                                super::multi_hook_checkpoint::HookCheckpoint::new(
                                    &journal,
                                    &ready.chain,
                                    &raw,
                                    call.arguments(),
                                    &self.hooks.pre_tool_use,
                                )
                                .and_then(|checkpoint| checkpoint.retire(&ready.report))
                                .is_ok()
                            });
                        if !retired {
                            cleanup_note = "; hook or formatter checkpoint cleanup is pending";
                        }
                    }
                    let detail = self.redact_output(format!(
                        "shadow diagnostics failed; the multi-file patch was not applied{cleanup_note}: {diagnostics_tail}"
                    ));
                    let detail = bounded_detail(&detail);
                    let detail = hook_ready.as_ref().map_or(detail.clone(), |ready| {
                        self.multi_hook_result_detail(detail, ready)
                    });
                    return Err(MultiApprovalError::ShadowFailed(detail));
                }
                crate::shadow_diagnostics::ShadowVerifyOutcome::Passed { diagnostics_tail } => {
                    diagnostics
                        .push_str(&self.redact_output(format!(
                            "\nshadow diagnostics: ok\n{diagnostics_tail}"
                        )));
                }
                crate::shadow_diagnostics::ShadowVerifyOutcome::Skipped { reason } => {
                    diagnostics.push_str(
                        &self.redact_output(format!("\nshadow diagnostics skipped: {reason}")),
                    );
                }
            }
        }
        let journal = recovery::lock_journal_for_publication(self.root())
            .map_err(MultiApprovalError::Workspace)?;
        let outcome = match self.publish_multi_edit_inner(
            &journal,
            &candidate,
            Some((&grant, call)),
            hook_ready.as_deref().map(|ready| (&raw, ready)),
            || cancel.is_cancelled(),
        ) {
            Ok(outcome) => outcome,
            Err(error) => {
                // The adapter returns Err only before handing control to the
                // journaled publisher, so there are no file effects to undo.
                let mut pending = false;
                if let Some(chain) = checkpoint_chain.as_ref() {
                    use super::multi_format_checkpoint::{CheckpointState, FormatCheckpoint};
                    let retired = chain
                        .formatter_checkpoint_key()
                        .ok()
                        .and_then(|key| FormatCheckpoint::new(&journal, &key).ok())
                        .is_some_and(|checkpoint| match checkpoint.load(effective) {
                            Ok(CheckpointState::Ready(result)) => {
                                checkpoint.retire_ready(effective, &result).is_ok()
                            }
                            _ => false,
                        });
                    pending |= !retired;
                }
                if let Some(ready) = hook_ready.as_ref() {
                    let retired = super::multi_hook_checkpoint::HookCheckpoint::new(
                        &journal,
                        &ready.chain,
                        &raw,
                        call.arguments(),
                        &self.hooks.pre_tool_use,
                    )
                    .and_then(|checkpoint| checkpoint.retire(&ready.report))
                    .is_ok();
                    pending |= !retired;
                }
                if hook_ready.is_none() {
                    return Err(MultiApprovalError::Workspace(error));
                }
                if error == WorkspaceRecoveryError::PermissionDenied {
                    return Err(MultiApprovalError::Policy(error));
                }
                let note = if pending {
                    "; checkpoint cleanup is pending"
                } else {
                    ""
                };
                let detail = format!(
                    "Multi-file patch was not committed; inspect the workspace and try again{note}."
                );
                let detail = hook_ready.as_ref().map_or(detail.clone(), |ready| {
                    self.multi_hook_result_detail(detail, ready)
                });
                return Err(MultiApprovalError::PublicationFailed(detail));
            }
        };
        let settled = matches!(
            outcome,
            PublishOutcome::Committed
                | PublishOutcome::CommittedCleanupPending { .. }
                | PublishOutcome::Rejected(_)
                | PublishOutcome::RolledBack(_)
        );
        if settled && let Some(chain) = checkpoint_chain {
            use super::multi_format_checkpoint::{CheckpointState, FormatCheckpoint};
            let retired = chain
                .formatter_checkpoint_key()
                .ok()
                .and_then(|key| FormatCheckpoint::new(&journal, &key).ok())
                .is_some_and(|checkpoint| match checkpoint.load(effective) {
                    Ok(CheckpointState::Ready(result)) => {
                        checkpoint.retire_ready(effective, &result).is_ok()
                    }
                    _ => false,
                });
            if !retired {
                diagnostics.push_str("\nFormatter checkpoint cleanup is pending; inspect workspace recovery before retrying.");
            }
        }
        if settled && let Some(ready) = hook_ready.as_ref() {
            use super::multi_hook_checkpoint::HookCheckpoint;
            let retired = HookCheckpoint::new(
                &journal,
                &ready.chain,
                &raw,
                call.arguments(),
                &self.hooks.pre_tool_use,
            )
            .and_then(|checkpoint| checkpoint.retire(&ready.report))
            .is_ok();
            if !retired {
                diagnostics.push_str("\nHook checkpoint cleanup is pending; inspect workspace recovery before retrying.");
            }
        }
        if matches!(
            outcome,
            PublishOutcome::Committed | PublishOutcome::CommittedCleanupPending { .. }
        ) && let Some(steps) = self.write_steps.as_ref()
        {
            for entry in candidate.entries() {
                if cancel.is_cancelled() {
                    break;
                }
                let Some(after) = &entry.after else { continue };
                if let Some(note) = steps.diagnostics(self.root(), &entry.path, &after.bytes) {
                    diagnostics.push('\n');
                    diagnostics.push_str(&self.redact_output(note));
                    if diagnostics.len() > 8 * 1024 {
                        break;
                    }
                }
            }
        }
        let diagnostics = bounded_text(diagnostics.as_bytes(), 8 * 1024);
        let diagnostics = hook_ready.as_ref().map_or(diagnostics.clone(), |ready| {
            self.multi_hook_result_detail(diagnostics, ready)
        });
        Ok(ApprovedMultiPatchOutcome::Published(outcome, diagnostics))
    }

    /// Re-evaluate all paths, then request at most one missing approval. A
    /// resume only carries the latest answer, so earlier answers are loaded
    /// from the durable sink and checked against the exact displayed requests.
    #[allow(dead_code)]
    fn advance_multi_approval(
        &self,
        record: &RecoveryRecord,
        call: &ValidatedToolCall,
        latest: Option<&crate::approvals::ApprovedAsk>,
    ) -> Result<MultiApprovalProgress, MultiApprovalError> {
        if call.tool() != "workspace_multi_patch" {
            return Err(MultiApprovalError::InvalidApproval);
        }
        let mut asks = Vec::new();
        for (index, entry) in record.entries().iter().enumerate() {
            match self.check_multi_edit_permission(&entry.path) {
                Ok(()) => {}
                Err(WorkspaceRecoveryError::ApprovalRequired) => asks.push(index),
                Err(error) => return Err(MultiApprovalError::Policy(error)),
            }
        }
        if asks.is_empty() {
            return Ok(MultiApprovalProgress::Ready(MultiApprovalGrant::new(
                record,
                call,
                Default::default(),
            )?));
        }
        let sink = self
            .approval_sink
            .as_ref()
            .ok_or(MultiApprovalError::Unavailable)?;
        let chain = match latest {
            Some(approved) => super::multi_approval::PathApprovalChain::resume(
                record,
                call.call_id(),
                call.arguments(),
                approved,
            ),
            None => super::multi_approval::PathApprovalChain::new(
                record,
                call.call_id(),
                call.arguments(),
            ),
        }
        .map_err(|_| MultiApprovalError::InvalidApproval)?;
        let approved_paths = chain
            .approved_indices(record, call.arguments(), sink.as_ref())
            .map_err(|_| MultiApprovalError::Unavailable)?;
        if let Some(latest) = latest
            && !(0..record.entries().len())
                .any(|index| approved_paths.contains(&index) && chain.authorizes(index, latest))
        {
            return Err(MultiApprovalError::InvalidApproval);
        }
        if let Some(index) = asks
            .into_iter()
            .find(|index| !approved_paths.contains(index))
        {
            let request = chain
                .request(record, index, call.arguments())
                .map_err(|_| MultiApprovalError::InvalidApproval)?;
            let token = sink
                .request(&request)
                .map_err(|_| MultiApprovalError::Unavailable)?;
            self.observe_approval_request(&request, &token);
            return Ok(MultiApprovalProgress::Waiting);
        }
        Ok(MultiApprovalProgress::Ready(MultiApprovalGrant::new(
            record,
            call,
            approved_paths,
        )?))
    }

    /// Authorize all raw paths before executing formatters, persist their
    /// result exactly once, and authorize changed final images per path.
    #[cfg(unix)]
    fn advance_formatted_multi_approval(
        &self,
        raw: &RecoveryRecord,
        call: &ValidatedToolCall,
        latest: Option<&crate::approvals::ApprovedAsk>,
        cancel: &CancellationToken,
    ) -> Result<FormattedProgress, MultiApprovalError> {
        self.advance_formatted_multi_approval_inner(raw, call, latest, cancel, None)
    }

    #[cfg(unix)]
    fn advance_formatted_multi_approval_inner(
        &self,
        raw: &RecoveryRecord,
        call: &ValidatedToolCall,
        latest: Option<&crate::approvals::ApprovedAsk>,
        cancel: &CancellationToken,
        prior_hook: Option<&MultiHookReady>,
    ) -> Result<FormattedProgress, MultiApprovalError> {
        use super::multi_approval::{ApprovalPhase, PathApprovalChain};
        use super::multi_format_checkpoint::{CheckpointState, FormatCheckpoint};

        if call.tool() != WORKSPACE_MULTI_PATCH_TOOL {
            return Err(MultiApprovalError::InvalidApproval);
        }
        let binding = self
            .write_steps
            .as_ref()
            .and_then(crate::write_steps::WriteSteps::formatter_approval_binding)
            .ok_or(MultiApprovalError::InvalidApproval)?;
        let mut asks = std::collections::BTreeSet::new();
        for (index, entry) in raw.entries().iter().enumerate() {
            match self.check_multi_edit_permission(&entry.path) {
                Ok(()) => {}
                Err(WorkspaceRecoveryError::ApprovalRequired) => {
                    asks.insert(index);
                }
                Err(error) => return Err(MultiApprovalError::Policy(error)),
            }
        }
        if prior_hook
            .is_some_and(|prior| &prior.candidate != raw || !prior.grant.matches(raw, call))
        {
            return Err(MultiApprovalError::InvalidApproval);
        }
        let chain = match (prior_hook, latest) {
            (Some(prior), _) => prior.chain.for_formatter(raw, call.arguments(), &binding),
            (None, Some(approved)) => PathApprovalChain::from_resolved_for_phase(
                raw,
                call.call_id(),
                call.arguments(),
                approved,
                ApprovalPhase::BeforeFormat,
                &binding,
            ),
            (None, None) => PathApprovalChain::new_before_format(
                raw,
                call.call_id(),
                call.arguments(),
                &binding,
            ),
        }
        .map_err(|_| MultiApprovalError::InvalidApproval)?;
        let sink = self.approval_sink.as_ref();
        if !asks.is_empty() && sink.is_none() {
            return Err(MultiApprovalError::Unavailable);
        }
        let before_approved = match sink {
            Some(sink) if !asks.is_empty() => chain
                .approved_indices(raw, call.arguments(), sink.as_ref())
                .map_err(|_| MultiApprovalError::Unavailable)?,
            _ => Default::default(),
        };
        let matches_before = latest.is_some_and(|answer| {
            before_approved
                .iter()
                .any(|&index| chain.authorizes(index, answer))
        });
        let matches_prior =
            latest.is_some() && prior_hook.is_some_and(|prior| prior.latest_matches);
        if let Some(&index) = asks.iter().find(|index| !before_approved.contains(index)) {
            if latest.is_some() && !matches_before && !matches_prior {
                return Err(MultiApprovalError::InvalidApproval);
            }
            let request = chain
                .request(raw, index, call.arguments())
                .map_err(|_| MultiApprovalError::InvalidApproval)?;
            if let Some(prior) = prior_hook {
                self.bind_multi_hook_pending(
                    &prior.original,
                    call,
                    &prior.chain,
                    &prior.report,
                    &request,
                )?;
            }
            let sink = sink.ok_or(MultiApprovalError::Unavailable)?;
            let token = sink
                .request(&request)
                .map_err(|_| MultiApprovalError::Unavailable)?;
            self.observe_approval_request(&request, &token);
            return Ok(FormattedProgress::Waiting);
        }
        cancel
            .check()
            .map_err(|_| MultiApprovalError::Prepare(MultiPatchError::Cancelled))?;
        let key = chain
            .formatter_checkpoint_key()
            .map_err(|_| MultiApprovalError::InvalidApproval)?;
        let (saved, command_token) = {
            let journal = recovery::lock_journal_for_publication(self.root())
                .map_err(MultiApprovalError::Workspace)?;
            let checkpoint = FormatCheckpoint::new(&journal, &key)
                .map_err(|_| MultiApprovalError::Checkpoint)?;
            match checkpoint
                .load(raw)
                .map_err(|_| MultiApprovalError::Checkpoint)?
            {
                CheckpointState::Missing => {
                    if latest.is_some() && !matches_before && !matches_prior {
                        return Err(MultiApprovalError::InvalidApproval);
                    }
                    if let Some(prior) = prior_hook {
                        self.observe_multi_hook_rewrite(call, prior)?;
                    }
                    recovery::ensure_external_can_start(&journal)
                        .map_err(MultiApprovalError::Workspace)?;
                    checkpoint
                        .start()
                        .map_err(|_| MultiApprovalError::Checkpoint)?;
                    let token = recovery::begin_external(&journal, "formatter")
                        .map_err(MultiApprovalError::Workspace)?;
                    (None, Some(token))
                }
                CheckpointState::Ready(result) => (Some(result), None),
                CheckpointState::Running | CheckpointState::Retired => {
                    return Err(MultiApprovalError::Checkpoint);
                }
            }
        };
        let result = if let Some(saved) = saved {
            saved
        } else {
            let token = command_token
                .as_deref()
                .ok_or(MultiApprovalError::Checkpoint)?;
            // The synced marker prevents another coordinator from starting the
            // same formatter while no journal lock is held across external code.
            let formatted = match self.format_multi_candidate(raw, cancel) {
                Ok(formatted) => formatted,
                Err(MultiPatchError::FormatterUncertain) => {
                    // A child or its process group may still be active. The
                    // running marker remains the authority on recovery.
                    return Err(MultiApprovalError::Checkpoint);
                }
                Err(error) => {
                    // This invocation owns the running marker and knows the
                    // formatter returned. A reopened process cannot infer that.
                    let journal = recovery::lock_journal_for_publication(self.root())
                        .map_err(MultiApprovalError::Workspace)?;
                    FormatCheckpoint::new(&journal, &key)
                        .and_then(|checkpoint| checkpoint.retire_after_failed_execution(raw))
                        .map_err(|_| MultiApprovalError::Checkpoint)?;
                    recovery::finish_external(&journal, token)
                        .map_err(MultiApprovalError::Workspace)?;
                    return Err(MultiApprovalError::Prepare(error));
                }
            };
            let journal = recovery::lock_journal_for_publication(self.root())
                .map_err(MultiApprovalError::Workspace)?;
            FormatCheckpoint::new(&journal, &key)
                .and_then(|checkpoint| checkpoint.finish(raw, &formatted, &binding))
                .map_err(|_| MultiApprovalError::Checkpoint)?;
            recovery::finish_external(&journal, token).map_err(MultiApprovalError::Workspace)?;
            formatted
        };
        let Some(record) = result.record.as_ref() else {
            if latest.is_some() && !matches_before && !matches_prior {
                return Err(MultiApprovalError::InvalidApproval);
            }
            if let Some(prior) = prior_hook {
                self.observe_multi_hook_rewrite(call, prior)?;
            }
            let journal = recovery::lock_journal_for_publication(self.root())
                .map_err(MultiApprovalError::Workspace)?;
            let mut recovery = WorkspaceRecovery { tools: self };
            for (index, entry) in raw.entries().iter().enumerate() {
                let observed = recovery.observe(&entry.path).map_err(|error| {
                    MultiApprovalError::Prepare(MultiPatchError::File { index, error })
                })?;
                if observed != entry.before {
                    return Err(MultiApprovalError::Prepare(MultiPatchError::File {
                        index,
                        error: WorkspaceRecoveryError::ChangedDuringRead,
                    }));
                }
            }
            FormatCheckpoint::new(&journal, &key)
                .and_then(|checkpoint| checkpoint.retire_ready(raw, &result))
                .map_err(|_| MultiApprovalError::Checkpoint)?;
            return Ok(FormattedProgress::NoChanges(result.notes));
        };
        if record == raw {
            if latest.is_some() && !matches_before && !matches_prior {
                return Err(MultiApprovalError::InvalidApproval);
            }
            return Ok(FormattedProgress::Ready(Box::new(FormattedReady {
                record: record.clone(),
                grant: MultiApprovalGrant::new(record, call, before_approved)?,
                notes: result.notes,
                chain,
            })));
        }
        let final_chain = chain
            .for_phase(record, call.arguments(), ApprovalPhase::AfterFormat)
            .map_err(|_| MultiApprovalError::InvalidApproval)?;
        let final_approved = match sink {
            Some(sink) if !asks.is_empty() => final_chain
                .approved_indices(record, call.arguments(), sink.as_ref())
                .map_err(|_| MultiApprovalError::Unavailable)?,
            _ => Default::default(),
        };
        let matches_final = latest.is_some_and(|answer| {
            final_approved
                .iter()
                .any(|&index| final_chain.authorizes(index, answer))
        });
        if latest.is_some() && !matches_before && !matches_final && !matches_prior {
            return Err(MultiApprovalError::InvalidApproval);
        }
        if let Some(index) =
            result
                .original_indices
                .iter()
                .enumerate()
                .find_map(|(index, original)| {
                    (asks.contains(original) && !final_approved.contains(&index)).then_some(index)
                })
        {
            let request = final_chain
                .request(record, index, call.arguments())
                .map_err(|_| MultiApprovalError::InvalidApproval)?;
            if let Some(prior) = prior_hook {
                self.bind_multi_hook_pending(
                    &prior.original,
                    call,
                    &prior.chain,
                    &prior.report,
                    &request,
                )?;
            }
            let sink = sink.ok_or(MultiApprovalError::Unavailable)?;
            let token = sink
                .request(&request)
                .map_err(|_| MultiApprovalError::Unavailable)?;
            self.observe_approval_request(&request, &token);
            return Ok(FormattedProgress::Waiting);
        }
        Ok(FormattedProgress::Ready(Box::new(FormattedReady {
            record: record.clone(),
            grant: MultiApprovalGrant::new(record, call, final_approved)?,
            notes: result.notes,
            chain,
        })))
    }

    /// Runtime-oriented orchestration. This remains unadvertised until
    /// per-path approvals, hooks, and crash-safe staging are integrated.
    #[cfg(unix)]
    #[allow(dead_code)]
    fn run_multi_patch(
        &self,
        raw: &str,
        cancel: &CancellationToken,
    ) -> Result<PublishOutcome<WorkspaceRecoveryError>, MultiPatchRunError> {
        cancel
            .check()
            .map_err(|_| MultiPatchRunError::Prepare(MultiPatchError::Cancelled))?;
        self.recover_before_edit()
            .map_err(MultiPatchRunError::Workspace)?;
        let record = self
            .prepare_multi_patch(raw, cancel)
            .map_err(MultiPatchRunError::Prepare)?;
        cancel
            .check()
            .map_err(|_| MultiPatchRunError::Prepare(MultiPatchError::Cancelled))?;
        let journal = recovery::lock_journal_for_publication(self.root())
            .map_err(MultiPatchRunError::Workspace)?;
        self.publish_multi_edit_with_cancel(&journal, &record, None, || cancel.is_cancelled())
            .map_err(MultiPatchRunError::Workspace)
    }

    /// Construct a complete immutable transaction candidate without writing.
    /// Permission and approval decisions are intentionally left to the caller
    /// so a future approval surface can show this exact candidate. Publication
    /// independently revalidates permissions, read proofs and preimages.
    #[allow(dead_code)]
    fn prepare_multi_patch(
        &self,
        raw: &str,
        cancel: &CancellationToken,
    ) -> Result<RecoveryRecord, MultiPatchError> {
        let parsed = parse_multi_patch_args(raw).map_err(|_| MultiPatchError::Invalid)?;
        if !self.check_read_epoch() {
            return Err(MultiPatchError::Invalid);
        }
        let authority = self
            .read_authority
            .as_ref()
            .ok_or(MultiPatchError::Invalid)?;
        let mut recovery = WorkspaceRecovery { tools: self };
        let mut entries = Vec::with_capacity(parsed.len());
        let mut total_bytes = 0usize;
        for (index, change) in parsed.into_iter().enumerate() {
            cancel.check().map_err(|_| MultiPatchError::Cancelled)?;
            let file_error = |error| MultiPatchError::File { index, error };
            if recovery::is_journal_path(&change.path)
                || is_git_internal_path(&change.path)
                || is_protected_settings_path(&change.path)
            {
                return Err(file_error(WorkspaceRecoveryError::UnsafeFile));
            }
            let target = recovery.target(&change.path).map_err(file_error)?;
            let before = recovery.observe(&change.path).map_err(file_error)?;
            match (&change.operation, &before) {
                (MultiFileOperation::Create(_), Some(_)) => {
                    return Err(file_error(WorkspaceRecoveryError::AlreadyExists));
                }
                (MultiFileOperation::Patch(_) | MultiFileOperation::Delete, None) => {
                    return Err(file_error(WorkspaceRecoveryError::MissingFile));
                }
                _ => {}
            }
            if before
                .as_ref()
                .is_some_and(|image| image.bytes.len() > MAX_FILE_READ_BYTES)
            {
                return Err(file_error(WorkspaceRecoveryError::LimitExceeded));
            }
            let guarded = authority
                .guard_write(self.root(), &target)
                .map_err(|_| file_error(WorkspaceRecoveryError::ReadRequired))?;
            if guarded.original.as_deref() != before.as_ref().map(|image| image.bytes.as_slice()) {
                return Err(file_error(WorkspaceRecoveryError::ReadRequired));
            }
            let after = match change.operation {
                MultiFileOperation::Patch(edits) => {
                    let original = before
                        .as_ref()
                        .ok_or_else(|| file_error(WorkspaceRecoveryError::Io))?;
                    let contents = String::from_utf8(original.bytes.clone())
                        .map_err(|_| MultiPatchError::Invalid)?;
                    let (updated, _) =
                        apply_ordered_patches(contents, &edits, cancel).map_err(|error| {
                            if error == OrderedPatchError::Cancelled {
                                MultiPatchError::Cancelled
                            } else {
                                MultiPatchError::Patch { index, error }
                            }
                        })?;
                    if team_memory_gate(self.root(), &change.path, updated.as_bytes(), "patch")
                        .is_some()
                    {
                        return Err(file_error(WorkspaceRecoveryError::UnsafeFile));
                    }
                    Some(FileImage {
                        bytes: updated.into_bytes(),
                        mode: original.mode,
                    })
                }
                MultiFileOperation::Create(content) => {
                    if before.is_some() {
                        return Err(file_error(WorkspaceRecoveryError::AlreadyExists));
                    }
                    if team_memory_gate(self.root(), &change.path, content.as_bytes(), "write")
                        .is_some()
                    {
                        return Err(file_error(WorkspaceRecoveryError::UnsafeFile));
                    }
                    Some(FileImage {
                        bytes: content.into_bytes(),
                        mode: Some(0o600),
                    })
                }
                MultiFileOperation::Delete => {
                    let original = before
                        .as_ref()
                        .ok_or_else(|| file_error(WorkspaceRecoveryError::Io))?;
                    std::str::from_utf8(&original.bytes).map_err(|_| MultiPatchError::Invalid)?;
                    None
                }
            };
            total_bytes = total_bytes
                .checked_add(before.as_ref().map_or(0, |image| image.bytes.len()))
                .and_then(|sum| {
                    sum.checked_add(after.as_ref().map_or(0, |image| image.bytes.len()))
                })
                .filter(|sum| *sum <= workspace::multi_edit_recovery::MAX_RECOVERY_BYTES)
                .ok_or_else(|| file_error(WorkspaceRecoveryError::LimitExceeded))?;
            entries.push(RecoveryEntry {
                path: change.path,
                before,
                after,
            });
        }
        RecoveryRecord::new(RecoveryState::Intent, entries).map_err(|_| MultiPatchError::Invalid)
    }

    /// Build final formatter images without changing workspace files. Callers
    /// must authorize every path before invoking this because formatters are
    /// external commands, then authorize the exact resulting images before
    /// publication. A resumed call must reuse a durable result rather than
    /// invoking the commands again.
    #[cfg(unix)]
    fn format_multi_candidate(
        &self,
        record: &RecoveryRecord,
        cancel: &CancellationToken,
    ) -> Result<FormattedMultiCandidate, MultiPatchError> {
        let mut entries = record.entries().to_vec();
        let mut total_bytes = entries
            .iter()
            .try_fold(0usize, |total, entry| {
                total
                    .checked_add(entry.before.as_ref().map_or(0, |image| image.bytes.len()))?
                    .checked_add(entry.after.as_ref().map_or(0, |image| image.bytes.len()))
            })
            .ok_or(MultiPatchError::Invalid)?;
        let mut notes = String::new();
        for (index, entry) in entries.iter_mut().enumerate() {
            cancel.check().map_err(|_| MultiPatchError::Cancelled)?;
            let Some(after) = entry.after.as_mut() else {
                continue;
            };
            let (formatted, note) = match self.write_steps.as_ref() {
                Some(steps) => {
                    let outcome = steps
                        .format_until(self.root(), &entry.path, &after.bytes, cancel)
                        .map_err(|error| match error {
                            crate::formatters::ExecutionError::Cancelled => {
                                MultiPatchError::Cancelled
                            }
                            crate::formatters::ExecutionError::TerminationUncertain => {
                                MultiPatchError::FormatterUncertain
                            }
                        })?;
                    let note = crate::formatters::describe(&outcome);
                    let bytes = match outcome {
                        crate::formatters::Outcome::Formatted { bytes, .. } => Some(bytes),
                        _ => None,
                    };
                    (bytes, note)
                }
                None => (None, None),
            };
            cancel.check().map_err(|_| MultiPatchError::Cancelled)?;
            if let Some(bytes) = formatted {
                if bytes.len() > MAX_FILE_READ_BYTES || std::str::from_utf8(&bytes).is_err() {
                    return Err(MultiPatchError::File {
                        index,
                        error: WorkspaceRecoveryError::LimitExceeded,
                    });
                }
                if team_memory_gate(self.root(), &entry.path, &bytes, "write").is_some() {
                    return Err(MultiPatchError::File {
                        index,
                        error: WorkspaceRecoveryError::UnsafeFile,
                    });
                }
                total_bytes = total_bytes
                    .checked_sub(after.bytes.len())
                    .and_then(|total| total.checked_add(bytes.len()))
                    .filter(|total| *total <= workspace::multi_edit_recovery::MAX_RECOVERY_BYTES)
                    .ok_or(MultiPatchError::File {
                        index,
                        error: WorkspaceRecoveryError::LimitExceeded,
                    })?;
                after.bytes = bytes;
            }
            if let Some(note) = note {
                notes.push('\n');
                notes.push_str(&self.redact_output(note));
            }
        }
        let mut original_indices = Vec::new();
        let changed = entries
            .into_iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                if entry.before == entry.after {
                    None
                } else {
                    original_indices.push(index);
                    Some(entry)
                }
            })
            .collect::<Vec<_>>();
        let record = if changed.is_empty() {
            None
        } else {
            Some(
                RecoveryRecord::new(RecoveryState::Intent, changed)
                    .map_err(|_| MultiPatchError::Invalid)?,
            )
        };
        Ok(FormattedMultiCandidate {
            record,
            original_indices,
            notes: bounded_text(notes.as_bytes(), 8 * 1024),
        })
    }

    /// Caller must acquire this workspace's exclusive journal lock. Pending
    /// approvals and configured write hooks remain unsupported and fail closed.
    /// This internal seam is not yet a model tool.
    #[allow(dead_code)]
    pub(crate) fn publish_multi_edit(
        &self,
        journal: &JournalStore,
        record: &RecoveryRecord,
        cancel: &workspace::CancellationToken,
    ) -> Result<PublishOutcome<WorkspaceRecoveryError>, WorkspaceRecoveryError> {
        self.publish_multi_edit_with_cancel(journal, record, None, || cancel.is_cancelled())
    }

    fn publish_multi_edit_with_cancel(
        &self,
        journal: &JournalStore,
        record: &RecoveryRecord,
        approval: Option<(&MultiApprovalGrant, &ValidatedToolCall)>,
        cancelled: impl Fn() -> bool,
    ) -> Result<PublishOutcome<WorkspaceRecoveryError>, WorkspaceRecoveryError> {
        self.publish_multi_edit_inner(journal, record, approval, None, cancelled)
    }

    fn publish_multi_edit_inner(
        &self,
        journal: &JournalStore,
        record: &RecoveryRecord,
        approval: Option<(&MultiApprovalGrant, &ValidatedToolCall)>,
        #[cfg(unix)] hook: Option<(&RecoveryRecord, &MultiHookReady)>,
        #[cfg(not(unix))] _hook: Option<()>,
        cancelled: impl Fn() -> bool,
    ) -> Result<PublishOutcome<WorkspaceRecoveryError>, WorkspaceRecoveryError> {
        if !cfg!(unix) {
            return Err(WorkspaceRecoveryError::Unsupported);
        }
        recovery::ensure_no_shadow_marker(journal.directory())?;
        recovery::ensure_no_external_marker(journal.directory())?;
        if approval.is_none()
            && (self.shadow_diagnostics.is_some()
                || self
                    .write_steps
                    .as_ref()
                    .is_some_and(crate::write_steps::WriteSteps::has_formatters))
        {
            return Err(WorkspaceRecoveryError::WriteStepsRequireIntegration);
        }
        if approval.is_some_and(|(grant, call)| !grant.matches(record, call)) {
            return Err(WorkspaceRecoveryError::PermissionDenied);
        }
        let mut requires_approval = false;
        let mut approved_paths = std::collections::BTreeSet::new();
        for (index, entry) in record.entries().iter().enumerate() {
            let path_approved =
                approval.is_some_and(|(grant, _)| grant.approved_paths.contains(&index));
            if path_approved {
                approved_paths.insert(entry.path.clone());
            }
            match self.check_multi_edit_permission(&entry.path) {
                Ok(()) => {}
                Err(WorkspaceRecoveryError::ApprovalRequired) if path_approved => {}
                Err(WorkspaceRecoveryError::ApprovalRequired) => requires_approval = true,
                Err(error) => return Err(error),
            }
        }
        if requires_approval {
            return Err(WorkspaceRecoveryError::ApprovalRequired);
        }
        #[cfg(unix)]
        if let Some((original, ready)) = hook {
            let (_, call) = approval.ok_or(WorkspaceRecoveryError::HooksRequireIntegration)?;
            if !ready.grant.matches(&ready.candidate, call)
                || super::multi_hook_checkpoint::HookCheckpoint::new(
                    journal,
                    &ready.chain,
                    original,
                    call.arguments(),
                    &self.hooks.pre_tool_use,
                )
                .and_then(|checkpoint| checkpoint.approval_binding(&ready.report))
                .is_err()
            {
                return Err(WorkspaceRecoveryError::HooksRequireIntegration);
            }
        } else {
            self.check_multi_edit_integrations()?;
        }
        #[cfg(not(unix))]
        self.check_multi_edit_integrations()?;
        if has_symlink_component(self.root(), JOURNAL_PATH) {
            return Err(WorkspaceRecoveryError::UnsafeFile);
        }
        let expected = protocol::host_path::canonicalize(self.root.join(JOURNAL_PATH))
            .map_err(|_| WorkspaceRecoveryError::InvalidPath)?;
        if expected != journal.directory() || !expected.starts_with(self.root()) {
            return Err(WorkspaceRecoveryError::InvalidPath);
        }
        if !self.write_locks.healthy() || !self.check_read_epoch() {
            return Err(WorkspaceRecoveryError::Lock);
        }
        let mut paths: Vec<_> = record
            .entries()
            .iter()
            .map(|entry| self.root.join(&entry.path))
            .collect();
        paths.sort_by_key(|path| path.to_string_lossy().to_ascii_lowercase());
        let locks: Vec<_> = paths
            .iter()
            .map(|path| self.write_locks.lock_for(path))
            .collect();
        let _guards: Vec<_> = locks
            .iter()
            .map(|lock| lock.lock().map_err(|_| WorkspaceRecoveryError::Lock))
            .collect::<Result<_, _>>()?;
        let recovery = WorkspaceRecovery { tools: self };
        let authority = self
            .read_authority
            .as_ref()
            .ok_or(WorkspaceRecoveryError::ReadRequired)?;
        for entry in record.entries() {
            let target = recovery.target(&entry.path)?;
            let guarded = authority
                .guard_write(self.root(), &target)
                .map_err(|_| WorkspaceRecoveryError::ReadRequired)?;
            if guarded.original.as_deref()
                != entry.before.as_ref().map(|image| image.bytes.as_slice())
            {
                return Err(WorkspaceRecoveryError::ReadRequired);
            }
        }
        let bytes = record
            .entries()
            .iter()
            .try_fold(0usize, |sum, entry| {
                sum.checked_add(entry.after.as_ref().map_or(0, |image| image.bytes.len()))
            })
            .ok_or(WorkspaceRecoveryError::LimitExceeded)?;
        if self.reserve_write_budget(bytes).is_some() {
            return Err(WorkspaceRecoveryError::LimitExceeded);
        }
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|_| WorkspaceRecoveryError::Io)?;
        let transaction_id: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let record = record
            .clone()
            .with_staging_plan(transaction_id)
            .map_err(|_| WorkspaceRecoveryError::Io)?;
        let mut adapter = WorkspacePublisher {
            recovery,
            staged: BTreeMap::new(),
            identities: BTreeMap::new(),
            approved_paths,
            journal_owned: false,
        };
        let outcome =
            multi_edit_publish::publish_journal_first(journal, &mut adapter, &record, cancelled);
        if matches!(
            outcome,
            PublishOutcome::Committed | PublishOutcome::CommittedCleanupPending { .. }
        ) && let Some(changes) = self.changes.as_ref()
        {
            for entry in record.entries() {
                let before = entry.before.as_ref().map(|image| line_count(&image.bytes));
                let after_bytes = entry
                    .after
                    .as_ref()
                    .map_or(&[][..], |image| image.bytes.as_slice());
                let hunks = match (
                    entry
                        .before
                        .as_ref()
                        .map(|image| std::str::from_utf8(&image.bytes))
                        .unwrap_or(Ok("")),
                    std::str::from_utf8(after_bytes),
                ) {
                    (Ok(before), Ok(after)) => match crate::line_diff::unified(before, after) {
                        crate::line_diff::DiffOutcome::Unified(text) => Some(text),
                        _ => None,
                    },
                    _ => None,
                };
                changes.wrote(
                    &entry.path,
                    before,
                    line_count(after_bytes),
                    hunks.as_deref(),
                );
            }
        }
        Ok(outcome)
    }

    pub(super) fn check_multi_edit_integrations(&self) -> Result<(), WorkspaceRecoveryError> {
        if !self.hooks.pre_tool_use.is_empty() {
            return Err(WorkspaceRecoveryError::HooksRequireIntegration);
        }
        Ok(())
    }

    fn check_multi_edit_permission(&self, path: &str) -> Result<(), WorkspaceRecoveryError> {
        if recovery::is_journal_path(path)
            || is_git_internal_path(path)
            || is_protected_settings_path(path)
        {
            return Err(WorkspaceRecoveryError::UnsafeFile);
        }
        if self.read_only_blocks(WORKSPACE_MULTI_PATCH_TOOL)
            || self
                .role_surface
                .is_some_and(|surface| !role_surface_allows(surface, WORKSPACE_MULTI_PATCH_TOOL))
            || self
                .agent_filter
                .as_ref()
                .is_some_and(|agent| !agent.allows(WORKSPACE_MULTI_PATCH_TOOL))
        {
            return Err(WorkspaceRecoveryError::PermissionDenied);
        }
        // Until a dedicated multi-file policy exists, require both existing
        // edit surfaces so a path-specific deny on either cannot be evaded.
        let mut requires_approval = false;
        for tool in [WORKSPACE_WRITE_TOOL, WORKSPACE_PATCH_TOOL] {
            if self.read_only_blocks(tool)
                || self
                    .role_surface
                    .is_some_and(|surface| !role_surface_allows(surface, tool))
                || self
                    .agent_filter
                    .as_ref()
                    .is_some_and(|agent| !agent.allows(tool))
            {
                return Err(WorkspaceRecoveryError::PermissionDenied);
            }
            let args = if tool == WORKSPACE_WRITE_TOOL {
                serde_json::json!({"path": path, "content": ""})
            } else {
                serde_json::json!({"path": path, "old": "x", "new": "y"})
            };
            if Self::rule_subject(tool, &args.to_string()).as_deref() != Some(path) {
                return Err(WorkspaceRecoveryError::InvalidPath);
            }
            let proposed = ProposedToolCall::new("multi-edit-policy", tool, args.to_string())
                .map_err(|_| WorkspaceRecoveryError::InvalidPath)?;
            match self.permission_for(&ValidatedToolCall::from_proposed(&proposed)) {
                Decision::Allow(_) => {}
                Decision::Ask(_) => requires_approval = true,
                Decision::Deny(_) => return Err(WorkspaceRecoveryError::PermissionDenied),
            }
        }
        let judge = |lattice: &PermissionLattice| {
            lattice.evaluate(WORKSPACE_MULTI_PATCH_TOOL, path, ToolClass::FileEdit)
        };
        let mut decision = judge(&self.permissions);
        if decision.is_allowed() && self.plan_mode.load(Ordering::SeqCst) {
            decision = judge(&self.permissions.in_plan_mode());
        }
        if decision == Decision::Allow(crate::permissions::DecisionReason::PlanFileCarveOut)
            && has_symlink_component(self.root(), path)
        {
            return Err(WorkspaceRecoveryError::PermissionDenied);
        }
        match decision {
            Decision::Allow(_) => {}
            Decision::Ask(_) => requires_approval = true,
            Decision::Deny(_) => return Err(WorkspaceRecoveryError::PermissionDenied),
        }
        if requires_approval {
            Err(WorkspaceRecoveryError::ApprovalRequired)
        } else {
            Ok(())
        }
    }
}

struct WorkspacePublisher<'a> {
    recovery: WorkspaceRecovery<'a>,
    staged: BTreeMap<String, PathBuf>,
    identities: BTreeMap<String, StageIdentity>,
    approved_paths: std::collections::BTreeSet<String>,
    journal_owned: bool,
}

impl WorkspacePublisher<'_> {
    fn check_path_permission(&self, path: &str) -> Result<(), WorkspaceRecoveryError> {
        match self.recovery.tools.check_multi_edit_permission(path) {
            Err(WorkspaceRecoveryError::ApprovalRequired) if self.approved_paths.contains(path) => {
                Ok(())
            }
            other => other,
        }
    }
    fn cleanup(&mut self) -> Result<(), WorkspaceRecoveryError> {
        let mut failed = false;
        for (_, path) in std::mem::take(&mut self.staged) {
            if let Err(error) = fs::remove_file(path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                failed = true;
            }
        }
        if failed {
            Err(WorkspaceRecoveryError::Io)
        } else {
            Ok(())
        }
    }
}
impl Drop for WorkspacePublisher<'_> {
    fn drop(&mut self) {
        if !self.journal_owned {
            let _ = self.cleanup();
        }
    }
}

impl RecoveryIo for WorkspacePublisher<'_> {
    type Error = WorkspaceRecoveryError;
    fn observe(&mut self, path: &str) -> Result<Option<FileImage>, Self::Error> {
        self.recovery.observe(path)
    }
    fn restore_if_matches(
        &mut self,
        path: &str,
        expected: &Option<FileImage>,
        before: &Option<FileImage>,
    ) -> Result<bool, Self::Error> {
        self.recovery.restore_if_matches(path, expected, before)
    }
    fn before_settlement(&mut self, record: &RecoveryRecord) -> Result<(), Self::Error> {
        #[cfg(all(test, unix))]
        if record.state() == RecoveryState::Committed {
            pause_at_crash_boundary(self.recovery.tools.root(), "committed");
        }
        self.recovery.before_settlement(record)?;
        if self.journal_owned {
            self.staged.clear();
            Ok(())
        } else {
            self.cleanup()
        }
    }
}

#[cfg(unix)]
impl JournaledPublishIo for WorkspacePublisher<'_> {
    fn prepare_journaled(
        &mut self,
        journal: &JournalStore,
        record: &mut RecoveryRecord,
    ) -> Result<(), JournaledPrepareError<Self::Error>> {
        self.journal_owned = true;
        for index in 0..record.entries().len() {
            let entry = record.entries()[index].clone();
            let Some(after) = entry.after else { continue };
            let mode = after.mode.ok_or(JournaledPrepareError::Io(
                WorkspaceRecoveryError::UnsafeFile,
            ))?;
            let target = self
                .recovery
                .target(&entry.path)
                .map_err(JournaledPrepareError::Io)?;
            let parent = target.parent().ok_or(JournaledPrepareError::Io(
                WorkspaceRecoveryError::InvalidPath,
            ))?;
            let name = record.staging_name(index).ok_or(JournaledPrepareError::Io(
                WorkspaceRecoveryError::InvalidPath,
            ))?;
            let staged = parent.join(name);
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options
                .open(&staged)
                .map_err(|_| JournaledPrepareError::Io(WorkspaceRecoveryError::Io))?;
            self.staged.insert(entry.path.clone(), staged);
            #[cfg(unix)]
            let identity = {
                use std::os::unix::fs::MetadataExt;
                let metadata = file
                    .metadata()
                    .map_err(|_| JournaledPrepareError::Io(WorkspaceRecoveryError::Io))?;
                StageIdentity {
                    device: metadata.dev(),
                    inode: metadata.ino(),
                }
            };
            fs::File::open(parent)
                .and_then(|dir| dir.sync_all())
                .map_err(|_| JournaledPrepareError::Io(WorkspaceRecoveryError::Io))?;
            #[cfg(test)]
            pause_at_crash_boundary(self.recovery.tools.root(), "created");
            *record = journal
                .register_stage(record, index, identity)
                .map_err(JournaledPrepareError::Journal)?;
            self.identities.insert(entry.path, identity);
            #[cfg(test)]
            pause_at_crash_boundary(self.recovery.tools.root(), "registered");
            #[cfg(test)]
            if std::env::var("RAPID_TEST_MULTI_CRASH_POINT").as_deref() == Ok("partial") {
                file.write_all(&after.bytes[..after.bytes.len().min(1)])
                    .and_then(|_| file.sync_all())
                    .map_err(|_| JournaledPrepareError::Io(WorkspaceRecoveryError::Io))?;
                pause_at_crash_boundary(self.recovery.tools.root(), "partial");
            }
            file.write_all(&after.bytes)
                .map_err(|_| JournaledPrepareError::Io(WorkspaceRecoveryError::Io))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(fs::Permissions::from_mode(mode))
                    .map_err(|_| JournaledPrepareError::Io(WorkspaceRecoveryError::Io))?;
            }
            file.sync_all()
                .map_err(|_| JournaledPrepareError::Io(WorkspaceRecoveryError::Io))?;
        }
        Ok(())
    }
}

#[cfg(not(unix))]
impl JournaledPublishIo for WorkspacePublisher<'_> {
    fn prepare_journaled(
        &mut self,
        _: &JournalStore,
        _: &mut RecoveryRecord,
    ) -> Result<(), JournaledPrepareError<Self::Error>> {
        Err(JournaledPrepareError::Io(
            WorkspaceRecoveryError::Unsupported,
        ))
    }
}
impl PublishIo for WorkspacePublisher<'_> {
    fn before_commit(&mut self, record: &RecoveryRecord) -> Result<(), Self::Error> {
        if !self.recovery.tools.check_read_epoch() {
            return Err(WorkspaceRecoveryError::Lock);
        }
        for entry in record.entries() {
            self.check_path_permission(&entry.path)?;
        }
        Ok(())
    }

    fn prepare(&mut self, record: &RecoveryRecord) -> Result<(), Self::Error> {
        let result = (|| {
            for entry in record.entries() {
                let Some(after) = &entry.after else {
                    continue;
                };
                let mode = after.mode.ok_or(WorkspaceRecoveryError::UnsafeFile)?;
                let target = self.recovery.target(&entry.path)?;
                let parent = target.parent().ok_or(WorkspaceRecoveryError::InvalidPath)?;
                let name = format!(
                    ".rapidlm-multi-{}-{}.tmp",
                    std::process::id(),
                    ATOMIC_WRITE_SEQ.fetch_add(1, Ordering::Relaxed)
                );
                let staged = parent.join(name);
                let mut options = fs::OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                let mut file = options
                    .open(&staged)
                    .map_err(|_| WorkspaceRecoveryError::Io)?;
                self.staged.insert(entry.path.clone(), staged);
                file.write_all(&after.bytes)
                    .map_err(|_| WorkspaceRecoveryError::Io)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    file.set_permissions(fs::Permissions::from_mode(mode))
                        .map_err(|_| WorkspaceRecoveryError::Io)?;
                }
                file.sync_all().map_err(|_| WorkspaceRecoveryError::Io)?;
                fs::File::open(parent)
                    .and_then(|dir| dir.sync_all())
                    .map_err(|_| WorkspaceRecoveryError::Io)?;
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = self.cleanup();
        }
        result
    }

    fn publish_if_matches(
        &mut self,
        path: &str,
        expected: &Option<FileImage>,
        after: &Option<FileImage>,
    ) -> Result<bool, Self::Error> {
        self.check_path_permission(path)?;
        if !self.recovery.tools.check_read_epoch() {
            return Err(WorkspaceRecoveryError::Lock);
        }
        let target = self.recovery.target(path)?;
        let guarded = self
            .recovery
            .tools
            .read_authority
            .as_ref()
            .ok_or(WorkspaceRecoveryError::ReadRequired)?
            .guard_write(self.recovery.tools.root(), &target)
            .map_err(|_| WorkspaceRecoveryError::ReadRequired)?;
        if guarded.original.as_deref() != expected.as_ref().map(|image| image.bytes.as_slice()) {
            return Err(WorkspaceRecoveryError::ReadRequired);
        }
        if &self.observe(path)? != expected {
            return Ok(false);
        }
        if let Some(after) = after {
            let staged = self.staged.get(path).ok_or(WorkspaceRecoveryError::Io)?;
            let metadata = fs::symlink_metadata(staged).map_err(|_| WorkspaceRecoveryError::Io)?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(WorkspaceRecoveryError::UnsafeFile);
            }
            #[cfg(unix)]
            if self.journal_owned {
                use std::os::unix::fs::MetadataExt;
                let identity = self
                    .identities
                    .get(path)
                    .ok_or(WorkspaceRecoveryError::UnsafeFile)?;
                if identity.device != metadata.dev() || identity.inode != metadata.ino() {
                    return Err(WorkspaceRecoveryError::UnsafeFile);
                }
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if Some(metadata.permissions().mode() & 0o7777) != after.mode {
                    return Err(WorkspaceRecoveryError::UnsafeFile);
                }
            }
            let contents =
                read_file_bounded(staged, workspace::multi_edit_recovery::MAX_RECOVERY_BYTES)
                    .map_err(|_| WorkspaceRecoveryError::Io)?;
            if contents != after.bytes {
                return Err(WorkspaceRecoveryError::UnsafeFile);
            }
            if expected.is_none() {
                match fs::hard_link(staged, &target) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        return Ok(false);
                    }
                    Err(_) => return Err(WorkspaceRecoveryError::Io),
                }
                self.recovery.invalidate(path)?;
                fs::remove_file(staged).map_err(|_| WorkspaceRecoveryError::Io)?;
            } else {
                fs::rename(staged, &target).map_err(|_| WorkspaceRecoveryError::Io)?;
                self.recovery.invalidate(path)?;
            }
            self.staged.remove(path);
        } else {
            fs::remove_file(&target).map_err(|_| WorkspaceRecoveryError::Io)?;
            self.recovery.invalidate(path)?;
        }
        let parent = target.parent().ok_or(WorkspaceRecoveryError::InvalidPath)?;
        fs::File::open(parent)
            .and_then(|dir| dir.sync_all())
            .map_err(|_| WorkspaceRecoveryError::Io)?;
        Ok(true)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use workspace::multi_edit_publish::PublishFailure;
    use workspace::multi_edit_recovery::{RecoveryEntry, RecoveryState};

    #[derive(Default)]
    struct ApprovalTestSink {
        pending: Mutex<Vec<crate::approvals::ApprovalRequest>>,
        resolved: Mutex<Vec<kernel::ApprovalRequestedPayload>>,
    }
    impl crate::approvals::ApprovalSink for ApprovalTestSink {
        fn request(&self, request: &crate::approvals::ApprovalRequest) -> Result<String, String> {
            self.pending.lock().unwrap().push(request.clone());
            Ok("test-wait".into())
        }
        fn approved_chain_sources(
            &self,
            _: &str,
            _: &str,
            _: &str,
            prefix: &str,
        ) -> Result<Vec<kernel::ApprovalRequestedPayload>, String> {
            Ok(self
                .resolved
                .lock()
                .unwrap()
                .iter()
                .filter(|request| {
                    request
                        .source
                        .as_deref()
                        .is_some_and(|source| source.starts_with(prefix))
                })
                .cloned()
                .collect())
        }
    }
    impl ApprovalTestSink {
        fn approve_next(&self) -> crate::approvals::ApprovedAsk {
            let request = self.pending.lock().unwrap().remove(0);
            let approved = crate::approvals::ApprovedAsk {
                source: request.source.clone(),
                arguments_digest: request.arguments_digest.clone(),
            };
            let payload = serde_json::from_value(serde_json::json!({
                "id":"test-wait", "call_id":request.call_id, "tool":request.tool, "summary":request.summary,
                "scope":request.scope, "diff":request.diff, "source":request.source, "arguments_digest":request.arguments_digest
            })).unwrap();
            self.resolved.lock().unwrap().push(payload);
            approved
        }
    }

    #[test]
    fn per_path_approval_chain_resumes_and_rejects_changed_candidates() {
        let root = Temp::new();
        let mut tools = tools(&root);
        tools.permissions = PermissionLattice::new(PermissionMode::Default);
        let sink = Arc::new(ApprovalTestSink::default());
        tools.approval_sink = Some(sink.clone());
        let proposed = ProposedToolCall::new("call", "workspace_multi_patch", "{}").unwrap();
        let call = ValidatedToolCall::from_proposed(&proposed);
        let candidate = record();
        assert_eq!(
            tools.advance_multi_approval(&candidate, &call, None),
            Ok(MultiApprovalProgress::Waiting)
        );
        assert_eq!(sink.pending.lock().unwrap()[0].scope, vec!["a"]);
        let first = sink.approve_next();
        assert_eq!(
            tools.advance_multi_approval(&candidate, &call, Some(&first)),
            Ok(MultiApprovalProgress::Waiting)
        );
        assert_eq!(sink.pending.lock().unwrap()[0].scope, vec!["b"]);
        let second = sink.approve_next();
        let grant = match tools
            .advance_multi_approval(&candidate, &call, Some(&second))
            .unwrap()
        {
            MultiApprovalProgress::Ready(grant) => grant,
            MultiApprovalProgress::Waiting => panic!("all paths were approved"),
        };
        assert_eq!(
            grant.approved_paths,
            std::collections::BTreeSet::from([0, 1])
        );
        let mut entries = candidate.entries().to_vec();
        entries[1].after.as_mut().unwrap().bytes = b"different".to_vec();
        let changed = RecoveryRecord::new(RecoveryState::Intent, entries).unwrap();
        assert_eq!(
            tools.advance_multi_approval(&changed, &call, Some(&second)),
            Err(MultiApprovalError::InvalidApproval)
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b0");
        let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
        let other = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("other", "workspace_multi_patch", "{}").unwrap(),
        );
        assert_eq!(
            tools.publish_multi_edit_with_cancel(
                &journal,
                &candidate,
                Some((&grant, &other)),
                || false
            ),
            Err(WorkspaceRecoveryError::PermissionDenied)
        );
        assert_eq!(
            tools.publish_multi_edit_with_cancel(&journal, &changed, Some((&grant, &call)), || {
                false
            }),
            Err(WorkspaceRecoveryError::PermissionDenied)
        );
        use crate::permissions::{RuleEffect, ToolPattern, ToolRule};
        tools.permissions =
            PermissionLattice::new(PermissionMode::Default).with_rules(vec![ToolRule {
                effect: RuleEffect::Deny,
                pattern: ToolPattern::parse("workspace_patch(b)").unwrap(),
            }]);
        assert_eq!(
            tools.publish_multi_edit_with_cancel(
                &journal,
                &candidate,
                Some((&grant, &call)),
                || false
            ),
            Err(WorkspaceRecoveryError::PermissionDenied)
        );
        tools.permissions = PermissionLattice::new(PermissionMode::Default);
        assert_eq!(
            tools
                .publish_multi_edit_with_cancel(&journal, &candidate, Some((&grant, &call)), || {
                    false
                })
                .unwrap(),
            PublishOutcome::Committed
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a1");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b1");
    }

    #[test]
    fn per_path_approval_checks_all_denials_before_requesting() {
        use crate::permissions::{RuleEffect, ToolPattern, ToolRule};
        let root = Temp::new();
        let mut tools = tools(&root);
        tools.permissions =
            PermissionLattice::new(PermissionMode::Default).with_rules(vec![ToolRule {
                effect: RuleEffect::Deny,
                pattern: ToolPattern::parse("workspace_patch(b)").unwrap(),
            }]);
        let sink = Arc::new(ApprovalTestSink::default());
        tools.approval_sink = Some(sink.clone());
        let call = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("call", "workspace_multi_patch", "{}").unwrap(),
        );
        assert_eq!(
            tools.advance_multi_approval(&record(), &call, None),
            Err(MultiApprovalError::Policy(
                WorkspaceRecoveryError::PermissionDenied
            ))
        );
        assert!(sink.pending.lock().unwrap().is_empty());
    }

    #[test]
    fn approved_runtime_waits_for_each_file_then_publishes_once() {
        let root = Temp::new();
        let mut tools = tools(&root);
        tools.permissions = PermissionLattice::new(PermissionMode::Default);
        let sink = Arc::new(ApprovalTestSink::default());
        tools.approval_sink = Some(sink.clone());
        let input = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]},{"path":"b","edits":[{"old":"b0","new":"b1"}]}]}"#;
        let call = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("call", "workspace_multi_patch", input).unwrap(),
        );
        let cancel = CancellationToken::new();
        assert_eq!(
            tools.run_multi_patch_approved(&call, None, &cancel),
            Ok(ApprovedMultiPatchOutcome::Waiting)
        );
        let first = sink.approve_next();
        assert_eq!(
            tools.run_multi_patch_approved(&call, Some(&first), &cancel),
            Ok(ApprovedMultiPatchOutcome::Waiting)
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b0");
        let second = sink.approve_next();
        assert_eq!(
            tools.run_multi_patch_approved(&call, Some(&second), &cancel),
            Ok(ApprovedMultiPatchOutcome::Published(
                PublishOutcome::Committed,
                String::new()
            ))
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a1");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b1");
        assert!(
            tools
                .run_multi_patch_approved(&call, Some(&second), &cancel)
                .is_err()
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a1");
        no_staging(&root);
    }

    #[test]
    fn approved_runtime_recovers_prior_decisions_from_reopened_ledger() {
        use crate::approvals::{ApprovedAsk, LedgerApprovalSink, client_call};
        use event_ledger::event::{ActorKind, ActorRef};
        use kernel::{ApprovalDecision, KernelClient};
        let root = Temp::new();
        let ledger_path = root.0.join("approvals.sqlite");
        let client = kernel::InProcessKernelClient::open(&ledger_path).unwrap();
        let actor = ActorRef::new(ActorKind::Human, &protocol::EventId::new().to_string()).unwrap();
        let session = client_call(client.create_session(kernel::CreateSession::new(
            protocol::ProjectId::new(),
            actor.clone(),
            protocol::TraceId::new(),
        )))
        .unwrap()
        .id();
        let mut tools = tools(&root);
        tools.permissions = PermissionLattice::new(PermissionMode::Default);
        tools.approval_sink = Some(Arc::new(LedgerApprovalSink::new(
            client.clone(),
            session,
            actor.clone(),
            root.0.clone(),
        )));
        let input = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]},{"path":"b","edits":[{"old":"b0","new":"b1"}]}]}"#;
        let call = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("call", "workspace_multi_patch", input).unwrap(),
        );
        let cancel = CancellationToken::new();
        assert_eq!(
            tools.run_multi_patch_approved(&call, None, &cancel),
            Ok(ApprovedMultiPatchOutcome::Waiting)
        );
        let pending = client_call(client.pending_approvals(session)).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].payload().scope, vec!["a"]);
        let first = ApprovedAsk::from_payload(pending[0].payload());
        let seq = client_call(client.get_session(session)).unwrap().seq();
        client_call(
            client.approve(
                kernel::ResolveApproval::new(
                    session,
                    seq,
                    ApprovalDecision::Approved,
                    actor.clone(),
                    protocol::TraceId::new(),
                )
                .with_wait_token(pending[0].token()),
            ),
        )
        .unwrap();
        tools.approval_sink = None;
        drop(client);
        let client = kernel::InProcessKernelClient::open(&ledger_path).unwrap();
        tools.approval_sink = Some(Arc::new(LedgerApprovalSink::new(
            client.clone(),
            session,
            actor.clone(),
            root.0.clone(),
        )));
        assert_eq!(
            tools.run_multi_patch_approved(&call, Some(&first), &cancel),
            Ok(ApprovedMultiPatchOutcome::Waiting)
        );
        let pending = client_call(client.pending_approvals(session)).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].payload().scope, vec!["b"]);
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        let second = ApprovedAsk::from_payload(pending[0].payload());
        let seq = client_call(client.get_session(session)).unwrap().seq();
        client_call(
            client.approve(
                kernel::ResolveApproval::new(
                    session,
                    seq,
                    ApprovalDecision::Approved,
                    actor.clone(),
                    protocol::TraceId::new(),
                )
                .with_wait_token(pending[0].token()),
            ),
        )
        .unwrap();
        tools.approval_sink = None;
        drop(client);
        let client = kernel::InProcessKernelClient::open(&ledger_path).unwrap();
        tools.approval_sink = Some(Arc::new(LedgerApprovalSink::new(
            client,
            session,
            actor,
            root.0.clone(),
        )));
        assert_eq!(
            tools.run_multi_patch_approved(&call, Some(&second), &cancel),
            Ok(ApprovedMultiPatchOutcome::Published(
                PublishOutcome::Committed,
                String::new()
            ))
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a1");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b1");
        no_staging(&root);
    }

    #[test]
    fn tool_dispatch_reports_each_committed_path_and_honors_its_own_deny_rules() {
        use crate::permissions::{RuleEffect, ToolPattern, ToolRule};
        struct Changes(Mutex<Vec<String>>);
        impl WorkspaceChanges for Changes {
            fn wrote(&self, path: &str, _: Option<u64>, _: u64, _: Option<&str>) {
                self.0.lock().unwrap().push(path.to_owned());
            }
        }
        let root = Temp::new();
        let mut tools = tools(&root);
        let changes = Arc::new(Changes(Mutex::new(Vec::new())));
        tools.set_workspace_changes(changes.clone());
        let input = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]},{"path":"b","edits":[{"old":"b0","new":"b1"}]}]}"#;
        let call = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("call", WORKSPACE_MULTI_PATCH_TOOL, input).unwrap(),
        );
        assert!(
            tools
                .tool_surface()
                .iter()
                .any(|tool| tool.name() == WORKSPACE_MULTI_PATCH_TOOL)
        );
        tools.permissions =
            PermissionLattice::new(PermissionMode::BypassPermissions).with_rules(vec![ToolRule {
                effect: RuleEffect::Deny,
                pattern: ToolPattern::parse("workspace_multi_patch(b)").unwrap(),
            }]);
        assert!(matches!(
            tools.execute(&call, &CancellationToken::new()).unwrap(),
            ToolStepResult::Denied { .. }
        ));
        assert!(changes.0.lock().unwrap().is_empty());
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        tools.permissions = PermissionLattice::new(PermissionMode::BypassPermissions);
        let outcome = tools.execute(&call, &CancellationToken::new()).unwrap();
        assert!(
            matches!(outcome, ToolStepResult::Succeeded { .. }),
            "{outcome:?}"
        );
        assert_eq!(*changes.0.lock().unwrap(), vec!["a", "b"]);
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a1");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b1");
    }

    #[test]
    fn tool_dispatch_can_edit_create_and_delete_as_one_transaction() {
        let root = Temp::new();
        let mut tools = tools(&root);
        let input = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]},{"path":"c","create":"new"},{"path":"b","delete":true}]}"#;
        let call = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("call", WORKSPACE_MULTI_PATCH_TOOL, input).unwrap(),
        );
        assert!(matches!(
            tools.execute(&call, &CancellationToken::new()).unwrap(),
            ToolStepResult::Succeeded { .. }
        ));
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a1");
        assert_eq!(fs::read(root.0.join("c")).unwrap(), b"new");
        assert_eq!(
            fs::metadata(root.0.join("c")).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!root.0.join("b").exists());
        no_staging(&root);
    }

    #[test]
    fn create_collision_and_unread_deletion_leave_all_paths_unchanged() {
        let root = Temp::new();
        let mut tools = WorkspaceTools::open_with_permissions(
            &root.0,
            PermissionLattice::new(PermissionMode::BypassPermissions),
        )
        .unwrap();
        let cancel = CancellationToken::new();
        for input in [
            r#"{"files":[{"path":"a","create":"replacement"},{"path":"c","create":"new"}]}"#,
            r#"{"files":[{"path":"b","delete":true},{"path":"c","create":"new"}]}"#,
            r#"{"files":[{"path":"c","create":"new","delete":true}]}"#,
            r#"{"files":[{"path":"c","delete":false}]}"#,
            r#"{"files":[{"path":"missing/c","create":"new"}]}"#,
        ] {
            let call = ValidatedToolCall::from_proposed(
                &ProposedToolCall::new("call", WORKSPACE_MULTI_PATCH_TOOL, input).unwrap(),
            );
            assert!(
                matches!(
                    tools.execute(&call, &cancel).unwrap(),
                    ToolStepResult::Failed { .. }
                ),
                "{input}"
            );
            assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
            assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b0");
            assert!(!root.0.join("c").exists());
        }
        no_staging(&root);
    }

    #[test]
    fn create_and_delete_approvals_show_the_action_and_exact_path() {
        let root = Temp::new();
        let mut tools = tools(&root);
        tools.permissions = PermissionLattice::new(PermissionMode::Default);
        let sink = Arc::new(ApprovalTestSink::default());
        tools.approval_sink = Some(sink.clone());
        let input = r#"{"files":[{"path":"c","create":"new"},{"path":"b","delete":true}]}"#;
        let call = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("call", WORKSPACE_MULTI_PATCH_TOOL, input).unwrap(),
        );
        let cancel = CancellationToken::new();
        assert_eq!(
            tools.run_multi_patch_approved(&call, None, &cancel),
            Ok(ApprovedMultiPatchOutcome::Waiting)
        );
        let first = sink.pending.lock().unwrap()[0].clone();
        assert_eq!(first.scope, vec!["c"]);
        assert!(
            first.summary.contains("create")
                && first.summary.contains("0600")
                && first.diff.contains("new")
        );
        let approved = sink.approve_next();
        assert_eq!(
            tools.run_multi_patch_approved(&call, Some(&approved), &cancel),
            Ok(ApprovedMultiPatchOutcome::Waiting)
        );
        let second = sink.pending.lock().unwrap()[0].clone();
        assert_eq!(second.scope, vec!["b"]);
        assert!(second.summary.contains("delete") && second.diff.contains("b0"));
        let approved = sink.approve_next();
        assert_eq!(
            tools.run_multi_patch_approved(&call, Some(&approved), &cancel),
            Ok(ApprovedMultiPatchOutcome::Published(
                PublishOutcome::Committed,
                String::new()
            ))
        );
        assert_eq!(fs::read(root.0.join("c")).unwrap(), b"new");
        assert!(!root.0.join("b").exists());
    }

    #[test]
    fn configured_pre_hooks_run_for_multi_file_dispatch() {
        let root = Temp::new();
        let mut tools = tools(&root);
        let script = root.0.join("pre.sh");
        fs::write(
            &script,
            format!(
                "printf x > '{}'\nprintf '%s\\n' '{{\"decision\":\"allow\"}}'\n",
                root.0.join("hook-ran").display()
            ),
        )
        .unwrap();
        tools
            .hooks
            .pre_tool_use
            .push(crate::hooks::HookEntry::plain(format!(
                "sh {}",
                script.display()
            )));
        assert!(
            tools
                .tool_surface()
                .iter()
                .any(|tool| tool.name() == WORKSPACE_MULTI_PATCH_TOOL)
        );
        let input = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]}]}"#;
        let call = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("call", WORKSPACE_MULTI_PATCH_TOOL, input).unwrap(),
        );
        assert!(matches!(
            tools.execute(&call, &CancellationToken::new()).unwrap(),
            ToolStepResult::Succeeded { .. }
        ));
        assert_eq!(fs::read(root.0.join("hook-ran")).unwrap(), b"x");
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a1");
    }

    #[test]
    fn multi_hook_approvals_precede_execution_and_survive_each_resume() {
        let root = Temp::new();
        let mut tools = tools(&root);
        tools.permissions = PermissionLattice::new(PermissionMode::Default);
        let sink = Arc::new(ApprovalTestSink::default());
        tools.approval_sink = Some(sink.clone());
        let script = root.0.join("pre-hook.sh");
        fs::write(&script, format!(
            "printf x >> '{}'\nprintf '%s\\n' '{{\"decision\":\"ask\",\"reason\":\"review\",\"updated_input\":{{\"files\":[{{\"path\":\"b\",\"edits\":[{{\"old\":\"b0\",\"new\":\"b1\"}}]}}]}}}}'\n",
            root.0.join("hook-count").display()
        )).unwrap();
        tools
            .hooks
            .pre_tool_use
            .push(crate::hooks::HookEntry::plain(format!(
                "sh {}",
                script.display()
            )));
        let input = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]}]}"#;
        let call = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("call", WORKSPACE_MULTI_PATCH_TOOL, input).unwrap(),
        );
        let cancel = CancellationToken::new();
        let raw = tools.prepare_multi_patch(input, &cancel).unwrap();
        assert!(matches!(
            tools.advance_multi_hook_approval(&raw, &call, None, &cancel),
            Ok(MultiHookProgress::Waiting)
        ));
        assert_eq!(sink.pending.lock().unwrap()[0].scope, vec!["a"]);
        assert!(!root.0.join("hook-count").exists());
        let raw_answer = sink.approve_next();
        assert!(matches!(
            tools.advance_multi_hook_approval(&raw, &call, Some(&raw_answer), &cancel),
            Ok(MultiHookProgress::Waiting)
        ));
        assert!(
            sink.pending.lock().unwrap()[0]
                .summary
                .contains("hook question")
        );
        assert_eq!(fs::read(root.0.join("hook-count")).unwrap(), b"x");
        let hook_answer = sink.approve_next();
        assert!(matches!(
            tools.advance_multi_hook_approval(&raw, &call, Some(&hook_answer), &cancel),
            Ok(MultiHookProgress::Waiting)
        ));
        assert_eq!(sink.pending.lock().unwrap()[0].scope, vec!["b"]);
        let path_answer = sink.approve_next();
        let saved = sink.resolved.lock().unwrap().pop().unwrap();
        assert!(matches!(
            tools.advance_multi_hook_approval(&raw, &call, Some(&path_answer), &cancel),
            Err(MultiApprovalError::InvalidApproval)
        ));
        sink.resolved.lock().unwrap().push(saved);
        let ready = match tools
            .advance_multi_hook_approval(&raw, &call, Some(&path_answer), &cancel)
            .unwrap()
        {
            MultiHookProgress::Ready(ready) => ready,
            _ => panic!("expected ready"),
        };
        assert_eq!(ready.candidate.entries()[0].path, "b");
        assert!(ready.grant.matches(&ready.candidate, &call));
        assert_eq!(fs::read(root.0.join("hook-count")).unwrap(), b"x");
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b0");
    }

    #[test]
    fn multi_hook_denial_and_later_path_policy_deny_prevent_execution() {
        use crate::permissions::{RuleEffect, ToolPattern, ToolRule};
        let root = Temp::new();
        let mut tools = tools(&root);
        tools.permissions =
            PermissionLattice::new(PermissionMode::Default).with_rules(vec![ToolRule {
                effect: RuleEffect::Deny,
                pattern: ToolPattern::parse("workspace_patch(b)").unwrap(),
            }]);
        let sink = Arc::new(ApprovalTestSink::default());
        tools.approval_sink = Some(sink.clone());
        let script = root.0.join("pre-deny.sh");
        fs::write(&script, format!("printf x >> '{}'\nprintf '%s\\n' '{{\"decision\":\"deny\",\"reason\":\"blocked\"}}'\n", root.0.join("hook-count").display())).unwrap();
        tools
            .hooks
            .pre_tool_use
            .push(crate::hooks::HookEntry::plain(format!(
                "sh {}",
                script.display()
            )));
        let input = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]},{"path":"b","edits":[{"old":"b0","new":"b1"}]}]}"#;
        let call = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("call", WORKSPACE_MULTI_PATCH_TOOL, input).unwrap(),
        );
        let cancel = CancellationToken::new();
        let raw = tools.prepare_multi_patch(input, &cancel).unwrap();
        assert!(matches!(
            tools.advance_multi_hook_approval(&raw, &call, None, &cancel),
            Err(MultiApprovalError::Policy(
                WorkspaceRecoveryError::PermissionDenied
            ))
        ));
        assert!(sink.pending.lock().unwrap().is_empty());
        assert!(!root.0.join("hook-count").exists());
        tools.permissions = PermissionLattice::new(PermissionMode::Default);
        assert!(matches!(
            tools.advance_multi_hook_approval(&raw, &call, None, &cancel),
            Ok(MultiHookProgress::Waiting)
        ));
        let first = sink.approve_next();
        assert!(matches!(
            tools.advance_multi_hook_approval(&raw, &call, Some(&first), &cancel),
            Ok(MultiHookProgress::Waiting)
        ));
        assert!(!root.0.join("hook-count").exists());
        let second = sink.approve_next();
        assert!(
            matches!(tools.advance_multi_hook_approval(&raw, &call, Some(&second), &cancel), Ok(MultiHookProgress::Denied(reason)) if reason == "blocked")
        );
        assert_eq!(fs::read(root.0.join("hook-count")).unwrap(), b"x");
        assert!(matches!(
            tools.advance_multi_hook_approval(&raw, &call, Some(&second), &cancel),
            Err(MultiApprovalError::Checkpoint)
        ));
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b0");
    }

    #[test]
    fn multi_hook_rewrite_rechecks_new_paths_and_preserves_provenance() {
        let root = Temp::new();
        let mut tools = tools(&root);
        tools.permissions = PermissionLattice::new(PermissionMode::Default);
        let original = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new(
                "call",
                WORKSPACE_MULTI_PATCH_TOOL,
                r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]}]}"#,
            )
            .unwrap(),
        );
        let input = serde_json::json!({"files":[{"path":"b","edits":[{"old":"b0","new":"b1"}]}]});
        let rewrite = crate::hooks::HookRewrite {
            hook: "pre_tool_use[1]".into(),
            command_digest: "abcdef123456".into(),
            input: input.as_object().unwrap().clone(),
            contributors: vec!["pre_tool_use[0]".into(), "pre_tool_use[1]".into()],
        };
        let prepared = tools
            .prepare_multi_hook_rewrite(&original, rewrite.clone(), &CancellationToken::new())
            .unwrap();
        assert_eq!(prepared.call.call_id(), "call");
        assert_eq!(prepared.candidate.entries()[0].path, "b");
        assert_eq!(
            prepared.candidate.entries()[0]
                .after
                .as_ref()
                .unwrap()
                .bytes,
            b"b1"
        );
        assert_eq!(prepared.asks, std::collections::BTreeSet::from([0]));
        assert!(prepared.marker.contains("pre_tool_use[0], pre_tool_use[1]"));
        assert_eq!(
            prepared.provenance.before_digest,
            sha256_hex(original.arguments().as_bytes())
        );
        assert_eq!(
            prepared.provenance.after_digest,
            sha256_hex(prepared.call.arguments().as_bytes())
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b0");
        fs::write(root.0.join("b"), b"external").unwrap();
        assert!(matches!(
            tools.prepare_multi_hook_rewrite(&original, rewrite, &CancellationToken::new()),
            Err(MultiHookRewriteError::Prepare(MultiPatchError::File {
                index: 0,
                error: WorkspaceRecoveryError::ReadRequired
            }))
        ));
    }

    #[test]
    fn multi_hook_rewrite_denial_on_any_path_precedes_all_asks() {
        use crate::permissions::{RuleEffect, ToolPattern, ToolRule};
        let root = Temp::new();
        let mut tools = tools(&root);
        tools.permissions =
            PermissionLattice::new(PermissionMode::Default).with_rules(vec![ToolRule {
                effect: RuleEffect::Deny,
                pattern: ToolPattern::parse("workspace_patch(b)").unwrap(),
            }]);
        let original = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new(
                "call",
                WORKSPACE_MULTI_PATCH_TOOL,
                r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]}]}"#,
            )
            .unwrap(),
        );
        let input = serde_json::json!({"files":[
            {"path":"a","edits":[{"old":"a0","new":"a1"}]},
            {"path":"b","edits":[{"old":"b0","new":"b1"}]}
        ]});
        let rewrite = crate::hooks::HookRewrite {
            hook: "pre_tool_use[0]".into(),
            command_digest: "abcdef123456".into(),
            input: input.as_object().unwrap().clone(),
            contributors: vec!["pre_tool_use[0]".into()],
        };
        assert!(matches!(
            tools.prepare_multi_hook_rewrite(&original, rewrite, &CancellationToken::new()),
            Err(MultiHookRewriteError::Policy(
                WorkspaceRecoveryError::PermissionDenied
            ))
        ));
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b0");
    }

    #[test]
    fn multi_file_post_hooks_run_only_after_terminal_success_or_failure() {
        let root = Temp::new();
        let mut tools = tools(&root);
        tools.permissions = PermissionLattice::new(PermissionMode::Default);
        let sink = Arc::new(ApprovalTestSink::default());
        tools.approval_sink = Some(sink.clone());
        let success_hook = root.0.join("success-hook.sh");
        let failure_hook = root.0.join("failure-hook.sh");
        fs::write(
            &success_hook,
            format!(
                "cat > '{}'; echo multi-success",
                root.0.join("post.json").display()
            ),
        )
        .unwrap();
        fs::write(
            &failure_hook,
            format!(
                "cat > '{}'; echo multi-failure",
                root.0.join("failure.json").display()
            ),
        )
        .unwrap();
        tools.hooks.post_tool_use =
            crate::hooks::entries(vec![format!("sh {}", success_hook.display())]);
        tools.hooks.post_tool_use_failure =
            crate::hooks::entries(vec![format!("sh {}", failure_hook.display())]);
        assert!(
            tools
                .tool_surface()
                .iter()
                .any(|tool| tool.name() == WORKSPACE_MULTI_PATCH_TOOL)
        );
        let input = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]},{"path":"b","edits":[{"old":"b0","new":"b1"}]}]}"#;
        let call = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("call", WORKSPACE_MULTI_PATCH_TOOL, input).unwrap(),
        );
        let cancel = CancellationToken::new();
        assert!(matches!(
            tools.execute(&call, &cancel).unwrap(),
            ToolStepResult::ApprovalRequired { .. }
        ));
        assert!(!root.0.join("post.json").exists());
        assert!(!root.0.join("failure.json").exists());
        let first = sink.approve_next();
        assert!(matches!(
            tools
                .execute_preapproved_from(&call, &cancel, Some(&first))
                .unwrap(),
            ToolStepResult::ApprovalRequired { .. }
        ));
        assert!(!root.0.join("post.json").exists());
        let second = sink.approve_next();
        let result = tools
            .execute_preapproved_from(&call, &cancel, Some(&second))
            .unwrap();
        assert!(
            matches!(&result, ToolStepResult::Succeeded { summary, .. } if summary.contains("multi-success")),
            "{result:?}"
        );
        let post: serde_json::Value =
            serde_json::from_slice(&fs::read(root.0.join("post.json")).unwrap()).unwrap();
        assert_eq!(post["tool"], WORKSPACE_MULTI_PATCH_TOOL);
        assert!(post["summary"].as_str().unwrap().contains("committed"));
        assert!(!root.0.join("failure.json").exists());
        tools.permissions = PermissionLattice::new(PermissionMode::BypassPermissions);
        let bad = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new(
                "bad",
                WORKSPACE_MULTI_PATCH_TOOL,
                r#"{"files":[{"path":"missing","delete":true}]}"#,
            )
            .unwrap(),
        );
        let result = tools.execute(&bad, &cancel).unwrap();
        assert!(
            matches!(&result, ToolStepResult::Failed { detail: Some(detail), .. } if detail.contains("multi-failure")),
            "{result:?}"
        );
        let failure: serde_json::Value =
            serde_json::from_slice(&fs::read(root.0.join("failure.json")).unwrap()).unwrap();
        assert_eq!(failure["event"], "post_tool_use_failure");
        assert_eq!(failure["tool"], WORKSPACE_MULTI_PATCH_TOOL);
    }

    #[test]
    fn configured_formatter_applies_to_auto_allowed_multi_file_dispatch() {
        let root = Temp::new();
        let mut tools = tools(&root);
        let (steps, problems) = crate::write_steps::WriteSteps::parse(&serde_json::json!({
            "formatters":[{"command":["sh","-c","tr a-z A-Z"],"globs":["*"]}]
        }));
        assert!(problems.is_empty(), "{problems:?}");
        tools.set_write_steps(steps.unwrap());
        assert!(
            tools
                .tool_surface()
                .iter()
                .any(|tool| tool.name() == WORKSPACE_MULTI_PATCH_TOOL)
        );
        let input = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]}]}"#;
        let call = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("call", WORKSPACE_MULTI_PATCH_TOOL, input).unwrap(),
        );
        let outcome = tools.execute(&call, &CancellationToken::new()).unwrap();
        assert!(
            matches!(outcome, ToolStepResult::Succeeded { .. }),
            "{outcome:?}"
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"A1");
    }

    #[test]
    fn shadow_checks_all_formatted_files_before_multi_file_publication() {
        let root = Temp::new();
        for args in [
            vec!["init", "-q"],
            vec!["add", "a", "b"],
            vec![
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "commit",
                "-qm",
                "seed",
            ],
        ] {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(&root.0)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let input = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]},{"path":"b","edits":[{"old":"b0","new":"b1"}]}]}"#;
        let call = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("call", WORKSPACE_MULTI_PATCH_TOOL, input).unwrap(),
        );
        let mut tools = tools(&root);
        let (steps, _) = crate::write_steps::WriteSteps::parse(&serde_json::json!({
            "formatters":[{"command":["sh","-c","tr a-z A-Z"],"globs":["*"]}]
        }));
        tools.set_write_steps(steps.unwrap());
        tools.set_shadow_diagnostics(crate::shadow_diagnostics::ShadowDiagnosticsConfig::parse(&serde_json::json!({
            "shadow_diagnostics":{"command":["sh","-c","test \"$(cat a)\" = A1 && test \"$(cat b)\" = B1"]}
        })).unwrap());
        assert!(
            tools
                .tool_surface()
                .iter()
                .any(|tool| tool.name() == WORKSPACE_MULTI_PATCH_TOOL)
        );
        let outcome = tools.execute(&call, &CancellationToken::new()).unwrap();
        assert!(
            matches!(&outcome, ToolStepResult::Succeeded { summary, .. } if summary.contains("shadow diagnostics: ok")),
            "{outcome:?}"
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"A1");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"B1");
    }

    #[test]
    fn failing_shadow_diagnostics_reject_all_multi_file_changes() {
        let root = Temp::new();
        for args in [
            vec!["init", "-q"],
            vec!["add", "a", "b"],
            vec![
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "commit",
                "-qm",
                "seed",
            ],
        ] {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(&root.0)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let mut tools = tools(&root);
        tools.set_shadow_diagnostics(
            crate::shadow_diagnostics::ShadowDiagnosticsConfig::parse(&serde_json::json!({
                "shadow_diagnostics":{"command":["false"]}
            }))
            .unwrap(),
        );
        let input = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]},{"path":"b","edits":[{"old":"b0","new":"b1"}]}]}"#;
        let call = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("call", WORKSPACE_MULTI_PATCH_TOOL, input).unwrap(),
        );
        let result = tools.execute(&call, &CancellationToken::new()).unwrap();
        assert!(
            matches!(&result, ToolStepResult::Failed { detail: Some(detail), .. } if detail.contains("shadow diagnostics failed")),
            "{result:?}"
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b0");
    }

    #[test]
    fn completed_formatter_failure_retires_hook_and_running_marker() {
        let root = Temp::new();
        let mut tools = tools(&root);
        let script = root.0.join("pre.sh");
        fs::write(&script, "printf '%s\\n' '{\"decision\":\"allow\"}'\n").unwrap();
        tools
            .hooks
            .pre_tool_use
            .push(crate::hooks::HookEntry::plain(format!(
                "sh {}",
                script.display()
            )));
        let (steps, problems) = crate::write_steps::WriteSteps::parse(&serde_json::json!({
            "formatters":[{"command":["sh","-c","yes x | head -c 8388609"],"globs":["*"]}]
        }));
        assert!(problems.is_empty());
        tools.set_write_steps(steps.unwrap());
        let input = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]}]}"#;
        let call = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("call", WORKSPACE_MULTI_PATCH_TOOL, input).unwrap(),
        );
        assert_eq!(
            tools.run_multi_patch_composed(&call, None, &CancellationToken::new(), true),
            Err(MultiApprovalError::Prepare(MultiPatchError::File {
                index: 0,
                error: WorkspaceRecoveryError::LimitExceeded
            }))
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
        let values: Vec<_> = fs::read_dir(journal.directory())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                let name = path.file_name().unwrap().to_string_lossy();
                name.starts_with("hook-") || name.starts_with("format-")
            })
            .map(|path| fs::read(path).unwrap())
            .collect();
        assert!(values.contains(&b"rapidlm.multi-hook.retired.v1\n".to_vec()));
        assert!(values.contains(&b"rapidlm.multi-format.retired.v1\n".to_vec()));
    }

    #[test]
    fn external_command_crash_child() {
        let Some(path) = std::env::var_os("RAPID_TEST_EXTERNAL_CRASH_ROOT") else {
            return;
        };
        let stage = std::env::var("RAPID_TEST_EXTERNAL_CRASH_STAGE").unwrap();
        let root = Temp(PathBuf::from(path));
        let mut tools = tools(&root);
        let marker = root.0.join("external-child-pid");
        let command = format!("printf '%s' \"$$\" > '{}'; exec sleep 30", marker.display());
        match stage.as_str() {
            "hook" => tools
                .hooks
                .pre_tool_use
                .push(crate::hooks::HookEntry::plain(command)),
            "formatter" => {
                let (steps, _) = crate::write_steps::WriteSteps::parse(
                    &serde_json::json!({"formatters":[{"command":["sh","-c",command],"globs":["*"]}]}),
                );
                tools.set_write_steps(steps.unwrap());
            }
            _ => panic!("unknown stage"),
        }
        let input = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]}]}"#;
        let call = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("call", WORKSPACE_MULTI_PATCH_TOOL, input).unwrap(),
        );
        let _ = tools.run_multi_patch_composed(&call, None, &CancellationToken::new(), true);
        panic!("external crash child was not killed");
    }

    #[test]
    fn killed_auto_allowed_hook_or_formatter_blocks_future_writes() {
        struct CleanupGroup(Option<u32>);
        impl Drop for CleanupGroup {
            fn drop(&mut self) {
                if let Some(pid) = self.0 {
                    let _ = process_signal::signal_process_group(
                        pid,
                        process_signal::GroupSignal::Kill,
                    );
                }
            }
        }
        for stage in ["hook", "formatter"] {
            let root = Temp::new();
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("exec_tools::publication::tests::external_command_crash_child")
                .env("RAPID_TEST_EXTERNAL_CRASH_ROOT", &root.0)
                .env("RAPID_TEST_EXTERNAL_CRASH_STAGE", stage)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let pid = loop {
                if let Ok(value) = fs::read_to_string(root.0.join("external-child-pid"))
                    && let Ok(pid) = value.parse::<u32>()
                {
                    break pid;
                }
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("{stage} did not start");
                }
                std::thread::sleep(Duration::from_millis(5));
            };
            let mut cleanup = CleanupGroup(Some(pid));
            child.kill().unwrap();
            child.wait().unwrap();
            process_signal::signal_process_group(pid, process_signal::GroupSignal::Kill).unwrap();
            cleanup.0 = None;
            assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
            assert_eq!(
                tools(&root).recover_before_edit(),
                Err(WorkspaceRecoveryError::ExternalUncertain),
                "{stage}"
            );
            assert_eq!(
                recovery::lock_journal_for_write(&root.0).err(),
                Some(WorkspaceRecoveryError::ExternalUncertain)
            );
            let guard =
                fs::read_to_string(root.0.join(JOURNAL_PATH).join("external-command-running"))
                    .unwrap();
            assert!(guard.starts_with(&format!("{stage}:")));
        }
    }

    #[test]
    fn shadow_process_crash_child() {
        let Some(path) = std::env::var_os("RAPID_TEST_SHADOW_CRASH_ROOT") else {
            return;
        };
        let root = Temp(PathBuf::from(path));
        let mut tools = tools(&root);
        let marker = root.0.join("shadow-child-pid");
        let command = format!("printf '%s' \"$$\" > '{}'; exec sleep 30", marker.display());
        tools.set_shadow_diagnostics(
            crate::shadow_diagnostics::ShadowDiagnosticsConfig::parse(
                &serde_json::json!({"shadow_diagnostics":{"command":["sh","-c",command]}}),
            )
            .unwrap(),
        );
        let input = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]}]}"#;
        let call = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("call", WORKSPACE_MULTI_PATCH_TOOL, input).unwrap(),
        );
        let _ = tools.run_multi_patch_composed(&call, None, &CancellationToken::new(), true);
        panic!("shadow crash child was not killed");
    }

    #[test]
    fn killed_shadow_coordinator_leaves_a_durable_write_gate() {
        struct CleanupGroup(Option<u32>);
        impl Drop for CleanupGroup {
            fn drop(&mut self) {
                if let Some(pid) = self.0 {
                    let _ = process_signal::signal_process_group(
                        pid,
                        process_signal::GroupSignal::Kill,
                    );
                }
            }
        }
        let root = Temp::new();
        for args in [
            vec!["init", "-q"],
            vec!["add", "a", "b"],
            vec![
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "commit",
                "-qm",
                "seed",
            ],
        ] {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(&root.0)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("exec_tools::publication::tests::shadow_process_crash_child")
            .env("RAPID_TEST_SHADOW_CRASH_ROOT", &root.0)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let pid = loop {
            if let Ok(pid) = fs::read_to_string(root.0.join("shadow-child-pid"))
                && let Ok(pid) = pid.parse::<u32>()
            {
                break pid;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("shadow process did not start");
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let mut cleanup = CleanupGroup(Some(pid));
        child.kill().unwrap();
        child.wait().unwrap();
        process_signal::signal_process_group(pid, process_signal::GroupSignal::Kill).unwrap();
        cleanup.0 = None;
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        assert_eq!(
            tools(&root).recover_before_edit(),
            Err(WorkspaceRecoveryError::ShadowUncertain)
        );
        assert_eq!(
            recovery::lock_journal_for_write(&root.0).err(),
            Some(WorkspaceRecoveryError::ShadowUncertain)
        );
    }

    #[test]
    fn uncertain_shadow_termination_blocks_future_writes() {
        let root = Temp::new();
        for args in [
            vec!["init", "-q"],
            vec!["add", "a", "b"],
            vec![
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "commit",
                "-qm",
                "seed",
            ],
        ] {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(&root.0)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let mut tools = tools(&root);
        let marker = root.0.join("uncertain-shadow-started");
        let command = format!("printf x > '{}'; sleep 10", marker.display());
        tools.set_shadow_diagnostics(
            crate::shadow_diagnostics::ShadowDiagnosticsConfig::parse(
                &serde_json::json!({"shadow_diagnostics":{"command":["sh","-c",command]}}),
            )
            .unwrap(),
        );
        let input = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]}]}"#;
        let call = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("call", WORKSPACE_MULTI_PATCH_TOOL, input).unwrap(),
        );
        let cancel = CancellationToken::new();
        let shared = cancel.clone();
        let worker = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !marker.exists() {
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(5));
            }
            shared.cancel();
        });
        let result = crate::shadow_diagnostics::with_uncertain_termination(|| {
            tools.run_multi_patch_composed(&call, None, &cancel, true)
        });
        worker.join().unwrap();
        assert!(
            matches!(&result, Err(MultiApprovalError::ShadowUncertain(_))),
            "{result:?}"
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        assert_eq!(
            tools.recover_before_edit(),
            Err(WorkspaceRecoveryError::ShadowUncertain)
        );
        assert_eq!(
            recovery::lock_journal_for_write(&root.0).err(),
            Some(WorkspaceRecoveryError::ShadowUncertain)
        );
        assert_eq!(
            fs::read(root.0.join(JOURNAL_PATH).join("shadow-running"))
                .unwrap()
                .len(),
            32
        );
    }

    #[test]
    fn uncertain_subprocess_termination_keeps_running_checkpoint() {
        for stage in ["hook", "formatter"] {
            let root = Temp::new();
            let mut tools = tools(&root);
            let marker = root.0.join("command-started");
            let script = root.0.join("pre-uncertain.sh");
            let pre = if stage == "hook" {
                format!("printf x > '{}'; sleep 10", marker.display())
            } else {
                "printf '%s\\n' '{\"decision\":\"allow\"}'".to_owned()
            };
            fs::write(&script, format!("{pre}\n")).unwrap();
            tools
                .hooks
                .pre_tool_use
                .push(crate::hooks::HookEntry::plain(format!(
                    "sh {}",
                    script.display()
                )));
            if stage == "formatter" {
                let command = format!("printf x > '{}'; sleep 10; cat", marker.display());
                let (steps, _) = crate::write_steps::WriteSteps::parse(
                    &serde_json::json!({"formatters":[{"command":["sh","-c",command],"globs":["*"]}]}),
                );
                tools.set_write_steps(steps.unwrap());
            }
            let input = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]}]}"#;
            let call = ValidatedToolCall::from_proposed(
                &ProposedToolCall::new("call", WORKSPACE_MULTI_PATCH_TOOL, input).unwrap(),
            );
            let cancel = CancellationToken::new();
            let shared = cancel.clone();
            let worker = std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while !marker.exists() {
                    assert!(std::time::Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(5));
                }
                shared.cancel();
            });
            let result = crate::shadow_diagnostics::with_uncertain_termination(|| {
                tools.run_multi_patch_composed(&call, None, &cancel, true)
            });
            worker.join().unwrap();
            assert_eq!(result, Err(MultiApprovalError::Checkpoint), "{stage}");
            assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
            let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
            let prefix = if stage == "hook" { "hook-" } else { "format-" };
            let path = fs::read_dir(journal.directory())
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(prefix)
                })
                .unwrap();
            let expected: &[u8] = if stage == "hook" {
                b"rapidlm.multi-hook.running.v1\n"
            } else {
                b"rapidlm.multi-format.running.v1\n"
            };
            assert_eq!(fs::read(path).unwrap(), expected, "{stage}");
        }
    }

    #[test]
    fn cancellation_interrupts_formatter_and_retires_completed_checkpoints() {
        let root = Temp::new();
        let mut tools = tools(&root);
        let script = root.0.join("pre-format-cancel.sh");
        fs::write(&script, "printf '%s\\n' '{\"decision\":\"allow\"}'\n").unwrap();
        tools
            .hooks
            .pre_tool_use
            .push(crate::hooks::HookEntry::plain(format!(
                "sh {}",
                script.display()
            )));
        let marker = root.0.join("formatter-started");
        let finished = root.0.join("formatter-finished");
        let command = format!(
            "printf x > '{}'; sleep 10; printf x > '{}'; cat",
            marker.display(),
            finished.display()
        );
        let (steps, problems) = crate::write_steps::WriteSteps::parse(
            &serde_json::json!({"formatters":[{"command":["sh","-c",command],"globs":["*"]}]}),
        );
        assert!(problems.is_empty());
        tools.set_write_steps(steps.unwrap());
        let input = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]}]}"#;
        let call = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("call", WORKSPACE_MULTI_PATCH_TOOL, input).unwrap(),
        );
        let cancel = CancellationToken::new();
        let shared = cancel.clone();
        let worker = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !marker.exists() {
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(5));
            }
            shared.cancel();
        });
        let started = std::time::Instant::now();
        assert_eq!(
            tools.run_multi_patch_composed(&call, None, &cancel, true),
            Err(MultiApprovalError::Prepare(MultiPatchError::Cancelled))
        );
        worker.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(!finished.exists());
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
        let values: Vec<_> = fs::read_dir(journal.directory())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                let name = path.file_name().unwrap().to_string_lossy();
                name.starts_with("hook-") || name.starts_with("format-")
            })
            .map(|path| fs::read(path).unwrap())
            .collect();
        assert!(values.contains(&b"rapidlm.multi-hook.retired.v1\n".to_vec()));
        assert!(values.contains(&b"rapidlm.multi-format.retired.v1\n".to_vec()));
    }

    #[test]
    fn cancellation_during_shadow_retires_completed_hook_and_formatter() {
        let root = Temp::new();
        for args in [
            vec!["init", "-q"],
            vec!["add", "a", "b"],
            vec![
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "commit",
                "-qm",
                "seed",
            ],
        ] {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(&root.0)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let mut tools = tools(&root);
        let script = root.0.join("pre-shadow-cancel.sh");
        fs::write(&script, "printf '%s\\n' '{\"decision\":\"allow\"}'\n").unwrap();
        tools
            .hooks
            .pre_tool_use
            .push(crate::hooks::HookEntry::plain(format!(
                "sh {}",
                script.display()
            )));
        let (steps, problems) = crate::write_steps::WriteSteps::parse(
            &serde_json::json!({"formatters":[{"command":["sh","-c","tr a-z A-Z"],"globs":["*"]}]}),
        );
        assert!(problems.is_empty());
        tools.set_write_steps(steps.unwrap());
        let marker = root.0.join("shadow-started");
        let command = format!("printf x > '{}'; sleep 5", marker.display());
        tools.set_shadow_diagnostics(
            crate::shadow_diagnostics::ShadowDiagnosticsConfig::parse(
                &serde_json::json!({"shadow_diagnostics":{"command":["sh","-c",command]}}),
            )
            .unwrap(),
        );
        let input = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]}]}"#;
        let call = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("call", WORKSPACE_MULTI_PATCH_TOOL, input).unwrap(),
        );
        let cancel = CancellationToken::new();
        let shared = cancel.clone();
        let worker = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !marker.exists() {
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(5));
            }
            shared.cancel();
        });
        assert_eq!(
            tools.run_multi_patch_composed(&call, None, &cancel, true),
            Err(MultiApprovalError::Prepare(MultiPatchError::Cancelled))
        );
        worker.join().unwrap();
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
        let values: Vec<_> = fs::read_dir(journal.directory())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                let name = path.file_name().unwrap().to_string_lossy();
                name.starts_with("hook-") || name.starts_with("format-")
            })
            .map(|path| fs::read(path).unwrap())
            .collect();
        assert!(values.contains(&b"rapidlm.multi-hook.retired.v1\n".to_vec()));
        assert!(values.contains(&b"rapidlm.multi-format.retired.v1\n".to_vec()));
    }

    #[test]
    fn cancellation_interrupts_hook_and_retires_its_marker() {
        let root = Temp::new();
        let mut tools = tools(&root);
        let script = root.0.join("pre-cancel.sh");
        let marker = root.0.join("hook-started");
        fs::write(
            &script,
            format!(
                "printf x > '{}'\nsleep 10\nprintf x > '{}'\nprintf '%s\\n' '{{\"decision\":\"allow\"}}'\n",
                marker.display(), root.0.join("hook-finished").display()
            ),
        )
        .unwrap();
        tools
            .hooks
            .pre_tool_use
            .push(crate::hooks::HookEntry::plain(format!(
                "sh {}",
                script.display()
            )));
        let input = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]}]}"#;
        let call = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("call", WORKSPACE_MULTI_PATCH_TOOL, input).unwrap(),
        );
        let cancel = CancellationToken::new();
        let shared = cancel.clone();
        let worker = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !marker.exists() {
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(5));
            }
            shared.cancel();
        });
        let started = std::time::Instant::now();
        assert_eq!(
            tools.run_multi_patch_composed(&call, None, &cancel, true),
            Err(MultiApprovalError::Prepare(MultiPatchError::Cancelled))
        );
        worker.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(!root.0.join("hook-finished").exists());
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
        let hook = fs::read_dir(journal.directory())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("hook-")
            })
            .unwrap();
        assert_eq!(fs::read(hook).unwrap(), b"rapidlm.multi-hook.retired.v1\n");
    }

    #[test]
    fn composed_prepublication_conflict_retires_saved_results() {
        let root = Temp::new();
        let mut tools = tools(&root);
        let script = root.0.join("rewrite.sh");
        fs::write(&script, "printf '%s\\n' '{\"decision\":\"allow\",\"updated_input\":{\"files\":[{\"path\":\"b\",\"edits\":[{\"old\":\"b0\",\"new\":\"b1\"}]}]}}'\n").unwrap();
        tools
            .hooks
            .pre_tool_use
            .push(crate::hooks::HookEntry::plain(format!(
                "sh {}",
                script.display()
            )));
        let (steps, problems) = crate::write_steps::WriteSteps::parse(
            &serde_json::json!({"formatters":[{"command":["sh","-c","printf external > b; tr a-z A-Z"],"globs":["*"]}]}),
        );
        assert!(problems.is_empty());
        tools.set_write_steps(steps.unwrap());
        let input = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]}]}"#;
        let call = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("call", WORKSPACE_MULTI_PATCH_TOOL, input).unwrap(),
        );
        let result = tools.run_multi_patch_composed(&call, None, &CancellationToken::new(), true);
        assert!(
            matches!(&result, Err(MultiApprovalError::PublicationFailed(detail)) if detail.contains("not committed") && detail.contains("rewrote")),
            "{result:?}"
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"external");
        let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
        let retired: Vec<_> = fs::read_dir(journal.directory())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                let name = path.file_name().unwrap().to_string_lossy();
                name.starts_with("hook-") || name.starts_with("format-")
            })
            .map(|path| fs::read(path).unwrap())
            .collect();
        assert!(retired.contains(&b"rapidlm.multi-hook.retired.v1\n".to_vec()));
        assert!(retired.contains(&b"rapidlm.multi-format.retired.v1\n".to_vec()));
    }

    #[test]
    fn composed_shadow_failure_retires_checkpoints_and_reports_hook_context() {
        #[derive(Default)]
        struct Events(
            std::sync::atomic::AtomicUsize,
            Mutex<std::collections::BTreeSet<String>>,
        );
        impl HookEvents for Events {
            fn decided(&self, _: &str, _: &str, _: &crate::hooks::HookDecisionRecord) {}
            fn rewritten(&self, _: &str, _: &str, _: &HookRewriteRecord) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            fn rewritten_once(
                &self,
                tool: &str,
                call: &str,
                record: &HookRewriteRecord,
                key: &str,
            ) -> Result<(), ()> {
                if self.1.lock().unwrap().insert(key.to_owned()) {
                    self.rewritten(tool, call, record);
                }
                Ok(())
            }
            fn failed(&self, _: &str, _: &str, _: &crate::hooks::HookFailure) {}
        }
        let root = Temp::new();
        for args in [
            vec!["init", "-q"],
            vec!["add", "a", "b"],
            vec![
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "commit",
                "-qm",
                "seed",
            ],
        ] {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(&root.0)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let mut tools = tools(&root);
        let events = Arc::new(Events::default());
        tools.set_hook_events(events.clone());
        let script = root.0.join("pre-shadow.sh");
        fs::write(&script, "printf '%s\\n' '{\"decision\":\"allow\",\"additional_context\":\"hook context\",\"updated_input\":{\"files\":[{\"path\":\"b\",\"edits\":[{\"old\":\"b0\",\"new\":\"b1\"}]}]}}'\n").unwrap();
        tools
            .hooks
            .pre_tool_use
            .push(crate::hooks::HookEntry::plain(format!(
                "sh {}",
                script.display()
            )));
        let (steps, problems) = crate::write_steps::WriteSteps::parse(
            &serde_json::json!({"formatters":[{"command":["sh","-c","tr a-z A-Z"],"globs":["*"]}]}),
        );
        assert!(problems.is_empty());
        tools.set_write_steps(steps.unwrap());
        tools.set_shadow_diagnostics(
            crate::shadow_diagnostics::ShadowDiagnosticsConfig::parse(
                &serde_json::json!({"shadow_diagnostics":{"command":["false"]}}),
            )
            .unwrap(),
        );
        let input = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]}]}"#;
        let call = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("call", WORKSPACE_MULTI_PATCH_TOOL, input).unwrap(),
        );
        let outcome = tools.run_multi_patch_composed(&call, None, &CancellationToken::new(), true);
        assert!(
            matches!(&outcome, Err(MultiApprovalError::ShadowFailed(detail)) if detail.contains("shadow diagnostics failed") && detail.contains("hook context") && detail.ends_with("rewrote workspace_multi_patch input]")),
            "{outcome:?}"
        );
        assert_eq!(events.0.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b0");
        let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
        let mut retired = Vec::new();
        for entry in fs::read_dir(journal.directory()).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy();
            if name.starts_with("hook-") || name.starts_with("format-") {
                retired.push(fs::read(path).unwrap());
            }
        }
        assert!(retired.contains(&b"rapidlm.multi-hook.retired.v1\n".to_vec()));
        assert!(retired.contains(&b"rapidlm.multi-format.retired.v1\n".to_vec()));
    }

    #[test]
    fn composed_hook_formatter_and_shadow_publish_only_approved_candidate() {
        let root = Temp::new();
        for args in [
            vec!["init", "-q"],
            vec!["add", "a", "b"],
            vec![
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "commit",
                "-qm",
                "seed",
            ],
        ] {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(&root.0)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let mut tools = tools(&root);
        tools.permissions = PermissionLattice::new(PermissionMode::Default);
        let sink = Arc::new(ApprovalTestSink::default());
        tools.approval_sink = Some(sink.clone());
        let script = root.0.join("pre-composed.sh");
        fs::write(&script, format!("printf x >> '{}'\nprintf '%s\\n' '{{\"decision\":\"ask\",\"reason\":\"review\",\"additional_context\":\"hook context\",\"updated_input\":{{\"files\":[{{\"path\":\"b\",\"edits\":[{{\"old\":\"b0\",\"new\":\"b1\"}}]}}]}}}}'\n", root.0.join("hook-count").display())).unwrap();
        tools
            .hooks
            .pre_tool_use
            .push(crate::hooks::HookEntry::plain(format!(
                "sh {}",
                script.display()
            )));
        let (steps, problems) = crate::write_steps::WriteSteps::parse(&serde_json::json!({
            "formatters":[{"command":["sh","-c","printf x >> format-count; tr a-z A-Z"],"globs":["*"]}]
        }));
        assert!(problems.is_empty());
        tools.set_write_steps(steps.unwrap());
        tools.set_shadow_diagnostics(crate::shadow_diagnostics::ShadowDiagnosticsConfig::parse(&serde_json::json!({
            "shadow_diagnostics":{"command":["sh","-c","test \"$(cat a)\" = a0 && test \"$(cat b)\" = B1"]}
        })).unwrap());
        let input = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]}]}"#;
        let call = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("call", WORKSPACE_MULTI_PATCH_TOOL, input).unwrap(),
        );
        let cancel = CancellationToken::new();
        let mut latest = None;
        for expected in [
            "a",
            "hook question",
            "b",
            "formatter may run",
            "formatted content",
        ] {
            assert!(matches!(
                tools.run_multi_patch_composed(&call, latest.as_ref(), &cancel, true),
                Ok(ApprovedMultiPatchOutcome::Waiting)
            ));
            let request = sink.pending.lock().unwrap()[0].clone();
            assert!(
                request.summary.contains(expected)
                    || request.scope.iter().any(|path| path == expected),
                "{request:?}"
            );
            latest = Some(sink.approve_next());
        }
        let outcome = tools
            .run_multi_patch_composed(&call, latest.as_ref(), &cancel, true)
            .unwrap();
        assert!(
            matches!(outcome, ApprovedMultiPatchOutcome::Published(PublishOutcome::Committed, ref notes) if notes.contains("shadow diagnostics: ok") && notes.contains("hook context") && notes.contains("rewrote")),
            "{outcome:?}"
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"B1");
        assert_eq!(fs::read(root.0.join("hook-count")).unwrap(), b"x");
        assert_eq!(fs::read(root.0.join("format-count")).unwrap(), b"x");
        let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
        let hook_file = fs::read_dir(journal.directory())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("hook-")
            })
            .unwrap();
        assert_eq!(
            fs::read(hook_file).unwrap(),
            b"rapidlm.multi-hook.retired.v1\n"
        );
    }

    #[test]
    fn rewritten_hook_and_formatter_approvals_share_a_chain_without_reexecution() {
        let root = Temp::new();
        let mut tools = tools(&root);
        tools.permissions = PermissionLattice::new(PermissionMode::Default);
        let sink = Arc::new(ApprovalTestSink::default());
        tools.approval_sink = Some(sink.clone());
        let script = root.0.join("pre-format-hook.sh");
        fs::write(&script, format!("printf x >> '{}'\nprintf '%s\\n' '{{\"decision\":\"ask\",\"reason\":\"review\",\"updated_input\":{{\"files\":[{{\"path\":\"b\",\"edits\":[{{\"old\":\"b0\",\"new\":\"b1\"}}]}}]}}}}'\n", root.0.join("hook-count").display())).unwrap();
        tools
            .hooks
            .pre_tool_use
            .push(crate::hooks::HookEntry::plain(format!(
                "sh {}",
                script.display()
            )));
        let (steps, problems) = crate::write_steps::WriteSteps::parse(&serde_json::json!({
            "formatters":[{"command":["sh","-c","printf x >> format-count; tr a-z A-Z"],"globs":["*"]}]
        }));
        assert!(problems.is_empty());
        tools.set_write_steps(steps.unwrap());
        let input = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]}]}"#;
        let call = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("call", WORKSPACE_MULTI_PATCH_TOOL, input).unwrap(),
        );
        let cancel = CancellationToken::new();
        let raw = tools.prepare_multi_patch(input, &cancel).unwrap();
        assert!(matches!(
            tools.advance_multi_hook_approval_inner(&raw, &call, None, &cancel, true),
            Ok(MultiHookProgress::Waiting)
        ));
        let first = sink.approve_next();
        assert!(matches!(
            tools.advance_multi_hook_approval_inner(&raw, &call, Some(&first), &cancel, true),
            Ok(MultiHookProgress::Waiting)
        ));
        let question = sink.approve_next();
        assert!(matches!(
            tools.advance_multi_hook_approval_inner(&raw, &call, Some(&question), &cancel, true),
            Ok(MultiHookProgress::Waiting)
        ));
        let rewritten = sink.approve_next();
        let prior = match tools
            .advance_multi_hook_approval_inner(&raw, &call, Some(&rewritten), &cancel, true)
            .unwrap()
        {
            MultiHookProgress::Ready(prior) => prior,
            _ => panic!("expected hook approval"),
        };
        assert!(matches!(
            tools.advance_formatted_multi_approval_inner(
                &prior.candidate,
                &call,
                Some(&rewritten),
                &cancel,
                Some(&prior)
            ),
            Ok(FormattedProgress::Waiting)
        ));
        assert!(
            sink.pending.lock().unwrap()[0]
                .summary
                .contains("formatter may run")
        );
        assert!(!root.0.join("format-count").exists());
        let before = sink.approve_next();
        let prior = match tools
            .advance_multi_hook_approval_inner(&raw, &call, Some(&before), &cancel, true)
            .unwrap()
        {
            MultiHookProgress::Ready(prior) => prior,
            _ => panic!("expected hook approval"),
        };
        assert!(!prior.latest_matches);
        let saved = sink.resolved.lock().unwrap().pop().unwrap();
        assert!(matches!(
            tools.advance_formatted_multi_approval_inner(
                &prior.candidate,
                &call,
                Some(&before),
                &cancel,
                Some(&prior)
            ),
            Err(MultiApprovalError::InvalidApproval)
        ));
        assert!(!root.0.join("format-count").exists());
        sink.resolved.lock().unwrap().push(saved);
        assert!(matches!(
            tools.advance_formatted_multi_approval_inner(
                &prior.candidate,
                &call,
                Some(&before),
                &cancel,
                Some(&prior)
            ),
            Ok(FormattedProgress::Waiting)
        ));
        assert!(sink.pending.lock().unwrap()[0].diff.contains("B1"));
        let after = sink.approve_next();
        let prior = match tools
            .advance_multi_hook_approval_inner(&raw, &call, Some(&after), &cancel, true)
            .unwrap()
        {
            MultiHookProgress::Ready(prior) => prior,
            _ => panic!("expected hook approval"),
        };
        let ready = match tools
            .advance_formatted_multi_approval_inner(
                &prior.candidate,
                &call,
                Some(&after),
                &cancel,
                Some(&prior),
            )
            .unwrap()
        {
            FormattedProgress::Ready(ready) => ready,
            _ => panic!("expected formatted approval"),
        };
        assert_eq!(
            ready.record.entries()[0].after.as_ref().unwrap().bytes,
            b"B1"
        );
        assert!(ready.grant.matches(&ready.record, &call));
        assert_eq!(fs::read(root.0.join("hook-count")).unwrap(), b"x");
        assert_eq!(fs::read(root.0.join("format-count")).unwrap(), b"x");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b0");
    }

    #[test]
    fn formatted_multi_patch_approves_raw_then_saved_final_images_without_reexecution() {
        let root = Temp::new();
        let mut tools = tools(&root);
        tools.permissions = PermissionLattice::new(PermissionMode::Default);
        let sink = Arc::new(ApprovalTestSink::default());
        tools.approval_sink = Some(sink.clone());
        let (steps, problems) = crate::write_steps::WriteSteps::parse(&serde_json::json!({
            "formatters":[{"command":["sh","-c","printf x >> format-count; tr a-z A-Z"],"globs":["*"]}]
        }));
        assert!(problems.is_empty());
        tools.set_write_steps(steps.unwrap());
        let input = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]},{"path":"b","edits":[{"old":"b0","new":"b1"}]}]}"#;
        let call = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("call", WORKSPACE_MULTI_PATCH_TOOL, input).unwrap(),
        );
        let cancel = CancellationToken::new();
        assert_eq!(
            tools.run_multi_patch_approved(&call, None, &cancel),
            Ok(ApprovedMultiPatchOutcome::Waiting)
        );
        assert!(
            sink.pending.lock().unwrap()[0]
                .summary
                .contains("formatter may run")
        );
        assert!(!root.0.join("format-count").exists());
        let first = sink.approve_next();
        assert_eq!(
            tools.run_multi_patch_approved(&call, Some(&first), &cancel),
            Ok(ApprovedMultiPatchOutcome::Waiting)
        );
        assert!(!root.0.join("format-count").exists());
        let second = sink.approve_next();
        assert_eq!(
            tools.run_multi_patch_approved(&call, Some(&second), &cancel),
            Ok(ApprovedMultiPatchOutcome::Waiting)
        );
        assert_eq!(fs::read(root.0.join("format-count")).unwrap(), b"xx");
        assert!(sink.pending.lock().unwrap()[0].diff.contains("A1"));
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        let third = sink.approve_next();
        assert_eq!(
            tools.run_multi_patch_approved(&call, Some(&third), &cancel),
            Ok(ApprovedMultiPatchOutcome::Waiting)
        );
        assert_eq!(fs::read(root.0.join("format-count")).unwrap(), b"xx");
        assert!(sink.pending.lock().unwrap()[0].diff.contains("B1"));
        let fourth = sink.approve_next();
        assert!(matches!(
            tools.run_multi_patch_approved(&call, Some(&fourth), &cancel),
            Ok(ApprovedMultiPatchOutcome::Published(
                PublishOutcome::Committed,
                _
            ))
        ));
        assert_eq!(fs::read(root.0.join("format-count")).unwrap(), b"xx");
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"A1");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"B1");
    }

    #[test]
    fn formatted_multi_patch_maps_final_approval_after_an_edit_becomes_a_noop() {
        let root = Temp::new();
        let mut tools = tools(&root);
        tools.permissions = PermissionLattice::new(PermissionMode::Default);
        let sink = Arc::new(ApprovalTestSink::default());
        tools.approval_sink = Some(sink.clone());
        let (steps, _) = crate::write_steps::WriteSteps::parse(&serde_json::json!({
            "formatters":[{"command":["sh","-c","sed 's/a1/a0/;s/b1/B1/'"],"globs":["*"]}]
        }));
        tools.set_write_steps(steps.unwrap());
        let input = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]},{"path":"b","edits":[{"old":"b0","new":"b1"}]}]}"#;
        let call = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("call", WORKSPACE_MULTI_PATCH_TOOL, input).unwrap(),
        );
        let cancel = CancellationToken::new();
        assert_eq!(
            tools.run_multi_patch_approved(&call, None, &cancel),
            Ok(ApprovedMultiPatchOutcome::Waiting)
        );
        let first = sink.approve_next();
        assert_eq!(
            tools.run_multi_patch_approved(&call, Some(&first), &cancel),
            Ok(ApprovedMultiPatchOutcome::Waiting)
        );
        let second = sink.approve_next();
        assert_eq!(
            tools.run_multi_patch_approved(&call, Some(&second), &cancel),
            Ok(ApprovedMultiPatchOutcome::Waiting)
        );
        assert_eq!(sink.pending.lock().unwrap()[0].scope, vec!["b"]);
        let final_answer = sink.approve_next();
        let outcome = tools.run_multi_patch_approved(&call, Some(&final_answer), &cancel);
        assert!(
            matches!(
                outcome,
                Ok(ApprovedMultiPatchOutcome::Published(
                    PublishOutcome::Committed,
                    _
                ))
            ),
            "{outcome:?}"
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"B1");
    }

    #[test]
    fn formatter_noop_checks_that_the_formatter_did_not_change_the_source_file() {
        let root = Temp::new();
        let mut tools = tools(&root);
        let (steps, _) = crate::write_steps::WriteSteps::parse(&serde_json::json!({
            "formatters":[{"command":["sh","-c","printf external > a; sed 's/1/0/'"],"globs":["*"]}]
        }));
        tools.set_write_steps(steps.unwrap());
        let input = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]}]}"#;
        let call = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("call", WORKSPACE_MULTI_PATCH_TOOL, input).unwrap(),
        );
        assert_eq!(
            tools.run_multi_patch_approved(&call, None, &CancellationToken::new()),
            Err(MultiApprovalError::Prepare(MultiPatchError::File {
                index: 0,
                error: WorkspaceRecoveryError::ChangedDuringRead
            }))
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"external");
    }

    #[test]
    fn changed_formatter_configuration_cannot_reuse_a_previous_approval() {
        let root = Temp::new();
        let mut tools = tools(&root);
        tools.permissions = PermissionLattice::new(PermissionMode::Default);
        let sink = Arc::new(ApprovalTestSink::default());
        tools.approval_sink = Some(sink.clone());
        let configure = |command: &str| {
            crate::write_steps::WriteSteps::parse(&serde_json::json!({
                "formatters":[{"command":["sh","-c",command],"globs":["*"]}]
            }))
            .0
            .unwrap()
        };
        tools.set_write_steps(configure("cat"));
        let input = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]}]}"#;
        let call = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("call", WORKSPACE_MULTI_PATCH_TOOL, input).unwrap(),
        );
        let cancel = CancellationToken::new();
        assert_eq!(
            tools.run_multi_patch_approved(&call, None, &cancel),
            Ok(ApprovedMultiPatchOutcome::Waiting)
        );
        let approved = sink.approve_next();
        tools.set_write_steps(configure("printf x > formatter-ran; cat"));
        assert_eq!(
            tools.run_multi_patch_approved(&call, Some(&approved), &cancel),
            Err(MultiApprovalError::InvalidApproval)
        );
        assert!(!root.0.join("formatter-ran").exists());
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
    }

    #[test]
    fn rewrite_event_failure_prevents_formatter_execution() {
        struct Events;
        impl HookEvents for Events {
            fn decided(&self, _: &str, _: &str, _: &crate::hooks::HookDecisionRecord) {}
            fn rewritten(&self, _: &str, _: &str, _: &HookRewriteRecord) {}
            fn rewritten_once(
                &self,
                _: &str,
                _: &str,
                _: &HookRewriteRecord,
                _: &str,
            ) -> Result<(), ()> {
                Err(())
            }
            fn failed(&self, _: &str, _: &str, _: &crate::hooks::HookFailure) {}
        }
        let root = Temp::new();
        let mut tools = tools(&root);
        tools.set_hook_events(Arc::new(Events));
        let script = root.0.join("event-failure.sh");
        fs::write(&script, "printf '%s\\n' '{\"decision\":\"allow\",\"updated_input\":{\"files\":[{\"path\":\"b\",\"edits\":[{\"old\":\"b0\",\"new\":\"b1\"}]}]}}'\n").unwrap();
        tools
            .hooks
            .pre_tool_use
            .push(crate::hooks::HookEntry::plain(format!(
                "sh {}",
                script.display()
            )));
        let (steps, _) = crate::write_steps::WriteSteps::parse(
            &serde_json::json!({"formatters":[{"command":["sh","-c","printf x > formatter-ran; cat"],"globs":["*"]}]}),
        );
        tools.set_write_steps(steps.unwrap());
        let input = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]}]}"#;
        let call = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("call", WORKSPACE_MULTI_PATCH_TOOL, input).unwrap(),
        );
        assert_eq!(
            tools.run_multi_patch_composed(&call, None, &CancellationToken::new(), true),
            Err(MultiApprovalError::Unavailable)
        );
        assert!(!root.0.join("formatter-ran").exists());
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b0");
        let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
        assert!(fs::read_dir(journal.directory()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("format-")
        }));
    }

    #[test]
    fn hook_rewrite_event_is_durable_before_formatting_and_not_duplicated_on_resume() {
        use crate::approvals::client_call;
        use event_ledger::event::{ActorKind, ActorRef};
        use kernel::KernelClient;
        let root = Temp::new();
        let path = root.0.join("rewrite-events.sqlite");
        let client = kernel::InProcessKernelClient::open(&path).unwrap();
        let actor = ActorRef::new(ActorKind::Human, &protocol::EventId::new().to_string()).unwrap();
        let session = client_call(client.create_session(kernel::CreateSession::new(
            protocol::ProjectId::new(),
            actor.clone(),
            protocol::TraceId::new(),
        )))
        .unwrap()
        .id();
        let mut tools = tools(&root);
        tools.permissions = PermissionLattice::new(PermissionMode::Default);
        let sink = Arc::new(ApprovalTestSink::default());
        tools.approval_sink = Some(sink.clone());
        tools.set_hook_events(crate::interactive::ledger_hook_events(
            &client, session, &actor,
        ));
        let script = root.0.join("rewrite-event.sh");
        fs::write(&script, "printf '%s\\n' '{\"decision\":\"ask\",\"reason\":\"review\",\"updated_input\":{\"files\":[{\"path\":\"b\",\"edits\":[{\"old\":\"b0\",\"new\":\"b1\"}]}]}}'\n").unwrap();
        tools
            .hooks
            .pre_tool_use
            .push(crate::hooks::HookEntry::plain(format!(
                "sh {}",
                script.display()
            )));
        let (steps, problems) = crate::write_steps::WriteSteps::parse(
            &serde_json::json!({"formatters":[{"command":["sh","-c","tr a-z A-Z"],"globs":["*"]}]}),
        );
        assert!(problems.is_empty());
        tools.set_write_steps(steps.unwrap());
        let input = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]}]}"#;
        let call = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("call", WORKSPACE_MULTI_PATCH_TOOL, input).unwrap(),
        );
        let cancel = CancellationToken::new();
        let mut latest = None;
        for _ in 0..4 {
            assert!(matches!(
                tools.run_multi_patch_composed(&call, latest.as_ref(), &cancel, true),
                Ok(ApprovedMultiPatchOutcome::Waiting)
            ));
            assert!(
                client
                    .recent_events_of_kinds(session, &["hook.input_rewritten"], 10)
                    .unwrap()
                    .is_empty()
            );
            latest = Some(sink.approve_next());
        }
        assert!(matches!(
            tools.run_multi_patch_composed(&call, latest.as_ref(), &cancel, true),
            Ok(ApprovedMultiPatchOutcome::Waiting)
        ));
        let event = client
            .recent_events_of_kinds(session, &["hook.input_rewritten"], 10)
            .unwrap();
        assert_eq!(event.len(), 1);
        assert_eq!(event[0].payload()["before"], input);
        let final_answer = sink.approve_next();
        drop(client);
        let reopened = kernel::InProcessKernelClient::open(&path).unwrap();
        tools.set_hook_events(crate::interactive::ledger_hook_events(
            &reopened, session, &actor,
        ));
        assert!(matches!(
            tools.run_multi_patch_composed(&call, Some(&final_answer), &cancel, true),
            Ok(ApprovedMultiPatchOutcome::Published(
                PublishOutcome::Committed,
                _
            ))
        ));
        assert_eq!(
            reopened
                .recent_events_of_kinds(session, &["hook.input_rewritten"], 10)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"B1");
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
    }

    #[test]
    fn saved_hook_decision_replays_once_into_reopened_session_ledger() {
        use super::super::multi_approval::PathApprovalChain;
        use super::super::multi_hook_checkpoint::HookCheckpoint;
        use crate::approvals::{ApprovalSink, client_call};
        use event_ledger::event::{ActorKind, ActorRef};
        use kernel::KernelClient;
        let root = Temp::new();
        let ledger_path = root.0.join("hook-events.sqlite");
        let client = kernel::InProcessKernelClient::open(&ledger_path).unwrap();
        let actor = ActorRef::new(ActorKind::Human, &protocol::EventId::new().to_string()).unwrap();
        let session = client_call(client.create_session(kernel::CreateSession::new(
            protocol::ProjectId::new(),
            actor.clone(),
            protocol::TraceId::new(),
        )))
        .unwrap()
        .id();
        let mut tools = tools(&root);
        let sink = Arc::new(ApprovalTestSink::default());
        tools.approval_sink = Some(sink.clone());
        let script = root.0.join("pre-replay.sh");
        fs::write(
            &script,
            format!(
                "printf x >> '{}'\nprintf '%s\\n' '{{\"decision\":\"allow\"}}'\n",
                root.0.join("hook-count").display()
            ),
        )
        .unwrap();
        tools
            .hooks
            .pre_tool_use
            .push(crate::hooks::HookEntry::plain(format!(
                "sh {}",
                script.display()
            )));
        let input = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]}]}"#;
        let call = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("call", WORKSPACE_MULTI_PATCH_TOOL, input).unwrap(),
        );
        let cancel = CancellationToken::new();
        let raw = tools.prepare_multi_patch(input, &cancel).unwrap();
        let chain = PathApprovalChain::new(&raw, "call", input).unwrap();
        let request = chain.request(&raw, 0, input).unwrap();
        sink.request(&request).unwrap();
        let answer = sink.approve_next();
        {
            let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
            let checkpoint =
                HookCheckpoint::new(&journal, &chain, &raw, input, &tools.hooks.pre_tool_use)
                    .unwrap();
            checkpoint.start().unwrap();
        }
        let report = crate::hooks::run_pre_tool_stage(
            &tools.hooks.pre_tool_use,
            WORKSPACE_MULTI_PATCH_TOOL,
            input,
            crate::hooks::HOOK_TIMEOUT,
        );
        {
            let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
            HookCheckpoint::new(&journal, &chain, &raw, input, &tools.hooks.pre_tool_use)
                .unwrap()
                .finish(&report)
                .unwrap();
        }
        assert!(
            client
                .recent_events_of_kinds(session, &["hook.decided"], 10)
                .unwrap()
                .is_empty()
        );
        tools.set_hook_events(crate::interactive::ledger_hook_events(
            &client, session, &actor,
        ));
        assert!(matches!(
            tools.advance_multi_hook_approval(&raw, &call, Some(&answer), &cancel),
            Ok(MultiHookProgress::Ready(_))
        ));
        assert!(matches!(
            tools.advance_multi_hook_approval(&raw, &call, Some(&answer), &cancel),
            Ok(MultiHookProgress::Ready(_))
        ));
        assert_eq!(
            client
                .recent_events_of_kinds(session, &["hook.decided"], 10)
                .unwrap()
                .len(),
            1
        );
        drop(client);
        let reopened = kernel::InProcessKernelClient::open(&ledger_path).unwrap();
        tools.set_hook_events(crate::interactive::ledger_hook_events(
            &reopened, session, &actor,
        ));
        assert!(matches!(
            tools.advance_multi_hook_approval(&raw, &call, Some(&answer), &cancel),
            Ok(MultiHookProgress::Ready(_))
        ));
        let events = reopened
            .recent_events_of_kinds(session, &["hook.decided"], 10)
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload()["decision"], "allow");
        assert_eq!(fs::read(root.0.join("hook-count")).unwrap(), b"x");
    }

    #[test]
    fn reopened_session_retires_exact_denied_hook_preview() {
        use super::super::multi_approval::PathApprovalChain;
        use super::super::multi_hook_checkpoint::{HookCheckpoint, State};
        use crate::approvals::{
            ApprovalSink, LedgerApprovalSink, SuspendedCall, SuspendedExchange, SuspendedHistory,
            SuspendedResult, SuspendedTurn, client_call,
        };
        use event_ledger::event::{ActorKind, ActorRef};
        use kernel::{ApprovalDecision, KernelClient};
        let root = Temp::new();
        let ledger_path = root.0.join("hook-cleanup.sqlite");
        let client = kernel::InProcessKernelClient::open(&ledger_path).unwrap();
        let actor = ActorRef::new(ActorKind::Human, &protocol::EventId::new().to_string()).unwrap();
        let session = client_call(client.create_session(kernel::CreateSession::new(
            protocol::ProjectId::new(),
            actor.clone(),
            protocol::TraceId::new(),
        )))
        .unwrap()
        .id();
        let tools = tools(&root);
        let args = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]}]}"#;
        let raw = tools
            .prepare_multi_patch(args, &CancellationToken::new())
            .unwrap();
        let chain = PathApprovalChain::new(&raw, "call", args).unwrap();
        let hooks = vec![crate::hooks::HookEntry::plain(
            "printf '%s\\n' '{\"decision\":\"ask\",\"reason\":\"review\"}'",
        )];
        let report = crate::hooks::run_pre_tool_stage(
            &hooks,
            WORKSPACE_MULTI_PATCH_TOOL,
            args,
            crate::hooks::HOOK_TIMEOUT,
        );
        let request = chain.request(&raw, 0, args).unwrap();
        {
            let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
            let checkpoint = HookCheckpoint::new(&journal, &chain, &raw, args, &hooks).unwrap();
            checkpoint.start().unwrap();
            checkpoint.finish(&report).unwrap();
            checkpoint.bind_pending(&report, &request).unwrap();
        }
        let sink = LedgerApprovalSink::new(client.clone(), session, actor.clone(), root.0.clone());
        let token = sink.request(&request).unwrap();
        let suspension = SuspendedTurn {
            task: "edit".into(),
            call_id: "call".into(),
            reason: "approval_required".into(),
            omitted_earlier_steps: 0,
            stop_continuations: 0,
            form_progress: None,
            video_binding: None,
            history: SuspendedHistory {
                exchanges: vec![SuspendedExchange {
                    calls: vec![SuspendedCall {
                        call_id: "call".into(),
                        tool: WORKSPACE_MULTI_PATCH_TOOL.into(),
                        arguments: args.into(),
                    }],
                    results: vec![SuspendedResult {
                        kind: "approval_required".into(),
                        call_id: "call".into(),
                        summary: String::new(),
                        detail: None,
                        handled: false,
                        question: String::new(),
                    }],
                    provider_replay: None,
                }],
            },
        };
        crate::approvals::record_suspension(
            &client,
            session,
            &actor,
            &token,
            "call",
            WORKSPACE_MULTI_PATCH_TOOL,
            &suspension,
        )
        .unwrap();
        assert_eq!(
            crate::interactive::reconcile_denied_formatter_checkpoints(&client, session, &root.0),
            Ok(0)
        );
        let seq = client_call(client.get_session(session)).unwrap().seq();
        client_call(
            client.approve(
                kernel::ResolveApproval::new(
                    session,
                    seq,
                    ApprovalDecision::Denied,
                    actor,
                    protocol::TraceId::new(),
                )
                .with_wait_token(&token),
            ),
        )
        .unwrap();
        drop(sink);
        drop(client);
        let reopened = kernel::InProcessKernelClient::open(&ledger_path).unwrap();
        assert_eq!(
            crate::interactive::reconcile_denied_formatter_checkpoints(&reopened, session, &root.0),
            Ok(1)
        );
        assert_eq!(
            crate::interactive::reconcile_denied_formatter_checkpoints(&reopened, session, &root.0),
            Ok(0)
        );
        let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
        assert_eq!(
            HookCheckpoint::new(&journal, &chain, &raw, args, &hooks)
                .unwrap()
                .load(),
            Ok(State::Retired)
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
    }

    #[test]
    fn reopened_session_cleans_a_durable_denial_but_not_a_pending_preview() {
        use super::super::multi_approval::{ApprovalPhase, PathApprovalChain};
        use super::super::multi_format_checkpoint::{CheckpointState, FormatCheckpoint};
        use crate::approvals::{
            ApprovalSink, LedgerApprovalSink, SuspendedCall, SuspendedExchange, SuspendedHistory,
            SuspendedResult, SuspendedTurn, client_call,
        };
        use event_ledger::event::{ActorKind, ActorRef};
        use kernel::{ApprovalDecision, KernelClient};
        let root = Temp::new();
        let ledger_path = root.0.join("cleanup.sqlite");
        let client = kernel::InProcessKernelClient::open(&ledger_path).unwrap();
        let actor = ActorRef::new(ActorKind::Human, &protocol::EventId::new().to_string()).unwrap();
        let session = client_call(client.create_session(kernel::CreateSession::new(
            protocol::ProjectId::new(),
            actor.clone(),
            protocol::TraceId::new(),
        )))
        .unwrap()
        .id();
        let raw = record();
        let args = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]},{"path":"b","edits":[{"old":"b0","new":"b1"}]}]}"#;
        let binding = "b".repeat(64);
        let chain = PathApprovalChain::new_before_format(&raw, "call", args, &binding).unwrap();
        let key = chain.formatter_checkpoint_key().unwrap();
        let mut entries = raw.entries().to_vec();
        entries[0].after = image("A1");
        entries[1].after = image("B1");
        let formatted = FormattedMultiCandidate {
            record: Some(RecoveryRecord::new(RecoveryState::Intent, entries).unwrap()),
            original_indices: vec![0, 1],
            notes: String::new(),
        };
        {
            let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
            let checkpoint = FormatCheckpoint::new(&journal, &key).unwrap();
            checkpoint.start().unwrap();
            checkpoint.finish(&raw, &formatted, &binding).unwrap();
        }
        let final_chain = chain
            .for_phase(
                formatted.record.as_ref().unwrap(),
                args,
                ApprovalPhase::AfterFormat,
            )
            .unwrap();
        let request = final_chain
            .request(formatted.record.as_ref().unwrap(), 0, args)
            .unwrap();
        let sink = LedgerApprovalSink::new(client.clone(), session, actor.clone(), root.0.clone());
        let token = sink.request(&request).unwrap();
        let suspension = SuspendedTurn {
            task: "edit".into(),
            call_id: "call".into(),
            reason: "approval_required".into(),
            omitted_earlier_steps: 0,
            stop_continuations: 0,
            form_progress: None,
            video_binding: None,
            history: SuspendedHistory {
                exchanges: vec![SuspendedExchange {
                    calls: vec![SuspendedCall {
                        call_id: "call".into(),
                        tool: WORKSPACE_MULTI_PATCH_TOOL.into(),
                        arguments: args.into(),
                    }],
                    results: vec![SuspendedResult {
                        kind: "approval_required".into(),
                        call_id: "call".into(),
                        summary: String::new(),
                        detail: None,
                        handled: false,
                        question: String::new(),
                    }],
                    provider_replay: None,
                }],
            },
        };
        crate::approvals::record_suspension(
            &client,
            session,
            &actor,
            &token,
            "call",
            WORKSPACE_MULTI_PATCH_TOOL,
            &suspension,
        )
        .unwrap();
        assert_eq!(
            crate::interactive::reconcile_denied_formatter_checkpoints(&client, session, &root.0),
            Ok(0)
        );
        let seq = client_call(client.get_session(session)).unwrap().seq();
        client_call(
            client.approve(
                kernel::ResolveApproval::new(
                    session,
                    seq,
                    ApprovalDecision::Denied,
                    actor.clone(),
                    protocol::TraceId::new(),
                )
                .with_wait_token(&token),
            ),
        )
        .unwrap();
        drop(sink);
        drop(client);
        let reopened = kernel::InProcessKernelClient::open(&ledger_path).unwrap();
        assert_eq!(
            crate::interactive::reconcile_denied_formatter_checkpoints(&reopened, session, &root.0),
            Ok(1)
        );
        assert_eq!(
            crate::interactive::reconcile_denied_formatter_checkpoints(&reopened, session, &root.0),
            Ok(0)
        );
        let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
        assert_eq!(
            FormatCheckpoint::new(&journal, &key).unwrap().load(&raw),
            Ok(CheckpointState::Retired)
        );
        drop(journal);
        let second_chain =
            PathApprovalChain::new_before_format(&raw, "call", args, &binding).unwrap();
        let second_key = second_chain.formatter_checkpoint_key().unwrap();
        {
            let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
            let checkpoint = FormatCheckpoint::new(&journal, &second_key).unwrap();
            checkpoint.start().unwrap();
            checkpoint.finish(&raw, &formatted, &binding).unwrap();
        }
        let after = second_chain
            .for_phase(
                formatted.record.as_ref().unwrap(),
                args,
                ApprovalPhase::AfterFormat,
            )
            .unwrap();
        let request = after
            .request(formatted.record.as_ref().unwrap(), 0, args)
            .unwrap();
        let sink =
            LedgerApprovalSink::new(reopened.clone(), session, actor.clone(), root.0.clone());
        let token = sink.request(&request).unwrap();
        crate::approvals::record_suspension(
            &reopened,
            session,
            &actor,
            &token,
            "call",
            WORKSPACE_MULTI_PATCH_TOOL,
            &suspension,
        )
        .unwrap();
        let seq = client_call(reopened.get_session(session)).unwrap().seq();
        client_call(
            reopened.approve(
                kernel::ResolveApproval::new(
                    session,
                    seq,
                    ApprovalDecision::Approved,
                    actor,
                    protocol::TraceId::new(),
                )
                .with_wait_token(&token),
            ),
        )
        .unwrap();
        assert_eq!(
            crate::interactive::reconcile_denied_formatter_checkpoints(&reopened, session, &root.0),
            Ok(0)
        );
        let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
        assert_eq!(
            FormatCheckpoint::new(&journal, &second_key)
                .unwrap()
                .load(&raw),
            Ok(CheckpointState::Ready(formatted))
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
    }

    #[test]
    fn formatter_candidate_changes_only_in_memory_and_skips_deleted_files() {
        let root = Temp::new();
        let mut tools = tools(&root);
        let (steps, problems) = crate::write_steps::WriteSteps::parse(&serde_json::json!({
            "formatters":[{"command":["sh","-c","tr a-z A-Z"],"globs":["*"]}]
        }));
        assert!(problems.is_empty());
        tools.set_write_steps(steps.unwrap());
        let mut entries = record().entries().to_vec();
        entries[1].after = None;
        let input = RecoveryRecord::new(RecoveryState::Intent, entries).unwrap();
        let result = tools
            .format_multi_candidate(&input, &CancellationToken::new())
            .unwrap();
        let formatted = result.record.unwrap();
        assert_eq!(formatted.entries()[0].after.as_ref().unwrap().bytes, b"A1");
        assert!(formatted.entries()[1].after.is_none());
        assert_eq!(result.original_indices, vec![0, 1]);
        assert!(result.notes.contains("formatted by"));
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b0");
    }

    #[test]
    fn formatter_candidate_reports_failure_and_honors_precancellation() {
        let root = Temp::new();
        let mut tools = tools(&root);
        let (steps, problems) = crate::write_steps::WriteSteps::parse(&serde_json::json!({
            "formatters":[{"command":["sh","-c","exit 1"],"globs":["*"]}]
        }));
        assert!(problems.is_empty());
        tools.set_write_steps(steps.unwrap());
        let original = record();
        let result = tools
            .format_multi_candidate(&original, &CancellationToken::new())
            .unwrap();
        assert_eq!(result.record, Some(original.clone()));
        assert!(result.notes.contains("did not run cleanly"));
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            tools.format_multi_candidate(&original, &cancel),
            Err(MultiPatchError::Cancelled)
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
    }

    #[test]
    fn formatter_candidate_refuses_oversized_output_without_modifying_files() {
        let root = Temp::new();
        let mut tools = tools(&root);
        let (steps, problems) = crate::write_steps::WriteSteps::parse(&serde_json::json!({
            "formatters":[{"command":["sh","-c","yes x | head -c 8388609"],"globs":["*"]}]
        }));
        assert!(problems.is_empty());
        tools.set_write_steps(steps.unwrap());
        assert_eq!(
            tools.format_multi_candidate(&record(), &CancellationToken::new()),
            Err(MultiPatchError::File {
                index: 0,
                error: WorkspaceRecoveryError::LimitExceeded
            })
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b0");
    }

    #[test]
    fn formatter_candidate_tracks_original_indices_when_edits_become_noops() {
        let root = Temp::new();
        let mut tools = tools(&root);
        let configure = |script: &str| {
            let (steps, problems) = crate::write_steps::WriteSteps::parse(&serde_json::json!({
                "formatters":[{"command":["sh","-c",script],"globs":["*"]}]
            }));
            assert!(problems.is_empty());
            steps.unwrap()
        };
        tools.set_write_steps(configure("sed 's/a1/a0/'"));
        let result = tools
            .format_multi_candidate(&record(), &CancellationToken::new())
            .unwrap();
        assert_eq!(result.original_indices, vec![1]);
        assert_eq!(result.record.unwrap().entries()[0].path, "b");
        tools.set_write_steps(configure("sed 's/1/0/'"));
        let result = tools
            .format_multi_candidate(&record(), &CancellationToken::new())
            .unwrap();
        assert!(result.original_indices.is_empty());
        assert!(result.record.is_none());
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b0");
    }

    #[test]
    fn formatter_candidate_enforces_aggregate_limit_during_construction() {
        let root = Temp::new();
        let mut tools = tools(&root);
        let (steps, problems) = crate::write_steps::WriteSteps::parse(&serde_json::json!({
            "formatters":[{"command":["sh","-c","yes x | head -c 6291456"],"globs":["*"]}]
        }));
        assert!(problems.is_empty());
        tools.set_write_steps(steps.unwrap());
        let mut entries = record().entries().to_vec();
        entries.push(RecoveryEntry {
            path: "c".into(),
            before: None,
            after: image("c1"),
        });
        let input = RecoveryRecord::new(RecoveryState::Intent, entries).unwrap();
        assert_eq!(
            tools.format_multi_candidate(&input, &CancellationToken::new()),
            Err(MultiPatchError::File {
                index: 2,
                error: WorkspaceRecoveryError::LimitExceeded
            })
        );
        assert!(!root.0.join("c").exists());
    }

    #[test]
    fn lsp_only_configuration_reports_each_committed_file() {
        let python = ["python3", "python"].into_iter().find(|name| {
            std::process::Command::new(name)
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        });
        let Some(python) = python else { return };
        let root = Temp::new();
        let script = root.0.join("fake-lsp.py");
        fs::write(&script, crate::lsp_diagnostics::FAKE_SERVER).unwrap();
        for (path, bytes) in [("a.txt", b"a0".as_slice()), ("b.txt", b"b0".as_slice())] {
            fs::write(root.0.join(path), bytes).unwrap();
        }
        let (steps, problems) = crate::write_steps::WriteSteps::parse(&serde_json::json!({
            "lsp":{"defaults":false,"wait_ms":5000,"servers":{"fake":{"command":[python,script.to_string_lossy()],"extensions":["txt"]}}}
        }));
        assert!(problems.is_empty(), "{problems:?}");
        let cache = crate::write_steps::LspCache::default();
        let mut tools = tools(&root);
        for (path, bytes) in [("a.txt", b"a0".as_slice()), ("b.txt", b"b0".as_slice())] {
            tools
                .read_authority
                .as_ref()
                .unwrap()
                .record_visible(tools.root(), path, bytes)
                .unwrap();
        }
        tools.set_write_steps(steps.unwrap().with_session(&root.0, &cache));
        assert!(
            tools
                .tool_surface()
                .iter()
                .any(|tool| tool.name() == WORKSPACE_MULTI_PATCH_TOOL)
        );
        let input = r#"{"files":[{"path":"a.txt","edits":[{"old":"a0","new":"ERROR"}]},{"path":"b.txt","edits":[{"old":"b0","new":"ERROR"}]}]}"#;
        let call = ValidatedToolCall::from_proposed(
            &ProposedToolCall::new("call", WORKSPACE_MULTI_PATCH_TOOL, input).unwrap(),
        );
        match tools.execute(&call, &CancellationToken::new()).unwrap() {
            ToolStepResult::Succeeded { summary, .. } => {
                assert!(
                    summary.contains("lsp (fake): 1 problem(s) in a.txt"),
                    "{summary}"
                );
                assert!(
                    summary.contains("lsp (fake): 1 problem(s) in b.txt"),
                    "{summary}"
                );
            }
            result => panic!("{result:?}"),
        }
        assert_eq!(fs::read(root.0.join("a.txt")).unwrap(), b"ERROR");
        assert_eq!(fs::read(root.0.join("b.txt")).unwrap(), b"ERROR");
    }

    #[test]
    fn batched_file_writes_follow_multi_patch_order_and_read_freshness() {
        let root = Temp::new();
        let tools = tools(&root);
        let multi = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]},{"path":"b","edits":[{"old":"b0","new":"b1"}]}]}"#;
        let calls = [
            ValidatedToolCall::from_proposed(
                &ProposedToolCall::new("multi", WORKSPACE_MULTI_PATCH_TOOL, multi).unwrap(),
            ),
            ValidatedToolCall::from_proposed(
                &ProposedToolCall::new(
                    "patch",
                    WORKSPACE_PATCH_TOOL,
                    r#"{"path":"b","old":"b1","new":"b2"}"#,
                )
                .unwrap(),
            ),
        ];
        let outcomes = batch_dispatch(&tools, &calls, &CancellationToken::new());
        assert!(matches!(&outcomes[0], Ok(ToolStepResult::Succeeded { .. })));
        assert!(matches!(&outcomes[1], Ok(ToolStepResult::Failed { .. })));
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a1");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b1");
    }

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "rapid-publish-real-{}-{}",
                std::process::id(),
                ATOMIC_WRITE_SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            fs::create_dir(root.join(".rapidlm")).unwrap();
            fs::create_dir(root.join(JOURNAL_PATH)).unwrap();
            fs::set_permissions(root.join(JOURNAL_PATH), fs::Permissions::from_mode(0o700))
                .unwrap();
            for (path, value) in [("a", "a0"), ("b", "b0")] {
                fs::write(root.join(path), value).unwrap();
                fs::set_permissions(root.join(path), fs::Permissions::from_mode(0o600)).unwrap();
            }
            Self(root)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn image(value: &str) -> Option<FileImage> {
        Some(FileImage {
            bytes: value.as_bytes().to_vec(),
            mode: Some(0o600),
        })
    }
    fn record() -> RecoveryRecord {
        RecoveryRecord::new(
            RecoveryState::Intent,
            vec![
                RecoveryEntry {
                    path: "a".into(),
                    before: image("a0"),
                    after: image("a1"),
                },
                RecoveryEntry {
                    path: "b".into(),
                    before: image("b0"),
                    after: image("b1"),
                },
            ],
        )
        .unwrap()
    }
    fn tools(root: &Temp) -> WorkspaceTools {
        let tools = WorkspaceTools::open_with_permissions(
            &root.0,
            PermissionLattice::new(PermissionMode::BypassPermissions),
        )
        .unwrap();
        for (path, value) in [("a", b"a0".as_slice()), ("b", b"b0".as_slice())] {
            tools
                .read_authority
                .as_ref()
                .unwrap()
                .record_visible(tools.root(), path, value)
                .unwrap();
        }
        tools
    }
    fn no_staging(root: &Temp) {
        assert!(fs::read_dir(&root.0).unwrap().all(|entry| {
            let name = entry.unwrap().file_name();
            let name = name.to_string_lossy();
            !name.starts_with(".rapidlm-multi-") && !name.starts_with(".rapidlm-stage-")
        }));
    }

    #[test]
    fn production_publish_crash_child() {
        let Some(root) = std::env::var_os("RAPID_TEST_REAL_PUBLISH_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let mut tools = WorkspaceTools::open_with_permissions(
            &root,
            PermissionLattice::new(PermissionMode::BypassPermissions),
        )
        .unwrap();
        for (path, bytes) in [("a", b"a0".as_slice()), ("b", b"b0".as_slice())] {
            tools
                .read_authority
                .as_ref()
                .unwrap()
                .record_visible(tools.root(), path, bytes)
                .unwrap();
        }
        let signal = root.join("child-ready");
        if std::env::var_os("RAPID_TEST_REAL_STAGE_ONLY").is_some() {
            let mut publisher = WorkspacePublisher {
                recovery: WorkspaceRecovery { tools: &tools },
                staged: BTreeMap::new(),
                identities: BTreeMap::new(),
                approved_paths: Default::default(),
                journal_owned: false,
            };
            let journal = JournalStore::lock(&root.join(JOURNAL_PATH)).unwrap();
            let mut intent = record()
                .with_staging_plan("0123456789abcdef0123456789abcdef".into())
                .unwrap();
            journal.write_intent(&intent).unwrap();
            publisher.prepare_journaled(&journal, &mut intent).unwrap();
            fs::write(&signal, b"staging complete").unwrap();
            std::thread::sleep(Duration::from_secs(60));
            panic!("test child did not stop after staging");
        }
        let cancel = CancellationToken::new();
        let crash_phase = std::env::var("RAPID_TEST_MULTI_CRASH_POINT").ok();
        if matches!(crash_phase.as_deref(), Some("rollback" | "cleanup")) {
            let shared = cancel.clone();
            tools.set_evidence_invalidator(Arc::new(move |_| {
                shared.cancel();
                Ok(0)
            }));
        } else if crash_phase.is_none()
            || matches!(
                crash_phase.as_deref(),
                Some("mixed_create" | "mixed_delete")
            )
        {
            let target = if crash_phase.as_deref() == Some("mixed_delete") {
                1
            } else {
                0
            };
            let count = AtomicU64::new(0);
            tools.set_evidence_invalidator(Arc::new(move |_| {
                if count.fetch_add(1, Ordering::SeqCst) == target {
                    fs::write(&signal, b"effect").unwrap();
                    std::thread::sleep(Duration::from_secs(60));
                }
                Ok(0)
            }));
        }
        let input = if matches!(
            crash_phase.as_deref(),
            Some("mixed_create" | "mixed_delete")
        ) {
            r#"{"files":[{"path":"c","create":"new"},{"path":"b","delete":true}]}"#
        } else {
            r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]},{"path":"b","edits":[{"old":"b0","new":"b1"}]}]}"#
        };
        let _ = tools.run_multi_patch(input, &cancel);
        panic!("test child did not stop at the first publication");
    }

    fn kill_real_publisher(root: &Temp, phase: &str) {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "exec_tools::publication::tests::production_publish_crash_child",
                "--nocapture",
            ])
            .env("RAPID_TEST_REAL_PUBLISH_ROOT", &root.0)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if phase == "staged" {
            command.env("RAPID_TEST_REAL_STAGE_ONLY", "1");
        } else if phase != "effect" {
            command.env("RAPID_TEST_MULTI_CRASH_POINT", phase);
        }
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !root.0.join("child-ready").exists() {
            if let Some(status) = child.try_wait().unwrap() {
                panic!("publisher exited before the effect: {status}");
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("publisher did not reach the effect before the deadline");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        child.kill().unwrap();
        assert!(!child.wait().unwrap().success());
        assert_eq!(
            fs::read(root.0.join("a")).unwrap(),
            if matches!(phase, "effect" | "committed") {
                b"a1"
            } else {
                b"a0"
            }
        );
        if phase == "mixed_delete" {
            assert!(!root.0.join("b").exists());
        } else {
            assert_eq!(
                fs::read(root.0.join("b")).unwrap(),
                if phase == "committed" { b"b1" } else { b"b0" }
            );
        }
        if matches!(phase, "mixed_create" | "mixed_delete") {
            assert_eq!(fs::read(root.0.join("c")).unwrap(), b"new");
        }
    }

    #[test]
    fn killed_real_publisher_is_rolled_back_by_reopened_workspace() {
        let root = Temp::new();
        kill_real_publisher(&root, "effect");
        let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
        assert_eq!(
            journal.load().unwrap().unwrap().state(),
            RecoveryState::Intent
        );
        drop(journal);
        let reopened = WorkspaceTools::open(&root.0).unwrap();
        reopened.recover_before_edit().unwrap();
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b0");
        let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
        assert_eq!(journal.load().unwrap(), None);
        no_staging(&root);
    }

    #[test]
    fn killed_create_or_delete_is_rolled_back_by_reopened_workspace() {
        for phase in ["mixed_create", "mixed_delete"] {
            let root = Temp::new();
            kill_real_publisher(&root, phase);
            let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
            assert_eq!(
                journal.load().unwrap().unwrap().state(),
                RecoveryState::Intent
            );
            drop(journal);
            WorkspaceTools::open(&root.0)
                .unwrap()
                .recover_before_edit()
                .unwrap();
            assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
            assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b0");
            assert!(!root.0.join("c").exists());
            assert_eq!(
                recovery::lock_journal_for_publication(&root.0)
                    .unwrap()
                    .load()
                    .unwrap(),
                None
            );
            no_staging(&root);
        }
    }

    #[test]
    fn reopened_workspace_preserves_external_change_after_publisher_crash() {
        let root = Temp::new();
        kill_real_publisher(&root, "effect");
        fs::write(root.0.join("a"), b"external").unwrap();
        let reopened = WorkspaceTools::open(&root.0).unwrap();
        assert_eq!(
            reopened.recover_before_edit(),
            Err(WorkspaceRecoveryError::Pending)
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"external");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b0");
        let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
        assert_eq!(
            journal.load().unwrap().unwrap().state(),
            RecoveryState::Intent
        );
    }

    #[test]
    fn killed_real_staging_after_intent_is_cleaned_on_restart() {
        let root = Temp::new();
        kill_real_publisher(&root, "staged");
        let reopened = WorkspaceTools::open(&root.0).unwrap();
        reopened.recover_before_edit().unwrap();
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b0");
        let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
        assert_eq!(journal.load().unwrap(), None);
        no_staging(&root);
    }

    #[test]
    fn killed_unregistered_stage_fails_closed_without_candidate_bytes() {
        let root = Temp::new();
        kill_real_publisher(&root, "created");
        let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
        let intent = journal.load().unwrap().unwrap();
        assert_eq!(intent.state(), RecoveryState::Intent);
        assert_eq!(intent.stage_identity(0), None);
        let stage = root.0.join(intent.staging_name(0).unwrap());
        assert_eq!(fs::read(&stage).unwrap(), b"");
        assert_eq!(
            fs::metadata(&stage).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(journal);
        let reopened = WorkspaceTools::open(&root.0).unwrap();
        assert_eq!(
            reopened.recover_before_edit(),
            Err(WorkspaceRecoveryError::UnsafeFile)
        );
        assert!(stage.exists());
        let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
        assert_eq!(journal.load().unwrap(), Some(intent));
    }

    #[test]
    fn killed_registered_and_partial_stages_are_removed_on_restart() {
        for phase in ["registered", "partial"] {
            let root = Temp::new();
            kill_real_publisher(&root, phase);
            let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
            let intent = journal.load().unwrap().unwrap();
            assert!(intent.stage_identity(0).is_some());
            let stage = root.0.join(intent.staging_name(0).unwrap());
            assert_eq!(
                fs::read(&stage).unwrap(),
                if phase == "partial" {
                    b"a".as_slice()
                } else {
                    b"".as_slice()
                }
            );
            drop(journal);
            WorkspaceTools::open(&root.0)
                .unwrap()
                .recover_before_edit()
                .unwrap();
            assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
            assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b0");
            assert!(!stage.exists());
            assert_eq!(
                recovery::lock_journal_for_publication(&root.0)
                    .unwrap()
                    .load()
                    .unwrap(),
                None
            );
            no_staging(&root);
        }
    }

    #[test]
    fn killed_committed_publisher_keeps_edits_on_restart() {
        let root = Temp::new();
        kill_real_publisher(&root, "committed");
        let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
        assert_eq!(
            journal.load().unwrap().unwrap().state(),
            RecoveryState::Committed
        );
        drop(journal);
        WorkspaceTools::open(&root.0)
            .unwrap()
            .recover_before_edit()
            .unwrap();
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a1");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b1");
        assert_eq!(
            recovery::lock_journal_for_publication(&root.0)
                .unwrap()
                .load()
                .unwrap(),
            None
        );
        no_staging(&root);
    }

    #[test]
    fn killed_rollback_is_finished_by_reopened_workspace() {
        for phase in ["rollback", "cleanup"] {
            let root = Temp::new();
            kill_real_publisher(&root, phase);
            let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
            assert_eq!(
                journal.load().unwrap().unwrap().state(),
                RecoveryState::Intent
            );
            drop(journal);
            WorkspaceTools::open(&root.0)
                .unwrap()
                .recover_before_edit()
                .unwrap();
            assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
            assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b0");
            assert_eq!(
                recovery::lock_journal_for_publication(&root.0)
                    .unwrap()
                    .load()
                    .unwrap(),
                None
            );
            no_staging(&root);
        }
    }

    #[test]
    fn runtime_orchestration_prepares_locks_and_publishes_with_the_live_token() {
        let root = Temp::new();
        let tools = tools(&root);
        let input = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]},{"path":"b","edits":[{"old":"b0","new":"b1"}]}]}"#;
        assert_eq!(
            tools
                .run_multi_patch(input, &CancellationToken::new())
                .unwrap(),
            PublishOutcome::Committed
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a1");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b1");
        let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
        assert_eq!(journal.load().unwrap(), None);
        no_staging(&root);
    }

    #[test]
    fn runtime_cancellation_after_first_publication_rolls_back_all_files() {
        let root = Temp::new();
        let mut tools = tools(&root);
        let cancel = CancellationToken::new();
        let shared = cancel.clone();
        tools.set_evidence_invalidator(Arc::new(move |_| {
            shared.cancel();
            Ok(0)
        }));
        let input = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]},{"path":"b","edits":[{"old":"b0","new":"b1"}]}]}"#;
        assert_eq!(
            tools.run_multi_patch(input, &cancel).unwrap(),
            PublishOutcome::RolledBack(PublishFailure::Cancelled)
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b0");
        let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
        assert_eq!(journal.load().unwrap(), None);
        no_staging(&root);
    }

    #[test]
    fn runtime_cancellation_before_start_creates_no_journal() {
        let root = Temp::new();
        fs::remove_dir_all(root.0.join(".rapidlm")).unwrap();
        let tools = tools(&root);
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            tools.run_multi_patch("{}", &cancel),
            Err(MultiPatchRunError::Prepare(MultiPatchError::Cancelled))
        );
        assert!(!root.0.join(".rapidlm").exists());
        no_staging(&root);
    }

    #[test]
    fn multi_patch_prepares_all_files_then_publishes_the_exact_candidate() {
        let root = Temp::new();
        let tools = tools(&root);
        let input = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"},{"old":"a1","new":"a2"}]},{"path":"b","edits":[{"old":"b0","new":"b1"}]}]}"#;
        let candidate = tools
            .prepare_multi_patch(input, &CancellationToken::new())
            .unwrap();
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        assert_eq!(candidate.entries()[0].after.as_ref().unwrap().bytes, b"a2");
        let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
        assert_eq!(
            tools
                .publish_multi_edit(&journal, &candidate, &workspace::CancellationToken::new())
                .unwrap(),
            PublishOutcome::Committed
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a2");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b1");
        assert_eq!(journal.load().unwrap(), None);
    }

    #[test]
    fn multi_patch_late_conflict_and_invalid_requests_never_modify_targets() {
        let root = Temp::new();
        let tools = tools(&root);
        let conflict = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]},{"path":"b","edits":[{"old":"missing","new":"b1"}]}]}"#;
        assert!(matches!(
            tools.prepare_multi_patch(conflict, &CancellationToken::new()),
            Err(MultiPatchError::Patch { index: 1, .. })
        ));
        for input in [
            r#"{"files":[]}"#,
            r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]},{"path":"a","edits":[{"old":"a0","new":"a2"}]}]}"#,
            r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]}],"extra":true}"#,
        ] {
            assert_eq!(
                tools.prepare_multi_patch(input, &CancellationToken::new()),
                Err(MultiPatchError::Invalid)
            );
        }
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            tools.prepare_multi_patch(conflict, &cancel),
            Err(MultiPatchError::Cancelled)
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b0");
        no_staging(&root);
    }

    #[test]
    fn multi_patch_requires_visible_fresh_bytes_for_every_file() {
        let root = Temp::new();
        let tools = tools(&root);
        fs::write(root.0.join("b"), "changed").unwrap();
        let input = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]},{"path":"b","edits":[{"old":"changed","new":"b1"}]}]}"#;
        assert!(matches!(
            tools.prepare_multi_patch(input, &CancellationToken::new()),
            Err(MultiPatchError::File {
                index: 1,
                error: WorkspaceRecoveryError::ReadRequired
            })
        ));
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"changed");
    }

    #[test]
    fn multi_patch_rechecks_preimages_after_candidate_preparation() {
        let root = Temp::new();
        let tools = tools(&root);
        let input = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]},{"path":"b","edits":[{"old":"b0","new":"b1"}]}]}"#;
        let candidate = tools
            .prepare_multi_patch(input, &CancellationToken::new())
            .unwrap();
        fs::write(root.0.join("b"), "external").unwrap();
        let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
        assert_eq!(
            tools.publish_multi_edit(&journal, &candidate, &workspace::CancellationToken::new()),
            Err(WorkspaceRecoveryError::ReadRequired)
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"external");
        assert_eq!(journal.load().unwrap(), None);
        no_staging(&root);
    }
    #[test]
    fn staged_workspace_publication_handles_create_and_delete() {
        let root = Temp::new();
        let tools = tools(&root);
        let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
        let record = RecoveryRecord::new(
            RecoveryState::Intent,
            vec![
                RecoveryEntry {
                    path: "a".into(),
                    before: image("a0"),
                    after: image("a1"),
                },
                RecoveryEntry {
                    path: "c".into(),
                    before: None,
                    after: image("c1"),
                },
                RecoveryEntry {
                    path: "b".into(),
                    before: image("b0"),
                    after: None,
                },
            ],
        )
        .unwrap();
        assert_eq!(
            tools
                .publish_multi_edit(&journal, &record, &workspace::CancellationToken::new())
                .unwrap(),
            PublishOutcome::Committed
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a1");
        assert_eq!(fs::read(root.0.join("c")).unwrap(), b"c1");
        assert_eq!(
            fs::metadata(root.0.join("c")).unwrap().permissions().mode() & 0o7777,
            0o600
        );
        assert!(!root.0.join("b").exists());
        assert_eq!(journal.load().unwrap(), None);
        no_staging(&root);
    }

    #[test]
    fn staged_workspace_publication_commits_both_files_and_invalidates_reads() {
        let root = Temp::new();
        let tools = tools(&root);
        let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
        assert_eq!(
            tools
                .publish_multi_edit(&journal, &record(), &workspace::CancellationToken::new())
                .unwrap(),
            PublishOutcome::Committed
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a1");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b1");
        assert_eq!(
            tools
                .read_authority
                .as_ref()
                .unwrap()
                .check(tools.root(), "a", b"a0")
                .unwrap(),
            crate::read_freshness::ReadVerdict::Unread
        );
        assert_eq!(journal.load().unwrap(), None);
        no_staging(&root);
    }
    #[test]
    fn staged_workspace_publication_requires_a_fresh_read_for_each_file() {
        let root = Temp::new();
        let tools = WorkspaceTools::open_with_permissions(
            &root.0,
            PermissionLattice::new(PermissionMode::BypassPermissions),
        )
        .unwrap();
        tools
            .read_authority
            .as_ref()
            .unwrap()
            .record_visible(tools.root(), "a", b"a0")
            .unwrap();
        let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
        assert_eq!(
            tools.publish_multi_edit(&journal, &record(), &workspace::CancellationToken::new()),
            Err(WorkspaceRecoveryError::ReadRequired)
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b0");
        assert_eq!(journal.load().unwrap(), None);
        no_staging(&root);
    }
    #[test]
    fn failed_invalidation_rolls_back_the_first_published_file() {
        let root = Temp::new();
        let mut tools = tools(&root);
        let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
        let count = Arc::new(AtomicU64::new(0));
        let shared = Arc::clone(&count);
        tools.set_evidence_invalidator(Arc::new(move |_| {
            if shared.fetch_add(1, Ordering::SeqCst) == 0 {
                Err("fail".into())
            } else {
                Ok(0)
            }
        }));
        assert_eq!(
            tools
                .publish_multi_edit(&journal, &record(), &workspace::CancellationToken::new())
                .unwrap(),
            PublishOutcome::RolledBack(PublishFailure::Io(WorkspaceRecoveryError::Invalidation))
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b0");
        assert_eq!(journal.load().unwrap(), None);
        no_staging(&root);
    }
    #[test]
    fn cancellation_after_first_publish_restores_the_original_files() {
        let root = Temp::new();
        let mut tools = tools(&root);
        let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
        let cancel = workspace::CancellationToken::new();
        let shared = cancel.clone();
        let count = Arc::new(AtomicU64::new(0));
        let calls = Arc::clone(&count);
        tools.set_evidence_invalidator(Arc::new(move |_| {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                shared.cancel();
            }
            Ok(0)
        }));
        assert_eq!(
            tools
                .publish_multi_edit(&journal, &record(), &cancel)
                .unwrap(),
            PublishOutcome::RolledBack(PublishFailure::Cancelled)
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b0");
        assert_eq!(journal.load().unwrap(), None);
        no_staging(&root);
    }

    #[test]
    fn each_path_must_pass_both_edit_policies_before_staging() {
        use crate::permissions::{RuleEffect, ToolPattern, ToolRule};
        for pattern in ["workspace_write(b)", "workspace_patch(b)"] {
            let root = Temp::new();
            let lattice =
                PermissionLattice::new(PermissionMode::BypassPermissions).with_rules(vec![
                    ToolRule {
                        effect: RuleEffect::Deny,
                        pattern: ToolPattern::parse(pattern).unwrap(),
                    },
                ]);
            let tools = WorkspaceTools::open_with_permissions(&root.0, lattice).unwrap();
            let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
            assert_eq!(
                tools.publish_multi_edit(&journal, &record(), &workspace::CancellationToken::new()),
                Err(WorkspaceRecoveryError::PermissionDenied)
            );
            assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
            assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b0");
            assert_eq!(journal.load().unwrap(), None);
            no_staging(&root);
        }
    }

    #[test]
    fn a_patch_deny_wins_over_a_write_approval_request() {
        use crate::permissions::{RuleEffect, ToolPattern, ToolRule};
        let root = Temp::new();
        let lattice = PermissionLattice::new(PermissionMode::Default).with_rules(vec![ToolRule {
            effect: RuleEffect::Deny,
            pattern: ToolPattern::parse("workspace_patch(a)").unwrap(),
        }]);
        let tools = WorkspaceTools::open_with_permissions(&root.0, lattice).unwrap();
        let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
        assert_eq!(
            tools.publish_multi_edit(&journal, &record(), &workspace::CancellationToken::new()),
            Err(WorkspaceRecoveryError::PermissionDenied)
        );
        assert_eq!(journal.load().unwrap(), None);
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        no_staging(&root);
    }

    #[test]
    fn a_later_path_deny_wins_over_an_earlier_path_ask() {
        use crate::permissions::{RuleEffect, ToolPattern, ToolRule};
        let root = Temp::new();
        let lattice = PermissionLattice::new(PermissionMode::Default).with_rules(vec![ToolRule {
            effect: RuleEffect::Deny,
            pattern: ToolPattern::parse("workspace_patch(b)").unwrap(),
        }]);
        let tools = WorkspaceTools::open_with_permissions(&root.0, lattice).unwrap();
        let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
        assert_eq!(
            tools.publish_multi_edit(&journal, &record(), &workspace::CancellationToken::new()),
            Err(WorkspaceRecoveryError::PermissionDenied)
        );
        assert_eq!(journal.load().unwrap(), None);
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b0");
        no_staging(&root);
    }

    #[test]
    fn pending_approval_hooks_and_write_budget_fail_closed() {
        let root = Temp::new();
        let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
        let default = WorkspaceTools::open(&root.0).unwrap();
        assert_eq!(
            default.publish_multi_edit(&journal, &record(), &workspace::CancellationToken::new()),
            Err(WorkspaceRecoveryError::ApprovalRequired)
        );
        let mut hooked = tools(&root);
        hooked
            .hooks
            .pre_tool_use
            .push(crate::hooks::HookEntry::plain("ignored"));
        assert_eq!(
            hooked.publish_multi_edit(&journal, &record(), &workspace::CancellationToken::new()),
            Err(WorkspaceRecoveryError::HooksRequireIntegration)
        );
        let mut limited = tools(&root);
        limited.max_write_bytes = 3;
        assert_eq!(
            limited.publish_multi_edit(&journal, &record(), &workspace::CancellationToken::new()),
            Err(WorkspaceRecoveryError::LimitExceeded)
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b0");
        assert_eq!(journal.load().unwrap(), None);
        no_staging(&root);
    }

    #[test]
    fn plan_mode_entered_after_first_write_rolls_back_the_transaction() {
        let root = Temp::new();
        let mut tools = tools(&root);
        let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
        let mode = Arc::clone(&tools.plan_mode);
        tools.set_evidence_invalidator(Arc::new(move |_| {
            mode.store(true, Ordering::SeqCst);
            Ok(0)
        }));
        assert_eq!(
            tools
                .publish_multi_edit(&journal, &record(), &workspace::CancellationToken::new())
                .unwrap(),
            PublishOutcome::RolledBack(PublishFailure::Io(
                WorkspaceRecoveryError::PermissionDenied
            ))
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b0");
        assert_eq!(journal.load().unwrap(), None);
        no_staging(&root);
    }

    #[test]
    fn plan_mode_entered_after_last_write_is_rechecked_before_commit() {
        let root = Temp::new();
        let mut tools = tools(&root);
        let journal = recovery::lock_journal_for_publication(&root.0).unwrap();
        let mode = Arc::clone(&tools.plan_mode);
        let count = Arc::new(AtomicU64::new(0));
        tools.set_evidence_invalidator(Arc::new(move |_| {
            if count.fetch_add(1, Ordering::SeqCst) == 1 {
                mode.store(true, Ordering::SeqCst);
            }
            Ok(0)
        }));
        assert_eq!(
            tools
                .publish_multi_edit(&journal, &record(), &workspace::CancellationToken::new())
                .unwrap(),
            PublishOutcome::RolledBack(PublishFailure::Io(
                WorkspaceRecoveryError::PermissionDenied
            ))
        );
        assert_eq!(fs::read(root.0.join("a")).unwrap(), b"a0");
        assert_eq!(fs::read(root.0.join("b")).unwrap(), b"b0");
        assert_eq!(journal.load().unwrap(), None);
        no_staging(&root);
    }
}
