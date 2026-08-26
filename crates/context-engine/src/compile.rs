//! Context compiler: hard token partitions with an untouchable output reserve.
//!
//! `compile` packs system/user/goal/diff/retrieved/memory/read-set blocks into
//! a [`ContextPacket`]. Reserved output tokens and the safety margin are never
//! borrowed. Pressure drops the lowest-value optional blocks first; mandatory
//! evidence is kept or the compile fails closed.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::time::{Duration, Instant};

use protocol::ContextItemId;

use crate::index::graph::MAX_LOCATOR_BYTES;
use crate::ingest::content::ContentHash;
use crate::repo_manifest::CancellationToken;
use crate::retrieval::candidates::{Freshness, TrustClass};
use crate::token_estimate::{
    TokenEstimateError, TokenEstimateLimits, TokenEstimator, TokenizerFamily,
};

/// Default wall-clock budget for one compile.
pub const DEFAULT_COMPILE_TIMEOUT: Duration = Duration::from_secs(2);

/// Default included-block cap after budgeting.
pub const DEFAULT_MAX_COMPILE_BLOCKS: usize = 256;

/// Default UTF-8 byte cap for one block body.
pub const DEFAULT_MAX_BLOCK_BYTES: usize = crate::token_estimate::DEFAULT_MAX_ESTIMATE_BYTES;

/// Default safety margin reserved beside the output budget.
pub const DEFAULT_SAFETY_MARGIN: u32 = 64;

/// Default leftover share for retrieved code/docs, in basis points.
pub const DEFAULT_RETRIEVED_SHARE_BPS: u16 = 6_000;

/// Default leftover share for durable memory/rules, in basis points.
pub const DEFAULT_MEMORY_SHARE_BPS: u16 = 2_500;

/// Default leftover share for read-set references, in basis points.
pub const DEFAULT_READ_SET_SHARE_BPS: u16 = 1_500;

const CANCEL_STRIDE: usize = 16;

/// Resource bounds for one [`compile`] call. Zero timeout is an immediate timeout.
#[derive(Clone, Debug)]
pub struct CompileLimits {
    max_blocks: usize,
    max_block_bytes: usize,
    timeout: Duration,
    cancel: CancellationToken,
    retrieved_share_bps: u16,
    memory_share_bps: u16,
    read_set_share_bps: u16,
}

/// Task, budget, and candidate blocks for one compile.
#[derive(Clone, Debug)]
pub struct CompileContext {
    task: String,
    goal: String,
    context_limit: u32,
    output_reserve: u32,
    static_prefix: u32,
    safety_margin: u32,
    tokenizer: TokenizerFamily,
    system: Vec<CompileInput>,
    user: Vec<CompileInput>,
    goal_blocks: Vec<CompileInput>,
    diff: Vec<CompileInput>,
    retrieved: Vec<CompileInput>,
    memory: Vec<CompileInput>,
    read_set: Vec<CompileInput>,
    pin_locators: BTreeSet<String>,
    limits: CompileLimits,
}

/// One candidate block before budgeting.
#[derive(Clone, Debug)]
pub struct CompileInput {
    locator: String,
    text: String,
    content_hash: Option<ContentHash>,
    estimated_tokens: Option<u32>,
    freshness: Freshness,
    trust: Option<TrustClass>,
    reason: Option<CompileReason>,
    score: u32,
}

/// Budgeted, model-visible context item.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextBlock {
    id: ContextItemId,
    locator: String,
    source: ContextSource,
    content_hash: ContentHash,
    text: String,
    estimated_tokens: u32,
    freshness: Freshness,
    trust: TrustClass,
    reason: CompileReason,
    pinned: bool,
}

/// Compiled packet: included blocks, drops, and hard partition usage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextPacket {
    blocks: Vec<ContextBlock>,
    dropped: Vec<DroppedBlock>,
    partitions: TokenPartitions,
}

/// Per-class used/cap accounting. Caps are hard: unused cap is not borrowed
/// by another class, and output reserve is never a class cap.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TokenPartitions {
    system: PartitionBudget,
    user: PartitionBudget,
    goal: PartitionBudget,
    diff: PartitionBudget,
    retrieved: PartitionBudget,
    memory: PartitionBudget,
    read_set: PartitionBudget,
    output_reserve: u32,
    safety_margin: u32,
    context_limit: u32,
    included_tokens: u32,
}

/// Tokens consumed versus the hard cap for one class.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PartitionBudget {
    used: u32,
    cap: u32,
}

/// Optional block excluded by pressure, partition cap, or duplicate identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DroppedBlock {
    locator: String,
    source: ContextSource,
    content_hash: ContentHash,
    estimated_tokens: u32,
    reason: DropReason,
}

/// Block class compiled into a hard partition.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum ContextSource {
    System,
    User,
    Goal,
    Diff,
    Retrieved,
    Memory,
    ReadSet,
}

/// Why a block was selected for the packet.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum CompileReason {
    System,
    User,
    Goal,
    Explicit,
    Diff,
    Error,
    Retrieved,
    Memory,
    ReadSet,
}

/// Why an optional block was omitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum DropReason {
    BudgetPressure,
    PartitionCap,
    Duplicate,
}

/// Typed compile failure. Display never echoes locators, hashes, or text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompileError {
    Cancelled,
    Timeout,
    InvalidBudget,
    InvalidBlock,
    ReservedUntouchable,
    MandatoryExceedsBudget,
    CapacityExceeded,
    TextTooLarge,
    TokenizerFailed,
}

struct Prepared {
    locator: String,
    source: ContextSource,
    content_hash: ContentHash,
    text: String,
    tokens: u32,
    freshness: Freshness,
    trust: TrustClass,
    reason: CompileReason,
    score: u32,
    pinned: bool,
    input_order: u32,
}

#[derive(Clone, Eq, PartialEq, Ord, PartialOrd)]
struct DedupKey {
    locator: String,
    content_hash: ContentHash,
}

impl CompileLimits {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn max_blocks(mut self, value: usize) -> Self {
        self.max_blocks = value;
        self
    }

    pub fn max_block_bytes(mut self, value: usize) -> Self {
        self.max_block_bytes = value;
        self
    }

    pub fn timeout(mut self, value: Duration) -> Self {
        self.timeout = value;
        self
    }

    pub fn cancellation(mut self, value: CancellationToken) -> Self {
        self.cancel = value;
        self
    }

    pub fn retrieved_share_bps(mut self, value: u16) -> Self {
        self.retrieved_share_bps = value;
        self
    }

    pub fn memory_share_bps(mut self, value: u16) -> Self {
        self.memory_share_bps = value;
        self
    }

    pub fn read_set_share_bps(mut self, value: u16) -> Self {
        self.read_set_share_bps = value;
        self
    }

    pub fn max_blocks_value(&self) -> usize {
        self.max_blocks
    }

    pub fn max_block_bytes_value(&self) -> usize {
        self.max_block_bytes
    }

    pub fn timeout_value(&self) -> Duration {
        self.timeout
    }

    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.cancel
    }

    pub fn retrieved_share_bps_value(&self) -> u16 {
        self.retrieved_share_bps
    }

    pub fn memory_share_bps_value(&self) -> u16 {
        self.memory_share_bps
    }

    pub fn read_set_share_bps_value(&self) -> u16 {
        self.read_set_share_bps
    }
}

impl Default for CompileLimits {
    fn default() -> Self {
        Self {
            max_blocks: DEFAULT_MAX_COMPILE_BLOCKS,
            max_block_bytes: DEFAULT_MAX_BLOCK_BYTES,
            timeout: DEFAULT_COMPILE_TIMEOUT,
            cancel: CancellationToken::new(),
            retrieved_share_bps: DEFAULT_RETRIEVED_SHARE_BPS,
            memory_share_bps: DEFAULT_MEMORY_SHARE_BPS,
            read_set_share_bps: DEFAULT_READ_SET_SHARE_BPS,
        }
    }
}

impl CompileContext {
    pub fn new(context_limit: u32, output_reserve: u32) -> Self {
        Self {
            task: String::new(),
            goal: String::new(),
            context_limit,
            output_reserve,
            static_prefix: 0,
            safety_margin: DEFAULT_SAFETY_MARGIN,
            tokenizer: TokenizerFamily::Unknown,
            system: Vec::new(),
            user: Vec::new(),
            goal_blocks: Vec::new(),
            diff: Vec::new(),
            retrieved: Vec::new(),
            memory: Vec::new(),
            read_set: Vec::new(),
            pin_locators: BTreeSet::new(),
            limits: CompileLimits::new(),
        }
    }

    pub fn task(mut self, value: impl Into<String>) -> Self {
        self.task = value.into();
        self
    }

    pub fn goal(mut self, value: impl Into<String>) -> Self {
        self.goal = value.into();
        self
    }

    pub fn static_prefix(mut self, tokens: u32) -> Self {
        self.static_prefix = tokens;
        self
    }

    pub fn safety_margin(mut self, tokens: u32) -> Self {
        self.safety_margin = tokens;
        self
    }

    pub fn tokenizer(mut self, family: TokenizerFamily) -> Self {
        self.tokenizer = family;
        self
    }

    pub fn limits(mut self, limits: CompileLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn system(mut self, block: CompileInput) -> Self {
        self.system.push(block);
        self
    }

    pub fn user(mut self, block: CompileInput) -> Self {
        self.user.push(block);
        self
    }

    pub fn goal_block(mut self, block: CompileInput) -> Self {
        self.goal_blocks.push(block);
        self
    }

    pub fn diff(mut self, block: CompileInput) -> Self {
        self.diff.push(block);
        self
    }

    pub fn retrieved(mut self, block: CompileInput) -> Self {
        self.retrieved.push(block);
        self
    }

    pub fn memory(mut self, block: CompileInput) -> Self {
        self.memory.push(block);
        self
    }

    pub fn read_set(mut self, block: CompileInput) -> Self {
        self.read_set.push(block);
        self
    }

    /// Include `block` as retrieved and mark its locator pinned (mandatory).
    pub fn pin(mut self, block: CompileInput) -> Self {
        self.pin_locators.insert(block.locator.clone());
        self.retrieved.push(block);
        self
    }

    pub fn pin_locator(mut self, locator: impl Into<String>) -> Self {
        self.pin_locators.insert(locator.into());
        self
    }

    pub fn task_text(&self) -> &str {
        &self.task
    }

    pub fn goal_text(&self) -> &str {
        &self.goal
    }

    pub fn context_limit(&self) -> u32 {
        self.context_limit
    }

    pub fn output_reserve(&self) -> u32 {
        self.output_reserve
    }

    pub fn static_prefix_value(&self) -> u32 {
        self.static_prefix
    }

    pub fn safety_margin_value(&self) -> u32 {
        self.safety_margin
    }

    pub fn tokenizer_family(&self) -> TokenizerFamily {
        self.tokenizer
    }

    pub fn limits_value(&self) -> &CompileLimits {
        &self.limits
    }

    pub fn pin_locators(&self) -> impl Iterator<Item = &str> {
        self.pin_locators.iter().map(String::as_str)
    }
}

impl CompileInput {
    pub fn new(locator: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            locator: locator.into(),
            text: text.into(),
            content_hash: None,
            estimated_tokens: None,
            freshness: Freshness::Unknown,
            trust: None,
            reason: None,
            score: 0,
        }
    }

    pub fn content_hash(mut self, value: ContentHash) -> Self {
        self.content_hash = Some(value);
        self
    }

    pub fn tokens(mut self, value: u32) -> Self {
        self.estimated_tokens = Some(value);
        self
    }

    pub fn freshness(mut self, value: Freshness) -> Self {
        self.freshness = value;
        self
    }

    pub fn trust(mut self, value: TrustClass) -> Self {
        self.trust = Some(value);
        self
    }

    pub fn reason(mut self, value: CompileReason) -> Self {
        self.reason = Some(value);
        self
    }

    pub fn score(mut self, value: u32) -> Self {
        self.score = value;
        self
    }

    pub fn locator(&self) -> &str {
        &self.locator
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

impl ContextBlock {
    pub fn id(&self) -> ContextItemId {
        self.id
    }

    pub fn locator(&self) -> &str {
        &self.locator
    }

    pub fn source(&self) -> ContextSource {
        self.source
    }

    pub fn content_hash(&self) -> ContentHash {
        self.content_hash
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn estimated_tokens(&self) -> u32 {
        self.estimated_tokens
    }

    pub fn freshness(&self) -> Freshness {
        self.freshness
    }

    pub fn trust(&self) -> TrustClass {
        self.trust
    }

    pub fn reason(&self) -> CompileReason {
        self.reason
    }

    pub fn is_pinned(&self) -> bool {
        self.pinned
    }

    pub fn is_mandatory(&self) -> bool {
        self.pinned || self.source.is_mandatory()
    }
}

impl ContextPacket {
    pub fn blocks(&self) -> &[ContextBlock] {
        &self.blocks
    }

    pub fn dropped(&self) -> &[DroppedBlock] {
        &self.dropped
    }

    pub fn partitions(&self) -> &TokenPartitions {
        &self.partitions
    }

    pub fn included_tokens(&self) -> u32 {
        self.partitions.included_tokens
    }

    pub fn reserved_output(&self) -> u32 {
        self.partitions.output_reserve
    }

    pub fn remaining_output(&self) -> u32 {
        self.partitions.output_reserve
    }
}

/// Partition and drop metrics for `/context` explainability.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextExplain {
    included: u32,
    dropped: u32,
    included_tokens: u32,
    reserved_output: u32,
    drop_budget_pressure: u32,
    drop_partition_cap: u32,
    drop_duplicate: u32,
}

impl ContextExplain {
    pub fn included(&self) -> u32 {
        self.included
    }

    pub fn dropped(&self) -> u32 {
        self.dropped
    }

    pub fn included_tokens(&self) -> u32 {
        self.included_tokens
    }

    pub fn reserved_output(&self) -> u32 {
        self.reserved_output
    }

    pub fn drop_budget_pressure(&self) -> u32 {
        self.drop_budget_pressure
    }

    pub fn drop_partition_cap(&self) -> u32 {
        self.drop_partition_cap
    }

    pub fn drop_duplicate(&self) -> u32 {
        self.drop_duplicate
    }
}

/// Deterministic explainability metrics. Never includes block text.
pub fn explain_packet(packet: &ContextPacket) -> ContextExplain {
    let mut drop_budget_pressure = 0u32;
    let mut drop_partition_cap = 0u32;
    let mut drop_duplicate = 0u32;
    for dropped in packet.dropped() {
        match dropped.reason() {
            DropReason::BudgetPressure => {
                drop_budget_pressure = drop_budget_pressure.saturating_add(1)
            }
            DropReason::PartitionCap => drop_partition_cap = drop_partition_cap.saturating_add(1),
            DropReason::Duplicate => drop_duplicate = drop_duplicate.saturating_add(1),
        }
    }
    ContextExplain {
        included: packet.blocks().len() as u32,
        dropped: packet.dropped().len() as u32,
        included_tokens: packet.included_tokens(),
        reserved_output: packet.reserved_output(),
        drop_budget_pressure,
        drop_partition_cap,
        drop_duplicate,
    }
}

impl TokenPartitions {
    pub fn system(&self) -> PartitionBudget {
        self.system
    }

    pub fn user(&self) -> PartitionBudget {
        self.user
    }

    pub fn goal(&self) -> PartitionBudget {
        self.goal
    }

    pub fn diff(&self) -> PartitionBudget {
        self.diff
    }

    pub fn retrieved(&self) -> PartitionBudget {
        self.retrieved
    }

    pub fn memory(&self) -> PartitionBudget {
        self.memory
    }

    pub fn read_set(&self) -> PartitionBudget {
        self.read_set
    }

    pub fn output_reserve(&self) -> u32 {
        self.output_reserve
    }

    pub fn safety_margin(&self) -> u32 {
        self.safety_margin
    }

    pub fn context_limit(&self) -> u32 {
        self.context_limit
    }

    pub fn included_tokens(&self) -> u32 {
        self.included_tokens
    }

    /// Tokens occupied by included blocks plus untouchable reserves.
    pub fn occupied(&self) -> u32 {
        let unused_system = self.system.cap.saturating_sub(self.system.used);
        self.included_tokens
            .saturating_add(unused_system)
            .saturating_add(self.output_reserve)
            .saturating_add(self.safety_margin)
    }

    pub fn by_source(&self, source: ContextSource) -> PartitionBudget {
        match source {
            ContextSource::System => self.system,
            ContextSource::User => self.user,
            ContextSource::Goal => self.goal,
            ContextSource::Diff => self.diff,
            ContextSource::Retrieved => self.retrieved,
            ContextSource::Memory => self.memory,
            ContextSource::ReadSet => self.read_set,
        }
    }
}

impl PartitionBudget {
    pub const fn used(self) -> u32 {
        self.used
    }

    pub const fn cap(self) -> u32 {
        self.cap
    }
}

impl DroppedBlock {
    pub fn locator(&self) -> &str {
        &self.locator
    }

    pub fn source(&self) -> ContextSource {
        self.source
    }

    pub fn content_hash(&self) -> ContentHash {
        self.content_hash
    }

    pub fn estimated_tokens(&self) -> u32 {
        self.estimated_tokens
    }

    pub fn reason(&self) -> DropReason {
        self.reason
    }
}

impl ContextSource {
    pub const ALL: &'static [Self] = &[
        Self::System,
        Self::User,
        Self::Goal,
        Self::Diff,
        Self::Retrieved,
        Self::Memory,
        Self::ReadSet,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Goal => "goal",
            Self::Diff => "diff",
            Self::Retrieved => "retrieved",
            Self::Memory => "memory",
            Self::ReadSet => "read_set",
        }
    }

    pub const fn is_mandatory(self) -> bool {
        matches!(self, Self::System | Self::User | Self::Goal | Self::Diff)
    }

    pub const fn is_optional(self) -> bool {
        !self.is_mandatory()
    }

    fn assemble_rank(self) -> u8 {
        match self {
            Self::System => 0,
            Self::User => 1,
            Self::Goal => 2,
            Self::Diff => 3,
            Self::Retrieved => 4,
            Self::Memory => 5,
            Self::ReadSet => 6,
        }
    }

    fn keep_rank(self) -> u8 {
        match self {
            Self::System => 6,
            Self::User => 5,
            Self::Goal => 4,
            Self::Diff => 3,
            Self::Retrieved => 2,
            Self::Memory => 1,
            Self::ReadSet => 0,
        }
    }

    fn default_score(self) -> u32 {
        match self {
            Self::System | Self::User | Self::Goal | Self::Diff => u32::MAX,
            Self::Retrieved => 500,
            Self::Memory => 300,
            Self::ReadSet => 200,
        }
    }

    fn default_trust(self) -> TrustClass {
        match self {
            Self::System | Self::User | Self::Goal | Self::Memory => TrustClass::Project,
            Self::Diff | Self::Retrieved | Self::ReadSet => TrustClass::Untrusted,
        }
    }

    fn default_reason(self) -> CompileReason {
        match self {
            Self::System => CompileReason::System,
            Self::User => CompileReason::User,
            Self::Goal => CompileReason::Goal,
            Self::Diff => CompileReason::Diff,
            Self::Retrieved => CompileReason::Retrieved,
            Self::Memory => CompileReason::Memory,
            Self::ReadSet => CompileReason::ReadSet,
        }
    }
}

impl CompileReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Goal => "goal",
            Self::Explicit => "explicit",
            Self::Diff => "diff",
            Self::Error => "error",
            Self::Retrieved => "retrieved",
            Self::Memory => "memory",
            Self::ReadSet => "read_set",
        }
    }
}

impl DropReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BudgetPressure => "budget_pressure",
            Self::PartitionCap => "partition_cap",
            Self::Duplicate => "duplicate",
        }
    }
}

impl CompileError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::InvalidBudget => "invalid_budget",
            Self::InvalidBlock => "invalid_block",
            Self::ReservedUntouchable => "reserved_untouchable",
            Self::MandatoryExceedsBudget => "mandatory_exceeds_budget",
            Self::CapacityExceeded => "capacity_exceeded",
            Self::TextTooLarge => "text_too_large",
            Self::TokenizerFailed => "tokenizer_failed",
        }
    }
}

impl fmt::Display for ContextSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for CompileReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for DropReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for CompileError {}

/// Compile `request` into a budgeted [`ContextPacket`].
///
/// Reserved output and the safety margin are subtracted first and are never
/// available to blocks. Optional blocks are admitted only inside their hard
/// leftover partitions. Pressure drops the lowest-value optional item first.
pub fn compile(request: &CompileContext) -> Result<ContextPacket, CompileError> {
    let started = Instant::now();
    check_ready(request, started)?;
    validate_budget(request)?;

    let mut prepared = prepare_blocks(request, started)?;
    check_ready(request, started)?;
    let mut dropped = dedup_prepared(&mut prepared);

    let system_used = sum_source(&prepared, ContextSource::System)?;
    let system_reserved = system_used.max(request.static_prefix);
    let reserved = add_tokens(request.output_reserve, request.safety_margin)?;
    let reserved = add_tokens(reserved, system_reserved)?;
    if reserved > request.context_limit {
        return Err(CompileError::ReservedUntouchable);
    }
    let after_reserved = request.context_limit - reserved;

    let mut mandatory_tokens = 0u32;
    for item in &prepared {
        if is_mandatory(item) && item.source != ContextSource::System {
            mandatory_tokens = add_tokens(mandatory_tokens, item.tokens)?;
        }
    }
    if mandatory_tokens > after_reserved {
        return Err(CompileError::MandatoryExceedsBudget);
    }
    let leftover = after_reserved - mandatory_tokens;

    let (retrieved_share, memory_share, read_set_share) = split_leftover(
        leftover,
        request.limits.retrieved_share_bps,
        request.limits.memory_share_bps,
        request.limits.read_set_share_bps,
    );
    let pinned_retrieved = sum_pinned(&prepared, ContextSource::Retrieved)?;
    let pinned_memory = sum_pinned(&prepared, ContextSource::Memory)?;
    let pinned_read_set = sum_pinned(&prepared, ContextSource::ReadSet)?;

    let retrieved_cap = add_tokens(retrieved_share, pinned_retrieved)?;
    let memory_cap = add_tokens(memory_share, pinned_memory)?;
    let read_set_cap = add_tokens(read_set_share, pinned_read_set)?;

    apply_optional_caps(
        &mut prepared,
        &mut dropped,
        ContextSource::Retrieved,
        retrieved_cap,
    );
    apply_optional_caps(
        &mut prepared,
        &mut dropped,
        ContextSource::Memory,
        memory_cap,
    );
    apply_optional_caps(
        &mut prepared,
        &mut dropped,
        ContextSource::ReadSet,
        read_set_cap,
    );

    if prepared.len() > request.limits.max_blocks {
        return Err(CompileError::CapacityExceeded);
    }

    prepared.sort_by(assemble_cmp);
    let blocks = prepared.iter().map(to_context_block).collect::<Vec<_>>();
    let included_tokens = sum_tokens(blocks.iter().map(|b| b.estimated_tokens))?;

    let user_used = sum_source(&prepared, ContextSource::User)?;
    let goal_used = sum_source(&prepared, ContextSource::Goal)?;
    let diff_used = sum_source(&prepared, ContextSource::Diff)?;
    let retrieved_used = sum_source(&prepared, ContextSource::Retrieved)?;
    let memory_used = sum_source(&prepared, ContextSource::Memory)?;
    let read_set_used = sum_source(&prepared, ContextSource::ReadSet)?;

    let partitions = TokenPartitions {
        system: PartitionBudget {
            used: system_used,
            cap: system_reserved,
        },
        user: PartitionBudget {
            used: user_used,
            cap: user_used,
        },
        goal: PartitionBudget {
            used: goal_used,
            cap: goal_used,
        },
        diff: PartitionBudget {
            used: diff_used,
            cap: diff_used,
        },
        retrieved: PartitionBudget {
            used: retrieved_used,
            cap: retrieved_cap,
        },
        memory: PartitionBudget {
            used: memory_used,
            cap: memory_cap,
        },
        read_set: PartitionBudget {
            used: read_set_used,
            cap: read_set_cap,
        },
        output_reserve: request.output_reserve,
        safety_margin: request.safety_margin,
        context_limit: request.context_limit,
        included_tokens,
    };

    if partitions.occupied() > request.context_limit {
        return Err(CompileError::ReservedUntouchable);
    }

    dropped.sort_by(|a, b| {
        a.source
            .assemble_rank()
            .cmp(&b.source.assemble_rank())
            .then(a.locator.cmp(&b.locator))
            .then(a.content_hash.cmp(&b.content_hash))
    });

    Ok(ContextPacket {
        blocks,
        dropped,
        partitions,
    })
}

fn validate_budget(request: &CompileContext) -> Result<(), CompileError> {
    if request.context_limit == 0 {
        return Err(CompileError::InvalidBudget);
    }
    let reserved = request
        .output_reserve
        .checked_add(request.safety_margin)
        .ok_or(CompileError::InvalidBudget)?;
    if reserved > request.context_limit {
        return Err(CompileError::ReservedUntouchable);
    }
    let share_total = u32::from(request.limits.retrieved_share_bps)
        .saturating_add(u32::from(request.limits.memory_share_bps))
        .saturating_add(u32::from(request.limits.read_set_share_bps));
    if share_total == 0 {
        return Err(CompileError::InvalidBudget);
    }
    Ok(())
}

fn prepare_blocks(
    request: &CompileContext,
    started: Instant,
) -> Result<Vec<Prepared>, CompileError> {
    let mut estimator = None;
    let mut out = Vec::new();
    let mut order = 0u32;

    let mut sections: Vec<(ContextSource, Vec<CompileInput>)> = vec![
        (ContextSource::System, request.system.clone()),
        (ContextSource::User, request.user.clone()),
        (ContextSource::Goal, request.goal_blocks.clone()),
        (ContextSource::Diff, request.diff.clone()),
        (ContextSource::Retrieved, request.retrieved.clone()),
        (ContextSource::Memory, request.memory.clone()),
        (ContextSource::ReadSet, request.read_set.clone()),
    ];

    if request.user.is_empty() && !request.task.is_empty() {
        sections[1]
            .1
            .push(CompileInput::new("task", request.task.clone()));
    }
    if request.goal_blocks.is_empty() && !request.goal.is_empty() {
        sections[2]
            .1
            .push(CompileInput::new("goal", request.goal.clone()));
    }

    for (source, blocks) in sections {
        for (step, input) in blocks.into_iter().enumerate() {
            if step == 0 || step.is_multiple_of(CANCEL_STRIDE) {
                check_ready(request, started)?;
            }
            let prepared = prepare_one(request, source, input, order, &mut estimator, started)?;
            order = order.saturating_add(1);
            out.push(prepared);
        }
    }
    Ok(out)
}

fn prepare_one(
    request: &CompileContext,
    source: ContextSource,
    input: CompileInput,
    input_order: u32,
    estimator: &mut Option<TokenEstimator>,
    started: Instant,
) -> Result<Prepared, CompileError> {
    validate_locator(&input.locator)?;
    if input.text.len() > request.limits.max_block_bytes {
        return Err(CompileError::TextTooLarge);
    }
    check_ready(request, started)?;

    let content_hash = input
        .content_hash
        .unwrap_or_else(|| ContentHash::from_bytes(input.text.as_bytes()));
    let tokens = match input.estimated_tokens {
        Some(tokens) => {
            if !input.text.is_empty() && tokens == 0 {
                1
            } else {
                tokens
            }
        }
        None => estimate_tokens(request, &input.text, estimator)?,
    };
    let pinned = request.pin_locators.contains(&input.locator);
    let reason = if pinned && source.is_optional() {
        input.reason.unwrap_or(CompileReason::Explicit)
    } else {
        input.reason.unwrap_or_else(|| source.default_reason())
    };
    let score = if input.score == 0 {
        source.default_score()
    } else {
        input.score
    };

    Ok(Prepared {
        locator: input.locator,
        source,
        content_hash,
        text: input.text,
        tokens,
        freshness: input.freshness,
        trust: input.trust.unwrap_or_else(|| source.default_trust()),
        reason,
        score,
        pinned,
        input_order,
    })
}

fn estimate_tokens(
    request: &CompileContext,
    text: &str,
    estimator: &mut Option<TokenEstimator>,
) -> Result<u32, CompileError> {
    if estimator.is_none() {
        *estimator = Some(TokenEstimator::with_limits(
            TokenEstimateLimits::new()
                .max_text_bytes(request.limits.max_block_bytes)
                .timeout(request.limits.timeout)
                .cancellation(request.limits.cancel.clone()),
        ));
    }
    let estimator = estimator.as_mut().ok_or(CompileError::TokenizerFailed)?;
    match estimator.estimate(request.tokenizer, text) {
        Ok(estimate) => Ok(estimate.tokens()),
        Err(TokenEstimateError::Cancelled) => Err(CompileError::Cancelled),
        Err(TokenEstimateError::Timeout) => Err(CompileError::Timeout),
        Err(TokenEstimateError::TextTooLarge) => Err(CompileError::TextTooLarge),
        Err(TokenEstimateError::TokenizerFailed) => Err(CompileError::TokenizerFailed),
    }
}

fn validate_locator(locator: &str) -> Result<(), CompileError> {
    if locator.is_empty() || locator.len() > MAX_LOCATOR_BYTES || locator.contains('\0') {
        return Err(CompileError::InvalidBlock);
    }
    Ok(())
}

fn dedup_prepared(items: &mut Vec<Prepared>) -> Vec<DroppedBlock> {
    let mut best: BTreeMap<DedupKey, Prepared> = BTreeMap::new();
    let mut dropped = Vec::new();
    let mut pending = Vec::new();
    std::mem::swap(items, &mut pending);

    for item in pending {
        let key = DedupKey {
            locator: item.locator.clone(),
            content_hash: item.content_hash,
        };
        match best.remove(&key) {
            None => {
                best.insert(key, item);
            }
            Some(existing) => {
                if keep_preferred(&item, &existing) {
                    dropped.push(to_dropped(&existing, DropReason::Duplicate));
                    best.insert(key, item);
                } else {
                    dropped.push(to_dropped(&item, DropReason::Duplicate));
                    best.insert(key, existing);
                }
            }
        }
    }

    *items = best.into_values().collect();
    dropped
}

fn keep_preferred(left: &Prepared, right: &Prepared) -> bool {
    match (is_mandatory(left), is_mandatory(right)) {
        (true, false) => return true,
        (false, true) => return false,
        _ => {}
    }
    match left.source.keep_rank().cmp(&right.source.keep_rank()) {
        Ordering::Greater => true,
        Ordering::Less => false,
        Ordering::Equal => match left.score.cmp(&right.score) {
            Ordering::Greater => true,
            Ordering::Less => false,
            Ordering::Equal => left.input_order < right.input_order,
        },
    }
}

fn apply_optional_caps(
    items: &mut Vec<Prepared>,
    dropped: &mut Vec<DroppedBlock>,
    source: ContextSource,
    cap: u32,
) {
    let mut class = Vec::new();
    let mut rest = Vec::new();
    for item in items.drain(..) {
        if item.source == source {
            class.push(item);
        } else {
            rest.push(item);
        }
    }

    let mut mandatory = Vec::new();
    let mut optional = Vec::new();
    for item in class {
        if is_mandatory(&item) {
            mandatory.push(item);
        } else {
            optional.push(item);
        }
    }

    let mut used = 0u32;
    let mut kept = Vec::new();
    for item in mandatory {
        used = used.saturating_add(item.tokens);
        kept.push(item);
    }

    optional.sort_by(drop_cmp);
    // Walk highest-value first so leftover cap keeps the best optional items.
    for item in optional.into_iter().rev() {
        let next = used.saturating_add(item.tokens);
        if next > cap {
            dropped.push(to_dropped(&item, DropReason::PartitionCap));
            continue;
        }
        used = next;
        kept.push(item);
    }

    rest.extend(kept);
    *items = rest;
}

fn is_mandatory(item: &Prepared) -> bool {
    item.pinned || item.source.is_mandatory()
}

fn drop_cmp(left: &Prepared, right: &Prepared) -> Ordering {
    left.score
        .cmp(&right.score)
        .then(freshness_value(left.freshness).cmp(&freshness_value(right.freshness)))
        .then(right.tokens.cmp(&left.tokens))
        .then(left.locator.cmp(&right.locator))
        .then(left.content_hash.cmp(&right.content_hash))
        .then(left.input_order.cmp(&right.input_order))
}

fn assemble_cmp(left: &Prepared, right: &Prepared) -> Ordering {
    left.source
        .assemble_rank()
        .cmp(&right.source.assemble_rank())
        .then(right.score.cmp(&left.score))
        .then(left.input_order.cmp(&right.input_order))
        .then(left.locator.cmp(&right.locator))
}

fn freshness_value(freshness: Freshness) -> u8 {
    match freshness {
        Freshness::Stale => 0,
        Freshness::Unknown => 1,
        Freshness::Fresh => 2,
    }
}

fn split_leftover(
    leftover: u32,
    retrieved_bps: u16,
    memory_bps: u16,
    read_set_bps: u16,
) -> (u32, u32, u32) {
    let total = u32::from(retrieved_bps)
        .saturating_add(u32::from(memory_bps))
        .saturating_add(u32::from(read_set_bps));
    if leftover == 0 || total == 0 {
        return (0, 0, 0);
    }
    let memory = leftover.saturating_mul(u32::from(memory_bps)) / total;
    let read_set = leftover.saturating_mul(u32::from(read_set_bps)) / total;
    let retrieved = leftover.saturating_sub(memory).saturating_sub(read_set);
    (retrieved, memory, read_set)
}

fn sum_source(items: &[Prepared], source: ContextSource) -> Result<u32, CompileError> {
    sum_tokens(
        items
            .iter()
            .filter(|item| item.source == source)
            .map(|item| item.tokens),
    )
}

fn sum_pinned(items: &[Prepared], source: ContextSource) -> Result<u32, CompileError> {
    sum_tokens(
        items
            .iter()
            .filter(|item| item.source == source && is_mandatory(item))
            .map(|item| item.tokens),
    )
}

fn sum_tokens<I>(tokens: I) -> Result<u32, CompileError>
where
    I: IntoIterator<Item = u32>,
{
    let mut total = 0u32;
    for value in tokens {
        total = add_tokens(total, value)?;
    }
    Ok(total)
}

fn add_tokens(left: u32, right: u32) -> Result<u32, CompileError> {
    left.checked_add(right)
        .ok_or(CompileError::MandatoryExceedsBudget)
}

fn to_context_block(item: &Prepared) -> ContextBlock {
    ContextBlock {
        id: ContextItemId::new(),
        locator: item.locator.clone(),
        source: item.source,
        content_hash: item.content_hash,
        text: item.text.clone(),
        estimated_tokens: item.tokens,
        freshness: item.freshness,
        trust: item.trust,
        reason: item.reason,
        pinned: item.pinned,
    }
}

fn to_dropped(item: &Prepared, reason: DropReason) -> DroppedBlock {
    DroppedBlock {
        locator: item.locator.clone(),
        source: item.source,
        content_hash: item.content_hash,
        estimated_tokens: item.tokens,
        reason,
    }
}

fn check_ready(request: &CompileContext, started: Instant) -> Result<(), CompileError> {
    if request.limits.cancel.is_cancelled() {
        return Err(CompileError::Cancelled);
    }
    if request.limits.timeout.is_zero() || started.elapsed() > request.limits.timeout {
        return Err(CompileError::Timeout);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(locator: &str, tokens: u32) -> CompileInput {
        CompileInput::new(locator, locator).tokens(tokens)
    }

    fn request(limit: u32, reserve: u32) -> CompileContext {
        CompileContext::new(limit, reserve).safety_margin(0)
    }

    fn locators(packet: &ContextPacket) -> Vec<&str> {
        packet.blocks().iter().map(ContextBlock::locator).collect()
    }

    fn dropped_locators(packet: &ContextPacket) -> Vec<&str> {
        packet.dropped().iter().map(DroppedBlock::locator).collect()
    }

    fn assert_block_metadata(block: &ContextBlock) {
        assert!(!block.locator().is_empty());
        assert_eq!(
            block.content_hash(),
            ContentHash::from_bytes(block.text().as_bytes())
        );
        assert!(block.estimated_tokens() > 0 || block.text().is_empty());
        let _ = block.source();
        let _ = block.trust();
        let _ = block.reason();
        let _ = block.freshness();
    }

    #[test]
    fn compile_refuses_to_borrow_reserved_output_budget() {
        let req = request(100, 40)
            .static_prefix(10)
            .safety_margin(10)
            .user(block("user", 20))
            .goal_block(block("goal", 15))
            .diff(block("err", 20));
        // allocatable = 100 - 40 - 10 - 10 = 40; mandatory user+goal+diff = 55.
        assert_eq!(
            compile(&req).expect_err("must not borrow reserve"),
            CompileError::MandatoryExceedsBudget
        );

        let fits = request(100, 40)
            .static_prefix(10)
            .safety_margin(10)
            .user(block("user", 20))
            .goal_block(block("goal", 10))
            .diff(block("err", 10));
        let packet = compile(&fits).expect("fits without reserve");
        assert_eq!(packet.reserved_output(), 40);
        assert_eq!(packet.remaining_output(), 40);
        assert_eq!(packet.included_tokens(), 40);
        assert_eq!(packet.partitions().occupied(), 100);
        assert!(packet.partitions().occupied() <= 100);
        assert_eq!(
            packet.included_tokens() + packet.reserved_output() + 10 + 10,
            100
        );
    }

    #[test]
    fn reserve_plus_safety_plus_prefix_never_available() {
        assert_eq!(
            compile(&request(50, 30).safety_margin(25)),
            Err(CompileError::ReservedUntouchable)
        );
        assert_eq!(
            compile(&request(50, 20).static_prefix(40).safety_margin(0)),
            Err(CompileError::ReservedUntouchable)
        );
        assert_eq!(
            compile(&CompileContext::new(0, 0).safety_margin(0)),
            Err(CompileError::InvalidBudget)
        );
    }

    #[test]
    fn every_included_block_has_source_hash_trust_tokens_reason_freshness() {
        let req = request(500, 50)
            .task("fix the failing test")
            .goal("all tests green")
            .system(block("sys", 8))
            .diff(
                block("src/lib.rs:10-20", 12)
                    .reason(CompileReason::Error)
                    .freshness(Freshness::Fresh),
            )
            .retrieved(
                block("src/main.rs:1-40", 20)
                    .score(80)
                    .freshness(Freshness::Stale),
            )
            .memory(block("prefers-exact-ranges", 6).score(40))
            .read_set(block("src/lib.rs:1-8", 4).freshness(Freshness::Fresh));
        let packet = compile(&req).expect("compile");
        assert!(!packet.blocks().is_empty());
        for block in packet.blocks() {
            assert_block_metadata(block);
        }
        let sources: BTreeSet<_> = packet.blocks().iter().map(ContextBlock::source).collect();
        assert!(sources.contains(&ContextSource::System));
        assert!(sources.contains(&ContextSource::User));
        assert!(sources.contains(&ContextSource::Goal));
        assert!(sources.contains(&ContextSource::Diff));
        assert!(sources.contains(&ContextSource::Retrieved));
        assert!(sources.contains(&ContextSource::Memory));
        assert!(sources.contains(&ContextSource::ReadSet));
        let error = packet
            .blocks()
            .iter()
            .find(|b| b.locator() == "src/lib.rs:10-20")
            .expect("diff");
        assert_eq!(error.reason(), CompileReason::Error);
        assert_eq!(error.trust(), TrustClass::Untrusted);
        assert_eq!(error.freshness(), Freshness::Fresh);
        assert!(error.is_mandatory());
    }

    #[test]
    fn pressure_drops_lowest_value_optional_before_mandatory_evidence() {
        let req = request(100, 20)
            .safety_margin(0)
            .static_prefix(0)
            .user(block("user", 10))
            .goal_block(block("goal", 10))
            .diff(block("failing-test", 20).reason(CompileReason::Error))
            .retrieved(block("high", 20).score(90).freshness(Freshness::Fresh))
            .retrieved(block("mid", 20).score(50).freshness(Freshness::Unknown))
            .retrieved(block("low", 20).score(10).freshness(Freshness::Stale))
            .memory(block("note", 10).score(5))
            .read_set(block("old-read", 10).score(1));
        // leftover after mandatory 40 = 40. shares 60/25/15 -> 24/10/6.
        let packet = compile(&req).expect("pressure");
        assert!(locators(&packet).contains(&"user"));
        assert!(locators(&packet).contains(&"goal"));
        assert!(locators(&packet).contains(&"failing-test"));
        assert!(locators(&packet).contains(&"high"));
        assert!(!locators(&packet).contains(&"low"));
        assert!(dropped_locators(&packet).contains(&"low"));
        assert!(
            packet
                .dropped()
                .iter()
                .any(|d| d.locator() == "low" && d.reason() == DropReason::PartitionCap)
        );
        let first = compile(&req).expect("first");
        let second = compile(&req).expect("second");
        assert_eq!(locators(&first), locators(&second));
        assert_eq!(dropped_locators(&first), dropped_locators(&second));
        assert!(first.partitions().occupied() <= 100);
        assert_eq!(first.remaining_output(), 20);
    }

    #[test]
    fn drop_order_is_score_then_freshness_then_size() {
        let req = request(80, 20)
            .retrieved(
                block("stale-same", 15)
                    .score(40)
                    .freshness(Freshness::Stale),
            )
            .retrieved(
                block("fresh-same", 15)
                    .score(40)
                    .freshness(Freshness::Fresh),
            )
            .retrieved(block("big-low", 25).score(10).freshness(Freshness::Fresh));
        // leftover = 60. retrieved share = 36. keep fresh-same (15) + stale-same (15) = 30;
        // big-low is lowest score.
        let packet = compile(&req).expect("order");
        assert!(locators(&packet).contains(&"fresh-same"));
        assert!(locators(&packet).contains(&"stale-same"));
        assert!(!locators(&packet).contains(&"big-low"));
    }

    #[test]
    fn pin_promotes_optional_to_mandatory() {
        let req = request(100, 20)
            .user(block("user", 10))
            .retrieved(block("keep-pin", 30).score(1))
            .retrieved(block("other", 30).score(90))
            .pin_locator("keep-pin");
        let packet = compile(&req).expect("pin");
        let pinned = packet
            .blocks()
            .iter()
            .find(|b| b.locator() == "keep-pin")
            .expect("pinned");
        assert!(pinned.is_pinned());
        assert!(pinned.is_mandatory());
        assert_eq!(pinned.reason(), CompileReason::Explicit);
        assert!(locators(&packet).contains(&"keep-pin"));
    }

    #[test]
    fn pin_that_exceeds_capacity_fails_closed() {
        let req = request(50, 20)
            .user(block("user", 10))
            .pin(block("huge-pin", 30).score(1));
        assert_eq!(
            compile(&req).expect_err("pin overflow"),
            CompileError::MandatoryExceedsBudget
        );
    }

    #[test]
    fn unused_system_prefix_is_not_borrowed_by_retrieved() {
        let req = request(100, 20)
            .static_prefix(40)
            .system(block("sys", 10))
            .retrieved(block("code", 50).score(100));
        let packet = compile(&req).expect("prefix");
        assert_eq!(packet.partitions().system().used(), 10);
        assert_eq!(packet.partitions().system().cap(), 40);
        // leftover after reserved 20+40 = 40, retrieved share 24. 50 > 24.
        assert!(!locators(&packet).contains(&"code"));
        assert_eq!(packet.remaining_output(), 20);
        assert!(packet.partitions().occupied() <= 100);
    }

    #[test]
    fn optional_classes_cannot_steal_each_others_partition() {
        let req = request(100, 20)
            .memory(block("mem-a", 20).score(10))
            .memory(block("mem-b", 20).score(9))
            .retrieved(block("tiny", 5).score(1));
        // leftover 80 -> retrieved 48, memory 20, read-set 12.
        let packet = compile(&req).expect("hard optional");
        assert_eq!(packet.partitions().memory().cap(), 20);
        assert!(packet.partitions().memory().used() <= 20);
        assert!(locators(&packet).contains(&"mem-a"));
        assert!(!locators(&packet).contains(&"mem-b"));
        assert!(locators(&packet).contains(&"tiny"));
    }

    #[test]
    fn duplicate_locator_and_hash_keeps_mandatory() {
        let hash = ContentHash::from_bytes(b"same");
        let req = request(200, 20)
            .diff(
                CompileInput::new("src/a.rs", "same")
                    .content_hash(hash)
                    .tokens(8),
            )
            .retrieved(
                CompileInput::new("src/a.rs", "same")
                    .content_hash(hash)
                    .tokens(8)
                    .score(99),
            );
        let packet = compile(&req).expect("dedup");
        let copies: Vec<_> = packet
            .blocks()
            .iter()
            .filter(|b| b.locator() == "src/a.rs")
            .collect();
        assert_eq!(copies.len(), 1);
        assert_eq!(copies[0].source(), ContextSource::Diff);
        assert!(
            packet
                .dropped()
                .iter()
                .any(|d| d.locator() == "src/a.rs" && d.reason() == DropReason::Duplicate)
        );
    }

    #[test]
    fn token_estimate_fills_missing_counts() {
        let req = request(400, 40).user(CompileInput::new("user", "hello world from the compiler"));
        let packet = compile(&req).expect("estimate");
        let user = &packet.blocks()[0];
        assert!(user.estimated_tokens() >= 1);
        assert_eq!(
            user.content_hash(),
            ContentHash::from_bytes(b"hello world from the compiler")
        );
    }

    #[test]
    fn cancelled_and_zero_timeout_fail_closed() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let cancelled = request(100, 10).limits(CompileLimits::new().cancellation(cancel));
        assert_eq!(compile(&cancelled), Err(CompileError::Cancelled));

        let timed_out = request(100, 10).limits(CompileLimits::new().timeout(Duration::ZERO));
        assert_eq!(compile(&timed_out), Err(CompileError::Timeout));
    }

    #[test]
    fn invalid_locator_and_oversized_text_fail() {
        assert_eq!(
            compile(&request(100, 10).user(CompileInput::new("", "x").tokens(1))),
            Err(CompileError::InvalidBlock)
        );
        let too_big = request(100, 10)
            .limits(CompileLimits::new().max_block_bytes(2))
            .user(CompileInput::new("u", "abcd").tokens(1));
        assert_eq!(compile(&too_big), Err(CompileError::TextTooLarge));
    }

    #[test]
    fn packet_ordering_is_partition_then_score() {
        let req = request(400, 20)
            .read_set(block("r", 4).score(1))
            .memory(block("m", 4).score(1))
            .retrieved(block("low-ret", 4).score(10))
            .retrieved(block("high-ret", 4).score(80))
            .diff(block("d", 4))
            .goal_block(block("g", 4))
            .user(block("u", 4))
            .system(block("s", 4));
        let packet = compile(&req).expect("order");
        let sources: Vec<_> = packet.blocks().iter().map(ContextBlock::source).collect();
        assert_eq!(
            sources,
            [
                ContextSource::System,
                ContextSource::User,
                ContextSource::Goal,
                ContextSource::Diff,
                ContextSource::Retrieved,
                ContextSource::Retrieved,
                ContextSource::Memory,
                ContextSource::ReadSet,
            ]
        );
        let retrieved: Vec<_> = packet
            .blocks()
            .iter()
            .filter(|b| b.source() == ContextSource::Retrieved)
            .map(ContextBlock::locator)
            .collect();
        assert_eq!(retrieved, ["high-ret", "low-ret"]);
    }

    #[test]
    fn default_repo_blocks_stay_untrusted() {
        let packet = compile(
            &request(200, 20)
                .diff(block("diff", 4))
                .retrieved(block("ret", 4))
                .read_set(block("rs", 4)),
        )
        .expect("trust");
        for block in packet.blocks() {
            assert_eq!(block.trust(), TrustClass::Untrusted);
        }
    }

    #[test]
    fn leftover_share_remainder_is_assigned_to_retrieved() {
        let req = request(103, 20)
            .safety_margin(0)
            .static_prefix(0)
            .user(block("user", 10))
            .goal_block(block("goal", 10));
        let packet = compile(&req).expect("shares");
        assert_eq!(packet.partitions().memory().cap(), 15);
        assert_eq!(packet.partitions().read_set().cap(), 9);
        assert_eq!(packet.partitions().retrieved().cap(), 39);
        assert_eq!(
            packet.partitions().retrieved().cap()
                + packet.partitions().memory().cap()
                + packet.partitions().read_set().cap(),
            63
        );
        assert_eq!(packet.reserved_output(), 20);
    }

    #[test]
    fn zero_optional_share_bps_is_invalid_budget() {
        let req = request(100, 10).limits(
            CompileLimits::new()
                .retrieved_share_bps(0)
                .memory_share_bps(0)
                .read_set_share_bps(0),
        );
        assert_eq!(compile(&req), Err(CompileError::InvalidBudget));
    }

    #[test]
    fn protected_categories_are_never_dropped_under_pressure() {
        let req = request(100, 20)
            .safety_margin(0)
            .static_prefix(0)
            .system(block("role-rules", 8))
            .user(block("task", 10))
            .goal_block(block("criteria", 10))
            .diff(block("blocker", 20).reason(CompileReason::Error))
            .retrieved(block("noise-a", 40).score(1))
            .retrieved(block("noise-b", 40).score(1))
            .memory(block("old-note", 20).score(1));
        let packet = compile(&req).expect("protected");
        let kept = locators(&packet);
        assert!(kept.contains(&"role-rules"));
        assert!(kept.contains(&"task"));
        assert!(kept.contains(&"criteria"));
        assert!(kept.contains(&"blocker"));
        for block in packet.blocks() {
            if block.is_mandatory() && !block.is_pinned() {
                assert!(
                    !packet
                        .dropped()
                        .iter()
                        .any(|d| d.locator() == block.locator()
                            && d.reason() != DropReason::Duplicate)
                );
            }
        }
        assert!(packet.dropped().iter().any(|d| {
            matches!(
                d.reason(),
                DropReason::PartitionCap | DropReason::BudgetPressure
            ) && (d.locator() == "noise-a" || d.locator() == "noise-b" || d.locator() == "old-note")
        }));
    }
}
