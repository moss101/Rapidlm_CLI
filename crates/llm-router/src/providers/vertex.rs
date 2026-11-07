//! Explicit Vertex publisher resource routes. Credentials and HTTP stay shared.
use crate::ProviderError;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VertexRoute {
    project: String,
    location: String,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VertexPublisher {
    Google,
    Anthropic,
}
impl VertexRoute {
    pub fn new(project: &str, location: &str) -> Result<Self, ProviderError> {
        let valid = |label: &str, max: usize| {
            !label.is_empty()
                && label.len() <= max
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && label
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
        };
        if !valid(project, 128) || !valid(location, 64) {
            return Err(ProviderError::InvalidRequest);
        }
        Ok(Self {
            project: project.to_owned(),
            location: location.to_owned(),
        })
    }
    pub fn project(&self) -> &str {
        &self.project
    }
    pub fn location(&self) -> &str {
        &self.location
    }
    pub fn default_base_url(&self) -> String {
        match self.location.as_str() {
            "global" => "https://aiplatform.googleapis.com/v1".to_owned(),
            "us" | "eu" => format!("https://aiplatform.{}.rep.googleapis.com/v1", self.location),
            _ => format!("https://{}-aiplatform.googleapis.com/v1", self.location),
        }
    }
    pub fn request_url(
        &self,
        base: &str,
        publisher: VertexPublisher,
        model: &str,
    ) -> Result<String, ProviderError> {
        let model = model.strip_prefix("models/").unwrap_or(model);
        if model.is_empty()
            || model.len() > crate::MAX_MODEL_ID_BYTES
            || !model
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.@".contains(&b))
            || model.starts_with('.')
            || model.ends_with('.')
            || model.contains("..")
        {
            return Err(ProviderError::InvalidRequest);
        }
        let model = model.replace('@', "%40");
        let (publisher, method) = match publisher {
            VertexPublisher::Google => ("google", "streamGenerateContent?alt=sse"),
            VertexPublisher::Anthropic => ("anthropic", "streamRawPredict"),
        };
        Ok(format!(
            "{}/projects/{}/locations/{}/publishers/{publisher}/models/{model}:{method}",
            base.trim_end_matches('/'),
            self.project,
            self.location
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn vertex_routes_keep_project_location_publisher_and_encoded_model_version() {
        let route = VertexRoute::new("project-123", "us-central1").unwrap();
        assert_eq!(
            route.default_base_url(),
            "https://us-central1-aiplatform.googleapis.com/v1"
        );
        assert_eq!(
            route
                .request_url(
                    "https://us-central1-aiplatform.googleapis.com/v1",
                    VertexPublisher::Anthropic,
                    "claude-3-5-sonnet-v2@20241022"
                )
                .unwrap(),
            "https://us-central1-aiplatform.googleapis.com/v1/projects/project-123/locations/us-central1/publishers/anthropic/models/claude-3-5-sonnet-v2%4020241022:streamRawPredict"
        );
        assert_eq!(
            VertexRoute::new("project-123", "global")
                .unwrap()
                .default_base_url(),
            "https://aiplatform.googleapis.com/v1"
        );
        for location in ["us", "eu"] {
            assert_eq!(
                VertexRoute::new("project-123", location)
                    .unwrap()
                    .default_base_url(),
                format!("https://aiplatform.{location}.rep.googleapis.com/v1")
            );
        }
        for (project, location) in [
            ("../escape", "global"),
            ("project", "global?x=y"),
            ("project", "us/other"),
            ("project", "Global"),
            ("", "us"),
            ("-project", "us"),
        ] {
            assert!(VertexRoute::new(project, location).is_err());
        }
        for model in ["../model", "model?token=x", "model/extra", "model@/escape"] {
            assert!(
                route
                    .request_url("https://example.test/v1", VertexPublisher::Google, model)
                    .is_err()
            );
        }
        assert!(crate::ModelId::parse("claude-3-5-sonnet-v2@20241022").is_ok());
        assert!(crate::ToolName::parse("tool@version").is_err());
    }
}
