//! Model selector over catalog rows and hard route constraints.
//!
//! [`ModelViewModel`] is a frontend projection. It never routes, persists a
//! pin, or broadens privacy/capability policy. Pin/unpin return typed intents
//! for the kernel client. The router remains the final hard-constraint
//! enforcer. Untrusted provider/model labels are sanitized.

use std::error::Error;
use std::fmt::{self, Display, Formatter};

use llm_router::{
    CatalogEntry, CatalogRevision, CatalogSnapshot, LatencyClass, MAX_CATALOG_ENTRIES, ModelPrices,
    ModelRef, PrivacyClass, ProviderCapabilities, RejectionReason, RouteRequest, hard_constraints,
};

use crate::sanitize::sanitize_untrusted;
use crate::state::CancellationToken;

/// Maximum projected catalog rows retained in one selector.
pub const MAX_MODEL_ITEMS: usize = MAX_CATALOG_ENTRIES;

/// Render width is clamped to this many columns.
pub const MAX_MODEL_COLS: u16 = 512;

/// Render height is clamped to this many rows.
pub const MAX_MODEL_ROWS: u16 = 256;

const CANCEL_STRIDE: usize = 8;

/// Eligible vs unavailable grouping used by the selector table.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum ModelAvailability {
    Eligible,
    Unavailable,
}

/// Local row cursor. Not domain state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct ModelSelection {
    row_index: usize,
}

/// Typed selector failure. Display never echoes provider or model ids.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ModelSelectError {
    Cancelled,
    BoundExceeded,
    InvalidSelection,
    AlreadyPinned,
    NotPinned,
    PolicyDenied,
}

/// Kernel-bound pin request. The selector does not apply it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelPinIntent {
    model: ModelRef,
    privacy: PrivacyClass,
    catalog_revision: CatalogRevision,
}

/// Kernel-bound unpin request. The selector does not apply it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelUnpinIntent {
    model: ModelRef,
    catalog_revision: CatalogRevision,
}

/// One projected catalog row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelRowView {
    model: ModelRef,
    display_model: String,
    display_provider: String,
    availability: ModelAvailability,
    capabilities: String,
    context_limit: u32,
    cost_hint: String,
    latency_hint: String,
    pinned: bool,
    unavailable_reason: Option<String>,
}

/// Frontend-only projection. Never routes or persists pins.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelViewModel {
    rows: Vec<ModelRowView>,
    selection: ModelSelection,
    catalog: CatalogSnapshot,
    request: RouteRequest,
}

/// One painted frame. `golden` omits trailing pad.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelFrame {
    width: u16,
    height: u16,
    lines: Vec<String>,
}

impl ModelAvailability {
    pub const ALL: &'static [Self] = &[Self::Eligible, Self::Unavailable];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Eligible => "eligible",
            Self::Unavailable => "unavailable",
        }
    }
}

impl ModelSelection {
    pub const fn new(row_index: usize) -> Self {
        Self { row_index }
    }

    pub const fn row_index(self) -> usize {
        self.row_index
    }
}

impl ModelSelectError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "model selector cancelled",
            Self::BoundExceeded => "model selector resource bound exceeded",
            Self::InvalidSelection => "model selection is out of range",
            Self::AlreadyPinned => "model is already pinned",
            Self::NotPinned => "model is not pinned",
            Self::PolicyDenied => "model pin violates hard privacy/capability policy",
        }
    }
}

impl ModelPinIntent {
    pub fn model(&self) -> &ModelRef {
        &self.model
    }

    pub const fn privacy(&self) -> PrivacyClass {
        self.privacy
    }

    pub const fn catalog_revision(&self) -> CatalogRevision {
        self.catalog_revision
    }

    /// Pin persistence must be committed by the kernel, not this view.
    pub const fn requires_durable_audit(&self) -> bool {
        true
    }
}

impl ModelUnpinIntent {
    pub fn model(&self) -> &ModelRef {
        &self.model
    }

    pub const fn catalog_revision(&self) -> CatalogRevision {
        self.catalog_revision
    }

    /// Unpin persistence must be committed by the kernel, not this view.
    pub const fn requires_durable_audit(&self) -> bool {
        true
    }
}

impl ModelRowView {
    pub fn model(&self) -> &ModelRef {
        &self.model
    }

    pub fn display_model(&self) -> &str {
        &self.display_model
    }

    pub fn display_provider(&self) -> &str {
        &self.display_provider
    }

    pub const fn availability(&self) -> ModelAvailability {
        self.availability
    }

    pub fn capabilities(&self) -> &str {
        &self.capabilities
    }

    pub const fn context_limit(&self) -> u32 {
        self.context_limit
    }

    pub fn cost_hint(&self) -> &str {
        &self.cost_hint
    }

    pub fn latency_hint(&self) -> &str {
        &self.latency_hint
    }

    pub const fn is_pinned(&self) -> bool {
        self.pinned
    }

    pub const fn is_eligible(&self) -> bool {
        matches!(self.availability, ModelAvailability::Eligible)
    }

    pub fn unavailable_reason(&self) -> Option<&str> {
        self.unavailable_reason.as_deref()
    }
}

impl ModelViewModel {
    /// Project catalog rows plus selection. Pins are not written.
    pub fn new(
        catalog: &CatalogSnapshot,
        request: &RouteRequest,
        selection: ModelSelection,
        cancel: &CancellationToken,
    ) -> Result<Self, ModelSelectError> {
        check_cancel(cancel)?;
        if catalog.entries().len() > MAX_MODEL_ITEMS {
            return Err(ModelSelectError::BoundExceeded);
        }
        let pin = request.user_pin();
        let mut rows = Vec::with_capacity(catalog.entries().len());
        for (index, entry) in catalog.entries().iter().enumerate() {
            if index.is_multiple_of(CANCEL_STRIDE) {
                check_cancel(cancel)?;
            }
            rows.push(project_entry(entry, pin, request));
        }
        rows.sort_by(|a, b| {
            a.availability
                .cmp(&b.availability)
                .then(a.display_provider.as_str().cmp(b.display_provider.as_str()))
                .then(a.display_model.as_str().cmp(b.display_model.as_str()))
        });
        let row_index = if rows.is_empty() {
            0
        } else {
            selection.row_index.min(rows.len() - 1)
        };
        Ok(Self {
            rows,
            selection: ModelSelection { row_index },
            catalog: catalog.clone(),
            request: request.clone(),
        })
    }

    pub fn from_catalog(
        catalog: &CatalogSnapshot,
        request: &RouteRequest,
        cancel: &CancellationToken,
    ) -> Result<Self, ModelSelectError> {
        Self::new(catalog, request, ModelSelection::default(), cancel)
    }

    pub fn rows(&self) -> &[ModelRowView] {
        &self.rows
    }

    pub fn selected_row(&self) -> Option<&ModelRowView> {
        self.rows.get(self.selection.row_index)
    }

    pub fn selection(&self) -> ModelSelection {
        self.selection
    }

    pub const fn privacy(&self) -> PrivacyClass {
        self.request.privacy()
    }

    pub fn pin_ref(&self) -> Option<&ModelRef> {
        self.request.user_pin()
    }

    pub const fn catalog_revision(&self) -> CatalogRevision {
        self.catalog.revision()
    }

    /// This panel never routes or persists pin state.
    pub const fn mutates_router(&self) -> bool {
        false
    }

    pub fn select_row(&self, index: usize) -> Result<Self, ModelSelectError> {
        if self.rows.is_empty() {
            if index == 0 {
                return Ok(self.clone());
            }
            return Err(ModelSelectError::InvalidSelection);
        }
        if index >= self.rows.len() {
            return Err(ModelSelectError::InvalidSelection);
        }
        let mut next = self.clone();
        next.selection.row_index = index;
        Ok(next)
    }

    pub fn select_next(&self) -> Self {
        let mut next = self.clone();
        if !self.rows.is_empty() && self.selection.row_index + 1 < self.rows.len() {
            next.selection.row_index += 1;
        }
        next
    }

    pub fn select_prev(&self) -> Self {
        let mut next = self.clone();
        next.selection.row_index = self.selection.row_index.saturating_sub(1);
        next
    }

    pub fn select_model(&self, model: &ModelRef) -> Result<Self, ModelSelectError> {
        let index = self
            .rows
            .iter()
            .position(|row| &row.model == model)
            .ok_or(ModelSelectError::InvalidSelection)?;
        self.select_row(index)
    }

    /// Request a user pin. Hard privacy/capability policy is checked first.
    ///
    /// Success is only [`ModelPinIntent`]. The view is not mutated. The router
    /// remains the final enforcer after the kernel accepts the intent.
    pub fn pin(&self, cancel: &CancellationToken) -> Result<ModelPinIntent, ModelSelectError> {
        check_cancel(cancel)?;
        let row = self
            .selected_row()
            .ok_or(ModelSelectError::InvalidSelection)?;
        self.pin_row(row)
    }

    pub fn pin_model(
        &self,
        model: &ModelRef,
        cancel: &CancellationToken,
    ) -> Result<ModelPinIntent, ModelSelectError> {
        check_cancel(cancel)?;
        let row = self
            .rows
            .iter()
            .find(|row| &row.model == model)
            .ok_or(ModelSelectError::InvalidSelection)?;
        self.pin_row(row)
    }

    /// Request an unpin. The view is not mutated.
    pub fn unpin(&self, cancel: &CancellationToken) -> Result<ModelUnpinIntent, ModelSelectError> {
        check_cancel(cancel)?;
        let row = self
            .selected_row()
            .ok_or(ModelSelectError::InvalidSelection)?;
        self.unpin_row(row)
    }

    pub fn unpin_model(
        &self,
        model: &ModelRef,
        cancel: &CancellationToken,
    ) -> Result<ModelUnpinIntent, ModelSelectError> {
        check_cancel(cancel)?;
        let row = self
            .rows
            .iter()
            .find(|row| &row.model == model)
            .ok_or(ModelSelectError::InvalidSelection)?;
        self.unpin_row(row)
    }

    pub fn render(&self, width: u16, height: u16) -> ModelFrame {
        let width = width.min(MAX_MODEL_COLS);
        let height = height.min(MAX_MODEL_ROWS);
        if width == 0 || height == 0 {
            return ModelFrame {
                width,
                height,
                lines: Vec::new(),
            };
        }
        let mut lines = vec![format!(
            "models privacy:{} pin:{}",
            self.request.privacy().as_str(),
            format_pin(self.request.user_pin())
        )];
        for group in ModelAvailability::ALL {
            lines.push(format!("{}:", group.as_str()));
            let mut any = false;
            for (index, row) in self.rows.iter().enumerate() {
                if row.availability != *group {
                    continue;
                }
                any = true;
                let marker = if index == self.selection.row_index {
                    '>'
                } else {
                    ' '
                };
                lines.push(format!("{marker} {}", format_row(row)));
            }
            if !any {
                lines.push("  (empty)".to_owned());
            }
        }
        if lines.len() > usize::from(height) {
            lines.truncate(usize::from(height));
        }
        ModelFrame {
            width,
            height,
            lines,
        }
    }

    fn pin_row(&self, row: &ModelRowView) -> Result<ModelPinIntent, ModelSelectError> {
        if row.pinned {
            return Err(ModelSelectError::AlreadyPinned);
        }
        let entry = self
            .catalog
            .get(&row.model)
            .ok_or(ModelSelectError::InvalidSelection)?;
        if hard_constraints(&self.request, entry).is_err() {
            return Err(ModelSelectError::PolicyDenied);
        }
        Ok(ModelPinIntent {
            model: row.model.clone(),
            privacy: self.request.privacy(),
            catalog_revision: self.catalog.revision(),
        })
    }

    fn unpin_row(&self, row: &ModelRowView) -> Result<ModelUnpinIntent, ModelSelectError> {
        if !row.pinned {
            return Err(ModelSelectError::NotPinned);
        }
        Ok(ModelUnpinIntent {
            model: row.model.clone(),
            catalog_revision: self.catalog.revision(),
        })
    }
}

impl ModelFrame {
    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// Stable dump used by golden tests. Trailing pad is omitted.
    pub fn golden(&self) -> String {
        self.lines.join("\n")
    }

    /// Exact-width rows written into the pane, padded/truncated to `height`.
    pub fn text(&self) -> String {
        let width = usize::from(self.width);
        let height = usize::from(self.height);
        let mut rows = Vec::with_capacity(height);
        for i in 0..height {
            let src = self.lines.get(i).map(String::as_str).unwrap_or("");
            rows.push(fit_width(src, width));
        }
        rows.join("\n")
    }
}

impl Display for ModelSelectError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for ModelSelectError {}

fn project_entry(
    entry: &CatalogEntry,
    pin: Option<&ModelRef>,
    request: &RouteRequest,
) -> ModelRowView {
    let descriptor = entry.descriptor();
    let model = descriptor.model_ref();
    let (availability, unavailable_reason) = match hard_constraints(request, entry) {
        Ok(()) => (ModelAvailability::Eligible, None),
        Err(reason) => (ModelAvailability::Unavailable, Some(format_reason(&reason))),
    };
    ModelRowView {
        display_model: sanitize_untrusted(descriptor.model().as_str()).into_owned(),
        display_provider: sanitize_untrusted(descriptor.provider().as_str()).into_owned(),
        availability,
        capabilities: format_caps(descriptor.capabilities()),
        context_limit: descriptor.context_limit(),
        cost_hint: format_cost(descriptor.prices()),
        latency_hint: format_latency(descriptor.latency_class()),
        pinned: pin == Some(&model),
        unavailable_reason,
        model,
    }
}

fn format_row(row: &ModelRowView) -> String {
    let pin = if row.pinned { "yes" } else { "no" };
    let mut line = format!(
        "{} provider:{} caps:{} ctx:{} cost:{} latency:{} pin:{pin}",
        row.display_model,
        row.display_provider,
        row.capabilities,
        row.context_limit,
        row.cost_hint,
        row.latency_hint
    );
    if let Some(reason) = row.unavailable_reason.as_deref() {
        line.push_str(" reason:");
        line.push_str(reason);
    }
    line
}

fn format_pin(pin: Option<&ModelRef>) -> String {
    match pin {
        None => "-".to_owned(),
        Some(model) => format!(
            "{}/{}",
            sanitize_untrusted(model.provider().as_str()),
            sanitize_untrusted(model.model().as_str())
        ),
    }
}

fn format_reason(reason: &RejectionReason) -> String {
    match reason {
        RejectionReason::ProviderUnavailable { reason } => {
            format!("provider_unavailable:{}", reason.as_str())
        }
        RejectionReason::PrivacyDenied { class } => {
            format!("privacy_denied:{}", class.as_str())
        }
        RejectionReason::ContextTooSmall {
            required,
            available,
        } => format!("context_too_small:{available}<{required}"),
        RejectionReason::OutputReserveTooLarge {
            required,
            available,
        } => format!("output_reserve_too_large:{available}<{required}"),
        RejectionReason::RegionDenied { required } => {
            format!("region_denied:{}", sanitize_untrusted(required.as_str()))
        }
        other => other.as_str().to_owned(),
    }
}

fn format_caps(caps: &ProviderCapabilities) -> String {
    let mut flags = Vec::new();
    if caps.tools() {
        flags.push("tools");
    }
    if caps.streaming() {
        flags.push("stream");
    }
    if caps.vision() {
        flags.push("vision");
    }
    if caps.caching() {
        flags.push("cache");
    }
    if caps.reasoning() == llm_router::ReasoningSupport::Exposed {
        flags.push("reason");
    }
    if caps.structured_output() {
        flags.push("json");
    }
    if flags.is_empty() {
        "-".to_owned()
    } else {
        flags.join(",")
    }
}

fn format_cost(prices: &ModelPrices) -> String {
    match (
        prices.input_usd_micros_per_million(),
        prices.output_usd_micros_per_million(),
    ) {
        (None, None) => "-".to_owned(),
        (input, output) => format!("{}/{}", format_usd(input), format_usd(output)),
    }
}

fn format_usd(micros: Option<u64>) -> String {
    match micros {
        None => "-".to_owned(),
        Some(value) => {
            let dollars = value / 1_000_000;
            let frac = (value % 1_000_000) / 10_000;
            format!("{dollars}.{frac:02}")
        }
    }
}

fn format_latency(class: LatencyClass) -> String {
    class.as_str().to_owned()
}

fn fit_width(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let cols = text.chars().count();
    if cols == width {
        return text.to_owned();
    }
    if cols < width {
        let mut out = text.to_owned();
        out.extend(std::iter::repeat_n(' ', width - cols));
        return out;
    }
    text.chars().take(width).collect()
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), ModelSelectError> {
    if cancel.is_cancelled() {
        Err(ModelSelectError::Cancelled)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_router::{
        CatalogConfig, CatalogModelSpec, DataPolicyTag, LatencyClass, ModelCapabilities,
        ModelCatalog, ModelId, ModelPrices, ModelPurpose, PriceTableVersion, ProviderCapEntry,
        ProviderCapIndex, ProviderCapabilities, ProviderId, ReasoningSupport, Region,
        UsageFieldSet,
    };

    const GOLDEN_80: &str = "\
models privacy:no_training pin:anthropic/claude-sonnet
eligible:
> claude-sonnet provider:anthropic caps:tools,stream,vision,cache,reason,json ctx:200000 cost:3.00/15.00 latency:interactive pin:yes
  local-llama provider:local caps:tools,stream,cache,reason,json ctx:8192 cost:- latency:standard pin:no
unavailable:
  ghost provider:missingco caps:tools,stream ctx:32000 cost:- latency:standard pin:no reason:provider_unavailable:provider_missing
  gpt-4o provider:openai caps:tools,stream,vision,cache,reason,json ctx:128000 cost:2.50/10.00 latency:interactive pin:no reason:privacy_denied:no_training
  old-mini provider:openai caps:tools,stream ctx:4096 cost:- latency:batch pin:no reason:disabled
  tiny-embed provider:openai caps:tools,stream,cache,reason,json ctx:1024 cost:0.10/0.40 latency:batch pin:no reason:context_too_small:1024<8000";

    fn cancel() -> CancellationToken {
        CancellationToken::new()
    }

    fn live() -> llm_router::CancellationToken {
        llm_router::CancellationToken::new()
    }

    fn caps(
        context_limit: u32,
        max_output: u32,
        tools: bool,
        vision: bool,
        streaming: bool,
        caching: bool,
        reasoning: ReasoningSupport,
        structured_output: bool,
    ) -> ProviderCapabilities {
        ProviderCapabilities::new(
            tools,
            streaming,
            vision,
            caching,
            reasoning,
            structured_output,
            context_limit,
            max_output,
            UsageFieldSet::new(true, true, true, true, true, false, false),
        )
        .expect("caps")
    }

    fn prices(input: Option<u64>, output: Option<u64>) -> ModelPrices {
        match (input, output) {
            (None, None) => ModelPrices::UNKNOWN,
            (input, output) => ModelPrices::new(
                input,
                output,
                None,
                Some(PriceTableVersion::parse("table-v1").expect("price table")),
            ),
        }
    }

    fn spec(
        provider: &str,
        model: &str,
        enabled: bool,
        capabilities: ProviderCapabilities,
        regions: &[&str],
        tags: &[&str],
        latency: LatencyClass,
        model_prices: ModelPrices,
    ) -> CatalogModelSpec {
        CatalogModelSpec::new(
            ProviderId::parse(provider).expect("provider"),
            ModelId::parse(model).expect("model"),
            enabled,
            capabilities,
            model_prices,
            regions
                .iter()
                .map(|region| Region::parse(*region).expect("region"))
                .collect(),
            tags.iter()
                .map(|tag| DataPolicyTag::parse(*tag).expect("tag"))
                .collect(),
            latency,
        )
    }

    fn catalog(rows: Vec<(CatalogModelSpec, Option<ProviderCapEntry>)>) -> CatalogSnapshot {
        let mut config_rows = Vec::new();
        let mut index = ProviderCapIndex::new();
        for (row, advertised) in rows {
            if let Some(advertised) = advertised {
                let provider = row.provider().clone();
                if index.get(&provider).is_none() {
                    index.insert(provider, advertised).expect("provider cap");
                }
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

    fn available(advertised: ProviderCapabilities) -> ProviderCapEntry {
        ProviderCapEntry::Available(advertised)
    }

    fn pin(provider: &str, model: &str) -> ModelRef {
        ModelRef::new(
            ProviderId::parse(provider).expect("p"),
            ModelId::parse(model).expect("m"),
        )
    }

    fn fixture_catalog() -> CatalogSnapshot {
        let sonnet = caps(
            200_000,
            16_384,
            true,
            true,
            true,
            true,
            ReasoningSupport::Exposed,
            true,
        );
        let local = caps(
            8_192,
            2_048,
            true,
            false,
            true,
            true,
            ReasoningSupport::Exposed,
            true,
        );
        let gpt = caps(
            128_000,
            16_384,
            true,
            true,
            true,
            true,
            ReasoningSupport::Exposed,
            true,
        );
        let mini = caps(
            4_096,
            1_024,
            true,
            false,
            true,
            false,
            ReasoningSupport::None,
            false,
        );
        let tiny = caps(
            1_024,
            256,
            true,
            false,
            true,
            true,
            ReasoningSupport::Exposed,
            true,
        );
        let ghost = caps(
            32_000,
            4_096,
            true,
            false,
            true,
            false,
            ReasoningSupport::None,
            false,
        );
        catalog(vec![
            (
                spec(
                    "anthropic",
                    "claude-sonnet",
                    true,
                    sonnet.clone(),
                    &["us"],
                    &["no-training"],
                    LatencyClass::Interactive,
                    prices(Some(3_000_000), Some(15_000_000)),
                ),
                Some(available(sonnet)),
            ),
            (
                spec(
                    "local",
                    "local-llama",
                    true,
                    local.clone(),
                    &["local"],
                    &["local-only", "no-training"],
                    LatencyClass::Standard,
                    prices(None, None),
                ),
                Some(available(local)),
            ),
            (
                spec(
                    "openai",
                    "gpt-4o",
                    true,
                    gpt.clone(),
                    &["us"],
                    &[],
                    LatencyClass::Interactive,
                    prices(Some(2_500_000), Some(10_000_000)),
                ),
                Some(available(gpt.clone())),
            ),
            (
                spec(
                    "openai",
                    "old-mini",
                    false,
                    mini,
                    &["us"],
                    &[],
                    LatencyClass::Batch,
                    prices(None, None),
                ),
                Some(available(gpt.clone())),
            ),
            (
                spec(
                    "openai",
                    "tiny-embed",
                    true,
                    tiny,
                    &["us"],
                    &["no-training"],
                    LatencyClass::Batch,
                    prices(Some(100_000), Some(400_000)),
                ),
                Some(available(gpt)),
            ),
            (
                spec(
                    "missingco",
                    "ghost",
                    true,
                    ghost,
                    &["us"],
                    &["no-training"],
                    LatencyClass::Standard,
                    prices(None, None),
                ),
                None,
            ),
        ])
    }

    fn fixture_request(user_pin: Option<ModelRef>, required: ModelCapabilities) -> RouteRequest {
        RouteRequest::new(
            ModelPurpose::Code,
            PrivacyClass::NoTraining,
            None,
            8_000,
            1_000,
            500,
            required,
            None,
            None,
            user_pin,
        )
        .expect("request")
    }

    fn fixture_model() -> ModelViewModel {
        ModelViewModel::from_catalog(
            &fixture_catalog(),
            &fixture_request(
                Some(pin("anthropic", "claude-sonnet")),
                ModelCapabilities::new(true, false, false, false, false, false),
            ),
            &cancel(),
        )
        .expect("model")
    }

    #[test]
    fn golden_80_120_200() {
        let model = fixture_model();
        assert_eq!(model.render(80, 24).golden(), GOLDEN_80);
        assert_eq!(model.render(120, 24).golden(), GOLDEN_80);
        assert_eq!(model.render(200, 24).golden(), GOLDEN_80);
        assert_eq!(model.render(80, 24).text().lines().count(), 24);
        assert!(
            model
                .render(80, 24)
                .text()
                .lines()
                .all(|line| line.chars().count() == 80)
        );
    }

    #[test]
    fn lists_provider_caps_context_cost_latency() {
        let model = fixture_model();
        let eligible: Vec<_> = model
            .rows()
            .iter()
            .filter(|row| row.is_eligible())
            .collect();
        assert_eq!(eligible.len(), 2);
        let sonnet = eligible
            .iter()
            .find(|row| row.display_model() == "claude-sonnet")
            .expect("sonnet");
        assert_eq!(sonnet.display_provider(), "anthropic");
        assert_eq!(
            sonnet.capabilities(),
            "tools,stream,vision,cache,reason,json"
        );
        assert_eq!(sonnet.context_limit(), 200_000);
        assert_eq!(sonnet.cost_hint(), "3.00/15.00");
        assert_eq!(sonnet.latency_hint(), "interactive");
        assert!(sonnet.is_pinned());
        let golden = model.render(80, 24).golden();
        assert!(golden.contains("provider:anthropic"));
        assert!(golden.contains("caps:tools,stream,vision,cache,reason,json"));
        assert!(golden.contains("ctx:200000"));
        assert!(golden.contains("cost:3.00/15.00"));
        assert!(golden.contains("latency:interactive"));
        assert!(golden.contains("cost:-"));
        assert!(golden.contains("latency:standard"));
    }

    #[test]
    fn unavailable_reason_is_visible() {
        let model = fixture_model();
        let golden = model.render(80, 24).golden();
        assert!(golden.contains("unavailable:"));
        assert!(golden.contains("reason:privacy_denied:no_training"));
        assert!(golden.contains("reason:disabled"));
        assert!(golden.contains("reason:provider_unavailable:provider_missing"));
        assert!(golden.contains("reason:context_too_small:1024<8000"));
        let gpt = model
            .rows()
            .iter()
            .find(|row| row.display_model() == "gpt-4o")
            .expect("gpt");
        assert_eq!(gpt.availability(), ModelAvailability::Unavailable);
        assert_eq!(gpt.unavailable_reason(), Some("privacy_denied:no_training"));
    }

    #[test]
    fn pin_of_eligible_returns_intent_without_mutating() {
        let model = fixture_model();
        let intent = model
            .pin_model(&pin("local", "local-llama"), &cancel())
            .expect("pin");
        assert_eq!(intent.model(), &pin("local", "local-llama"));
        assert_eq!(intent.privacy(), PrivacyClass::NoTraining);
        assert!(intent.requires_durable_audit());
        assert!(!model.mutates_router());
        assert!(
            !model
                .rows()
                .iter()
                .find(|row| row.display_model() == "local-llama")
                .expect("local")
                .is_pinned()
        );
        assert!(
            model
                .rows()
                .iter()
                .find(|row| row.display_model() == "claude-sonnet")
                .expect("sonnet")
                .is_pinned()
        );
    }

    #[test]
    fn pin_violating_privacy_fails_closed() {
        let model = fixture_model();
        assert_eq!(
            model.pin_model(&pin("openai", "gpt-4o"), &cancel()),
            Err(ModelSelectError::PolicyDenied)
        );
        assert_eq!(
            ModelSelectError::PolicyDenied.as_str(),
            "model pin violates hard privacy/capability policy"
        );
        assert!(
            !model
                .rows()
                .iter()
                .find(|row| row.display_model() == "gpt-4o")
                .expect("gpt")
                .is_pinned()
        );
    }

    #[test]
    fn pin_violating_capability_fails_closed() {
        let model = ModelViewModel::from_catalog(
            &fixture_catalog(),
            &fixture_request(
                Some(pin("anthropic", "claude-sonnet")),
                ModelCapabilities::new(true, false, true, false, false, false),
            ),
            &cancel(),
        )
        .expect("model");
        let local = model
            .rows()
            .iter()
            .find(|row| row.display_model() == "local-llama")
            .expect("local");
        assert_eq!(local.availability(), ModelAvailability::Unavailable);
        assert_eq!(local.unavailable_reason(), Some("missing_vision"));
        assert_eq!(
            model.pin_model(&pin("local", "local-llama"), &cancel()),
            Err(ModelSelectError::PolicyDenied)
        );
    }

    #[test]
    fn pin_of_disabled_or_missing_provider_fails_closed() {
        let model = fixture_model();
        assert_eq!(
            model.pin_model(&pin("openai", "old-mini"), &cancel()),
            Err(ModelSelectError::PolicyDenied)
        );
        assert_eq!(
            model.pin_model(&pin("missingco", "ghost"), &cancel()),
            Err(ModelSelectError::PolicyDenied)
        );
    }

    #[test]
    fn already_pinned_fails_closed() {
        let model = fixture_model();
        assert_eq!(
            model.pin_model(&pin("anthropic", "claude-sonnet"), &cancel()),
            Err(ModelSelectError::AlreadyPinned)
        );
    }

    #[test]
    fn unpin_returns_intent_without_mutating() {
        let model = fixture_model();
        let intent = model
            .unpin_model(&pin("anthropic", "claude-sonnet"), &cancel())
            .expect("unpin");
        assert_eq!(intent.model(), &pin("anthropic", "claude-sonnet"));
        assert!(intent.requires_durable_audit());
        assert!(
            model
                .rows()
                .iter()
                .find(|row| row.display_model() == "claude-sonnet")
                .expect("sonnet")
                .is_pinned()
        );
        assert_eq!(
            model.unpin_model(&pin("local", "local-llama"), &cancel()),
            Err(ModelSelectError::NotPinned)
        );
    }

    #[test]
    fn selection_walks_eligible_then_unavailable() {
        let model = fixture_model();
        assert_eq!(
            model.selected_row().map(ModelRowView::display_model),
            Some("claude-sonnet")
        );
        let next = model.select_next();
        assert_eq!(
            next.selected_row().map(ModelRowView::display_model),
            Some("local-llama")
        );
        let unavailable = next.select_next();
        assert_eq!(
            unavailable.selected_row().map(ModelRowView::display_model),
            Some("ghost")
        );
    }

    #[test]
    fn cancellation_is_honored() {
        let token = CancellationToken::new();
        token.cancel();
        assert_eq!(
            ModelViewModel::from_catalog(
                &fixture_catalog(),
                &fixture_request(None, ModelCapabilities::NONE),
                &token
            ),
            Err(ModelSelectError::Cancelled)
        );
        let model = fixture_model();
        assert_eq!(
            model.pin_model(&pin("local", "local-llama"), &token),
            Err(ModelSelectError::Cancelled)
        );
    }

    #[test]
    fn empty_catalog_renders_empty_groups() {
        let snapshot = catalog(vec![]);
        let model = ModelViewModel::from_catalog(
            &snapshot,
            &fixture_request(None, ModelCapabilities::NONE),
            &cancel(),
        )
        .expect("empty");
        assert!(model.rows().is_empty());
        let golden = model.render(80, 8).golden();
        assert!(golden.contains("eligible:"));
        assert!(golden.contains("unavailable:"));
        assert!(golden.contains("(empty)"));
        assert_eq!(
            model.pin(&cancel()),
            Err(ModelSelectError::InvalidSelection)
        );
    }
}
