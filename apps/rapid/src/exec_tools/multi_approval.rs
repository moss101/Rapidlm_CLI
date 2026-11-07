//! Candidate-bound, per-path approval identities for multi-file patches.
use crate::approvals::{ApprovalRequest, ApprovalSink, ApprovedAsk, arguments_digest};
use crate::line_diff::{self, DiffOutcome};
use sha2::{Digest, Sha256};
use workspace::multi_edit_recovery::{RecoveryRecord, RecoveryState};

const TOOL: &str = super::WORKSPACE_MULTI_PATCH_TOOL;

/// Formatter execution and publication are separately authorized. Keeping the
/// phase in the candidate fingerprint prevents one decision from being reused
/// for the other, even when the formatter leaves the bytes unchanged.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)] // The formatter phases are not exposed until orchestration is wired.
pub(super) enum ApprovalPhase {
    Plain,
    AfterHook,
    BeforeFormat,
    AfterFormat,
}

/// A nonce identifies one approval chain, not a standing grant. The
/// fingerprint also binds call id, raw arguments, every path, original and
/// resulting bytes, modes, and ordering. A fresh call must mint a fresh nonce.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub(super) struct PathApprovalChain {
    nonce: String,
    fingerprint: String,
    arguments_digest: String,
    call_id: String,
    entries: usize,
    phase: ApprovalPhase,
    formatter_binding: String,
}

#[allow(dead_code)]
impl PathApprovalChain {
    pub(super) fn new(record: &RecoveryRecord, call_id: &str, arguments: &str) -> Result<Self, ()> {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|_| ())?;
        let nonce = random.iter().map(|byte| format!("{byte:02x}")).collect();
        Self::from_nonce(record, call_id, arguments, nonce, ApprovalPhase::Plain, "")
    }

    pub(super) fn new_before_format(
        record: &RecoveryRecord,
        call_id: &str,
        arguments: &str,
        formatter_binding: &str,
    ) -> Result<Self, ()> {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|_| ())?;
        let nonce = random.iter().map(|byte| format!("{byte:02x}")).collect();
        Self::from_nonce(
            record,
            call_id,
            arguments,
            nonce,
            ApprovalPhase::BeforeFormat,
            formatter_binding,
        )
    }

    /// Derive another phase from the same chain. Its separate fingerprint and
    /// displayed request must still be verified against the durable ledger.
    pub(super) fn for_phase(
        &self,
        record: &RecoveryRecord,
        arguments: &str,
        phase: ApprovalPhase,
    ) -> Result<Self, ()> {
        Self::from_nonce(
            record,
            &self.call_id,
            arguments,
            self.nonce.clone(),
            phase,
            &self.formatter_binding,
        )
    }

    /// Bind rewritten path approvals to the persisted hook result while
    /// retaining the original call's argument digest for durable resumption.
    pub(super) fn for_hook_rewrite(
        &self,
        record: &RecoveryRecord,
        original_arguments: &str,
        hook_binding: &str,
    ) -> Result<Self, ()> {
        Self::from_nonce(
            record,
            &self.call_id,
            original_arguments,
            self.nonce.clone(),
            ApprovalPhase::AfterHook,
            hook_binding,
        )
    }

    /// Continue the same chain with a separately bound formatter phase.
    pub(super) fn for_formatter(
        &self,
        record: &RecoveryRecord,
        original_arguments: &str,
        formatter_binding: &str,
    ) -> Result<Self, ()> {
        if self.arguments_digest != arguments_digest(original_arguments) {
            return Err(());
        }
        Self::from_nonce(
            record,
            &self.call_id,
            original_arguments,
            self.nonce.clone(),
            ApprovalPhase::BeforeFormat,
            formatter_binding,
        )
    }

    /// Opaque workspace-local checkpoint identity, bound to the entire raw
    /// candidate, call, formatter configuration, phase and random chain.
    pub(super) fn formatter_checkpoint_key(&self) -> Result<String, ()> {
        if self.phase != ApprovalPhase::BeforeFormat {
            return Err(());
        }
        Ok(format!("{}{}", self.nonce, self.fingerprint))
    }

    /// Bind a hook checkpoint to the original approval chain and its complete
    /// ordered hook configuration. The nonce remains the replay identity.
    pub(super) fn hook_checkpoint_key(
        &self,
        original: &RecoveryRecord,
        arguments: &str,
        hooks: &[crate::hooks::HookEntry],
    ) -> Result<String, ()> {
        let base = self.request(original, 0, arguments)?;
        let config = hook_configuration_bytes(hooks)?;
        let mut hash = Sha256::new();
        hash.update(b"rapidlm.multi-edit.hook-checkpoint.v1\0");
        for part in [
            base.source.as_deref().ok_or(())?.as_bytes(),
            config.as_slice(),
        ] {
            hash.update((part.len() as u64).to_be_bytes());
            hash.update(part);
        }
        let fingerprint: String = hash
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Ok(format!("{}{fingerprint}", self.nonce))
    }

    pub(super) fn matches_chain_request(&self, request: &ApprovalRequest) -> bool {
        let Some(source) = request.source.as_deref() else {
            return false;
        };
        let parts: Vec<_> = source.split(':').collect();
        parts.len() == 5
            && parts[0] == "multi-edit"
            && parts[1] == "v1"
            && parts[2] == self.nonce
            && parts[3].len() == 64
            && parts[3].bytes().all(is_lower_hex)
            && parts[4].parse::<usize>().is_ok_and(|index| {
                index.to_string() == parts[4]
                    && index < workspace::multi_edit_recovery::MAX_RECOVERY_FILES
            })
            && request.tool == TOOL
            && request.call_id == self.call_id
            && request.arguments_digest.as_deref() == Some(self.arguments_digest.as_str())
            && request.remember_as.is_none()
    }

    fn from_nonce(
        record: &RecoveryRecord,
        call_id: &str,
        arguments: &str,
        nonce: String,
        phase: ApprovalPhase,
        formatter_binding: &str,
    ) -> Result<Self, ()> {
        if record.state() != RecoveryState::Intent
            || record.has_staging_plan()
            || call_id.is_empty()
            || call_id.len() > agent_runtime::MAX_CALL_ID_BYTES
            || arguments.len() > agent_runtime::MAX_ARGUMENT_BYTES
            || nonce.len() != 32
            || !nonce.bytes().all(is_lower_hex)
            || (phase == ApprovalPhase::Plain && !formatter_binding.is_empty())
            || (phase != ApprovalPhase::Plain
                && (formatter_binding.len() != 64 || !formatter_binding.bytes().all(is_lower_hex)))
        {
            return Err(());
        }
        let encoded = record.encode().map_err(|_| ())?;
        let mut hash = Sha256::new();
        hash.update(b"rapidlm.multi-edit.approval.v1\0");
        // Preserve the existing plain-phase fingerprint so interrupted
        // approvals created by earlier versions can still resume.
        match phase {
            ApprovalPhase::Plain => {}
            ApprovalPhase::AfterHook => hash.update(b"hook-after\0"),
            ApprovalPhase::BeforeFormat => hash.update(b"formatter-before\0"),
            ApprovalPhase::AfterFormat => hash.update(b"formatter-after\0"),
        }
        if phase != ApprovalPhase::Plain {
            hash.update(formatter_binding.as_bytes());
        }
        for part in [call_id.as_bytes(), arguments.as_bytes(), encoded.as_slice()] {
            hash.update((part.len() as u64).to_be_bytes());
            hash.update(part);
        }
        let fingerprint = hash
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Ok(Self {
            nonce,
            fingerprint,
            arguments_digest: arguments_digest(arguments),
            call_id: call_id.to_owned(),
            entries: record.entries().len(),
            phase,
            formatter_binding: formatter_binding.to_owned(),
        })
    }

    /// Reconstruct a chain only from a resolved approval supplied by the
    /// durable resume machinery; arbitrary model input is not authority.
    pub(super) fn resume(
        record: &RecoveryRecord,
        call_id: &str,
        arguments: &str,
        approved: &ApprovedAsk,
    ) -> Result<Self, ()> {
        if !approved.covers(arguments) {
            return Err(());
        }
        let source = approved.source.as_deref().ok_or(())?;
        let pieces: Vec<_> = source.split(':').collect();
        if pieces.len() != 5 || pieces[0] != "multi-edit" || pieces[1] != "v1" {
            return Err(());
        }
        let index = pieces[4].parse::<usize>().map_err(|_| ())?;
        if index.to_string() != pieces[4] {
            return Err(());
        }
        let chain = Self::from_nonce(
            record,
            call_id,
            arguments,
            pieces[2].to_owned(),
            ApprovalPhase::Plain,
            "",
        )?;
        if chain.source(index).as_deref() != Some(source) {
            return Err(());
        }
        Ok(chain)
    }

    /// Recover only a chain identifier from a resolved decision. This does
    /// not authorize the decision: callers must match it to an exact request
    /// returned by `approved_indices` for the appropriate phase.
    pub(super) fn from_resolved_for_phase(
        record: &RecoveryRecord,
        call_id: &str,
        arguments: &str,
        approved: &ApprovedAsk,
        phase: ApprovalPhase,
        formatter_binding: &str,
    ) -> Result<Self, ()> {
        if !approved.covers(arguments) {
            return Err(());
        }
        let pieces: Vec<_> = approved.source.as_deref().ok_or(())?.split(':').collect();
        if pieces.len() != 5
            || pieces[0] != "multi-edit"
            || pieces[1] != "v1"
            || pieces[3].len() != 64
            || !pieces[3].bytes().all(is_lower_hex)
        {
            return Err(());
        }
        let index = pieces[4].parse::<usize>().map_err(|_| ())?;
        if index.to_string() != pieces[4]
            || index >= workspace::multi_edit_recovery::MAX_RECOVERY_FILES
        {
            return Err(());
        }
        Self::from_nonce(
            record,
            call_id,
            arguments,
            pieces[2].to_owned(),
            phase,
            formatter_binding,
        )
    }

    fn source(&self, index: usize) -> Option<String> {
        (index < self.entries)
            .then(|| format!("multi-edit:v1:{}:{}:{index}", self.nonce, self.fingerprint))
    }

    /// Recover decisions only from a durable sink and verify the exact
    /// human-visible request for each path, including its preview and scope.
    pub(super) fn approved_indices(
        &self,
        record: &RecoveryRecord,
        arguments: &str,
        sink: &dyn ApprovalSink,
    ) -> Result<std::collections::BTreeSet<usize>, ()> {
        if Self::from_nonce(
            record,
            &self.call_id,
            arguments,
            self.nonce.clone(),
            self.phase,
            &self.formatter_binding,
        )? != *self
        {
            return Err(());
        }
        let prefix = format!("multi-edit:v1:{}:{}:", self.nonce, self.fingerprint);
        let requests = sink
            .approved_chain_sources(&self.call_id, TOOL, &self.arguments_digest, &prefix)
            .map_err(|_| ())?;
        let mut indices = std::collections::BTreeSet::new();
        for stored in requests {
            let index = (0..self.entries)
                .find(|index| self.source(*index).as_deref() == stored.source.as_deref())
                .ok_or(())?;
            let expected = self.request(record, index, arguments)?;
            if stored.call_id != expected.call_id
                || stored.tool != expected.tool
                || stored.summary != expected.summary
                || stored.scope != expected.scope
                || stored.diff != expected.diff
                || stored.source != expected.source
                || stored.arguments_digest != expected.arguments_digest
                || stored.remember_as.is_some()
            {
                return Err(());
            }
            indices.insert(index);
        }
        Ok(indices)
    }

    /// `approved` must come from a resolved ledger decision, not a model or
    /// caller-provided list. Each decision authorizes exactly one index.
    pub(super) fn authorizes(&self, index: usize, approved: &ApprovedAsk) -> bool {
        approved.arguments_digest.as_deref() == Some(self.arguments_digest.as_str())
            && approved.source.as_deref() == self.source(index).as_deref()
            && self.source(index).is_some()
    }

    pub(super) fn request(
        &self,
        record: &RecoveryRecord,
        index: usize,
        arguments: &str,
    ) -> Result<ApprovalRequest, ()> {
        let rebound = Self::from_nonce(
            record,
            &self.call_id,
            arguments,
            self.nonce.clone(),
            self.phase,
            &self.formatter_binding,
        )?;
        if &rebound != self {
            return Err(());
        }
        let entry = record.entries().get(index).ok_or(())?;
        if entry.path.len() > kernel::client::MAX_APPROVAL_SCOPE_BYTES {
            return Err(());
        }
        let action = match (&entry.before, &entry.after) {
            (None, Some(image)) => format!("create (mode {:04o})", image.mode.ok_or(())?),
            (Some(_), None) => "delete".to_owned(),
            (Some(_), Some(_)) => "edit".to_owned(),
            (None, None) => return Err(()),
        };
        let phase_note = match self.phase {
            ApprovalPhase::Plain => "",
            ApprovalPhase::AfterHook => "; review changes selected by the pre-use hook",
            ApprovalPhase::BeforeFormat => "; configured formatter may run after authorization",
            ApprovalPhase::AfterFormat => "; review formatted content for publication",
        };
        let summary = format!(
            "Approve {action} {} (file {} of {}){phase_note}",
            entry.path,
            index + 1,
            self.entries
        );
        if summary.len() > kernel::MAX_APPROVAL_SUMMARY_BYTES {
            return Err(());
        }
        let before = entry
            .before
            .as_ref()
            .map(|image| std::str::from_utf8(&image.bytes))
            .transpose()
            .map_err(|_| ())?
            .unwrap_or("");
        let after = entry
            .after
            .as_ref()
            .map(|image| std::str::from_utf8(&image.bytes))
            .transpose()
            .map_err(|_| ())?
            .unwrap_or("");
        let diff = match line_diff::unified(before, after) {
            DiffOutcome::Unified(diff) => diff,
            DiffOutcome::Identical => "(no line-level changes)".to_owned(),
            DiffOutcome::TooLarge => {
                "(diff too large to display; inspect the file changes before deciding)".to_owned()
            }
        };
        Ok(ApprovalRequest {
            tool: TOOL.to_owned(),
            call_id: self.call_id.clone(),
            summary,
            scope: vec![entry.path.clone()],
            diff,
            source: self.source(index),
            arguments_digest: Some(self.arguments_digest.clone()),
            remember_as: None,
        })
    }
}

fn is_lower_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
}

fn hook_configuration_bytes(hooks: &[crate::hooks::HookEntry]) -> Result<Vec<u8>, ()> {
    if hooks.is_empty() || hooks.len() > crate::hooks::MAX_HOOKS_PER_STAGE {
        return Err(());
    }
    serde_json::to_vec(&hooks.iter().map(|entry| serde_json::json!({
        "command":entry.command, "matcher":entry.matcher.as_ref().map(crate::hooks::HookMatcher::source),
        "timeout_ms":entry.timeout_ms, "continue_turn":entry.continue_turn
    })).collect::<Vec<_>>()).map_err(|_| ())
}

/// One hook question inside a candidate-bound multi-file chain. It does not
/// approve any path: a rewritten candidate still needs per-path decisions.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(dead_code)] // Connected by the pending pre-use hook coordinator.
pub(super) struct MultiHookQuestion {
    request: ApprovalRequest,
    source_prefix: String,
}

#[allow(dead_code)]
impl MultiHookQuestion {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        chain: &PathApprovalChain,
        original: &RecoveryRecord,
        original_arguments: &str,
        effective: &RecoveryRecord,
        effective_arguments: &str,
        hooks: &[crate::hooks::HookEntry],
        report: &crate::hooks::PreHookReport,
        visible_reason: &str,
    ) -> Result<Self, ()> {
        let crate::hooks::PreHookOutcome::Ask {
            hook,
            command_digest,
            reason,
        } = &report.outcome
        else {
            return Err(());
        };
        if hooks.is_empty()
            || hooks.len() > crate::hooks::MAX_HOOKS_PER_STAGE
            || visible_reason.len() > 256
            || effective_arguments.len() > agent_runtime::MAX_ARGUMENT_BYTES
        {
            return Err(());
        }
        if !super::publication::candidate_matches_multi_patch(original_arguments, original)
            || !super::publication::candidate_matches_multi_patch(effective_arguments, effective)
        {
            return Err(());
        }
        let base = chain.request(original, 0, original_arguments)?;
        match &report.rewrite {
            Some(rewrite)
                if serde_json::to_vec(&rewrite.input).map_err(|_| ())?
                    == effective_arguments.as_bytes() => {}
            None if original_arguments == effective_arguments => {}
            _ => return Err(()),
        }
        if effective.state() != RecoveryState::Intent || effective.has_staging_plan() {
            return Err(());
        }
        let effective_record = effective.encode().map_err(|_| ())?;
        let hook_config = hook_configuration_bytes(hooks)?;
        let decisions = report.decisions.iter().map(|entry| serde_json::json!({
            "hook":entry.hook, "event":entry.event, "command_digest":entry.command_digest,
            "decision":entry.decision.as_str(), "reason":entry.reason, "grant_attempted":entry.grant_attempted
        })).collect::<Vec<_>>();
        let contexts = report
            .context
            .iter()
            .map(|entry| serde_json::json!({"hook":entry.hook, "text":entry.text}))
            .collect::<Vec<_>>();
        let rewrite = report.rewrite.as_ref().map(|entry| {
            serde_json::json!({
                "hook":entry.hook, "command_digest":entry.command_digest,
                "input":entry.input, "contributors":entry.contributors
            })
        });
        let report_binding = serde_json::to_vec(&serde_json::json!({
            "hooks":hook_config, "decisions":decisions, "contexts":contexts, "rewrite":rewrite,
            "asking_hook":hook, "command_digest":command_digest, "reason":reason,
        }))
        .map_err(|_| ())?;
        let mut hash = Sha256::new();
        hash.update(b"rapidlm.multi-edit.hook-question.v1\0");
        for part in [
            base.source.as_deref().ok_or(())?.as_bytes(),
            original_arguments.as_bytes(),
            effective_arguments.as_bytes(),
            effective_record.as_slice(),
            report_binding.as_slice(),
            visible_reason.as_bytes(),
        ] {
            hash.update((part.len() as u64).to_be_bytes());
            hash.update(part);
        }
        let fingerprint: String = hash
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let source_prefix = format!("multi-edit:v1:{}:{}:", chain.nonce, fingerprint);
        let summary = format!(
            "Approve {hook} hook question for multi-file edit: {visible_reason}; file approvals are separate"
        );
        if summary.len() > kernel::MAX_APPROVAL_SUMMARY_BYTES {
            return Err(());
        }
        let request = ApprovalRequest {
            tool: TOOL.to_owned(),
            call_id: chain.call_id.clone(),
            summary,
            scope: vec![],
            diff: String::new(),
            source: Some(format!("{source_prefix}0")),
            arguments_digest: Some(arguments_digest(original_arguments)),
            remember_as: None,
        };
        Ok(Self {
            request,
            source_prefix,
        })
    }

    pub(super) fn request(&self) -> &ApprovalRequest {
        &self.request
    }

    /// Only an exact, durably approved displayed request counts. A latest
    /// answer is not itself authority and may belong to another phase.
    pub(super) fn is_approved(&self, sink: &dyn ApprovalSink) -> Result<bool, ()> {
        let requests = sink
            .approved_chain_sources(
                &self.request.call_id,
                TOOL,
                self.request.arguments_digest.as_deref().ok_or(())?,
                &self.source_prefix,
            )
            .map_err(|_| ())?;
        let mut found = false;
        for stored in requests {
            if stored.call_id != self.request.call_id
                || stored.tool != self.request.tool
                || stored.summary != self.request.summary
                || stored.scope != self.request.scope
                || stored.diff != self.request.diff
                || stored.source != self.request.source
                || stored.arguments_digest != self.request.arguments_digest
                || stored.remember_as.is_some()
            {
                return Err(());
            }
            found = true;
        }
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workspace::multi_edit_recovery::{FileImage, RecoveryEntry};

    fn record() -> RecoveryRecord {
        RecoveryRecord::new(
            RecoveryState::Intent,
            ["a", "b"]
                .into_iter()
                .map(|path| RecoveryEntry {
                    path: path.to_owned(),
                    before: Some(FileImage {
                        bytes: format!("{path}0\n").into_bytes(),
                        mode: Some(0o600),
                    }),
                    after: Some(FileImage {
                        bytes: format!("{path}1\n").into_bytes(),
                        mode: Some(0o600),
                    }),
                })
                .collect(),
        )
        .unwrap()
    }

    fn approved(request: &ApprovalRequest) -> ApprovedAsk {
        ApprovedAsk {
            source: request.source.clone(),
            arguments_digest: request.arguments_digest.clone(),
        }
    }

    #[test]
    fn decisions_are_bound_to_one_path_and_one_candidate() {
        let record = record();
        let args = r#"{"files":[]}"#;
        let chain = PathApprovalChain::new(&record, "call", args).unwrap();
        let first = chain.request(&record, 0, args).unwrap();
        let second = chain.request(&record, 1, args).unwrap();
        assert_eq!(first.scope, vec!["a"]);
        assert_eq!(second.scope, vec!["b"]);
        assert!(first.diff.contains("a0") && first.diff.contains("a1"));
        assert!(chain.authorizes(0, &approved(&first)));
        assert!(!chain.authorizes(1, &approved(&first)));
        assert_eq!(
            PathApprovalChain::resume(&record, "call", args, &approved(&first)).unwrap(),
            chain
        );
        assert!(PathApprovalChain::resume(&record, "other-call", args, &approved(&first)).is_err());
        assert!(PathApprovalChain::resume(&record, "call", "changed", &approved(&first)).is_err());
        let mut entries = record.entries().to_vec();
        entries[1].after.as_mut().unwrap().bytes = b"changed\n".to_vec();
        let changed = RecoveryRecord::new(RecoveryState::Intent, entries).unwrap();
        assert!(PathApprovalChain::resume(&changed, "call", args, &approved(&first)).is_err());
        assert!(chain.request(&changed, 0, args).is_err());
        let other = PathApprovalChain::new(&record, "call", args).unwrap();
        assert!(!other.authorizes(0, &approved(&first)));
    }

    #[test]
    fn malformed_or_unbound_sources_do_not_authorize() {
        let record = record();
        let args = "{}";
        let chain = PathApprovalChain::new(&record, "call", args).unwrap();
        let request = chain.request(&record, 0, args).unwrap();
        for source in [
            None,
            Some("multi-edit:v1:bad:bad:0".into()),
            request.source.clone().map(|s| format!("{s}0")),
        ] {
            let bad = ApprovedAsk {
                source,
                arguments_digest: request.arguments_digest.clone(),
            };
            assert!(PathApprovalChain::resume(&record, "call", args, &bad).is_err());
            assert!(!chain.authorizes(0, &bad));
        }
        assert!(!chain.authorizes(2, &ApprovedAsk::default()));
    }

    #[test]
    fn multi_hook_question_binds_candidate_configuration_report_and_durable_preview() {
        struct Sink(Vec<kernel::ApprovalRequestedPayload>);
        impl ApprovalSink for Sink {
            fn request(&self, _: &ApprovalRequest) -> Result<String, String> {
                Err("unused".into())
            }
            fn approved_chain_sources(
                &self,
                _: &str,
                _: &str,
                _: &str,
                _: &str,
            ) -> Result<Vec<kernel::ApprovalRequestedPayload>, String> {
                Ok(self.0.clone())
            }
        }
        let candidate = record();
        let args = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]},{"path":"b","edits":[{"old":"b0","new":"b1"}]}]}"#;
        let chain = PathApprovalChain::new(&candidate, "call", args).unwrap();
        let hooks = vec![crate::hooks::HookEntry::plain("echo secret-command")];
        let report = crate::hooks::PreHookReport {
            outcome: crate::hooks::PreHookOutcome::Ask {
                hook: "pre_tool_use[0]".into(),
                command_digest: "123456789abc".into(),
                reason: "review".into(),
            },
            decisions: vec![],
            rewrite: None,
            context: vec![crate::hooks::HookContext {
                hook: "pre_tool_use[0]".into(),
                text: "context".into(),
            }],
        };
        let question = MultiHookQuestion::new(
            &chain, &candidate, args, &candidate, args, &hooks, &report, "review",
        )
        .unwrap();
        assert!(
            question
                .request()
                .summary
                .contains("file approvals are separate")
        );
        assert!(!question.request().summary.contains("secret-command"));
        assert!(question.request().scope.is_empty());
        let request = question.request();
        let stored: kernel::ApprovalRequestedPayload = serde_json::from_value(serde_json::json!({
            "id":"wait", "call_id":request.call_id, "tool":request.tool,
            "summary":request.summary, "scope":request.scope, "diff":request.diff,
            "source":request.source, "arguments_digest":request.arguments_digest
        }))
        .unwrap();
        assert_eq!(question.is_approved(&Sink(vec![])), Ok(false));
        assert_eq!(question.is_approved(&Sink(vec![stored.clone()])), Ok(true));
        let mut tampered = stored.clone();
        tampered.summary.push_str(" changed");
        assert_eq!(question.is_approved(&Sink(vec![stored, tampered])), Err(()));
        let changed_hooks = vec![crate::hooks::HookEntry::plain("echo different")];
        assert_ne!(
            chain.hook_checkpoint_key(&candidate, args, &hooks).unwrap(),
            chain
                .hook_checkpoint_key(&candidate, args, &changed_hooks)
                .unwrap()
        );
        let changed = MultiHookQuestion::new(
            &chain,
            &candidate,
            args,
            &candidate,
            args,
            &changed_hooks,
            &report,
            "review",
        )
        .unwrap();
        assert_ne!(question.request().source, changed.request().source);
        let mut changed_report = report.clone();
        changed_report.context[0].text.push_str(" changed");
        let changed = MultiHookQuestion::new(
            &chain,
            &candidate,
            args,
            &candidate,
            args,
            &hooks,
            &changed_report,
            "review",
        )
        .unwrap();
        assert_ne!(question.request().source, changed.request().source);
        let mut changed_entries = candidate.entries().to_vec();
        changed_entries[0].after.as_mut().unwrap().bytes = b"changed".to_vec();
        let changed_candidate =
            RecoveryRecord::new(RecoveryState::Intent, changed_entries).unwrap();
        assert!(
            MultiHookQuestion::new(
                &chain,
                &candidate,
                args,
                &changed_candidate,
                args,
                &hooks,
                &report,
                "review"
            )
            .is_err()
        );
        assert!(
            MultiHookQuestion::new(
                &chain, &candidate, args, &candidate, "changed", &hooks, &report, "review"
            )
            .is_err()
        );
        let rewrite_input = serde_json::json!({"files":[
            {"path":"a","edits":[{"old":"a0\n","new":"changed"}]},
            {"path":"b","edits":[{"old":"b0","new":"b1"}]}
        ]});
        let mut rewritten_report = report.clone();
        rewritten_report.rewrite = Some(crate::hooks::HookRewrite {
            hook: "pre_tool_use[0]".into(),
            command_digest: "123456789abc".into(),
            input: rewrite_input.as_object().unwrap().clone(),
            contributors: vec!["pre_tool_use[0]".into()],
        });
        let rewritten_args = rewrite_input.to_string();
        let rewritten = MultiHookQuestion::new(
            &chain,
            &candidate,
            args,
            &changed_candidate,
            &rewritten_args,
            &hooks,
            &rewritten_report,
            "review",
        )
        .unwrap();
        assert_ne!(question.request().source, rewritten.request().source);
        assert!(
            MultiHookQuestion::new(
                &chain,
                &candidate,
                args,
                &changed_candidate,
                args,
                &hooks,
                &rewritten_report,
                "review"
            )
            .is_err()
        );
    }

    #[test]
    fn hook_question_approval_is_recovered_from_a_reopened_ledger() {
        use crate::approvals::{LedgerApprovalSink, client_call};
        use event_ledger::event::{ActorKind, ActorRef};
        use kernel::{ApprovalDecision, KernelClient};
        let path = std::env::temp_dir().join(format!(
            "rapid-hook-chain-{}-{}.sqlite",
            std::process::id(),
            protocol::EventId::new()
        ));
        let client = kernel::InProcessKernelClient::open(&path).unwrap();
        let actor = ActorRef::new(ActorKind::Human, &protocol::EventId::new().to_string()).unwrap();
        let session = client_call(client.create_session(kernel::CreateSession::new(
            protocol::ProjectId::new(),
            actor.clone(),
            protocol::TraceId::new(),
        )))
        .unwrap()
        .id();
        let candidate = record();
        let args = r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]},{"path":"b","edits":[{"old":"b0","new":"b1"}]}]}"#;
        let chain = PathApprovalChain::new(&candidate, "call", args).unwrap();
        let hooks = vec![crate::hooks::HookEntry::plain("echo hook")];
        let report = crate::hooks::PreHookReport {
            outcome: crate::hooks::PreHookOutcome::Ask {
                hook: "pre_tool_use[0]".into(),
                command_digest: "123456789abc".into(),
                reason: "review".into(),
            },
            decisions: vec![],
            rewrite: None,
            context: vec![],
        };
        let question = MultiHookQuestion::new(
            &chain, &candidate, args, &candidate, args, &hooks, &report, "review",
        )
        .unwrap();
        let sink =
            LedgerApprovalSink::new(client.clone(), session, actor.clone(), std::env::temp_dir());
        let token = sink.request(question.request()).unwrap();
        assert!(!question.is_approved(&sink).unwrap());
        let pending = client_call(client.pending_approvals(session)).unwrap();
        let answer = ApprovedAsk::from_payload(pending[0].payload());
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
                .with_wait_token(&token),
            ),
        )
        .unwrap();
        drop(sink);
        drop(client);
        let reopened = kernel::InProcessKernelClient::open(&path).unwrap();
        let sink = LedgerApprovalSink::new(reopened, session, actor, std::env::temp_dir());
        let recovered = PathApprovalChain::from_resolved_for_phase(
            &candidate,
            "call",
            args,
            &answer,
            ApprovalPhase::Plain,
            "",
        )
        .unwrap();
        assert_eq!(recovered, chain);
        let recovered_question = MultiHookQuestion::new(
            &recovered, &candidate, args, &candidate, args, &hooks, &report, "review",
        )
        .unwrap();
        assert_eq!(recovered_question, question);
        assert!(recovered_question.is_approved(&sink).unwrap());
        assert!(!recovered.authorizes(0, &answer));
        drop(sink);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
    }

    #[test]
    fn rewritten_paths_recover_nonce_without_granting_original_paths() {
        let original = record();
        let args = "{}";
        let chain = PathApprovalChain::new(&original, "call", args).unwrap();
        let mut entries = original.entries().to_vec();
        let mut extra = entries[0].clone();
        extra.path = "c".into();
        entries.push(extra);
        let rewritten = RecoveryRecord::new(RecoveryState::Intent, entries).unwrap();
        let after = chain
            .for_hook_rewrite(&rewritten, args, &"a".repeat(64))
            .unwrap();
        let request = after.request(&rewritten, 2, args).unwrap();
        assert_eq!(request.scope, vec!["c"]);
        assert!(request.summary.contains("pre-use hook"));
        let answer = approved(&request);
        let recovered = PathApprovalChain::from_resolved_for_phase(
            &original,
            "call",
            args,
            &answer,
            ApprovalPhase::Plain,
            "",
        )
        .unwrap();
        assert_eq!(recovered, chain);
        assert!(!recovered.authorizes(0, &answer));
        assert_eq!(
            recovered
                .for_hook_rewrite(&rewritten, args, &"a".repeat(64))
                .unwrap(),
            after
        );
        assert!(
            !recovered
                .for_hook_rewrite(&rewritten, args, &"b".repeat(64))
                .unwrap()
                .authorizes(2, &answer)
        );
        assert!(PathApprovalChain::resume(&original, "call", args, &answer).is_err());
        let mut out_of_bounds = answer.clone();
        let source = out_of_bounds.source.as_mut().unwrap();
        source.replace_range(
            source.rfind(':').unwrap() + 1..,
            &workspace::multi_edit_recovery::MAX_RECOVERY_FILES.to_string(),
        );
        assert!(
            PathApprovalChain::from_resolved_for_phase(
                &original,
                "call",
                args,
                &out_of_bounds,
                ApprovalPhase::Plain,
                "",
            )
            .is_err()
        );
    }

    #[test]
    fn formatter_phases_bind_distinct_requests_and_recover_only_the_nonce() {
        let record = record();
        let args = "{}";
        let binding = "a".repeat(64);
        let before = PathApprovalChain::new_before_format(&record, "call", args, &binding).unwrap();
        let after = before
            .for_phase(&record, args, ApprovalPhase::AfterFormat)
            .unwrap();
        let before_request = before.request(&record, 0, args).unwrap();
        let after_request = after.request(&record, 0, args).unwrap();
        assert!(before_request.summary.contains("formatter may run"));
        assert!(after_request.summary.contains("formatted content"));
        assert_ne!(before_request.source, after_request.source);
        assert!(!after.authorizes(0, &approved(&before_request)));
        assert!(!before.authorizes(0, &approved(&after_request)));
        let recovered = PathApprovalChain::from_resolved_for_phase(
            &record,
            "call",
            args,
            &approved(&after_request),
            ApprovalPhase::BeforeFormat,
            &binding,
        )
        .unwrap();
        assert_eq!(recovered, before);
        assert!(!recovered.authorizes(0, &approved(&after_request)));
        let changed_binding = "b".repeat(64);
        let changed = PathApprovalChain::from_resolved_for_phase(
            &record,
            "call",
            args,
            &approved(&before_request),
            ApprovalPhase::BeforeFormat,
            &changed_binding,
        )
        .unwrap();
        assert!(!changed.authorizes(0, &approved(&before_request)));
        assert!(
            PathApprovalChain::resume(&record, "call", args, &approved(&before_request)).is_err()
        );
        let malformed = ApprovedAsk {
            source: Some("multi-edit:v1:0123456789abcdef0123456789abcdef:bad:0".into()),
            arguments_digest: before_request.arguments_digest.clone(),
        };
        assert!(
            PathApprovalChain::from_resolved_for_phase(
                &record,
                "call",
                args,
                &malformed,
                ApprovalPhase::BeforeFormat,
                &binding,
            )
            .is_err()
        );
    }

    #[test]
    fn durable_history_must_match_the_full_per_path_request() {
        struct Sink(Vec<kernel::ApprovalRequestedPayload>);
        impl ApprovalSink for Sink {
            fn request(&self, _: &ApprovalRequest) -> Result<String, String> {
                Err("unused".into())
            }
            fn approved_chain_sources(
                &self,
                _: &str,
                _: &str,
                _: &str,
                _: &str,
            ) -> Result<Vec<kernel::ApprovalRequestedPayload>, String> {
                Ok(self.0.clone())
            }
        }
        let record = record();
        let chain = PathApprovalChain::new(&record, "call", "{}").unwrap();
        let request = chain.request(&record, 0, "{}").unwrap();
        let stored: kernel::ApprovalRequestedPayload = serde_json::from_value(serde_json::json!({
            "id":"wait", "call_id":request.call_id, "tool":request.tool,
            "summary":request.summary, "scope":request.scope, "diff":request.diff,
            "source":request.source, "arguments_digest":request.arguments_digest
        }))
        .unwrap();
        assert_eq!(
            chain
                .approved_indices(&record, "{}", &Sink(vec![stored.clone()]))
                .unwrap(),
            std::collections::BTreeSet::from([0])
        );
        let mut tampered = stored;
        tampered.scope = vec!["b".into()];
        assert!(
            chain
                .approved_indices(&record, "{}", &Sink(vec![tampered]))
                .is_err()
        );
        let mut changed = record.entries().to_vec();
        changed[0].after.as_mut().unwrap().bytes = b"different".to_vec();
        let changed = RecoveryRecord::new(RecoveryState::Intent, changed).unwrap();
        assert!(
            chain
                .approved_indices(&changed, "{}", &Sink(vec![]))
                .is_err()
        );
    }
}
