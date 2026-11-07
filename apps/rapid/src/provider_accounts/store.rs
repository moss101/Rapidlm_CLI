//! Locked account metadata with keychain-only token bundles and crash recovery.
//! Refresh intent is durable before a grant. A staged bundle can be recovered;
//! an interrupted grant without one stays explicit for the manager to retry under
//! the same exclusive lock and original issuer/client binding.
use super::{AccountError as Error, AccountSpec, MAX_RECORD_BYTES, protocol::Tokens};
use crate::mcp_oauth::Secret;
use auth::{CredentialKind, KeychainItemMeta, PlatformKeychain, SecretRef};
use llm_router::CancellationToken;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

const CHUNK_BYTES: usize = 1024;
const MAX_CHUNKS: usize = MAX_RECORD_BYTES / CHUNK_BYTES;
/// The journal is bounded; rotations stop before another remote grant if
/// persistent keychain failures have filled the cleanup backlog.
const MAX_RETIRED: usize = 64;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    len: usize,
    chunks: usize,
    sha256: String,
}
impl Manifest {
    fn parse(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > 512 {
            return Err(Error::Storage);
        }
        let manifest: Self = serde_json::from_slice(bytes).map_err(|_| Error::Storage)?;
        if manifest.len == 0
            || manifest.len > MAX_RECORD_BYTES
            || manifest.chunks != manifest.len.div_ceil(CHUNK_BYTES)
            || manifest.sha256.len() != 64
            || !manifest.sha256.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(Error::Storage);
        }
        Ok(manifest)
    }
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
struct CriticalCommit<'a> {
    flag: &'a Cell<bool>,
}
impl<'a> CriticalCommit<'a> {
    fn new(flag: &'a Cell<bool>) -> Self {
        flag.set(true);
        Self { flag }
    }
}
impl Drop for CriticalCommit<'_> {
    fn drop(&mut self) {
        self.flag.set(false);
    }
}

const SCHEMA: &str = "rapidlm.provider_account.v1";
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum AccountState {
    Active {
        handle: String,
    },
    Refreshing {
        current: String,
        pending: String,
    },
    /// A durable tombstone retaining only keychain handles until deletion succeeds.
    Revoking {
        retired: Vec<String>,
    },
    Revoked,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountRecord {
    schema: String,
    pub account: String,
    pub provider: String,
    pub binding: String,
    pub generation: u64,
    pub state: AccountState,
    /// Superseded handles awaiting deletion. No token bytes enter this record.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retired: Vec<String>,
    /// A replacement login's reserved handle, committed before any keychain write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Bundle {
    schema: String,
    binding: String,
    generation: u64,
    access: String,
    refresh: Option<String>,
    obtained_at: u64,
    expires_at: Option<u64>,
    scopes: Vec<String>,
}
impl Drop for Bundle {
    fn drop(&mut self) {
        wipe(&mut self.access);
        if let Some(refresh) = &mut self.refresh {
            wipe(refresh);
        }
    }
}
fn wipe(text: &mut String) {
    let mut bytes = std::mem::take(text).into_bytes();
    bytes.fill(0);
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}
#[derive(Clone)]
pub struct AccountStore {
    root: PathBuf,
    backend: Arc<dyn PlatformKeychain>,
}
impl std::fmt::Debug for AccountStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AccountStore(<private account store>)")
    }
}
pub struct Transaction<'a> {
    store: &'a AccountStore,
    spec: &'a AccountSpec,
    _lock: File,
    cancel: &'a CancellationToken,
    critical: Cell<bool>,
}
impl AccountStore {
    pub fn new(root: PathBuf, backend: Arc<dyn PlatformKeychain>) -> Self {
        Self { root, backend }
    }
    /// Probe without reading or creating a credential.
    pub fn available(&self) -> bool {
        self.backend.probe().is_available()
    }
    pub fn transaction<'a>(
        &'a self,
        spec: &'a AccountSpec,
        cancel: &'a CancellationToken,
    ) -> Result<Transaction<'a>, Error> {
        cancel.check().map_err(|_| Error::Cancelled)?;
        private_dir(&self.root)?;
        let path = self.root.join(format!("{}.lock", spec.id()));
        reject_symlink(&path)?;
        let file = private_open(&path, false)?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            cancel.check().map_err(|_| Error::Cancelled)?;
            match file.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        return Err(Error::Busy);
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => return Err(Error::Storage),
            }
        }
        Ok(Transaction {
            store: self,
            spec,
            _lock: file,
            cancel,
            critical: Cell::new(false),
        })
    }
    fn record_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.json"))
    }
}
impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        let _ = self._lock.unlock();
    }
}
impl Transaction<'_> {
    fn check(&self) -> Result<(), Error> {
        if self.critical.get() {
            Ok(())
        } else {
            self.cancel.check().map_err(|_| Error::Cancelled)
        }
    }
    /// Establish a durable revision before waiting for an interactive login.
    /// A concurrent logout can then invalidate even the first login attempt.
    pub fn login_revision(&self) -> Result<u64, Error> {
        if let Some(record) = self.record()? {
            return Ok(record.generation);
        }
        let record = AccountRecord {
            schema: SCHEMA.to_owned(),
            account: self.spec.id().to_owned(),
            provider: self.spec.definition().provider.clone(),
            binding: self.spec.binding().to_owned(),
            generation: 0,
            state: AccountState::Revoked,
            retired: Vec::new(),
            staged: None,
        };
        self.write_record(&record)?;
        Ok(record.generation)
    }
    /// Load a device identity provisioned during an explicit Kimi login.
    /// The transaction lock prevents readers from observing a partial write.
    pub fn kimi_identity(&self, create: bool) -> Result<super::net::KimiDeviceIdentity, Error> {
        if self.spec.definition().kind != super::AccountKind::Kimi {
            return Err(Error::InvalidConfig);
        }
        self.check()?;
        let path = self.store.root.join(format!(
            "{}.{}.device.json",
            self.spec.id(),
            self.spec.binding()
        ));
        reject_symlink(&path)?;
        #[derive(Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Identity {
            binding: String,
            device_id: String,
        }
        let read = || -> Result<Identity, Error> {
            let mut bytes = Vec::new();
            File::open(&path)
                .map_err(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        Error::Missing
                    } else {
                        Error::Storage
                    }
                })?
                .take(2049)
                .read_to_end(&mut bytes)
                .map_err(|_| Error::Storage)?;
            if bytes.len() > 2048 {
                return Err(Error::Storage);
            }
            serde_json::from_slice(&bytes).map_err(|_| Error::Storage)
        };
        let identity = match read() {
            Ok(identity) => identity,
            Err(Error::Missing) if create => {
                let identity = Identity {
                    binding: self.spec.binding().to_owned(),
                    device_id: crate::mcp_oauth::random_token(24).map_err(|_| Error::Storage)?,
                };
                let mut file = private_open(&path, true)?;
                let bytes = serde_json::to_vec(&identity).map_err(|_| Error::Storage)?;
                file.write_all(&bytes)
                    .and_then(|_| file.sync_all())
                    .map_err(|_| Error::Storage)?;
                #[cfg(unix)]
                File::open(&self.store.root)
                    .and_then(|dir| dir.sync_all())
                    .map_err(|_| Error::Storage)?;
                identity
            }
            Err(error) => return Err(error),
        };
        if identity.binding != self.spec.binding() {
            return Err(Error::BindingMismatch);
        }
        super::net::KimiDeviceIdentity::new(
            identity.device_id,
            "RapidLM".to_owned(),
            std::env::consts::ARCH.to_owned(),
            std::env::consts::OS.to_owned(),
        )
    }
    pub fn record(&self) -> Result<Option<AccountRecord>, Error> {
        self.check()?;
        let path = self.store.record_path(self.spec.id());
        reject_symlink(&path)?;
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(Error::Storage),
        };
        let mut bytes = Vec::new();
        file.take((MAX_RECORD_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::Storage)?;
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(Error::Storage);
        }
        let record: AccountRecord = serde_json::from_slice(&bytes).map_err(|_| Error::Storage)?;
        if record.schema != SCHEMA
            || record.account != self.spec.id()
            || record.provider != self.spec.definition().provider
            || record.binding != self.spec.binding()
        {
            return Err(Error::BindingMismatch);
        }
        if record.retired.len() > MAX_RETIRED {
            return Err(Error::Storage);
        }
        self.validate_retired(&record.retired, record.generation, false)?;
        if let Some(staged) = &record.staged {
            let step = if matches!(record.state, AccountState::Refreshing { .. }) {
                2
            } else {
                1
            };
            if matches!(record.state, AccountState::Revoking { .. })
                || staged
                    != &self.handle(record.generation.checked_add(step).ok_or(Error::Storage)?)
                || record.retired.contains(staged)
            {
                return Err(Error::BindingMismatch);
            }
        }
        match &record.state {
            AccountState::Active { handle } => {
                self.check_handle(handle, record.generation)?;
                if record.retired.contains(handle) {
                    return Err(Error::BindingMismatch);
                }
            }
            AccountState::Refreshing { current, pending } => {
                self.check_handle(
                    current,
                    record.generation.checked_sub(1).ok_or(Error::Storage)?,
                )?;
                self.check_handle(
                    pending,
                    record.generation.checked_add(1).ok_or(Error::Storage)?,
                )?;
                if record.retired.contains(current) || record.retired.contains(pending) {
                    return Err(Error::BindingMismatch);
                }
            }
            AccountState::Revoking { retired } => {
                if retired.is_empty()
                    || retired.len() > MAX_RETIRED + 3
                    || !record.retired.is_empty()
                {
                    return Err(Error::Storage);
                }
                self.validate_retired(retired, record.generation, true)?;
            }
            AccountState::Revoked if !record.retired.is_empty() => return Err(Error::Storage),
            AccountState::Revoked => {}
        }
        Ok(Some(record))
    }
    /// Load the current durable winner; recover a committed keychain bundle left
    /// behind by a crash before the metadata rename. Never adopt a foreign scope.
    pub fn load(&self) -> Result<(AccountRecord, Tokens), Error> {
        let mut record = self.record()?.ok_or(Error::Missing)?;
        self.sweep_staged(&mut record)?;
        match record.state.clone() {
            AccountState::Revoked | AccountState::Revoking { .. } => Err(Error::Revoked),
            AccountState::Active { handle } => {
                let tokens = self.read_bundle(&handle, record.generation)?;
                self.sweep_retired(&mut record)?;
                Ok((record, tokens))
            }
            AccountState::Refreshing { current, pending } => {
                let generation = record.generation.checked_add(1).ok_or(Error::Storage)?;
                let tokens = self.read_bundle(&pending, generation).map_err(|e| {
                    if e == Error::Missing {
                        Error::Interrupted
                    } else {
                        e
                    }
                })?;
                record.generation = generation;
                record.state = AccountState::Active { handle: pending };
                if record.retired.len() >= MAX_RETIRED {
                    return Err(Error::Storage);
                }
                record.retired.push(current);
                self.write_record(&record)?;
                self.sweep_retired(&mut record)?;
                Ok((record, tokens))
            }
        }
    }
    /// The manager may retry an interrupted refresh only while holding this
    /// transaction and matching the original durable intent. A newer winner or
    /// tombstone prevents the retry; rejection invalidates the old credential.
    pub fn interrupted_tokens(&self, intent: &AccountRecord) -> Result<Tokens, Error> {
        if self.record()?.as_ref() != Some(intent) {
            return Err(Error::Stale);
        }
        let AccountState::Refreshing { current, .. } = &intent.state else {
            return Err(Error::Stale);
        };
        self.read_bundle(
            current,
            intent.generation.checked_sub(1).ok_or(Error::Storage)?,
        )
    }
    /// Install tokens only if no login/revoke/rotation changed the revision while
    /// the user was in the browser. Callers capture this revision before waiting.
    pub fn install(&self, expected: Option<u64>, tokens: &Tokens) -> Result<AccountRecord, Error> {
        let mut previous = self.record()?;
        if previous.as_ref().map(|r| r.generation) != expected {
            return Err(Error::Stale);
        }
        if previous
            .as_ref()
            .is_some_and(|record| matches!(record.state, AccountState::Revoking { .. }))
        {
            previous = Some(self.retry_revocation()?);
        }
        if let Some(previous) = &mut previous {
            self.sweep_staged(previous)?;
            if previous.staged.is_some() {
                return Err(Error::Keychain);
            }
            self.sweep_retired(previous)?;
            let additional = match previous.state {
                AccountState::Active { .. } => 1,
                AccountState::Refreshing { .. } => 2,
                AccountState::Revoked => 0,
                AccountState::Revoking { .. } => return Err(Error::Storage),
            };
            if previous.retired.len() + additional > MAX_RETIRED {
                return Err(Error::Keychain);
            }
        }
        // A refresh intent already reserved the next generation's handle.
        // A replacement login must not overwrite it, even after a crash.
        let step = if previous
            .as_ref()
            .is_some_and(|r| matches!(r.state, AccountState::Refreshing { .. }))
        {
            2
        } else {
            1
        };
        let generation = expected
            .unwrap_or(0)
            .checked_add(step)
            .ok_or(Error::Storage)?;
        let handle = self.handle(generation);
        // Reserve the future alias durably before writing any secret. A crash
        // or failed keychain cleanup can then be retried without losing it.
        let mut journal = previous.clone().unwrap_or_else(|| AccountRecord {
            schema: SCHEMA.to_owned(),
            account: self.spec.id().to_owned(),
            provider: self.spec.definition().provider.clone(),
            binding: self.spec.binding().to_owned(),
            generation: 0,
            state: AccountState::Revoked,
            retired: Vec::new(),
            staged: None,
        });
        journal.staged = Some(handle.clone());
        self.write_record(&journal)?;
        self.write_bundle(&handle, generation, tokens)?;
        let mut retired = previous
            .as_ref()
            .map_or_else(Vec::new, |record| record.retired.clone());
        if let Some(previous) = &previous {
            match &previous.state {
                AccountState::Active { handle } => retired.push(handle.clone()),
                AccountState::Refreshing { current, pending } => {
                    retired.push(current.clone());
                    retired.push(pending.clone());
                }
                AccountState::Revoked => {}
                AccountState::Revoking { .. } => unreachable!(),
            }
        }
        let mut record = AccountRecord {
            schema: SCHEMA.to_owned(),
            account: self.spec.id().to_owned(),
            provider: self.spec.definition().provider.clone(),
            binding: self.spec.binding().to_owned(),
            generation,
            state: AccountState::Active {
                handle: handle.clone(),
            },
            retired,
            staged: None,
        };
        self.write_record(&record)?;
        self.sweep_retired(&mut record)?;
        Ok(record)
    }
    /// Persist intent before contacting a single-use refresh endpoint. The
    /// transaction must remain held across grant, keychain save and final commit.
    pub fn begin_refresh(&self, expected: u64) -> Result<AccountRecord, Error> {
        let mut record = self.record()?.ok_or(Error::Missing)?;
        if record.generation != expected {
            return Err(Error::Stale);
        }
        self.sweep_staged(&mut record)?;
        if record.staged.is_some() {
            return Err(Error::Keychain);
        }
        self.sweep_retired(&mut record)?;
        if record.retired.len() >= MAX_RETIRED {
            return Err(Error::Keychain);
        }
        let AccountState::Active { handle } = record.state else {
            return Err(Error::Stale);
        };
        record.generation = expected.checked_add(1).ok_or(Error::Storage)?;
        let pending = self.handle(record.generation.checked_add(1).ok_or(Error::Storage)?);
        record.state = AccountState::Refreshing {
            current: handle,
            pending,
        };
        self.write_record(&record)?;
        Ok(record)
    }
    pub fn finish_refresh(
        &self,
        intent: &AccountRecord,
        tokens: &Tokens,
    ) -> Result<AccountRecord, Error> {
        // A received rotation must reach durable storage even if its consumer
        // cancelled. The caller checks cancellation before releasing the token.
        let _critical = CriticalCommit::new(&self.critical);
        if self.record()?.as_ref() != Some(intent) {
            return Err(Error::Stale);
        }
        let AccountState::Refreshing { current, pending } = &intent.state else {
            return Err(Error::Stale);
        };
        let generation = intent.generation.checked_add(1).ok_or(Error::Storage)?;
        self.write_bundle(pending, generation, tokens)?;
        let mut next = intent.clone();
        next.generation = generation;
        next.state = AccountState::Active {
            handle: pending.clone(),
        };
        if next.retired.len() >= MAX_RETIRED {
            return Err(Error::Storage);
        }
        next.retired.push(current.clone());
        // If this rename fails, leave the intent and staged bundle recoverable.
        self.write_record(&next)?;
        self.sweep_retired(&mut next)?;
        Ok(next)
    }
    /// Safe only when no new token was received (e.g. an explicitly rejected
    /// request). Transport-uncertain outcomes retain the journal for inspection.
    pub fn restore_refresh(&self, intent: &AccountRecord) -> Result<(), Error> {
        let AccountState::Refreshing { current, .. } = &intent.state else {
            return Err(Error::Stale);
        };
        let prior_generation = intent.generation.checked_sub(1).ok_or(Error::Storage)?;
        let tokens = self.read_bundle(current, prior_generation)?;
        self.finish_refresh(intent, &tokens).map(|_| ())
    }
    /// An explicit logout still revokes locally after its remote request was
    /// cancelled or failed. It never releases the retired token to a model.
    pub fn revoke_after_remote(&self, expected: u64) -> Result<AccountRecord, Error> {
        let _critical = CriticalCommit::new(&self.critical);
        self.revoke(expected)
    }
    /// Tombstone before deleting secrets. A failed keychain deletion never makes
    /// a revoked account usable again, including after a fresh process starts.
    pub fn revoke(&self, expected: u64) -> Result<AccountRecord, Error> {
        let mut record = self.record()?.ok_or(Error::Missing)?;
        if record.generation != expected {
            return Err(Error::Stale);
        }
        if matches!(record.state, AccountState::Revoking { .. }) {
            record.generation = record.generation.checked_add(1).ok_or(Error::Storage)?;
            self.write_record(&record)?;
            return self.retry_revocation();
        }
        if matches!(record.state, AccountState::Revoked) {
            record.generation = record.generation.checked_add(1).ok_or(Error::Storage)?;
            if let Some(staged) = record.staged.take() {
                record.state = AccountState::Revoking {
                    retired: vec![staged],
                };
                self.write_record(&record)?;
                return self.retry_revocation();
            }
            self.write_record(&record)?;
            return Ok(record);
        }
        let mut retired = std::mem::take(&mut record.retired);
        let staged = record.staged.take();
        let step = if staged.is_some() && matches!(record.state, AccountState::Refreshing { .. }) {
            2
        } else {
            1
        };
        match &record.state {
            AccountState::Active { handle } => retired.push(handle.clone()),
            AccountState::Refreshing { current, pending } => {
                retired.push(current.clone());
                retired.push(pending.clone());
            }
            _ => unreachable!(),
        }
        if let Some(staged) = staged {
            retired.push(staged);
        }
        record.generation = record.generation.checked_add(step).ok_or(Error::Storage)?;
        record.state = AccountState::Revoking { retired };
        self.write_record(&record)?;
        self.retry_revocation()
    }
    /// Retry a previously committed tombstone without restoring account access.
    pub fn retry_revocation(&self) -> Result<AccountRecord, Error> {
        let mut record = self.record()?.ok_or(Error::Missing)?;
        if matches!(record.state, AccountState::Revoked) {
            self.sweep_staged(&mut record)?;
            return if record.staged.is_none() {
                Ok(record)
            } else {
                Err(Error::Keychain)
            };
        }
        if !matches!(record.state, AccountState::Revoking { .. }) {
            return Err(Error::Stale);
        }
        if !self.cleanup(&record.state) {
            return Err(Error::Keychain);
        }
        record.state = AccountState::Revoked;
        self.write_record(&record)?;
        Ok(record)
    }
    fn handle(&self, generation: u64) -> String {
        format!(
            "provider-oauth:{}:{}:{generation}",
            self.spec.id(),
            self.spec.binding()
        )
    }
    fn check_handle(&self, handle: &str, generation: u64) -> Result<(), Error> {
        if handle == self.handle(generation) {
            Ok(())
        } else {
            Err(Error::BindingMismatch)
        }
    }
    fn validate_retired(
        &self,
        handles: &[String],
        generation: u64,
        allow_equal: bool,
    ) -> Result<(), Error> {
        let mut seen = std::collections::BTreeSet::new();
        let prefix = format!("provider-oauth:{}:{}:", self.spec.id(), self.spec.binding());
        for handle in handles {
            let number = handle
                .strip_prefix(&prefix)
                .and_then(|text| text.parse::<u64>().ok())
                .ok_or(Error::BindingMismatch)?;
            if number == 0
                || number > generation
                || (!allow_equal && number == generation)
                || handle != &self.handle(number)
                || !seen.insert(handle)
            {
                return Err(Error::BindingMismatch);
            }
        }
        Ok(())
    }
    /// Keep failed entries durable; clear successful deletions only after a
    /// second atomic metadata commit. Repeated deletion is idempotent.
    fn sweep_retired(&self, record: &mut AccountRecord) -> Result<(), Error> {
        if record.retired.is_empty() {
            return Ok(());
        }
        let mut next = record.clone();
        next.retired.retain(|handle| !self.delete_bundle(handle));
        if next.retired != record.retired {
            self.write_record(&next)?;
            *record = next;
        }
        Ok(())
    }
    fn sweep_staged(&self, record: &mut AccountRecord) -> Result<(), Error> {
        let Some(handle) = &record.staged else {
            return Ok(());
        };
        if self.delete_bundle(handle) {
            let mut next = record.clone();
            next.staged = None;
            self.write_record(&next)?;
            *record = next;
        }
        Ok(())
    }
    fn item(&self, handle: &str) -> Result<KeychainItemMeta, Error> {
        KeychainItemMeta::new(
            SecretRef::from_alias(handle).map_err(|_| Error::Storage)?,
            CredentialKind::OauthRefresh,
            Some(self.spec.definition().provider.clone()),
        )
        .map_err(|_| Error::Storage)
    }
    fn read_bundle(&self, handle: &str, generation: u64) -> Result<Tokens, Error> {
        self.check()?;
        if !self.store.backend.probe().is_available() {
            return Err(Error::Keychain);
        }
        let manifest_bytes = self
            .store
            .backend
            .get(&self.item(handle)?, &auth::CancellationToken::new())
            .map_err(|e| {
                if e == auth::StoreError::NotFound {
                    Error::Missing
                } else {
                    Error::Keychain
                }
            })?;
        let manifest = Manifest::parse(&manifest_bytes)?;
        let mut bytes = Vec::with_capacity(manifest.len);
        for index in 0..manifest.chunks {
            self.check()?;
            let mut chunk = self
                .store
                .backend
                .get(
                    &self.item(&format!("{handle}:chunk{index}"))?,
                    &auth::CancellationToken::new(),
                )
                .map_err(|_| Error::Keychain)?;
            if chunk.is_empty()
                || chunk.len() > CHUNK_BYTES
                || bytes.len() + chunk.len() > manifest.len
            {
                chunk.fill(0);
                bytes.fill(0);
                return Err(Error::Storage);
            }
            bytes.extend_from_slice(&chunk);
            chunk.fill(0);
        }
        if bytes.len() != manifest.len || digest(&bytes) != manifest.sha256 {
            bytes.fill(0);
            return Err(Error::Storage);
        }
        let parsed = serde_json::from_slice::<Bundle>(&bytes);
        bytes.fill(0);
        let bundle = parsed.map_err(|_| Error::Storage)?;
        if bundle.schema != SCHEMA
            || bundle.binding != self.spec.binding()
            || bundle.generation != generation
        {
            return Err(Error::BindingMismatch);
        }
        let tokens = Tokens {
            access: Secret::new(bundle.access.clone()),
            refresh: bundle.refresh.clone().map(Secret::new),
            obtained_at: bundle.obtained_at,
            expires_at: bundle.expires_at,
            scopes: bundle.scopes.clone(),
        };
        tokens.validate()?;
        Ok(tokens)
    }
    fn write_bundle(&self, handle: &str, generation: u64, tokens: &Tokens) -> Result<(), Error> {
        self.check()?;
        tokens.validate()?;
        if !self.store.backend.probe().is_available() {
            return Err(Error::Keychain);
        }
        let bundle = Bundle {
            schema: SCHEMA.to_owned(),
            binding: self.spec.binding().to_owned(),
            generation,
            access: tokens.access.expose().to_owned(),
            refresh: tokens.refresh.as_ref().map(|r| r.expose().to_owned()),
            obtained_at: tokens.obtained_at,
            expires_at: tokens.expires_at,
            scopes: tokens.scopes.clone(),
        };
        let mut bytes = serde_json::to_vec(&bundle).map_err(|_| Error::Storage)?;
        if bytes.len() > MAX_RECORD_BYTES {
            bytes.fill(0);
            return Err(Error::Storage);
        }
        let manifest = Manifest {
            len: bytes.len(),
            chunks: bytes.len().div_ceil(CHUNK_BYTES),
            sha256: digest(&bytes),
        };
        let mut written = 0;
        let result = (|| {
            for (index, chunk) in bytes.chunks(CHUNK_BYTES).enumerate() {
                self.check()?;
                self.store
                    .backend
                    .put(
                        &self.item(&format!("{handle}:chunk{index}"))?,
                        chunk,
                        &auth::CancellationToken::new(),
                    )
                    .map_err(|_| Error::Keychain)?;
                written = index + 1;
            }
            let manifest = serde_json::to_vec(&manifest).map_err(|_| Error::Storage)?;
            self.store
                .backend
                .put(
                    &self.item(handle)?,
                    &manifest,
                    &auth::CancellationToken::new(),
                )
                .map_err(|_| Error::Keychain)
        })();
        bytes.fill(0);
        if result.is_err() {
            // Include the item whose put may have partially succeeded.
            for index in 0..=(written.min(MAX_CHUNKS - 1)) {
                self.delete_item(&format!("{handle}:chunk{index}"));
            }
            self.delete_item(handle);
        }
        result
    }
    fn delete_item(&self, handle: &str) -> bool {
        self.item(handle).is_ok_and(|item| {
            matches!(
                self.store
                    .backend
                    .delete(&item, &auth::CancellationToken::new()),
                Ok(()) | Err(auth::StoreError::NotFound)
            )
        })
    }
    fn delete_bundle(&self, handle: &str) -> bool {
        if !self.store.backend.probe().is_available() {
            return false;
        }
        let count = self
            .item(handle)
            .ok()
            .and_then(|item| {
                self.store
                    .backend
                    .get(&item, &auth::CancellationToken::new())
                    .ok()
            })
            .and_then(|bytes| Manifest::parse(&bytes).ok())
            .map_or(MAX_CHUNKS, |m| m.chunks);
        let mut success = true;
        for index in 0..count {
            success &= self.delete_item(&format!("{handle}:chunk{index}"));
        }
        success &= self.delete_item(handle);
        success
    }
    fn cleanup(&self, state: &AccountState) -> bool {
        match state {
            AccountState::Active { handle } => self.delete_bundle(handle),
            AccountState::Refreshing { current, pending } => {
                let first = self.delete_bundle(current);
                self.delete_bundle(pending) && first
            }
            AccountState::Revoking { retired } => {
                let mut success = true;
                for handle in retired {
                    success &= self.delete_bundle(handle);
                }
                success
            }
            AccountState::Revoked => true,
        }
    }
    fn write_record(&self, record: &AccountRecord) -> Result<(), Error> {
        self.check()?;
        let bytes = serde_json::to_vec(record).map_err(|_| Error::Storage)?;
        let destination = self.store.record_path(self.spec.id());
        reject_symlink(&destination)?;
        let suffix = crate::mcp_oauth::random_token(12).map_err(|_| Error::Storage)?;
        let temporary = self
            .store
            .root
            .join(format!(".{}.{}.tmp", self.spec.id(), suffix));
        let result = (|| {
            let mut file = private_open(&temporary, true)?;
            file.write_all(&bytes).map_err(|_| Error::Storage)?;
            file.sync_all().map_err(|_| Error::Storage)?;
            std::fs::rename(&temporary, &destination).map_err(|_| Error::Storage)?;
            #[cfg(unix)]
            File::open(&self.store.root)
                .and_then(|dir| dir.sync_all())
                .map_err(|_| Error::Storage)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    }
}
fn reject_symlink(path: &Path) -> Result<(), Error> {
    match path.symlink_metadata() {
        Ok(meta) if meta.file_type().is_symlink() => Err(Error::Storage),
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(Error::Storage),
        _ => Ok(()),
    }
}
fn private_dir(path: &Path) -> Result<(), Error> {
    reject_symlink(path)?;
    std::fs::create_dir_all(path).map_err(|_| Error::Storage)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| Error::Storage)?;
    }
    Ok(())
}
fn private_open(path: &Path, exclusive: bool) -> Result<File, Error> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    if exclusive {
        options.create_new(true);
    } else {
        options.create(true).truncate(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(|_| Error::Storage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_accounts::tests::spec;
    use crate::provider_keychain::testing::MemoryKeychain;
    use std::sync::atomic::{AtomicUsize, Ordering};
    fn root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "rapid-provider-account-{name}-{}",
            ::protocol::SessionId::new()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }
    fn tokens(access: &str, refresh: &str) -> Tokens {
        Tokens {
            access: Secret::new(access.to_owned()),
            refresh: Some(Secret::new(refresh.to_owned())),
            obtained_at: 1000,
            expires_at: Some(4600),
            scopes: vec!["inference".to_owned()],
        }
    }
    #[test]
    fn account_store_keeps_large_tokens_only_in_bounded_keychain_chunks_across_restart_and_rotation()
     {
        let root = root("roundtrip");
        let backend = Arc::new(MemoryKeychain::default());
        let store = AccountStore::new(root.clone(), backend.clone());
        let spec = spec();
        let cancel = CancellationToken::new();
        let access = format!("private-access-canary{}", "a".repeat(7000));
        let initial = tokens(&access, "private-refresh-canary");
        let first = {
            let tx = store.transaction(&spec, &cancel).unwrap();
            assert!(tx.record().unwrap().is_none());
            tx.install(None, &initial).unwrap()
        };
        let raw = std::fs::read_to_string(store.record_path("work")).unwrap();
        assert!(!raw.contains("private-"));
        assert!(
            backend
                .items
                .lock()
                .unwrap()
                .values()
                .all(|bytes| bytes.len() <= 1024)
        );
        let reopened = AccountStore::new(root.clone(), backend.clone());
        let tx = reopened.transaction(&spec, &cancel).unwrap();
        let (record, read) = tx.load().unwrap();
        assert_eq!(record, first);
        assert_eq!(read.access.expose(), access);
        let intent = tx.begin_refresh(record.generation).unwrap();
        assert_eq!(intent.generation, 2);
        assert!(matches!(
            tx.install(Some(first.generation), &initial),
            Err(Error::Stale)
        ));
        let rotated = tokens("new-access-canary", "new-refresh-canary");
        let next = tx.finish_refresh(&intent, &rotated).unwrap();
        assert_eq!(next.generation, 3);
        assert_eq!(
            tx.load().unwrap().1.refresh.unwrap().expose(),
            "new-refresh-canary"
        );
        assert!(
            !backend
                .items
                .lock()
                .unwrap()
                .values()
                .any(|v| String::from_utf8_lossy(v).contains("private-refresh-canary"))
        );
        drop(tx);
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn account_refresh_journal_recovers_a_staged_bundle_and_refuses_an_unknown_inflight_grant() {
        let root = root("recovery");
        let backend = Arc::new(MemoryKeychain::default());
        let store = AccountStore::new(root.clone(), backend.clone());
        let spec = spec();
        let cancel = CancellationToken::new();
        let intent = {
            let tx = store.transaction(&spec, &cancel).unwrap();
            let active = tx
                .install(None, &tokens("before", "refresh-before"))
                .unwrap();
            tx.begin_refresh(active.generation).unwrap()
        };
        {
            let tx = store.transaction(&spec, &cancel).unwrap();
            assert!(matches!(tx.load(), Err(Error::Interrupted)));
            let AccountState::Refreshing { pending, .. } = &intent.state else {
                panic!("intent")
            };
            tx.write_bundle(
                pending,
                intent.generation + 1,
                &tokens("staged-access", "staged-refresh"),
            )
            .unwrap();
        }
        let reopened = AccountStore::new(root.clone(), backend);
        let tx = reopened.transaction(&spec, &cancel).unwrap();
        let (recovered, tokens) = tx.load().unwrap();
        assert_eq!(recovered.generation, 3);
        assert_eq!(tokens.access.expose(), "staged-access");
        assert!(matches!(recovered.state, AccountState::Active { .. }));
        assert!(matches!(
            tx.finish_refresh(&intent, &tokens),
            Err(Error::Stale)
        ));
        drop(tx);
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn account_login_cannot_overwrite_a_newer_refresh_or_revocation_and_relogin_does_not_reuse_pending_handles()
     {
        let root = root("stale-login");
        let backend = Arc::new(MemoryKeychain::default());
        let store = AccountStore::new(root.clone(), backend);
        let spec = spec();
        let cancel = CancellationToken::new();
        let tx = store.transaction(&spec, &cancel).unwrap();
        let first = tx.install(None, &tokens("a", "r")).unwrap();
        let intent = tx.begin_refresh(first.generation).unwrap();
        assert!(matches!(
            tx.install(Some(first.generation), &tokens("late", "late-r")),
            Err(Error::Stale)
        ));
        let login = tx
            .install(Some(intent.generation), &tokens("relogin", "new-r"))
            .unwrap();
        assert_eq!(login.generation, 4);
        assert_eq!(tx.load().unwrap().1.access.expose(), "relogin");
        let revoked = tx.revoke(login.generation).unwrap();
        assert!(matches!(revoked.state, AccountState::Revoked));
        assert!(matches!(tx.load(), Err(Error::Revoked)));
        assert!(matches!(
            tx.install(Some(login.generation), &tokens("late", "late-r")),
            Err(Error::Stale)
        ));
        drop(tx);
        let reopened = store.transaction(&spec, &cancel).unwrap();
        assert!(matches!(reopened.load(), Err(Error::Revoked)));
        drop(reopened);
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn account_received_rotation_commits_after_cancellation_without_releasing_it_to_cancelled_readers()
     {
        let root = root("cancel-commit");
        let backend = Arc::new(MemoryKeychain::default());
        let store = AccountStore::new(root.clone(), backend);
        let spec = spec();
        let cancel = CancellationToken::new();
        {
            let tx = store.transaction(&spec, &cancel).unwrap();
            let first = tx.install(None, &tokens("old", "old-r")).unwrap();
            let intent = tx.begin_refresh(first.generation).unwrap();
            cancel.cancel();
            tx.finish_refresh(&intent, &tokens("rotated", "rotated-r"))
                .unwrap();
            assert!(matches!(tx.load(), Err(Error::Cancelled)));
        }
        let fresh = CancellationToken::new();
        let tx = store.transaction(&spec, &fresh).unwrap();
        assert_eq!(tx.load().unwrap().1.access.expose(), "rotated");
        drop(tx);
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn account_failed_keychain_writes_preserve_the_prior_winner_and_failed_deletions_stay_revoked()
    {
        let root = root("keychain-faults");
        let backend = Arc::new(MemoryKeychain::default());
        let store = AccountStore::new(root.clone(), backend.clone());
        let spec = spec();
        let cancel = CancellationToken::new();
        let tx = store.transaction(&spec, &cancel).unwrap();
        let first = tx
            .install(None, &tokens("old-access", "old-refresh"))
            .unwrap();
        *backend.failing_puts.lock().unwrap() = 1;
        assert!(matches!(
            tx.install(Some(first.generation), &tokens("new-access", "new-refresh")),
            Err(Error::Keychain)
        ));
        assert_eq!(tx.load().unwrap().1.access.expose(), "old-access");
        *backend.failing_deletes.lock().unwrap() = 100;
        assert!(matches!(tx.revoke(first.generation), Err(Error::Keychain)));
        assert!(matches!(tx.load(), Err(Error::Revoked)));
        drop(tx);
        let tx = store.transaction(&spec, &cancel).unwrap();
        assert!(matches!(tx.load(), Err(Error::Revoked)));
        assert!(matches!(
            tx.record().unwrap().unwrap().state,
            AccountState::Revoking { .. }
        ));
        *backend.failing_deletes.lock().unwrap() = 0;
        assert!(matches!(
            tx.retry_revocation().unwrap().state,
            AccountState::Revoked
        ));
        assert!(backend.items.lock().unwrap().is_empty());
        assert!(matches!(tx.load(), Err(Error::Revoked)));
        drop(tx);
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn retired_bundles_survive_failed_login_and_refresh_cleanup_then_are_retried_after_restart() {
        let root = root("retired-restart");
        let backend = Arc::new(MemoryKeychain::default());
        let store = AccountStore::new(root.clone(), backend.clone());
        let spec = spec();
        let cancel = CancellationToken::new();
        {
            let tx = store.transaction(&spec, &cancel).unwrap();
            let first = tx
                .install(None, &tokens("old-access-canary", "old-refresh-canary"))
                .unwrap();
            *backend.failing_deletes.lock().unwrap() = 10_000;
            let replacement = tx
                .install(
                    Some(first.generation),
                    &tokens("replacement-canary", "replacement-refresh"),
                )
                .unwrap();
            assert_eq!(replacement.retired.len(), 1);
            let intent = tx.begin_refresh(replacement.generation).unwrap();
            let winner = tx
                .finish_refresh(&intent, &tokens("winner-canary", "winner-refresh"))
                .unwrap();
            assert_eq!(winner.retired.len(), 2);
            assert_eq!(tx.load().unwrap().1.access.expose(), "winner-canary");
            let metadata = std::fs::read_to_string(store.record_path(spec.id())).unwrap();
            assert!(
                !metadata.contains("old-refresh-canary") && !metadata.contains("winner-canary")
            );
        }
        *backend.failing_deletes.lock().unwrap() = 0;
        let reopened = AccountStore::new(root.clone(), backend.clone());
        let tx = reopened.transaction(&spec, &cancel).unwrap();
        let (record, winner) = tx.load().unwrap();
        assert_eq!(winner.access.expose(), "winner-canary");
        assert!(record.retired.is_empty());
        let items = backend.items.lock().unwrap();
        assert!(!items.values().any(|value| {
            String::from_utf8_lossy(value).contains("old-access-canary")
                || String::from_utf8_lossy(value).contains("replacement-canary")
        }));
        drop(items);
        drop(tx);
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn revocation_retains_every_previously_retired_handle_until_keychain_cleanup_succeeds() {
        let root = root("retired-revoke");
        let backend = Arc::new(MemoryKeychain::default());
        let store = AccountStore::new(root.clone(), backend.clone());
        let spec = spec();
        let cancel = CancellationToken::new();
        let tx = store.transaction(&spec, &cancel).unwrap();
        let first = tx.install(None, &tokens("first", "first-refresh")).unwrap();
        *backend.failing_deletes.lock().unwrap() = 10_000;
        let second = tx
            .install(Some(first.generation), &tokens("second", "second-refresh"))
            .unwrap();
        assert_eq!(tx.revoke(second.generation).unwrap_err(), Error::Keychain);
        let record = tx.record().unwrap().unwrap();
        assert!(record.retired.is_empty());
        assert!(
            matches!(record.state, AccountState::Revoking { ref retired } if retired.len() == 2)
        );
        assert!(matches!(tx.load(), Err(Error::Revoked)));
        drop(tx);
        *backend.failing_deletes.lock().unwrap() = 0;
        let tx = store.transaction(&spec, &cancel).unwrap();
        assert!(matches!(
            tx.retry_revocation().unwrap().state,
            AccountState::Revoked
        ));
        assert!(backend.items.lock().unwrap().is_empty());
        drop(tx);
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn interrupted_refresh_recovery_journals_a_failed_retired_deletion() {
        let root = root("retired-staged");
        let backend = Arc::new(MemoryKeychain::default());
        let store = AccountStore::new(root.clone(), backend.clone());
        let spec = spec();
        let cancel = CancellationToken::new();
        let tx = store.transaction(&spec, &cancel).unwrap();
        let initial = tx
            .install(None, &tokens("before", "before-refresh"))
            .unwrap();
        let intent = tx.begin_refresh(initial.generation).unwrap();
        let AccountState::Refreshing { pending, .. } = &intent.state else {
            panic!("intent")
        };
        tx.write_bundle(
            pending,
            intent.generation + 1,
            &tokens("after", "after-refresh"),
        )
        .unwrap();
        *backend.failing_deletes.lock().unwrap() = 100;
        let (record, winner) = tx.load().unwrap();
        assert_eq!(winner.access.expose(), "after");
        assert_eq!(record.retired.len(), 1);
        drop(tx);
        *backend.failing_deletes.lock().unwrap() = 0;
        let tx = store.transaction(&spec, &cancel).unwrap();
        let (record, winner) = tx.load().unwrap();
        assert!(record.retired.is_empty());
        assert_eq!(winner.access.expose(), "after");
        drop(tx);
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn retired_backlog_is_bounded_and_cannot_name_an_active_or_foreign_bundle() {
        let root = root("retired-bound");
        let backend = Arc::new(MemoryKeychain::default());
        let store = AccountStore::new(root.clone(), backend.clone());
        let spec = spec();
        let cancel = CancellationToken::new();
        let tx = store.transaction(&spec, &cancel).unwrap();
        let mut record = tx.install(None, &tokens("active", "refresh")).unwrap();
        record.generation = (MAX_RETIRED + 1) as u64;
        record.state = AccountState::Active {
            handle: tx.handle(record.generation),
        };
        record.retired = (1..=MAX_RETIRED as u64)
            .map(|generation| tx.handle(generation))
            .collect();
        tx.write_record(&record).unwrap();
        *backend.failing_deletes.lock().unwrap() = 100_000;
        assert_eq!(
            tx.begin_refresh(record.generation).unwrap_err(),
            Error::Keychain
        );
        assert_eq!(tx.record().unwrap().unwrap().generation, record.generation);
        record.retired[0] = tx.handle(record.generation);
        tx.write_record(&record).unwrap();
        assert!(matches!(tx.record(), Err(Error::BindingMismatch)));
        record.retired[0] = "provider-oauth:foreign:binding:1".to_owned();
        tx.write_record(&record).unwrap();
        assert!(matches!(tx.record(), Err(Error::BindingMismatch)));
        record.retired.clear();
        record.staged = Some(tx.handle(record.generation));
        tx.write_record(&record).unwrap();
        assert!(matches!(tx.record(), Err(Error::BindingMismatch)));
        record.staged = Some("provider-oauth:foreign:binding:66".to_owned());
        tx.write_record(&record).unwrap();
        assert!(matches!(tx.record(), Err(Error::BindingMismatch)));
        drop(tx);
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn failed_login_staging_remains_durable_and_does_not_replace_the_active_bundle() {
        let root = root("login-staging");
        let backend = Arc::new(MemoryKeychain::default());
        let store = AccountStore::new(root.clone(), backend.clone());
        let spec = spec();
        let cancel = CancellationToken::new();
        let tx = store.transaction(&spec, &cancel).unwrap();
        let active = tx
            .install(None, &tokens("old-winner", "old-refresh"))
            .unwrap();
        *backend.failing_puts.lock().unwrap() = 1;
        *backend.failing_deletes.lock().unwrap() = 100_000;
        assert_eq!(
            tx.install(
                Some(active.generation),
                &tokens("failed-login", "new-refresh")
            )
            .unwrap_err(),
            Error::Keychain
        );
        let record = tx.record().unwrap().unwrap();
        assert_eq!(
            record.staged.as_deref(),
            Some(tx.handle(active.generation + 1).as_str())
        );
        assert_eq!(tx.load().unwrap().1.access.expose(), "old-winner");
        assert_eq!(
            tx.begin_refresh(active.generation).unwrap_err(),
            Error::Keychain
        );
        drop(tx);
        *backend.failing_deletes.lock().unwrap() = 0;
        let tx = store.transaction(&spec, &cancel).unwrap();
        let (record, winner) = tx.load().unwrap();
        assert_eq!(winner.access.expose(), "old-winner");
        assert!(record.staged.is_none());
        let replacement = tx
            .install(
                Some(record.generation),
                &tokens("new-winner", "new-refresh"),
            )
            .unwrap();
        assert!(replacement.staged.is_none());
        assert_eq!(tx.load().unwrap().1.access.expose(), "new-winner");
        drop(tx);
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn interrupted_first_login_and_refresh_replacement_keep_staged_handles_for_logout() {
        for during_refresh in [false, true] {
            let root = root("staged-logout");
            let backend = Arc::new(MemoryKeychain::default());
            let store = AccountStore::new(root.clone(), backend.clone());
            let spec = spec();
            let cancel = CancellationToken::new();
            let tx = store.transaction(&spec, &cancel).unwrap();
            let mut record = if during_refresh {
                let active = tx.install(None, &tokens("current", "refresh")).unwrap();
                tx.begin_refresh(active.generation).unwrap()
            } else {
                AccountRecord {
                    schema: SCHEMA.into(),
                    account: spec.id().into(),
                    provider: spec.definition().provider.clone(),
                    binding: spec.binding().into(),
                    generation: 0,
                    state: AccountState::Revoked,
                    retired: vec![],
                    staged: None,
                }
            };
            let staged_generation = record.generation + if during_refresh { 2 } else { 1 };
            let staged = tx.handle(staged_generation);
            record.staged = Some(staged.clone());
            tx.write_record(&record).unwrap();
            tx.write_bundle(
                &staged,
                staged_generation,
                &tokens("staged", "staged-refresh"),
            )
            .unwrap();
            *backend.failing_deletes.lock().unwrap() = 100_000;
            assert_eq!(tx.revoke(record.generation).unwrap_err(), Error::Keychain);
            let saved = tx.record().unwrap().unwrap();
            if during_refresh {
                assert_eq!(saved.generation, record.generation + 2);
                assert!(
                    matches!(saved.state, AccountState::Revoking { ref retired } if retired.contains(&staged))
                );
            } else {
                assert!(saved.staged.is_none());
                assert!(
                    matches!(saved.state, AccountState::Revoking { ref retired } if retired.contains(&staged))
                );
            }
            drop(tx);
            *backend.failing_deletes.lock().unwrap() = 0;
            let tx = store.transaction(&spec, &cancel).unwrap();
            let logged_out = tx.retry_revocation().unwrap();
            assert!(matches!(logged_out.state, AccountState::Revoked));
            assert!(logged_out.staged.is_none());
            assert!(backend.items.lock().unwrap().is_empty());
            drop(tx);
            let _ = std::fs::remove_dir_all(root);
        }
    }
    #[test]
    fn kimi_device_identity_is_created_only_on_explicit_request_and_is_bound_across_restart() {
        let root = root("kimi-identity");
        let store = AccountStore::new(root.clone(), Arc::new(MemoryKeychain::default()));
        let mut definition = spec().definition().clone();
        definition.kind = super::super::AccountKind::Kimi;
        definition.flow = super::super::AccountFlow::Device;
        definition.authorize_url = None;
        definition.redirect_url = None;
        definition.device_url = Some("http://127.0.0.1:4400/device".into());
        let spec = AccountSpec::new("work", definition.clone()).unwrap();
        let cancel = CancellationToken::new();
        let tx = store.transaction(&spec, &cancel).unwrap();
        assert!(matches!(tx.kimi_identity(false), Err(Error::Missing)));
        tx.kimi_identity(true).unwrap();
        let path = root.join(format!("{}.{}.device.json", spec.id(), spec.binding()));
        let first = std::fs::read(&path).unwrap();
        assert!(!first.is_empty());
        drop(tx);
        let tx = store.transaction(&spec, &cancel).unwrap();
        tx.kimi_identity(false).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), first);
        drop(tx);
        definition.client_id = "different-client".into();
        let other = AccountSpec::new("work", definition).unwrap();
        assert!(matches!(
            store
                .transaction(&other, &cancel)
                .unwrap()
                .kimi_identity(false),
            Err(Error::Missing)
        ));
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn account_rotation_and_lock_survive_process_restart() {
        // FileKeychain is a test-only fixture here: it contains only synthetic
        // canaries, and production backend selection never uses this backend.
        const ENV: &str = "RAPID_TEST_ACCOUNT_RESTART_DIR";
        if let Some(path) = std::env::var_os(ENV) {
            let root = std::path::PathBuf::from(path);
            let store = AccountStore::new(
                root.join("metadata"),
                Arc::new(auth::FileKeychain::new(root.join("fake-keychain"))),
            );
            let spec = spec();
            let cancel = CancellationToken::new();
            std::fs::write(root.join("ready"), b"ready").unwrap();
            let tx = store.transaction(&spec, &cancel).unwrap();
            let (record, before) = tx.load().unwrap();
            assert_eq!(before.access.expose(), "before-process");
            let intent = tx.begin_refresh(record.generation).unwrap();
            let mut winner = tx
                .finish_refresh(&intent, &tokens("after-process", "after-refresh"))
                .unwrap();
            let staged_generation = winner.generation + 1;
            let staged = tx.handle(staged_generation);
            winner.staged = Some(staged.clone());
            tx.write_record(&winner).unwrap();
            tx.write_bundle(
                &staged,
                staged_generation,
                &tokens("interrupted-login", "interrupted-refresh"),
            )
            .unwrap();
            std::fs::write(root.join("done"), b"done").unwrap();
            return;
        }
        let root = root("process-restart");
        std::fs::create_dir_all(&root).unwrap();
        let store = AccountStore::new(
            root.join("metadata"),
            Arc::new(auth::FileKeychain::new(root.join("fake-keychain"))),
        );
        let spec = spec();
        let cancel = CancellationToken::new();
        let tx = store.transaction(&spec, &cancel).unwrap();
        tx.install(None, &tokens("before-process", "before-refresh"))
            .unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "provider_accounts::store::tests::account_rotation_and_lock_survive_process_restart", "--nocapture"])
            .env(ENV, &root).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !root.join("ready").exists() && Instant::now() < deadline {
            assert!(child.try_wait().unwrap().is_none());
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(root.join("ready").exists());
        std::thread::sleep(Duration::from_millis(100));
        assert!(!root.join("done").exists());
        drop(tx);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                panic!("child did not finish");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(status.success());
        assert!(root.join("done").exists());
        let reopened = AccountStore::new(
            root.join("metadata"),
            Arc::new(auth::FileKeychain::new(root.join("fake-keychain"))),
        );
        let tx = reopened.transaction(&spec, &cancel).unwrap();
        let (record, after) = tx.load().unwrap();
        assert_eq!(after.access.expose(), "after-process");
        assert_eq!(after.refresh.unwrap().expose(), "after-refresh");
        assert!(record.staged.is_none());
        assert!(matches!(
            tx.read_bundle(&tx.handle(record.generation + 1), record.generation + 1),
            Err(Error::Missing)
        ));
        drop(tx);
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn account_scope_mismatch_and_keychain_corruption_are_errors_not_missing_credentials() {
        let root = root("binding");
        let backend = Arc::new(MemoryKeychain::default());
        let store = AccountStore::new(root.clone(), backend.clone());
        let spec = spec();
        let cancel = CancellationToken::new();
        let first = {
            let tx = store.transaction(&spec, &cancel).unwrap();
            tx.install(None, &tokens("access", "refresh")).unwrap()
        };
        let mut def = spec.definition().clone();
        def.client_id = "other-client".to_owned();
        let other = AccountSpec::new("work", def).unwrap();
        assert!(matches!(
            store.transaction(&other, &cancel).unwrap().load(),
            Err(Error::BindingMismatch)
        ));
        let AccountState::Active { handle } = first.state else {
            panic!("active")
        };
        backend
            .items
            .lock()
            .unwrap()
            .get_mut(&format!("{handle}:chunk0"))
            .unwrap()[0] ^= 1;
        assert!(matches!(
            store.transaction(&spec, &cancel).unwrap().load(),
            Err(Error::Storage)
        ));
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn account_refresh_transactions_serialize_across_store_instances_and_cancel_lock_waiters() {
        let root = root("concurrency");
        let backend = Arc::new(MemoryKeychain::default());
        let spec = spec();
        let cancel = CancellationToken::new();
        let store = AccountStore::new(root.clone(), backend.clone());
        {
            let tx = store.transaction(&spec, &cancel).unwrap();
            tx.install(None, &tokens("old", "old-r")).unwrap();
        }
        let refreshes = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();
        for _ in 0..6 {
            let store = AccountStore::new(root.clone(), backend.clone());
            let spec = spec.clone();
            let refreshes = refreshes.clone();
            workers.push(std::thread::spawn(move || {
                let cancel = CancellationToken::new();
                let tx = store.transaction(&spec, &cancel).unwrap();
                let (record, read) = tx.load().unwrap();
                if read.access.expose() == "old" {
                    refreshes.fetch_add(1, Ordering::SeqCst);
                    let intent = tx.begin_refresh(record.generation).unwrap();
                    std::thread::sleep(Duration::from_millis(10));
                    tx.finish_refresh(&intent, &tokens("new", "new-r")).unwrap();
                }
                assert_eq!(tx.load().unwrap().1.access.expose(), "new");
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
        let held = store.transaction(&spec, &cancel).unwrap();
        let blocked_store = store.clone();
        let blocked_spec = spec.clone();
        let blocked_cancel = CancellationToken::new();
        let stop = blocked_cancel.clone();
        let waiter = std::thread::spawn(move || {
            blocked_store
                .transaction(&blocked_spec, &blocked_cancel)
                .err()
        });
        std::thread::sleep(Duration::from_millis(30));
        stop.cancel();
        assert_eq!(waiter.join().unwrap(), Some(Error::Cancelled));
        drop(held);
        let _ = std::fs::remove_dir_all(root);
    }
}
