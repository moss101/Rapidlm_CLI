//! Usage/cost accounting for provider stream fragments.
//!
//! [`UsageAccumulator`] folds `Usage`/`Completed` snapshots once into
//! canonical [`ModelUsage`]. Fragments are last-write snapshots, never
//! summed with terminal usage. Missing usage marks cost unknown, never
//! zero. Estimates always record a price-table revision.

use std::error::Error;
use std::fmt;

use protocol::{ApiError, ErrorCode, TraceId};
use serde::de::{self, Deserializer};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};

use crate::provider::{
    CancellationToken, MAX_STREAM_EVENTS, ModelPrices, ModelStream, ModelStreamEvent,
    NormalizedUsage, PriceTableVersion, UsageCost,
};

/// Wire schema name for [`ModelUsage`].
pub const MODEL_USAGE_SCHEMA: &str = "rapidlm.model_usage";

/// v1 schema version for accounted usage objects.
pub const MODEL_USAGE_SCHEMA_VERSION: u16 = 1;

const CANCEL_CHECK_EVERY: usize = 16;
const MILLION: u128 = 1_000_000;

const USAGE_FIELDS: &[&str] = &[
    "schema",
    "schema_version",
    "input_tokens",
    "cached_input_tokens",
    "uncached_input_tokens",
    "output_tokens",
    "reasoning_tokens",
    "tool_tokens",
    "cost",
];

/// Failure while accumulating or estimating usage.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum UsageError {
    Cancelled,
    BoundExceeded,
}

/// Accounted cost. Unknown is never encoded as zero.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum AccountedCost {
    Unknown,
    Reported {
        usd_micros: u64,
    },
    Estimated {
        usd_micros: u64,
        price_table_version: PriceTableVersion,
    },
}

/// Canonical usage emitted into model events after stream accounting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelUsage {
    input_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    uncached_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
    tool_tokens: Option<u64>,
    cost: AccountedCost,
}

/// Combines provider stream fragments once and emits [`ModelUsage`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageAccumulator {
    input_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    uncached_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
    tool_tokens: Option<u64>,
    reported_cost: Option<u64>,
    sealed: bool,
}

impl UsageError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::BoundExceeded => "bound_exceeded",
        }
    }

    pub fn code(self) -> Option<ErrorCode> {
        match self {
            Self::Cancelled => None,
            Self::BoundExceeded => Some(ErrorCode::ConfigInvalid),
        }
    }

    /// Convert to the public envelope. Cancellation is not an API error.
    pub fn into_api_error(self, trace_id: TraceId) -> Option<ApiError> {
        let code = self.code()?;
        let message = match self {
            Self::Cancelled => return None,
            Self::BoundExceeded => "Usage accounting exceeds a documented bound",
        };
        Some(
            ApiError::new(code, message, trace_id)
                .unwrap_or_else(|_| ApiError::from_unknown(trace_id, &self)),
        )
    }
}

impl AccountedCost {
    pub const fn as_kind_str(&self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Reported { .. } => "reported",
            Self::Estimated { .. } => "estimated",
        }
    }

    pub const fn usd_micros(&self) -> Option<u64> {
        match self {
            Self::Unknown => None,
            Self::Reported { usd_micros } | Self::Estimated { usd_micros, .. } => Some(*usd_micros),
        }
    }

    pub fn price_table_version(&self) -> Option<&PriceTableVersion> {
        match self {
            Self::Estimated {
                price_table_version,
                ..
            } => Some(price_table_version),
            Self::Unknown | Self::Reported { .. } => None,
        }
    }
}

impl ModelUsage {
    pub fn new(
        input_tokens: Option<u64>,
        cached_input_tokens: Option<u64>,
        uncached_input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        reasoning_tokens: Option<u64>,
        tool_tokens: Option<u64>,
        cost: AccountedCost,
    ) -> Self {
        Self {
            input_tokens,
            cached_input_tokens,
            uncached_input_tokens,
            output_tokens,
            reasoning_tokens,
            tool_tokens,
            cost,
        }
    }

    /// Account a completed provider stream exactly once.
    pub fn from_stream(
        stream: &ModelStream,
        prices: &ModelPrices,
        cancel: &CancellationToken,
    ) -> Result<Self, UsageError> {
        let mut acc = UsageAccumulator::new();
        acc.observe_stream(stream, cancel)?;
        acc.finish(prices)
    }

    /// Account one already-normalized snapshot (non-stream path).
    pub fn from_normalized(
        usage: &NormalizedUsage,
        prices: &ModelPrices,
    ) -> Result<Self, UsageError> {
        let mut acc = UsageAccumulator::new();
        acc.apply_snapshot(usage);
        acc.finish(prices)
    }

    pub const fn input_tokens(&self) -> Option<u64> {
        self.input_tokens
    }
    pub const fn cached_input_tokens(&self) -> Option<u64> {
        self.cached_input_tokens
    }
    pub const fn uncached_input_tokens(&self) -> Option<u64> {
        self.uncached_input_tokens
    }
    pub const fn output_tokens(&self) -> Option<u64> {
        self.output_tokens
    }
    pub const fn reasoning_tokens(&self) -> Option<u64> {
        self.reasoning_tokens
    }
    pub const fn tool_tokens(&self) -> Option<u64> {
        self.tool_tokens
    }
    pub fn cost(&self) -> &AccountedCost {
        &self.cost
    }
}

impl UsageAccumulator {
    pub const fn new() -> Self {
        Self {
            input_tokens: None,
            cached_input_tokens: None,
            uncached_input_tokens: None,
            output_tokens: None,
            reasoning_tokens: None,
            tool_tokens: None,
            reported_cost: None,
            sealed: false,
        }
    }

    /// Observe one stream event. Terminal `Completed` seals the snapshot.
    pub fn observe(
        &mut self,
        event: &ModelStreamEvent,
        cancel: &CancellationToken,
    ) -> Result<(), UsageError> {
        cancel.check().map_err(|_| UsageError::Cancelled)?;
        match event {
            ModelStreamEvent::Usage(usage) => {
                if !self.sealed {
                    self.apply_snapshot(usage);
                }
            }
            ModelStreamEvent::Completed { usage, .. } => {
                if !self.sealed {
                    self.apply_snapshot(usage);
                    self.sealed = true;
                }
            }
            ModelStreamEvent::TextDelta { .. }
            | ModelStreamEvent::ToolCallStart { .. }
            | ModelStreamEvent::ToolCallArgumentsDelta { .. }
            | ModelStreamEvent::Failed { .. } => {}
        }
        Ok(())
    }

    /// Fold every event on `stream` once. A later `Completed` is not added.
    pub fn observe_stream(
        &mut self,
        stream: &ModelStream,
        cancel: &CancellationToken,
    ) -> Result<(), UsageError> {
        cancel.check().map_err(|_| UsageError::Cancelled)?;
        if stream.events().len() > MAX_STREAM_EVENTS {
            return Err(UsageError::BoundExceeded);
        }
        for (i, event) in stream.events().iter().enumerate() {
            if i.is_multiple_of(CANCEL_CHECK_EVERY) {
                cancel.check().map_err(|_| UsageError::Cancelled)?;
            }
            self.observe(event, cancel)?;
        }
        Ok(())
    }

    /// Emit canonical usage. Estimates require a versioned price table.
    pub fn finish(self, prices: &ModelPrices) -> Result<ModelUsage, UsageError> {
        let counts = derived_counts(
            self.input_tokens,
            self.cached_input_tokens,
            self.uncached_input_tokens,
        )?;
        let cost = match self.reported_cost {
            Some(usd_micros) => AccountedCost::Reported { usd_micros },
            None => estimate_cost(counts, self.output_tokens, prices)?,
        };
        Ok(ModelUsage::new(
            counts.input,
            counts.cached,
            counts.uncached,
            self.output_tokens,
            self.reasoning_tokens,
            self.tool_tokens,
            cost,
        ))
    }

    fn apply_snapshot(&mut self, usage: &NormalizedUsage) {
        if let Some(value) = usage.input_tokens() {
            self.input_tokens = Some(value);
        }
        if let Some(value) = usage.cached_input_tokens() {
            self.cached_input_tokens = Some(value);
        }
        if let Some(value) = usage.uncached_input_tokens() {
            self.uncached_input_tokens = Some(value);
        }
        if let Some(value) = usage.output_tokens() {
            self.output_tokens = Some(value);
        }
        if let Some(value) = usage.reasoning_tokens() {
            self.reasoning_tokens = Some(value);
        }
        if let Some(value) = usage.tool_tokens() {
            self.tool_tokens = Some(value);
        }
        if let UsageCost::Reported { usd_micros } = usage.cost() {
            self.reported_cost = Some(usd_micros);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DerivedCounts {
    input: Option<u64>,
    cached: Option<u64>,
    uncached: Option<u64>,
}

fn derived_counts(
    input: Option<u64>,
    cached: Option<u64>,
    uncached: Option<u64>,
) -> Result<DerivedCounts, UsageError> {
    let input = match (input, cached, uncached) {
        (Some(input), _, _) => Some(input),
        (None, Some(cached), Some(uncached)) => Some(
            cached
                .checked_add(uncached)
                .ok_or(UsageError::BoundExceeded)?,
        ),
        (None, _, _) => None,
    };
    let (cached, uncached) = match (cached, uncached, input) {
        (None, None, Some(input)) => (Some(0), Some(input)),
        (Some(cached), None, Some(input)) if input >= cached => {
            (Some(cached), Some(input - cached))
        }
        (None, Some(uncached), Some(input)) if input >= uncached => {
            (Some(input - uncached), Some(uncached))
        }
        (Some(cached), Some(uncached), _) => (Some(cached), Some(uncached)),
        other => (other.0, other.1),
    };
    Ok(DerivedCounts {
        input,
        cached,
        uncached,
    })
}

fn estimate_cost(
    counts: DerivedCounts,
    output_tokens: Option<u64>,
    prices: &ModelPrices,
) -> Result<AccountedCost, UsageError> {
    let Some(version) = prices.table_version().cloned() else {
        return Ok(AccountedCost::Unknown);
    };
    let Some(output_tokens) = output_tokens else {
        return Ok(AccountedCost::Unknown);
    };
    let (cached, uncached) = match billed_input(counts) {
        Some(split) => split,
        None => return Ok(AccountedCost::Unknown),
    };

    let mut total: u128 = 0;
    if uncached > 0 {
        let Some(price) = prices.input_usd_micros_per_million() else {
            return Ok(AccountedCost::Unknown);
        };
        total = add_token_cost(total, uncached, price)?;
    }
    if cached > 0 {
        let Some(price) = prices.cached_input_usd_micros_per_million() else {
            return Ok(AccountedCost::Unknown);
        };
        total = add_token_cost(total, cached, price)?;
    }
    if output_tokens > 0 {
        let Some(output_price) = prices.output_usd_micros_per_million() else {
            return Ok(AccountedCost::Unknown);
        };
        total = add_token_cost(total, output_tokens, output_price)?;
    }
    let usd_micros = u64::try_from(total).map_err(|_| UsageError::BoundExceeded)?;
    Ok(AccountedCost::Estimated {
        usd_micros,
        price_table_version: version,
    })
}

fn billed_input(counts: DerivedCounts) -> Option<(u64, u64)> {
    match (counts.cached, counts.uncached, counts.input) {
        (Some(cached), Some(uncached), _) => Some((cached, uncached)),
        (None, None, Some(input)) => Some((0, input)),
        (Some(cached), None, Some(input)) if input >= cached => Some((cached, input - cached)),
        (None, Some(uncached), Some(input)) if input >= uncached => {
            Some((input - uncached, uncached))
        }
        _ => None,
    }
}

fn add_token_cost(
    total: u128,
    tokens: u64,
    usd_micros_per_million: u64,
) -> Result<u128, UsageError> {
    let part = u128::from(tokens)
        .checked_mul(u128::from(usd_micros_per_million))
        .ok_or(UsageError::BoundExceeded)?
        / MILLION;
    total.checked_add(part).ok_or(UsageError::BoundExceeded)
}

impl Default for UsageAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for UsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cancelled => "usage accounting cancelled",
            Self::BoundExceeded => "usage accounting exceeds a documented bound",
        })
    }
}

impl Error for UsageError {}

impl Serialize for AccountedCost {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Unknown => {
                let mut state = serializer.serialize_struct("AccountedCost", 1)?;
                state.serialize_field("kind", "unknown")?;
                state.end()
            }
            Self::Reported { usd_micros } => {
                let mut state = serializer.serialize_struct("AccountedCost", 2)?;
                state.serialize_field("kind", "reported")?;
                state.serialize_field("usd_micros", usd_micros)?;
                state.end()
            }
            Self::Estimated {
                usd_micros,
                price_table_version,
            } => {
                let mut state = serializer.serialize_struct("AccountedCost", 3)?;
                state.serialize_field("kind", "estimated")?;
                state.serialize_field("usd_micros", usd_micros)?;
                state.serialize_field("price_table_version", price_table_version)?;
                state.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for AccountedCost {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            kind: String,
            usd_micros: Option<u64>,
            price_table_version: Option<PriceTableVersion>,
        }
        let raw = Raw::deserialize(deserializer)?;
        match raw.kind.as_str() {
            "unknown" => {
                if raw.usd_micros.is_some() || raw.price_table_version.is_some() {
                    return Err(de::Error::custom(
                        "unknown cost cannot include usd_micros or price_table_version",
                    ));
                }
                Ok(Self::Unknown)
            }
            "reported" => {
                if raw.price_table_version.is_some() {
                    return Err(de::Error::custom(
                        "reported cost cannot include price_table_version",
                    ));
                }
                let usd_micros = raw
                    .usd_micros
                    .ok_or_else(|| de::Error::missing_field("usd_micros"))?;
                Ok(Self::Reported { usd_micros })
            }
            "estimated" => {
                let usd_micros = raw
                    .usd_micros
                    .ok_or_else(|| de::Error::missing_field("usd_micros"))?;
                let price_table_version = raw
                    .price_table_version
                    .ok_or_else(|| de::Error::missing_field("price_table_version"))?;
                Ok(Self::Estimated {
                    usd_micros,
                    price_table_version,
                })
            }
            _ => Err(de::Error::custom("unknown accounted cost kind")),
        }
    }
}

impl Serialize for ModelUsage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("ModelUsage", USAGE_FIELDS.len())?;
        state.serialize_field("schema", MODEL_USAGE_SCHEMA)?;
        state.serialize_field("schema_version", &MODEL_USAGE_SCHEMA_VERSION)?;
        state.serialize_field("input_tokens", &self.input_tokens)?;
        state.serialize_field("cached_input_tokens", &self.cached_input_tokens)?;
        state.serialize_field("uncached_input_tokens", &self.uncached_input_tokens)?;
        state.serialize_field("output_tokens", &self.output_tokens)?;
        state.serialize_field("reasoning_tokens", &self.reasoning_tokens)?;
        state.serialize_field("tool_tokens", &self.tool_tokens)?;
        state.serialize_field("cost", &self.cost)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ModelUsage {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            schema: String,
            schema_version: u16,
            input_tokens: Option<u64>,
            cached_input_tokens: Option<u64>,
            uncached_input_tokens: Option<u64>,
            output_tokens: Option<u64>,
            reasoning_tokens: Option<u64>,
            tool_tokens: Option<u64>,
            cost: AccountedCost,
        }
        let raw = Raw::deserialize(deserializer)?;
        if raw.schema != MODEL_USAGE_SCHEMA || raw.schema_version != MODEL_USAGE_SCHEMA_VERSION {
            return Err(de::Error::custom("unsupported model usage schema"));
        }
        Ok(Self::new(
            raw.input_tokens,
            raw.cached_input_tokens,
            raw.uncached_input_tokens,
            raw.output_tokens,
            raw.reasoning_tokens,
            raw.tool_tokens,
            raw.cost,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{
        FinishReason, ModelId, ModelRef, ModelRequestId, ProviderId, UsageExtValue,
    };
    use protocol::TraceId;

    const GOLDEN_ESTIMATED: &str = r#"{"schema":"rapidlm.model_usage","schema_version":1,"input_tokens":12,"cached_input_tokens":4,"uncached_input_tokens":8,"output_tokens":3,"reasoning_tokens":1,"tool_tokens":null,"cost":{"kind":"estimated","usd_micros":42,"price_table_version":"openai-2026-04"}}"#;

    const GOLDEN_REPORTED: &str = r#"{"schema":"rapidlm.model_usage","schema_version":1,"input_tokens":12,"cached_input_tokens":4,"uncached_input_tokens":8,"output_tokens":3,"reasoning_tokens":null,"tool_tokens":null,"cost":{"kind":"reported","usd_micros":99}}"#;

    const GOLDEN_UNKNOWN: &str = r#"{"schema":"rapidlm.model_usage","schema_version":1,"input_tokens":null,"cached_input_tokens":null,"uncached_input_tokens":null,"output_tokens":null,"reasoning_tokens":null,"tool_tokens":null,"cost":{"kind":"unknown"}}"#;

    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    fn table() -> PriceTableVersion {
        PriceTableVersion::parse("openai-2026-04").expect("price table")
    }

    fn prices() -> ModelPrices {
        ModelPrices::new(
            Some(2_000_000),
            Some(8_000_000),
            Some(500_000),
            Some(table()),
        )
    }

    fn usage(
        input: Option<u64>,
        cached: Option<u64>,
        uncached: Option<u64>,
        output: Option<u64>,
        reasoning: Option<u64>,
        cost: UsageCost,
    ) -> NormalizedUsage {
        NormalizedUsage::new(input, cached, uncached, output, reasoning, None, cost)
    }

    fn stream(events: Vec<ModelStreamEvent>) -> ModelStream {
        ModelStream::from_events(
            ModelRequestId::parse("018f3c8a-7e2b-7a10-8c4d-0123456789ab").expect("id"),
            ModelRef::new(
                ProviderId::parse("openai").expect("provider"),
                ModelId::parse("gpt-4.1").expect("model"),
            ),
            events,
            &live(),
        )
        .expect("stream")
    }

    fn account(events: Vec<ModelStreamEvent>, prices: &ModelPrices) -> ModelUsage {
        ModelUsage::from_stream(&stream(events), prices, &live()).expect("account")
    }

    #[test]
    fn estimated_usage_golden_round_trips() {
        let usage = ModelUsage::new(
            Some(12),
            Some(4),
            Some(8),
            Some(3),
            Some(1),
            None,
            AccountedCost::Estimated {
                usd_micros: 42,
                price_table_version: table(),
            },
        );
        let json = serde_json::to_string(&usage).expect("serialize");
        assert_eq!(json, GOLDEN_ESTIMATED);
        let decoded: ModelUsage = serde_json::from_str(GOLDEN_ESTIMATED).expect("decode");
        assert_eq!(decoded, usage);
        assert_eq!(decoded.cost().price_table_version(), Some(&table()));
    }

    #[test]
    fn reported_and_unknown_usage_golden_round_trips() {
        let reported = ModelUsage::new(
            Some(12),
            Some(4),
            Some(8),
            Some(3),
            None,
            None,
            AccountedCost::Reported { usd_micros: 99 },
        );
        assert_eq!(
            serde_json::to_string(&reported).expect("serialize"),
            GOLDEN_REPORTED
        );
        let decoded: ModelUsage = serde_json::from_str(GOLDEN_REPORTED).expect("decode");
        assert_eq!(decoded, reported);

        let unknown = ModelUsage::new(None, None, None, None, None, None, AccountedCost::Unknown);
        assert_eq!(
            serde_json::to_string(&unknown).expect("serialize"),
            GOLDEN_UNKNOWN
        );
        let decoded: ModelUsage = serde_json::from_str(GOLDEN_UNKNOWN).expect("decode");
        assert_eq!(decoded, unknown);
        assert_eq!(decoded.cost(), &AccountedCost::Unknown);
        assert_ne!(decoded.cost().usd_micros(), Some(0));
    }

    #[test]
    fn stream_usage_plus_completed_is_not_double_counted() {
        let snapshot = usage(
            Some(12),
            Some(4),
            Some(8),
            Some(3),
            Some(1),
            UsageCost::Unknown,
        );
        let accounted = account(
            vec![
                ModelStreamEvent::Usage(snapshot.clone()),
                ModelStreamEvent::Completed {
                    finish: FinishReason::Stop,
                    usage: snapshot,
                },
            ],
            &prices(),
        );
        assert_eq!(accounted.input_tokens(), Some(12));
        assert_eq!(accounted.cached_input_tokens(), Some(4));
        assert_eq!(accounted.uncached_input_tokens(), Some(8));
        assert_eq!(accounted.output_tokens(), Some(3));
        assert_eq!(accounted.reasoning_tokens(), Some(1));
        assert_eq!(
            accounted.cost(),
            &AccountedCost::Estimated {
                usd_micros: 42,
                price_table_version: table(),
            }
        );
    }

    #[test]
    fn later_usage_after_completed_is_ignored() {
        let first = usage(Some(10), None, None, Some(2), None, UsageCost::Unknown);
        let late = usage(
            Some(10_000),
            None,
            None,
            Some(2_000),
            None,
            UsageCost::Unknown,
        );
        let accounted = account(
            vec![
                ModelStreamEvent::Completed {
                    finish: FinishReason::Stop,
                    usage: first,
                },
                ModelStreamEvent::Usage(late),
            ],
            &prices(),
        );
        assert_eq!(accounted.input_tokens(), Some(10));
        assert_eq!(accounted.output_tokens(), Some(2));
    }

    #[test]
    fn partial_fragments_merge_without_adding() {
        let input_only = usage(Some(12), Some(4), None, None, None, UsageCost::Unknown);
        let output_only = usage(None, None, None, Some(3), Some(1), UsageCost::Unknown);
        let accounted = account(
            vec![
                ModelStreamEvent::Usage(input_only.clone()),
                ModelStreamEvent::Usage(output_only),
                ModelStreamEvent::Completed {
                    finish: FinishReason::Stop,
                    usage: input_only,
                },
            ],
            &prices(),
        );
        assert_eq!(accounted.input_tokens(), Some(12));
        assert_eq!(accounted.cached_input_tokens(), Some(4));
        assert_eq!(accounted.uncached_input_tokens(), Some(8));
        assert_eq!(accounted.output_tokens(), Some(3));
        assert_eq!(
            accounted.cost(),
            &AccountedCost::Estimated {
                usd_micros: 42,
                price_table_version: table(),
            }
        );
    }

    #[test]
    fn provider_reported_cost_is_preferred_over_estimate() {
        let snapshot = usage(
            Some(12),
            Some(4),
            Some(8),
            Some(3),
            None,
            UsageCost::Reported { usd_micros: 99 },
        );
        let accounted = ModelUsage::from_normalized(&snapshot, &prices()).expect("account");
        assert_eq!(
            accounted.cost(),
            &AccountedCost::Reported { usd_micros: 99 }
        );
        assert_eq!(accounted.cost().price_table_version(), None);
    }

    #[test]
    fn estimate_records_price_table_revision() {
        let snapshot = usage(
            Some(12),
            Some(4),
            Some(8),
            Some(3),
            Some(1),
            UsageCost::Unknown,
        );
        let accounted = ModelUsage::from_normalized(&snapshot, &prices()).expect("account");
        match accounted.cost() {
            AccountedCost::Estimated {
                usd_micros,
                price_table_version,
            } => {
                assert_eq!(*usd_micros, 42);
                assert_eq!(price_table_version.as_str(), "openai-2026-04");
            }
            other => panic!("expected estimated cost, got {other:?}"),
        }
    }

    #[test]
    fn missing_price_table_revision_is_unknown_not_zero() {
        let snapshot = usage(Some(12), None, None, Some(3), None, UsageCost::Unknown);
        let prices = ModelPrices::new(Some(2_000_000), Some(8_000_000), None, None);
        let accounted = ModelUsage::from_normalized(&snapshot, &prices).expect("account");
        assert_eq!(accounted.cost(), &AccountedCost::Unknown);
        assert_ne!(accounted.cost().usd_micros(), Some(0));
    }

    #[test]
    fn missing_usage_is_unknown_not_zero() {
        let accounted = account(
            vec![ModelStreamEvent::Completed {
                finish: FinishReason::Stop,
                usage: usage(None, None, None, None, None, UsageCost::Unknown),
            }],
            &prices(),
        );
        assert_eq!(accounted.cost(), &AccountedCost::Unknown);
        assert_ne!(accounted.cost(), &AccountedCost::Reported { usd_micros: 0 });
        assert_ne!(
            accounted.cost(),
            &AccountedCost::Estimated {
                usd_micros: 0,
                price_table_version: table(),
            }
        );
    }

    #[test]
    fn cached_tokens_without_cached_price_are_unknown() {
        let snapshot = usage(
            Some(12),
            Some(4),
            Some(8),
            Some(3),
            None,
            UsageCost::Unknown,
        );
        let prices = ModelPrices::new(Some(2_000_000), Some(8_000_000), None, Some(table()));
        let accounted = ModelUsage::from_normalized(&snapshot, &prices).expect("account");
        assert_eq!(accounted.cost(), &AccountedCost::Unknown);
    }

    #[test]
    fn input_is_not_double_billed_when_cache_split_is_present() {
        let snapshot = usage(
            Some(12),
            Some(4),
            Some(8),
            Some(0),
            None,
            UsageCost::Unknown,
        );
        let accounted = ModelUsage::from_normalized(&snapshot, &prices()).expect("account");
        // 8 * 2 + 4 * 0.5 + 0 * 8 = 18, not 12 * 2 + 4 * 0.5.
        assert_eq!(
            accounted.cost(),
            &AccountedCost::Estimated {
                usd_micros: 18,
                price_table_version: table(),
            }
        );
    }

    #[test]
    fn extras_on_provider_usage_do_not_change_canonical_fields() {
        let mut snapshot = usage(Some(5), None, None, Some(1), None, UsageCost::Unknown);
        snapshot
            .insert_extra("audio_tokens", UsageExtValue::U64(7))
            .expect("extra");
        let accounted = ModelUsage::from_normalized(&snapshot, &prices()).expect("account");
        assert_eq!(accounted.input_tokens(), Some(5));
        assert_eq!(accounted.output_tokens(), Some(1));
        assert_eq!(accounted.uncached_input_tokens(), Some(5));
        assert_eq!(accounted.cached_input_tokens(), Some(0));
    }

    #[test]
    fn cancelled_stream_does_not_emit_usage() {
        let cancel = live();
        cancel.cancel();
        let snapshot = usage(Some(12), None, None, Some(3), None, UsageCost::Unknown);
        let err = ModelUsage::from_stream(
            &stream(vec![ModelStreamEvent::Completed {
                finish: FinishReason::Stop,
                usage: snapshot,
            }]),
            &prices(),
            &cancel,
        )
        .expect_err("cancelled");
        assert_eq!(err, UsageError::Cancelled);
        assert_eq!(err.code(), None);
        let trace = "018f3c8a-7e2b-7a10-8c4d-0123456789ab"
            .parse::<TraceId>()
            .expect("trace");
        assert!(err.into_api_error(trace).is_none());
    }

    #[test]
    fn unknown_cost_kind_and_estimated_without_revision_fail_closed() {
        assert!(serde_json::from_str::<AccountedCost>(r#"{"kind":"free"}"#).is_err());
        assert!(
            serde_json::from_str::<AccountedCost>(r#"{"kind":"estimated","usd_micros":1}"#)
                .is_err()
        );
        assert!(
            serde_json::from_str::<AccountedCost>(r#"{"kind":"unknown","usd_micros":0}"#).is_err()
        );
    }
}
