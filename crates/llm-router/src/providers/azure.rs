//! Explicit Azure API mode and credential binding. HTTP and encoders remain shared.
use super::openai_compatible::{OpenAiApiStyle, ProviderAuthStyle};
use crate::ProviderError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AzureAuth {
    ApiKey,
    Entra,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AzureUrlMode {
    #[default]
    V1,
    Deployment,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AzureOptions {
    api_version: String,
    auth: AzureAuth,
    url_mode: AzureUrlMode,
    legacy_max_tokens: bool,
}

impl AzureOptions {
    pub fn new(api_version: &str, auth: AzureAuth) -> Result<Self, ProviderError> {
        let date = api_version.strip_suffix("-preview").unwrap_or(api_version);
        let dated = date.len() == 10
            && date.as_bytes().iter().enumerate().all(|(at, b)| {
                if at == 4 || at == 7 {
                    *b == b'-'
                } else {
                    b.is_ascii_digit()
                }
            });
        if !matches!(api_version, "v1" | "preview") && !dated {
            return Err(ProviderError::InvalidRequest);
        }
        Ok(Self {
            api_version: api_version.to_owned(),
            auth,
            url_mode: AzureUrlMode::V1,
            legacy_max_tokens: false,
        })
    }
    pub fn with_url_mode(mut self, mode: AzureUrlMode) -> Self {
        self.url_mode = mode;
        self
    }
    pub fn with_legacy_max_tokens(mut self, legacy: bool) -> Self {
        self.legacy_max_tokens = legacy;
        self
    }
    pub fn legacy_max_tokens(&self) -> bool {
        self.legacy_max_tokens
    }
    pub fn url_mode(&self) -> AzureUrlMode {
        self.url_mode
    }
    pub fn api_version(&self) -> &str {
        &self.api_version
    }
    pub fn auth(&self) -> AzureAuth {
        self.auth
    }
    pub fn auth_style(&self) -> ProviderAuthStyle {
        match self.auth {
            AzureAuth::ApiKey => ProviderAuthStyle::AzureApiKey,
            AzureAuth::Entra => ProviderAuthStyle::Bearer,
        }
    }
    pub fn request_url(
        &self,
        base: &str,
        style: OpenAiApiStyle,
        deployment: &str,
    ) -> Result<String, ProviderError> {
        if deployment.is_empty()
            || deployment.len() > crate::MAX_MODEL_ID_BYTES
            || !deployment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            || deployment.starts_with('.')
            || deployment.ends_with('.')
            || deployment.contains("..")
        {
            return Err(ProviderError::InvalidRequest);
        }
        let base = base.trim_end_matches('/');
        let suffix = match style {
            OpenAiApiStyle::ChatCompletions => "chat/completions",
            OpenAiApiStyle::Responses => "responses",
        };
        let path = match self.url_mode {
            AzureUrlMode::V1 => format!("{base}/v1/{suffix}"),
            AzureUrlMode::Deployment => format!("{base}/deployments/{deployment}/{suffix}"),
        };
        Ok(format!("{path}?api-version={}", self.api_version))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn azure_modes_bind_deployment_version_and_fixed_auth_without_query_injection() {
        let legacy = AzureOptions::new("2024-10-21", AzureAuth::ApiKey)
            .unwrap()
            .with_url_mode(AzureUrlMode::Deployment);
        assert_eq!(
            legacy
                .request_url(
                    "https://resource.openai.azure.com/openai",
                    OpenAiApiStyle::ChatCompletions,
                    "my-deployment"
                )
                .unwrap(),
            "https://resource.openai.azure.com/openai/deployments/my-deployment/chat/completions?api-version=2024-10-21"
        );
        assert_eq!(legacy.auth_style(), ProviderAuthStyle::AzureApiKey);
        let v1 = AzureOptions::new("v1", AzureAuth::Entra).unwrap();
        assert_eq!(
            v1.request_url(
                "https://resource.openai.azure.com/openai/",
                OpenAiApiStyle::Responses,
                "deployment"
            )
            .unwrap(),
            "https://resource.openai.azure.com/openai/v1/responses?api-version=v1"
        );
        assert_eq!(v1.auth_style(), ProviderAuthStyle::Bearer);
        let preview = AzureOptions::new("2025-04-01-preview", AzureAuth::ApiKey).unwrap();
        assert_eq!(
            preview
                .request_url(
                    "http://127.0.0.1/openai",
                    OpenAiApiStyle::Responses,
                    "deployment"
                )
                .unwrap(),
            "http://127.0.0.1/openai/v1/responses?api-version=2025-04-01-preview"
        );
        for version in [
            "",
            "v1&key=secret",
            "2024-10-21?key=secret",
            "2024/10/21",
            "x\r\nheader",
        ] {
            assert!(AzureOptions::new(version, AzureAuth::ApiKey).is_err());
        }
        for deployment in ["../bad", "a/b", "bad?x=1", "a@b", ".", "two..dots"] {
            assert!(
                legacy
                    .request_url(
                        "http://localhost",
                        OpenAiApiStyle::ChatCompletions,
                        deployment
                    )
                    .is_err()
            );
        }
    }
}
