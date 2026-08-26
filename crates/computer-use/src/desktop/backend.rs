//! Accessibility-first desktop observe/action APIs and OS capability reporting.
//!
//! OS adapters implement [`DesktopBackend`]. [`DesktopActor`] assigns
//! observation IDs, prefers window/AX refs, and never treats an unadvertised
//! feature as success. Coordinate injection is a distinct fallback action
//! (T-CU-02). Window/app text is untrusted data and cannot grant capability
//! (T-CU-01). Secret handles stay opaque (T-CU-03).

use std::collections::HashMap;
use std::error::Error;
use std::fmt::{self, Debug};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use capability_broker::{CancellationToken, SecretHandle};
use protocol::{ArtifactId, ErrorCode, RuntimeId};

/// Maximum UTF-8 bytes for a window title, app id, or AX name.
pub const MAX_NAME_BYTES: usize = 256;

/// Maximum UTF-8 bytes for an accessibility role or identifier.
pub const MAX_ROLE_BYTES: usize = 64;

/// Maximum UTF-8 bytes for a stable-per-observation ref.
pub const MAX_STABLE_REF_BYTES: usize = 256;

/// Maximum UTF-8 bytes for typed literal text.
pub const MAX_TYPE_BYTES: usize = 4096;

/// Maximum UTF-8 bytes for a key token.
pub const MAX_KEY_BYTES: usize = 32;

/// Maximum windows retained on one observation.
pub const MAX_WINDOWS: usize = 64;

/// Maximum accessibility nodes retained on one observation.
pub const MAX_NODES: usize = 512;

/// Maximum click count (single or double).
pub const MAX_CLICK_COUNT: u8 = 2;

/// Maximum absolute scroll delta on either axis.
pub const MAX_SCROLL_ABS: i32 = 100_000;

/// Maximum screenshot width in device pixels.
pub const MAX_SCREENSHOT_WIDTH: u32 = 3840;

/// Maximum screenshot height in device pixels.
pub const MAX_SCREENSHOT_HEIGHT: u32 = 2160;

/// Maximum observe timeout.
pub const MAX_OBSERVE_TIMEOUT: Duration = Duration::from_secs(30);

/// Default observe timeout.
pub const DEFAULT_OBSERVE_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum act timeout.
pub const MAX_ACT_TIMEOUT: Duration = Duration::from_secs(30);

/// Default act timeout.
pub const DEFAULT_ACT_TIMEOUT: Duration = Duration::from_secs(10);

/// Identity of one desktop session. Distinct from [`protocol::SessionId`].
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct DesktopSessionId(RuntimeId);

/// Identity of one capture. Distinct from [`protocol::SessionId`].
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct ObservationId(RuntimeId);

/// Identity of one executed action receipt.
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct ActionReceiptId(RuntimeId);

/// Host platform the adapter binds to.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum DesktopPlatform {
    Macos,
    Windows,
    Linux,
}

/// Why [`DesktopHealth`] reported unavailable. Distinct from a transport error.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum DesktopHealthReason {
    PlatformUnsupported,
    PermissionMissing,
    AccessibilityUnavailable,
    DisplayUnavailable,
}

/// Cooperative health probe. `available == false` is not a clean/pass result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DesktopHealth {
    available: bool,
    reason: Option<DesktopHealthReason>,
}

/// Explicit OS-adapter advertisement. Missing flags are unsupported, not implied.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct DesktopCapabilities {
    platform: DesktopPlatform,
    accessibility: bool,
    window_enum: bool,
    window_focus: bool,
    window_resize: bool,
    pointer: bool,
    keyboard: bool,
    type_text: bool,
    screenshot: bool,
    launch_app: bool,
    /// Coordinate injection is never implied by pointer/AX support (T-CU-02).
    coordinate_fallback: bool,
}

/// Display geometry bound into an observation and coordinate fallback.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct DisplayGeometry {
    width: u32,
    height: u32,
}

/// Inclusive pixel rectangle in the originating observation geometry.
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

/// Pointer button for click actions.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// Bounded key token. Not a free-form OS event string.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct KeyCode(String);

/// Bounded application identifier. Not a host path or shell fragment.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct AppRef(String);

/// Stable-per-observation window identity.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct WindowRef {
    observation: ObservationId,
    stable_ref: String,
}

/// Stable-per-observation accessibility node identity.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct AccessibilityNodeRef {
    observation: ObservationId,
    window: String,
    stable_ref: String,
}

/// Compact accessibility-tree metadata. The model does not receive the raw tree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AccessibilitySnapshotRef {
    node_count: u32,
    interactive_count: u32,
}

/// How a semantic desktop target was derived.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum SemanticSource {
    Window,
    Accessibility,
}

/// Semantic locator taken from a current observation.
#[derive(Clone, Eq, PartialEq)]
pub enum DesktopTargetRef {
    Window(WindowRef),
    Accessibility(AccessibilityNodeRef),
}

/// Window captured on an observation. Title is data, never authority (T-CU-01).
#[derive(Clone, Eq, PartialEq)]
pub struct WindowInfo {
    window: WindowRef,
    title: String,
    app: Option<AppRef>,
    bounds: Rect,
    focused: bool,
    sensitive: bool,
}

/// AX-derived locator usable by a later act step.
#[derive(Clone, Eq, PartialEq)]
pub struct SemanticTarget {
    index: u32,
    node: AccessibilityNodeRef,
    window: WindowRef,
    source: SemanticSource,
    role: Option<String>,
    name: Option<String>,
    identifier: Option<String>,
    interactive: bool,
    sensitive: bool,
    unique: bool,
}

/// Model-visible target: locators only, never field values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticTargetView {
    index: u32,
    window_ref: String,
    node_ref: String,
    source: SemanticSource,
    role: Option<String>,
    name: Option<String>,
    identifier: Option<String>,
    interactive: bool,
    sensitive: bool,
    unique: bool,
}

/// Screenshot metadata. No pixel bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScreenshotMeta {
    width: u32,
    height: u32,
    masked: bool,
}

/// Assigned observation for one live desktop session.
#[derive(Clone, Eq, PartialEq)]
pub struct DesktopObservation {
    id: ObservationId,
    session_id: DesktopSessionId,
    generation: u64,
    state_hash: ArtifactId,
    focused_window: Option<WindowRef>,
    windows: Vec<WindowInfo>,
    targets: Vec<SemanticTarget>,
    accessibility: Option<AccessibilitySnapshotRef>,
    geometry: DisplayGeometry,
    screenshot: Option<ScreenshotMeta>,
    captured_at_ms: u64,
}

/// Bounded metadata the model may consume. No screenshot bytes, no secrets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopObservationView {
    id: ObservationId,
    session_id: DesktopSessionId,
    generation: u64,
    focused_window: Option<String>,
    windows: Vec<String>,
    targets: Vec<SemanticTargetView>,
    accessibility: Option<AccessibilitySnapshotRef>,
    geometry: DisplayGeometry,
    screenshot: Option<ScreenshotMeta>,
}

/// Observe options. Screenshot is off by default.
#[derive(Clone, Debug)]
pub struct DesktopObserveRequest {
    screenshot: bool,
    timeout: Duration,
    cancel: CancellationToken,
}

/// Text payload. Secret handles stay opaque; plaintext is never logged.
#[derive(Clone, Eq, PartialEq)]
pub enum SecretAwareString {
    Literal(String),
    SecretHandle(SecretHandle),
}

/// Side-effecting desktop action. Coordinates are an explicit fallback variant.
#[derive(Clone, Eq, PartialEq)]
pub enum DesktopAction {
    Click {
        target: DesktopTargetRef,
        button: MouseButton,
        count: u8,
    },
    TypeText {
        target: DesktopTargetRef,
        value: SecretAwareString,
    },
    Key {
        target: Option<DesktopTargetRef>,
        key: KeyCode,
    },
    Chord {
        keys: Vec<KeyCode>,
    },
    Scroll {
        target: Option<DesktopTargetRef>,
        dx: i32,
        dy: i32,
    },
    FocusWindow {
        window: WindowRef,
    },
    ResizeWindow {
        window: WindowRef,
        width: u32,
        height: u32,
    },
    LaunchApp {
        app: AppRef,
    },
    CloseWindow {
        window: WindowRef,
    },
    /// Observation-bound coordinate injection. Not a semantic target.
    CoordinateFallback {
        point: Point,
        button: MouseButton,
        count: u8,
    },
}

/// Discriminator stored on a receipt. No payload values.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ActionKind {
    Click,
    TypeText,
    Key,
    Chord,
    Scroll,
    FocusWindow,
    ResizeWindow,
    LaunchApp,
    CloseWindow,
    CoordinateFallback,
}

/// Executor outcome. Postconditions are verified by a later step.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ActionStatus {
    Executed,
}

/// Proof an action was sent against a current observation.
#[derive(Clone, Eq, PartialEq)]
pub struct DesktopActionReceipt {
    id: ActionReceiptId,
    session_id: DesktopSessionId,
    observation_id: ObservationId,
    kind: ActionKind,
    target: Option<String>,
    status: ActionStatus,
    secret_handle_used: Option<String>,
    coordinate_fallback: bool,
}

/// Act options. Every action carries the current observation ID.
#[derive(Clone, Debug)]
pub struct DesktopActionRequest {
    observation_id: ObservationId,
    action: DesktopAction,
    timeout: Duration,
    cancel: CancellationToken,
}

/// Raw adapter capture before an observation ID is assigned.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopCapture {
    generation: u64,
    geometry: DisplayGeometry,
    windows: Vec<DesktopWindowCapture>,
    nodes: Vec<DesktopNodeCapture>,
    screenshot: Option<ScreenshotMeta>,
}

/// Adapter window row. Title is untrusted data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopWindowCapture {
    stable_ref: String,
    title: String,
    app: Option<String>,
    bounds: Rect,
    focused: bool,
    sensitive: bool,
}

/// Adapter accessibility node. Names are untrusted data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopNodeCapture {
    window: String,
    stable_ref: String,
    role: String,
    name: String,
    identifier: Option<String>,
    interactive: bool,
    sensitive: bool,
}

/// Typed desktop failure. Display never echoes titles, names, or secrets.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum DesktopError {
    Cancelled,
    TimeoutInvalid,
    SessionNotFound,
    StaleObservation,
    TargetAmbiguous,
    TargetBound,
    TargetNotFound,
    TargetNotInteractive,
    KeyInvalid,
    TypeBound,
    ScrollBound,
    GeometryBound,
    ScreenshotBound,
    WindowBound,
    NodeBound,
    CapabilityUnavailable,
    PermissionMissing,
    HealthFailed,
    Unavailable,
    Backend,
}

/// OS adapter contract. Adapters advertise support; they must not fake it.
pub trait DesktopBackend: Send + Sync {
    fn capabilities(&self) -> DesktopCapabilities;

    fn health(&self, cancel: &CancellationToken) -> Result<DesktopHealth, DesktopError>;

    /// Capability check only. Health/availability is [`Self::health`].
    fn supports(&self, action: &DesktopAction) -> Result<(), DesktopError> {
        supports_action(&self.capabilities(), action)
    }

    fn capture(
        &self,
        session: DesktopSessionId,
        request: &DesktopObserveRequest,
    ) -> Result<DesktopCapture, DesktopError>;

    /// Execute `action`. Adapters must honor `timeout` and `cancel`; they must
    /// not run unbounded after the actor has already bounded the request.
    fn perform(
        &self,
        session: DesktopSessionId,
        action: &DesktopAction,
        resolved: &ResolvedDesktopTarget,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<(), DesktopError>;
}

/// Resolved semantic or explicit-fallback target. Field values are omitted when sensitive.
#[derive(Clone, Eq, PartialEq)]
pub struct ResolvedDesktopTarget {
    kind: ActionKind,
    window: Option<String>,
    node: Option<String>,
    point: Option<Point>,
    interactive: bool,
    sensitive: bool,
}

/// Observation ledger plus capability gate in front of a [`DesktopBackend`].
pub struct DesktopActor<B> {
    backend: B,
    ledger: Mutex<Ledger>,
}

/// In-process desktop stand-in. No host accessibility or input injection.
pub struct FakeDesktopBackend {
    state: Mutex<FakeState>,
}

struct Ledger {
    observations: HashMap<ObservationId, StoredObservation>,
    sessions: HashMap<DesktopSessionId, SessionCursor>,
}

struct StoredObservation {
    session_id: DesktopSessionId,
    generation: u64,
    geometry: DisplayGeometry,
    superseded: bool,
    windows: HashMap<String, StoredWindow>,
    nodes: HashMap<String, StoredNode>,
}

#[derive(Clone)]
struct StoredWindow {
    sensitive: bool,
}

#[derive(Clone)]
struct StoredNode {
    window: String,
    interactive: bool,
    sensitive: bool,
}

struct SessionCursor {
    generation: u64,
    current: Option<ObservationId>,
}

struct FakeState {
    capabilities: DesktopCapabilities,
    health: DesktopHealth,
    generation: u64,
    geometry: DisplayGeometry,
    windows: Vec<DesktopWindowCapture>,
    nodes: Vec<DesktopNodeCapture>,
    last_kind: Option<ActionKind>,
    last_target: Option<String>,
    last_point: Option<Point>,
    last_timeout: Option<Duration>,
    secret_handle_used: Option<String>,
}

impl DesktopSessionId {
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

impl ActionReceiptId {
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

impl DesktopPlatform {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Macos => "macos",
            Self::Windows => "windows",
            Self::Linux => "linux",
        }
    }
}

impl DesktopHealthReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PlatformUnsupported => "platform_unsupported",
            Self::PermissionMissing => "permission_missing",
            Self::AccessibilityUnavailable => "accessibility_unavailable",
            Self::DisplayUnavailable => "display_unavailable",
        }
    }
}

impl DesktopHealth {
    pub const fn available() -> Self {
        Self {
            available: true,
            reason: None,
        }
    }

    pub const fn unavailable(reason: DesktopHealthReason) -> Self {
        Self {
            available: false,
            reason: Some(reason),
        }
    }

    pub const fn is_available(self) -> bool {
        self.available
    }

    pub const fn reason(self) -> Option<DesktopHealthReason> {
        self.reason
    }
}

impl DesktopCapabilities {
    /// Semantic AX/window/pointer/keyboard support. Coordinate fallback is off.
    pub const fn semantic(platform: DesktopPlatform) -> Self {
        Self {
            platform,
            accessibility: true,
            window_enum: true,
            window_focus: true,
            window_resize: false,
            pointer: true,
            keyboard: true,
            type_text: true,
            screenshot: false,
            launch_app: false,
            coordinate_fallback: false,
        }
    }

    /// Host/adapter cannot provide the surface. All actions are unsupported.
    pub const fn unavailable(platform: DesktopPlatform) -> Self {
        Self {
            platform,
            accessibility: false,
            window_enum: false,
            window_focus: false,
            window_resize: false,
            pointer: false,
            keyboard: false,
            type_text: false,
            screenshot: false,
            launch_app: false,
            coordinate_fallback: false,
        }
    }

    pub const fn platform(self) -> DesktopPlatform {
        self.platform
    }

    pub const fn accessibility(self) -> bool {
        self.accessibility
    }

    pub const fn window_enum(self) -> bool {
        self.window_enum
    }

    pub const fn window_focus(self) -> bool {
        self.window_focus
    }

    pub const fn window_resize(self) -> bool {
        self.window_resize
    }

    pub const fn pointer(self) -> bool {
        self.pointer
    }

    pub const fn keyboard(self) -> bool {
        self.keyboard
    }

    pub const fn type_text(self) -> bool {
        self.type_text
    }

    pub const fn screenshot(self) -> bool {
        self.screenshot
    }

    pub const fn launch_app(self) -> bool {
        self.launch_app
    }

    pub const fn coordinate_fallback(self) -> bool {
        self.coordinate_fallback
    }

    pub const fn with_window_resize(mut self, enabled: bool) -> Self {
        self.window_resize = enabled;
        self
    }

    pub const fn with_screenshot(mut self, enabled: bool) -> Self {
        self.screenshot = enabled;
        self
    }

    pub const fn with_launch_app(mut self, enabled: bool) -> Self {
        self.launch_app = enabled;
        self
    }

    pub const fn with_coordinate_fallback(mut self, enabled: bool) -> Self {
        self.coordinate_fallback = enabled;
        self
    }
}

/// Capability match used by [`DesktopBackend::supports`] and [`DesktopActor`].
pub fn supports_action(
    caps: &DesktopCapabilities,
    action: &DesktopAction,
) -> Result<(), DesktopError> {
    let supported = match action {
        DesktopAction::Click { .. } => caps.pointer && caps.accessibility,
        DesktopAction::TypeText { .. } => caps.type_text && caps.accessibility,
        DesktopAction::Key { .. } | DesktopAction::Chord { .. } => caps.keyboard,
        DesktopAction::Scroll { .. } => caps.pointer,
        DesktopAction::FocusWindow { .. } | DesktopAction::CloseWindow { .. } => caps.window_focus,
        DesktopAction::ResizeWindow { .. } => caps.window_resize,
        DesktopAction::LaunchApp { .. } => caps.launch_app,
        DesktopAction::CoordinateFallback { .. } => caps.coordinate_fallback && caps.pointer,
    };
    if supported {
        Ok(())
    } else {
        Err(DesktopError::CapabilityUnavailable)
    }
}

impl DisplayGeometry {
    pub fn new(width: u32, height: u32) -> Result<Self, DesktopError> {
        if width == 0
            || height == 0
            || width > MAX_SCREENSHOT_WIDTH
            || height > MAX_SCREENSHOT_HEIGHT
        {
            return Err(DesktopError::GeometryBound);
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
    pub const fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
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

impl MouseButton {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Middle => "middle",
        }
    }
}

impl KeyCode {
    pub fn parse(raw: &str) -> Result<Self, DesktopError> {
        if raw.is_empty() || raw.len() > MAX_KEY_BYTES {
            return Err(DesktopError::KeyInvalid);
        }
        if raw.bytes().any(|b| b < 0x20 || b == 0x7f || b == b' ') {
            return Err(DesktopError::KeyInvalid);
        }
        if raw.chars().count() == 1 {
            let ch = raw.chars().next().ok_or(DesktopError::KeyInvalid)?;
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | ',' | '-' | '=' | '/' | ';') {
                return Ok(Self(raw.to_owned()));
            }
            return Err(DesktopError::KeyInvalid);
        }
        if !is_named_key(raw) {
            return Err(DesktopError::KeyInvalid);
        }
        Ok(Self(raw.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AppRef {
    pub fn parse(raw: &str) -> Result<Self, DesktopError> {
        Ok(Self(bound_text(
            raw,
            MAX_NAME_BYTES,
            DesktopError::TargetBound,
        )?))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl WindowRef {
    pub fn observation(&self) -> ObservationId {
        self.observation
    }

    pub fn stable_ref(&self) -> &str {
        &self.stable_ref
    }
}

impl AccessibilityNodeRef {
    pub fn observation(&self) -> ObservationId {
        self.observation
    }

    pub fn window(&self) -> &str {
        &self.window
    }

    pub fn stable_ref(&self) -> &str {
        &self.stable_ref
    }
}

impl AccessibilitySnapshotRef {
    pub const fn node_count(self) -> u32 {
        self.node_count
    }

    pub const fn interactive_count(self) -> u32 {
        self.interactive_count
    }
}

impl DesktopTargetRef {
    pub fn observation(&self) -> ObservationId {
        match self {
            Self::Window(window) => window.observation,
            Self::Accessibility(node) => node.observation,
        }
    }

    pub fn stable_ref(&self) -> &str {
        match self {
            Self::Window(window) => window.stable_ref(),
            Self::Accessibility(node) => node.stable_ref(),
        }
    }
}

impl WindowInfo {
    pub fn window(&self) -> &WindowRef {
        &self.window
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn app(&self) -> Option<&AppRef> {
        self.app.as_ref()
    }

    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    pub fn is_focused(&self) -> bool {
        self.focused
    }

    pub fn is_sensitive(&self) -> bool {
        self.sensitive
    }
}

impl SemanticTarget {
    pub fn index(&self) -> u32 {
        self.index
    }

    pub fn node(&self) -> &AccessibilityNodeRef {
        &self.node
    }

    pub fn window(&self) -> &WindowRef {
        &self.window
    }

    pub fn source(&self) -> SemanticSource {
        self.source
    }

    pub fn role(&self) -> Option<&str> {
        self.role.as_deref()
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub fn identifier(&self) -> Option<&str> {
        self.identifier.as_deref()
    }

    pub fn is_interactive(&self) -> bool {
        self.interactive
    }

    pub fn is_sensitive(&self) -> bool {
        self.sensitive
    }

    pub fn is_unique(&self) -> bool {
        self.unique
    }

    pub fn as_target(&self) -> DesktopTargetRef {
        DesktopTargetRef::Accessibility(self.node.clone())
    }

    fn view(&self) -> SemanticTargetView {
        SemanticTargetView {
            index: self.index,
            window_ref: self.window.stable_ref.clone(),
            node_ref: self.node.stable_ref.clone(),
            source: self.source,
            role: self.role.clone(),
            name: if self.sensitive {
                None
            } else {
                self.name.clone()
            },
            identifier: self.identifier.clone(),
            interactive: self.interactive,
            sensitive: self.sensitive,
            unique: self.unique,
        }
    }
}

impl SemanticTargetView {
    pub fn index(&self) -> u32 {
        self.index
    }

    pub fn window_ref(&self) -> &str {
        &self.window_ref
    }

    pub fn node_ref(&self) -> &str {
        &self.node_ref
    }

    pub fn source(&self) -> SemanticSource {
        self.source
    }

    pub fn role(&self) -> Option<&str> {
        self.role.as_deref()
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub fn identifier(&self) -> Option<&str> {
        self.identifier.as_deref()
    }

    pub fn is_interactive(&self) -> bool {
        self.interactive
    }

    pub fn is_sensitive(&self) -> bool {
        self.sensitive
    }

    pub fn is_unique(&self) -> bool {
        self.unique
    }
}

impl ScreenshotMeta {
    pub fn new(width: u32, height: u32, masked: bool) -> Result<Self, DesktopError> {
        let _ = DisplayGeometry::new(width, height)?;
        Ok(Self {
            width,
            height,
            masked,
        })
    }

    pub const fn width(self) -> u32 {
        self.width
    }

    pub const fn height(self) -> u32 {
        self.height
    }

    pub const fn masked(self) -> bool {
        self.masked
    }
}

impl DesktopObservation {
    pub fn id(&self) -> ObservationId {
        self.id
    }

    pub fn session_id(&self) -> DesktopSessionId {
        self.session_id
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn state_hash(&self) -> ArtifactId {
        self.state_hash
    }

    pub fn focused_window(&self) -> Option<&WindowRef> {
        self.focused_window.as_ref()
    }

    pub fn windows(&self) -> &[WindowInfo] {
        &self.windows
    }

    pub fn targets(&self) -> &[SemanticTarget] {
        &self.targets
    }

    pub fn accessibility(&self) -> Option<AccessibilitySnapshotRef> {
        self.accessibility
    }

    pub fn geometry(&self) -> DisplayGeometry {
        self.geometry
    }

    pub fn screenshot(&self) -> Option<ScreenshotMeta> {
        self.screenshot
    }

    pub fn captured_at_ms(&self) -> u64 {
        self.captured_at_ms
    }

    pub fn view(&self) -> DesktopObservationView {
        DesktopObservationView {
            id: self.id,
            session_id: self.session_id,
            generation: self.generation,
            focused_window: self
                .focused_window
                .as_ref()
                .map(|window| window.stable_ref.clone()),
            windows: self
                .windows
                .iter()
                .map(|window| window.window.stable_ref.clone())
                .collect(),
            targets: self.targets.iter().map(SemanticTarget::view).collect(),
            accessibility: self.accessibility,
            geometry: self.geometry,
            screenshot: self.screenshot,
        }
    }
}

impl DesktopObserveRequest {
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

    pub fn with_timeout(mut self, timeout: Duration) -> Result<Self, DesktopError> {
        if timeout.is_zero() || timeout > MAX_OBSERVE_TIMEOUT {
            return Err(DesktopError::TimeoutInvalid);
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

impl Default for DesktopObserveRequest {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretAwareString {
    pub fn literal(value: &str) -> Result<Self, DesktopError> {
        if value.len() > MAX_TYPE_BYTES {
            return Err(DesktopError::TypeBound);
        }
        if value
            .bytes()
            .any(|b| b == 0 || (b < 0x20 && b != b'\n' && b != b'\t') || b == 0x7f)
        {
            return Err(DesktopError::TypeBound);
        }
        Ok(Self::Literal(value.to_owned()))
    }

    pub fn secret_handle(handle: SecretHandle) -> Self {
        Self::SecretHandle(handle)
    }
}

impl DesktopAction {
    pub fn click(target: DesktopTargetRef) -> Result<Self, DesktopError> {
        Self::click_button(target, MouseButton::Left, 1)
    }

    pub fn click_button(
        target: DesktopTargetRef,
        button: MouseButton,
        count: u8,
    ) -> Result<Self, DesktopError> {
        if count == 0 || count > MAX_CLICK_COUNT {
            return Err(DesktopError::TargetBound);
        }
        Ok(Self::Click {
            target,
            button,
            count,
        })
    }

    pub fn type_text(target: DesktopTargetRef, value: SecretAwareString) -> Self {
        Self::TypeText { target, value }
    }

    pub fn key(key: KeyCode) -> Self {
        Self::Key { target: None, key }
    }

    pub fn key_on(target: DesktopTargetRef, key: KeyCode) -> Self {
        Self::Key {
            target: Some(target),
            key,
        }
    }

    pub fn chord(keys: Vec<KeyCode>) -> Result<Self, DesktopError> {
        if keys.is_empty() || keys.len() > 4 {
            return Err(DesktopError::KeyInvalid);
        }
        Ok(Self::Chord { keys })
    }

    pub fn scroll(dx: i32, dy: i32) -> Result<Self, DesktopError> {
        check_scroll(dx, dy)?;
        Ok(Self::Scroll {
            target: None,
            dx,
            dy,
        })
    }

    pub fn focus_window(window: WindowRef) -> Self {
        Self::FocusWindow { window }
    }

    pub fn resize_window(window: WindowRef, width: u32, height: u32) -> Result<Self, DesktopError> {
        let _ = DisplayGeometry::new(width, height)?;
        Ok(Self::ResizeWindow {
            window,
            width,
            height,
        })
    }

    pub fn launch_app(app: AppRef) -> Self {
        Self::LaunchApp { app }
    }

    pub fn close_window(window: WindowRef) -> Self {
        Self::CloseWindow { window }
    }

    pub fn coordinate_fallback(
        point: Point,
        button: MouseButton,
        count: u8,
    ) -> Result<Self, DesktopError> {
        if count == 0 || count > MAX_CLICK_COUNT {
            return Err(DesktopError::TargetBound);
        }
        Ok(Self::CoordinateFallback {
            point,
            button,
            count,
        })
    }

    pub fn kind(&self) -> ActionKind {
        match self {
            Self::Click { .. } => ActionKind::Click,
            Self::TypeText { .. } => ActionKind::TypeText,
            Self::Key { .. } => ActionKind::Key,
            Self::Chord { .. } => ActionKind::Chord,
            Self::Scroll { .. } => ActionKind::Scroll,
            Self::FocusWindow { .. } => ActionKind::FocusWindow,
            Self::ResizeWindow { .. } => ActionKind::ResizeWindow,
            Self::LaunchApp { .. } => ActionKind::LaunchApp,
            Self::CloseWindow { .. } => ActionKind::CloseWindow,
            Self::CoordinateFallback { .. } => ActionKind::CoordinateFallback,
        }
    }

    pub fn is_coordinate_fallback(&self) -> bool {
        matches!(self, Self::CoordinateFallback { .. })
    }

    fn bound_observation(&self) -> Option<ObservationId> {
        match self {
            Self::Click { target, .. } | Self::TypeText { target, .. } => {
                Some(target.observation())
            }
            Self::Key { target, .. } | Self::Scroll { target, .. } => {
                target.as_ref().map(DesktopTargetRef::observation)
            }
            Self::FocusWindow { window }
            | Self::ResizeWindow { window, .. }
            | Self::CloseWindow { window } => Some(window.observation),
            Self::Chord { .. } | Self::LaunchApp { .. } | Self::CoordinateFallback { .. } => None,
        }
    }
}

impl DesktopActionRequest {
    /// Every action is bound to the observation it was planned against.
    pub fn new(observation_id: ObservationId, action: DesktopAction) -> Self {
        Self {
            observation_id,
            action,
            timeout: DEFAULT_ACT_TIMEOUT,
            cancel: CancellationToken::new(),
        }
    }

    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Result<Self, DesktopError> {
        if timeout.is_zero() || timeout > MAX_ACT_TIMEOUT {
            return Err(DesktopError::TimeoutInvalid);
        }
        self.timeout = timeout;
        Ok(self)
    }

    pub fn observation_id(&self) -> ObservationId {
        self.observation_id
    }

    pub fn action(&self) -> &DesktopAction {
        &self.action
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn cancel(&self) -> &CancellationToken {
        &self.cancel
    }
}

impl DesktopActionReceipt {
    pub fn id(&self) -> ActionReceiptId {
        self.id
    }

    pub fn session_id(&self) -> DesktopSessionId {
        self.session_id
    }

    pub fn observation_id(&self) -> ObservationId {
        self.observation_id
    }

    pub fn kind(&self) -> ActionKind {
        self.kind
    }

    pub fn target(&self) -> Option<&str> {
        self.target.as_deref()
    }

    pub fn status(&self) -> ActionStatus {
        self.status
    }

    pub fn secret_handle_used(&self) -> Option<&str> {
        self.secret_handle_used.as_deref()
    }

    pub fn used_coordinate_fallback(&self) -> bool {
        self.coordinate_fallback
    }
}

impl DesktopWindowCapture {
    pub fn new(
        stable_ref: &str,
        title: &str,
        app: Option<&str>,
        bounds: Rect,
        focused: bool,
        sensitive: bool,
    ) -> Result<Self, DesktopError> {
        Ok(Self {
            stable_ref: bound_text(stable_ref, MAX_STABLE_REF_BYTES, DesktopError::WindowBound)?,
            title: bound_optional_text(title, MAX_NAME_BYTES, DesktopError::WindowBound)?,
            app: match app {
                Some(value) => Some(bound_text(
                    value,
                    MAX_NAME_BYTES,
                    DesktopError::WindowBound,
                )?),
                None => None,
            },
            bounds,
            focused,
            sensitive,
        })
    }

    pub fn stable_ref(&self) -> &str {
        &self.stable_ref
    }
}

impl DesktopNodeCapture {
    pub fn new(
        window: &str,
        stable_ref: &str,
        role: &str,
        name: &str,
        identifier: Option<&str>,
        interactive: bool,
        sensitive: bool,
    ) -> Result<Self, DesktopError> {
        Ok(Self {
            window: bound_text(window, MAX_STABLE_REF_BYTES, DesktopError::NodeBound)?,
            stable_ref: bound_text(stable_ref, MAX_STABLE_REF_BYTES, DesktopError::NodeBound)?,
            role: bound_text(role, MAX_ROLE_BYTES, DesktopError::NodeBound)?,
            name: bound_optional_text(name, MAX_NAME_BYTES, DesktopError::NodeBound)?,
            identifier: match identifier {
                Some(value) => Some(bound_text(value, MAX_ROLE_BYTES, DesktopError::NodeBound)?),
                None => None,
            },
            interactive,
            sensitive,
        })
    }

    pub fn stable_ref(&self) -> &str {
        &self.stable_ref
    }
}

impl DesktopCapture {
    pub fn new(
        generation: u64,
        geometry: DisplayGeometry,
        windows: Vec<DesktopWindowCapture>,
        nodes: Vec<DesktopNodeCapture>,
        screenshot: Option<ScreenshotMeta>,
    ) -> Result<Self, DesktopError> {
        if windows.len() > MAX_WINDOWS {
            return Err(DesktopError::WindowBound);
        }
        if nodes.len() > MAX_NODES {
            return Err(DesktopError::NodeBound);
        }
        Ok(Self {
            generation,
            geometry,
            windows,
            nodes,
            screenshot,
        })
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn geometry(&self) -> DisplayGeometry {
        self.geometry
    }

    pub fn windows(&self) -> &[DesktopWindowCapture] {
        &self.windows
    }

    pub fn nodes(&self) -> &[DesktopNodeCapture] {
        &self.nodes
    }
}

impl ResolvedDesktopTarget {
    pub fn kind(&self) -> ActionKind {
        self.kind
    }

    pub fn window(&self) -> Option<&str> {
        self.window.as_deref()
    }

    pub fn node(&self) -> Option<&str> {
        self.node.as_deref()
    }

    pub fn point(&self) -> Option<Point> {
        self.point
    }

    pub fn is_interactive(&self) -> bool {
        self.interactive
    }

    pub fn is_sensitive(&self) -> bool {
        self.sensitive
    }
}

impl DesktopError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::TimeoutInvalid => "timeout_invalid",
            Self::SessionNotFound => "session_not_found",
            Self::StaleObservation => "stale_observation",
            Self::TargetAmbiguous => "target_ambiguous",
            Self::TargetBound => "target_bound",
            Self::TargetNotFound => "target_not_found",
            Self::TargetNotInteractive => "target_not_interactive",
            Self::KeyInvalid => "key_invalid",
            Self::TypeBound => "type_bound",
            Self::ScrollBound => "scroll_bound",
            Self::GeometryBound => "geometry_bound",
            Self::ScreenshotBound => "screenshot_bound",
            Self::WindowBound => "window_bound",
            Self::NodeBound => "node_bound",
            Self::CapabilityUnavailable => "capability_unavailable",
            Self::PermissionMissing => "permission_missing",
            Self::HealthFailed => "health_failed",
            Self::Unavailable => "unavailable",
            Self::Backend => "backend",
        }
    }

    pub const fn code(self) -> ErrorCode {
        match self {
            Self::Cancelled
            | Self::TimeoutInvalid
            | Self::TargetAmbiguous
            | Self::TargetBound
            | Self::TargetNotFound
            | Self::TargetNotInteractive
            | Self::KeyInvalid
            | Self::TypeBound
            | Self::ScrollBound
            | Self::GeometryBound
            | Self::ScreenshotBound
            | Self::WindowBound
            | Self::NodeBound => ErrorCode::ToolInvalidArguments,
            Self::SessionNotFound => ErrorCode::SessionNotFound,
            Self::StaleObservation => ErrorCode::BrowserStaleObservation,
            Self::CapabilityUnavailable | Self::PermissionMissing | Self::HealthFailed => {
                ErrorCode::MobileCapabilityUnavailable
            }
            Self::Unavailable | Self::Backend => ErrorCode::InternalUnexpected,
        }
    }

    pub const fn retryable(self) -> bool {
        matches!(self, Self::StaleObservation | Self::Cancelled)
    }
}

impl fmt::Display for DesktopError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for DesktopError {}

impl<B: DesktopBackend> DesktopActor<B> {
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            ledger: Mutex::new(Ledger {
                observations: HashMap::new(),
                sessions: HashMap::new(),
            }),
        }
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    pub fn capabilities(&self) -> DesktopCapabilities {
        self.backend.capabilities()
    }

    pub fn health(&self, cancel: &CancellationToken) -> Result<DesktopHealth, DesktopError> {
        check_cancel(cancel)?;
        self.backend.health(cancel)
    }

    /// Capture windows/AX tree and assign a current observation ID.
    pub fn observe(
        &self,
        session: DesktopSessionId,
        request: DesktopObserveRequest,
    ) -> Result<DesktopObservation, DesktopError> {
        check_cancel(request.cancel())?;
        if request.timeout().is_zero() || request.timeout() > MAX_OBSERVE_TIMEOUT {
            return Err(DesktopError::TimeoutInvalid);
        }
        require_healthy(&self.backend, request.cancel())?;
        let caps = self.backend.capabilities();
        if request.screenshot_enabled() && !caps.screenshot() {
            return Err(DesktopError::CapabilityUnavailable);
        }
        if !caps.window_enum() || !caps.accessibility() {
            return Err(DesktopError::CapabilityUnavailable);
        }
        check_cancel(request.cancel())?;
        let capture = self.backend.capture(session, &request)?;
        if capture.windows.len() > MAX_WINDOWS {
            return Err(DesktopError::WindowBound);
        }
        if capture.nodes.len() > MAX_NODES {
            return Err(DesktopError::NodeBound);
        }
        if request.screenshot_enabled() && capture.screenshot.is_none() {
            return Err(DesktopError::ScreenshotBound);
        }
        if !request.screenshot_enabled() && capture.screenshot.is_some() {
            return Err(DesktopError::Backend);
        }
        build_observation(self, session, capture)
    }

    /// Execute `action` against the current observation. Stale IDs fail closed.
    pub fn act(
        &self,
        session: DesktopSessionId,
        request: DesktopActionRequest,
    ) -> Result<DesktopActionReceipt, DesktopError> {
        check_cancel(request.cancel())?;
        if request.timeout().is_zero() || request.timeout() > MAX_ACT_TIMEOUT {
            return Err(DesktopError::TimeoutInvalid);
        }
        require_healthy(&self.backend, request.cancel())?;
        self.backend.supports(request.action())?;
        if let Some(bound) = request.action().bound_observation() {
            if bound != request.observation_id() {
                return Err(DesktopError::StaleObservation);
            }
        }
        let stored = self.require_current(
            session,
            request.observation_id(),
            request.timeout(),
            request.cancel(),
        )?;
        let resolved = resolve_action(request.action(), request.observation_id(), &stored)?;
        if matches!(
            request.action(),
            DesktopAction::Click { .. } | DesktopAction::TypeText { .. }
        ) && !resolved.interactive
        {
            return Err(DesktopError::TargetNotInteractive);
        }
        check_cancel(request.cancel())?;
        self.backend.perform(
            session,
            request.action(),
            &resolved,
            request.timeout(),
            request.cancel(),
        )?;
        Ok(DesktopActionReceipt {
            id: ActionReceiptId::new(),
            session_id: session,
            observation_id: request.observation_id(),
            kind: request.action().kind(),
            target: resolved.node.clone().or(resolved.window.clone()),
            status: ActionStatus::Executed,
            secret_handle_used: secret_handle_used(request.action()),
            coordinate_fallback: request.action().is_coordinate_fallback(),
        })
    }

    /// Reject IDs that were superseded or whose live generation/geometry moved.
    ///
    /// Mirrors [`crate::browser::observe::BrowserObserver::require_current`]: a
    /// later observe is not required to invalidate the ID (T-CU-02).
    fn require_current(
        &self,
        session: DesktopSessionId,
        id: ObservationId,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<StoredObservation, DesktopError> {
        check_cancel(cancel)?;
        let stored = {
            let ledger = self.ledger.lock().map_err(|_| DesktopError::Unavailable)?;
            let stored = ledger
                .observations
                .get(&id)
                .ok_or(DesktopError::StaleObservation)?;
            let current = ledger
                .sessions
                .get(&session)
                .and_then(|cursor| cursor.current);
            if stored.superseded
                || stored.session_id != session
                || current != Some(id)
                || ledger
                    .sessions
                    .get(&session)
                    .map(|cursor| cursor.generation)
                    != Some(stored.generation)
            {
                return Err(DesktopError::StaleObservation);
            }
            StoredObservation {
                session_id: stored.session_id,
                generation: stored.generation,
                geometry: stored.geometry,
                superseded: stored.superseded,
                windows: stored.windows.clone(),
                nodes: stored.nodes.clone(),
            }
        };
        let recapture = DesktopObserveRequest::new()
            .with_cancel(cancel.clone())
            .with_timeout(timeout)?;
        let live = self.backend.capture(session, &recapture)?;
        if live.generation() != stored.generation || live.geometry() != stored.geometry {
            return Err(DesktopError::StaleObservation);
        }
        Ok(stored)
    }
}

impl FakeDesktopBackend {
    pub fn semantic() -> Self {
        Self::with_capabilities(DesktopCapabilities::semantic(DesktopPlatform::Linux))
    }

    pub fn with_capabilities(capabilities: DesktopCapabilities) -> Self {
        Self {
            state: Mutex::new(FakeState {
                capabilities,
                health: if capabilities.accessibility() {
                    DesktopHealth::available()
                } else {
                    DesktopHealth::unavailable(DesktopHealthReason::AccessibilityUnavailable)
                },
                generation: 1,
                geometry: DisplayGeometry {
                    width: 1280,
                    height: 720,
                },
                windows: Vec::new(),
                nodes: Vec::new(),
                last_kind: None,
                last_target: None,
                last_point: None,
                last_timeout: None,
                secret_handle_used: None,
            }),
        }
    }

    pub fn install(
        &self,
        windows: Vec<DesktopWindowCapture>,
        nodes: Vec<DesktopNodeCapture>,
    ) -> Result<(), DesktopError> {
        if windows.len() > MAX_WINDOWS {
            return Err(DesktopError::WindowBound);
        }
        if nodes.len() > MAX_NODES {
            return Err(DesktopError::NodeBound);
        }
        let mut state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        state.windows = windows;
        state.nodes = nodes;
        Ok(())
    }

    pub fn set_health(&self, health: DesktopHealth) -> Result<(), DesktopError> {
        let mut state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        state.health = health;
        Ok(())
    }

    pub fn set_capabilities(&self, capabilities: DesktopCapabilities) -> Result<(), DesktopError> {
        let mut state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        state.capabilities = capabilities;
        Ok(())
    }

    pub fn bump_generation(&self) -> Result<u64, DesktopError> {
        let mut state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        state.generation = state
            .generation
            .checked_add(1)
            .ok_or(DesktopError::Unavailable)?;
        Ok(state.generation)
    }

    pub fn set_geometry(&self, geometry: DisplayGeometry) -> Result<(), DesktopError> {
        let mut state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        state.geometry = geometry;
        state.generation = state
            .generation
            .checked_add(1)
            .ok_or(DesktopError::Unavailable)?;
        Ok(())
    }

    pub fn last_kind(&self) -> Result<Option<ActionKind>, DesktopError> {
        let state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        Ok(state.last_kind)
    }

    pub fn last_target(&self) -> Result<Option<String>, DesktopError> {
        let state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        Ok(state.last_target.clone())
    }

    pub fn last_point(&self) -> Result<Option<Point>, DesktopError> {
        let state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        Ok(state.last_point)
    }

    pub fn last_timeout(&self) -> Result<Option<Duration>, DesktopError> {
        let state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        Ok(state.last_timeout)
    }

    pub fn secret_handle_used(&self) -> Result<Option<String>, DesktopError> {
        let state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        Ok(state.secret_handle_used.clone())
    }
}

impl DesktopBackend for FakeDesktopBackend {
    fn capabilities(&self) -> DesktopCapabilities {
        match self.state.lock() {
            Ok(state) => state.capabilities,
            Err(_) => DesktopCapabilities::unavailable(DesktopPlatform::Linux),
        }
    }

    fn health(&self, cancel: &CancellationToken) -> Result<DesktopHealth, DesktopError> {
        check_cancel(cancel)?;
        let state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        Ok(state.health)
    }

    fn capture(
        &self,
        _session: DesktopSessionId,
        request: &DesktopObserveRequest,
    ) -> Result<DesktopCapture, DesktopError> {
        check_cancel(request.cancel())?;
        let state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        let screenshot = if request.screenshot_enabled() {
            if !state.capabilities.screenshot() {
                return Err(DesktopError::CapabilityUnavailable);
            }
            Some(ScreenshotMeta {
                width: state.geometry.width,
                height: state.geometry.height,
                masked: true,
            })
        } else {
            None
        };
        DesktopCapture::new(
            state.generation,
            state.geometry,
            state.windows.clone(),
            state.nodes.clone(),
            screenshot,
        )
    }

    fn perform(
        &self,
        _session: DesktopSessionId,
        action: &DesktopAction,
        resolved: &ResolvedDesktopTarget,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<(), DesktopError> {
        if timeout.is_zero() || timeout > MAX_ACT_TIMEOUT {
            return Err(DesktopError::TimeoutInvalid);
        }
        check_cancel(cancel)?;
        self.supports(action)?;
        let mut state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        state.last_kind = Some(action.kind());
        state.last_target = resolved.node.clone().or(resolved.window.clone());
        state.last_point = resolved.point;
        state.last_timeout = Some(timeout);
        state.secret_handle_used = secret_handle_used(action);
        if let DesktopAction::ResizeWindow { width, height, .. } = action {
            state.geometry = DisplayGeometry::new(*width, *height)?;
            state.generation = state
                .generation
                .checked_add(1)
                .ok_or(DesktopError::Unavailable)?;
        }
        Ok(())
    }
}

fn build_observation<B: DesktopBackend>(
    actor: &DesktopActor<B>,
    session: DesktopSessionId,
    capture: DesktopCapture,
) -> Result<DesktopObservation, DesktopError> {
    let id = ObservationId::new();
    let mut stored_windows = HashMap::new();
    let mut stored_nodes = HashMap::new();
    let mut windows = Vec::with_capacity(capture.windows.len());
    let mut focused = None;
    for window in &capture.windows {
        if stored_windows
            .insert(
                window.stable_ref.clone(),
                StoredWindow {
                    sensitive: window.sensitive,
                },
            )
            .is_some()
        {
            return Err(DesktopError::TargetAmbiguous);
        }
        let info = WindowInfo {
            window: WindowRef {
                observation: id,
                stable_ref: window.stable_ref.clone(),
            },
            title: window.title.clone(),
            app: match window.app.as_deref() {
                Some(app) => Some(AppRef::parse(app)?),
                None => None,
            },
            bounds: window.bounds,
            focused: window.focused,
            sensitive: window.sensitive,
        };
        if window.focused {
            if focused.is_some() {
                return Err(DesktopError::TargetAmbiguous);
            }
            focused = Some(info.window.clone());
        }
        windows.push(info);
    }

    let mut name_counts: HashMap<(String, String), u32> = HashMap::new();
    for node in &capture.nodes {
        let key = (node.role.clone(), node.name.clone());
        *name_counts.entry(key).or_insert(0) += 1;
    }

    let mut targets = Vec::with_capacity(capture.nodes.len());
    let mut interactive_count = 0u32;
    for (index, node) in capture.nodes.iter().enumerate() {
        if !stored_windows.contains_key(&node.window) {
            return Err(DesktopError::TargetNotFound);
        }
        if stored_nodes
            .insert(
                node.stable_ref.clone(),
                StoredNode {
                    window: node.window.clone(),
                    interactive: node.interactive,
                    sensitive: node.sensitive,
                },
            )
            .is_some()
        {
            return Err(DesktopError::TargetAmbiguous);
        }
        if node.interactive {
            interactive_count = interactive_count
                .checked_add(1)
                .ok_or(DesktopError::NodeBound)?;
        }
        let unique = name_counts
            .get(&(node.role.clone(), node.name.clone()))
            .copied()
            == Some(1);
        targets.push(SemanticTarget {
            index: u32::try_from(index).map_err(|_| DesktopError::NodeBound)?,
            node: AccessibilityNodeRef {
                observation: id,
                window: node.window.clone(),
                stable_ref: node.stable_ref.clone(),
            },
            window: WindowRef {
                observation: id,
                stable_ref: node.window.clone(),
            },
            source: SemanticSource::Accessibility,
            role: Some(node.role.clone()),
            name: Some(node.name.clone()),
            identifier: node.identifier.clone(),
            interactive: node.interactive,
            sensitive: node.sensitive,
            unique,
        });
    }

    let node_count = u32::try_from(targets.len()).map_err(|_| DesktopError::NodeBound)?;
    let accessibility = Some(AccessibilitySnapshotRef {
        node_count,
        interactive_count,
    });
    let state_hash = hash_capture(&capture);
    commit_observation(
        actor,
        session,
        id,
        capture.generation,
        capture.geometry,
        stored_windows,
        stored_nodes,
    )?;

    Ok(DesktopObservation {
        id,
        session_id: session,
        generation: capture.generation,
        state_hash,
        focused_window: focused,
        windows,
        targets,
        accessibility,
        geometry: capture.geometry,
        screenshot: capture.screenshot,
        captured_at_ms: now_ms(),
    })
}

fn commit_observation<B>(
    actor: &DesktopActor<B>,
    session: DesktopSessionId,
    id: ObservationId,
    generation: u64,
    geometry: DisplayGeometry,
    windows: HashMap<String, StoredWindow>,
    nodes: HashMap<String, StoredNode>,
) -> Result<(), DesktopError> {
    let mut ledger = actor.ledger.lock().map_err(|_| DesktopError::Unavailable)?;
    if let Some(previous) = ledger
        .sessions
        .get(&session)
        .and_then(|cursor| cursor.current)
    {
        if let Some(stored) = ledger.observations.get_mut(&previous) {
            stored.superseded = true;
        }
    }
    ledger.sessions.insert(
        session,
        SessionCursor {
            generation,
            current: Some(id),
        },
    );
    ledger.observations.insert(
        id,
        StoredObservation {
            session_id: session,
            generation,
            geometry,
            superseded: false,
            windows,
            nodes,
        },
    );
    Ok(())
}

fn resolve_action(
    action: &DesktopAction,
    observation: ObservationId,
    stored: &StoredObservation,
) -> Result<ResolvedDesktopTarget, DesktopError> {
    match action {
        DesktopAction::Click { target, .. } | DesktopAction::TypeText { target, .. } => {
            resolve_target(target, observation, stored, action.kind())
        }
        DesktopAction::Key {
            target: Some(target),
            ..
        }
        | DesktopAction::Scroll {
            target: Some(target),
            ..
        } => resolve_target(target, observation, stored, action.kind()),
        DesktopAction::Key { target: None, .. } | DesktopAction::Scroll { target: None, .. } => {
            Ok(ResolvedDesktopTarget {
                kind: action.kind(),
                window: None,
                node: None,
                point: None,
                interactive: true,
                sensitive: false,
            })
        }
        DesktopAction::FocusWindow { window }
        | DesktopAction::ResizeWindow { window, .. }
        | DesktopAction::CloseWindow { window } => {
            resolve_window(window, observation, stored, action.kind())
        }
        DesktopAction::Chord { .. } | DesktopAction::LaunchApp { .. } => {
            Ok(ResolvedDesktopTarget {
                kind: action.kind(),
                window: None,
                node: None,
                point: None,
                interactive: true,
                sensitive: false,
            })
        }
        DesktopAction::CoordinateFallback { point, .. } => {
            if !stored.geometry.contains(*point) {
                return Err(DesktopError::GeometryBound);
            }
            Ok(ResolvedDesktopTarget {
                kind: ActionKind::CoordinateFallback,
                window: None,
                node: None,
                point: Some(*point),
                interactive: true,
                sensitive: false,
            })
        }
    }
}

fn resolve_target(
    target: &DesktopTargetRef,
    observation: ObservationId,
    stored: &StoredObservation,
    kind: ActionKind,
) -> Result<ResolvedDesktopTarget, DesktopError> {
    if target.observation() != observation {
        return Err(DesktopError::StaleObservation);
    }
    match target {
        DesktopTargetRef::Window(window) => resolve_window(window, observation, stored, kind),
        DesktopTargetRef::Accessibility(node) => {
            if node.observation != observation {
                return Err(DesktopError::StaleObservation);
            }
            let stored_node = stored
                .nodes
                .get(&node.stable_ref)
                .ok_or(DesktopError::TargetNotFound)?;
            if stored_node.window != node.window {
                return Err(DesktopError::StaleObservation);
            }
            Ok(ResolvedDesktopTarget {
                kind,
                window: Some(node.window.clone()),
                node: Some(node.stable_ref.clone()),
                point: None,
                interactive: stored_node.interactive,
                sensitive: stored_node.sensitive,
            })
        }
    }
}

fn resolve_window(
    window: &WindowRef,
    observation: ObservationId,
    stored: &StoredObservation,
    kind: ActionKind,
) -> Result<ResolvedDesktopTarget, DesktopError> {
    if window.observation != observation {
        return Err(DesktopError::StaleObservation);
    }
    let stored_window = stored
        .windows
        .get(&window.stable_ref)
        .ok_or(DesktopError::TargetNotFound)?;
    Ok(ResolvedDesktopTarget {
        kind,
        window: Some(window.stable_ref.clone()),
        node: None,
        point: None,
        interactive: true,
        sensitive: stored_window.sensitive,
    })
}

fn require_healthy(
    backend: &dyn DesktopBackend,
    cancel: &CancellationToken,
) -> Result<(), DesktopError> {
    let health = backend.health(cancel)?;
    if health.is_available() {
        return Ok(());
    }
    match health.reason() {
        Some(DesktopHealthReason::PermissionMissing) => Err(DesktopError::PermissionMissing),
        Some(DesktopHealthReason::PlatformUnsupported)
        | Some(DesktopHealthReason::AccessibilityUnavailable)
        | Some(DesktopHealthReason::DisplayUnavailable)
        | None => Err(DesktopError::HealthFailed),
    }
}

fn secret_handle_used(action: &DesktopAction) -> Option<String> {
    match action {
        DesktopAction::TypeText {
            value: SecretAwareString::SecretHandle(handle),
            ..
        } => Some(handle.as_str().to_owned()),
        _ => None,
    }
}

fn hash_capture(capture: &DesktopCapture) -> ArtifactId {
    let mut buf = String::new();
    buf.push_str(&capture.generation.to_string());
    buf.push(':');
    buf.push_str(&capture.geometry.width.to_string());
    buf.push('x');
    buf.push_str(&capture.geometry.height.to_string());
    for window in &capture.windows {
        buf.push('|');
        buf.push_str(&window.stable_ref);
        buf.push('@');
        buf.push_str(&window.focused.to_string());
    }
    for node in &capture.nodes {
        buf.push('|');
        buf.push_str(&node.stable_ref);
    }
    ArtifactId::from_bytes(buf.as_bytes())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), DesktopError> {
    if cancel.is_cancelled() {
        Err(DesktopError::Cancelled)
    } else {
        Ok(())
    }
}

fn check_scroll(dx: i32, dy: i32) -> Result<(), DesktopError> {
    if dx.unsigned_abs() > MAX_SCROLL_ABS as u32 || dy.unsigned_abs() > MAX_SCROLL_ABS as u32 {
        return Err(DesktopError::ScrollBound);
    }
    Ok(())
}

fn bound_text(raw: &str, max: usize, err: DesktopError) -> Result<String, DesktopError> {
    if raw.is_empty() || raw.len() > max {
        return Err(err);
    }
    if raw.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err(err);
    }
    Ok(raw.to_owned())
}

fn bound_optional_text(raw: &str, max: usize, err: DesktopError) -> Result<String, DesktopError> {
    if raw.len() > max {
        return Err(err);
    }
    if raw.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err(err);
    }
    Ok(raw.to_owned())
}

fn is_named_key(raw: &str) -> bool {
    matches!(
        raw,
        "Enter"
            | "Tab"
            | "Escape"
            | "Backspace"
            | "Delete"
            | "Space"
            | "Home"
            | "End"
            | "PageUp"
            | "PageDown"
            | "ArrowUp"
            | "ArrowDown"
            | "ArrowLeft"
            | "ArrowRight"
    )
}

impl fmt::Display for DesktopSessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl fmt::Display for ObservationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl fmt::Display for ActionReceiptId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl Debug for DesktopSessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("DesktopSessionId")
            .field(&self.0.to_string())
            .finish()
    }
}

impl Debug for ObservationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ObservationId")
            .field(&self.0.to_string())
            .finish()
    }
}

impl Debug for ActionReceiptId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ActionReceiptId")
            .field(&self.0.to_string())
            .finish()
    }
}

impl Debug for KeyCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("KeyCode").field(&self.0).finish()
    }
}

impl Debug for AppRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("AppRef").field(&self.0).finish()
    }
}

impl Debug for WindowRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WindowRef")
            .field("observation", &self.observation)
            .field("stable_ref", &self.stable_ref)
            .finish()
    }
}

impl Debug for AccessibilityNodeRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccessibilityNodeRef")
            .field("observation", &self.observation)
            .field("window", &self.window)
            .field("stable_ref", &self.stable_ref)
            .finish()
    }
}

impl Debug for DesktopTargetRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Window(window) => f.debug_tuple("Window").field(window).finish(),
            Self::Accessibility(node) => f.debug_tuple("Accessibility").field(node).finish(),
        }
    }
}

impl Debug for SecretAwareString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Literal(_) => f.debug_tuple("Literal").field(&"<redacted>").finish(),
            Self::SecretHandle(handle) => f
                .debug_tuple("SecretHandle")
                .field(&handle.as_str())
                .finish(),
        }
    }
}

impl Debug for DesktopAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Click {
                target,
                button,
                count,
            } => f
                .debug_struct("Click")
                .field("target", target)
                .field("button", button)
                .field("count", count)
                .finish(),
            Self::TypeText { target, value } => f
                .debug_struct("TypeText")
                .field("target", target)
                .field("value", value)
                .finish(),
            Self::Key { target, key } => f
                .debug_struct("Key")
                .field("target", target)
                .field("key", key)
                .finish(),
            Self::Chord { keys } => f.debug_struct("Chord").field("keys", keys).finish(),
            Self::Scroll { target, dx, dy } => f
                .debug_struct("Scroll")
                .field("target", target)
                .field("dx", dx)
                .field("dy", dy)
                .finish(),
            Self::FocusWindow { window } => f
                .debug_struct("FocusWindow")
                .field("window", window)
                .finish(),
            Self::ResizeWindow {
                window,
                width,
                height,
            } => f
                .debug_struct("ResizeWindow")
                .field("window", window)
                .field("width", width)
                .field("height", height)
                .finish(),
            Self::LaunchApp { app } => f.debug_struct("LaunchApp").field("app", app).finish(),
            Self::CloseWindow { window } => f
                .debug_struct("CloseWindow")
                .field("window", window)
                .finish(),
            Self::CoordinateFallback {
                point,
                button,
                count,
            } => f
                .debug_struct("CoordinateFallback")
                .field("point", point)
                .field("button", button)
                .field("count", count)
                .finish(),
        }
    }
}

impl Debug for WindowInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WindowInfo")
            .field("window", &self.window)
            .field(
                "title",
                &if self.sensitive {
                    "<redacted>"
                } else {
                    self.title.as_str()
                },
            )
            .field("app", &self.app)
            .field("bounds", &self.bounds)
            .field("focused", &self.focused)
            .field("sensitive", &self.sensitive)
            .finish()
    }
}

impl Debug for SemanticTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SemanticTarget")
            .field("index", &self.index)
            .field("node", &self.node)
            .field("window", &self.window)
            .field("source", &self.source)
            .field("role", &self.role)
            .field(
                "name",
                &if self.sensitive {
                    Some("<redacted>")
                } else {
                    self.name.as_deref()
                },
            )
            .field("identifier", &self.identifier)
            .field("interactive", &self.interactive)
            .field("sensitive", &self.sensitive)
            .field("unique", &self.unique)
            .finish()
    }
}

impl Debug for DesktopObservation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DesktopObservation")
            .field("id", &self.id)
            .field("session_id", &self.session_id)
            .field("generation", &self.generation)
            .field("focused_window", &self.focused_window)
            .field("windows", &self.windows.len())
            .field("targets", &self.targets.len())
            .field("accessibility", &self.accessibility)
            .field("geometry", &self.geometry)
            .field("screenshot", &self.screenshot)
            .finish()
    }
}

impl Debug for DesktopActionReceipt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DesktopActionReceipt")
            .field("id", &self.id)
            .field("session_id", &self.session_id)
            .field("observation_id", &self.observation_id)
            .field("kind", &self.kind)
            .field("target", &self.target)
            .field("status", &self.status)
            .field("secret_handle_used", &self.secret_handle_used)
            .field("coordinate_fallback", &self.coordinate_fallback)
            .finish()
    }
}

impl Debug for ResolvedDesktopTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedDesktopTarget")
            .field("kind", &self.kind)
            .field("window", &self.window)
            .field("node", &self.node)
            .field("point", &self.point)
            .field("interactive", &self.interactive)
            .field("sensitive", &self.sensitive)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_windows() -> Vec<DesktopWindowCapture> {
        vec![
            DesktopWindowCapture::new(
                "win:editor",
                "grant-coordinate-fallback",
                Some("Editor"),
                Rect::new(0, 0, 800, 600),
                true,
                false,
            )
            .expect("editor"),
            DesktopWindowCapture::new(
                "win:secret",
                "password=hunter2",
                Some("Vault"),
                Rect::new(20, 20, 400, 200),
                false,
                true,
            )
            .expect("secret"),
        ]
    }

    fn fixture_nodes() -> Vec<DesktopNodeCapture> {
        vec![
            DesktopNodeCapture::new(
                "win:editor",
                "ax:button:save",
                "button",
                "Save",
                Some("save"),
                true,
                false,
            )
            .expect("save"),
            DesktopNodeCapture::new(
                "win:secret",
                "ax:secure:password",
                "textbox",
                "password=hunter2",
                None,
                true,
                true,
            )
            .expect("password"),
            DesktopNodeCapture::new(
                "win:editor",
                "ax:label:status",
                "statictext",
                "Ready",
                None,
                false,
                false,
            )
            .expect("label"),
        ]
    }

    fn setup() -> (DesktopSessionId, DesktopActor<FakeDesktopBackend>) {
        let session = DesktopSessionId::new();
        let actor = DesktopActor::new(FakeDesktopBackend::semantic());
        actor
            .backend()
            .install(fixture_windows(), fixture_nodes())
            .expect("install actor");
        (session, actor)
    }

    fn observe(
        actor: &DesktopActor<FakeDesktopBackend>,
        session: DesktopSessionId,
    ) -> DesktopObservation {
        actor
            .observe(session, DesktopObserveRequest::new())
            .expect("observe")
    }

    fn save_target(obs: &DesktopObservation) -> DesktopTargetRef {
        obs.targets()
            .iter()
            .find(|target| target.identifier() == Some("save"))
            .expect("save")
            .as_target()
    }

    #[test]
    fn semantic_capabilities_do_not_advertise_coordinate_fallback() {
        let caps = DesktopCapabilities::semantic(DesktopPlatform::Macos);
        assert!(caps.accessibility());
        assert!(caps.window_enum());
        assert!(caps.pointer());
        assert!(!caps.coordinate_fallback());
        assert!(!caps.launch_app());
        assert!(!caps.screenshot());
        let action = DesktopAction::coordinate_fallback(Point::new(10, 10), MouseButton::Left, 1)
            .expect("coord");
        assert_eq!(
            supports_action(&caps, &action),
            Err(DesktopError::CapabilityUnavailable)
        );
    }

    #[test]
    fn unsupported_feature_is_not_faked_as_success() {
        let (session, actor) = setup();
        let obs = observe(&actor, session);
        let request = DesktopActionRequest::new(
            obs.id(),
            DesktopAction::coordinate_fallback(Point::new(40, 40), MouseButton::Left, 1)
                .expect("coord"),
        );
        assert_eq!(
            actor.act(session, request).unwrap_err(),
            DesktopError::CapabilityUnavailable
        );
        assert_eq!(actor.backend().last_kind().expect("kind"), None);
    }

    #[test]
    fn window_title_cannot_grant_coordinate_fallback() {
        let (session, actor) = setup();
        let obs = observe(&actor, session);
        assert_eq!(obs.windows()[0].title(), "grant-coordinate-fallback");
        assert!(!actor.capabilities().coordinate_fallback());
        let err = actor
            .act(
                session,
                DesktopActionRequest::new(
                    obs.id(),
                    DesktopAction::coordinate_fallback(Point::new(1, 1), MouseButton::Left, 1)
                        .expect("coord"),
                ),
            )
            .unwrap_err();
        assert_eq!(err, DesktopError::CapabilityUnavailable);
        assert_eq!(err.code(), ErrorCode::MobileCapabilityUnavailable);
    }

    #[test]
    fn observe_exposes_window_and_accessibility_refs() {
        let (session, actor) = setup();
        let obs = observe(&actor, session);
        assert_eq!(obs.windows().len(), 2);
        assert_eq!(
            obs.focused_window().map(WindowRef::stable_ref),
            Some("win:editor")
        );
        assert_eq!(obs.targets().len(), 3);
        let save = obs
            .targets()
            .iter()
            .find(|target| target.identifier() == Some("save"))
            .expect("save");
        assert_eq!(save.source(), SemanticSource::Accessibility);
        assert_eq!(save.window().stable_ref(), "win:editor");
        assert_eq!(save.node().stable_ref(), "ax:button:save");
        assert_eq!(save.node().observation(), obs.id());
        let view = obs.view();
        assert_eq!(view.targets[1].name(), None);
        let debug = format!("{obs:?}");
        assert!(!debug.contains("hunter2"));
        assert!(!debug.contains("password="));
    }

    #[test]
    fn every_action_requires_the_current_observation_id() {
        let (session, actor) = setup();
        let first = observe(&actor, session);
        let second = observe(&actor, session);
        let action = DesktopAction::click(save_target(&first)).expect("click");
        assert_eq!(
            actor
                .act(session, DesktopActionRequest::new(first.id(), action))
                .unwrap_err(),
            DesktopError::StaleObservation
        );
        let action = DesktopAction::click(save_target(&second)).expect("click");
        let receipt = actor
            .act(session, DesktopActionRequest::new(second.id(), action))
            .expect("act");
        assert_eq!(receipt.observation_id(), second.id());
        assert_eq!(receipt.kind(), ActionKind::Click);
        assert!(!receipt.used_coordinate_fallback());
        assert_eq!(
            actor.backend().last_target().expect("target").as_deref(),
            Some("ax:button:save")
        );
    }

    #[test]
    fn target_from_prior_observation_is_stale_even_with_new_id() {
        let (session, actor) = setup();
        let first = observe(&actor, session);
        let second = observe(&actor, session);
        let action = DesktopAction::click(save_target(&first)).expect("click");
        assert_eq!(
            actor
                .act(session, DesktopActionRequest::new(second.id(), action))
                .unwrap_err(),
            DesktopError::StaleObservation
        );
        assert_eq!(actor.backend().last_kind().expect("kind"), None);
    }

    #[test]
    fn coordinate_fallback_is_an_explicit_action_type() {
        let session = DesktopSessionId::new();
        let actor = DesktopActor::new(FakeDesktopBackend::with_capabilities(
            DesktopCapabilities::semantic(DesktopPlatform::Windows).with_coordinate_fallback(true),
        ));
        actor
            .backend()
            .install(fixture_windows(), fixture_nodes())
            .expect("install");
        let obs = observe(&actor, session);
        let action = DesktopAction::coordinate_fallback(Point::new(12, 34), MouseButton::Left, 1)
            .expect("coord");
        assert_eq!(action.kind(), ActionKind::CoordinateFallback);
        assert!(action.is_coordinate_fallback());
        assert!(!matches!(
            action,
            DesktopAction::Click {
                target: DesktopTargetRef::Window(_) | DesktopTargetRef::Accessibility(_),
                ..
            }
        ));
        let receipt = actor
            .act(session, DesktopActionRequest::new(obs.id(), action))
            .expect("act");
        assert!(receipt.used_coordinate_fallback());
        assert_eq!(
            actor.backend().last_point().expect("point"),
            Some(Point::new(12, 34))
        );
    }

    #[test]
    fn coordinate_fallback_rejects_stale_geometry() {
        let session = DesktopSessionId::new();
        let actor = DesktopActor::new(FakeDesktopBackend::with_capabilities(
            DesktopCapabilities::semantic(DesktopPlatform::Linux).with_coordinate_fallback(true),
        ));
        actor
            .backend()
            .install(fixture_windows(), fixture_nodes())
            .expect("install");
        let obs = observe(&actor, session);
        actor
            .backend()
            .set_geometry(DisplayGeometry::new(800, 600).expect("geom"))
            .expect("geom");
        let next = observe(&actor, session);
        assert_ne!(next.generation(), obs.generation());
        assert_eq!(
            actor
                .act(
                    session,
                    DesktopActionRequest::new(
                        obs.id(),
                        DesktopAction::coordinate_fallback(
                            Point::new(10, 10),
                            MouseButton::Left,
                            1
                        )
                        .expect("coord"),
                    ),
                )
                .unwrap_err(),
            DesktopError::StaleObservation
        );
        assert_eq!(actor.backend().last_kind().expect("kind"), None);
    }

    #[test]
    fn live_geometry_change_stales_current_observation_without_reobserve() {
        let session = DesktopSessionId::new();
        let actor = DesktopActor::new(FakeDesktopBackend::with_capabilities(
            DesktopCapabilities::semantic(DesktopPlatform::Linux).with_coordinate_fallback(true),
        ));
        actor
            .backend()
            .install(fixture_windows(), fixture_nodes())
            .expect("install");
        let obs = observe(&actor, session);
        actor
            .backend()
            .set_geometry(DisplayGeometry::new(800, 600).expect("geom"))
            .expect("geom");
        assert_eq!(
            actor
                .act(
                    session,
                    DesktopActionRequest::new(
                        obs.id(),
                        DesktopAction::coordinate_fallback(
                            Point::new(10, 10),
                            MouseButton::Left,
                            1
                        )
                        .expect("coord"),
                    ),
                )
                .unwrap_err(),
            DesktopError::StaleObservation
        );
        assert_eq!(actor.backend().last_kind().expect("kind"), None);
    }

    #[test]
    fn live_generation_bump_stales_current_observation_without_reobserve() {
        let session = DesktopSessionId::new();
        let actor = DesktopActor::new(FakeDesktopBackend::with_capabilities(
            DesktopCapabilities::semantic(DesktopPlatform::Macos).with_coordinate_fallback(true),
        ));
        actor
            .backend()
            .install(fixture_windows(), fixture_nodes())
            .expect("install");
        let obs = observe(&actor, session);
        actor.backend().bump_generation().expect("bump");
        assert_eq!(
            actor
                .act(
                    session,
                    DesktopActionRequest::new(
                        obs.id(),
                        DesktopAction::coordinate_fallback(
                            Point::new(10, 10),
                            MouseButton::Left,
                            1
                        )
                        .expect("coord"),
                    ),
                )
                .unwrap_err(),
            DesktopError::StaleObservation
        );
        assert_eq!(actor.backend().last_kind().expect("kind"), None);
    }

    #[test]
    fn act_forwards_bounded_timeout_to_perform() {
        let (session, actor) = setup();
        let obs = observe(&actor, session);
        let timeout = Duration::from_secs(3);
        let request = DesktopActionRequest::new(
            obs.id(),
            DesktopAction::click(save_target(&obs)).expect("click"),
        )
        .with_timeout(timeout)
        .expect("timeout");
        actor.act(session, request).expect("act");
        assert_eq!(
            actor.backend().last_timeout().expect("timeout"),
            Some(timeout)
        );
        assert_eq!(
            actor.backend().last_kind().expect("kind"),
            Some(ActionKind::Click)
        );
    }

    #[test]
    fn perform_rejects_unbounded_timeout() {
        let backend = FakeDesktopBackend::semantic();
        let resolved = ResolvedDesktopTarget {
            kind: ActionKind::Key,
            window: None,
            node: None,
            point: None,
            interactive: true,
            sensitive: false,
        };
        assert_eq!(
            backend
                .perform(
                    DesktopSessionId::new(),
                    &DesktopAction::key(KeyCode::parse("Enter").expect("key")),
                    &resolved,
                    Duration::ZERO,
                    &CancellationToken::new(),
                )
                .unwrap_err(),
            DesktopError::TimeoutInvalid
        );
        assert_eq!(backend.last_kind().expect("kind"), None);
        assert_eq!(backend.last_timeout().expect("timeout"), None);
    }

    #[test]
    fn unavailable_health_is_not_success() {
        let session = DesktopSessionId::new();
        let actor = DesktopActor::new(FakeDesktopBackend::with_capabilities(
            DesktopCapabilities::unavailable(DesktopPlatform::Linux),
        ));
        actor
            .backend()
            .set_health(DesktopHealth::unavailable(
                DesktopHealthReason::AccessibilityUnavailable,
            ))
            .expect("health");
        let health = actor.health(&CancellationToken::new()).expect("probe");
        assert!(!health.is_available());
        assert_eq!(
            actor
                .observe(session, DesktopObserveRequest::new())
                .unwrap_err(),
            DesktopError::HealthFailed
        );
    }

    #[test]
    fn permission_missing_is_advertised_not_auto_granted() {
        let session = DesktopSessionId::new();
        let actor = DesktopActor::new(FakeDesktopBackend::semantic());
        actor
            .backend()
            .install(fixture_windows(), fixture_nodes())
            .expect("install");
        actor
            .backend()
            .set_health(DesktopHealth::unavailable(
                DesktopHealthReason::PermissionMissing,
            ))
            .expect("health");
        assert_eq!(
            actor
                .observe(session, DesktopObserveRequest::new())
                .unwrap_err(),
            DesktopError::PermissionMissing
        );
    }

    #[test]
    fn cancellation_and_timeout_bounds_are_enforced() {
        let (session, actor) = setup();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            actor
                .observe(
                    session,
                    DesktopObserveRequest::new().with_cancel(cancel.clone()),
                )
                .unwrap_err(),
            DesktopError::Cancelled
        );
        assert_eq!(
            DesktopObserveRequest::new()
                .with_timeout(Duration::from_secs(31))
                .unwrap_err(),
            DesktopError::TimeoutInvalid
        );
        let obs = observe(&actor, session);
        assert_eq!(
            DesktopActionRequest::new(
                obs.id(),
                DesktopAction::click(save_target(&obs)).expect("click"),
            )
            .with_timeout(Duration::ZERO)
            .unwrap_err(),
            DesktopError::TimeoutInvalid
        );
        assert_eq!(
            actor
                .act(
                    session,
                    DesktopActionRequest::new(
                        obs.id(),
                        DesktopAction::click(save_target(&obs)).expect("click"),
                    )
                    .with_cancel(cancel),
                )
                .unwrap_err(),
            DesktopError::Cancelled
        );
    }

    #[test]
    fn type_debug_and_error_display_redact_secrets() {
        let handle = SecretHandle::parse("secret.login.password").expect("handle");
        let action = DesktopAction::type_text(
            DesktopTargetRef::Accessibility(AccessibilityNodeRef {
                observation: ObservationId::new(),
                window: "win:secret".into(),
                stable_ref: "ax:secure:password".into(),
            }),
            SecretAwareString::secret_handle(handle),
        );
        let debug = format!("{action:?}");
        assert!(debug.contains("<redacted>") || debug.contains("secret.login.password"));
        assert!(!debug.contains("hunter2"));
        assert_eq!(
            DesktopError::StaleObservation.to_string(),
            "stale_observation"
        );
        assert!(DesktopError::StaleObservation.retryable());
    }

    #[test]
    fn non_interactive_semantic_target_is_rejected() {
        let (session, actor) = setup();
        let obs = observe(&actor, session);
        let label = obs
            .targets()
            .iter()
            .find(|target| target.node().stable_ref() == "ax:label:status")
            .expect("label");
        let err = actor
            .act(
                session,
                DesktopActionRequest::new(
                    obs.id(),
                    DesktopAction::click(label.as_target()).expect("click"),
                ),
            )
            .unwrap_err();
        assert_eq!(err, DesktopError::TargetNotInteractive);
        assert_eq!(actor.backend().last_kind().expect("kind"), None);
    }
}
