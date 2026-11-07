//! Copilot inference token exchange. No account discovery, login or redirects.
use llm_router::providers::{
    copilot::CopilotOptions,
    dial::{DialGate, ProxyConfig},
    openai_compatible::{Http1Transport, ProviderHttpRequest, classify_http_error},
};
use llm_router::{
    CancellationToken, EphemeralCredential, ProfileId, ProviderError, ProviderId, WireAuthorization,
};
use std::{
    fmt,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

// Keeps dynamic source authentication local to this boundary.
struct Source(Box<dyn WireAuthorization>);
impl WireAuthorization for Source {
    fn bearer_token(
        &self,
        credential: &EphemeralCredential,
        cancel: &CancellationToken,
    ) -> Result<String, ProviderError> {
        self.0.bearer_token(credential, cancel)
    }
}
struct ApiToken {
    value: String,
    valid_until: Option<Instant>,
}
impl Drop for ApiToken {
    fn drop(&mut self) {
        let mut bytes = std::mem::take(&mut self.value).into_bytes();
        bytes.fill(0);
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}
pub struct CopilotWireAuth {
    options: CopilotOptions,
    provider: ProviderId,
    profile: ProfileId,
    transport: Http1Transport<Source>,
    cache: Mutex<Option<ApiToken>>,
}
impl fmt::Debug for CopilotWireAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CopilotWireAuth")
            .field("provider", &self.provider)
            .field("profile", &self.profile)
            .finish_non_exhaustive()
    }
}
impl CopilotWireAuth {
    pub fn new(
        options: CopilotOptions,
        source: Box<dyn WireAuthorization>,
        provider: ProviderId,
        profile: ProfileId,
        proxy: Option<&ProxyConfig>,
        gate: Option<Arc<dyn DialGate>>,
    ) -> Result<Self, ProviderError> {
        if options.token_url().is_none() {
            return Err(ProviderError::InvalidRequest);
        }
        let mut transport =
            Http1Transport::with_limits(Source(source), Duration::from_secs(10), 16 * 1024);
        if let Some(proxy) = proxy {
            transport = transport.with_proxy(proxy.clone());
        }
        if let Some(gate) = gate {
            transport = transport.with_dial_gate(gate);
        }
        Ok(Self {
            options,
            provider,
            profile,
            transport,
            cache: Mutex::new(None),
        })
    }
}
impl WireAuthorization for CopilotWireAuth {
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
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut cache = loop {
            cancel.check()?;
            match self.cache.try_lock() {
                Ok(cache) => break cache,
                Err(std::sync::TryLockError::Poisoned(_)) => return Err(ProviderError::AuthFailed),
                Err(std::sync::TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        return Err(ProviderError::AuthFailed);
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        };
        if let Some(token) = cache
            .as_ref()
            .filter(|t| t.valid_until.is_some_and(|expiry| expiry > Instant::now()))
        {
            return Ok(token.value.clone());
        }
        *cache = None;
        let mut headers = self.options.identity_headers();
        headers.push(("Accept".to_owned(), "application/json".to_owned()));
        let response = self.transport.execute_get(
            &ProviderHttpRequest::new(
                self.options.token_url().ok_or(ProviderError::AuthFailed)?,
                &headers,
                &[],
                credential,
            ),
            cancel,
        )?;
        classify_http_error(&response)?;
        // Non-2xx, including redirects, are never followed or interpreted as tokens.
        if response.status() != 200 {
            return Err(ProviderError::AuthFailed);
        }
        let token = parse_token(response.body())?;
        cancel.check()?;
        let value = token.value.clone();
        if token.valid_until.is_some() {
            *cache = Some(token);
        }
        Ok(value)
    }
}
fn parse_token(bytes: &[u8]) -> Result<ApiToken, ProviderError> {
    #[derive(serde::Deserialize)]
    struct Response {
        token: String,
        expires_at: u64,
    }
    let response: Response =
        serde_json::from_slice(bytes).map_err(|_| ProviderError::AuthFailed)?;
    if response.token.is_empty()
        || response.token.len() > 8192
        || !response.token.bytes().all(|b| (0x21..=0x7e).contains(&b))
    {
        return Err(ProviderError::AuthFailed);
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ProviderError::AuthFailed)?
        .as_secs();
    let ttl = response
        .expires_at
        .checked_sub(now)
        .filter(|ttl| *ttl > 0)
        .ok_or(ProviderError::AuthFailed)?;
    let valid_until = (ttl > 30).then(|| Instant::now() + Duration::from_secs(ttl.min(3600) - 30));
    Ok(ApiToken {
        value: response.token,
        valid_until,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn copilot_exchange_tokens_require_expiry_and_accept_opaque_signed_values_without_trusting_metadata()
     {
        let expiry = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        let bytes = serde_json::json!({"token":"tid=test;exp=123;sku=fixture:hmac","expires_at":expiry,"endpoints":{"api":"https://untrusted.example"},"refresh_in":1800}).to_string();
        let token = parse_token(bytes.as_bytes()).unwrap();
        assert_eq!(token.value, "tid=test;exp=123;sku=fixture:hmac");
        assert!(token.valid_until.is_some());
        for bad in [
            serde_json::json!({"token":"canary", "expires_at":1}),
            serde_json::json!({"token":"canary"}),
            serde_json::json!({"token":"canary\r\nheader", "expires_at":expiry}),
            serde_json::json!({"token":"Bearer canary", "expires_at":expiry}),
            serde_json::json!({"token":"", "expires_at":expiry}),
        ] {
            assert!(parse_token(bad.to_string().as_bytes()).is_err());
        }
        let near =
            serde_json::json!({"token":"valid-once", "expires_at":expiry - 3590}).to_string();
        assert!(parse_token(near.as_bytes()).unwrap().valid_until.is_none());
    }
}
