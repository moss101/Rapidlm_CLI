//! Explicit GitHub Copilot Chat/Responses routing and request identity.
//! Account acquisition is separate; no borrowed editor identity or ambient login.
use super::openai_compatible::{OpenAiApiStyle, OpenAiCompatibleEndpoint, credential_endpoint};
use crate::{CanonicalModelRequest, ContentPart, MessageRole, ProviderError};

pub const PUBLIC_API_URL: &str = "https://api.githubcopilot.com";
pub const PUBLIC_TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CopilotAuth {
    /// An already exchanged Copilot inference token.
    #[default]
    ApiToken,
    /// A GitHub credential exchanged for a separate expiring inference token.
    GithubToken,
}
#[derive(Clone, Eq, PartialEq)]
pub struct CopilotOptions {
    auth: CopilotAuth,
    token_url: Option<String>,
    integration_id: String,
    token_url_explicit: bool,
}
impl std::fmt::Debug for CopilotOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CopilotOptions")
            .field("auth", &self.auth)
            .finish_non_exhaustive()
    }
}
impl CopilotOptions {
    pub fn new(
        auth: CopilotAuth,
        token_url: Option<&str>,
        integration_id: &str,
    ) -> Result<Self, ProviderError> {
        if integration_id.is_empty()
            || integration_id.len() > 128
            || !integration_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            || auth == CopilotAuth::ApiToken && token_url.is_some()
        {
            return Err(ProviderError::InvalidRequest);
        }
        let token_url_explicit = token_url.is_some();
        let token_url = if auth == CopilotAuth::GithubToken {
            Some(credential_endpoint(token_url.unwrap_or(PUBLIC_TOKEN_URL))?)
        } else {
            None
        };
        Ok(Self {
            auth,
            token_url,
            integration_id: integration_id.to_owned(),
            token_url_explicit,
        })
    }
    pub fn auth(&self) -> CopilotAuth {
        self.auth
    }
    pub fn token_url(&self) -> Option<&str> {
        self.token_url.as_deref()
    }
    pub fn integration_id(&self) -> &str {
        &self.integration_id
    }
    pub fn endpoint(
        &self,
        base: &str,
        style: OpenAiApiStyle,
    ) -> Result<OpenAiCompatibleEndpoint, ProviderError> {
        // A custom enterprise/proxy inference origin must not implicitly receive
        // an account credential through the public token exchange.
        if self.auth == CopilotAuth::GithubToken
            && !self.token_url_explicit
            && credential_endpoint(base)? != credential_endpoint(PUBLIC_API_URL)?
        {
            return Err(ProviderError::InvalidRequest);
        }
        OpenAiCompatibleEndpoint::copilot(&credential_endpoint(base)?, style, self.clone())
    }
    pub fn scope(&self, request_url: &str) -> String {
        format!(
            "{request_url}|copilot={:?}|token={}|integration={}",
            self.auth,
            self.token_url.as_deref().unwrap_or(""),
            self.integration_id
        )
    }
    pub fn identity_headers(&self) -> Vec<(String, String)> {
        vec![
            (
                "User-Agent".to_owned(),
                format!("RapidLM/{}", env!("CARGO_PKG_VERSION")),
            ),
            (
                "Editor-Version".to_owned(),
                format!("rapidlm/{}", env!("CARGO_PKG_VERSION")),
            ),
            (
                "Editor-Plugin-Version".to_owned(),
                format!("rapidlm/{}", env!("CARGO_PKG_VERSION")),
            ),
            (
                "Copilot-Integration-Id".to_owned(),
                self.integration_id.clone(),
            ),
        ]
    }
    pub fn request_headers(&self, request: &CanonicalModelRequest) -> Vec<(String, String)> {
        let mut headers = self.identity_headers();
        headers.push(("Openai-Intent".to_owned(), "conversation-edits".to_owned()));
        // Synthetic user tool results must not reset an autonomous continuation
        // to a human-originated call. Inspect canonical history, not model names.
        let agent = request
            .messages()
            .iter()
            .any(|m| matches!(m.role(), MessageRole::Assistant | MessageRole::Tool));
        headers.push((
            "X-Initiator".to_owned(),
            if agent { "agent" } else { "user" }.to_owned(),
        ));
        if request
            .messages()
            .iter()
            .flat_map(|m| m.parts())
            .any(|p| matches!(p, ContentPart::Image { .. } | ContentPart::ImageData { .. }))
        {
            headers.push(("Copilot-Vision-Request".to_owned(), "true".to_owned()));
        }
        headers
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CancellationToken, CanonicalMessage, CatalogRevision, ModelId, ModelPurpose, ModelRef,
        ModelRequestId, ProviderId,
    };
    use protocol::{Baggage, TraceContext, TraceId};
    fn request(messages: Vec<CanonicalMessage>) -> CanonicalModelRequest {
        CanonicalModelRequest::new(
            ModelRequestId::parse("copilot-request").unwrap(),
            ModelRef::new(
                ProviderId::parse("copilot-chat").unwrap(),
                ModelId::parse("fixture-model").unwrap(),
            ),
            ModelPurpose::Code,
            messages,
            vec![],
            Some(16),
            CatalogRevision::new(1).unwrap(),
            TraceContext::new(TraceId::new(), None, Baggage::empty()),
            &CancellationToken::new(),
        )
        .unwrap()
    }
    #[test]
    fn copilot_routes_and_auth_bindings_are_explicit_secure_and_scoped() {
        let direct = CopilotOptions::new(CopilotAuth::ApiToken, None, "rapidlm-test").unwrap();
        let github = CopilotOptions::new(CopilotAuth::GithubToken, None, "rapidlm-test").unwrap();
        for style in [OpenAiApiStyle::ChatCompletions, OpenAiApiStyle::Responses] {
            let endpoint = direct.endpoint(PUBLIC_API_URL, style).unwrap();
            assert_eq!(
                endpoint.request_url(),
                OpenAiCompatibleEndpoint::new(PUBLIC_API_URL, style)
                    .unwrap()
                    .request_url()
            );
            assert_ne!(
                endpoint.replay_endpoint(),
                github
                    .endpoint(PUBLIC_API_URL, style)
                    .unwrap()
                    .replay_endpoint()
            );
        }
        assert!(
            github
                .endpoint(
                    "https://copilot-api.company.ghe.com",
                    OpenAiApiStyle::Responses
                )
                .is_err()
        );
        let enterprise = CopilotOptions::new(
            CopilotAuth::GithubToken,
            Some("https://api.company.ghe.com/copilot_internal/v2/token"),
            "my-enterprise",
        )
        .unwrap();
        assert!(
            enterprise
                .endpoint(
                    "https://copilot-api.company.ghe.com",
                    OpenAiApiStyle::ChatCompletions
                )
                .is_ok()
        );
        assert!(
            CopilotOptions::new(
                CopilotAuth::GithubToken,
                Some(PUBLIC_TOKEN_URL),
                "my-product"
            )
            .unwrap()
            .endpoint(
                "https://api.business.githubcopilot.com",
                OpenAiApiStyle::Responses
            )
            .is_ok()
        );
        assert!(
            direct
                .endpoint("http://remote.example", OpenAiApiStyle::Responses)
                .is_err()
        );
        for url in [
            "http://169.254.169.254/token",
            "https://user:password@example.com/token",
            "https://example.com/token?secret=bad",
            "http://remote.example/token",
        ] {
            assert!(
                CopilotOptions::new(CopilotAuth::GithubToken, Some(url), "rapidlm-test").is_err()
            );
        }
        assert!(
            CopilotOptions::new(CopilotAuth::ApiToken, Some(PUBLIC_TOKEN_URL), "test").is_err()
        );
        assert!(CopilotOptions::new(CopilotAuth::ApiToken, None, "bad\r\nAuthorization").is_err());
    }
    #[test]
    fn copilot_headers_identify_rapidlm_and_preserve_agent_and_vision_context() {
        let options = CopilotOptions::new(CopilotAuth::ApiToken, None, "my-product").unwrap();
        let user = CanonicalMessage::new(
            MessageRole::User,
            vec![ContentPart::text("task").unwrap()],
            None,
            vec![],
        )
        .unwrap();
        let initial = request(vec![user.clone()]);
        let headers = options.request_headers(&initial);
        assert!(headers.contains(&("X-Initiator".to_owned(), "user".to_owned())));
        assert!(headers.contains(&("Copilot-Integration-Id".to_owned(), "my-product".to_owned())));
        assert!(
            headers
                .iter()
                .any(|(name, value)| name == "Editor-Plugin-Version"
                    && value.starts_with("rapidlm/"))
        );
        assert!(!format!("{headers:?}").contains("vscode"));
        let assistant = CanonicalMessage::new(
            MessageRole::Assistant,
            vec![ContentPart::text("continuing").unwrap()],
            None,
            vec![],
        )
        .unwrap();
        let image = CanonicalMessage::new(
            MessageRole::User,
            vec![ContentPart::image_data("data:image/png;base64,aGVsbG8=").unwrap()],
            None,
            vec![],
        )
        .unwrap();
        let headers = options.request_headers(&request(vec![user, assistant, image]));
        assert!(headers.contains(&("X-Initiator".to_owned(), "agent".to_owned())));
        assert!(headers.contains(&("Copilot-Vision-Request".to_owned(), "true".to_owned())));
    }
}
