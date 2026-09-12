//! OS-user-bound local daemon client authentication.
//!
//! The daemon issues a high-entropy token, stores it in a user-private runtime
//! directory with owner-only mode/ACL, and requires a one-use challenge proof
//! before session APIs. Token bytes never appear in `Debug`/`Display`, error
//! messages, or CLI argument lists.

use std::fmt::{self, Debug, Display, Formatter};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{Ordering, compiler_fence};

use sha2::{Digest, Sha256};

use crate::store::CancellationToken;

/// Owner-only mode applied to the token file after write.
pub const TOKEN_FILE_MODE: u32 = 0o600;

/// Owner-only mode required on the token file's immediate parent directory.
pub const TOKEN_DIR_MODE: u32 = 0o700;

/// File name used under the user-private runtime directory.
pub const TOKEN_FILE_NAME: &str = "daemon.token";

/// Versioned on-disk token schema identifier.
pub const TOKEN_SCHEMA: &str = "rapidlm.auth.daemon_token";

/// Current token file schema version.
pub const TOKEN_SCHEMA_VERSION: u32 = 1;

/// Random token length.
pub const TOKEN_BYTES: usize = 32;

/// Challenge nonce length.
pub const NONCE_BYTES: usize = 32;

/// Challenge identifier length.
pub const CHALLENGE_ID_BYTES: usize = 16;

/// Maximum accepted OS username length.
pub const MAX_USERNAME_BYTES: usize = 64;

/// Maximum accepted runtime-directory path length.
pub const MAX_RUNTIME_PATH_BYTES: usize = 512;

/// Maximum simultaneous unused challenges.
pub const MAX_PENDING_CHALLENGES: usize = 32;

/// Hard ceiling for a token file (header + username + token).
pub const MAX_TOKEN_FILE_BYTES: usize = 256;

const TOKEN_MAGIC: &[u8; 4] = b"RLDT";
const HMAC_DOMAIN: &[u8] = b"rapidlm.daemon.auth.v1";
const GRANT_DOMAIN: &[u8] = b"rapidlm.daemon.grant.v1";

/// Typed failures for local daemon authentication. Display never includes token bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DaemonAuthError {
    Cancelled,
    AuthRequired,
    TokenMismatch,
    TokenMissing,
    UserMismatch,
    InsecurePermissions { mode: u32 },
    InvalidPath,
    InvalidToken,
    ChallengeUnknown,
    ChallengeSpent,
    BoundExceeded { limit: usize, requested: usize },
    Io,
    Entropy,
    LockPoisoned,
}

/// OS identity the token is bound to. Username is diagnostic; uid is authoritative.
#[derive(Clone, Eq, PartialEq)]
pub struct OsUserBinding {
    uid: u32,
    username: String,
}

/// Handle returned after issue. Identifies the file; never carries token bytes.
#[derive(Clone, Eq, PartialEq)]
pub struct DaemonTokenHandle {
    path: PathBuf,
    binding: OsUserBinding,
    byte_len: usize,
}

/// One-use server challenge. The nonce is not secret; the token is.
#[derive(Clone, Eq, PartialEq)]
pub struct AuthChallenge {
    id: [u8; CHALLENGE_ID_BYTES],
    nonce: [u8; NONCE_BYTES],
}

/// Client proof of token knowledge. Response bytes are redacted.
#[derive(Clone, Eq, PartialEq)]
pub struct AuthProof {
    challenge_id: [u8; CHALLENGE_ID_BYTES],
    response: [u8; TOKEN_BYTES],
}

impl AuthProof {
    /// Hex-encoded response for the wire proof frame.
    pub fn response_hex(&self) -> String {
        hex_encode(&self.response)
    }

    /// Reconstruct a proof from wire bytes. Only the constant-time MAC check
    /// in [`DaemonAuth::authenticate`] grants any authority.
    pub fn from_parts(challenge_id: [u8; CHALLENGE_ID_BYTES], response: [u8; TOKEN_BYTES]) -> Self {
        Self {
            challenge_id,
            response,
        }
    }
}

/// Proof that challenge/auth succeeded for this OS user. Not constructible outside Auth.
#[derive(Clone, Eq, PartialEq)]
pub struct ClientGrant {
    uid: u32,
    tag: [u8; TOKEN_BYTES],
}

/// Ticket proving a session API was authorized after challenge/auth.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuthorizedSessionApi {
    kind: SessionApiKind,
}

/// Session APIs that must not run before a live [`ClientGrant`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum SessionApiKind {
    Enumerate,
    Get,
    Create,
    SubmitTurn,
    Interrupt,
    Subscribe,
    Approve,
    Fork,
    Rewind,
}

/// Daemon-side authenticator. Holds the issued token in memory only after load/issue.
pub struct DaemonAuth {
    runtime_dir: PathBuf,
    binding: OsUserBinding,
    inner: Mutex<Inner>,
}

/// Local client. Loads the token file only to compute a proof; never exposes it.
pub struct LocalDaemonClient {
    runtime_dir: PathBuf,
    binding: OsUserBinding,
}

struct Inner {
    token: Option<DaemonToken>,
    pending: Vec<PendingChallenge>,
}

struct DaemonToken {
    bytes: [u8; TOKEN_BYTES],
}

struct PendingChallenge {
    id: [u8; CHALLENGE_ID_BYTES],
    nonce: [u8; NONCE_BYTES],
}

struct StoredToken {
    binding: OsUserBinding,
    token: DaemonToken,
}

impl DaemonAuthError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Cancelled => "auth.cancelled",
            Self::AuthRequired | Self::TokenMismatch | Self::TokenMissing => "auth.required",
            Self::UserMismatch => "auth.user_mismatch",
            Self::InsecurePermissions { .. } => "auth.insecure_permissions",
            Self::InvalidPath => "auth.invalid_path",
            Self::InvalidToken => "auth.token_invalid",
            Self::ChallengeUnknown => "auth.challenge_unknown",
            Self::ChallengeSpent => "auth.challenge_spent",
            Self::BoundExceeded { .. } => "auth.bound_exceeded",
            Self::Io => "auth.io",
            Self::Entropy => "auth.entropy",
            Self::LockPoisoned => "auth.lock_poisoned",
        }
    }

    pub fn retryable(&self) -> bool {
        false
    }
}

impl Display for DaemonAuthError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("local daemon auth operation was cancelled"),
            Self::AuthRequired => {
                f.write_str("local daemon authentication is required before session APIs")
            }
            Self::TokenMismatch => f.write_str("local daemon authentication failed"),
            Self::TokenMissing => f.write_str("local daemon auth token is missing"),
            Self::UserMismatch => {
                f.write_str("local daemon auth token is bound to a different OS user")
            }
            Self::InsecurePermissions { mode } => {
                write!(
                    f,
                    "local daemon auth path is not owner-only (mode {mode:#o})"
                )
            }
            Self::InvalidPath => f.write_str("local daemon runtime path is invalid"),
            Self::InvalidToken => f.write_str("local daemon auth token file is invalid"),
            Self::ChallengeUnknown => f.write_str("local daemon auth challenge is unknown"),
            Self::ChallengeSpent => f.write_str("local daemon auth challenge was already used"),
            Self::BoundExceeded { limit, requested } => {
                write!(
                    f,
                    "local daemon auth bound exceeded ({requested} > {limit})"
                )
            }
            Self::Io => f.write_str("local daemon auth I/O failed"),
            Self::Entropy => f.write_str("local daemon auth entropy source failed"),
            Self::LockPoisoned => f.write_str("local daemon auth lock was poisoned"),
        }
    }
}

impl std::error::Error for DaemonAuthError {}

impl OsUserBinding {
    pub fn uid(&self) -> u32 {
        self.uid
    }

    pub fn username(&self) -> &str {
        &self.username
    }
}

impl DaemonTokenHandle {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn binding(&self) -> &OsUserBinding {
        &self.binding
    }

    pub fn byte_len(&self) -> usize {
        self.byte_len
    }
}

impl AuthChallenge {
    pub fn id_hex(&self) -> String {
        hex_encode(&self.id)
    }

    pub fn nonce(&self) -> &[u8; NONCE_BYTES] {
        &self.nonce
    }
}

impl AuthChallenge {
    /// Rebuild a challenge from wire bytes for the client-side proof.
    pub fn from_parts(id: [u8; CHALLENGE_ID_BYTES], nonce: [u8; NONCE_BYTES]) -> Self {
        Self { id, nonce }
    }
}

impl AuthProof {
    pub fn challenge_id_hex(&self) -> String {
        hex_encode(&self.challenge_id)
    }
}

impl ClientGrant {
    pub fn uid(&self) -> u32 {
        self.uid
    }
}

impl AuthorizedSessionApi {
    pub fn kind(self) -> SessionApiKind {
        self.kind
    }
}

impl DaemonAuth {
    /// Open a user-private runtime directory and load a token if one exists.
    pub fn open(
        runtime_dir: impl Into<PathBuf>,
        cancel: &CancellationToken,
    ) -> Result<Self, DaemonAuthError> {
        cancel.check().map_err(|_| DaemonAuthError::Cancelled)?;
        let runtime_dir = validate_runtime_dir(runtime_dir.into())?;
        ensure_private_dir(&runtime_dir)?;
        let binding = binding_from_dir(&runtime_dir)?;
        let auth = Self {
            runtime_dir,
            binding,
            inner: Mutex::new(Inner {
                token: None,
                pending: Vec::new(),
            }),
        };
        match load_stored_token(&auth.runtime_dir, &auth.binding, cancel) {
            Ok(stored) => {
                auth.replace_token(stored.token)?;
            }
            Err(DaemonAuthError::TokenMissing) => {}
            Err(err) => return Err(err),
        }
        Ok(auth)
    }

    /// Issue a new OS-user-bound token and persist it with restrictive mode/ACL.
    pub fn issue(&self, cancel: &CancellationToken) -> Result<DaemonTokenHandle, DaemonAuthError> {
        cancel.check().map_err(|_| DaemonAuthError::Cancelled)?;
        ensure_private_dir(&self.runtime_dir)?;
        let binding = binding_from_dir(&self.runtime_dir)?;
        if binding.uid != self.binding.uid {
            return Err(DaemonAuthError::UserMismatch);
        }
        let mut bytes = [0u8; TOKEN_BYTES];
        fill_random(&mut bytes)?;
        let token = DaemonToken { bytes };
        persist_token(&self.runtime_dir, &binding, &token, cancel)?;
        self.replace_token(token)?;
        Ok(DaemonTokenHandle {
            path: token_path(&self.runtime_dir),
            binding,
            byte_len: TOKEN_BYTES,
        })
    }

    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    pub fn token_path(&self) -> PathBuf {
        token_path(&self.runtime_dir)
    }

    pub fn binding(&self) -> &OsUserBinding {
        &self.binding
    }

    pub fn has_token(&self) -> Result<bool, DaemonAuthError> {
        Ok(self.lock()?.token.is_some())
    }

    /// Mint a one-use challenge. Does not reveal or require presenting the token.
    pub fn issue_challenge(
        &self,
        cancel: &CancellationToken,
    ) -> Result<AuthChallenge, DaemonAuthError> {
        cancel.check().map_err(|_| DaemonAuthError::Cancelled)?;
        let mut inner = self.lock()?;
        if inner.token.is_none() {
            return Err(DaemonAuthError::TokenMissing);
        }
        if inner.pending.len() >= MAX_PENDING_CHALLENGES {
            return Err(DaemonAuthError::BoundExceeded {
                limit: MAX_PENDING_CHALLENGES,
                requested: inner.pending.len().saturating_add(1),
            });
        }
        let mut id = [0u8; CHALLENGE_ID_BYTES];
        let mut nonce = [0u8; NONCE_BYTES];
        fill_random(&mut id)?;
        fill_random(&mut nonce)?;
        inner.pending.push(PendingChallenge { id, nonce });
        Ok(AuthChallenge { id, nonce })
    }

    /// Verify a client proof with constant-time comparison. Wrong/missing tokens fail closed.
    pub fn authenticate(
        &self,
        proof: &AuthProof,
        cancel: &CancellationToken,
    ) -> Result<ClientGrant, DaemonAuthError> {
        cancel.check().map_err(|_| DaemonAuthError::Cancelled)?;
        check_token_store_private(&self.runtime_dir)?;
        let mut inner = self.lock()?;
        let pending = take_pending(&mut inner.pending, &proof.challenge_id)?;
        let token = inner.token.as_ref().ok_or(DaemonAuthError::TokenMissing)?;
        let expected = proof_mac(&token.bytes, &pending.id, &pending.nonce, self.binding.uid);
        if !ct_eq(&expected, &proof.response) {
            return Err(DaemonAuthError::TokenMismatch);
        }
        let tag = grant_mac(&token.bytes, self.binding.uid);
        Ok(ClientGrant {
            uid: self.binding.uid,
            tag,
        })
    }

    /// Reject missing/wrong grants before any session API, including enumeration.
    pub fn authorize_session_api(
        &self,
        grant: Option<&ClientGrant>,
        kind: SessionApiKind,
        cancel: &CancellationToken,
    ) -> Result<AuthorizedSessionApi, DaemonAuthError> {
        cancel.check().map_err(|_| DaemonAuthError::Cancelled)?;
        let grant = grant.ok_or(DaemonAuthError::AuthRequired)?;
        let inner = self.lock()?;
        let token = inner.token.as_ref().ok_or(DaemonAuthError::TokenMissing)?;
        if grant.uid != self.binding.uid {
            return Err(DaemonAuthError::UserMismatch);
        }
        let expected = grant_mac(&token.bytes, self.binding.uid);
        if !ct_eq(&expected, &grant.tag) {
            return Err(DaemonAuthError::TokenMismatch);
        }
        Ok(AuthorizedSessionApi { kind })
    }

    /// Run session enumeration only after a live grant is verified.
    pub fn enumerate_sessions<F, T>(
        &self,
        grant: Option<&ClientGrant>,
        cancel: &CancellationToken,
        enumerate: F,
    ) -> Result<T, DaemonAuthError>
    where
        F: FnOnce() -> T,
    {
        self.authorize_session_api(grant, SessionApiKind::Enumerate, cancel)?;
        Ok(enumerate())
    }

    fn replace_token(&self, token: DaemonToken) -> Result<(), DaemonAuthError> {
        let mut inner = self.lock()?;
        if let Some(previous) = inner.token.as_mut() {
            previous.wipe();
        }
        for pending in &mut inner.pending {
            pending.wipe();
        }
        inner.pending.clear();
        inner.token = Some(token);
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>, DaemonAuthError> {
        self.inner.lock().map_err(|_| DaemonAuthError::LockPoisoned)
    }
}

impl LocalDaemonClient {
    /// Open a client against an existing user-private runtime directory.
    pub fn open(
        runtime_dir: impl Into<PathBuf>,
        cancel: &CancellationToken,
    ) -> Result<Self, DaemonAuthError> {
        cancel.check().map_err(|_| DaemonAuthError::Cancelled)?;
        let runtime_dir = validate_runtime_dir(runtime_dir.into())?;
        check_token_store_private(&runtime_dir)?;
        let stored = load_stored_token(&runtime_dir, &binding_from_dir(&runtime_dir)?, cancel)?;
        drop(stored.token);
        Ok(Self {
            runtime_dir,
            binding: stored.binding,
        })
    }

    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    pub fn binding(&self) -> &OsUserBinding {
        &self.binding
    }

    /// CLI arguments that locate the runtime directory. Token bytes cannot be represented.
    pub fn argv(&self) -> Vec<String> {
        vec![
            "--daemon-runtime".to_owned(),
            self.runtime_dir.display().to_string(),
        ]
    }

    /// Compute a challenge proof from the owner-only token file.
    pub fn prove(
        &self,
        challenge: &AuthChallenge,
        cancel: &CancellationToken,
    ) -> Result<AuthProof, DaemonAuthError> {
        cancel.check().map_err(|_| DaemonAuthError::Cancelled)?;
        let stored = load_stored_token(&self.runtime_dir, &self.binding, cancel)?;
        if stored.binding.uid != self.binding.uid {
            return Err(DaemonAuthError::UserMismatch);
        }
        let response = proof_mac(
            &stored.token.bytes,
            &challenge.id,
            &challenge.nonce,
            stored.binding.uid,
        );
        Ok(AuthProof {
            challenge_id: challenge.id,
            response,
        })
    }
}

impl Drop for DaemonAuth {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(token) = inner.token.as_mut() {
                token.wipe();
            }
            for pending in &mut inner.pending {
                pending.wipe();
            }
            inner.pending.clear();
            inner.token = None;
        }
    }
}

impl Drop for DaemonToken {
    fn drop(&mut self) {
        self.wipe();
    }
}

impl Drop for AuthProof {
    fn drop(&mut self) {
        wipe_array(&mut self.response);
    }
}

impl Drop for ClientGrant {
    fn drop(&mut self) {
        wipe_array(&mut self.tag);
    }
}

impl DaemonToken {
    fn wipe(&mut self) {
        wipe_array(&mut self.bytes);
    }
}

impl PendingChallenge {
    fn wipe(&mut self) {
        wipe_array(&mut self.id);
        wipe_array(&mut self.nonce);
    }
}

impl Debug for OsUserBinding {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("OsUserBinding")
            .field("uid", &self.uid)
            .field("username", &self.username)
            .finish()
    }
}

impl Debug for DaemonTokenHandle {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("DaemonTokenHandle")
            .field("path", &self.path)
            .field("binding", &self.binding)
            .field("byte_len", &self.byte_len)
            .field("redacted", &true)
            .finish()
    }
}

impl Debug for AuthChallenge {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthChallenge")
            .field("id", &self.id_hex())
            .finish()
    }
}

impl Debug for AuthProof {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthProof")
            .field("challenge_id", &self.challenge_id_hex())
            .field("redacted", &true)
            .finish()
    }
}

impl Display for AuthProof {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("AuthProof(<redacted>)")
    }
}

impl Debug for ClientGrant {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientGrant")
            .field("uid", &self.uid)
            .field("redacted", &true)
            .finish()
    }
}

impl Debug for DaemonAuth {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("DaemonAuth")
            .field("runtime_dir", &self.runtime_dir)
            .field("binding", &self.binding)
            .field("redacted", &true)
            .finish()
    }
}

impl Debug for LocalDaemonClient {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalDaemonClient")
            .field("runtime_dir", &self.runtime_dir)
            .field("binding", &self.binding)
            .field("redacted", &true)
            .finish()
    }
}

fn token_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join(TOKEN_FILE_NAME)
}

fn tmp_token_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join("daemon.token.tmp")
}

fn validate_runtime_dir(path: PathBuf) -> Result<PathBuf, DaemonAuthError> {
    if path.as_os_str().is_empty() {
        return Err(DaemonAuthError::InvalidPath);
    }
    if path.as_os_str().len() > MAX_RUNTIME_PATH_BYTES {
        return Err(DaemonAuthError::InvalidPath);
    }
    if path.file_name().is_none() {
        return Err(DaemonAuthError::InvalidPath);
    }
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {}
            Component::CurDir | Component::ParentDir => return Err(DaemonAuthError::InvalidPath),
        }
    }
    Ok(path)
}

fn parse_username(raw: &str) -> Result<String, DaemonAuthError> {
    if raw.len() > MAX_USERNAME_BYTES {
        return Err(DaemonAuthError::BoundExceeded {
            limit: MAX_USERNAME_BYTES,
            requested: raw.len(),
        });
    }
    if raw.is_empty() {
        return Ok(String::new());
    }
    if !raw
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(DaemonAuthError::InvalidToken);
    }
    Ok(raw.to_owned())
}

fn current_username() -> Result<String, DaemonAuthError> {
    match std::env::var("USER").or_else(|_| std::env::var("LOGNAME")) {
        Ok(raw) => parse_username(&raw),
        Err(_) => Ok(String::new()),
    }
}

fn binding_from_dir(runtime_dir: &Path) -> Result<OsUserBinding, DaemonAuthError> {
    Ok(OsUserBinding {
        uid: dir_owner_uid(runtime_dir)?,
        username: current_username()?,
    })
}

#[cfg(unix)]
fn dir_owner_uid(path: &Path) -> Result<u32, DaemonAuthError> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::symlink_metadata(path).map_err(|_| DaemonAuthError::Io)?;
    if meta.file_type().is_symlink() || !meta.file_type().is_dir() {
        return Err(DaemonAuthError::InvalidPath);
    }
    Ok(meta.uid())
}

#[cfg(not(unix))]
fn dir_owner_uid(_path: &Path) -> Result<u32, DaemonAuthError> {
    Ok(0)
}

fn ensure_private_dir(path: &Path) -> Result<(), DaemonAuthError> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.file_type().is_dir() {
                return Err(DaemonAuthError::InvalidPath);
            }
            check_dir_private(path)
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
                && !parent.exists()
            {
                fs::create_dir_all(parent).map_err(|_| DaemonAuthError::Io)?;
            }
            fs::create_dir(path).map_err(|_| DaemonAuthError::Io)?;
            set_unix_mode(path, TOKEN_DIR_MODE)?;
            check_dir_private(path)
        }
        Err(_) => Err(DaemonAuthError::Io),
    }
}

fn check_token_store_private(runtime_dir: &Path) -> Result<(), DaemonAuthError> {
    check_dir_private(runtime_dir)?;
    let path = token_path(runtime_dir);
    if !path.exists() {
        return Ok(());
    }
    check_file_private(&path)
}

#[cfg(unix)]
fn set_unix_mode(path: &Path, mode: u32) -> Result<(), DaemonAuthError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|_| DaemonAuthError::InsecurePermissions { mode: 0 })
}

#[cfg(not(unix))]
fn set_unix_mode(_path: &Path, _mode: u32) -> Result<(), DaemonAuthError> {
    Ok(())
}

#[cfg(unix)]
fn check_dir_private(path: &Path) -> Result<(), DaemonAuthError> {
    use std::os::unix::fs::PermissionsExt;
    let meta = fs::symlink_metadata(path).map_err(|_| DaemonAuthError::Io)?;
    if meta.file_type().is_symlink() || !meta.file_type().is_dir() {
        return Err(DaemonAuthError::InvalidPath);
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode != TOKEN_DIR_MODE {
        return Err(DaemonAuthError::InsecurePermissions { mode });
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_dir_private(path: &Path) -> Result<(), DaemonAuthError> {
    let meta = fs::symlink_metadata(path).map_err(|_| DaemonAuthError::Io)?;
    if meta.file_type().is_symlink() || !meta.file_type().is_dir() {
        return Err(DaemonAuthError::InvalidPath);
    }
    Ok(())
}

#[cfg(unix)]
fn check_file_private(path: &Path) -> Result<(), DaemonAuthError> {
    use std::os::unix::fs::PermissionsExt;
    let meta = fs::symlink_metadata(path).map_err(|_| DaemonAuthError::Io)?;
    if meta.file_type().is_symlink() {
        return Err(DaemonAuthError::InsecurePermissions { mode: 0 });
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 || mode & 0o600 != 0o600 {
        return Err(DaemonAuthError::InsecurePermissions { mode });
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_file_private(path: &Path) -> Result<(), DaemonAuthError> {
    let meta = fs::symlink_metadata(path).map_err(|_| DaemonAuthError::Io)?;
    if meta.file_type().is_symlink() {
        return Err(DaemonAuthError::InsecurePermissions { mode: 0 });
    }
    Ok(())
}

fn persist_token(
    runtime_dir: &Path,
    binding: &OsUserBinding,
    token: &DaemonToken,
    cancel: &CancellationToken,
) -> Result<(), DaemonAuthError> {
    cancel.check().map_err(|_| DaemonAuthError::Cancelled)?;
    ensure_private_dir(runtime_dir)?;
    if binding.uid != dir_owner_uid(runtime_dir)? {
        return Err(DaemonAuthError::UserMismatch);
    }
    let dest = token_path(runtime_dir);
    let tmp = tmp_token_path(runtime_dir);
    if tmp.exists() {
        let _ = fs::remove_file(&tmp);
    }
    let encoded = encode_token_file(binding, token);
    {
        let mut file = create_private_file(&tmp)?;
        if let Err(err) = file.write_all(&encoded).and_then(|()| file.sync_all()) {
            let _ = err;
            let _ = fs::remove_file(&tmp);
            return Err(DaemonAuthError::Io);
        }
    }
    if let Err(err) = set_unix_mode(&tmp, TOKEN_FILE_MODE) {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    if let Err(err) = fs::rename(&tmp, &dest) {
        let _ = fs::remove_file(&tmp);
        let _ = err;
        return Err(DaemonAuthError::Io);
    }
    check_file_private(&dest)?;
    check_dir_private(runtime_dir)
}

fn create_private_file(path: &Path) -> Result<File, DaemonAuthError> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(TOKEN_FILE_MODE);
    }
    opts.open(path).map_err(|_| DaemonAuthError::Io)
}

fn load_stored_token(
    runtime_dir: &Path,
    expected: &OsUserBinding,
    cancel: &CancellationToken,
) -> Result<StoredToken, DaemonAuthError> {
    cancel.check().map_err(|_| DaemonAuthError::Cancelled)?;
    check_dir_private(runtime_dir)?;
    let path = token_path(runtime_dir);
    match fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Err(DaemonAuthError::TokenMissing);
        }
        Err(_) => return Err(DaemonAuthError::Io),
    }
    check_file_private(&path)?;
    let mut file = File::open(&path).map_err(|_| DaemonAuthError::Io)?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)
        .map_err(|_| DaemonAuthError::Io)?;
    if buf.len() > MAX_TOKEN_FILE_BYTES {
        wipe_vec(&mut buf);
        return Err(DaemonAuthError::BoundExceeded {
            limit: MAX_TOKEN_FILE_BYTES,
            requested: buf.len(),
        });
    }
    let parsed = match decode_token_file(&buf) {
        Ok(parsed) => parsed,
        Err(err) => {
            wipe_vec(&mut buf);
            return Err(err);
        }
    };
    wipe_vec(&mut buf);
    if parsed.binding.uid != expected.uid || parsed.binding.uid != dir_owner_uid(runtime_dir)? {
        return Err(DaemonAuthError::UserMismatch);
    }
    Ok(parsed)
}

fn encode_token_file(binding: &OsUserBinding, token: &DaemonToken) -> Vec<u8> {
    let username = binding.username.as_bytes();
    let mut out = Vec::with_capacity(4 + 1 + 4 + 1 + username.len() + TOKEN_BYTES);
    out.extend_from_slice(TOKEN_MAGIC);
    out.push(TOKEN_SCHEMA_VERSION as u8);
    out.extend_from_slice(&binding.uid.to_be_bytes());
    out.push(username.len() as u8);
    out.extend_from_slice(username);
    out.extend_from_slice(&token.bytes);
    out
}

fn decode_token_file(bytes: &[u8]) -> Result<StoredToken, DaemonAuthError> {
    if bytes.len() < 4 + 1 + 4 + 1 + TOKEN_BYTES {
        return Err(DaemonAuthError::InvalidToken);
    }
    if bytes[..4] != TOKEN_MAGIC[..] {
        return Err(DaemonAuthError::InvalidToken);
    }
    if bytes[4] as u32 != TOKEN_SCHEMA_VERSION {
        return Err(DaemonAuthError::InvalidToken);
    }
    let uid = u32::from_be_bytes([bytes[5], bytes[6], bytes[7], bytes[8]]);
    let user_len = bytes[9] as usize;
    let token_at = 10 + user_len;
    if user_len > MAX_USERNAME_BYTES {
        return Err(DaemonAuthError::BoundExceeded {
            limit: MAX_USERNAME_BYTES,
            requested: user_len,
        });
    }
    if bytes.len() != token_at + TOKEN_BYTES {
        return Err(DaemonAuthError::InvalidToken);
    }
    let username = parse_username(
        std::str::from_utf8(&bytes[10..token_at]).map_err(|_| DaemonAuthError::InvalidToken)?,
    )?;
    let mut token_bytes = [0u8; TOKEN_BYTES];
    token_bytes.copy_from_slice(&bytes[token_at..]);
    Ok(StoredToken {
        binding: OsUserBinding { uid, username },
        token: DaemonToken { bytes: token_bytes },
    })
}

fn take_pending(
    pending: &mut Vec<PendingChallenge>,
    id: &[u8; CHALLENGE_ID_BYTES],
) -> Result<PendingChallenge, DaemonAuthError> {
    let index = pending
        .iter()
        .position(|item| ct_eq(&item.id, id))
        .ok_or(DaemonAuthError::ChallengeUnknown)?;
    Ok(pending.remove(index))
}

fn proof_mac(
    token: &[u8; TOKEN_BYTES],
    id: &[u8; CHALLENGE_ID_BYTES],
    nonce: &[u8; NONCE_BYTES],
    uid: u32,
) -> [u8; TOKEN_BYTES] {
    let mut message = Vec::with_capacity(HMAC_DOMAIN.len() + CHALLENGE_ID_BYTES + NONCE_BYTES + 4);
    message.extend_from_slice(HMAC_DOMAIN);
    message.extend_from_slice(id);
    message.extend_from_slice(nonce);
    message.extend_from_slice(&uid.to_be_bytes());
    let mac = hmac_sha256(token, &message);
    wipe_vec(&mut message);
    mac
}

fn grant_mac(token: &[u8; TOKEN_BYTES], uid: u32) -> [u8; TOKEN_BYTES] {
    let mut message = Vec::with_capacity(GRANT_DOMAIN.len() + 4);
    message.extend_from_slice(GRANT_DOMAIN);
    message.extend_from_slice(&uid.to_be_bytes());
    let mac = hmac_sha256(token, &message);
    wipe_vec(&mut message);
    mac
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; TOKEN_BYTES] {
    const BLOCK: usize = 64;
    let mut key_block = [0u8; BLOCK];
    if key.len() > BLOCK {
        let hashed = Sha256::digest(key);
        key_block[..hashed.len()].copy_from_slice(&hashed);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= key_block[i];
        opad[i] ^= key_block[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(message);
    let inner_hash = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_hash);
    let digest = outer.finalize();
    let mut mac = [0u8; TOKEN_BYTES];
    mac.copy_from_slice(&digest);
    wipe_array(&mut key_block);
    wipe_array(&mut ipad);
    wipe_array(&mut opad);
    mac
}

fn ct_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut acc = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        acc |= a ^ b;
    }
    acc == 0
}

/// The OS CSPRNG — `getrandom(2)` / `/dev/urandom` on Unix, `BCryptGenRandom`
/// on Windows — through the same crate `uuid` already draws ids from. The
/// Windows arm used to be a bare `Err(Entropy)`, so no daemon token could
/// ever be issued there; the first Windows test run said so twelve times.
fn fill_random(buf: &mut [u8]) -> Result<(), DaemonAuthError> {
    getrandom::fill(buf).map_err(|_| DaemonAuthError::Entropy)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn wipe_array<const N: usize>(buf: &mut [u8; N]) {
    for byte in buf.iter_mut() {
        *byte = 0;
    }
    compiler_fence(Ordering::SeqCst);
}

fn wipe_vec(buf: &mut Vec<u8>) {
    for byte in buf.iter_mut() {
        *byte = 0;
    }
    compiler_fence(Ordering::SeqCst);
    buf.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    struct TempRuntime {
        dir: PathBuf,
    }

    impl TempRuntime {
        fn create() -> Self {
            let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("rapidlm-daemon-auth-{}-{seq}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("temp runtime dir");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&dir, fs::Permissions::from_mode(TOKEN_DIR_MODE))
                    .expect("dir mode");
            }
            Self { dir }
        }
    }

    impl Drop for TempRuntime {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    fn issued() -> (TempRuntime, DaemonAuth, DaemonTokenHandle) {
        let tmp = TempRuntime::create();
        let auth = DaemonAuth::open(&tmp.dir, &live()).expect("open");
        let handle = auth.issue(&live()).expect("issue");
        (tmp, auth, handle)
    }

    fn token_hex(path: &Path) -> String {
        let bytes = fs::read(path).expect("read token file");
        let stored = decode_token_file(&bytes).expect("decode");
        hex_encode(&stored.token.bytes)
    }

    fn assert_no_token(label: &str, rendered: &str, token: &str) {
        assert!(
            !rendered.contains(token),
            "{label} leaked token plaintext: {rendered}"
        );
        assert!(
            !rendered.to_ascii_lowercase().contains(token),
            "{label} leaked token substring: {rendered}"
        );
    }

    #[test]
    fn issue_stores_owner_only_os_user_bound_token() {
        let (tmp, auth, handle) = issued();
        assert_eq!(handle.path(), auth.token_path());
        assert!(
            handle.path().starts_with(&tmp.dir),
            "token lives in the runtime dir"
        );
        assert_eq!(handle.byte_len(), TOKEN_BYTES);
        assert_eq!(handle.binding().uid(), auth.binding().uid());
        assert!(auth.has_token().expect("has token"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let file_mode = fs::symlink_metadata(handle.path())
                .expect("meta")
                .permissions()
                .mode()
                & 0o777;
            let dir_mode = fs::symlink_metadata(&tmp.dir)
                .expect("dir meta")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(file_mode, TOKEN_FILE_MODE);
            assert_eq!(dir_mode, TOKEN_DIR_MODE);
            assert_eq!(
                fs::symlink_metadata(handle.path()).expect("file uid").uid(),
                handle.binding().uid()
            );
        }
    }

    #[test]
    fn challenge_auth_allows_session_enumeration() {
        let (tmp, auth, _) = issued();
        let client = LocalDaemonClient::open(&tmp.dir, &live()).expect("client");
        let challenge = auth.issue_challenge(&live()).expect("challenge");
        let proof = client.prove(&challenge, &live()).expect("prove");
        let grant = auth.authenticate(&proof, &live()).expect("authn");
        let mut enumerated = false;
        let names = auth
            .enumerate_sessions(Some(&grant), &live(), || {
                enumerated = true;
                vec!["s-1".to_owned()]
            })
            .expect("enumerate");
        assert!(enumerated);
        assert_eq!(names, vec!["s-1".to_owned()]);
    }

    #[test]
    fn missing_token_rejected_before_session_enumeration() {
        let (tmp, auth, _) = issued();
        let _ = tmp;
        let enumerated = AtomicBool::new(false);
        let err = auth
            .enumerate_sessions(None, &live(), || {
                enumerated.store(true, Ordering::SeqCst);
                vec!["must-not-run".to_owned()]
            })
            .expect_err("missing grant");
        assert_eq!(err, DaemonAuthError::AuthRequired);
        assert_eq!(err.code(), "auth.required");
        assert!(!err.retryable());
        assert!(
            !enumerated.load(Ordering::SeqCst),
            "session enumeration ran before auth"
        );
    }

    #[test]
    fn wrong_token_rejected_before_session_enumeration() {
        let (tmp, auth, _) = issued();
        let client = LocalDaemonClient::open(&tmp.dir, &live()).expect("client");
        let challenge = auth.issue_challenge(&live()).expect("challenge");
        let mut proof = client.prove(&challenge, &live()).expect("prove");
        proof.response[0] ^= 0xff;
        let err = auth.authenticate(&proof, &live()).expect_err("wrong token");
        assert_eq!(err, DaemonAuthError::TokenMismatch);
        assert_eq!(err.code(), "auth.required");

        let enumerated = AtomicBool::new(false);
        let denied = auth
            .enumerate_sessions(None, &live(), || {
                enumerated.store(true, Ordering::SeqCst);
                vec!["must-not-run".to_owned()]
            })
            .expect_err("still unauthenticated");
        assert_eq!(denied, DaemonAuthError::AuthRequired);
        assert!(!enumerated.load(Ordering::SeqCst));
    }

    #[test]
    fn forged_grant_is_rejected_before_enumeration() {
        let (_tmp, auth, _) = issued();
        let forged = ClientGrant {
            uid: auth.binding().uid(),
            tag: [7u8; TOKEN_BYTES],
        };
        let enumerated = AtomicBool::new(false);
        let err = auth
            .enumerate_sessions(Some(&forged), &live(), || {
                enumerated.store(true, Ordering::SeqCst);
                1
            })
            .expect_err("forged grant");
        assert_eq!(err, DaemonAuthError::TokenMismatch);
        assert!(!enumerated.load(Ordering::SeqCst));
    }

    #[test]
    fn wrong_os_user_token_is_rejected() {
        let (tmp, auth, handle) = issued();
        let mut bytes = fs::read(handle.path()).expect("read");
        // Flip stored uid (bytes 5..9) without changing Unix file owner.
        bytes[8] ^= 0xff;
        fs::write(handle.path(), &bytes).expect("rewrite");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(handle.path(), fs::Permissions::from_mode(TOKEN_FILE_MODE))
                .expect("restore mode");
        }
        let err = LocalDaemonClient::open(&tmp.dir, &live()).expect_err("wrong user");
        assert_eq!(err, DaemonAuthError::UserMismatch);
        assert_eq!(err.code(), "auth.user_mismatch");
        assert_no_token(
            "wrong-user error",
            &err.to_string(),
            &token_hex(handle.path()),
        );
        let _ = auth;
    }

    #[test]
    fn insecure_token_file_is_rejected() {
        let (tmp, auth, handle) = issued();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(handle.path(), fs::Permissions::from_mode(0o644))
                .expect("widen token");
            let err = LocalDaemonClient::open(&tmp.dir, &live()).expect_err("insecure");
            assert!(matches!(err, DaemonAuthError::InsecurePermissions { mode } if mode == 0o644));
            let challenge = auth.issue_challenge(&live());
            // Server also refuses to authenticate against a widened file.
            if let Ok(challenge) = challenge {
                let client_dir = tmp.dir.clone();
                let proof = AuthProof {
                    challenge_id: challenge.id,
                    response: [1u8; TOKEN_BYTES],
                };
                let _ = client_dir;
                let auth_err = auth
                    .authenticate(&proof, &live())
                    .expect_err("insecure authn");
                assert!(matches!(
                    auth_err,
                    DaemonAuthError::InsecurePermissions { .. }
                ));
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (tmp, auth, handle);
        }
    }

    #[test]
    fn token_never_appears_in_logs_or_cli_args() {
        let (tmp, auth, handle) = issued();
        let token = token_hex(handle.path());
        let client = LocalDaemonClient::open(&tmp.dir, &live()).expect("client");
        let challenge = auth.issue_challenge(&live()).expect("challenge");
        let proof = client.prove(&challenge, &live()).expect("prove");
        let grant = auth.authenticate(&proof, &live()).expect("authn");
        let argv = client.argv().join(" ");

        assert_no_token("DaemonAuth Debug", &format!("{auth:?}"), &token);
        assert_no_token("handle Debug", &format!("{handle:?}"), &token);
        assert_no_token("client Debug", &format!("{client:?}"), &token);
        assert_no_token("challenge Debug", &format!("{challenge:?}"), &token);
        assert_no_token("proof Debug", &format!("{proof:?}"), &token);
        assert_no_token("proof Display", &format!("{proof}"), &token);
        assert_no_token("grant Debug", &format!("{grant:?}"), &token);
        assert_no_token("client argv", &argv, &token);
        assert!(argv.contains("--daemon-runtime"));
        assert!(argv.contains(tmp.dir.to_string_lossy().as_ref()));
        assert_no_token(
            "token-mismatch Display",
            &DaemonAuthError::TokenMismatch.to_string(),
            &token,
        );
    }

    #[test]
    fn spent_challenge_cannot_be_replayed() {
        let (tmp, auth, _) = issued();
        let client = LocalDaemonClient::open(&tmp.dir, &live()).expect("client");
        let challenge = auth.issue_challenge(&live()).expect("challenge");
        let proof = client.prove(&challenge, &live()).expect("prove");
        auth.authenticate(&proof, &live()).expect("first use");
        let err = auth.authenticate(&proof, &live()).expect_err("replay");
        assert_eq!(err, DaemonAuthError::ChallengeUnknown);
    }

    #[test]
    fn cancelled_operations_fail_closed() {
        let tmp = TempRuntime::create();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            DaemonAuth::open(&tmp.dir, &cancel).expect_err("cancelled open"),
            DaemonAuthError::Cancelled
        );
        let auth = DaemonAuth::open(&tmp.dir, &live()).expect("open");
        assert_eq!(
            auth.issue(&cancel).expect_err("cancelled issue"),
            DaemonAuthError::Cancelled
        );
        auth.issue(&live()).expect("issue");
        let client = LocalDaemonClient::open(&tmp.dir, &live()).expect("client");
        let challenge = auth.issue_challenge(&live()).expect("challenge");
        assert_eq!(
            client
                .prove(&challenge, &cancel)
                .expect_err("cancelled prove"),
            DaemonAuthError::Cancelled
        );
        assert_eq!(
            auth.authorize_session_api(None, SessionApiKind::Enumerate, &cancel)
                .expect_err("cancelled authz"),
            DaemonAuthError::Cancelled
        );
    }

    #[test]
    fn parent_dir_components_are_rejected() {
        let err = DaemonAuth::open("../escape-runtime", &live()).expect_err("dotdot");
        assert_eq!(err, DaemonAuthError::InvalidPath);
    }

    #[test]
    fn issue_rotates_token_and_invalidates_prior_challenges() {
        let (tmp, auth, first) = issued();
        let client = LocalDaemonClient::open(&tmp.dir, &live()).expect("client");
        let challenge = auth.issue_challenge(&live()).expect("challenge");
        let old_proof = client.prove(&challenge, &live()).expect("old proof");
        let first_hex = token_hex(first.path());
        let second = auth.issue(&live()).expect("rotate");
        assert_ne!(first_hex, token_hex(second.path()));
        let err = auth
            .authenticate(&old_proof, &live())
            .expect_err("stale challenge");
        assert_eq!(err, DaemonAuthError::ChallengeUnknown);
        let fresh = auth.issue_challenge(&live()).expect("fresh");
        let new_client = LocalDaemonClient::open(&tmp.dir, &live()).expect("reopen");
        let proof = new_client.prove(&fresh, &live()).expect("new proof");
        auth.authenticate(&proof, &live()).expect("rotated authn");
    }

    #[test]
    fn session_apis_other_than_enumerate_also_require_grant() {
        let (_tmp, auth, _) = issued();
        for kind in [
            SessionApiKind::Get,
            SessionApiKind::Create,
            SessionApiKind::SubmitTurn,
            SessionApiKind::Subscribe,
        ] {
            let err = auth
                .authorize_session_api(None, kind, &live())
                .expect_err("auth required");
            assert_eq!(err, DaemonAuthError::AuthRequired);
        }
    }

    #[test]
    fn token_schema_constant_is_stable() {
        assert_eq!(TOKEN_SCHEMA, "rapidlm.auth.daemon_token");
        assert_eq!(TOKEN_SCHEMA_VERSION, 1);
        assert_eq!(TOKEN_FILE_NAME, "daemon.token");
    }
}
