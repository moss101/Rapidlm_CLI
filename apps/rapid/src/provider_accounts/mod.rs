//! Provider account authorization, separate from MCP server authorization.
//! Protocol and storage are inert until an explicit login or an authorized
//! model request drives them. Secrets never enter account metadata files.
pub mod cli;
pub mod login;
pub mod manager;
pub mod net;
pub mod protocol;
pub mod store;
pub mod wire;

use crate::mcp_oauth::Url;
use serde::{Deserialize, Serialize};

pub const MAX_ACCOUNTS: usize = 64;
pub const MAX_RECORD_BYTES: usize = 64 * 1024;
pub const MAX_TOKEN_BYTES: usize = 8192;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccountError {
    InvalidConfig,
    InvalidReply,
    BindingMismatch,
    Missing,
    Revoked,
    Interrupted,
    Stale,
    Busy,
    Cancelled,
    Storage,
    Keychain,
    Expired,
    Denied,
    Pending,
    SlowDown,
    Transient,
    Unauthorized,
}
impl std::fmt::Display for AccountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "provider account: {self:?}")
    }
}
impl std::error::Error for AccountError {}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AccountKind {
    Standard,
    Anthropic,
    Github,
    Kimi,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AccountFlow {
    Pkce,
    Device,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BodyEncoding {
    Form,
    Json,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GrantEndpoint {
    Device,
    Token,
    Revoke,
}
/// Public registration and endpoint configuration, never tokens or a client secret.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountDefinition {
    pub kind: AccountKind,
    pub flow: AccountFlow,
    pub encoding: BodyEncoding,
    pub provider: String,
    pub issuer: String,
    pub client_id: String,
    pub resource: String,
    pub token_url: String,
    #[serde(default)]
    pub authorize_url: Option<String>,
    #[serde(default)]
    pub device_url: Option<String>,
    #[serde(default)]
    pub revoke_url: Option<String>,
    #[serde(default)]
    pub redirect_url: Option<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub verification_origin: Option<String>,
    #[serde(default)]
    pub exchange_url: Option<String>,
}
#[derive(Clone, Eq, PartialEq)]
pub struct AccountSpec {
    id: String,
    definition: AccountDefinition,
    binding: String,
}
impl std::fmt::Debug for AccountSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountSpec")
            .field("id", &self.id)
            .field("kind", &self.definition.kind)
            .field("provider", &self.definition.provider)
            .finish_non_exhaustive()
    }
}
impl AccountSpec {
    pub fn new(id: &str, mut definition: AccountDefinition) -> Result<Self, AccountError> {
        llm_router::ProfileId::parse(id).map_err(|_| AccountError::InvalidConfig)?;
        llm_router::ProviderId::parse(&definition.provider)
            .map_err(|_| AccountError::InvalidConfig)?;
        if !crate::mcp_oauth::valid_client_id(&definition.client_id)
            || definition.scopes.len() > 32
            || definition.scopes.iter().any(|s| !valid_scope(s))
        {
            return Err(AccountError::InvalidConfig);
        }
        definition.issuer = endpoint(&definition.issuer)?;
        definition.resource = endpoint(&definition.resource)?;
        definition.token_url = endpoint(&definition.token_url)?;
        for value in [
            &mut definition.authorize_url,
            &mut definition.device_url,
            &mut definition.revoke_url,
            &mut definition.verification_origin,
            &mut definition.exchange_url,
        ]
        .into_iter()
        .flatten()
        {
            *value = endpoint(value)?;
        }
        // redirect_uri is a registered literal, not a normalized service URL.
        // Validate it, but preserve explicit default ports and trailing slashes.
        if let Some(redirect) = &definition.redirect_url {
            endpoint(redirect)?;
        }
        if match definition.flow {
            AccountFlow::Pkce => {
                definition.authorize_url.is_none()
                    || definition.redirect_url.is_none()
                    || definition.device_url.is_some()
            }
            AccountFlow::Device => {
                definition.device_url.is_none()
                    || definition.authorize_url.is_some()
                    || definition.redirect_url.is_some()
            }
        } {
            return Err(AccountError::InvalidConfig);
        }
        if matches!(definition.kind, AccountKind::Github | AccountKind::Kimi)
            && definition.flow != AccountFlow::Device
            || definition.kind == AccountKind::Anthropic
                && (definition.flow != AccountFlow::Pkce
                    || definition.encoding != BodyEncoding::Json)
        {
            return Err(AccountError::InvalidConfig);
        }
        if definition.kind == AccountKind::Github
            && (!matches!(
                definition.provider.as_str(),
                "copilot-chat" | "copilot-responses"
            ) || definition.exchange_url.is_none())
            || definition.kind != AccountKind::Github && definition.exchange_url.is_some()
            || definition.kind == AccountKind::Anthropic && definition.provider != "anthropic"
        {
            return Err(AccountError::InvalidConfig);
        }
        definition.scopes.sort();
        definition.scopes.dedup();
        let encoded =
            serde_json::to_string(&definition).map_err(|_| AccountError::InvalidConfig)?;
        let binding = protocol::binding_key(id, &encoded);
        Ok(Self {
            id: id.to_owned(),
            definition,
            binding,
        })
    }
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn definition(&self) -> &AccountDefinition {
        &self.definition
    }
    pub fn binding(&self) -> &str {
        &self.binding
    }
    pub fn accepts_resource(&self, provider: &str, resource: &str) -> bool {
        self.definition.provider == provider
            && endpoint(resource).is_ok_and(|url| url == self.definition.resource)
    }
}
pub(crate) fn endpoint(text: &str) -> Result<String, AccountError> {
    let url = Url::parse(text).map_err(|_| AccountError::InvalidConfig)?;
    if !url.is_https() && !url.is_loopback()
        || text.contains(['?', '#'])
        || llm_router::OpenAiCompatibleEndpoint::new(
            text,
            llm_router::OpenAiApiStyle::ChatCompletions,
        )
        .is_err()
    {
        return Err(AccountError::InvalidConfig);
    }
    // Canonicalize default ports, host case and a redundant trailing slash once.
    Ok(url.to_string().trim_end_matches('/').to_owned())
}
pub(crate) fn valid_scope(scope: &str) -> bool {
    !scope.is_empty()
        && scope.len() <= 128
        && scope
            .bytes()
            .all(|b| (0x21..=0x7e).contains(&b) && b != b'"' && b != b'\\')
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub fn spec() -> AccountSpec {
        AccountSpec::new(
            "work",
            AccountDefinition {
                kind: AccountKind::Standard,
                flow: AccountFlow::Pkce,
                encoding: BodyEncoding::Form,
                provider: "openai-compatible".to_owned(),
                issuer: "http://127.0.0.1:4400".to_owned(),
                client_id: "rapidlm-test-client".to_owned(),
                resource: "http://127.0.0.1:5500/v1".to_owned(),
                token_url: "http://127.0.0.1:4400/token".to_owned(),
                authorize_url: Some("http://127.0.0.1:4400/authorize".to_owned()),
                device_url: None,
                revoke_url: Some("http://127.0.0.1:4400/revoke".to_owned()),
                redirect_url: Some("http://127.0.0.1:6600/callback".to_owned()),
                scopes: vec!["inference".to_owned()],
                verification_origin: None,
                exchange_url: None,
            },
        )
        .unwrap()
    }
    #[test]
    fn account_definitions_bind_provider_resource_issuer_client_and_exact_native_endpoints() {
        let spec = spec();
        let mut def = spec.definition().clone();
        assert!(spec.accepts_resource("openai-compatible", "http://127.0.0.1:5500/v1/"));
        assert!(!spec.accepts_resource("anthropic", "http://127.0.0.1:5500/v1"));
        assert!(!spec.accepts_resource("openai-compatible", "http://127.0.0.1:5500/other"));
        for change in 0..3 {
            let mut changed = def.clone();
            match change {
                0 => changed.client_id = "other-client".to_owned(),
                1 => changed.token_url = "http://127.0.0.1:4400/other".to_owned(),
                _ => changed.resource = "http://127.0.0.1:5500/other".to_owned(),
            }
            assert_ne!(
                spec.binding(),
                AccountSpec::new("work", changed).unwrap().binding()
            );
        }
        for invalid in [
            "http://remote.example/token",
            "https://user:password@example.com/token",
            "https://169.254.169.254/token",
            "https://example.com/token?secret=bad",
        ] {
            def.token_url = invalid.to_owned();
            assert!(AccountSpec::new("work", def.clone()).is_err());
        }
        assert!(AccountSpec::new("../escape", spec.definition().clone()).is_err());
    }
}
