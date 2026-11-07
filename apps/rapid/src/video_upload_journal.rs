//! Durable intent records for provider video uploads.
//!
//! The record contains no video bytes, credentials, or resumable URL. An
//! intent alone never authorizes deletion. Finalizing records indicate that
//! the final request may have reached the provider; recovery must verify the
//! complete remote file before cleanup.

use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const SCHEMA: &str = "rapidlm.video_upload.v1";
const MAX_RECORD_BYTES: u64 = 4096;
const MAX_RECORDS: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JournalError {
    Storage,
    Busy,
    Invalid,
    Full,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UploadPhase {
    Intent,
    Finalizing,
    Finalized,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadRecord {
    schema: String,
    id: String,
    scope: String,
    name: String,
    mime: String,
    size: u64,
    sha256: String,
    phase: UploadPhase,
    /// Absent on legacy records; such records are never auto-deleted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credential_binding: Option<String>,
}

/// A one-way, in-memory recovery key derived from the configured API key.
/// It is never written directly to disk or included in debug output.
pub struct CredentialBindingKey([u8; 32]);
impl CredentialBindingKey {
    pub fn from_api_key(key: &str) -> Self {
        use sha2::Digest as _;
        let mut digest = sha2::Sha256::new();
        digest.update(b"rapidlm video recovery key v1\0");
        digest.update(key.as_bytes());
        Self(digest.finalize().into())
    }
    fn for_record(&self, id: &str, scope: &str) -> String {
        use sha2::Digest as _;
        let mut digest = sha2::Sha256::new();
        digest.update(b"rapidlm video recovery binding v1\0");
        digest.update(self.0);
        digest.update(id.as_bytes());
        digest.update(scope.as_bytes());
        hex(&digest.finalize())
    }
}
impl Drop for CredentialBindingKey {
    fn drop(&mut self) {
        self.0.fill(0);
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}

impl std::fmt::Debug for UploadRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UploadRecord")
            .field("identity", &"[redacted]")
            .field("phase", &self.phase)
            .finish()
    }
}

impl UploadRecord {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn scope(&self) -> &str {
        &self.scope
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn mime(&self) -> &str {
        &self.mime
    }
    pub fn size(&self) -> u64 {
        self.size
    }
    pub fn phase(&self) -> UploadPhase {
        self.phase
    }
    fn matches_credential(&self, key: &CredentialBindingKey) -> bool {
        self.credential_binding.as_deref() == Some(key.for_record(&self.id, &self.scope).as_str())
    }
    pub fn digest(&self) -> Result<[u8; 32], JournalError> {
        let mut result = [0u8; 32];
        for (i, byte) in result.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&self.sha256[i * 2..i * 2 + 2], 16)
                .map_err(|_| JournalError::Invalid)?;
        }
        Ok(result)
    }
    fn validate(&self) -> Result<(), JournalError> {
        fn hex(s: &str, len: usize) -> bool {
            s.len() == len
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }
        if self.schema != SCHEMA
            || !hex(&self.id, 32)
            || !hex(&self.scope, 64)
            || self.name != format!("files/rapid-{}", self.id)
            || !hex(&self.sha256, 64)
            || self
                .credential_binding
                .as_ref()
                .is_some_and(|binding| !hex(binding, 64))
            || self.size == 0
            || self.size > llm_router::providers::google_genai::MAX_UPLOADED_VIDEO_BYTES
            || !matches!(self.mime.as_str(), "video/mp4" | "video/mov" | "video/webm")
        {
            return Err(JournalError::Invalid);
        }
        Ok(())
    }
}

pub struct UploadJournal {
    root: PathBuf,
}

/// Owns a per-record OS lock. Dropping it preserves the record for recovery.
pub struct UploadLease {
    root: PathBuf,
    lock: File,
    record: UploadRecord,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sync_dir(root: &Path) -> Result<(), JournalError> {
    File::open(root)
        .and_then(|f| f.sync_all())
        .map_err(|_| JournalError::Storage)
}

fn private_new(path: &Path) -> Result<File, JournalError> {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path).map_err(|_| JournalError::Storage)
}

impl UploadJournal {
    pub fn open(root: PathBuf) -> Result<Self, JournalError> {
        if root
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink())
        {
            return Err(JournalError::Storage);
        }
        fs::create_dir_all(&root).map_err(|_| JournalError::Storage)?;
        if !root
            .symlink_metadata()
            .map_err(|_| JournalError::Storage)?
            .is_dir()
        {
            return Err(JournalError::Storage);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
                .map_err(|_| JournalError::Storage)?;
        }
        Ok(Self { root })
    }

    /// Persist and sync an intent before the caller starts any upload.
    pub fn begin(
        &self,
        scope: &str,
        mime: &str,
        size: u64,
        sha256: [u8; 32],
    ) -> Result<UploadLease, JournalError> {
        self.begin_bound(scope, mime, size, sha256, None)
    }

    pub fn begin_bound(
        &self,
        scope: &str,
        mime: &str,
        size: u64,
        sha256: [u8; 32],
        credential: Option<&CredentialBindingKey>,
    ) -> Result<UploadLease, JournalError> {
        let gate_path = self.root.join(".journal.lock");
        if gate_path.symlink_metadata().is_ok_and(|m| !m.is_file()) {
            return Err(JournalError::Storage);
        }
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let gate = opts.open(gate_path).map_err(|_| JournalError::Storage)?;
        gate.try_lock().map_err(|e| match e {
            std::fs::TryLockError::WouldBlock => JournalError::Busy,
            _ => JournalError::Storage,
        })?;
        if self.records()?.len() >= MAX_RECORDS {
            return Err(JournalError::Full);
        }
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|_| JournalError::Storage)?;
        let id = hex(&random);
        let credential_binding = credential.map(|key| key.for_record(&id, scope));
        let record = UploadRecord {
            schema: SCHEMA.to_owned(),
            name: format!("files/rapid-{id}"),
            id,
            scope: scope.to_owned(),
            mime: mime.to_owned(),
            size,
            sha256: hex(&sha256),
            phase: UploadPhase::Intent,
            credential_binding,
        };
        record.validate()?;
        let lock = private_new(&self.root.join(format!("{}.lock", record.id)))?;
        lock.try_lock().map_err(|_| JournalError::Busy)?;
        let lease = UploadLease {
            root: self.root.clone(),
            lock,
            record,
        };
        lease.write_record(true)?;
        Ok(lease)
    }

    /// List record IDs. Corrupt records remain present and must not be ignored.
    pub fn records(&self) -> Result<Vec<String>, JournalError> {
        let mut ids = Vec::new();
        for entry in fs::read_dir(&self.root).map_err(|_| JournalError::Storage)? {
            let entry = entry.map_err(|_| JournalError::Storage)?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                return Err(JournalError::Invalid);
            };
            if let Some(id) = name.strip_suffix(".json") {
                if id.len() != 32
                    || !id
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                {
                    return Err(JournalError::Invalid);
                }
                ids.push(id.to_owned());
                if ids.len() > MAX_RECORDS {
                    return Err(JournalError::Full);
                }
            }
        }
        ids.sort();
        Ok(ids)
    }

    /// Reopen only a record whose previous owner no longer holds its lock.
    pub fn acquire(&self, id: &str) -> Result<UploadLease, JournalError> {
        if id.len() != 32
            || !id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(JournalError::Invalid);
        }
        let lock_path = self.root.join(format!("{id}.lock"));
        if !lock_path
            .symlink_metadata()
            .map_err(|_| JournalError::Storage)?
            .is_file()
        {
            return Err(JournalError::Storage);
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(lock_path)
            .map_err(|_| JournalError::Storage)?;
        lock.try_lock().map_err(|e| match e {
            std::fs::TryLockError::WouldBlock => JournalError::Busy,
            _ => JournalError::Storage,
        })?;
        let path = self.root.join(format!("{id}.json"));
        if !path
            .symlink_metadata()
            .map_err(|_| JournalError::Storage)?
            .is_file()
        {
            return Err(JournalError::Storage);
        }
        let mut bytes = Vec::new();
        File::open(path)
            .map_err(|_| JournalError::Storage)?
            .take(MAX_RECORD_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| JournalError::Storage)?;
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            return Err(JournalError::Invalid);
        }
        let record: UploadRecord =
            serde_json::from_slice(&bytes).map_err(|_| JournalError::Invalid)?;
        record.validate()?;
        if record.id != id {
            return Err(JournalError::Invalid);
        }
        Ok(UploadLease {
            root: self.root.clone(),
            lock,
            record,
        })
    }
}

impl UploadLease {
    pub fn record(&self) -> &UploadRecord {
        &self.record
    }

    fn write_record(&self, first: bool) -> Result<(), JournalError> {
        let path = self.root.join(format!("{}.json", self.record.id));
        let bytes = serde_json::to_vec(&self.record).map_err(|_| JournalError::Storage)?;
        if first {
            let mut file = private_new(&path)?;
            file.write_all(&bytes)
                .and_then(|_| file.sync_all())
                .map_err(|_| JournalError::Storage)?;
        } else {
            let mut nonce = [0u8; 8];
            getrandom::fill(&mut nonce).map_err(|_| JournalError::Storage)?;
            let temp = self
                .root
                .join(format!("{}.{}.tmp", self.record.id, hex(&nonce)));
            let mut file = private_new(&temp)?;
            if file
                .write_all(&bytes)
                .and_then(|_| file.sync_all())
                .and_then(|_| fs::rename(&temp, &path))
                .is_err()
            {
                let _ = fs::remove_file(temp);
                return Err(JournalError::Storage);
            }
        }
        sync_dir(&self.root)
    }

    /// Persist before sending the final chunk, which may commit remotely even
    /// when its response is lost or this process exits.
    pub fn mark_finalizing(&mut self) -> Result<(), JournalError> {
        if self.record.phase != UploadPhase::Intent {
            return Err(JournalError::Invalid);
        }
        self.record.phase = UploadPhase::Finalizing;
        if let Err(error) = self.write_record(false) {
            self.record.phase = UploadPhase::Intent;
            return Err(error);
        }
        Ok(())
    }

    /// Call only after a matching finalization response has been received.
    pub fn mark_finalized(&mut self) -> Result<(), JournalError> {
        if self.record.phase != UploadPhase::Finalizing {
            return Err(JournalError::Invalid);
        }
        self.record.phase = UploadPhase::Finalized;
        if let Err(error) = self.write_record(false) {
            self.record.phase = UploadPhase::Finalizing;
            return Err(error);
        }
        Ok(())
    }

    /// Remove only after the caller has confirmed remote deletion.
    pub fn remove_after_confirmed_delete(self) -> Result<(), JournalError> {
        fs::remove_file(self.root.join(format!("{}.json", self.record.id)))
            .map_err(|_| JournalError::Storage)?;
        sync_dir(&self.root)?;
        // The lock inode remains, avoiding races with a second opener. It is
        // small and can be reclaimed only by a separate coordinated sweep.
        Ok(())
    }
}

impl Drop for UploadLease {
    fn drop(&mut self) {
        let _ = self.lock.unlock();
    }
}

/// Result of staging and submitting one video. It retains the journal lock
/// until cleanup or drop so another recovery worker cannot race its user.
pub struct UploadedVideo {
    lease: UploadLease,
    handle: llm_router::providers::google_genai::GoogleFileHandle,
}

#[derive(Debug, Eq, PartialEq)]
pub enum LifecycleError {
    Journal(JournalError),
    Rejected(llm_router::ProviderError),
    /// Remote state may exist; its journal record was retained.
    Uncertain,
}

/// Persist intent, then upload immutable borrowed bytes and persist successful
/// finalization. A failed exchange never removes the recovery record.
pub fn upload_video<T: llm_router::providers::openai_compatible::HttpTransport>(
    journal: &UploadJournal,
    adapter: &llm_router::providers::google_genai::GoogleGenAiAdapter<'_, T>,
    mime: &str,
    bytes: &[u8],
    cancel: &llm_router::CancellationToken,
) -> Result<UploadedVideo, LifecycleError> {
    upload_video_bound(journal, adapter, mime, bytes, None, cancel)
}

pub fn upload_video_bound<T: llm_router::providers::openai_compatible::HttpTransport>(
    journal: &UploadJournal,
    adapter: &llm_router::providers::google_genai::GoogleGenAiAdapter<'_, T>,
    mime: &str,
    bytes: &[u8],
    credential: Option<&CredentialBindingKey>,
    cancel: &llm_router::CancellationToken,
) -> Result<UploadedVideo, LifecycleError> {
    use llm_router::providers::google_genai::GoogleUploadError;
    use sha2::Digest as _;
    cancel.check().map_err(LifecycleError::Rejected)?;
    let digest: [u8; 32] = sha2::Sha256::digest(bytes).into();
    let mut lease = journal
        .begin_bound(
            &adapter.file_scope(),
            mime,
            bytes.len() as u64,
            digest,
            credential,
        )
        .map_err(LifecycleError::Journal)?;
    let map_error = |error| match error {
        GoogleUploadError::Rejected(reason) => LifecycleError::Rejected(reason),
        GoogleUploadError::Uncertain => LifecycleError::Uncertain,
    };
    let mut upload = adapter
        .begin_named_video_upload(
            lease.record().name(),
            mime,
            bytes.len() as u64,
            digest,
            cancel,
        )
        .map_err(map_error)?;
    let mut result = None;
    for chunk in bytes.chunks(upload.preferred_chunk_size()) {
        if upload.offset() + chunk.len() as u64 == bytes.len() as u64 {
            lease.mark_finalizing().map_err(LifecycleError::Journal)?;
        }
        result = adapter
            .upload_video_chunk(&mut upload, chunk, cancel)
            .map_err(map_error)?;
    }
    let handle = result.ok_or(LifecycleError::Uncertain)?;
    lease.mark_finalized().map_err(LifecycleError::Journal)?;
    Ok(UploadedVideo { lease, handle })
}

#[derive(Default, Debug, Eq, PartialEq)]
pub struct RecoveryReport {
    pub deleted: usize,
    pub retained: usize,
}

/// Retry at most two finalizing or finalized same-key records. Intents and
/// legacy records are not evidence that finalization was attempted.
pub fn recover_finalized<T: llm_router::providers::openai_compatible::HttpTransport>(
    journal: &UploadJournal,
    adapter: &llm_router::providers::google_genai::GoogleGenAiAdapter<'_, T>,
    credential: &CredentialBindingKey,
    cancel: &llm_router::CancellationToken,
) -> Result<RecoveryReport, JournalError> {
    let mut report = RecoveryReport::default();
    let mut attempted = 0;
    let mut ids = journal.records()?;
    if !ids.is_empty() {
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).map_err(|_| JournalError::Storage)?;
        let start = (u64::from_le_bytes(random) % ids.len() as u64) as usize;
        ids.rotate_left(start);
    }
    for id in ids {
        if cancel.is_cancelled() || attempted == 2 {
            break;
        }
        let lease = match journal.acquire(&id) {
            Ok(lease) => lease,
            Err(JournalError::Busy) => {
                report.retained += 1;
                continue;
            }
            Err(error) => return Err(error),
        };
        let record = lease.record();
        if !matches!(
            record.phase(),
            UploadPhase::Finalizing | UploadPhase::Finalized
        ) || record.scope() != adapter.file_scope()
            || !record.matches_credential(credential)
        {
            report.retained += 1;
            continue;
        }
        attempted += 1;
        let handle = match adapter.bind_file(record.name()) {
            Ok(handle) => handle,
            Err(_) => {
                report.retained += 1;
                continue;
            }
        };
        let verified = matches!(
            adapter.verify_video_file(
                &handle,
                record.mime(),
                record.size(),
                record.digest()?,
                cancel,
            ),
            Ok(llm_router::providers::google_genai::GoogleFileState::Active)
        );
        if verified && adapter.delete_file(&handle, cancel).is_ok() {
            lease.remove_after_confirmed_delete()?;
            report.deleted += 1;
        } else {
            report.retained += 1;
        }
    }
    Ok(report)
}

impl UploadedVideo {
    pub fn handle(&self) -> &llm_router::providers::google_genai::GoogleFileHandle {
        &self.handle
    }

    /// Retain the journal if deletion fails or has an ambiguous outcome.
    pub fn delete<T: llm_router::providers::openai_compatible::HttpTransport>(
        self,
        adapter: &llm_router::providers::google_genai::GoogleGenAiAdapter<'_, T>,
        cancel: &llm_router::CancellationToken,
    ) -> Result<(), LifecycleError> {
        if self.lease.record().scope() != adapter.file_scope() {
            return Err(LifecycleError::Rejected(
                llm_router::ProviderError::InvalidRequest,
            ));
        }
        adapter
            .delete_file(&self.handle, cancel)
            .map_err(|_| LifecycleError::Uncertain)?;
        self.lease
            .remove_after_confirmed_delete()
            .map_err(LifecycleError::Journal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: PathBuf,
        name: std::sync::Mutex<Option<String>>,
        fail_upload: bool,
        fail_delete: bool,
        status_state: &'static str,
    }
    impl llm_router::HttpTransport for Fixture {
        fn execute(
            &self,
            request: &llm_router::ProviderHttpRequest<'_>,
            _: &llm_router::CancellationToken,
        ) -> Result<llm_router::ProviderHttpResponse, llm_router::ProviderError> {
            if request.url().contains('?') {
                let ids = UploadJournal::open(self.root.clone())
                    .unwrap()
                    .records()
                    .unwrap();
                let record: UploadRecord = serde_json::from_slice(
                    &fs::read(self.root.join(format!("{}.json", ids[0]))).unwrap(),
                )
                .unwrap();
                assert_eq!(
                    record.phase(),
                    UploadPhase::Finalizing,
                    "finalization intent must be persisted before the final request"
                );
                if self.fail_upload {
                    return Err(llm_router::ProviderError::Connection);
                }
                let name = self.name.lock().unwrap().clone().unwrap();
                let body = serde_json::json!({"file":{"name":name,"mimeType":"video/mp4"}});
                return llm_router::ProviderHttpResponse::new(
                    200,
                    vec![],
                    body.to_string().into_bytes(),
                );
            }
            let ids = UploadJournal::open(self.root.clone())
                .unwrap()
                .records()
                .unwrap();
            assert_eq!(ids.len(), 1, "intent must exist before start");
            let body: serde_json::Value = serde_json::from_slice(request.body()).unwrap();
            let name = body["file"]["name"].as_str().unwrap().to_owned();
            assert_eq!(name, format!("files/rapid-{}", ids[0]));
            *self.name.lock().unwrap() = Some(name);
            llm_router::ProviderHttpResponse::new(
                200,
                vec![(
                    "X-Goog-Upload-URL".into(),
                    "https://example.test/upload/v1beta/files?upload_id=test".into(),
                )],
                vec![],
            )
        }
        fn execute_control(
            &self,
            method: llm_router::providers::openai_compatible::ProviderControlMethod,
            request: &llm_router::ProviderHttpRequest<'_>,
            _: &llm_router::CancellationToken,
        ) -> Result<llm_router::ProviderHttpResponse, llm_router::ProviderError> {
            use llm_router::providers::openai_compatible::ProviderControlMethod;
            if method == ProviderControlMethod::Get {
                use base64::{Engine as _, engine::general_purpose::STANDARD};
                use sha2::Digest as _;
                let id = request.url().rsplit('/').next().unwrap();
                let body = serde_json::json!({"name": format!("files/{id}"), "state":self.status_state, "mimeType":"video/mp4", "sizeBytes":"6", "sha256Hash":STANDARD.encode(sha2::Sha256::digest(b"sample"))});
                return llm_router::ProviderHttpResponse::new(
                    200,
                    vec![],
                    body.to_string().into_bytes(),
                );
            }
            assert_eq!(method, ProviderControlMethod::Delete);
            if self.fail_delete {
                return Err(llm_router::ProviderError::Connection);
            }
            llm_router::ProviderHttpResponse::new(204, vec![], vec![])
        }
    }

    fn adapter<'a>(
        root: PathBuf,
        store: &'a auth::InMemoryCredentialStore,
        fail_upload: bool,
        fail_delete: bool,
    ) -> llm_router::providers::google_genai::GoogleGenAiAdapter<'a, Fixture> {
        adapter_with_state(root, store, fail_upload, fail_delete, "ACTIVE")
    }

    fn adapter_with_state<'a>(
        root: PathBuf,
        store: &'a auth::InMemoryCredentialStore,
        fail_upload: bool,
        fail_delete: bool,
        status_state: &'static str,
    ) -> llm_router::providers::google_genai::GoogleGenAiAdapter<'a, Fixture> {
        use llm_router::providers::google_genai::{
            GoogleGenAiAdapter, GoogleGenAiConfig, GoogleGenAiEndpoint,
        };
        let profile = llm_router::ProviderProfile::new(
            "work",
            llm_router::ProviderId::parse("google-genai").unwrap(),
            auth::SecretRef::from_alias("test-key").unwrap(),
        )
        .unwrap();
        let caps = llm_router::ProviderCapabilities::new(
            true,
            true,
            false,
            false,
            llm_router::ReasoningSupport::None,
            false,
            8192,
            1024,
            llm_router::UsageFieldSet::new(false, false, false, false, false, false, false),
        )
        .unwrap();
        let config = GoogleGenAiConfig::new(
            profile,
            GoogleGenAiEndpoint::new("https://example.test/v1beta").unwrap(),
            caps,
        )
        .unwrap()
        .with_video_input(true);
        GoogleGenAiAdapter::new(
            config,
            Fixture {
                root,
                name: std::sync::Mutex::new(None),
                fail_upload,
                fail_delete,
                status_state,
            },
            store,
        )
    }

    fn store() -> auth::InMemoryCredentialStore {
        use auth::CredentialStore as _;
        let store = auth::InMemoryCredentialStore::new();
        let key = auth::SecretValue::from_str(
            auth::SecretRef::from_alias("test-key").unwrap(),
            "private-key",
        )
        .unwrap();
        store
            .put(
                auth::CredentialPut::ephemeral(key, auth::CredentialKind::ProviderApiKey)
                    .with_provider("google-genai")
                    .unwrap(),
                &auth::CancellationToken::new(),
            )
            .unwrap();
        store
    }

    fn root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "rapid-video-journal-{}",
            protocol::SessionId::new()
        ))
    }

    #[test]
    fn upload_intent_survives_owner_and_finalization_is_durable() {
        let root = root();
        let journal = UploadJournal::open(root.clone()).unwrap();
        let scope = "a".repeat(64);
        let lease = journal.begin(&scope, "video/mp4", 42, [7; 32]).unwrap();
        let id = lease.record().id().to_owned();
        assert_eq!(lease.record().name(), format!("files/rapid-{id}"));
        assert_eq!(journal.records().unwrap(), vec![id.clone()]);
        assert!(matches!(journal.acquire(&id), Err(JournalError::Busy)));
        drop(lease);
        let mut recovered = UploadJournal::open(root.clone())
            .unwrap()
            .acquire(&id)
            .unwrap();
        assert_eq!(recovered.record().phase(), UploadPhase::Intent);
        assert_eq!(recovered.record().digest().unwrap(), [7; 32]);
        assert_eq!(recovered.mark_finalized(), Err(JournalError::Invalid));
        recovered.mark_finalizing().unwrap();
        drop(recovered);
        let mut recovered = journal.acquire(&id).unwrap();
        assert_eq!(recovered.record().phase(), UploadPhase::Finalizing);
        recovered.mark_finalized().unwrap();
        drop(recovered);
        let recovered = journal.acquire(&id).unwrap();
        assert_eq!(recovered.record().phase(), UploadPhase::Finalized);
        recovered.remove_after_confirmed_delete().unwrap();
        assert!(journal.records().unwrap().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn upload_intent_survives_a_killed_owner_process() {
        const CHILD_ROOT: &str = "RAPID_VIDEO_JOURNAL_TEST_CHILD";
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            let root = PathBuf::from(root);
            let journal = UploadJournal::open(root.clone()).unwrap();
            let lease = journal
                .begin(&"a".repeat(64), "video/mp4", 42, [7; 32])
                .unwrap();
            fs::write(root.join("ready"), lease.record().id()).unwrap();
            loop {
                std::thread::park();
            }
        }
        let root = root();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("video_upload_journal::tests::upload_intent_survives_a_killed_owner_process")
            .arg("--nocapture")
            .env(CHILD_ROOT, &root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let marker = root.join("ready");
        while !marker.exists() && std::time::Instant::now() < deadline {
            assert!(
                child.try_wait().unwrap().is_none(),
                "child exited before persisting intent"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if !marker.exists() {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child did not persist its intent");
        }
        let id = fs::read_to_string(&marker).expect("child did not persist its intent");
        let journal = UploadJournal::open(root.clone()).unwrap();
        assert!(matches!(journal.acquire(&id), Err(JournalError::Busy)));
        child.kill().unwrap();
        child.wait().unwrap();
        let recovered = journal.acquire(&id).unwrap();
        assert_eq!(recovered.record().phase(), UploadPhase::Intent);
        drop(recovered);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn upload_lifecycle_records_before_network_and_retains_uncertain_outcomes() {
        let root = root();
        let journal = UploadJournal::open(root.clone()).unwrap();
        let store = store();
        let cancel = llm_router::CancellationToken::new();
        let client = adapter(root.clone(), &store, false, false);
        let upload = upload_video(&journal, &client, "video/mp4", b"sample", &cancel).unwrap();
        let id = journal.records().unwrap().pop().unwrap();
        assert!(matches!(journal.acquire(&id), Err(JournalError::Busy)));
        upload.delete(&client, &cancel).unwrap();
        assert!(journal.records().unwrap().is_empty());

        let client = adapter(root.clone(), &store, true, false);
        assert!(matches!(
            upload_video(&journal, &client, "video/mp4", b"sample", &cancel),
            Err(LifecycleError::Uncertain)
        ));
        let id = journal.records().unwrap().pop().unwrap();
        assert_eq!(
            journal.acquire(&id).unwrap().record().phase(),
            UploadPhase::Finalizing
        );
        fs::remove_dir_all(&root).unwrap();

        let second_root = self::root();
        let journal = UploadJournal::open(second_root.clone()).unwrap();
        let client = adapter(second_root.clone(), &store, false, true);
        let upload = upload_video(&journal, &client, "video/mp4", b"sample", &cancel).unwrap();
        assert_eq!(
            upload.delete(&client, &cancel),
            Err(LifecycleError::Uncertain)
        );
        let id = journal.records().unwrap().pop().unwrap();
        assert_eq!(
            journal.acquire(&id).unwrap().record().phase(),
            UploadPhase::Finalized
        );
        fs::remove_dir_all(second_root).unwrap();
    }

    #[test]
    fn recovery_deletes_only_finalized_records_bound_to_the_original_key_and_content() {
        let root = root();
        let journal = UploadJournal::open(root.clone()).unwrap();
        let store = store();
        let cancel = llm_router::CancellationToken::new();
        let client = adapter(root.clone(), &store, false, false);
        let original = CredentialBindingKey::from_api_key("private-key");
        let changed = CredentialBindingKey::from_api_key("changed-key");
        let upload = upload_video_bound(
            &journal,
            &client,
            "video/mp4",
            b"sample",
            Some(&original),
            &cancel,
        )
        .unwrap();
        let id = journal.records().unwrap().pop().unwrap();
        assert_eq!(
            recover_finalized(&journal, &client, &original, &cancel)
                .unwrap()
                .deleted,
            0
        );
        drop(upload);
        assert_eq!(
            recover_finalized(&journal, &client, &changed, &cancel)
                .unwrap()
                .deleted,
            0
        );
        assert_eq!(
            journal.acquire(&id).unwrap().record().phase(),
            UploadPhase::Finalized
        );
        assert_eq!(
            recover_finalized(&journal, &client, &original, &cancel)
                .unwrap()
                .deleted,
            1
        );
        assert!(journal.records().unwrap().is_empty());

        let intent = journal
            .begin_bound(
                &client.file_scope(),
                "video/mp4",
                6,
                [0; 32],
                Some(&original),
            )
            .unwrap();
        let intent_id = intent.record().id().to_owned();
        drop(intent);
        assert_eq!(
            recover_finalized(&journal, &client, &original, &cancel)
                .unwrap()
                .deleted,
            0
        );
        assert_eq!(
            journal.acquire(&intent_id).unwrap().record().phase(),
            UploadPhase::Intent
        );
        let mut wrong = journal
            .begin_bound(
                &client.file_scope(),
                "video/mp4",
                6,
                [1; 32],
                Some(&original),
            )
            .unwrap();
        wrong.mark_finalizing().unwrap();
        wrong.mark_finalized().unwrap();
        let wrong_id = wrong.record().id().to_owned();
        drop(wrong);
        assert_eq!(
            recover_finalized(&journal, &client, &original, &cancel)
                .unwrap()
                .deleted,
            0
        );
        assert_eq!(
            journal.acquire(&wrong_id).unwrap().record().phase(),
            UploadPhase::Finalized
        );
        let mut legacy = journal
            .begin(&client.file_scope(), "video/mp4", 6, [2; 32])
            .unwrap();
        legacy.mark_finalizing().unwrap();
        legacy.mark_finalized().unwrap();
        let legacy_id = legacy.record().id().to_owned();
        drop(legacy);
        assert_eq!(
            recover_finalized(&journal, &client, &original, &cancel)
                .unwrap()
                .deleted,
            0
        );
        assert_eq!(
            journal.acquire(&legacy_id).unwrap().record().phase(),
            UploadPhase::Finalized
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recovery_handles_lost_finalize_response_only_after_file_becomes_active() {
        let root = root();
        let journal = UploadJournal::open(root.clone()).unwrap();
        let store = store();
        let cancel = llm_router::CancellationToken::new();
        let key = CredentialBindingKey::from_api_key("private-key");
        let failing = adapter(root.clone(), &store, true, false);
        assert!(matches!(
            upload_video_bound(
                &journal,
                &failing,
                "video/mp4",
                b"sample",
                Some(&key),
                &cancel
            ),
            Err(LifecycleError::Uncertain)
        ));
        let id = journal.records().unwrap().pop().unwrap();
        assert_eq!(
            journal.acquire(&id).unwrap().record().phase(),
            UploadPhase::Finalizing
        );
        let processing = adapter_with_state(root.clone(), &store, false, false, "PROCESSING");
        assert_eq!(
            recover_finalized(&journal, &processing, &key, &cancel)
                .unwrap()
                .deleted,
            0
        );
        assert_eq!(
            journal.acquire(&id).unwrap().record().phase(),
            UploadPhase::Finalizing
        );
        let active = adapter(root.clone(), &store, false, false);
        assert_eq!(
            recover_finalized(&journal, &active, &key, &cancel)
                .unwrap()
                .deleted,
            1
        );
        assert!(journal.records().unwrap().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn journal_refuses_invalid_identity_and_corrupt_records() {
        let root = root();
        let journal = UploadJournal::open(root.clone()).unwrap();
        assert!(matches!(
            journal.begin("scope", "video/mp4", 1, [0; 32]),
            Err(JournalError::Invalid)
        ));
        assert!(matches!(
            journal.begin(&"a".repeat(64), "text/plain", 1, [0; 32]),
            Err(JournalError::Invalid)
        ));
        assert!(matches!(
            journal.acquire("../other"),
            Err(JournalError::Invalid)
        ));
        let lease = journal
            .begin(&"a".repeat(64), "video/webm", 1, [0; 32])
            .unwrap();
        let id = lease.record().id().to_owned();
        drop(lease);
        fs::write(root.join(format!("{id}.json")), b"{}").unwrap();
        assert!(matches!(journal.acquire(&id), Err(JournalError::Invalid)));
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn journal_refuses_symlink_records_and_restricts_permissions() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let root = root();
        let journal = UploadJournal::open(root.clone()).unwrap();
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let lease = journal
            .begin(&"a".repeat(64), "video/mov", 1, [0; 32])
            .unwrap();
        let id = lease.record().id().to_owned();
        assert_eq!(
            fs::metadata(root.join(format!("{id}.json")))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        drop(lease);
        let path = root.join(format!("{id}.json"));
        fs::remove_file(&path).unwrap();
        symlink("/dev/null", path).unwrap();
        assert!(matches!(journal.acquire(&id), Err(JournalError::Storage)));
        fs::remove_dir_all(root).unwrap();
    }
}
