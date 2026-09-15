//! macOS AX adapter behind [`DesktopBackend`].
//!
//! AX windows/nodes become stable-per-observation target refs. Trust is
//! probed without prompting (T-CU-01). Stale AX elements map to retryable
//! [`DesktopError::StaleObservation`] (T-CU-02). Secret handles stay opaque
//! (T-CU-03). Coordinate injection is never implied by AX support (T-CU-02).

use std::collections::{HashMap, HashSet};
use std::fmt::{self, Debug};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

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

/// Live macOS host: real Accessibility access through the `osascript` /
/// System Events scripting bridge — the automation surface the OS supports
/// without this crate linking ApplicationServices (it is
/// `forbid(unsafe_code)`). The probe never prompts for, grants, or changes
/// OS privacy settings: a TCC consent dialog that is still pending surfaces
/// as a bounded timeout, reported exactly like a denial.
#[derive(Debug, Default)]
pub struct LiveMacosAxHost {
    /// Snapshot generation counter; refs carry it, so a ref from an older
    /// observation never resolves against a newer snapshot's locators. The
    /// generation identifies a STATE version, not a call count: an
    /// unchanged desktop re-captures under the SAME generation (the actor's
    /// `require_current` recaptures and demands an unchanged generation
    /// between observe and act), and only a changed walk bumps it.
    generation: AtomicU64,
    /// Content hash → generation of the last snapshot, for the
    /// unchanged-state check above.
    last_state: Mutex<Option<(u64, u64)>>,
    /// Observation-bound element locators from the LATEST snapshot: node or
    /// window ref → its `System Events` path. Replaced wholesale on every
    /// snapshot, so a stale ref misses and the backend maps the miss to the
    /// retryable [`DesktopError::StaleObservation`] (T-CU-02).
    locators: Mutex<HashMap<String, ElementLocator>>,
}

/// Where an observed element lived in the System Events object tree when
/// its snapshot was taken — the path an action re-walks.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ElementLocator {
    process: String,
    /// Window ordinal within the process at snapshot time: the System
    /// Events addressing an act re-walks. Deliberately the ordinal, never
    /// the title — titles churn between observation and act.
    window_index: usize,
    /// UI-element ordinal within the window; `None` for window-level refs.
    element_index: Option<usize>,
    /// Observed AX role, used to pick how a typed value is delivered
    /// (`set value` on text roles, `keystroke` otherwise).
    role: String,
}

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
            host: LiveMacosAxHost::default(),
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
    /// The no-prompt trust probe: System Events reports whether the calling
    /// context has Accessibility (`UI elements enabled`). Requires no Apple
    /// Event the caller could act on; first-ever use may still surface the
    /// OS's own Automation consent dialog — pending consent bounds out to a
    /// timeout, reported as not trusted, never as granted.
    const PROBE_SCRIPT: &str = "tell application \"System Events\" to UI elements enabled";

    /// Bounded AX walk: visible processes → windows (name, position, size)
    /// → first-level UI elements (role, name), as TAB-separated rows.
    /// `F` frontmost process, `W` window, `N` element. Names are untrusted
    /// data and may be `missing value`; every field read is individually
    /// `try`-wrapped so one hostile element cannot fail the walk.
    const SNAPSHOT_SCRIPT: &str = r#"set TAB to character id 9
set LF to linefeed
set out to ""
tell application "System Events"
	set frontApp to ""
	try
		set frontApp to name of first application process whose frontmost is true
	end try
	set out to "F" & TAB & frontApp & LF
	set pcount to 0
	repeat with p in (application processes whose visible is true)
		set pcount to pcount + 1
		if pcount > 6 then exit repeat
		set pname to name of p
		set widx to 0
		try
			repeat with w in windows of p
				set widx to widx + 1
				if widx > 4 then exit repeat
				set wname to ""
				try
					set wname to name of w
				end try
				set wx to 0
				set wy to 0
				set ww to 0
				set wh to 0
				set wfocused to false
				try
					set wx to item 1 of (get position of w)
					set wy to item 2 of (get position of w)
					set ww to item 1 of (get size of w)
					set wh to item 2 of (get size of w)
				end try
				try
					set wfocused to (get value of attribute "AXMain" of w)
				end try
				set out to out & "W" & TAB & pname & TAB & wname & TAB & wx & TAB & wy & TAB & ww & TAB & wh & TAB & wfocused & LF
				set eidx to 0
				try
					repeat with e in UI elements of w
						set eidx to eidx + 1
						if eidx > 24 then exit repeat
						set erole to ""
						try
							set erole to role of e
						end try
						set ename to ""
						try
							set ename to name of e
						end try
						set out to out & "N" & TAB & pname & TAB & widx & TAB & eidx & TAB & erole & TAB & ename & LF
					end repeat
				end try
			end repeat
		end try
	end repeat
end tell
return out"#;

    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve a current-snapshot ref to its locator. A miss is the stale
    /// observation case: the ref names an element from an earlier (or
    /// fabricated) snapshot.
    fn current_locator(&self, id: &str) -> Result<ElementLocator, DesktopError> {
        let locators = self.locators.lock().unwrap_or_else(|p| p.into_inner());
        locators
            .get(id)
            .cloned()
            .ok_or(DesktopError::TargetNotFound)
    }

    /// Translate a validated action against its resolved target into the
    /// AppleScript that performs it. Pure — the caller runs it.
    fn plan_act(
        &self,
        action: &DesktopAction,
        resolved: &ResolvedDesktopTarget,
    ) -> Result<String, DesktopError> {
        match action {
            DesktopAction::Click { .. } => {
                if let Some(node) = resolved.node() {
                    let loc = self.current_locator(node)?;
                    if loc.element_index.is_none() {
                        return Err(DesktopError::TargetNotFound);
                    }
                    Ok(tell_events(&format!(
                        "tell process \"{}\"\n\tclick {}\nend tell",
                        apple_quote(&loc.process),
                        element_path(&loc),
                    )))
                } else if let Some(win) = resolved.window() {
                    let loc = self.current_locator(win)?;
                    Ok(raise_window_script(&loc))
                } else {
                    Err(DesktopError::TargetNotFound)
                }
            }
            DesktopAction::TypeText { value, .. } => {
                // Secret handles never resolve to plaintext here (T-CU-03):
                // the host has no broker, so a handle cannot be typed.
                let SecretAwareString::Literal(text) = value else {
                    return Err(DesktopError::CapabilityUnavailable);
                };
                let quoted = apple_quote(text);
                if let Some(node) = resolved.node() {
                    let loc = self.current_locator(node)?;
                    if is_text_role(&loc.role) {
                        return Ok(tell_events(&format!(
                            "tell process \"{}\"\n\tset value of {} to \"{}\"\nend tell",
                            apple_quote(&loc.process),
                            element_path(&loc),
                            quoted,
                        )));
                    }
                    // Non-text target: keystroke to the frontmost app after
                    // focusing the observed element.
                    let focus = focus_element_statement(&loc);
                    return Ok(tell_events(&format!("{focus}\n\tkeystroke \"{quoted}\"",)));
                }
                Ok(tell_events(&format!("keystroke \"{quoted}\"")))
            }
            DesktopAction::Key { key, target } => {
                let stroke = key_statement(key.as_str())?;
                let focus = match target.as_ref().and_then(|_| resolved.node()) {
                    Some(node) => focus_element_statement(&self.current_locator(node)?),
                    None => String::new(),
                };
                Ok(tell_events(
                    &format!("{focus}\n\t{stroke}").trim_end().to_owned(),
                ))
            }
            DesktopAction::Chord { keys } => {
                // System Events has no simultaneous-chord primitive for
                // arbitrary key sets, and its parseable keys carry no
                // modifiers — a single-key "chord" is a key, anything else
                // is reported unsupported rather than faked.
                let [only] = keys.as_slice() else {
                    return Err(DesktopError::CapabilityUnavailable);
                };
                let stroke = key_statement(only.as_str())?;
                Ok(tell_events(&stroke))
            }
            DesktopAction::FocusWindow { .. } | DesktopAction::CloseWindow { .. } => {
                let win = resolved.window().ok_or(DesktopError::TargetNotFound)?;
                let loc = self.current_locator(win)?;
                if action_matches_window(&loc) {
                    if matches!(action, DesktopAction::CloseWindow { .. }) {
                        Ok(tell_events(&format!(
                            "tell process \"{}\"\n\tclick (first UI element of {} whose subrole is \"AXCloseButton\")\nend tell",
                            apple_quote(&loc.process),
                            window_path(&loc),
                        )))
                    } else {
                        Ok(raise_window_script(&loc))
                    }
                } else {
                    Err(DesktopError::TargetNotFound)
                }
            }
            // The adapter advertises no pointer-wheel, resize, app-launch,
            // or coordinate surface (see `DesktopCapabilities::semantic`);
            // semantic AX support never implies coordinate injection
            // (T-CU-02). Reported unsupported, never faked.
            DesktopAction::Scroll { .. }
            | DesktopAction::ResizeWindow { .. }
            | DesktopAction::LaunchApp { .. }
            | DesktopAction::CoordinateFallback { .. } => Err(DesktopError::CapabilityUnavailable),
        }
    }

    /// Parse one bounded `SNAPSHOT_SCRIPT` output into a snapshot,
    /// recording every element's locator under its observation-bound ref.
    /// Pure — tests feed recorded transcripts.
    fn parse_snapshot(
        &self,
        raw: &str,
        generation: u64,
        locators: &mut HashMap<String, ElementLocator>,
    ) -> Result<MacosAxSnapshot, DesktopError> {
        let mut windows = Vec::new();
        let mut nodes = Vec::new();
        // Window refs are globally unique per snapshot (a per-process
        // ordinal would collide across processes); the locator's
        // window_index stays per-process, which is the System Events
        // addressing space an act re-walks.
        let mut next_window_ordinal = 0usize;
        // (process, window ordinal) → (win ref, observed title) the node
        // rows point at; the title rides on the node's locator because a
        // named window re-anchors an act more stably than an ordinal.
        let mut window_refs: HashMap<(String, usize), (String, String)> = HashMap::new();
        let mut frontmost = String::new();
        for line in raw.lines() {
            let fields: Vec<&str> = line.split('\t').collect();
            match fields.first().copied() {
                // `F` names the frontmost process: a window is only "the
                // focused window" when it is its app's AXMain window AND
                // its app is frontmost (every app has a main window, but
                // only the front app's is the key window).
                Some("F") => {
                    frontmost = fields.get(1).copied().unwrap_or("").trim().to_owned();
                }
                Some("W") if fields.len() >= 8 => {
                    let nums = &fields[fields.len() - 5..];
                    let (x, y, w, h) = match (
                        nums[0].trim().parse::<i32>(),
                        nums[1].trim().parse::<i32>(),
                        nums[2].trim().parse::<i32>(),
                        nums[3].trim().parse::<i32>(),
                    ) {
                        (Ok(x), Ok(y), Ok(w), Ok(h)) => (x, y, w, h),
                        _ => continue,
                    };
                    let process = fields[1].trim().to_owned();
                    let focused = nums[4].trim() == "true" && process == frontmost;
                    // A tab inside a window title shifts the columns; the
                    // title is everything between the process and the
                    // geometry, joined back with spaces.
                    let title = fields[2..fields.len() - 5].join(" ");
                    let title = clean_field(&title);
                    let index = window_refs
                        .keys()
                        .filter(|(proc, _)| proc == &process)
                        .count()
                        + 1;
                    next_window_ordinal += 1;
                    let id = format!("{}{generation}:{}", WINDOW_REF_PREFIX, next_window_ordinal);
                    let window = MacosAxWindow::new(
                        &id,
                        &title,
                        Some(&process),
                        super::backend::Rect::new(x, y, x.saturating_add(w), y.saturating_add(h)),
                        focused,
                        false,
                    )?;
                    locators.insert(
                        id.clone(),
                        ElementLocator {
                            process: process.clone(),
                            window_index: index,
                            element_index: None,
                            role: String::new(),
                        },
                    );
                    window_refs.insert((process.clone(), index), (id, title));
                    windows.push(window);
                }
                Some("N") if fields.len() >= 6 => {
                    let process = fields[1].trim().to_owned();
                    let (Ok(window_index), Ok(element_index)) = (
                        fields[2].trim().parse::<usize>(),
                        fields[3].trim().parse::<usize>(),
                    ) else {
                        continue;
                    };
                    let role = clean_field(fields[4]);
                    let name = clean_field(&fields[5..].join(" "));
                    let Some((window_id, _window_title)) =
                        window_refs.get(&(process.clone(), window_index))
                    else {
                        continue;
                    };
                    let id = format!("{}{generation}:{}", NODE_REF_PREFIX, nodes.len() + 1);
                    let actions = role_actions(&role);
                    let node = MacosAxNode::new(
                        &id,
                        window_id,
                        &role,
                        &name,
                        None,
                        &actions,
                        role == SECURE_TEXT_ROLE,
                    )?;
                    locators.insert(
                        id,
                        ElementLocator {
                            process,
                            window_index,
                            element_index: Some(element_index),
                            role,
                        },
                    );
                    nodes.push(node);
                    if nodes.len() >= MAX_NODES {
                        break;
                    }
                }
                _ => {}
            }
        }
        MacosAxSnapshot::new(generation, live_geometry(), windows, nodes)
    }
}

/// 1920×1080 stand-in geometry: AX-only observations carry no coordinate
/// targets (`coordinate_fallback` is off), so this bounds derived rects
/// rather than locating anything. Distinct from `default_geometry`'s
/// scripted-test value so a live transcript can never be confused with a
/// scripted one.
fn live_geometry() -> DisplayGeometry {
    DisplayGeometry::new(1920, 1080).expect("1920x1080 is inside the geometry bounds")
}

/// The one role whose value is a secret (T-CU-03).
const SECURE_TEXT_ROLE: &str = "AXSecureTextField";

/// `System Events` path for a locator's window: the snapshot-time ordinal
/// within its process. Deliberately NOT the window title — titles churn
/// (a terminal retitles on every prompt), while the ordinal is stable from
/// observation through the act that follows it.
fn window_path(loc: &ElementLocator) -> String {
    format!("window {}", loc.window_index)
}

fn element_path(loc: &ElementLocator) -> String {
    match loc.element_index {
        Some(index) => format!("UI element {index} of {}", window_path(loc)),
        None => window_path(loc),
    }
}

fn tell_events(body: &str) -> String {
    format!("tell application \"System Events\"\n{body}\nend tell")
}

fn raise_window_script(loc: &ElementLocator) -> String {
    tell_events(&format!(
        "set frontmost of process \"{}\" to true\ntell process \"{}\"\n\tperform action \"AXRaise\" of {}\nend tell",
        apple_quote(&loc.process),
        apple_quote(&loc.process),
        window_path(loc),
    ))
}

fn focus_element_statement(loc: &ElementLocator) -> String {
    format!(
        "tell process \"{}\"\n\tset focused of {} to true\nend tell",
        apple_quote(&loc.process),
        element_path(loc),
    )
}

/// Roles whose value is text the `set value` verb addresses.
fn is_text_role(role: &str) -> bool {
    matches!(
        role,
        "AXTextField" | "AXTextArea" | "AXSearchField" | "AXComboBox"
    )
}

/// Whether a locator denotes a window (not an element within one).
fn action_matches_window(loc: &ElementLocator) -> bool {
    loc.element_index.is_none()
}

/// The AX actions an observed role affords. Conservative: a role not in
/// this table affords nothing.
fn role_actions(role: &str) -> Vec<MacosAxAction> {
    let press = matches!(
        role,
        "AXButton"
            | "AXLink"
            | "AXCheckBox"
            | "AXRadioButton"
            | "AXPopUpButton"
            | "AXMenuButton"
            | "AXMenuBarItem"
            | "AXMenuItem"
    );
    let value = is_text_role(role) || role == SECURE_TEXT_ROLE;
    let pick = matches!(role, "AXMenuItem" | "AXMenu");
    let confirm = role == "AXCheckBox" || is_text_role(role);
    let mut actions = Vec::new();
    if press {
        actions.push(MacosAxAction::Press);
    }
    if value {
        actions.push(MacosAxAction::SetValue);
    }
    if confirm {
        actions.push(MacosAxAction::Confirm);
    }
    if pick {
        actions.push(MacosAxAction::Pick);
    }
    actions
}

/// Named key → AppleScript `key code`; a single character → `keystroke`.
fn key_statement(raw: &str) -> Result<String, DesktopError> {
    const ENTER: u32 = 36;
    let code = match raw {
        "Enter" => ENTER,
        "Tab" => 48,
        "Escape" => 53,
        "Backspace" => 51,
        "Delete" => 117,
        "Space" => 49,
        "Home" => 115,
        "End" => 119,
        "PageUp" => 116,
        "PageDown" => 121,
        "ArrowUp" => 126,
        "ArrowDown" => 125,
        "ArrowLeft" => 123,
        "ArrowRight" => 124,
        other => {
            let mut chars = other.chars();
            let (Some(only), None) = (chars.next(), chars.next()) else {
                return Err(DesktopError::KeyInvalid);
            };
            return Ok(format!("keystroke \"{}\"", apple_quote(&only.to_string())));
        }
    };
    Ok(format!("key code {code}"))
}

/// Escape `text` as an AppleScript double-quoted literal body: backslashes
/// and quotes escaped, control characters dropped (never sent to a shell
/// or the scripting bridge).
fn apple_quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if (c as u32) < 0x20 || (c as u32) == 0x7f => {}
            c => out.push(c),
        }
    }
    out
}

/// Normalize an observed field: `missing value` and bounds-checking
/// sentinels become empty, embedded tabs/newlines collapse to spaces.
fn clean_field(text: &str) -> String {
    let cleaned = text.replace(['\t', '\n', '\r'], " ");
    if cleaned == "missing value" {
        String::new()
    } else {
        cleaned
    }
}

/// Drain one child pipe to the end on its own thread, capped at `cap`
/// bytes. Reading only after `wait` would deadlock on a full pipe.
fn join_drain<R: std::io::Read + Send + 'static>(
    pipe: Option<R>,
    cap: usize,
) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = std::io::Read::read_to_end(&mut pipe, &mut buf);
            buf.truncate(cap);
        }
        String::from_utf8_lossy(&buf).into_owned()
    })
}

/// One bounded `osascript` run. Stdout can be substantial (a snapshot), so
/// both pipes are drained on dedicated threads — no wait-then-read
/// deadlock on a filled pipe. Cancel kills the child and reports
/// [`DesktopError::Cancelled`]; the timeout kills it and reports
/// [`DesktopError::Backend`] (the probe maps its own timeout to a
/// not-trusted verdict).
fn run_osascript(
    script: &str,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<String, DesktopError> {
    if !cfg!(target_os = "macos") {
        return Err(DesktopError::HealthFailed);
    }
    let started = Instant::now();
    let mut child = std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|_| DesktopError::HealthFailed)?;
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    // Both pipes drain on dedicated threads: reading only after `wait`
    // would deadlock once a snapshot fills the 64KiB pipe buffer.
    let stdout_task = join_drain(stdout_pipe, 1024 * 1024);
    let stderr_task = join_drain(stderr_pipe, 4096);
    let outcome = loop {
        if cancel.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            break Err(DesktopError::Cancelled);
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = stdout_task.join().unwrap_or_default();
                let stderr = stderr_task.join().unwrap_or_default();
                if status.success() {
                    break Ok(stdout);
                }
                break Err(classify_osascript_failure(&stderr));
            }
            Ok(None) => {
                if started.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(DesktopError::Backend);
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => break Err(DesktopError::Backend),
        }
    };
    outcome
}

/// Map `osascript`'s failure output onto the typed error: an element that
/// no longer exists is the retryable stale-observation case, permission
/// errors stay typed, everything else is a backend failure.
fn classify_osascript_failure(stderr: &str) -> DesktopError {
    if stderr.contains("-1728")
        || stderr.contains("Can\u{2019}t get")
        || stderr.contains("Can't get")
    {
        DesktopError::TargetNotFound
    } else if stderr.contains("-1743")
        || stderr.contains("-25211")
        || stderr.contains("not allowed")
        || stderr.contains("assistive")
    {
        DesktopError::PermissionMissing
    } else {
        DesktopError::Backend
    }
}

impl MacosAxHost for LiveMacosAxHost {
    fn probe(&self, cancel: &CancellationToken) -> Result<MacosAxProbe, DesktopError> {
        check_cancel(cancel)?;
        if !cfg!(target_os = "macos") {
            return Ok(MacosAxProbe::new(false, false, false));
        }
        // Bounded, no-prompt: a pending consent dialog times out into the
        // same "not trusted" verdict as an explicit denial.
        let trusted = matches!(
            run_osascript(Self::PROBE_SCRIPT, Duration::from_secs(3), cancel),
            Ok(output) if output.trim() == "true"
        );
        Ok(MacosAxProbe::new(true, trusted, true))
    }

    fn snapshot(
        &self,
        _session: DesktopSessionId,
        request: &DesktopObserveRequest,
    ) -> Result<MacosAxSnapshot, DesktopError> {
        check_cancel(request.cancel())?;
        if !cfg!(target_os = "macos") {
            return Err(DesktopError::HealthFailed);
        }
        let raw = run_osascript(Self::SNAPSHOT_SCRIPT, request.timeout(), request.cancel())?;
        // The generation is the STATE version: unchanged content keeps the
        // previous generation (an act's require_current recapture must see
        // the same generation its observation had); changed content bumps.
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&raw, &mut hasher);
        let state = std::hash::Hasher::finish(&hasher);
        let mut last = self.last_state.lock().unwrap_or_else(|p| p.into_inner());
        let generation = match *last {
            Some((prev_state, prev_generation)) if prev_state == state => prev_generation,
            _ => self.generation.fetch_add(1, Ordering::SeqCst) + 1,
        };
        *last = Some((state, generation));
        let mut locators = HashMap::new();
        let snapshot = self.parse_snapshot(&raw, generation, &mut locators)?;
        *self.locators.lock().unwrap_or_else(|p| p.into_inner()) = locators;
        Ok(snapshot)
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
        if !cfg!(target_os = "macos") {
            return Err(DesktopError::HealthFailed);
        }
        let script = self.plan_act(action, resolved)?;
        run_osascript(&script, timeout, cancel).map(|_| ())
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
    fn live_host_probe_tracks_the_bridge_and_still_fails_closed_without_trust() {
        // The live host runs the real bounded System Events check, so its
        // verdict tracks the actual OS trust state instead of the hardcoded
        // "never trusted" the stub reported. The contract that must hold
        // everywhere: the verdict is typed, bounded, and on the bridge's
        // answer — never assumed granted, and a pending consent dialog
        // counts as not granted. Off the platform, everything fails closed.
        let cancel = CancellationToken::new();
        let probe = LiveMacosAxHost::default()
            .probe(&cancel)
            .expect("typed probe verdict");
        if !cfg!(target_os = "macos") {
            assert!(!probe.macos() && !probe.trusted());
            let backend = MacosDesktopBackend::live();
            let health = backend.health(&cancel).expect("health");
            assert!(!health.is_available());
            assert_eq!(
                health.reason(),
                Some(DesktopHealthReason::PlatformUnsupported)
            );
            let err = backend
                .capture(DesktopSessionId::new(), &DesktopObserveRequest::new())
                .unwrap_err();
            assert_eq!(err, DesktopError::HealthFailed);
            return;
        }
        // On macOS: the probe's verdict must equal what the same bridge
        // reports under the same bound.
        let bridge_trusted = matches!(
            run_osascript(LiveMacosAxHost::PROBE_SCRIPT, Duration::from_secs(3), &cancel),
            Ok(output) if output.trim() == "true"
        );
        assert_eq!(
            probe.trusted(),
            bridge_trusted,
            "the probe must report the bridge's actual trust state"
        );
        let backend = MacosDesktopBackend::live();
        let health = backend.health(&cancel).expect("health");
        assert_eq!(health.is_available(), bridge_trusted);
        if !bridge_trusted {
            let err = backend
                .capture(DesktopSessionId::new(), &DesktopObserveRequest::new())
                .unwrap_err();
            assert_eq!(err, DesktopError::PermissionMissing);
        }
    }

    #[test]
    fn live_snapshot_parse_and_locators_are_observation_bound() {
        // A recorded transcript of SNAPSHOT_SCRIPT output parses into
        // windows/nodes with generation-bound refs and complete locators;
        // the next generation invalidates every previous ref.
        let host = LiveMacosAxHost::default();
        let transcript = "F\tTextEdit\n\
W\tTextEdit\tnotes.txt\t10\t20\t800\t600\ttrue\n\
N\tTextEdit\t1\t1\tAXButton\tSave\n\
N\tTextEdit\t1\t2\tAXSecureTextField\tmissing value\n\
N\tTextEdit\t1\t3\tAXStaticText\tweird\tname\n";
        let mut locators = HashMap::new();
        let snapshot = host
            .parse_snapshot(transcript, 7, &mut locators)
            .expect("parse");
        assert_eq!(snapshot.windows().len(), 1);
        let window = &snapshot.windows()[0];
        assert_eq!(window.id(), "win:7:1");
        assert_eq!(window.title(), "notes.txt");
        assert_eq!(window.app(), Some("TextEdit"));
        assert!(window.is_focused(), "frontmost process marks focus");
        assert_eq!(snapshot.nodes().len(), 3);
        let button = &snapshot.nodes()[0];
        assert_eq!(button.id(), "ax:7:1");
        assert!(button.actions().contains(&MacosAxAction::Press));
        let secure = &snapshot.nodes()[1];
        assert!(secure.is_sensitive(), "secure fields are sensitive");
        assert!(secure.actions().contains(&MacosAxAction::SetValue));
        let weird = &snapshot.nodes()[2];
        assert_eq!(weird.name(), "weird name", "embedded tabs collapse");
        // Locators re-walk the recorded System Events paths.
        assert_eq!(locators["win:7:1"].process, "TextEdit");
        assert_eq!(locators["win:7:1"].element_index, None);
        assert_eq!(locators["ax:7:1"].element_index, Some(1));
        assert_eq!(locators["ax:7:1"].role, "AXButton");
        // Generation numbering is carried by the refs: a ref from an older
        // generation never resolves after the map is replaced.
        let stale: HashMap<String, ElementLocator> = locators
            .drain()
            .filter(|(id, _)| id.contains(":7:"))
            .collect();
        let _ = host.parse_snapshot(transcript, 8, &mut locators);
        assert!(
            !stale
                .iter()
                .any(|(id, _)| locators.contains_key(id.as_str()))
        );
    }

    #[test]
    fn live_act_scripts_and_bounds() {
        use crate::desktop::backend::test_support::{point, resolved_target, target_node};
        use crate::desktop::backend::{KeyCode, ObservationId};
        // Planned act scripts address the recorded System Events path; a
        // stale or fabricated ref is the retryable not-found case; surface
        // gaps stay typed instead of faked.
        let host = LiveMacosAxHost::default();
        let mut locators = HashMap::new();
        let _ = host
            .parse_snapshot(
                "F\tTextEdit\nW\tTextEdit\tnotes.txt\t0\t0\t800\t600\ttrue\nN\tTextEdit\t1\t2\tAXTextField\tbody\n",
                1,
                &mut locators,
            )
            .expect("parse");
        *host.locators.lock().unwrap_or_else(|p| p.into_inner()) = locators;

        let observation = ObservationId::new();
        let target = target_node(observation, "win:1:1", "ax:1:1");
        let node_resolution = resolved_target(
            ActionKind::Click,
            None,
            Some("ax:1:1".to_owned()),
            true,
            false,
        );
        // A text-field click addresses the element path inside the process.
        let click = DesktopAction::click(target).expect("click");
        let script = host.plan_act(&click, &node_resolution).expect("plan");
        assert!(
            script.contains("click UI element 2 of window 1"),
            "acts address the snapshot-time window ordinal: {script}"
        );
        assert!(script.contains("tell process \"TextEdit\""), "{script}");
        // Text into a text role uses `set value`, never keystroke, and the
        // literal is quoted so it cannot break out of the AppleScript.
        let type_target = target_node(observation, "win:1:1", "ax:1:1");
        let type_text = DesktopAction::TypeText {
            target: type_target,
            value: SecretAwareString::literal("hello \"world\"").expect("literal"),
        };
        let script = host
            .plan_act(
                &type_text,
                &resolved_target(
                    ActionKind::TypeText,
                    None,
                    Some("ax:1:1".to_owned()),
                    true,
                    false,
                ),
            )
            .expect("plan");
        assert!(script.contains("set value of"), "{script}");
        assert!(script.contains("hello \\\"world\\\""), "escaping: {script}");
        // A stale ref is not-found (the backend maps it to StaleObservation).
        let stale = host.plan_act(
            &click,
            &resolved_target(
                ActionKind::Click,
                None,
                Some("ax:0:9".to_owned()),
                true,
                false,
            ),
        );
        assert_eq!(stale.unwrap_err(), DesktopError::TargetNotFound);
        // Coordinate fallback is never implied by AX support (T-CU-02).
        let point = DesktopAction::CoordinateFallback {
            point: point(5, 5),
            button: MouseButton::Left,
            count: 1,
        };
        assert_eq!(
            host.plan_act(
                &point,
                &resolved_target(ActionKind::CoordinateFallback, None, None, true, false)
            )
            .unwrap_err(),
            DesktopError::CapabilityUnavailable
        );
        // Keys become key codes.
        let enter = DesktopAction::key(KeyCode::parse("Enter").expect("key"));
        let script = host
            .plan_act(
                &enter,
                &resolved_target(ActionKind::Key, None, None, false, false),
            )
            .expect("plan");
        assert!(script.contains("key code 36"), "{script}");
    }

    #[test]
    fn apple_quote_never_breaks_out_of_a_literal() {
        assert_eq!(apple_quote("plain"), "plain");
        assert_eq!(apple_quote("sa\"fe"), "sa\\\"fe");
        assert_eq!(apple_quote("back\\slash"), "back\\\\slash");
        assert_eq!(apple_quote("no\ncontrols\r"), "nocontrols");
    }

    #[test]
    #[ignore = "live macOS AX: needs a WindowServer session and Accessibility \
                trust; run with `cargo test -p computer-use -- --ignored` on \
                a desktop where the host terminal is trusted"]
    fn live_workflow_observes_acts_verifies_and_recovers() {
        use crate::desktop::backend::test_support::window_ref;
        let actor = DesktopActor::new(MacosDesktopBackend::live());
        let cancel = CancellationToken::new();
        // AUTHORIZE: health is the typed trust gate. No prompt, no grant —
        // an untrusted host stops here with the typed reason.
        let health = actor.health(&cancel).expect("health");
        assert!(
            health.is_available(),
            "grant Accessibility to the host terminal in System Settings, then rerun"
        );
        let session = DesktopSessionId::new();
        let observe_request = || {
            DesktopObserveRequest::new()
                .with_cancel(cancel.clone())
                .with_timeout(Duration::from_secs(20))
                .expect("observe timeout")
        };
        // OBSERVE: real windows from the running session.
        let observation = actor.observe(session, observe_request()).expect("observe");
        assert!(
            !observation.windows().is_empty(),
            "a logged-in desktop session has windows"
        );
        // ACT on the already-focused window: a real AXRaise with a
        // deterministic, non-disruptive postcondition.
        let target = observation
            .focused_window()
            .cloned()
            .or_else(|| observation.windows().first().map(|w| w.window().clone()))
            .expect("a focused window");
        let action = DesktopAction::FocusWindow {
            window: target.clone(),
        };
        let act_request = DesktopActionRequest::new(observation.id(), action)
            .with_cancel(cancel.clone())
            .with_timeout(Duration::from_secs(15))
            .expect("act timeout");
        actor.act(session, act_request).expect("raise");
        // VERIFY: a fresh observation still enumerates the window (the
        // raise neither closed nor moved it out of the session).
        let verify = actor
            .observe(session, observe_request())
            .expect("re-observe");
        assert!(
            verify
                .windows()
                .iter()
                .any(|w| w.window().stable_ref() == target.stable_ref()
                    || w.title() == target.stable_ref()),
            "the raised window must survive into the verifying observation"
        );
        // RECOVER: an action bound to the superseded first observation
        // fails with the RETRYABLE stale-observation error, and a fresh
        // observation makes the workflow proceed again.
        let stale_action = DesktopAction::FocusWindow {
            window: target.clone(),
        };
        let stale_request = DesktopActionRequest::new(observation.id(), stale_action)
            .with_cancel(cancel.clone())
            .with_timeout(Duration::from_secs(15))
            .expect("act timeout");
        let err = actor.act(session, stale_request).unwrap_err();
        assert_eq!(err, DesktopError::StaleObservation);
        let recovered = actor
            .observe(session, observe_request())
            .expect("re-observe");
        assert!(!recovered.windows().is_empty());
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
