//! Durable pre-use hook results for multi-file edits. The coordinator must
//! authorize paths before starting a hook and must not re-execute a running or
//! uncertain checkpoint. All operations require the exclusive journal lock.
#![allow(dead_code)] // Runtime orchestration remains gated until connected.

use super::multi_approval::PathApprovalChain;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use workspace::multi_edit_journal::JournalStore;
use workspace::multi_edit_recovery::RecoveryRecord;

const RUNNING: &[u8] = b"rapidlm.multi-hook.running.v1\n";
const RETIRED: &[u8] = b"rapidlm.multi-hook.retired.v1\n";
const MAX_BYTES: usize = 1024 * 1024;

#[derive(Debug, Eq, PartialEq)]
pub(super) enum Error {
    Invalid,
    Io,
    Exists,
    Uncertain,
}
#[derive(Debug, Eq, PartialEq)]
pub(super) enum State {
    Missing,
    Running,
    Retired,
    Ready(Box<crate::hooks::PreHookReport>),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    schema: String,
    version: u8,
    key: String,
    report_sha256: String,
    report: WireReport,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    schema: String,
    version: u8,
    key: String,
    request_sha256: String,
}

fn request_digest(request: &crate::approvals::ApprovalRequest) -> Result<String, Error> {
    let bytes = serde_json::to_vec(&serde_json::json!({
        "tool": request.tool, "call_id": request.call_id, "summary": request.summary,
        "scope": request.scope, "diff": request.diff, "source": request.source,
        "arguments_digest": request.arguments_digest, "remember_as": request.remember_as,
    }))
    .map_err(|_| Error::Invalid)?;
    if bytes.len() > MAX_BYTES {
        return Err(Error::Invalid);
    }
    Ok(super::sha256_hex(&bytes))
}

fn safe_bytes(path: &std::path::Path) -> Result<Option<Vec<u8>>, Error> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(Error::Io),
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.len() > MAX_BYTES as u64
    {
        return Err(Error::Invalid);
    }
    let file = File::open(path).map_err(|_| Error::Io)?;
    let opened = file.metadata().map_err(|_| Error::Io)?;
    if metadata.dev() != opened.dev() || metadata.ino() != opened.ino() {
        return Err(Error::Invalid);
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::Io)?;
    if bytes.len() > MAX_BYTES {
        return Err(Error::Invalid);
    }
    Ok(Some(bytes))
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireReport {
    outcome: WireOutcome,
    decisions: Vec<WireDecision>,
    rewrite: Option<WireRewrite>,
    context: Vec<WireContext>,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum WireOutcome {
    Allowed,
    Denied {
        reason: String,
    },
    Ask {
        hook: String,
        command_digest: String,
        reason: String,
    },
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireDecision {
    hook: String,
    command_digest: String,
    decision: protocol::HookDecision,
    reason: Option<String>,
    grant_attempted: bool,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRewrite {
    hook: String,
    command_digest: String,
    input: serde_json::Map<String, serde_json::Value>,
    contributors: Vec<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireContext {
    hook: String,
    text: String,
}

impl From<&crate::hooks::PreHookReport> for WireReport {
    fn from(report: &crate::hooks::PreHookReport) -> Self {
        use crate::hooks::PreHookOutcome;
        let outcome = match &report.outcome {
            PreHookOutcome::Allowed => WireOutcome::Allowed,
            PreHookOutcome::Denied { reason } => WireOutcome::Denied {
                reason: reason.clone(),
            },
            PreHookOutcome::Ask {
                hook,
                command_digest,
                reason,
            } => WireOutcome::Ask {
                hook: hook.clone(),
                command_digest: command_digest.clone(),
                reason: reason.clone(),
            },
        };
        Self {
            outcome,
            decisions: report
                .decisions
                .iter()
                .map(|d| WireDecision {
                    hook: d.hook.clone(),
                    command_digest: d.command_digest.clone(),
                    decision: d.decision,
                    reason: d.reason.clone(),
                    grant_attempted: d.grant_attempted,
                })
                .collect(),
            rewrite: report.rewrite.as_ref().map(|r| WireRewrite {
                hook: r.hook.clone(),
                command_digest: r.command_digest.clone(),
                input: r.input.clone(),
                contributors: r.contributors.clone(),
            }),
            context: report
                .context
                .iter()
                .map(|c| WireContext {
                    hook: c.hook.clone(),
                    text: c.text.clone(),
                })
                .collect(),
        }
    }
}

impl WireReport {
    fn into_validated(
        self,
        hooks: &[crate::hooks::HookEntry],
        original_arguments: &str,
    ) -> Result<crate::hooks::PreHookReport, Error> {
        use crate::hooks::{
            HookContext, HookDecisionRecord, HookRewrite, HookSpec, PreHookOutcome,
        };
        if hooks.is_empty()
            || hooks.len() > crate::hooks::MAX_HOOKS_PER_STAGE
            || self.decisions.len() > hooks.len()
            || self.context.len() > hooks.len()
        {
            return Err(Error::Invalid);
        }
        let lookup = |name: &str, digest: &str| -> Result<usize, Error> {
            let number = name
                .strip_prefix("pre_tool_use[")
                .and_then(|name| name.strip_suffix(']'))
                .ok_or(Error::Invalid)?;
            let index: usize = number.parse().map_err(|_| Error::Invalid)?;
            if index.to_string() != number {
                return Err(Error::Invalid);
            }
            let hook = hooks.get(index).ok_or(Error::Invalid)?;
            if !hook.applies_to(Some(super::WORKSPACE_MULTI_PATCH_TOOL))
                || digest != &super::sha256_hex(hook.command.as_bytes())[..12]
            {
                return Err(Error::Invalid);
            }
            Ok(index)
        };
        let mut prior = None;
        for decision in &self.decisions {
            let index = lookup(&decision.hook, &decision.command_digest)?;
            if prior.is_some_and(|previous| previous >= index)
                || decision.reason.as_ref().is_some_and(|reason| {
                    reason.is_empty() || reason.len() > protocol::MAX_HOOK_REASON_BYTES
                })
            {
                return Err(Error::Invalid);
            }
            prior = Some(index);
        }
        let outcome = match self.outcome {
            WireOutcome::Allowed => {
                if self.decisions.iter().any(|d| {
                    matches!(
                        d.decision,
                        protocol::HookDecision::Ask | protocol::HookDecision::Deny
                    )
                }) {
                    return Err(Error::Invalid);
                }
                PreHookOutcome::Allowed
            }
            WireOutcome::Denied { reason } => {
                if reason.len() > crate::hooks::MAX_HOOK_STDERR_BYTES
                    || self.rewrite.is_some()
                    || !self.context.is_empty()
                {
                    return Err(Error::Invalid);
                }
                PreHookOutcome::Denied { reason }
            }
            WireOutcome::Ask {
                hook,
                command_digest,
                reason,
            } => {
                lookup(&hook, &command_digest)?;
                let first = self
                    .decisions
                    .iter()
                    .find(|d| d.decision == protocol::HookDecision::Ask)
                    .ok_or(Error::Invalid)?;
                let expected = first
                    .reason
                    .clone()
                    .unwrap_or_else(|| format!("{} hook asked for approval", first.hook));
                if first.hook != hook
                    || first.command_digest != command_digest
                    || reason != expected
                    || self
                        .decisions
                        .iter()
                        .any(|d| d.decision == protocol::HookDecision::Deny)
                {
                    return Err(Error::Invalid);
                }
                PreHookOutcome::Ask {
                    hook,
                    command_digest,
                    reason,
                }
            }
        };
        let mut context_prior = None;
        for context in &self.context {
            let decision = self
                .decisions
                .iter()
                .find(|d| d.hook == context.hook)
                .ok_or(Error::Invalid)?;
            let index = lookup(&context.hook, &decision.command_digest)?;
            if context_prior.is_some_and(|previous| previous >= index)
                || context.text.trim().is_empty()
                || context.text.len() > protocol::MAX_HOOK_CONTEXT_BYTES
            {
                return Err(Error::Invalid);
            }
            context_prior = Some(index);
        }
        let rewrite = match self.rewrite {
            Some(rewrite) => {
                lookup(&rewrite.hook, &rewrite.command_digest)?;
                if rewrite.contributors.is_empty()
                    || rewrite.contributors.len() > hooks.len()
                    || rewrite.contributors.last() != Some(&rewrite.hook)
                    || serde_json::to_vec(&rewrite.input)
                        .map_err(|_| Error::Invalid)?
                        .len()
                        > protocol::MAX_HOOK_RESULT_BYTES
                    || serde_json::from_str::<serde_json::Value>(original_arguments)
                        .ok()
                        .as_ref()
                        == Some(&serde_json::Value::Object(rewrite.input.clone()))
                {
                    return Err(Error::Invalid);
                }
                let mut previous = None;
                for name in &rewrite.contributors {
                    let decision = self
                        .decisions
                        .iter()
                        .find(|d| &d.hook == name)
                        .ok_or(Error::Invalid)?;
                    let index = lookup(name, &decision.command_digest)?;
                    if previous.is_some_and(|prior| prior >= index) {
                        return Err(Error::Invalid);
                    }
                    previous = Some(index);
                }
                Some(HookRewrite {
                    hook: rewrite.hook,
                    command_digest: rewrite.command_digest,
                    input: rewrite.input,
                    contributors: rewrite.contributors,
                })
            }
            None => None,
        };
        Ok(crate::hooks::PreHookReport {
            outcome,
            decisions: self
                .decisions
                .into_iter()
                .map(|d| HookDecisionRecord {
                    hook: d.hook,
                    event: "pre_tool_use",
                    command_digest: d.command_digest,
                    decision: d.decision,
                    reason: d.reason,
                    grant_attempted: d.grant_attempted,
                })
                .collect(),
            rewrite,
            context: self
                .context
                .into_iter()
                .map(|c| HookContext {
                    hook: c.hook,
                    text: c.text,
                })
                .collect(),
        })
    }
}

pub(super) struct HookCheckpoint<'a> {
    journal: &'a JournalStore,
    key: String,
    hooks: &'a [crate::hooks::HookEntry],
    arguments: &'a str,
    chain: &'a PathApprovalChain,
}
impl<'a> HookCheckpoint<'a> {
    pub(super) fn new(
        journal: &'a JournalStore,
        chain: &'a PathApprovalChain,
        raw: &RecoveryRecord,
        arguments: &'a str,
        hooks: &'a [crate::hooks::HookEntry],
    ) -> Result<Self, Error> {
        if !super::publication::candidate_matches_multi_patch(arguments, raw) {
            return Err(Error::Invalid);
        }
        let key = chain
            .hook_checkpoint_key(raw, arguments, hooks)
            .map_err(|_| Error::Invalid)?;
        Ok(Self {
            journal,
            key,
            hooks,
            arguments,
            chain,
        })
    }
    fn path(&self) -> std::path::PathBuf {
        self.journal
            .directory()
            .join(format!("hook-{}.json", &self.key[..32]))
    }
    fn temporary(&self) -> std::path::PathBuf {
        self.journal
            .directory()
            .join(format!(".hook-{}.tmp", &self.key[..32]))
    }
    fn no_temporary(&self) -> Result<(), Error> {
        match fs::symlink_metadata(self.temporary()) {
            Ok(_) => Err(Error::Uncertain),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(Error::Io),
        }
    }
    pub(super) fn start(&self) -> Result<(), Error> {
        self.no_temporary()?;
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.path())
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Err(Error::Exists),
            Err(_) => return Err(Error::Io),
        };
        file.write_all(RUNNING)
            .and_then(|()| file.sync_all())
            .map_err(|_| Error::Uncertain)?;
        File::open(self.journal.directory())
            .and_then(|dir| dir.sync_all())
            .map_err(|_| Error::Uncertain)
    }
    pub(super) fn load(&self) -> Result<State, Error> {
        self.no_temporary()?;
        let metadata = match fs::symlink_metadata(self.path()) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(State::Missing),
            Err(_) => return Err(Error::Io),
        };
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.permissions().mode() & 0o077 != 0
            || metadata.len() > MAX_BYTES as u64
        {
            return Err(Error::Invalid);
        }
        let file = File::open(self.path()).map_err(|_| Error::Io)?;
        let opened = file.metadata().map_err(|_| Error::Io)?;
        if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
            return Err(Error::Invalid);
        }
        let mut bytes = Vec::new();
        file.take(MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::Io)?;
        if bytes == RUNNING {
            return Ok(State::Running);
        }
        if bytes == RETIRED {
            return Ok(State::Retired);
        }
        if bytes.len() > MAX_BYTES {
            return Err(Error::Invalid);
        }
        let stored: Stored = serde_json::from_slice(&bytes).map_err(|_| Error::Invalid)?;
        if stored.schema != "rapidlm.multi_hook_checkpoint"
            || stored.version != 1
            || stored.key != self.key
            || stored.report_sha256
                != super::sha256_hex(
                    &serde_json::to_vec(&stored.report).map_err(|_| Error::Invalid)?,
                )
        {
            return Err(Error::Invalid);
        }
        Ok(State::Ready(Box::new(
            stored.report.into_validated(self.hooks, self.arguments)?,
        )))
    }
    pub(super) fn finish(&self, report: &crate::hooks::PreHookReport) -> Result<(), Error> {
        if self.load()? != State::Running {
            return Err(Error::Invalid);
        }
        let wire = WireReport::from(report);
        let report_sha256 =
            super::sha256_hex(&serde_json::to_vec(&wire).map_err(|_| Error::Invalid)?);
        let bytes = serde_json::to_vec(&Stored {
            schema: "rapidlm.multi_hook_checkpoint".into(),
            version: 1,
            key: self.key.clone(),
            report_sha256,
            report: wire,
        })
        .map_err(|_| Error::Invalid)?;
        if bytes.len() > MAX_BYTES {
            return Err(Error::Invalid);
        }
        let stored: Stored = serde_json::from_slice(&bytes).map_err(|_| Error::Invalid)?;
        if stored.report.into_validated(self.hooks, self.arguments)? != *report {
            return Err(Error::Invalid);
        }
        self.replace(&bytes)
    }
    /// The live owner may retire its marker after the interrupted hook has
    /// been terminated. A reopened running marker remains uncertain.
    pub(super) fn retire_after_cancelled_execution(&self) -> Result<(), Error> {
        if self.load()? != State::Running {
            return Err(Error::Invalid);
        }
        self.replace(RETIRED)
    }
    pub(super) fn retire(&self, expected: &crate::hooks::PreHookReport) -> Result<(), Error> {
        if self.load()? != State::Ready(Box::new(expected.clone())) {
            return Err(Error::Invalid);
        }
        let nonce = &self.key[..32];
        let temporary = self
            .journal
            .directory()
            .join(format!(".hook-{nonce}.pending.tmp"));
        match fs::symlink_metadata(temporary) {
            Ok(_) => return Err(Error::Uncertain),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(Error::Io),
        }
        self.replace(RETIRED)?;
        let pending = self
            .journal
            .directory()
            .join(format!("hook-{nonce}.pending"));
        match fs::remove_file(pending) {
            Ok(()) => File::open(self.journal.directory())
                .and_then(|dir| dir.sync_all())
                .map_err(|_| Error::Uncertain),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(Error::Uncertain),
        }
    }
    /// Bind the exact visible pending request before handing it to the sink.
    /// Only its digest is retained outside the report checkpoint.
    pub(super) fn bind_pending(
        &self,
        expected: &crate::hooks::PreHookReport,
        request: &crate::approvals::ApprovalRequest,
    ) -> Result<(), Error> {
        if self.load()? != State::Ready(Box::new(expected.clone()))
            || !self.chain.matches_chain_request(request)
        {
            return Err(Error::Invalid);
        }
        let pending = Pending {
            schema: "rapidlm.multi_hook_pending".into(),
            version: 1,
            key: self.key.clone(),
            request_sha256: request_digest(request)?,
        };
        let bytes = serde_json::to_vec(&pending).map_err(|_| Error::Invalid)?;
        let nonce = &self.key[..32];
        let temporary = self
            .journal
            .directory()
            .join(format!(".hook-{nonce}.pending.tmp"));
        let target = self
            .journal
            .directory()
            .join(format!("hook-{nonce}.pending"));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|_| Error::Uncertain)?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|_| Error::Uncertain)?;
        fs::rename(temporary, target).map_err(|_| Error::Uncertain)?;
        File::open(self.journal.directory())
            .and_then(|dir| dir.sync_all())
            .map_err(|_| Error::Uncertain)
    }
    /// Produce a stable binding only for the exact validated, durable result.
    /// The checkpoint key already covers the original chain and hook config.
    pub(super) fn approval_binding(
        &self,
        expected: &crate::hooks::PreHookReport,
    ) -> Result<String, Error> {
        if self.load()? != State::Ready(Box::new(expected.clone())) {
            return Err(Error::Invalid);
        }
        let report = serde_json::to_vec(&WireReport::from(expected)).map_err(|_| Error::Invalid)?;
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        hash.update(b"rapidlm.multi-edit.hook-result.v1\0");
        for part in [self.key.as_bytes(), report.as_slice()] {
            hash.update((part.len() as u64).to_be_bytes());
            hash.update(part);
        }
        Ok(hash
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect())
    }
    fn replace(&self, bytes: &[u8]) -> Result<(), Error> {
        self.no_temporary()?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.temporary())
            .map_err(|_| Error::Uncertain)?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|_| Error::Uncertain)?;
        fs::rename(self.temporary(), self.path()).map_err(|_| Error::Uncertain)?;
        File::open(self.journal.directory())
            .and_then(|dir| dir.sync_all())
            .map_err(|_| Error::Uncertain)
    }
}

/// Retire only the checkpoint whose persisted pending request exactly matches
/// a durably denied request. The caller must verify the ledger denial and the
/// original suspended call before invoking this under the journal lock.
pub(super) fn retire_denied(
    journal: &JournalStore,
    request: &kernel::ApprovalRequestedPayload,
    arguments: &str,
) -> Result<bool, Error> {
    if request.tool != super::WORKSPACE_MULTI_PATCH_TOOL
        || request.arguments_digest.as_deref()
            != Some(crate::approvals::arguments_digest(arguments).as_str())
        || request.remember_as.is_some()
    {
        return Ok(false);
    }
    let Some(source) = request.source.as_deref() else {
        return Ok(false);
    };
    let pieces: Vec<_> = source.split(':').collect();
    let is_hex = |part: &str, length: usize| {
        part.len() == length
            && part
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    };
    if pieces.len() != 5
        || pieces[0] != "multi-edit"
        || pieces[1] != "v1"
        || !is_hex(pieces[2], 32)
        || !is_hex(pieces[3], 64)
        || !pieces[4].parse::<usize>().is_ok_and(|i| {
            i.to_string() == pieces[4] && i < workspace::multi_edit_recovery::MAX_RECOVERY_FILES
        })
    {
        return Ok(false);
    }
    let nonce = pieces[2];
    let directory = journal.directory();
    let pending_path = directory.join(format!("hook-{nonce}.pending"));
    let pending_temp = directory.join(format!(".hook-{nonce}.pending.tmp"));
    let report_temp = directory.join(format!(".hook-{nonce}.tmp"));
    for path in [&pending_temp, &report_temp] {
        match fs::symlink_metadata(path) {
            Ok(_) => return Err(Error::Uncertain),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(Error::Io),
        }
    }
    let Some(bytes) = safe_bytes(&pending_path)? else {
        return Ok(false);
    };
    let pending: Pending = serde_json::from_slice(&bytes).map_err(|_| Error::Invalid)?;
    if pending.schema != "rapidlm.multi_hook_pending"
        || pending.version != 1
        || pending.key.len() != 96
        || !pending.key.starts_with(nonce)
        || !is_hex(&pending.key, 96)
    {
        return Err(Error::Invalid);
    }
    let presented = crate::approvals::ApprovalRequest {
        tool: request.tool.clone(),
        call_id: request.call_id.clone(),
        summary: request.summary.clone(),
        scope: request.scope.clone(),
        diff: request.diff.clone(),
        source: request.source.clone(),
        arguments_digest: request.arguments_digest.clone(),
        remember_as: request.remember_as.clone(),
    };
    if pending.request_sha256 != request_digest(&presented)? {
        return Ok(false);
    }
    let report_path = directory.join(format!("hook-{nonce}.json"));
    let Some(bytes) = safe_bytes(&report_path)? else {
        return Err(Error::Uncertain);
    };
    if bytes == RETIRED {
        fs::remove_file(pending_path).map_err(|_| Error::Uncertain)?;
        File::open(directory)
            .and_then(|dir| dir.sync_all())
            .map_err(|_| Error::Uncertain)?;
        return Ok(true);
    }
    if bytes == RUNNING {
        return Err(Error::Uncertain);
    }
    let stored: Stored = serde_json::from_slice(&bytes).map_err(|_| Error::Invalid)?;
    if stored.schema != "rapidlm.multi_hook_checkpoint"
        || stored.version != 1
        || stored.key != pending.key
        || stored.report_sha256
            != super::sha256_hex(&serde_json::to_vec(&stored.report).map_err(|_| Error::Invalid)?)
    {
        return Err(Error::Invalid);
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&report_temp)
        .map_err(|_| Error::Uncertain)?;
    file.write_all(RETIRED)
        .and_then(|()| file.sync_all())
        .map_err(|_| Error::Uncertain)?;
    fs::rename(report_temp, report_path).map_err(|_| Error::Uncertain)?;
    File::open(directory)
        .and_then(|dir| dir.sync_all())
        .map_err(|_| Error::Uncertain)?;
    fs::remove_file(pending_path).map_err(|_| Error::Uncertain)?;
    File::open(directory)
        .and_then(|dir| dir.sync_all())
        .map_err(|_| Error::Uncertain)?;
    Ok(true)
}

pub(super) fn has_pending_for_request(
    journal: &JournalStore,
    request: &kernel::ApprovalRequestedPayload,
) -> Result<bool, Error> {
    let Some(source) = request.source.as_deref() else {
        return Ok(false);
    };
    let parts: Vec<_> = source.split(':').collect();
    if request.tool != super::WORKSPACE_MULTI_PATCH_TOOL
        || parts.len() != 5
        || parts[0] != "multi-edit"
        || parts[1] != "v1"
        || parts[2].len() != 32
        || !parts[2]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Ok(false);
    }
    Ok(safe_bytes(
        &journal
            .directory()
            .join(format!("hook-{}.pending", parts[2])),
    )?
    .is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use workspace::multi_edit_recovery::{FileImage, RecoveryEntry, RecoveryState};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Temp(std::path::PathBuf);
    impl Temp {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "rapid-hook-checkpoint-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
            Self(root)
        }
        fn lock(&self) -> JournalStore {
            JournalStore::lock(&self.0).unwrap()
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn args() -> &'static str {
        r#"{"files":[{"path":"a","edits":[{"old":"a0","new":"a1"}]}]}"#
    }
    fn raw() -> RecoveryRecord {
        RecoveryRecord::new(
            RecoveryState::Intent,
            vec![RecoveryEntry {
                path: "a".into(),
                before: Some(FileImage {
                    bytes: b"a0".to_vec(),
                    mode: Some(0o600),
                }),
                after: Some(FileImage {
                    bytes: b"a1".to_vec(),
                    mode: Some(0o600),
                }),
            }],
        )
        .unwrap()
    }
    fn configured(root: &Temp) -> Vec<crate::hooks::HookEntry> {
        let script = root.0.join("hook.sh");
        let command = format!("printf x >> '{}'\n", root.0.join("hook-count").display());
        let result = r#"printf '%s\n' '{"decision":"ask","reason":"review","additional_context":"context","updated_input":{"files":[{"path":"b","create":"new"}]}}'
"#;
        fs::write(&script, format!("{command}{result}")).unwrap();
        vec![crate::hooks::HookEntry::plain(format!(
            "sh {}",
            script.display()
        ))]
    }
    #[test]
    fn real_hook_result_survives_reopen_without_reexecution() {
        let root = Temp::new();
        let hooks = configured(&root);
        let original = raw();
        let chain = PathApprovalChain::new(&original, "call", args()).unwrap();
        {
            let journal = root.lock();
            let checkpoint =
                HookCheckpoint::new(&journal, &chain, &original, args(), &hooks).unwrap();
            assert_eq!(checkpoint.load(), Ok(State::Missing));
            checkpoint.start().unwrap();
            assert_eq!(checkpoint.start(), Err(Error::Exists));
        }
        let report = crate::hooks::run_pre_tool_stage(
            &hooks,
            super::super::WORKSPACE_MULTI_PATCH_TOOL,
            args(),
            crate::hooks::HOOK_TIMEOUT,
        );
        assert!(matches!(
            report.outcome,
            crate::hooks::PreHookOutcome::Ask { .. }
        ));
        assert!(report.rewrite.is_some());
        {
            let journal = root.lock();
            let checkpoint =
                HookCheckpoint::new(&journal, &chain, &original, args(), &hooks).unwrap();
            assert_eq!(checkpoint.load(), Ok(State::Running));
            checkpoint.finish(&report).unwrap();
        }
        let journal = root.lock();
        let checkpoint = HookCheckpoint::new(&journal, &chain, &original, args(), &hooks).unwrap();
        assert_eq!(
            checkpoint.load(),
            Ok(State::Ready(Box::new(report.clone())))
        );
        let binding = checkpoint.approval_binding(&report).unwrap();
        assert_eq!(binding.len(), 64);
        assert_eq!(checkpoint.approval_binding(&report).unwrap(), binding);
        let mut altered = report.clone();
        altered.context[0].text.push_str(" changed");
        assert_eq!(checkpoint.approval_binding(&altered), Err(Error::Invalid));
        assert_eq!(checkpoint.start(), Err(Error::Exists));
        assert_eq!(fs::read(root.0.join("hook-count")).unwrap(), b"x");
        let mut changed = hooks.clone();
        changed[0].command.push_str(" # changed");
        let changed_checkpoint =
            HookCheckpoint::new(&journal, &chain, &original, args(), &changed).unwrap();
        assert_eq!(changed_checkpoint.load(), Err(Error::Invalid));
        assert_eq!(
            changed_checkpoint.approval_binding(&report),
            Err(Error::Invalid)
        );
        let saved = fs::read(checkpoint.path()).unwrap();
        let mut tampered: serde_json::Value = serde_json::from_slice(&saved).unwrap();
        tampered["report"]["context"][0]["text"] = serde_json::json!("changed");
        fs::write(checkpoint.path(), serde_json::to_vec(&tampered).unwrap()).unwrap();
        assert_eq!(checkpoint.load(), Err(Error::Invalid));
        fs::write(checkpoint.path(), saved).unwrap();
        checkpoint.retire(&report).unwrap();
        assert_eq!(checkpoint.load(), Ok(State::Retired));
        assert_eq!(checkpoint.approval_binding(&report), Err(Error::Invalid));
        assert_eq!(checkpoint.start(), Err(Error::Exists));
    }
    #[test]
    fn denied_cleanup_requires_the_exact_bound_preview_and_retires_content() {
        let root = Temp::new();
        let hooks = configured(&root);
        let original = raw();
        let chain = PathApprovalChain::new(&original, "call", args()).unwrap();
        let journal = root.lock();
        let checkpoint = HookCheckpoint::new(&journal, &chain, &original, args(), &hooks).unwrap();
        checkpoint.start().unwrap();
        let report = crate::hooks::run_pre_tool_stage(
            &hooks,
            super::super::WORKSPACE_MULTI_PATCH_TOOL,
            args(),
            crate::hooks::HOOK_TIMEOUT,
        );
        checkpoint.finish(&report).unwrap();
        let request = chain.request(&original, 0, args()).unwrap();
        checkpoint.bind_pending(&report, &request).unwrap();
        let mut payload: kernel::ApprovalRequestedPayload =
            serde_json::from_value(serde_json::json!({
                "id":"wait", "call_id":request.call_id, "tool":request.tool,
                "summary":request.summary, "scope":request.scope, "diff":request.diff,
                "source":request.source, "arguments_digest":request.arguments_digest
            }))
            .unwrap();
        payload.diff.push_str("tampered");
        assert_eq!(retire_denied(&journal, &payload, args()), Ok(false));
        assert_eq!(
            checkpoint.load(),
            Ok(State::Ready(Box::new(report.clone())))
        );
        payload.diff = request.diff.clone();
        assert_eq!(retire_denied(&journal, &payload, "different"), Ok(false));
        let pending_temp = journal
            .directory()
            .join(format!(".hook-{}.pending.tmp", &checkpoint.key[..32]));
        fs::write(&pending_temp, b"partial").unwrap();
        assert_eq!(
            retire_denied(&journal, &payload, args()),
            Err(Error::Uncertain)
        );
        fs::remove_file(pending_temp).unwrap();
        assert_eq!(retire_denied(&journal, &payload, args()), Ok(true));
        assert_eq!(checkpoint.load(), Ok(State::Retired));
        assert_eq!(retire_denied(&journal, &payload, args()), Ok(false));
        assert_eq!(fs::read(checkpoint.path()).unwrap(), RETIRED);
    }

    #[test]
    fn invalid_hook_report_never_replaces_running_marker() {
        let root = Temp::new();
        let hooks = configured(&root);
        let original = raw();
        let chain = PathApprovalChain::new(&original, "call", args()).unwrap();
        let journal = root.lock();
        let checkpoint = HookCheckpoint::new(&journal, &chain, &original, args(), &hooks).unwrap();
        checkpoint.start().unwrap();
        let report = crate::hooks::run_pre_tool_stage(
            &hooks,
            super::super::WORKSPACE_MULTI_PATCH_TOOL,
            args(),
            crate::hooks::HOOK_TIMEOUT,
        );
        let mut invalid = report.clone();
        invalid.decisions[0].command_digest = "000000000000".into();
        assert_eq!(checkpoint.finish(&invalid), Err(Error::Invalid));
        assert_eq!(checkpoint.load(), Ok(State::Running));
        invalid = report.clone();
        invalid.context[0].text = "x".repeat(protocol::MAX_HOOK_CONTEXT_BYTES + 1);
        assert_eq!(checkpoint.finish(&invalid), Err(Error::Invalid));
        assert_eq!(checkpoint.load(), Ok(State::Running));
        fs::write(checkpoint.temporary(), b"partial").unwrap();
        assert_eq!(checkpoint.load(), Err(Error::Uncertain));
        assert_eq!(checkpoint.start(), Err(Error::Uncertain));
    }
    #[test]
    fn symlink_and_public_checkpoint_files_fail_closed() {
        let root = Temp::new();
        let hooks = configured(&root);
        let original = raw();
        let chain = PathApprovalChain::new(&original, "call", args()).unwrap();
        let journal = root.lock();
        let checkpoint = HookCheckpoint::new(&journal, &chain, &original, args(), &hooks).unwrap();
        std::os::unix::fs::symlink("missing", checkpoint.path()).unwrap();
        assert_eq!(checkpoint.load(), Err(Error::Invalid));
        assert_eq!(checkpoint.start(), Err(Error::Exists));
        fs::remove_file(checkpoint.path()).unwrap();
        fs::write(checkpoint.path(), RUNNING).unwrap();
        fs::set_permissions(checkpoint.path(), fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(checkpoint.load(), Err(Error::Invalid));
    }

    #[test]
    fn hook_checkpoint_crash_child() {
        let Some(root) = std::env::var_os("RAPID_TEST_HOOK_CHECKPOINT_ROOT") else {
            return;
        };
        let root = Temp(std::path::PathBuf::from(root));
        let hooks = configured(&root);
        let original = raw();
        let approved = crate::approvals::ApprovedAsk {
            source: std::env::var("RAPID_TEST_HOOK_CHAIN_SOURCE").ok(),
            arguments_digest: Some(crate::approvals::arguments_digest(args())),
        };
        let chain = PathApprovalChain::resume(&original, "call", args(), &approved).unwrap();
        let journal = root.lock();
        let checkpoint = HookCheckpoint::new(&journal, &chain, &original, args(), &hooks).unwrap();
        checkpoint.start().unwrap();
        fs::write(root.0.join("child-ready"), b"synced").unwrap();
        std::thread::sleep(std::time::Duration::from_secs(60));
        panic!("hook checkpoint child was not killed");
    }

    #[test]
    fn killed_hook_checkpoint_process_remains_uncertain_on_reopen() {
        let root = Temp::new();
        let hooks = configured(&root);
        let original = raw();
        let chain = PathApprovalChain::new(&original, "call", args()).unwrap();
        let source = chain.request(&original, 0, args()).unwrap().source.unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("exec_tools::multi_hook_checkpoint::tests::hook_checkpoint_crash_child")
            .env("RAPID_TEST_HOOK_CHECKPOINT_ROOT", &root.0)
            .env("RAPID_TEST_HOOK_CHAIN_SOURCE", source)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !root.0.join("child-ready").exists() {
            if let Some(status) = child.try_wait().unwrap() {
                panic!("hook checkpoint child exited: {status}");
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("hook checkpoint child did not start");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        child.kill().unwrap();
        child.wait().unwrap();
        let journal = root.lock();
        let checkpoint = HookCheckpoint::new(&journal, &chain, &original, args(), &hooks).unwrap();
        assert_eq!(checkpoint.load(), Ok(State::Running));
        assert_eq!(checkpoint.start(), Err(Error::Exists));
        assert!(!root.0.join("hook-count").exists());
    }
}
