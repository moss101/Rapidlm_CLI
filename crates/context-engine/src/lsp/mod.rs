//! Optional language-server enrichment for definitions, references, and details.
//!
//! Configured servers are queried through an injected [`LspClient`]. Timeouts,
//! crashes, and unavailability degrade to empty facts so compile/search stay
//! available. Project-controlled executable config is inactive until trust is
//! established. Locations are content/version checked against a caller-supplied
//! snapshot before they become indexable facts tagged `source=lsp`.

use std::error::Error;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use protocol::{RepoId, RepoPath};

use crate::ingest::content::{ContentHash, SourceLanguage};
use crate::repo_manifest::CancellationToken;

/// Default wall-clock budget for one enrichment call.
pub const DEFAULT_LSP_TIMEOUT: Duration = Duration::from_secs(2);

/// Default crash restart backoff. Doubles on each consecutive crash, capped.
pub const DEFAULT_CRASH_BACKOFF: Duration = Duration::from_secs(5);

/// Default maximum indexable facts returned from one query.
pub const DEFAULT_MAX_LSP_FACTS: usize = 256;

/// Default UTF-8 byte cap across one raw LSP payload.
pub const DEFAULT_MAX_LSP_RESPONSE_BYTES: usize = 64 * 1024;

/// Default UTF-8 byte cap for a stored name.
pub const DEFAULT_MAX_LSP_NAME_BYTES: usize = 256;

/// Default UTF-8 byte cap for hover/detail text.
pub const DEFAULT_MAX_LSP_DETAIL_BYTES: usize = 1_024;

/// Default UTF-8 byte cap for a raw location URI.
pub const DEFAULT_MAX_LSP_URI_BYTES: usize = 4_096;

const MAX_CRASH_SHIFT: u32 = 4;
const SCAN_MULTIPLIER: usize = 4;
const FILE_SCHEME: &str = "file";

/// Per-call resource bounds. Zero timeout degrades immediately.
#[derive(Clone, Debug)]
pub struct LspLimits {
    timeout: Duration,
    crash_backoff: Duration,
    max_facts: usize,
    max_response_bytes: usize,
    max_name_bytes: usize,
    max_detail_bytes: usize,
    max_uri_bytes: usize,
    cancel: CancellationToken,
}

/// Whether a server binding came from host config or the project tree.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum LspConfigOrigin {
    Host,
    Project,
}

/// Explicit project-trust gate for project-controlled executable config.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ProjectTrust {
    Untrusted,
    Trusted,
}

/// One configured language-server binding. The adapter never execs `command`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LspServerConfig {
    language: SourceLanguage,
    origin: LspConfigOrigin,
}

/// Which LSP methods to issue for one [`SymbolQuery`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct LspQueryKinds {
    definitions: bool,
    references: bool,
    types: bool,
    diagnostics: bool,
    details: bool,
}

/// Canonical snapshot used to version-check an LSP location before indexing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DocumentSnapshot {
    path: RepoPath,
    content_hash: ContentHash,
    document_version: u64,
    text: Option<String>,
}

/// Query for definitions, references, and/or symbol details.
#[derive(Clone, Debug)]
pub struct SymbolQuery {
    repo_id: RepoId,
    path: RepoPath,
    language: SourceLanguage,
    name: String,
    line: u32,
    character: u32,
    kinds: LspQueryKinds,
    snapshots: Vec<DocumentSnapshot>,
    cancel: CancellationToken,
}

/// Request handed to an [`LspClient`]. Timeouts and caps are already applied.
#[derive(Clone, Debug)]
pub struct LspRequest {
    language: SourceLanguage,
    path: RepoPath,
    name: String,
    line: u32,
    character: u32,
    document_version: u64,
    content_hash: ContentHash,
    kinds: LspQueryKinds,
    max_facts: usize,
    max_bytes: usize,
    timeout: Duration,
}

/// One untrusted location or hover returned by a language server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LspRawLocation {
    uri: String,
    name: String,
    detail: Option<String>,
    kind: LspFactKind,
    start_line: u32,
    start_character: u32,
    end_line: u32,
    end_character: u32,
    start_byte: Option<u32>,
    end_byte: Option<u32>,
    document_version: Option<u64>,
    content_hash: Option<ContentHash>,
}

/// Bounded batch of untrusted locations.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LspRawBatch {
    locations: Vec<LspRawLocation>,
}

/// Injected transport. Context Engine does not spawn language servers.
pub trait LspClient {
    fn query(
        &self,
        request: &LspRequest,
        cancel: &CancellationToken,
    ) -> Result<LspRawBatch, LspTransportError>;
}

/// Transport-level failure. Display never echoes URIs or source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LspTransportError {
    Unavailable,
    Timeout,
    Cancelled,
    Crash,
    ResponseTooLarge,
}

/// Half-open byte/line span after content/version validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct LspRange {
    start_byte: u32,
    end_byte: u32,
    start_line: u32,
    end_line: u32,
}

/// Classification of an indexable LSP fact.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum LspFactKind {
    Definition,
    Reference,
    Type,
    Diagnostic,
    Detail,
}

/// Provenance tag. Enrichment facts are always `lsp`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum LspFactSource {
    Lsp,
}

/// One content/version-checked fact safe to index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LspFact {
    kind: LspFactKind,
    source: LspFactSource,
    repo_id: RepoId,
    path: RepoPath,
    name: String,
    detail: Option<String>,
    range: LspRange,
    content_hash: ContentHash,
    document_version: u64,
}

/// Observable enricher health. Degraded is not a compile/search outage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LspHealth {
    Disabled,
    Healthy,
    Degraded(LspDegradeReason),
}

/// Why enrichment is not producing facts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LspDegradeReason {
    Unavailable,
    Timeout,
    Crash,
    UntrustedConfig,
    NoMatchingServer,
}

/// Bounded health snapshot a caller may emit as a health event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LspHealthEvent {
    health: LspHealth,
    language: Option<SourceLanguage>,
}

/// Result of [`LspEnricher::enrich`]. Facts are already version-checked.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LspEnrichment {
    facts: Vec<LspFact>,
    health: LspHealth,
    skipped_stale: u32,
    skipped_untrusted_path: u32,
    skipped_bound: u32,
}

/// Typed enricher failure. Display never echoes names, URIs, or host paths.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LspError {
    Cancelled,
    InvalidQuery,
}

/// Optional LSP adapter. Missing/untrusted/crashed servers degrade in place.
pub struct LspEnricher<C> {
    client: Option<C>,
    servers: Vec<LspServerConfig>,
    trust: ProjectTrust,
    repo_root: Option<PathBuf>,
    limits: LspLimits,
    health: LspHealth,
    crash_count: u32,
    backoff_until: Option<Instant>,
}

#[derive(Clone, Copy)]
enum PathReject {
    Escape,
}

impl LspLimits {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn timeout(mut self, value: Duration) -> Self {
        self.timeout = value;
        self
    }

    pub fn crash_backoff(mut self, value: Duration) -> Self {
        self.crash_backoff = value;
        self
    }

    pub fn max_facts(mut self, value: usize) -> Self {
        self.max_facts = value;
        self
    }

    pub fn max_response_bytes(mut self, value: usize) -> Self {
        self.max_response_bytes = value;
        self
    }

    pub fn max_name_bytes(mut self, value: usize) -> Self {
        self.max_name_bytes = value;
        self
    }

    pub fn max_detail_bytes(mut self, value: usize) -> Self {
        self.max_detail_bytes = value;
        self
    }

    pub fn max_uri_bytes(mut self, value: usize) -> Self {
        self.max_uri_bytes = value;
        self
    }

    pub fn cancellation(mut self, value: CancellationToken) -> Self {
        self.cancel = value;
        self
    }

    pub fn timeout_value(&self) -> Duration {
        self.timeout
    }

    pub fn crash_backoff_value(&self) -> Duration {
        self.crash_backoff
    }

    pub fn max_facts_value(&self) -> usize {
        self.max_facts
    }

    pub fn max_response_bytes_value(&self) -> usize {
        self.max_response_bytes
    }

    pub fn max_name_bytes_value(&self) -> usize {
        self.max_name_bytes
    }

    pub fn max_detail_bytes_value(&self) -> usize {
        self.max_detail_bytes
    }

    pub fn max_uri_bytes_value(&self) -> usize {
        self.max_uri_bytes
    }

    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.cancel
    }
}

impl Default for LspLimits {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_LSP_TIMEOUT,
            crash_backoff: DEFAULT_CRASH_BACKOFF,
            max_facts: DEFAULT_MAX_LSP_FACTS,
            max_response_bytes: DEFAULT_MAX_LSP_RESPONSE_BYTES,
            max_name_bytes: DEFAULT_MAX_LSP_NAME_BYTES,
            max_detail_bytes: DEFAULT_MAX_LSP_DETAIL_BYTES,
            max_uri_bytes: DEFAULT_MAX_LSP_URI_BYTES,
            cancel: CancellationToken::new(),
        }
    }
}

impl LspServerConfig {
    pub fn new(language: SourceLanguage, origin: LspConfigOrigin) -> Self {
        Self { language, origin }
    }

    pub fn language(&self) -> SourceLanguage {
        self.language
    }

    pub fn origin(&self) -> LspConfigOrigin {
        self.origin
    }
}

impl LspQueryKinds {
    pub fn all() -> Self {
        Self {
            definitions: true,
            references: true,
            types: true,
            diagnostics: true,
            details: true,
        }
    }

    pub fn none() -> Self {
        Self {
            definitions: false,
            references: false,
            types: false,
            diagnostics: false,
            details: false,
        }
    }

    pub fn definitions(mut self, value: bool) -> Self {
        self.definitions = value;
        self
    }

    pub fn references(mut self, value: bool) -> Self {
        self.references = value;
        self
    }

    pub fn types(mut self, value: bool) -> Self {
        self.types = value;
        self
    }

    pub fn diagnostics(mut self, value: bool) -> Self {
        self.diagnostics = value;
        self
    }

    pub fn details(mut self, value: bool) -> Self {
        self.details = value;
        self
    }

    pub fn wants_definitions(self) -> bool {
        self.definitions
    }

    pub fn wants_references(self) -> bool {
        self.references
    }

    pub fn wants_types(self) -> bool {
        self.types
    }

    pub fn wants_diagnostics(self) -> bool {
        self.diagnostics
    }

    pub fn wants_details(self) -> bool {
        self.details
    }

    fn effective(self) -> Self {
        if self.definitions || self.references || self.types || self.diagnostics || self.details {
            self
        } else {
            Self::all()
        }
    }
}

impl Default for LspQueryKinds {
    fn default() -> Self {
        Self::all()
    }
}

impl DocumentSnapshot {
    pub fn new(path: RepoPath, content_hash: ContentHash, document_version: u64) -> Self {
        Self {
            path,
            content_hash,
            document_version,
            text: None,
        }
    }

    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.text = Some(text.into());
        self
    }

    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    pub fn content_hash(&self) -> ContentHash {
        self.content_hash
    }

    pub fn document_version(&self) -> u64 {
        self.document_version
    }

    pub fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }
}

impl SymbolQuery {
    pub fn new(
        repo_id: RepoId,
        path: RepoPath,
        language: SourceLanguage,
        name: impl Into<String>,
        snapshot: DocumentSnapshot,
    ) -> Self {
        Self {
            repo_id,
            path,
            language,
            name: name.into(),
            line: 0,
            character: 0,
            kinds: LspQueryKinds::all(),
            snapshots: vec![snapshot],
            cancel: CancellationToken::new(),
        }
    }

    pub fn line(mut self, value: u32) -> Self {
        self.line = value;
        self
    }

    pub fn character(mut self, value: u32) -> Self {
        self.character = value;
        self
    }

    pub fn kinds(mut self, value: LspQueryKinds) -> Self {
        self.kinds = value;
        self
    }

    pub fn snapshot(mut self, value: DocumentSnapshot) -> Self {
        self.snapshots.push(value);
        self
    }

    pub fn cancellation(mut self, value: CancellationToken) -> Self {
        self.cancel = value;
        self
    }

    pub fn repo_id(&self) -> RepoId {
        self.repo_id
    }

    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    pub fn language(&self) -> SourceLanguage {
        self.language
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn snapshots(&self) -> &[DocumentSnapshot] {
        &self.snapshots
    }
}

impl LspRequest {
    pub fn language(&self) -> SourceLanguage {
        self.language
    }

    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn line(&self) -> u32 {
        self.line
    }

    pub fn character(&self) -> u32 {
        self.character
    }

    pub fn document_version(&self) -> u64 {
        self.document_version
    }

    pub fn content_hash(&self) -> ContentHash {
        self.content_hash
    }

    pub fn kinds(&self) -> LspQueryKinds {
        self.kinds
    }

    pub fn max_facts(&self) -> usize {
        self.max_facts
    }

    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }
}

impl LspRawLocation {
    pub fn new(uri: impl Into<String>, kind: LspFactKind) -> Self {
        Self {
            uri: uri.into(),
            name: String::new(),
            detail: None,
            kind,
            start_line: 0,
            start_character: 0,
            end_line: 0,
            end_character: 0,
            start_byte: None,
            end_byte: None,
            document_version: None,
            content_hash: None,
        }
    }

    pub fn name(mut self, value: impl Into<String>) -> Self {
        self.name = value.into();
        self
    }

    pub fn detail(mut self, value: impl Into<String>) -> Self {
        self.detail = Some(value.into());
        self
    }

    pub fn lines(
        mut self,
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
    ) -> Self {
        self.start_line = start_line;
        self.start_character = start_character;
        self.end_line = end_line;
        self.end_character = end_character;
        self
    }

    pub fn bytes(mut self, start_byte: u32, end_byte: u32) -> Self {
        self.start_byte = Some(start_byte);
        self.end_byte = Some(end_byte);
        self
    }

    pub fn document_version(mut self, value: u64) -> Self {
        self.document_version = Some(value);
        self
    }

    pub fn content_hash(mut self, value: ContentHash) -> Self {
        self.content_hash = Some(value);
        self
    }

    pub fn uri(&self) -> &str {
        &self.uri
    }

    fn payload_bytes(&self) -> usize {
        self.uri
            .len()
            .saturating_add(self.name.len())
            .saturating_add(self.detail.as_ref().map(String::len).unwrap_or(0))
    }
}

impl LspRawBatch {
    pub fn new(locations: Vec<LspRawLocation>) -> Self {
        Self { locations }
    }

    pub fn locations(&self) -> &[LspRawLocation] {
        &self.locations
    }

    fn payload_bytes(&self) -> usize {
        self.locations
            .iter()
            .fold(0usize, |acc, loc| acc.saturating_add(loc.payload_bytes()))
    }
}

impl LspRange {
    pub fn start_byte(self) -> u32 {
        self.start_byte
    }

    pub fn end_byte(self) -> u32 {
        self.end_byte
    }

    pub fn start_line(self) -> u32 {
        self.start_line
    }

    pub fn end_line(self) -> u32 {
        self.end_line
    }
}

impl LspFactKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Definition => "definition",
            Self::Reference => "reference",
            Self::Type => "type",
            Self::Diagnostic => "diagnostic",
            Self::Detail => "detail",
        }
    }
}

impl fmt::Display for LspFactKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl LspFactSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lsp => "lsp",
        }
    }
}

impl fmt::Display for LspFactSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl LspFact {
    pub fn kind(&self) -> LspFactKind {
        self.kind
    }

    pub fn source(&self) -> LspFactSource {
        self.source
    }

    pub fn repo_id(&self) -> RepoId {
        self.repo_id
    }

    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    pub fn range(&self) -> LspRange {
        self.range
    }

    pub fn content_hash(&self) -> ContentHash {
        self.content_hash
    }

    pub fn document_version(&self) -> u64 {
        self.document_version
    }
}

impl LspHealth {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Healthy => "healthy",
            Self::Degraded(LspDegradeReason::Unavailable) => "degraded_unavailable",
            Self::Degraded(LspDegradeReason::Timeout) => "degraded_timeout",
            Self::Degraded(LspDegradeReason::Crash) => "degraded_crash",
            Self::Degraded(LspDegradeReason::UntrustedConfig) => "degraded_untrusted_config",
            Self::Degraded(LspDegradeReason::NoMatchingServer) => "degraded_no_matching_server",
        }
    }

    pub const fn is_degraded(self) -> bool {
        matches!(self, Self::Degraded(_))
    }
}

impl LspDegradeReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::Timeout => "timeout",
            Self::Crash => "crash",
            Self::UntrustedConfig => "untrusted_config",
            Self::NoMatchingServer => "no_matching_server",
        }
    }
}

impl LspHealthEvent {
    pub fn health(self) -> LspHealth {
        self.health
    }

    pub fn language(self) -> Option<SourceLanguage> {
        self.language
    }
}

impl LspEnrichment {
    fn empty(health: LspHealth) -> Self {
        Self {
            facts: Vec::new(),
            health,
            skipped_stale: 0,
            skipped_untrusted_path: 0,
            skipped_bound: 0,
        }
    }

    /// Content/version-checked facts. Safe to index; tagged `source=lsp`.
    pub fn facts(&self) -> &[LspFact] {
        &self.facts
    }

    pub fn health(&self) -> LspHealth {
        self.health
    }

    pub fn is_degraded(&self) -> bool {
        self.health.is_degraded()
    }

    pub fn skipped_stale(&self) -> u32 {
        self.skipped_stale
    }

    pub fn skipped_untrusted_path(&self) -> u32 {
        self.skipped_untrusted_path
    }

    pub fn skipped_bound(&self) -> u32 {
        self.skipped_bound
    }

    pub fn health_event(&self, language: Option<SourceLanguage>) -> LspHealthEvent {
        LspHealthEvent {
            health: self.health,
            language,
        }
    }
}

impl LspError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::InvalidQuery => "invalid_query",
        }
    }
}

impl fmt::Display for LspError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for LspError {}

impl LspTransportError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::Crash => "crash",
            Self::ResponseTooLarge => "response_too_large",
        }
    }
}

impl fmt::Display for LspTransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for LspTransportError {}

impl<C> LspEnricher<C> {
    pub fn disabled(limits: LspLimits) -> Self {
        Self {
            client: None,
            servers: Vec::new(),
            trust: ProjectTrust::Untrusted,
            repo_root: None,
            limits,
            health: LspHealth::Disabled,
            crash_count: 0,
            backoff_until: None,
        }
    }

    pub fn health(&self) -> LspHealth {
        self.health
    }

    pub fn is_disabled(&self) -> bool {
        matches!(self.health, LspHealth::Disabled) || self.client.is_none()
    }
}

impl<C: LspClient> LspEnricher<C> {
    pub fn new(
        client: C,
        servers: Vec<LspServerConfig>,
        trust: ProjectTrust,
        repo_root: Option<PathBuf>,
        limits: LspLimits,
    ) -> Self {
        let health = if servers.is_empty() {
            LspHealth::Disabled
        } else if servers.iter().all(|s| s.origin == LspConfigOrigin::Project)
            && trust != ProjectTrust::Trusted
        {
            LspHealth::Degraded(LspDegradeReason::UntrustedConfig)
        } else {
            LspHealth::Healthy
        };
        Self {
            client: Some(client),
            servers,
            trust,
            repo_root: repo_root.map(|root| lexical_normalize(&root).unwrap_or(root)),
            limits,
            health,
            crash_count: 0,
            backoff_until: None,
        }
    }

    /// Query configured servers. Timeout/unavailable returns empty facts.
    pub fn enrich(&mut self, query: &SymbolQuery) -> Result<LspEnrichment, LspError> {
        if self.limits.cancel.is_cancelled() || query.cancel.is_cancelled() {
            return Err(LspError::Cancelled);
        }
        let prepared = prepare_query(query, &self.limits)?;
        if self.client.is_none() || self.servers.is_empty() {
            self.health = LspHealth::Disabled;
            return Ok(LspEnrichment::empty(self.health));
        }
        if let Some(reason) = self.gate(query.language) {
            self.health = LspHealth::Degraded(reason);
            return Ok(LspEnrichment::empty(self.health));
        }
        let started = Instant::now();
        if self.limits.timeout.is_zero() || started.elapsed() > self.limits.timeout {
            self.health = LspHealth::Degraded(LspDegradeReason::Timeout);
            return Ok(LspEnrichment::empty(self.health));
        }

        let Some(client) = self.client.as_ref() else {
            self.health = LspHealth::Disabled;
            return Ok(LspEnrichment::empty(self.health));
        };
        let batch = match client.query(&prepared, &query.cancel) {
            Ok(batch) => batch,
            Err(LspTransportError::Cancelled) => return Err(LspError::Cancelled),
            Err(LspTransportError::Timeout) => {
                self.health = LspHealth::Degraded(LspDegradeReason::Timeout);
                return Ok(LspEnrichment::empty(self.health));
            }
            Err(LspTransportError::Crash) => {
                self.record_crash();
                return Ok(LspEnrichment::empty(self.health));
            }
            Err(LspTransportError::Unavailable | LspTransportError::ResponseTooLarge) => {
                self.health = LspHealth::Degraded(LspDegradeReason::Unavailable);
                return Ok(LspEnrichment::empty(self.health));
            }
        };
        if query.cancel.is_cancelled() || self.limits.cancel.is_cancelled() {
            return Err(LspError::Cancelled);
        }
        if started.elapsed() > self.limits.timeout {
            self.health = LspHealth::Degraded(LspDegradeReason::Timeout);
            return Ok(LspEnrichment::empty(self.health));
        }
        if batch.payload_bytes() > self.limits.max_response_bytes {
            self.health = LspHealth::Degraded(LspDegradeReason::Unavailable);
            return Ok(LspEnrichment::empty(self.health));
        }

        let kinds = query.kinds.effective();
        let mut out = LspEnrichment::empty(LspHealth::Healthy);
        let scan_cap = self.limits.max_facts.saturating_mul(SCAN_MULTIPLIER).max(1);
        for location in batch.locations.iter().take(scan_cap) {
            if query.cancel.is_cancelled() || self.limits.cancel.is_cancelled() {
                return Err(LspError::Cancelled);
            }
            if !kind_allowed(location.kind, kinds) {
                out.skipped_bound = out.skipped_bound.saturating_add(1);
                continue;
            }
            match self.accept_location(query, location) {
                Ok(fact) => {
                    if out.facts.len() >= self.limits.max_facts {
                        out.skipped_bound = out.skipped_bound.saturating_add(1);
                        continue;
                    }
                    out.facts.push(fact);
                }
                Err(AcceptReject::Stale) => {
                    out.skipped_stale = out.skipped_stale.saturating_add(1);
                }
                Err(AcceptReject::UntrustedPath) => {
                    out.skipped_untrusted_path = out.skipped_untrusted_path.saturating_add(1);
                }
                Err(AcceptReject::Bound) => {
                    out.skipped_bound = out.skipped_bound.saturating_add(1);
                }
            }
        }
        self.crash_count = 0;
        self.backoff_until = None;
        self.health = LspHealth::Healthy;
        out.health = LspHealth::Healthy;
        Ok(out)
    }

    fn gate(&self, language: SourceLanguage) -> Option<LspDegradeReason> {
        if self
            .backoff_until
            .is_some_and(|until| Instant::now() < until)
        {
            return Some(LspDegradeReason::Crash);
        }
        self.select_server(language).err()
    }

    fn select_server(
        &self,
        language: SourceLanguage,
    ) -> Result<&LspServerConfig, LspDegradeReason> {
        let mut saw_language = false;
        for server in &self.servers {
            if server.language != language {
                continue;
            }
            saw_language = true;
            match server.origin {
                LspConfigOrigin::Host => return Ok(server),
                LspConfigOrigin::Project => {
                    if self.trust == ProjectTrust::Trusted {
                        return Ok(server);
                    }
                }
            }
        }
        if saw_language {
            Err(LspDegradeReason::UntrustedConfig)
        } else {
            Err(LspDegradeReason::NoMatchingServer)
        }
    }

    fn record_crash(&mut self) {
        self.crash_count = self.crash_count.saturating_add(1);
        let shift = self.crash_count.saturating_sub(1).min(MAX_CRASH_SHIFT);
        let delay = self
            .limits
            .crash_backoff
            .saturating_mul(1u32.checked_shl(shift).unwrap_or(u32::MAX));
        self.backoff_until = Some(Instant::now() + delay);
        self.health = LspHealth::Degraded(LspDegradeReason::Crash);
    }

    fn accept_location(
        &self,
        query: &SymbolQuery,
        location: &LspRawLocation,
    ) -> Result<LspFact, AcceptReject> {
        if location.uri.len() > self.limits.max_uri_bytes {
            return Err(AcceptReject::Bound);
        }
        let path = resolve_location_path(
            &location.uri,
            self.repo_root.as_deref(),
            self.limits.max_uri_bytes,
        )
        .map_err(|_| AcceptReject::UntrustedPath)?;
        let snapshot = query
            .snapshots
            .iter()
            .find(|snap| snap.path == path)
            .ok_or(AcceptReject::Stale)?;
        if location
            .document_version
            .is_some_and(|version| version != snapshot.document_version)
        {
            return Err(AcceptReject::Stale);
        }
        if location
            .content_hash
            .is_some_and(|hash| hash != snapshot.content_hash)
        {
            return Err(AcceptReject::Stale);
        }
        let range = validated_range(location, snapshot.text.as_deref())?;
        let name = location.name.trim();
        let name = if name.is_empty() {
            query.name.trim()
        } else {
            name
        };
        if name.is_empty() {
            return Err(AcceptReject::Bound);
        }
        let name = truncate_chars(name, self.limits.max_name_bytes);
        if name.is_empty() {
            return Err(AcceptReject::Bound);
        }
        let detail = location.detail.as_ref().and_then(|raw| {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(truncate_chars(trimmed, self.limits.max_detail_bytes))
            }
        });
        Ok(LspFact {
            kind: location.kind,
            source: LspFactSource::Lsp,
            repo_id: query.repo_id,
            path,
            name,
            detail,
            range,
            content_hash: snapshot.content_hash,
            document_version: snapshot.document_version,
        })
    }
}

enum AcceptReject {
    Stale,
    UntrustedPath,
    Bound,
}

fn prepare_query(query: &SymbolQuery, limits: &LspLimits) -> Result<LspRequest, LspError> {
    let name = truncate_chars(query.name.trim(), limits.max_name_bytes);
    if name.is_empty() {
        return Err(LspError::InvalidQuery);
    }
    let snapshot = query
        .snapshots
        .iter()
        .find(|snap| snap.path == query.path)
        .ok_or(LspError::InvalidQuery)?;
    Ok(LspRequest {
        language: query.language,
        path: query.path.clone(),
        name,
        line: query.line,
        character: query.character,
        document_version: snapshot.document_version,
        content_hash: snapshot.content_hash,
        kinds: query.kinds.effective(),
        max_facts: limits.max_facts,
        max_bytes: limits.max_response_bytes,
        timeout: limits.timeout,
    })
}

fn kind_allowed(kind: LspFactKind, kinds: LspQueryKinds) -> bool {
    match kind {
        LspFactKind::Definition => kinds.definitions,
        LspFactKind::Reference => kinds.references,
        LspFactKind::Type => kinds.types,
        LspFactKind::Diagnostic => kinds.diagnostics,
        LspFactKind::Detail => kinds.details,
    }
}

fn validated_range(
    location: &LspRawLocation,
    text: Option<&str>,
) -> Result<LspRange, AcceptReject> {
    if let Some(text) = text {
        let (start_byte, end_byte) = match (location.start_byte, location.end_byte) {
            (Some(start), Some(end)) => (start, end),
            _ => line_span_to_bytes(
                text,
                location.start_line,
                location.start_character,
                location.end_line,
                location.end_character,
            )
            .ok_or(AcceptReject::Bound)?,
        };
        if end_byte < start_byte {
            return Err(AcceptReject::Bound);
        }
        let len = u32::try_from(text.len()).map_err(|_| AcceptReject::Bound)?;
        if end_byte > len {
            return Err(AcceptReject::Bound);
        }
        Ok(LspRange {
            start_byte,
            end_byte,
            start_line: location.start_line,
            end_line: location.end_line,
        })
    } else {
        let start_byte = location.start_byte.unwrap_or(0);
        let end_byte = location.end_byte.unwrap_or(start_byte);
        if end_byte < start_byte {
            return Err(AcceptReject::Bound);
        }
        Ok(LspRange {
            start_byte,
            end_byte,
            start_line: location.start_line,
            end_line: location.end_line,
        })
    }
}

fn line_span_to_bytes(
    text: &str,
    start_line: u32,
    start_character: u32,
    end_line: u32,
    end_character: u32,
) -> Option<(u32, u32)> {
    let start = line_col_to_byte(text, start_line, start_character)?;
    let end = line_col_to_byte(text, end_line, end_character)?;
    if end >= start {
        Some((start, end))
    } else {
        None
    }
}

fn line_col_to_byte(text: &str, line: u32, character: u32) -> Option<u32> {
    let mut remaining_lines = line;
    let mut offset = 0usize;
    for raw_line in text.split_inclusive('\n') {
        if remaining_lines == 0 {
            let col = usize::try_from(character).ok()?;
            if col > raw_line.len() {
                return None;
            }
            if !raw_line.is_char_boundary(col) {
                return None;
            }
            return u32::try_from(offset.saturating_add(col)).ok();
        }
        remaining_lines = remaining_lines.saturating_sub(1);
        offset = offset.saturating_add(raw_line.len());
    }
    if remaining_lines == 0 && character == 0 {
        return u32::try_from(offset).ok();
    }
    None
}

fn resolve_location_path(
    uri: &str,
    repo_root: Option<&Path>,
    max_uri_bytes: usize,
) -> Result<RepoPath, PathReject> {
    if uri.len() > max_uri_bytes || uri.is_empty() {
        return Err(PathReject::Escape);
    }
    if has_uri_scheme(uri) {
        let host_path = parse_file_uri(uri, max_uri_bytes).ok_or(PathReject::Escape)?;
        let root = repo_root.ok_or(PathReject::Escape)?;
        relative_to_root(root, &host_path)
    } else {
        RepoPath::parse(uri).map_err(|_| PathReject::Escape)
    }
}

fn has_uri_scheme(uri: &str) -> bool {
    let bytes = uri.as_bytes();
    if bytes.first().is_none_or(|b| !b.is_ascii_alphabetic()) {
        return false;
    }
    for (i, b) in bytes.iter().copied().enumerate().skip(1) {
        if b == b':' {
            return i >= 1;
        }
        if !(b.is_ascii_alphanumeric() || matches!(b, b'+' | b'.' | b'-')) {
            return false;
        }
    }
    false
}

fn parse_file_uri(uri: &str, max_bytes: usize) -> Option<PathBuf> {
    let rest = strip_scheme(uri, FILE_SCHEME)?;
    let path_part = if let Some(rest) = rest.strip_prefix("//") {
        let slash = rest.find('/')?;
        let auth = &rest[..slash];
        if !auth.is_empty() && !auth.eq_ignore_ascii_case("localhost") && auth != "127.0.0.1" {
            return None;
        }
        &rest[slash..]
    } else if rest.starts_with('/') {
        rest
    } else {
        return None;
    };
    let cut = path_part.find(['?', '#']).unwrap_or(path_part.len());
    let decoded = percent_decode(&path_part[..cut], max_bytes)?;
    if decoded.contains('\0') || decoded.chars().any(char::is_control) {
        return None;
    }
    Some(PathBuf::from(decoded))
}

fn strip_scheme<'a>(uri: &'a str, scheme: &str) -> Option<&'a str> {
    let (head, tail) = uri.split_at(scheme.len().min(uri.len()));
    if head.eq_ignore_ascii_case(scheme) {
        tail.strip_prefix(':')
    } else {
        None
    }
}

fn percent_decode(input: &str, max_bytes: usize) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len().min(max_bytes));
    let mut i = 0usize;
    while i < bytes.len() {
        if out.len() >= max_bytes {
            return None;
        }
        if bytes[i] == b'%' {
            let hi = *bytes.get(i + 1)?;
            let lo = *bytes.get(i + 2)?;
            out.push((hex_nibble(hi)? << 4) | hex_nibble(lo)?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn lexical_normalize(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                _ => return None,
            },
            Component::Normal(part) => out.push(part),
        }
    }
    Some(out)
}

fn relative_to_root(root: &Path, target: &Path) -> Result<RepoPath, PathReject> {
    let root = lexical_normalize(root).ok_or(PathReject::Escape)?;
    let target = lexical_normalize(target).ok_or(PathReject::Escape)?;
    let rel = target.strip_prefix(&root).map_err(|_| PathReject::Escape)?;
    if rel.as_os_str().is_empty() {
        return Err(PathReject::Escape);
    }
    let mut joined = String::new();
    for component in rel.components() {
        let Component::Normal(part) = component else {
            return Err(PathReject::Escape);
        };
        let part = part.to_str().ok_or(PathReject::Escape)?;
        if !joined.is_empty() {
            joined.push('/');
        }
        joined.push_str(part);
    }
    RepoPath::parse(&joined).map_err(|_| PathReject::Escape)
}

fn truncate_chars(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    input[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct ScriptedClient {
        responses: Mutex<Vec<Result<LspRawBatch, LspTransportError>>>,
        calls: AtomicU32,
    }

    impl ScriptedClient {
        fn new(responses: Vec<Result<LspRawBatch, LspTransportError>>) -> Self {
            Self {
                responses: Mutex::new(responses),
                calls: AtomicU32::new(0),
            }
        }

        fn calls(&self) -> u32 {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl LspClient for ScriptedClient {
        fn query(
            &self,
            _request: &LspRequest,
            cancel: &CancellationToken,
        ) -> Result<LspRawBatch, LspTransportError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if cancel.is_cancelled() {
                return Err(LspTransportError::Cancelled);
            }
            let mut queue = self.responses.lock().expect("script lock");
            if queue.is_empty() {
                return Err(LspTransportError::Unavailable);
            }
            queue.remove(0)
        }
    }

    fn path(raw: &str) -> RepoPath {
        RepoPath::parse(raw).expect("repo path")
    }

    fn hash(text: &str) -> ContentHash {
        ContentHash::from_bytes(text.as_bytes())
    }

    fn snap(rel: &str, text: &str, version: u64) -> DocumentSnapshot {
        DocumentSnapshot::new(path(rel), hash(text), version).with_text(text)
    }

    fn query(rel: &str, name: &str, text: &str, version: u64) -> SymbolQuery {
        SymbolQuery::new(
            RepoId::new(),
            path(rel),
            SourceLanguage::Rust,
            name,
            snap(rel, text, version),
        )
    }

    fn host_rust() -> Vec<LspServerConfig> {
        vec![LspServerConfig::new(
            SourceLanguage::Rust,
            LspConfigOrigin::Host,
        )]
    }

    fn enricher(
        responses: Vec<Result<LspRawBatch, LspTransportError>>,
    ) -> LspEnricher<ScriptedClient> {
        LspEnricher::new(
            ScriptedClient::new(responses),
            host_rust(),
            ProjectTrust::Trusted,
            Some(PathBuf::from("/repo")),
            LspLimits::new(),
        )
    }

    fn loc(uri: &str, kind: LspFactKind, name: &str) -> LspRawLocation {
        LspRawLocation::new(uri, kind)
            .name(name)
            .bytes(0, 2)
            .document_version(1)
            .content_hash(hash("fn x() {}"))
    }

    #[test]
    fn enrich_returns_facts_tagged_source_lsp() {
        let src = "fn x() {}";
        let batch = LspRawBatch::new(vec![
            loc("src/lib.rs", LspFactKind::Definition, "x"),
            loc("src/lib.rs", LspFactKind::Reference, "x"),
            loc("src/lib.rs", LspFactKind::Detail, "x").detail("fn x()"),
        ]);
        let mut enricher = enricher(vec![Ok(batch)]);
        let out = enricher
            .enrich(&query("src/lib.rs", "x", src, 1))
            .expect("enrich");
        assert_eq!(out.facts().len(), 3);
        assert!(out.facts().iter().all(|f| f.source() == LspFactSource::Lsp));
        assert!(out.facts().iter().all(|f| f.source().as_str() == "lsp"));
        assert_eq!(out.facts()[0].kind(), LspFactKind::Definition);
        assert_eq!(out.facts()[2].detail(), Some("fn x()"));
        assert_eq!(out.health(), LspHealth::Healthy);
        assert!(!out.is_degraded());
    }

    #[test]
    fn enrich_accepts_type_and_diagnostic_facts() {
        let src = "fn x() {}";
        let batch = LspRawBatch::new(vec![
            loc("src/lib.rs", LspFactKind::Type, "x").detail("fn()"),
            loc("src/lib.rs", LspFactKind::Diagnostic, "x").detail("unused"),
        ]);
        let mut type_enricher = enricher(vec![Ok(batch)]);
        let out = type_enricher
            .enrich(&query("src/lib.rs", "x", src, 1))
            .expect("enrich");
        assert_eq!(out.facts().len(), 2);
        assert_eq!(out.facts()[0].kind(), LspFactKind::Type);
        assert_eq!(out.facts()[0].kind().as_str(), "type");
        assert_eq!(out.facts()[1].kind(), LspFactKind::Diagnostic);
        assert_eq!(out.facts()[1].kind().as_str(), "diagnostic");
        assert!(out.facts().iter().all(|f| f.source() == LspFactSource::Lsp));

        let mut defs_only_enricher = enricher(vec![Ok(LspRawBatch::new(vec![
            loc("src/lib.rs", LspFactKind::Type, "x"),
            loc("src/lib.rs", LspFactKind::Diagnostic, "x"),
        ]))]);
        let defs_only =
            query("src/lib.rs", "x", src, 1).kinds(LspQueryKinds::none().definitions(true));
        let filtered_out = defs_only_enricher.enrich(&defs_only).expect("filtered");
        assert!(filtered_out.facts().is_empty());
    }

    #[test]
    fn unavailable_does_not_fail_enrich() {
        let mut enricher = enricher(vec![Err(LspTransportError::Unavailable)]);
        let out = enricher
            .enrich(&query("src/lib.rs", "x", "fn x() {}", 1))
            .expect("degrade");
        assert!(out.facts().is_empty());
        assert_eq!(
            out.health(),
            LspHealth::Degraded(LspDegradeReason::Unavailable)
        );
        assert_eq!(
            out.health_event(Some(SourceLanguage::Rust)).health(),
            out.health()
        );
    }

    #[test]
    fn timeout_does_not_fail_enrich() {
        let mut enricher = enricher(vec![Err(LspTransportError::Timeout)]);
        let out = enricher
            .enrich(&query("src/lib.rs", "x", "fn x() {}", 1))
            .expect("degrade");
        assert!(out.facts().is_empty());
        assert_eq!(out.health(), LspHealth::Degraded(LspDegradeReason::Timeout));
    }

    #[test]
    fn zero_timeout_degrades_without_calling_client() {
        let client = ScriptedClient::new(vec![Ok(LspRawBatch::new(vec![loc(
            "src/lib.rs",
            LspFactKind::Definition,
            "x",
        )]))]);
        let mut enricher = LspEnricher::new(
            client,
            host_rust(),
            ProjectTrust::Trusted,
            None,
            LspLimits::new().timeout(Duration::ZERO),
        );
        let out = enricher
            .enrich(&query("src/lib.rs", "x", "fn x() {}", 1))
            .expect("degrade");
        assert!(out.facts().is_empty());
        assert_eq!(out.health(), LspHealth::Degraded(LspDegradeReason::Timeout));
        assert_eq!(enricher.client.as_ref().expect("client").calls(), 0);
    }

    #[test]
    fn cancellation_is_typed() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut enricher = enricher(vec![Ok(LspRawBatch::default())]);
        let q = query("src/lib.rs", "x", "fn x() {}", 1).cancellation(cancel);
        assert!(matches!(enricher.enrich(&q), Err(LspError::Cancelled)));
    }

    #[test]
    fn stale_version_is_not_indexed() {
        let src = "fn x() {}";
        let stale = loc("src/lib.rs", LspFactKind::Definition, "x").document_version(9);
        let mut enricher = enricher(vec![Ok(LspRawBatch::new(vec![stale]))]);
        let out = enricher
            .enrich(&query("src/lib.rs", "x", src, 1))
            .expect("enrich");
        assert!(out.facts().is_empty());
        assert_eq!(out.skipped_stale(), 1);
        assert_eq!(out.health(), LspHealth::Healthy);
    }

    #[test]
    fn content_hash_mismatch_is_not_indexed() {
        let src = "fn x() {}";
        let stale = loc("src/lib.rs", LspFactKind::Definition, "x").content_hash(hash("other"));
        let mut enricher = enricher(vec![Ok(LspRawBatch::new(vec![stale]))]);
        let out = enricher
            .enrich(&query("src/lib.rs", "x", src, 1))
            .expect("enrich");
        assert!(out.facts().is_empty());
        assert_eq!(out.skipped_stale(), 1);
    }

    #[test]
    fn location_without_snapshot_is_not_indexed() {
        let src = "fn x() {}";
        let other = loc("src/other.rs", LspFactKind::Definition, "x");
        let mut enricher = enricher(vec![Ok(LspRawBatch::new(vec![other]))]);
        let out = enricher
            .enrich(&query("src/lib.rs", "x", src, 1))
            .expect("enrich");
        assert!(out.facts().is_empty());
        assert_eq!(out.skipped_stale(), 1);
    }

    #[test]
    fn file_uri_inside_root_is_accepted() {
        let src = "fn x() {}";
        let batch = LspRawBatch::new(vec![loc(
            "file:///repo/src/lib.rs",
            LspFactKind::Definition,
            "x",
        )]);
        let mut enricher = enricher(vec![Ok(batch)]);
        let out = enricher
            .enrich(&query("src/lib.rs", "x", src, 1))
            .expect("enrich");
        assert_eq!(out.facts().len(), 1);
        assert_eq!(out.facts()[0].path().as_str(), "src/lib.rs");
    }

    #[test]
    fn path_traversal_from_lsp_is_rejected() {
        let src = "fn x() {}";
        let attacks = [
            "file:///repo/src/../../etc/passwd",
            "file:///repo/src/%2e%2e/%2e%2e/etc/passwd",
            "file:///etc/passwd",
            "file://evil.example/repo/src/lib.rs",
            "https://example.invalid/src/lib.rs",
            "../src/lib.rs",
            "src/../../secrets.env",
        ];
        let batch = LspRawBatch::new(
            attacks
                .iter()
                .map(|uri| loc(uri, LspFactKind::Definition, "x"))
                .collect(),
        );
        let mut enricher = enricher(vec![Ok(batch)]);
        let out = enricher
            .enrich(&query("src/lib.rs", "x", src, 1))
            .expect("enrich");
        assert!(out.facts().is_empty());
        assert_eq!(out.skipped_untrusted_path(), attacks.len() as u32);
    }

    #[test]
    fn range_outside_source_is_rejected() {
        let src = "fn x() {}";
        let bad = loc("src/lib.rs", LspFactKind::Definition, "x").bytes(0, 10_000);
        let mut enricher = enricher(vec![Ok(LspRawBatch::new(vec![bad]))]);
        let out = enricher
            .enrich(&query("src/lib.rs", "x", src, 1))
            .expect("enrich");
        assert!(out.facts().is_empty());
        assert_eq!(out.skipped_bound(), 1);
    }

    #[test]
    fn project_untrusted_config_does_not_call_client() {
        let client = ScriptedClient::new(vec![Ok(LspRawBatch::new(vec![loc(
            "src/lib.rs",
            LspFactKind::Definition,
            "x",
        )]))]);
        let mut enricher = LspEnricher::new(
            client,
            vec![LspServerConfig::new(
                SourceLanguage::Rust,
                LspConfigOrigin::Project,
            )],
            ProjectTrust::Untrusted,
            None,
            LspLimits::new(),
        );
        let out = enricher
            .enrich(&query("src/lib.rs", "x", "fn x() {}", 1))
            .expect("degrade");
        assert!(out.facts().is_empty());
        assert_eq!(
            out.health(),
            LspHealth::Degraded(LspDegradeReason::UntrustedConfig)
        );
        assert_eq!(enricher.client.as_ref().expect("client").calls(), 0);
    }

    #[test]
    fn project_trusted_config_is_used() {
        let src = "fn x() {}";
        let mut enricher = LspEnricher::new(
            ScriptedClient::new(vec![Ok(LspRawBatch::new(vec![loc(
                "src/lib.rs",
                LspFactKind::Definition,
                "x",
            )]))]),
            vec![LspServerConfig::new(
                SourceLanguage::Rust,
                LspConfigOrigin::Project,
            )],
            ProjectTrust::Trusted,
            None,
            LspLimits::new(),
        );
        let out = enricher
            .enrich(&query("src/lib.rs", "x", src, 1))
            .expect("enrich");
        assert_eq!(out.facts().len(), 1);
    }

    #[test]
    fn crash_backoffs_and_skips_client() {
        let client = ScriptedClient::new(vec![
            Err(LspTransportError::Crash),
            Ok(LspRawBatch::new(vec![loc(
                "src/lib.rs",
                LspFactKind::Definition,
                "x",
            )])),
        ]);
        let mut enricher = LspEnricher::new(
            client,
            host_rust(),
            ProjectTrust::Trusted,
            None,
            LspLimits::new().crash_backoff(Duration::from_secs(60)),
        );
        let q = query("src/lib.rs", "x", "fn x() {}", 1);
        let first = enricher.enrich(&q).expect("crash degrade");
        assert!(first.facts().is_empty());
        assert_eq!(first.health(), LspHealth::Degraded(LspDegradeReason::Crash));
        let second = enricher.enrich(&q).expect("backoff");
        assert!(second.facts().is_empty());
        assert_eq!(
            second.health(),
            LspHealth::Degraded(LspDegradeReason::Crash)
        );
        assert_eq!(enricher.client.as_ref().expect("client").calls(), 1);
    }

    #[test]
    fn disabled_enricher_returns_empty() {
        let mut enricher = LspEnricher::<ScriptedClient>::disabled(LspLimits::new());
        let out = enricher
            .enrich(&query("src/lib.rs", "x", "fn x() {}", 1))
            .expect("disabled");
        assert!(out.facts().is_empty());
        assert_eq!(out.health(), LspHealth::Disabled);
        assert!(enricher.is_disabled());
    }

    #[test]
    fn missing_language_server_degrades() {
        let mut enricher = LspEnricher::new(
            ScriptedClient::new(vec![Ok(LspRawBatch::default())]),
            vec![LspServerConfig::new(
                SourceLanguage::Python,
                LspConfigOrigin::Host,
            )],
            ProjectTrust::Trusted,
            None,
            LspLimits::new(),
        );
        let out = enricher
            .enrich(&query("src/lib.rs", "x", "fn x() {}", 1))
            .expect("degrade");
        assert!(out.facts().is_empty());
        assert_eq!(
            out.health(),
            LspHealth::Degraded(LspDegradeReason::NoMatchingServer)
        );
        assert_eq!(enricher.client.as_ref().expect("client").calls(), 0);
    }

    #[test]
    fn fact_cap_is_enforced() {
        let src = "fn x() {}";
        let locs = (0..8)
            .map(|_| loc("src/lib.rs", LspFactKind::Reference, "x"))
            .collect();
        let mut enricher = LspEnricher::new(
            ScriptedClient::new(vec![Ok(LspRawBatch::new(locs))]),
            host_rust(),
            ProjectTrust::Trusted,
            None,
            LspLimits::new().max_facts(2),
        );
        let out = enricher
            .enrich(&query("src/lib.rs", "x", src, 1))
            .expect("enrich");
        assert_eq!(out.facts().len(), 2);
        assert_eq!(out.skipped_bound(), 6);
    }

    #[test]
    fn oversized_payload_is_not_indexed() {
        let src = "fn x() {}";
        let big = loc("src/lib.rs", LspFactKind::Detail, "x").detail("n".repeat(64));
        let mut enricher = LspEnricher::new(
            ScriptedClient::new(vec![Ok(LspRawBatch::new(vec![big]))]),
            host_rust(),
            ProjectTrust::Trusted,
            None,
            LspLimits::new().max_response_bytes(8),
        );
        let out = enricher
            .enrich(&query("src/lib.rs", "x", src, 1))
            .expect("degrade");
        assert!(out.facts().is_empty());
        assert_eq!(
            out.health(),
            LspHealth::Degraded(LspDegradeReason::Unavailable)
        );
    }

    #[test]
    fn empty_name_is_invalid_query() {
        let mut enricher = enricher(vec![Ok(LspRawBatch::default())]);
        let q = query("src/lib.rs", "   ", "fn x() {}", 1);
        assert!(matches!(enricher.enrich(&q), Err(LspError::InvalidQuery)));
        assert_eq!(enricher.client.as_ref().expect("client").calls(), 0);
    }

    #[test]
    fn error_display_is_safe() {
        assert_eq!(LspError::Cancelled.to_string(), "cancelled");
        assert_eq!(LspError::InvalidQuery.to_string(), "invalid_query");
        assert!(!LspError::InvalidQuery.to_string().contains('/'));
        assert!(!LspTransportError::Crash.to_string().contains("file:"));
        assert!(
            !format!(
                "{:?}",
                loc("src/secret.rs", LspFactKind::Detail, "TOKEN").detail("sk-test")
            )
            .contains("unused")
        );
        let err = LspError::InvalidQuery;
        assert!(!err.to_string().contains("src/"));
        assert!(!err.to_string().contains("secret"));
    }
}
