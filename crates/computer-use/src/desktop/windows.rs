//! Windows UI Automation adapter behind [`DesktopBackend`].
//!
//! AutomationElement ControlType/AutomationId/IsPassword and supported
//! patterns map to canonical targets/actions. Unsupported patterns
//! (`LegacyIAccessible`, …) are never a core action path: they return
//! [`DesktopError::CapabilityUnavailable`] (T-CU-01). There is no
//! PowerShell/`cmd` fallback. Stale UIA elements map to retryable
//! [`DesktopError::StaleObservation`] (T-CU-02). Secret handles stay
//! opaque (T-CU-03). Coordinate injection is never implied by UIA
//! support (T-CU-02).

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

/// Prefix for observation-bound window refs derived from UIA windows.
pub const WINDOW_REF_PREFIX: &str = "win:";

/// Prefix for observation-bound node refs derived from AutomationElements.
pub const NODE_REF_PREFIX: &str = "uia:";

/// UIA patterns this adapter will invoke. Others are unsupported.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum WindowsUiaPattern {
    Invoke,
    Value,
    Text,
    Toggle,
    SelectionItem,
    ExpandCollapse,
    Scroll,
    RangeValue,
    Window,
    Transform,
}

/// Classification of a raw UIA pattern name or numeric id.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum UiaPatternClass {
    Supported(WindowsUiaPattern),
    /// Recognized UIA pattern that is never a core action path.
    Unsupported,
}

/// Result of a no-prompt UIA availability probe.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct WindowsUiaProbe {
    windows: bool,
    uia: bool,
    display: bool,
}

/// UIA window row before observation IDs are assigned.
#[derive(Clone, Eq, PartialEq)]
pub struct WindowsUiaWindow {
    id: String,
    title: String,
    app: Option<String>,
    bounds: super::backend::Rect,
    focused: bool,
    sensitive: bool,
    patterns: Vec<WindowsUiaPattern>,
}

/// AutomationElement row before observation IDs are assigned.
#[derive(Clone, Eq, PartialEq)]
pub struct WindowsUiaNode {
    id: String,
    window_id: String,
    control_type: String,
    name: String,
    automation_id: Option<String>,
    patterns: Vec<WindowsUiaPattern>,
    is_password: bool,
    sensitive: bool,
}

/// UIA tree capture produced by a [`WindowsUiaHost`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowsUiaSnapshot {
    generation: u64,
    geometry: DisplayGeometry,
    windows: Vec<WindowsUiaWindow>,
    nodes: Vec<WindowsUiaNode>,
}

/// UI Automation host used by [`WindowsDesktopBackend`].
///
/// Implementations must not spawn PowerShell/`cmd` and must not prompt
/// for, grant, or change OS privacy settings.
pub trait WindowsUiaHost: Send + Sync {
    /// Probe UIA/display availability without requesting permission.
    fn probe(&self, cancel: &CancellationToken) -> Result<WindowsUiaProbe, DesktopError>;

    fn snapshot(
        &self,
        session: DesktopSessionId,
        request: &DesktopObserveRequest,
    ) -> Result<WindowsUiaSnapshot, DesktopError>;

    fn act(
        &self,
        session: DesktopSessionId,
        action: &DesktopAction,
        resolved: &ResolvedDesktopTarget,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<(), DesktopError>;
}

/// Desktop adapter that maps UIA trees onto [`DesktopBackend`].
pub struct WindowsDesktopBackend<H = LiveWindowsUiaHost> {
    host: H,
}

/// Platform probe only. Does not link UIAutomationCore or spawn a shell.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LiveWindowsUiaHost;

/// In-process UIA stand-in. No host COM/UIA or input injection.
pub struct ScriptedWindowsUiaHost {
    state: Mutex<ScriptedState>,
}

struct ScriptedState {
    windows_os: bool,
    uia: bool,
    display: bool,
    shell_fallback_attempts: u32,
    generation: u64,
    geometry: DisplayGeometry,
    windows: Vec<WindowsUiaWindow>,
    nodes: Vec<WindowsUiaNode>,
    live_elements: HashSet<String>,
    last_kind: Option<ActionKind>,
    last_target: Option<String>,
    last_timeout: Option<Duration>,
    last_pattern: Option<WindowsUiaPattern>,
    secret_handle_used: Option<String>,
}

impl WindowsUiaPattern {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Invoke => "InvokePattern",
            Self::Value => "ValuePattern",
            Self::Text => "TextPattern",
            Self::Toggle => "TogglePattern",
            Self::SelectionItem => "SelectionItemPattern",
            Self::ExpandCollapse => "ExpandCollapsePattern",
            Self::Scroll => "ScrollPattern",
            Self::RangeValue => "RangeValuePattern",
            Self::Window => "WindowPattern",
            Self::Transform => "TransformPattern",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match classify_uia_pattern(raw).ok()? {
            UiaPatternClass::Supported(pattern) => Some(pattern),
            UiaPatternClass::Unsupported => None,
        }
    }

    pub const fn supports(self, kind: ActionKind) -> bool {
        match (self, kind) {
            (
                Self::Invoke | Self::Toggle | Self::SelectionItem | Self::ExpandCollapse,
                ActionKind::Click,
            ) => true,
            (Self::Value | Self::Text | Self::RangeValue, ActionKind::TypeText) => true,
            (Self::Scroll, ActionKind::Scroll) => true,
            (Self::Window, ActionKind::FocusWindow | ActionKind::CloseWindow) => true,
            (Self::Transform | Self::Window, ActionKind::ResizeWindow) => true,
            _ => false,
        }
    }

    pub const fn is_interactive(self) -> bool {
        matches!(
            self,
            Self::Invoke
                | Self::Value
                | Self::Text
                | Self::Toggle
                | Self::SelectionItem
                | Self::ExpandCollapse
                | Self::Scroll
                | Self::RangeValue
        )
    }
}

/// Map a raw AutomationElement pattern name or numeric id.
pub fn classify_uia_pattern(raw: &str) -> Result<UiaPatternClass, DesktopError> {
    let key = normalize_uia_token(raw, DesktopError::NodeBound)?;
    match key.as_str() {
        "invoke" | "invokepattern" | "uia_invokepatternid" | "10000" => {
            Ok(UiaPatternClass::Supported(WindowsUiaPattern::Invoke))
        }
        "value" | "valuepattern" | "uia_valuepatternid" | "10002" => {
            Ok(UiaPatternClass::Supported(WindowsUiaPattern::Value))
        }
        "text" | "textpattern" | "uia_textpatternid" | "10014" => {
            Ok(UiaPatternClass::Supported(WindowsUiaPattern::Text))
        }
        "toggle" | "togglepattern" | "uia_togglepatternid" | "10015" => {
            Ok(UiaPatternClass::Supported(WindowsUiaPattern::Toggle))
        }
        "selectionitem" | "selectionitempattern" | "uia_selectionitempatternid" | "10010" => {
            Ok(UiaPatternClass::Supported(WindowsUiaPattern::SelectionItem))
        }
        "expandcollapse" | "expandcollapsepattern" | "uia_expandcollapsepatternid" | "10005" => Ok(
            UiaPatternClass::Supported(WindowsUiaPattern::ExpandCollapse),
        ),
        "scroll" | "scrollpattern" | "uia_scrollpatternid" | "10004" => {
            Ok(UiaPatternClass::Supported(WindowsUiaPattern::Scroll))
        }
        "rangevalue" | "rangevaluepattern" | "uia_rangevaluepatternid" | "10003" => {
            Ok(UiaPatternClass::Supported(WindowsUiaPattern::RangeValue))
        }
        "window" | "windowpattern" | "uia_windowpatternid" | "10009" => {
            Ok(UiaPatternClass::Supported(WindowsUiaPattern::Window))
        }
        "transform" | "transformpattern" | "uia_transformpatternid" | "10016" => {
            Ok(UiaPatternClass::Supported(WindowsUiaPattern::Transform))
        }
        "legacyiaccessible"
        | "legacyiaccessiblepattern"
        | "uia_legacyiaccessiblepatternid"
        | "10018"
        | "virtualizeditem"
        | "virtualizeditempattern"
        | "uia_virtualizeditempatternid"
        | "10020"
        | "synchronizedinput"
        | "synchronizedinputpattern"
        | "10021"
        | "objectmodel"
        | "objectmodelpattern"
        | "10022"
        | "annotation"
        | "annotationpattern"
        | "10023"
        | "customnavigation"
        | "customnavigationpattern"
        | "10028"
        | "dock"
        | "dockpattern"
        | "10011"
        | "grid"
        | "gridpattern"
        | "10006"
        | "griditem"
        | "griditempattern"
        | "10007"
        | "table"
        | "tablepattern"
        | "10012"
        | "tableitem"
        | "tableitempattern"
        | "10013"
        | "multipleview"
        | "multipleviewpattern"
        | "10008"
        | "itemcontainer"
        | "itemcontainerpattern"
        | "10019"
        | "scrollitem"
        | "scrollitempattern"
        | "10017"
        | "drag"
        | "dragpattern"
        | "10025"
        | "droptarget"
        | "droptargetpattern"
        | "10026"
        | "textchild"
        | "textchildpattern"
        | "10024"
        | "textedit"
        | "texteditpattern"
        | "10027"
        | "text2"
        | "textpattern2"
        | "10034"
        | "selection"
        | "selectionpattern"
        | "10001"
        | "selection2"
        | "selectionpattern2"
        | "10029"
        | "spreadsheet"
        | "spreadsheetpattern"
        | "10031"
        | "spreadsheetitem"
        | "spreadsheetitempattern"
        | "10032"
        | "styles"
        | "stylespattern"
        | "10033" => Ok(UiaPatternClass::Unsupported),
        _ => Err(DesktopError::NodeBound),
    }
}

/// Keep supported patterns; drop unsupported ones. Invalid tokens fail closed.
pub fn map_uia_patterns(raw: &[&str]) -> Result<Vec<WindowsUiaPattern>, DesktopError> {
    if raw.len() > 8 {
        return Err(DesktopError::NodeBound);
    }
    let mut out = Vec::new();
    for name in raw {
        match classify_uia_pattern(name)? {
            UiaPatternClass::Supported(pattern) if !out.contains(&pattern) => out.push(pattern),
            UiaPatternClass::Supported(_) | UiaPatternClass::Unsupported => {}
        }
    }
    Ok(out)
}

/// Whether any advertised pattern can perform `kind`.
pub fn uia_pattern_supports_action(patterns: &[WindowsUiaPattern], kind: ActionKind) -> bool {
    match kind {
        ActionKind::Key | ActionKind::Chord => true,
        ActionKind::LaunchApp | ActionKind::CoordinateFallback => false,
        _ => patterns.iter().any(|pattern| pattern.supports(kind)),
    }
}

impl WindowsUiaProbe {
    pub const fn new(windows: bool, uia: bool, display: bool) -> Self {
        Self {
            windows,
            uia,
            display,
        }
    }

    pub const fn windows(self) -> bool {
        self.windows
    }

    pub const fn uia(self) -> bool {
        self.uia
    }

    pub const fn display(self) -> bool {
        self.display
    }
}

impl WindowsUiaWindow {
    pub fn new(
        id: &str,
        title: &str,
        app: Option<&str>,
        bounds: super::backend::Rect,
        focused: bool,
        sensitive: bool,
        patterns: &[WindowsUiaPattern],
    ) -> Result<Self, DesktopError> {
        if patterns.len() > 8 {
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
            patterns: patterns.to_vec(),
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

    pub fn patterns(&self) -> &[WindowsUiaPattern] {
        &self.patterns
    }
}

impl WindowsUiaNode {
    pub fn new(
        id: &str,
        window_id: &str,
        control_type: &str,
        name: &str,
        automation_id: Option<&str>,
        patterns: &[WindowsUiaPattern],
        is_password: bool,
        sensitive: bool,
    ) -> Result<Self, DesktopError> {
        if patterns.len() > 8 {
            return Err(DesktopError::NodeBound);
        }
        Ok(Self {
            id: bound_token(id, MAX_STABLE_REF_BYTES, DesktopError::NodeBound)?,
            window_id: bound_token(window_id, MAX_STABLE_REF_BYTES, DesktopError::NodeBound)?,
            control_type: bound_token(control_type, MAX_ROLE_BYTES + 16, DesktopError::NodeBound)?,
            name: bound_optional(name, MAX_NAME_BYTES, DesktopError::NodeBound)?,
            automation_id: match automation_id {
                Some(value) => Some(bound_token(value, MAX_ROLE_BYTES, DesktopError::NodeBound)?),
                None => None,
            },
            patterns: patterns.to_vec(),
            is_password,
            sensitive,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn window_id(&self) -> &str {
        &self.window_id
    }

    pub fn control_type(&self) -> &str {
        &self.control_type
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn automation_id(&self) -> Option<&str> {
        self.automation_id.as_deref()
    }

    pub fn patterns(&self) -> &[WindowsUiaPattern] {
        &self.patterns
    }

    pub fn is_password(&self) -> bool {
        self.is_password
    }

    pub fn is_sensitive(&self) -> bool {
        self.sensitive
    }
}

impl WindowsUiaSnapshot {
    pub fn new(
        generation: u64,
        geometry: DisplayGeometry,
        windows: Vec<WindowsUiaWindow>,
        nodes: Vec<WindowsUiaNode>,
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

    pub fn windows(&self) -> &[WindowsUiaWindow] {
        &self.windows
    }

    pub fn nodes(&self) -> &[WindowsUiaNode] {
        &self.nodes
    }
}

impl WindowsDesktopBackend<LiveWindowsUiaHost> {
    pub fn live() -> Self {
        Self {
            host: LiveWindowsUiaHost,
        }
    }
}

impl WindowsDesktopBackend<ScriptedWindowsUiaHost> {
    pub fn scripted(host: ScriptedWindowsUiaHost) -> Self {
        Self { host }
    }
}

impl<H: WindowsUiaHost> WindowsDesktopBackend<H> {
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

impl<H: WindowsUiaHost> DesktopBackend for WindowsDesktopBackend<H> {
    fn capabilities(&self) -> DesktopCapabilities {
        match self.host.probe(&CancellationToken::new()) {
            Ok(probe) if probe.windows() => DesktopCapabilities::semantic(DesktopPlatform::Windows),
            _ => DesktopCapabilities::unavailable(DesktopPlatform::Windows),
        }
    }

    fn health(&self, cancel: &CancellationToken) -> Result<DesktopHealth, DesktopError> {
        check_cancel(cancel)?;
        let probe = self.host.probe(cancel)?;
        if !probe.windows() {
            return Ok(DesktopHealth::unavailable(
                DesktopHealthReason::PlatformUnsupported,
            ));
        }
        if !probe.display() {
            return Ok(DesktopHealth::unavailable(
                DesktopHealthReason::DisplayUnavailable,
            ));
        }
        if !probe.uia() {
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

impl LiveWindowsUiaHost {
    pub const fn new() -> Self {
        Self
    }
}

impl WindowsUiaHost for LiveWindowsUiaHost {
    fn probe(&self, cancel: &CancellationToken) -> Result<WindowsUiaProbe, DesktopError> {
        check_cancel(cancel)?;
        // Never spawn powershell.exe / cmd.exe / wscript as a UIA substitute.
        // This crate does not link UIAutomationCore (`forbid(unsafe_code)`),
        // so UIA cannot be confirmed and is reported unavailable rather than
        // assumed present.
        Ok(WindowsUiaProbe::new(
            cfg!(target_os = "windows"),
            false,
            cfg!(target_os = "windows"),
        ))
    }

    fn snapshot(
        &self,
        _session: DesktopSessionId,
        request: &DesktopObserveRequest,
    ) -> Result<WindowsUiaSnapshot, DesktopError> {
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

impl ScriptedWindowsUiaHost {
    pub fn granted() -> Self {
        Self::with_probe(true, true, true)
    }

    pub fn uia_unavailable() -> Self {
        Self::with_probe(true, false, true)
    }

    pub fn not_windows() -> Self {
        Self::with_probe(false, false, false)
    }

    fn with_probe(windows_os: bool, uia: bool, display: bool) -> Self {
        Self {
            state: Mutex::new(ScriptedState {
                windows_os,
                uia,
                display,
                shell_fallback_attempts: 0,
                generation: 1,
                geometry: default_geometry(),
                windows: Vec::new(),
                nodes: Vec::new(),
                live_elements: HashSet::new(),
                last_kind: None,
                last_target: None,
                last_timeout: None,
                last_pattern: None,
                secret_handle_used: None,
            }),
        }
    }

    pub fn install(
        &self,
        windows: Vec<WindowsUiaWindow>,
        nodes: Vec<WindowsUiaNode>,
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

    pub fn set_uia(&self, uia: bool) -> Result<(), DesktopError> {
        let mut state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        state.uia = uia;
        Ok(())
    }

    pub fn set_display(&self, display: bool) -> Result<(), DesktopError> {
        let mut state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        state.display = display;
        Ok(())
    }

    /// Mark a UIA window or node invalid without spawning a shell.
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

    /// Shell/PowerShell fallback count. Adapter paths must leave this at zero.
    pub fn shell_fallback_attempts(&self) -> Result<u32, DesktopError> {
        let state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        Ok(state.shell_fallback_attempts)
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

    pub fn last_pattern(&self) -> Result<Option<WindowsUiaPattern>, DesktopError> {
        let state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        Ok(state.last_pattern)
    }

    pub fn secret_handle_used(&self) -> Result<Option<String>, DesktopError> {
        let state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        Ok(state.secret_handle_used.clone())
    }
}

impl WindowsUiaHost for ScriptedWindowsUiaHost {
    fn probe(&self, cancel: &CancellationToken) -> Result<WindowsUiaProbe, DesktopError> {
        check_cancel(cancel)?;
        let state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        Ok(WindowsUiaProbe::new(
            state.windows_os,
            state.uia,
            state.display,
        ))
    }

    fn snapshot(
        &self,
        _session: DesktopSessionId,
        request: &DesktopObserveRequest,
    ) -> Result<WindowsUiaSnapshot, DesktopError> {
        check_cancel(request.cancel())?;
        let state = self.state.lock().map_err(|_| DesktopError::Unavailable)?;
        if !state.windows_os {
            return Err(DesktopError::HealthFailed);
        }
        if !state.uia {
            return Err(DesktopError::HealthFailed);
        }
        WindowsUiaSnapshot::new(
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
        if !state.windows_os || !state.uia {
            return Err(DesktopError::HealthFailed);
        }
        if let Some(target) = resolved.node().or(resolved.window()) {
            if !state.live_elements.contains(target) {
                return Err(DesktopError::StaleObservation);
            }
        }
        let patterns = patterns_for(&state, resolved)?.to_vec();
        if !uia_pattern_supports_action(&patterns, action.kind()) {
            // No Invoke/Value/Window/… match and no PowerShell SendKeys path.
            return Err(DesktopError::CapabilityUnavailable);
        }
        state.last_kind = Some(action.kind());
        state.last_target = resolved
            .node()
            .map(str::to_owned)
            .or_else(|| resolved.window().map(str::to_owned));
        state.last_timeout = Some(timeout);
        state.last_pattern = patterns
            .iter()
            .copied()
            .find(|pattern| pattern.supports(action.kind()));
        state.secret_handle_used = secret_handle_used(action);
        Ok(())
    }
}

/// Map a UIA ControlType (`Button`, `UIA_ButtonControlTypeId`, `50000`) onto
/// the desktop semantic role (`button`). LocalizedControlType is untrusted
/// data and is not accepted as a role (T-CU-01).
pub fn normalize_uia_control_type(raw: &str) -> Result<String, DesktopError> {
    let key = normalize_uia_token(raw, DesktopError::NodeBound)?;
    let mapped = match key.as_str() {
        "button"
        | "splitbutton"
        | "uia_buttoncontroltypeid"
        | "50000"
        | "uia_splitbuttoncontroltypeid"
        | "50031" => "button",
        "checkbox" | "uia_checkboxcontroltypeid" | "50002" => "checkbox",
        "radiobutton" | "uia_radiobuttoncontroltypeid" | "50013" => "radiobutton",
        "edit"
        | "document"
        | "uia_editcontroltypeid"
        | "50004"
        | "uia_documentcontroltypeid"
        | "50030" => "textbox",
        "text" | "uia_textcontroltypeid" | "50020" => "statictext",
        "hyperlink" | "uia_hyperlinkcontroltypeid" | "50005" => "link",
        "image" | "uia_imagecontroltypeid" | "50006" => "image",
        "combobox" | "uia_comboboxcontroltypeid" | "50003" => "combobox",
        "menuitem" | "uia_menuitemcontroltypeid" | "50011" => "menuitem",
        "menu"
        | "menubar"
        | "uia_menucontroltypeid"
        | "50009"
        | "uia_menubarcontroltypeid"
        | "50010" => "menu",
        "slider"
        | "thumb"
        | "uia_slidercontroltypeid"
        | "50015"
        | "uia_thumbcontroltypeid"
        | "50027" => "slider",
        "spinner" | "uia_spinnercontroltypeid" | "50016" => "spinbutton",
        "tab"
        | "tabitem"
        | "uia_tabcontroltypeid"
        | "50018"
        | "uia_tabitemcontroltypeid"
        | "50019" => "tab",
        "scrollbar" | "uia_scrollbarcontroltypeid" | "50014" => "scrollbar",
        "window" | "uia_windowcontroltypeid" | "50032" => "window",
        "dialog" => "dialog",
        "group"
        | "pane"
        | "uia_groupcontroltypeid"
        | "50026"
        | "uia_panecontroltypeid"
        | "50033" => "group",
        "toolbar" | "uia_toolbarcontroltypeid" | "50021" => "toolbar",
        "tree" | "uia_treecontroltypeid" | "50023" => "tree",
        "treeitem" | "uia_treeitemcontroltypeid" | "50024" => "treeitem",
        "datagrid"
        | "table"
        | "uia_datagridcontroltypeid"
        | "50028"
        | "uia_tablecontroltypeid"
        | "50036" => "table",
        "dataitem" | "uia_dataitemcontroltypeid" | "50029" => "row",
        "header"
        | "headeritem"
        | "uia_headercontroltypeid"
        | "50034"
        | "uia_headeritemcontroltypeid"
        | "50035" => "column",
        "list" | "uia_listcontroltypeid" | "50008" => "list",
        "listitem" | "uia_listitemcontroltypeid" | "50007" => "listitem",
        "heading" | "titlebar" | "uia_titlebarcontroltypeid" | "50037" => "heading",
        "progressbar" | "uia_progressbarcontroltypeid" | "50012" => "progressbar",
        "statusbar" | "uia_statusbarcontroltypeid" | "50017" => "statusbar",
        "tooltip" | "uia_tooltipcontroltypeid" | "50022" => "tooltip",
        "separator" | "uia_separatorcontroltypeid" | "50038" => "separator",
        "calendar" | "uia_calendarcontroltypeid" | "50001" => "calendar",
        "custom" | "uia_customcontroltypeid" | "50025" => "custom",
        "appbar" | "uia_appbarcontroltypeid" | "50040" => "toolbar",
        "semanticzoom" | "uia_semanticzoomcontroltypeid" | "50039" => "group",
        other if is_role_token(other) => other,
        _ => return Err(DesktopError::NodeBound),
    };
    if mapped.len() > MAX_ROLE_BYTES {
        return Err(DesktopError::NodeBound);
    }
    Ok(mapped.to_owned())
}

/// Password fields are sensitive regardless of untrusted Name text (T-CU-03).
/// UIA has no Password ControlType; `IsPassword` is authoritative.
pub fn uia_element_is_password(_control_type: &str, is_password: bool) -> bool {
    is_password
}

fn map_snapshot(snapshot: WindowsUiaSnapshot) -> Result<DesktopCapture, DesktopError> {
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

fn map_window(window: &WindowsUiaWindow) -> Result<DesktopWindowCapture, DesktopError> {
    let sensitive = window.sensitive || is_system_secure_desktop_app(window.app.as_deref());
    DesktopWindowCapture::new(
        &window_stable_ref(&window.id)?,
        &window.title,
        window.app.as_deref(),
        window.bounds,
        window.focused,
        sensitive,
    )
}

fn map_node(node: &WindowsUiaNode) -> Result<DesktopNodeCapture, DesktopError> {
    let role = normalize_uia_control_type(&node.control_type)?;
    let sensitive = node.sensitive || uia_element_is_password(&node.control_type, node.is_password);
    let interactive = node_is_interactive(&role, &node.patterns);
    DesktopNodeCapture::new(
        &window_stable_ref(&node.window_id)?,
        &node_stable_ref(&node.id)?,
        &role,
        &node.name,
        node.automation_id.as_deref(),
        interactive,
        sensitive,
    )
}

fn node_is_interactive(role: &str, patterns: &[WindowsUiaPattern]) -> bool {
    if patterns.iter().any(|pattern| pattern.is_interactive()) {
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

fn is_system_secure_desktop_app(app: Option<&str>) -> bool {
    match app {
        Some(name) => {
            let key = name.to_ascii_lowercase();
            matches!(
                key.as_str(),
                "consent"
                    | "consent.exe"
                    | "consentui"
                    | "logonui"
                    | "logonui.exe"
                    | "credentialuibroker"
                    | "credentialuibroker.exe"
                    | "windows.security"
                    | "useraccountcontrol"
                    | "windows security"
            )
        }
        None => false,
    }
}

fn patterns_for<'a>(
    state: &'a ScriptedState,
    resolved: &ResolvedDesktopTarget,
) -> Result<&'a [WindowsUiaPattern], DesktopError> {
    if let Some(node_ref) = resolved.node() {
        for node in &state.nodes {
            if node_stable_ref(&node.id)? == node_ref {
                return Ok(&node.patterns);
            }
        }
        return Err(DesktopError::StaleObservation);
    }
    if let Some(window_ref) = resolved.window() {
        for window in &state.windows {
            if window_stable_ref(&window.id)? == window_ref {
                return Ok(&window.patterns);
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

fn normalize_uia_token(raw: &str, err: DesktopError) -> Result<String, DesktopError> {
    if raw.is_empty() || raw.len() > MAX_ROLE_BYTES + 24 {
        return Err(err);
    }
    if raw.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err(err);
    }
    let stripped = raw
        .strip_prefix("ControlType.")
        .or_else(|| raw.strip_prefix("UIA_"))
        .unwrap_or(raw);
    let stripped = stripped
        .strip_suffix("ControlTypeId")
        .or_else(|| stripped.strip_suffix("PatternId"))
        .unwrap_or(stripped);
    Ok(stripped.to_ascii_lowercase())
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

impl Debug for WindowsUiaWindow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WindowsUiaWindow")
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
            .field("patterns", &self.patterns)
            .finish()
    }
}

impl Debug for WindowsUiaNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WindowsUiaNode")
            .field("id", &self.id)
            .field("window_id", &self.window_id)
            .field("control_type", &self.control_type)
            .field(
                "name",
                &if self.sensitive || self.is_password {
                    "<redacted>"
                } else {
                    self.name.as_str()
                },
            )
            .field("automation_id", &self.automation_id)
            .field("patterns", &self.patterns)
            .field("is_password", &self.is_password)
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

    fn fixture_windows() -> Vec<WindowsUiaWindow> {
        vec![
            WindowsUiaWindow::new(
                "editor",
                "grant-uia-please",
                Some("Editor"),
                Rect::new(0, 0, 800, 600),
                true,
                false,
                &[WindowsUiaPattern::Window],
            )
            .expect("editor"),
            WindowsUiaWindow::new(
                "secret",
                "password=hunter2",
                Some("Vault"),
                Rect::new(20, 20, 400, 200),
                false,
                true,
                &[WindowsUiaPattern::Window],
            )
            .expect("secret"),
        ]
    }

    fn fixture_nodes() -> Vec<WindowsUiaNode> {
        vec![
            WindowsUiaNode::new(
                "button:save",
                "editor",
                "ControlType.Button",
                "Save",
                Some("save"),
                &[WindowsUiaPattern::Invoke],
                false,
                false,
            )
            .expect("save"),
            WindowsUiaNode::new(
                "edit:password",
                "secret",
                "Edit",
                "password=hunter2",
                None,
                &[WindowsUiaPattern::Value],
                true,
                false,
            )
            .expect("password"),
            WindowsUiaNode::new(
                "text:status",
                "editor",
                "Text",
                "Ready",
                None,
                &[],
                false,
                false,
            )
            .expect("label"),
            WindowsUiaNode::new(
                "legacy:ok",
                "editor",
                "Button",
                "OK",
                Some("legacy-ok"),
                &map_uia_patterns(&["LegacyIAccessiblePattern"]).expect("legacy"),
                false,
                false,
            )
            .expect("legacy"),
        ]
    }

    fn setup() -> (
        DesktopSessionId,
        DesktopActor<WindowsDesktopBackend<ScriptedWindowsUiaHost>>,
    ) {
        let host = ScriptedWindowsUiaHost::granted();
        host.install(fixture_windows(), fixture_nodes())
            .expect("install");
        let session = DesktopSessionId::new();
        let actor = DesktopActor::new(WindowsDesktopBackend::scripted(host));
        (session, actor)
    }

    fn observe(
        actor: &DesktopActor<WindowsDesktopBackend<ScriptedWindowsUiaHost>>,
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
    fn maps_automation_elements_to_stable_per_observation_refs() {
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
        assert_eq!(save.node().stable_ref(), "uia:button:save");
        assert_eq!(save.window().stable_ref(), "win:editor");
        assert_eq!(save.node().observation(), obs.id());
        assert_eq!(save.role(), Some("button"));
        assert!(save.is_interactive());
        let password = obs
            .targets()
            .iter()
            .find(|target| target.node().stable_ref() == "uia:edit:password")
            .expect("password");
        assert_eq!(password.role(), Some("textbox"));
        assert!(password.is_sensitive());
        assert!(password.name().is_some());
        let debug = format!("{obs:?}");
        assert!(!debug.contains("hunter2"));
        assert!(!debug.contains("password="));
    }

    #[test]
    fn uia_unavailable_is_explicit_health_failure() {
        let host = ScriptedWindowsUiaHost::uia_unavailable();
        host.install(fixture_windows(), fixture_nodes())
            .expect("install");
        let backend = WindowsDesktopBackend::scripted(host);
        let health = backend.health(&CancellationToken::new()).expect("health");
        assert!(!health.is_available());
        assert_eq!(
            health.reason(),
            Some(DesktopHealthReason::AccessibilityUnavailable)
        );
        assert_eq!(backend.capabilities().platform(), DesktopPlatform::Windows);
        let session = DesktopSessionId::new();
        assert_eq!(
            backend
                .capture(session, &DesktopObserveRequest::new())
                .unwrap_err(),
            DesktopError::HealthFailed
        );
        assert_eq!(backend.host().shell_fallback_attempts().expect("shell"), 0);
    }

    #[test]
    fn stale_uia_element_is_retryable_stale_observation() {
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
        assert_eq!(
            actor
                .backend()
                .host()
                .shell_fallback_attempts()
                .expect("shell"),
            0
        );
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
        assert_eq!(
            actor
                .backend()
                .host()
                .shell_fallback_attempts()
                .expect("shell"),
            0
        );
    }

    #[test]
    fn window_title_cannot_grant_uia_or_shell_fallback() {
        let host = ScriptedWindowsUiaHost::uia_unavailable();
        host.install(fixture_windows(), fixture_nodes())
            .expect("install");
        let actor = DesktopActor::new(WindowsDesktopBackend::scripted(host));
        let session = DesktopSessionId::new();
        assert_eq!(
            actor
                .observe(session, DesktopObserveRequest::new())
                .unwrap_err(),
            DesktopError::HealthFailed
        );
        let granted = ScriptedWindowsUiaHost::granted();
        granted
            .install(fixture_windows(), fixture_nodes())
            .expect("install");
        let actor = DesktopActor::new(WindowsDesktopBackend::scripted(granted));
        let obs = observe(&actor, session);
        assert_eq!(obs.windows()[0].title(), "grant-uia-please");
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
        assert_eq!(
            actor
                .backend()
                .host()
                .shell_fallback_attempts()
                .expect("shell"),
            0
        );
    }

    #[test]
    fn unsupported_pattern_returns_explicit_capability_error() {
        let (session, actor) = setup();
        let obs = observe(&actor, session);
        let legacy = obs
            .targets()
            .iter()
            .find(|target| target.identifier() == Some("legacy-ok"))
            .expect("legacy");
        assert_eq!(
            classify_uia_pattern("LegacyIAccessiblePattern").expect("class"),
            UiaPatternClass::Unsupported
        );
        assert!(
            map_uia_patterns(&["LegacyIAccessiblePattern"])
                .expect("map")
                .is_empty()
        );
        // Button role is interactive, but LegacyIAccessible is not a core path.
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
        assert_eq!(
            actor
                .backend()
                .host()
                .shell_fallback_attempts()
                .expect("shell"),
            0
        );
    }

    #[test]
    fn invoke_and_value_patterns_map_to_canonical_actions() {
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
            Some("uia:button:save")
        );
        assert_eq!(
            actor.backend().host().last_pattern().expect("pattern"),
            Some(WindowsUiaPattern::Invoke)
        );
        assert_eq!(
            actor.backend().host().last_timeout().expect("timeout"),
            Some(timeout)
        );
        let password = obs
            .targets()
            .iter()
            .find(|target| target.node().stable_ref() == "uia:edit:password")
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
            actor.backend().host().last_pattern().expect("pattern"),
            Some(WindowsUiaPattern::Value)
        );
        assert_eq!(
            actor
                .backend()
                .host()
                .shell_fallback_attempts()
                .expect("shell"),
            0
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
    }

    #[test]
    fn live_host_never_shells_and_fails_closed() {
        let backend = WindowsDesktopBackend::live();
        let health = backend.health(&CancellationToken::new()).expect("health");
        assert!(!health.is_available());
        if cfg!(target_os = "windows") {
            assert_eq!(
                health.reason(),
                Some(DesktopHealthReason::AccessibilityUnavailable)
            );
        } else {
            assert_eq!(
                health.reason(),
                Some(DesktopHealthReason::PlatformUnsupported)
            );
        }
        let session = DesktopSessionId::new();
        assert_eq!(
            backend
                .capture(session, &DesktopObserveRequest::new())
                .unwrap_err(),
            DesktopError::HealthFailed
        );
    }

    #[test]
    fn scripted_non_windows_is_platform_unsupported() {
        let backend = WindowsDesktopBackend::scripted(ScriptedWindowsUiaHost::not_windows());
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
            .find(|target| target.node().stable_ref() == "uia:edit:password")
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
    fn non_interactive_uia_target_is_rejected() {
        let (session, actor) = setup();
        let obs = observe(&actor, session);
        let label = obs
            .targets()
            .iter()
            .find(|target| target.node().stable_ref() == "uia:text:status")
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
    fn normalize_uia_control_type_table() {
        assert_eq!(
            normalize_uia_control_type("ControlType.Button").expect("button"),
            "button"
        );
        assert_eq!(normalize_uia_control_type("50000").expect("id"), "button");
        assert_eq!(
            normalize_uia_control_type("UIA_EditControlTypeId").expect("edit"),
            "textbox"
        );
        assert_eq!(
            normalize_uia_control_type("Hyperlink").expect("link"),
            "link"
        );
        assert!(uia_element_is_password("Edit", true));
        assert!(!uia_element_is_password("Edit", false));
        assert_eq!(
            normalize_uia_control_type("Button\n").unwrap_err(),
            DesktopError::NodeBound
        );
        assert_eq!(
            classify_uia_pattern("UIA_InvokePatternId").expect("invoke"),
            UiaPatternClass::Supported(WindowsUiaPattern::Invoke)
        );
        assert_eq!(
            classify_uia_pattern("10018").expect("legacy"),
            UiaPatternClass::Unsupported
        );
        assert_eq!(
            classify_uia_pattern("powershell").unwrap_err(),
            DesktopError::NodeBound
        );
    }

    #[test]
    fn system_secure_desktop_app_is_classified_sensitive() {
        let host = ScriptedWindowsUiaHost::granted();
        host.install(
            vec![
                WindowsUiaWindow::new(
                    "uac",
                    "User Account Control",
                    Some("Consent.exe"),
                    Rect::new(0, 0, 400, 200),
                    true,
                    false,
                    &[WindowsUiaPattern::Window],
                )
                .expect("uac"),
            ],
            Vec::new(),
        )
        .expect("install");
        let actor = DesktopActor::new(WindowsDesktopBackend::scripted(host));
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
        assert_eq!(
            actor
                .backend()
                .host()
                .shell_fallback_attempts()
                .expect("shell"),
            0
        );
    }

    #[test]
    fn missing_window_pattern_is_capability_unavailable() {
        let host = ScriptedWindowsUiaHost::granted();
        host.install(
            vec![
                WindowsUiaWindow::new(
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
        let actor = DesktopActor::new(WindowsDesktopBackend::scripted(host));
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
        assert_eq!(
            actor
                .backend()
                .host()
                .shell_fallback_attempts()
                .expect("shell"),
            0
        );
    }
}
