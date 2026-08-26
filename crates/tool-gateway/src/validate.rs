//! Validate provider tool-call arguments against the canonical catalog.
//!
//! This is the privilege gate before Capability Broker. Malformed or oversized
//! arguments never become a [`CanonicalToolInvocation`]. Display never echoes
//! argument values. Threats: `T-001`, `T-002`, `T-003`, `T-007`, `T-012`, `T-020`.

use std::error::Error;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use protocol::{ArtifactId, ErrorCode, RepoPath};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::schema::{
    GatewayTool, MAX_EXTERNAL_ARGUMENTS_BYTES, MAX_PATCH_OPS, ToolCatalog, denied_argument_name,
};

/// Wire schema version for [`ToolCall`] / [`CanonicalToolInvocation`].
pub const TOOL_INVOCATION_SCHEMA: u16 = 1;

/// Maximum UTF-8 bytes accepted in `call_id`.
pub const MAX_CALL_ID_BYTES: usize = 128;

/// Maximum UTF-8 bytes of raw argument JSON accepted before parse.
///
/// Matches the workspace combined patch bound plus a small envelope.
pub const MAX_ARGUMENT_BYTES: usize = 32 * 1024 * 1024 + 64 * 1024;

/// Maximum bytes accepted for a provider-mapped call envelope.
pub const MAX_ENVELOPE_BYTES: usize = MAX_ARGUMENT_BYTES + 1024;

/// Combined `content` bytes allowed across one `workspace.patch` call.
pub const MAX_PATCH_TOTAL_BYTES: usize = 32 * 1024 * 1024;

/// Maximum nesting depth of argument JSON.
pub const MAX_JSON_DEPTH: usize = 16;

/// Maximum JSON values visited while validating one call.
pub const MAX_JSON_NODES: usize = 65_536;

/// Maximum issues captured on one validation pass.
pub const MAX_ISSUES: usize = 16;

const CANCEL_STRIDE: usize = 16;

/// One schema violation located at a JSON pointer under the arguments root.
///
/// The pointer is relative to the arguments object, e.g. `/query` or
/// `/ops/1/content`. Display never echoes the offending value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepairIssue {
    path: String,
    error: ValidateError,
}

impl RepairIssue {
    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn error(&self) -> ValidateError {
        self.error
    }
}

const UUID_PATTERN: &str = "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$";
const ARTIFACT_ID_PATTERN: &str = "^sha256:[0-9a-f]{64}$";
const IDENT_PATTERN: &str = "^[A-Za-z0-9][A-Za-z0-9._:-]{0,254}$";
const REPO_PATH_PATTERN: &str = r"^[^/\x00][^\\:\x00]*$";

/// Cooperative cancellation for validation of large argument trees.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

/// Untrusted provider-mapped tool call. Construct via [`ToolCall::parse`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolCall {
    schema: u16,
    call_id: String,
    tool: String,
    arguments: Value,
}

/// Catalog-validated invocation. The only value dispatch may receive.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalToolInvocation {
    schema: u16,
    call_id: String,
    tool: GatewayTool,
    arguments: Value,
}

/// Typed validation failure. Display never echoes argument values.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ValidateError {
    Cancelled,
    UnsupportedSchema,
    UnknownTool,
    InvalidCallId,
    MalformedArguments,
    OversizedArguments,
    DeniedArgument,
    SchemaMismatch,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCall {
    schema: u16,
    call_id: String,
    tool: String,
    arguments: Value,
}

struct CheckCtx<'a> {
    cancel: &'a CancellationToken,
    nodes: usize,
    path: Vec<String>,
    issues: Vec<RepairIssue>,
    collect: bool,
}

impl CheckCtx<'_> {
    fn record(&mut self, error: ValidateError) {
        if self.collect && self.issues.len() < MAX_ISSUES {
            self.issues.push(RepairIssue {
                path: path_pointer(&self.path),
                error,
            });
        }
    }

    fn push(&mut self, segment: impl Into<String>) {
        self.path.push(segment.into());
    }

    fn pop(&mut self) {
        self.path.pop();
    }
}

impl CancellationToken {
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolCall {
    /// Parse a provider-mapped envelope. Oversized input is rejected before JSON.
    pub fn parse(bytes: &[u8]) -> Result<Self, ValidateError> {
        if bytes.len() > MAX_ENVELOPE_BYTES {
            return Err(ValidateError::OversizedArguments);
        }
        let raw: RawCall =
            serde_json::from_slice(bytes).map_err(|_| ValidateError::MalformedArguments)?;
        let arguments = coerce_arguments(raw.arguments)?;
        Ok(Self {
            schema: raw.schema,
            call_id: raw.call_id,
            tool: raw.tool,
            arguments,
        })
    }

    /// Structured constructor. [`validate`] still enforces every bound.
    pub fn new(
        schema: u16,
        call_id: impl Into<String>,
        tool: impl Into<String>,
        arguments: Value,
    ) -> Self {
        Self {
            schema,
            call_id: call_id.into(),
            tool: tool.into(),
            arguments,
        }
    }

    pub fn schema(&self) -> u16 {
        self.schema
    }

    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    pub fn tool(&self) -> &str {
        &self.tool
    }

    pub fn arguments(&self) -> &Value {
        &self.arguments
    }
}

impl CanonicalToolInvocation {
    pub fn schema(&self) -> u16 {
        self.schema
    }

    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    pub fn tool(&self) -> GatewayTool {
        self.tool
    }

    pub fn arguments(&self) -> &Value {
        &self.arguments
    }
}

impl Serialize for CanonicalToolInvocation {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("CanonicalToolInvocation", 4)?;
        state.serialize_field("schema", &self.schema)?;
        state.serialize_field("call_id", &self.call_id)?;
        state.serialize_field("tool", self.tool.as_str())?;
        state.serialize_field("arguments", &self.arguments)?;
        state.end()
    }
}

impl ValidateError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "tool validation cancelled",
            Self::UnsupportedSchema => "unsupported tool-call schema",
            Self::UnknownTool => "unknown tool",
            Self::InvalidCallId => "invalid call id",
            Self::MalformedArguments => "malformed arguments",
            Self::OversizedArguments => "arguments exceed the byte bound",
            Self::DeniedArgument => "denied argument",
            Self::SchemaMismatch => "arguments do not match the catalog schema",
        }
    }

    /// Public code for validation failures. Cancellation is not a tool-argument error.
    pub const fn error_code(self) -> Option<ErrorCode> {
        match self {
            Self::Cancelled => None,
            _ => Some(ErrorCode::ToolInvalidArguments),
        }
    }
}

impl fmt::Display for ValidateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for ValidateError {}

/// Validate `call` against the v1 catalog. Failure never yields an invocation.
pub fn validate(
    call: &ToolCall,
    cancel: &CancellationToken,
) -> Result<CanonicalToolInvocation, ValidateError> {
    let mut ctx = CheckCtx {
        cancel,
        nodes: 0,
        path: Vec::new(),
        issues: Vec::new(),
        collect: false,
    };
    validate_inner(call, &mut ctx)
}

/// Validate and, on failure, return the located JSON-pointer issues.
///
/// Every issue is bounded to `MAX_ISSUES`. Cancellation or oversize are still
/// surfaced as `Cancelled`/`OversizedArguments` only when validation fails for
/// that reason; otherwise the first located schema errors are returned.
pub fn validate_issues(
    call: &ToolCall,
    cancel: &CancellationToken,
) -> Result<CanonicalToolInvocation, Vec<RepairIssue>> {
    let mut ctx = CheckCtx {
        cancel,
        nodes: 0,
        path: Vec::new(),
        issues: Vec::new(),
        collect: true,
    };
    match validate_inner(call, &mut ctx) {
        Ok(invocation) => Ok(invocation),
        Err(_) => Err(ctx.issues),
    }
}

fn validate_inner(
    call: &ToolCall,
    ctx: &mut CheckCtx<'_>,
) -> Result<CanonicalToolInvocation, ValidateError> {
    if ctx.cancel.is_cancelled() {
        return Err(ValidateError::Cancelled);
    }
    if call.schema != TOOL_INVOCATION_SCHEMA {
        ctx.record(ValidateError::UnsupportedSchema);
        return Err(ValidateError::UnsupportedSchema);
    }
    if !valid_call_id(&call.call_id) {
        ctx.record(ValidateError::InvalidCallId);
        return Err(ValidateError::InvalidCallId);
    }
    let tool = match GatewayTool::from_str(&call.tool) {
        Ok(tool) => tool,
        Err(_) => {
            ctx.record(ValidateError::UnknownTool);
            return Err(ValidateError::UnknownTool);
        }
    };
    let arguments = match &call.arguments {
        Value::Object(_) => call.arguments.clone(),
        Value::String(raw) => match coerce_arguments(Value::String(raw.clone())) {
            Ok(arguments) => arguments,
            Err(err) => {
                ctx.record(err);
                return Err(err);
            }
        },
        _ => {
            ctx.record(ValidateError::MalformedArguments);
            return Err(ValidateError::MalformedArguments);
        }
    };
    let encoded = match serde_json::to_vec(&arguments) {
        Ok(bytes) => bytes,
        Err(_) => {
            ctx.record(ValidateError::MalformedArguments);
            return Err(ValidateError::MalformedArguments);
        }
    };
    if encoded.len() > MAX_ARGUMENT_BYTES {
        ctx.record(ValidateError::OversizedArguments);
        return Err(ValidateError::OversizedArguments);
    }

    if let Err(err) = count_nodes(&arguments, 0, ctx) {
        ctx.record(err);
        return Err(err);
    }
    if let Err(err) = reject_denied_keys(&arguments, ctx) {
        ctx.record(err);
        return Err(err);
    }

    let catalog = ToolCatalog::v1();
    let schema = match catalog.get(tool) {
        Some(entry) => entry.parameters(),
        None => {
            ctx.record(ValidateError::UnknownTool);
            return Err(ValidateError::UnknownTool);
        }
    };
    apply_schema(schema, &arguments, 0, ctx)?;
    if let Err(err) = check_tool_payload_bounds(tool, &arguments) {
        ctx.record(err);
        return Err(err);
    }

    Ok(CanonicalToolInvocation {
        schema: TOOL_INVOCATION_SCHEMA,
        call_id: call.call_id.clone(),
        tool,
        arguments,
    })
}

/// Parse then validate a provider-mapped envelope.
pub fn validate_bytes(
    bytes: &[u8],
    cancel: &CancellationToken,
) -> Result<CanonicalToolInvocation, ValidateError> {
    let call = ToolCall::parse(bytes)?;
    validate(&call, cancel)
}

fn coerce_arguments(value: Value) -> Result<Value, ValidateError> {
    match value {
        Value::Object(_) => Ok(value),
        Value::String(raw) => {
            if raw.len() > MAX_ARGUMENT_BYTES {
                return Err(ValidateError::OversizedArguments);
            }
            let parsed: Value =
                serde_json::from_str(&raw).map_err(|_| ValidateError::MalformedArguments)?;
            if parsed.is_object() {
                Ok(parsed)
            } else {
                Err(ValidateError::MalformedArguments)
            }
        }
        _ => Err(ValidateError::MalformedArguments),
    }
}

fn valid_call_id(call_id: &str) -> bool {
    !call_id.is_empty()
        && call_id.len() <= MAX_CALL_ID_BYTES
        && call_id.bytes().all(|b| b >= 0x20 && b != 0x7f)
}

fn check_cancel(ctx: &CheckCtx<'_>) -> Result<(), ValidateError> {
    if ctx.cancel.is_cancelled() {
        Err(ValidateError::Cancelled)
    } else {
        Ok(())
    }
}

fn path_pointer(path: &[String]) -> String {
    if path.is_empty() {
        return "/".to_owned();
    }
    let mut pointer = String::with_capacity(path.iter().map(|s| s.len() + 1).sum());
    for segment in path {
        pointer.push('/');
        pointer.push_str(segment);
    }
    pointer
}

fn touch(ctx: &mut CheckCtx<'_>) -> Result<(), ValidateError> {
    ctx.nodes = ctx.nodes.saturating_add(1);
    if ctx.nodes > MAX_JSON_NODES {
        return Err(ValidateError::OversizedArguments);
    }
    if ctx.nodes.is_multiple_of(CANCEL_STRIDE) {
        check_cancel(ctx)?;
    }
    Ok(())
}

fn count_nodes(value: &Value, depth: usize, ctx: &mut CheckCtx<'_>) -> Result<(), ValidateError> {
    if depth > MAX_JSON_DEPTH {
        return Err(ValidateError::OversizedArguments);
    }
    touch(ctx)?;
    match value {
        Value::Array(items) => {
            for item in items {
                count_nodes(item, depth.saturating_add(1), ctx)?;
            }
        }
        Value::Object(map) => {
            for (key, child) in map {
                if key.chars().any(char::is_control) {
                    return Err(ValidateError::MalformedArguments);
                }
                count_nodes(child, depth.saturating_add(1), ctx)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn reject_denied_keys(value: &Value, ctx: &mut CheckCtx<'_>) -> Result<(), ValidateError> {
    check_cancel(ctx)?;
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if is_denied_key(key) {
                    return Err(ValidateError::DeniedArgument);
                }
                reject_denied_keys(child, ctx)?;
            }
        }
        Value::Array(items) => {
            for item in items {
                reject_denied_keys(item, ctx)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn is_denied_key(name: &str) -> bool {
    if denied_argument_name(name) {
        return true;
    }
    let lower = name.to_ascii_lowercase();
    lower != name && denied_argument_name(&lower)
}

fn check_tool_payload_bounds(tool: GatewayTool, arguments: &Value) -> Result<(), ValidateError> {
    match tool {
        GatewayTool::WorkspacePatch => {
            let Some(ops) = arguments.get("ops").and_then(Value::as_array) else {
                return Ok(());
            };
            if ops.len() > MAX_PATCH_OPS {
                return Err(ValidateError::OversizedArguments);
            }
            let mut total = 0usize;
            for op in ops {
                if let Some(content) = op.get("content").and_then(Value::as_str) {
                    total = total.saturating_add(content.len());
                    if total > MAX_PATCH_TOTAL_BYTES {
                        return Err(ValidateError::OversizedArguments);
                    }
                }
            }
            Ok(())
        }
        GatewayTool::ExternalCall => {
            let Some(extra) = arguments.get("arguments") else {
                return Ok(());
            };
            let encoded =
                serde_json::to_vec(extra).map_err(|_| ValidateError::MalformedArguments)?;
            if encoded.len() > MAX_EXTERNAL_ARGUMENTS_BYTES {
                Err(ValidateError::OversizedArguments)
            } else {
                Ok(())
            }
        }
        _ => Ok(()),
    }
}

fn apply_schema(
    schema: &Value,
    instance: &Value,
    depth: usize,
    ctx: &mut CheckCtx<'_>,
) -> Result<(), ValidateError> {
    if depth > MAX_JSON_DEPTH {
        let err = ValidateError::OversizedArguments;
        ctx.record(err);
        return Err(err);
    }
    check_cancel(ctx)?;
    let Value::Object(schema_obj) = schema else {
        let err = ValidateError::SchemaMismatch;
        ctx.record(err);
        return Err(err);
    };

    if let Some(expected) = schema_obj.get("type")
        && !type_matches(expected, instance)
    {
        let err = ValidateError::SchemaMismatch;
        ctx.record(err);
        return Err(err);
    }
    if let Some(expected) = schema_obj.get("const")
        && instance != expected
    {
        let err = ValidateError::SchemaMismatch;
        ctx.record(err);
        return Err(err);
    }
    if let Some(Value::Array(options)) = schema_obj.get("enum")
        && !options.iter().any(|option| option == instance)
    {
        let err = ValidateError::SchemaMismatch;
        ctx.record(err);
        return Err(err);
    }

    match instance {
        Value::String(text) => {
            if let Err(err) = apply_string_schema(schema_obj, text) {
                ctx.record(err);
                return Err(err);
            }
        }
        Value::Number(number) => {
            if let Err(err) = apply_number_schema(schema_obj, number) {
                ctx.record(err);
                return Err(err);
            }
        }
        Value::Object(map) => apply_object_schema(schema_obj, map, depth, ctx)?,
        Value::Array(items) => apply_array_schema(schema_obj, items, depth, ctx)?,
        _ => {}
    }

    if let Some(Value::Array(clauses)) = schema_obj.get("allOf") {
        for clause in clauses {
            apply_schema(clause, instance, depth.saturating_add(1), ctx)?;
        }
    }
    apply_conditional(schema_obj, instance, depth, ctx)?;
    Ok(())
}

fn schema_satisfied(
    schema: &Value,
    instance: &Value,
    depth: usize,
    ctx: &mut CheckCtx<'_>,
) -> Result<bool, ValidateError> {
    let collect = ctx.collect;
    ctx.collect = false;
    let result = match apply_schema(schema, instance, depth, ctx) {
        Ok(()) => Ok(true),
        Err(ValidateError::Cancelled) => Err(ValidateError::Cancelled),
        Err(ValidateError::OversizedArguments) => Err(ValidateError::OversizedArguments),
        Err(_) => Ok(false),
    };
    ctx.collect = collect;
    result
}

fn apply_conditional(
    schema_obj: &Map<String, Value>,
    instance: &Value,
    depth: usize,
    ctx: &mut CheckCtx<'_>,
) -> Result<(), ValidateError> {
    let Some(if_schema) = schema_obj.get("if") else {
        return Ok(());
    };
    let matched = schema_satisfied(if_schema, instance, depth.saturating_add(1), ctx)?;
    if matched {
        if let Some(then_schema) = schema_obj.get("then") {
            apply_schema(then_schema, instance, depth.saturating_add(1), ctx)?;
        }
    } else if let Some(else_schema) = schema_obj.get("else") {
        apply_schema(else_schema, instance, depth.saturating_add(1), ctx)?;
    }
    Ok(())
}

fn type_matches(expected: &Value, instance: &Value) -> bool {
    let Some(type_name) = expected.as_str() else {
        return false;
    };
    match type_name {
        "object" => instance.is_object(),
        "array" => instance.is_array(),
        "string" => instance.is_string(),
        "boolean" => instance.is_boolean(),
        "integer" => as_integer(instance).is_some(),
        "number" => instance.is_number(),
        "null" => instance.is_null(),
        _ => false,
    }
}

fn apply_string_schema(schema_obj: &Map<String, Value>, text: &str) -> Result<(), ValidateError> {
    if let Some(min) = schema_obj.get("minLength").and_then(Value::as_u64)
        && (text.len() as u64) < min
    {
        return Err(ValidateError::SchemaMismatch);
    }
    if let Some(max) = schema_obj.get("maxLength").and_then(Value::as_u64)
        && (text.len() as u64) > max
    {
        return Err(ValidateError::OversizedArguments);
    }
    if let Some(pattern) = schema_obj.get("pattern").and_then(Value::as_str)
        && !pattern_matches(pattern, text)
    {
        return Err(ValidateError::SchemaMismatch);
    }
    Ok(())
}

fn apply_number_schema(
    schema_obj: &Map<String, Value>,
    number: &serde_json::Number,
) -> Result<(), ValidateError> {
    let Some(value) = integer_i64(number) else {
        if schema_obj.get("type").and_then(Value::as_str) == Some("integer") {
            return Err(ValidateError::SchemaMismatch);
        }
        return Ok(());
    };
    if let Some(min) = schema_obj.get("minimum").and_then(json_i64)
        && value < min
    {
        return Err(ValidateError::SchemaMismatch);
    }
    if let Some(max) = schema_obj.get("maximum").and_then(json_i64)
        && value > max
    {
        return Err(ValidateError::SchemaMismatch);
    }
    Ok(())
}

fn apply_object_schema(
    schema_obj: &Map<String, Value>,
    map: &Map<String, Value>,
    depth: usize,
    ctx: &mut CheckCtx<'_>,
) -> Result<(), ValidateError> {
    if let Some(max) = schema_obj.get("maxProperties").and_then(Value::as_u64)
        && (map.len() as u64) > max
    {
        let err = ValidateError::OversizedArguments;
        ctx.record(err);
        return Err(err);
    }
    if let Some(Value::Array(required)) = schema_obj.get("required") {
        for key in required {
            let Some(name) = key.as_str() else {
                let err = ValidateError::SchemaMismatch;
                ctx.record(err);
                return Err(err);
            };
            if !map.contains_key(name) {
                ctx.push(name);
                ctx.record(ValidateError::SchemaMismatch);
                ctx.pop();
                return Err(ValidateError::SchemaMismatch);
            }
        }
    }
    let properties = schema_obj.get("properties").and_then(Value::as_object);
    let additional = schema_obj.get("additionalProperties");
    for (key, value) in map {
        if let Some(prop_schema) = properties.and_then(|props| props.get(key)) {
            ctx.push(key);
            let result = apply_schema(prop_schema, value, depth.saturating_add(1), ctx);
            ctx.pop();
            result?;
            continue;
        }
        match additional {
            None | Some(Value::Bool(true)) => {}
            Some(Value::Bool(false)) => {
                ctx.push(key);
                ctx.record(ValidateError::SchemaMismatch);
                ctx.pop();
                return Err(ValidateError::SchemaMismatch);
            }
            Some(extra_schema) => {
                ctx.push(key);
                let result = apply_schema(extra_schema, value, depth.saturating_add(1), ctx);
                ctx.pop();
                result?;
            }
        }
    }
    Ok(())
}

fn apply_array_schema(
    schema_obj: &Map<String, Value>,
    items: &[Value],
    depth: usize,
    ctx: &mut CheckCtx<'_>,
) -> Result<(), ValidateError> {
    if let Some(min) = schema_obj.get("minItems").and_then(Value::as_u64)
        && (items.len() as u64) < min
    {
        let err = ValidateError::SchemaMismatch;
        ctx.record(err);
        return Err(err);
    }
    if let Some(max) = schema_obj.get("maxItems").and_then(Value::as_u64)
        && (items.len() as u64) > max
    {
        let err = ValidateError::OversizedArguments;
        ctx.record(err);
        return Err(err);
    }
    if schema_obj.get("uniqueItems") == Some(&Value::Bool(true)) {
        for (index, item) in items.iter().enumerate() {
            if items[..index].iter().any(|seen| seen == item) {
                let err = ValidateError::SchemaMismatch;
                ctx.push(index.to_string());
                ctx.record(err);
                ctx.pop();
                return Err(err);
            }
        }
    }
    if let Some(item_schema) = schema_obj.get("items") {
        for (index, item) in items.iter().enumerate() {
            ctx.push(index.to_string());
            let result = apply_schema(item_schema, item, depth.saturating_add(1), ctx);
            ctx.pop();
            result?;
        }
    }
    Ok(())
}

fn pattern_matches(pattern: &str, value: &str) -> bool {
    match pattern {
        UUID_PATTERN => is_uuid(value),
        ARTIFACT_ID_PATTERN => ArtifactId::from_str(value).is_ok(),
        IDENT_PATTERN => is_ident(value),
        REPO_PATH_PATTERN => is_repo_path_pattern(value) && RepoPath::parse(value).is_ok(),
        _ => false,
    }
}

fn is_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (index, byte) in bytes.iter().copied().enumerate() {
        let hex = matches!(byte, b'0'..=b'9' | b'a'..=b'f');
        let hyphen = byte == b'-' && matches!(index, 8 | 13 | 18 | 23);
        if !hex && !hyphen {
            return false;
        }
        if matches!(index, 8 | 13 | 18 | 23) && !hyphen {
            return false;
        }
    }
    true
}

fn is_ident(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphanumeric() || value.len() > 256 {
        return false;
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | ':' | '-'))
}

fn is_repo_path_pattern(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if first == '/' || first == '\0' {
        return false;
    }
    chars.all(|ch| ch != '\\' && ch != ':' && ch != '\0')
}

fn as_integer(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => integer_i64(number),
        _ => None,
    }
}

fn integer_i64(number: &serde_json::Number) -> Option<i64> {
    number
        .as_i64()
        .or_else(|| number.as_u64().and_then(|value| i64::try_from(value).ok()))
}

fn json_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => integer_i64(number),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CONTRACT_CALL: &str = r#"{"schema":1,"call_id":"call_7","tool":"repo.search","arguments":{"query":"CapabilityLease","repos":["main"],"limit":20}}"#;
    const AGENT_ID: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ab";
    const ARTIFACT: &str =
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const SECRET: &str = "hunter2-capability-lease";

    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    fn ok(tool: &str, arguments: Value) -> CanonicalToolInvocation {
        let call = ToolCall::new(TOOL_INVOCATION_SCHEMA, "call_7", tool, arguments);
        validate(&call, &live()).expect("valid call")
    }

    fn err(tool: &str, arguments: Value) -> ValidateError {
        let call = ToolCall::new(TOOL_INVOCATION_SCHEMA, "call_7", tool, arguments);
        validate(&call, &live()).expect_err("invalid call")
    }

    fn assert_no_secret(err: ValidateError) {
        let rendered = err.to_string();
        assert!(
            !rendered.contains(SECRET),
            "error leaked payload: {rendered}"
        );
        assert!(
            !rendered.contains("hunter2"),
            "error leaked canary: {rendered}"
        );
        assert_eq!(err.error_code(), Some(ErrorCode::ToolInvalidArguments));
    }

    #[test]
    fn contract_example_validates() {
        let invocation = validate_bytes(CONTRACT_CALL.as_bytes(), &live()).expect("contract");
        assert_eq!(invocation.schema(), TOOL_INVOCATION_SCHEMA);
        assert_eq!(invocation.call_id(), "call_7");
        assert_eq!(invocation.tool(), GatewayTool::RepoSearch);
        assert_eq!(invocation.arguments()["query"], "CapabilityLease");
        let encoded = serde_json::to_string(&invocation).expect("serialize");
        assert!(encoded.contains("\"tool\":\"repo.search\""));
        assert!(!encoded.contains(SECRET));
    }

    #[test]
    fn each_v1_tool_accepts_minimal_valid_arguments() {
        let cases: &[(&str, Value)] = &[
            ("repo.search", json!({"query": "lease"})),
            (
                "repo.read",
                json!({"path": "crates/tool-gateway/src/lib.rs"}),
            ),
            (
                "workspace.patch",
                json!({"ops":[{"op":"create_file","path":"src/lib.rs","content":""}]}),
            ),
            ("workspace.status", json!({})),
            ("shell.exec", json!({"argv":["rg","lease"]})),
            (
                "agent.spawn",
                json!({"role":"explorer","task":"map the crate","access":"read_only"}),
            ),
            (
                "agent.result",
                json!({"agent_id": AGENT_ID, "action":"inspect"}),
            ),
            ("goal.update", json!({"action":"pause","reason":"wait"})),
            ("browser.act", json!({"action":"observe"})),
            ("mobile.act", json!({"action":"observe"})),
            (
                "external.call",
                json!({"kind":"mcp","server":"docs","tool":"search"}),
            ),
            (
                "evidence.record",
                json!({"kind":"test","assertion":"unit pass","subject_ref":"crates/tool-gateway"}),
            ),
        ];
        for (tool, arguments) in cases {
            let invocation = ok(tool, arguments.clone());
            assert_eq!(invocation.tool().as_str(), *tool);
        }
    }

    #[test]
    fn malformed_and_unknown_never_yield_invocation() {
        assert_eq!(
            ToolCall::parse(b"not-json").expect_err("malformed"),
            ValidateError::MalformedArguments
        );
        assert_eq!(
            err("sudo.exec", json!({"argv":["id"]})),
            ValidateError::UnknownTool
        );
        assert_eq!(
            err("repo.delete", json!({"path":"x"})),
            ValidateError::UnknownTool
        );
        let mut call = ToolCall::new(2, "call_7", "repo.search", json!({"query":"x"}));
        assert_eq!(
            validate(&call, &live()).expect_err("schema"),
            ValidateError::UnsupportedSchema
        );
        call = ToolCall::new(1, "", "repo.search", json!({"query":"x"}));
        assert_eq!(
            validate(&call, &live()).expect_err("call id"),
            ValidateError::InvalidCallId
        );
        call = ToolCall::new(1, "call_7", "repo.search", json!(["query"]));
        assert_eq!(
            validate(&call, &live()).expect_err("array args"),
            ValidateError::MalformedArguments
        );
        for err in [
            ValidateError::UnknownTool,
            ValidateError::MalformedArguments,
            ValidateError::OversizedArguments,
            ValidateError::DeniedArgument,
            ValidateError::SchemaMismatch,
        ] {
            assert_eq!(err.error_code(), Some(ErrorCode::ToolInvalidArguments));
        }
    }

    #[test]
    fn oversized_envelope_rejected_before_parse() {
        let mut bytes = Vec::from(
            &b"{\"schema\":1,\"call_id\":\"x\",\"tool\":\"repo.search\",\"arguments\":{"[..],
        );
        bytes.extend(std::iter::repeat_n(b'x', MAX_ENVELOPE_BYTES));
        assert_eq!(
            ToolCall::parse(&bytes).expect_err("oversized"),
            ValidateError::OversizedArguments
        );
    }

    #[test]
    fn extra_and_denied_arguments_fail_closed() {
        assert_eq!(
            err(
                "repo.search",
                json!({"query":"x","capability_lease": SECRET})
            ),
            ValidateError::DeniedArgument
        );
        assert_eq!(
            err(
                "repo.read",
                json!({"path":"src/lib.rs","host_path":"/etc/passwd"})
            ),
            ValidateError::DeniedArgument
        );
        assert_eq!(
            err("shell.exec", json!({"argv":["id"],"sudo":true})),
            ValidateError::DeniedArgument
        );
        assert_eq!(
            err(
                "goal.update",
                json!({"action":"complete","force_complete":true})
            ),
            ValidateError::DeniedArgument
        );
        assert_eq!(
            err(
                "external.call",
                json!({
                    "kind":"mcp",
                    "server":"docs",
                    "tool":"search",
                    "arguments":{"secret_plaintext": SECRET}
                })
            ),
            ValidateError::DeniedArgument
        );
        assert_eq!(
            err(
                "external.call",
                json!({
                    "kind":"plugin",
                    "plugin":"ext",
                    "tool":"run",
                    "arguments":{"Capability_Lease": SECRET}
                })
            ),
            ValidateError::DeniedArgument
        );
        assert_eq!(
            err("repo.search", json!({"query":"x","unexpected":true})),
            ValidateError::SchemaMismatch
        );
        assert_no_secret(err(
            "repo.search",
            json!({"query": SECRET, "password": SECRET}),
        ));
    }

    #[test]
    fn shell_string_is_never_inferred() {
        assert_eq!(
            err("shell.exec", json!({"script":"rm -rf /"})),
            ValidateError::SchemaMismatch
        );
        assert_eq!(
            err("shell.exec", json!({"argv":["id"],"shell":true})),
            ValidateError::SchemaMismatch
        );
        let invocation = ok("shell.exec", json!({"shell":true,"script":"echo ok"}));
        assert_eq!(invocation.arguments()["shell"], true);
        assert!(invocation.arguments().get("env").is_none());
    }

    #[test]
    fn path_traversal_and_absolute_paths_rejected() {
        for path in [
            "../etc/passwd",
            "/etc/passwd",
            r"C:\Windows\System32",
            r"..\secret",
            "foo/../../etc/shadow",
        ] {
            let error = err("repo.read", json!({"path": path}));
            assert_eq!(error, ValidateError::SchemaMismatch, "accepted {path}");
            assert!(!error.to_string().contains(path));
        }
        assert_eq!(
            err("shell.exec", json!({"argv":["ls"],"cwd":"../"})),
            ValidateError::SchemaMismatch
        );
    }

    #[test]
    fn computer_use_non_observe_requires_observation_id() {
        assert_eq!(
            err("browser.act", json!({"action":"click"})),
            ValidateError::SchemaMismatch
        );
        assert_eq!(
            err("mobile.act", json!({"action":"tap"})),
            ValidateError::SchemaMismatch
        );
        let invocation = ok(
            "browser.act",
            json!({"action":"click","observation_id":"obs-1"}),
        );
        assert_eq!(invocation.arguments()["observation_id"], "obs-1");
    }

    #[test]
    fn missing_required_and_wrong_types_fail() {
        assert_eq!(err("repo.search", json!({})), ValidateError::SchemaMismatch);
        assert_eq!(
            err("repo.search", json!({"query": 1})),
            ValidateError::SchemaMismatch
        );
        assert_eq!(
            err("repo.search", json!({"query":"x","limit":20.5})),
            ValidateError::SchemaMismatch
        );
        assert_eq!(
            err("repo.search", json!({"query":"x","mode":"regex"})),
            ValidateError::SchemaMismatch
        );
        assert_eq!(
            err(
                "workspace.patch",
                json!({"ops":[{"op":"replace_range","path":"a.rs"}]})
            ),
            ValidateError::SchemaMismatch
        );
        assert_eq!(
            err("external.call", json!({"kind":"mcp","tool":"search"})),
            ValidateError::SchemaMismatch
        );
        assert_eq!(
            err(
                "external.call",
                json!({"kind":"plugin","server":"x","tool":"search"})
            ),
            ValidateError::SchemaMismatch
        );
        let artifact = ok(
            "evidence.record",
            json!({
                "kind":"artifact",
                "assertion":"built",
                "subject_ref":"target/debug/tool-gateway",
                "artifact_id": ARTIFACT
            }),
        );
        assert_eq!(artifact.arguments()["artifact_id"], ARTIFACT);
        assert_eq!(
            err(
                "evidence.record",
                json!({
                    "kind":"artifact",
                    "assertion":"built",
                    "subject_ref":"x",
                    "artifact_id":"sha256:deadbeef"
                })
            ),
            ValidateError::SchemaMismatch
        );
    }

    #[test]
    fn string_argument_json_is_accepted_after_parse() {
        let call = ToolCall::parse(
            br#"{"schema":1,"call_id":"call_7","tool":"repo.search","arguments":"{\"query\":\"ok\"}"}"#,
        )
        .expect("string args");
        let invocation = validate(&call, &live()).expect("coerced");
        assert_eq!(invocation.arguments()["query"], "ok");
    }

    #[test]
    fn cancellation_is_honored() {
        let cancel = live();
        cancel.cancel();
        let call = ToolCall::new(
            TOOL_INVOCATION_SCHEMA,
            "call_7",
            "repo.search",
            json!({"query":"x"}),
        );
        assert_eq!(
            validate(&call, &cancel).expect_err("cancel"),
            ValidateError::Cancelled
        );
        assert_eq!(ValidateError::Cancelled.error_code(), None);
        assert_eq!(
            ValidateError::Cancelled.to_string(),
            "tool validation cancelled"
        );
    }

    #[test]
    fn provider_string_arguments_over_bound_are_rejected() {
        let huge = "x".repeat(MAX_ARGUMENT_BYTES + 1);
        assert_eq!(
            coerce_arguments(Value::String(huge)).expect_err("bound"),
            ValidateError::OversizedArguments
        );
    }

    #[test]
    fn fuzz_parser_and_validator_have_no_panic() {
        let mut corpus: Vec<Vec<u8>> = vec![
            Vec::new(),
            b"null".to_vec(),
            b"[]".to_vec(),
            b"{".to_vec(),
            b"{\"schema\":1}".to_vec(),
            CONTRACT_CALL.as_bytes().to_vec(),
            br#"{"schema":1,"call_id":"call_7","tool":"repo.search","arguments":{"query":"x","lease":"y"}}"#.to_vec(),
            format!(
                r#"{{"schema":1,"call_id":"c","tool":"shell.exec","arguments":{{"script":"{SECRET}"}}}}"#
            )
            .into_bytes(),
            format!(
                r#"{{"schema":1,"call_id":"c","tool":"repo.read","arguments":{{"path":"../{SECRET}"}}}}"#
            )
            .into_bytes(),
            br#"{"schema":1,"call_id":"c","tool":"browser.act","arguments":{"action":"click"}}"#.to_vec(),
            br#"{"schema":1,"call_id":"c","tool":"goal.update","arguments":{"action":"force_complete"}}"#.to_vec(),
            br#"{"schema":1,"call_id":"c\u0000","tool":"repo.search","arguments":{}}"#.to_vec(),
            vec![0xff, 0xfe, 0x00],
            vec![b'{'; 64],
        ];

        let mut nested =
            String::from("{\"schema\":1,\"call_id\":\"c\",\"tool\":\"repo.search\",\"arguments\":");
        for _ in 0..32 {
            nested.push_str("{\"a\":");
        }
        nested.push_str("1");
        for _ in 0..32 {
            nested.push('}');
        }
        nested.push('}');
        corpus.push(nested.into_bytes());

        let mut wide = String::from(
            "{\"schema\":1,\"call_id\":\"c\",\"tool\":\"repo.search\",\"arguments\":{",
        );
        for i in 0..300 {
            if i > 0 {
                wide.push(',');
            }
            wide.push_str(&format!("\"k{i}\":{i}"));
        }
        wide.push_str("}}");
        corpus.push(wide.into_bytes());

        for (index, bytes) in corpus.iter().enumerate() {
            let result = std::panic::catch_unwind(|| validate_bytes(bytes, &live()));
            assert!(result.is_ok(), "panic on corpus[{index}]");
            if let Ok(Err(error)) = result {
                assert!(
                    !error.to_string().contains(SECRET),
                    "corpus[{index}] leaked secret"
                );
            }
        }
    }

    #[test]
    fn envelope_unknown_fields_are_rejected() {
        let raw = br#"{"schema":1,"call_id":"call_7","tool":"repo.search","arguments":{"query":"x"},"capability_lease":"tok"}"#;
        assert_eq!(
            ToolCall::parse(raw).expect_err("extra envelope"),
            ValidateError::MalformedArguments
        );
    }

    #[test]
    fn issue_paths_are_located_and_bounded() {
        let missing = ToolCall::new(1, "call_7", "repo.search", json!({}));
        let issues = validate_issues(&missing, &live()).expect_err("missing");
        assert!(issues.iter().any(
            |issue| issue.path() == "/query" && issue.error() == ValidateError::SchemaMismatch
        ));

        let wrong_type = ToolCall::new(1, "call_7", "repo.search", json!({"query": 1}));
        let issues = validate_issues(&wrong_type, &live()).expect_err("type");
        assert!(issues.iter().any(
            |issue| issue.path() == "/query" && issue.error() == ValidateError::SchemaMismatch
        ));

        let nested = ToolCall::new(
            1,
            "call_7",
            "workspace.patch",
            json!({"ops":[{"op":"create_file","path":"a.rs","content":""},{"op":"replace_range","path":"a.rs"}]}),
        );
        let issues = validate_issues(&nested, &live()).expect_err("nested");
        assert!(issues.iter().any(|issue| issue.path().starts_with("/ops/")));

        let denied = ToolCall::new(
            1,
            "call_7",
            "repo.search",
            json!({"query":"x","capability_lease":SECRET}),
        );
        let issues = validate_issues(&denied, &live()).expect_err("denied");
        assert!(
            issues
                .iter()
                .any(|issue| issue.error() == ValidateError::DeniedArgument)
        );

        let ok_call = ToolCall::new(1, "call_7", "repo.search", json!({"query":"lease"}));
        let invocation = validate_issues(&ok_call, &live()).expect("valid");
        assert_eq!(invocation.tool(), GatewayTool::RepoSearch);
    }
}
