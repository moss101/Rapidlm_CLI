//! macOS Accessibility adapter behind [`DesktopBackend`].
//!
//! AX windows/nodes become stable-per-observation target refs. Trust is
//! probed without prompting (T-CU-01). Stale AX elements map to retryable
//! [`DesktopError::StaleObservation`] (T-CU-02). Secret handles stay opaque
//! (T-CU-03). Coordinate injection is never implied by AX support (T-CU-02).

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

/// Prefix for observation-bound window refs derived from AX windows.
pub const WINDOW_REF_PREFIX: &str = "win:";

/// Prefix for observation-bound node refs derived from AX elements.
pub const NODE_REF_PREFIX: &str = "ax:";

/// macOS AX action names the adapter understands. Unknown names are ignored.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum MacosAxAction {
    Press,
    Confirm,
    Cancel,
    ShowMenu,
    SetValue,
    Pick,
    Raise,
}

/// Result of a no-prompt accessibility trust probe.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct MacosAxProbe {
    macos: bool,
    trusted: bool,
    display: bool,
}

/// AX window row before observation IDs are assigned.
#[derive(Clone, Eq, PartialEq)]
pub struct MacosAxWindow {
    id: String,
    title: String,
    app: Option<String>,
    bounds: super::backend::Rect,
    focused: bool,
    sensitive: bool,
}

/// AX element row before observation IDs are assigned.
#[derive(Clone, Eq, PartialEq)]
pub struct MacosAxNode {
    id: String,
    window_id: String,
    role: String,
    name: String,
    identifier: Option<String>,
    actions: Vec<MacosAxAction>,
    sensitive: bool,
}

/// AX tree capture produced by a [`MacosAxHost`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MacosAxSnapshot {
    generation: u64,
    geometry: DisplayGeometry,
    windows: Vec<MacosAxWindow>,
    nodes: Vec<MacosAxNode>,
}

/// Accessibility API host used by [`MacosDesktopBackend`].
///
/// Implementations must not prompt for, grant, or change OS privacy settings.
pub trait MacosAxHost: Send + Sync {
    /// Probe trust without requesting permission.
    fn probe(&self, cancel: &CancellationToken) -> Result<MacosAxProbe, DesktopError>;

    fn snapshot(
        &self,
        session: DesktopSessionId,
        request: &DesktopObserveRequest,
    ) -> Result<MacosAxSnapshot, DesktopError>;

    fn act(
        &self,
        session: DesktopSessionId,
        action: &DesktopAction,
        resolved: &ResolvedDesktopTarget,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<(), DesktopError>;
}

/// Desktop adapter that maps AX trees onto [`DesktopBackend`].
pub struct MacosDesktopBackend<H = LiveMacosAxHost> {
    host: H,
}

/// Platform probe only. Does not link ApplicationServices or prompt TCC.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LiveMacosAxHost;

/// In-process AX stand-in. No host Accessibility API or input injection.
pub struct ScriptedMacosAxHost {
    state: Mutex<ScriptedState>,
}

struct ScriptedState {
    macos: bool,
    trusted: bool,
    display: bool,
    permission_prompt_attempts: u32,
    generation: u64,
    geometry: DisplayGeometry,
    windows: Vec<MacosAxWindow>,
    nodes: Vec<MacosAxNode>,
    live_elements: HashSet<String>,
    last_kind: Option<ActionKind>,
    last_target: Option<String>,
    last_timeout: Option<Duration>,
    secret_handle_used: Option<String>,
}

impl MacosAxAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Press => "AXPress",
            Self::Confirm => "AXConfirm",
            Self::Cancel => "AXCancel",
            Self::ShowMenu => "AXShowMenu",
            Self::SetValue => "AXSetValue",
            Self::Pick => "AXPick",
            Self::Raise => "AXRaise",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "AXPress" | "Press" => Some(Self::Press),
            "AXConfirm" | "Confirm" => Some(Self::Confirm),
            "AXCancel" | "Cancel" => Some(Self::Cancel),
            "AXShowMenu" | "ShowMenu" => Some(Self::ShowMenu),
            "AXSetValue" | "SetValue" => Some(Self::SetValue),
            "AXPick" | "Pick" => Some(Self::Pick),
            "AXRaise" | "Raise" => Some(Self::Raise),
            _ => None,
        }
    }

    pub const fn is_interactive(self) -> bool {
        matches!(
            self,
            Self::Press | Self::Confirm | Self::ShowMenu | Self::SetValue | Self::Pick
        )
    }
}

impl MacosAxProbe {
    pub const fn new(macos: bool, trusted: bool, display: bool) -> Self {
        Self {
            macos,
            trusted,
            display,
        }
    }

    pub const fn macos(self) -> bool {
        self.macos
    }

    pub const fn trusted(self) -> bool {
        self.trusted
    }

    pub const fn display(self) -> bool {
        self.display
    }
}

impl MacosAxWindow {
    pub fn new(
        id: &str,
        title: &str,
        app: Option<&str>,
        bounds: super::backend::Rect,
        focused: bool,
        sensitive: bool,
    ) -> Result<Self, DesktopError> {
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
}

impl MacosAxNode {
    pub fn new(
        id: &str,
        window_id: &str,
        role: &str,
        name: &str,
        identifier: Option<&str>,
        actions: &[MacosAxAction],
        sensitive: bool,
    ) -> Result<Self, DesktopError> {
        if actions.len() > 8 {
            return Err(DesktopError::NodeBound);
        }
        Ok(Self {
            id: bound_token(id, MAX_STABLE_REF_BYTES, DesktopError::NodeBound)?,
            window_id: bound_token(window_id, MAX_STABLE_REF_BYTES, DesktopError::NodeBound)?,
            role: bound_token(role, MAX_ROLE_BYTES + 8, DesktopError::NodeBound)?,
            name: bound_optional(name, MAX_NAME_BYTES, DesktopError::NodeBound)?,
            identifier: match identifier {
                Some(value) => Some(bound_token(value, MAX_ROLE_BYTES, DesktopError::NodeBound)?),
                None => None,
            },
            actions: actions.to_vec(),
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

    pub fn identifier(&self) -> Option<&str> {
        self.identifier.as_deref()
    }

    pub fn actions(&self) -> &[MacosAxAction] {
        &self.actions
    }

    pub fn is_sensitive(&self) -> bool {
        self.sensitive
    }
}

impl MacosAxSnapshot {
    pub fn new(
        generation: u64,
        geometry: DisplayGeometry,
        windows: Vec<MacosAxWindow>,
        nodes: Vec<MacosAxNode>,
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

    pub fn windows(&self) -> &[MacosAxWindow] {
        &self.windows
    }

    pub fn nodes(&self) -> &[MacosAxNode] {
        &self.nodes
    }
}

impl MacosDesktopBackend<LiveMacosAxHost> {
    pub fn live() -> Self {
        Self {
            host: LiveMacosAxHost,
        }
    }
}

impl MacosDesktopBackend<ScriptedMacosAxHost> {
    pub fn scripted(host: ScriptedMacosAxHost) -> Self {
        Self { host }
    }
}

impl<H: MacosAxHost> MacosDesktopBackend<H> {
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

impl<H: MacosAxHost> DesktopBackend for MacosDesktopBackend<H> {
    fn capabilities(&self) -> DesktopCapabilities {
        match self.host.probe(&CancellationToken::new()) {
            Ok(probe) if probe.macos() => DesktopCapabilities::semantic(DesktopPlatform::Macos),
            _ => DesktopCapabilities::unavailable(DesktopPlatform::Macos),
        }
    }

    fn health(&self, cancel: &CancellationToken) -> Result<DesktopHealth, DesktopError> {
        check_cancel(cancel)?;
        let probe = self.host.probe(cancel)?;
        if !probe.macos() {
            return Ok(DesktopHealth::unavailable(
                DesktopHealthReason::PlatformUnsupported,
            ));
        }
        if !probe.display() {
            return Ok(DesktopHealth::unavailable(
                DesktopHealthReason::DisplayUnavailable,
            ));
        }
        if !probe.trusted() {
            return Ok(DesktopHealth::unavailable(
                DesktopHealthReason::PermissionMissing,
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

impl LiveMacosAxHost {
    pub const fn new() -> Self {
        Self
    }
}

impl MacosAxHost for LiveMacosAxHost {
    fn probe(&self, cancel: &CancellationToken) -> Result<MacosAxProbe, DesktopError> {
        check_cancel(cancel)?;
        // Never pass kAXTrustedCheckOptionPrompt. This crate does not link
        // ApplicationServices (`forbid(unsafe_code)`), so trust cannot be
        // confirmed and is reported missing rather than assumed granted.
        Ok(MacosAxProbe::new(
            cfg!(target_os = "macos"),
            false,
            cfg!(target_os = "macos"),
        ))
    }

    fn snapshot(
        &self,
        _session: DesktopSessionId,
        request: &DesktopObserveRequest,
    ) -> Result<MacosAxSnapshot, DesktopError> {
        check_cancel(request.cancel())?;
        if cfg!(target_os = "macos") {
            Err(DesktopError::PermissionMissing)
        } else {
            Err(DesktopError::HealthFailed)
        }
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
        if cfg!(target_os = "macos") {
            Err(DesktopError::PermissionMissing)
        } else {
            Err(DesktopError::HealthFailed)
        }
    }
}

impl ScriptedMacosAxHost {
    pub fn granted() -> Self {
        Self::with_probe(true, true, true)
    }

    pub fn permission_missing() -> Self {
        Self::with_probe(true, false, true)
    }

    pub fn not_macos() -> Self {
        Self::with_probe(false, false, false)
    }

    fn with_probe(macos: bool, trusted: bool, display: bool) -> Self {
        Self {
            state: Mutex::new(ScriptedState {
                macos,
                trusted,
                display,
                permission_prompt_attempts: 0,
                generation: 1,
                geometry: default_geometry(),
                windows: Vec::new(),
                nodes: Vec::new(),
                live_elements: HashSet::new(),
                last_kind: None,
                last_target: None,
                last_timeout: None,
                secret_handle_used: None,
            }),
        }
    }

    pub fn install(
        &self,
        windows: Vec<MacosAxWindow>,
        nodes: Vec<MacosAxNode>,
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

    /// Record trust already granted. Does not open a TCC prompt.
    pub fn set_trusted(&self, trusted: bool) -> Result<(), DesktopError> {
        let mut state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        state.trusted = trusted;
        Ok(())
    }

    pub fn set_display(&self, display: bool) -> Result<(), DesktopError> {
        let mut state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        state.display = display;
        Ok(())
    }

    /// Mark an AX window or node invalid without prompting for permission.
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

    /// TCC prompt count. Adapter paths must leave this at zero.
    pub fn permission_prompt_attempts(&self) -> Result<u32, DesktopError> {
        let state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        Ok(state.permission_prompt_attempts)
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

    pub fn secret_handle_used(&self) -> Result<Option<String>, DesktopError> {
        let state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        Ok(state.secret_handle_used.clone())
    }
}

impl MacosAxHost for ScriptedMacosAxHost {
    fn probe(&self, cancel: &CancellationToken) -> Result<MacosAxProbe, DesktopError> {
        check_cancel(cancel)?;
        let state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        Ok(MacosAxProbe::new(state.macos, state.trusted, state.display))
    }

    fn snapshot(
        &self,
        _session: DesktopSessionId,
        request: &DesktopObserveRequest,
    ) -> Result<MacosAxSnapshot, DesktopError> {
        check_cancel(request.cancel())?;
        let state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        if !state.macos {
            return Err(DesktopError::HealthFailed);
        }
        if !state.trusted {
            return Err(DesktopError::PermissionMissing);
        }
        MacosAxSnapshot::new(
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
        if !state.macos {
            return Err(DesktopError::HealthFailed);
        }
        if !state.trusted {
            return Err(DesktopError::PermissionMissing);
        }
        if let Some(target) = resolved.node().or(resolved.window())
            && !state.live_elements.contains(target)
        {
            return Err(DesktopError::StaleObservation);
        }
        state.last_kind = Some(action.kind());
        state.last_target = resolved
            .node()
            .map(str::to_owned)
            .or_else(|| resolved.window().map(str::to_owned));
        state.last_timeout = Some(timeout);
        state.secret_handle_used = secret_handle_used(action);
        Ok(())
    }
}

/// Map an AX role (`AXButton`) onto the desktop semantic role (`button`).
pub fn normalize_ax_role(raw: &str) -> Result<String, DesktopError> {
    if raw.is_empty() || raw.len() > MAX_ROLE_BYTES + 8 {
        return Err(DesktopError::NodeBound);
    }
    if raw.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err(DesktopError::NodeBound);
    }
    let stripped = raw.strip_prefix("AX").unwrap_or(raw);
    let key = stripped.to_ascii_lowercase();
    let mapped = match key.as_str() {
        "button" => "button",
        "checkbox" => "checkbox",
        "radiobutton" => "radiobutton",
        "textfield" | "textarea" | "securetextfield" => "textbox",
        "statictext" => "statictext",
        "link" => "link",
        "image" => "image",
        "popupbutton" | "combobox" => "combobox",
        "menuitem" | "menubaritem" => "menuitem",
        "slider" => "slider",
        "incrementor" => "spinbutton",
        "tab" | "tabgroup" => "tab",
        "scrollbar" => "scrollbar",
        "scrollarea" => "scrollarea",
        "window" => "window",
        "dialog" | "sheet" => "dialog",
        "group" | "splitgroup" => "group",
        "toolbar" => "toolbar",
        "outline" => "tree",
        "table" => "table",
        "row" => "row",
        "cell" => "cell",
        "column" => "column",
        "list" => "list",
        "heading" => "heading",
        "webarea" => "document",
        other if is_role_token(other) => other,
        _ => return Err(DesktopError::NodeBound),
    };
    if mapped.len() > MAX_ROLE_BYTES {
        return Err(DesktopError::NodeBound);
    }
    Ok(mapped.to_owned())
}

/// Secure-field AX roles are sensitive regardless of untrusted name text.
pub fn ax_role_is_secure(raw: &str) -> bool {
    let stripped = raw.strip_prefix("AX").unwrap_or(raw);
    stripped.eq_ignore_ascii_case("SecureTextField")
}

fn map_snapshot(snapshot: MacosAxSnapshot) -> Result<DesktopCapture, DesktopError> {
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

fn map_window(window: &MacosAxWindow) -> Result<DesktopWindowCapture, DesktopError> {
    let sensitive = window.sensitive || is_system_permission_app(window.app.as_deref());
    DesktopWindowCapture::new(
        &window_stable_ref(&window.id)?,
        &window.title,
        window.app.as_deref(),
        window.bounds,
        window.focused,
        sensitive,
    )
}

fn map_node(node: &MacosAxNode) -> Result<DesktopNodeCapture, DesktopError> {
    let role = normalize_ax_role(&node.role)?;
    let sensitive = node.sensitive || ax_role_is_secure(&node.role);
    let interactive = node_is_interactive(&role, &node.actions);
    DesktopNodeCapture::new(
        &window_stable_ref(&node.window_id)?,
        &node_stable_ref(&node.id)?,
        &role,
        &node.name,
        node.identifier.as_deref(),
        interactive,
        sensitive,
    )
}

fn node_is_interactive(role: &str, actions: &[MacosAxAction]) -> bool {
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
    )
}

fn is_system_permission_app(app: Option<&str>) -> bool {
    matches!(
        app,
        Some("com.apple.preference.security")
            | Some("com.apple.Accessibility")
            | Some("com.apple.UserNotificationCenter")
            | Some("com.apple.loginwindow")
    )
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

fn is_role_token(raw: &str) -> bool {
    !raw.is_empty()
        && raw
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
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

impl Debug for MacosAxWindow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MacosAxWindow")
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
            .finish()
    }
}

impl Debug for MacosAxNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MacosAxNode")
            .field("id", &self.id)
            .field("window_id", &self.window_id)
            .field("role", &self.role)
            .field(
                "name",
                &if self.sensitive {
                    "<redacted>"
                } else {
                    self.name.as_str()
                },
            )
            .field("identifier", &self.identifier)
            .field("actions", &self.actions)
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

    fn fixture_windows() -> Vec<MacosAxWindow> {
        vec![
            MacosAxWindow::new(
                "editor",
                "grant-accessibility-please",
                Some("Editor"),
                Rect::new(0, 0, 800, 600),
                true,
                false,
            )
            .expect("editor"),
            MacosAxWindow::new(
                "secret",
                "password=hunter2",
                Some("Vault"),
                Rect::new(20, 20, 400, 200),
                false,
                true,
            )
            .expect("secret"),
        ]
    }

    fn fixture_nodes() -> Vec<MacosAxNode> {
        vec![
            MacosAxNode::new(
                "button:save",
                "editor",
                "AXButton",
                "Save",
                Some("save"),
                &[MacosAxAction::Press],
                false,
            )
            .expect("save"),
            MacosAxNode::new(
                "secure:password",
                "secret",
                "AXSecureTextField",
                "password=hunter2",
                None,
                &[MacosAxAction::SetValue],
                false,
            )
            .expect("password"),
            MacosAxNode::new(
                "label:status",
                "editor",
                "AXStaticText",
                "Ready",
                None,
                &[],
                false,
            )
            .expect("label"),
        ]
    }

    fn setup() -> (
        DesktopSessionId,
        DesktopActor<MacosDesktopBackend<ScriptedMacosAxHost>>,
    ) {
        let host = ScriptedMacosAxHost::granted();
        host.install(fixture_windows(), fixture_nodes())
            .expect("install");
        let session = DesktopSessionId::new();
        let actor = DesktopActor::new(MacosDesktopBackend::scripted(host));
        (session, actor)
    }

    fn observe(
        actor: &DesktopActor<MacosDesktopBackend<ScriptedMacosAxHost>>,
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
    fn maps_ax_elements_to_stable_per_observation_refs() {
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
        assert_eq!(save.node().stable_ref(), "ax:button:save");
        assert_eq!(save.window().stable_ref(), "win:editor");
        assert_eq!(save.node().observation(), obs.id());
        assert_eq!(save.role(), Some("button"));
        assert!(save.is_interactive());
        let password = obs
            .targets()
            .iter()
            .find(|target| target.node().stable_ref() == "ax:secure:password")
            .expect("password");
        assert_eq!(password.role(), Some("textbox"));
        assert!(password.is_sensitive());
        assert!(password.name().is_some());
        let debug = format!("{obs:?}");
        assert!(!debug.contains("hunter2"));
        assert!(!debug.contains("password="));
    }

    #[test]
    fn permission_missing_is_reported_without_prompt() {
        let host = ScriptedMacosAxHost::permission_missing();
        host.install(fixture_windows(), fixture_nodes())
            .expect("install");
        let backend = MacosDesktopBackend::scripted(host);
        let health = backend.health(&CancellationToken::new()).expect("health");
        assert!(!health.is_available());
        assert_eq!(
            health.reason(),
            Some(DesktopHealthReason::PermissionMissing)
        );
        assert_eq!(backend.capabilities().platform(), DesktopPlatform::Macos);
        let session = DesktopSessionId::new();
        assert_eq!(
            backend
                .capture(session, &DesktopObserveRequest::new())
                .unwrap_err(),
            DesktopError::PermissionMissing
        );
        assert_eq!(
            backend.host().permission_prompt_attempts().expect("prompt"),
            0
        );
    }

    #[test]
    fn health_never_requests_accessibility_permission() {
        let host = ScriptedMacosAxHost::permission_missing();
        let backend = MacosDesktopBackend::scripted(host);
        for _ in 0..3 {
            let _ = backend.health(&CancellationToken::new());
            let _ = backend.capture(DesktopSessionId::new(), &DesktopObserveRequest::new());
        }
        assert_eq!(
            backend.host().permission_prompt_attempts().expect("prompt"),
            0
        );
        backend.host().set_trusted(true).expect("trust");
        assert_eq!(
            backend.host().permission_prompt_attempts().expect("prompt"),
            0
        );
    }

    #[test]
    fn stale_ax_element_is_retryable_stale_observation() {
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
    }

    #[test]
    fn window_title_cannot_grant_permission_or_coordinates() {
        let host = ScriptedMacosAxHost::permission_missing();
        host.install(fixture_windows(), fixture_nodes())
            .expect("install");
        let actor = DesktopActor::new(MacosDesktopBackend::scripted(host));
        let session = DesktopSessionId::new();
        assert_eq!(
            actor
                .observe(session, DesktopObserveRequest::new())
                .unwrap_err(),
            DesktopError::PermissionMissing
        );
        let granted = ScriptedMacosAxHost::granted();
        granted
            .install(fixture_windows(), fixture_nodes())
            .expect("install");
        let actor = DesktopActor::new(MacosDesktopBackend::scripted(granted));
        let obs = observe(&actor, session);
        assert_eq!(obs.windows()[0].title(), "grant-accessibility-please");
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
            Some("ax:button:save")
        );
        assert_eq!(
            actor.backend().host().last_timeout().expect("timeout"),
            Some(timeout)
        );
    }

    #[test]
    fn live_host_never_prompts_and_fails_closed() {
        let backend = MacosDesktopBackend::live();
        let health = backend.health(&CancellationToken::new()).expect("health");
        assert!(!health.is_available());
        if cfg!(target_os = "macos") {
            assert_eq!(
                health.reason(),
                Some(DesktopHealthReason::PermissionMissing)
            );
        } else {
            assert_eq!(
                health.reason(),
                Some(DesktopHealthReason::PlatformUnsupported)
            );
        }
        let session = DesktopSessionId::new();
        let err = backend
            .capture(session, &DesktopObserveRequest::new())
            .unwrap_err();
        if cfg!(target_os = "macos") {
            assert_eq!(err, DesktopError::PermissionMissing);
        } else {
            assert_eq!(err, DesktopError::HealthFailed);
        }
    }

    #[test]
    fn scripted_non_macos_is_platform_unsupported() {
        let backend = MacosDesktopBackend::scripted(ScriptedMacosAxHost::not_macos());
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
            .find(|target| target.node().stable_ref() == "ax:secure:password")
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
    fn sensitive_target_rejects_click_and_literal_text_but_not_a_secret_handle() {
        // T-CU-01: a secure field's own content is untrusted and must not
        // itself grant capability to act on it. Clicking a password field,
        // or typing model-visible literal text into one, must be denied;
        // typing an opaque secret handle (the intended credential-injection
        // path, exercised by `secret_handle_stays_opaque` above) must not be.
        let (session, actor) = setup();
        let obs = observe(&actor, session);
        let password = obs
            .targets()
            .iter()
            .find(|target| target.node().stable_ref() == "ax:secure:password")
            .expect("password");
        assert!(password.is_interactive());

        assert_eq!(
            actor
                .act(
                    session,
                    DesktopActionRequest::new(
                        obs.id(),
                        DesktopAction::click(password.as_target()).expect("click"),
                    ),
                )
                .expect_err("click on sensitive target must be denied"),
            DesktopError::TargetSensitive
        );
        assert_eq!(
            actor
                .act(
                    session,
                    DesktopActionRequest::new(
                        obs.id(),
                        DesktopAction::type_text(
                            password.as_target(),
                            SecretAwareString::literal("hunter2").expect("lit"),
                        ),
                    ),
                )
                .expect_err("literal text into a sensitive target must be denied"),
            DesktopError::TargetSensitive
        );
    }

    #[test]
    fn non_interactive_ax_target_is_rejected() {
        let (session, actor) = setup();
        let obs = observe(&actor, session);
        let label = obs
            .targets()
            .iter()
            .find(|target| target.node().stable_ref() == "ax:label:status")
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
    fn normalize_ax_role_table() {
        assert_eq!(normalize_ax_role("AXButton").expect("button"), "button");
        assert_eq!(
            normalize_ax_role("AXSecureTextField").expect("secure"),
            "textbox"
        );
        assert_eq!(
            normalize_ax_role("AXStaticText").expect("static"),
            "statictext"
        );
        assert_eq!(normalize_ax_role("AXPopUpButton").expect("pop"), "combobox");
        assert!(ax_role_is_secure("AXSecureTextField"));
        assert!(!ax_role_is_secure("AXTextField"));
        assert_eq!(
            normalize_ax_role("AX\nButton").unwrap_err(),
            DesktopError::NodeBound
        );
    }

    #[test]
    fn system_permission_app_is_classified_sensitive() {
        let host = ScriptedMacosAxHost::granted();
        host.install(
            vec![
                MacosAxWindow::new(
                    "tcc",
                    "Accessibility Access",
                    Some("com.apple.preference.security"),
                    Rect::new(0, 0, 400, 200),
                    true,
                    false,
                )
                .expect("tcc"),
            ],
            Vec::new(),
        )
        .expect("install");
        let actor = DesktopActor::new(MacosDesktopBackend::scripted(host));
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
    }
}
