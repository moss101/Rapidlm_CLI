//! Native Google generateContent protocol over the shared HTTP transport.
//! Private signed parts remain bound to their original calls and origin.
use super::openai_compatible::{
    HttpTransport, MAX_HTTP_REQUEST_BYTES, MAX_HTTP_RESPONSE_BYTES, OpenAiApiStyle,
    OpenAiCompatibleEndpoint, ProviderAuthStyle, ProviderControlMethod, ProviderHttpRequest,
    classify_http_error, push_argument_deltas, push_event, push_text_deltas, sse_data_blocks,
};
use crate::{
    CancellationToken, CanonicalMessage, CanonicalModelRequest, ContentPart, CredentialResolver,
    FinishReason, MessageRole, ModelStream, ModelStreamEvent, NormalizedUsage, ProviderAdapter,
    ProviderCapabilities, ProviderError, ProviderProfile, ReasoningEffort, ReasoningSupport,
    ToolCallId, ToolName, UsageCost, UsageExtValue,
};
use protocol::provider_replay::{
    MAX_REPLAY_BYTES, MAX_REPLAY_ITEMS, ProviderReplayState, ReplayCall, ReplayData, ReplayItem,
    ReplayProtocol, scope_key,
};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoogleGenAiEndpoint {
    base_url: String,
    vertex: Option<super::vertex::VertexRoute>,
}

/// A provider-issued Files API name, not an arbitrary URL.
#[derive(Clone, Eq, PartialEq)]
pub struct GoogleFileName(String);

/// File name bound to the configured provider profile and origin.
#[derive(Clone, Eq, PartialEq)]
pub struct GoogleFileHandle {
    name: GoogleFileName,
    scope: String,
}
impl std::fmt::Debug for GoogleFileHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GoogleFileHandle([redacted])")
    }
}

impl std::fmt::Debug for GoogleFileName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GoogleFileName([redacted])")
    }
}
impl GoogleFileName {
    pub fn parse(name: &str) -> Result<Self, ProviderError> {
        let id = name
            .strip_prefix("files/")
            .ok_or(ProviderError::InvalidRequest)?;
        if id.is_empty()
            || id.len() > 40
            || !id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            || id.starts_with('-')
            || id.ends_with('-')
        {
            return Err(ProviderError::InvalidRequest);
        }
        Ok(Self(name.to_owned()))
    }
}

/// Bounded processing state returned by the Files API.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GoogleFileState {
    Processing,
    Active,
    Failed,
}

/// The CLI's target upper bound for one uploaded source video.
pub const MAX_UPLOADED_VIDEO_BYTES: u64 = 100 * 1024 * 1024;
/// Each request stays below the shared provider transport's body bound.
pub const MAX_VIDEO_UPLOAD_CHUNK_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Eq, PartialEq)]
pub enum GoogleUploadError {
    Rejected(ProviderError),
    /// A request may have changed remote state. Do not replay it blindly.
    Uncertain,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UploadState {
    Active,
    Uncertain,
    Finished,
}

/// An in-memory resumable session. Its URL is an opaque secret and cannot be
/// serialized or logged. A failed exchange permanently marks it uncertain.
pub struct GoogleVideoUpload {
    url: String,
    scope: String,
    mime_type: String,
    total: u64,
    offset: u64,
    chunk_granularity: usize,
    expected_name: Option<GoogleFileName>,
    expected_sha256: [u8; 32],
    hasher: sha2::Sha256,
    state: UploadState,
}
impl std::fmt::Debug for GoogleVideoUpload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleVideoUpload")
            .field("url", &"[redacted]")
            .field("total", &self.total)
            .field("offset", &self.offset)
            .field("state", &self.state)
            .finish()
    }
}
impl GoogleVideoUpload {
    pub fn offset(&self) -> u64 {
        self.offset
    }
    /// Largest permitted non-final chunk respecting the server's granularity.
    /// The final chunk may be shorter.
    pub fn preferred_chunk_size(&self) -> usize {
        MAX_VIDEO_UPLOAD_CHUNK_BYTES / self.chunk_granularity * self.chunk_granularity
    }
    pub fn is_uncertain(&self) -> bool {
        self.state == UploadState::Uncertain
    }
}
impl GoogleGenAiEndpoint {
    pub fn new(base_url: &str) -> Result<Self, ProviderError> {
        let checked = OpenAiCompatibleEndpoint::new(base_url, OpenAiApiStyle::ChatCompletions)?;
        Ok(Self {
            base_url: checked.base_url().to_owned(),
            vertex: None,
        })
    }
    pub fn vertex(
        base_url: &str,
        route: super::vertex::VertexRoute,
    ) -> Result<Self, ProviderError> {
        let mut endpoint = Self::new(base_url)?;
        endpoint.vertex = Some(route);
        Ok(endpoint)
    }
    pub fn is_vertex(&self) -> bool {
        self.vertex.is_some()
    }
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
    fn file_url(&self, name: &GoogleFileName) -> Result<String, ProviderError> {
        if self.is_vertex() {
            return Err(ProviderError::InvalidRequest);
        }
        Ok(format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            name.0
        ))
    }
    fn upload_start_url(&self) -> Result<String, ProviderError> {
        if self.is_vertex() {
            return Err(ProviderError::InvalidRequest);
        }
        let parent = self
            .base_url
            .trim_end_matches('/')
            .strip_suffix("/v1beta")
            .ok_or(ProviderError::InvalidRequest)?;
        Ok(format!("{parent}/upload/v1beta/files"))
    }
    pub fn request_url(&self, model: &str) -> Result<String, ProviderError> {
        if let Some(route) = &self.vertex {
            return route.request_url(
                &self.base_url,
                super::vertex::VertexPublisher::Google,
                model,
            );
        }
        let model = model.strip_prefix("models/").unwrap_or(model);
        if model.is_empty()
            || model.len() > 256
            || !model
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        {
            return Err(ProviderError::InvalidRequest);
        }
        Ok(format!(
            "{}/models/{model}:streamGenerateContent?alt=sse",
            self.base_url.trim_end_matches('/')
        ))
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoogleGenAiConfig {
    profile: ProviderProfile,
    endpoint: GoogleGenAiEndpoint,
    capabilities: ProviderCapabilities,
    video_input: bool,
}
impl GoogleGenAiConfig {
    pub fn new(
        profile: ProviderProfile,
        endpoint: GoogleGenAiEndpoint,
        capabilities: ProviderCapabilities,
    ) -> Result<Self, ProviderError> {
        if !capabilities.streaming() {
            return Err(ProviderError::InvalidRequest);
        }
        Ok(Self {
            profile,
            endpoint,
            capabilities,
            video_input: false,
        })
    }
    /// Explicitly opt in when the selected model supports video input.
    pub fn with_video_input(mut self, enabled: bool) -> Self {
        self.video_input = enabled;
        self
    }
    pub fn profile(&self) -> &ProviderProfile {
        &self.profile
    }
    pub fn endpoint(&self) -> &GoogleGenAiEndpoint {
        &self.endpoint
    }
    pub fn capabilities(&self) -> &ProviderCapabilities {
        &self.capabilities
    }
}
pub struct GoogleGenAiAdapter<'a, T> {
    config: GoogleGenAiConfig,
    transport: T,
    store: &'a dyn auth::CredentialStore,
}
impl<'a, T: HttpTransport> GoogleGenAiAdapter<'a, T> {
    pub fn new(
        config: GoogleGenAiConfig,
        transport: T,
        store: &'a dyn auth::CredentialStore,
    ) -> Self {
        Self {
            config,
            transport,
            store,
        }
    }
    pub fn config(&self) -> &GoogleGenAiConfig {
        &self.config
    }
    /// Non-secret binding for tracking files belonging to this configured
    /// profile and endpoint. It does not prove the identity behind an alias.
    pub fn file_scope(&self) -> String {
        super::replay::scope(&self.config.profile, self.config.endpoint.base_url())
    }
    /// Bind a validated provider file name to this profile and endpoint.
    pub fn bind_file(&self, name: &str) -> Result<GoogleFileHandle, ProviderError> {
        let name = GoogleFileName::parse(name)?;
        self.config.endpoint.file_url(&name)?;
        Ok(GoogleFileHandle {
            name,
            scope: super::replay::scope(&self.config.profile, self.config.endpoint.base_url()),
        })
    }
    /// Create a video part for this configured endpoint and credential
    /// binding. The adapter checks that the remote file is active at use.
    pub fn video_file_part(
        &self,
        handle: &GoogleFileHandle,
        mime_type: &str,
    ) -> Result<ContentPart, ProviderError> {
        if !self.config.video_input
            || handle.scope
                != super::replay::scope(&self.config.profile, self.config.endpoint.base_url())
        {
            return Err(ProviderError::InvalidRequest);
        }
        let uri = self.config.endpoint.file_url(&handle.name)?;
        let file =
            crate::provider::VideoFileRef::new(uri, mime_type.to_owned(), handle.scope.clone())?;
        Ok(ContentPart::VideoFile { file })
    }
    /// Start one resumable video upload. A failed exchange may have created a
    /// remote session even if its URL was not received.
    pub fn begin_video_upload(
        &self,
        mime_type: &str,
        total: u64,
        expected_sha256: [u8; 32],
        cancel: &CancellationToken,
    ) -> Result<GoogleVideoUpload, GoogleUploadError> {
        self.begin_video_upload_inner(None, mime_type, total, expected_sha256, cancel)
    }

    /// Start an upload with a caller-chosen Files API name. Recording this
    /// name before the request lets a caller reconcile an ambiguous outcome.
    pub fn begin_named_video_upload(
        &self,
        name: &str,
        mime_type: &str,
        total: u64,
        expected_sha256: [u8; 32],
        cancel: &CancellationToken,
    ) -> Result<GoogleVideoUpload, GoogleUploadError> {
        let name = GoogleFileName::parse(name).map_err(GoogleUploadError::Rejected)?;
        self.begin_video_upload_inner(Some(name), mime_type, total, expected_sha256, cancel)
    }

    fn begin_video_upload_inner(
        &self,
        expected_name: Option<GoogleFileName>,
        mime_type: &str,
        total: u64,
        expected_sha256: [u8; 32],
        cancel: &CancellationToken,
    ) -> Result<GoogleVideoUpload, GoogleUploadError> {
        cancel.check().map_err(GoogleUploadError::Rejected)?;
        if !self.config.video_input
            || total == 0
            || total > MAX_UPLOADED_VIDEO_BYTES
            || !matches!(
                mime_type,
                "video/mp4"
                    | "video/mpeg"
                    | "video/mov"
                    | "video/avi"
                    | "video/x-flv"
                    | "video/mpg"
                    | "video/webm"
                    | "video/wmv"
                    | "video/3gpp"
            )
        {
            return Err(GoogleUploadError::Rejected(ProviderError::InvalidRequest));
        }
        let url = self
            .config
            .endpoint
            .upload_start_url()
            .map_err(GoogleUploadError::Rejected)?;
        let credential = CredentialResolver::new(self.store)
            .resolve(self.config.profile.provider(), &self.config.profile, cancel)
            .map_err(GoogleUploadError::Rejected)?;
        let headers = vec![
            ("content-type".to_owned(), "application/json".to_owned()),
            ("x-goog-upload-protocol".to_owned(), "resumable".to_owned()),
            ("x-goog-upload-command".to_owned(), "start".to_owned()),
            (
                "x-goog-upload-header-content-length".to_owned(),
                total.to_string(),
            ),
            (
                "x-goog-upload-header-content-type".to_owned(),
                mime_type.to_owned(),
            ),
        ];
        let mut file = json!({"display_name": "RapidLM video"});
        if let Some(name) = &expected_name {
            file["name"] = json!(name.0);
        }
        let body = serde_json::to_vec(&json!({"file": file}))
            .map_err(|_| GoogleUploadError::Rejected(ProviderError::InvalidRequest))?;
        let request = ProviderHttpRequest::new(&url, &headers, &body, &credential)
            .with_auth_style(ProviderAuthStyle::GoogleApiKey);
        let response = self
            .transport
            .execute(&request, cancel)
            .map_err(|_| GoogleUploadError::Uncertain)?;
        classify_http_error(&response).map_err(|_| GoogleUploadError::Uncertain)?;
        let mut candidates = response
            .headers()
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("x-goog-upload-url"));
        let Some((_, upload_url)) = candidates.next() else {
            return Err(GoogleUploadError::Uncertain);
        };
        if candidates.next().is_some()
            || !super::openai_compatible::same_origin_and_path(&url, upload_url)
        {
            return Err(GoogleUploadError::Uncertain);
        }
        let mut granularities = response
            .headers()
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("x-goog-upload-chunk-granularity"));
        let chunk_granularity = match granularities.next() {
            Some((_, value)) => {
                if granularities.next().is_some() {
                    return Err(GoogleUploadError::Uncertain);
                }
                value
                    .parse::<usize>()
                    .ok()
                    .filter(|value| *value > 0 && *value <= MAX_VIDEO_UPLOAD_CHUNK_BYTES)
                    .ok_or(GoogleUploadError::Uncertain)?
            }
            None => 1,
        };
        Ok(GoogleVideoUpload {
            url: upload_url.to_owned(),
            scope: super::replay::scope(&self.config.profile, self.config.endpoint.base_url()),
            mime_type: mime_type.to_owned(),
            total,
            offset: 0,
            chunk_granularity,
            expected_name,
            expected_sha256,
            hasher: <sha2::Sha256 as sha2::Digest>::new(),
            state: UploadState::Active,
        })
    }

    /// Send the next bounded chunk. `Some` means the last chunk returned a
    /// valid file name; `None` means another chunk remains. Failed or
    /// ambiguous exchanges cannot be retried on this handle.
    pub fn upload_video_chunk(
        &self,
        upload: &mut GoogleVideoUpload,
        chunk: &[u8],
        cancel: &CancellationToken,
    ) -> Result<Option<GoogleFileHandle>, GoogleUploadError> {
        cancel.check().map_err(GoogleUploadError::Rejected)?;
        if upload.state != UploadState::Active
            || !self.config.video_input
            || upload.scope
                != super::replay::scope(&self.config.profile, self.config.endpoint.base_url())
            || chunk.is_empty()
            || chunk.len() > MAX_VIDEO_UPLOAD_CHUNK_BYTES
        {
            return Err(GoogleUploadError::Rejected(ProviderError::InvalidRequest));
        }
        let next = upload
            .offset
            .checked_add(chunk.len() as u64)
            .filter(|next| *next <= upload.total)
            .ok_or(GoogleUploadError::Rejected(ProviderError::BoundExceeded))?;
        let final_chunk = next == upload.total;
        if !final_chunk && !chunk.len().is_multiple_of(upload.chunk_granularity) {
            return Err(GoogleUploadError::Rejected(ProviderError::InvalidRequest));
        }
        use sha2::Digest as _;
        let mut updated_hash = upload.hasher.clone();
        updated_hash.update(chunk);
        if final_chunk
            && <[u8; 32]>::from(updated_hash.clone().finalize()) != upload.expected_sha256
        {
            upload.state = UploadState::Uncertain;
            return Err(GoogleUploadError::Uncertain);
        }
        let credential = CredentialResolver::new(self.store)
            .resolve(self.config.profile.provider(), &self.config.profile, cancel)
            .map_err(GoogleUploadError::Rejected)?;
        let headers = vec![
            ("content-type".to_owned(), upload.mime_type.clone()),
            ("x-goog-upload-offset".to_owned(), upload.offset.to_string()),
            (
                "x-goog-upload-command".to_owned(),
                if final_chunk {
                    "upload, finalize"
                } else {
                    "upload"
                }
                .to_owned(),
            ),
        ];
        let request = ProviderHttpRequest::new(&upload.url, &headers, chunk, &credential)
            .with_auth_style(ProviderAuthStyle::GoogleApiKey);
        upload.state = UploadState::Uncertain;
        let response = self
            .transport
            .execute(&request, cancel)
            .map_err(|_| GoogleUploadError::Uncertain)?;
        classify_http_error(&response).map_err(|_| GoogleUploadError::Uncertain)?;
        let mut received = response
            .headers()
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("x-goog-upload-size-received"));
        if let Some((_, count)) = received.next()
            && (received.next().is_some() || count.parse::<u64>().ok() != Some(next))
        {
            return Err(GoogleUploadError::Uncertain);
        }
        if !final_chunk {
            upload.offset = next;
            upload.hasher = updated_hash;
            upload.state = UploadState::Active;
            return Ok(None);
        }
        let body: Value =
            serde_json::from_slice(response.body()).map_err(|_| GoogleUploadError::Uncertain)?;
        let file = body.get("file").ok_or(GoogleUploadError::Uncertain)?;
        if file.get("mimeType").and_then(Value::as_str) != Some(upload.mime_type.as_str()) {
            return Err(GoogleUploadError::Uncertain);
        }
        let name = file
            .get("name")
            .and_then(Value::as_str)
            .ok_or(GoogleUploadError::Uncertain)?;
        let handle = self
            .bind_file(name)
            .map_err(|_| GoogleUploadError::Uncertain)?;
        if upload
            .expected_name
            .as_ref()
            .is_some_and(|name| *name != handle.name)
        {
            return Err(GoogleUploadError::Uncertain);
        }
        upload.offset = next;
        upload.hasher = updated_hash;
        upload.state = UploadState::Finished;
        Ok(Some(handle))
    }
    /// Read a file's processing status. Unknown states or mismatched names
    /// are refused rather than interpreted as ready.
    pub fn file_status(
        &self,
        handle: &GoogleFileHandle,
        cancel: &CancellationToken,
    ) -> Result<GoogleFileState, ProviderError> {
        self.file_metadata(handle, cancel)
            .map(|(state, _, _)| state)
    }

    /// Validate the inference URI before treating an active video as ready.
    pub fn video_file_status(
        &self,
        handle: &GoogleFileHandle,
        mime_type: &str,
        cancel: &CancellationToken,
    ) -> Result<GoogleFileState, ProviderError> {
        let (state, mime, uri) = self.file_metadata(handle, cancel)?;
        if state == GoogleFileState::Active {
            if mime.as_deref() != Some(mime_type) {
                return Err(ProviderError::InvalidRequest);
            }
            self.validate_reported_video_uri(handle, mime_type, uri.as_deref())?;
        }
        Ok(state)
    }

    fn validate_reported_video_uri(
        &self,
        handle: &GoogleFileHandle,
        mime_type: &str,
        uri: Option<&str>,
    ) -> Result<String, ProviderError> {
        let uri = uri.ok_or(ProviderError::InvalidRequest)?;
        // Provider data is forwarded to the provider, never dialled locally.
        crate::provider::VideoFileRef::new(
            uri.to_owned(),
            mime_type.to_owned(),
            handle.scope.clone(),
        )?;
        OpenAiCompatibleEndpoint::new(uri, OpenAiApiStyle::ChatCompletions)?;
        if self.config.endpoint.base_url().starts_with("https://") && !uri.starts_with("https://") {
            return Err(ProviderError::InvalidRequest);
        }
        Ok(uri.to_owned())
    }

    /// Verify provider metadata against the exact staged video before using
    /// a recovered name. A name alone is not proof that a file is ours.
    pub fn verify_video_file(
        &self,
        handle: &GoogleFileHandle,
        mime_type: &str,
        size: u64,
        sha256: [u8; 32],
        cancel: &CancellationToken,
    ) -> Result<GoogleFileState, ProviderError> {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        if !crate::provider::valid_video_mime(mime_type)
            || size == 0
            || size > MAX_UPLOADED_VIDEO_BYTES
        {
            return Err(ProviderError::InvalidRequest);
        }
        let response = self.file_control(ProviderControlMethod::Get, handle, cancel)?;
        classify_http_error(&response)?;
        let value: Value =
            serde_json::from_slice(response.body()).map_err(|_| ProviderError::Permanent)?;
        if value.get("name").and_then(Value::as_str) != Some(handle.name.0.as_str())
            || value.get("mimeType").and_then(Value::as_str) != Some(mime_type)
            || value.get("sizeBytes").and_then(Value::as_str) != Some(size.to_string().as_str())
            || value.get("sha256Hash").and_then(Value::as_str)
                != Some(STANDARD.encode(sha256).as_str())
        {
            return Err(ProviderError::InvalidRequest);
        }
        match value.get("state").and_then(Value::as_str) {
            Some("PROCESSING") => Ok(GoogleFileState::Processing),
            Some("ACTIVE") => Ok(GoogleFileState::Active),
            Some("FAILED") => Ok(GoogleFileState::Failed),
            _ => Err(ProviderError::Permanent),
        }
    }
    fn file_metadata(
        &self,
        handle: &GoogleFileHandle,
        cancel: &CancellationToken,
    ) -> Result<(GoogleFileState, Option<String>, Option<String>), ProviderError> {
        let response = self.file_control(ProviderControlMethod::Get, handle, cancel)?;
        classify_http_error(&response)?;
        let value: Value =
            serde_json::from_slice(response.body()).map_err(|_| ProviderError::Permanent)?;
        if value.get("name").and_then(Value::as_str) != Some(handle.name.0.as_str()) {
            return Err(ProviderError::Permanent);
        }
        let state = match value.get("state").and_then(Value::as_str) {
            Some("PROCESSING") => GoogleFileState::Processing,
            Some("ACTIVE") => GoogleFileState::Active,
            Some("FAILED") => GoogleFileState::Failed,
            _ => return Err(ProviderError::Permanent),
        };
        let mime = value
            .get("mimeType")
            .and_then(Value::as_str)
            .filter(|mime| mime.len() <= 128)
            .map(str::to_owned);
        let uri = value
            .get("uri")
            .and_then(Value::as_str)
            .filter(|uri| uri.len() <= 8192)
            .map(str::to_owned);
        Ok((state, mime, uri))
    }
    /// Delete one named file using the configured profile and endpoint.
    /// A missing file is not treated as confirmation of deletion.
    pub fn delete_file(
        &self,
        handle: &GoogleFileHandle,
        cancel: &CancellationToken,
    ) -> Result<(), ProviderError> {
        let response = self.file_control(ProviderControlMethod::Delete, handle, cancel)?;
        classify_http_error(&response)
    }
    fn file_control(
        &self,
        method: ProviderControlMethod,
        handle: &GoogleFileHandle,
        cancel: &CancellationToken,
    ) -> Result<super::openai_compatible::ProviderHttpResponse, ProviderError> {
        cancel.check()?;
        if handle.scope
            != super::replay::scope(&self.config.profile, self.config.endpoint.base_url())
        {
            return Err(ProviderError::InvalidRequest);
        }
        let url = self.config.endpoint.file_url(&handle.name)?;
        let credential = CredentialResolver::new(self.store).resolve(
            self.config.profile.provider(),
            &self.config.profile,
            cancel,
        )?;
        let request = ProviderHttpRequest::new(&url, &[], &[], &credential)
            .with_auth_style(ProviderAuthStyle::GoogleApiKey);
        self.transport.execute_control(method, &request, cancel)
    }
    pub fn invoke_sync(
        &self,
        req: CanonicalModelRequest,
        cancel: &CancellationToken,
    ) -> Result<ModelStream, ProviderError> {
        self.invoke_inner(req, cancel, None)
    }
    pub fn invoke_sync_streaming(
        &self,
        req: CanonicalModelRequest,
        cancel: &CancellationToken,
        sink: &mut dyn FnMut(&str),
    ) -> Result<ModelStream, ProviderError> {
        self.invoke_inner(req, cancel, Some(sink))
    }
    fn invoke_inner(
        &self,
        req: CanonicalModelRequest,
        cancel: &CancellationToken,
        sink: Option<&mut dyn FnMut(&str)>,
    ) -> Result<ModelStream, ProviderError> {
        cancel.check()?;
        let url = self
            .config
            .endpoint
            .request_url(req.model().model().as_str())?;
        super::replay::validate(
            &req,
            Some(ReplayProtocol::GoogleGenAi),
            &self.config.profile,
            &url,
        )?;
        if req.model().provider() != self.config.profile.provider()
            || (!req.tools().is_empty() && !self.config.capabilities.tools())
        {
            return Err(ProviderError::InvalidRequest);
        }
        if req.messages().iter().flat_map(|m| m.parts()).any(|part| {
            matches!(
                part,
                ContentPart::VideoData { .. } | ContentPart::VideoFile { .. }
            )
        }) && !self.config.video_input
        {
            return Err(ProviderError::InvalidRequest);
        }
        let mut remote_videos = req
            .messages()
            .iter()
            .flat_map(|message| message.parts())
            .filter_map(|part| match part {
                ContentPart::VideoFile { file } => Some(file),
                _ => None,
            });
        let remote_video = if let Some(file) = remote_videos.next() {
            if remote_videos.next().is_some()
                || file.scope()
                    != super::replay::scope(&self.config.profile, self.config.endpoint.base_url())
            {
                return Err(ProviderError::InvalidRequest);
            }
            let prefix = format!("{}/", self.config.endpoint.base_url().trim_end_matches('/'));
            let name = GoogleFileName::parse(
                file.uri()
                    .strip_prefix(&prefix)
                    .ok_or(ProviderError::InvalidRequest)?,
            )?;
            if self.config.endpoint.file_url(&name)? != file.uri() {
                return Err(ProviderError::InvalidRequest);
            }
            let handle = GoogleFileHandle {
                name,
                scope: file.scope().to_owned(),
            };
            Some((handle, file.mime_type()))
        } else {
            None
        };
        if req
            .messages()
            .iter()
            .flat_map(|m| m.parts())
            .any(|p| matches!(p, ContentPart::Image { .. } | ContentPart::ImageData { .. }))
            && !self.config.capabilities.vision()
        {
            return Err(ProviderError::InvalidRequest);
        }
        if req.reasoning_effort().is_some()
            && self.config.capabilities.reasoning() == ReasoningSupport::None
        {
            return Err(ProviderError::InvalidRequest);
        }
        let mut payload = encode_google_payload(&req, cancel)?;
        if let Some((handle, mime)) = remote_video {
            let (state, remote_mime, uri) = self.file_metadata(&handle, cancel)?;
            if state != GoogleFileState::Active || remote_mime.as_deref() != Some(mime) {
                return Err(ProviderError::InvalidRequest);
            }
            let uri = self.validate_reported_video_uri(&handle, mime, uri.as_deref())?;
            let mut replaced = 0;
            for content in payload["contents"]
                .as_array_mut()
                .ok_or(ProviderError::InvalidRequest)?
            {
                for part in content["parts"]
                    .as_array_mut()
                    .ok_or(ProviderError::InvalidRequest)?
                {
                    if let Some(file) = part.get_mut("fileData") {
                        file["fileUri"] = Value::String(uri.clone());
                        replaced += 1;
                    }
                }
            }
            if replaced != 1 {
                return Err(ProviderError::InvalidRequest);
            }
        }
        let bytes = serde_json::to_vec(&payload).map_err(|_| ProviderError::InvalidRequest)?;
        if bytes.len() > MAX_HTTP_REQUEST_BYTES {
            return Err(ProviderError::BoundExceeded);
        }
        let credential = CredentialResolver::new(self.store).resolve(
            self.config.profile.provider(),
            &self.config.profile,
            cancel,
        )?;
        let headers = [
            ("content-type".to_owned(), "application/json".to_owned()),
            ("accept".to_owned(), "text/event-stream".to_owned()),
            (
                "x-rapidlm-request-id".to_owned(),
                req.request_id().as_str().to_owned(),
            ),
        ];
        let outbound = ProviderHttpRequest::new(&url, &headers, &bytes, &credential)
            .with_auth_style(if self.config.endpoint.is_vertex() {
                ProviderAuthStyle::Bearer
            } else {
                ProviderAuthStyle::GoogleApiKey
            });
        let response = if let Some(sink) = sink {
            let mut live = LiveText::default();
            let response = self
                .transport
                .execute_streaming(&outbound, cancel, &mut |chunk| live.feed(chunk, sink))?;
            classify_http_error(&response)?;
            if live.failed {
                return Err(ProviderError::Permanent);
            }
            response
        } else {
            self.transport.execute(&outbound, cancel)?
        };
        drop(credential);
        classify_http_error(&response)?;
        parse_google_response(&req, response.body(), &self.config.profile, &url, cancel)
    }
}
impl<T: HttpTransport> ProviderAdapter for GoogleGenAiAdapter<'_, T> {
    fn capabilities(&self) -> ProviderCapabilities {
        self.config.capabilities.clone()
    }
    fn invoke(
        &self,
        req: CanonicalModelRequest,
        cancel: CancellationToken,
    ) -> impl Future<Output = Result<ModelStream, ProviderError>> + Send {
        let result = self.invoke_sync(req, &cancel);
        async move { result }
    }
}

pub fn encode_google_payload(
    req: &CanonicalModelRequest,
    cancel: &CancellationToken,
) -> Result<Value, ProviderError> {
    cancel.check()?;
    if req.messages().iter().any(|message| {
        message.role() != MessageRole::User
            && message.parts().iter().any(|part| {
                matches!(
                    part,
                    ContentPart::VideoData { .. } | ContentPart::VideoFile { .. }
                )
            })
    }) {
        return Err(ProviderError::InvalidRequest);
    }
    let mut system = Vec::new();
    let mut contents: Vec<Value> = Vec::new();
    // A result belongs to the call's name and original optional wire id, not a guessed name.
    let mut pending: BTreeMap<String, (String, Option<String>)> = BTreeMap::new();
    for message in req.messages() {
        cancel.check()?;
        if message.role() == MessageRole::System {
            for part in message.parts() {
                match part {
                    ContentPart::Text { text } => system.push(json!({"text":text})),
                    _ => return Err(ProviderError::InvalidRequest),
                }
            }
            continue;
        }
        let role = if message.role() == MessageRole::Assistant {
            "model"
        } else {
            "user"
        };
        let parts = if message.role() == MessageRole::Tool {
            let id = message
                .tool_call_id()
                .ok_or(ProviderError::InvalidRequest)?
                .as_str();
            let (name, wire_id) = pending.remove(id).ok_or(ProviderError::InvalidRequest)?;
            let text: String = message
                .parts()
                .iter()
                .filter_map(|p| {
                    if let ContentPart::Text { text } = p {
                        Some(text.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            let mut output = json!({"name":name,"response":{"output":text}});
            if let Some(id) = wire_id {
                output["id"] = id.into();
            }
            let media = message
                .parts()
                .iter()
                .filter(|p| !matches!(p, ContentPart::Text { .. }))
                .map(encode_part)
                .collect::<Result<Vec<_>, _>>()?;
            if !media.is_empty() {
                output["parts"] = media.into();
            }
            vec![json!({"functionResponse":output})]
        } else if message.role() == MessageRole::Assistant {
            if !pending.is_empty() {
                return Err(ProviderError::InvalidRequest);
            }
            let parts = encode_model_parts(message)?;
            for (index, call) in message.tool_calls().iter().enumerate() {
                if !google_name(call.name().as_str()) {
                    return Err(ProviderError::InvalidRequest);
                }
                let wire_id = original_id(message, index, call.call_id().as_str());
                if pending
                    .insert(
                        call.call_id().as_str().to_owned(),
                        (call.name().as_str().to_owned(), wire_id),
                    )
                    .is_some()
                {
                    return Err(ProviderError::InvalidRequest);
                }
            }
            parts
        } else {
            if !pending.is_empty() {
                return Err(ProviderError::InvalidRequest);
            }
            message
                .parts()
                .iter()
                .map(encode_part)
                .collect::<Result<Vec<_>, _>>()?
        };
        if parts.is_empty() {
            continue;
        }
        // Adjacent user/tool results make one content with ordered response parts.
        if let Some(last) = contents.last_mut().filter(|last| last["role"] == role) {
            last["parts"]
                .as_array_mut()
                .ok_or(ProviderError::InvalidRequest)?
                .extend(parts);
        } else {
            contents.push(json!({"role":role,"parts":parts}));
        }
    }
    if contents.is_empty()
        || !pending.is_empty()
        || contents.first().is_none_or(|c| c["role"] != "user")
    {
        return Err(ProviderError::InvalidRequest);
    }
    let mut payload = json!({"contents":contents,"generationConfig":{"candidateCount":1,"responseModalities":["TEXT"]}});
    if !system.is_empty() {
        payload["systemInstruction"] = json!({"parts":system});
    }
    if let Some(max) = req.max_output_tokens() {
        payload["generationConfig"]["maxOutputTokens"] = max.into();
    }
    if let Some(effort) = req.reasoning_effort() {
        let config = match effort {
            ReasoningEffort::None => json!({"thinkingBudget":0}),
            ReasoningEffort::Minimal => json!({"thinkingLevel":"MINIMAL"}),
            ReasoningEffort::Low => json!({"thinkingLevel":"LOW"}),
            ReasoningEffort::Medium => json!({"thinkingLevel":"MEDIUM"}),
            ReasoningEffort::High => json!({"thinkingLevel":"HIGH"}),
            _ => return Err(ProviderError::InvalidRequest),
        };
        payload["generationConfig"]["thinkingConfig"] = config;
    }
    if !req.tools().is_empty() {
        let mut declarations = Vec::new();
        for tool in req.tools() {
            if !google_name(tool.name().as_str()) {
                return Err(ProviderError::InvalidRequest);
            }
            declarations.push(json!({"name":tool.name().as_str(),"description":tool.description(),"parametersJsonSchema":tool.parameters()}));
        }
        payload["tools"] = json!([{"functionDeclarations":declarations}]);
    }
    Ok(payload)
}
fn encode_part(part: &ContentPart) -> Result<Value, ProviderError> {
    match part {
        ContentPart::Text { text } => Ok(json!({"text":text})),
        ContentPart::ImageData { data_url } => {
            let (prefix, data) = data_url
                .split_once(";base64,")
                .ok_or(ProviderError::InvalidRequest)?;
            let mime = prefix
                .strip_prefix("data:")
                .filter(|mime| {
                    matches!(
                        *mime,
                        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
                    )
                })
                .ok_or(ProviderError::InvalidRequest)?;
            Ok(json!({"inlineData":{"mimeType":mime,"data":data}}))
        }
        ContentPart::VideoData { mime_type, data } => {
            use base64::{Engine as _, engine::general_purpose::STANDARD};
            if data.len() > crate::provider::MAX_INLINE_VIDEO_BYTES.div_ceil(3) * 4 {
                return Err(ProviderError::BoundExceeded);
            }
            let bytes = STANDARD
                .decode(data)
                .map_err(|_| ProviderError::InvalidRequest)?;
            ContentPart::video_data(mime_type, &bytes)?;
            Ok(json!({"inlineData":{"mimeType":mime_type,"data":data}}))
        }
        ContentPart::VideoFile { file } => {
            Ok(json!({"fileData":{"mimeType":file.mime_type(),"fileUri":file.uri()}}))
        }
        // An internal artifact id is not a Google file URI. Resolve it before this boundary.
        ContentPart::Image { .. } => Err(ProviderError::InvalidRequest),
    }
}
fn original_id(message: &CanonicalMessage, index: usize, fallback: &str) -> Option<String> {
    message
        .provider_replay()
        .and_then(|state| {
            state.items().iter().find_map(|item| match &item.data {
                ReplayData::GoogleCall { wire_id, .. } if item.before_call == index => {
                    Some(wire_id.clone())
                }
                _ => None,
            })
        })
        .unwrap_or_else(|| Some(fallback.to_owned()))
}
fn call_part(
    message: &CanonicalMessage,
    index: usize,
    signature: Option<&str>,
) -> Result<Value, ProviderError> {
    let call = message
        .tool_calls()
        .get(index)
        .ok_or(ProviderError::InvalidRequest)?;
    let args: Value =
        serde_json::from_str(call.arguments()).map_err(|_| ProviderError::InvalidRequest)?;
    if !args.is_object() || !google_name(call.name().as_str()) {
        return Err(ProviderError::InvalidRequest);
    }
    let mut part = json!({"functionCall":{"name":call.name().as_str(),"args":args}});
    if let Some(id) = original_id(message, index, call.call_id().as_str()) {
        part["functionCall"]["id"] = id.into();
    }
    if let Some(sig) = signature {
        part["thoughtSignature"] = sig.into();
    }
    Ok(part)
}
fn encode_model_parts(message: &CanonicalMessage) -> Result<Vec<Value>, ProviderError> {
    let Some(state) = message.provider_replay() else {
        let mut parts = message
            .parts()
            .iter()
            .map(encode_part)
            .collect::<Result<Vec<_>, _>>()?;
        for index in 0..message.tool_calls().len() {
            parts.push(call_part(message, index, None)?);
        }
        return Ok(parts);
    };
    if state.protocol() != ReplayProtocol::GoogleGenAi {
        return Err(ProviderError::InvalidRequest);
    }
    let mut parts = Vec::new();
    let mut calls = BTreeSet::new();
    // Text is already present in the private wire snapshot. For a length continuation,
    // prepend only earlier accumulated answer text, not this response's text twice.
    let visible: String = state
        .items()
        .iter()
        .filter_map(|item| match &item.data {
            ReplayData::GoogleText {
                text: Some(text),
                thought: false,
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let requested: String = message
        .parts()
        .iter()
        .filter_map(|part| {
            if let ContentPart::Text { text } = part {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect();
    if !requested.is_empty() && requested != visible {
        let prefix = requested
            .strip_suffix(&visible)
            .ok_or(ProviderError::InvalidRequest)?;
        if !prefix.is_empty() {
            parts.push(json!({"text":prefix}));
        }
    }
    for item in state.items() {
        match &item.data {
            ReplayData::GoogleText {
                text,
                thought,
                signature,
            } => {
                let mut part = json!({});
                if let Some(text) = text {
                    part["text"] = text.as_str().into();
                }
                if *thought {
                    part["thought"] = true.into();
                }
                if let Some(signature) = signature {
                    part["thoughtSignature"] = signature.as_str().into();
                }
                parts.push(part);
            }
            ReplayData::GoogleCall { signature, .. } => {
                if !calls.insert(item.before_call) {
                    return Err(ProviderError::InvalidRequest);
                }
                parts.push(call_part(message, item.before_call, signature.as_deref())?);
            }
            _ => return Err(ProviderError::InvalidRequest),
        }
    }
    if calls.len() != message.tool_calls().len() {
        return Err(ProviderError::InvalidRequest);
    }
    Ok(parts)
}
fn google_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn frames(body: &[u8]) -> Result<Vec<Value>, ProviderError> {
    if body.len() > MAX_HTTP_RESPONSE_BYTES {
        return Err(ProviderError::Permanent);
    }
    let text = std::str::from_utf8(body).map_err(|_| ProviderError::Permanent)?;
    if text.trim_start().starts_with('{') {
        return Ok(vec![
            serde_json::from_str(text).map_err(|_| ProviderError::Permanent)?,
        ]);
    }
    let blocks = sse_data_blocks(text);
    if blocks.is_empty() {
        return Err(ProviderError::Permanent);
    }
    blocks
        .into_iter()
        .filter(|s| s != "[DONE]")
        .map(|s| serde_json::from_str(&s).map_err(|_| ProviderError::Permanent))
        .collect()
}
fn candidate(frame: &Value) -> Result<Option<&Value>, ProviderError> {
    if !frame.is_object()
        || frame.get("error").is_some()
        || frame
            .pointer("/promptFeedback/blockReason")
            .and_then(Value::as_str)
            .is_some_and(|s| s != "BLOCK_REASON_UNSPECIFIED")
    {
        return Err(ProviderError::Permanent);
    }
    let Some(candidates) = frame.get("candidates") else {
        return Ok(None);
    };
    let candidates = candidates.as_array().ok_or(ProviderError::Permanent)?;
    if candidates.len() > 1 {
        return Err(ProviderError::Permanent);
    }
    let Some(candidate) = candidates.first() else {
        return Ok(None);
    };
    if candidate
        .get("index")
        .is_some_and(|i| i.as_u64() != Some(0))
        || candidate
            .pointer("/content/role")
            .is_some_and(|r| r != "model")
    {
        return Err(ProviderError::Permanent);
    }
    if candidate
        .get("finishReason")
        .is_some_and(|r| !matches!(r.as_str(), Some("STOP" | "MAX_TOKENS")))
    {
        return Err(ProviderError::Permanent);
    }
    Ok(Some(candidate))
}
fn optional_string(value: &Value, key: &str) -> Result<Option<String>, ProviderError> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        _ => Err(ProviderError::Permanent),
    }
}
fn parse_google_response(
    req: &CanonicalModelRequest,
    body: &[u8],
    profile: &ProviderProfile,
    url: &str,
    cancel: &CancellationToken,
) -> Result<ModelStream, ProviderError> {
    cancel.check()?;
    let mut events = Vec::new();
    let mut calls = Vec::new();
    let mut items = Vec::new();
    let mut finish = None;
    let mut usage = Map::new();
    let mut ids = BTreeSet::new();
    let mut replay_bytes = 0usize;
    for frame in frames(body)? {
        cancel.check()?;
        if let Some(next) = frame.get("usageMetadata") {
            for (key, value) in next.as_object().ok_or(ProviderError::Permanent)? {
                usage.insert(key.clone(), value.clone());
            }
        }
        let Some(candidate) = candidate(&frame)? else {
            continue;
        };
        if let Some(reason) = candidate.get("finishReason") {
            finish = Some(reason.as_str().ok_or(ProviderError::Permanent)?.to_owned());
        }
        if let Some(parts) = candidate.pointer("/content/parts") {
            for part in parts.as_array().ok_or(ProviderError::Permanent)? {
                cancel.check()?;
                let signature = optional_string(part, "thoughtSignature")?;
                if let Some(fc) = part.get("functionCall") {
                    if part.get("text").is_some() || part.get("thought").is_some_and(|v| v != false)
                    {
                        return Err(ProviderError::Permanent);
                    }
                    let name = fc
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|name| google_name(name))
                        .ok_or(ProviderError::Permanent)?;
                    let wire_id = optional_string(fc, "id")?;
                    let internal_id = wire_id.clone().unwrap_or_else(|| {
                        format!(
                            "g{}_{}",
                            &scope_key(&[req.request_id().as_str()])[..16],
                            calls.len()
                        )
                    });
                    let call_id =
                        ToolCallId::parse(&internal_id).map_err(|_| ProviderError::Permanent)?;
                    if !ids.insert(internal_id.clone()) || calls.len() >= crate::MAX_TOOL_CALLS {
                        return Err(ProviderError::Permanent);
                    }
                    if fc.get("partialArgs").is_some() || fc.get("willContinue").is_some() {
                        return Err(ProviderError::Permanent);
                    }
                    let args = fc.get("args").cloned().unwrap_or_else(|| json!({}));
                    if !args.is_object() {
                        return Err(ProviderError::Permanent);
                    }
                    let args =
                        serde_json::to_string(&args).map_err(|_| ProviderError::Permanent)?;
                    let call = ReplayCall::new(&internal_id, name, &args)
                        .map_err(|_| ProviderError::Permanent)?;
                    push_event(
                        &mut events,
                        ModelStreamEvent::ToolCallStart {
                            call_id: call_id.clone(),
                            name: ToolName::parse(name).map_err(|_| ProviderError::Permanent)?,
                        },
                    )?;
                    push_argument_deltas(&mut events, call_id, &args)?;
                    replay_bytes += signature.as_ref().map_or(0, String::len) + internal_id.len();
                    items.push(ReplayItem {
                        before_call: calls.len(),
                        data: ReplayData::GoogleCall {
                            call_id: internal_id,
                            wire_id,
                            signature,
                        },
                    });
                    calls.push(call);
                } else {
                    if part.as_object().is_none_or(|obj| {
                        obj.keys().any(|key| {
                            !matches!(key.as_str(), "text" | "thought" | "thoughtSignature")
                        })
                    }) {
                        return Err(ProviderError::Permanent);
                    }
                    let text = optional_string(part, "text")?;
                    let thought = match part.get("thought") {
                        None => false,
                        Some(Value::Bool(b)) => *b,
                        _ => return Err(ProviderError::Permanent),
                    };
                    if text.is_none() && signature.is_none() {
                        return Err(ProviderError::Permanent);
                    }
                    if !thought && let Some(text) = &text {
                        push_text_deltas(&mut events, text)?;
                    }
                    replay_bytes += text.as_ref().map_or(0, String::len)
                        + signature.as_ref().map_or(0, String::len);
                    // Unsigned text chunks can be coalesced; signed boundaries are exact.
                    if signature.is_none()
                        && let Some(ReplayItem {
                            before_call,
                            data:
                                ReplayData::GoogleText {
                                    text: Some(prior),
                                    thought: prior_thought,
                                    signature: None,
                                },
                        }) = items.last_mut()
                        && *before_call == calls.len()
                        && *prior_thought == thought
                        && let Some(text) = &text
                    {
                        prior.push_str(text);
                    } else {
                        items.push(ReplayItem {
                            before_call: calls.len(),
                            data: ReplayData::GoogleText {
                                text,
                                thought,
                                signature,
                            },
                        });
                    }
                }
                if replay_bytes > MAX_REPLAY_BYTES || items.len() > MAX_REPLAY_ITEMS {
                    return Err(ProviderError::Permanent);
                }
            }
        }
    }
    let finish = match finish.as_deref() {
        Some("STOP") if !calls.is_empty() => FinishReason::ToolCalls,
        Some("STOP") => FinishReason::Stop,
        Some("MAX_TOKENS") if calls.is_empty() => FinishReason::Length,
        _ => return Err(ProviderError::Permanent),
    };
    let usage = normalize_usage(&usage)?;
    push_event(&mut events, ModelStreamEvent::Completed { finish, usage })?;
    let stream = ModelStream::from_events(
        req.request_id().clone(),
        req.model().clone(),
        events,
        cancel,
    )?;
    if items.is_empty() {
        return Ok(stream);
    }
    let state = ProviderReplayState::new(
        ReplayProtocol::GoogleGenAi,
        profile.provider().as_str(),
        req.model().model().as_str(),
        &super::replay::scope(profile, url),
        calls,
        items,
    )
    .map_err(|_| ProviderError::Permanent)?;
    stream.with_provider_replay(Some(state))
}
fn normalize_usage(usage: &Map<String, Value>) -> Result<NormalizedUsage, ProviderError> {
    let read = |key: &str| -> Result<Option<u64>, ProviderError> {
        match usage.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value.as_u64().map(Some).ok_or(ProviderError::Permanent),
        }
    };
    let prompt = read("promptTokenCount")?;
    let cached = read("cachedContentTokenCount")?;
    let tool = read("toolUsePromptTokenCount")?;
    let thoughts = read("thoughtsTokenCount")?;
    let candidates = read("candidatesTokenCount")?;
    let add = |a: Option<u64>, b: Option<u64>| -> Result<Option<u64>, ProviderError> {
        if a.is_none() && b.is_none() {
            Ok(None)
        } else {
            a.unwrap_or(0)
                .checked_add(b.unwrap_or(0))
                .map(Some)
                .ok_or(ProviderError::Permanent)
        }
    };
    let input = add(prompt, tool)?;
    let output = add(candidates, thoughts)?;
    if cached
        .zip(input)
        .is_some_and(|(cache, input)| cache > input)
    {
        return Err(ProviderError::Permanent);
    }
    let mut normalized = NormalizedUsage::new(
        input,
        cached,
        input.map(|input| input.saturating_sub(cached.unwrap_or(0))),
        output,
        thoughts,
        tool,
        UsageCost::Unknown,
    );
    if let Some(total) = read("totalTokenCount")? {
        normalized.insert_extra("totalTokenCount", UsageExtValue::U64(total))?;
    }
    Ok(normalized)
}
#[derive(Default)]
struct LiveText {
    pending: String,
    failed: bool,
}
impl LiveText {
    fn feed(&mut self, chunk: &str, sink: &mut dyn FnMut(&str)) {
        if self.failed {
            return;
        }
        self.pending.push_str(chunk);
        if self.pending.len() > MAX_HTTP_RESPONSE_BYTES {
            self.failed = true;
            return;
        }
        loop {
            let end = match (self.pending.find("\n\n"), self.pending.find("\r\n\r\n")) {
                (Some(a), Some(b)) => Some(if a < b { (a, 2) } else { (b, 4) }),
                (Some(a), None) => Some((a, 2)),
                (None, Some(b)) => Some((b, 4)),
                _ => None,
            };
            let Some((at, width)) = end else {
                break;
            };
            let block: String = self.pending.drain(..at + width).collect();
            for data in sse_data_blocks(&block) {
                if data == "[DONE]" {
                    continue;
                }
                let result = (|| -> Result<(), ProviderError> {
                    let frame: Value =
                        serde_json::from_str(&data).map_err(|_| ProviderError::Permanent)?;
                    if let Some(candidate) = candidate(&frame)?
                        && let Some(parts) = candidate.pointer("/content/parts")
                    {
                        for part in parts.as_array().ok_or(ProviderError::Permanent)? {
                            if part.get("thought").is_none_or(|b| b == false)
                                && let Some(text) = part.get("text").and_then(Value::as_str)
                            {
                                sink(text);
                            }
                        }
                    }
                    Ok(())
                })();
                if result.is_err() {
                    self.failed = true;
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CanonicalToolSpec, CatalogRevision, ModelId, ModelPurpose, ModelRef, ModelRequestId,
        ProviderId, ToolCall,
    };
    use protocol::{Baggage, TraceContext, TraceId};
    fn profile() -> ProviderProfile {
        ProviderProfile::new(
            "work",
            ProviderId::parse("google-genai").unwrap(),
            auth::SecretRef::from_alias("env:GEMINI_API_KEY").unwrap(),
        )
        .unwrap()
    }
    fn req() -> CanonicalModelRequest {
        CanonicalModelRequest::new(ModelRequestId::parse("req-google-1").unwrap(),ModelRef::new(profile().provider().clone(),ModelId::parse("gemini-fixture").unwrap()),ModelPurpose::Code,
            vec![CanonicalMessage::new(MessageRole::System,vec![ContentPart::text("system instructions").unwrap()],None,vec![]).unwrap(),CanonicalMessage::new(MessageRole::User,vec![ContentPart::text("task").unwrap()],None,vec![]).unwrap()],
            vec![CanonicalToolSpec::new(ToolName::parse("read").unwrap(),"read a file",json!({"type":"object","properties":{"path":{"type":"string"}},"additionalProperties":false})).unwrap()],Some(512),CatalogRevision::new(1).unwrap(),TraceContext::new(TraceId::new(),None,Baggage::empty()),&CancellationToken::new()).unwrap()
    }
    fn response(parts: Value) -> Value {
        json!({"candidates":[{"index":0,"content":{"role":"model","parts":parts},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":20,"cachedContentTokenCount":5,"candidatesTokenCount":6,"thoughtsTokenCount":3,"toolUsePromptTokenCount":2,"totalTokenCount":31}})
    }
    fn parse(request: &CanonicalModelRequest, body: &Value) -> Result<ModelStream, ProviderError> {
        parse_google_response(
            request,
            body.to_string().as_bytes(),
            &profile(),
            "https://example.test/v1beta/models/gemini-fixture:streamGenerateContent?alt=sse",
            &CancellationToken::new(),
        )
    }
    fn assistant(stream: &ModelStream) -> CanonicalMessage {
        let calls = stream
            .events()
            .iter()
            .filter_map(|event| {
                if let ModelStreamEvent::ToolCallStart { call_id, name } = event {
                    let args: String = stream
                        .events()
                        .iter()
                        .filter_map(|e| {
                            if let ModelStreamEvent::ToolCallArgumentsDelta {
                                call_id: id,
                                arguments_delta,
                            } = e
                            {
                                if id == call_id {
                                    Some(arguments_delta.as_str())
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        })
                        .collect();
                    Some(ToolCall::new(call_id.clone(), name.clone(), args).unwrap())
                } else {
                    None
                }
            })
            .collect();
        CanonicalMessage::new(MessageRole::Assistant, vec![], None, calls)
            .unwrap()
            .with_provider_replay(stream.provider_replay().unwrap().clone())
            .unwrap()
    }
    #[test]
    fn google_file_status_and_deletion_use_bound_named_resources() {
        use super::super::openai_compatible::{ProviderHttpRequest, ProviderHttpResponse};
        use crate::provider::UsageFieldSet;
        use auth::CredentialStore as _;
        struct Files {
            calls: std::sync::Mutex<Vec<(ProviderControlMethod, String)>>,
            replies: std::sync::Mutex<std::collections::VecDeque<(u16, Vec<u8>)>>,
        }
        impl HttpTransport for Files {
            fn execute(
                &self,
                _: &ProviderHttpRequest<'_>,
                _: &CancellationToken,
            ) -> Result<ProviderHttpResponse, ProviderError> {
                panic!("unexpected POST")
            }
            fn execute_control(
                &self,
                method: ProviderControlMethod,
                request: &ProviderHttpRequest<'_>,
                _: &CancellationToken,
            ) -> Result<ProviderHttpResponse, ProviderError> {
                self.calls
                    .lock()
                    .unwrap()
                    .push((method, request.url().to_owned()));
                let (status, body) = self.replies.lock().unwrap().pop_front().unwrap();
                ProviderHttpResponse::new(status, vec![], body)
            }
        }
        let store = auth::InMemoryCredentialStore::new();
        let secret = auth::SecretValue::from_str(
            auth::SecretRef::from_alias("env:GEMINI_API_KEY").unwrap(),
            "test-key",
        )
        .unwrap();
        store
            .put(
                auth::CredentialPut::ephemeral(secret, auth::CredentialKind::ProviderApiKey)
                    .with_provider("google-genai")
                    .unwrap(),
                &auth::CancellationToken::new(),
            )
            .unwrap();
        let caps = ProviderCapabilities::new(
            true,
            true,
            false,
            false,
            ReasoningSupport::None,
            false,
            8192,
            1024,
            UsageFieldSet::new(false, false, false, false, false, false, false),
        )
        .unwrap();
        let config = GoogleGenAiConfig::new(
            profile(),
            GoogleGenAiEndpoint::new("https://example.test/v1beta").unwrap(),
            caps,
        )
        .unwrap();
        let transport = Files {
            calls: std::sync::Mutex::new(Vec::new()),
            replies: std::sync::Mutex::new(std::collections::VecDeque::from([
                (
                    200,
                    br#"{"name":"files/abc-123","state":"PROCESSING"}"#.to_vec(),
                ),
                (
                    200,
                    br#"{"name":"files/abc-123","state":"ACTIVE"}"#.to_vec(),
                ),
                (200, br#"{"name":"files/other","state":"ACTIVE"}"#.to_vec()),
                (
                    200,
                    br#"{"name":"files/abc-123","state":"UNKNOWN"}"#.to_vec(),
                ),
                (204, Vec::new()),
                (404, Vec::new()),
                (200, br#"{"name":"files/abc-123","state":"ACTIVE","mimeType":"video/mp4","sizeBytes":"5","sha256Hash":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="}"#.to_vec()),
                (200, br#"{"name":"files/abc-123","state":"ACTIVE","mimeType":"video/mp4","sizeBytes":"5","sha256Hash":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="}"#.to_vec()),
                (200, br#"{"name":"files/abc-123","state":"ACTIVE","mimeType":"video/mp4","sizeBytes":"5","sha256Hash":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="}"#.to_vec()),
                (200, br#"{"name":"files/abc-123","state":"ACTIVE","mimeType":"video/mp4","sizeBytes":"5","sha256Hash":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="}"#.to_vec()),
                (200, br#"{"name":"files/abc-123","state":"ACTIVE","mimeType":"video/mp4"}"#.to_vec()),
            ])),
        };
        let adapter = GoogleGenAiAdapter::new(config, transport, &store);
        let file = adapter.bind_file("files/abc-123").unwrap();
        assert_eq!(
            adapter
                .file_status(&file, &CancellationToken::new())
                .unwrap(),
            GoogleFileState::Processing
        );
        assert_eq!(
            adapter
                .file_status(&file, &CancellationToken::new())
                .unwrap(),
            GoogleFileState::Active
        );
        assert_eq!(
            adapter.file_status(&file, &CancellationToken::new()),
            Err(ProviderError::Permanent)
        );
        assert_eq!(
            adapter.file_status(&file, &CancellationToken::new()),
            Err(ProviderError::Permanent)
        );
        assert!(
            adapter
                .delete_file(&file, &CancellationToken::new())
                .is_ok()
        );
        assert_eq!(
            adapter.delete_file(&file, &CancellationToken::new()),
            Err(ProviderError::Permanent)
        );
        assert_eq!(
            adapter.verify_video_file(&file, "video/mp4", 5, [0; 32], &CancellationToken::new()),
            Ok(GoogleFileState::Active)
        );
        for (mime, size, hash) in [
            ("video/webm", 5, [0; 32]),
            ("video/mp4", 6, [0; 32]),
            ("video/mp4", 5, [1; 32]),
            ("video/mp4", 5, [0; 32]),
        ] {
            assert_eq!(
                adapter.verify_video_file(&file, mime, size, hash, &CancellationToken::new()),
                Err(ProviderError::InvalidRequest)
            );
        }
        let calls = adapter.transport.calls.lock().unwrap();
        assert_eq!(calls.len(), 11);
        assert!(
            calls
                .iter()
                .all(|(_, url)| url == "https://example.test:443/v1beta/files/abc-123")
        );
        assert_eq!(calls[4].0, ProviderControlMethod::Delete);
        drop(calls);
        let mut different_config = adapter.config().clone();
        different_config.endpoint = GoogleGenAiEndpoint::new("https://other.test/v1beta").unwrap();
        let other = GoogleGenAiAdapter::new(
            different_config,
            Files {
                calls: std::sync::Mutex::new(Vec::new()),
                replies: std::sync::Mutex::new(std::collections::VecDeque::new()),
            },
            &store,
        );
        assert_eq!(
            other.file_status(&file, &CancellationToken::new()),
            Err(ProviderError::InvalidRequest)
        );
        assert!(other.transport.calls.lock().unwrap().is_empty());
        let mut different_account = adapter.config().clone();
        different_account.profile = ProviderProfile::new(
            "work",
            ProviderId::parse("google-genai").unwrap(),
            auth::SecretRef::from_alias("another-account").unwrap(),
        )
        .unwrap();
        let other = GoogleGenAiAdapter::new(
            different_account,
            Files {
                calls: std::sync::Mutex::new(Vec::new()),
                replies: std::sync::Mutex::new(std::collections::VecDeque::new()),
            },
            &store,
        );
        assert_eq!(
            other.delete_file(&file, &CancellationToken::new()),
            Err(ProviderError::InvalidRequest)
        );
        assert!(other.transport.calls.lock().unwrap().is_empty());
        for bad in [
            "https://example.test/files/x",
            "files/../private",
            "files/x?key=a",
            "files/",
            "files/-invalid",
            "files/UPPER",
            "files/x/y",
        ] {
            assert!(GoogleFileName::parse(bad).is_err(), "{bad}");
        }
        assert!(GoogleFileName::parse(&format!("files/{}", "a".repeat(40))).is_ok());
        assert!(GoogleFileName::parse(&format!("files/{}", "a".repeat(41))).is_err());
        assert!(!format!("{file:?}").contains("abc-123"));
    }

    #[test]
    fn google_resumable_video_upload_is_scoped_chunked_and_fail_closed_on_uncertainty() {
        use super::super::openai_compatible::{ProviderHttpRequest, ProviderHttpResponse};
        use crate::provider::UsageFieldSet;
        use auth::CredentialStore as _;
        use sha2::Digest as _;
        type CapturedUpload = (String, Vec<(String, String)>, Vec<u8>);
        struct UploadFixture {
            calls: std::sync::Mutex<Vec<CapturedUpload>>,
            replies: std::sync::Mutex<
                std::collections::VecDeque<Result<ProviderHttpResponse, ProviderError>>,
            >,
        }
        impl HttpTransport for UploadFixture {
            fn execute(
                &self,
                request: &ProviderHttpRequest<'_>,
                _: &CancellationToken,
            ) -> Result<ProviderHttpResponse, ProviderError> {
                self.calls.lock().unwrap().push((
                    request.url().to_owned(),
                    request.headers().to_vec(),
                    request.body().to_vec(),
                ));
                self.replies.lock().unwrap().pop_front().unwrap()
            }
        }
        let store = auth::InMemoryCredentialStore::new();
        let secret = auth::SecretValue::from_str(
            auth::SecretRef::from_alias("env:GEMINI_API_KEY").unwrap(),
            "test-key",
        )
        .unwrap();
        store
            .put(
                auth::CredentialPut::ephemeral(secret, auth::CredentialKind::ProviderApiKey)
                    .with_provider("google-genai")
                    .unwrap(),
                &auth::CancellationToken::new(),
            )
            .unwrap();
        let caps = ProviderCapabilities::new(
            true,
            true,
            false,
            false,
            ReasoningSupport::None,
            false,
            8192,
            1024,
            UsageFieldSet::new(false, false, false, false, false, false, false),
        )
        .unwrap();
        let config = GoogleGenAiConfig::new(
            profile(),
            GoogleGenAiEndpoint::new("https://example.test/v1beta").unwrap(),
            caps,
        )
        .unwrap()
        .with_video_input(true);
        let start = || {
            ProviderHttpResponse::new(
                200,
                vec![
                    (
                        "X-Goog-Upload-URL".into(),
                        "https://example.test/upload/v1beta/files?upload_id=opaque".into(),
                    ),
                    ("X-Goog-Upload-Chunk-Granularity".into(), "2".into()),
                ],
                vec![],
            )
            .unwrap()
        };
        let transport = UploadFixture {
            calls: std::sync::Mutex::new(Vec::new()),
            replies: std::sync::Mutex::new(std::collections::VecDeque::from([
                Ok(start()),
                Ok(ProviderHttpResponse::new(
                    200,
                    vec![("X-Goog-Upload-Size-Received".into(), "2".into())],
                    vec![],
                )
                .unwrap()),
                Ok(ProviderHttpResponse::new(
                    200,
                    vec![],
                    br#"{"file":{"name":"files/abc-123","mimeType":"video/mp4"}}"#.to_vec(),
                )
                .unwrap()),
                Ok(start()),
                Err(ProviderError::Connection),
                Ok(ProviderHttpResponse::new(
                    200,
                    vec![(
                        "X-Goog-Upload-URL".into(),
                        "https://attacker.test/upload/v1beta/files?upload_id=opaque".into(),
                    )],
                    vec![],
                )
                .unwrap()),
                Ok(start()),
                Ok(start()),
                Ok(ProviderHttpResponse::new(
                    200,
                    vec![],
                    br#"{"file":{"name":"files/recoverable","mimeType":"video/mp4"}}"#.to_vec(),
                )
                .unwrap()),
                Ok(start()),
                Ok(ProviderHttpResponse::new(
                    200,
                    vec![],
                    br#"{"file":{"name":"files/different","mimeType":"video/mp4"}}"#.to_vec(),
                )
                .unwrap()),
            ])),
        };
        let adapter = GoogleGenAiAdapter::new(config, transport, &store);
        assert!(matches!(
            adapter.begin_video_upload(
                "video/mp4",
                MAX_UPLOADED_VIDEO_BYTES + 1,
                [0; 32],
                &CancellationToken::new()
            ),
            Err(GoogleUploadError::Rejected(ProviderError::InvalidRequest))
        ));
        let mut upload = adapter
            .begin_video_upload(
                "video/mp4",
                5,
                sha2::Sha256::digest(b"abcde").into(),
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(upload.preferred_chunk_size(), MAX_VIDEO_UPLOAD_CHUNK_BYTES);
        upload.chunk_granularity = 3;
        assert_eq!(upload.preferred_chunk_size() % 3, 0);
        assert!(upload.preferred_chunk_size() <= MAX_VIDEO_UPLOAD_CHUNK_BYTES);
        upload.chunk_granularity = 2;
        assert!(!format!("{upload:?}").contains("opaque"));
        assert!(matches!(
            adapter.upload_video_chunk(&mut upload, b"a", &CancellationToken::new()),
            Err(GoogleUploadError::Rejected(ProviderError::InvalidRequest))
        ));
        assert_eq!(
            adapter
                .upload_video_chunk(&mut upload, b"ab", &CancellationToken::new())
                .unwrap(),
            None
        );
        assert_eq!(upload.offset(), 2);
        let file = adapter
            .upload_video_chunk(&mut upload, b"cde", &CancellationToken::new())
            .unwrap()
            .unwrap();
        assert!(!format!("{file:?}").contains("abc-123"));
        assert!(matches!(
            adapter.upload_video_chunk(&mut upload, b"again", &CancellationToken::new()),
            Err(GoogleUploadError::Rejected(ProviderError::InvalidRequest))
        ));
        let mut uncertain = adapter
            .begin_video_upload(
                "video/mp4",
                2,
                sha2::Sha256::digest(b"ab").into(),
                &CancellationToken::new(),
            )
            .unwrap();
        assert!(matches!(
            adapter.upload_video_chunk(&mut uncertain, b"ab", &CancellationToken::new()),
            Err(GoogleUploadError::Uncertain)
        ));
        assert!(uncertain.is_uncertain());
        assert!(matches!(
            adapter.upload_video_chunk(&mut uncertain, b"ab", &CancellationToken::new()),
            Err(GoogleUploadError::Rejected(ProviderError::InvalidRequest))
        ));
        assert!(matches!(
            adapter.begin_video_upload(
                "video/mp4",
                2,
                sha2::Sha256::digest(b"ab").into(),
                &CancellationToken::new()
            ),
            Err(GoogleUploadError::Uncertain)
        ));
        let mut wrong_content = adapter
            .begin_video_upload(
                "video/mp4",
                2,
                sha2::Sha256::digest(b"ab").into(),
                &CancellationToken::new(),
            )
            .unwrap();
        assert!(matches!(
            adapter.upload_video_chunk(&mut wrong_content, b"zz", &CancellationToken::new()),
            Err(GoogleUploadError::Uncertain)
        ));
        assert!(wrong_content.is_uncertain());
        assert!(matches!(
            adapter.begin_named_video_upload(
                "files/UPPER",
                "video/mp4",
                2,
                [0; 32],
                &CancellationToken::new()
            ),
            Err(GoogleUploadError::Rejected(ProviderError::InvalidRequest))
        ));
        let mut named = adapter
            .begin_named_video_upload(
                "files/recoverable",
                "video/mp4",
                2,
                sha2::Sha256::digest(b"ab").into(),
                &CancellationToken::new(),
            )
            .unwrap();
        assert!(
            adapter
                .upload_video_chunk(&mut named, b"ab", &CancellationToken::new())
                .unwrap()
                .is_some()
        );
        let mut mismatch = adapter
            .begin_named_video_upload(
                "files/recoverable",
                "video/mp4",
                2,
                sha2::Sha256::digest(b"ab").into(),
                &CancellationToken::new(),
            )
            .unwrap();
        assert!(matches!(
            adapter.upload_video_chunk(&mut mismatch, b"ab", &CancellationToken::new()),
            Err(GoogleUploadError::Uncertain)
        ));
        assert!(mismatch.is_uncertain());
        let calls = adapter.transport.calls.lock().unwrap();
        assert_eq!(calls.len(), 11);
        let named_start: Value = serde_json::from_slice(&calls[7].2).unwrap();
        assert_eq!(named_start["file"]["name"], "files/recoverable");
        assert_eq!(calls[0].0, "https://example.test:443/upload/v1beta/files");
        assert_eq!(calls[1].2, b"ab");
        assert_eq!(calls[2].2, b"cde");
        assert!(
            calls[1]
                .1
                .contains(&("x-goog-upload-offset".into(), "0".into()))
        );
        assert!(
            calls[2]
                .1
                .contains(&("x-goog-upload-offset".into(), "2".into()))
        );
        assert!(
            calls[2]
                .1
                .contains(&("x-goog-upload-command".into(), "upload, finalize".into()))
        );
    }

    #[test]
    fn google_resumable_video_upload_inference_and_cleanup_use_guarded_http() {
        use super::super::openai_compatible::{Http1Transport, StaticWireAuth};
        use crate::provider::UsageFieldSet;
        use auth::CredentialStore as _;
        use sha2::Digest as _;
        use std::io::{Read as _, Write as _};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        struct Gate(Arc<AtomicUsize>);
        impl super::super::dial::DialGate for Gate {
            fn permit(
                &self,
                _: super::super::dial::DialTarget<'_>,
                _: Option<&super::super::dial::ProxyTarget>,
                addrs: &[std::net::SocketAddr],
            ) -> Result<Vec<std::net::SocketAddr>, ProviderError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(addrs.to_vec())
            }
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for i in 0..6 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                    .unwrap();
                let mut data = Vec::new();
                let mut buf = [0; 1024];
                loop {
                    let n = stream.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    data.extend_from_slice(&buf[..n]);
                    if let Some(end) = data.windows(4).position(|window| window == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&data[..end]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                line.split_once(':')
                                    .filter(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                                    .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if data.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                requests.push(String::from_utf8(data).unwrap());
                let (extra, body) = match i {
                    0 => (
                        format!(
                            "X-Goog-Upload-URL: http://{addr}/upload/v1beta/files?upload_id=opaque\r\n"
                        ),
                        String::new(),
                    ),
                    1 => (
                        "X-Goog-Upload-Size-Received: 2\r\n".to_owned(),
                        String::new(),
                    ),
                    2 => (
                        String::new(),
                        r#"{"file":{"name":"files/abc-123","mimeType":"video/mp4"}}"#.to_owned(),
                    ),
                    3 => (
                        String::new(),
                        json!({"name":"files/abc-123","state":"ACTIVE","mimeType":"video/mp4","uri":format!("http://{addr}/provider-media/abc-123")}).to_string(),
                    ),
                    4 => (
                        String::new(),
                        response(json!([{"text":"video received"}])).to_string(),
                    ),
                    _ => (String::new(), String::new()),
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
            requests
        });
        let store = auth::InMemoryCredentialStore::new();
        let secret = auth::SecretValue::from_str(
            auth::SecretRef::from_alias("env:GEMINI_API_KEY").unwrap(),
            "test-key",
        )
        .unwrap();
        store
            .put(
                auth::CredentialPut::ephemeral(secret, auth::CredentialKind::ProviderApiKey)
                    .with_provider("google-genai")
                    .unwrap(),
                &auth::CancellationToken::new(),
            )
            .unwrap();
        let caps = ProviderCapabilities::new(
            true,
            true,
            false,
            false,
            ReasoningSupport::None,
            false,
            8192,
            1024,
            UsageFieldSet::new(false, false, false, false, false, false, false),
        )
        .unwrap();
        let config = GoogleGenAiConfig::new(
            profile(),
            GoogleGenAiEndpoint::new(&format!("http://{addr}/v1beta")).unwrap(),
            caps,
        )
        .unwrap()
        .with_video_input(true);
        let count = Arc::new(AtomicUsize::new(0));
        let transport = Http1Transport::new(StaticWireAuth::bearer("fixture-key").unwrap())
            .with_dial_gate(Arc::new(Gate(count.clone())));
        let adapter = GoogleGenAiAdapter::new(config, transport, &store);
        let mut upload = adapter
            .begin_named_video_upload(
                "files/abc-123",
                "video/mp4",
                5,
                sha2::Sha256::digest(b"abcde").into(),
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(
            adapter
                .upload_video_chunk(&mut upload, b"ab", &CancellationToken::new())
                .unwrap(),
            None
        );
        let handle = adapter
            .upload_video_chunk(&mut upload, b"cde", &CancellationToken::new())
            .unwrap()
            .unwrap();
        let part = adapter.video_file_part(&handle, "video/mp4").unwrap();
        let roundtrip: ContentPart =
            serde_json::from_slice(&serde_json::to_vec(&part).unwrap()).unwrap();
        assert_eq!(roundtrip, part);
        assert!(!format!("{part:?}").contains("abc-123"));
        let mut messages = req().messages().to_vec();
        messages.push(CanonicalMessage::new(MessageRole::User, vec![part], None, vec![]).unwrap());
        adapter
            .invoke_sync(
                req().with_messages(messages).unwrap(),
                &CancellationToken::new(),
            )
            .unwrap();
        adapter
            .delete_file(&handle, &CancellationToken::new())
            .unwrap();
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /upload/v1beta/files HTTP/1.1\r\n"));
        let start_body: Value =
            serde_json::from_str(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(start_body["file"]["name"], "files/abc-123");
        assert!(requests[1].starts_with("POST /upload/v1beta/files?upload_id=opaque HTTP/1.1\r\n"));
        assert!(requests[2].contains("x-goog-upload-command: upload, finalize\r\n"));
        assert!(requests[2].contains("x-goog-upload-offset: 2\r\n"));
        assert!(requests[1].ends_with("\r\n\r\nab") && requests[2].ends_with("\r\n\r\ncde"));
        assert!(requests[3].starts_with("GET /v1beta/files/abc-123 HTTP/1.1\r\n"));
        assert!(requests[4].starts_with(
            "POST /v1beta/models/gemini-fixture:streamGenerateContent?alt=sse HTTP/1.1\r\n"
        ));
        let inference: Value =
            serde_json::from_str(requests[4].split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(
            inference["contents"][0]["parts"][1]["fileData"]["fileUri"],
            format!("http://{addr}/provider-media/abc-123")
        );
        assert_eq!(
            inference["contents"][0]["parts"][1]["fileData"]["mimeType"],
            "video/mp4"
        );
        assert!(requests[5].starts_with("DELETE /v1beta/files/abc-123 HTTP/1.1\r\n"));
        assert!(
            requests
                .iter()
                .all(|request| request.contains("x-goog-api-key: fixture-key\r\n"))
        );
        assert_eq!(count.load(Ordering::SeqCst), 6);
    }

    #[test]
    fn uploaded_video_is_refused_for_wrong_binding_state_mime_or_provider() {
        use super::super::openai_compatible::{ProviderHttpRequest, ProviderHttpResponse};
        use crate::provider::UsageFieldSet;
        use auth::CredentialStore as _;
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        struct Status {
            state: &'static str,
            mime: &'static str,
            uri: Option<&'static str>,
            gets: Arc<AtomicUsize>,
            posts: Arc<AtomicUsize>,
        }
        impl HttpTransport for Status {
            fn execute(
                &self,
                _: &ProviderHttpRequest<'_>,
                _: &CancellationToken,
            ) -> Result<ProviderHttpResponse, ProviderError> {
                self.posts.fetch_add(1, Ordering::SeqCst);
                Err(ProviderError::Connection)
            }
            fn execute_control(
                &self,
                _: ProviderControlMethod,
                _: &ProviderHttpRequest<'_>,
                _: &CancellationToken,
            ) -> Result<ProviderHttpResponse, ProviderError> {
                self.gets.fetch_add(1, Ordering::SeqCst);
                ProviderHttpResponse::new(
                    200,
                    vec![],
                    json!({"name":"files/abc-123","state":self.state,"mimeType":self.mime,"uri":self.uri})
                        .to_string()
                        .into_bytes(),
                )
            }
        }
        let store = auth::InMemoryCredentialStore::new();
        let secret = auth::SecretValue::from_str(
            auth::SecretRef::from_alias("env:GEMINI_API_KEY").unwrap(),
            "test-key",
        )
        .unwrap();
        store
            .put(
                auth::CredentialPut::ephemeral(secret, auth::CredentialKind::ProviderApiKey)
                    .with_provider("google-genai")
                    .unwrap(),
                &auth::CancellationToken::new(),
            )
            .unwrap();
        let caps = ProviderCapabilities::new(
            true,
            true,
            false,
            false,
            ReasoningSupport::None,
            false,
            8192,
            1024,
            UsageFieldSet::new(false, false, false, false, false, false, false),
        )
        .unwrap();
        let config = GoogleGenAiConfig::new(
            profile(),
            GoogleGenAiEndpoint::new("https://example.test/v1beta").unwrap(),
            caps,
        )
        .unwrap()
        .with_video_input(true);
        for (state, mime, uri) in [
            ("PROCESSING", "video/mp4", None),
            ("ACTIVE", "video/webm", None),
            ("FAILED", "video/mp4", None),
            ("ACTIVE", "video/mp4", None),
            ("ACTIVE", "video/mp4", Some("https://user@evil.test/file")),
            (
                "ACTIVE",
                "video/mp4",
                Some("https://example.test/file?key=secret"),
            ),
            ("ACTIVE", "video/mp4", Some("http://example.test/file")),
            ("ACTIVE", "video/mp4", Some("https://169.254.169.254/file")),
        ] {
            let gets = Arc::new(AtomicUsize::new(0));
            let posts = Arc::new(AtomicUsize::new(0));
            let adapter = GoogleGenAiAdapter::new(
                config.clone(),
                Status {
                    state,
                    mime,
                    uri,
                    gets: gets.clone(),
                    posts: posts.clone(),
                },
                &store,
            );
            let file = adapter.bind_file("files/abc-123").unwrap();
            let part = adapter.video_file_part(&file, "video/mp4").unwrap();
            let mut messages = req().messages().to_vec();
            messages
                .push(CanonicalMessage::new(MessageRole::User, vec![part], None, vec![]).unwrap());
            let request = req().with_messages(messages).unwrap();
            assert!(matches!(
                adapter.invoke_sync(request.clone(), &CancellationToken::new()),
                Err(ProviderError::InvalidRequest)
            ));
            assert_eq!(gets.load(Ordering::SeqCst), 1);
            assert_eq!(posts.load(Ordering::SeqCst), 0);
            for style in [OpenAiApiStyle::ChatCompletions, OpenAiApiStyle::Responses] {
                assert_eq!(
                    super::super::openai_compatible::encode_provider_payload(
                        &request,
                        style,
                        &CancellationToken::new()
                    ),
                    Err(ProviderError::InvalidRequest)
                );
            }
            assert_eq!(
                super::super::anthropic::encode_anthropic_payload(
                    &request,
                    512,
                    &CancellationToken::new()
                ),
                Err(ProviderError::InvalidRequest)
            );
            let mut other_config = config.clone();
            other_config.endpoint = GoogleGenAiEndpoint::new("https://other.test/v1beta").unwrap();
            let other = GoogleGenAiAdapter::new(
                other_config,
                Status {
                    state,
                    mime,
                    uri,
                    gets: gets.clone(),
                    posts: posts.clone(),
                },
                &store,
            );
            assert!(matches!(
                other.invoke_sync(request, &CancellationToken::new()),
                Err(ProviderError::InvalidRequest)
            ));
            assert_eq!(gets.load(Ordering::SeqCst), 1);
            assert_eq!(posts.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn google_file_control_reaches_a_guarded_http_endpoint_with_api_key() {
        use super::super::openai_compatible::{Http1Transport, StaticWireAuth};
        use crate::provider::UsageFieldSet;
        use auth::CredentialStore as _;
        use std::io::{Read as _, Write as _};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        struct Gate(Arc<AtomicUsize>);
        impl super::super::dial::DialGate for Gate {
            fn permit(
                &self,
                _: super::super::dial::DialTarget<'_>,
                _: Option<&super::super::dial::ProxyTarget>,
                addrs: &[std::net::SocketAddr],
            ) -> Result<Vec<std::net::SocketAddr>, ProviderError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(addrs.to_vec())
            }
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for i in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                    .unwrap();
                let mut data = Vec::new();
                let mut buf = [0; 1024];
                while !data.windows(4).any(|window| window == b"\r\n\r\n") {
                    let n = stream.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    data.extend_from_slice(&buf[..n]);
                }
                requests.push(String::from_utf8(data).unwrap());
                let body = if i == 0 {
                    r#"{"name":"files/abc-123","state":"ACTIVE"}"#
                } else {
                    ""
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
            requests
        });
        let store = auth::InMemoryCredentialStore::new();
        let secret = auth::SecretValue::from_str(
            auth::SecretRef::from_alias("env:GEMINI_API_KEY").unwrap(),
            "test-key",
        )
        .unwrap();
        store
            .put(
                auth::CredentialPut::ephemeral(secret, auth::CredentialKind::ProviderApiKey)
                    .with_provider("google-genai")
                    .unwrap(),
                &auth::CancellationToken::new(),
            )
            .unwrap();
        let caps = ProviderCapabilities::new(
            true,
            true,
            false,
            false,
            ReasoningSupport::None,
            false,
            8192,
            1024,
            UsageFieldSet::new(false, false, false, false, false, false, false),
        )
        .unwrap();
        let config = GoogleGenAiConfig::new(
            profile(),
            GoogleGenAiEndpoint::new(&format!("http://{addr}/v1beta")).unwrap(),
            caps,
        )
        .unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let transport = Http1Transport::new(StaticWireAuth::bearer("fixture-key").unwrap())
            .with_dial_gate(Arc::new(Gate(count.clone())));
        let adapter = GoogleGenAiAdapter::new(config, transport, &store);
        let file = adapter.bind_file("files/abc-123").unwrap();
        assert_eq!(
            adapter
                .file_status(&file, &CancellationToken::new())
                .unwrap(),
            GoogleFileState::Active
        );
        adapter
            .delete_file(&file, &CancellationToken::new())
            .unwrap();
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /v1beta/files/abc-123 HTTP/1.1\r\n"));
        assert!(requests[1].starts_with("DELETE /v1beta/files/abc-123 HTTP/1.1\r\n"));
        assert!(
            requests
                .iter()
                .all(|request| request.contains("x-goog-api-key: fixture-key\r\n"))
        );
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }
    #[test]
    fn inline_video_is_bounded_explicit_and_never_silently_sent_elsewhere() {
        use super::super::openai_compatible::{ProviderHttpRequest, ProviderHttpResponse};
        use crate::provider::UsageFieldSet;
        struct NeverDial;
        impl HttpTransport for NeverDial {
            fn execute(
                &self,
                _: &ProviderHttpRequest<'_>,
                _: &CancellationToken,
            ) -> Result<ProviderHttpResponse, ProviderError> {
                panic!("video must be rejected before transport without explicit opt-in")
            }
        }
        let video = ContentPart::video_data("video/mp4", b"video fixture").unwrap();
        let mut messages = req().messages().to_vec();
        messages.push(
            CanonicalMessage::new(MessageRole::User, vec![video.clone()], None, vec![]).unwrap(),
        );
        let request = req().with_messages(messages).unwrap();
        let wire = encode_google_payload(&request, &CancellationToken::new()).unwrap();
        assert_eq!(
            wire["contents"][0]["parts"][1]["inlineData"]["mimeType"],
            "video/mp4"
        );
        assert_eq!(
            wire["contents"][0]["parts"][1]["inlineData"]["data"],
            "dmlkZW8gZml4dHVyZQ=="
        );
        let encoded = serde_json::to_vec(&video).unwrap();
        assert_eq!(
            serde_json::from_slice::<ContentPart>(&encoded).unwrap(),
            video
        );
        assert!(ContentPart::video_data("text/plain", b"x").is_err());
        assert!(
            ContentPart::video_data(
                "video/mp4",
                &vec![0; crate::provider::MAX_INLINE_VIDEO_BYTES + 1]
            )
            .is_err()
        );
        let malformed = ContentPart::VideoData {
            mime_type: "video/mp4".into(),
            data: "%%%".into(),
        };
        let mut malformed_messages = req().messages().to_vec();
        malformed_messages
            .push(CanonicalMessage::new(MessageRole::User, vec![malformed], None, vec![]).unwrap());
        assert_eq!(
            encode_google_payload(
                &req().with_messages(malformed_messages).unwrap(),
                &CancellationToken::new()
            ),
            Err(ProviderError::InvalidRequest)
        );
        let caps = ProviderCapabilities::new(
            true,
            true,
            false,
            false,
            ReasoningSupport::None,
            false,
            8192,
            1024,
            UsageFieldSet::new(false, false, false, false, false, false, false),
        )
        .unwrap();
        let config = GoogleGenAiConfig::new(
            profile(),
            GoogleGenAiEndpoint::new("https://example.test/v1beta").unwrap(),
            caps,
        )
        .unwrap();
        let store = auth::InMemoryCredentialStore::new();
        assert!(matches!(
            GoogleGenAiAdapter::new(config.clone(), NeverDial, &store)
                .invoke_sync(request.clone(), &CancellationToken::new()),
            Err(ProviderError::InvalidRequest)
        ));
        use auth::CredentialStore as _;
        let secret = auth::SecretValue::from_str(
            auth::SecretRef::from_alias("env:GEMINI_API_KEY").unwrap(),
            "test-key",
        )
        .unwrap();
        store
            .put(
                auth::CredentialPut::ephemeral(secret, auth::CredentialKind::ProviderApiKey)
                    .with_provider("google-genai")
                    .unwrap(),
                &auth::CancellationToken::new(),
            )
            .unwrap();
        struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl HttpTransport for Capture {
            fn execute(
                &self,
                request: &ProviderHttpRequest<'_>,
                _: &CancellationToken,
            ) -> Result<ProviderHttpResponse, ProviderError> {
                *self.0.lock().unwrap() = request.body().to_vec();
                ProviderHttpResponse::new(
                    200,
                    vec![],
                    response(json!([{"text":"ok"}])).to_string().into_bytes(),
                )
            }
        }
        let sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let adapter =
            GoogleGenAiAdapter::new(config.with_video_input(true), Capture(sent.clone()), &store);
        assert!(
            adapter
                .invoke_sync(request.clone(), &CancellationToken::new())
                .is_ok()
        );
        let sent: Value = serde_json::from_slice(&sent.lock().unwrap()).unwrap();
        assert_eq!(
            sent["contents"][0]["parts"][1]["inlineData"]["data"],
            "dmlkZW8gZml4dHVyZQ=="
        );
        for style in [OpenAiApiStyle::ChatCompletions, OpenAiApiStyle::Responses] {
            assert_eq!(
                super::super::openai_compatible::encode_provider_payload(
                    &request,
                    style,
                    &CancellationToken::new()
                ),
                Err(ProviderError::InvalidRequest)
            );
        }
        assert_eq!(
            super::super::anthropic::encode_anthropic_payload(
                &request,
                512,
                &CancellationToken::new()
            ),
            Err(ProviderError::InvalidRequest)
        );
    }
    #[test]
    fn google_native_payload_preserves_system_images_tools_bounds_and_explicit_effort() {
        let request = req();
        let mut messages = request.messages().to_vec();
        messages.push(
            CanonicalMessage::new(
                MessageRole::User,
                vec![ContentPart::image_data("data:image/png;base64,aGVsbG8=").unwrap()],
                None,
                vec![],
            )
            .unwrap(),
        );
        let request = request
            .with_messages(messages)
            .unwrap()
            .with_reasoning_effort(ReasoningEffort::Low);
        let wire = encode_google_payload(&request, &CancellationToken::new()).unwrap();
        assert_eq!(
            wire["systemInstruction"]["parts"][0]["text"],
            "system instructions"
        );
        assert_eq!(
            wire["contents"][0]["parts"][1]["inlineData"]["data"],
            "aGVsbG8="
        );
        assert_eq!(
            wire["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "LOW"
        );
        assert_eq!(wire["generationConfig"]["maxOutputTokens"], 512);
        assert_eq!(
            wire["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"]["additionalProperties"],
            false
        );
        for model in [
            "../evil",
            "gemini?key=x",
            "models/evil/path",
            "gemini#x",
            "gemini:bad",
            "",
        ] {
            assert!(
                GoogleGenAiEndpoint::new("https://example.test/v1beta")
                    .unwrap()
                    .request_url(model)
                    .is_err(),
                "{model}"
            );
        }
        assert_eq!(
            GoogleGenAiEndpoint::new("https://example.test/v1beta/")
                .unwrap()
                .request_url("models/gemini-fixture")
                .unwrap(),
            "https://example.test:443/v1beta/models/gemini-fixture:streamGenerateContent?alt=sse"
        );
        for origin in [
            "http://169.254.169.254/v1beta",
            "http://u:p@example.test",
            "https://example.test/v1beta?key=secret",
        ] {
            assert!(GoogleGenAiEndpoint::new(origin).is_err());
        }
    }
    #[test]
    fn google_parallel_calls_and_signed_text_replay_exact_parts_and_original_optional_ids() {
        let request = req();
        let parts = json!([
            {"text":"private-thought","thought":true,"thoughtSignature":"think-signature"},
            {"text":"explaining","thoughtSignature":"text-signature"},
            {"functionCall":{"name":"read","args":{"path":"a.txt"}},"thoughtSignature":"call-signature"},
            {"functionCall":{"id":"call-z","name":"read","args":{"path":"b.txt"}}},
            {"thoughtSignature":"empty-final-signature"}
        ]);
        let body = response(parts.clone());
        let stream = parse(&request, &body).unwrap();
        let text: String = stream
            .events()
            .iter()
            .filter_map(|e| {
                if let ModelStreamEvent::TextDelta { text } = e {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(text, "explaining");
        assert!(!format!("{:?}", stream.provider_replay()).contains("private-thought"));
        let assistant = assistant(&stream);
        let restored: CanonicalMessage =
            serde_json::from_str(&serde_json::to_string(&assistant).unwrap()).unwrap();
        let mut messages = request.messages().to_vec();
        messages.push(restored.clone());
        for call in restored.tool_calls() {
            messages.push(
                CanonicalMessage::new(
                    MessageRole::Tool,
                    vec![
                        ContentPart::text("result").unwrap(),
                        ContentPart::image_data("data:image/jpeg;base64,aGVsbG8=").unwrap(),
                    ],
                    Some(call.call_id().clone()),
                    vec![],
                )
                .unwrap(),
            );
        }
        let request = request.with_messages(messages).unwrap();
        let wire = encode_google_payload(&request, &CancellationToken::new()).unwrap();
        assert_eq!(wire["contents"][1]["parts"], parts);
        let results = &wire["contents"][2]["parts"];
        assert!(results[0]["functionResponse"].get("id").is_none());
        assert_eq!(results[1]["functionResponse"]["id"], "call-z");
        assert_eq!(results[0]["functionResponse"]["name"], "read");
        assert_eq!(
            results[0]["functionResponse"]["parts"][0]["inlineData"]["mimeType"],
            "image/jpeg"
        );
        let usage = stream.terminal_usage().unwrap();
        assert_eq!(usage.input_tokens(), Some(22));
        assert_eq!(usage.output_tokens(), Some(9));
        assert_eq!(usage.cached_input_tokens(), Some(5));
        assert_eq!(usage.reasoning_tokens(), Some(3));
        assert!(
            super::super::replay::validate(
                &request,
                Some(ReplayProtocol::GoogleGenAi),
                &profile(),
                "https://other.test/v1beta"
            )
            .is_err()
        );
        let public = protocol::provider_replay::public_payload(
            &json!({"provider_replay":stream.provider_replay()}),
        );
        assert!(!public.to_string().contains("empty-final-signature"));
    }
    #[test]
    fn google_sse_coalesces_unsigned_deltas_and_preserves_signature_only_tail() {
        let request = req();
        let mut wire = String::new();
        let mut live = LiveText::default();
        let mut visible = String::new();
        for part in [
            json!({"text":"hidden","thought":true}),
            json!({"text":"🐈"}),
        ]
        .into_iter()
        .chain(std::iter::repeat_n(json!({"text":"a"}), 2000))
        .chain([json!({"text":"","thoughtSignature":"last-signature"})])
        {
            let frame = json!({"candidates":[{"content":{"role":"model","parts":[part]}}]});
            let data = format!("data: {frame}\r\n\r\n");
            for ch in data.chars() {
                live.feed(&ch.to_string(), &mut |s| visible.push_str(s));
            }
            wire.push_str(&data);
        }
        wire.push_str(&format!(
            "data: {}\r\n\r\n",
            json!({"candidates":[{"finishReason":"STOP"}]})
        ));
        let stream = parse_google_response(
            &request,
            wire.as_bytes(),
            &profile(),
            "https://example.test",
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(visible, format!("🐈{}", "a".repeat(2000)));
        assert!(!live.failed);
        let items = stream.provider_replay().unwrap().items();
        assert_eq!(items.len(), 3);
        assert!(
            matches!(&items[2].data,ReplayData::GoogleText {text:Some(text),signature:Some(sig),..} if text.is_empty()&&sig=="last-signature")
        );
    }
    #[test]
    fn google_blocks_malformed_or_incomplete_provider_output_before_tools_can_execute() {
        let request = req();
        for mut body in [
            response(
                json!([{"functionCall":{"id":"same","name":"read","args":{}}},{"functionCall":{"id":"same","name":"read","args":{}}}]),
            ),
            response(json!([{"functionCall":{"name":"../invalid","args":{}}}])),
            response(json!([{"functionCall":{"name":"read","args":[]}}])),
            response(json!([{"inlineData":{"mimeType":"image/png","data":"a"}}])),
            response(json!([{"text":"private","thought":"true"}])),
            response(json!([{"thoughtSignature":"x".repeat(MAX_REPLAY_BYTES+1)}])),
        ] {
            assert!(parse(&request, &body).is_err());
            body["candidates"][0]["finishReason"] = "SAFETY".into();
            assert!(parse(&request, &body).is_err());
        }
        for reason in [
            "SAFETY",
            "RECITATION",
            "MALFORMED_FUNCTION_CALL",
            "OTHER",
            "MISSING_THOUGHT_SIGNATURE",
            "FINISH_REASON_UNSPECIFIED",
        ] {
            let mut body = response(json!([{"text":"not successful"}]));
            body["candidates"][0]["finishReason"] = reason.into();
            assert_eq!(parse(&request, &body), Err(ProviderError::Permanent));
        }
        let mut truncated = response(json!([{"functionCall":{"name":"read","args":{}}}]));
        truncated["candidates"][0]["finishReason"] = "MAX_TOKENS".into();
        assert!(parse(&request, &truncated).is_err());
        let mut incomplete = response(json!([{"text":"partial"}]));
        incomplete["candidates"][0]
            .as_object_mut()
            .unwrap()
            .remove("finishReason");
        assert!(parse(&request, &incomplete).is_err());
        assert!(
            parse(
                &request,
                &json!({"promptFeedback":{"blockReason":"SAFETY"}})
            )
            .is_err()
        );
        let mut multi = response(json!([{"text":"x"}]));
        multi["candidates"]
            .as_array_mut()
            .unwrap()
            .push(json!({"index":1}));
        assert!(parse(&request, &multi).is_err());
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            parse_google_response(&request, b"{}", &profile(), "https://example.test", &cancel),
            Err(ProviderError::Cancelled)
        );
    }
}
