//! Linux AT-SPI adapter behind [`DesktopBackend`].
//!
//! Session-bus / AT-SPI availability is discovered from the existing
//! environment only. The adapter never starts `dbus-daemon`,
//! `dbus-launch`, `at-spi-bus-launcher`, or `at-spi2-registryd`
//! (T-CU-05). Unavailable AT-SPI is an explicit health failure, not a
//! silent X11/`xdotool` fallback (T-CU-02). Coordinate injection is
//! never implied by AT-SPI support. Window/app text is untrusted data
//! (T-CU-01). Secret handles stay opaque (T-CU-03).

use std::collections::HashSet;
use std::fmt::{self, Debug};
use std::sync::Mutex;
use std::time::Duration;

use capability_broker::CancellationToken;

use super::backend::{
    ActionKind, DesktopAction, DesktopBackend, DesktopCapabilities, DesktopCapture, DesktopError,
    DesktopHealth, DesktopHealthReason, DesktopNodeCapture, DesktopObserveRequest, DesktopPlatform,
    DesktopSessionId, DesktopWindowCapture, DisplayGeometry, MAX_ACT_TIMEOUT, MAX_NAME_BYTES,
    MAX_NODES, MAX_OBSERVE_TIMEOUT, MAX_ROLE_BYTES, MAX_STABLE_REF_BYTES, MAX_WINDOWS,
    ResolvedDesktopTarget, SecretAwareString, supports_action,
};

/// Prefix for observation-bound window refs derived from AT-SPI frames.
pub const WINDOW_REF_PREFIX: &str = "win:";

/// Prefix for observation-bound node refs derived from Accessible objects.
pub const NODE_REF_PREFIX: &str = "atspi:";

/// AT-SPI actions this adapter will invoke. Others are unsupported.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum LinuxAtspiAction {
    Click,
    Press,
    Toggle,
    SetText,
    Activate,
    Expand,
    Collapse,
    Scroll,
}

/// Classification of a raw AT-SPI action name.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum AtspiActionClass {
    Supported(LinuxAtspiAction),
    /// Recognized input-injection name that is never a core action path.
    Unsupported,
}

/// Result of a no-start session-bus / AT-SPI availability probe.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct LinuxAtspiProbe {
    linux: bool,
    session_bus: bool,
    atspi: bool,
    display: bool,
}

/// AT-SPI window/frame row before observation IDs are assigned.
#[derive(Clone, Eq, PartialEq)]
pub struct LinuxAtspiWindow {
    id: String,
    title: String,
    app: Option<String>,
    bounds: super::backend::Rect,
    focused: bool,
    sensitive: bool,
    actions: Vec<LinuxAtspiAction>,
}

/// Accessible object row before observation IDs are assigned.
#[derive(Clone, Eq, PartialEq)]
pub struct LinuxAtspiNode {
    id: String,
    window_id: String,
    role: String,
    name: String,
    object_id: Option<String>,
    actions: Vec<LinuxAtspiAction>,
    password: bool,
    sensitive: bool,
}

/// AT-SPI tree capture produced by a [`LinuxAtspiHost`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinuxAtspiSnapshot {
    generation: u64,
    geometry: DisplayGeometry,
    windows: Vec<LinuxAtspiWindow>,
    nodes: Vec<LinuxAtspiNode>,
}

/// AT-SPI host used by [`LinuxDesktopBackend`].
///
/// Implementations must not start a session bus or AT-SPI registry and
/// must not inject input through `xdotool` / `ydotool` / `xte`.
pub trait LinuxAtspiHost: Send + Sync {
    /// Probe session-bus / AT-SPI / display without starting services.
    fn probe(&self, cancel: &CancellationToken) -> Result<LinuxAtspiProbe, DesktopError>;

    fn snapshot(
        &self,
        session: DesktopSessionId,
        request: &DesktopObserveRequest,
    ) -> Result<LinuxAtspiSnapshot, DesktopError>;

    fn act(
        &self,
        session: DesktopSessionId,
        action: &DesktopAction,
        resolved: &ResolvedDesktopTarget,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<(), DesktopError>;
}

/// Desktop adapter that maps AT-SPI trees onto [`DesktopBackend`].
pub struct LinuxDesktopBackend<H = LiveLinuxAtspiHost> {
    host: H,
}

/// Platform / environment probe only. Does not link libatspi or start a bus.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LiveLinuxAtspiHost;

/// In-process AT-SPI stand-in. No host D-Bus or input injection.
pub struct ScriptedLinuxAtspiHost {
    state: Mutex<ScriptedState>,
}

struct ScriptedState {
    linux: bool,
    session_bus: bool,
    atspi: bool,
    display: bool,
    privileged_service_starts: u32,
    coordinate_injection_attempts: u32,
    generation: u64,
    geometry: DisplayGeometry,
    windows: Vec<LinuxAtspiWindow>,
    nodes: Vec<LinuxAtspiNode>,
    live_elements: HashSet<String>,
    last_kind: Option<ActionKind>,
    last_target: Option<String>,
    last_timeout: Option<Duration>,
    last_action: Option<LinuxAtspiAction>,
    secret_handle_used: Option<String>,
}

impl LinuxAtspiAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Click => "click",
            Self::Press => "press",
            Self::Toggle => "toggle",
            Self::SetText => "settext",
            Self::Activate => "activate",
            Self::Expand => "expand",
            Self::Collapse => "collapse",
            Self::Scroll => "scroll",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match classify_atspi_action(raw).ok()? {
            AtspiActionClass::Supported(action) => Some(action),
            AtspiActionClass::Unsupported => None,
        }
    }

    pub const fn supports(self, kind: ActionKind) -> bool {
        matches!(
            (self, kind),
            (
                Self::Click | Self::Press | Self::Toggle | Self::Activate,
                ActionKind::Click
            )
                | (Self::SetText, ActionKind::TypeText)
                | (Self::Scroll, ActionKind::Scroll)
                | (Self::Activate, ActionKind::FocusWindow)
                | (Self::Expand | Self::Collapse, ActionKind::Click)
        )
    }

    pub const fn is_interactive(self) -> bool {
        matches!(
            self,
            Self::Click
                | Self::Press
                | Self::Toggle
                | Self::SetText
                | Self::Activate
                | Self::Expand
                | Self::Collapse
                | Self::Scroll
        )
    }
}

/// Map a raw AT-SPI action name. Coordinate/X11 injectors are unsupported.
pub fn classify_atspi_action(raw: &str) -> Result<AtspiActionClass, DesktopError> {
    let key = normalize_atspi_token(raw, DesktopError::NodeBound)?;
    match key.as_str() {
        "click" | "clickaction" | "doaction_click" => {
            Ok(AtspiActionClass::Supported(LinuxAtspiAction::Click))
        }
        "press" | "pressaction" => Ok(AtspiActionClass::Supported(LinuxAtspiAction::Press)),
        "toggle" | "toggleaction" => Ok(AtspiActionClass::Supported(LinuxAtspiAction::Toggle)),
        "settext" | "set_text" | "settextcontents" | "setvalue" => {
            Ok(AtspiActionClass::Supported(LinuxAtspiAction::SetText))
        }
        "activate" | "raise" | "grabfocus" => {
            Ok(AtspiActionClass::Supported(LinuxAtspiAction::Activate))
        }
        "expand" => Ok(AtspiActionClass::Supported(LinuxAtspiAction::Expand)),
        "collapse" => Ok(AtspiActionClass::Supported(LinuxAtspiAction::Collapse)),
        "scroll" | "scrollto" | "scrollinto" => {
            Ok(AtspiActionClass::Supported(LinuxAtspiAction::Scroll))
        }
        "xdotool"
        | "ydotool"
        | "xte"
        | "xwarp"
        | "xdotool_mousemove"
        | "evemu"
        | "uinput"
        | "wtype"
        | "ydotool_click"
        | "synaptics"
        | "xtest"
        | "x11_send_event"
        | "wayland_virtual_pointer" => Ok(AtspiActionClass::Unsupported),
        _ => Err(DesktopError::NodeBound),
    }
}

/// Keep supported actions; drop unsupported ones. Invalid tokens fail closed.
pub fn map_atspi_actions(raw: &[&str]) -> Result<Vec<LinuxAtspiAction>, DesktopError> {
    if raw.len() > 8 {
        return Err(DesktopError::NodeBound);
    }
    let mut out = Vec::new();
    for name in raw {
        match classify_atspi_action(name)? {
            AtspiActionClass::Supported(action) if !out.contains(&action) => out.push(action),
            AtspiActionClass::Supported(_) | AtspiActionClass::Unsupported => {}
        }
    }
    Ok(out)
}

/// Whether any advertised AT-SPI action can perform `kind`.
pub fn atspi_action_supports_action(actions: &[LinuxAtspiAction], kind: ActionKind) -> bool {
    match kind {
        ActionKind::Key | ActionKind::Chord => true,
        ActionKind::LaunchApp | ActionKind::CoordinateFallback | ActionKind::ResizeWindow => false,
        ActionKind::CloseWindow => false,
        _ => actions.iter().any(|action| action.supports(kind)),
    }
}

impl LinuxAtspiProbe {
    pub const fn new(linux: bool, session_bus: bool, atspi: bool, display: bool) -> Self {
        Self {
            linux,
            session_bus,
            atspi,
            display,
        }
    }

    pub const fn linux(self) -> bool {
        self.linux
    }

    pub const fn session_bus(self) -> bool {
        self.session_bus
    }

    pub const fn atspi(self) -> bool {
        self.atspi
    }

    pub const fn display(self) -> bool {
        self.display
    }
}

impl LinuxAtspiWindow {
    pub fn new(
        id: &str,
        title: &str,
        app: Option<&str>,
        bounds: super::backend::Rect,
        focused: bool,
        sensitive: bool,
        actions: &[LinuxAtspiAction],
    ) -> Result<Self, DesktopError> {
        if actions.len() > 8 {
            return Err(DesktopError::WindowBound);
        }
        Ok(Self {
            id: bound_token(id, MAX_STABLE_REF_BYTES, DesktopError::WindowBound)?,
            title: bound_optional(title, MAX_NAME_BYTES, DesktopError::WindowBound)?,
            app: match app {
                Some(value) => Some(bound_token(
                    value,
                    MAX_NAME_BYTES,
                    DesktopError::WindowBound,
                )?),
                None => None,
            },
            bounds,
            focused,
            sensitive,
            actions: actions.to_vec(),
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn app(&self) -> Option<&str> {
        self.app.as_deref()
    }

    pub fn bounds(&self) -> super::backend::Rect {
        self.bounds
    }

    pub fn is_focused(&self) -> bool {
        self.focused
    }

    pub fn is_sensitive(&self) -> bool {
        self.sensitive
    }

    pub fn actions(&self) -> &[LinuxAtspiAction] {
        &self.actions
    }
}

impl LinuxAtspiNode {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: &str,
        window_id: &str,
        role: &str,
        name: &str,
        object_id: Option<&str>,
        actions: &[LinuxAtspiAction],
        password: bool,
        sensitive: bool,
    ) -> Result<Self, DesktopError> {
        if actions.len() > 8 {
            return Err(DesktopError::NodeBound);
        }
        Ok(Self {
            id: bound_token(id, MAX_STABLE_REF_BYTES, DesktopError::NodeBound)?,
            window_id: bound_token(window_id, MAX_STABLE_REF_BYTES, DesktopError::NodeBound)?,
            role: bound_token(role, MAX_ROLE_BYTES + 24, DesktopError::NodeBound)?,
            name: bound_optional(name, MAX_NAME_BYTES, DesktopError::NodeBound)?,
            object_id: match object_id {
                Some(value) => Some(bound_token(value, MAX_ROLE_BYTES, DesktopError::NodeBound)?),
                None => None,
            },
            actions: actions.to_vec(),
            password,
            sensitive,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn window_id(&self) -> &str {
        &self.window_id
    }

    pub fn role(&self) -> &str {
        &self.role
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn object_id(&self) -> Option<&str> {
        self.object_id.as_deref()
    }

    pub fn actions(&self) -> &[LinuxAtspiAction] {
        &self.actions
    }

    pub fn is_password(&self) -> bool {
        self.password
    }

    pub fn is_sensitive(&self) -> bool {
        self.sensitive
    }
}

impl LinuxAtspiSnapshot {
    pub fn new(
        generation: u64,
        geometry: DisplayGeometry,
        windows: Vec<LinuxAtspiWindow>,
        nodes: Vec<LinuxAtspiNode>,
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
        })
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn geometry(&self) -> DisplayGeometry {
        self.geometry
    }

    pub fn windows(&self) -> &[LinuxAtspiWindow] {
        &self.windows
    }

    pub fn nodes(&self) -> &[LinuxAtspiNode] {
        &self.nodes
    }
}

impl LinuxDesktopBackend<LiveLinuxAtspiHost> {
    pub fn live() -> Self {
        Self {
            host: LiveLinuxAtspiHost,
        }
    }
}

impl LinuxDesktopBackend<ScriptedLinuxAtspiHost> {
    pub fn scripted(host: ScriptedLinuxAtspiHost) -> Self {
        Self { host }
    }
}

impl<H: LinuxAtspiHost> LinuxDesktopBackend<H> {
    pub fn new(host: H) -> Self {
        Self { host }
    }

    pub fn host(&self) -> &H {
        &self.host
    }

    fn require_ready(&self, cancel: &CancellationToken) -> Result<(), DesktopError> {
        let health = self.health(cancel)?;
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
}

impl<H: LinuxAtspiHost> DesktopBackend for LinuxDesktopBackend<H> {
    fn capabilities(&self) -> DesktopCapabilities {
        match self.host.probe(&CancellationToken::new()) {
            Ok(probe) if probe.linux() => DesktopCapabilities::semantic(DesktopPlatform::Linux),
            _ => DesktopCapabilities::unavailable(DesktopPlatform::Linux),
        }
    }

    fn health(&self, cancel: &CancellationToken) -> Result<DesktopHealth, DesktopError> {
        check_cancel(cancel)?;
        let probe = self.host.probe(cancel)?;
        if !probe.linux() {
            return Ok(DesktopHealth::unavailable(
                DesktopHealthReason::PlatformUnsupported,
            ));
        }
        if !probe.display() {
            return Ok(DesktopHealth::unavailable(
                DesktopHealthReason::DisplayUnavailable,
            ));
        }
        if !probe.session_bus() || !probe.atspi() {
            return Ok(DesktopHealth::unavailable(
                DesktopHealthReason::AccessibilityUnavailable,
            ));
        }
        Ok(DesktopHealth::available())
    }

    fn capture(
        &self,
        session: DesktopSessionId,
        request: &DesktopObserveRequest,
    ) -> Result<DesktopCapture, DesktopError> {
        check_cancel(request.cancel())?;
        if request.timeout().is_zero() || request.timeout() > MAX_OBSERVE_TIMEOUT {
            return Err(DesktopError::TimeoutInvalid);
        }
        self.require_ready(request.cancel())?;
        if request.screenshot_enabled() {
            return Err(DesktopError::CapabilityUnavailable);
        }
        let snapshot = self.host.snapshot(session, request)?;
        map_snapshot(snapshot)
    }

    fn perform(
        &self,
        session: DesktopSessionId,
        action: &DesktopAction,
        resolved: &ResolvedDesktopTarget,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<(), DesktopError> {
        if timeout.is_zero() || timeout > MAX_ACT_TIMEOUT {
            return Err(DesktopError::TimeoutInvalid);
        }
        check_cancel(cancel)?;
        self.require_ready(cancel)?;
        supports_action(&self.capabilities(), action)?;
        match self.host.act(session, action, resolved, timeout, cancel) {
            Err(DesktopError::TargetNotFound) => Err(DesktopError::StaleObservation),
            other => other,
        }
    }
}

impl LiveLinuxAtspiHost {
    pub const fn new() -> Self {
        Self
    }
}

impl LinuxAtspiHost for LiveLinuxAtspiHost {
    fn probe(&self, cancel: &CancellationToken) -> Result<LinuxAtspiProbe, DesktopError> {
        check_cancel(cancel)?;
        // Discover only. Never spawn dbus-daemon / dbus-launch /
        // at-spi-bus-launcher / at-spi2-registryd. This crate does not
        // link libatspi (`forbid(unsafe_code)`), so the registry cannot
        // be confirmed and is reported unavailable rather than assumed
        // present.
        Ok(LinuxAtspiProbe::new(
            cfg!(target_os = "linux"),
            cfg!(target_os = "linux") && session_bus_address_present(),
            false,
            cfg!(target_os = "linux") && display_present(),
        ))
    }

    fn snapshot(
        &self,
        _session: DesktopSessionId,
        request: &DesktopObserveRequest,
    ) -> Result<LinuxAtspiSnapshot, DesktopError> {
        check_cancel(request.cancel())?;
        Err(DesktopError::HealthFailed)
    }

    fn act(
        &self,
        _session: DesktopSessionId,
        _action: &DesktopAction,
        _resolved: &ResolvedDesktopTarget,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<(), DesktopError> {
        if timeout.is_zero() || timeout > MAX_ACT_TIMEOUT {
            return Err(DesktopError::TimeoutInvalid);
        }
        check_cancel(cancel)?;
        Err(DesktopError::HealthFailed)
    }
}

impl ScriptedLinuxAtspiHost {
    pub fn granted() -> Self {
        Self::with_probe(true, true, true, true)
    }

    pub fn atspi_unavailable() -> Self {
        Self::with_probe(true, true, false, true)
    }

    pub fn session_bus_missing() -> Self {
        Self::with_probe(true, false, false, true)
    }

    pub fn display_unavailable() -> Self {
        Self::with_probe(true, true, true, false)
    }

    pub fn not_linux() -> Self {
        Self::with_probe(false, false, false, false)
    }

    fn with_probe(linux: bool, session_bus: bool, atspi: bool, display: bool) -> Self {
        Self {
            state: Mutex::new(ScriptedState {
                linux,
                session_bus,
                atspi,
                display,
                privileged_service_starts: 0,
                coordinate_injection_attempts: 0,
                generation: 1,
                geometry: default_geometry(),
                windows: Vec::new(),
                nodes: Vec::new(),
                live_elements: HashSet::new(),
                last_kind: None,
                last_target: None,
                last_timeout: None,
                last_action: None,
                secret_handle_used: None,
            }),
        }
    }

    pub fn install(
        &self,
        windows: Vec<LinuxAtspiWindow>,
        nodes: Vec<LinuxAtspiNode>,
    ) -> Result<(), DesktopError> {
        if windows.len() > MAX_WINDOWS {
            return Err(DesktopError::WindowBound);
        }
        if nodes.len() > MAX_NODES {
            return Err(DesktopError::NodeBound);
        }
        let mut state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        let mut live = HashSet::new();
        for window in &windows {
            live.insert(window_stable_ref(&window.id)?);
        }
        for node in &nodes {
            live.insert(node_stable_ref(&node.id)?);
        }
        state.windows = windows;
        state.nodes = nodes;
        state.live_elements = live;
        Ok(())
    }

    pub fn set_atspi(&self, atspi: bool) -> Result<(), DesktopError> {
        let mut state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        state.atspi = atspi;
        Ok(())
    }

    pub fn set_session_bus(&self, session_bus: bool) -> Result<(), DesktopError> {
        let mut state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        state.session_bus = session_bus;
        Ok(())
    }

    pub fn set_display(&self, display: bool) -> Result<(), DesktopError> {
        let mut state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        state.display = display;
        Ok(())
    }

    /// Mark an AT-SPI window or node invalid without starting a bus.
    pub fn invalidate_element(&self, id: &str) -> Result<(), DesktopError> {
        let mut state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        let window_ref = window_stable_ref(id).unwrap_or_else(|_| id.to_owned());
        let node_ref = node_stable_ref(id).unwrap_or_else(|_| id.to_owned());
        state.live_elements.remove(id);
        state.live_elements.remove(&window_ref);
        state.live_elements.remove(&node_ref);
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

    /// dbus/AT-SPI launcher count. Adapter paths must leave this at zero.
    pub fn privileged_service_starts(&self) -> Result<u32, DesktopError> {
        let state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        Ok(state.privileged_service_starts)
    }

    /// xdotool/X11 injection count. Adapter paths must leave this at zero.
    pub fn coordinate_injection_attempts(&self) -> Result<u32, DesktopError> {
        let state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        Ok(state.coordinate_injection_attempts)
    }

    pub fn last_kind(&self) -> Result<Option<ActionKind>, DesktopError> {
        let state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        Ok(state.last_kind)
    }

    pub fn last_target(&self) -> Result<Option<String>, DesktopError> {
        let state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        Ok(state.last_target.clone())
    }

    pub fn last_timeout(&self) -> Result<Option<Duration>, DesktopError> {
        let state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        Ok(state.last_timeout)
    }

    pub fn last_action(&self) -> Result<Option<LinuxAtspiAction>, DesktopError> {
        let state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        Ok(state.last_action)
    }

    pub fn secret_handle_used(&self) -> Result<Option<String>, DesktopError> {
        let state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        Ok(state.secret_handle_used.clone())
    }
}

impl LinuxAtspiHost for ScriptedLinuxAtspiHost {
    fn probe(&self, cancel: &CancellationToken) -> Result<LinuxAtspiProbe, DesktopError> {
        check_cancel(cancel)?;
        let state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        Ok(LinuxAtspiProbe::new(
            state.linux,
            state.session_bus,
            state.atspi,
            state.display,
        ))
    }

    fn snapshot(
        &self,
        _session: DesktopSessionId,
        request: &DesktopObserveRequest,
    ) -> Result<LinuxAtspiSnapshot, DesktopError> {
        check_cancel(request.cancel())?;
        let state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        if !state.linux || !state.session_bus || !state.atspi {
            return Err(DesktopError::HealthFailed);
        }
        LinuxAtspiSnapshot::new(
            state.generation,
            state.geometry,
            state.windows.clone(),
            state.nodes.clone(),
        )
    }

    fn act(
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
        let mut state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        if !state.linux || !state.session_bus || !state.atspi {
            return Err(DesktopError::HealthFailed);
        }
        if let Some(target) = resolved.node().or(resolved.window())
            && !state.live_elements.contains(target) {
                return Err(DesktopError::StaleObservation);
            }
        let actions = actions_for(&state, resolved)?.to_vec();
        if !atspi_action_supports_action(&actions, action.kind()) {
            // No AT-SPI click/settext/activate match and no xdotool path.
            return Err(DesktopError::CapabilityUnavailable);
        }
        state.last_kind = Some(action.kind());
        state.last_target = resolved
            .node()
            .map(str::to_owned)
            .or_else(|| resolved.window().map(str::to_owned));
        state.last_timeout = Some(timeout);
        state.last_action = actions
            .iter()
            .copied()
            .find(|item| item.supports(action.kind()));
        state.secret_handle_used = secret_handle_used(action);
        Ok(())
    }
}

/// Map an AT-SPI role (`ATSPI_ROLE_PUSH_BUTTON`, `push button`) onto the
/// desktop semantic role (`button`). Localized names are untrusted data
/// and are not accepted as a role (T-CU-01).
pub fn normalize_atspi_role(raw: &str) -> Result<String, DesktopError> {
    let key = normalize_atspi_token(raw, DesktopError::NodeBound)?;
    let mapped = match key.as_str() {
        "push_button" | "pushbutton" | "button" | "toggle_button" | "togglebutton" | "43"
        | "63" => "button",
        "check_box" | "checkbox" | "7" => "checkbox",
        "radio_button" | "radiobutton" | "44" => "radiobutton",
        "password_text" | "passwordtext" | "entry" | "text" | "40" | "62" => "textbox",
        "label" | "static" | "29" => "statictext",
        "link" => "link",
        "image" | "icon" | "27" | "26" => "image",
        "combo_box" | "combobox" | "11" => "combobox",
        "menu_item" | "menuitem" | "check_menu_item" | "radio_menu_item" | "tearoff_menu_item"
        | "35" => "menuitem",
        "menu" | "menu_bar" | "menubar" | "popup_menu" | "33" | "34" | "41" => "menu",
        "slider" | "dial" | "51" | "15" => "slider",
        "spin_button" | "spinbutton" | "52" => "spinbutton",
        "page_tab" | "pagetab" | "page_tab_list" | "37" | "38" => "tab",
        "scroll_bar" | "scrollbar" | "48" => "scrollbar",
        "scroll_pane" | "scrollpane" | "49" => "scrollarea",
        "window" | "frame" | "desktop_frame" | "70" | "23" | "14" => "window",
        "dialog" | "alert" | "file_chooser" | "color_chooser" | "font_chooser" | "16" | "2" => {
            "dialog"
        }
        "panel" | "filler" | "layered_pane" | "root_pane" | "grouping" | "39" | "20" => "group",
        "tool_bar" | "toolbar" | "64" => "toolbar",
        "tree" | "tree_table" | "66" | "67" => "tree",
        "tree_item" | "treeitem" => "treeitem",
        "table" | "55" => "table",
        "table_row" | "tablerow" | "58" => "row",
        "table_cell" | "tablecell" | "56" => "cell",
        "column_header" | "table_column_header" | "10" | "57" => "column",
        "list" | "list_box" | "31" => "list",
        "list_item" | "listitem" | "32" => "listitem",
        "heading" | "header" => "heading",
        "document_frame" | "document_text" | "document_web" | "html_container" => "document",
        "progress_bar" | "progressbar" | "level_bar" | "42" => "progressbar",
        "status_bar" | "statusbar" | "54" => "statusbar",
        "tool_tip" | "tooltip" | "65" => "tooltip",
        "separator" | "50" => "separator",
        "calendar" | "5" => "calendar",
        "application" => "application",
        "terminal" | "61" => "terminal",
        "unknown" | "custom" => "custom",
        other if is_role_token(other) => other,
        _ => return Err(DesktopError::NodeBound),
    };
    if mapped.len() > MAX_ROLE_BYTES {
        return Err(DesktopError::NodeBound);
    }
    Ok(mapped.to_owned())
}

/// Password-text AT-SPI roles are sensitive regardless of untrusted name text.
pub fn atspi_role_is_password(raw: &str) -> bool {
    match normalize_atspi_token(raw, DesktopError::NodeBound) {
        Ok(key) => matches!(key.as_str(), "password_text" | "passwordtext" | "40"),
        Err(_) => false,
    }
}

fn map_snapshot(snapshot: LinuxAtspiSnapshot) -> Result<DesktopCapture, DesktopError> {
    let mut windows = Vec::with_capacity(snapshot.windows.len());
    for window in &snapshot.windows {
        windows.push(map_window(window)?);
    }
    let mut nodes = Vec::with_capacity(snapshot.nodes.len());
    for node in &snapshot.nodes {
        nodes.push(map_node(node)?);
    }
    DesktopCapture::new(snapshot.generation, snapshot.geometry, windows, nodes, None)
}

fn map_window(window: &LinuxAtspiWindow) -> Result<DesktopWindowCapture, DesktopError> {
    let sensitive = window.sensitive || is_system_auth_app(window.app.as_deref());
    DesktopWindowCapture::new(
        &window_stable_ref(&window.id)?,
        &window.title,
        window.app.as_deref(),
        window.bounds,
        window.focused,
        sensitive,
    )
}

fn map_node(node: &LinuxAtspiNode) -> Result<DesktopNodeCapture, DesktopError> {
    let role = normalize_atspi_role(&node.role)?;
    let sensitive = node.sensitive || node.password || atspi_role_is_password(&node.role);
    let interactive = node_is_interactive(&role, &node.actions);
    DesktopNodeCapture::new(
        &window_stable_ref(&node.window_id)?,
        &node_stable_ref(&node.id)?,
        &role,
        &node.name,
        node.object_id.as_deref(),
        interactive,
        sensitive,
    )
}

fn node_is_interactive(role: &str, actions: &[LinuxAtspiAction]) -> bool {
    if actions.iter().any(|action| action.is_interactive()) {
        return true;
    }
    matches!(
        role,
        "button"
            | "checkbox"
            | "radiobutton"
            | "textbox"
            | "link"
            | "combobox"
            | "menuitem"
            | "slider"
            | "spinbutton"
            | "tab"
            | "scrollbar"
            | "listitem"
            | "treeitem"
    )
}

fn is_system_auth_app(app: Option<&str>) -> bool {
    match app {
        Some(name) => {
            let key = name.to_ascii_lowercase();
            matches!(
                key.as_str(),
                "polkit"
                    | "polkit-gnome-authentication-agent-1"
                    | "org.freedesktop.policykit.authenticationagent"
                    | "pkexec"
                    | "lxpolkit"
                    | "mate-polkit"
                    | "gnome-keyring"
                    | "gnome-keyring-prompt"
                    | "gcr-prompter"
                    | "pinentry"
                    | "pinentry-gnome3"
                    | "ssh-askpass"
                    | "kdesu"
                    | "gksu"
                    | "gdm"
                    | "sddm"
                    | "lightdm"
            )
        }
        None => false,
    }
}

fn actions_for<'a>(
    state: &'a ScriptedState,
    resolved: &ResolvedDesktopTarget,
) -> Result<&'a [LinuxAtspiAction], DesktopError> {
    if let Some(node_ref) = resolved.node() {
        for node in &state.nodes {
            if node_stable_ref(&node.id)? == node_ref {
                return Ok(&node.actions);
            }
        }
        return Err(DesktopError::StaleObservation);
    }
    if let Some(window_ref) = resolved.window() {
        for window in &state.windows {
            if window_stable_ref(&window.id)? == window_ref {
                return Ok(&window.actions);
            }
        }
        return Err(DesktopError::StaleObservation);
    }
    Ok(&[])
}

fn window_stable_ref(id: &str) -> Result<String, DesktopError> {
    prefixed_ref(WINDOW_REF_PREFIX, id, DesktopError::WindowBound)
}

fn node_stable_ref(id: &str) -> Result<String, DesktopError> {
    prefixed_ref(NODE_REF_PREFIX, id, DesktopError::NodeBound)
}

fn prefixed_ref(prefix: &str, id: &str, err: DesktopError) -> Result<String, DesktopError> {
    let body = if let Some(rest) = id.strip_prefix(prefix) {
        rest
    } else {
        id
    };
    let _ = bound_token(body, MAX_STABLE_REF_BYTES.saturating_sub(prefix.len()), err)?;
    let mapped = format!("{prefix}{body}");
    if mapped.len() > MAX_STABLE_REF_BYTES {
        return Err(err);
    }
    Ok(mapped)
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

fn bound_token(raw: &str, max: usize, err: DesktopError) -> Result<String, DesktopError> {
    if raw.is_empty() || raw.len() > max {
        return Err(err);
    }
    if raw.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err(err);
    }
    Ok(raw.to_owned())
}

fn bound_optional(raw: &str, max: usize, err: DesktopError) -> Result<String, DesktopError> {
    if raw.len() > max {
        return Err(err);
    }
    if raw.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err(err);
    }
    Ok(raw.to_owned())
}

fn normalize_atspi_token(raw: &str, err: DesktopError) -> Result<String, DesktopError> {
    if raw.is_empty() || raw.len() > MAX_ROLE_BYTES + 24 {
        return Err(err);
    }
    if raw.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err(err);
    }
    let stripped = raw
        .strip_prefix("ATSPI_ROLE_")
        .or_else(|| raw.strip_prefix("AtspiRole."))
        .or_else(|| raw.strip_prefix("ROLE_"))
        .or_else(|| raw.strip_prefix("role-"))
        .unwrap_or(raw);
    let mut out = String::with_capacity(stripped.len());
    for ch in stripped.chars() {
        match ch {
            ' ' | '-' => out.push('_'),
            other => out.push(other.to_ascii_lowercase()),
        }
    }
    Ok(out)
}

fn is_role_token(raw: &str) -> bool {
    !raw.is_empty()
        && raw
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn session_bus_address_present() -> bool {
    env_nonempty("DBUS_SESSION_BUS_ADDRESS")
}

fn display_present() -> bool {
    env_nonempty("WAYLAND_DISPLAY") || env_nonempty("DISPLAY")
}

fn env_nonempty(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), DesktopError> {
    if cancel.is_cancelled() {
        Err(DesktopError::Cancelled)
    } else {
        Ok(())
    }
}

fn default_geometry() -> DisplayGeometry {
    match DisplayGeometry::new(1280, 720) {
        Ok(geometry) => geometry,
        // 1280x720 is non-zero and below MAX_SCREENSHOT_*.
        Err(_) => unreachable!("1280x720 is a valid DisplayGeometry"),
    }
}

impl Debug for LinuxAtspiWindow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LinuxAtspiWindow")
            .field("id", &self.id)
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
            .field("actions", &self.actions)
            .finish()
    }
}

impl Debug for LinuxAtspiNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LinuxAtspiNode")
            .field("id", &self.id)
            .field("window_id", &self.window_id)
            .field("role", &self.role)
            .field(
                "name",
                &if self.sensitive || self.password {
                    "<redacted>"
                } else {
                    self.name.as_str()
                },
            )
            .field("object_id", &self.object_id)
            .field("actions", &self.actions)
            .field("password", &self.password)
            .field("sensitive", &self.sensitive)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop::backend::{
        DesktopActionRequest, DesktopActor, DesktopObservation, DesktopTargetRef, MouseButton,
        Point, Rect, WindowRef,
    };
    use capability_broker::SecretHandle;

    fn fixture_windows() -> Vec<LinuxAtspiWindow> {
        vec![
            LinuxAtspiWindow::new(
                "editor",
                "grant-atspi-please",
                Some("Editor"),
                Rect::new(0, 0, 800, 600),
                true,
                false,
                &[LinuxAtspiAction::Activate],
            )
            .expect("editor"),
            LinuxAtspiWindow::new(
                "secret",
                "password=hunter2",
                Some("Vault"),
                Rect::new(20, 20, 400, 200),
                false,
                true,
                &[LinuxAtspiAction::Activate],
            )
            .expect("secret"),
        ]
    }

    fn fixture_nodes() -> Vec<LinuxAtspiNode> {
        vec![
            LinuxAtspiNode::new(
                "button:save",
                "editor",
                "ATSPI_ROLE_PUSH_BUTTON",
                "Save",
                Some("save"),
                &[LinuxAtspiAction::Click],
                false,
                false,
            )
            .expect("save"),
            LinuxAtspiNode::new(
                "edit:password",
                "secret",
                "password text",
                "password=hunter2",
                None,
                &[LinuxAtspiAction::SetText],
                true,
                false,
            )
            .expect("password"),
            LinuxAtspiNode::new(
                "label:status",
                "editor",
                "ATSPI_ROLE_LABEL",
                "Ready",
                None,
                &[],
                false,
                false,
            )
            .expect("label"),
            LinuxAtspiNode::new(
                "legacy:ok",
                "editor",
                "push button",
                "OK",
                Some("xdotool-ok"),
                &map_atspi_actions(&["xdotool"]).expect("xdotool"),
                false,
                false,
            )
            .expect("legacy"),
        ]
    }

    fn setup() -> (
        DesktopSessionId,
        DesktopActor<LinuxDesktopBackend<ScriptedLinuxAtspiHost>>,
    ) {
        let host = ScriptedLinuxAtspiHost::granted();
        host.install(fixture_windows(), fixture_nodes())
            .expect("install");
        let session = DesktopSessionId::new();
        let actor = DesktopActor::new(LinuxDesktopBackend::scripted(host));
        (session, actor)
    }

    fn observe(
        actor: &DesktopActor<LinuxDesktopBackend<ScriptedLinuxAtspiHost>>,
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

    fn assert_no_privileged_or_coordinate(host: &ScriptedLinuxAtspiHost) {
        assert_eq!(host.privileged_service_starts().expect("bus"), 0);
        assert_eq!(host.coordinate_injection_attempts().expect("coord"), 0);
    }

    #[test]
    fn maps_atspi_elements_to_stable_per_observation_refs() {
        let (session, actor) = setup();
        let obs = observe(&actor, session);
        assert_eq!(obs.windows().len(), 2);
        assert_eq!(
            obs.focused_window().map(WindowRef::stable_ref),
            Some("win:editor")
        );
        let save = obs
            .targets()
            .iter()
            .find(|target| target.identifier() == Some("save"))
            .expect("save");
        assert_eq!(save.node().stable_ref(), "atspi:button:save");
        assert_eq!(save.window().stable_ref(), "win:editor");
        assert_eq!(save.node().observation(), obs.id());
        assert_eq!(save.role(), Some("button"));
        assert!(save.is_interactive());
        let password = obs
            .targets()
            .iter()
            .find(|target| target.node().stable_ref() == "atspi:edit:password")
            .expect("password");
        assert_eq!(password.role(), Some("textbox"));
        assert!(password.is_sensitive());
        assert!(password.name().is_some());
        let debug = format!("{obs:?}");
        assert!(!debug.contains("hunter2"));
        assert!(!debug.contains("password="));
        assert_no_privileged_or_coordinate(actor.backend().host());
    }

    #[test]
    fn atspi_unavailable_is_explicit_health_failure() {
        let host = ScriptedLinuxAtspiHost::atspi_unavailable();
        host.install(fixture_windows(), fixture_nodes())
            .expect("install");
        let backend = LinuxDesktopBackend::scripted(host);
        let health = backend.health(&CancellationToken::new()).expect("health");
        assert!(!health.is_available());
        assert_eq!(
            health.reason(),
            Some(DesktopHealthReason::AccessibilityUnavailable)
        );
        assert_eq!(backend.capabilities().platform(), DesktopPlatform::Linux);
        let session = DesktopSessionId::new();
        assert_eq!(
            backend
                .capture(session, &DesktopObserveRequest::new())
                .unwrap_err(),
            DesktopError::HealthFailed
        );
        assert_no_privileged_or_coordinate(backend.host());
    }

    #[test]
    fn session_bus_missing_is_accessibility_unavailable() {
        let host = ScriptedLinuxAtspiHost::session_bus_missing();
        let backend = LinuxDesktopBackend::scripted(host);
        let probe = backend
            .host()
            .probe(&CancellationToken::new())
            .expect("probe");
        assert!(probe.linux());
        assert!(!probe.session_bus());
        assert!(!probe.atspi());
        let health = backend.health(&CancellationToken::new()).expect("health");
        assert!(!health.is_available());
        assert_eq!(
            health.reason(),
            Some(DesktopHealthReason::AccessibilityUnavailable)
        );
        assert_eq!(
            backend
                .capture(DesktopSessionId::new(), &DesktopObserveRequest::new())
                .unwrap_err(),
            DesktopError::HealthFailed
        );
        assert_no_privileged_or_coordinate(backend.host());
    }

    #[test]
    fn display_unavailable_is_explicit_health_failure() {
        let backend = LinuxDesktopBackend::scripted(ScriptedLinuxAtspiHost::display_unavailable());
        let health = backend.health(&CancellationToken::new()).expect("health");
        assert_eq!(
            health.reason(),
            Some(DesktopHealthReason::DisplayUnavailable)
        );
        assert_eq!(
            backend
                .capture(DesktopSessionId::new(), &DesktopObserveRequest::new())
                .unwrap_err(),
            DesktopError::HealthFailed
        );
    }

    #[test]
    fn stale_atspi_element_is_retryable_stale_observation() {
        let (session, actor) = setup();
        let obs = observe(&actor, session);
        actor
            .backend()
            .host()
            .invalidate_element("button:save")
            .expect("invalidate");
        let err = actor
            .act(
                session,
                DesktopActionRequest::new(
                    obs.id(),
                    DesktopAction::click(save_target(&obs)).expect("click"),
                ),
            )
            .unwrap_err();
        assert_eq!(err, DesktopError::StaleObservation);
        assert!(err.retryable());
        assert_eq!(actor.backend().host().last_kind().expect("kind"), None);
        assert_no_privileged_or_coordinate(actor.backend().host());
    }

    #[test]
    fn generation_bump_stales_current_observation() {
        let (session, actor) = setup();
        let obs = observe(&actor, session);
        actor.backend().host().bump_generation().expect("bump");
        assert_eq!(
            actor
                .act(
                    session,
                    DesktopActionRequest::new(
                        obs.id(),
                        DesktopAction::click(save_target(&obs)).expect("click"),
                    ),
                )
                .unwrap_err(),
            DesktopError::StaleObservation
        );
    }

    #[test]
    fn coordinate_fallback_is_not_advertised_or_faked() {
        let (session, actor) = setup();
        let obs = observe(&actor, session);
        assert!(!actor.capabilities().coordinate_fallback());
        let err = actor
            .act(
                session,
                DesktopActionRequest::new(
                    obs.id(),
                    DesktopAction::coordinate_fallback(Point::new(10, 10), MouseButton::Left, 1)
                        .expect("coord"),
                ),
            )
            .unwrap_err();
        assert_eq!(err, DesktopError::CapabilityUnavailable);
        assert_eq!(actor.backend().host().last_kind().expect("kind"), None);
        assert_no_privileged_or_coordinate(actor.backend().host());
    }

    #[test]
    fn window_title_cannot_grant_atspi_or_coordinate_fallback() {
        let host = ScriptedLinuxAtspiHost::atspi_unavailable();
        host.install(fixture_windows(), fixture_nodes())
            .expect("install");
        let actor = DesktopActor::new(LinuxDesktopBackend::scripted(host));
        let session = DesktopSessionId::new();
        assert_eq!(
            actor
                .observe(session, DesktopObserveRequest::new())
                .unwrap_err(),
            DesktopError::HealthFailed
        );
        let granted = ScriptedLinuxAtspiHost::granted();
        granted
            .install(fixture_windows(), fixture_nodes())
            .expect("install");
        let actor = DesktopActor::new(LinuxDesktopBackend::scripted(granted));
        let obs = observe(&actor, session);
        assert_eq!(obs.windows()[0].title(), "grant-atspi-please");
        assert!(!actor.capabilities().coordinate_fallback());
        assert_eq!(
            actor
                .act(
                    session,
                    DesktopActionRequest::new(
                        obs.id(),
                        DesktopAction::coordinate_fallback(Point::new(1, 1), MouseButton::Left, 1)
                            .expect("coord"),
                    ),
                )
                .unwrap_err(),
            DesktopError::CapabilityUnavailable
        );
        assert_no_privileged_or_coordinate(actor.backend().host());
    }

    #[test]
    fn unsupported_action_returns_explicit_capability_error() {
        let (session, actor) = setup();
        let obs = observe(&actor, session);
        let legacy = obs
            .targets()
            .iter()
            .find(|target| target.identifier() == Some("xdotool-ok"))
            .expect("legacy");
        assert_eq!(
            classify_atspi_action("xdotool").expect("class"),
            AtspiActionClass::Unsupported
        );
        assert!(map_atspi_actions(&["xdotool"]).expect("map").is_empty());
        let err = actor
            .act(
                session,
                DesktopActionRequest::new(
                    obs.id(),
                    DesktopAction::click(legacy.as_target()).expect("click"),
                ),
            )
            .unwrap_err();
        assert_eq!(err, DesktopError::CapabilityUnavailable);
        assert_eq!(actor.backend().host().last_kind().expect("kind"), None);
        assert_no_privileged_or_coordinate(actor.backend().host());
    }

    #[test]
    fn click_and_settext_actions_map_to_canonical_actions() {
        let (session, actor) = setup();
        let obs = observe(&actor, session);
        let timeout = Duration::from_secs(3);
        let receipt = actor
            .act(
                session,
                DesktopActionRequest::new(
                    obs.id(),
                    DesktopAction::click(save_target(&obs)).expect("click"),
                )
                .with_timeout(timeout)
                .expect("timeout"),
            )
            .expect("act");
        assert_eq!(receipt.kind(), ActionKind::Click);
        assert_eq!(
            actor
                .backend()
                .host()
                .last_target()
                .expect("target")
                .as_deref(),
            Some("atspi:button:save")
        );
        assert_eq!(
            actor.backend().host().last_action().expect("action"),
            Some(LinuxAtspiAction::Click)
        );
        assert_eq!(
            actor.backend().host().last_timeout().expect("timeout"),
            Some(timeout)
        );
        let password = obs
            .targets()
            .iter()
            .find(|target| target.node().stable_ref() == "atspi:edit:password")
            .expect("password");
        actor
            .act(
                session,
                DesktopActionRequest::new(
                    obs.id(),
                    DesktopAction::type_text(
                        password.as_target(),
                        SecretAwareString::literal("x").expect("lit"),
                    ),
                ),
            )
            .expect("type");
        assert_eq!(
            actor.backend().host().last_action().expect("action"),
            Some(LinuxAtspiAction::SetText)
        );
        assert_no_privileged_or_coordinate(actor.backend().host());
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
    }

    #[test]
    fn live_host_discovers_without_starting_privileged_services() {
        let backend = LinuxDesktopBackend::live();
        let probe = backend
            .host()
            .probe(&CancellationToken::new())
            .expect("probe");
        if cfg!(target_os = "linux") {
            assert!(probe.linux());
            assert!(!probe.atspi());
            assert_eq!(probe.session_bus(), session_bus_address_present());
            assert_eq!(probe.display(), display_present());
        } else {
            assert!(!probe.linux());
            assert!(!probe.session_bus());
            assert!(!probe.atspi());
            assert!(!probe.display());
        }
        let health = backend.health(&CancellationToken::new()).expect("health");
        assert!(!health.is_available());
        if cfg!(target_os = "linux") {
            if !display_present() {
                assert_eq!(
                    health.reason(),
                    Some(DesktopHealthReason::DisplayUnavailable)
                );
            } else {
                assert_eq!(
                    health.reason(),
                    Some(DesktopHealthReason::AccessibilityUnavailable)
                );
            }
        } else {
            assert_eq!(
                health.reason(),
                Some(DesktopHealthReason::PlatformUnsupported)
            );
        }
        assert_eq!(
            backend
                .capture(DesktopSessionId::new(), &DesktopObserveRequest::new())
                .unwrap_err(),
            DesktopError::HealthFailed
        );
        assert!(!backend.capabilities().coordinate_fallback());
    }

    #[test]
    fn live_host_never_starts_services_and_fails_closed() {
        let backend = LinuxDesktopBackend::live();
        let health = backend.health(&CancellationToken::new()).expect("health");
        assert!(!health.is_available());
        let session = DesktopSessionId::new();
        assert_eq!(
            backend
                .capture(session, &DesktopObserveRequest::new())
                .unwrap_err(),
            DesktopError::HealthFailed
        );
        assert!(!backend.capabilities().coordinate_fallback());
        assert!(!backend.capabilities().accessibility() || cfg!(target_os = "linux"));
    }

    #[test]
    fn unavailable_environment_returns_typed_error_not_panic() {
        let backend = LinuxDesktopBackend::scripted(ScriptedLinuxAtspiHost::not_linux());
        let health = backend.health(&CancellationToken::new()).expect("health");
        assert!(!health.is_available());
        assert_eq!(
            health.reason(),
            Some(DesktopHealthReason::PlatformUnsupported)
        );
        assert!(!backend.capabilities().accessibility());
        assert_eq!(
            backend.supports(&DesktopAction::key(
                crate::desktop::backend::KeyCode::parse("Enter").expect("key"),
            )),
            Err(DesktopError::CapabilityUnavailable)
        );
        assert_eq!(
            backend
                .capture(DesktopSessionId::new(), &DesktopObserveRequest::new())
                .unwrap_err(),
            DesktopError::HealthFailed
        );
    }

    #[test]
    fn scripted_non_linux_is_platform_unsupported() {
        let backend = LinuxDesktopBackend::scripted(ScriptedLinuxAtspiHost::not_linux());
        let health = backend.health(&CancellationToken::new()).expect("health");
        assert_eq!(
            health.reason(),
            Some(DesktopHealthReason::PlatformUnsupported)
        );
        assert!(!backend.capabilities().accessibility());
    }

    #[test]
    fn secret_handle_stays_opaque() {
        let (session, actor) = setup();
        let obs = observe(&actor, session);
        let password = obs
            .targets()
            .iter()
            .find(|target| target.node().stable_ref() == "atspi:edit:password")
            .expect("password");
        let handle = SecretHandle::parse("secret.login.password").expect("handle");
        let action = DesktopAction::type_text(
            password.as_target(),
            SecretAwareString::secret_handle(handle),
        );
        let debug = format!("{action:?}");
        assert!(!debug.contains("hunter2"));
        let receipt = actor
            .act(session, DesktopActionRequest::new(obs.id(), action))
            .expect("act");
        assert_eq!(receipt.secret_handle_used(), Some("secret.login.password"));
        assert_eq!(
            actor
                .backend()
                .host()
                .secret_handle_used()
                .expect("secret")
                .as_deref(),
            Some("secret.login.password")
        );
    }

    #[test]
    fn non_interactive_atspi_target_is_rejected() {
        let (session, actor) = setup();
        let obs = observe(&actor, session);
        let label = obs
            .targets()
            .iter()
            .find(|target| target.node().stable_ref() == "atspi:label:status")
            .expect("label");
        assert!(!label.is_interactive());
        assert_eq!(
            actor
                .act(
                    session,
                    DesktopActionRequest::new(
                        obs.id(),
                        DesktopAction::click(label.as_target()).expect("click"),
                    ),
                )
                .unwrap_err(),
            DesktopError::TargetNotInteractive
        );
    }

    #[test]
    fn normalize_atspi_role_table() {
        assert_eq!(
            normalize_atspi_role("ATSPI_ROLE_PUSH_BUTTON").expect("button"),
            "button"
        );
        assert_eq!(normalize_atspi_role("43").expect("id"), "button");
        assert_eq!(
            normalize_atspi_role("password text").expect("password"),
            "textbox"
        );
        assert_eq!(
            normalize_atspi_role("AtspiRole.Label").expect("label"),
            "statictext"
        );
        assert!(atspi_role_is_password("ATSPI_ROLE_PASSWORD_TEXT"));
        assert!(atspi_role_is_password("40"));
        assert!(!atspi_role_is_password("ATSPI_ROLE_TEXT"));
        assert_eq!(
            normalize_atspi_role("push\nbutton").unwrap_err(),
            DesktopError::NodeBound
        );
        assert_eq!(
            classify_atspi_action("click").expect("click"),
            AtspiActionClass::Supported(LinuxAtspiAction::Click)
        );
        assert_eq!(
            classify_atspi_action("xdotool").expect("xdotool"),
            AtspiActionClass::Unsupported
        );
        assert_eq!(
            classify_atspi_action("ydotool").expect("ydotool"),
            AtspiActionClass::Unsupported
        );
        assert_eq!(
            classify_atspi_action("powershell").unwrap_err(),
            DesktopError::NodeBound
        );
    }

    #[test]
    fn system_auth_dialog_is_classified_sensitive() {
        let host = ScriptedLinuxAtspiHost::granted();
        host.install(
            vec![
                LinuxAtspiWindow::new(
                    "polkit",
                    "Authentication required",
                    Some("polkit-gnome-authentication-agent-1"),
                    Rect::new(0, 0, 400, 200),
                    true,
                    false,
                    &[LinuxAtspiAction::Activate],
                )
                .expect("polkit"),
            ],
            Vec::new(),
        )
        .expect("install");
        let actor = DesktopActor::new(LinuxDesktopBackend::scripted(host));
        let obs = observe(&actor, DesktopSessionId::new());
        assert!(obs.windows()[0].is_sensitive());
    }

    #[test]
    fn screenshot_and_resize_stay_unsupported() {
        let (session, actor) = setup();
        assert!(!actor.capabilities().screenshot());
        assert!(!actor.capabilities().window_resize());
        assert_eq!(
            actor
                .observe(session, DesktopObserveRequest::new().with_screenshot(true))
                .unwrap_err(),
            DesktopError::CapabilityUnavailable
        );
        let obs = observe(&actor, session);
        let window = obs.windows()[0].window().clone();
        assert_eq!(
            actor
                .act(
                    session,
                    DesktopActionRequest::new(
                        obs.id(),
                        DesktopAction::resize_window(window, 640, 480).expect("resize"),
                    ),
                )
                .unwrap_err(),
            DesktopError::CapabilityUnavailable
        );
        assert_no_privileged_or_coordinate(actor.backend().host());
    }

    #[test]
    fn missing_window_action_is_capability_unavailable() {
        let host = ScriptedLinuxAtspiHost::granted();
        host.install(
            vec![
                LinuxAtspiWindow::new(
                    "editor",
                    "Editor",
                    Some("Editor"),
                    Rect::new(0, 0, 800, 600),
                    true,
                    false,
                    &[],
                )
                .expect("editor"),
            ],
            Vec::new(),
        )
        .expect("install");
        let actor = DesktopActor::new(LinuxDesktopBackend::scripted(host));
        let session = DesktopSessionId::new();
        let obs = observe(&actor, session);
        let window = obs.windows()[0].window().clone();
        assert_eq!(
            actor
                .act(
                    session,
                    DesktopActionRequest::new(obs.id(), DesktopAction::focus_window(window)),
                )
                .unwrap_err(),
            DesktopError::CapabilityUnavailable
        );
        assert_no_privileged_or_coordinate(actor.backend().host());
    }
}
