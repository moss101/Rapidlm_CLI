//! Hard route filters applied before scoring.
//!
//! [`eligible`] removes models that fail tool, vision, context, privacy,
//! region, provider-availability, or user-pin constraints. A pin that
//! violates hard policy fails explicitly; it never selects another model.

use std::error::Error;
use std::fmt;

use protocol::{ApiError, ErrorCode, TraceId};

use crate::catalog::{
    CatalogEntry, CatalogEntryStatus, CatalogHash, CatalogSnapshot, CatalogStatusReason,
    MAX_CATALOG_ENTRIES,
};
use crate::provider::{
    CancellationToken, CatalogRevision, DataPolicyTag, ModelCapabilities, ModelDescriptor,
    ModelPurpose, ModelRef, PrivacyClass, ProviderCapabilities, ProviderId, Region,
};

/// Data-policy tag required by [`PrivacyClass::NoTraining`].
pub const NO_TRAINING_TAG: &str = "no-training";

/// Data-policy tag that marks a model as on-device / non-egress.
pub const LOCAL_ONLY_TAG: &str = "local-only";

/// Provider identifier treated as local for [`PrivacyClass::LocalOnly`].
pub const LOCAL_PROVIDER_ID: &str = "local";

/// Maximum models retained in one [`EligibleSet`].
pub const MAX_ELIGIBLE_MODELS: usize = MAX_CATALOG_ENTRIES;

const CANCEL_CHECK_EVERY: usize = 16;

/// Routing request consumed by hard filters (and later scoring).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteRequest {
    purpose: ModelPurpose,
    privacy: PrivacyClass,
    required_region: Option<Region>,
    min_context: u32,
    input_tokens: u32,
    output_reserve: u32,
    required: ModelCapabilities,
    latency_slo_ms: Option<u64>,
    budget_remaining_usd_micros: Option<u64>,
    user_pin: Option<ModelRef>,
}

/// Models that survived every hard constraint, plus recorded rejections.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EligibleSet {
    models: Vec<ModelDescriptor>,
    rejections: Vec<HardRejection>,
    catalog_revision: CatalogRevision,
    catalog_hash: CatalogHash,
}

/// Why one catalog row was excluded from [`EligibleSet`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HardRejection {
    model: ModelRef,
    reason: RejectionReason,
}

/// First hard constraint a catalog row failed.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RejectionReason {
    Disabled,
    ProviderUnavailable { reason: CatalogStatusReason },
    MissingTools,
    MissingVision,
    MissingStreaming,
    MissingCaching,
    MissingReasoning,
    MissingStructuredOutput,
    ContextTooSmall { required: u32, available: u32 },
    OutputReserveTooLarge { required: u32, available: u32 },
    PrivacyDenied { class: PrivacyClass },
    RegionDenied { required: Region },
    UserPinExcludes,
}

/// Why filtering produced no route. A pin never falls back to another model.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum NoRouteReason {
    EmptyCatalog,
    NoneEligible {
        rejections: Vec<HardRejection>,
    },
    PinNotInCatalog {
        pin: ModelRef,
    },
    PinUnavailable {
        pin: ModelRef,
        status: CatalogEntryStatus,
    },
    PinViolatesPolicy {
        pin: ModelRef,
        reason: RejectionReason,
    },
}

/// Typed filter failure. Display never echoes provider bodies or secrets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RouteFilterError {
    Cancelled,
    InvalidRequest,
    BoundExceeded,
    NoRoute(NoRouteReason),
}

impl RouteRequest {
    /// Build a request. [`PrivacyClass::Regional`] requires `required_region`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        purpose: ModelPurpose,
        privacy: PrivacyClass,
        required_region: Option<Region>,
        min_context: u32,
        input_tokens: u32,
        output_reserve: u32,
        required: ModelCapabilities,
        latency_slo_ms: Option<u64>,
        budget_remaining_usd_micros: Option<u64>,
        user_pin: Option<ModelRef>,
    ) -> Result<Self, RouteFilterError> {
        if privacy == PrivacyClass::Regional && required_region.is_none() {
            return Err(RouteFilterError::InvalidRequest);
        }
        Ok(Self {
            purpose,
            privacy,
            required_region,
            min_context,
            input_tokens,
            output_reserve,
            required,
            latency_slo_ms,
            budget_remaining_usd_micros,
            user_pin,
        })
    }

    pub const fn purpose(&self) -> ModelPurpose {
        self.purpose
    }
    pub const fn privacy(&self) -> PrivacyClass {
        self.privacy
    }
    pub fn required_region(&self) -> Option<&Region> {
        self.required_region.as_ref()
    }
    pub const fn min_context(&self) -> u32 {
        self.min_context
    }
    pub const fn input_tokens(&self) -> u32 {
        self.input_tokens
    }
    pub const fn output_reserve(&self) -> u32 {
        self.output_reserve
    }
    pub const fn required(&self) -> ModelCapabilities {
        self.required
    }
    pub const fn latency_slo_ms(&self) -> Option<u64> {
        self.latency_slo_ms
    }
    pub const fn budget_remaining_usd_micros(&self) -> Option<u64> {
        self.budget_remaining_usd_micros
    }
    pub fn user_pin(&self) -> Option<&ModelRef> {
        self.user_pin.as_ref()
    }

    /// Context tokens that must fit in the model window.
    pub const fn required_context(&self) -> u32 {
        let from_tokens = self.input_tokens.saturating_add(self.output_reserve);
        if self.min_context > from_tokens {
            self.min_context
        } else {
            from_tokens
        }
    }
}

impl EligibleSet {
    pub fn models(&self) -> &[ModelDescriptor] {
        &self.models
    }
    pub fn rejections(&self) -> &[HardRejection] {
        &self.rejections
    }
    pub const fn catalog_revision(&self) -> CatalogRevision {
        self.catalog_revision
    }
    pub const fn catalog_hash(&self) -> CatalogHash {
        self.catalog_hash
    }
    pub fn len(&self) -> usize {
        self.models.len()
    }
    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }
}

impl HardRejection {
    pub fn model(&self) -> &ModelRef {
        &self.model
    }
    pub fn reason(&self) -> &RejectionReason {
        &self.reason
    }
}

impl RejectionReason {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::ProviderUnavailable { .. } => "provider_unavailable",
            Self::MissingTools => "missing_tools",
            Self::MissingVision => "missing_vision",
            Self::MissingStreaming => "missing_streaming",
            Self::MissingCaching => "missing_caching",
            Self::MissingReasoning => "missing_reasoning",
            Self::MissingStructuredOutput => "missing_structured_output",
            Self::ContextTooSmall { .. } => "context_too_small",
            Self::OutputReserveTooLarge { .. } => "output_reserve_too_large",
            Self::PrivacyDenied { .. } => "privacy_denied",
            Self::RegionDenied { .. } => "region_denied",
            Self::UserPinExcludes => "user_pin_excludes",
        }
    }
}

impl NoRouteReason {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::EmptyCatalog => "empty_catalog",
            Self::NoneEligible { .. } => "none_eligible",
            Self::PinNotInCatalog { .. } => "pin_not_in_catalog",
            Self::PinUnavailable { .. } => "pin_unavailable",
            Self::PinViolatesPolicy { .. } => "pin_violates_policy",
        }
    }
}

/// Filter `catalog` by hard constraints. Scoring is out of scope.
pub fn eligible(
    request: &RouteRequest,
    catalog: &CatalogSnapshot,
    cancel: &CancellationToken,
) -> Result<EligibleSet, RouteFilterError> {
    cancel.check().map_err(|_| RouteFilterError::Cancelled)?;
    if catalog.entries().len() > MAX_CATALOG_ENTRIES {
        return Err(RouteFilterError::BoundExceeded);
    }
    if catalog.entries().is_empty() {
        return Err(RouteFilterError::NoRoute(NoRouteReason::EmptyCatalog));
    }

    let mut models = Vec::new();
    let mut rejections = Vec::new();
    for (i, entry) in catalog.entries().iter().enumerate() {
        if i.is_multiple_of(CANCEL_CHECK_EVERY) {
            cancel.check().map_err(|_| RouteFilterError::Cancelled)?;
        }
        let model = entry.descriptor().model_ref();
        if let Some(pin) = request.user_pin()
            && &model != pin {
                push_rejection(&mut rejections, model, RejectionReason::UserPinExcludes)?;
                continue;
            }
        match hard_constraints(request, entry) {
            Ok(()) => {
                if models.len() >= MAX_ELIGIBLE_MODELS {
                    return Err(RouteFilterError::BoundExceeded);
                }
                models.push(entry.descriptor().clone());
            }
            Err(reason) => push_rejection(&mut rejections, model, reason)?,
        }
    }

    if let Some(pin) = request.user_pin() {
        return pin_outcome(pin, catalog, models, rejections);
    }
    if models.is_empty() {
        return Err(RouteFilterError::NoRoute(NoRouteReason::NoneEligible {
            rejections,
        }));
    }
    Ok(EligibleSet {
        models,
        rejections,
        catalog_revision: catalog.revision(),
        catalog_hash: catalog.hash(),
    })
}

/// First failing hard constraint, or `Ok` if the row may be scored.
pub fn hard_constraints(
    request: &RouteRequest,
    entry: &CatalogEntry,
) -> Result<(), RejectionReason> {
    match entry.status() {
        CatalogEntryStatus::Available => {}
        CatalogEntryStatus::Disabled => return Err(RejectionReason::Disabled),
        CatalogEntryStatus::Unavailable { reason } => {
            return Err(RejectionReason::ProviderUnavailable { reason });
        }
    }
    let descriptor = entry.descriptor();
    if let Some(reason) = missing_capability(descriptor.capabilities(), &request.required) {
        return Err(reason);
    }
    let needed = request.required_context();
    if descriptor.context_limit() < needed {
        return Err(RejectionReason::ContextTooSmall {
            required: needed,
            available: descriptor.context_limit(),
        });
    }
    if request.output_reserve > 0 && descriptor.max_output() < request.output_reserve {
        return Err(RejectionReason::OutputReserveTooLarge {
            required: request.output_reserve,
            available: descriptor.max_output(),
        });
    }
    if !privacy_allows(request.privacy, descriptor) {
        return Err(RejectionReason::PrivacyDenied {
            class: request.privacy,
        });
    }
    if let Some(region) = request.required_region.as_ref()
        && !descriptor.regions().iter().any(|listed| listed == region) {
            return Err(RejectionReason::RegionDenied {
                required: region.clone(),
            });
        }
    Ok(())
}

fn missing_capability(
    advertised: &ProviderCapabilities,
    required: &ModelCapabilities,
) -> Option<RejectionReason> {
    if required.tools() && !advertised.tools() {
        return Some(RejectionReason::MissingTools);
    }
    if required.vision() && !advertised.vision() {
        return Some(RejectionReason::MissingVision);
    }
    if required.streaming() && !advertised.streaming() {
        return Some(RejectionReason::MissingStreaming);
    }
    if required.caching() && !advertised.caching() {
        return Some(RejectionReason::MissingCaching);
    }
    if required.reasoning() && advertised.reasoning() != crate::provider::ReasoningSupport::Exposed
    {
        return Some(RejectionReason::MissingReasoning);
    }
    if required.structured_output() && !advertised.structured_output() {
        return Some(RejectionReason::MissingStructuredOutput);
    }
    None
}

fn privacy_allows(class: PrivacyClass, descriptor: &ModelDescriptor) -> bool {
    match class {
        PrivacyClass::Unrestricted => true,
        PrivacyClass::NoTraining => descriptor.data_policy_tags().iter().any(is_no_training_tag),
        PrivacyClass::Regional => true,
        PrivacyClass::LocalOnly => is_local_model(descriptor),
    }
}

fn is_local_model(descriptor: &ModelDescriptor) -> bool {
    is_local_provider(descriptor.provider()) || has_tag(descriptor, LOCAL_ONLY_TAG)
}

fn has_tag(descriptor: &ModelDescriptor, tag: &str) -> bool {
    descriptor
        .data_policy_tags()
        .iter()
        .any(|listed| listed.as_str() == tag)
}

fn push_rejection(
    rejections: &mut Vec<HardRejection>,
    model: ModelRef,
    reason: RejectionReason,
) -> Result<(), RouteFilterError> {
    if rejections.len() >= MAX_CATALOG_ENTRIES {
        return Err(RouteFilterError::BoundExceeded);
    }
    rejections.push(HardRejection { model, reason });
    Ok(())
}

fn pin_outcome(
    pin: &ModelRef,
    catalog: &CatalogSnapshot,
    models: Vec<ModelDescriptor>,
    rejections: Vec<HardRejection>,
) -> Result<EligibleSet, RouteFilterError> {
    if let Some(chosen) = models.first() {
        if chosen.model_ref() != *pin || models.len() != 1 {
            return Err(RouteFilterError::NoRoute(
                NoRouteReason::PinViolatesPolicy {
                    pin: pin.clone(),
                    reason: RejectionReason::UserPinExcludes,
                },
            ));
        }
        return Ok(EligibleSet {
            models,
            rejections,
            catalog_revision: catalog.revision(),
            catalog_hash: catalog.hash(),
        });
    }
    match catalog.get(pin) {
        None => Err(RouteFilterError::NoRoute(NoRouteReason::PinNotInCatalog {
            pin: pin.clone(),
        })),
        Some(entry) => {
            let reason = rejections
                .iter()
                .find(|rejection| rejection.model() == pin)
                .map(|rejection| rejection.reason().clone())
                .unwrap_or(match entry.status() {
                    CatalogEntryStatus::Disabled => RejectionReason::Disabled,
                    CatalogEntryStatus::Unavailable { reason } => {
                        RejectionReason::ProviderUnavailable { reason }
                    }
                    CatalogEntryStatus::Available => RejectionReason::UserPinExcludes,
                });
            Err(RouteFilterError::NoRoute(match reason {
                RejectionReason::Disabled => NoRouteReason::PinUnavailable {
                    pin: pin.clone(),
                    status: CatalogEntryStatus::Disabled,
                },
                RejectionReason::ProviderUnavailable { reason } => NoRouteReason::PinUnavailable {
                    pin: pin.clone(),
                    status: CatalogEntryStatus::Unavailable { reason },
                },
                other => NoRouteReason::PinViolatesPolicy {
                    pin: pin.clone(),
                    reason: other,
                },
            }))
        }
    }
}

impl RouteFilterError {
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Cancelled => None,
            Self::InvalidRequest | Self::BoundExceeded => Some(ErrorCode::ConfigInvalid),
            Self::NoRoute(NoRouteReason::PinViolatesPolicy { .. })
            | Self::NoRoute(NoRouteReason::PinUnavailable { .. })
            | Self::NoRoute(NoRouteReason::NoneEligible { .. }) => Some(ErrorCode::PolicyDenied),
            Self::NoRoute(NoRouteReason::EmptyCatalog)
            | Self::NoRoute(NoRouteReason::PinNotInCatalog { .. }) => {
                Some(ErrorCode::ConfigInvalid)
            }
        }
    }

    pub fn into_api_error(&self, trace_id: TraceId) -> Option<ApiError> {
        let code = self.code()?;
        let message = match self {
            Self::Cancelled => return None,
            Self::InvalidRequest => "Route request is invalid",
            Self::BoundExceeded => "Route filter exceeds a documented bound",
            Self::NoRoute(NoRouteReason::EmptyCatalog) => "Model catalog is empty",
            Self::NoRoute(NoRouteReason::NoneEligible { .. }) => {
                "No model satisfies hard route constraints"
            }
            Self::NoRoute(NoRouteReason::PinNotInCatalog { .. }) => {
                "User pin is not present in the catalog"
            }
            Self::NoRoute(NoRouteReason::PinUnavailable { .. }) => {
                "User pin is disabled or unavailable"
            }
            Self::NoRoute(NoRouteReason::PinViolatesPolicy { .. }) => {
                "User pin violates hard route policy"
            }
        };
        Some(
            ApiError::new(code, message, trace_id)
                .unwrap_or_else(|_| ApiError::from_unknown(trace_id, self)),
        )
    }
}

impl fmt::Display for RejectionReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for NoRouteReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for RouteFilterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cancelled => "route filter cancelled",
            Self::InvalidRequest => "route request is invalid",
            Self::BoundExceeded => "route filter exceeds a documented bound",
            Self::NoRoute(_) => "no eligible model",
        })
    }
}

impl Error for RouteFilterError {}

/// Whether `provider` is the reserved local adapter identity.
pub fn is_local_provider(provider: &ProviderId) -> bool {
    provider.as_str() == LOCAL_PROVIDER_ID
}

/// Whether `tag` is the reserved no-training policy token.
pub fn is_no_training_tag(tag: &DataPolicyTag) -> bool {
    tag.as_str() == NO_TRAINING_TAG
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{
        CatalogConfig, CatalogModelSpec, ModelCatalog, ProviderCapEntry, ProviderCapIndex,
    };
    use crate::provider::{
        CatalogRevision, LatencyClass, ModelId, ModelPrices, PriceTableVersion, ReasoningSupport,
        UsageFieldSet,
    };
    use proptest::prelude::*;

    const GOLDEN_TRACE: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ab";

    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    fn caps(
        context_limit: u32,
        max_output: u32,
        tools: bool,
        vision: bool,
    ) -> ProviderCapabilities {
        ProviderCapabilities::new(
            tools,
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
        regions: &[&str],
        tags: &[&str],
    ) -> CatalogModelSpec {
        CatalogModelSpec::new(
            ProviderId::parse(provider).expect("provider"),
            ModelId::parse(model).expect("model"),
            enabled,
            capabilities,
            prices(),
            regions
                .iter()
                .map(|region| Region::parse(*region).expect("region"))
                .collect(),
            tags.iter()
                .map(|tag| DataPolicyTag::parse(*tag).expect("tag"))
                .collect(),
            LatencyClass::Interactive,
        )
    }

    fn catalog(rows: Vec<(CatalogModelSpec, ProviderCapEntry)>) -> CatalogSnapshot {
        let mut config_rows = Vec::new();
        let mut index = ProviderCapIndex::new();
        for (row, advertised) in rows {
            let provider = row.provider().clone();
            if index.get(&provider).is_none() {
                index.insert(provider, advertised).expect("provider cap");
            }
            config_rows.push(row);
        }
        let config =
            CatalogConfig::new(CatalogRevision::new(3).expect("rev"), config_rows).expect("config");
        ModelCatalog::build(&config, &index, &live())
            .expect("catalog")
            .snapshot()
            .clone()
    }

    fn available_entry(advertised: ProviderCapabilities) -> ProviderCapEntry {
        ProviderCapEntry::Available(advertised)
    }

    fn request(
        privacy: PrivacyClass,
        region: Option<&str>,
        min_context: u32,
        input_tokens: u32,
        output_reserve: u32,
        required: ModelCapabilities,
        user_pin: Option<ModelRef>,
    ) -> RouteRequest {
        RouteRequest::new(
            ModelPurpose::Code,
            privacy,
            region.map(|r| Region::parse(r).expect("region")),
            min_context,
            input_tokens,
            output_reserve,
            required,
            None,
            None,
            user_pin,
        )
        .expect("request")
    }

    fn pin(provider: &str, model: &str) -> ModelRef {
        ModelRef::new(
            ProviderId::parse(provider).expect("p"),
            ModelId::parse(model).expect("m"),
        )
    }

    fn none_required() -> ModelCapabilities {
        ModelCapabilities::NONE
    }

    fn tools_and_vision() -> ModelCapabilities {
        ModelCapabilities::new(true, false, true, false, false, false)
    }

    #[test]
    fn regional_privacy_without_region_is_invalid() {
        let err = RouteRequest::new(
            ModelPurpose::Chat,
            PrivacyClass::Regional,
            None,
            0,
            0,
            0,
            none_required(),
            None,
            None,
            None,
        )
        .expect_err("regional");
        assert_eq!(err, RouteFilterError::InvalidRequest);
    }

    #[test]
    fn empty_catalog_has_no_route() {
        let snapshot = catalog(vec![]);
        let req = request(
            PrivacyClass::Unrestricted,
            None,
            0,
            0,
            0,
            none_required(),
            None,
        );
        assert_eq!(
            eligible(&req, &snapshot, &live()),
            Err(RouteFilterError::NoRoute(NoRouteReason::EmptyCatalog))
        );
    }

    #[test]
    fn available_matching_model_is_eligible() {
        let advertised = caps(128_000, 8192, true, true);
        let snapshot = catalog(vec![(
            spec(
                "openai",
                "gpt-4.1",
                true,
                advertised.clone(),
                &["us", "eu"],
                &[NO_TRAINING_TAG],
            ),
            available_entry(advertised),
        )]);
        let req = request(
            PrivacyClass::NoTraining,
            Some("us"),
            8_000,
            1_000,
            512,
            tools_and_vision(),
            None,
        );
        let set = eligible(&req, &snapshot, &live()).expect("eligible");
        assert_eq!(set.len(), 1);
        assert_eq!(set.models()[0].model().as_str(), "gpt-4.1");
        assert!(set.rejections().is_empty());
    }

    #[test]
    fn disabled_and_unavailable_are_never_eligible() {
        let advertised = caps(8_192, 1024, true, false);
        let snapshot = catalog(vec![
            (
                spec(
                    "openai",
                    "gpt-disabled",
                    false,
                    advertised.clone(),
                    &["us"],
                    &[],
                ),
                available_entry(advertised.clone()),
            ),
            (
                spec(
                    "local",
                    "llama-3",
                    true,
                    advertised.clone(),
                    &["us"],
                    &[LOCAL_ONLY_TAG],
                ),
                ProviderCapEntry::Unavailable,
            ),
        ]);
        let req = request(
            PrivacyClass::Unrestricted,
            None,
            0,
            0,
            0,
            none_required(),
            None,
        );
        let err = eligible(&req, &snapshot, &live()).expect_err("none");
        match err {
            RouteFilterError::NoRoute(NoRouteReason::NoneEligible { rejections }) => {
                assert_eq!(rejections.len(), 2);
                assert!(
                    rejections
                        .iter()
                        .any(|r| r.reason() == &RejectionReason::Disabled)
                );
                assert!(rejections.iter().any(|r| matches!(
                    r.reason(),
                    RejectionReason::ProviderUnavailable {
                        reason: CatalogStatusReason::ProviderUnavailable
                    }
                )));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn missing_tools_or_vision_is_rejected() {
        let advertised = caps(8_192, 1024, false, false);
        let snapshot = catalog(vec![(
            spec("openai", "tiny", true, advertised.clone(), &["us"], &[]),
            available_entry(advertised),
        )]);
        let req = request(
            PrivacyClass::Unrestricted,
            None,
            0,
            0,
            0,
            tools_and_vision(),
            None,
        );
        let err = eligible(&req, &snapshot, &live()).expect_err("caps");
        match err {
            RouteFilterError::NoRoute(NoRouteReason::NoneEligible { rejections }) => {
                assert_eq!(rejections[0].reason(), &RejectionReason::MissingTools);
            }
            other => panic!("unexpected {other:?}"),
        }
        let vision_only = request(
            PrivacyClass::Unrestricted,
            None,
            0,
            0,
            0,
            ModelCapabilities::new(false, false, true, false, false, false),
            None,
        );
        let err = eligible(&vision_only, &snapshot, &live()).expect_err("vision");
        match err {
            RouteFilterError::NoRoute(NoRouteReason::NoneEligible { rejections }) => {
                assert_eq!(rejections[0].reason(), &RejectionReason::MissingVision);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn context_and_output_reserve_are_hard() {
        let advertised = caps(4_096, 256, true, false);
        let snapshot = catalog(vec![(
            spec("openai", "small", true, advertised.clone(), &["us"], &[]),
            available_entry(advertised),
        )]);
        let too_wide = request(
            PrivacyClass::Unrestricted,
            None,
            8_192,
            0,
            0,
            none_required(),
            None,
        );
        let err = eligible(&too_wide, &snapshot, &live()).expect_err("ctx");
        match err {
            RouteFilterError::NoRoute(NoRouteReason::NoneEligible { rejections }) => {
                assert_eq!(
                    rejections[0].reason(),
                    &RejectionReason::ContextTooSmall {
                        required: 8_192,
                        available: 4_096
                    }
                );
            }
            other => panic!("unexpected {other:?}"),
        }
        let too_much_out = request(
            PrivacyClass::Unrestricted,
            None,
            0,
            100,
            512,
            none_required(),
            None,
        );
        let err = eligible(&too_much_out, &snapshot, &live()).expect_err("out");
        match err {
            RouteFilterError::NoRoute(NoRouteReason::NoneEligible { rejections }) => {
                assert!(matches!(
                    rejections[0].reason(),
                    RejectionReason::ContextTooSmall { .. }
                        | RejectionReason::OutputReserveTooLarge { .. }
                ));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn privacy_and_region_are_hard() {
        let cloud = caps(8_192, 1024, true, false);
        let local = caps(8_192, 1024, true, false);
        let snapshot = catalog(vec![
            (
                spec("openai", "gpt-4.1", true, cloud.clone(), &["us"], &[]),
                available_entry(cloud),
            ),
            (
                spec(
                    "local",
                    "llama-3",
                    true,
                    local.clone(),
                    &["eu"],
                    &[LOCAL_ONLY_TAG, NO_TRAINING_TAG],
                ),
                available_entry(local),
            ),
        ]);
        let no_train = request(
            PrivacyClass::NoTraining,
            None,
            0,
            0,
            0,
            none_required(),
            None,
        );
        let set = eligible(&no_train, &snapshot, &live()).expect("no-train");
        assert_eq!(set.len(), 1);
        assert_eq!(set.models()[0].provider().as_str(), "local");

        let local_only = request(
            PrivacyClass::LocalOnly,
            None,
            0,
            0,
            0,
            none_required(),
            None,
        );
        let set = eligible(&local_only, &snapshot, &live()).expect("local");
        assert_eq!(set.len(), 1);
        assert!(is_local_model(&set.models()[0]));

        let eu = request(
            PrivacyClass::Regional,
            Some("eu"),
            0,
            0,
            0,
            none_required(),
            None,
        );
        let set = eligible(&eu, &snapshot, &live()).expect("eu");
        assert_eq!(set.len(), 1);
        assert_eq!(set.models()[0].model().as_str(), "llama-3");
    }

    #[test]
    fn user_pin_fails_explicitly_and_does_not_override_policy() {
        let cloud = caps(128_000, 8192, true, true);
        let local = caps(8_192, 1024, true, false);
        let snapshot = catalog(vec![
            (
                spec("openai", "gpt-4.1", true, cloud.clone(), &["us"], &[]),
                available_entry(cloud),
            ),
            (
                spec(
                    "local",
                    "llama-3",
                    true,
                    local.clone(),
                    &["us"],
                    &[LOCAL_ONLY_TAG, NO_TRAINING_TAG],
                ),
                available_entry(local),
            ),
        ]);
        let pinned_cloud = request(
            PrivacyClass::LocalOnly,
            None,
            0,
            0,
            0,
            none_required(),
            Some(pin("openai", "gpt-4.1")),
        );
        let err = eligible(&pinned_cloud, &snapshot, &live()).expect_err("pin");
        match err {
            RouteFilterError::NoRoute(NoRouteReason::PinViolatesPolicy { pin, reason }) => {
                assert_eq!(pin.provider().as_str(), "openai");
                assert_eq!(
                    reason,
                    RejectionReason::PrivacyDenied {
                        class: PrivacyClass::LocalOnly
                    }
                );
            }
            other => panic!("silent override or wrong class: {other:?}"),
        }

        let pinned_tools = request(
            PrivacyClass::Unrestricted,
            None,
            0,
            0,
            0,
            tools_and_vision(),
            Some(pin("local", "llama-3")),
        );
        let err = eligible(&pinned_tools, &snapshot, &live()).expect_err("vision pin");
        assert!(matches!(
            err,
            RouteFilterError::NoRoute(NoRouteReason::PinViolatesPolicy {
                reason: RejectionReason::MissingVision,
                ..
            })
        ));

        let ok_pin = request(
            PrivacyClass::LocalOnly,
            None,
            0,
            0,
            0,
            none_required(),
            Some(pin("local", "llama-3")),
        );
        let set = eligible(&ok_pin, &snapshot, &live()).expect("ok pin");
        assert_eq!(set.len(), 1);
        assert_eq!(set.models()[0].model().as_str(), "llama-3");
        assert!(
            set.rejections()
                .iter()
                .all(|r| r.reason() == &RejectionReason::UserPinExcludes)
        );
    }

    #[test]
    fn missing_or_unavailable_pin_is_explicit() {
        let advertised = caps(8_192, 1024, true, false);
        let snapshot = catalog(vec![(
            spec(
                "openai",
                "gpt-disabled",
                false,
                advertised.clone(),
                &["us"],
                &[],
            ),
            available_entry(advertised),
        )]);
        let missing = request(
            PrivacyClass::Unrestricted,
            None,
            0,
            0,
            0,
            none_required(),
            Some(pin("openai", "missing-model")),
        );
        assert!(matches!(
            eligible(&missing, &snapshot, &live()),
            Err(RouteFilterError::NoRoute(
                NoRouteReason::PinNotInCatalog { .. }
            ))
        ));
        let disabled = request(
            PrivacyClass::Unrestricted,
            None,
            0,
            0,
            0,
            none_required(),
            Some(pin("openai", "gpt-disabled")),
        );
        assert!(matches!(
            eligible(&disabled, &snapshot, &live()),
            Err(RouteFilterError::NoRoute(NoRouteReason::PinUnavailable {
                status: CatalogEntryStatus::Disabled,
                ..
            }))
        ));
    }

    #[test]
    fn cancellation_is_honored() {
        let advertised = caps(8_192, 1024, true, false);
        let snapshot = catalog(vec![(
            spec("openai", "gpt-4.1", true, advertised.clone(), &["us"], &[]),
            available_entry(advertised),
        )]);
        let req = request(
            PrivacyClass::Unrestricted,
            None,
            0,
            0,
            0,
            none_required(),
            None,
        );
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            eligible(&req, &snapshot, &cancel),
            Err(RouteFilterError::Cancelled)
        );
    }

    #[test]
    fn display_and_api_errors_do_not_echo_secrets() {
        let leak = "sk-ant-api03-not-a-real-secret";
        let err = RouteFilterError::NoRoute(NoRouteReason::PinViolatesPolicy {
            pin: pin("openai", "gpt-4.1"),
            reason: RejectionReason::PrivacyDenied {
                class: PrivacyClass::LocalOnly,
            },
        });
        let shown = err.to_string();
        assert!(!shown.contains(leak));
        assert!(!shown.contains("sk-"));
        assert_eq!(shown, "no eligible model");
        let api = err
            .into_api_error(GOLDEN_TRACE.parse().expect("trace"))
            .expect("api");
        assert_eq!(api.code(), ErrorCode::PolicyDenied);
        assert!(!api.message().contains(leak));
        assert!(!api.retryable());
        assert!(
            RouteFilterError::Cancelled
                .into_api_error(GOLDEN_TRACE.parse().expect("trace"))
                .is_none()
        );
    }

    fn assert_set_obeys_hard_filters(
        req: &RouteRequest,
        catalog: &CatalogSnapshot,
        set: &EligibleSet,
    ) {
        assert!(!set.is_empty());
        assert!(set.len() <= MAX_ELIGIBLE_MODELS);
        if let Some(pin) = req.user_pin() {
            assert_eq!(set.len(), 1);
            assert_eq!(&set.models()[0].model_ref(), pin);
        }
        for model in set.models() {
            let entry = catalog.get(&model.model_ref()).expect("in catalog");
            assert!(
                hard_constraints(req, entry).is_ok(),
                "eligible model failed hard_constraints"
            );
        }
        for entry in catalog.entries() {
            if hard_constraints(req, entry).is_err() {
                assert!(
                    set.models()
                        .iter()
                        .all(|model| model.model_ref() != entry.descriptor().model_ref()),
                    "ineligible model leaked into EligibleSet"
                );
            }
        }
    }

    #[test]
    fn property_ineligible_model_never_returned() {
        #[allow(clippy::type_complexity)] // exhaustive routing-case table
        let cases: &[(
            PrivacyClass,
            Option<&str>,
            ModelCapabilities,
            Option<(&str, &str)>,
        )] = &[
            (PrivacyClass::Unrestricted, None, none_required(), None),
            (PrivacyClass::NoTraining, None, tools_and_vision(), None),
            (
                PrivacyClass::LocalOnly,
                None,
                none_required(),
                Some(("openai", "gpt-4.1")),
            ),
            (
                PrivacyClass::Regional,
                Some("eu"),
                ModelCapabilities::new(true, false, false, false, false, false),
                Some(("local", "llama-3")),
            ),
            (
                PrivacyClass::Unrestricted,
                Some("us"),
                tools_and_vision(),
                Some(("openai", "gpt-4.1")),
            ),
        ];
        let cloud = caps(128_000, 8192, true, true);
        let small = caps(2_048, 256, false, false);
        let local = caps(8_192, 1024, true, false);
        let snapshot = catalog(vec![
            (
                spec("openai", "gpt-4.1", true, cloud.clone(), &["us"], &[]),
                available_entry(cloud),
            ),
            (
                spec("anthropic", "tiny", true, small.clone(), &["eu"], &[]),
                available_entry(small),
            ),
            (
                spec(
                    "local",
                    "llama-3",
                    true,
                    local.clone(),
                    &["eu"],
                    &[LOCAL_ONLY_TAG, NO_TRAINING_TAG],
                ),
                available_entry(local),
            ),
            (
                spec(
                    "openai",
                    "disabled",
                    false,
                    caps(8_192, 1024, true, true),
                    &["us"],
                    &[NO_TRAINING_TAG],
                ),
                available_entry(caps(8_192, 1024, true, true)),
            ),
        ]);
        for (privacy, region, required, pin_ref) in cases {
            let req = request(
                *privacy,
                *region,
                4_096,
                1_000,
                128,
                *required,
                pin_ref.map(|(p, m)| pin(p, m)),
            );
            match eligible(&req, &snapshot, &live()) {
                Ok(set) => assert_set_obeys_hard_filters(&req, &snapshot, &set),
                Err(RouteFilterError::NoRoute(reason)) => {
                    if pin_ref.is_some() {
                        assert!(matches!(
                            reason,
                            NoRouteReason::PinViolatesPolicy { .. }
                                | NoRouteReason::PinUnavailable { .. }
                                | NoRouteReason::PinNotInCatalog { .. }
                        ));
                    }
                    for entry in snapshot.entries() {
                        if req.user_pin().is_none() {
                            assert!(hard_constraints(&req, entry).is_err());
                        }
                    }
                }
                Err(other) => panic!("unexpected {other:?}"),
            }
        }
    }

    #[test]
    fn pin_of_ineligible_model_never_returns_sibling() {
        let advertised = caps(8_192, 1024, true, false);
        let snapshot = catalog(vec![
            (
                spec("openai", "cloud", true, advertised.clone(), &["us"], &[]),
                available_entry(advertised.clone()),
            ),
            (
                spec(
                    "local",
                    "ok",
                    true,
                    advertised.clone(),
                    &["us"],
                    &[LOCAL_ONLY_TAG],
                ),
                available_entry(advertised),
            ),
        ]);
        let req = request(
            PrivacyClass::LocalOnly,
            None,
            0,
            0,
            0,
            none_required(),
            Some(pin("openai", "cloud")),
        );
        match eligible(&req, &snapshot, &live()) {
            Ok(set) => panic!("pin override leaked {:?}", set.models()),
            Err(RouteFilterError::NoRoute(NoRouteReason::PinViolatesPolicy { pin, .. })) => {
                assert_eq!(pin.model().as_str(), "cloud");
            }
            other => panic!("expected explicit pin policy failure, got {other:?}"),
        }
    }

    fn arb_privacy() -> impl Strategy<Value = PrivacyClass> {
        prop_oneof![
            Just(PrivacyClass::Unrestricted),
            Just(PrivacyClass::NoTraining),
            Just(PrivacyClass::Regional),
            Just(PrivacyClass::LocalOnly),
        ]
    }

    proptest! {
        #[test]
        fn property_eligible_set_never_contains_ineligible(
            privacy in arb_privacy(),
            require_tools in any::<bool>(),
            require_vision in any::<bool>(),
            want_eu in any::<bool>(),
            pin_cloud in any::<bool>(),
            min_context in 0u32..16_384,
        ) {
            let cloud = caps(128_000, 8192, true, true);
            let local = caps(8_192, 1024, require_tools, require_vision);
            let snapshot = catalog(vec![
                (
                    spec("openai", "gpt-4.1", true, cloud.clone(), &["us"], &[]),
                    available_entry(cloud),
                ),
                (
                    spec(
                        "local",
                        "llama-3",
                        true,
                        local.clone(),
                        &["eu"],
                        &[LOCAL_ONLY_TAG, NO_TRAINING_TAG],
                    ),
                    available_entry(local),
                ),
            ]);
            let region = match privacy {
                PrivacyClass::Regional => Some(if want_eu { "eu" } else { "us" }),
                _ if want_eu => Some("eu"),
                _ => None,
            };
            let user_pin = if pin_cloud {
                Some(pin("openai", "gpt-4.1"))
            } else {
                None
            };
            let req = request(
                privacy,
                region,
                min_context,
                0,
                0,
                ModelCapabilities::new(require_tools, false, require_vision, false, false, false),
                user_pin,
            );
            match eligible(&req, &snapshot, &live()) {
                Ok(set) => assert_set_obeys_hard_filters(&req, &snapshot, &set),
                Err(RouteFilterError::NoRoute(NoRouteReason::PinViolatesPolicy { .. }))
                | Err(RouteFilterError::NoRoute(NoRouteReason::PinUnavailable { .. }))
                | Err(RouteFilterError::NoRoute(NoRouteReason::PinNotInCatalog { .. })) => {
                    prop_assert!(pin_cloud);
                }
                Err(RouteFilterError::NoRoute(NoRouteReason::NoneEligible { rejections })) => {
                    prop_assert!(!pin_cloud);
                    prop_assert!(!rejections.is_empty());
                    for entry in snapshot.entries() {
                        prop_assert!(hard_constraints(&req, entry).is_err());
                    }
                }
                Err(RouteFilterError::Cancelled)
                | Err(RouteFilterError::InvalidRequest)
                | Err(RouteFilterError::BoundExceeded)
                | Err(RouteFilterError::NoRoute(NoRouteReason::EmptyCatalog)) => {
                    prop_assert!(false, "unexpected closed-class error");
                }
            }
        }
    }
}
