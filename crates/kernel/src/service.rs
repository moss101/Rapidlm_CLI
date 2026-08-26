//! Kernel service lifecycle, health snapshots, and dependency declaration.
//!
//! Services declare a stable [`ServiceId`] and an explicit dependency list.
//! [`validate_dependency_graph`] rejects cycles and unknown/duplicate ids
//! before any start order is computed. Health is a lock-free snapshot.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use protocol::TraceContext;
use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};

use crate::CancellationToken;

/// Health snapshot schema written on every constructed snapshot.
pub const HEALTH_SNAPSHOT_SCHEMA: u16 = 1;

/// Maximum UTF-8 bytes accepted in a [`ServiceId`].
pub const MAX_SERVICE_ID_BYTES: usize = 64;

/// Maximum services accepted in one dependency graph.
pub const MAX_SERVICES: usize = 64;

/// Maximum declared dependencies per service.
pub const MAX_DEPENDENCIES: usize = 32;

const CANCEL_CHECK_EVERY: usize = 16;
const HEALTH_SNAPSHOT_FIELDS: &[&str] = &["schema", "service", "status", "last_error", "since"];

/// Stable kernel service identifier. Comparison uses the parsed form only.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ServiceId(String);

/// Failure when parsing a [`ServiceId`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceIdError {
    Empty,
    TooLong,
    Invalid,
}

/// Declared service identity plus the services it requires at start.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceDecl {
    id: ServiceId,
    dependencies: Vec<ServiceId>,
}

/// Typed lifecycle/declaration failure. Messages never include source chains.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServiceError {
    Cancelled,
    DeadlineExceeded,
    InvalidId,
    DuplicateDependency { id: ServiceId },
    UnknownDependency { id: ServiceId },
    DuplicateService { id: ServiceId },
    DependencyCycle { cycle: Vec<ServiceId> },
    TooManyServices,
    TooManyDependencies,
    Failed,
}

/// Observable lifecycle state stored on a health snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ServiceStatus {
    Stopped,
    Starting,
    Running,
    Quiescing,
    Degraded,
    Failed,
}

/// Sanitized last-error class on a health snapshot. Never a raw cause string.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ServiceFailureKind {
    Cancelled,
    DeadlineExceeded,
    InvalidDeclaration,
    UnknownDependency,
    DuplicateService,
    DependencyCycle,
    BoundExceeded,
    Failed,
}

/// `ServiceHealth { status, last_error, since }` from the kernel contract.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceHealth {
    status: ServiceStatus,
    last_error: Option<ServiceFailureKind>,
    since: u64,
}

/// Serializable, non-blocking health observation for one service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HealthSnapshot {
    schema: u16,
    service: ServiceId,
    health: ServiceHealth,
}

/// Lock-free publisher for [`ServiceHealth`] snapshots.
#[derive(Debug)]
pub struct HealthState {
    status: AtomicU8,
    last_error: AtomicU8,
    since: AtomicU64,
}

/// Start context: cancellation and trace correlation only.
///
/// Ledger, telemetry, clock, and artifact handles are later kernel services
/// and are not invented here.
#[derive(Clone, Debug)]
pub struct ServiceContext {
    cancel: CancellationToken,
    trace: TraceContext,
}

/// Kernel service lifecycle. Implementors own their health atomics.
///
/// `health` is synchronous and must not block on I/O or locks. `start`,
/// `quiesce`, and `stop` honor [`CancellationToken`] / the quiesce deadline.
pub trait LifecycleService: Send + Sync {
    fn id(&self) -> &ServiceId;
    fn dependencies(&self) -> &[ServiceId];

    fn start(&self, ctx: ServiceContext) -> impl Future<Output = Result<(), ServiceError>> + Send;

    fn quiesce(&self, deadline: Instant) -> impl Future<Output = Result<(), ServiceError>> + Send;

    fn stop(&self) -> impl Future<Output = Result<(), ServiceError>> + Send;

    fn health(&self) -> HealthSnapshot;
}

/// Reject cycles, unknown dependencies, and duplicate ids.
///
/// On success, returns a deterministic topological order: a service appears
/// only after every service it depends on.
pub fn validate_dependency_graph(
    decls: &[ServiceDecl],
    cancel: &CancellationToken,
) -> Result<Vec<ServiceId>, ServiceError> {
    cancel.check_service()?;
    if decls.len() > MAX_SERVICES {
        return Err(ServiceError::TooManyServices);
    }

    let mut by_id: BTreeMap<&ServiceId, &ServiceDecl> = BTreeMap::new();
    for (i, decl) in decls.iter().enumerate() {
        if i.is_multiple_of(CANCEL_CHECK_EVERY) {
            cancel.check_service()?;
        }
        if decl.dependencies.len() > MAX_DEPENDENCIES {
            return Err(ServiceError::TooManyDependencies);
        }
        if by_id.insert(decl.id(), decl).is_some() {
            return Err(ServiceError::DuplicateService {
                id: decl.id().clone(),
            });
        }
    }

    let mut outgoing: BTreeMap<&ServiceId, Vec<&ServiceId>> = BTreeMap::new();
    let mut indegree: BTreeMap<&ServiceId, usize> = BTreeMap::new();
    for id in by_id.keys() {
        outgoing.insert(*id, Vec::new());
        indegree.insert(*id, 0);
    }

    for (i, decl) in decls.iter().enumerate() {
        if i.is_multiple_of(CANCEL_CHECK_EVERY) {
            cancel.check_service()?;
        }
        let mut seen_deps = BTreeSet::new();
        for dep in decl.dependencies() {
            if !by_id.contains_key(dep) {
                return Err(ServiceError::UnknownDependency { id: dep.clone() });
            }
            if !seen_deps.insert(dep) {
                return Err(ServiceError::DuplicateDependency { id: dep.clone() });
            }
            outgoing
                .get_mut(dep)
                .expect("dep present in outgoing after closed-graph check")
                .push(decl.id());
            *indegree
                .get_mut(decl.id())
                .expect("decl id present in indegree") += 1;
        }
    }

    let mut ready: Vec<&ServiceId> = indegree
        .iter()
        .filter_map(|(id, deg)| (*deg == 0).then_some(*id))
        .collect();
    ready.sort();

    let mut order = Vec::with_capacity(decls.len());
    let mut remaining = indegree;
    while !ready.is_empty() {
        cancel.check_service()?;
        let id = ready.remove(0);
        order.push(id.clone());
        remaining.remove(id);
        let mut unlocked = Vec::new();
        if let Some(dependents) = outgoing.get(id) {
            for (i, dependent) in dependents.iter().enumerate() {
                if i.is_multiple_of(CANCEL_CHECK_EVERY) {
                    cancel.check_service()?;
                }
                let Some(deg) = remaining.get_mut(dependent) else {
                    continue;
                };
                *deg -= 1;
                if *deg == 0 {
                    unlocked.push(*dependent);
                }
            }
        }
        unlocked.sort();
        ready.extend(unlocked);
    }

    if !remaining.is_empty() {
        let leftover: BTreeSet<ServiceId> = remaining.keys().map(|id| (*id).clone()).collect();
        let adj: BTreeMap<ServiceId, Vec<ServiceId>> = leftover
            .iter()
            .map(|id| {
                let next = outgoing
                    .get(id)
                    .into_iter()
                    .flatten()
                    .filter(|dep| leftover.contains(dep))
                    .map(|dep| (*dep).clone())
                    .collect();
                (id.clone(), next)
            })
            .collect();
        return Err(ServiceError::DependencyCycle {
            cycle: find_cycle(&leftover, &adj),
        });
    }

    Ok(order)
}

impl ServiceId {
    pub fn parse(raw: impl AsRef<str>) -> Result<Self, ServiceIdError> {
        let raw = raw.as_ref();
        if raw.is_empty() {
            return Err(ServiceIdError::Empty);
        }
        if raw.len() > MAX_SERVICE_ID_BYTES {
            return Err(ServiceIdError::TooLong);
        }
        let mut chars = raw.chars();
        let Some(first) = chars.next() else {
            return Err(ServiceIdError::Empty);
        };
        if !first.is_ascii_lowercase() {
            return Err(ServiceIdError::Invalid);
        }
        let mut prev_hyphen = false;
        for ch in chars {
            match ch {
                'a'..='z' | '0'..='9' => prev_hyphen = false,
                '-' if !prev_hyphen => prev_hyphen = true,
                _ => return Err(ServiceIdError::Invalid),
            }
        }
        if prev_hyphen {
            return Err(ServiceIdError::Invalid);
        }
        Ok(Self(raw.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ServiceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ServiceId {
    type Err = ServiceIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for ServiceId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ServiceId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(raw).map_err(de::Error::custom)
    }
}

impl ServiceDecl {
    pub fn new(
        id: ServiceId,
        dependencies: impl IntoIterator<Item = ServiceId>,
    ) -> Result<Self, ServiceError> {
        let mut seen = BTreeSet::new();
        let mut deps = Vec::new();
        for dep in dependencies {
            if !seen.insert(dep.clone()) {
                return Err(ServiceError::DuplicateDependency { id: dep });
            }
            deps.push(dep);
            if deps.len() > MAX_DEPENDENCIES {
                return Err(ServiceError::TooManyDependencies);
            }
        }
        Ok(Self {
            id,
            dependencies: deps,
        })
    }

    pub fn from_service(service: &impl LifecycleService) -> Result<Self, ServiceError> {
        Self::new(service.id().clone(), service.dependencies().iter().cloned())
    }

    pub fn id(&self) -> &ServiceId {
        &self.id
    }

    pub fn dependencies(&self) -> &[ServiceId] {
        &self.dependencies
    }
}

impl ServiceError {
    pub fn failure_kind(&self) -> ServiceFailureKind {
        match self {
            Self::Cancelled => ServiceFailureKind::Cancelled,
            Self::DeadlineExceeded => ServiceFailureKind::DeadlineExceeded,
            Self::InvalidId | Self::DuplicateDependency { .. } => {
                ServiceFailureKind::InvalidDeclaration
            }
            Self::UnknownDependency { .. } => ServiceFailureKind::UnknownDependency,
            Self::DuplicateService { .. } => ServiceFailureKind::DuplicateService,
            Self::DependencyCycle { .. } => ServiceFailureKind::DependencyCycle,
            Self::TooManyServices | Self::TooManyDependencies => ServiceFailureKind::BoundExceeded,
            Self::Failed => ServiceFailureKind::Failed,
        }
    }
}

impl fmt::Display for ServiceIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "service id must be non-empty",
            Self::TooLong => "service id exceeds the byte bound",
            Self::Invalid => "service id must be lowercase kebab-case",
        })
    }
}

impl Error for ServiceIdError {}

impl fmt::Display for ServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("service operation cancelled"),
            Self::DeadlineExceeded => f.write_str("service quiesce deadline exceeded"),
            Self::InvalidId => f.write_str("service id is invalid"),
            Self::DuplicateDependency { id } => {
                write!(f, "duplicate dependency {id}")
            }
            Self::UnknownDependency { id } => write!(f, "unknown dependency {id}"),
            Self::DuplicateService { id } => write!(f, "duplicate service {id}"),
            Self::DependencyCycle { cycle } => {
                f.write_str("service dependency cycle:")?;
                for id in cycle {
                    write!(f, " {id}")?;
                }
                Ok(())
            }
            Self::TooManyServices => f.write_str("service graph exceeds the service bound"),
            Self::TooManyDependencies => f.write_str("service exceeds the dependency bound"),
            Self::Failed => f.write_str("service failed"),
        }
    }
}

impl Error for ServiceError {}

impl ServiceStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Quiescing => "quiescing",
            Self::Degraded => "degraded",
            Self::Failed => "failed",
        }
    }

    const fn as_u8(self) -> u8 {
        match self {
            Self::Stopped => 1,
            Self::Starting => 2,
            Self::Running => 3,
            Self::Quiescing => 4,
            Self::Degraded => 5,
            Self::Failed => 6,
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Stopped,
            2 => Self::Starting,
            3 => Self::Running,
            4 => Self::Quiescing,
            5 => Self::Degraded,
            6 => Self::Failed,
            _ => Self::Failed,
        }
    }
}

impl fmt::Display for ServiceStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for ServiceStatus {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ServiceStatus {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match String::deserialize(deserializer)?.as_str() {
            "stopped" => Ok(Self::Stopped),
            "starting" => Ok(Self::Starting),
            "running" => Ok(Self::Running),
            "quiescing" => Ok(Self::Quiescing),
            "degraded" => Ok(Self::Degraded),
            "failed" => Ok(Self::Failed),
            other => Err(de::Error::unknown_variant(
                other,
                &[
                    "stopped",
                    "starting",
                    "running",
                    "quiescing",
                    "degraded",
                    "failed",
                ],
            )),
        }
    }
}

impl ServiceFailureKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::DeadlineExceeded => "deadline_exceeded",
            Self::InvalidDeclaration => "invalid_declaration",
            Self::UnknownDependency => "unknown_dependency",
            Self::DuplicateService => "duplicate_service",
            Self::DependencyCycle => "dependency_cycle",
            Self::BoundExceeded => "bound_exceeded",
            Self::Failed => "failed",
        }
    }

    const fn as_u8(self) -> u8 {
        match self {
            Self::Cancelled => 1,
            Self::DeadlineExceeded => 2,
            Self::InvalidDeclaration => 3,
            Self::UnknownDependency => 4,
            Self::DuplicateService => 5,
            Self::DependencyCycle => 6,
            Self::BoundExceeded => 7,
            Self::Failed => 8,
        }
    }

    fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => None,
            1 => Some(Self::Cancelled),
            2 => Some(Self::DeadlineExceeded),
            3 => Some(Self::InvalidDeclaration),
            4 => Some(Self::UnknownDependency),
            5 => Some(Self::DuplicateService),
            6 => Some(Self::DependencyCycle),
            7 => Some(Self::BoundExceeded),
            8 => Some(Self::Failed),
            _ => Some(Self::Failed),
        }
    }
}

impl fmt::Display for ServiceFailureKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for ServiceFailureKind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ServiceFailureKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match String::deserialize(deserializer)?.as_str() {
            "cancelled" => Ok(Self::Cancelled),
            "deadline_exceeded" => Ok(Self::DeadlineExceeded),
            "invalid_declaration" => Ok(Self::InvalidDeclaration),
            "unknown_dependency" => Ok(Self::UnknownDependency),
            "duplicate_service" => Ok(Self::DuplicateService),
            "dependency_cycle" => Ok(Self::DependencyCycle),
            "bound_exceeded" => Ok(Self::BoundExceeded),
            "failed" => Ok(Self::Failed),
            other => Err(de::Error::unknown_variant(
                other,
                &[
                    "cancelled",
                    "deadline_exceeded",
                    "invalid_declaration",
                    "unknown_dependency",
                    "duplicate_service",
                    "dependency_cycle",
                    "bound_exceeded",
                    "failed",
                ],
            )),
        }
    }
}

impl ServiceHealth {
    pub fn new(status: ServiceStatus, last_error: Option<ServiceFailureKind>, since: u64) -> Self {
        Self {
            status,
            last_error,
            since,
        }
    }

    pub fn status(&self) -> ServiceStatus {
        self.status
    }

    pub fn last_error(&self) -> Option<ServiceFailureKind> {
        self.last_error
    }

    pub fn since(&self) -> u64 {
        self.since
    }
}

impl HealthSnapshot {
    pub fn new(service: ServiceId, health: ServiceHealth) -> Self {
        Self {
            schema: HEALTH_SNAPSHOT_SCHEMA,
            service,
            health,
        }
    }

    pub fn schema(&self) -> u16 {
        self.schema
    }

    pub fn service(&self) -> &ServiceId {
        &self.service
    }

    pub fn health(&self) -> &ServiceHealth {
        &self.health
    }

    pub fn status(&self) -> ServiceStatus {
        self.health.status
    }

    pub fn last_error(&self) -> Option<ServiceFailureKind> {
        self.health.last_error
    }

    pub fn since(&self) -> u64 {
        self.health.since
    }
}

impl Serialize for HealthSnapshot {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("HealthSnapshot", 5)?;
        state.serialize_field("schema", &self.schema)?;
        state.serialize_field("service", &self.service)?;
        state.serialize_field("status", &self.health.status)?;
        state.serialize_field("last_error", &self.health.last_error)?;
        state.serialize_field("since", &self.health.since)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for HealthSnapshot {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_struct("HealthSnapshot", HEALTH_SNAPSHOT_FIELDS, HealthVisitor)
    }
}

struct HealthVisitor;

#[derive(Clone, Copy)]
enum HealthField {
    Schema,
    Service,
    Status,
    LastError,
    Since,
}

impl HealthField {
    fn from_str(value: &str) -> Option<Self> {
        match value {
            "schema" => Some(Self::Schema),
            "service" => Some(Self::Service),
            "status" => Some(Self::Status),
            "last_error" => Some(Self::LastError),
            "since" => Some(Self::Since),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for HealthField {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_identifier(HealthFieldVisitor)
    }
}

struct HealthFieldVisitor;

impl Visitor<'_> for HealthFieldVisitor {
    type Value = HealthField;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a HealthSnapshot field")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        HealthField::from_str(value).ok_or_else(|| E::unknown_field(value, HEALTH_SNAPSHOT_FIELDS))
    }
}

impl<'de> Visitor<'de> for HealthVisitor {
    type Value = HealthSnapshot;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a kernel HealthSnapshot object")
    }

    fn visit_map<M: MapAccess<'de>>(self, mut access: M) -> Result<Self::Value, M::Error> {
        let mut schema = None;
        let mut service = None;
        let mut status = None;
        let mut last_error = None;
        let mut since = None;

        while let Some(field) = access.next_key()? {
            match field {
                HealthField::Schema => assign_once(&mut schema, access.next_value()?, "schema")?,
                HealthField::Service => assign_once(&mut service, access.next_value()?, "service")?,
                HealthField::Status => assign_once(&mut status, access.next_value()?, "status")?,
                HealthField::LastError => {
                    assign_once(&mut last_error, access.next_value()?, "last_error")?;
                }
                HealthField::Since => assign_once(&mut since, access.next_value()?, "since")?,
            }
        }

        let schema: u16 = schema.ok_or_else(|| de::Error::missing_field("schema"))?;
        if schema != HEALTH_SNAPSHOT_SCHEMA {
            return Err(de::Error::custom("unsupported health snapshot schema"));
        }
        let service = service.ok_or_else(|| de::Error::missing_field("service"))?;
        let status = status.ok_or_else(|| de::Error::missing_field("status"))?;
        let last_error = last_error.ok_or_else(|| de::Error::missing_field("last_error"))?;
        let since = since.ok_or_else(|| de::Error::missing_field("since"))?;

        Ok(HealthSnapshot::new(
            service,
            ServiceHealth::new(status, last_error, since),
        ))
    }
}

impl HealthState {
    pub fn new(status: ServiceStatus, since: u64) -> Self {
        Self {
            status: AtomicU8::new(status.as_u8()),
            last_error: AtomicU8::new(0),
            since: AtomicU64::new(since),
        }
    }

    /// Publish a new snapshot. Store-only; never waits on another snapshot.
    pub fn record(
        &self,
        status: ServiceStatus,
        last_error: Option<ServiceFailureKind>,
        since: u64,
    ) {
        self.status.store(status.as_u8(), Ordering::Release);
        self.last_error.store(
            last_error.map(ServiceFailureKind::as_u8).unwrap_or(0),
            Ordering::Release,
        );
        self.since.store(since, Ordering::Release);
    }

    /// Load a copy without locking. Safe to call from `health()`.
    pub fn snapshot(&self) -> ServiceHealth {
        let since = self.since.load(Ordering::Acquire);
        let status = ServiceStatus::from_u8(self.status.load(Ordering::Acquire));
        let last_error = ServiceFailureKind::from_u8(self.last_error.load(Ordering::Acquire));
        ServiceHealth::new(status, last_error, since)
    }
}

impl ServiceContext {
    pub fn new(cancel: CancellationToken, trace: TraceContext) -> Self {
        Self { cancel, trace }
    }

    pub fn cancel(&self) -> &CancellationToken {
        &self.cancel
    }

    pub fn trace(&self) -> &TraceContext {
        &self.trace
    }

    pub fn check(&self) -> Result<(), ServiceError> {
        self.cancel.check_service()
    }

    pub fn now_unix_ms(&self) -> u64 {
        unix_now_ms()
    }
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn assign_once<T, E: de::Error>(
    slot: &mut Option<T>,
    value: T,
    field: &'static str,
) -> Result<(), E> {
    if slot.is_some() {
        Err(E::duplicate_field(field))
    } else {
        *slot = Some(value);
        Ok(())
    }
}

fn find_cycle(
    leftover: &BTreeSet<ServiceId>,
    adj: &BTreeMap<ServiceId, Vec<ServiceId>>,
) -> Vec<ServiceId> {
    let mut stack = Vec::new();
    let mut on_stack = BTreeSet::new();
    let mut seen = BTreeSet::new();

    fn visit(
        node: &ServiceId,
        leftover: &BTreeSet<ServiceId>,
        adj: &BTreeMap<ServiceId, Vec<ServiceId>>,
        stack: &mut Vec<ServiceId>,
        on_stack: &mut BTreeSet<ServiceId>,
        seen: &mut BTreeSet<ServiceId>,
    ) -> Option<Vec<ServiceId>> {
        if !leftover.contains(node) {
            return None;
        }
        if on_stack.contains(node) {
            let start = stack
                .iter()
                .position(|id| id == node)
                .expect("node on_stack is present on the DFS stack");
            let mut cycle = stack[start..].to_vec();
            cycle.push(node.clone());
            return Some(cycle);
        }
        if !seen.insert(node.clone()) {
            return None;
        }
        stack.push(node.clone());
        on_stack.insert(node.clone());
        if let Some(nexts) = adj.get(node) {
            for next in nexts {
                if let Some(cycle) = visit(next, leftover, adj, stack, on_stack, seen) {
                    return Some(cycle);
                }
            }
        }
        stack.pop();
        on_stack.remove(node);
        None
    }

    for id in leftover {
        if let Some(cycle) = visit(id, leftover, adj, &mut stack, &mut on_stack, &mut seen) {
            return cycle;
        }
    }
    leftover.iter().cloned().collect()
}

trait CancellationTokenExt {
    fn check_service(&self) -> Result<(), ServiceError>;
}

impl CancellationTokenExt for CancellationToken {
    fn check_service(&self) -> Result<(), ServiceError> {
        if self.is_cancelled() {
            Err(ServiceError::Cancelled)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::task::{Context, Poll, Waker};
    use std::thread;
    use std::time::Duration;

    const GOLDEN_FAILED: &str = r#"{"schema":1,"service":"event-ledger","status":"failed","last_error":"cancelled","since":0}"#;

    struct FakeService {
        id: ServiceId,
        deps: Vec<ServiceId>,
        health: HealthState,
    }

    impl FakeService {
        fn new(id: &str, deps: &[&str]) -> Self {
            Self {
                id: ServiceId::parse(id).expect("id"),
                deps: deps
                    .iter()
                    .map(|d| ServiceId::parse(*d).expect("dep"))
                    .collect(),
                health: HealthState::new(ServiceStatus::Stopped, 0),
            }
        }
    }

    impl LifecycleService for FakeService {
        fn id(&self) -> &ServiceId {
            &self.id
        }

        fn dependencies(&self) -> &[ServiceId] {
            &self.deps
        }

        async fn start(&self, ctx: ServiceContext) -> Result<(), ServiceError> {
            ctx.check()?;
            self.health
                .record(ServiceStatus::Running, None, ctx.now_unix_ms());
            Ok(())
        }

        async fn quiesce(&self, deadline: Instant) -> Result<(), ServiceError> {
            if Instant::now() >= deadline {
                self.health.record(
                    ServiceStatus::Failed,
                    Some(ServiceFailureKind::DeadlineExceeded),
                    unix_now_ms(),
                );
                return Err(ServiceError::DeadlineExceeded);
            }
            self.health
                .record(ServiceStatus::Quiescing, None, unix_now_ms());
            Ok(())
        }

        async fn stop(&self) -> Result<(), ServiceError> {
            self.health
                .record(ServiceStatus::Stopped, None, unix_now_ms());
            Ok(())
        }

        fn health(&self) -> HealthSnapshot {
            HealthSnapshot::new(self.id.clone(), self.health.snapshot())
        }
    }

    fn sid(raw: &str) -> ServiceId {
        ServiceId::parse(raw).expect("service id")
    }

    fn decl(id: &str, deps: &[&str]) -> ServiceDecl {
        ServiceDecl::new(sid(id), deps.iter().copied().map(sid)).expect("decl")
    }

    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    fn block_on<F: Future>(fut: F) -> F::Output {
        let waker = Waker::noop();
        let mut cx = Context::from_waker(&waker);
        let mut fut = std::pin::pin!(fut);
        loop {
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(out) => return out,
                Poll::Pending => thread::yield_now(),
            }
        }
    }

    #[test]
    fn acyclic_graph_returns_dependency_first_order() {
        let decls = [
            decl("config", &[]),
            decl("event-ledger", &["config"]),
            decl("policy", &["event-ledger", "config"]),
        ];
        let order = validate_dependency_graph(&decls, &live()).expect("acyclic");
        let pos = |name: &str| {
            order
                .iter()
                .position(|id| id.as_str() == name)
                .expect("present")
        };
        assert!(pos("config") < pos("event-ledger"));
        assert!(pos("event-ledger") < pos("policy"));
        assert!(pos("config") < pos("policy"));
    }

    #[test]
    fn two_node_cycle_is_rejected() {
        let decls = [decl("alpha", &["beta"]), decl("beta", &["alpha"])];
        match validate_dependency_graph(&decls, &live()) {
            Err(ServiceError::DependencyCycle { cycle }) => {
                assert!(cycle.windows(2).any(|w| w[0] == w[1] || cycle.len() >= 3));
                let names: BTreeSet<_> = cycle.iter().map(ServiceId::as_str).collect();
                assert!(names.contains("alpha"));
                assert!(names.contains("beta"));
            }
            other => panic!("expected cycle, got {other:?}"),
        }
    }

    #[test]
    fn three_node_cycle_is_rejected() {
        let decls = [decl("a", &["c"]), decl("b", &["a"]), decl("c", &["b"])];
        let err = validate_dependency_graph(&decls, &live()).expect_err("cycle");
        match err {
            ServiceError::DependencyCycle { cycle } => {
                assert!(cycle.len() >= 3);
                let names: BTreeSet<_> = cycle.iter().map(ServiceId::as_str).collect();
                assert_eq!(names, ["a", "b", "c"].into_iter().collect::<BTreeSet<_>>());
            }
            other => panic!("expected cycle, got {other:?}"),
        }
    }

    #[test]
    fn self_dependency_is_a_cycle() {
        let decls = [decl("loop", &["loop"])];
        match validate_dependency_graph(&decls, &live()) {
            Err(ServiceError::DependencyCycle { cycle }) => {
                assert!(cycle.iter().all(|id| id.as_str() == "loop"));
                assert!(!cycle.is_empty());
            }
            other => panic!("expected cycle, got {other:?}"),
        }
    }

    #[test]
    fn unknown_and_duplicate_ids_are_rejected() {
        let missing = [decl("kernel", &["event-ledger"])];
        assert!(matches!(
            validate_dependency_graph(&missing, &live()),
            Err(ServiceError::UnknownDependency { id }) if id.as_str() == "event-ledger"
        ));

        let dup = [decl("config", &[]), decl("config", &[])];
        assert!(matches!(
            validate_dependency_graph(&dup, &live()),
            Err(ServiceError::DuplicateService { id }) if id.as_str() == "config"
        ));
    }

    #[test]
    fn cancelled_validation_fails_closed() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err =
            validate_dependency_graph(&[decl("config", &[])], &cancel).expect_err("cancelled");
        assert_eq!(err, ServiceError::Cancelled);
    }

    #[test]
    fn service_id_rejects_invalid_forms() {
        for sample in [
            "",
            "Config",
            "-x",
            "x-",
            "x--y",
            "has_underscore",
            "has.dot",
            " ",
        ] {
            assert!(ServiceId::parse(sample).is_err(), "accepted {sample:?}");
        }
        assert_eq!(
            ServiceId::parse("x".repeat(MAX_SERVICE_ID_BYTES + 1)),
            Err(ServiceIdError::TooLong)
        );
        assert_eq!(
            ServiceId::parse("event-ledger").unwrap().as_str(),
            "event-ledger"
        );
    }

    #[test]
    fn health_snapshot_round_trips_and_omits_cause_strings() {
        let snap = HealthSnapshot::new(
            sid("event-ledger"),
            ServiceHealth::new(
                ServiceStatus::Failed,
                Some(ServiceFailureKind::Cancelled),
                0,
            ),
        );
        let json = serde_json::to_string(&snap).expect("serialize");
        assert_eq!(json, GOLDEN_FAILED);
        let decoded: HealthSnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, snap);
        assert!(!json.contains("password"));
        assert!(!json.contains("source"));
        assert!(!json.contains("backtrace"));
        assert_eq!(decoded.last_error(), Some(ServiceFailureKind::Cancelled));
    }

    #[test]
    fn health_snapshot_is_non_blocking_under_contention() {
        let state = Arc::new(HealthState::new(ServiceStatus::Starting, 1));
        let stop = Arc::new(AtomicBool::new(false));
        let writers: Vec<_> = (0..4_u64)
            .map(|i| {
                let state = Arc::clone(&state);
                let stop = Arc::clone(&stop);
                thread::spawn(move || {
                    let mut n = i;
                    while !stop.load(Ordering::Relaxed) {
                        state.record(ServiceStatus::Running, None, n);
                        n = n.wrapping_add(4);
                    }
                })
            })
            .collect();

        let deadline = Instant::now() + Duration::from_millis(250);
        for _ in 0..50_000 {
            let health = state.snapshot();
            let snap = HealthSnapshot::new(sid("config"), health);
            let _ = serde_json::to_string(&snap).expect("serialize");
            assert!(
                Instant::now() < deadline,
                "health snapshot blocked under contention"
            );
        }

        stop.store(true, Ordering::Relaxed);
        for writer in writers {
            writer.join().expect("writer");
        }
    }

    #[test]
    fn lifecycle_methods_update_health() {
        let svc = FakeService::new("config", &[]);
        assert_eq!(svc.health().status(), ServiceStatus::Stopped);

        let ctx = ServiceContext::new(live(), TraceContext::root());
        block_on(svc.start(ctx)).expect("start");
        assert_eq!(svc.health().status(), ServiceStatus::Running);
        assert!(svc.health().since() > 0);

        block_on(svc.quiesce(Instant::now() + Duration::from_secs(1))).expect("quiesce");
        assert_eq!(svc.health().status(), ServiceStatus::Quiescing);

        block_on(svc.stop()).expect("stop");
        assert_eq!(svc.health().status(), ServiceStatus::Stopped);

        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let err = block_on(svc.start(ServiceContext::new(cancelled, TraceContext::root())));
        assert_eq!(err, Err(ServiceError::Cancelled));
    }

    #[test]
    fn declaration_from_service_feeds_cycle_check() {
        let a = FakeService::new("alpha", &["beta"]);
        let b = FakeService::new("beta", &["alpha"]);
        let decls = [
            ServiceDecl::from_service(&a).expect("a"),
            ServiceDecl::from_service(&b).expect("b"),
        ];
        assert!(matches!(
            validate_dependency_graph(&decls, &live()),
            Err(ServiceError::DependencyCycle { .. })
        ));
    }

    #[test]
    fn too_many_dependencies_are_rejected() {
        let deps: Vec<ServiceId> = (0..=MAX_DEPENDENCIES)
            .map(|i| sid(&format!("dep-{i}")))
            .collect();
        assert_eq!(
            ServiceDecl::new(sid("too-many"), deps).expect_err("bound"),
            ServiceError::TooManyDependencies
        );
    }
}
