//! Safe retry/fallback state machine for provider invocations.
//!
//! Retry and fallback are allowed only for transient infrastructure
//! failures, only before any stream or tool side effect, and only onto
//! models that already passed hard route filters. Auth, config, and
//! safety failures stop unless policy names an explicit eligible
//! alternate.

use std::error::Error;
use std::fmt;

use protocol::{ApiError, ErrorCode, TraceId};
use serde::de::{self, Deserializer};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};

use crate::provider::{CancellationToken, ModelRef, ProviderError};
use crate::route::filter::RejectionReason;
use crate::route::score::RouteDecision;

/// Wire schema name for [`FallbackPolicy`].
pub const FALLBACK_POLICY_SCHEMA: &str = "rapidlm.fallback_policy";

/// Wire schema name for [`FallbackPlan`].
pub const FALLBACK_PLAN_SCHEMA: &str = "rapidlm.fallback_plan";

/// v1 fallback policy / plan schema version.
pub const FALLBACK_POLICY_VERSION: u16 = 1;

/// Inclusive maximum same-model retries a policy may request.
pub const MAX_SAME_MODEL_RETRIES: u8 = 4;

/// Inclusive maximum models visited in one chain, including the primary.
pub const MAX_FALLBACK_MODELS: usize = 8;

/// Inclusive maximum explicit auth/config/safety alternates.
pub const MAX_EXPLICIT_ALTERNATES: usize = 8;

/// Default same-model retries after the first attempt.
pub const DEFAULT_SAME_MODEL_RETRIES: u8 = 2;

/// Default backoff base used by [`FallbackPolicy::standard`].
pub const DEFAULT_BACKOFF_BASE_MS: u64 = 200;

/// Default backoff cap used by [`FallbackPolicy::standard`].
pub const DEFAULT_BACKOFF_CAP_MS: u64 = 5_000;

const POLICY_FIELDS: &[&str] = &[
    "schema",
    "schema_version",
    "max_same_model_retries",
    "max_models",
    "backoff_base_ms",
    "backoff_cap_ms",
    "explicit_alternates",
];

/// How far the current provider attempt progressed before failing.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum AttemptProgress {
    /// Request failed before any stream event. Retry/fallback may be safe.
    PreResponse,
    /// Text or usage already streamed; automatic retry is refused.
    PartiallyStreamed,
    /// A tool call was observed; a side effect may already have occurred.
    ToolSideEffect,
}

/// Classified failure that the controller is allowed to see.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum FailureClass {
    Transient,
    RateLimited { retry_after_ms: Option<u64> },
    Auth,
    Config,
    Safety,
    ContextTooLarge,
    Cancelled,
    Permanent,
}

/// Input that produced a [`FallbackPlan`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FallbackTrigger {
    Provider(ProviderError),
    Safety,
    Config,
}

/// Why the controller stopped instead of retrying or falling back.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum StopReason {
    AuthFailure,
    ConfigFailure,
    SafetyFailure,
    ContextTooLarge,
    PermanentFailure,
    Cancelled,
    SideEffectRisk,
    PartialStreamUnsafe,
    RetryBudgetExhausted,
    FallbackChainExhausted,
    UserPinForbidsFallback,
    HardConstraintViolation,
}

/// Allowed next step after a pre-response failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FallbackAction {
    RetrySame {
        model: ModelRef,
        attempt: u8,
        backoff_ms: u64,
    },
    FallbackTo {
        from: ModelRef,
        to: ModelRef,
        backoff_ms: u64,
    },
    Stop {
        model: ModelRef,
        reason: StopReason,
    },
}

/// Distinguishes a pre-response safe retry from streamed/tool state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FallbackPlan {
    /// No stream or tool event observed. Retry or fallback may proceed.
    PreResponse {
        action: FallbackAction,
        failure: FailureClass,
    },
    /// Partial model output already left the provider.
    PartiallyStreamed {
        model: ModelRef,
        failure: FailureClass,
        reason: StopReason,
    },
    /// A tool call may already have executed. Never auto-replayed.
    ToolSideEffect {
        model: ModelRef,
        failure: FailureClass,
        reason: StopReason,
    },
}

/// Versioned retry/fallback bounds. Auth/config/safety alternates are explicit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FallbackPolicy {
    max_same_model_retries: u8,
    max_models: usize,
    backoff_base_ms: u64,
    backoff_cap_ms: u64,
    explicit_alternates: Vec<ModelRef>,
}

/// Retry/fallback state machine bound to one [`RouteDecision`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FallbackController {
    policy: FallbackPolicy,
    policy_version: u16,
    chain: Vec<ModelRef>,
    index: usize,
    same_model_retries: u8,
    pin_forbids_fallback: bool,
    terminal: Option<FallbackPlan>,
}

/// Typed fallback failure. Display never echoes provider bodies or secrets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FallbackError {
    Cancelled,
    InvalidPolicy,
    BoundExceeded,
    NoEligibleModel,
    HardConstraintViolation,
    StalePlan,
}

impl AttemptProgress {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PreResponse => "pre_response",
            Self::PartiallyStreamed => "partially_streamed",
            Self::ToolSideEffect => "tool_side_effect",
        }
    }

    /// Whether automatic retry or fallback is even consider-able.
    pub const fn allows_automatic_retry(self) -> bool {
        matches!(self, Self::PreResponse)
    }
}

impl FailureClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Transient => "transient",
            Self::RateLimited { .. } => "rate_limited",
            Self::Auth => "auth",
            Self::Config => "config",
            Self::Safety => "safety",
            Self::ContextTooLarge => "context_too_large",
            Self::Cancelled => "cancelled",
            Self::Permanent => "permanent",
        }
    }

    pub const fn is_transient(self) -> bool {
        matches!(self, Self::Transient | Self::RateLimited { .. })
    }

    /// Auth/config/safety stop unless policy names an explicit alternate.
    pub const fn requires_explicit_alternate(self) -> bool {
        matches!(self, Self::Auth | Self::Config | Self::Safety)
    }

    pub const fn retry_after_ms(self) -> Option<u64> {
        match self {
            Self::RateLimited { retry_after_ms } => retry_after_ms,
            _ => None,
        }
    }
}

impl FallbackTrigger {
    pub fn classify(&self) -> FailureClass {
        classify_failure(self)
    }
}

/// Map a trigger onto the closed failure taxonomy.
pub fn classify_failure(trigger: &FallbackTrigger) -> FailureClass {
    match trigger {
        FallbackTrigger::Safety => FailureClass::Safety,
        FallbackTrigger::Config => FailureClass::Config,
        FallbackTrigger::Provider(error) => match error {
            ProviderError::Cancelled => FailureClass::Cancelled,
            ProviderError::AuthFailed => FailureClass::Auth,
            ProviderError::RateLimited { retry_after_ms } => FailureClass::RateLimited {
                retry_after_ms: *retry_after_ms,
            },
            ProviderError::ContextTooLarge => FailureClass::ContextTooLarge,
            ProviderError::Transient => FailureClass::Transient,
            ProviderError::InvalidRequest
            | ProviderError::BoundExceeded
            | ProviderError::UnknownVariant => FailureClass::Config,
            ProviderError::Permanent => FailureClass::Permanent,
        },
    }
}

impl StopReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AuthFailure => "auth_failure",
            Self::ConfigFailure => "config_failure",
            Self::SafetyFailure => "safety_failure",
            Self::ContextTooLarge => "context_too_large",
            Self::PermanentFailure => "permanent_failure",
            Self::Cancelled => "cancelled",
            Self::SideEffectRisk => "side_effect_risk",
            Self::PartialStreamUnsafe => "partial_stream_unsafe",
            Self::RetryBudgetExhausted => "retry_budget_exhausted",
            Self::FallbackChainExhausted => "fallback_chain_exhausted",
            Self::UserPinForbidsFallback => "user_pin_forbids_fallback",
            Self::HardConstraintViolation => "hard_constraint_violation",
        }
    }
}

impl FallbackAction {
    pub fn model(&self) -> &ModelRef {
        match self {
            Self::RetrySame { model, .. } | Self::Stop { model, .. } => model,
            Self::FallbackTo { to, .. } => to,
        }
    }

    pub const fn backoff_ms(&self) -> Option<u64> {
        match self {
            Self::RetrySame { backoff_ms, .. } | Self::FallbackTo { backoff_ms, .. } => {
                Some(*backoff_ms)
            }
            Self::Stop { .. } => None,
        }
    }

    pub const fn is_stop(&self) -> bool {
        matches!(self, Self::Stop { .. })
    }
}

impl FallbackPlan {
    pub const fn progress(&self) -> AttemptProgress {
        match self {
            Self::PreResponse { .. } => AttemptProgress::PreResponse,
            Self::PartiallyStreamed { .. } => AttemptProgress::PartiallyStreamed,
            Self::ToolSideEffect { .. } => AttemptProgress::ToolSideEffect,
        }
    }

    pub const fn failure(&self) -> FailureClass {
        match self {
            Self::PreResponse { failure, .. }
            | Self::PartiallyStreamed { failure, .. }
            | Self::ToolSideEffect { failure, .. } => *failure,
        }
    }

    pub fn current_model(&self) -> &ModelRef {
        match self {
            Self::PreResponse { action, .. } => match action {
                FallbackAction::RetrySame { model, .. } | FallbackAction::Stop { model, .. } => {
                    model
                }
                FallbackAction::FallbackTo { from, .. } => from,
            },
            Self::PartiallyStreamed { model, .. } | Self::ToolSideEffect { model, .. } => model,
        }
    }

    /// Pre-response retry or fallback that the caller may execute.
    pub fn is_safe_retry(&self) -> bool {
        matches!(
            self,
            Self::PreResponse {
                action: FallbackAction::RetrySame { .. } | FallbackAction::FallbackTo { .. },
                ..
            }
        )
    }

    pub fn action(&self) -> Option<&FallbackAction> {
        match self {
            Self::PreResponse { action, .. } => Some(action),
            Self::PartiallyStreamed { .. } | Self::ToolSideEffect { .. } => None,
        }
    }

    pub fn stop_reason(&self) -> Option<StopReason> {
        match self {
            Self::PreResponse {
                action: FallbackAction::Stop { reason, .. },
                ..
            } => Some(*reason),
            Self::PartiallyStreamed { reason, .. } | Self::ToolSideEffect { reason, .. } => {
                Some(*reason)
            }
            Self::PreResponse { .. } => None,
        }
    }

    pub fn backoff_ms(&self) -> Option<u64> {
        self.action().and_then(FallbackAction::backoff_ms)
    }
}

impl FallbackPolicy {
    pub fn new(
        max_same_model_retries: u8,
        max_models: usize,
        backoff_base_ms: u64,
        backoff_cap_ms: u64,
        explicit_alternates: Vec<ModelRef>,
    ) -> Result<Self, FallbackError> {
        if max_same_model_retries > MAX_SAME_MODEL_RETRIES
            || max_models == 0
            || max_models > MAX_FALLBACK_MODELS
            || backoff_base_ms == 0
            || backoff_cap_ms < backoff_base_ms
            || explicit_alternates.len() > MAX_EXPLICIT_ALTERNATES
        {
            return Err(FallbackError::InvalidPolicy);
        }
        if has_duplicate_refs(&explicit_alternates) {
            return Err(FallbackError::InvalidPolicy);
        }
        Ok(Self {
            max_same_model_retries,
            max_models,
            backoff_base_ms,
            backoff_cap_ms,
            explicit_alternates,
        })
    }

    /// Default v1 bounds: two same-model retries, then eligible fallbacks.
    pub fn standard() -> Self {
        Self {
            max_same_model_retries: DEFAULT_SAME_MODEL_RETRIES,
            max_models: MAX_FALLBACK_MODELS,
            backoff_base_ms: DEFAULT_BACKOFF_BASE_MS,
            backoff_cap_ms: DEFAULT_BACKOFF_CAP_MS,
            explicit_alternates: Vec::new(),
        }
    }

    pub fn with_explicit_alternates(
        mut self,
        explicit_alternates: Vec<ModelRef>,
    ) -> Result<Self, FallbackError> {
        if explicit_alternates.len() > MAX_EXPLICIT_ALTERNATES
            || has_duplicate_refs(&explicit_alternates)
        {
            return Err(FallbackError::InvalidPolicy);
        }
        self.explicit_alternates = explicit_alternates;
        Ok(self)
    }

    pub const fn version(&self) -> u16 {
        FALLBACK_POLICY_VERSION
    }
    pub const fn max_same_model_retries(&self) -> u8 {
        self.max_same_model_retries
    }
    pub const fn max_models(&self) -> usize {
        self.max_models
    }
    pub const fn backoff_base_ms(&self) -> u64 {
        self.backoff_base_ms
    }
    pub const fn backoff_cap_ms(&self) -> u64 {
        self.backoff_cap_ms
    }
    pub fn explicit_alternates(&self) -> &[ModelRef] {
        &self.explicit_alternates
    }
}

impl FallbackController {
    /// Bind a controller to a scored decision. Chain members must be eligible.
    pub fn from_decision(
        decision: &RouteDecision,
        policy: FallbackPolicy,
        cancel: &CancellationToken,
    ) -> Result<Self, FallbackError> {
        cancel.check().map_err(|_| FallbackError::Cancelled)?;
        let chain = build_chain(decision, &policy)?;
        if chain.is_empty() {
            return Err(FallbackError::NoEligibleModel);
        }
        let pin_forbids_fallback = decision
            .hard_rejections()
            .iter()
            .any(|rejection| matches!(rejection.reason(), RejectionReason::UserPinExcludes));
        Ok(Self {
            policy,
            policy_version: decision.policy_version(),
            chain,
            index: 0,
            same_model_retries: 0,
            pin_forbids_fallback,
            terminal: None,
        })
    }

    pub fn policy(&self) -> &FallbackPolicy {
        &self.policy
    }
    pub const fn policy_version(&self) -> u16 {
        self.policy_version
    }
    pub fn chain(&self) -> &[ModelRef] {
        &self.chain
    }
    pub fn current(&self) -> &ModelRef {
        &self.chain[self.index]
    }
    pub const fn same_model_retries(&self) -> u8 {
        self.same_model_retries
    }
    pub fn is_terminal(&self) -> bool {
        self.terminal.is_some()
    }

    /// Peek the next action without mutating retry/fallback counters.
    pub fn plan(
        &self,
        trigger: &FallbackTrigger,
        progress: AttemptProgress,
        cancel: &CancellationToken,
    ) -> Result<FallbackPlan, FallbackError> {
        cancel.check().map_err(|_| FallbackError::Cancelled)?;
        if let Some(done) = self.terminal.as_ref() {
            return Ok(done.clone());
        }
        let failure = classify_failure(trigger);
        let current = self.current().clone();
        match progress {
            AttemptProgress::ToolSideEffect => {
                return Ok(FallbackPlan::ToolSideEffect {
                    model: current,
                    failure,
                    reason: StopReason::SideEffectRisk,
                });
            }
            AttemptProgress::PartiallyStreamed => {
                return Ok(FallbackPlan::PartiallyStreamed {
                    model: current,
                    failure,
                    reason: StopReason::PartialStreamUnsafe,
                });
            }
            AttemptProgress::PreResponse => {}
        }

        let action = self.pre_response_action(failure, current)?;
        Ok(FallbackPlan::PreResponse { action, failure })
    }

    /// Commit a plan produced by [`Self::plan`]. Safe-retry plans advance state.
    pub fn apply(&mut self, plan: &FallbackPlan) -> Result<(), FallbackError> {
        if let Some(done) = self.terminal.as_ref() {
            return if done == plan {
                Ok(())
            } else {
                Err(FallbackError::StalePlan)
            };
        }
        if plan.current_model() != self.current() {
            return Err(FallbackError::StalePlan);
        }
        match plan {
            FallbackPlan::PartiallyStreamed { .. } | FallbackPlan::ToolSideEffect { .. } => {
                self.terminal = Some(plan.clone());
                Ok(())
            }
            FallbackPlan::PreResponse { action, .. } => match action {
                FallbackAction::Stop { .. } => {
                    self.terminal = Some(plan.clone());
                    Ok(())
                }
                FallbackAction::RetrySame { model, attempt, .. } => {
                    if model != self.current()
                        || *attempt != self.same_model_retries.saturating_add(1)
                    {
                        return Err(FallbackError::StalePlan);
                    }
                    self.same_model_retries = *attempt;
                    Ok(())
                }
                FallbackAction::FallbackTo { from, to, .. } => {
                    if from != self.current() {
                        return Err(FallbackError::StalePlan);
                    }
                    let next = self
                        .chain
                        .iter()
                        .position(|model| model == to)
                        .ok_or(FallbackError::HardConstraintViolation)?;
                    if next <= self.index {
                        return Err(FallbackError::HardConstraintViolation);
                    }
                    self.index = next;
                    self.same_model_retries = 0;
                    Ok(())
                }
            },
        }
    }

    fn pre_response_action(
        &self,
        failure: FailureClass,
        current: ModelRef,
    ) -> Result<FallbackAction, FallbackError> {
        match failure {
            FailureClass::Cancelled => {
                return Ok(stop(current, StopReason::Cancelled));
            }
            FailureClass::ContextTooLarge => {
                return Ok(stop(current, StopReason::ContextTooLarge));
            }
            FailureClass::Permanent => {
                return Ok(stop(current, StopReason::PermanentFailure));
            }
            FailureClass::Auth | FailureClass::Config | FailureClass::Safety => {
                return Ok(self.explicit_or_stop(current, failure));
            }
            FailureClass::Transient | FailureClass::RateLimited { .. } => {}
        }

        if self.same_model_retries < self.policy.max_same_model_retries {
            let attempt = self.same_model_retries.saturating_add(1);
            return Ok(FallbackAction::RetrySame {
                model: current,
                attempt,
                backoff_ms: backoff_ms(&self.policy, attempt, failure.retry_after_ms()),
            });
        }
        match self.next_eligible(&current) {
            Some(to) => Ok(FallbackAction::FallbackTo {
                from: current,
                to,
                backoff_ms: backoff_ms(&self.policy, 1, failure.retry_after_ms()),
            }),
            None => Ok(stop(current, self.exhausted_reason())),
        }
    }

    fn explicit_or_stop(&self, current: ModelRef, failure: FailureClass) -> FallbackAction {
        match self.explicit_alternate(&current) {
            Some(to) => FallbackAction::FallbackTo {
                from: current,
                to,
                backoff_ms: self.policy.backoff_base_ms,
            },
            None => stop(current, stop_reason_for(failure)),
        }
    }

    fn explicit_alternate(&self, current: &ModelRef) -> Option<ModelRef> {
        self.policy.explicit_alternates.iter().find_map(|alt| {
            if alt == current {
                return None;
            }
            self.chain
                .iter()
                .enumerate()
                .find(|(index, model)| *index > self.index && *model == alt)
                .map(|(_, model)| model.clone())
        })
    }

    fn next_eligible(&self, current: &ModelRef) -> Option<ModelRef> {
        if self.pin_forbids_fallback {
            return None;
        }
        self.chain
            .iter()
            .enumerate()
            .find(|(index, model)| *index > self.index && *model != current)
            .map(|(_, model)| model.clone())
    }

    fn exhausted_reason(&self) -> StopReason {
        if self.pin_forbids_fallback {
            StopReason::UserPinForbidsFallback
        } else {
            StopReason::FallbackChainExhausted
        }
    }
}

fn stop(model: ModelRef, reason: StopReason) -> FallbackAction {
    FallbackAction::Stop { model, reason }
}

fn stop_reason_for(failure: FailureClass) -> StopReason {
    match failure {
        FailureClass::Auth => StopReason::AuthFailure,
        FailureClass::Config => StopReason::ConfigFailure,
        FailureClass::Safety => StopReason::SafetyFailure,
        FailureClass::ContextTooLarge => StopReason::ContextTooLarge,
        FailureClass::Cancelled => StopReason::Cancelled,
        FailureClass::Permanent => StopReason::PermanentFailure,
        FailureClass::Transient | FailureClass::RateLimited { .. } => {
            StopReason::RetryBudgetExhausted
        }
    }
}

fn backoff_ms(policy: &FallbackPolicy, attempt: u8, retry_after_ms: Option<u64>) -> u64 {
    let shift = u32::from(attempt.saturating_sub(1).min(16));
    let factor = 1u64 << shift;
    let exponential = policy
        .backoff_base_ms
        .saturating_mul(factor)
        .min(policy.backoff_cap_ms);
    match retry_after_ms {
        Some(after) => exponential.max(after),
        None => exponential,
    }
}

fn build_chain(
    decision: &RouteDecision,
    policy: &FallbackPolicy,
) -> Result<Vec<ModelRef>, FallbackError> {
    let eligible: Vec<&ModelRef> = decision
        .candidate_scores()
        .iter()
        .map(|row| row.model())
        .collect();
    if eligible.is_empty() {
        return Err(FallbackError::NoEligibleModel);
    }
    let selected = decision.model();
    if !eligible.iter().any(|model| *model == selected) {
        return Err(FallbackError::HardConstraintViolation);
    }

    let mut chain = Vec::with_capacity(policy.max_models.min(eligible.len()));
    push_chain_member(&mut chain, selected.clone(), policy.max_models)?;

    let extras: Vec<ModelRef> = if decision.fallback_chain().is_empty() {
        eligible
            .iter()
            .filter(|model| **model != selected)
            .map(|model| (*model).clone())
            .collect()
    } else {
        decision.fallback_chain().to_vec()
    };

    for model in extras {
        if chain.len() >= policy.max_models {
            break;
        }
        if !is_eligible_candidate(decision, &model) {
            continue;
        }
        if chain.iter().any(|listed| listed == &model) {
            continue;
        }
        chain.push(model);
    }
    Ok(chain)
}

fn is_eligible_candidate(decision: &RouteDecision, model: &ModelRef) -> bool {
    decision
        .candidate_scores()
        .iter()
        .any(|row| row.model() == model)
        && decision
            .hard_rejections()
            .iter()
            .all(|rejection| rejection.model() != model)
}

fn push_chain_member(
    chain: &mut Vec<ModelRef>,
    model: ModelRef,
    max_models: usize,
) -> Result<(), FallbackError> {
    if chain.len() >= max_models {
        return Err(FallbackError::BoundExceeded);
    }
    chain.push(model);
    Ok(())
}

fn has_duplicate_refs(models: &[ModelRef]) -> bool {
    models.iter().enumerate().any(|(index, model)| {
        models
            .iter()
            .skip(index.saturating_add(1))
            .any(|other| other == model)
    })
}

impl FallbackError {
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Cancelled => None,
            Self::InvalidPolicy | Self::BoundExceeded => Some(ErrorCode::ConfigInvalid),
            Self::NoEligibleModel | Self::HardConstraintViolation => Some(ErrorCode::PolicyDenied),
            Self::StalePlan => Some(ErrorCode::InternalUnexpected),
        }
    }

    pub fn into_api_error(&self, trace_id: TraceId) -> Option<ApiError> {
        let code = self.code()?;
        let message = match self {
            Self::Cancelled => return None,
            Self::InvalidPolicy => "Fallback policy is invalid",
            Self::BoundExceeded => "Fallback controller exceeds a documented bound",
            Self::NoEligibleModel => "No eligible model for fallback",
            Self::HardConstraintViolation => "Fallback would violate a hard route constraint",
            Self::StalePlan => "Fallback plan does not match controller state",
        };
        Some(
            ApiError::new(code, message, trace_id)
                .unwrap_or_else(|_| ApiError::from_unknown(trace_id, self)),
        )
    }
}

impl fmt::Display for AttemptProgress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for FailureClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for StopReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for FallbackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cancelled => "fallback cancelled",
            Self::InvalidPolicy => "fallback policy is invalid",
            Self::BoundExceeded => "fallback controller exceeds a documented bound",
            Self::NoEligibleModel => "no eligible model for fallback",
            Self::HardConstraintViolation => "fallback would violate a hard route constraint",
            Self::StalePlan => "fallback plan does not match controller state",
        })
    }
}

impl Error for FallbackError {}

impl Serialize for FallbackPolicy {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("FallbackPolicy", POLICY_FIELDS.len())?;
        state.serialize_field("schema", FALLBACK_POLICY_SCHEMA)?;
        state.serialize_field("schema_version", &FALLBACK_POLICY_VERSION)?;
        state.serialize_field("max_same_model_retries", &self.max_same_model_retries)?;
        state.serialize_field("max_models", &self.max_models)?;
        state.serialize_field("backoff_base_ms", &self.backoff_base_ms)?;
        state.serialize_field("backoff_cap_ms", &self.backoff_cap_ms)?;
        state.serialize_field("explicit_alternates", &self.explicit_alternates)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for FallbackPolicy {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            schema: String,
            schema_version: u16,
            max_same_model_retries: u8,
            max_models: usize,
            backoff_base_ms: u64,
            backoff_cap_ms: u64,
            explicit_alternates: Vec<ModelRef>,
        }
        let raw = Raw::deserialize(deserializer)?;
        if raw.schema != FALLBACK_POLICY_SCHEMA || raw.schema_version != FALLBACK_POLICY_VERSION {
            return Err(de::Error::custom("unsupported fallback policy version"));
        }
        Self::new(
            raw.max_same_model_retries,
            raw.max_models,
            raw.backoff_base_ms,
            raw.backoff_cap_ms,
            raw.explicit_alternates,
        )
        .map_err(|_| de::Error::custom("invalid fallback policy"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{
        CatalogConfig, CatalogModelSpec, ModelCatalog, ProviderCapEntry, ProviderCapIndex,
    };
    use crate::provider::{
        CatalogRevision, DataPolicyTag, LatencyClass, ModelCapabilities, ModelId, ModelPrices,
        ModelPurpose, PriceTableVersion, PrivacyClass, ProviderCapabilities, ProviderId,
        ReasoningSupport, Region, UsageFieldSet,
    };
    use crate::route::filter::{eligible, RouteRequest, LOCAL_ONLY_TAG, NO_TRAINING_TAG};
    use crate::route::score::{
        select, QualityPrior, ReliabilityPrior, RoutePolicyV1, RouteWeightsV1, ScoreDefaultsV1,
    };

    const GOLDEN_TRACE: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ab";

    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    fn caps(context_limit: u32, max_output: u32, tools: bool) -> ProviderCapabilities {
        ProviderCapabilities::new(
            tools,
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
        tags: &[&str],
        regions: &[&str],
    ) -> CatalogModelSpec {
        CatalogModelSpec::new(
            ProviderId::parse(provider).expect("provider"),
            ModelId::parse(model).expect("model"),
            true,
            capabilities,
            prices,
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
        privacy: PrivacyClass,
        region: Option<&str>,
        required: ModelCapabilities,
        user_pin: Option<ModelRef>,
    ) -> RouteRequest {
        RouteRequest::new(
            ModelPurpose::Code,
            privacy,
            region.map(|raw| Region::parse(raw).expect("region")),
            0,
            1_000,
            100,
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

    fn two_model_catalog() -> crate::catalog::CatalogSnapshot {
        let advertised = caps(128_000, 8192, true);
        catalog(vec![
            spec(
                "openai",
                "gpt-4.1",
                advertised.clone(),
                prices(Some(2_000_000), Some(8_000_000)),
                &[NO_TRAINING_TAG],
                &["us"],
            ),
            spec(
                "anthropic",
                "claude-sonnet",
                advertised,
                prices(Some(3_000_000), Some(15_000_000)),
                &[NO_TRAINING_TAG],
                &["us"],
            ),
        ])
    }

    fn ranked_decision() -> RouteDecision {
        let snapshot = two_model_catalog();
        let req = request(
            PrivacyClass::Unrestricted,
            None,
            ModelCapabilities::NONE,
            None,
        );
        let set = eligible(&req, &snapshot, &live()).expect("eligible");
        let policy = RoutePolicyV1::new(
            RouteWeightsV1::standard(),
            ScoreDefaultsV1::standard(),
            vec![
                QualityPrior::new(pin("openai", "gpt-4.1"), ModelPurpose::Code, 9_000).expect("q"),
            ],
            vec![ReliabilityPrior::new(pin("openai", "gpt-4.1"), 8_000).expect("r")],
        )
        .expect("policy");
        select(&req, &set, &policy, &live()).expect("select")
    }

    fn controller_from(decision: &RouteDecision, policy: FallbackPolicy) -> FallbackController {
        FallbackController::from_decision(decision, policy, &live()).expect("controller")
    }

    fn transient() -> FallbackTrigger {
        FallbackTrigger::Provider(ProviderError::Transient)
    }

    fn rate_limited(retry_after_ms: Option<u64>) -> FallbackTrigger {
        FallbackTrigger::Provider(ProviderError::RateLimited { retry_after_ms })
    }

    fn auth() -> FallbackTrigger {
        FallbackTrigger::Provider(ProviderError::AuthFailed)
    }

    #[test]
    fn classify_maps_provider_errors() {
        assert_eq!(classify_failure(&transient()), FailureClass::Transient);
        assert!(classify_failure(&rate_limited(Some(250))).is_transient());
        assert_eq!(classify_failure(&auth()), FailureClass::Auth);
        assert_eq!(
            classify_failure(&FallbackTrigger::Provider(ProviderError::InvalidRequest)),
            FailureClass::Config
        );
        assert_eq!(
            classify_failure(&FallbackTrigger::Safety),
            FailureClass::Safety
        );
        assert!(!ProviderError::AuthFailed.is_retryable());
        assert!(ProviderError::Transient.is_retryable());
    }

    #[test]
    fn pre_response_transient_retries_same_model_with_bounded_backoff() {
        let decision = ranked_decision();
        let policy = FallbackPolicy::new(2, 8, 200, 5_000, Vec::new()).expect("policy");
        let mut controller = controller_from(&decision, policy);
        assert_eq!(controller.current().model().as_str(), "gpt-4.1");
        assert_eq!(controller.chain().len(), 2);

        let first = controller
            .plan(&transient(), AttemptProgress::PreResponse, &live())
            .expect("plan");
        assert!(first.is_safe_retry());
        assert_eq!(first.progress(), AttemptProgress::PreResponse);
        match first.action() {
            Some(FallbackAction::RetrySame {
                model,
                attempt,
                backoff_ms,
            }) => {
                assert_eq!(model.model().as_str(), "gpt-4.1");
                assert_eq!(*attempt, 1);
                assert_eq!(*backoff_ms, 200);
            }
            other => panic!("expected retry {other:?}"),
        }
        controller.apply(&first).expect("apply");

        let second = controller
            .plan(&transient(), AttemptProgress::PreResponse, &live())
            .expect("plan");
        match second.action() {
            Some(FallbackAction::RetrySame {
                attempt,
                backoff_ms,
                ..
            }) => {
                assert_eq!(*attempt, 2);
                assert_eq!(*backoff_ms, 400);
            }
            other => panic!("expected second retry {other:?}"),
        }
        controller.apply(&second).expect("apply");
    }

    #[test]
    fn rate_limit_honors_retry_after_and_cap() {
        let decision = ranked_decision();
        let policy = FallbackPolicy::new(2, 8, 200, 500, Vec::new()).expect("policy");
        let controller = controller_from(&decision, policy);
        let plan = controller
            .plan(
                &rate_limited(Some(1_500)),
                AttemptProgress::PreResponse,
                &live(),
            )
            .expect("plan");
        assert_eq!(plan.backoff_ms(), Some(1_500));
        let capped = controller
            .plan(&transient(), AttemptProgress::PreResponse, &live())
            .expect("plan");
        assert_eq!(capped.backoff_ms(), Some(200));
    }

    #[test]
    fn exhausted_retries_fall_back_to_next_eligible_model() {
        let decision = ranked_decision();
        let policy = FallbackPolicy::new(1, 8, 200, 5_000, Vec::new()).expect("policy");
        let mut controller = controller_from(&decision, policy);
        let retry = controller
            .plan(&transient(), AttemptProgress::PreResponse, &live())
            .expect("retry");
        controller.apply(&retry).expect("apply retry");
        let fallback = controller
            .plan(&transient(), AttemptProgress::PreResponse, &live())
            .expect("fallback");
        assert!(fallback.is_safe_retry());
        match fallback.action() {
            Some(FallbackAction::FallbackTo { from, to, .. }) => {
                assert_eq!(from.model().as_str(), "gpt-4.1");
                assert_eq!(to.provider().as_str(), "anthropic");
                assert_eq!(to.model().as_str(), "claude-sonnet");
            }
            other => panic!("expected fallback {other:?}"),
        }
        controller.apply(&fallback).expect("apply fallback");
        assert_eq!(controller.current().model().as_str(), "claude-sonnet");
        assert_eq!(controller.same_model_retries(), 0);
    }

    #[test]
    fn auth_config_safety_stop_without_explicit_alternate() {
        let decision = ranked_decision();
        let mut controller = controller_from(&decision, FallbackPolicy::standard());
        for trigger in [
            auth(),
            FallbackTrigger::Config,
            FallbackTrigger::Safety,
            FallbackTrigger::Provider(ProviderError::InvalidRequest),
        ] {
            let plan = controller
                .plan(&trigger, AttemptProgress::PreResponse, &live())
                .expect("plan");
            assert!(!plan.is_safe_retry());
            assert!(matches!(
                plan.stop_reason(),
                Some(
                    StopReason::AuthFailure | StopReason::ConfigFailure | StopReason::SafetyFailure
                )
            ));
        }
        let permanent = controller
            .plan(
                &FallbackTrigger::Provider(ProviderError::Permanent),
                AttemptProgress::PreResponse,
                &live(),
            )
            .expect("permanent");
        assert_eq!(permanent.stop_reason(), Some(StopReason::PermanentFailure));
        controller.apply(&permanent).expect("apply");
        assert!(controller.is_terminal());
    }

    #[test]
    fn explicit_alternate_is_used_only_when_eligible() {
        let decision = ranked_decision();
        let policy = FallbackPolicy::standard()
            .with_explicit_alternates(vec![pin("anthropic", "claude-sonnet")])
            .expect("alts");
        let mut controller = controller_from(&decision, policy);
        let plan = controller
            .plan(&auth(), AttemptProgress::PreResponse, &live())
            .expect("plan");
        match plan.action() {
            Some(FallbackAction::FallbackTo { to, .. }) => {
                assert_eq!(to.model().as_str(), "claude-sonnet");
            }
            other => panic!("expected explicit alternate {other:?}"),
        }
        controller.apply(&plan).expect("apply");
        assert_eq!(controller.current().model().as_str(), "claude-sonnet");
    }

    #[test]
    fn explicit_alternate_cannot_cross_hard_rejection() {
        let advertised = caps(128_000, 8192, true);
        let snapshot = catalog(vec![
            spec(
                "local",
                "llama-3",
                advertised.clone(),
                prices(Some(1), Some(1)),
                &[LOCAL_ONLY_TAG, NO_TRAINING_TAG],
                &["us"],
            ),
            spec(
                "openai",
                "gpt-4.1",
                advertised,
                prices(Some(2_000_000), Some(8_000_000)),
                &[],
                &["us"],
            ),
        ]);
        let req = request(PrivacyClass::LocalOnly, None, ModelCapabilities::NONE, None);
        let set = eligible(&req, &snapshot, &live()).expect("eligible");
        assert_eq!(set.len(), 1);
        assert!(!set.rejections().is_empty());
        let decision = select(&req, &set, &RoutePolicyV1::standard(), &live()).expect("select");
        let policy = FallbackPolicy::standard()
            .with_explicit_alternates(vec![pin("openai", "gpt-4.1")])
            .expect("alts");
        let controller = controller_from(&decision, policy);
        assert_eq!(controller.chain().len(), 1);
        let plan = controller
            .plan(&auth(), AttemptProgress::PreResponse, &live())
            .expect("plan");
        assert_eq!(plan.stop_reason(), Some(StopReason::AuthFailure));
        assert!(!plan.is_safe_retry());
    }

    #[test]
    fn weaker_capability_class_cannot_enter_chain() {
        let tools = caps(128_000, 8192, true);
        let no_tools = caps(128_000, 8192, false);
        let snapshot = catalog(vec![
            spec(
                "openai",
                "gpt-4.1",
                tools,
                prices(Some(2_000_000), Some(8_000_000)),
                &[],
                &["us"],
            ),
            spec(
                "local",
                "tiny",
                no_tools,
                prices(Some(1), Some(1)),
                &[],
                &["us"],
            ),
        ]);
        let req = request(
            PrivacyClass::Unrestricted,
            None,
            ModelCapabilities::new(true, false, false, false, false, false),
            None,
        );
        let set = eligible(&req, &snapshot, &live()).expect("eligible");
        assert_eq!(set.len(), 1);
        let decision = select(&req, &set, &RoutePolicyV1::standard(), &live()).expect("select");
        let policy = FallbackPolicy::standard()
            .with_explicit_alternates(vec![pin("local", "tiny")])
            .expect("alts");
        let controller = controller_from(&decision, policy);
        assert_eq!(controller.chain().len(), 1);
        let plan = controller
            .plan(&transient(), AttemptProgress::PreResponse, &live())
            .expect("plan");
        assert!(matches!(
            plan.action(),
            Some(FallbackAction::RetrySame { .. })
        ));
        let mut exhausted = controller;
        let first = exhausted
            .plan(&transient(), AttemptProgress::PreResponse, &live())
            .expect("r1");
        exhausted.apply(&first).expect("a1");
        let second = exhausted
            .plan(&transient(), AttemptProgress::PreResponse, &live())
            .expect("r2");
        exhausted.apply(&second).expect("a2");
        let stop = exhausted
            .plan(&transient(), AttemptProgress::PreResponse, &live())
            .expect("stop");
        assert_eq!(stop.stop_reason(), Some(StopReason::FallbackChainExhausted));
    }

    #[test]
    fn tool_side_effect_is_never_replayed() {
        let decision = ranked_decision();
        let mut controller = controller_from(&decision, FallbackPolicy::standard());
        let plan = controller
            .plan(&transient(), AttemptProgress::ToolSideEffect, &live())
            .expect("plan");
        assert!(!plan.is_safe_retry());
        assert_eq!(plan.progress(), AttemptProgress::ToolSideEffect);
        assert_eq!(plan.stop_reason(), Some(StopReason::SideEffectRisk));
        assert!(plan.action().is_none());
        controller.apply(&plan).expect("apply");
        assert!(controller.is_terminal());
        let again = controller
            .plan(&transient(), AttemptProgress::PreResponse, &live())
            .expect("again");
        assert_eq!(again, plan);
    }

    #[test]
    fn partially_streamed_is_not_a_safe_retry() {
        let decision = ranked_decision();
        let controller = controller_from(&decision, FallbackPolicy::standard());
        let plan = controller
            .plan(&transient(), AttemptProgress::PartiallyStreamed, &live())
            .expect("plan");
        assert!(!plan.is_safe_retry());
        assert_eq!(plan.progress(), AttemptProgress::PartiallyStreamed);
        assert_eq!(plan.stop_reason(), Some(StopReason::PartialStreamUnsafe));
        match plan {
            FallbackPlan::PartiallyStreamed { .. } => {}
            other => panic!("expected partial stream plan {other:?}"),
        }
    }

    #[test]
    fn user_pin_never_falls_back_to_another_model() {
        let snapshot = two_model_catalog();
        let req = request(
            PrivacyClass::Unrestricted,
            None,
            ModelCapabilities::NONE,
            Some(pin("openai", "gpt-4.1")),
        );
        let set = eligible(&req, &snapshot, &live()).expect("pin");
        let decision = select(&req, &set, &RoutePolicyV1::standard(), &live()).expect("select");
        let policy = FallbackPolicy::new(1, 8, 200, 5_000, Vec::new()).expect("policy");
        let mut controller = controller_from(&decision, policy);
        assert_eq!(controller.chain().len(), 1);
        let retry = controller
            .plan(&transient(), AttemptProgress::PreResponse, &live())
            .expect("retry");
        controller.apply(&retry).expect("apply");
        let stop = controller
            .plan(&transient(), AttemptProgress::PreResponse, &live())
            .expect("stop");
        assert_eq!(stop.stop_reason(), Some(StopReason::UserPinForbidsFallback));
        assert!(!stop.is_safe_retry());
    }

    #[test]
    fn context_too_large_and_cancel_do_not_fallback() {
        let decision = ranked_decision();
        let controller = controller_from(&decision, FallbackPolicy::standard());
        let too_large = controller
            .plan(
                &FallbackTrigger::Provider(ProviderError::ContextTooLarge),
                AttemptProgress::PreResponse,
                &live(),
            )
            .expect("ctx");
        assert_eq!(too_large.stop_reason(), Some(StopReason::ContextTooLarge));
        let cancelled = controller
            .plan(
                &FallbackTrigger::Provider(ProviderError::Cancelled),
                AttemptProgress::PreResponse,
                &live(),
            )
            .expect("cancel trigger");
        assert_eq!(cancelled.stop_reason(), Some(StopReason::Cancelled));
        let token = CancellationToken::new();
        token.cancel();
        assert_eq!(
            controller.plan(&transient(), AttemptProgress::PreResponse, &token),
            Err(FallbackError::Cancelled)
        );
    }

    #[test]
    fn stale_plan_is_rejected() {
        let decision = ranked_decision();
        let policy = FallbackPolicy::new(1, 8, 200, 5_000, Vec::new()).expect("policy");
        let mut controller = controller_from(&decision, policy);
        let first = controller
            .plan(&transient(), AttemptProgress::PreResponse, &live())
            .expect("first");
        controller.apply(&first).expect("apply");
        assert_eq!(controller.apply(&first), Err(FallbackError::StalePlan));
    }

    #[test]
    fn invalid_policy_is_rejected() {
        assert_eq!(
            FallbackPolicy::new(0, 0, 200, 5_000, Vec::new()),
            Err(FallbackError::InvalidPolicy)
        );
        assert_eq!(
            FallbackPolicy::new(MAX_SAME_MODEL_RETRIES + 1, 8, 200, 5_000, Vec::new()),
            Err(FallbackError::InvalidPolicy)
        );
        assert_eq!(
            FallbackPolicy::new(2, 8, 0, 5_000, Vec::new()),
            Err(FallbackError::InvalidPolicy)
        );
        assert_eq!(
            FallbackPolicy::new(2, 8, 500, 200, Vec::new()),
            Err(FallbackError::InvalidPolicy)
        );
        let dup = vec![pin("openai", "gpt-4.1"), pin("openai", "gpt-4.1")];
        assert_eq!(
            FallbackPolicy::standard().with_explicit_alternates(dup),
            Err(FallbackError::InvalidPolicy)
        );
    }

    #[test]
    fn policy_serialization_round_trips() {
        let policy = FallbackPolicy::new(2, 4, 200, 1_000, vec![pin("anthropic", "claude-sonnet")])
            .expect("policy");
        let json = serde_json::to_string(&policy).expect("json");
        let decoded: FallbackPolicy = serde_json::from_str(&json).expect("decode");
        assert_eq!(decoded, policy);
        assert!(json.contains("\"schema\":\"rapidlm.fallback_policy\""));
        assert!(json.contains("\"schema_version\":1"));
        assert!(!json.contains("sk-"));
    }

    #[test]
    fn display_and_api_errors_do_not_echo_secrets() {
        let leak = "sk-ant-api03-not-a-real-secret";
        let err = FallbackError::HardConstraintViolation;
        let shown = err.to_string();
        assert!(!shown.contains(leak));
        assert!(!shown.contains("sk-"));
        assert_eq!(shown, "fallback would violate a hard route constraint");
        let api = err
            .into_api_error(GOLDEN_TRACE.parse().expect("trace"))
            .expect("api");
        assert_eq!(api.code(), ErrorCode::PolicyDenied);
        assert!(!api.message().contains(leak));
        assert!(!api.retryable());
        assert!(FallbackError::Cancelled
            .into_api_error(GOLDEN_TRACE.parse().expect("trace"))
            .is_none());
    }

    #[test]
    fn chain_never_includes_hard_rejected_models() {
        let decision = ranked_decision();
        let rejected: Vec<_> = decision
            .hard_rejections()
            .iter()
            .map(|row| row.model().clone())
            .collect();
        let controller = controller_from(&decision, FallbackPolicy::standard());
        for model in controller.chain() {
            assert!(
                !rejected.iter().any(|denied| denied == model),
                "denied model entered chain: {model:?}"
            );
            assert!(decision
                .candidate_scores()
                .iter()
                .any(|row| row.model() == model));
        }
        assert_eq!(controller.chain()[0], *decision.model());
    }
}
