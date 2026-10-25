//! User-configured credential helpers. This is a credential boundary, not an agent job.
//! Output never becomes a log/artifact/tool result. Process control uses the shared group primitives.
use llm_router::{
    CancellationToken, EphemeralCredential, ProfileId, ProviderError, ProviderId, WireAuthorization,
};
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_STDOUT: usize = 16 * 1024;
const MAX_TOKEN: usize = 8 * 1024;
#[derive(Clone, Eq, PartialEq)]
pub struct CredentialProcess {
    argv: Vec<String>,
    timeout_ms: u64,
}
impl fmt::Debug for CredentialProcess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialProcess")
            .field("argv", &"<configured helper>")
            .field("timeout_ms", &self.timeout_ms)
            .finish()
    }
}
impl CredentialProcess {
    pub fn new(argv: Vec<String>, timeout_ms: u64) -> Result<Self, ProviderError> {
        if argv.is_empty()
            || argv.len() > 32
            || !(100..=30_000).contains(&timeout_ms)
            || argv
                .iter()
                .any(|arg| arg.len() > 4096 || arg.chars().any(char::is_control))
            || argv[0].is_empty()
            || argv.iter().map(String::len).sum::<usize>() > 16 * 1024
        {
            return Err(ProviderError::InvalidRequest);
        }
        let program = Path::new(&argv[0]);
        if !program.is_absolute() && program.components().count() != 1 {
            return Err(ProviderError::InvalidRequest);
        }
        Ok(Self { argv, timeout_ms })
    }
    pub fn adc() -> Self {
        Self {
            argv: vec![
                "gcloud".to_owned(),
                "auth".to_owned(),
                "application-default".to_owned(),
                "print-access-token".to_owned(),
                "--quiet".to_owned(),
            ],
            timeout_ms: 10_000,
        }
    }
    pub fn identity(&self) -> String {
        protocol::provider_replay::scope_key(
            &self.argv.iter().map(String::as_str).collect::<Vec<_>>(),
        )
    }
    pub fn argv(&self) -> &[String] {
        &self.argv
    }
    pub fn timeout_ms(&self) -> u64 {
        self.timeout_ms
    }
}

struct AccessToken {
    token: String,
    valid_until: Option<Instant>,
}
impl Drop for AccessToken {
    fn drop(&mut self) {
        let mut bytes = std::mem::take(&mut self.token).into_bytes();
        bytes.fill(0);
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}
pub struct ProcessWireAuth {
    process: CredentialProcess,
    provider: ProviderId,
    profile: ProfileId,
    cache: Mutex<Option<AccessToken>>,
}
impl fmt::Debug for ProcessWireAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProcessWireAuth")
            .field("provider", &self.provider)
            .field("profile", &self.profile)
            .finish_non_exhaustive()
    }
}
impl ProcessWireAuth {
    pub fn new(process: CredentialProcess, provider: ProviderId, profile: ProfileId) -> Self {
        Self {
            process,
            provider,
            profile,
            cache: Mutex::new(None),
        }
    }
}
impl WireAuthorization for ProcessWireAuth {
    fn bearer_token(
        &self,
        credential: &EphemeralCredential,
        cancel: &CancellationToken,
    ) -> Result<String, ProviderError> {
        cancel.check()?;
        if credential.provider() != &self.provider
            || credential.profile() != &self.profile
            || credential.kind() != auth::CredentialKind::OauthAccess
        {
            return Err(ProviderError::AuthFailed);
        }
        let lock_deadline = Instant::now() + Duration::from_millis(self.process.timeout_ms);
        let mut cache = loop {
            cancel.check()?;
            match self.cache.try_lock() {
                Ok(cache) => break cache,
                Err(std::sync::TryLockError::Poisoned(_)) => return Err(ProviderError::AuthFailed),
                Err(std::sync::TryLockError::WouldBlock) => {
                    if Instant::now() >= lock_deadline {
                        return Err(ProviderError::AuthFailed);
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        };
        if let Some(token) = cache.as_ref().filter(|token| {
            token
                .valid_until
                .is_some_and(|expiry| expiry > Instant::now())
        }) {
            return Ok(token.token.clone());
        }
        *cache = None;
        let output = run(&self.process, cancel)?;
        let token = parse_token(&output)?;
        let value = token.token.clone();
        if token.valid_until.is_some() {
            *cache = Some(token);
        }
        Ok(value)
    }
}

fn executable(program: &str) -> Result<PathBuf, ProviderError> {
    let candidate = Path::new(program);
    if candidate.is_absolute() {
        return Ok(candidate.to_owned());
    }
    for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .filter(|dir| dir.is_absolute())
    {
        let candidate = dir.join(program);
        if candidate.is_file() {
            return Ok(candidate);
        }
        #[cfg(windows)]
        for extension in ["exe", "com"] {
            let candidate = dir.join(format!("{program}.{extension}"));
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(ProviderError::AuthFailed)
}
fn helper_environment() -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    let mut env = Vec::new();
    for (key, value) in std::env::vars_os() {
        let name = key.to_string_lossy();
        let folded = name.to_ascii_uppercase();
        let allowed = matches!(
            folded.as_str(),
            "HOME"
                | "USERPROFILE"
                | "SYSTEMROOT"
                | "SYSTEMDRIVE"
                | "COMSPEC"
                | "APPDATA"
                | "LOCALAPPDATA"
                | "PROGRAMFILES"
                | "PROGRAMFILES(X86)"
                | "TMP"
                | "TEMP"
                | "LANG"
                | "LC_ALL"
                | "TZ"
                | "HTTP_PROXY"
                | "HTTPS_PROXY"
                | "NO_PROXY"
                | "GOOGLE_APPLICATION_CREDENTIALS"
                | "GOOGLE_CLOUD_PROJECT"
                | "GOOGLE_CLOUD_QUOTA_PROJECT"
                | "GCLOUD_PROJECT"
        ) || folded.starts_with("CLOUDSDK_");
        if allowed {
            env.push((key, value));
        }
    }
    if let Some(path) = std::env::var_os("PATH")
        && let Ok(path) =
            std::env::join_paths(std::env::split_paths(&path).filter(|dir| dir.is_absolute()))
    {
        env.push(("PATH".into(), path));
    }
    env.push(("CLOUDSDK_CORE_DISABLE_PROMPTS".into(), "1".into()));
    env
}
fn run(spec: &CredentialProcess, cancel: &CancellationToken) -> Result<Vec<u8>, ProviderError> {
    cancel.check()?;
    let program = executable(&spec.argv[0])?;
    let mut command = Command::new(program);
    command
        .args(&spec.argv[1..])
        .env_clear()
        .envs(helper_environment())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // Do not execute from the repository or implicitly consume repository-relative files.
    if let Some(home) = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|home| home.is_absolute() && home.is_dir())
    {
        command.current_dir(home);
    } else {
        command.current_dir(std::env::temp_dir());
    }
    process_signal::isolate_process_group(&mut command);
    let mut child = command.spawn().map_err(|_| ProviderError::AuthFailed)?;
    let stdout = child.stdout.take().ok_or_else(|| {
        process_signal::terminate_process_group_default(&mut child);
        ProviderError::AuthFailed
    })?;
    let (send, receive) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout.take((MAX_STDOUT + 1) as u64).read_to_end(&mut bytes);
        let _ = send.send(if result.is_ok() && bytes.len() <= MAX_STDOUT {
            Ok(bytes)
        } else {
            Err(ProviderError::AuthFailed)
        });
    });
    let deadline = Instant::now() + Duration::from_millis(spec.timeout_ms);
    let mut output = None;
    let mut status = None;
    loop {
        if cancel.is_cancelled() {
            process_signal::terminate_process_group_default(&mut child);
            return Err(ProviderError::Cancelled);
        }
        if Instant::now() >= deadline {
            process_signal::terminate_process_group_default(&mut child);
            return Err(ProviderError::AuthFailed);
        }
        if output.is_none() {
            match receive.try_recv() {
                Ok(Ok(bytes)) => output = Some(bytes),
                Ok(Err(_)) | Err(mpsc::TryRecvError::Disconnected) => {
                    process_signal::terminate_process_group_default(&mut child);
                    return Err(ProviderError::AuthFailed);
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if status.is_none() {
            status = child.try_wait().map_err(|_| {
                process_signal::terminate_process_group_default(&mut child);
                ProviderError::AuthFailed
            })?;
        }
        if status.is_some() && output.is_some() {
            let status = status.ok_or(ProviderError::AuthFailed)?;
            let output = output.take().ok_or(ProviderError::AuthFailed)?;
            // Reap/terminate descendants too: a helper never leaves a background job.
            process_signal::terminate_process_group_default(&mut child);
            return if status.success() {
                Ok(output)
            } else {
                Err(ProviderError::AuthFailed)
            };
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn parse_token(output: &[u8]) -> Result<AccessToken, ProviderError> {
    let text = std::str::from_utf8(output)
        .map_err(|_| ProviderError::AuthFailed)?
        .trim();
    let mut expires_in = None;
    let token = if text.starts_with('{') {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Token {
            access_token: String,
            #[serde(default)]
            token_type: Option<String>,
            #[serde(default)]
            expires_in: Option<u64>,
            #[serde(default)]
            expires_at: Option<u64>,
        }
        let token: Token = serde_json::from_str(text).map_err(|_| ProviderError::AuthFailed)?;
        if token
            .token_type
            .as_ref()
            .is_some_and(|kind| !kind.eq_ignore_ascii_case("bearer"))
            || token.expires_in.is_some() && token.expires_at.is_some()
        {
            return Err(ProviderError::AuthFailed);
        }
        expires_in = token.expires_in;
        if let Some(expiry) = token.expires_at {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| ProviderError::AuthFailed)?
                .as_secs();
            expires_in = Some(
                expiry
                    .checked_sub(now)
                    .filter(|ttl| *ttl > 0)
                    .ok_or(ProviderError::AuthFailed)?,
            );
        }
        token.access_token
    } else {
        text.to_owned()
    };
    if token.is_empty()
        || token.len() > MAX_TOKEN
        || !token.bytes().all(|b| b.is_ascii_graphic())
        || expires_in == Some(0)
    {
        return Err(ProviderError::AuthFailed);
    }
    // Cache only explicit expiry, max1h, refresh30s early. Plain ADC output is reread each request.
    let valid_until = expires_in
        .filter(|ttl| *ttl > 30)
        .map(|ttl| Instant::now() + Duration::from_secs(ttl.min(3600) - 30));
    Ok(AccessToken { token, valid_until })
}
