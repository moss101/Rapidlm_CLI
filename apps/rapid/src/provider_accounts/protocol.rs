//! Bounded PKCE and device grants. No I/O, discovery, login or ambient identity.
use super::{
    AccountError as Error, AccountFlow, AccountKind, AccountSpec, BodyEncoding, GrantEndpoint,
    MAX_RECORD_BYTES, MAX_TOKEN_BYTES, endpoint, valid_scope,
};
use crate::mcp_oauth::{Pkce, Secret, Url, form_body, same_secret};
use serde_json::Value;

pub fn binding_key(id: &str, definition: &str) -> String {
    ::protocol::provider_replay::scope_key(&["rapidlm.provider-account.v1", id, definition])
}
#[derive(Clone, Debug)]
pub struct Tokens {
    pub access: Secret,
    pub refresh: Option<Secret>,
    pub obtained_at: u64,
    pub expires_at: Option<u64>,
    pub scopes: Vec<String>,
}
impl Tokens {
    pub fn validate(&self) -> Result<(), Error> {
        valid_token(self.access.expose())?;
        if let Some(refresh) = &self.refresh {
            valid_token(refresh.expose())?;
        }
        if self.expires_at.is_some_and(|expiry| {
            expiry <= self.obtained_at || expiry - self.obtained_at > 30 * 24 * 3600
        }) || self.scopes.len() > 32
            || self.scopes.iter().any(|s| !valid_scope(s))
        {
            return Err(Error::InvalidReply);
        }
        Ok(())
    }
    pub fn usable(&self, now: u64) -> bool {
        self.expires_at.is_none_or(|expiry| expiry > now)
    }
    pub fn refresh_due(&self, now: u64) -> bool {
        self.expires_at.is_some_and(|expiry| {
            let ttl = expiry.saturating_sub(self.obtained_at);
            expiry.saturating_sub(now) <= 300.max(ttl / 2).min(ttl)
        })
    }
}
pub fn valid_token(token: &str) -> Result<(), Error> {
    if token.is_empty()
        || token.len() > MAX_TOKEN_BYTES
        || !token.bytes().all(|b| (0x21..=0x7e).contains(&b))
    {
        Err(Error::InvalidReply)
    } else {
        Ok(())
    }
}
/// A grant request body. Debug never exposes codes, verifiers or refresh tokens.
pub struct GrantBody {
    binding: String,
    endpoint: GrantEndpoint,
    encoding: BodyEncoding,
    bytes: Vec<u8>,
}
impl GrantBody {
    pub fn matches(&self, spec: &AccountSpec, endpoint: GrantEndpoint) -> bool {
        self.binding == spec.binding() && self.endpoint == endpoint
    }
    pub fn content_type(&self) -> &'static str {
        match self.encoding {
            BodyEncoding::Form => "application/x-www-form-urlencoded",
            BodyEncoding::Json => "application/json",
        }
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}
impl std::fmt::Debug for GrantBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GrantBody(<private>)")
    }
}
impl Drop for GrantBody {
    fn drop(&mut self) {
        self.bytes.fill(0);
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}
fn body(
    spec: &AccountSpec,
    endpoint: GrantEndpoint,
    pairs: &[(&str, &str)],
) -> Result<GrantBody, Error> {
    let encoding = spec.definition().encoding;
    let bytes = match encoding {
        BodyEncoding::Form => form_body(pairs).into_bytes(),
        BodyEncoding::Json => serde_json::to_vec(
            &pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), Value::String((*v).to_owned())))
                .collect::<serde_json::Map<_, _>>(),
        )
        .map_err(|_| Error::InvalidReply)?,
    };
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(Error::InvalidReply);
    }
    Ok(GrantBody {
        binding: spec.binding().to_owned(),
        endpoint,
        encoding,
        bytes,
    })
}
/// Single-use by ownership: exchanging consumes the attempt, even on bad input.
pub struct PkceAttempt {
    spec: AccountSpec,
    state: Secret,
    pkce: Pkce,
}
impl std::fmt::Debug for PkceAttempt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PkceAttempt")
            .field("account", &self.spec)
            .finish_non_exhaustive()
    }
}
impl PkceAttempt {
    pub fn new(spec: AccountSpec) -> Result<Self, Error> {
        if spec.definition().flow != AccountFlow::Pkce {
            return Err(Error::InvalidConfig);
        }
        Ok(Self {
            spec,
            state: Secret::new(crate::mcp_oauth::new_state().map_err(|_| Error::Storage)?),
            pkce: Pkce::generate().map_err(|_| Error::Storage)?,
        })
    }
    pub fn state(&self) -> &str {
        self.state.expose()
    }
    pub fn authorization_url(&self) -> String {
        let d = self.spec.definition();
        let scope = d.scopes.join(" ");
        let mut pairs = vec![
            ("response_type", "code"),
            ("client_id", d.client_id.as_str()),
            ("redirect_uri", d.redirect_url.as_deref().unwrap_or("")),
            ("state", self.state.expose()),
            ("code_challenge", self.pkce.challenge()),
            ("code_challenge_method", "S256"),
        ];
        if !scope.is_empty() {
            pairs.push(("scope", &scope));
        }
        if d.kind == AccountKind::Anthropic {
            pairs.push(("code", "true"));
        }
        format!(
            "{}?{}",
            d.authorize_url.as_deref().unwrap_or(""),
            form_body(&pairs)
        )
    }
    pub fn exchange(
        self,
        code: &str,
        state: &str,
        issuer: Option<&str>,
    ) -> Result<GrantBody, Error> {
        if !same_secret(state, self.state.expose())
            || issuer.is_some_and(|s| {
                endpoint(s).ok().as_deref() != Some(self.spec.definition().issuer.as_str())
            })
        {
            return Err(Error::BindingMismatch);
        }
        valid_token(code)?;
        let d = self.spec.definition();
        let mut pairs = vec![
            ("grant_type", "authorization_code"),
            ("client_id", d.client_id.as_str()),
            ("code", code),
            ("redirect_uri", d.redirect_url.as_deref().unwrap_or("")),
            ("code_verifier", self.pkce.verifier().expose()),
        ];
        if d.kind == AccountKind::Anthropic {
            pairs.push(("state", state));
        }
        body(&self.spec, GrantEndpoint::Token, &pairs)
    }
}
#[derive(Clone)]
pub struct DeviceGrant {
    binding: String,
    pub device_code: Secret,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub expires_at: u64,
    pub interval: u64,
}
impl std::fmt::Debug for DeviceGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceGrant")
            .field("expires_at", &self.expires_at)
            .field("interval", &self.interval)
            .finish_non_exhaustive()
    }
}
pub fn device_request(spec: &AccountSpec) -> Result<GrantBody, Error> {
    let d = spec.definition();
    if d.flow != AccountFlow::Device {
        return Err(Error::InvalidConfig);
    }
    let scopes = d.scopes.join(" ");
    let mut pairs = vec![("client_id", d.client_id.as_str())];
    if !scopes.is_empty() {
        pairs.push(("scope", &scopes));
    }
    body(spec, GrantEndpoint::Device, &pairs)
}
pub fn parse_device(spec: &AccountSpec, bytes: &[u8], now: u64) -> Result<DeviceGrant, Error> {
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(Error::InvalidReply);
    }
    let v: Value = serde_json::from_slice(bytes).map_err(|_| Error::InvalidReply)?;
    let get = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or(Error::InvalidReply)
    };
    let code = get("device_code")?;
    valid_token(code)?;
    let user = get("user_code")?;
    if user.len() > 128 || !user.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        return Err(Error::InvalidReply);
    }
    let complete_text = v.get("verification_uri_complete").and_then(Value::as_str);
    let base_uri = match v
        .get("verification_uri")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        Some(uri) => endpoint(uri).map_err(|_| Error::InvalidReply)?,
        None if spec.definition().kind == AccountKind::Kimi => {
            let url = Url::parse(complete_text.ok_or(Error::InvalidReply)?)
                .map_err(|_| Error::InvalidReply)?;
            format!("{}{}", url.origin(), url.path())
        }
        None => return Err(Error::InvalidReply),
    };
    let canonical = endpoint(&base_uri).map_err(|_| Error::InvalidReply)?;
    let issuer = Url::parse(
        spec.definition()
            .verification_origin
            .as_deref()
            .unwrap_or(&spec.definition().issuer),
    )
    .map_err(|_| Error::InvalidConfig)?;
    let verification = Url::parse(&canonical).map_err(|_| Error::InvalidReply)?;
    if issuer.origin() != verification.origin() {
        return Err(Error::BindingMismatch);
    }
    let complete = v
        .get("verification_uri_complete")
        .map(|value| -> Result<String, Error> {
            let text = value.as_str().ok_or(Error::InvalidReply)?;
            let url = Url::parse(text).map_err(|_| Error::InvalidReply)?;
            if url.origin() != verification.origin() || !url.is_https() && !url.is_loopback() {
                return Err(Error::BindingMismatch);
            }
            Ok(url.to_string())
        })
        .transpose()?;
    if spec.definition().kind == AccountKind::Kimi && complete.is_none() {
        return Err(Error::InvalidReply);
    }
    let expiry = match v.get("expires_in") {
        None | Some(Value::Null) if spec.definition().kind == AccountKind::Kimi => 900,
        Some(value) => duration_number(spec, value)
            .filter(|n| (1..=3600).contains(n))
            .ok_or(Error::InvalidReply)?,
        None => return Err(Error::InvalidReply),
    };
    let interval = v
        .get("interval")
        .map(|v| {
            duration_number(spec, v)
                .filter(|n| (1..=60).contains(n))
                .ok_or(Error::InvalidReply)
        })
        .transpose()?
        .unwrap_or(5);
    Ok(DeviceGrant {
        binding: spec.binding().to_owned(),
        device_code: Secret::new(code.to_owned()),
        user_code: user.to_owned(),
        verification_uri: canonical,
        verification_uri_complete: complete,
        expires_at: now.checked_add(expiry).ok_or(Error::InvalidReply)?,
        interval,
    })
}
pub fn poll_request(spec: &AccountSpec, device: &DeviceGrant) -> Result<GrantBody, Error> {
    if device.binding != spec.binding() {
        return Err(Error::BindingMismatch);
    }
    body(
        spec,
        GrantEndpoint::Token,
        &[
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("client_id", &spec.definition().client_id),
            ("device_code", device.device_code.expose()),
        ],
    )
}
pub fn refresh_request(spec: &AccountSpec, tokens: &Tokens) -> Result<GrantBody, Error> {
    let refresh = tokens.refresh.as_ref().ok_or(Error::Expired)?;
    body(
        spec,
        GrantEndpoint::Token,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", &spec.definition().client_id),
            ("refresh_token", refresh.expose()),
        ],
    )
}
pub fn revoke_request(spec: &AccountSpec, tokens: &Tokens) -> Result<GrantBody, Error> {
    let (token, hint) = tokens
        .refresh
        .as_ref()
        .map_or((&tokens.access, "access_token"), |r| (r, "refresh_token"));
    body(
        spec,
        GrantEndpoint::Revoke,
        &[
            ("token", token.expose()),
            ("token_type_hint", hint),
            ("client_id", &spec.definition().client_id),
        ],
    )
}
pub fn parse_tokens(
    spec: &AccountSpec,
    status: u16,
    bytes: &[u8],
    now: u64,
    prior: Option<&Tokens>,
) -> Result<Tokens, Error> {
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(Error::InvalidReply);
    }
    let v: Value = serde_json::from_slice(bytes).map_err(|_| {
        if status >= 500 {
            Error::Transient
        } else {
            Error::InvalidReply
        }
    })?;
    if let Some(error) = v.get("error").and_then(Value::as_str) {
        return Err(match error {
            "authorization_pending" => Error::Pending,
            "slow_down" => Error::SlowDown,
            "access_denied" => Error::Denied,
            "expired_token" => Error::Expired,
            "invalid_grant" => Error::Revoked,
            "invalid_client" | "unauthorized_client" => Error::Unauthorized,
            _ if status >= 500 => Error::Transient,
            _ => Error::InvalidReply,
        });
    }
    if !(200..300).contains(&status) {
        return Err(if status >= 500 {
            Error::Transient
        } else if matches!(status, 401 | 403) {
            Error::Unauthorized
        } else {
            Error::InvalidReply
        });
    }
    if let Some(iss) = v.get("iss")
        && iss.as_str().and_then(|s| endpoint(s).ok()).as_deref()
            != Some(spec.definition().issuer.as_str())
    {
        return Err(Error::BindingMismatch);
    }
    if v.get("token_type").is_some_and(|kind| {
        kind.as_str()
            .is_none_or(|s| !s.eq_ignore_ascii_case("bearer"))
    }) {
        return Err(Error::InvalidReply);
    }
    let access = v
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or(Error::InvalidReply)?;
    valid_token(access)?;
    let refresh = match v.get("refresh_token") {
        None | Some(Value::Null) => prior.and_then(|p| p.refresh.clone()),
        Some(Value::String(s)) if !s.is_empty() => {
            valid_token(s)?;
            Some(Secret::new(s.clone()))
        }
        _ => return Err(Error::InvalidReply),
    };
    if spec.definition().kind == AccountKind::Kimi
        && !v
            .get("refresh_token")
            .is_some_and(|value| value.as_str().is_some_and(|token| !token.is_empty()))
    {
        return Err(Error::InvalidReply);
    }
    let lifetime = v
        .get("expires_in")
        .map(|v| {
            duration_number(spec, v)
                .filter(|n| (1..=30 * 24 * 3600).contains(n))
                .ok_or(Error::InvalidReply)
        })
        .transpose()?;
    if lifetime.is_none()
        && matches!(
            spec.definition().kind,
            AccountKind::Anthropic | AccountKind::Kimi
        )
    {
        return Err(Error::InvalidReply);
    }
    let expires_at = lifetime
        .map(|seconds| now.checked_add(seconds).ok_or(Error::InvalidReply))
        .transpose()?;
    let scopes = match v.get("scope") {
        None => prior.map_or_else(|| spec.definition().scopes.clone(), |p| p.scopes.clone()),
        Some(Value::String(s)) => s
            .split(' ')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
        _ => return Err(Error::InvalidReply),
    };
    let tokens = Tokens {
        access: Secret::new(access.to_owned()),
        refresh,
        obtained_at: now,
        expires_at,
        scopes,
    };
    tokens.validate()?;
    Ok(tokens)
}

fn duration_number(spec: &AccountSpec, value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        if spec.definition().kind != AccountKind::Kimi {
            return None;
        }
        let text = value.as_str()?;
        (!text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| text.parse().ok())
            .flatten()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn kimi_token_replies_require_a_refresh_token_for_each_rotation() {
        let mut definition = crate::provider_accounts::tests::spec().definition().clone();
        definition.kind = AccountKind::Kimi;
        definition.flow = AccountFlow::Device;
        definition.authorize_url = None;
        definition.redirect_url = None;
        definition.device_url = Some("http://127.0.0.1:4400/device".into());
        let spec = AccountSpec::new("work", definition).unwrap();
        let valid = parse_tokens(
            &spec,
            200,
            br#"{"access_token":"access","refresh_token":"refresh","expires_in":3600}"#,
            1,
            None,
        )
        .unwrap();
        assert!(
            parse_tokens(
                &spec,
                200,
                br#"{"access_token":"access","refresh_token":"refresh","expires_in":"3600"}"#,
                1,
                None
            )
            .is_ok()
        );
        assert!(matches!(
            parse_tokens(
                &spec,
                200,
                br#"{"access_token":"access","expires_in":3600}"#,
                2,
                Some(&valid)
            ),
            Err(Error::InvalidReply)
        ));
        assert!(matches!(
            parse_tokens(
                &spec,
                200,
                br#"{"access_token":"access","expires_in":3600}"#,
                2,
                None
            ),
            Err(Error::InvalidReply)
        ));
    }
    use crate::provider_accounts::tests::spec;
    #[test]
    fn account_pkce_attempts_are_single_use_state_bound_and_keep_verifiers_out_of_urls_and_debug() {
        let attempt = PkceAttempt::new(spec()).unwrap();
        let url = attempt.authorization_url();
        assert!(url.contains("code_challenge_method=S256"));
        assert!(!url.contains(attempt.pkce.verifier().expose()));
        assert!(!format!("{attempt:?}").contains(attempt.state.expose()));
        let state = attempt.state.expose().to_owned();
        let verifier = attempt.pkce.verifier().expose().to_owned();
        let grant = attempt
            .exchange("private-code-canary", &state, Some("http://127.0.0.1:4400"))
            .unwrap();
        let body = std::str::from_utf8(grant.bytes()).unwrap();
        assert!(body.contains(&format!("code_verifier={verifier}")));
        assert!(!format!("{grant:?}").contains("private-code"));
        assert!(matches!(
            PkceAttempt::new(spec())
                .unwrap()
                .exchange("code", "wrong-state", None),
            Err(Error::BindingMismatch)
        ));
    }
    #[test]
    fn account_token_rotation_keeps_omitted_refresh_values_and_rejects_invalid_issuers_and_secret_output()
     {
        let spec = spec();
        let prior=parse_tokens(&spec,200,br#"{"access_token":"access-first","refresh_token":"refresh-first","expires_in":3600,"scope":"inference","token_type":"Bearer"}"#,1000,None).unwrap();
        assert_eq!(prior.expires_at, Some(4600));
        assert!(!prior.refresh_due(1100));
        assert!(prior.refresh_due(3000));
        let rotated = parse_tokens(
            &spec,
            200,
            br#"{"access_token":"access-second","expires_in":3600}"#,
            3000,
            Some(&prior),
        )
        .unwrap();
        assert_eq!(rotated.refresh.unwrap().expose(), "refresh-first");
        let bad =
            br#"{"access_token":"private-canary","expires_in":3600,"iss":"https://other.example"}"#;
        assert_eq!(
            parse_tokens(&spec, 200, bad, 3000, None).unwrap_err(),
            Error::BindingMismatch
        );
        assert!(
            !format!("{prior:?}{:?}", refresh_request(&spec, &prior).unwrap())
                .contains("refresh-first")
        );
        assert!(matches!(
            parse_tokens(
                &spec,
                400,
                br#"{"error":"invalid_grant","error_description":"secret-canary"}"#,
                3000,
                None
            ),
            Err(Error::Revoked)
        ));
        assert!(matches!(
            parse_tokens(&spec, 503, b"unavailable private-canary", 3000, None),
            Err(Error::Transient)
        ));
        assert!(
            parse_tokens(
                &spec,
                200,
                br#"{"access_token":"bad\r\nheader"}"#,
                3000,
                None
            )
            .is_err()
        );
    }
    #[test]
    fn account_device_grants_handle_pending_slowdown_expiry_and_bound_verification_origins() {
        let mut def = spec().definition().clone();
        def.flow = AccountFlow::Device;
        def.kind = AccountKind::Kimi;
        def.authorize_url = None;
        def.redirect_url = None;
        def.device_url = Some("http://127.0.0.1:4400/device".to_owned());
        let spec = AccountSpec::new("work", def).unwrap();
        let json=br#"{"device_code":"private-device-canary","user_code":"ABCD-1234","verification_uri_complete":"http://127.0.0.1:4400/activate?user_code=ABCD-1234","expires_in":300,"interval":5}"#;
        let grant = parse_device(&spec, json, 1000).unwrap();
        assert_eq!(grant.expires_at, 1300);
        assert_eq!(grant.interval, 5);
        assert!(
            !format!("{grant:?}{:?}", poll_request(&spec, &grant).unwrap())
                .contains("private-device")
        );
        let malicious = String::from_utf8(json.to_vec())
            .unwrap()
            .replace("127.0.0.1:4400/activate", "evil.example/activate");
        assert!(parse_device(&spec, malicious.as_bytes(), 1000).is_err());
        for (error, result) in [
            ("authorization_pending", Error::Pending),
            ("slow_down", Error::SlowDown),
            ("expired_token", Error::Expired),
            ("access_denied", Error::Denied),
        ] {
            let body =
                serde_json::json!({"error":error,"error_description":"private-canary"}).to_string();
            assert_eq!(
                parse_tokens(&spec, 400, body.as_bytes(), 1000, None).unwrap_err(),
                result
            );
        }
    }
}

#[cfg(test)]
mod binding_tests {
    use super::*;
    use crate::provider_accounts::tests::spec;
    #[test]
    fn account_grant_bodies_keep_their_original_issuer_binding_and_endpoint_role() {
        let spec = spec();
        let tokens = Tokens {
            access: Secret::new("access".to_owned()),
            refresh: Some(Secret::new("refresh".to_owned())),
            obtained_at: 1,
            expires_at: Some(100),
            scopes: vec![],
        };
        let grant = refresh_request(&spec, &tokens).unwrap();
        assert!(grant.matches(&spec, GrantEndpoint::Token));
        assert!(!grant.matches(&spec, GrantEndpoint::Device));
        let mut def = spec.definition().clone();
        def.token_url = "http://127.0.0.1:4400/other".to_owned();
        let other = AccountSpec::new("work", def).unwrap();
        assert!(!grant.matches(&other, GrantEndpoint::Token));
    }
    #[test]
    fn account_pkce_keeps_the_registered_redirect_literal_exact() {
        let mut def = spec().definition().clone();
        def.redirect_url = Some("http://localhost:80/callback/".to_owned());
        let spec = AccountSpec::new("work", def).unwrap();
        let attempt = PkceAttempt::new(spec).unwrap();
        let state = attempt.state().to_owned();
        let encoded = crate::mcp_oauth::encode_component("http://localhost:80/callback/");
        assert!(
            attempt
                .authorization_url()
                .contains(&format!("redirect_uri={encoded}"))
        );
        let grant = attempt.exchange("code", &state, None).unwrap();
        assert!(
            std::str::from_utf8(grant.bytes())
                .unwrap()
                .contains(&format!("redirect_uri={encoded}"))
        );
    }
}
