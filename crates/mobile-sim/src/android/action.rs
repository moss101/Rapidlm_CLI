//! Android observe/act adapter: UI hierarchy, screenshots, typed input.
//!
//! Stateful actions follow observe → resolve → authorize → act → verify.
//! Semantic targets win over coordinates. Raw `adb shell` is a separate
//! privileged capability and is never implied by tap/text/key (T-CU-01).

use std::collections::HashMap;
use std::error::Error;
use std::fmt::{self, Debug};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use capability_broker::{CancellationToken, SecretHandle};
use protocol::{ArtifactId, ArtifactRef, ErrorCode, RedactionClass, RuntimeId};

use super::manager::{AndroidDeviceHandle, AndroidDeviceId, AndroidManagerError, DeviceSerial};

/// Maximum UTF-8 bytes accepted for a UI node role/class token.
pub const MAX_ROLE_BYTES: usize = 64;

/// Maximum UTF-8 bytes accepted for accessible name / content-desc / text.
pub const MAX_NAME_BYTES: usize = 256;

/// Maximum UTF-8 bytes accepted for a resource-id.
pub const MAX_RESOURCE_ID_BYTES: usize = 256;

/// Maximum UTF-8 bytes accepted for package / activity.
pub const MAX_COMPONENT_BYTES: usize = 256;

/// Maximum UTF-8 bytes accepted for typed text.
pub const MAX_TEXT_BYTES: usize = 512;

/// Maximum UTF-8 bytes accepted for a deep-link URI.
pub const MAX_DEEPLINK_BYTES: usize = 2048;

/// Maximum UI nodes retained from one hierarchy dump.
pub const MAX_NODES: usize = 512;

/// Maximum raw uiautomator dump accepted from the backend.
pub const MAX_HIERARCHY_BYTES: usize = 256 * 1024;

/// Maximum screenshot payload accepted from the backend.
pub const MAX_SCREENSHOT_BYTES: u64 = 2 * 1024 * 1024;

/// Maximum screenshot width in device pixels (portrait or landscape).
pub const MAX_SCREENSHOT_WIDTH: u32 = 2560;

/// Maximum screenshot height in device pixels (portrait or landscape).
pub const MAX_SCREENSHOT_HEIGHT: u32 = 2560;

/// Maximum observe timeout.
pub const MAX_OBSERVE_TIMEOUT: Duration = Duration::from_secs(30);

/// Default observe timeout.
pub const DEFAULT_OBSERVE_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum act timeout.
pub const MAX_ACTION_TIMEOUT: Duration = Duration::from_secs(30);

/// Default act timeout.
pub const DEFAULT_ACTION_TIMEOUT: Duration = Duration::from_secs(10);

/// Artifact media type for a bounded device screenshot.
pub const SCREENSHOT_MEDIA_TYPE: &str = "image/png";

const REDACTED_SCREENSHOT: &[u8] = b"rapidlm.android.screenshot.redacted.v1";
const HOST_POLL: Duration = Duration::from_millis(10);
const MAX_HOST_OUTPUT_BYTES: usize = MAX_HIERARCHY_BYTES;

/// Identity of one device capture. Distinct from [`AndroidDeviceId`].
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct ObservationId(RuntimeId);

/// Device UI generation captured with an observation or action receipt.
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct DeviceStateId(RuntimeId);

/// How a stable target ref was derived.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum SemanticSource {
    ResourceId,
    Accessibility,
    RoleName,
    Visual,
    Coordinate,
}

/// Compact accessibility-tree metadata. The model does not receive the raw tree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AccessibilitySnapshotRef {
    node_count: u32,
    interactive_count: u32,
}

/// Device display geometry bound into coordinate targets.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct Geometry {
    width: u32,
    height: u32,
}

/// Inclusive pixel rectangle from a uiautomator `bounds` attribute.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

/// Pixel point in the originating observation geometry.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct Point {
    x: i32,
    y: i32,
}

/// Emulator display orientation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Orientation {
    Portrait,
    Landscape,
    ReversePortrait,
    ReverseLandscape,
}

/// Bounded hardware/system key. Not an arbitrary keyevent integer.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum AndroidKey {
    Back,
    Home,
    Enter,
    Tab,
    Escape,
    Delete,
    AppSwitch,
    VolumeUp,
    VolumeDown,
}

/// AX/UI-derived locator usable by a later act step.
#[derive(Clone, Eq, PartialEq)]
pub struct SemanticTarget {
    index: u32,
    stable_ref: String,
    source: SemanticSource,
    role: Option<String>,
    name: Option<String>,
    resource_id: Option<String>,
    interactive: bool,
    sensitive: bool,
    unique: bool,
    bounds: Rect,
}

/// Model-visible target: locators only, never field values.
#[derive(Clone, Eq, PartialEq)]
pub struct SemanticTargetView {
    index: u32,
    stable_ref: String,
    source: SemanticSource,
    role: Option<String>,
    name: Option<String>,
    resource_id: Option<String>,
    interactive: bool,
    sensitive: bool,
    unique: bool,
}

/// Screenshot handle. No pixel bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScreenshotMeta {
    artifact: ArtifactRef,
    width: u32,
    height: u32,
    masked: bool,
}

/// Assigned observation for one live emulator handle.
#[derive(Clone, Eq, PartialEq)]
pub struct AndroidObservation {
    id: ObservationId,
    device_id: AndroidDeviceId,
    generation: u64,
    device_state_id: DeviceStateId,
    state_hash: ArtifactId,
    package: String,
    activity: String,
    orientation: Orientation,
    geometry: Geometry,
    targets: Vec<SemanticTarget>,
    accessibility: AccessibilitySnapshotRef,
    screenshot: Option<ScreenshotMeta>,
    captured_at_ms: u64,
}

/// Bounded metadata the model may consume. No screenshot bytes, no secrets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AndroidObservationView {
    id: ObservationId,
    device_id: AndroidDeviceId,
    generation: u64,
    device_state_id: DeviceStateId,
    package: String,
    activity: String,
    orientation: Orientation,
    targets: Vec<SemanticTargetView>,
    screenshot: Option<ScreenshotMeta>,
    accessibility: AccessibilitySnapshotRef,
}

/// Observe options. Screenshot is off by default.
#[derive(Clone, Debug)]
pub struct ObserveRequest {
    screenshot: bool,
    timeout: Duration,
    cancel: CancellationToken,
}

/// Target resolution order: resource-id / AX → role+name → visual → coordinate.
#[derive(Clone, Eq, PartialEq)]
pub enum TargetRef {
    ResourceId(String),
    Accessibility {
        node: String,
    },
    RoleName {
        role: String,
        name: String,
    },
    Visual {
        observation: ObservationId,
        region: Rect,
        label: String,
    },
    Coordinate {
        observation: ObservationId,
        point: Point,
    },
}

/// Type payload. Secret handles never carry plaintext.
#[derive(Clone, Eq, PartialEq)]
pub enum SecretAwareText {
    Literal(String),
    SecretHandle(SecretHandle),
}

/// Typed mobile action. There is no shell/raw-adb variant.
#[derive(Clone, Eq, PartialEq)]
pub enum AndroidAction {
    Tap { count: u8 },
    TypeText { value: SecretAwareText },
    Key { key: AndroidKey },
    Rotate { orientation: Orientation },
    DeepLink { uri: DeepLinkUri },
}

/// Validated deep-link URI. Shell metacharacters cannot be constructed.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct DeepLinkUri(String);

/// Act request. Observation is required except for key / deep link.
#[derive(Clone)]
pub struct AndroidActionRequest {
    observation_id: Option<ObservationId>,
    action: AndroidAction,
    target: Option<TargetRef>,
    expected: Vec<UiAssertion>,
    timeout: Duration,
    cancel: CancellationToken,
}

/// Postcondition checked against the after-observation.
#[derive(Clone, Eq, PartialEq)]
pub enum UiAssertion {
    TextVisible(String),
    ResourceVisible(String),
    Orientation(Orientation),
    Package(String),
}

/// Assertion outcome. Failure does not rewrite the action status to success.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssertionResult {
    passed: bool,
    kind: &'static str,
}

/// Side-effect outcome. Model prose cannot override this.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ActionStatus {
    Succeeded,
    Failed,
    Denied,
}

/// Receipt with before/after device state IDs. No secret plaintext.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AndroidActionReceipt {
    before: DeviceStateId,
    after: DeviceStateId,
    before_observation: Option<ObservationId>,
    after_observation: ObservationId,
    action: ActionKind,
    status: ActionStatus,
    target_strategy: Option<SemanticSource>,
    secret_handle_used: bool,
    assertions: Vec<AssertionResult>,
}

/// Model-visible action class. Payloads are omitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ActionKind {
    Tap,
    TypeText,
    Key,
    Rotate,
    DeepLink,
}

/// One node from a uiautomator / accessibility dump.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UiNode {
    resource_id: Option<String>,
    content_desc: Option<String>,
    text: Option<String>,
    class: String,
    package: Option<String>,
    role: String,
    bounds: Rect,
    clickable: bool,
    focused: bool,
    password: bool,
    sensitive: bool,
    interactive: bool,
}

/// Backend capture used by [`AndroidUiBackend`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceDump {
    package: String,
    activity: String,
    orientation: Orientation,
    geometry: Geometry,
    generation: u64,
    nodes: Vec<UiNode>,
    screenshot: Option<RawScreenshot>,
}

/// Backend screenshot prior to bound checks and redaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawScreenshot {
    bytes: Vec<u8>,
    width: u32,
    height: u32,
}

/// Typed adb argv kind. Callers cannot inject a shell script.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TypedAdbCommand {
    DumpHierarchy,
    Screenshot,
    Tap { x: i32, y: i32 },
    TypeText { encoded: String },
    Key { code: u32 },
    Rotate { value: u8 },
    DeepLink { uri: String },
}

/// Typed observe/act failure. Display never echoes node text or URIs.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum AndroidActionError {
    Cancelled,
    Timeout,
    TimeoutInvalid,
    CapabilityUnavailable,
    DeviceNotFound,
    DeviceClosed,
    ObservationRequired,
    StaleObservation,
    TargetRequired,
    TargetNotFound,
    AmbiguousTarget,
    TargetBound,
    CoordinateOutOfBounds,
    InvalidText,
    InvalidDeepLink,
    InvalidTarget,
    ScreenshotBound,
    HierarchyBound,
    SensitiveDenied,
    SecretUnresolved,
    ShellCapabilityRequired,
    AssertionFailed,
    Unavailable,
    Backend,
}

/// Capture UI hierarchy / screenshot and apply typed input. No raw shell.
pub trait AndroidUiBackend: Send + Sync {
    fn dump(
        &self,
        serial: &DeviceSerial,
        include_screenshot: bool,
        cancel: &CancellationToken,
    ) -> Result<DeviceDump, AndroidActionError>;

    fn tap(
        &self,
        serial: &DeviceSerial,
        point: Point,
        cancel: &CancellationToken,
    ) -> Result<(), AndroidActionError>;

    fn type_text(
        &self,
        serial: &DeviceSerial,
        text: &str,
        secret: bool,
        cancel: &CancellationToken,
    ) -> Result<(), AndroidActionError>;

    fn key(
        &self,
        serial: &DeviceSerial,
        key: AndroidKey,
        cancel: &CancellationToken,
    ) -> Result<(), AndroidActionError>;

    fn rotate(
        &self,
        serial: &DeviceSerial,
        orientation: Orientation,
        cancel: &CancellationToken,
    ) -> Result<(), AndroidActionError>;

    fn deeplink(
        &self,
        serial: &DeviceSerial,
        uri: &DeepLinkUri,
        cancel: &CancellationToken,
    ) -> Result<(), AndroidActionError>;
}

/// Resolves [`SecretHandle`] only at the type-text executor boundary.
pub trait SecretResolver: Send + Sync {
    fn resolve(&self, handle: &SecretHandle) -> Result<String, AndroidActionError>;
}

/// Fail-closed resolver. Production paths use Auth, not this adapter.
pub struct DenySecretResolver;

/// Test resolver. Values are never shown in Debug.
pub struct MapSecretResolver {
    values: Mutex<HashMap<String, String>>,
}

/// Assigns observation IDs, resolves targets, and records before/after state.
pub struct AndroidActor {
    backend: Arc<dyn AndroidUiBackend>,
    secrets: Arc<dyn SecretResolver>,
    ledger: Mutex<Ledger>,
}

struct Ledger {
    observations: HashMap<ObservationId, StoredObservation>,
    devices: HashMap<AndroidDeviceId, DeviceCursor>,
}

struct StoredObservation {
    device_id: AndroidDeviceId,
    generation: u64,
    fingerprint: ArtifactId,
    geometry: Geometry,
    orientation: Orientation,
    package: String,
    activity: String,
    targets: Vec<SemanticTarget>,
    device_state_id: DeviceStateId,
    superseded: bool,
}

struct DeviceCursor {
    generation: u64,
    current: Option<ObservationId>,
    current_state: Option<DeviceStateId>,
}

/// In-process UI stand-in. No host adb or physical device is used.
pub struct FakeAndroidUi {
    state: Mutex<FakeState>,
}

struct FakeState {
    devices: HashMap<String, FakeDeviceUi>,
}

struct FakeDeviceUi {
    package: String,
    activity: String,
    orientation: Orientation,
    geometry: Geometry,
    generation: u64,
    nodes: Vec<UiNode>,
    screenshot: Option<RawScreenshot>,
    tap_handlers: HashMap<String, Vec<UiNode>>,
    last_ops: Vec<RecordedOp>,
    last_typed: Option<TypedRecord>,
    last_deeplink: Option<String>,
    last_key: Option<AndroidKey>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RecordedOp {
    Dump,
    Screenshot,
    Tap { x: i32, y: i32 },
    TypeText { secret: bool },
    Key,
    Rotate,
    DeepLink,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum TypedRecord {
    Literal(String),
    Secret,
}

/// Host SDK adb backend. Commands are typed argv; there is no shell string.
#[derive(Debug)]
pub struct HostAdbUi {
    adb: PathBuf,
    cwd: PathBuf,
}

impl ObservationId {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self(RuntimeId::new())
    }

    pub const fn from_runtime_id(id: RuntimeId) -> Self {
        Self(id)
    }

    pub const fn as_runtime_id(self) -> RuntimeId {
        self.0
    }
}

impl DeviceStateId {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self(RuntimeId::new())
    }

    pub const fn from_runtime_id(id: RuntimeId) -> Self {
        Self(id)
    }

    pub const fn as_runtime_id(self) -> RuntimeId {
        self.0
    }
}

impl Geometry {
    pub const fn new(width: u32, height: u32) -> Result<Self, AndroidActionError> {
        if width == 0
            || height == 0
            || width > MAX_SCREENSHOT_WIDTH
            || height > MAX_SCREENSHOT_HEIGHT
        {
            return Err(AndroidActionError::ScreenshotBound);
        }
        Ok(Self { width, height })
    }

    pub const fn width(self) -> u32 {
        self.width
    }

    pub const fn height(self) -> u32 {
        self.height
    }

    pub const fn contains(self, point: Point) -> bool {
        point.x >= 0
            && point.y >= 0
            && (point.x as u32) < self.width
            && (point.y as u32) < self.height
    }
}

impl Rect {
    pub const fn new(
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    ) -> Result<Self, AndroidActionError> {
        if right < left || bottom < top {
            return Err(AndroidActionError::InvalidTarget);
        }
        Ok(Self {
            left,
            top,
            right,
            bottom,
        })
    }

    pub const fn left(self) -> i32 {
        self.left
    }

    pub const fn top(self) -> i32 {
        self.top
    }

    pub const fn right(self) -> i32 {
        self.right
    }

    pub const fn bottom(self) -> i32 {
        self.bottom
    }

    pub const fn contains(self, point: Point) -> bool {
        point.x >= self.left && point.x < self.right && point.y >= self.top && point.y < self.bottom
    }

    pub const fn intersects(self, other: Self) -> bool {
        self.left < other.right
            && other.left < self.right
            && self.top < other.bottom
            && other.top < self.bottom
    }

    pub const fn center(self) -> Point {
        Point {
            x: self.left + (self.right - self.left) / 2,
            y: self.top + (self.bottom - self.top) / 2,
        }
    }
}

impl Point {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }

    pub const fn x(self) -> i32 {
        self.x
    }

    pub const fn y(self) -> i32 {
        self.y
    }
}

impl Orientation {
    pub const fn as_rotation(self) -> u8 {
        match self {
            Self::Portrait => 0,
            Self::Landscape => 1,
            Self::ReversePortrait => 2,
            Self::ReverseLandscape => 3,
        }
    }

    pub const fn from_rotation(value: u8) -> Self {
        match value % 4 {
            1 => Self::Landscape,
            2 => Self::ReversePortrait,
            3 => Self::ReverseLandscape,
            _ => Self::Portrait,
        }
    }
}

impl AndroidKey {
    pub const fn keycode(self) -> u32 {
        match self {
            Self::Back => 4,
            Self::Home => 3,
            Self::Enter => 66,
            Self::Tab => 61,
            Self::Escape => 111,
            Self::Delete => 67,
            Self::AppSwitch => 187,
            Self::VolumeUp => 24,
            Self::VolumeDown => 25,
        }
    }
}

impl ObserveRequest {
    pub fn new() -> Self {
        Self {
            screenshot: false,
            timeout: DEFAULT_OBSERVE_TIMEOUT,
            cancel: CancellationToken::new(),
        }
    }

    pub fn with_screenshot(mut self, enabled: bool) -> Self {
        self.screenshot = enabled;
        self
    }

    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Result<Self, AndroidActionError> {
        if timeout.is_zero() || timeout > MAX_OBSERVE_TIMEOUT {
            return Err(AndroidActionError::TimeoutInvalid);
        }
        self.timeout = timeout;
        Ok(self)
    }

    pub fn screenshot_enabled(&self) -> bool {
        self.screenshot
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn cancel(&self) -> &CancellationToken {
        &self.cancel
    }
}

impl Default for ObserveRequest {
    fn default() -> Self {
        Self::new()
    }
}

impl DeepLinkUri {
    /// Accept a bounded `scheme:rest` URI. Shell tokens and option injection fail.
    pub fn parse(raw: &str) -> Result<Self, AndroidActionError> {
        if raw.is_empty() || raw.len() > MAX_DEEPLINK_BYTES {
            return Err(AndroidActionError::InvalidDeepLink);
        }
        if raw.starts_with('-') || raw.contains('\0') || raw.chars().any(char::is_control) {
            return Err(AndroidActionError::InvalidDeepLink);
        }
        let Some((scheme, rest)) = raw.split_once(':') else {
            return Err(AndroidActionError::InvalidDeepLink);
        };
        if scheme.is_empty()
            || !scheme
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'+' || b == b'-')
        {
            return Err(AndroidActionError::InvalidDeepLink);
        }
        if rest.bytes().any(|b| !is_deeplink_byte(b)) {
            return Err(AndroidActionError::InvalidDeepLink);
        }
        Ok(Self(raw.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl SecretAwareText {
    pub fn literal(value: &str) -> Result<Self, AndroidActionError> {
        if value.is_empty() || value.len() > MAX_TEXT_BYTES {
            return Err(AndroidActionError::InvalidText);
        }
        if value.contains('\0') || value.chars().any(char::is_control) {
            return Err(AndroidActionError::InvalidText);
        }
        Ok(Self::Literal(value.to_owned()))
    }

    pub fn secret(handle: SecretHandle) -> Self {
        Self::SecretHandle(handle)
    }
}

impl AndroidAction {
    pub const fn kind(&self) -> ActionKind {
        match self {
            Self::Tap { .. } => ActionKind::Tap,
            Self::TypeText { .. } => ActionKind::TypeText,
            Self::Key { .. } => ActionKind::Key,
            Self::Rotate { .. } => ActionKind::Rotate,
            Self::DeepLink { .. } => ActionKind::DeepLink,
        }
    }

    pub const fn requires_observation(&self) -> bool {
        !matches!(self, Self::Key { .. } | Self::DeepLink { .. })
    }

    pub const fn requires_target(&self) -> bool {
        matches!(self, Self::Tap { .. } | Self::TypeText { .. })
    }
}

impl AndroidActionRequest {
    pub fn new(action: AndroidAction) -> Result<Self, AndroidActionError> {
        Ok(Self {
            observation_id: None,
            action,
            target: None,
            expected: Vec::new(),
            timeout: DEFAULT_ACTION_TIMEOUT,
            cancel: CancellationToken::new(),
        })
    }

    pub fn tap(observation: ObservationId, target: TargetRef) -> Result<Self, AndroidActionError> {
        Ok(Self {
            observation_id: Some(observation),
            action: AndroidAction::Tap { count: 1 },
            target: Some(target),
            expected: Vec::new(),
            timeout: DEFAULT_ACTION_TIMEOUT,
            cancel: CancellationToken::new(),
        })
    }

    pub fn type_text(
        observation: ObservationId,
        target: TargetRef,
        value: SecretAwareText,
    ) -> Result<Self, AndroidActionError> {
        Ok(Self {
            observation_id: Some(observation),
            action: AndroidAction::TypeText { value },
            target: Some(target),
            expected: Vec::new(),
            timeout: DEFAULT_ACTION_TIMEOUT,
            cancel: CancellationToken::new(),
        })
    }

    pub fn key(key: AndroidKey) -> Result<Self, AndroidActionError> {
        Self::new(AndroidAction::Key { key })
    }

    pub fn rotate(
        observation: ObservationId,
        orientation: Orientation,
    ) -> Result<Self, AndroidActionError> {
        Ok(Self {
            observation_id: Some(observation),
            action: AndroidAction::Rotate { orientation },
            target: None,
            expected: Vec::new(),
            timeout: DEFAULT_ACTION_TIMEOUT,
            cancel: CancellationToken::new(),
        })
    }

    pub fn deeplink(uri: DeepLinkUri) -> Result<Self, AndroidActionError> {
        Self::new(AndroidAction::DeepLink { uri })
    }

    pub fn with_observation(mut self, id: ObservationId) -> Self {
        self.observation_id = Some(id);
        self
    }

    pub fn with_target(mut self, target: TargetRef) -> Self {
        self.target = Some(target);
        self
    }

    pub fn with_expected(mut self, expected: Vec<UiAssertion>) -> Self {
        self.expected = expected;
        self
    }

    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Result<Self, AndroidActionError> {
        if timeout.is_zero() || timeout > MAX_ACTION_TIMEOUT {
            return Err(AndroidActionError::TimeoutInvalid);
        }
        self.timeout = timeout;
        Ok(self)
    }

    pub fn observation_id(&self) -> Option<ObservationId> {
        self.observation_id
    }

    pub fn action(&self) -> &AndroidAction {
        &self.action
    }

    pub fn target(&self) -> Option<&TargetRef> {
        self.target.as_ref()
    }

    pub fn cancel(&self) -> &CancellationToken {
        &self.cancel
    }
}

impl TargetRef {
    pub fn resource_id(id: &str) -> Result<Self, AndroidActionError> {
        Ok(Self::ResourceId(bound_text(
            id,
            MAX_RESOURCE_ID_BYTES,
            AndroidActionError::InvalidTarget,
        )?))
    }

    pub fn accessibility(node: &str) -> Result<Self, AndroidActionError> {
        Ok(Self::Accessibility {
            node: bound_text(
                node,
                MAX_RESOURCE_ID_BYTES,
                AndroidActionError::InvalidTarget,
            )?,
        })
    }

    pub fn role_name(role: &str, name: &str) -> Result<Self, AndroidActionError> {
        Ok(Self::RoleName {
            role: bound_text(role, MAX_ROLE_BYTES, AndroidActionError::InvalidTarget)?,
            name: bound_text(name, MAX_NAME_BYTES, AndroidActionError::InvalidTarget)?,
        })
    }
}

impl UiNode {
    pub fn button(resource_id: &str, name: &str, bounds: Rect) -> Result<Self, AndroidActionError> {
        Self::build(
            Some(resource_id),
            None,
            Some(name),
            "android.widget.Button",
            None,
            "button",
            bounds,
            true,
            false,
            false,
            false,
        )
    }

    pub fn edit_text(
        resource_id: &str,
        name: &str,
        bounds: Rect,
        password: bool,
    ) -> Result<Self, AndroidActionError> {
        Self::build(
            Some(resource_id),
            None,
            Some(name),
            "android.widget.EditText",
            None,
            "edittext",
            bounds,
            true,
            true,
            password,
            password,
        )
    }

    pub fn text(name: &str, bounds: Rect) -> Result<Self, AndroidActionError> {
        Self::build(
            None,
            None,
            Some(name),
            "android.widget.TextView",
            None,
            "text",
            bounds,
            false,
            false,
            false,
            false,
        )
    }

    pub fn permission_allow(bounds: Rect) -> Result<Self, AndroidActionError> {
        Self::build(
            Some("com.android.permissioncontroller:id/permission_allow_button"),
            None,
            Some("Allow"),
            "android.widget.Button",
            Some("com.android.permissioncontroller"),
            "button",
            bounds,
            true,
            false,
            false,
            true,
        )
    }

    pub fn with_content_desc(mut self, desc: &str) -> Result<Self, AndroidActionError> {
        self.content_desc = Some(bound_text(
            desc,
            MAX_NAME_BYTES,
            AndroidActionError::TargetBound,
        )?);
        Ok(self)
    }

    pub fn with_package(mut self, package: &str) -> Result<Self, AndroidActionError> {
        self.package = Some(bound_text(
            package,
            MAX_COMPONENT_BYTES,
            AndroidActionError::TargetBound,
        )?);
        Ok(self)
    }

    pub fn resource_id(&self) -> Option<&str> {
        self.resource_id.as_deref()
    }

    pub fn visible_text(&self) -> Option<&str> {
        self.text.as_deref()
    }

    pub fn role(&self) -> &str {
        &self.role
    }

    pub fn is_sensitive(&self) -> bool {
        self.sensitive
    }

    pub fn is_password(&self) -> bool {
        self.password
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        resource_id: Option<&str>,
        content_desc: Option<&str>,
        text: Option<&str>,
        class: &str,
        package: Option<&str>,
        role: &str,
        bounds: Rect,
        clickable: bool,
        focused: bool,
        password: bool,
        sensitive: bool,
    ) -> Result<Self, AndroidActionError> {
        Ok(Self {
            resource_id: optional_text(resource_id, MAX_RESOURCE_ID_BYTES)?,
            content_desc: optional_text(content_desc, MAX_NAME_BYTES)?,
            text: optional_text(text, MAX_NAME_BYTES)?,
            class: bound_text(class, MAX_COMPONENT_BYTES, AndroidActionError::TargetBound)?,
            package: optional_text(package, MAX_COMPONENT_BYTES)?,
            role: bound_text(role, MAX_ROLE_BYTES, AndroidActionError::TargetBound)?,
            bounds,
            clickable,
            focused,
            password,
            sensitive,
            interactive: clickable || password || role == "edittext",
        })
    }
}

impl RawScreenshot {
    pub fn new(bytes: Vec<u8>, width: u32, height: u32) -> Result<Self, AndroidActionError> {
        check_screenshot_bounds(bytes.len() as u64, width, height)?;
        Ok(Self {
            bytes,
            width,
            height,
        })
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }
}

impl DeviceDump {
    pub fn new(
        package: &str,
        activity: &str,
        orientation: Orientation,
        geometry: Geometry,
        generation: u64,
        nodes: Vec<UiNode>,
    ) -> Result<Self, AndroidActionError> {
        if nodes.len() > MAX_NODES {
            return Err(AndroidActionError::HierarchyBound);
        }
        Ok(Self {
            package: bound_text(
                package,
                MAX_COMPONENT_BYTES,
                AndroidActionError::TargetBound,
            )?,
            activity: bound_text(
                activity,
                MAX_COMPONENT_BYTES,
                AndroidActionError::TargetBound,
            )?,
            orientation,
            geometry,
            generation,
            nodes,
            screenshot: None,
        })
    }

    pub fn with_screenshot(mut self, screenshot: RawScreenshot) -> Self {
        self.screenshot = Some(screenshot);
        self
    }

    pub fn nodes(&self) -> &[UiNode] {
        &self.nodes
    }
}

impl DenySecretResolver {
    pub const fn new() -> Self {
        Self
    }
}

impl Default for DenySecretResolver {
    fn default() -> Self {
        Self
    }
}

impl SecretResolver for DenySecretResolver {
    fn resolve(&self, _handle: &SecretHandle) -> Result<String, AndroidActionError> {
        Err(AndroidActionError::SecretUnresolved)
    }
}

impl MapSecretResolver {
    pub fn new() -> Self {
        Self {
            values: Mutex::new(HashMap::new()),
        }
    }

    pub fn insert(&self, handle: &SecretHandle, value: &str) -> Result<(), AndroidActionError> {
        if value.len() > MAX_TEXT_BYTES {
            return Err(AndroidActionError::InvalidText);
        }
        let mut values = self
            .values
            .lock()
            .map_err(|_| AndroidActionError::Unavailable)?;
        values.insert(handle.as_str().to_owned(), value.to_owned());
        Ok(())
    }
}

impl Default for MapSecretResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretResolver for MapSecretResolver {
    fn resolve(&self, handle: &SecretHandle) -> Result<String, AndroidActionError> {
        let values = self
            .values
            .lock()
            .map_err(|_| AndroidActionError::Unavailable)?;
        values
            .get(handle.as_str())
            .cloned()
            .ok_or(AndroidActionError::SecretUnresolved)
    }
}

impl Debug for MapSecretResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MapSecretResolver").finish_non_exhaustive()
    }
}

impl AndroidActor {
    pub fn new(backend: Arc<dyn AndroidUiBackend>) -> Self {
        Self::with_secrets(backend, Arc::new(DenySecretResolver))
    }

    pub fn with_secrets(
        backend: Arc<dyn AndroidUiBackend>,
        secrets: Arc<dyn SecretResolver>,
    ) -> Self {
        Self {
            backend,
            secrets,
            ledger: Mutex::new(Ledger {
                observations: HashMap::new(),
                devices: HashMap::new(),
            }),
        }
    }

    pub fn observe(
        &self,
        handle: &AndroidDeviceHandle,
        request: ObserveRequest,
    ) -> Result<AndroidObservation, AndroidActionError> {
        check_cancel(request.cancel())?;
        if request.timeout().is_zero() || request.timeout() > MAX_OBSERVE_TIMEOUT {
            return Err(AndroidActionError::TimeoutInvalid);
        }
        require_assigned_handle(handle)?;
        let dump = self.backend.dump(
            handle.serial(),
            request.screenshot_enabled(),
            request.cancel(),
        )?;
        self.commit_observation(handle, dump, request.screenshot_enabled())
    }

    pub fn act(
        &self,
        handle: &AndroidDeviceHandle,
        request: AndroidActionRequest,
    ) -> Result<AndroidActionReceipt, AndroidActionError> {
        check_cancel(request.cancel())?;
        if request.timeout.is_zero() || request.timeout > MAX_ACTION_TIMEOUT {
            return Err(AndroidActionError::TimeoutInvalid);
        }
        require_assigned_handle(handle)?;

        if request.action.requires_observation() && request.observation_id.is_none() {
            return Err(AndroidActionError::ObservationRequired);
        }

        let stored = match request.observation_id {
            Some(id) => Some(self.require_current(handle, id, request.cancel())?),
            None => None,
        };

        let resolved = if request.action.requires_target() {
            let target = request
                .target
                .as_ref()
                .ok_or(AndroidActionError::TargetRequired)?;
            let stored = stored
                .as_ref()
                .ok_or(AndroidActionError::ObservationRequired)?;
            let observation_id = request
                .observation_id
                .ok_or(AndroidActionError::ObservationRequired)?;
            Some(resolve_target(target, stored, observation_id)?)
        } else {
            None
        };

        authorize(&request.action, resolved.as_ref())?;

        let before = match stored.as_ref() {
            Some(obs) => obs.device_state_id,
            None => {
                self.capture_state(handle, request.cancel())?
                    .device_state_id
            }
        };

        let secret_handle_used = execute_action(
            self.backend.as_ref(),
            self.secrets.as_ref(),
            handle.serial(),
            &request.action,
            resolved.as_ref(),
            request.cancel(),
        )?;

        let after = self.observe(
            handle,
            ObserveRequest::new().with_cancel(request.cancel.clone()),
        )?;
        let assertions = evaluate_assertions(&request.expected, &after);
        let status = if assertions.iter().all(|item| item.passed) {
            ActionStatus::Succeeded
        } else {
            ActionStatus::Failed
        };

        Ok(AndroidActionReceipt {
            before,
            after: after.device_state_id,
            before_observation: request.observation_id,
            after_observation: after.id,
            action: request.action.kind(),
            status,
            target_strategy: resolved.map(|item| item.source),
            secret_handle_used,
            assertions,
        })
    }

    /// Raw adb shell is not available through this adapter (T-CU-01).
    pub fn shell(
        &self,
        handle: &AndroidDeviceHandle,
        _command: &str,
    ) -> Result<(), AndroidActionError> {
        require_assigned_handle(handle)?;
        Err(AndroidActionError::ShellCapabilityRequired)
    }

    fn require_current(
        &self,
        handle: &AndroidDeviceHandle,
        id: ObservationId,
        cancel: &CancellationToken,
    ) -> Result<StoredObservation, AndroidActionError> {
        check_cancel(cancel)?;
        let stored = {
            let ledger = self
                .ledger
                .lock()
                .map_err(|_| AndroidActionError::Unavailable)?;
            let stored = ledger
                .observations
                .get(&id)
                .ok_or(AndroidActionError::StaleObservation)?;
            let current = ledger
                .devices
                .get(&handle.id())
                .and_then(|cursor| cursor.current);
            if stored.superseded || stored.device_id != handle.id() || current != Some(id) {
                return Err(AndroidActionError::StaleObservation);
            }
            stored.clone()
        };
        let dump = self.backend.dump(handle.serial(), false, cancel)?;
        // Currentness is the hierarchy/geometry fingerprint, not dump.generation.
        // Host dumps must not pin generation=1 (T-CU-02).
        if dump.geometry != stored.geometry || hierarchy_fingerprint(&dump) != stored.fingerprint {
            return Err(AndroidActionError::StaleObservation);
        }
        Ok(stored)
    }

    fn capture_state(
        &self,
        handle: &AndroidDeviceHandle,
        cancel: &CancellationToken,
    ) -> Result<AndroidObservation, AndroidActionError> {
        self.observe(handle, ObserveRequest::new().with_cancel(cancel.clone()))
    }

    fn commit_observation(
        &self,
        handle: &AndroidDeviceHandle,
        dump: DeviceDump,
        want_screenshot: bool,
    ) -> Result<AndroidObservation, AndroidActionError> {
        if dump.nodes.len() > MAX_NODES {
            return Err(AndroidActionError::HierarchyBound);
        }
        let targets = derive_targets(&dump.nodes)?;
        let accessibility = AccessibilitySnapshotRef {
            node_count: dump.nodes.len() as u32,
            interactive_count: dump.nodes.iter().filter(|node| node.interactive).count() as u32,
        };
        let screenshot = if want_screenshot {
            Some(persist_screenshot(&dump)?)
        } else {
            None
        };
        let state_hash = hash_state(
            &dump.package,
            &dump.activity,
            dump.generation,
            dump.orientation,
            &targets,
        );
        let id = ObservationId::new();
        let device_state_id = DeviceStateId::new();
        let generation = self.store(handle.id(), id, device_state_id, &dump, targets.clone())?;
        Ok(AndroidObservation {
            id,
            device_id: handle.id(),
            generation,
            device_state_id,
            state_hash,
            package: dump.package,
            activity: dump.activity,
            orientation: dump.orientation,
            geometry: dump.geometry,
            targets,
            accessibility,
            screenshot,
            captured_at_ms: now_ms(),
        })
    }

    fn store(
        &self,
        device_id: AndroidDeviceId,
        id: ObservationId,
        device_state_id: DeviceStateId,
        dump: &DeviceDump,
        targets: Vec<SemanticTarget>,
    ) -> Result<u64, AndroidActionError> {
        let mut ledger = self
            .ledger
            .lock()
            .map_err(|_| AndroidActionError::Unavailable)?;
        if let Some(previous) = ledger
            .devices
            .get(&device_id)
            .and_then(|cursor| cursor.current)
            && let Some(stored) = ledger.observations.get_mut(&previous)
        {
            stored.superseded = true;
        }
        let generation = {
            let cursor = ledger.devices.entry(device_id).or_insert(DeviceCursor {
                generation: 0,
                current: None,
                current_state: None,
            });
            let generation = cursor
                .generation
                .checked_add(1)
                .ok_or(AndroidActionError::Unavailable)?;
            cursor.generation = generation;
            cursor.current = Some(id);
            cursor.current_state = Some(device_state_id);
            generation
        };
        ledger.observations.insert(
            id,
            StoredObservation {
                device_id,
                generation,
                fingerprint: hierarchy_fingerprint(dump),
                geometry: dump.geometry,
                orientation: dump.orientation,
                package: dump.package.clone(),
                activity: dump.activity.clone(),
                targets,
                device_state_id,
                superseded: false,
            },
        );
        Ok(generation)
    }
}

impl AndroidObservation {
    pub fn id(&self) -> ObservationId {
        self.id
    }

    pub fn device_id(&self) -> AndroidDeviceId {
        self.device_id
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn device_state_id(&self) -> DeviceStateId {
        self.device_state_id
    }

    pub fn state_hash(&self) -> ArtifactId {
        self.state_hash
    }

    pub fn package(&self) -> &str {
        &self.package
    }

    pub fn activity(&self) -> &str {
        &self.activity
    }

    pub fn orientation(&self) -> Orientation {
        self.orientation
    }

    pub fn geometry(&self) -> Geometry {
        self.geometry
    }

    pub fn targets(&self) -> &[SemanticTarget] {
        &self.targets
    }

    pub fn accessibility(&self) -> AccessibilitySnapshotRef {
        self.accessibility
    }

    pub fn screenshot(&self) -> Option<&ScreenshotMeta> {
        self.screenshot.as_ref()
    }

    pub fn model_view(&self) -> AndroidObservationView {
        AndroidObservationView {
            id: self.id,
            device_id: self.device_id,
            generation: self.generation,
            device_state_id: self.device_state_id,
            package: self.package.clone(),
            activity: self.activity.clone(),
            orientation: self.orientation,
            targets: self
                .targets
                .iter()
                .map(|target| SemanticTargetView {
                    index: target.index,
                    stable_ref: target.stable_ref.clone(),
                    source: target.source,
                    role: target.role.clone(),
                    name: if target.sensitive {
                        None
                    } else {
                        target.name.clone()
                    },
                    resource_id: target.resource_id.clone(),
                    interactive: target.interactive,
                    sensitive: target.sensitive,
                    unique: target.unique,
                })
                .collect(),
            screenshot: self.screenshot.clone(),
            accessibility: self.accessibility,
        }
    }
}

impl SemanticTarget {
    pub fn index(&self) -> u32 {
        self.index
    }

    pub fn stable_ref(&self) -> &str {
        &self.stable_ref
    }

    pub fn source(&self) -> SemanticSource {
        self.source
    }

    pub fn resource_id(&self) -> Option<&str> {
        self.resource_id.as_deref()
    }

    pub fn is_sensitive(&self) -> bool {
        self.sensitive
    }

    pub fn is_unique(&self) -> bool {
        self.unique
    }

    pub fn bounds(&self) -> Rect {
        self.bounds
    }
}

impl ScreenshotMeta {
    pub fn artifact(&self) -> &ArtifactRef {
        &self.artifact
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn is_masked(&self) -> bool {
        self.masked
    }
}

impl AndroidActionReceipt {
    pub fn before(&self) -> DeviceStateId {
        self.before
    }

    pub fn after(&self) -> DeviceStateId {
        self.after
    }

    pub fn before_observation(&self) -> Option<ObservationId> {
        self.before_observation
    }

    pub fn after_observation(&self) -> ObservationId {
        self.after_observation
    }

    pub fn action(&self) -> ActionKind {
        self.action
    }

    pub fn status(&self) -> ActionStatus {
        self.status
    }

    pub fn target_strategy(&self) -> Option<SemanticSource> {
        self.target_strategy
    }

    pub fn secret_handle_used(&self) -> bool {
        self.secret_handle_used
    }

    pub fn assertions(&self) -> &[AssertionResult] {
        &self.assertions
    }
}

impl AssertionResult {
    pub fn passed(&self) -> bool {
        self.passed
    }

    pub fn kind(&self) -> &'static str {
        self.kind
    }
}

impl FakeAndroidUi {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(FakeState {
                devices: HashMap::new(),
            }),
        }
    }

    pub fn install(
        &self,
        handle: &AndroidDeviceHandle,
        dump: DeviceDump,
    ) -> Result<(), AndroidActionError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| AndroidActionError::Unavailable)?;
        state.devices.insert(
            handle.serial().as_str().to_owned(),
            FakeDeviceUi {
                package: dump.package,
                activity: dump.activity,
                orientation: dump.orientation,
                geometry: dump.geometry,
                generation: dump.generation.max(1),
                nodes: dump.nodes,
                screenshot: dump.screenshot,
                tap_handlers: HashMap::new(),
                last_ops: Vec::new(),
                last_typed: None,
                last_deeplink: None,
                last_key: None,
            },
        );
        Ok(())
    }

    /// Replace nodes/bounds without bumping generation or screen geometry.
    /// Used to prove fingerprint stale-rejection (T-CU-02).
    pub fn replace_nodes_same_geometry(
        &self,
        handle: &AndroidDeviceHandle,
        nodes: Vec<UiNode>,
    ) -> Result<(), AndroidActionError> {
        if nodes.len() > MAX_NODES {
            return Err(AndroidActionError::HierarchyBound);
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| AndroidActionError::Unavailable)?;
        let device = state
            .devices
            .get_mut(handle.serial().as_str())
            .ok_or(AndroidActionError::DeviceNotFound)?;
        device.nodes = nodes;
        Ok(())
    }

    pub fn set_nodes(
        &self,
        handle: &AndroidDeviceHandle,
        nodes: Vec<UiNode>,
    ) -> Result<(), AndroidActionError> {
        if nodes.len() > MAX_NODES {
            return Err(AndroidActionError::HierarchyBound);
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| AndroidActionError::Unavailable)?;
        let device = state
            .devices
            .get_mut(handle.serial().as_str())
            .ok_or(AndroidActionError::DeviceNotFound)?;
        device.nodes = nodes;
        device.generation = device
            .generation
            .checked_add(1)
            .ok_or(AndroidActionError::Unavailable)?;
        Ok(())
    }

    pub fn on_tap(
        &self,
        handle: &AndroidDeviceHandle,
        resource_id: &str,
        next: Vec<UiNode>,
    ) -> Result<(), AndroidActionError> {
        if next.len() > MAX_NODES {
            return Err(AndroidActionError::HierarchyBound);
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| AndroidActionError::Unavailable)?;
        let device = state
            .devices
            .get_mut(handle.serial().as_str())
            .ok_or(AndroidActionError::DeviceNotFound)?;
        device.tap_handlers.insert(resource_id.to_owned(), next);
        Ok(())
    }

    pub fn last_ops(&self, handle: &AndroidDeviceHandle) -> Vec<&'static str> {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.devices.get(handle.serial().as_str()).cloned())
            .map(|device| {
                device
                    .last_ops
                    .iter()
                    .map(|op| match op {
                        RecordedOp::Dump => "dump",
                        RecordedOp::Screenshot => "screenshot",
                        RecordedOp::Tap { .. } => "tap",
                        RecordedOp::TypeText { .. } => "type_text",
                        RecordedOp::Key => "key",
                        RecordedOp::Rotate => "rotate",
                        RecordedOp::DeepLink => "deeplink",
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn last_typed_literal(&self, handle: &AndroidDeviceHandle) -> Option<String> {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.devices.get(handle.serial().as_str()).cloned())
            .and_then(|device| match device.last_typed {
                Some(TypedRecord::Literal(value)) => Some(value),
                Some(TypedRecord::Secret) | None => None,
            })
    }

    pub fn last_typed_was_secret(&self, handle: &AndroidDeviceHandle) -> bool {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.devices.get(handle.serial().as_str()).cloned())
            .is_some_and(|device| matches!(device.last_typed, Some(TypedRecord::Secret)))
    }

    pub fn last_deeplink(&self, handle: &AndroidDeviceHandle) -> Option<String> {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.devices.get(handle.serial().as_str()).cloned())
            .and_then(|device| device.last_deeplink)
    }

    pub fn executed_shell(&self, handle: &AndroidDeviceHandle) -> bool {
        let _ = handle;
        false
    }
}

impl Default for FakeAndroidUi {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for FakeDeviceUi {
    fn clone(&self) -> Self {
        Self {
            package: self.package.clone(),
            activity: self.activity.clone(),
            orientation: self.orientation,
            geometry: self.geometry,
            generation: self.generation,
            nodes: self.nodes.clone(),
            screenshot: self.screenshot.clone(),
            tap_handlers: self.tap_handlers.clone(),
            last_ops: self.last_ops.clone(),
            last_typed: self.last_typed.clone(),
            last_deeplink: self.last_deeplink.clone(),
            last_key: self.last_key,
        }
    }
}

impl AndroidUiBackend for FakeAndroidUi {
    fn dump(
        &self,
        serial: &DeviceSerial,
        include_screenshot: bool,
        cancel: &CancellationToken,
    ) -> Result<DeviceDump, AndroidActionError> {
        check_cancel(cancel)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| AndroidActionError::Unavailable)?;
        let device = state
            .devices
            .get_mut(serial.as_str())
            .ok_or(AndroidActionError::DeviceNotFound)?;
        device.last_ops.push(RecordedOp::Dump);
        if include_screenshot {
            device.last_ops.push(RecordedOp::Screenshot);
        }
        Ok(DeviceDump {
            package: device.package.clone(),
            activity: device.activity.clone(),
            orientation: device.orientation,
            geometry: device.geometry,
            generation: device.generation,
            nodes: device.nodes.clone(),
            screenshot: if include_screenshot {
                device.screenshot.clone()
            } else {
                None
            },
        })
    }

    fn tap(
        &self,
        serial: &DeviceSerial,
        point: Point,
        cancel: &CancellationToken,
    ) -> Result<(), AndroidActionError> {
        check_cancel(cancel)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| AndroidActionError::Unavailable)?;
        let device = state
            .devices
            .get_mut(serial.as_str())
            .ok_or(AndroidActionError::DeviceNotFound)?;
        device.last_ops.push(RecordedOp::Tap {
            x: point.x,
            y: point.y,
        });
        let hit = device
            .nodes
            .iter()
            .rev()
            .find(|node| node.interactive && node.bounds.contains(point))
            .and_then(|node| node.resource_id.clone());
        if let Some(id) = hit
            && let Some(next) = device.tap_handlers.get(&id).cloned()
        {
            device.nodes = next;
        }
        bump_generation(device)?;
        Ok(())
    }

    fn type_text(
        &self,
        serial: &DeviceSerial,
        text: &str,
        secret: bool,
        cancel: &CancellationToken,
    ) -> Result<(), AndroidActionError> {
        check_cancel(cancel)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| AndroidActionError::Unavailable)?;
        let device = state
            .devices
            .get_mut(serial.as_str())
            .ok_or(AndroidActionError::DeviceNotFound)?;
        device.last_ops.push(RecordedOp::TypeText { secret });
        device.last_typed = Some(if secret {
            TypedRecord::Secret
        } else {
            TypedRecord::Literal(text.to_owned())
        });
        bump_generation(device)?;
        Ok(())
    }

    fn key(
        &self,
        serial: &DeviceSerial,
        key: AndroidKey,
        cancel: &CancellationToken,
    ) -> Result<(), AndroidActionError> {
        check_cancel(cancel)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| AndroidActionError::Unavailable)?;
        let device = state
            .devices
            .get_mut(serial.as_str())
            .ok_or(AndroidActionError::DeviceNotFound)?;
        device.last_ops.push(RecordedOp::Key);
        device.last_key = Some(key);
        bump_generation(device)?;
        Ok(())
    }

    fn rotate(
        &self,
        serial: &DeviceSerial,
        orientation: Orientation,
        cancel: &CancellationToken,
    ) -> Result<(), AndroidActionError> {
        check_cancel(cancel)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| AndroidActionError::Unavailable)?;
        let device = state
            .devices
            .get_mut(serial.as_str())
            .ok_or(AndroidActionError::DeviceNotFound)?;
        device.last_ops.push(RecordedOp::Rotate);
        if device.orientation != orientation {
            let (width, height) = (device.geometry.height, device.geometry.width);
            device.geometry = Geometry::new(width, height)?;
            device.orientation = orientation;
        }
        bump_generation(device)?;
        Ok(())
    }

    fn deeplink(
        &self,
        serial: &DeviceSerial,
        uri: &DeepLinkUri,
        cancel: &CancellationToken,
    ) -> Result<(), AndroidActionError> {
        check_cancel(cancel)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| AndroidActionError::Unavailable)?;
        let device = state
            .devices
            .get_mut(serial.as_str())
            .ok_or(AndroidActionError::DeviceNotFound)?;
        device.last_ops.push(RecordedOp::DeepLink);
        device.last_deeplink = Some(uri.as_str().to_owned());
        bump_generation(device)?;
        Ok(())
    }
}

impl AndroidUiBackend for Arc<FakeAndroidUi> {
    fn dump(
        &self,
        serial: &DeviceSerial,
        include_screenshot: bool,
        cancel: &CancellationToken,
    ) -> Result<DeviceDump, AndroidActionError> {
        AndroidUiBackend::dump(&**self, serial, include_screenshot, cancel)
    }

    fn tap(
        &self,
        serial: &DeviceSerial,
        point: Point,
        cancel: &CancellationToken,
    ) -> Result<(), AndroidActionError> {
        AndroidUiBackend::tap(&**self, serial, point, cancel)
    }

    fn type_text(
        &self,
        serial: &DeviceSerial,
        text: &str,
        secret: bool,
        cancel: &CancellationToken,
    ) -> Result<(), AndroidActionError> {
        AndroidUiBackend::type_text(&**self, serial, text, secret, cancel)
    }

    fn key(
        &self,
        serial: &DeviceSerial,
        key: AndroidKey,
        cancel: &CancellationToken,
    ) -> Result<(), AndroidActionError> {
        AndroidUiBackend::key(&**self, serial, key, cancel)
    }

    fn rotate(
        &self,
        serial: &DeviceSerial,
        orientation: Orientation,
        cancel: &CancellationToken,
    ) -> Result<(), AndroidActionError> {
        AndroidUiBackend::rotate(&**self, serial, orientation, cancel)
    }

    fn deeplink(
        &self,
        serial: &DeviceSerial,
        uri: &DeepLinkUri,
        cancel: &CancellationToken,
    ) -> Result<(), AndroidActionError> {
        AndroidUiBackend::deeplink(&**self, serial, uri, cancel)
    }
}

impl HostAdbUi {
    pub fn open(sdk_root: impl AsRef<Path>) -> Result<Self, AndroidActionError> {
        let sdk_root = sdk_root.as_ref();
        if !sdk_root.is_absolute() {
            return Err(AndroidActionError::CapabilityUnavailable);
        }
        let adb = sdk_root.join("platform-tools").join(adb_bin_name());
        if !adb.is_file() {
            return Err(AndroidActionError::CapabilityUnavailable);
        }
        Ok(Self {
            adb,
            cwd: sdk_root.to_path_buf(),
        })
    }
}

impl AndroidUiBackend for HostAdbUi {
    fn dump(
        &self,
        serial: &DeviceSerial,
        include_screenshot: bool,
        cancel: &CancellationToken,
    ) -> Result<DeviceDump, AndroidActionError> {
        let xml = run_adb(
            &self.adb,
            &self.cwd,
            &adb_argv(serial, &TypedAdbCommand::DumpHierarchy),
            Duration::from_secs(10),
            cancel,
        )?;
        if xml.len() > MAX_HIERARCHY_BYTES {
            return Err(AndroidActionError::HierarchyBound);
        }
        let body = String::from_utf8(xml).map_err(|_| AndroidActionError::Backend)?;
        let mut dump = parse_uiautomator_dump(&body)?;
        if include_screenshot {
            let bytes = run_adb(
                &self.adb,
                &self.cwd,
                &adb_argv(serial, &TypedAdbCommand::Screenshot),
                Duration::from_secs(10),
                cancel,
            )?;
            dump.screenshot = Some(RawScreenshot::new(
                bytes,
                dump.geometry.width,
                dump.geometry.height,
            )?);
        }
        Ok(dump)
    }

    fn tap(
        &self,
        serial: &DeviceSerial,
        point: Point,
        cancel: &CancellationToken,
    ) -> Result<(), AndroidActionError> {
        run_adb(
            &self.adb,
            &self.cwd,
            &adb_argv(
                serial,
                &TypedAdbCommand::Tap {
                    x: point.x,
                    y: point.y,
                },
            ),
            Duration::from_secs(10),
            cancel,
        )
        .map(|_| ())
    }

    fn type_text(
        &self,
        serial: &DeviceSerial,
        text: &str,
        _secret: bool,
        cancel: &CancellationToken,
    ) -> Result<(), AndroidActionError> {
        let encoded = encode_input_text(text)?;
        run_adb(
            &self.adb,
            &self.cwd,
            &adb_argv(serial, &TypedAdbCommand::TypeText { encoded }),
            Duration::from_secs(10),
            cancel,
        )
        .map(|_| ())
    }

    fn key(
        &self,
        serial: &DeviceSerial,
        key: AndroidKey,
        cancel: &CancellationToken,
    ) -> Result<(), AndroidActionError> {
        run_adb(
            &self.adb,
            &self.cwd,
            &adb_argv(
                serial,
                &TypedAdbCommand::Key {
                    code: key.keycode(),
                },
            ),
            Duration::from_secs(10),
            cancel,
        )
        .map(|_| ())
    }

    fn rotate(
        &self,
        serial: &DeviceSerial,
        orientation: Orientation,
        cancel: &CancellationToken,
    ) -> Result<(), AndroidActionError> {
        run_adb(
            &self.adb,
            &self.cwd,
            &adb_argv(
                serial,
                &TypedAdbCommand::Rotate {
                    value: orientation.as_rotation(),
                },
            ),
            Duration::from_secs(10),
            cancel,
        )
        .map(|_| ())
    }

    fn deeplink(
        &self,
        serial: &DeviceSerial,
        uri: &DeepLinkUri,
        cancel: &CancellationToken,
    ) -> Result<(), AndroidActionError> {
        run_adb(
            &self.adb,
            &self.cwd,
            &adb_argv(
                serial,
                &TypedAdbCommand::DeepLink {
                    uri: uri.as_str().to_owned(),
                },
            ),
            Duration::from_secs(10),
            cancel,
        )
        .map(|_| ())
    }
}

/// Typed argv for one adb operation. Never a concatenated shell line.
pub fn adb_argv(serial: &DeviceSerial, command: &TypedAdbCommand) -> Vec<String> {
    let mut argv = vec!["-s".to_owned(), serial.as_str().to_owned()];
    match command {
        TypedAdbCommand::DumpHierarchy => {
            argv.extend([
                "exec-out".into(),
                "uiautomator".into(),
                "dump".into(),
                "/dev/tty".into(),
            ]);
        }
        TypedAdbCommand::Screenshot => {
            argv.extend(["exec-out".into(), "screencap".into(), "-p".into()]);
        }
        TypedAdbCommand::Tap { x, y } => {
            argv.extend([
                "shell".into(),
                "input".into(),
                "tap".into(),
                x.to_string(),
                y.to_string(),
            ]);
        }
        TypedAdbCommand::TypeText { encoded } => {
            argv.extend([
                "shell".into(),
                "input".into(),
                "text".into(),
                encoded.clone(),
            ]);
        }
        TypedAdbCommand::Key { code } => {
            argv.extend([
                "shell".into(),
                "input".into(),
                "keyevent".into(),
                code.to_string(),
            ]);
        }
        TypedAdbCommand::Rotate { value } => {
            argv.extend([
                "shell".into(),
                "settings".into(),
                "put".into(),
                "system".into(),
                "user_rotation".into(),
                value.to_string(),
            ]);
        }
        TypedAdbCommand::DeepLink { uri } => {
            argv.extend([
                "shell".into(),
                "am".into(),
                "start".into(),
                "-a".into(),
                "android.intent.action.VIEW".into(),
                "-d".into(),
                uri.clone(),
            ]);
        }
    }
    argv
}

/// Encode `adb input text` so device sh cannot see metacharacters.
pub fn encode_input_text(value: &str) -> Result<String, AndroidActionError> {
    if value.is_empty() || value.len() > MAX_TEXT_BYTES {
        return Err(AndroidActionError::InvalidText);
    }
    let mut out = String::new();
    for ch in value.chars() {
        if ch == ' ' {
            out.push_str("%s");
        } else if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            out.push(ch);
        } else if ch.is_control() {
            return Err(AndroidActionError::InvalidText);
        } else {
            let mut buf = [0u8; 4];
            for byte in ch.encode_utf8(&mut buf).as_bytes() {
                out.push('%');
                out.push(HEX[(*byte >> 4) as usize] as char);
                out.push(HEX[(*byte & 0x0f) as usize] as char);
            }
        }
        if out.len() > MAX_TEXT_BYTES * 3 {
            return Err(AndroidActionError::InvalidText);
        }
    }
    Ok(out)
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";

impl AndroidActionError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::TimeoutInvalid => "timeout_invalid",
            Self::CapabilityUnavailable => "capability_unavailable",
            Self::DeviceNotFound => "device_not_found",
            Self::DeviceClosed => "device_closed",
            Self::ObservationRequired => "observation_required",
            Self::StaleObservation => "browser.stale_observation",
            Self::TargetRequired => "target_required",
            Self::TargetNotFound => "target_not_found",
            Self::AmbiguousTarget => "ambiguous_target",
            Self::TargetBound => "target_bound",
            Self::CoordinateOutOfBounds => "coordinate_out_of_bounds",
            Self::InvalidText => "invalid_text",
            Self::InvalidDeepLink => "invalid_deeplink",
            Self::InvalidTarget => "invalid_target",
            Self::ScreenshotBound => "screenshot_bound",
            Self::HierarchyBound => "hierarchy_bound",
            Self::SensitiveDenied => "sensitive_denied",
            Self::SecretUnresolved => "secret_unresolved",
            Self::ShellCapabilityRequired => "shell_capability_required",
            Self::AssertionFailed => "assertion_failed",
            Self::Unavailable => "unavailable",
            Self::Backend => "backend",
        }
    }

    pub const fn code(self) -> ErrorCode {
        match self {
            Self::Cancelled
            | Self::TimeoutInvalid
            | Self::ObservationRequired
            | Self::TargetRequired
            | Self::TargetNotFound
            | Self::AmbiguousTarget
            | Self::TargetBound
            | Self::CoordinateOutOfBounds
            | Self::InvalidText
            | Self::InvalidDeepLink
            | Self::InvalidTarget
            | Self::ScreenshotBound
            | Self::HierarchyBound
            | Self::SecretUnresolved => ErrorCode::ToolInvalidArguments,
            Self::Timeout => ErrorCode::ProcessTimeout,
            Self::CapabilityUnavailable => ErrorCode::MobileCapabilityUnavailable,
            Self::DeviceNotFound | Self::DeviceClosed => ErrorCode::SessionNotFound,
            Self::StaleObservation => ErrorCode::BrowserStaleObservation,
            Self::SensitiveDenied | Self::ShellCapabilityRequired => ErrorCode::PolicyDenied,
            Self::AssertionFailed => ErrorCode::GoalEvidenceMissing,
            Self::Unavailable | Self::Backend => ErrorCode::InternalUnexpected,
        }
    }
}

impl fmt::Display for AndroidActionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for AndroidActionError {}

impl fmt::Display for ObservationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl fmt::Display for DeviceStateId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl Debug for ObservationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ObservationId")
            .field(&self.0.to_string())
            .finish()
    }
}

impl Debug for DeviceStateId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("DeviceStateId")
            .field(&self.0.to_string())
            .finish()
    }
}

impl Debug for SemanticTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SemanticTarget")
            .field("index", &self.index)
            .field("stable_ref", &self.stable_ref)
            .field("source", &self.source)
            .field("sensitive", &self.sensitive)
            .finish_non_exhaustive()
    }
}

impl Debug for SemanticTargetView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SemanticTargetView")
            .field("index", &self.index)
            .field("stable_ref", &self.stable_ref)
            .field("source", &self.source)
            .field("sensitive", &self.sensitive)
            .finish_non_exhaustive()
    }
}

impl Debug for SecretAwareText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Literal(value) => f.debug_tuple("Literal").field(value).finish(),
            Self::SecretHandle(_) => f.debug_tuple("SecretHandle").field(&"<redacted>").finish(),
        }
    }
}

impl Debug for AndroidActionRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AndroidActionRequest")
            .field("observation_id", &self.observation_id)
            .field("action", &self.action)
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

impl Debug for AndroidAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tap { count } => f.debug_struct("Tap").field("count", count).finish(),
            Self::TypeText { value } => f.debug_struct("TypeText").field("value", value).finish(),
            Self::Key { key } => f.debug_struct("Key").field("key", key).finish(),
            Self::Rotate { orientation } => f
                .debug_struct("Rotate")
                .field("orientation", orientation)
                .finish(),
            Self::DeepLink { .. } => f.debug_struct("DeepLink").finish_non_exhaustive(),
        }
    }
}

impl Debug for DeepLinkUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("DeepLinkUri").field(&self.0).finish()
    }
}

impl Debug for TargetRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ResourceId(id) => f.debug_tuple("ResourceId").field(id).finish(),
            Self::Accessibility { node } => {
                f.debug_struct("Accessibility").field("node", node).finish()
            }
            Self::RoleName { role, name } => f
                .debug_struct("RoleName")
                .field("role", role)
                .field("name", name)
                .finish(),
            Self::Visual {
                observation,
                region,
                label,
            } => f
                .debug_struct("Visual")
                .field("observation", observation)
                .field("region", region)
                .field("label", label)
                .finish(),
            Self::Coordinate { observation, point } => f
                .debug_struct("Coordinate")
                .field("observation", observation)
                .field("point", point)
                .finish(),
        }
    }
}

impl Debug for AndroidObservation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AndroidObservation")
            .field("id", &self.id)
            .field("device_id", &self.device_id)
            .field("generation", &self.generation)
            .field("device_state_id", &self.device_state_id)
            .finish_non_exhaustive()
    }
}

impl Clone for StoredObservation {
    fn clone(&self) -> Self {
        Self {
            device_id: self.device_id,
            generation: self.generation,
            fingerprint: self.fingerprint,
            geometry: self.geometry,
            orientation: self.orientation,
            package: self.package.clone(),
            activity: self.activity.clone(),
            targets: self.targets.clone(),
            device_state_id: self.device_state_id,
            superseded: self.superseded,
        }
    }
}

struct ResolvedTarget {
    point: Point,
    source: SemanticSource,
    sensitive: bool,
    password: bool,
    role: Option<String>,
}

fn require_assigned_handle(handle: &AndroidDeviceHandle) -> Result<(), AndroidActionError> {
    match handle.require_assigned() {
        Ok(()) => Ok(()),
        Err(AndroidManagerError::DeviceClosed)
        | Err(AndroidManagerError::DeviceNotFound)
        | Err(AndroidManagerError::NotOwner) => Err(AndroidActionError::DeviceClosed),
        Err(AndroidManagerError::Cancelled) => Err(AndroidActionError::Cancelled),
        Err(AndroidManagerError::Unavailable) => Err(AndroidActionError::Unavailable),
        Err(_) => Err(AndroidActionError::DeviceClosed),
    }
}

fn execute_action(
    backend: &dyn AndroidUiBackend,
    secrets: &dyn SecretResolver,
    serial: &DeviceSerial,
    action: &AndroidAction,
    target: Option<&ResolvedTarget>,
    cancel: &CancellationToken,
) -> Result<bool, AndroidActionError> {
    match action {
        AndroidAction::Tap { count } => {
            let target = target.ok_or(AndroidActionError::TargetRequired)?;
            let n = (*count).max(1);
            for _ in 0..n {
                backend.tap(serial, target.point, cancel)?;
            }
            Ok(false)
        }
        AndroidAction::TypeText { value } => {
            let target = target.ok_or(AndroidActionError::TargetRequired)?;
            match value {
                SecretAwareText::Literal(text) => {
                    if target.password {
                        return Err(AndroidActionError::SensitiveDenied);
                    }
                    backend.type_text(serial, text, false, cancel)?;
                    Ok(false)
                }
                SecretAwareText::SecretHandle(handle) => {
                    let plaintext = secrets.resolve(handle)?;
                    let result = backend.type_text(serial, &plaintext, true, cancel);
                    drop(plaintext);
                    result?;
                    Ok(true)
                }
            }
        }
        AndroidAction::Key { key } => {
            backend.key(serial, *key, cancel)?;
            Ok(false)
        }
        AndroidAction::Rotate { orientation } => {
            backend.rotate(serial, *orientation, cancel)?;
            Ok(false)
        }
        AndroidAction::DeepLink { uri } => {
            backend.deeplink(serial, uri, cancel)?;
            Ok(false)
        }
    }
}

fn authorize(
    action: &AndroidAction,
    target: Option<&ResolvedTarget>,
) -> Result<(), AndroidActionError> {
    if let Some(target) = target {
        if target.sensitive
            && matches!(action, AndroidAction::Tap { .. })
            && target.role.as_deref() == Some("button")
            && !target.password
        {
            return Err(AndroidActionError::SensitiveDenied);
        }
        if matches!(
            action,
            AndroidAction::TypeText {
                value: SecretAwareText::Literal(_)
            }
        ) && target.password
        {
            return Err(AndroidActionError::SensitiveDenied);
        }
    }
    Ok(())
}

fn resolve_target(
    target: &TargetRef,
    stored: &StoredObservation,
    observation_id: ObservationId,
) -> Result<ResolvedTarget, AndroidActionError> {
    match target {
        TargetRef::ResourceId(id) | TargetRef::Accessibility { node: id } => {
            let matches: Vec<&SemanticTarget> = stored
                .targets
                .iter()
                .filter(|item| {
                    item.resource_id.as_deref() == Some(id.as_str()) || item.stable_ref == *id
                })
                .collect();
            pick_unique(matches, SemanticSource::ResourceId)
        }
        TargetRef::RoleName { role, name } => {
            let matches: Vec<&SemanticTarget> = stored
                .targets
                .iter()
                .filter(|item| {
                    item.role.as_deref() == Some(role.as_str())
                        && item.name.as_deref() == Some(name.as_str())
                })
                .collect();
            pick_unique(matches, SemanticSource::RoleName)
        }
        TargetRef::Visual {
            observation,
            region,
            label,
        } => {
            if *observation != observation_id {
                return Err(AndroidActionError::StaleObservation);
            }
            let matches: Vec<&SemanticTarget> = stored
                .targets
                .iter()
                .filter(|item| {
                    item.name.as_deref() == Some(label.as_str()) && item.bounds.intersects(*region)
                })
                .collect();
            if matches.is_empty() {
                return Err(AndroidActionError::TargetNotFound);
            }
            pick_unique(matches, SemanticSource::Visual)
        }
        TargetRef::Coordinate { observation, point } => {
            if *observation != observation_id {
                return Err(AndroidActionError::StaleObservation);
            }
            if !stored.geometry.contains(*point) {
                return Err(AndroidActionError::CoordinateOutOfBounds);
            }
            let hit = stored
                .targets
                .iter()
                .find(|item| item.bounds.contains(*point));
            Ok(ResolvedTarget {
                point: *point,
                source: SemanticSource::Coordinate,
                sensitive: hit.is_some_and(|item| item.sensitive),
                password: hit
                    .is_some_and(|item| item.sensitive && item.role.as_deref() == Some("edittext")),
                role: hit.and_then(|item| item.role.clone()),
            })
        }
    }
}

fn pick_unique(
    matches: Vec<&SemanticTarget>,
    source: SemanticSource,
) -> Result<ResolvedTarget, AndroidActionError> {
    match matches.as_slice() {
        [] => Err(AndroidActionError::TargetNotFound),
        [one] => Ok(resolved_from_target(one, source)),
        many => {
            let mut unique = many.iter().copied().filter(|item| item.unique);
            match (unique.next(), unique.next()) {
                (Some(one), None) => Ok(resolved_from_target(one, source)),
                (Some(_), Some(_)) | (None, _) => Err(AndroidActionError::AmbiguousTarget),
            }
        }
    }
}

fn resolved_from_target(target: &SemanticTarget, source: SemanticSource) -> ResolvedTarget {
    ResolvedTarget {
        point: target.bounds.center(),
        source,
        sensitive: target.sensitive,
        password: target.sensitive && target.role.as_deref() == Some("edittext"),
        role: target.role.clone(),
    }
}

fn derive_targets(nodes: &[UiNode]) -> Result<Vec<SemanticTarget>, AndroidActionError> {
    if nodes.len() > MAX_NODES {
        return Err(AndroidActionError::HierarchyBound);
    }
    let mut resource_counts: HashMap<&str, u32> = HashMap::new();
    let mut role_name_counts: HashMap<(&str, &str), u32> = HashMap::new();
    for node in nodes {
        if let Some(id) = node.resource_id.as_deref() {
            *resource_counts.entry(id).or_insert(0) += 1;
        }
        if let Some(name) = node_name(node) {
            *role_name_counts
                .entry((node.role.as_str(), name))
                .or_insert(0) += 1;
        }
    }
    let mut targets = Vec::new();
    for (index, node) in nodes.iter().enumerate() {
        if !node.interactive && node.text.is_none() && node.content_desc.is_none() {
            continue;
        }
        let (source, stable_ref, unique) = if let Some(id) = node.resource_id.as_deref() {
            (
                SemanticSource::ResourceId,
                id.to_owned(),
                resource_counts.get(id).copied() == Some(1),
            )
        } else if let Some(desc) = node.content_desc.as_deref() {
            (
                SemanticSource::Accessibility,
                format!("{}:{desc}", node.role),
                role_name_counts.get(&(node.role.as_str(), desc)).copied() == Some(1),
            )
        } else if let Some(name) = node_name(node) {
            (
                SemanticSource::RoleName,
                format!("{}:{name}", node.role),
                role_name_counts.get(&(node.role.as_str(), name)).copied() == Some(1),
            )
        } else {
            (
                SemanticSource::RoleName,
                format!("{}#{index}", node.role),
                false,
            )
        };
        targets.push(SemanticTarget {
            index: index as u32,
            stable_ref,
            source,
            role: Some(node.role.clone()),
            name: node_name(node).map(str::to_owned),
            resource_id: node.resource_id.clone(),
            interactive: node.interactive,
            sensitive: node.sensitive,
            unique,
            bounds: node.bounds,
        });
    }
    Ok(targets)
}

fn node_name(node: &UiNode) -> Option<&str> {
    node.content_desc
        .as_deref()
        .filter(|value| !value.is_empty())
        .or_else(|| node.text.as_deref().filter(|value| !value.is_empty()))
}

fn persist_screenshot(dump: &DeviceDump) -> Result<ScreenshotMeta, AndroidActionError> {
    let masked = dump.nodes.iter().any(|node| node.password && node.focused);
    let (bytes, width, height) = match dump.screenshot.as_ref() {
        Some(raw) => {
            check_screenshot_bounds(raw.bytes.len() as u64, raw.width, raw.height)?;
            if masked {
                (REDACTED_SCREENSHOT.to_vec(), raw.width, raw.height)
            } else {
                (raw.bytes.clone(), raw.width, raw.height)
            }
        }
        None => (
            REDACTED_SCREENSHOT.to_vec(),
            dump.geometry.width,
            dump.geometry.height,
        ),
    };
    let redaction = if masked {
        RedactionClass::Secret
    } else {
        RedactionClass::Sensitive
    };
    Ok(ScreenshotMeta {
        artifact: ArtifactRef::new(
            ArtifactId::from_bytes(&bytes),
            SCREENSHOT_MEDIA_TYPE,
            bytes.len() as u64,
            redaction,
        ),
        width,
        height,
        masked,
    })
}

fn hierarchy_fingerprint(dump: &DeviceDump) -> ArtifactId {
    fingerprint_hierarchy(
        &dump.package,
        &dump.activity,
        dump.orientation,
        dump.geometry,
        &dump.nodes,
    )
}

fn fingerprint_hierarchy(
    package: &str,
    activity: &str,
    orientation: Orientation,
    geometry: Geometry,
    nodes: &[UiNode],
) -> ArtifactId {
    let mut buf = Vec::new();
    buf.extend_from_slice(package.as_bytes());
    buf.push(0);
    buf.extend_from_slice(activity.as_bytes());
    buf.push(0);
    buf.extend_from_slice(&geometry.width.to_le_bytes());
    buf.extend_from_slice(&geometry.height.to_le_bytes());
    buf.push(orientation.as_rotation());
    buf.extend_from_slice(&(nodes.len() as u32).to_le_bytes());
    for node in nodes {
        push_opt_bytes(&mut buf, node.resource_id.as_deref());
        push_opt_bytes(&mut buf, Some(node.role.as_str()));
        push_opt_bytes(&mut buf, Some(node.class.as_str()));
        push_opt_bytes(&mut buf, node.content_desc.as_deref());
        push_opt_bytes(&mut buf, node.text.as_deref());
        buf.extend_from_slice(&node.bounds.left.to_le_bytes());
        buf.extend_from_slice(&node.bounds.top.to_le_bytes());
        buf.extend_from_slice(&node.bounds.right.to_le_bytes());
        buf.extend_from_slice(&node.bounds.bottom.to_le_bytes());
        buf.push(u8::from(node.clickable));
        buf.push(u8::from(node.focused));
        buf.push(u8::from(node.password));
        buf.push(u8::from(node.interactive));
    }
    ArtifactId::from_bytes(&buf)
}

fn hierarchy_generation(
    package: &str,
    activity: &str,
    orientation: Orientation,
    geometry: Geometry,
    nodes: &[UiNode],
) -> u64 {
    let fingerprint = fingerprint_hierarchy(package, activity, orientation, geometry, nodes);
    let digest = fingerprint.as_digest();
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&digest[..8]);
    u64::from_le_bytes(raw)
}

fn push_opt_bytes(buf: &mut Vec<u8>, value: Option<&str>) {
    match value {
        Some(text) => {
            buf.extend_from_slice(text.as_bytes());
            buf.push(0);
        }
        None => buf.push(0xff),
    }
}

fn hash_state(
    package: &str,
    activity: &str,
    generation: u64,
    orientation: Orientation,
    targets: &[SemanticTarget],
) -> ArtifactId {
    let mut buf = Vec::new();
    buf.extend_from_slice(package.as_bytes());
    buf.push(0);
    buf.extend_from_slice(activity.as_bytes());
    buf.push(0);
    buf.extend_from_slice(&generation.to_le_bytes());
    buf.push(orientation.as_rotation());
    for target in targets {
        buf.extend_from_slice(target.stable_ref.as_bytes());
        buf.push(0);
    }
    ArtifactId::from_bytes(&buf)
}

fn evaluate_assertions(
    expected: &[UiAssertion],
    after: &AndroidObservation,
) -> Vec<AssertionResult> {
    expected
        .iter()
        .map(|assertion| match assertion {
            UiAssertion::TextVisible(text) => AssertionResult {
                passed: after
                    .targets
                    .iter()
                    .any(|target| target.name.as_deref() == Some(text.as_str())),
                kind: "text_visible",
            },
            UiAssertion::ResourceVisible(id) => AssertionResult {
                passed: after
                    .targets
                    .iter()
                    .any(|target| target.resource_id.as_deref() == Some(id.as_str())),
                kind: "resource_visible",
            },
            UiAssertion::Orientation(orientation) => AssertionResult {
                passed: after.orientation == *orientation,
                kind: "orientation",
            },
            UiAssertion::Package(package) => AssertionResult {
                passed: after.package == *package,
                kind: "package",
            },
        })
        .collect()
}

fn parse_uiautomator_dump(xml: &str) -> Result<DeviceDump, AndroidActionError> {
    if xml.len() > MAX_HIERARCHY_BYTES {
        return Err(AndroidActionError::HierarchyBound);
    }
    let rotation = attr_after(xml, "rotation=\"").and_then(|value| value.parse().ok());
    let orientation = Orientation::from_rotation(rotation.unwrap_or(0));
    let mut nodes = Vec::new();
    let mut package = String::from("unknown");
    let mut activity = String::from("unknown");
    let mut max_right = 0i32;
    let mut max_bottom = 0i32;
    let mut rest = xml;
    while let Some(start) = rest.find("<node") {
        let after = &rest[start + 5..];
        let Some(end_rel) = after.find('>') else {
            break;
        };
        let attrs = &after[..end_rel];
        rest = &after[end_rel + 1..];
        let node = parse_node_attrs(attrs)?;
        if let Some(pkg) = node.package.as_deref()
            && package == "unknown"
        {
            package = pkg.to_owned();
        }
        max_right = max_right.max(node.bounds.right);
        max_bottom = max_bottom.max(node.bounds.bottom);
        nodes.push(node);
        if nodes.len() > MAX_NODES {
            return Err(AndroidActionError::HierarchyBound);
        }
    }
    if let Some(found) = attr_after(xml, "content-desc=\"activity:") {
        activity = bound_text(found, MAX_COMPONENT_BYTES, AndroidActionError::TargetBound)?;
    }
    let width = if max_right > 0 {
        max_right as u32
    } else {
        1080
    };
    let height = if max_bottom > 0 {
        max_bottom as u32
    } else {
        1920
    };
    let geometry = Geometry::new(
        width.min(MAX_SCREENSHOT_WIDTH),
        height.min(MAX_SCREENSHOT_HEIGHT),
    )?;
    let generation = hierarchy_generation(&package, &activity, orientation, geometry, &nodes);
    DeviceDump::new(
        &package,
        &activity,
        orientation,
        geometry,
        generation,
        nodes,
    )
}

fn parse_node_attrs(attrs: &str) -> Result<UiNode, AndroidActionError> {
    let resource_id = attr_value(attrs, "resource-id");
    let content_desc = attr_value(attrs, "content-desc");
    let text = attr_value(attrs, "text");
    let class = attr_value(attrs, "class").unwrap_or_else(|| "android.view.View".to_owned());
    let package = attr_value(attrs, "package");
    let clickable = attr_bool(attrs, "clickable");
    let focused = attr_bool(attrs, "focused");
    let password = attr_bool(attrs, "password");
    let bounds = parse_bounds(&attr_value(attrs, "bounds").unwrap_or_else(|| "[0,0][0,0]".into()))?;
    let role = role_from_class(&class);
    let sensitive = password
        || package.as_deref() == Some("com.android.permissioncontroller")
        || class.to_ascii_lowercase().contains("password");
    let interactive = clickable || password || role == "edittext";
    Ok(UiNode {
        resource_id: empty_to_none(resource_id),
        content_desc: empty_to_none(content_desc),
        text: empty_to_none(text),
        class,
        package: empty_to_none(package),
        role,
        bounds,
        clickable,
        focused,
        password,
        sensitive,
        interactive,
    })
}

fn parse_bounds(raw: &str) -> Result<Rect, AndroidActionError> {
    let trimmed = raw.trim();
    let err = AndroidActionError::InvalidTarget;
    let rest = trimmed.strip_prefix('[').ok_or(err)?;
    let (left, rest) = rest.split_once(',').ok_or(err)?;
    let (top, rest) = rest.split_once(']').ok_or(err)?;
    let rest = rest.strip_prefix('[').ok_or(err)?;
    let (right, rest) = rest.split_once(',').ok_or(err)?;
    let bottom = rest.strip_suffix(']').ok_or(err)?;
    Rect::new(
        left.parse().map_err(|_| err)?,
        top.parse().map_err(|_| err)?,
        right.parse().map_err(|_| err)?,
        bottom.parse().map_err(|_| err)?,
    )
}

fn role_from_class(class: &str) -> String {
    let tail = class
        .rsplit('.')
        .next()
        .unwrap_or(class)
        .to_ascii_lowercase();
    match tail.as_str() {
        "button" | "imagebutton" | "compoundbutton" => "button".into(),
        "edittext" | "autocompletetextview" => "edittext".into(),
        "textview" => "text".into(),
        "checkbox" => "checkbox".into(),
        "switch" | "switchcompat" => "switch".into(),
        other => other.chars().take(MAX_ROLE_BYTES).collect(),
    }
}

fn attr_value(attrs: &str, name: &str) -> Option<String> {
    let key = format!("{name}=\"");
    let start = attrs.find(&key)? + key.len();
    let rest = &attrs[start..];
    let end = rest.find('"')?;
    Some(decode_xml_entities(&rest[..end]))
}

fn attr_after<'a>(xml: &'a str, prefix: &str) -> Option<&'a str> {
    let start = xml.find(prefix)? + prefix.len();
    let rest = &xml[start..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

fn attr_bool(attrs: &str, name: &str) -> bool {
    matches!(attr_value(attrs, name).as_deref(), Some("true"))
}

fn decode_xml_entities(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

fn empty_to_none(value: Option<String>) -> Option<String> {
    value.filter(|item| !item.is_empty())
}

fn optional_text(value: Option<&str>, max: usize) -> Result<Option<String>, AndroidActionError> {
    match value {
        Some(text) if !text.is_empty() => Ok(Some(bound_text(
            text,
            max,
            AndroidActionError::TargetBound,
        )?)),
        Some(_) | None => Ok(None),
    }
}

fn bound_text(
    value: &str,
    max: usize,
    err: AndroidActionError,
) -> Result<String, AndroidActionError> {
    if value.is_empty() || value.len() > max {
        return Err(err);
    }
    if value.contains('\0') {
        return Err(err);
    }
    Ok(value.to_owned())
}

fn check_screenshot_bounds(bytes: u64, width: u32, height: u32) -> Result<(), AndroidActionError> {
    if bytes == 0 || bytes > MAX_SCREENSHOT_BYTES {
        return Err(AndroidActionError::ScreenshotBound);
    }
    if width == 0 || height == 0 || width > MAX_SCREENSHOT_WIDTH || height > MAX_SCREENSHOT_HEIGHT {
        return Err(AndroidActionError::ScreenshotBound);
    }
    Ok(())
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), AndroidActionError> {
    if cancel.is_cancelled() {
        Err(AndroidActionError::Cancelled)
    } else {
        Ok(())
    }
}

fn is_deeplink_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b':' | b'/' | b'?' | b'&' | b'=' | b'.' | b'_' | b'-' | b'@' | b'%' | b'#' | b'+'
        )
}

fn bump_generation(device: &mut FakeDeviceUi) -> Result<(), AndroidActionError> {
    device.generation = device
        .generation
        .checked_add(1)
        .ok_or(AndroidActionError::Unavailable)?;
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

fn adb_bin_name() -> &'static str {
    if cfg!(windows) { "adb.exe" } else { "adb" }
}

fn run_adb(
    program: &Path,
    cwd: &Path,
    args: &[String],
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Vec<u8>, AndroidActionError> {
    check_cancel(cancel)?;
    if timeout.is_zero() {
        return Err(AndroidActionError::TimeoutInvalid);
    }
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| AndroidActionError::Backend)?;
    let deadline = Instant::now() + timeout;
    loop {
        if cancel.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(AndroidActionError::Cancelled);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(AndroidActionError::Timeout);
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return Err(AndroidActionError::Backend);
                }
                let mut stdout = child.stdout.take().ok_or(AndroidActionError::Backend)?;
                let mut buf = Vec::new();
                stdout
                    .by_ref()
                    .take(MAX_HOST_OUTPUT_BYTES as u64 + 1)
                    .read_to_end(&mut buf)
                    .map_err(|_| AndroidActionError::Backend)?;
                if buf.len() > MAX_HOST_OUTPUT_BYTES {
                    return Err(AndroidActionError::HierarchyBound);
                }
                return Ok(buf);
            }
            Ok(None) => std::thread::sleep(HOST_POLL),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(AndroidActionError::Backend);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::android::manager::{AndroidManager, AndroidSpec};
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    struct TempEnv {
        root: PathBuf,
    }

    impl TempEnv {
        fn create() -> Self {
            let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "rapidlm-android-action-{}-{seq}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).expect("root");
            Self { root }
        }
    }

    impl Drop for TempEnv {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn fixture() -> (
        TempEnv,
        AndroidManager,
        AndroidDeviceHandle,
        Arc<FakeAndroidUi>,
        AndroidActor,
    ) {
        let env = TempEnv::create();
        let manager = AndroidManager::open(&env.root).expect("manager");
        let handle = manager
            .acquire(AndroidSpec::assigned("Pixel_6", "session-a").expect("spec"))
            .expect("acquire");
        let ui = Arc::new(FakeAndroidUi::new());
        let geometry = Geometry::new(1080, 1920).expect("geometry");
        let save = UiNode::button(
            "com.example:id/save",
            "Save",
            Rect::new(100, 200, 300, 280).expect("bounds"),
        )
        .expect("save");
        let dump = DeviceDump::new(
            "com.example",
            "MainActivity",
            Orientation::Portrait,
            geometry,
            1,
            vec![save],
        )
        .expect("dump")
        .with_screenshot(
            RawScreenshot::new(vec![0x89, 0x50, 0x4E, 0x47], 1080, 1920).expect("png"),
        );
        ui.install(&handle, dump).expect("install");
        ui.on_tap(
            &handle,
            "com.example:id/save",
            vec![
                UiNode::text("Saved", Rect::new(100, 200, 300, 240).expect("saved")).expect("text"),
            ],
        )
        .expect("handler");
        let actor = AndroidActor::new(Arc::clone(&ui) as Arc<dyn AndroidUiBackend>);
        (env, manager, handle, ui, actor)
    }

    #[test]
    fn observe_captures_hierarchy_and_screenshot_metadata() {
        let (_env, _manager, handle, _ui, actor) = fixture();
        let obs = actor
            .observe(&handle, ObserveRequest::new().with_screenshot(true))
            .expect("observe");
        assert_eq!(obs.package(), "com.example");
        assert_eq!(obs.targets().len(), 1);
        assert_eq!(obs.targets()[0].resource_id(), Some("com.example:id/save"));
        assert_eq!(obs.targets()[0].source(), SemanticSource::ResourceId);
        let shot = obs.screenshot().expect("screenshot");
        assert_eq!(shot.width(), 1080);
        assert!(!shot.is_masked());
        assert_eq!(shot.artifact().media_type, SCREENSHOT_MEDIA_TYPE);
        let view = obs.model_view();
        assert_eq!(view.targets.len(), 1);
        assert!(view.screenshot.is_some());
    }

    #[test]
    fn tap_semantic_target_records_before_after_state_ids() {
        let (_env, _manager, handle, ui, actor) = fixture();
        let before = actor.observe(&handle, ObserveRequest::new()).expect("obs");
        let receipt = actor
            .act(
                &handle,
                AndroidActionRequest::tap(
                    before.id(),
                    TargetRef::resource_id("com.example:id/save").expect("target"),
                )
                .expect("req")
                .with_expected(vec![UiAssertion::TextVisible("Saved".into())]),
            )
            .expect("act");
        assert_ne!(receipt.before(), receipt.after());
        assert_eq!(receipt.status(), ActionStatus::Succeeded);
        assert_eq!(receipt.target_strategy(), Some(SemanticSource::ResourceId));
        assert_eq!(receipt.before_observation(), Some(before.id()));
        assert_ne!(receipt.after_observation(), before.id());
        assert!(ui.last_ops(&handle).contains(&"tap"));
        assert!(!ui.executed_shell(&handle));
    }

    #[test]
    fn tap_requires_current_observation() {
        let (_env, _manager, handle, _ui, actor) = fixture();
        let err = actor
            .act(
                &handle,
                AndroidActionRequest::new(AndroidAction::Tap { count: 1 })
                    .expect("req")
                    .with_target(TargetRef::resource_id("com.example:id/save").expect("target")),
            )
            .expect_err("obs required");
        assert_eq!(err, AndroidActionError::ObservationRequired);
        assert_eq!(err.code(), ErrorCode::ToolInvalidArguments);
    }

    #[test]
    fn stale_observation_is_rejected_after_prior_act() {
        let (_env, _manager, handle, _ui, actor) = fixture();
        let first = actor
            .observe(&handle, ObserveRequest::new())
            .expect("first");
        actor
            .act(
                &handle,
                AndroidActionRequest::tap(
                    first.id(),
                    TargetRef::resource_id("com.example:id/save").expect("target"),
                )
                .expect("req"),
            )
            .expect("first act");
        let err = actor
            .act(
                &handle,
                AndroidActionRequest::tap(
                    first.id(),
                    TargetRef::resource_id("com.example:id/save").expect("target"),
                )
                .expect("req"),
            )
            .expect_err("stale");
        assert_eq!(err, AndroidActionError::StaleObservation);
        assert_eq!(err.code(), ErrorCode::BrowserStaleObservation);
    }

    #[test]
    fn coordinate_target_rejected_after_rotation_changes_geometry() {
        let (_env, _manager, handle, ui, actor) = fixture();
        let first = actor
            .observe(&handle, ObserveRequest::new())
            .expect("first");
        actor
            .act(
                &handle,
                AndroidActionRequest::rotate(first.id(), Orientation::Landscape).expect("rotate"),
            )
            .expect("rotated");
        assert_eq!(
            ui.last_ops(&handle)
                .iter()
                .filter(|op| **op == "rotate")
                .count(),
            1
        );
        let err = actor
            .act(
                &handle,
                AndroidActionRequest::tap(
                    first.id(),
                    TargetRef::Coordinate {
                        observation: first.id(),
                        point: Point::new(200, 240),
                    },
                )
                .expect("req"),
            )
            .expect_err("stale after rotate");
        assert_eq!(err, AndroidActionError::StaleObservation);
    }

    #[test]
    fn key_and_deeplink_do_not_require_observation() {
        let (_env, _manager, handle, ui, actor) = fixture();
        let key = actor
            .act(
                &handle,
                AndroidActionRequest::key(AndroidKey::Back).expect("key"),
            )
            .expect("key");
        assert_ne!(key.before(), key.after());
        assert_eq!(key.before_observation(), None);
        assert_eq!(key.action(), ActionKind::Key);

        let uri = DeepLinkUri::parse("myapp://open/item").expect("uri");
        let link = actor
            .act(
                &handle,
                AndroidActionRequest::deeplink(uri).expect("deeplink"),
            )
            .expect("deeplink");
        assert_eq!(link.action(), ActionKind::DeepLink);
        assert_eq!(
            ui.last_deeplink(&handle).as_deref(),
            Some("myapp://open/item")
        );
        assert!(!ui.executed_shell(&handle));
    }

    #[test]
    fn ambiguous_role_name_does_not_tap_first_match() {
        let env = TempEnv::create();
        let manager = AndroidManager::open(&env.root).expect("manager");
        let handle = manager
            .acquire(AndroidSpec::assigned("Pixel_6", "session-a").expect("spec"))
            .expect("acquire");
        let ui = Arc::new(FakeAndroidUi::new());
        let a = UiNode::button(
            "com.example:id/a",
            "OK",
            Rect::new(0, 0, 80, 40).expect("a"),
        )
        .expect("a");
        let b = UiNode::button(
            "com.example:id/b",
            "OK",
            Rect::new(90, 0, 170, 40).expect("b"),
        )
        .expect("b");
        ui.install(
            &handle,
            DeviceDump::new(
                "com.example",
                "MainActivity",
                Orientation::Portrait,
                Geometry::new(1080, 1920).expect("g"),
                1,
                vec![a, b],
            )
            .expect("dump"),
        )
        .expect("install");
        let actor = AndroidActor::new(Arc::clone(&ui) as Arc<dyn AndroidUiBackend>);
        let obs = actor.observe(&handle, ObserveRequest::new()).expect("obs");
        let err = actor
            .act(
                &handle,
                AndroidActionRequest::tap(
                    obs.id(),
                    TargetRef::role_name("button", "OK").expect("target"),
                )
                .expect("req"),
            )
            .expect_err("ambiguous");
        assert_eq!(err, AndroidActionError::AmbiguousTarget);
        assert!(!ui.last_ops(&handle).contains(&"tap"));
    }

    #[test]
    fn password_literal_is_denied_secret_handle_is_redacted() {
        let env = TempEnv::create();
        let manager = AndroidManager::open(&env.root).expect("manager");
        let handle = manager
            .acquire(AndroidSpec::assigned("Pixel_6", "session-a").expect("spec"))
            .expect("acquire");
        let ui = Arc::new(FakeAndroidUi::new());
        let field = UiNode::edit_text(
            "com.example:id/password",
            "Password",
            Rect::new(40, 80, 400, 140).expect("field"),
            true,
        )
        .expect("field");
        ui.install(
            &handle,
            DeviceDump::new(
                "com.example",
                "LoginActivity",
                Orientation::Portrait,
                Geometry::new(1080, 1920).expect("g"),
                1,
                vec![field],
            )
            .expect("dump")
            .with_screenshot(RawScreenshot::new(vec![1, 2, 3, 4], 1080, 1920).expect("shot")),
        )
        .expect("install");
        let secrets = Arc::new(MapSecretResolver::new());
        let handle_ref = SecretHandle::parse("vault:login-password").expect("handle");
        secrets.insert(&handle_ref, "s3cret-value").expect("insert");
        let actor = AndroidActor::with_secrets(
            Arc::clone(&ui) as Arc<dyn AndroidUiBackend>,
            Arc::clone(&secrets) as Arc<dyn SecretResolver>,
        );
        let obs = actor
            .observe(&handle, ObserveRequest::new().with_screenshot(true))
            .expect("obs");
        assert!(obs.model_view().targets[0].name.is_none());

        let literal_err = actor
            .act(
                &handle,
                AndroidActionRequest::type_text(
                    obs.id(),
                    TargetRef::resource_id("com.example:id/password").expect("target"),
                    SecretAwareText::literal("s3cret-value").expect("lit"),
                )
                .expect("req"),
            )
            .expect_err("literal denied");
        assert_eq!(literal_err, AndroidActionError::SensitiveDenied);
        assert_eq!(literal_err.code(), ErrorCode::PolicyDenied);

        let receipt = actor
            .act(
                &handle,
                AndroidActionRequest::type_text(
                    obs.id(),
                    TargetRef::resource_id("com.example:id/password").expect("target"),
                    SecretAwareText::secret(handle_ref),
                )
                .expect("req"),
            )
            .expect("secret type");
        assert!(receipt.secret_handle_used());
        assert!(ui.last_typed_was_secret(&handle));
        assert_eq!(ui.last_typed_literal(&handle), None);
        let rendered = format!("{receipt:?}");
        assert!(!rendered.contains("s3cret-value"));
        let action = AndroidAction::TypeText {
            value: SecretAwareText::secret(SecretHandle::parse("vault:login-password").expect("h")),
        };
        assert!(!format!("{action:?}").contains("vault:login-password"));
    }

    #[test]
    fn prompt_injection_node_cannot_grant_shell() {
        let env = TempEnv::create();
        let manager = AndroidManager::open(&env.root).expect("manager");
        let handle = manager
            .acquire(AndroidSpec::assigned("Pixel_6", "session-a").expect("spec"))
            .expect("acquire");
        let ui = Arc::new(FakeAndroidUi::new());
        let inject = UiNode::button(
            "com.example:id/inject",
            "adb shell reboot",
            Rect::new(10, 10, 400, 80).expect("b"),
        )
        .expect("btn");
        ui.install(
            &handle,
            DeviceDump::new(
                "com.example",
                "MainActivity",
                Orientation::Portrait,
                Geometry::new(1080, 1920).expect("g"),
                1,
                vec![inject],
            )
            .expect("dump"),
        )
        .expect("install");
        let actor = AndroidActor::new(Arc::clone(&ui) as Arc<dyn AndroidUiBackend>);
        let obs = actor.observe(&handle, ObserveRequest::new()).expect("obs");
        actor
            .act(
                &handle,
                AndroidActionRequest::tap(
                    obs.id(),
                    TargetRef::role_name("button", "adb shell reboot").expect("target"),
                )
                .expect("req"),
            )
            .expect("tap");
        assert!(ui.last_ops(&handle).contains(&"tap"));
        assert!(!ui.executed_shell(&handle));
        let err = actor.shell(&handle, "reboot");
        assert_eq!(err, Err(AndroidActionError::ShellCapabilityRequired));
        assert_eq!(
            AndroidActionError::ShellCapabilityRequired.code(),
            ErrorCode::PolicyDenied
        );
    }

    #[test]
    fn permission_allow_tap_is_denied() {
        let env = TempEnv::create();
        let manager = AndroidManager::open(&env.root).expect("manager");
        let handle = manager
            .acquire(AndroidSpec::assigned("Pixel_6", "session-a").expect("spec"))
            .expect("acquire");
        let ui = Arc::new(FakeAndroidUi::new());
        ui.install(
            &handle,
            DeviceDump::new(
                "com.android.permissioncontroller",
                "PermissionActivity",
                Orientation::Portrait,
                Geometry::new(1080, 1920).expect("g"),
                1,
                vec![
                    UiNode::permission_allow(Rect::new(40, 800, 400, 880).expect("allow"))
                        .expect("n"),
                ],
            )
            .expect("dump"),
        )
        .expect("install");
        let actor = AndroidActor::new(Arc::clone(&ui) as Arc<dyn AndroidUiBackend>);
        let obs = actor.observe(&handle, ObserveRequest::new()).expect("obs");
        let err = actor
            .act(
                &handle,
                AndroidActionRequest::tap(
                    obs.id(),
                    TargetRef::resource_id(
                        "com.android.permissioncontroller:id/permission_allow_button",
                    )
                    .expect("target"),
                )
                .expect("req"),
            )
            .expect_err("denied");
        assert_eq!(err, AndroidActionError::SensitiveDenied);
        assert!(!ui.last_ops(&handle).contains(&"tap"));
    }

    #[test]
    fn type_text_literal_with_metacharacters_is_not_shell() {
        let (_env, _manager, handle, ui, actor) = fixture();
        let field = UiNode::edit_text(
            "com.example:id/name",
            "Name",
            Rect::new(40, 300, 400, 360).expect("f"),
            false,
        )
        .expect("field");
        ui.set_nodes(&handle, vec![field]).expect("nodes");
        let obs = actor.observe(&handle, ObserveRequest::new()).expect("obs");
        actor
            .act(
                &handle,
                AndroidActionRequest::type_text(
                    obs.id(),
                    TargetRef::resource_id("com.example:id/name").expect("target"),
                    SecretAwareText::literal("hello;reboot").expect("lit"),
                )
                .expect("req"),
            )
            .expect("type");
        assert_eq!(
            ui.last_typed_literal(&handle).as_deref(),
            Some("hello;reboot")
        );
        assert!(!ui.executed_shell(&handle));
        assert_eq!(
            encode_input_text("hello;reboot").expect("enc"),
            "hello%3Breboot"
        );
    }

    #[test]
    fn deeplink_rejects_shell_metacharacters() {
        for raw in [
            "myapp://x;reboot",
            "https://example.com|wipe",
            "-d http://x",
            "nocolon",
            "app://x$(reboot)",
        ] {
            assert_eq!(
                DeepLinkUri::parse(raw),
                Err(AndroidActionError::InvalidDeepLink),
                "{raw}"
            );
        }
    }

    #[test]
    fn host_argv_is_typed_and_not_concatenated_shell() {
        let serial = DeviceSerial::parse("emulator-5554").expect("serial");
        let dump = adb_argv(&serial, &TypedAdbCommand::DumpHierarchy);
        assert_eq!(
            dump,
            vec![
                "-s",
                "emulator-5554",
                "exec-out",
                "uiautomator",
                "dump",
                "/dev/tty"
            ]
        );
        let shot = adb_argv(&serial, &TypedAdbCommand::Screenshot);
        assert_eq!(
            shot,
            vec!["-s", "emulator-5554", "exec-out", "screencap", "-p"]
        );
        let tap = adb_argv(&serial, &TypedAdbCommand::Tap { x: 10, y: 20 });
        assert_eq!(
            tap,
            vec!["-s", "emulator-5554", "shell", "input", "tap", "10", "20"]
        );
        assert!(tap.iter().all(|part| !part.contains(' ')));
        let text = adb_argv(
            &serial,
            &TypedAdbCommand::TypeText {
                encoded: encode_input_text("hello;reboot").expect("enc"),
            },
        );
        assert_eq!(text.last().map(String::as_str), Some("hello%3Breboot"));
        assert!(!text.iter().any(|part| part.contains(';')));
        let key = adb_argv(&serial, &TypedAdbCommand::Key { code: 4 });
        assert_eq!(
            key,
            vec!["-s", "emulator-5554", "shell", "input", "keyevent", "4"]
        );
        let rotate = adb_argv(&serial, &TypedAdbCommand::Rotate { value: 1 });
        assert_eq!(
            rotate,
            vec![
                "-s",
                "emulator-5554",
                "shell",
                "settings",
                "put",
                "system",
                "user_rotation",
                "1"
            ]
        );
        let link = adb_argv(
            &serial,
            &TypedAdbCommand::DeepLink {
                uri: "myapp://open/item".into(),
            },
        );
        assert_eq!(
            link,
            vec![
                "-s",
                "emulator-5554",
                "shell",
                "am",
                "start",
                "-a",
                "android.intent.action.VIEW",
                "-d",
                "myapp://open/item"
            ]
        );
    }

    #[test]
    fn host_backend_unavailable_without_sdk() {
        let err = HostAdbUi::open("/nonexistent/android-sdk").expect_err("missing");
        assert_eq!(err, AndroidActionError::CapabilityUnavailable);
        assert_eq!(err.code(), ErrorCode::MobileCapabilityUnavailable);
    }

    #[test]
    fn cancel_before_observe_or_act_fails_closed() {
        let (_env, _manager, handle, _ui, actor) = fixture();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            actor
                .observe(&handle, ObserveRequest::new().with_cancel(cancel.clone()))
                .expect_err("obs"),
            AndroidActionError::Cancelled
        );
        let obs_cancel = CancellationToken::new();
        let obs = actor.observe(&handle, ObserveRequest::new()).expect("obs");
        obs_cancel.cancel();
        assert_eq!(
            actor
                .act(
                    &handle,
                    AndroidActionRequest::tap(
                        obs.id(),
                        TargetRef::resource_id("com.example:id/save").expect("t"),
                    )
                    .expect("req")
                    .with_cancel(obs_cancel),
                )
                .expect_err("act"),
            AndroidActionError::Cancelled
        );
    }

    #[test]
    fn uiautomator_dump_parser_reads_resource_id_and_bounds() {
        let xml = r#"<hierarchy rotation="1">
            <node index="0" text="" resource-id="" class="android.widget.FrameLayout" package="com.example" content-desc="" clickable="false" password="false" focused="false" bounds="[0,0][1080,1920]">
            <node text="Save" resource-id="com.example:id/save" class="android.widget.Button" package="com.example" content-desc="" clickable="true" password="false" focused="false" bounds="[100,200][300,280]"/>
            </node>
            </hierarchy>"#;
        let dump = parse_uiautomator_dump(xml).expect("parse");
        assert_eq!(dump.orientation, Orientation::Landscape);
        assert_eq!(dump.package, "com.example");
        assert_eq!(dump.nodes.len(), 2);
        assert_eq!(dump.nodes[1].resource_id(), Some("com.example:id/save"));
        assert_eq!(dump.nodes[1].role(), "button");
        assert_eq!(dump.nodes[1].bounds.center(), Point::new(200, 240));
        let moved = xml.replace("[100,200][300,280]", "[400,800][600,880]");
        let dump_moved = parse_uiautomator_dump(&moved).expect("parse moved");
        assert_eq!(dump.geometry, dump_moved.geometry);
        assert_ne!(
            dump.generation, dump_moved.generation,
            "host dump generation is content-derived, not pinned to 1"
        );
        assert_ne!(
            hierarchy_fingerprint(&dump),
            hierarchy_fingerprint(&dump_moved)
        );
    }

    #[test]
    fn invalid_timeouts_fail_closed() {
        assert_eq!(
            ObserveRequest::new()
                .with_timeout(Duration::ZERO)
                .expect_err("zero"),
            AndroidActionError::TimeoutInvalid
        );
        assert_eq!(
            AndroidActionRequest::key(AndroidKey::Home)
                .expect("k")
                .with_timeout(MAX_ACTION_TIMEOUT + Duration::from_secs(1))
                .expect_err("max"),
            AndroidActionError::TimeoutInvalid
        );
    }

    #[test]
    fn in_place_hierarchy_change_stales_tap_and_coordinate() {
        let (_env, _manager, handle, ui, actor) = fixture();
        let obs = actor.observe(&handle, ObserveRequest::new()).expect("obs");
        let moved = UiNode::button(
            "com.example:id/save",
            "Save",
            Rect::new(400, 800, 600, 880).expect("moved"),
        )
        .expect("btn");
        ui.replace_nodes_same_geometry(&handle, vec![moved])
            .expect("mutate");
        let tap_err = actor
            .act(
                &handle,
                AndroidActionRequest::tap(
                    obs.id(),
                    TargetRef::resource_id("com.example:id/save").expect("t"),
                )
                .expect("req"),
            )
            .expect_err("stale tap");
        assert_eq!(tap_err, AndroidActionError::StaleObservation);
        assert_eq!(tap_err.code(), ErrorCode::BrowserStaleObservation);
        let coord_err = actor
            .act(
                &handle,
                AndroidActionRequest::tap(
                    obs.id(),
                    TargetRef::Coordinate {
                        observation: obs.id(),
                        point: Point::new(200, 240),
                    },
                )
                .expect("req"),
            )
            .expect_err("stale coordinate");
        assert_eq!(coord_err, AndroidActionError::StaleObservation);
        assert_eq!(coord_err.code(), ErrorCode::BrowserStaleObservation);
        assert!(!ui.last_ops(&handle).contains(&"tap"));
    }

    #[test]
    fn stop_then_observe_or_act_rejects_stale_device_lease() {
        let (_env, _manager, handle, ui, actor) = fixture();
        let obs = actor.observe(&handle, ObserveRequest::new()).expect("obs");
        handle
            .stop(&CancellationToken::new())
            .expect("stop assigned handle");
        let observe_err = actor
            .observe(&handle, ObserveRequest::new())
            .expect_err("stopped observe");
        assert_eq!(observe_err, AndroidActionError::DeviceClosed);
        assert_eq!(observe_err.code(), ErrorCode::SessionNotFound);
        let act_err = actor
            .act(
                &handle,
                AndroidActionRequest::tap(
                    obs.id(),
                    TargetRef::resource_id("com.example:id/save").expect("t"),
                )
                .expect("req"),
            )
            .expect_err("stopped act");
        assert_eq!(act_err, AndroidActionError::DeviceClosed);
        assert_eq!(act_err.code(), ErrorCode::SessionNotFound);
        assert!(!ui.last_ops(&handle).contains(&"tap"));
    }

    #[test]
    fn missing_device_fails_closed() {
        let env = TempEnv::create();
        let manager = AndroidManager::open(&env.root).expect("manager");
        let handle = manager
            .acquire(AndroidSpec::assigned("Pixel_6", "session-a").expect("spec"))
            .expect("acquire");
        let actor = AndroidActor::new(Arc::new(FakeAndroidUi::new()));
        assert_eq!(
            actor
                .observe(&handle, ObserveRequest::new())
                .expect_err("missing"),
            AndroidActionError::DeviceNotFound
        );
    }
}
