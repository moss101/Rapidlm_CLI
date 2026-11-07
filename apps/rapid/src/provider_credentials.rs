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
    pub fn aws_profile(profile: &str, timeout_ms: u64) -> Result<Self, ProviderError> {
        if profile.is_empty()
            || profile.len() > 128
            || profile.starts_with('-')
            || profile.chars().any(char::is_control)
        {
            return Err(ProviderError::InvalidRequest);
        }
        Self::new(
            vec![
                "aws".to_owned(),
                "configure".to_owned(),
                "export-credentials".to_owned(),
                "--profile".to_owned(),
                profile.to_owned(),
                "--format".to_owned(),
                "process".to_owned(),
                "--no-cli-pager".to_owned(),
                "--no-cli-auto-prompt".to_owned(),
            ],
            timeout_ms,
        )
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
    kind: auth::CredentialKind,
    opaque_tokens: bool,
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
    pub fn with_opaque_tokens(mut self) -> Self {
        self.opaque_tokens = true;
        self
    }
    pub fn with_kind(mut self, kind: auth::CredentialKind) -> Self {
        self.kind = kind;
        self
    }
    pub fn new(process: CredentialProcess, provider: ProviderId, profile: ProfileId) -> Self {
        Self {
            process,
            provider,
            profile,
            kind: auth::CredentialKind::OauthAccess,
            opaque_tokens: false,
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
            || credential.kind() != self.kind
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
        let token = parse_token_with_format(&output, self.opaque_tokens)?;
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
        for extension in ["exe", "com", "cmd", "bat"] {
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
                | "AZURE_CONFIG_DIR"
                | "AZURE_CLIENT_ID"
                | "AZURE_TENANT_ID"
                | "AZURE_CLIENT_SECRET"
                | "AZURE_CLIENT_CERTIFICATE_PATH"
                | "AZURE_CLIENT_CERTIFICATE_PASSWORD"
                | "AZURE_AUTHORITY_HOST"
                | "AZURE_FEDERATED_TOKEN_FILE"
                | "AWS_ACCESS_KEY_ID"
                | "AWS_SECRET_ACCESS_KEY"
                | "AWS_SESSION_TOKEN"
                | "AWS_PROFILE"
                | "AWS_DEFAULT_PROFILE"
                | "AWS_REGION"
                | "AWS_DEFAULT_REGION"
                | "AWS_CONFIG_FILE"
                | "AWS_SHARED_CREDENTIALS_FILE"
                | "AWS_CA_BUNDLE"
                | "AWS_ROLE_ARN"
                | "AWS_ROLE_SESSION_NAME"
                | "AWS_WEB_IDENTITY_TOKEN_FILE"
                | "AWS_EC2_METADATA_DISABLED"
                | "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI"
                | "AWS_CONTAINER_CREDENTIALS_FULL_URI"
                | "AWS_CONTAINER_AUTHORIZATION_TOKEN"
                | "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE"
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
    env.push(("AWS_PAGER".into(), "".into()));
    env.push(("AWS_CLI_AUTO_PROMPT".into(), "off".into()));
    env.push(("CLOUDSDK_CORE_DISABLE_PROMPTS".into(), "1".into()));
    env.push(("CLOUDSDK_CORE_LOG_HTTP".into(), "false".into()));
    env.push(("CLOUDSDK_CORE_DISABLE_FILE_LOGGING".into(), "true".into()));
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
#[cfg(test)]
fn parse_token(output: &[u8]) -> Result<AccessToken, ProviderError> {
    parse_token_with_format(output, false)
}
fn parse_token_with_format(output: &[u8], opaque: bool) -> Result<AccessToken, ProviderError> {
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
        || !token.bytes().all(|b| {
            if opaque {
                (0x21..=0x7e).contains(&b)
            } else {
                b.is_ascii_alphanumeric() || b"-._~+/=".contains(&b)
            }
        })
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

/// AWS signing credentials are resolved only after the model dial is permitted.
/// An explicit process delegates the configured CLI chain; otherwise only the
/// three standard environment credentials are used. Never begins a login.
pub struct AwsWireAuth {
    process: Option<CredentialProcess>,
    provider: ProviderId,
    profile: ProfileId,
    region: String,
    cache: Mutex<Option<AwsAccess>>,
}
struct AwsAccess {
    credentials: llm_router::providers::aws_sigv4::AwsCredentials,
    valid_until: Option<Instant>,
}
impl fmt::Debug for AwsWireAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AwsWireAuth")
            .field("provider", &self.provider)
            .field("profile", &self.profile)
            .finish_non_exhaustive()
    }
}
impl AwsWireAuth {
    pub fn new(
        process: Option<CredentialProcess>,
        provider: ProviderId,
        profile: ProfileId,
        region: String,
    ) -> Self {
        Self {
            process,
            provider,
            profile,
            region,
            cache: Mutex::new(None),
        }
    }
    fn credentials(
        &self,
        cancel: &CancellationToken,
    ) -> Result<llm_router::providers::aws_sigv4::AwsCredentials, ProviderError> {
        let Some(process) = &self.process else {
            let access =
                std::env::var("AWS_ACCESS_KEY_ID").map_err(|_| ProviderError::AuthFailed)?;
            let secret =
                std::env::var("AWS_SECRET_ACCESS_KEY").map_err(|_| ProviderError::AuthFailed)?;
            let token = match std::env::var("AWS_SESSION_TOKEN") {
                Ok(value) => Some(value).filter(|s| !s.is_empty()),
                Err(std::env::VarError::NotPresent) => None,
                Err(_) => return Err(ProviderError::AuthFailed),
            };
            return llm_router::providers::aws_sigv4::AwsCredentials::new(
                &access,
                &secret,
                token.as_deref(),
            );
        };
        let deadline = Instant::now() + Duration::from_millis(process.timeout_ms());
        let mut cache = loop {
            cancel.check()?;
            match self.cache.try_lock() {
                Ok(guard) => break guard,
                Err(std::sync::TryLockError::Poisoned(_)) => return Err(ProviderError::AuthFailed),
                Err(std::sync::TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        return Err(ProviderError::AuthFailed);
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        };
        if let Some(access) = cache.as_ref().filter(|access| {
            access
                .valid_until
                .is_some_and(|expiry| expiry > Instant::now())
        }) {
            return Ok(access.credentials.clone());
        }
        *cache = None;
        let output = run(process, cancel)?;
        let access = parse_aws(&output)?;
        let credentials = access.credentials.clone();
        if access.valid_until.is_some() {
            *cache = Some(access);
        }
        Ok(credentials)
    }
}
impl WireAuthorization for AwsWireAuth {
    fn bearer_token(
        &self,
        _: &EphemeralCredential,
        _: &CancellationToken,
    ) -> Result<String, ProviderError> {
        Err(ProviderError::AuthFailed)
    }
    fn aws_sigv4(
        &self,
        credential: &EphemeralCredential,
        request: &llm_router::providers::aws_sigv4::SignableRequest<'_>,
        cancel: &CancellationToken,
    ) -> Result<llm_router::providers::aws_sigv4::AwsAuthorization, ProviderError> {
        cancel.check()?;
        if credential.provider() != &self.provider
            || credential.profile() != &self.profile
            || credential.kind() != auth::CredentialKind::AwsSigning
        {
            return Err(ProviderError::AuthFailed);
        }
        let credentials = self.credentials(cancel)?;
        cancel.check()?;
        let now = time::OffsetDateTime::now_utc();
        let date = format!(
            "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
            now.year(),
            u8::from(now.month()),
            now.day(),
            now.hour(),
            now.minute(),
            now.second()
        );
        llm_router::providers::aws_sigv4::sign(
            request,
            &credentials,
            &self.region,
            "bedrock",
            &date,
        )
    }
}
fn parse_aws(output: &[u8]) -> Result<AwsAccess, ProviderError> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "PascalCase", deny_unknown_fields)]
    struct Wire {
        version: u32,
        access_key_id: String,
        secret_access_key: String,
        #[serde(default)]
        session_token: Option<String>,
        #[serde(default)]
        expiration: Option<String>,
    }
    let wire: Wire = serde_json::from_slice(output).map_err(|_| ProviderError::AuthFailed)?;
    if wire.version != 1 {
        return Err(ProviderError::AuthFailed);
    }
    let credentials = llm_router::providers::aws_sigv4::AwsCredentials::new(
        &wire.access_key_id,
        &wire.secret_access_key,
        wire.session_token.as_deref(),
    )?;
    let valid_until = if let Some(expiry) = wire.expiration {
        let expiry =
            time::OffsetDateTime::parse(&expiry, &time::format_description::well_known::Rfc3339)
                .map_err(|_| ProviderError::AuthFailed)?;
        let remaining = (expiry - time::OffsetDateTime::now_utc()).whole_seconds();
        if remaining <= 0 {
            return Err(ProviderError::AuthFailed);
        }
        (remaining > 30)
            .then(|| Instant::now() + Duration::from_secs((remaining as u64).min(3600) - 30))
    } else {
        None
    };
    Ok(AwsAccess {
        credentials,
        valid_until,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use auth::{
        CredentialKind, CredentialPut, CredentialStore, InMemoryCredentialStore, SecretRef,
        SecretValue,
    };
    fn process(script: &str, timeout: u64) -> CredentialProcess {
        CredentialProcess::new(
            vec![
                test_fixtures::tool_str("sh"),
                "-c".to_owned(),
                script.to_owned(),
            ],
            timeout,
        )
        .unwrap()
    }
    fn credential() -> EphemeralCredential {
        let store = InMemoryCredentialStore::new();
        let id = SecretRef::from_alias("process:test").unwrap();
        store
            .put(
                CredentialPut::ephemeral(
                    SecretValue::from_str(id.clone(), "helper-authority").unwrap(),
                    CredentialKind::OauthAccess,
                )
                .with_provider("vertex-genai")
                .unwrap(),
                &auth::CancellationToken::new(),
            )
            .unwrap();
        let profile = llm_router::ProviderProfile::with_kind(
            "work",
            ProviderId::parse("vertex-genai").unwrap(),
            id,
            CredentialKind::OauthAccess,
        )
        .unwrap();
        llm_router::CredentialResolver::new(&store)
            .resolve(profile.provider(), &profile, &CancellationToken::new())
            .unwrap()
    }
    fn wire(spec: CredentialProcess) -> ProcessWireAuth {
        ProcessWireAuth::new(
            spec,
            ProviderId::parse("vertex-genai").unwrap(),
            ProfileId::parse("work").unwrap(),
        )
    }
    fn aws_credential() -> EphemeralCredential {
        let store = InMemoryCredentialStore::new();
        let id = SecretRef::from_alias("aws-process:test").unwrap();
        store
            .put(
                CredentialPut::ephemeral(
                    SecretValue::from_str(id.clone(), "authority").unwrap(),
                    CredentialKind::AwsSigning,
                )
                .with_provider("bedrock")
                .unwrap(),
                &auth::CancellationToken::new(),
            )
            .unwrap();
        let profile = llm_router::ProviderProfile::with_kind(
            "work",
            ProviderId::parse("bedrock").unwrap(),
            id,
            CredentialKind::AwsSigning,
        )
        .unwrap();
        llm_router::CredentialResolver::new(&store)
            .resolve(profile.provider(), &profile, &CancellationToken::new())
            .unwrap()
    }
    #[test]
    fn aws_process_credentials_are_strict_redacted_and_refresh_before_expiry() {
        let root =
            std::env::temp_dir().join(format!("rapid-aws-helper-{}", protocol::SessionId::new()));
        std::fs::create_dir_all(&root).unwrap();
        let counter = root.join("count");
        let expiry = (time::OffsetDateTime::now_utc() + time::Duration::hours(1))
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        let output=serde_json::json!({"Version":1,"AccessKeyId":"AKIDEXAMPLE","SecretAccessKey":"private-aws-secret","SessionToken":"private-aws-session","Expiration":expiry}).to_string();
        let spec = CredentialProcess::new(
            vec![
                test_fixtures::tool_str("sh"),
                "-c".to_owned(),
                "printf x >> \"$1\"; printf '%s' \"$2\"".to_owned(),
                "fixture".to_owned(),
                counter.to_string_lossy().into_owned(),
                output,
            ],
            1000,
        )
        .unwrap();
        let wire = AwsWireAuth::new(
            Some(spec),
            ProviderId::parse("bedrock").unwrap(),
            ProfileId::parse("work").unwrap(),
            "us-east-1".to_owned(),
        );
        let request = llm_router::providers::aws_sigv4::SignableRequest {
            method: "POST",
            host: "owned.example",
            path_and_query: "/model/test/converse-stream",
            headers: &[],
            body: b"{}",
            payload_hash_header: true,
        };
        let credential = aws_credential();
        let cancel = CancellationToken::new();
        for _ in 0..2 {
            let auth = wire.aws_sigv4(&credential, &request, &cancel).unwrap();
            assert!(
                auth.authorization()
                    .contains("/us-east-1/bedrock/aws4_request")
            );
            assert_eq!(auth.session_token(), Some("private-aws-session"));
            assert!(!format!("{auth:?}{wire:?}").contains("private-aws"));
        }
        assert_eq!(std::fs::read_to_string(&counter).unwrap(), "x");
        wire.cache.lock().unwrap().as_mut().unwrap().valid_until =
            Some(Instant::now() - Duration::from_secs(1));
        wire.aws_sigv4(&credential, &request, &cancel).unwrap();
        assert_eq!(std::fs::read_to_string(&counter).unwrap(), "xx");
        assert!(wire.bearer_token(&credential, &cancel).is_err());
        for bad in [
            serde_json::json!({"Version":2,"AccessKeyId":"AKID","SecretAccessKey":"secret"}),
            serde_json::json!({"Version":1,"AccessKeyId":"AKID","SecretAccessKey":"secret","Expiration":"2000-01-01T00:00:00Z"}),
            serde_json::json!({"Version":1,"AccessKeyId":"AKID","SecretAccessKey":"secret","Expiration":"not-a-date"}),
            serde_json::json!({"Version":1,"AccessKeyId":"AKID","SecretAccessKey":"secret","Unknown":true}),
        ] {
            assert!(parse_aws(bad.to_string().as_bytes()).is_err());
        }
        assert!(CredentialProcess::aws_profile("--debug", 1000).is_err());
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn a_credential_helper_is_bounded_redacted_argv_and_accepts_only_valid_tokens() {
        let token = wire(process(
            "printf 'private-helper-token'; printf 'private-stderr-token' >&2",
            1000,
        ));
        assert_eq!(
            token
                .bearer_token(&credential(), &CancellationToken::new())
                .unwrap(),
            "private-helper-token"
        );
        assert!(!format!("{token:?}").contains("private-helper-token"));
        assert!(
            !format!("{:?}", process("printf 'argument-secret'", 1000)).contains("argument-secret")
        );
        for output in [
            "",
            "Bearer token",
            "{\"access_token\":\"token\",\"token_type\":\"Basic\"}",
            "{\"access_token\":\"token\",\"expires_in\":0}",
            "{\"access_token\":\"token\",\"expires_at\":1}",
            "{\"access_token\":\"token\",\"extra\":true}",
            "\"quoted-token\"",
        ] {
            assert!(parse_token(output.as_bytes()).is_err(), "{output}");
        }
        assert!(parse_token(&vec![b'x'; MAX_TOKEN + 1]).is_err());
        for (argv, timeout) in [
            (vec![], 1000),
            (vec!["./repo-helper".to_owned()], 1000),
            (vec!["tool".to_owned()], 0),
            (vec!["tool".to_owned()], 30_001),
            (vec!["tool".to_owned(), "bad\nargument".to_owned()], 1000),
        ] {
            assert!(CredentialProcess::new(argv, timeout).is_err());
        }
    }
    #[test]
    fn helper_failure_overflow_timeout_and_cancellation_never_return_partial_tokens() {
        let overflowing = format!(
            "{} if=/dev/zero bs=20000 count=1 2>/dev/null",
            test_fixtures::tool_str("dd")
        );
        for spec in [
            process("printf 'partial-private-token'; exit 1", 1000),
            process(&overflowing, 1000),
            process("printf 'partial-private-token'; sleep 5", 100),
        ] {
            let start = Instant::now();
            let error = wire(spec)
                .bearer_token(&credential(), &CancellationToken::new())
                .unwrap_err();
            assert_eq!(error, ProviderError::AuthFailed);
            assert!(start.elapsed() < Duration::from_secs(3));
            assert!(!format!("{error:?}").contains("partial-private-token"));
        }
        let cancel = CancellationToken::new();
        let watched = cancel.clone();
        let cancelling = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            watched.cancel();
        });
        let start = Instant::now();
        assert_eq!(
            wire(process("sleep 5; printf stale-token", 10_000))
                .bearer_token(&credential(), &cancel),
            Err(ProviderError::Cancelled)
        );
        assert!(start.elapsed() < Duration::from_secs(2));
        cancelling.join().unwrap();
    }
    #[test]
    fn helper_json_expiry_caches_then_refreshes_without_reusing_an_expired_token() {
        let root = std::env::temp_dir().join(format!(
            "rapid-credential-helper-{}",
            protocol::SessionId::new()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let counter = root.join("count");
        let script = "printf x >> \"$1\"; printf '{\"access_token\":\"fresh-token\",\"expires_in\":3600,\"token_type\":\"Bearer\"}'";
        let spec = CredentialProcess::new(
            vec![
                test_fixtures::tool_str("sh"),
                "-c".to_owned(),
                script.to_owned(),
                "fixture".to_owned(),
                counter.to_string_lossy().into_owned(),
            ],
            1000,
        )
        .unwrap();
        let auth = wire(spec);
        let credential = credential();
        let cancel = CancellationToken::new();
        for _ in 0..2 {
            assert_eq!(
                auth.bearer_token(&credential, &cancel).unwrap(),
                "fresh-token"
            );
        }
        assert_eq!(std::fs::read_to_string(&counter).unwrap(), "x");
        auth.cache.lock().unwrap().as_mut().unwrap().valid_until =
            Some(Instant::now() - Duration::from_secs(1));
        assert_eq!(
            auth.bearer_token(&credential, &cancel).unwrap(),
            "fresh-token"
        );
        assert_eq!(std::fs::read_to_string(&counter).unwrap(), "xx");
        let _ = std::fs::remove_dir_all(root);
    }
}
