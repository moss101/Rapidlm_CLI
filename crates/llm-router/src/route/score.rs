//! Versioned route scoring applied after hard filters.
//!
//! [`score`] is a pure, inspectable function. [`select`] ranks an
//! [`EligibleSet`] with a deterministic tie-break and records the policy
//! version plus every score component on the decision.

use std::cmp::Ordering;
use std::error::Error;
use std::fmt;

use protocol::{ApiError, ErrorCode, TraceId};
use serde::de::{self, Deserializer};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};

use crate::provider::{CancellationToken, LatencyClass, ModelDescriptor, ModelPurpose, ModelRef};
use crate::route::filter::{EligibleSet, HardRejection, RouteRequest};

/// Wire schema name for [`RoutePolicyV1`] and recorded decisions.
pub const ROUTE_POLICY_SCHEMA: &str = "rapidlm.route_policy";

/// Wire schema name for [`ScoreBreakdown`].
pub const SCORE_BREAKDOWN_SCHEMA: &str = "rapidlm.score_breakdown";

/// Wire schema name for [`RouteDecision`].
pub const ROUTE_DECISION_SCHEMA: &str = "rapidlm.route_decision";

/// v1 router policy / score formula version recorded on every selection.
pub const ROUTE_POLICY_VERSION: u16 = 1;

/// Inclusive upper bound for component scores and priors.
pub const SCORE_SCALE: u16 = 10_000;

/// Maximum quality-prior rows on one policy.
pub const MAX_QUALITY_PRIORS: usize = 1_024;

/// Maximum reliability-prior rows on one policy.
pub const MAX_RELIABILITY_PRIORS: usize = 256;

/// Maximum candidate scores retained on one [`RouteDecision`].
pub const MAX_CANDIDATE_SCORES: usize = crate::route::filter::MAX_ELIGIBLE_MODELS;

const CANCEL_CHECK_EVERY: usize = 16;
const MILLION: u128 = 1_000_000;

const POLICY_FIELDS: &[&str] = &[
    "schema",
    "schema_version",
    "weights",
    "defaults",
    "quality_priors",
    "reliability_priors",
];

const BREAKDOWN_FIELDS: &[&str] = &[
    "schema",
    "schema_version",
    "model",
    "policy_version",
    "weights",
    "components",
    "total",
    "quality_prior",
    "reliability_prior",
    "estimated_cost_usd_micros",
    "latency_class",
    "purpose",
];

const DECISION_FIELDS: &[&str] = &[
    "schema",
    "schema_version",
    "model",
    "policy_version",
    "selected",
    "candidate_scores",
    "hard_rejections",
    "fallback_chain",
];

/// Explicit v1 weights. Zero is allowed per component; all-zero is rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct RouteWeightsV1 {
    quality: u16,
    cost: u16,
    latency: u16,
    reliability: u16,
}

/// Versioned latency-class budgets used only for scoring, not hard filters.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct LatencyBudgetsV1 {
    interactive_ms: u64,
    standard_ms: u64,
    batch_ms: u64,
}

/// Default priors and scoring scales for policy v1.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ScoreDefaultsV1 {
    quality: u16,
    reliability: u16,
    cost_reference_usd_micros: u64,
    latency: LatencyBudgetsV1,
}

/// Task-purpose quality prior for one model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QualityPrior {
    model: ModelRef,
    purpose: ModelPurpose,
    score: u16,
}

/// Reliability prior for one model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReliabilityPrior {
    model: ModelRef,
    score: u16,
}

/// Versioned weighted policy. Only v1 is constructible.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoutePolicyV1 {
    weights: RouteWeightsV1,
    defaults: ScoreDefaultsV1,
    quality_priors: Vec<QualityPrior>,
    reliability_priors: Vec<ReliabilityPrior>,
}

/// Four recorded component scores (each in `0..=SCORE_SCALE`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ScoreComponents {
    quality: u16,
    cost: u16,
    latency: u16,
    reliability: u16,
}

/// Inspectable result of [`score`]. Comparison uses [`ScoreBreakdown::total_cmp`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScoreBreakdown {
    model: ModelRef,
    policy_version: u16,
    weights: RouteWeightsV1,
    components: ScoreComponents,
    total: u64,
    quality_prior: u16,
    reliability_prior: u16,
    estimated_cost_usd_micros: Option<u64>,
    latency_class: LatencyClass,
    purpose: ModelPurpose,
}

/// Selected route plus every candidate breakdown and hard-filter rejections.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteDecision {
    model: ModelRef,
    policy_version: u16,
    selected: ScoreBreakdown,
    candidate_scores: Vec<ScoreBreakdown>,
    hard_rejections: Vec<HardRejection>,
    fallback_chain: Vec<ModelRef>,
}

/// Typed scoring failure. Display never echoes provider bodies or secrets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RouteScoreError {
    Cancelled,
    InvalidPolicy,
    BoundExceeded,
    NoEligibleModel,
}

impl RouteWeightsV1 {
    pub const fn new(
        quality: u16,
        cost: u16,
        latency: u16,
        reliability: u16,
    ) -> Result<Self, RouteScoreError> {
        if quality == 0 && cost == 0 && latency == 0 && reliability == 0 {
            return Err(RouteScoreError::InvalidPolicy);
        }
        Ok(Self {
            quality,
            cost,
            latency,
            reliability,
        })
    }

    /// Default v1 mix: quality 40%, cost 25%, latency 20%, reliability 15%.
    pub const fn standard() -> Self {
        Self {
            quality: 4_000,
            cost: 2_500,
            latency: 2_000,
            reliability: 1_500,
        }
    }

    pub const fn quality(self) -> u16 {
        self.quality
    }
    pub const fn cost(self) -> u16 {
        self.cost
    }
    pub const fn latency(self) -> u16 {
        self.latency
    }
    pub const fn reliability(self) -> u16 {
        self.reliability
    }

    pub fn sum(self) -> u64 {
        u64::from(self.quality)
            + u64::from(self.cost)
            + u64::from(self.latency)
            + u64::from(self.reliability)
    }
}

impl LatencyBudgetsV1 {
    pub fn new(
        interactive_ms: u64,
        standard_ms: u64,
        batch_ms: u64,
    ) -> Result<Self, RouteScoreError> {
        if interactive_ms == 0
            || standard_ms == 0
            || batch_ms == 0
            || interactive_ms > standard_ms
            || standard_ms > batch_ms
        {
            return Err(RouteScoreError::InvalidPolicy);
        }
        Ok(Self {
            interactive_ms,
            standard_ms,
            batch_ms,
        })
    }

    pub const fn standard() -> Self {
        Self {
            interactive_ms: 2_000,
            standard_ms: 15_000,
            batch_ms: 120_000,
        }
    }

    pub const fn interactive_ms(self) -> u64 {
        self.interactive_ms
    }
    pub const fn standard_ms(self) -> u64 {
        self.standard_ms
    }
    pub const fn batch_ms(self) -> u64 {
        self.batch_ms
    }

    pub const fn expected_ms(self, class: LatencyClass) -> u64 {
        match class {
            LatencyClass::Interactive => self.interactive_ms,
            LatencyClass::Standard => self.standard_ms,
            LatencyClass::Batch => self.batch_ms,
        }
    }
}

impl ScoreDefaultsV1 {
    pub fn new(
        quality: u16,
        reliability: u16,
        cost_reference_usd_micros: u64,
        latency: LatencyBudgetsV1,
    ) -> Result<Self, RouteScoreError> {
        if quality > SCORE_SCALE || reliability > SCORE_SCALE || cost_reference_usd_micros == 0 {
            return Err(RouteScoreError::InvalidPolicy);
        }
        Ok(Self {
            quality,
            reliability,
            cost_reference_usd_micros,
            latency,
        })
    }

    pub const fn standard() -> Self {
        Self {
            quality: 5_000,
            reliability: 5_000,
            cost_reference_usd_micros: 1_000_000,
            latency: LatencyBudgetsV1::standard(),
        }
    }

    pub const fn quality(self) -> u16 {
        self.quality
    }
    pub const fn reliability(self) -> u16 {
        self.reliability
    }
    pub const fn cost_reference_usd_micros(self) -> u64 {
        self.cost_reference_usd_micros
    }
    pub const fn latency(self) -> LatencyBudgetsV1 {
        self.latency
    }
}

impl QualityPrior {
    pub fn new(
        model: ModelRef,
        purpose: ModelPurpose,
        score: u16,
    ) -> Result<Self, RouteScoreError> {
        if score > SCORE_SCALE {
            return Err(RouteScoreError::InvalidPolicy);
        }
        Ok(Self {
            model,
            purpose,
            score,
        })
    }

    pub fn model(&self) -> &ModelRef {
        &self.model
    }
    pub const fn purpose(&self) -> ModelPurpose {
        self.purpose
    }
    pub const fn score(&self) -> u16 {
        self.score
    }
}

impl ReliabilityPrior {
    pub fn new(model: ModelRef, score: u16) -> Result<Self, RouteScoreError> {
        if score > SCORE_SCALE {
            return Err(RouteScoreError::InvalidPolicy);
        }
        Ok(Self { model, score })
    }

    pub fn model(&self) -> &ModelRef {
        &self.model
    }
    pub const fn score(&self) -> u16 {
        self.score
    }
}

impl RoutePolicyV1 {
    pub fn new(
        weights: RouteWeightsV1,
        defaults: ScoreDefaultsV1,
        quality_priors: Vec<QualityPrior>,
        reliability_priors: Vec<ReliabilityPrior>,
    ) -> Result<Self, RouteScoreError> {
        if quality_priors.len() > MAX_QUALITY_PRIORS
            || reliability_priors.len() > MAX_RELIABILITY_PRIORS
        {
            return Err(RouteScoreError::BoundExceeded);
        }
        for (i, prior) in quality_priors.iter().enumerate() {
            if quality_priors[..i]
                .iter()
                .any(|seen| seen.model == prior.model && seen.purpose == prior.purpose)
            {
                return Err(RouteScoreError::InvalidPolicy);
            }
        }
        for (i, prior) in reliability_priors.iter().enumerate() {
            if reliability_priors[..i]
                .iter()
                .any(|seen| seen.model == prior.model)
            {
                return Err(RouteScoreError::InvalidPolicy);
            }
        }
        Ok(Self {
            weights,
            defaults,
            quality_priors,
            reliability_priors,
        })
    }

    /// Documented v1 policy: standard weights, mid-scale defaults, no model priors.
    pub fn standard() -> Self {
        // Standard constructors are locally proven valid.
        Self {
            weights: RouteWeightsV1::standard(),
            defaults: ScoreDefaultsV1::standard(),
            quality_priors: Vec::new(),
            reliability_priors: Vec::new(),
        }
    }

    pub const fn version(&self) -> u16 {
        ROUTE_POLICY_VERSION
    }
    pub const fn schema(&self) -> &'static str {
        ROUTE_POLICY_SCHEMA
    }
    pub const fn weights(&self) -> RouteWeightsV1 {
        self.weights
    }
    pub const fn defaults(&self) -> ScoreDefaultsV1 {
        self.defaults
    }
    pub fn quality_priors(&self) -> &[QualityPrior] {
        &self.quality_priors
    }
    pub fn reliability_priors(&self) -> &[ReliabilityPrior] {
        &self.reliability_priors
    }

    fn quality_prior(&self, model: &ModelRef, purpose: ModelPurpose) -> u16 {
        self.quality_priors
            .iter()
            .find(|prior| &prior.model == model && prior.purpose == purpose)
            .map(QualityPrior::score)
            .unwrap_or(self.defaults.quality)
    }

    fn reliability_prior(&self, model: &ModelRef) -> u16 {
        self.reliability_priors
            .iter()
            .find(|prior| &prior.model == model)
            .map(ReliabilityPrior::score)
            .unwrap_or(self.defaults.reliability)
    }
}

impl ScoreComponents {
    pub const fn quality(self) -> u16 {
        self.quality
    }
    pub const fn cost(self) -> u16 {
        self.cost
    }
    pub const fn latency(self) -> u16 {
        self.latency
    }
    pub const fn reliability(self) -> u16 {
        self.reliability
    }
}

impl ScoreBreakdown {
    pub fn model(&self) -> &ModelRef {
        &self.model
    }
    pub const fn policy_version(&self) -> u16 {
        self.policy_version
    }
    pub const fn weights(&self) -> RouteWeightsV1 {
        self.weights
    }
    pub const fn components(&self) -> ScoreComponents {
        self.components
    }
    pub const fn total(&self) -> u64 {
        self.total
    }
    pub const fn quality_prior(&self) -> u16 {
        self.quality_prior
    }
    pub const fn reliability_prior(&self) -> u16 {
        self.reliability_prior
    }
    pub const fn estimated_cost_usd_micros(&self) -> Option<u64> {
        self.estimated_cost_usd_micros
    }
    pub const fn latency_class(&self) -> LatencyClass {
        self.latency_class
    }
    pub const fn purpose(&self) -> ModelPurpose {
        self.purpose
    }

    /// Deterministic total order: higher total, then quality, reliability,
    /// lower known cost, then lexicographically smaller provider/model.
    pub fn total_cmp(&self, other: &Self) -> Ordering {
        self.total
            .cmp(&other.total)
            .then(self.components.quality.cmp(&other.components.quality))
            .then(
                self.components
                    .reliability
                    .cmp(&other.components.reliability),
            )
            .then(cmp_cost(
                self.estimated_cost_usd_micros,
                other.estimated_cost_usd_micros,
            ))
            .then(
                other
                    .model
                    .provider()
                    .as_str()
                    .cmp(self.model.provider().as_str()),
            )
            .then(
                other
                    .model
                    .model()
                    .as_str()
                    .cmp(self.model.model().as_str()),
            )
    }
}

impl RouteDecision {
    pub fn model(&self) -> &ModelRef {
        &self.model
    }
    pub const fn policy_version(&self) -> u16 {
        self.policy_version
    }
    pub fn selected(&self) -> &ScoreBreakdown {
        &self.selected
    }
    pub fn candidate_scores(&self) -> &[ScoreBreakdown] {
        &self.candidate_scores
    }
    pub fn hard_rejections(&self) -> &[HardRejection] {
        &self.hard_rejections
    }
    pub fn fallback_chain(&self) -> &[ModelRef] {
        &self.fallback_chain
    }
}

/// Score one model with policy v1. Does not apply hard filters.
pub fn score(
    model: &ModelDescriptor,
    request: &RouteRequest,
    policy_v1: &RoutePolicyV1,
) -> ScoreBreakdown {
    let model_ref = model.model_ref();
    let purpose = request.purpose();
    let quality_prior = policy_v1.quality_prior(&model_ref, purpose);
    let reliability_prior = policy_v1.reliability_prior(&model_ref);
    let estimated_cost_usd_micros = estimate_cost_usd_micros(model, request);
    let quality = quality_prior;
    let reliability = reliability_prior;
    let cost = cost_score(
        estimated_cost_usd_micros,
        request.budget_remaining_usd_micros(),
        policy_v1.defaults.cost_reference_usd_micros,
    );
    let latency = latency_score(
        model.latency_class(),
        request.latency_slo_ms(),
        policy_v1.defaults.latency,
    );
    let weights = policy_v1.weights;
    let total = u64::from(quality) * u64::from(weights.quality)
        + u64::from(cost) * u64::from(weights.cost)
        + u64::from(latency) * u64::from(weights.latency)
        + u64::from(reliability) * u64::from(weights.reliability);
    ScoreBreakdown {
        model: model_ref,
        policy_version: policy_v1.version(),
        weights,
        components: ScoreComponents {
            quality,
            cost,
            latency,
            reliability,
        },
        total,
        quality_prior,
        reliability_prior,
        estimated_cost_usd_micros,
        latency_class: model.latency_class(),
        purpose,
    }
}

/// Rank eligible models. Tie-break is deterministic. Decision records
/// `policy_version` and the selected components.
pub fn select(
    request: &RouteRequest,
    eligible: &EligibleSet,
    policy_v1: &RoutePolicyV1,
    cancel: &CancellationToken,
) -> Result<RouteDecision, RouteScoreError> {
    cancel.check().map_err(|_| RouteScoreError::Cancelled)?;
    if eligible.models().len() > MAX_CANDIDATE_SCORES {
        return Err(RouteScoreError::BoundExceeded);
    }
    if eligible.is_empty() {
        return Err(RouteScoreError::NoEligibleModel);
    }

    let mut candidate_scores = Vec::with_capacity(eligible.len());
    for (i, model) in eligible.models().iter().enumerate() {
        if i.is_multiple_of(CANCEL_CHECK_EVERY) {
            cancel.check().map_err(|_| RouteScoreError::Cancelled)?;
        }
        candidate_scores.push(score(model, request, policy_v1));
    }
    candidate_scores.sort_by(|left, right| left.total_cmp(right).reverse());

    let selected = candidate_scores
        .first()
        .cloned()
        .ok_or(RouteScoreError::NoEligibleModel)?;
    Ok(RouteDecision {
        model: selected.model.clone(),
        policy_version: policy_v1.version(),
        selected,
        candidate_scores,
        hard_rejections: eligible.rejections().to_vec(),
        fallback_chain: Vec::new(),
    })
}

fn estimate_cost_usd_micros(model: &ModelDescriptor, request: &RouteRequest) -> Option<u64> {
    let prices = model.prices();
    let input = prices.input_usd_micros_per_million()?;
    let output = prices.output_usd_micros_per_million()?;
    let input_part = token_cost_micros(request.input_tokens(), input);
    let output_part = token_cost_micros(request.output_reserve(), output);
    Some(input_part.saturating_add(output_part))
}

fn token_cost_micros(tokens: u32, usd_micros_per_million: u64) -> u64 {
    let raw = u128::from(tokens) * u128::from(usd_micros_per_million) / MILLION;
    u64::try_from(raw).unwrap_or(u64::MAX)
}

fn cost_score(estimate: Option<u64>, budget: Option<u64>, reference: u64) -> u16 {
    let Some(estimate) = estimate else {
        return 0;
    };
    if let Some(budget) = budget {
        if budget == 0 {
            return if estimate == 0 { SCORE_SCALE } else { 0 };
        }
        if estimate > budget {
            return 0;
        }
        return ((u64::from(SCORE_SCALE) * (budget - estimate)) / budget) as u16;
    }
    let reference = reference.max(1);
    ((u64::from(SCORE_SCALE) * reference) / (reference + estimate)) as u16
}

fn latency_score(class: LatencyClass, slo_ms: Option<u64>, budgets: LatencyBudgetsV1) -> u16 {
    let class_ms = budgets.expected_ms(class);
    let best = budgets.interactive_ms;
    let worst = budgets.batch_ms;
    let span = worst.saturating_sub(best);
    let delta = worst.saturating_sub(class_ms.min(worst));
    let class_score = (u64::from(SCORE_SCALE) * delta)
        .checked_div(span)
        .map(|value| value as u16)
        .unwrap_or(SCORE_SCALE);
    match slo_ms {
        None => class_score,
        Some(slo) if class_ms <= slo => class_score,
        Some(slo) => ((u64::from(class_score) * slo) / class_ms.max(1)) as u16,
    }
}

fn cmp_cost(left: Option<u64>, right: Option<u64>) -> Ordering {
    match (left, right) {
        (Some(a), Some(b)) => b.cmp(&a),
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => Ordering::Equal,
    }
}

impl RouteScoreError {
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Cancelled => None,
            Self::InvalidPolicy | Self::BoundExceeded => Some(ErrorCode::ConfigInvalid),
            Self::NoEligibleModel => Some(ErrorCode::PolicyDenied),
        }
    }

    pub fn into_api_error(&self, trace_id: TraceId) -> Option<ApiError> {
        let code = self.code()?;
        let message = match self {
            Self::Cancelled => return None,
            Self::InvalidPolicy => "Route scoring policy is invalid",
            Self::BoundExceeded => "Route scoring exceeds a documented bound",
            Self::NoEligibleModel => "No eligible model to score",
        };
        Some(
            ApiError::new(code, message, trace_id)
                .unwrap_or_else(|_| ApiError::from_unknown(trace_id, self)),
        )
    }
}

impl fmt::Display for RouteScoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cancelled => "route scoring cancelled",
            Self::InvalidPolicy => "route scoring policy is invalid",
            Self::BoundExceeded => "route scoring exceeds a documented bound",
            Self::NoEligibleModel => "no eligible model",
        })
    }
}

impl Error for RouteScoreError {}

impl Serialize for RouteWeightsV1 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("RouteWeightsV1", 4)?;
        state.serialize_field("quality", &self.quality)?;
        state.serialize_field("cost", &self.cost)?;
        state.serialize_field("latency", &self.latency)?;
        state.serialize_field("reliability", &self.reliability)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for RouteWeightsV1 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            quality: u16,
            cost: u16,
            latency: u16,
            reliability: u16,
        }
        let raw = Raw::deserialize(deserializer)?;
        Self::new(raw.quality, raw.cost, raw.latency, raw.reliability).map_err(de::Error::custom)
    }
}

impl Serialize for LatencyBudgetsV1 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("LatencyBudgetsV1", 3)?;
        state.serialize_field("interactive_ms", &self.interactive_ms)?;
        state.serialize_field("standard_ms", &self.standard_ms)?;
        state.serialize_field("batch_ms", &self.batch_ms)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for LatencyBudgetsV1 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            interactive_ms: u64,
            standard_ms: u64,
            batch_ms: u64,
        }
        let raw = Raw::deserialize(deserializer)?;
        Self::new(raw.interactive_ms, raw.standard_ms, raw.batch_ms).map_err(de::Error::custom)
    }
}

impl Serialize for ScoreDefaultsV1 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("ScoreDefaultsV1", 4)?;
        state.serialize_field("quality", &self.quality)?;
        state.serialize_field("reliability", &self.reliability)?;
        state.serialize_field("cost_reference_usd_micros", &self.cost_reference_usd_micros)?;
        state.serialize_field("latency", &self.latency)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ScoreDefaultsV1 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            quality: u16,
            reliability: u16,
            cost_reference_usd_micros: u64,
            latency: LatencyBudgetsV1,
        }
        let raw = Raw::deserialize(deserializer)?;
        Self::new(
            raw.quality,
            raw.reliability,
            raw.cost_reference_usd_micros,
            raw.latency,
        )
        .map_err(de::Error::custom)
    }
}

impl Serialize for QualityPrior {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("QualityPrior", 3)?;
        state.serialize_field("model", &self.model)?;
        state.serialize_field("purpose", &self.purpose)?;
        state.serialize_field("score", &self.score)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for QualityPrior {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            model: ModelRef,
            purpose: ModelPurpose,
            score: u16,
        }
        let raw = Raw::deserialize(deserializer)?;
        Self::new(raw.model, raw.purpose, raw.score).map_err(de::Error::custom)
    }
}

impl Serialize for ReliabilityPrior {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("ReliabilityPrior", 2)?;
        state.serialize_field("model", &self.model)?;
        state.serialize_field("score", &self.score)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ReliabilityPrior {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            model: ModelRef,
            score: u16,
        }
        let raw = Raw::deserialize(deserializer)?;
        Self::new(raw.model, raw.score).map_err(de::Error::custom)
    }
}

impl Serialize for RoutePolicyV1 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("RoutePolicyV1", POLICY_FIELDS.len())?;
        state.serialize_field("schema", ROUTE_POLICY_SCHEMA)?;
        state.serialize_field("schema_version", &ROUTE_POLICY_VERSION)?;
        state.serialize_field("weights", &self.weights)?;
        state.serialize_field("defaults", &self.defaults)?;
        state.serialize_field("quality_priors", &self.quality_priors)?;
        state.serialize_field("reliability_priors", &self.reliability_priors)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for RoutePolicyV1 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            schema: String,
            schema_version: u16,
            weights: RouteWeightsV1,
            defaults: ScoreDefaultsV1,
            quality_priors: Vec<QualityPrior>,
            reliability_priors: Vec<ReliabilityPrior>,
        }
        let raw = Raw::deserialize(deserializer)?;
        if raw.schema != ROUTE_POLICY_SCHEMA || raw.schema_version != ROUTE_POLICY_VERSION {
            return Err(de::Error::custom("unsupported route policy version"));
        }
        Self::new(
            raw.weights,
            raw.defaults,
            raw.quality_priors,
            raw.reliability_priors,
        )
        .map_err(de::Error::custom)
    }
}

impl Serialize for ScoreComponents {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("ScoreComponents", 4)?;
        state.serialize_field("quality", &self.quality)?;
        state.serialize_field("cost", &self.cost)?;
        state.serialize_field("latency", &self.latency)?;
        state.serialize_field("reliability", &self.reliability)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ScoreComponents {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            quality: u16,
            cost: u16,
            latency: u16,
            reliability: u16,
        }
        let raw = Raw::deserialize(deserializer)?;
        if raw.quality > SCORE_SCALE
            || raw.cost > SCORE_SCALE
            || raw.latency > SCORE_SCALE
            || raw.reliability > SCORE_SCALE
        {
            return Err(de::Error::custom("score component exceeds scale"));
        }
        Ok(Self {
            quality: raw.quality,
            cost: raw.cost,
            latency: raw.latency,
            reliability: raw.reliability,
        })
    }
}

impl Serialize for ScoreBreakdown {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("ScoreBreakdown", BREAKDOWN_FIELDS.len())?;
        state.serialize_field("schema", SCORE_BREAKDOWN_SCHEMA)?;
        state.serialize_field("schema_version", &ROUTE_POLICY_VERSION)?;
        state.serialize_field("model", &self.model)?;
        state.serialize_field("policy_version", &self.policy_version)?;
        state.serialize_field("weights", &self.weights)?;
        state.serialize_field("components", &self.components)?;
        state.serialize_field("total", &self.total)?;
        state.serialize_field("quality_prior", &self.quality_prior)?;
        state.serialize_field("reliability_prior", &self.reliability_prior)?;
        state.serialize_field("estimated_cost_usd_micros", &self.estimated_cost_usd_micros)?;
        state.serialize_field("latency_class", &self.latency_class)?;
        state.serialize_field("purpose", &self.purpose)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ScoreBreakdown {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            schema: String,
            schema_version: u16,
            model: ModelRef,
            policy_version: u16,
            weights: RouteWeightsV1,
            components: ScoreComponents,
            total: u64,
            quality_prior: u16,
            reliability_prior: u16,
            estimated_cost_usd_micros: Option<u64>,
            latency_class: LatencyClass,
            purpose: ModelPurpose,
        }
        let raw = Raw::deserialize(deserializer)?;
        if raw.schema != SCORE_BREAKDOWN_SCHEMA || raw.schema_version != ROUTE_POLICY_VERSION {
            return Err(de::Error::custom("unsupported score breakdown version"));
        }
        if raw.policy_version != ROUTE_POLICY_VERSION
            || raw.quality_prior > SCORE_SCALE
            || raw.reliability_prior > SCORE_SCALE
        {
            return Err(de::Error::custom("invalid score breakdown"));
        }
        Ok(Self {
            model: raw.model,
            policy_version: raw.policy_version,
            weights: raw.weights,
            components: raw.components,
            total: raw.total,
            quality_prior: raw.quality_prior,
            reliability_prior: raw.reliability_prior,
            estimated_cost_usd_micros: raw.estimated_cost_usd_micros,
            latency_class: raw.latency_class,
            purpose: raw.purpose,
        })
    }
}

impl Serialize for HardRejection {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("HardRejection", 2)?;
        state.serialize_field("model", self.model())?;
        state.serialize_field("reason", self.reason().as_str())?;
        state.end()
    }
}

impl Serialize for RouteDecision {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("RouteDecision", DECISION_FIELDS.len())?;
        state.serialize_field("schema", ROUTE_DECISION_SCHEMA)?;
        state.serialize_field("schema_version", &ROUTE_POLICY_VERSION)?;
        state.serialize_field("model", &self.model)?;
        state.serialize_field("policy_version", &self.policy_version)?;
        state.serialize_field("selected", &self.selected)?;
        state.serialize_field("candidate_scores", &self.candidate_scores)?;
        state.serialize_field("hard_rejections", &self.hard_rejections)?;
        state.serialize_field("fallback_chain", &self.fallback_chain)?;
        state.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{
        CatalogConfig, CatalogModelSpec, ModelCatalog, ProviderCapEntry, ProviderCapIndex,
    };
    use crate::provider::{
        CatalogRevision, DataPolicyTag, ModelCapabilities, ModelId, ModelPrices, PriceTableVersion,
        PrivacyClass, ProviderCapabilities, ProviderId, ReasoningSupport, Region, UsageFieldSet,
    };
    use crate::route::filter::{LOCAL_ONLY_TAG, NO_TRAINING_TAG, eligible};

    const GOLDEN_TRACE: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ab";

    const GOLDEN_BREAKDOWN: &str = concat!(
        r#"{"schema":"rapidlm.score_breakdown","schema_version":1,"#,
        r#""model":{"provider":"openai","model":"gpt-4.1"},"policy_version":1,"#,
        r#""weights":{"quality":4000,"cost":2500,"latency":2000,"reliability":1500},"#,
        r#""components":{"quality":8000,"cost":8333,"latency":10000,"reliability":5000},"#,
        r#""total":80332500,"quality_prior":8000,"reliability_prior":5000,"#,
        r#""estimated_cost_usd_micros":200000,"latency_class":"interactive","purpose":"code"}"#
    );

    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    fn caps(context_limit: u32, max_output: u32) -> ProviderCapabilities {
        ProviderCapabilities::new(
            true,
            true,
            true,
            true,
            ReasoningSupport::Exposed,
            true,
            context_limit,
            max_output,
            UsageFieldSet::new(true, true, true, true, true, false, false),
        )
        .expect("caps")
    }

    fn prices(input: Option<u64>, output: Option<u64>) -> ModelPrices {
        ModelPrices::new(
            input,
            output,
            input.map(|v| v / 4),
            Some(PriceTableVersion::parse("openai-2026-04").expect("price table")),
        )
    }

    fn spec(
        provider: &str,
        model: &str,
        capabilities: ProviderCapabilities,
        prices: ModelPrices,
        latency: LatencyClass,
        tags: &[&str],
    ) -> CatalogModelSpec {
        CatalogModelSpec::new(
            ProviderId::parse(provider).expect("provider"),
            ModelId::parse(model).expect("model"),
            true,
            capabilities,
            prices,
            vec![Region::parse("us").expect("region")],
            tags.iter()
                .map(|tag| DataPolicyTag::parse(*tag).expect("tag"))
                .collect(),
            latency,
        )
    }

    fn catalog(rows: Vec<CatalogModelSpec>) -> crate::catalog::CatalogSnapshot {
        let mut index = ProviderCapIndex::new();
        for row in &rows {
            if index.get(row.provider()).is_none() {
                index
                    .insert(
                        row.provider().clone(),
                        ProviderCapEntry::Available(row.capabilities().clone()),
                    )
                    .expect("provider cap");
            }
        }
        let config =
            CatalogConfig::new(CatalogRevision::new(3).expect("rev"), rows).expect("config");
        ModelCatalog::build(&config, &index, &live())
            .expect("catalog")
            .snapshot()
            .clone()
    }

    fn request(
        purpose: ModelPurpose,
        input_tokens: u32,
        output_reserve: u32,
        latency_slo_ms: Option<u64>,
        budget_remaining_usd_micros: Option<u64>,
        user_pin: Option<ModelRef>,
    ) -> RouteRequest {
        RouteRequest::new(
            purpose,
            PrivacyClass::Unrestricted,
            None,
            0,
            input_tokens,
            output_reserve,
            ModelCapabilities::NONE,
            latency_slo_ms,
            budget_remaining_usd_micros,
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

    fn named<'a>(
        snapshot: &'a crate::catalog::CatalogSnapshot,
        provider: &str,
        model: &str,
    ) -> &'a ModelDescriptor {
        snapshot
            .get(&pin(provider, model))
            .expect("named")
            .descriptor()
    }

    fn two_model_catalog() -> crate::catalog::CatalogSnapshot {
        let advertised = caps(128_000, 8192);
        catalog(vec![
            spec(
                "openai",
                "gpt-4.1",
                advertised.clone(),
                prices(Some(2_000_000), Some(8_000_000)),
                LatencyClass::Interactive,
                &[NO_TRAINING_TAG],
            ),
            spec(
                "anthropic",
                "claude-sonnet",
                advertised,
                prices(Some(3_000_000), Some(15_000_000)),
                LatencyClass::Standard,
                &[NO_TRAINING_TAG],
            ),
        ])
    }

    fn policy_with_priors(
        weights: RouteWeightsV1,
        quality: &[(&str, &str, ModelPurpose, u16)],
        reliability: &[(&str, &str, u16)],
    ) -> RoutePolicyV1 {
        RoutePolicyV1::new(
            weights,
            ScoreDefaultsV1::standard(),
            quality
                .iter()
                .map(|(provider, model, purpose, score)| {
                    QualityPrior::new(pin(provider, model), *purpose, *score).expect("q")
                })
                .collect(),
            reliability
                .iter()
                .map(|(provider, model, score)| {
                    ReliabilityPrior::new(pin(provider, model), *score).expect("r")
                })
                .collect(),
        )
        .expect("policy")
    }

    #[test]
    fn score_is_deterministic_and_inspectable() {
        let snapshot = two_model_catalog();
        let model = named(&snapshot, "openai", "gpt-4.1");
        let req = request(ModelPurpose::Code, 50_000, 12_500, None, None, None);
        let policy = policy_with_priors(
            RouteWeightsV1::standard(),
            &[("openai", "gpt-4.1", ModelPurpose::Code, 8_000)],
            &[],
        );
        let first = score(model, &req, &policy);
        let second = score(model, &req, &policy);
        assert_eq!(first, second);
        assert_eq!(first.policy_version(), ROUTE_POLICY_VERSION);
        assert_eq!(first.components().quality(), 8_000);
        assert_eq!(first.quality_prior(), 8_000);
        assert_eq!(first.reliability_prior(), 5_000);
        assert_eq!(first.estimated_cost_usd_micros(), Some(200_000));
        assert_eq!(first.latency_class(), LatencyClass::Interactive);
        assert_eq!(first.purpose(), ModelPurpose::Code);
        assert_eq!(first.total(), 80_332_500);
        assert_eq!(
            serde_json::to_string(&first).expect("json"),
            GOLDEN_BREAKDOWN
        );
        let decoded: ScoreBreakdown = serde_json::from_str(GOLDEN_BREAKDOWN).expect("decode");
        assert_eq!(decoded, first);
    }

    #[test]
    fn purpose_quality_prior_changes_score() {
        let snapshot = two_model_catalog();
        let model = named(&snapshot, "openai", "gpt-4.1");
        let policy = policy_with_priors(
            RouteWeightsV1::standard(),
            &[
                ("openai", "gpt-4.1", ModelPurpose::Code, 9_000),
                ("openai", "gpt-4.1", ModelPurpose::Chat, 1_000),
            ],
            &[],
        );
        let code = score(
            model,
            &request(ModelPurpose::Code, 1_000, 100, None, None, None),
            &policy,
        );
        let chat = score(
            model,
            &request(ModelPurpose::Chat, 1_000, 100, None, None, None),
            &policy,
        );
        assert!(code.total() > chat.total());
        assert_eq!(code.components().quality(), 9_000);
        assert_eq!(chat.components().quality(), 1_000);
    }

    #[test]
    fn versioned_weights_change_ranking() {
        let snapshot = two_model_catalog();
        let req = request(ModelPurpose::Code, 4_000, 1_000, None, None, None);
        let set = eligible(&req, &snapshot, &live()).expect("eligible");
        let quality_heavy = policy_with_priors(
            RouteWeightsV1::new(10_000, 0, 0, 0).expect("w"),
            &[
                ("openai", "gpt-4.1", ModelPurpose::Code, 9_000),
                ("anthropic", "claude-sonnet", ModelPurpose::Code, 4_000),
            ],
            &[],
        );
        let cost_heavy = policy_with_priors(
            RouteWeightsV1::new(0, 10_000, 0, 0).expect("w"),
            &[
                ("openai", "gpt-4.1", ModelPurpose::Code, 9_000),
                ("anthropic", "claude-sonnet", ModelPurpose::Code, 4_000),
            ],
            &[],
        );
        let quality_pick = select(&req, &set, &quality_heavy, &live()).expect("quality");
        let cost_pick = select(&req, &set, &cost_heavy, &live()).expect("cost");
        assert_eq!(quality_pick.model().model().as_str(), "gpt-4.1");
        assert_eq!(cost_pick.model().model().as_str(), "gpt-4.1");
        assert!(
            quality_pick.selected().components().quality()
                > cost_pick
                    .candidate_scores()
                    .iter()
                    .find(|row| row.model().model().as_str() == "claude-sonnet")
                    .expect("sonnet")
                    .components()
                    .quality()
        );
        let cheap_only = catalog(vec![
            spec(
                "openai",
                "pricey",
                caps(128_000, 8192),
                prices(Some(50_000_000), Some(50_000_000)),
                LatencyClass::Interactive,
                &[],
            ),
            spec(
                "local",
                "cheap",
                caps(128_000, 8192),
                prices(Some(100_000), Some(100_000)),
                LatencyClass::Interactive,
                &[LOCAL_ONLY_TAG],
            ),
        ]);
        let set = eligible(&req, &cheap_only, &live()).expect("eligible");
        let quality_heavy = policy_with_priors(
            RouteWeightsV1::new(10_000, 0, 0, 0).expect("w"),
            &[
                ("openai", "pricey", ModelPurpose::Code, 10_000),
                ("local", "cheap", ModelPurpose::Code, 1_000),
            ],
            &[],
        );
        let cost_heavy = policy_with_priors(
            RouteWeightsV1::new(0, 10_000, 0, 0).expect("w"),
            &[
                ("openai", "pricey", ModelPurpose::Code, 10_000),
                ("local", "cheap", ModelPurpose::Code, 1_000),
            ],
            &[],
        );
        assert_eq!(
            select(&req, &set, &quality_heavy, &live())
                .expect("q")
                .model()
                .model()
                .as_str(),
            "pricey"
        );
        assert_eq!(
            select(&req, &set, &cost_heavy, &live())
                .expect("c")
                .model()
                .model()
                .as_str(),
            "cheap"
        );
    }

    #[test]
    fn latency_and_reliability_affect_score() {
        let advertised = caps(8_192, 1024);
        let snapshot = catalog(vec![
            spec(
                "openai",
                "fast",
                advertised.clone(),
                prices(Some(1_000_000), Some(1_000_000)),
                LatencyClass::Interactive,
                &[],
            ),
            spec(
                "openai",
                "batch",
                advertised,
                prices(Some(1_000_000), Some(1_000_000)),
                LatencyClass::Batch,
                &[],
            ),
        ]);
        let req = request(ModelPurpose::Chat, 1_000, 100, Some(3_000), None, None);
        let set = eligible(&req, &snapshot, &live()).expect("eligible");
        let policy = policy_with_priors(RouteWeightsV1::new(0, 0, 10_000, 0).expect("w"), &[], &[]);
        let decision = select(&req, &set, &policy, &live()).expect("select");
        assert_eq!(decision.model().model().as_str(), "fast");
        assert!(
            decision.selected().components().latency()
                > decision.candidate_scores()[1].components().latency()
        );

        let reliability = policy_with_priors(
            RouteWeightsV1::new(0, 0, 0, 10_000).expect("w"),
            &[],
            &[("openai", "fast", 1_000), ("openai", "batch", 9_000)],
        );
        let decision = select(&req, &set, &reliability, &live()).expect("rel");
        assert_eq!(decision.model().model().as_str(), "batch");
        assert_eq!(decision.selected().reliability_prior(), 9_000);
    }

    #[test]
    fn unknown_cost_is_never_treated_as_free() {
        let advertised = caps(8_192, 1024);
        let snapshot = catalog(vec![
            spec(
                "openai",
                "priced",
                advertised.clone(),
                prices(Some(2_000_000), Some(2_000_000)),
                LatencyClass::Standard,
                &[],
            ),
            spec(
                "openai",
                "unknown",
                advertised,
                ModelPrices::UNKNOWN,
                LatencyClass::Standard,
                &[],
            ),
        ]);
        let req = request(ModelPurpose::Chat, 1_000, 100, None, None, None);
        let policy = policy_with_priors(RouteWeightsV1::new(0, 10_000, 0, 0).expect("w"), &[], &[]);
        let set = eligible(&req, &snapshot, &live()).expect("eligible");
        let decision = select(&req, &set, &policy, &live()).expect("select");
        assert_eq!(decision.model().model().as_str(), "priced");
        let unknown = decision
            .candidate_scores()
            .iter()
            .find(|row| row.model().model().as_str() == "unknown")
            .expect("unknown");
        assert_eq!(unknown.estimated_cost_usd_micros(), None);
        assert_eq!(unknown.components().cost(), 0);
        assert!(decision.selected().components().cost() > 0);
    }

    #[test]
    fn over_budget_cost_component_is_zero() {
        let snapshot = two_model_catalog();
        let model = named(&snapshot, "openai", "gpt-4.1");
        let policy = RoutePolicyV1::standard();
        let over = request(ModelPurpose::Code, 50_000, 12_500, None, Some(1_000), None);
        let under = request(
            ModelPurpose::Code,
            50_000,
            12_500,
            None,
            Some(1_000_000),
            None,
        );
        assert_eq!(score(model, &over, &policy).components().cost(), 0);
        assert!(score(model, &under, &policy).components().cost() > 0);
        assert_eq!(
            score(model, &over, &policy).estimated_cost_usd_micros(),
            Some(200_000)
        );
    }

    #[test]
    fn tie_break_is_deterministic() {
        let advertised = caps(8_192, 1024);
        let snapshot = catalog(vec![
            spec(
                "zeta",
                "z-model",
                advertised.clone(),
                prices(Some(1_000_000), Some(1_000_000)),
                LatencyClass::Standard,
                &[],
            ),
            spec(
                "alpha",
                "a-model",
                advertised,
                prices(Some(1_000_000), Some(1_000_000)),
                LatencyClass::Standard,
                &[],
            ),
        ]);
        let req = request(ModelPurpose::Chat, 1_000, 100, None, None, None);
        let set = eligible(&req, &snapshot, &live()).expect("eligible");
        let policy = RoutePolicyV1::standard();
        let first = select(&req, &set, &policy, &live()).expect("a");
        let second = select(&req, &set, &policy, &live()).expect("b");
        assert_eq!(first, second);
        assert_eq!(first.model().provider().as_str(), "alpha");
        assert_eq!(first.candidate_scores().len(), 2);
        assert_eq!(
            first.candidate_scores()[0].total_cmp(&first.candidate_scores()[1]),
            Ordering::Greater
        );
        assert_eq!(
            first.candidate_scores()[0].total_cmp(&first.candidate_scores()[0]),
            Ordering::Equal
        );
    }

    #[test]
    fn select_records_policy_version_and_components() {
        let snapshot = two_model_catalog();
        let req = request(ModelPurpose::Code, 1_000, 100, None, None, None);
        let set = eligible(&req, &snapshot, &live()).expect("eligible");
        let policy = policy_with_priors(
            RouteWeightsV1::standard(),
            &[("openai", "gpt-4.1", ModelPurpose::Code, 8_000)],
            &[("openai", "gpt-4.1", 7_000)],
        );
        let decision = select(&req, &set, &policy, &live()).expect("select");
        assert_eq!(decision.policy_version(), ROUTE_POLICY_VERSION);
        assert_eq!(decision.selected().policy_version(), ROUTE_POLICY_VERSION);
        assert_eq!(decision.selected().weights(), policy.weights());
        assert_eq!(decision.selected().components().quality(), 8_000);
        assert_eq!(decision.selected().components().reliability(), 7_000);
        assert_eq!(decision.candidate_scores().len(), 2);
        assert!(decision.fallback_chain().is_empty());
        assert_eq!(decision.hard_rejections(), set.rejections());
        for row in decision.candidate_scores() {
            assert_eq!(row.policy_version(), ROUTE_POLICY_VERSION);
            assert!(row.components().quality() <= SCORE_SCALE);
            assert!(row.components().cost() <= SCORE_SCALE);
            assert!(row.components().latency() <= SCORE_SCALE);
            assert!(row.components().reliability() <= SCORE_SCALE);
        }
        let json = serde_json::to_string(&decision).expect("decision json");
        assert!(json.contains("\"policy_version\":1"));
        assert!(json.contains("\"components\""));
        assert!(!json.contains("sk-"));
    }

    #[test]
    fn pin_is_still_scored() {
        let snapshot = two_model_catalog();
        let req = request(
            ModelPurpose::Code,
            1_000,
            100,
            None,
            None,
            Some(pin("anthropic", "claude-sonnet")),
        );
        let set = eligible(&req, &snapshot, &live()).expect("pin");
        let decision = select(&req, &set, &RoutePolicyV1::standard(), &live()).expect("select");
        assert_eq!(decision.model().model().as_str(), "claude-sonnet");
        assert_eq!(decision.candidate_scores().len(), 1);
        assert!(!set.rejections().is_empty());
        assert_eq!(decision.hard_rejections().len(), set.rejections().len());
    }

    #[test]
    fn cancellation_and_empty_set_are_typed() {
        let snapshot = two_model_catalog();
        let req = request(ModelPurpose::Chat, 1, 1, None, None, None);
        let set = eligible(&req, &snapshot, &live()).expect("eligible");
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            select(&req, &set, &RoutePolicyV1::standard(), &cancel),
            Err(RouteScoreError::Cancelled)
        );
        assert!(
            RouteScoreError::Cancelled
                .into_api_error(GOLDEN_TRACE.parse().expect("trace"))
                .is_none()
        );
    }

    #[test]
    fn invalid_policy_is_rejected() {
        assert_eq!(
            RouteWeightsV1::new(0, 0, 0, 0),
            Err(RouteScoreError::InvalidPolicy)
        );
        assert_eq!(
            QualityPrior::new(
                pin("openai", "gpt-4.1"),
                ModelPurpose::Code,
                SCORE_SCALE + 1
            ),
            Err(RouteScoreError::InvalidPolicy)
        );
        assert_eq!(
            LatencyBudgetsV1::new(10_000, 1_000, 1_000),
            Err(RouteScoreError::InvalidPolicy)
        );
        let dup = RoutePolicyV1::new(
            RouteWeightsV1::standard(),
            ScoreDefaultsV1::standard(),
            vec![
                QualityPrior::new(pin("openai", "gpt-4.1"), ModelPurpose::Code, 1).expect("a"),
                QualityPrior::new(pin("openai", "gpt-4.1"), ModelPurpose::Code, 2).expect("b"),
            ],
            vec![],
        );
        assert_eq!(dup, Err(RouteScoreError::InvalidPolicy));
    }

    #[test]
    fn display_and_api_errors_do_not_echo_secrets() {
        let leak = "sk-ant-api03-not-a-real-secret";
        let err = RouteScoreError::NoEligibleModel;
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
    }

    #[test]
    fn policy_serialization_round_trips() {
        let policy = policy_with_priors(
            RouteWeightsV1::standard(),
            &[("openai", "gpt-4.1", ModelPurpose::Code, 8_000)],
            &[("openai", "gpt-4.1", 7_000)],
        );
        let json = serde_json::to_string(&policy).expect("json");
        let decoded: RoutePolicyV1 = serde_json::from_str(&json).expect("decode");
        assert_eq!(decoded, policy);
        assert!(json.contains("\"schema\":\"rapidlm.route_policy\""));
        assert!(json.contains("\"schema_version\":1"));
    }
}
