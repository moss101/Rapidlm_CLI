//! Capability-isolated WASM plugin runtime.
//!
//! Guest modules run with explicit fuel, wall-clock, and memory bounds.
//! WASI / preview1 / preview2 imports are never linked — capability denial
//! cannot be bypassed by engine defaults (T-006). Host functions exist only
//! for brokered grants that already appear on the validated manifest.

use std::error::Error;
use std::fmt;
use std::time::{Duration, Instant};

use capability_broker::{CancellationToken, Capability, FilesystemRoot, ResourceDescriptor};
use protocol::{ApiError, ErrorCode, TraceId};
use serde_json::{Map, Value};

use crate::manifest::{ManifestError, PluginId, PluginManifest, validate_for_load};

/// WIT host import module. The only legal guest import namespace.
pub const HOST_WIT_MODULE: &str = "rapidlm:plugin/host@1.0.0";

/// WIT tool export interface recorded on manifests.
pub const TOOL_WIT_INTERFACE: &str = "rapidlm:plugin/tool@1.0.0";

/// Default guest export implementing `call`.
pub const DEFAULT_CALL_EXPORT: &str = "call";

/// WASM page size in bytes.
pub const WASM_PAGE_BYTES: u32 = 65_536;

/// Default instruction fuel for one instantiate or call.
pub const DEFAULT_MAX_FUEL: u64 = 1_000_000;

/// Default wall-clock bound for one instantiate or call.
pub const DEFAULT_MAX_DURATION: Duration = Duration::from_millis(100);

/// Default linear-memory ceiling (16 pages / 1 MiB).
pub const DEFAULT_MAX_MEMORY_BYTES: u32 = 16 * WASM_PAGE_BYTES;

/// Default guest output / host-call payload ceiling.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// Default module binary ceiling.
pub const DEFAULT_MAX_MODULE_BYTES: usize = 1024 * 1024;

/// Default call-stack frame ceiling.
pub const DEFAULT_MAX_STACK_FRAMES: usize = 256;

/// Hard fuel ceiling. Larger requested limits fail closed.
pub const HARD_MAX_FUEL: u64 = 100_000_000;

/// Hard wall-clock ceiling.
pub const HARD_MAX_DURATION: Duration = Duration::from_secs(5);

/// Hard linear-memory ceiling (64 pages / 4 MiB).
pub const HARD_MAX_MEMORY_BYTES: u32 = 64 * WASM_PAGE_BYTES;

/// Hard output ceiling.
pub const HARD_MAX_OUTPUT_BYTES: usize = 256 * 1024;

/// Hard module binary ceiling.
pub const HARD_MAX_MODULE_BYTES: usize = 4 * 1024 * 1024;

/// Hard call-stack ceiling.
pub const HARD_MAX_STACK_FRAMES: usize = 1_024;

const MAX_TYPES: usize = 256;
const MAX_FUNCS: usize = 256;
const MAX_IMPORTS: usize = 32;
const MAX_EXPORTS: usize = 32;
const MAX_LOCALS: usize = 256;
const MAX_PARAMS: usize = 16;
const MAX_RESULTS: usize = 16;
const MAX_OPS: usize = 64 * 1024;
const MAX_CONTROL_DEPTH: usize = 64;
const MAX_IDENT_BYTES: usize = 256;
const MAX_OPERATION_BYTES: usize = 256;
const MAX_EVIDENCE_REFS: usize = 16;
const CANCEL_STRIDE: usize = 16;
const LIMIT_CHECK_STRIDE: u64 = 64;
const WASM_MAGIC: [u8; 4] = [0x00, 0x61, 0x73, 0x6d];
const WASM_VERSION: [u8; 4] = [0x01, 0x00, 0x00, 0x00];
const WASM_COMPONENT_VERSION: [u8; 4] = [0x0d, 0x00, 0x01, 0x00];

/// Fuel / time / memory / output bounds for one plugin activation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResourceLimits {
    max_fuel: u64,
    max_duration: Duration,
    max_memory_bytes: u32,
    max_output_bytes: usize,
    max_module_bytes: usize,
    max_stack_frames: usize,
}

/// Brokered grant that may be linked as a host import.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrantedCapability {
    capability: Capability,
    resource: ResourceDescriptor,
}

/// WIT `request` payload. Untrusted guest/caller data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginRequest {
    operation: String,
    args_json: String,
}

/// WIT `response` payload. Guest text is untrusted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginResponse {
    result_json: String,
    evidence_refs: Vec<String>,
}

/// Side-effect executor for granted host imports.
///
/// The runtime never opens files, sockets, or processes itself. A handler
/// must still re-validate a capability lease before any executor side effect.
pub trait HostCapabilityHandler {
    fn invoke(
        &mut self,
        capability: Capability,
        resource: &ResourceDescriptor,
        args: &[u8],
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>, WasmError>;
}

/// Default handler: every host call is denied. No ambient access.
#[derive(Clone, Copy, Debug, Default)]
pub struct DenyAllHost;

/// Factory that stamps instances with the same resource limits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WasmPluginHost {
    limits: ResourceLimits,
}

/// Instantiated plugin. No WASI filesystem, network, or process is attached.
pub struct PluginInstance {
    plugin_id: PluginId,
    module: CompiledModule,
    memory: Vec<u8>,
    max_pages: u32,
    globals: Vec<Val>,
    granted: Vec<GrantedCapability>,
    limits: ResourceLimits,
    host: Box<dyn HostCapabilityHandler>,
    call_export: u32,
}

/// Typed instantiate / invoke failure. Display never echoes guest bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WasmError {
    Cancelled,
    InvalidLimits,
    ModuleTooLarge,
    InvalidModule,
    UnsupportedComponent,
    IncompatibleManifest,
    MissingExport,
    MissingMemory,
    UnresolvedImport,
    AmbientWasi,
    CapabilityDenied,
    GrantNotRequested,
    FamilyMismatch,
    FuelExhausted,
    TimeLimit,
    MemoryLimit,
    OutputTooLarge,
    StackOverflow,
    Trap,
    InvalidRequest,
}

impl ResourceLimits {
    pub fn new(
        max_fuel: u64,
        max_duration: Duration,
        max_memory_bytes: u32,
        max_output_bytes: usize,
        max_module_bytes: usize,
        max_stack_frames: usize,
    ) -> Result<Self, WasmError> {
        if max_fuel > HARD_MAX_FUEL
            || max_duration > HARD_MAX_DURATION
            || max_memory_bytes > HARD_MAX_MEMORY_BYTES
            || max_output_bytes > HARD_MAX_OUTPUT_BYTES
            || max_module_bytes > HARD_MAX_MODULE_BYTES
            || max_module_bytes == 0
            || max_stack_frames == 0
            || max_stack_frames > HARD_MAX_STACK_FRAMES
        {
            return Err(WasmError::InvalidLimits);
        }
        Ok(Self {
            max_fuel,
            max_duration,
            max_memory_bytes,
            max_output_bytes,
            max_module_bytes,
            max_stack_frames,
        })
    }

    pub fn standard() -> Self {
        Self {
            max_fuel: DEFAULT_MAX_FUEL,
            max_duration: DEFAULT_MAX_DURATION,
            max_memory_bytes: DEFAULT_MAX_MEMORY_BYTES,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            max_module_bytes: DEFAULT_MAX_MODULE_BYTES,
            max_stack_frames: DEFAULT_MAX_STACK_FRAMES,
        }
    }

    pub fn max_fuel(self) -> u64 {
        self.max_fuel
    }

    pub fn max_duration(self) -> Duration {
        self.max_duration
    }

    pub fn max_memory_bytes(self) -> u32 {
        self.max_memory_bytes
    }

    pub fn max_output_bytes(self) -> usize {
        self.max_output_bytes
    }

    pub fn max_module_bytes(self) -> usize {
        self.max_module_bytes
    }

    pub fn max_stack_frames(self) -> usize {
        self.max_stack_frames
    }

    fn max_pages(self) -> u32 {
        self.max_memory_bytes / WASM_PAGE_BYTES
    }
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self::standard()
    }
}

impl GrantedCapability {
    pub fn new(capability: Capability, resource: ResourceDescriptor) -> Result<Self, WasmError> {
        capability
            .compatible_with(&resource)
            .map_err(|_| WasmError::FamilyMismatch)?;
        reject_ambient(&resource)?;
        Ok(Self {
            capability,
            resource,
        })
    }

    pub fn capability(&self) -> Capability {
        self.capability
    }

    pub fn resource(&self) -> &ResourceDescriptor {
        &self.resource
    }
}

impl PluginRequest {
    pub fn new(operation: &str, args_json: &str) -> Result<Self, WasmError> {
        if !valid_ident(operation, MAX_OPERATION_BYTES) {
            return Err(WasmError::InvalidRequest);
        }
        if args_json.len() > HARD_MAX_OUTPUT_BYTES || !valid_text(args_json) {
            return Err(WasmError::InvalidRequest);
        }
        Ok(Self {
            operation: operation.to_owned(),
            args_json: args_json.to_owned(),
        })
    }

    pub fn operation(&self) -> &str {
        &self.operation
    }

    pub fn args_json(&self) -> &str {
        &self.args_json
    }
}

impl PluginResponse {
    pub fn new(result_json: &str, evidence_refs: Vec<String>) -> Result<Self, WasmError> {
        if result_json.len() > HARD_MAX_OUTPUT_BYTES || !valid_text(result_json) {
            return Err(WasmError::OutputTooLarge);
        }
        if evidence_refs.len() > MAX_EVIDENCE_REFS {
            return Err(WasmError::OutputTooLarge);
        }
        for item in &evidence_refs {
            if item.len() > MAX_IDENT_BYTES || !valid_text(item) {
                return Err(WasmError::OutputTooLarge);
            }
        }
        Ok(Self {
            result_json: result_json.to_owned(),
            evidence_refs,
        })
    }

    pub fn empty() -> Self {
        Self {
            result_json: "{}".to_owned(),
            evidence_refs: Vec::new(),
        }
    }

    pub fn result_json(&self) -> &str {
        &self.result_json
    }

    pub fn evidence_refs(&self) -> &[String] {
        &self.evidence_refs
    }
}

impl HostCapabilityHandler for DenyAllHost {
    fn invoke(
        &mut self,
        _capability: Capability,
        _resource: &ResourceDescriptor,
        _args: &[u8],
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>, WasmError> {
        cancel.check().map_err(|_| WasmError::Cancelled)?;
        Err(WasmError::CapabilityDenied)
    }
}

impl WasmPluginHost {
    pub fn new(limits: ResourceLimits) -> Self {
        Self { limits }
    }

    pub fn limits(&self) -> ResourceLimits {
        self.limits
    }

    /// Instantiate with [`DenyAllHost`]. The instance has no ambient I/O.
    pub fn instantiate(
        &self,
        wasm: &[u8],
        manifest: &PluginManifest,
        granted: &[GrantedCapability],
        cancel: &CancellationToken,
    ) -> Result<PluginInstance, WasmError> {
        self.instantiate_with_host(wasm, manifest, granted, Box::new(DenyAllHost), cancel)
    }

    pub fn instantiate_with_host(
        &self,
        wasm: &[u8],
        manifest: &PluginManifest,
        granted: &[GrantedCapability],
        host: Box<dyn HostCapabilityHandler>,
        cancel: &CancellationToken,
    ) -> Result<PluginInstance, WasmError> {
        instantiate_with(wasm, manifest, granted, self.limits, host, cancel)
    }
}

impl Default for WasmPluginHost {
    fn default() -> Self {
        Self::new(ResourceLimits::standard())
    }
}

/// Instantiate a core module with default limits and [`DenyAllHost`].
pub fn instantiate(
    wasm: &[u8],
    manifest: &PluginManifest,
    granted: &[GrantedCapability],
    cancel: &CancellationToken,
) -> Result<PluginInstance, WasmError> {
    WasmPluginHost::default().instantiate(wasm, manifest, granted, cancel)
}

fn instantiate_with(
    wasm: &[u8],
    manifest: &PluginManifest,
    granted: &[GrantedCapability],
    limits: ResourceLimits,
    host: Box<dyn HostCapabilityHandler>,
    cancel: &CancellationToken,
) -> Result<PluginInstance, WasmError> {
    cancel.check().map_err(|_| WasmError::Cancelled)?;
    validate_for_load(manifest).map_err(map_manifest)?;
    if wasm.len() > limits.max_module_bytes {
        return Err(WasmError::ModuleTooLarge);
    }
    let granted = normalize_grants(manifest, granted, cancel)?;
    let compiled = parse_module(wasm, &granted, cancel)?;
    let (memory, max_pages) = instantiate_memory(&compiled, limits)?;
    let globals = compiled.globals.clone();
    let call_export = resolve_call_export(&compiled, manifest)?;
    let mut instance = PluginInstance {
        plugin_id: manifest.id().clone(),
        module: compiled,
        memory,
        max_pages,
        globals,
        granted,
        limits,
        host,
        call_export,
    };
    if let Some(start) = instance.module.start {
        instance.run_func(start, &[], cancel)?;
    }
    Ok(instance)
}

impl PluginInstance {
    pub fn plugin_id(&self) -> &PluginId {
        &self.plugin_id
    }

    pub fn granted_capabilities(&self) -> &[GrantedCapability] {
        &self.granted
    }

    /// Ambient WASI filesystem is never attached to an instance.
    pub fn has_ambient_filesystem(&self) -> bool {
        false
    }

    /// Ambient WASI sockets / HTTP are never attached to an instance.
    pub fn has_ambient_network(&self) -> bool {
        false
    }

    /// Ambient WASI process / CLI are never attached to an instance.
    pub fn has_ambient_process(&self) -> bool {
        false
    }

    /// Invoke the WIT `call` export under a fresh fuel/time budget.
    pub fn call(
        &mut self,
        request: &PluginRequest,
        cancel: &CancellationToken,
    ) -> Result<PluginResponse, WasmError> {
        cancel.check().map_err(|_| WasmError::Cancelled)?;
        if request.args_json.len() > self.limits.max_output_bytes {
            return Err(WasmError::OutputTooLarge);
        }
        let func = self.func(self.call_export)?;
        match (func.ty.params.as_slice(), func.ty.results.as_slice()) {
            ([], []) => {
                self.run_func(self.call_export, &[], cancel)?;
                Ok(PluginResponse::empty())
            }
            ([], [ValType::I32]) => {
                let results = self.run_func(self.call_export, &[], cancel)?;
                let code = results
                    .first()
                    .and_then(Val::as_i32)
                    .ok_or(WasmError::Trap)?;
                if code < 0 {
                    return Err(status_error(code));
                }
                Ok(PluginResponse::empty())
            }
            ([ValType::I32, ValType::I32, ValType::I32, ValType::I32], [ValType::I32]) => {
                self.call_abi(request, cancel)
            }
            _ => Err(WasmError::MissingExport),
        }
    }

    fn call_abi(
        &mut self,
        request: &PluginRequest,
        cancel: &CancellationToken,
    ) -> Result<PluginResponse, WasmError> {
        if self.memory.is_empty() {
            return Err(WasmError::MissingMemory);
        }
        let payload = encode_request(request)?;
        if payload.len() > self.limits.max_output_bytes {
            return Err(WasmError::OutputTooLarge);
        }
        let req_ptr = 0i32;
        let req_len = i32::try_from(payload.len()).map_err(|_| WasmError::OutputTooLarge)?;
        let out_ptr = req_len;
        let available = self.memory.len().saturating_sub(out_ptr as usize);
        let out_cap = i32::try_from(available.min(self.limits.max_output_bytes))
            .map_err(|_| WasmError::OutputTooLarge)?;
        if payload.len() > self.memory.len() {
            return Err(WasmError::MemoryLimit);
        }
        self.memory[..payload.len()].copy_from_slice(&payload);
        let results = self.run_func(
            self.call_export,
            &[
                Val::I32(req_ptr),
                Val::I32(req_len),
                Val::I32(out_ptr),
                Val::I32(out_cap),
            ],
            cancel,
        )?;
        let written = results
            .first()
            .and_then(Val::as_i32)
            .ok_or(WasmError::Trap)?;
        if written < 0 {
            return Err(status_error(written));
        }
        let written = written as usize;
        let start = out_ptr as usize;
        let end = start.checked_add(written).ok_or(WasmError::Trap)?;
        if end > self.memory.len() || written > self.limits.max_output_bytes {
            return Err(WasmError::OutputTooLarge);
        }
        let slice = self.memory.get(start..end).ok_or(WasmError::Trap)?;
        decode_response(slice)
    }

    fn func(&self, idx: u32) -> Result<&FuncBody, WasmError> {
        self.module
            .funcs
            .get(idx as usize)
            .ok_or(WasmError::InvalidModule)
    }
}

impl fmt::Debug for PluginInstance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginInstance")
            .field("plugin_id", &self.plugin_id.as_str())
            .field("granted", &self.granted.len())
            .field("memory_bytes", &self.memory.len())
            .finish_non_exhaustive()
    }
}

impl WasmError {
    pub fn code(self) -> Option<ErrorCode> {
        match self {
            Self::Cancelled => None,
            Self::AmbientWasi
            | Self::CapabilityDenied
            | Self::GrantNotRequested
            | Self::FamilyMismatch
            | Self::UnresolvedImport => Some(ErrorCode::PluginCapabilityDenied),
            Self::FuelExhausted | Self::TimeLimit | Self::MemoryLimit | Self::OutputTooLarge => {
                Some(ErrorCode::ProcessTimeout)
            }
            Self::InvalidLimits
            | Self::ModuleTooLarge
            | Self::InvalidModule
            | Self::UnsupportedComponent
            | Self::IncompatibleManifest
            | Self::MissingExport
            | Self::MissingMemory
            | Self::InvalidRequest => Some(ErrorCode::ConfigInvalid),
            Self::StackOverflow | Self::Trap => Some(ErrorCode::ToolInvalidArguments),
        }
    }

    pub fn into_api_error(self, trace_id: TraceId) -> Option<ApiError> {
        let code = self.code()?;
        Some(
            ApiError::new(code, self.as_str(), trace_id)
                .unwrap_or_else(|_| ApiError::from_unknown(trace_id, &self)),
        )
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "plugin wasm execution cancelled",
            Self::InvalidLimits => "plugin resource limits exceed the hard ceiling",
            Self::ModuleTooLarge => "plugin module exceeds the configured bound",
            Self::InvalidModule => "plugin module is not a valid core wasm binary",
            Self::UnsupportedComponent => "plugin component binaries are not enabled",
            Self::IncompatibleManifest => "plugin manifest is incompatible with the wasm host",
            Self::MissingExport => "plugin module does not export the required call function",
            Self::MissingMemory => "plugin call abi requires exported linear memory",
            Self::UnresolvedImport => "plugin module imports an unknown host function",
            Self::AmbientWasi => "plugin module requested ambient wasi capabilities",
            Self::CapabilityDenied => "plugin capability is not granted",
            Self::GrantNotRequested => "plugin grant is not declared on the manifest",
            Self::FamilyMismatch => "plugin grant family does not match the resource",
            Self::FuelExhausted => "plugin fuel limit reached",
            Self::TimeLimit => "plugin time limit reached",
            Self::MemoryLimit => "plugin memory limit reached",
            Self::OutputTooLarge => "plugin output exceeds the configured bound",
            Self::StackOverflow => "plugin call stack limit reached",
            Self::Trap => "plugin trapped",
            Self::InvalidRequest => "plugin request is invalid",
        }
    }
}

impl fmt::Display for WasmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for WasmError {}

fn normalize_grants(
    manifest: &PluginManifest,
    granted: &[GrantedCapability],
    cancel: &CancellationToken,
) -> Result<Vec<GrantedCapability>, WasmError> {
    if granted.len() > crate::manifest::MAX_REQUESTED_CAPS {
        return Err(WasmError::GrantNotRequested);
    }
    let mut out = Vec::with_capacity(granted.len());
    for (i, grant) in granted.iter().enumerate() {
        if i % CANCEL_STRIDE == 0 {
            cancel.check().map_err(|_| WasmError::Cancelled)?;
        }
        reject_ambient(grant.resource())?;
        grant
            .capability
            .compatible_with(&grant.resource)
            .map_err(|_| WasmError::FamilyMismatch)?;
        let declared = manifest.requested_caps().iter().any(|requested| {
            requested.capability() == grant.capability && requested.resource() == &grant.resource
        });
        if !declared {
            return Err(WasmError::GrantNotRequested);
        }
        out.push(grant.clone());
    }
    Ok(out)
}

fn reject_ambient(resource: &ResourceDescriptor) -> Result<(), WasmError> {
    match resource {
        ResourceDescriptor::Filesystem(scope) if scope.root() == FilesystemRoot::Host => {
            Err(WasmError::AmbientWasi)
        }
        _ => Ok(()),
    }
}

fn map_manifest(err: ManifestError) -> WasmError {
    match err {
        ManifestError::Cancelled => WasmError::Cancelled,
        ManifestError::AmbientHostFilesystem | ManifestError::AmbientNetwork => {
            WasmError::AmbientWasi
        }
        ManifestError::UnknownCapability | ManifestError::FamilyMismatch => {
            WasmError::CapabilityDenied
        }
        _ => WasmError::IncompatibleManifest,
    }
}

fn status_error(code: i32) -> WasmError {
    match code {
        -1 => WasmError::CapabilityDenied,
        -2 => WasmError::OutputTooLarge,
        -3 => WasmError::MemoryLimit,
        _ => WasmError::Trap,
    }
}

fn valid_text(value: &str) -> bool {
    !value.contains('\0') && !value.chars().any(char::is_control)
}

fn valid_ident(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && valid_text(value)
}

fn encode_request(request: &PluginRequest) -> Result<Vec<u8>, WasmError> {
    let mut object = Map::new();
    object.insert(
        "operation".to_owned(),
        Value::String(request.operation.clone()),
    );
    object.insert(
        "args_json".to_owned(),
        Value::String(request.args_json.clone()),
    );
    serde_json::to_vec(&Value::Object(object)).map_err(|_| WasmError::InvalidRequest)
}

fn decode_response(bytes: &[u8]) -> Result<PluginResponse, WasmError> {
    if bytes.is_empty() {
        return Ok(PluginResponse::empty());
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|_| WasmError::Trap)?;
    let object = value.as_object().ok_or(WasmError::Trap)?;
    for key in object.keys() {
        if key != "result_json" && key != "evidence_refs" {
            return Err(WasmError::Trap);
        }
    }
    let result_json = match object.get("result_json") {
        Some(Value::String(text)) => text.as_str(),
        Some(_) => return Err(WasmError::Trap),
        None => "{}",
    };
    let refs = match object.get("evidence_refs") {
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let text = item.as_str().ok_or(WasmError::Trap)?;
                out.push(text.to_owned());
            }
            out
        }
        Some(_) => return Err(WasmError::Trap),
        None => Vec::new(),
    };
    PluginResponse::new(result_json, refs)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ValType {
    I32,
    I64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Val {
    I32(i32),
    I64(i64),
}

impl Val {
    fn as_i32(&self) -> Option<i32> {
        match self {
            Self::I32(v) => Some(*v),
            Self::I64(_) => None,
        }
    }

    fn default_for(ty: ValType) -> Self {
        match ty {
            ValType::I32 => Self::I32(0),
            ValType::I64 => Self::I64(0),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FuncType {
    params: Vec<ValType>,
    results: Vec<ValType>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Op {
    Unreachable,
    Nop,
    Block { end: u32 },
    Loop { start: u32, end: u32 },
    If { else_pc: u32, end: u32 },
    Else { end: u32 },
    End,
    Br { depth: u32 },
    BrIf { depth: u32 },
    Return,
    Call { func: u32 },
    Drop,
    LocalGet(u32),
    LocalSet(u32),
    LocalTee(u32),
    GlobalGet(u32),
    GlobalSet(u32),
    I32Load { offset: u32 },
    I32Store { offset: u32 },
    MemorySize,
    MemoryGrow,
    I32Const(i32),
    I64Const(i64),
    I32Eqz,
    I32Eq,
    I32Ne,
    I32LtS,
    I32GtS,
    I32Add,
    I32Sub,
    I32Mul,
    I32And,
    I32Or,
    I32Xor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LabelKind {
    Block,
    Loop,
    If,
}

#[derive(Clone, Copy, Debug)]
struct Label {
    kind: LabelKind,
    stack_base: usize,
    target: u32,
}

#[derive(Clone, Debug)]
enum FuncKind {
    Host(Capability),
    Local,
}

#[derive(Clone, Debug)]
struct FuncBody {
    ty: FuncType,
    kind: FuncKind,
    locals: Vec<ValType>,
    ops: Vec<Op>,
}

#[derive(Clone, Debug)]
struct CompiledModule {
    funcs: Vec<FuncBody>,
    globals: Vec<Val>,
    memory_min: u32,
    memory_max: Option<u32>,
    has_memory: bool,
    start: Option<u32>,
    exports: Vec<(String, u32)>,
}

fn parse_module(
    wasm: &[u8],
    granted: &[GrantedCapability],
    cancel: &CancellationToken,
) -> Result<CompiledModule, WasmError> {
    cancel.check().map_err(|_| WasmError::Cancelled)?;
    if wasm.len() < 8 {
        return Err(WasmError::InvalidModule);
    }
    if wasm[0..4] != WASM_MAGIC {
        return Err(WasmError::InvalidModule);
    }
    if wasm[4..8] == WASM_COMPONENT_VERSION {
        return Err(WasmError::UnsupportedComponent);
    }
    if wasm[4..8] != WASM_VERSION {
        return Err(WasmError::InvalidModule);
    }

    let mut offset = 8usize;
    let mut seen = [false; 12];
    let mut types: Vec<FuncType> = Vec::new();
    let mut imports: Vec<FuncBody> = Vec::new();
    let mut func_types: Vec<u32> = Vec::new();
    let mut memory_min = 0u32;
    let mut memory_max = None;
    let mut has_memory = false;
    let mut globals: Vec<Val> = Vec::new();
    let mut exports: Vec<(String, u32)> = Vec::new();
    let mut start = None;
    let mut code_bodies: Vec<(Vec<ValType>, Vec<Op>)> = Vec::new();

    while offset < wasm.len() {
        cancel.check().map_err(|_| WasmError::Cancelled)?;
        let id = wasm[offset];
        offset += 1;
        let size = read_uleb32(wasm, &mut offset)? as usize;
        let end = offset.checked_add(size).ok_or(WasmError::InvalidModule)?;
        if end > wasm.len() {
            return Err(WasmError::InvalidModule);
        }
        let body = &wasm[offset..end];
        offset = end;
        if id != 0 {
            let idx = id as usize;
            if idx >= seen.len() || seen[idx] {
                return Err(WasmError::InvalidModule);
            }
            seen[idx] = true;
        }
        match id {
            0 => {}
            1 => types = parse_type_section(body, cancel)?,
            2 => imports = parse_import_section(body, &types, granted, cancel)?,
            3 => func_types = parse_function_section(body, types.len(), cancel)?,
            4 => return Err(WasmError::UnresolvedImport),
            5 => {
                let mem = parse_memory_section(body)?;
                memory_min = mem.0;
                memory_max = mem.1;
                has_memory = true;
            }
            6 => globals = parse_global_section(body, cancel)?,
            7 => exports = parse_export_section(body, cancel)?,
            8 => start = Some(parse_start_section(body)?),
            9 => return Err(WasmError::UnresolvedImport),
            10 => code_bodies = parse_code_section(body, cancel)?,
            11 | 12 => {}
            _ => return Err(WasmError::InvalidModule),
        }
    }

    if func_types.len() != code_bodies.len() {
        return Err(WasmError::InvalidModule);
    }
    if imports.len() + func_types.len() > MAX_FUNCS {
        return Err(WasmError::InvalidModule);
    }

    let mut funcs = imports;
    for (i, type_idx) in func_types.iter().enumerate() {
        if i % CANCEL_STRIDE == 0 {
            cancel.check().map_err(|_| WasmError::Cancelled)?;
        }
        let ty = types
            .get(*type_idx as usize)
            .cloned()
            .ok_or(WasmError::InvalidModule)?;
        let (locals, ops) = code_bodies[i].clone();
        if ty.params.len() + locals.len() > MAX_LOCALS {
            return Err(WasmError::InvalidModule);
        }
        funcs.push(FuncBody {
            ty,
            kind: FuncKind::Local,
            locals,
            ops,
        });
    }

    if let Some(idx) = start {
        let func = funcs.get(idx as usize).ok_or(WasmError::InvalidModule)?;
        if !func.ty.params.is_empty() || !func.ty.results.is_empty() {
            return Err(WasmError::InvalidModule);
        }
    }

    Ok(CompiledModule {
        funcs,
        globals,
        memory_min,
        memory_max,
        has_memory,
        start,
        exports,
    })
}

fn parse_type_section(body: &[u8], cancel: &CancellationToken) -> Result<Vec<FuncType>, WasmError> {
    let mut offset = 0usize;
    let count = read_uleb32(body, &mut offset)? as usize;
    if count > MAX_TYPES {
        return Err(WasmError::InvalidModule);
    }
    let mut types = Vec::with_capacity(count);
    for i in 0..count {
        if i % CANCEL_STRIDE == 0 {
            cancel.check().map_err(|_| WasmError::Cancelled)?;
        }
        let form = read_byte(body, &mut offset)?;
        if form != 0x60 {
            return Err(WasmError::InvalidModule);
        }
        types.push(FuncType {
            params: read_valtypes(body, &mut offset)?,
            results: read_valtypes(body, &mut offset)?,
        });
    }
    if offset != body.len() {
        return Err(WasmError::InvalidModule);
    }
    Ok(types)
}

fn parse_import_section(
    body: &[u8],
    types: &[FuncType],
    granted: &[GrantedCapability],
    cancel: &CancellationToken,
) -> Result<Vec<FuncBody>, WasmError> {
    let mut offset = 0usize;
    let count = read_uleb32(body, &mut offset)? as usize;
    if count > MAX_IMPORTS {
        return Err(WasmError::InvalidModule);
    }
    let mut imports = Vec::with_capacity(count);
    for i in 0..count {
        if i % CANCEL_STRIDE == 0 {
            cancel.check().map_err(|_| WasmError::Cancelled)?;
        }
        let module = read_name(body, &mut offset)?;
        let field = read_name(body, &mut offset)?;
        let kind = read_byte(body, &mut offset)?;
        match classify_import(&module, &field) {
            ImportClass::AmbientWasi => return Err(WasmError::AmbientWasi),
            ImportClass::Unknown => return Err(WasmError::UnresolvedImport),
            ImportClass::Brokered(capability) => {
                if kind != 0x00 {
                    return Err(WasmError::UnresolvedImport);
                }
                let type_idx = read_uleb32(body, &mut offset)? as usize;
                let ty = types
                    .get(type_idx)
                    .cloned()
                    .ok_or(WasmError::InvalidModule)?;
                if !host_func_type(&ty) {
                    return Err(WasmError::InvalidModule);
                }
                if !granted.iter().any(|grant| grant.capability == capability) {
                    return Err(WasmError::CapabilityDenied);
                }
                imports.push(FuncBody {
                    ty,
                    kind: FuncKind::Host(capability),
                    locals: Vec::new(),
                    ops: Vec::new(),
                });
            }
        }
    }
    if offset != body.len() {
        return Err(WasmError::InvalidModule);
    }
    Ok(imports)
}

fn parse_function_section(
    body: &[u8],
    type_count: usize,
    cancel: &CancellationToken,
) -> Result<Vec<u32>, WasmError> {
    let mut offset = 0usize;
    let count = read_uleb32(body, &mut offset)? as usize;
    if count > MAX_FUNCS {
        return Err(WasmError::InvalidModule);
    }
    let mut idxs = Vec::with_capacity(count);
    for i in 0..count {
        if i % CANCEL_STRIDE == 0 {
            cancel.check().map_err(|_| WasmError::Cancelled)?;
        }
        let idx = read_uleb32(body, &mut offset)?;
        if idx as usize >= type_count {
            return Err(WasmError::InvalidModule);
        }
        idxs.push(idx);
    }
    if offset != body.len() {
        return Err(WasmError::InvalidModule);
    }
    Ok(idxs)
}

fn parse_memory_section(body: &[u8]) -> Result<(u32, Option<u32>), WasmError> {
    let mut offset = 0usize;
    let count = read_uleb32(body, &mut offset)?;
    if count != 1 {
        return Err(WasmError::InvalidModule);
    }
    let flags = read_uleb32(body, &mut offset)?;
    let min = read_uleb32(body, &mut offset)?;
    let max = if flags == 1 {
        Some(read_uleb32(body, &mut offset)?)
    } else if flags == 0 {
        None
    } else {
        return Err(WasmError::InvalidModule);
    };
    if matches!(max, Some(limit) if limit < min) {
        return Err(WasmError::InvalidModule);
    }
    if offset != body.len() {
        return Err(WasmError::InvalidModule);
    }
    Ok((min, max))
}

fn parse_global_section(body: &[u8], cancel: &CancellationToken) -> Result<Vec<Val>, WasmError> {
    let mut offset = 0usize;
    let count = read_uleb32(body, &mut offset)? as usize;
    if count > MAX_LOCALS {
        return Err(WasmError::InvalidModule);
    }
    let mut globals = Vec::with_capacity(count);
    for i in 0..count {
        if i % CANCEL_STRIDE == 0 {
            cancel.check().map_err(|_| WasmError::Cancelled)?;
        }
        let ty = read_valtype(read_byte(body, &mut offset)?)?;
        let _mut = read_byte(body, &mut offset)?;
        let opcode = read_byte(body, &mut offset)?;
        let value = match (ty, opcode) {
            (ValType::I32, 0x41) => Val::I32(read_ileb32(body, &mut offset)?),
            (ValType::I64, 0x42) => Val::I64(read_ileb64(body, &mut offset)?),
            _ => return Err(WasmError::InvalidModule),
        };
        if read_byte(body, &mut offset)? != 0x0b {
            return Err(WasmError::InvalidModule);
        }
        globals.push(value);
    }
    if offset != body.len() {
        return Err(WasmError::InvalidModule);
    }
    Ok(globals)
}

fn parse_export_section(
    body: &[u8],
    cancel: &CancellationToken,
) -> Result<Vec<(String, u32)>, WasmError> {
    let mut offset = 0usize;
    let count = read_uleb32(body, &mut offset)? as usize;
    if count > MAX_EXPORTS {
        return Err(WasmError::InvalidModule);
    }
    let mut exports = Vec::with_capacity(count);
    for i in 0..count {
        if i % CANCEL_STRIDE == 0 {
            cancel.check().map_err(|_| WasmError::Cancelled)?;
        }
        let name = read_name(body, &mut offset)?;
        let kind = read_byte(body, &mut offset)?;
        let idx = read_uleb32(body, &mut offset)?;
        if kind == 0x00 {
            if exports.iter().any(|(existing, _)| existing == &name) {
                return Err(WasmError::InvalidModule);
            }
            exports.push((name, idx));
        }
    }
    if offset != body.len() {
        return Err(WasmError::InvalidModule);
    }
    Ok(exports)
}

fn parse_start_section(body: &[u8]) -> Result<u32, WasmError> {
    let mut offset = 0usize;
    let idx = read_uleb32(body, &mut offset)?;
    if offset != body.len() {
        return Err(WasmError::InvalidModule);
    }
    Ok(idx)
}

type DecodedFunc = (Vec<ValType>, Vec<Op>);

fn parse_code_section(
    body: &[u8],
    cancel: &CancellationToken,
) -> Result<Vec<DecodedFunc>, WasmError> {
    let mut offset = 0usize;
    let count = read_uleb32(body, &mut offset)? as usize;
    if count > MAX_FUNCS {
        return Err(WasmError::InvalidModule);
    }
    let mut funcs = Vec::with_capacity(count);
    for i in 0..count {
        if i % CANCEL_STRIDE == 0 {
            cancel.check().map_err(|_| WasmError::Cancelled)?;
        }
        let size = read_uleb32(body, &mut offset)? as usize;
        let end = offset.checked_add(size).ok_or(WasmError::InvalidModule)?;
        if end > body.len() {
            return Err(WasmError::InvalidModule);
        }
        funcs.push(parse_func_body(&body[offset..end], cancel)?);
        offset = end;
    }
    if offset != body.len() {
        return Err(WasmError::InvalidModule);
    }
    Ok(funcs)
}

fn parse_func_body(
    body: &[u8],
    cancel: &CancellationToken,
) -> Result<(Vec<ValType>, Vec<Op>), WasmError> {
    let mut offset = 0usize;
    let local_sets = read_uleb32(body, &mut offset)? as usize;
    let mut locals = Vec::new();
    for i in 0..local_sets {
        if i % CANCEL_STRIDE == 0 {
            cancel.check().map_err(|_| WasmError::Cancelled)?;
        }
        let count = read_uleb32(body, &mut offset)? as usize;
        let ty = read_valtype(read_byte(body, &mut offset)?)?;
        if locals.len().saturating_add(count) > MAX_LOCALS {
            return Err(WasmError::InvalidModule);
        }
        locals.extend(std::iter::repeat_n(ty, count));
    }
    let ops = decode_ops(&body[offset..], cancel)?;
    Ok((locals, ops))
}

#[derive(Clone, Copy)]
enum Ctrl {
    Block(usize),
    Loop(usize),
    If(usize),
}

fn decode_ops(body: &[u8], cancel: &CancellationToken) -> Result<Vec<Op>, WasmError> {
    let mut offset = 0usize;
    let mut ops = Vec::new();
    let mut control = Vec::new();
    while offset < body.len() {
        if ops.len() % CANCEL_STRIDE == 0 {
            cancel.check().map_err(|_| WasmError::Cancelled)?;
        }
        if ops.len() >= MAX_OPS {
            return Err(WasmError::InvalidModule);
        }
        let opcode = read_byte(body, &mut offset)?;
        match opcode {
            0x00 => ops.push(Op::Unreachable),
            0x01 => ops.push(Op::Nop),
            0x02 => {
                read_block_type(body, &mut offset)?;
                if control.len() >= MAX_CONTROL_DEPTH {
                    return Err(WasmError::InvalidModule);
                }
                control.push(Ctrl::Block(ops.len()));
                ops.push(Op::Block { end: 0 });
            }
            0x03 => {
                read_block_type(body, &mut offset)?;
                if control.len() >= MAX_CONTROL_DEPTH {
                    return Err(WasmError::InvalidModule);
                }
                let idx = ops.len();
                control.push(Ctrl::Loop(idx));
                ops.push(Op::Loop {
                    start: (idx as u32) + 1,
                    end: 0,
                });
            }
            0x04 => {
                read_block_type(body, &mut offset)?;
                if control.len() >= MAX_CONTROL_DEPTH {
                    return Err(WasmError::InvalidModule);
                }
                control.push(Ctrl::If(ops.len()));
                ops.push(Op::If { else_pc: 0, end: 0 });
            }
            0x05 => {
                let Some(Ctrl::If(idx)) = control.last().copied() else {
                    return Err(WasmError::InvalidModule);
                };
                ops.push(Op::Else { end: 0 });
                let else_pc = ops.len() as u32;
                if let Op::If { else_pc: slot, .. } = &mut ops[idx] {
                    *slot = else_pc;
                }
            }
            0x0b => {
                ops.push(Op::End);
                let end_after = ops.len() as u32;
                match control.pop() {
                    Some(Ctrl::Block(idx) | Ctrl::Loop(idx) | Ctrl::If(idx)) => {
                        patch_end(&mut ops, idx, end_after);
                    }
                    None => {
                        if offset != body.len() {
                            return Err(WasmError::InvalidModule);
                        }
                        return Ok(ops);
                    }
                }
            }
            0x0c => ops.push(Op::Br {
                depth: read_uleb32(body, &mut offset)?,
            }),
            0x0d => ops.push(Op::BrIf {
                depth: read_uleb32(body, &mut offset)?,
            }),
            0x0f => ops.push(Op::Return),
            0x10 => ops.push(Op::Call {
                func: read_uleb32(body, &mut offset)?,
            }),
            0x1a => ops.push(Op::Drop),
            0x20 => ops.push(Op::LocalGet(read_uleb32(body, &mut offset)?)),
            0x21 => ops.push(Op::LocalSet(read_uleb32(body, &mut offset)?)),
            0x22 => ops.push(Op::LocalTee(read_uleb32(body, &mut offset)?)),
            0x23 => ops.push(Op::GlobalGet(read_uleb32(body, &mut offset)?)),
            0x24 => ops.push(Op::GlobalSet(read_uleb32(body, &mut offset)?)),
            0x28 => {
                let _align = read_uleb32(body, &mut offset)?;
                ops.push(Op::I32Load {
                    offset: read_uleb32(body, &mut offset)?,
                });
            }
            0x36 => {
                let _align = read_uleb32(body, &mut offset)?;
                ops.push(Op::I32Store {
                    offset: read_uleb32(body, &mut offset)?,
                });
            }
            0x3f => {
                if read_byte(body, &mut offset)? != 0x00 {
                    return Err(WasmError::InvalidModule);
                }
                ops.push(Op::MemorySize);
            }
            0x40 => {
                if read_byte(body, &mut offset)? != 0x00 {
                    return Err(WasmError::InvalidModule);
                }
                ops.push(Op::MemoryGrow);
            }
            0x41 => ops.push(Op::I32Const(read_ileb32(body, &mut offset)?)),
            0x42 => ops.push(Op::I64Const(read_ileb64(body, &mut offset)?)),
            0x45 => ops.push(Op::I32Eqz),
            0x46 => ops.push(Op::I32Eq),
            0x47 => ops.push(Op::I32Ne),
            0x48 => ops.push(Op::I32LtS),
            0x4a => ops.push(Op::I32GtS),
            0x6a => ops.push(Op::I32Add),
            0x6b => ops.push(Op::I32Sub),
            0x6c => ops.push(Op::I32Mul),
            0x71 => ops.push(Op::I32And),
            0x72 => ops.push(Op::I32Or),
            0x73 => ops.push(Op::I32Xor),
            _ => return Err(WasmError::InvalidModule),
        }
    }
    Err(WasmError::InvalidModule)
}

fn patch_end(ops: &mut [Op], idx: usize, end_after: u32) {
    match &mut ops[idx] {
        Op::Block { end } | Op::Loop { end, .. } | Op::If { end, .. } => *end = end_after,
        _ => {}
    }
    for op in ops.iter_mut() {
        match op {
            Op::Else { end } if *end == 0 => *end = end_after,
            _ => {}
        }
    }
}

fn read_block_type(data: &[u8], offset: &mut usize) -> Result<(), WasmError> {
    let byte = read_byte(data, offset)?;
    match byte {
        0x40 | 0x7f | 0x7e => Ok(()),
        _ => Err(WasmError::InvalidModule),
    }
}

fn instantiate_memory(
    module: &CompiledModule,
    limits: ResourceLimits,
) -> Result<(Vec<u8>, u32), WasmError> {
    if !module.has_memory {
        return Ok((Vec::new(), 0));
    }
    let host_max = limits.max_pages();
    if module.memory_min > host_max {
        return Err(WasmError::MemoryLimit);
    }
    if matches!(module.memory_max, Some(declared) if declared < module.memory_min) {
        return Err(WasmError::InvalidModule);
    }
    let max_pages = match module.memory_max {
        Some(declared) => declared.min(host_max),
        None => host_max,
    };
    let bytes = (module.memory_min as usize)
        .checked_mul(WASM_PAGE_BYTES as usize)
        .ok_or(WasmError::MemoryLimit)?;
    if bytes > limits.max_memory_bytes as usize {
        return Err(WasmError::MemoryLimit);
    }
    Ok((vec![0u8; bytes], max_pages))
}

fn resolve_call_export(
    module: &CompiledModule,
    manifest: &PluginManifest,
) -> Result<u32, WasmError> {
    let wanted = match manifest.entrypoint().export() {
        Some(export) if export == TOOL_WIT_INTERFACE => DEFAULT_CALL_EXPORT,
        Some(export) => export,
        None => DEFAULT_CALL_EXPORT,
    };
    module
        .exports
        .iter()
        .find(|(name, _)| name == wanted)
        .map(|(_, idx)| *idx)
        .ok_or(WasmError::MissingExport)
}

enum ImportClass {
    Brokered(Capability),
    AmbientWasi,
    Unknown,
}

fn classify_import(module: &str, field: &str) -> ImportClass {
    if is_wasi_module(module) || is_wasi_field(field) {
        return ImportClass::AmbientWasi;
    }
    if module == HOST_WIT_MODULE {
        return match field.parse::<Capability>() {
            Ok(capability) => ImportClass::Brokered(capability),
            Err(_) => ImportClass::Unknown,
        };
    }
    ImportClass::Unknown
}

fn is_wasi_module(module: &str) -> bool {
    let lowered = module.to_ascii_lowercase();
    lowered == "wasi_snapshot_preview1"
        || lowered == "wasi_unstable"
        || lowered == "wasi"
        || lowered.starts_with("wasi:")
        || lowered.contains("wasi:filesystem")
        || lowered.contains("wasi:sockets")
        || lowered.contains("wasi:cli")
        || lowered.contains("wasi:http")
        || lowered.contains("wasi:io")
        || lowered.contains("wasi:clocks")
        || lowered.contains("wasi:random")
        || lowered.contains("wasi:path")
}

fn is_wasi_field(field: &str) -> bool {
    let lowered = field.to_ascii_lowercase();
    lowered.starts_with("fd_")
        || lowered.starts_with("path_")
        || lowered.starts_with("sock_")
        || lowered.starts_with("proc_")
        || lowered.starts_with("environ_")
        || lowered.starts_with("args_")
        || lowered.contains("preopen")
        || lowered.contains("get-directories")
}

fn host_func_type(ty: &FuncType) -> bool {
    ty.params == [ValType::I32, ValType::I32, ValType::I32, ValType::I32]
        && ty.results == [ValType::I32]
}

impl PluginInstance {
    fn run_func(
        &mut self,
        func_idx: u32,
        args: &[Val],
        cancel: &CancellationToken,
    ) -> Result<Vec<Val>, WasmError> {
        cancel.check().map_err(|_| WasmError::Cancelled)?;
        let deadline = Instant::now() + self.limits.max_duration;
        let mut fuel = self.limits.max_fuel;
        let mut stack: Vec<Val> = Vec::new();
        self.call_inner(func_idx, args, &mut stack, &mut fuel, deadline, cancel, 0)?;
        let arity = self.func(func_idx)?.ty.results.len();
        if stack.len() < arity {
            return Err(WasmError::Trap);
        }
        Ok(stack.split_off(stack.len() - arity))
    }

    // One interpreter frame's worth of state, threaded explicitly so the
    // recursion depth and fuel are visible at every call site.
    #[allow(clippy::too_many_arguments)]
    fn call_inner(
        &mut self,
        func_idx: u32,
        args: &[Val],
        stack: &mut Vec<Val>,
        fuel: &mut u64,
        deadline: Instant,
        cancel: &CancellationToken,
        depth: usize,
    ) -> Result<(), WasmError> {
        let header = self.func(func_idx)?.clone();
        match header.kind {
            FuncKind::Host(capability) => {
                if args.len() != 4 {
                    return Err(WasmError::Trap);
                }
                for arg in args {
                    stack.push(*arg);
                }
                return self.call_host(capability, stack, fuel, deadline, cancel);
            }
            FuncKind::Local => {}
        }
        if args.len() != header.ty.params.len() {
            return Err(WasmError::Trap);
        }
        let mut locals = Vec::with_capacity(header.ty.params.len() + header.locals.len());
        locals.extend_from_slice(args);
        locals.extend(header.locals.iter().copied().map(Val::default_for));
        let mut pc = 0usize;
        let mut labels: Vec<Label> = Vec::new();
        let stack_base = stack.len();
        let ops = header.ops;
        while pc < ops.len() {
            self.charge(fuel, deadline, cancel)?;
            match ops[pc] {
                Op::Unreachable => return Err(WasmError::Trap),
                Op::Nop => pc += 1,
                Op::Block { end } => {
                    labels.push(Label {
                        kind: LabelKind::Block,
                        stack_base: stack.len(),
                        target: end,
                    });
                    pc += 1;
                }
                Op::Loop { start, end: _ } => {
                    labels.push(Label {
                        kind: LabelKind::Loop,
                        stack_base: stack.len(),
                        target: start,
                    });
                    pc += 1;
                }
                Op::If { else_pc, end } => {
                    let cond = pop_i32(stack)?;
                    labels.push(Label {
                        kind: LabelKind::If,
                        stack_base: stack.len(),
                        target: end,
                    });
                    if cond == 0 {
                        pc = if else_pc == 0 {
                            end as usize
                        } else {
                            else_pc as usize
                        };
                    } else {
                        pc += 1;
                    }
                }
                Op::Else { end } => {
                    pc = end as usize;
                }
                Op::End => {
                    labels.pop();
                    pc += 1;
                }
                Op::Br { depth } => {
                    pc = self.branch(&mut labels, stack, depth)?;
                }
                Op::BrIf { depth } => {
                    if pop_i32(stack)? != 0 {
                        pc = self.branch(&mut labels, stack, depth)?;
                    } else {
                        pc += 1;
                    }
                }
                Op::Return => {
                    if stack.len() < header.ty.results.len() {
                        return Err(WasmError::Trap);
                    }
                    let keep = stack.split_off(stack.len() - header.ty.results.len());
                    stack.truncate(stack_base);
                    stack.extend(keep);
                    return Ok(());
                }
                Op::Call { func } => {
                    // `labels` only tracks Block/Loop/If nesting within this
                    // one activation and is reset to empty on every call
                    // (including this recursive one) — it can never bound
                    // cross-call recursion depth. `depth` is the real native
                    // call-stack depth accumulated across `call_inner`
                    // invocations; a guest that recurses without pushing any
                    // structured-control labels (e.g. one bare self-call)
                    // must still be stopped here, or it overflows the real
                    // OS thread stack — an uncatchable process abort, not a
                    // contained trap — long before fuel could exhaust it.
                    if depth + 1 > self.limits.max_stack_frames {
                        return Err(WasmError::StackOverflow);
                    }
                    let callee = self.func(func)?.clone();
                    if stack.len() < callee.ty.params.len() {
                        return Err(WasmError::Trap);
                    }
                    let args = stack.split_off(stack.len() - callee.ty.params.len());
                    self.call_inner(func, &args, stack, fuel, deadline, cancel, depth + 1)?;
                    pc += 1;
                }
                Op::Drop => {
                    pop_val(stack)?;
                    pc += 1;
                }
                Op::LocalGet(idx) => {
                    let val = *locals.get(idx as usize).ok_or(WasmError::Trap)?;
                    stack.push(val);
                    pc += 1;
                }
                Op::LocalSet(idx) => {
                    let val = pop_val(stack)?;
                    let slot = locals.get_mut(idx as usize).ok_or(WasmError::Trap)?;
                    *slot = val;
                    pc += 1;
                }
                Op::LocalTee(idx) => {
                    let val = *stack.last().ok_or(WasmError::Trap)?;
                    let slot = locals.get_mut(idx as usize).ok_or(WasmError::Trap)?;
                    *slot = val;
                    pc += 1;
                }
                Op::GlobalGet(idx) => {
                    let val = *self.globals.get(idx as usize).ok_or(WasmError::Trap)?;
                    stack.push(val);
                    pc += 1;
                }
                Op::GlobalSet(idx) => {
                    let val = pop_val(stack)?;
                    let slot = self.globals.get_mut(idx as usize).ok_or(WasmError::Trap)?;
                    *slot = val;
                    pc += 1;
                }
                Op::I32Load { offset } => {
                    let addr = pop_i32(stack)? as u32;
                    let value = self.load_i32(addr, offset)?;
                    stack.push(Val::I32(value));
                    pc += 1;
                }
                Op::I32Store { offset } => {
                    let value = pop_i32(stack)?;
                    let addr = pop_i32(stack)? as u32;
                    self.store_i32(addr, offset, value)?;
                    pc += 1;
                }
                Op::MemorySize => {
                    stack.push(Val::I32(self.current_pages() as i32));
                    pc += 1;
                }
                Op::MemoryGrow => {
                    let delta = pop_i32(stack)?;
                    let grown = self.grow_memory(delta)?;
                    stack.push(Val::I32(grown));
                    pc += 1;
                }
                Op::I32Const(v) => {
                    stack.push(Val::I32(v));
                    pc += 1;
                }
                Op::I64Const(v) => {
                    stack.push(Val::I64(v));
                    pc += 1;
                }
                Op::I32Eqz => {
                    let v = pop_i32(stack)?;
                    stack.push(Val::I32(i32::from(v == 0)));
                    pc += 1;
                }
                Op::I32Eq => {
                    binop_i32(stack, |a, b| i32::from(a == b))?;
                    pc += 1;
                }
                Op::I32Ne => {
                    binop_i32(stack, |a, b| i32::from(a != b))?;
                    pc += 1;
                }
                Op::I32LtS => {
                    binop_i32(stack, |a, b| i32::from(a < b))?;
                    pc += 1;
                }
                Op::I32GtS => {
                    binop_i32(stack, |a, b| i32::from(a > b))?;
                    pc += 1;
                }
                Op::I32Add => {
                    binop_i32(stack, i32::wrapping_add)?;
                    pc += 1;
                }
                Op::I32Sub => {
                    binop_i32(stack, i32::wrapping_sub)?;
                    pc += 1;
                }
                Op::I32Mul => {
                    binop_i32(stack, i32::wrapping_mul)?;
                    pc += 1;
                }
                Op::I32And => {
                    binop_i32(stack, |a, b| a & b)?;
                    pc += 1;
                }
                Op::I32Or => {
                    binop_i32(stack, |a, b| a | b)?;
                    pc += 1;
                }
                Op::I32Xor => {
                    binop_i32(stack, |a, b| a ^ b)?;
                    pc += 1;
                }
            }
        }
        if stack.len() < stack_base + header.ty.results.len() {
            return Err(WasmError::Trap);
        }
        let keep = stack.split_off(stack.len() - header.ty.results.len());
        stack.truncate(stack_base);
        stack.extend(keep);
        Ok(())
    }

    fn branch(
        &self,
        labels: &mut Vec<Label>,
        stack: &mut [Val],
        depth: u32,
    ) -> Result<usize, WasmError> {
        let idx = labels
            .len()
            .checked_sub(1)
            .and_then(|top| top.checked_sub(depth as usize))
            .ok_or(WasmError::Trap)?;
        let label = labels[idx];
        match label.kind {
            LabelKind::Loop => {
                labels.truncate(idx + 1);
                if stack.len() < label.stack_base {
                    return Err(WasmError::Trap);
                }
                Ok(label.target as usize)
            }
            LabelKind::Block | LabelKind::If => {
                labels.truncate(idx);
                if stack.len() < label.stack_base {
                    return Err(WasmError::Trap);
                }
                Ok(label.target as usize)
            }
        }
    }

    fn charge(
        &self,
        fuel: &mut u64,
        deadline: Instant,
        cancel: &CancellationToken,
    ) -> Result<(), WasmError> {
        if *fuel == 0 {
            return Err(WasmError::FuelExhausted);
        }
        *fuel -= 1;
        if (*fuel).is_multiple_of(LIMIT_CHECK_STRIDE) {
            cancel.check().map_err(|_| WasmError::Cancelled)?;
            if Instant::now() >= deadline {
                return Err(WasmError::TimeLimit);
            }
        }
        Ok(())
    }

    fn current_pages(&self) -> u32 {
        (self.memory.len() / WASM_PAGE_BYTES as usize) as u32
    }

    fn grow_memory(&mut self, delta: i32) -> Result<i32, WasmError> {
        if delta < 0 {
            return Err(WasmError::Trap);
        }
        let current = self.current_pages();
        let delta = delta as u32;
        let Some(new_pages) = current.checked_add(delta) else {
            return Err(WasmError::MemoryLimit);
        };
        if new_pages > self.max_pages {
            return Err(WasmError::MemoryLimit);
        }
        let new_bytes = (new_pages as usize)
            .checked_mul(WASM_PAGE_BYTES as usize)
            .ok_or(WasmError::MemoryLimit)?;
        if new_bytes > self.limits.max_memory_bytes as usize {
            return Err(WasmError::MemoryLimit);
        }
        self.memory.resize(new_bytes, 0);
        Ok(current as i32)
    }

    fn load_i32(&self, addr: u32, offset: u32) -> Result<i32, WasmError> {
        let start = addr.checked_add(offset).ok_or(WasmError::Trap)? as usize;
        let end = start.checked_add(4).ok_or(WasmError::Trap)?;
        let bytes = self.memory.get(start..end).ok_or(WasmError::Trap)?;
        let mut raw = [0u8; 4];
        raw.copy_from_slice(bytes);
        Ok(i32::from_le_bytes(raw))
    }

    fn store_i32(&mut self, addr: u32, offset: u32, value: i32) -> Result<(), WasmError> {
        let start = addr.checked_add(offset).ok_or(WasmError::Trap)? as usize;
        let end = start.checked_add(4).ok_or(WasmError::Trap)?;
        let slot = self.memory.get_mut(start..end).ok_or(WasmError::Trap)?;
        slot.copy_from_slice(&value.to_le_bytes());
        Ok(())
    }

    fn call_host(
        &mut self,
        capability: Capability,
        stack: &mut Vec<Val>,
        fuel: &mut u64,
        deadline: Instant,
        cancel: &CancellationToken,
    ) -> Result<(), WasmError> {
        self.charge(fuel, deadline, cancel)?;
        let out_cap = pop_i32(stack)? as u32;
        let out_ptr = pop_i32(stack)? as u32;
        let args_len = pop_i32(stack)? as u32;
        let args_ptr = pop_i32(stack)? as u32;
        let grant = self
            .granted
            .iter()
            .find(|grant| grant.capability == capability)
            .ok_or(WasmError::CapabilityDenied)?
            .clone();
        if args_len as usize > self.limits.max_output_bytes {
            return Err(WasmError::OutputTooLarge);
        }
        let args = self.read_guest(args_ptr, args_len)?;
        let result = self
            .host
            .invoke(capability, &grant.resource, &args, cancel)?;
        if result.len() > self.limits.max_output_bytes || result.len() > out_cap as usize {
            return Err(WasmError::OutputTooLarge);
        }
        self.write_guest(out_ptr, &result)?;
        stack.push(Val::I32(
            i32::try_from(result.len()).map_err(|_| WasmError::OutputTooLarge)?,
        ));
        Ok(())
    }

    fn read_guest(&self, ptr: u32, len: u32) -> Result<Vec<u8>, WasmError> {
        let start = ptr as usize;
        let end = start.checked_add(len as usize).ok_or(WasmError::Trap)?;
        self.memory
            .get(start..end)
            .map(|slice| slice.to_vec())
            .ok_or(WasmError::Trap)
    }

    fn write_guest(&mut self, ptr: u32, bytes: &[u8]) -> Result<(), WasmError> {
        if bytes.is_empty() {
            return Ok(());
        }
        let start = ptr as usize;
        let end = start.checked_add(bytes.len()).ok_or(WasmError::Trap)?;
        let slot = self.memory.get_mut(start..end).ok_or(WasmError::Trap)?;
        slot.copy_from_slice(bytes);
        Ok(())
    }
}

fn pop_val(stack: &mut Vec<Val>) -> Result<Val, WasmError> {
    stack.pop().ok_or(WasmError::Trap)
}

fn pop_i32(stack: &mut Vec<Val>) -> Result<i32, WasmError> {
    match pop_val(stack)? {
        Val::I32(v) => Ok(v),
        Val::I64(_) => Err(WasmError::Trap),
    }
}

fn binop_i32(stack: &mut Vec<Val>, op: fn(i32, i32) -> i32) -> Result<(), WasmError> {
    let b = pop_i32(stack)?;
    let a = pop_i32(stack)?;
    stack.push(Val::I32(op(a, b)));
    Ok(())
}

fn read_valtypes(data: &[u8], offset: &mut usize) -> Result<Vec<ValType>, WasmError> {
    let count = read_uleb32(data, offset)? as usize;
    if count > MAX_PARAMS.max(MAX_RESULTS) {
        return Err(WasmError::InvalidModule);
    }
    let mut types = Vec::with_capacity(count);
    for _ in 0..count {
        types.push(read_valtype(read_byte(data, offset)?)?);
    }
    Ok(types)
}

fn read_valtype(byte: u8) -> Result<ValType, WasmError> {
    match byte {
        0x7f => Ok(ValType::I32),
        0x7e => Ok(ValType::I64),
        _ => Err(WasmError::InvalidModule),
    }
}

fn read_name(data: &[u8], offset: &mut usize) -> Result<String, WasmError> {
    let len = read_uleb32(data, offset)? as usize;
    if len > MAX_IDENT_BYTES {
        return Err(WasmError::InvalidModule);
    }
    let end = offset.checked_add(len).ok_or(WasmError::InvalidModule)?;
    let bytes = data.get(*offset..end).ok_or(WasmError::InvalidModule)?;
    *offset = end;
    let text = std::str::from_utf8(bytes).map_err(|_| WasmError::InvalidModule)?;
    if !valid_text(text) {
        return Err(WasmError::InvalidModule);
    }
    Ok(text.to_owned())
}

fn read_byte(data: &[u8], offset: &mut usize) -> Result<u8, WasmError> {
    let byte = *data.get(*offset).ok_or(WasmError::InvalidModule)?;
    *offset += 1;
    Ok(byte)
}

fn read_uleb32(data: &[u8], offset: &mut usize) -> Result<u32, WasmError> {
    let mut result = 0u32;
    let mut shift = 0u32;
    loop {
        let byte = read_byte(data, offset)?;
        let bits = u32::from(byte & 0x7f);
        if shift == 28 && bits > 0x0f {
            return Err(WasmError::InvalidModule);
        }
        result |= bits << shift;
        if byte & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
        if shift > 28 {
            return Err(WasmError::InvalidModule);
        }
    }
}

fn read_ileb32(data: &[u8], offset: &mut usize) -> Result<i32, WasmError> {
    let mut result = 0u32;
    let mut shift = 0u32;
    let mut byte;
    loop {
        byte = read_byte(data, offset)?;
        result |= u32::from(byte & 0x7f) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            break;
        }
        if shift >= 32 {
            return Err(WasmError::InvalidModule);
        }
    }
    if shift < 32 && byte & 0x40 != 0 {
        result |= !0u32 << shift;
    }
    Ok(result as i32)
}

fn read_ileb64(data: &[u8], offset: &mut usize) -> Result<i64, WasmError> {
    let mut result = 0u64;
    let mut shift = 0u32;
    let mut byte;
    loop {
        byte = read_byte(data, offset)?;
        result |= u64::from(byte & 0x7f) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            break;
        }
        if shift >= 64 {
            return Err(WasmError::InvalidModule);
        }
    }
    if shift < 64 && byte & 0x40 != 0 {
        result |= !0u64 << shift;
    }
    Ok(result as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use capability_broker::{FilesystemScope, NetworkScheme, NetworkScope};
    use protocol::ArtifactId;
    use std::sync::{Arc, Mutex};

    const PACKAGE_FIXTURE: &[u8] = b"fixture-plugin-wasm";

    struct RecordingHost {
        #[allow(clippy::type_complexity)] // compact test recorder for one call site
        calls: Arc<Mutex<Vec<(Capability, Vec<u8>)>>>,
        response: Vec<u8>,
    }

    impl HostCapabilityHandler for RecordingHost {
        fn invoke(
            &mut self,
            capability: Capability,
            _resource: &ResourceDescriptor,
            args: &[u8],
            cancel: &CancellationToken,
        ) -> Result<Vec<u8>, WasmError> {
            cancel.check().map_err(|_| WasmError::Cancelled)?;
            self.calls
                .lock()
                .map_err(|_| WasmError::Trap)?
                .push((capability, args.to_vec()));
            Ok(self.response.clone())
        }
    }

    fn package_digest() -> ArtifactId {
        ArtifactId::from_bytes(PACKAGE_FIXTURE)
    }

    fn manifest_json() -> String {
        format!(
            r#"{{"schema":"rapidlm.plugin_manifest","schema_version":1,"id":"acme.fmt","version":"1.2.3","publisher":"acme","digest":"{digest}","entrypoint":"plugin.wasm#rapidlm:plugin/tool@1.0.0","wit_version":"1.0.0","compatibility":{{"min":"1.0.0","max":"2.0.0"}},"requested_caps":[{{"capability":{{"schema":"rapidlm.capability","schema_version":1,"family":"fs","action":"read"}},"resource":{{"schema":"rapidlm.resource_descriptor","schema_version":1,"kind":"filesystem","root":"repo","glob":"src/**/*.rs"}}}}],"skills":["fmt"],"hooks":[],"mcp_servers":[]}}"#,
            digest = package_digest()
        )
    }

    fn load_manifest_ok() -> PluginManifest {
        crate::load_manifest(manifest_json().as_bytes(), &CancellationToken::new())
            .expect("valid manifest")
    }

    fn repo_fs_grant() -> GrantedCapability {
        GrantedCapability::new(
            Capability::FsRead,
            ResourceDescriptor::Filesystem(FilesystemScope::repo("src/**/*.rs").expect("fs")),
        )
        .expect("grant")
    }

    fn leb(mut value: u32) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if value == 0 {
                break;
            }
        }
        out
    }

    fn name(value: &str) -> Vec<u8> {
        let mut out = leb(value.len() as u32);
        out.extend_from_slice(value.as_bytes());
        out
    }

    fn section(id: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![id];
        out.extend(leb(body.len() as u32));
        out.extend_from_slice(body);
        out
    }

    fn module(sections: &[Vec<u8>]) -> Vec<u8> {
        let mut out = vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
        for section in sections {
            out.extend(section);
        }
        out
    }

    fn type_section(types: &[&[u8]]) -> Vec<u8> {
        let mut body = leb(types.len() as u32);
        for ty in types {
            body.extend_from_slice(ty);
        }
        section(1, &body)
    }

    fn fn_type(params: &[u8], results: &[u8]) -> Vec<u8> {
        let mut body = vec![0x60];
        body.extend(leb(params.len() as u32));
        body.extend_from_slice(params);
        body.extend(leb(results.len() as u32));
        body.extend_from_slice(results);
        body
    }

    fn function_section(idxs: &[u32]) -> Vec<u8> {
        let mut body = leb(idxs.len() as u32);
        for idx in idxs {
            body.extend(leb(*idx));
        }
        section(3, &body)
    }

    fn memory_section(min: u32, max: Option<u32>) -> Vec<u8> {
        let mut body = leb(1);
        match max {
            Some(max) => {
                body.push(0x01);
                body.extend(leb(min));
                body.extend(leb(max));
            }
            None => {
                body.push(0x00);
                body.extend(leb(min));
            }
        }
        section(5, &body)
    }

    fn export_func(export_name: &str, idx: u32) -> Vec<u8> {
        let mut body = leb(1);
        body.extend(name(export_name));
        body.push(0x00);
        body.extend(leb(idx));
        section(7, &body)
    }

    fn code_section(bodies: &[Vec<u8>]) -> Vec<u8> {
        let mut body = leb(bodies.len() as u32);
        for item in bodies {
            body.extend(leb(item.len() as u32));
            body.extend_from_slice(item);
        }
        section(10, &body)
    }

    fn import_func(module_name: &str, field: &str, type_idx: u32) -> Vec<u8> {
        let mut body = leb(1);
        body.extend(name(module_name));
        body.extend(name(field));
        body.push(0x00);
        body.extend(leb(type_idx));
        section(2, &body)
    }

    fn infinite_loop_wasm() -> Vec<u8> {
        module(&[
            type_section(&[&fn_type(&[], &[])]),
            function_section(&[0]),
            export_func("call", 0),
            code_section(&[vec![0x00, 0x03, 0x40, 0x0c, 0x00, 0x0b, 0x0b]]),
        ])
    }

    fn memory_bomb_wasm() -> Vec<u8> {
        module(&[
            type_section(&[&fn_type(&[], &[])]),
            function_section(&[0]),
            memory_section(1, Some(1024)),
            export_func("call", 0),
            code_section(&[vec![
                0x00, 0x03, 0x40, 0x41, 0x01, 0x40, 0x00, 0x1a, 0x0c, 0x00, 0x0b, 0x0b,
            ]]),
        ])
    }

    fn return_zero_wasm() -> Vec<u8> {
        module(&[
            type_section(&[&fn_type(&[], &[0x7f])]),
            function_section(&[0]),
            export_func("call", 0),
            code_section(&[vec![0x00, 0x41, 0x00, 0x0b]]),
        ])
    }

    /// A single exported function whose body is nothing but `call 0` (itself)
    /// followed by `end` — no block/loop/if, so it pushes zero structured
    /// control labels on every activation.
    fn self_recursive_wasm() -> Vec<u8> {
        module(&[
            type_section(&[&fn_type(&[], &[])]),
            function_section(&[0]),
            export_func("call", 0),
            code_section(&[vec![0x00, 0x10, 0x00, 0x0b]]),
        ])
    }

    fn host_call_wasm(module_name: &str, field: &str) -> Vec<u8> {
        module(&[
            type_section(&[
                &fn_type(&[0x7f, 0x7f, 0x7f, 0x7f], &[0x7f]),
                &fn_type(&[], &[0x7f]),
            ]),
            import_func(module_name, field, 0),
            function_section(&[1]),
            memory_section(1, None),
            export_func("call", 1),
            code_section(&[vec![
                0x00, 0x41, 0x00, 0x41, 0x00, 0x41, 0x00, 0x41, 0x00, 0x10, 0x00, 0x0b,
            ]]),
        ])
    }

    fn tight_limits() -> ResourceLimits {
        ResourceLimits::new(
            2_000,
            Duration::from_millis(50),
            2 * WASM_PAGE_BYTES,
            4_096,
            64 * 1024,
            32,
        )
        .expect("limits")
    }

    fn instantiate_ok(wasm: &[u8], granted: &[GrantedCapability]) -> PluginInstance {
        WasmPluginHost::new(tight_limits())
            .instantiate(
                wasm,
                &load_manifest_ok(),
                granted,
                &CancellationToken::new(),
            )
            .expect("instantiate")
    }

    #[test]
    fn plugin_instance_has_no_ambient_access_by_default() {
        let instance = instantiate_ok(&return_zero_wasm(), &[]);
        assert!(!instance.has_ambient_filesystem());
        assert!(!instance.has_ambient_network());
        assert!(!instance.has_ambient_process());
        assert!(instance.granted_capabilities().is_empty());
        let response = {
            let mut instance = instance;
            instance
                .call(
                    &PluginRequest::new("ping", "{}").expect("req"),
                    &CancellationToken::new(),
                )
                .expect("call")
        };
        assert_eq!(response.result_json(), "{}");
        assert!(response.evidence_refs().is_empty());
    }

    #[test]
    fn infinite_loop_fixture_terminates_within_fuel() {
        let started = Instant::now();
        let err = WasmPluginHost::new(tight_limits())
            .instantiate(
                &infinite_loop_wasm(),
                &load_manifest_ok(),
                &[],
                &CancellationToken::new(),
            )
            .expect("instantiate")
            .call(
                &PluginRequest::new("loop", "{}").expect("req"),
                &CancellationToken::new(),
            )
            .expect_err("loop must terminate");
        assert_eq!(err, WasmError::FuelExhausted);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(!err.to_string().contains("loop"));
    }

    #[test]
    fn memory_fixture_terminates_within_limit() {
        let started = Instant::now();
        let err = WasmPluginHost::new(tight_limits())
            .instantiate(
                &memory_bomb_wasm(),
                &load_manifest_ok(),
                &[],
                &CancellationToken::new(),
            )
            .expect("instantiate")
            .call(
                &PluginRequest::new("grow", "{}").expect("req"),
                &CancellationToken::new(),
            )
            .expect_err("memory bomb must terminate");
        assert_eq!(err, WasmError::MemoryLimit);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn bare_self_recursion_is_stopped_by_max_stack_frames() {
        // A guest function that calls itself with no Block/Loop/If never
        // pushes a structured-control label, so a check keyed on label
        // nesting can never see this recursion — it must be bounded by real
        // call-stack depth instead, or it native-stack-overflows the host
        // process (an uncatchable abort) well before `tight_limits()`'s
        // 2_000-unit fuel cap could stop it.
        let err = WasmPluginHost::new(tight_limits())
            .instantiate(
                &self_recursive_wasm(),
                &load_manifest_ok(),
                &[],
                &CancellationToken::new(),
            )
            .expect("instantiate")
            .call(
                &PluginRequest::new("run", "{}").expect("req"),
                &CancellationToken::new(),
            )
            .expect_err("unbounded self-recursion must be rejected");
        assert_eq!(err, WasmError::StackOverflow);
    }

    #[test]
    fn time_limit_terminates_loop_before_fuel() {
        let limits = ResourceLimits::new(
            HARD_MAX_FUEL,
            Duration::from_millis(1),
            2 * WASM_PAGE_BYTES,
            4_096,
            64 * 1024,
            32,
        )
        .expect("limits");
        let err = WasmPluginHost::new(limits)
            .instantiate(
                &infinite_loop_wasm(),
                &load_manifest_ok(),
                &[],
                &CancellationToken::new(),
            )
            .expect("instantiate")
            .call(
                &PluginRequest::new("loop", "{}").expect("req"),
                &CancellationToken::new(),
            )
            .expect_err("time bound");
        assert_eq!(err, WasmError::TimeLimit);
    }

    #[test]
    fn cancellation_stops_execution() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = instantiate(&return_zero_wasm(), &load_manifest_ok(), &[], &cancel)
            .expect_err("cancelled");
        assert_eq!(err, WasmError::Cancelled);
        assert!(err.code().is_none());
    }

    #[test]
    fn wasi_preview1_denied_even_when_fs_granted() {
        let wasm = host_call_wasm("wasi_snapshot_preview1", "fd_write");
        let err = WasmPluginHost::new(tight_limits())
            .instantiate(
                &wasm,
                &load_manifest_ok(),
                &[repo_fs_grant()],
                &CancellationToken::new(),
            )
            .expect_err("wasi must not instantiate");
        assert_eq!(err, WasmError::AmbientWasi);
        assert_eq!(
            err.into_api_error(TraceId::new()).expect("api").code(),
            ErrorCode::PluginCapabilityDenied
        );
        assert!(!WasmError::AmbientWasi.to_string().contains("fd_write"));
        assert!(
            !WasmError::AmbientWasi
                .to_string()
                .contains("wasi_snapshot_preview1")
        );
    }

    #[test]
    fn wasi_filesystem_preopen_denied_with_empty_grants() {
        let wasm = host_call_wasm("wasi:filesystem/preopens", "get-directories");
        let err = instantiate(&wasm, &load_manifest_ok(), &[], &CancellationToken::new())
            .expect_err("preopen");
        assert_eq!(err, WasmError::AmbientWasi);
    }

    #[test]
    fn capability_denial_cannot_use_wasi_defaults() {
        let wasm = host_call_wasm(HOST_WIT_MODULE, "fs.read");
        let err = instantiate(&wasm, &load_manifest_ok(), &[], &CancellationToken::new())
            .expect_err("ungranted host import");
        assert_eq!(err, WasmError::CapabilityDenied);

        let net = host_call_wasm(HOST_WIT_MODULE, "net.connect");
        let err = WasmPluginHost::new(tight_limits())
            .instantiate(
                &net,
                &load_manifest_ok(),
                &[repo_fs_grant()],
                &CancellationToken::new(),
            )
            .expect_err("fs grant is not net");
        assert_eq!(err, WasmError::CapabilityDenied);
    }

    #[test]
    fn granted_host_function_is_the_only_import() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let host = RecordingHost {
            calls: Arc::clone(&calls),
            response: Vec::new(),
        };
        let mut instance = WasmPluginHost::new(tight_limits())
            .instantiate_with_host(
                &host_call_wasm(HOST_WIT_MODULE, "fs.read"),
                &load_manifest_ok(),
                &[repo_fs_grant()],
                Box::new(host),
                &CancellationToken::new(),
            )
            .expect("granted import");
        assert!(!instance.has_ambient_filesystem());
        let response = instance
            .call(
                &PluginRequest::new("read", "{}").expect("req"),
                &CancellationToken::new(),
            )
            .expect("host call");
        assert_eq!(response.result_json(), "{}");
        let recorded = calls.lock().expect("lock");
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].0, Capability::FsRead);
    }

    #[test]
    fn grant_not_declared_on_manifest_is_rejected() {
        let net = GrantedCapability::new(
            Capability::NetConnect,
            ResourceDescriptor::Network(
                NetworkScope::new(NetworkScheme::Https, "example.com", 443).expect("net"),
            ),
        )
        .expect("grant");
        let err = instantiate(
            &return_zero_wasm(),
            &load_manifest_ok(),
            &[net],
            &CancellationToken::new(),
        )
        .expect_err("undeclared grant");
        assert_eq!(err, WasmError::GrantNotRequested);
    }

    #[test]
    fn deny_all_host_blocks_side_effect_after_grant() {
        let err = WasmPluginHost::new(tight_limits())
            .instantiate(
                &host_call_wasm(HOST_WIT_MODULE, "fs.read"),
                &load_manifest_ok(),
                &[repo_fs_grant()],
                &CancellationToken::new(),
            )
            .expect("linked")
            .call(
                &PluginRequest::new("read", "{}").expect("req"),
                &CancellationToken::new(),
            )
            .expect_err("deny-all");
        assert_eq!(err, WasmError::CapabilityDenied);
    }

    #[test]
    fn oversized_module_and_invalid_limits_fail_closed() {
        let too_big = vec![0u8; DEFAULT_MAX_MODULE_BYTES + 1];
        assert_eq!(
            instantiate(
                &too_big,
                &load_manifest_ok(),
                &[],
                &CancellationToken::new()
            )
            .expect_err("size"),
            WasmError::ModuleTooLarge
        );
        assert_eq!(
            ResourceLimits::new(
                HARD_MAX_FUEL + 1,
                DEFAULT_MAX_DURATION,
                DEFAULT_MAX_MEMORY_BYTES,
                DEFAULT_MAX_OUTPUT_BYTES,
                DEFAULT_MAX_MODULE_BYTES,
                DEFAULT_MAX_STACK_FRAMES,
            )
            .expect_err("fuel"),
            WasmError::InvalidLimits
        );
        let component = {
            let mut bytes = return_zero_wasm();
            bytes[4..8].copy_from_slice(&WASM_COMPONENT_VERSION);
            bytes
        };
        assert_eq!(
            instantiate(
                &component,
                &load_manifest_ok(),
                &[],
                &CancellationToken::new()
            )
            .expect_err("component"),
            WasmError::UnsupportedComponent
        );
    }

    #[test]
    fn error_display_does_not_echo_untrusted_bytes() {
        for err in [
            WasmError::AmbientWasi,
            WasmError::CapabilityDenied,
            WasmError::InvalidModule,
            WasmError::Trap,
        ] {
            let text = err.to_string();
            assert!(!text.contains("/etc/passwd"));
            assert!(!text.contains("fd_write"));
            assert!(!text.contains("wasi_snapshot_preview1"));
            assert!(!text.contains("get-directories"));
        }
    }
}
