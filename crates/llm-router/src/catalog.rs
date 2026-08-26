//! Validated immutable provider/model catalog snapshot.
//!
//! [`ModelCatalog::build`] loads configured entries, rejects duplicate IDs and
//! impossible metadata, and keeps disabled/unavailable models with an explicit
//! status. The snapshot carries a revision and a content hash.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::str::FromStr;

use protocol::{ApiError, ArtifactId, ErrorCode, TraceId};
use serde::de::{self, Deserializer};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};

use crate::provider::{
    CancellationToken, CatalogRevision, DataPolicyTag, LatencyClass, MAX_DATA_POLICY_TAGS,
    MAX_REGIONS, ModelDescriptor, ModelId, ModelPrices, ModelRef, ProviderCapabilities,
    ProviderError, ProviderId, ReasoningSupport, Region,
};

/// Wire schema name for [`CatalogSnapshot`].
pub const CATALOG_SNAPSHOT_SCHEMA: &str = "rapidlm.catalog_snapshot";

/// v1 schema version for catalog snapshot objects.
pub const CATALOG_SCHEMA_VERSION: u16 = 1;

/// Maximum configured models in one catalog snapshot.
pub const MAX_CATALOG_ENTRIES: usize = 256;

/// Maximum providers in one [`ProviderCapIndex`].
pub const MAX_CATALOG_PROVIDERS: usize = 64;

const CANCEL_CHECK_EVERY: usize = 16;

const SNAPSHOT_FIELDS: &[&str] = &["schema", "schema_version", "revision", "hash", "entries"];

/// Content hash of a catalog snapshot (`sha256:<hex>`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct CatalogHash(ArtifactId);

/// Configured provider/model rows loaded into a snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogConfig {
    revision: CatalogRevision,
    entries: Vec<CatalogModelSpec>,
}

/// One configured model. Status is decided at build time.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogModelSpec {
    provider: ProviderId,
    model: ModelId,
    enabled: bool,
    capabilities: ProviderCapabilities,
    prices: ModelPrices,
    regions: Vec<Region>,
    data_policy_tags: Vec<DataPolicyTag>,
    latency_class: LatencyClass,
}

/// Advertised provider capabilities used to diagnose availability.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderCapIndex {
    entries: BTreeMap<ProviderId, ProviderCapEntry>,
}

/// Runtime advertisement for one provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderCapEntry {
    Available(ProviderCapabilities),
    Unavailable,
}

/// Validated immutable catalog. Mutations require a new [`CatalogRevision`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelCatalog {
    snapshot: CatalogSnapshot,
}

/// Immutable catalog generation with revision and content hash.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogSnapshot {
    revision: CatalogRevision,
    hash: CatalogHash,
    entries: Vec<CatalogEntry>,
}

/// One catalog row. Disabled/unavailable rows stay visible.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogEntry {
    status: CatalogEntryStatus,
    descriptor: ModelDescriptor,
}

/// Diagnosable availability. Missing rows are never dropped.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum CatalogEntryStatus {
    Available,
    Disabled,
    Unavailable { reason: CatalogStatusReason },
}

/// Why an unavailable row was retained instead of omitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum CatalogStatusReason {
    ProviderMissing,
    ProviderUnavailable,
}

/// Typed catalog failure. Display never echoes provider bodies.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CatalogError {
    Cancelled,
    DuplicateId {
        provider: ProviderId,
        model: ModelId,
    },
    DuplicateProviderId {
        provider: ProviderId,
    },
    ImpossibleMetadata {
        provider: ProviderId,
        model: ModelId,
        kind: ImpossibleMetadataKind,
    },
    BoundExceeded,
    InvalidRequest,
}

/// Why configured metadata cannot describe a real model.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum ImpossibleMetadataKind {
    OutputExceedsContext,
    DuplicateRegion,
    DuplicateDataPolicyTag,
    PriceWithoutTable,
    CapabilityExceedsProvider,
    ContextExceedsProvider,
    OutputExceedsProvider,
}

impl CatalogHash {
    pub const fn as_artifact_id(self) -> ArtifactId {
        self.0
    }
}

impl fmt::Display for CatalogHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for CatalogHash {
    type Err = protocol::ArtifactIdParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.parse()?))
    }
}

impl CatalogConfig {
    pub fn new(
        revision: CatalogRevision,
        entries: Vec<CatalogModelSpec>,
    ) -> Result<Self, CatalogError> {
        if entries.len() > MAX_CATALOG_ENTRIES {
            return Err(CatalogError::BoundExceeded);
        }
        Ok(Self { revision, entries })
    }

    pub const fn revision(&self) -> CatalogRevision {
        self.revision
    }

    pub fn entries(&self) -> &[CatalogModelSpec] {
        &self.entries
    }
}

impl CatalogModelSpec {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: ProviderId,
        model: ModelId,
        enabled: bool,
        capabilities: ProviderCapabilities,
        prices: ModelPrices,
        regions: Vec<Region>,
        data_policy_tags: Vec<DataPolicyTag>,
        latency_class: LatencyClass,
    ) -> Self {
        Self {
            provider,
            model,
            enabled,
            capabilities,
            prices,
            regions,
            data_policy_tags,
            latency_class,
        }
    }

    pub fn provider(&self) -> &ProviderId {
        &self.provider
    }
    pub fn model(&self) -> &ModelId {
        &self.model
    }
    pub fn model_ref(&self) -> ModelRef {
        ModelRef::new(self.provider.clone(), self.model.clone())
    }
    pub const fn enabled(&self) -> bool {
        self.enabled
    }
    pub fn capabilities(&self) -> &ProviderCapabilities {
        &self.capabilities
    }
    pub fn prices(&self) -> &ModelPrices {
        &self.prices
    }
    pub fn regions(&self) -> &[Region] {
        &self.regions
    }
    pub fn data_policy_tags(&self) -> &[DataPolicyTag] {
        &self.data_policy_tags
    }
    pub const fn latency_class(&self) -> LatencyClass {
        self.latency_class
    }
}

impl ProviderCapIndex {
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    pub fn insert(
        &mut self,
        provider: ProviderId,
        entry: ProviderCapEntry,
    ) -> Result<(), CatalogError> {
        if self.entries.contains_key(&provider) {
            return Err(CatalogError::DuplicateProviderId { provider });
        }
        if self.entries.len() >= MAX_CATALOG_PROVIDERS {
            return Err(CatalogError::BoundExceeded);
        }
        self.entries.insert(provider, entry);
        Ok(())
    }

    pub fn get(&self, provider: &ProviderId) -> Option<&ProviderCapEntry> {
        self.entries.get(provider)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl ModelCatalog {
    /// Load configured models against advertised provider capabilities.
    ///
    /// Duplicate `(provider, model)` IDs and impossible metadata fail closed.
    /// Disabled and unavailable rows stay in the snapshot with a status.
    pub fn build(
        config: &CatalogConfig,
        provider_caps: &ProviderCapIndex,
        cancel: &CancellationToken,
    ) -> Result<Self, CatalogError> {
        cancel.check().map_err(map_provider)?;
        if config.entries.len() > MAX_CATALOG_ENTRIES || provider_caps.len() > MAX_CATALOG_PROVIDERS
        {
            return Err(CatalogError::BoundExceeded);
        }

        let mut seen = BTreeSet::new();
        let mut entries = Vec::with_capacity(config.entries.len());
        for (i, spec) in config.entries.iter().enumerate() {
            if i.is_multiple_of(CANCEL_CHECK_EVERY) {
                cancel.check().map_err(map_provider)?;
            }
            if !seen.insert((spec.provider.clone(), spec.model.clone())) {
                return Err(CatalogError::DuplicateId {
                    provider: spec.provider.clone(),
                    model: spec.model.clone(),
                });
            }
            validate_spec_metadata(spec)?;
            let status = resolve_status(spec, provider_caps)?;
            let descriptor = ModelDescriptor::new(
                spec.provider.clone(),
                spec.model.clone(),
                spec.capabilities.clone(),
                spec.prices.clone(),
                spec.regions.clone(),
                spec.data_policy_tags.clone(),
                spec.latency_class,
                config.revision,
                cancel,
            )
            .map_err(map_provider)?;
            entries.push(CatalogEntry { status, descriptor });
        }

        Ok(Self {
            snapshot: CatalogSnapshot::from_entries(config.revision, entries, cancel)?,
        })
    }

    pub fn snapshot(&self) -> &CatalogSnapshot {
        &self.snapshot
    }

    pub const fn revision(&self) -> CatalogRevision {
        self.snapshot.revision
    }

    pub const fn hash(&self) -> CatalogHash {
        self.snapshot.hash
    }

    pub fn entries(&self) -> &[CatalogEntry] {
        &self.snapshot.entries
    }

    pub fn get(&self, model: &ModelRef) -> Option<&CatalogEntry> {
        self.snapshot.get(model)
    }
}

impl CatalogSnapshot {
    fn from_entries(
        revision: CatalogRevision,
        mut entries: Vec<CatalogEntry>,
        cancel: &CancellationToken,
    ) -> Result<Self, CatalogError> {
        cancel.check().map_err(map_provider)?;
        entries.sort_by(|a, b| {
            a.descriptor
                .provider()
                .cmp(b.descriptor.provider())
                .then_with(|| a.descriptor.model().cmp(b.descriptor.model()))
        });
        let hash = hash_snapshot(revision, &entries, cancel)?;
        Ok(Self {
            revision,
            hash,
            entries,
        })
    }

    pub const fn revision(&self) -> CatalogRevision {
        self.revision
    }

    pub const fn hash(&self) -> CatalogHash {
        self.hash
    }

    pub fn entries(&self) -> &[CatalogEntry] {
        &self.entries
    }

    pub fn get(&self, model: &ModelRef) -> Option<&CatalogEntry> {
        self.entries.iter().find(|entry| {
            entry.descriptor.provider() == model.provider()
                && entry.descriptor.model() == model.model()
        })
    }

    /// Descriptors that are enabled and currently available.
    pub fn available(&self) -> impl Iterator<Item = &ModelDescriptor> {
        self.entries.iter().filter_map(|entry| match entry.status {
            CatalogEntryStatus::Available => Some(&entry.descriptor),
            CatalogEntryStatus::Disabled | CatalogEntryStatus::Unavailable { .. } => None,
        })
    }
}

impl CatalogEntry {
    pub const fn status(&self) -> CatalogEntryStatus {
        self.status
    }

    pub fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    pub const fn is_available(&self) -> bool {
        matches!(self.status, CatalogEntryStatus::Available)
    }
}

impl CatalogEntryStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Disabled => "disabled",
            Self::Unavailable { .. } => "unavailable",
        }
    }

    pub const fn reason(self) -> Option<CatalogStatusReason> {
        match self {
            Self::Unavailable { reason } => Some(reason),
            Self::Available | Self::Disabled => None,
        }
    }
}

impl CatalogStatusReason {
    pub const ALL: &'static [Self] = &[Self::ProviderMissing, Self::ProviderUnavailable];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProviderMissing => "provider_missing",
            Self::ProviderUnavailable => "provider_unavailable",
        }
    }
}

impl ImpossibleMetadataKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OutputExceedsContext => "output_exceeds_context",
            Self::DuplicateRegion => "duplicate_region",
            Self::DuplicateDataPolicyTag => "duplicate_data_policy_tag",
            Self::PriceWithoutTable => "price_without_table",
            Self::CapabilityExceedsProvider => "capability_exceeds_provider",
            Self::ContextExceedsProvider => "context_exceeds_provider",
            Self::OutputExceedsProvider => "output_exceeds_provider",
        }
    }
}

impl CatalogError {
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Cancelled => None,
            Self::DuplicateId { .. }
            | Self::DuplicateProviderId { .. }
            | Self::ImpossibleMetadata { .. }
            | Self::BoundExceeded
            | Self::InvalidRequest => Some(ErrorCode::ConfigInvalid),
        }
    }

    pub fn into_api_error(&self, trace_id: TraceId) -> Option<ApiError> {
        let code = self.code()?;
        let message = match self {
            Self::Cancelled => return None,
            Self::DuplicateId { .. } => "Catalog contains a duplicate model id",
            Self::DuplicateProviderId { .. } => "Catalog contains a duplicate provider id",
            Self::ImpossibleMetadata { .. } => "Catalog entry has impossible metadata",
            Self::BoundExceeded => "Catalog exceeds a documented bound",
            Self::InvalidRequest => "Catalog request is invalid",
        };
        Some(
            ApiError::new(code, message, trace_id)
                .unwrap_or_else(|_| ApiError::from_unknown(trace_id, self)),
        )
    }
}

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cancelled => "catalog build cancelled",
            Self::DuplicateId { .. } => "catalog contains a duplicate model id",
            Self::DuplicateProviderId { .. } => "catalog contains a duplicate provider id",
            Self::ImpossibleMetadata { .. } => "catalog entry has impossible metadata",
            Self::BoundExceeded => "catalog exceeds a documented bound",
            Self::InvalidRequest => "catalog request is invalid",
        })
    }
}

impl Error for CatalogError {}

impl Default for ProviderCapIndex {
    fn default() -> Self {
        Self::new()
    }
}

fn validate_spec_metadata(spec: &CatalogModelSpec) -> Result<(), CatalogError> {
    if spec.capabilities.max_output() > spec.capabilities.context_limit() {
        return Err(impossible(
            spec,
            ImpossibleMetadataKind::OutputExceedsContext,
        ));
    }
    if spec.regions.len() > MAX_REGIONS || spec.data_policy_tags.len() > MAX_DATA_POLICY_TAGS {
        return Err(CatalogError::BoundExceeded);
    }
    if has_duplicates(&spec.regions) {
        return Err(impossible(spec, ImpossibleMetadataKind::DuplicateRegion));
    }
    if has_duplicates(&spec.data_policy_tags) {
        return Err(impossible(
            spec,
            ImpossibleMetadataKind::DuplicateDataPolicyTag,
        ));
    }
    if prices_require_table(&spec.prices) && spec.prices.table_version().is_none() {
        return Err(impossible(spec, ImpossibleMetadataKind::PriceWithoutTable));
    }
    Ok(())
}

fn resolve_status(
    spec: &CatalogModelSpec,
    provider_caps: &ProviderCapIndex,
) -> Result<CatalogEntryStatus, CatalogError> {
    if !spec.enabled {
        return Ok(CatalogEntryStatus::Disabled);
    }
    match provider_caps.get(&spec.provider) {
        None => Ok(CatalogEntryStatus::Unavailable {
            reason: CatalogStatusReason::ProviderMissing,
        }),
        Some(ProviderCapEntry::Unavailable) => Ok(CatalogEntryStatus::Unavailable {
            reason: CatalogStatusReason::ProviderUnavailable,
        }),
        Some(ProviderCapEntry::Available(advertised)) => {
            if let Some(kind) = exceeds_provider(&spec.capabilities, advertised) {
                return Err(impossible(spec, kind));
            }
            Ok(CatalogEntryStatus::Available)
        }
    }
}

fn exceeds_provider(
    spec: &ProviderCapabilities,
    advertised: &ProviderCapabilities,
) -> Option<ImpossibleMetadataKind> {
    if spec.context_limit() > advertised.context_limit() {
        return Some(ImpossibleMetadataKind::ContextExceedsProvider);
    }
    if spec.max_output() > advertised.max_output() {
        return Some(ImpossibleMetadataKind::OutputExceedsProvider);
    }
    let extra_reason = spec.reasoning() == ReasoningSupport::Exposed
        && advertised.reasoning() == ReasoningSupport::None;
    if (spec.tools() && !advertised.tools())
        || (spec.streaming() && !advertised.streaming())
        || (spec.vision() && !advertised.vision())
        || (spec.caching() && !advertised.caching())
        || (spec.structured_output() && !advertised.structured_output())
        || extra_reason
    {
        return Some(ImpossibleMetadataKind::CapabilityExceedsProvider);
    }
    None
}

fn prices_require_table(prices: &ModelPrices) -> bool {
    prices.input_usd_micros_per_million().is_some()
        || prices.output_usd_micros_per_million().is_some()
        || prices.cached_input_usd_micros_per_million().is_some()
}

fn has_duplicates<T: Ord>(items: &[T]) -> bool {
    let mut seen = BTreeSet::new();
    items.iter().any(|item| !seen.insert(item))
}

fn impossible(spec: &CatalogModelSpec, kind: ImpossibleMetadataKind) -> CatalogError {
    CatalogError::ImpossibleMetadata {
        provider: spec.provider.clone(),
        model: spec.model.clone(),
        kind,
    }
}

fn map_provider(err: ProviderError) -> CatalogError {
    match err {
        ProviderError::Cancelled => CatalogError::Cancelled,
        ProviderError::BoundExceeded => CatalogError::BoundExceeded,
        _ => CatalogError::InvalidRequest,
    }
}

#[derive(Serialize)]
struct CatalogHashDocument<'a> {
    schema: &'static str,
    schema_version: u16,
    revision: u64,
    entries: &'a [CatalogHashEntry],
}

#[derive(Serialize)]
struct CatalogHashEntry {
    provider: String,
    model: String,
    status: &'static str,
    reason: Option<&'static str>,
    capabilities: ProviderCapabilities,
    prices: ModelPrices,
    regions: Vec<String>,
    data_policy_tags: Vec<String>,
    latency_class: &'static str,
}

impl CatalogHashEntry {
    fn from_entry(entry: &CatalogEntry) -> Self {
        let descriptor = &entry.descriptor;
        Self {
            provider: descriptor.provider().as_str().to_owned(),
            model: descriptor.model().as_str().to_owned(),
            status: entry.status.as_str(),
            reason: entry.status.reason().map(CatalogStatusReason::as_str),
            capabilities: descriptor.capabilities().clone(),
            prices: descriptor.prices().clone(),
            regions: descriptor
                .regions()
                .iter()
                .map(|region| region.as_str().to_owned())
                .collect(),
            data_policy_tags: descriptor
                .data_policy_tags()
                .iter()
                .map(|tag| tag.as_str().to_owned())
                .collect(),
            latency_class: descriptor.latency_class().as_str(),
        }
    }
}

fn hash_snapshot(
    revision: CatalogRevision,
    entries: &[CatalogEntry],
    cancel: &CancellationToken,
) -> Result<CatalogHash, CatalogError> {
    cancel.check().map_err(map_provider)?;
    let mut hashed = Vec::with_capacity(entries.len());
    for (i, entry) in entries.iter().enumerate() {
        if i.is_multiple_of(CANCEL_CHECK_EVERY) {
            cancel.check().map_err(map_provider)?;
        }
        hashed.push(CatalogHashEntry::from_entry(entry));
    }
    let document = CatalogHashDocument {
        schema: CATALOG_SNAPSHOT_SCHEMA,
        schema_version: CATALOG_SCHEMA_VERSION,
        revision: revision.get(),
        entries: &hashed,
    };
    let bytes = serde_json::to_vec(&document).map_err(|_| CatalogError::InvalidRequest)?;
    Ok(CatalogHash(ArtifactId::from_bytes(&bytes)))
}

impl Serialize for CatalogHash {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CatalogHash {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let id = ArtifactId::deserialize(deserializer)?;
        Ok(Self(id))
    }
}

impl Serialize for CatalogEntryStatus {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("CatalogEntryStatus", 2)?;
        state.serialize_field("status", self.as_str())?;
        state.serialize_field("reason", &self.reason().map(CatalogStatusReason::as_str))?;
        state.end()
    }
}

impl Serialize for CatalogEntry {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("CatalogEntry", 3)?;
        state.serialize_field("status", self.status.as_str())?;
        state.serialize_field(
            "reason",
            &self.status.reason().map(CatalogStatusReason::as_str),
        )?;
        state.serialize_field("descriptor", &self.descriptor)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for CatalogEntry {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            status: String,
            reason: Option<String>,
            descriptor: ModelDescriptor,
        }
        let raw = Raw::deserialize(deserializer)?;
        let status =
            parse_entry_status(&raw.status, raw.reason.as_deref()).map_err(de::Error::custom)?;
        Ok(Self {
            status,
            descriptor: raw.descriptor,
        })
    }
}

impl Serialize for CatalogSnapshot {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("CatalogSnapshot", SNAPSHOT_FIELDS.len())?;
        state.serialize_field("schema", CATALOG_SNAPSHOT_SCHEMA)?;
        state.serialize_field("schema_version", &CATALOG_SCHEMA_VERSION)?;
        state.serialize_field("revision", &self.revision)?;
        state.serialize_field("hash", &self.hash)?;
        state.serialize_field("entries", &self.entries)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for CatalogSnapshot {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            schema: String,
            schema_version: u16,
            revision: CatalogRevision,
            hash: CatalogHash,
            entries: Vec<CatalogEntry>,
        }
        let raw = Raw::deserialize(deserializer)?;
        if raw.schema != CATALOG_SNAPSHOT_SCHEMA {
            return Err(de::Error::custom("unknown catalog snapshot schema"));
        }
        if raw.schema_version != CATALOG_SCHEMA_VERSION {
            return Err(de::Error::custom("unsupported catalog snapshot version"));
        }
        if raw.entries.len() > MAX_CATALOG_ENTRIES {
            return Err(de::Error::custom("catalog exceeds a documented bound"));
        }
        for entry in &raw.entries {
            if entry.descriptor.catalog_revision() != raw.revision {
                return Err(de::Error::custom(
                    "catalog entry revision does not match snapshot",
                ));
            }
        }
        let computed = hash_snapshot(raw.revision, &raw.entries, &CancellationToken::new())
            .map_err(de::Error::custom)?;
        if computed != raw.hash {
            return Err(de::Error::custom("catalog snapshot hash mismatch"));
        }
        Ok(Self {
            revision: raw.revision,
            hash: raw.hash,
            entries: raw.entries,
        })
    }
}

fn parse_entry_status(
    status: &str,
    reason: Option<&str>,
) -> Result<CatalogEntryStatus, CatalogError> {
    match (status, reason) {
        ("available", None) => Ok(CatalogEntryStatus::Available),
        ("disabled", None) => Ok(CatalogEntryStatus::Disabled),
        ("unavailable", Some("provider_missing")) => Ok(CatalogEntryStatus::Unavailable {
            reason: CatalogStatusReason::ProviderMissing,
        }),
        ("unavailable", Some("provider_unavailable")) => Ok(CatalogEntryStatus::Unavailable {
            reason: CatalogStatusReason::ProviderUnavailable,
        }),
        _ => Err(CatalogError::InvalidRequest),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ModelPrices, PriceTableVersion, UsageFieldSet};

    const GOLDEN_TRACE: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ab";
    const GOLDEN_HASH: &str =
        "sha256:04fe861114cb41aa263f15aa81c4384a8cdd5bfb32a26f089c817684405522bf";
    const GOLDEN_SNAPSHOT: &str = r#"{"schema":"rapidlm.catalog_snapshot","schema_version":1,"revision":7,"hash":"sha256:04fe861114cb41aa263f15aa81c4384a8cdd5bfb32a26f089c817684405522bf","entries":[{"status":"disabled","reason":null,"descriptor":{"schema":"rapidlm.model_descriptor","schema_version":1,"provider":"anthropic","model":"claude-3-5-sonnet","capabilities":{"schema":"rapidlm.provider_capabilities","schema_version":1,"tools":true,"streaming":true,"vision":true,"caching":true,"reasoning":"exposed","structured_output":true,"context_limit":200000,"max_output":8192,"usage_fields":{"input":true,"cached_input":true,"uncached_input":true,"output":true,"reasoning":true,"tool":false,"cost":false}},"context_limit":200000,"max_output":8192,"prices":{"input_usd_micros_per_million":2000000,"output_usd_micros_per_million":8000000,"cached_input_usd_micros_per_million":500000,"table_version":"openai-2026-04"},"regions":["us","eu"],"data_policy_tags":["no-training"],"latency_class":"interactive","catalog_revision":7}},{"status":"unavailable","reason":"provider_unavailable","descriptor":{"schema":"rapidlm.model_descriptor","schema_version":1,"provider":"local","model":"llama-3","capabilities":{"schema":"rapidlm.provider_capabilities","schema_version":1,"tools":true,"streaming":true,"vision":false,"caching":true,"reasoning":"exposed","structured_output":true,"context_limit":8192,"max_output":1024,"usage_fields":{"input":true,"cached_input":true,"uncached_input":true,"output":true,"reasoning":true,"tool":false,"cost":false}},"context_limit":8192,"max_output":1024,"prices":{"input_usd_micros_per_million":2000000,"output_usd_micros_per_million":8000000,"cached_input_usd_micros_per_million":500000,"table_version":"openai-2026-04"},"regions":["us","eu"],"data_policy_tags":["no-training"],"latency_class":"interactive","catalog_revision":7}},{"status":"available","reason":null,"descriptor":{"schema":"rapidlm.model_descriptor","schema_version":1,"provider":"openai","model":"gpt-4.1","capabilities":{"schema":"rapidlm.provider_capabilities","schema_version":1,"tools":true,"streaming":true,"vision":false,"caching":true,"reasoning":"exposed","structured_output":true,"context_limit":128000,"max_output":8192,"usage_fields":{"input":true,"cached_input":true,"uncached_input":true,"output":true,"reasoning":true,"tool":false,"cost":false}},"context_limit":128000,"max_output":8192,"prices":{"input_usd_micros_per_million":2000000,"output_usd_micros_per_million":8000000,"cached_input_usd_micros_per_million":500000,"table_version":"openai-2026-04"},"regions":["us","eu"],"data_policy_tags":["no-training"],"latency_class":"interactive","catalog_revision":7}}]}"#;

    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    fn caps(context_limit: u32, max_output: u32, vision: bool) -> ProviderCapabilities {
        ProviderCapabilities::new(
            true,
            true,
            vision,
            true,
            ReasoningSupport::Exposed,
            true,
            context_limit,
            max_output,
            UsageFieldSet::new(true, true, true, true, true, false, false),
        )
        .expect("caps")
    }

    fn prices() -> ModelPrices {
        ModelPrices::new(
            Some(2_000_000),
            Some(8_000_000),
            Some(500_000),
            Some(PriceTableVersion::parse("openai-2026-04").expect("price table")),
        )
    }

    fn spec(
        provider: &str,
        model: &str,
        enabled: bool,
        capabilities: ProviderCapabilities,
    ) -> CatalogModelSpec {
        CatalogModelSpec::new(
            ProviderId::parse(provider).expect("provider"),
            ModelId::parse(model).expect("model"),
            enabled,
            capabilities,
            prices(),
            vec![
                Region::parse("us").expect("us"),
                Region::parse("eu").expect("eu"),
            ],
            vec![DataPolicyTag::parse("no-training").expect("tag")],
            LatencyClass::Interactive,
        )
    }

    fn build_fixture() -> ModelCatalog {
        let config = CatalogConfig::new(
            CatalogRevision::new(7).expect("revision"),
            vec![
                spec(
                    "anthropic",
                    "claude-3-5-sonnet",
                    false,
                    caps(200_000, 8192, true),
                ),
                spec("openai", "gpt-4.1", true, caps(128_000, 8192, false)),
                spec("local", "llama-3", true, caps(8_192, 1024, false)),
            ],
        )
        .expect("config");
        let mut provider_caps = ProviderCapIndex::new();
        provider_caps
            .insert(
                ProviderId::parse("openai").expect("openai"),
                ProviderCapEntry::Available(caps(128_000, 8192, false)),
            )
            .expect("openai caps");
        provider_caps
            .insert(
                ProviderId::parse("anthropic").expect("anthropic"),
                ProviderCapEntry::Available(caps(200_000, 8192, true)),
            )
            .expect("anthropic caps");
        provider_caps
            .insert(
                ProviderId::parse("local").expect("local"),
                ProviderCapEntry::Unavailable,
            )
            .expect("local caps");
        ModelCatalog::build(&config, &provider_caps, &live()).expect("catalog")
    }

    #[test]
    fn snapshot_has_revision_and_hash() {
        let catalog = build_fixture();
        assert_eq!(catalog.revision(), CatalogRevision::new(7).expect("rev"));
        assert_eq!(catalog.hash().to_string(), GOLDEN_HASH);
        assert_eq!(
            catalog.hash().to_string(),
            catalog.snapshot().hash().to_string()
        );
    }

    #[test]
    fn hash_is_deterministic_and_order_independent() {
        let first = build_fixture();
        let reversed = {
            let config = CatalogConfig::new(
                CatalogRevision::new(7).expect("revision"),
                vec![
                    spec("local", "llama-3", true, caps(8_192, 1024, false)),
                    spec("openai", "gpt-4.1", true, caps(128_000, 8192, false)),
                    spec(
                        "anthropic",
                        "claude-3-5-sonnet",
                        false,
                        caps(200_000, 8192, true),
                    ),
                ],
            )
            .expect("config");
            let mut provider_caps = ProviderCapIndex::new();
            provider_caps
                .insert(
                    ProviderId::parse("local").expect("local"),
                    ProviderCapEntry::Unavailable,
                )
                .expect("local");
            provider_caps
                .insert(
                    ProviderId::parse("anthropic").expect("anthropic"),
                    ProviderCapEntry::Available(caps(200_000, 8192, true)),
                )
                .expect("anthropic");
            provider_caps
                .insert(
                    ProviderId::parse("openai").expect("openai"),
                    ProviderCapEntry::Available(caps(128_000, 8192, false)),
                )
                .expect("openai");
            ModelCatalog::build(&config, &provider_caps, &live()).expect("catalog")
        };
        assert_eq!(first.hash(), reversed.hash());
        assert_eq!(first.snapshot(), reversed.snapshot());
        assert_eq!(
            first.entries()[0].descriptor().provider().as_str(),
            "anthropic"
        );
        assert_eq!(first.entries()[1].descriptor().provider().as_str(), "local");
        assert_eq!(
            first.entries()[2].descriptor().provider().as_str(),
            "openai"
        );
    }

    #[test]
    fn snapshot_serialization_golden_round_trips() {
        let catalog = build_fixture();
        let json = serde_json::to_string(catalog.snapshot()).expect("serialize");
        assert_eq!(json, GOLDEN_SNAPSHOT);
        let decoded: CatalogSnapshot = serde_json::from_str(GOLDEN_SNAPSHOT).expect("decode");
        assert_eq!(decoded, *catalog.snapshot());
        assert_eq!(decoded.hash().to_string(), GOLDEN_HASH);
    }

    #[test]
    fn disabled_and_unavailable_models_are_retained() {
        let catalog = build_fixture();
        assert_eq!(catalog.entries().len(), 3);
        let disabled = catalog
            .get(&ModelRef::new(
                ProviderId::parse("anthropic").expect("p"),
                ModelId::parse("claude-3-5-sonnet").expect("m"),
            ))
            .expect("disabled");
        assert_eq!(disabled.status(), CatalogEntryStatus::Disabled);
        assert!(!disabled.is_available());
        let unavailable = catalog
            .get(&ModelRef::new(
                ProviderId::parse("local").expect("p"),
                ModelId::parse("llama-3").expect("m"),
            ))
            .expect("unavailable");
        assert_eq!(
            unavailable.status(),
            CatalogEntryStatus::Unavailable {
                reason: CatalogStatusReason::ProviderUnavailable,
            }
        );
        let missing_config = CatalogConfig::new(
            CatalogRevision::new(1).expect("rev"),
            vec![spec("missing", "model-a", true, caps(8_192, 1024, false))],
        )
        .expect("config");
        let missing =
            ModelCatalog::build(&missing_config, &ProviderCapIndex::new(), &live()).expect("built");
        assert_eq!(
            missing.entries()[0].status(),
            CatalogEntryStatus::Unavailable {
                reason: CatalogStatusReason::ProviderMissing,
            }
        );
        assert_eq!(catalog.snapshot().available().count(), 1);
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let config = CatalogConfig::new(
            CatalogRevision::new(1).expect("rev"),
            vec![
                spec("openai", "gpt-4.1", true, caps(128_000, 8192, false)),
                spec("openai", "gpt-4.1", false, caps(128_000, 8192, false)),
            ],
        )
        .expect("config");
        let err = ModelCatalog::build(&config, &ProviderCapIndex::new(), &live()).expect_err("dup");
        assert_eq!(
            err,
            CatalogError::DuplicateId {
                provider: ProviderId::parse("openai").expect("p"),
                model: ModelId::parse("gpt-4.1").expect("m"),
            }
        );
        let mut caps_index = ProviderCapIndex::new();
        caps_index
            .insert(
                ProviderId::parse("openai").expect("p"),
                ProviderCapEntry::Unavailable,
            )
            .expect("first");
        let dup_provider = caps_index.insert(
            ProviderId::parse("openai").expect("p"),
            ProviderCapEntry::Unavailable,
        );
        assert_eq!(
            dup_provider,
            Err(CatalogError::DuplicateProviderId {
                provider: ProviderId::parse("openai").expect("p"),
            })
        );
    }

    #[test]
    fn impossible_metadata_is_rejected() {
        let too_much_output = CatalogModelSpec::new(
            ProviderId::parse("openai").expect("p"),
            ModelId::parse("gpt-4.1").expect("m"),
            true,
            caps(1_024, 2_048, false),
            ModelPrices::UNKNOWN,
            vec![],
            vec![],
            LatencyClass::Standard,
        );
        let err = ModelCatalog::build(
            &CatalogConfig::new(CatalogRevision::new(1).expect("rev"), vec![too_much_output])
                .expect("config"),
            &ProviderCapIndex::new(),
            &live(),
        )
        .expect_err("output");
        assert!(matches!(
            err,
            CatalogError::ImpossibleMetadata {
                kind: ImpossibleMetadataKind::OutputExceedsContext,
                ..
            }
        ));

        let priced = CatalogModelSpec::new(
            ProviderId::parse("openai").expect("p"),
            ModelId::parse("gpt-4.1").expect("m"),
            true,
            caps(8_192, 1024, false),
            ModelPrices::new(Some(1), None, None, None),
            vec![],
            vec![],
            LatencyClass::Standard,
        );
        let err = ModelCatalog::build(
            &CatalogConfig::new(CatalogRevision::new(1).expect("rev"), vec![priced]).expect("cfg"),
            &ProviderCapIndex::new(),
            &live(),
        )
        .expect_err("price");
        assert!(matches!(
            err,
            CatalogError::ImpossibleMetadata {
                kind: ImpossibleMetadataKind::PriceWithoutTable,
                ..
            }
        ));

        let dup_region = CatalogModelSpec::new(
            ProviderId::parse("openai").expect("p"),
            ModelId::parse("gpt-4.1").expect("m"),
            true,
            caps(8_192, 1024, false),
            ModelPrices::UNKNOWN,
            vec![
                Region::parse("us").expect("us"),
                Region::parse("us").expect("us2"),
            ],
            vec![],
            LatencyClass::Standard,
        );
        let err = ModelCatalog::build(
            &CatalogConfig::new(CatalogRevision::new(1).expect("rev"), vec![dup_region])
                .expect("cfg"),
            &ProviderCapIndex::new(),
            &live(),
        )
        .expect_err("region");
        assert!(matches!(
            err,
            CatalogError::ImpossibleMetadata {
                kind: ImpossibleMetadataKind::DuplicateRegion,
                ..
            }
        ));

        let mut provider_caps = ProviderCapIndex::new();
        provider_caps
            .insert(
                ProviderId::parse("openai").expect("p"),
                ProviderCapEntry::Available(caps(8_192, 1024, false)),
            )
            .expect("caps");
        let vision = spec("openai", "gpt-4.1", true, caps(8_192, 1024, true));
        let err = ModelCatalog::build(
            &CatalogConfig::new(CatalogRevision::new(2).expect("rev"), vec![vision]).expect("cfg"),
            &provider_caps,
            &live(),
        )
        .expect_err("vision");
        assert!(matches!(
            err,
            CatalogError::ImpossibleMetadata {
                kind: ImpossibleMetadataKind::CapabilityExceedsProvider,
                ..
            }
        ));
    }

    #[test]
    fn cancellation_is_honored() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let config = CatalogConfig::new(
            CatalogRevision::new(1).expect("rev"),
            vec![spec("openai", "gpt-4.1", true, caps(8_192, 1024, false))],
        )
        .expect("config");
        assert_eq!(
            ModelCatalog::build(&config, &ProviderCapIndex::new(), &cancel),
            Err(CatalogError::Cancelled)
        );
    }

    #[test]
    fn hash_mismatch_fails_closed() {
        let catalog = build_fixture();
        let mut value = serde_json::to_value(catalog.snapshot()).expect("json");
        value["hash"] =
            serde_json::Value::String(ArtifactId::from_bytes(b"tampered-catalog").to_string());
        assert!(serde_json::from_value::<CatalogSnapshot>(value).is_err());
    }

    #[test]
    fn unknown_snapshot_fields_fail_closed() {
        let catalog = build_fixture();
        let mut value = serde_json::to_value(catalog.snapshot()).expect("json");
        value["secret_grant"] = serde_json::Value::Bool(true);
        assert!(serde_json::from_value::<CatalogSnapshot>(value).is_err());
    }

    #[test]
    fn catalog_errors_map_to_config_invalid() {
        let trace = GOLDEN_TRACE.parse::<TraceId>().expect("trace");
        let err = CatalogError::DuplicateId {
            provider: ProviderId::parse("openai").expect("p"),
            model: ModelId::parse("gpt-4.1").expect("m"),
        };
        let api = err.into_api_error(trace).expect("api");
        assert_eq!(api.code(), ErrorCode::ConfigInvalid);
        assert!(!api.retryable());
        assert!(CatalogError::Cancelled.into_api_error(trace).is_none());
    }
}
