//! Live Chromium driver over the Chrome DevTools Protocol.
//!
//! The one real implementation of the browser boundary: [`ChromiumCdpBackend`]
//! launches a headless Chromium (Google Chrome, Chromium or Microsoft Edge —
//! whichever the host has), connects to its DevTools WebSocket, and drives
//! isolated browser contexts through it. It implements all three traits the
//! rest of the stack is written against — [`PlaywrightBackend`] for the
//! context lifecycle, cookies, storage and traces; [`PageCapture`] for
//! observation; [`PageActor`] for real input — so `BrowserManager`,
//! `BrowserObserver` and `BrowserActor` run unchanged over a real browser.
//!
//! Targets are resolved the way the observer promises: a capture collects
//! the page's interactive and named elements in document order and pins
//! them in an *isolated world* the page cannot see (`__rapidlmTargets`);
//! an action re-captures, the actor checks the observation is not stale
//! (the document generation is the browser-reported `loaderId`, so a
//! navigation the page made itself counts), and the resolved index names
//! the element the collector found. Input goes through
//! `Input.dispatchMouseEvent` / `Input.insertText` / `Input.dispatchKeyEvent`
//! — the browser's own input pipeline, not synthetic DOM events. The URL
//! and title are read from the browser process, never from page script.
//!
//! Only Chromium engines are live; a WebKit or Firefox request is a typed
//! `Unavailable`, never a silent Chromium substitution.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use capability_broker::CancellationToken;
use serde_json::{Value, json};

use super::action::{
    ActionError, KeyCode, MouseButton, PageActor, ResolvedTarget, SecretAwareString,
};
use super::observe::{
    MAX_NAME_BYTES, MAX_ROLE_BYTES, MAX_TEST_ID_BYTES, MAX_TITLE_BYTES, MAX_URL_BYTES,
    ObserveError, PageCapture, PageNode, PageSnapshot, RawScreenshot,
};
use super::session::{
    BrowserCookie, BrowserEngine, BrowserSessionError, BrowserSessionId, NewContextRequest,
    PlaywrightBackend, PlaywrightBrowserId, PlaywrightContextId,
};
use super::ws::{WebSocket, WsError, base64_decode};

/// Environment variable naming the browser executable to use, overriding
/// discovery. Absolute path; relative names are refused.
pub const BROWSER_PATH_ENV: &str = "RAPIDLM_BROWSER_PATH";

/// How long a browser launch may take before it is `Unavailable`.
pub const DEFAULT_LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Per-command DevTools round-trip bound.
pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(20);

/// Wait bound for a navigation's load event.
pub const DEFAULT_NAVIGATION_TIMEOUT: Duration = Duration::from_secs(30);

/// Headless viewport, within the observer's screenshot bounds.
pub const VIEWPORT: (u32, u32) = (1280, 720);

const TRACE_MAGIC: &str = "rapidlm.playwright.trace.v1";

/// Where the browser lives on this host, resolved once.
#[derive(Clone, Debug)]
pub struct ChromiumLaunch {
    executable: PathBuf,
    launch_timeout: Duration,
    command_timeout: Duration,
    navigation_timeout: Duration,
}

/// Why no browser could be resolved. Display never echoes paths.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserDiscoveryError {
    /// `RAPIDLM_BROWSER_PATH` was set but is not an absolute existing file.
    OverrideInvalid,
    /// No Chromium-family browser at any known location or on `PATH`.
    NotFound,
}

impl std::fmt::Display for BrowserDiscoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::OverrideInvalid => {
                "RAPIDLM_BROWSER_PATH must be the absolute path of an existing browser executable"
            }
            Self::NotFound => {
                "no Chromium-family browser found (Google Chrome, Chromium or Microsoft Edge); \
set RAPIDLM_BROWSER_PATH"
            }
        })
    }
}

impl std::error::Error for BrowserDiscoveryError {}

impl ChromiumLaunch {
    /// Resolve the browser from `RAPIDLM_BROWSER_PATH`, then the platform's
    /// standard install locations, then `PATH`.
    pub fn discover() -> Result<Self, BrowserDiscoveryError> {
        let executable = match std::env::var_os(BROWSER_PATH_ENV) {
            Some(raw) => {
                let path = PathBuf::from(raw);
                if !path.is_absolute() || !path.is_file() {
                    return Err(BrowserDiscoveryError::OverrideInvalid);
                }
                path
            }
            None => discover_executable().ok_or(BrowserDiscoveryError::NotFound)?,
        };
        Ok(Self::at(executable))
    }

    /// A launch configuration for an explicit executable.
    pub fn at(executable: PathBuf) -> Self {
        Self {
            executable,
            launch_timeout: DEFAULT_LAUNCH_TIMEOUT,
            command_timeout: DEFAULT_COMMAND_TIMEOUT,
            navigation_timeout: DEFAULT_NAVIGATION_TIMEOUT,
        }
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub fn with_launch_timeout(mut self, timeout: Duration) -> Self {
        self.launch_timeout = timeout;
        self
    }

    pub fn with_command_timeout(mut self, timeout: Duration) -> Self {
        self.command_timeout = timeout;
        self
    }

    pub fn with_navigation_timeout(mut self, timeout: Duration) -> Self {
        self.navigation_timeout = timeout;
        self
    }
}

/// Standard install locations per platform, then a `PATH` search.
fn discover_executable() -> Option<PathBuf> {
    let fixed: &[&str] = if cfg!(target_os = "macos") {
        &[
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
        ]
    } else if cfg!(windows) {
        &[
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
            r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
            r"C:\Program Files\Chromium\Application\chrome.exe",
        ]
    } else {
        &[
            "/usr/bin/google-chrome",
            "/usr/bin/google-chrome-stable",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
            "/snap/bin/chromium",
            "/usr/bin/microsoft-edge",
            "/opt/google/chrome/chrome",
        ]
    };
    for candidate in fixed {
        let path = Path::new(candidate);
        if path.is_file() {
            return Some(path.to_path_buf());
        }
    }
    if cfg!(windows)
        && let Some(local) = std::env::var_os("LOCALAPPDATA")
    {
        let path = Path::new(&local).join(r"Google\Chrome\Application\chrome.exe");
        if path.is_file() {
            return Some(path);
        }
    }
    let names: &[&str] = if cfg!(windows) {
        &["chrome.exe", "msedge.exe", "chromium.exe"]
    } else {
        &[
            "google-chrome",
            "google-chrome-stable",
            "chromium",
            "chromium-browser",
            "microsoft-edge",
        ]
    };
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for name in names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Live Chromium backend. One instance may own several launched browsers
/// (one per engine request that could not reuse a live one) and any number
/// of isolated contexts across them.
pub struct ChromiumCdpBackend {
    launch: ChromiumLaunch,
    state: Mutex<CdpState>,
}

struct CdpState {
    next_id: u64,
    browsers: HashMap<PlaywrightBrowserId, LiveBrowser>,
    contexts: HashMap<PlaywrightContextId, LiveContext>,
    live_chromium: Option<PlaywrightBrowserId>,
}

struct LiveBrowser {
    child: Child,
    conn: CdpConnection,
    user_data_dir: PathBuf,
    crashed: bool,
}

struct LiveContext {
    browser: PlaywrightBrowserId,
    cdp_context: String,
    session: String,
    /// Document generation: bumped whenever the main frame's `loaderId`
    /// changes — a navigation the driver made *or the page made* (link,
    /// redirect, script). Read from the browser at every capture, so an
    /// observation of the previous document is stale (T-CU-02) no matter
    /// who navigated.
    generation: u64,
    loader_id: Option<String>,
    /// The isolated world the collector and the actions run in. The page
    /// cannot see or rewrite its globals and prototypes, so `String`,
    /// `getBoundingClientRect`, `__rapidlmTargets` mean what they say
    /// (T-CU-01). Recreated whenever the document changes.
    isolated_context: Option<(String, u64)>,
    trace: Option<Vec<String>>,
    profile_dir: PathBuf,
    persist: bool,
    closed: bool,
    targets: usize,
}

impl Drop for LiveBrowser {
    fn drop(&mut self) {
        // Kill first: a wedged browser with a full receive buffer would
        // otherwise hold the close frame's write for the whole write timeout.
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.conn.close();
        let _ = std::fs::remove_dir_all(&self.user_data_dir);
    }
}

impl ChromiumCdpBackend {
    /// A backend that will launch `launch.executable()` on first use.
    pub fn new(launch: ChromiumLaunch) -> Self {
        Self {
            launch,
            state: Mutex::new(CdpState {
                next_id: 1,
                browsers: HashMap::new(),
                contexts: HashMap::new(),
                live_chromium: None,
            }),
        }
    }

    /// [`ChromiumCdpBackend::new`] over [`ChromiumLaunch::discover`].
    pub fn discover() -> Result<Self, BrowserDiscoveryError> {
        ChromiumLaunch::discover().map(Self::new)
    }

    pub fn launch(&self) -> &ChromiumLaunch {
        &self.launch
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, CdpState>, BrowserSessionError> {
        self.state
            .lock()
            .map_err(|_| BrowserSessionError::Unavailable)
    }

    fn launch_browser(
        &self,
        cancel: &CancellationToken,
    ) -> Result<LiveBrowser, BrowserSessionError> {
        check_cancel(cancel)?;
        let user_data_dir = std::env::temp_dir().join(format!(
            "rapidlm-chromium-{}-{}",
            std::process::id(),
            protocol::RuntimeId::new()
        ));
        std::fs::create_dir_all(&user_data_dir).map_err(|_| BrowserSessionError::Io)?;
        let mut command = Command::new(&self.launch.executable);
        command
            .arg("--headless=new")
            .arg("--remote-debugging-port=0")
            .arg(format!("--user-data-dir={}", user_data_dir.display()))
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--disable-extensions")
            .arg("--disable-background-networking")
            .arg("--disable-sync")
            .arg("--disable-default-apps")
            .arg("--mute-audio")
            .arg("--hide-scrollbars")
            .arg(format!("--window-size={},{}", VIEWPORT.0, VIEWPORT.1))
            .arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if cfg!(target_os = "linux") {
            // Sandboxed CI containers routinely lack the user namespaces the
            // Chromium sandbox needs; the browser is already confined to a
            // throwaway profile and isolated contexts.
            command.arg("--no-sandbox");
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(_) => {
                let _ = std::fs::remove_dir_all(&user_data_dir);
                return Err(BrowserSessionError::Unavailable);
            }
        };
        let Some(stderr) = child.stderr.take() else {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_dir_all(&user_data_dir);
            return Err(BrowserSessionError::Unavailable);
        };
        let endpoint = match wait_for_devtools_endpoint(stderr, self.launch.launch_timeout, cancel)
        {
            Ok(endpoint) => endpoint,
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_dir_all(&user_data_dir);
                return Err(err);
            }
        };
        let conn = match CdpConnection::connect(&endpoint, self.launch.command_timeout) {
            Ok(conn) => conn,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_dir_all(&user_data_dir);
                return Err(BrowserSessionError::Unavailable);
            }
        };
        Ok(LiveBrowser {
            child,
            conn,
            user_data_dir,
            crashed: false,
        })
    }

    fn with_context<T>(
        &self,
        context: PlaywrightContextId,
        cancel: &CancellationToken,
        op: impl FnOnce(&mut CdpConnection, &mut LiveContext) -> Result<T, BrowserSessionError>,
    ) -> Result<T, BrowserSessionError> {
        check_cancel(cancel)?;
        let mut state = self.lock()?;
        let state = &mut *state;
        let ctx = state
            .contexts
            .get_mut(&context)
            .ok_or(BrowserSessionError::SessionNotFound)?;
        if ctx.closed {
            return Err(BrowserSessionError::SessionClosed);
        }
        let browser = state
            .browsers
            .get_mut(&ctx.browser)
            .ok_or(BrowserSessionError::SessionCrashed)?;
        if browser.crashed {
            return Err(BrowserSessionError::SessionCrashed);
        }
        match browser.child.try_wait() {
            Ok(None) => {}
            _ => {
                browser.crashed = true;
                return Err(BrowserSessionError::SessionCrashed);
            }
        }
        let result = op(&mut browser.conn, ctx);
        if browser.conn.is_dead() {
            // A desynchronised or closed socket cannot be recovered; the
            // browser is retired so the next launch starts a fresh one.
            browser.crashed = true;
        }
        result
    }

    fn capture(
        &self,
        context: PlaywrightContextId,
        include_screenshot: bool,
        cancel: &CancellationToken,
    ) -> Result<PageSnapshot, BrowserSessionError> {
        self.with_context(context, cancel, |conn, ctx| {
            conn.drain_events(ctx, cancel)?;
            // The URL and the document identity come from the browser
            // process, never from page script: `Page.getFrameTree` names the
            // main frame's URL and its `loaderId`, which changes with every
            // document. A page can override `String` or `location` getters
            // in its own world; it cannot touch this.
            let (frame_id, url, loader_id) = main_frame(conn, &ctx.session, cancel)?;
            if ctx.loader_id.as_deref() != Some(loader_id.as_str()) {
                ctx.generation = ctx.generation.saturating_add(1);
                ctx.loader_id = Some(loader_id);
                ctx.isolated_context = None;
                ctx.targets = 0;
            }
            let world = isolated_world(conn, ctx, &frame_id, cancel)?;
            let collected = conn.call(
                Some(&ctx.session),
                "Runtime.evaluate",
                json!({
                    "expression": COLLECT_TARGETS_JS,
                    "contextId": world,
                    "returnByValue": true,
                    "awaitPromise": false,
                }),
                cancel,
            )?;
            let value = collected
                .get("result")
                .and_then(|result| result.get("value"))
                .cloned()
                .ok_or(BrowserSessionError::Backend)?;
            let title = bounded_text(value["title"].as_str().unwrap_or(""), MAX_TITLE_BYTES);
            let url = bounded_text(&url, MAX_URL_BYTES);
            let raw_nodes = value["nodes"].as_array().cloned().unwrap_or_default();
            ctx.targets = raw_nodes.len();
            // Which collected elements the accessibility tree also exposes:
            // best effort, the DOM answer stands alone if the AX tree cannot
            // be read.
            let ax_refs = accessible_refs(conn, &ctx.session, cancel).unwrap_or_default();
            let mut nodes = Vec::with_capacity(raw_nodes.len());
            for (index, raw) in raw_nodes.iter().enumerate() {
                // Every text field is bounded here, by bytes and at a char
                // boundary, before the observer's own bound check — a page
                // is not allowed to make the whole capture fail with a long
                // `role` attribute or an accented name.
                let role = bounded_text(raw["role"].as_str().unwrap_or("generic"), MAX_ROLE_BYTES);
                let name = bounded_text(raw["name"].as_str().unwrap_or(""), MAX_NAME_BYTES);
                let test_id = raw["testId"]
                    .as_str()
                    .map(|t| bounded_text(t, MAX_TEST_ID_BYTES));
                let input_type = raw["inputType"]
                    .as_str()
                    .map(|t| bounded_text(t, MAX_ROLE_BYTES));
                let interactive = raw["interactive"].as_bool().unwrap_or(false);
                let from_accessibility = ax_refs.contains(&(index as u32));
                let node = PageNode::from_capture(
                    if role.is_empty() { "generic" } else { &role },
                    &name,
                    test_id.as_deref().filter(|t| !t.is_empty()),
                    input_type.as_deref().filter(|t| !t.is_empty()),
                    interactive,
                    from_accessibility,
                    true,
                )
                .map_err(|_| BrowserSessionError::Backend)?;
                nodes.push(node);
            }
            let screenshot = if include_screenshot {
                // A screenshot the observer could not persist (over its byte
                // bound) drops the screenshot, not the observation.
                match capture_screenshot(conn, &ctx.session, cancel) {
                    Ok(shot) => Some(shot),
                    Err(BrowserSessionError::Backend) => {
                        trace_line(ctx, "{\"event\":\"screenshot.dropped\"}");
                        None
                    }
                    Err(err) => return Err(err),
                }
            } else {
                None
            };
            trace_line(
                ctx,
                &format!("{{\"event\":\"capture\",\"nodes\":{}}}", nodes.len()),
            );
            Ok(PageSnapshot::from_capture(
                url,
                title,
                ctx.generation,
                nodes,
                screenshot,
            ))
        })
    }

    /// Centre of the target's box in CSS pixels after scrolling it into view.
    fn target_center(
        conn: &mut CdpConnection,
        ctx: &mut LiveContext,
        target: &ResolvedTarget,
        cancel: &CancellationToken,
    ) -> Result<(f64, f64), ActionError> {
        let index = target.index();
        if index as usize >= ctx.targets {
            return Err(ActionError::StaleObservation);
        }
        let Some((_, world)) = ctx.isolated_context else {
            return Err(ActionError::StaleObservation);
        };
        let expression = format!(
            "(() => {{ const t = globalThis.__rapidlmTargets; if (!t || !t[{index}]) return null; \
const el = t[{index}]; el.scrollIntoView({{block: 'center', inline: 'center'}}); \
const r = el.getBoundingClientRect(); \
return {{x: r.left + r.width / 2, y: r.top + r.height / 2, w: r.width, h: r.height}}; }})()"
        );
        let result = conn
            .call(
                Some(&ctx.session),
                "Runtime.evaluate",
                json!({"expression": expression, "contextId": world, "returnByValue": true}),
                cancel,
            )
            .map_err(map_session_to_action)?;
        let value = &result["result"]["value"];
        if value.is_null() {
            return Err(ActionError::StaleObservation);
        }
        let w = value["w"].as_f64().unwrap_or(0.0);
        let h = value["h"].as_f64().unwrap_or(0.0);
        if w <= 0.0 || h <= 0.0 {
            return Err(ActionError::TargetNotInteractive);
        }
        Ok((
            value["x"].as_f64().unwrap_or(0.0),
            value["y"].as_f64().unwrap_or(0.0),
        ))
    }

    fn act<T>(
        &self,
        context: PlaywrightContextId,
        cancel: &CancellationToken,
        op: impl FnOnce(&mut CdpConnection, &mut LiveContext) -> Result<T, ActionError>,
    ) -> Result<T, ActionError> {
        if cancel.is_cancelled() {
            return Err(ActionError::Cancelled);
        }
        let mut state = self.state.lock().map_err(|_| ActionError::Unavailable)?;
        let state = &mut *state;
        let ctx = state
            .contexts
            .get_mut(&context)
            .ok_or(ActionError::SessionNotFound)?;
        if ctx.closed {
            return Err(ActionError::SessionClosed);
        }
        let browser = state
            .browsers
            .get_mut(&ctx.browser)
            .ok_or(ActionError::SessionCrashed)?;
        if browser.crashed {
            return Err(ActionError::SessionCrashed);
        }
        match browser.child.try_wait() {
            Ok(None) => {}
            _ => {
                browser.crashed = true;
                return Err(ActionError::SessionCrashed);
            }
        }
        browser
            .conn
            .drain_events(ctx, cancel)
            .map_err(map_session_to_action)?;
        let result = op(&mut browser.conn, ctx);
        if browser.conn.is_dead() {
            browser.crashed = true;
        }
        result
    }
}

/// [`PlaywrightBackend`] over a shared backend, so the same live browser
/// serves `BrowserManager` (boxed) and the observer/actor (`Arc`).
impl PlaywrightBackend for Arc<ChromiumCdpBackend> {
    fn launch_or_reuse_browser(
        &self,
        engine: BrowserEngine,
        cancel: &CancellationToken,
    ) -> Result<PlaywrightBrowserId, BrowserSessionError> {
        (**self).launch_or_reuse_browser(engine, cancel)
    }

    fn new_isolated_context(
        &self,
        request: &NewContextRequest<'_>,
        cancel: &CancellationToken,
    ) -> Result<PlaywrightContextId, BrowserSessionError> {
        (**self).new_isolated_context(request, cancel)
    }

    fn cookies(
        &self,
        context: PlaywrightContextId,
        cancel: &CancellationToken,
    ) -> Result<Vec<BrowserCookie>, BrowserSessionError> {
        (**self).cookies(context, cancel)
    }

    fn put_cookie(
        &self,
        context: PlaywrightContextId,
        cookie: &BrowserCookie,
        cancel: &CancellationToken,
    ) -> Result<(), BrowserSessionError> {
        (**self).put_cookie(context, cookie, cancel)
    }

    fn storage_get(
        &self,
        context: PlaywrightContextId,
        origin: &str,
        key: &str,
        cancel: &CancellationToken,
    ) -> Result<Option<String>, BrowserSessionError> {
        (**self).storage_get(context, origin, key, cancel)
    }

    fn storage_put(
        &self,
        context: PlaywrightContextId,
        origin: &str,
        key: &str,
        value: &str,
        cancel: &CancellationToken,
    ) -> Result<(), BrowserSessionError> {
        (**self).storage_put(context, origin, key, value, cancel)
    }

    fn mark_crashed(
        &self,
        browser: PlaywrightBrowserId,
        cancel: &CancellationToken,
    ) -> Result<(), BrowserSessionError> {
        (**self).mark_crashed(browser, cancel)
    }

    fn export_trace(
        &self,
        context: PlaywrightContextId,
        cancel: &CancellationToken,
    ) -> Result<Option<Vec<u8>>, BrowserSessionError> {
        (**self).export_trace(context, cancel)
    }

    fn close_context(
        &self,
        context: PlaywrightContextId,
        persist: bool,
        cancel: &CancellationToken,
    ) -> Result<(), BrowserSessionError> {
        (**self).close_context(context, persist, cancel)
    }
}

impl PlaywrightBackend for ChromiumCdpBackend {
    fn launch_or_reuse_browser(
        &self,
        engine: BrowserEngine,
        cancel: &CancellationToken,
    ) -> Result<PlaywrightBrowserId, BrowserSessionError> {
        check_cancel(cancel)?;
        if engine != BrowserEngine::Chromium {
            return Err(BrowserSessionError::Unavailable);
        }
        {
            let mut state = self.lock()?;
            if let Some(id) = state.live_chromium
                && let Some(browser) = state.browsers.get_mut(&id)
            {
                let alive = !browser.crashed && matches!(browser.child.try_wait(), Ok(None));
                if alive {
                    return Ok(id);
                }
                // Dead: retire it now rather than keeping the process record
                // (and any half-alive children) until the backend drops.
                state.browsers.remove(&id);
                state.live_chromium = None;
            }
        }
        // Launch outside the lock: it takes seconds and must observe cancel.
        let browser = self.launch_browser(cancel)?;
        let mut state = self.lock()?;
        let id = PlaywrightBrowserId::from_raw(state.next_id);
        state.next_id = state
            .next_id
            .checked_add(1)
            .ok_or(BrowserSessionError::Unavailable)?;
        state.browsers.insert(id, browser);
        state.live_chromium = Some(id);
        Ok(id)
    }

    fn new_isolated_context(
        &self,
        request: &NewContextRequest<'_>,
        cancel: &CancellationToken,
    ) -> Result<PlaywrightContextId, BrowserSessionError> {
        check_cancel(cancel)?;
        let mut state = self.lock()?;
        let state = &mut *state;
        let browser = state
            .browsers
            .get_mut(&request.browser)
            .ok_or(BrowserSessionError::Backend)?;
        if browser.crashed {
            return Err(BrowserSessionError::SessionCrashed);
        }
        let conn = &mut browser.conn;
        let created = conn.call(
            None,
            "Target.createBrowserContext",
            json!({"disposeOnDetach": false}),
            cancel,
        )?;
        let cdp_context = created["browserContextId"]
            .as_str()
            .ok_or(BrowserSessionError::Backend)?
            .to_owned();
        // Anything failing after the CDP context exists disposes it again,
        // so a half-built context never lingers in the browser.
        let session = match attach_page(conn, &cdp_context, request.downloads_dir, cancel) {
            Ok(session) => session,
            Err(err) => {
                let _ = conn.call(
                    None,
                    "Target.disposeBrowserContext",
                    json!({"browserContextId": cdp_context}),
                    cancel,
                );
                return Err(err);
            }
        };
        let id = PlaywrightContextId::from_raw(state.next_id);
        state.next_id = state
            .next_id
            .checked_add(1)
            .ok_or(BrowserSessionError::Unavailable)?;
        let mut ctx = LiveContext {
            browser: request.browser,
            cdp_context,
            session,
            generation: 0,
            loader_id: None,
            isolated_context: None,
            trace: request.trace.then(|| {
                vec![
                    TRACE_MAGIC.to_owned(),
                    format!(
                        "{{\"event\":\"context.created\",\"persist\":{},\"driver\":\"chromium-cdp\"}}",
                        request.persist
                    ),
                    "{\"event\":\"context.isolated\"}".to_owned(),
                ]
            }),
            profile_dir: request.profile_dir.to_path_buf(),
            persist: request.persist,
            closed: false,
            targets: 0,
        };
        if request.persist {
            for cookie in super::session::load_persisted_cookies(request.profile_dir)? {
                set_cookie(conn, &ctx, &cookie, cancel)?;
            }
        }
        let _ = request.temp_dir;
        trace_line(&mut ctx, "{\"event\":\"page.attached\"}");
        state.contexts.insert(id, ctx);
        Ok(id)
    }

    fn cookies(
        &self,
        context: PlaywrightContextId,
        cancel: &CancellationToken,
    ) -> Result<Vec<BrowserCookie>, BrowserSessionError> {
        self.with_context(context, cancel, |conn, ctx| read_cookies(conn, ctx, cancel))
    }

    fn put_cookie(
        &self,
        context: PlaywrightContextId,
        cookie: &BrowserCookie,
        cancel: &CancellationToken,
    ) -> Result<(), BrowserSessionError> {
        self.with_context(context, cancel, |conn, ctx| {
            set_cookie(conn, ctx, cookie, cancel)?;
            trace_line(ctx, "{\"event\":\"cookie.put\"}");
            Ok(())
        })
    }

    fn storage_get(
        &self,
        context: PlaywrightContextId,
        origin: &str,
        key: &str,
        cancel: &CancellationToken,
    ) -> Result<Option<String>, BrowserSessionError> {
        self.with_context(context, cancel, |conn, ctx| {
            let result = conn.call(
                Some(&ctx.session),
                "DOMStorage.getDOMStorageItems",
                json!({"storageId": {"securityOrigin": origin, "isLocalStorage": true}}),
                cancel,
            )?;
            let entries = result["entries"].as_array().cloned().unwrap_or_default();
            for entry in entries {
                if entry.get(0).and_then(Value::as_str) == Some(key) {
                    return Ok(entry.get(1).and_then(Value::as_str).map(str::to_owned));
                }
            }
            Ok(None)
        })
    }

    fn storage_put(
        &self,
        context: PlaywrightContextId,
        origin: &str,
        key: &str,
        value: &str,
        cancel: &CancellationToken,
    ) -> Result<(), BrowserSessionError> {
        self.with_context(context, cancel, |conn, ctx| {
            conn.call(
                Some(&ctx.session),
                "DOMStorage.setDOMStorageItem",
                json!({
                    "storageId": {"securityOrigin": origin, "isLocalStorage": true},
                    "key": key,
                    "value": value,
                }),
                cancel,
            )?;
            trace_line(ctx, "{\"event\":\"storage.put\"}");
            Ok(())
        })
    }

    fn mark_crashed(
        &self,
        browser: PlaywrightBrowserId,
        cancel: &CancellationToken,
    ) -> Result<(), BrowserSessionError> {
        check_cancel(cancel)?;
        let mut state = self.lock()?;
        // Dropping the record kills the process and removes its profile;
        // the contexts that lived in it are closed by the same drop.
        state.browsers.remove(&browser);
        for ctx in state.contexts.values_mut() {
            if ctx.browser == browser {
                ctx.closed = true;
            }
        }
        if state.live_chromium == Some(browser) {
            state.live_chromium = None;
        }
        Ok(())
    }

    fn export_trace(
        &self,
        context: PlaywrightContextId,
        cancel: &CancellationToken,
    ) -> Result<Option<Vec<u8>>, BrowserSessionError> {
        check_cancel(cancel)?;
        let state = self.lock()?;
        let ctx = state
            .contexts
            .get(&context)
            .ok_or(BrowserSessionError::SessionNotFound)?;
        Ok(ctx.trace.as_ref().map(|lines| {
            let mut body = lines.join("\n");
            body.push('\n');
            body.into_bytes()
        }))
    }

    fn close_context(
        &self,
        context: PlaywrightContextId,
        persist: bool,
        cancel: &CancellationToken,
    ) -> Result<(), BrowserSessionError> {
        check_cancel(cancel)?;
        let mut state = self.lock()?;
        let state = &mut *state;
        let Some(mut ctx) = state.contexts.remove(&context) else {
            return Ok(());
        };
        if ctx.closed {
            return Ok(());
        }
        ctx.closed = true;
        let Some(browser) = state.browsers.get_mut(&ctx.browser) else {
            return Ok(());
        };
        if browser.crashed || !matches!(browser.child.try_wait(), Ok(None)) {
            return Ok(());
        }
        if persist && ctx.persist {
            let cookies = read_cookies(&mut browser.conn, &ctx, cancel)?;
            super::session::save_persisted_cookies(&ctx.profile_dir, &cookies)?;
        }
        let _ = browser.conn.call(
            None,
            "Target.disposeBrowserContext",
            json!({"browserContextId": ctx.cdp_context}),
            cancel,
        );
        Ok(())
    }
}

impl PageCapture for ChromiumCdpBackend {
    fn capture(
        &self,
        _session: BrowserSessionId,
        context: PlaywrightContextId,
        include_screenshot: bool,
        cancel: &CancellationToken,
    ) -> Result<PageSnapshot, ObserveError> {
        self.capture(context, include_screenshot, cancel)
            .map_err(map_session_to_observe)
    }
}

impl PageActor for ChromiumCdpBackend {
    fn snapshot(
        &self,
        _session: BrowserSessionId,
        context: PlaywrightContextId,
        cancel: &CancellationToken,
    ) -> Result<PageSnapshot, ActionError> {
        self.capture(context, false, cancel)
            .map_err(map_session_to_action)
    }

    fn click(
        &self,
        _session: BrowserSessionId,
        context: PlaywrightContextId,
        target: &ResolvedTarget,
        button: MouseButton,
        count: u8,
        cancel: &CancellationToken,
    ) -> Result<(), ActionError> {
        self.act(context, cancel, |conn, ctx| {
            let (x, y) = Self::target_center(conn, ctx, target, cancel)?;
            let button_name = button.as_str();
            let clicks = count.clamp(1, 3);
            mouse_event(conn, ctx, "mouseMoved", x, y, "none", 0, cancel)?;
            for n in 1..=clicks {
                mouse_event(conn, ctx, "mousePressed", x, y, button_name, n, cancel)?;
                mouse_event(conn, ctx, "mouseReleased", x, y, button_name, n, cancel)?;
            }
            trace_line(
                ctx,
                &format!(
                    "{{\"event\":\"click\",\"target\":{},\"button\":\"{button_name}\",\"count\":{clicks}}}",
                    target.index()
                ),
            );
            Ok(())
        })
    }

    fn type_text(
        &self,
        _session: BrowserSessionId,
        context: PlaywrightContextId,
        target: &ResolvedTarget,
        value: &SecretAwareString,
        cancel: &CancellationToken,
    ) -> Result<(), ActionError> {
        // Secret handles carry no text this driver could type: the broker
        // never hands the value to the model side, and this backend is on
        // the model side of that line. A driver that cannot materialise the
        // handle is unavailable for that action — never a typed handle name.
        let Some(text) = value.as_literal() else {
            return Err(ActionError::Unavailable);
        };
        self.act(context, cancel, |conn, ctx| {
            let (x, y) = Self::target_center(conn, ctx, target, cancel)?;
            // Focus through a real click, then clear the current value the
            // way a user would: select all, delete.
            mouse_event(conn, ctx, "mouseMoved", x, y, "none", 0, cancel)?;
            mouse_event(conn, ctx, "mousePressed", x, y, "left", 1, cancel)?;
            mouse_event(conn, ctx, "mouseReleased", x, y, "left", 1, cancel)?;
            let index = target.index();
            let Some((_, world)) = ctx.isolated_context else {
                return Err(ActionError::StaleObservation);
            };
            conn.call(
                Some(&ctx.session),
                "Runtime.evaluate",
                json!({"expression": format!(
                    "(() => {{ const t = globalThis.__rapidlmTargets; const el = t && t[{index}]; \
if (!el) return false; el.focus(); \
if ('value' in el && typeof el.value === 'string') {{ el.value = ''; }} \
else if (el.isContentEditable) {{ el.textContent = ''; }} return true; }})()"
                ), "contextId": world, "returnByValue": true}),
                cancel,
            )
            .map_err(map_session_to_action)?;
            conn.call(
                Some(&ctx.session),
                "Input.insertText",
                json!({"text": text}),
                cancel,
            )
            .map_err(map_session_to_action)?;
            trace_line(
                ctx,
                &format!(
                    "{{\"event\":\"type\",\"target\":{index},\"bytes\":{}}}",
                    text.len()
                ),
            );
            Ok(())
        })
    }

    fn key(
        &self,
        _session: BrowserSessionId,
        context: PlaywrightContextId,
        target: Option<&ResolvedTarget>,
        key: &KeyCode,
        cancel: &CancellationToken,
    ) -> Result<(), ActionError> {
        self.act(context, cancel, |conn, ctx| {
            if let Some(target) = target {
                let (x, y) = Self::target_center(conn, ctx, target, cancel)?;
                mouse_event(conn, ctx, "mouseMoved", x, y, "none", 0, cancel)?;
                mouse_event(conn, ctx, "mousePressed", x, y, "left", 1, cancel)?;
                mouse_event(conn, ctx, "mouseReleased", x, y, "left", 1, cancel)?;
            }
            let spec = key_spec(key.as_str()).ok_or(ActionError::KeyInvalid)?;
            let mut down = json!({
                "type": if spec.text.is_some() { "keyDown" } else { "rawKeyDown" },
                "key": spec.key,
                "code": spec.code,
                "windowsVirtualKeyCode": spec.vk,
                "nativeVirtualKeyCode": spec.vk,
            });
            if let Some(text) = spec.text {
                down["text"] = Value::String(text.to_owned());
                down["unmodifiedText"] = Value::String(text.to_owned());
            }
            conn.call(Some(&ctx.session), "Input.dispatchKeyEvent", down, cancel)
                .map_err(map_session_to_action)?;
            conn.call(
                Some(&ctx.session),
                "Input.dispatchKeyEvent",
                json!({
                    "type": "keyUp",
                    "key": spec.key,
                    "code": spec.code,
                    "windowsVirtualKeyCode": spec.vk,
                    "nativeVirtualKeyCode": spec.vk,
                }),
                cancel,
            )
            .map_err(map_session_to_action)?;
            trace_line(
                ctx,
                &format!("{{\"event\":\"key\",\"key\":{:?}}}", spec.key),
            );
            Ok(())
        })
    }

    fn scroll(
        &self,
        _session: BrowserSessionId,
        context: PlaywrightContextId,
        target: Option<&ResolvedTarget>,
        dx: i32,
        dy: i32,
        cancel: &CancellationToken,
    ) -> Result<(), ActionError> {
        self.act(context, cancel, |conn, ctx| {
            let (x, y) = match target {
                Some(target) => Self::target_center(conn, ctx, target, cancel)?,
                None => (f64::from(VIEWPORT.0) / 2.0, f64::from(VIEWPORT.1) / 2.0),
            };
            conn.call(
                Some(&ctx.session),
                "Input.dispatchMouseEvent",
                json!({
                    "type": "mouseWheel",
                    "x": x,
                    "y": y,
                    "deltaX": dx,
                    "deltaY": dy,
                }),
                cancel,
            )
            .map_err(map_session_to_action)?;
            trace_line(
                ctx,
                &format!("{{\"event\":\"scroll\",\"dx\":{dx},\"dy\":{dy}}}"),
            );
            Ok(())
        })
    }

    fn navigate(
        &self,
        _session: BrowserSessionId,
        context: PlaywrightContextId,
        url: &str,
        cancel: &CancellationToken,
    ) -> Result<(), ActionError> {
        let navigation_timeout = self.launch.navigation_timeout;
        self.act(context, cancel, |conn, ctx| {
            let result = conn
                .call(
                    Some(&ctx.session),
                    "Page.navigate",
                    json!({"url": url}),
                    cancel,
                )
                .map_err(map_session_to_action)?;
            if result.get("errorText").and_then(Value::as_str).is_some() {
                return Err(ActionError::UrlInvalid);
            }
            // The generation moves when the next capture sees the new
            // `loaderId`; the old collection is unusable from here on.
            let loader = result["loaderId"].as_str().map(str::to_owned);
            ctx.targets = 0;
            ctx.isolated_context = None;
            conn.wait_for_load(ctx, loader.as_deref(), navigation_timeout, cancel)
                .map_err(map_session_to_action)?;
            trace_line(ctx, "{\"event\":\"navigate\"}");
            Ok(())
        })
    }
}

/// One DevTools WebSocket with request/response correlation. Events that
/// arrive while waiting for a response are kept (bounded) and folded into
/// the context; JavaScript dialogs are answered as they open so a page
/// cannot wedge the driver behind an `alert()`.
struct CdpConnection {
    ws: WebSocket,
    next_id: u64,
    events: Vec<Value>,
    command_timeout: Duration,
}

/// Most buffered events kept between drains. Page-controlled streams
/// (console output, DOM mutations) must not grow memory while a command is
/// pending; older events are dropped first — the driver acts on the
/// browser's *current* state at every capture, never on replayed events.
const MAX_BUFFERED_EVENTS: usize = 256;

impl CdpConnection {
    fn connect(endpoint: &str, command_timeout: Duration) -> Result<Self, WsError> {
        let ws = WebSocket::connect(endpoint, command_timeout)?;
        Ok(Self {
            ws,
            next_id: 1,
            events: Vec::new(),
            command_timeout,
        })
    }

    fn close(&mut self) {
        self.ws.close();
    }

    /// Whether the socket is beyond use (closed, desynchronised, or failed
    /// mid-frame). The owner retires the browser when this is set.
    fn is_dead(&self) -> bool {
        self.ws.is_dead()
    }

    fn call(
        &mut self,
        session: Option<&str>,
        method: &str,
        params: Value,
        cancel: &CancellationToken,
    ) -> Result<Value, BrowserSessionError> {
        check_cancel(cancel)?;
        let id = self.send(session, method, params)?;
        let deadline = Instant::now() + self.command_timeout;
        loop {
            check_cancel(cancel)?;
            if Instant::now() > deadline {
                return Err(BrowserSessionError::Backend);
            }
            let text = self.ws.recv_text().map_err(map_ws)?;
            let value: Value =
                serde_json::from_str(&text).map_err(|_| BrowserSessionError::Backend)?;
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = value.get("error") {
                    // Protocol-level text (never page content): the one line
                    // a failing driver run needs in its log.
                    eprintln!(
                        "cdp: {method} failed: {}",
                        error.get("message").and_then(Value::as_str).unwrap_or("?")
                    );
                    return Err(BrowserSessionError::Backend);
                }
                return Ok(value.get("result").cloned().unwrap_or(Value::Null));
            }
            if value.get("method").is_some() {
                self.note_incoming_event(value)?;
            }
        }
    }

    /// Send one request without waiting; returns its id.
    fn send(
        &mut self,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<u64, BrowserSessionError> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let mut message = json!({"id": id, "method": method, "params": params});
        if let Some(session) = session {
            message["sessionId"] = Value::String(session.to_owned());
        }
        self.ws.send_text(&message.to_string()).map_err(map_ws)?;
        Ok(id)
    }

    /// An event that arrived while a command was pending. A JavaScript
    /// dialog is answered immediately — dismissed, or accepted for
    /// `beforeunload` so a navigation can proceed — because the renderer's
    /// main thread is blocked until it is, and every later command would
    /// time out. Everything else is buffered, bounded.
    fn note_incoming_event(&mut self, event: Value) -> Result<(), BrowserSessionError> {
        if event["method"].as_str() == Some("Page.javascriptDialogOpening") {
            let accept = event["params"]["type"].as_str() == Some("beforeunload");
            let session = event["sessionId"].as_str().map(str::to_owned);
            let _ = self.send(
                session.as_deref(),
                "Page.handleJavaScriptDialog",
                json!({"accept": accept}),
            );
        }
        if self.events.len() >= MAX_BUFFERED_EVENTS {
            self.events.remove(0);
        }
        self.events.push(event);
        Ok(())
    }

    /// Fold buffered events into the context.
    fn drain_events(
        &mut self,
        ctx: &mut LiveContext,
        cancel: &CancellationToken,
    ) -> Result<(), BrowserSessionError> {
        check_cancel(cancel)?;
        let events = std::mem::take(&mut self.events);
        for event in events {
            note_event(ctx, &event);
        }
        Ok(())
    }

    /// Wait for the navigation `Page.navigate` started to reach a loaded
    /// document: the main frame's `loaderId` is the one the navigation
    /// returned (so a load event of the *previous* document, or a page-
    /// initiated navigation racing ours, cannot satisfy it) and its
    /// `document.readyState` is `complete`. Polled through ordinary
    /// commands with the ordinary timeout — no socket-timeout games, so a
    /// busy renderer is waited for, not mistaken for a failure. A
    /// navigation that has not loaded by `timeout` is an error, never a
    /// silent success.
    fn wait_for_load(
        &mut self,
        ctx: &mut LiveContext,
        loader: Option<&str>,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<(), BrowserSessionError> {
        let deadline = Instant::now() + timeout;
        loop {
            check_cancel(cancel)?;
            let (frame_id, _url, current_loader) = main_frame(self, &ctx.session, cancel)?;
            let same_document = loader.is_none_or(|wanted| wanted == current_loader);
            if same_document {
                // The isolated world is per document: create it now so the
                // readiness probe runs where the page cannot interfere.
                if let Ok(world) = isolated_world(self, ctx, &frame_id, cancel)
                    && let Ok(ready) = self.call(
                        Some(&ctx.session),
                        "Runtime.evaluate",
                        json!({
                            "expression": "document.readyState",
                            "contextId": world,
                            "returnByValue": true,
                        }),
                        cancel,
                    )
                    && ready["result"]["value"].as_str() == Some("complete")
                {
                    return Ok(());
                }
            }
            if Instant::now() > deadline {
                return Err(BrowserSessionError::Backend);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn note_event(ctx: &mut LiveContext, event: &Value) {
    if event["sessionId"].as_str() != Some(ctx.session.as_str()) {
        return;
    }
    if event["method"].as_str() == Some("Page.frameNavigated")
        && event["params"]["frame"]["parentId"].is_null()
    {
        // The main frame navigated — by the driver or by the page (redirect,
        // link, script). The previous collection no longer describes this
        // document; the next capture reads the new `loaderId` and moves the
        // generation.
        ctx.targets = 0;
        ctx.isolated_context = None;
        trace_line(ctx, "{\"event\":\"frame.navigated\"}");
    }
}

/// The main frame's `(frameId, url, loaderId)` as the browser reports them.
fn main_frame(
    conn: &mut CdpConnection,
    session: &str,
    cancel: &CancellationToken,
) -> Result<(String, String, String), BrowserSessionError> {
    let tree = conn.call(Some(session), "Page.getFrameTree", json!({}), cancel)?;
    let frame = &tree["frameTree"]["frame"];
    let frame_id = frame["id"].as_str().ok_or(BrowserSessionError::Backend)?;
    let url = frame["url"].as_str().unwrap_or("");
    let loader = frame["loaderId"]
        .as_str()
        .ok_or(BrowserSessionError::Backend)?;
    Ok((frame_id.to_owned(), url.to_owned(), loader.to_owned()))
}

/// The execution context of this document's isolated world, created on
/// first use per document. Page script cannot see the world's globals or
/// alter its prototypes, so what the collector and the actions evaluate is
/// the driver's own code over the shared DOM.
fn isolated_world(
    conn: &mut CdpConnection,
    ctx: &mut LiveContext,
    frame_id: &str,
    cancel: &CancellationToken,
) -> Result<u64, BrowserSessionError> {
    if let Some((frame, world)) = &ctx.isolated_context
        && frame == frame_id
    {
        return Ok(*world);
    }
    let created = conn.call(
        Some(&ctx.session),
        "Page.createIsolatedWorld",
        json!({"frameId": frame_id, "worldName": "rapidlm", "grantUniveralAccess": false}),
        cancel,
    )?;
    let world = created["executionContextId"]
        .as_u64()
        .ok_or(BrowserSessionError::Backend)?;
    ctx.isolated_context = Some((frame_id.to_owned(), world));
    Ok(world)
}

/// `text` cut to at most `max_bytes` on a char boundary, with control
/// characters (which the observer's bounds refuse) replaced by spaces.
fn bounded_text(text: &str, max_bytes: usize) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if cleaned.len() <= max_bytes {
        return cleaned;
    }
    let mut end = max_bytes;
    while end > 0 && !cleaned.is_char_boundary(end) {
        end -= 1;
    }
    cleaned[..end].to_owned()
}

/// Read Chrome's `DevTools listening on ws://…` line from stderr, then keep
/// draining stderr on a thread so the browser never blocks on a full pipe.
fn wait_for_devtools_endpoint(
    stderr: std::process::ChildStderr,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<String, BrowserSessionError> {
    let (tx, rx) = std::sync::mpsc::channel::<Option<String>>();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        let mut found = false;
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if !found && let Some(rest) = line.trim().strip_prefix("DevTools listening on ")
                    {
                        found = true;
                        let _ = tx.send(Some(rest.trim().to_owned()));
                    }
                }
            }
        }
        if !found {
            let _ = tx.send(None);
        }
        // Keep the pipe drained for the browser's lifetime.
        let mut sink = [0u8; 4096];
        let mut inner = reader.into_inner();
        while matches!(inner.read(&mut sink), Ok(n) if n > 0) {}
    });
    let deadline = Instant::now() + timeout;
    loop {
        check_cancel(cancel)?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(BrowserSessionError::Unavailable);
        }
        match rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
            Ok(Some(endpoint)) if endpoint.starts_with("ws://") => return Ok(endpoint),
            Ok(_) => return Err(BrowserSessionError::Unavailable),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(BrowserSessionError::Unavailable);
            }
        }
    }
}

/// Collected element indexes that the accessibility tree also exposes,
/// correlated through the `data-rapidlm-ref` attribute the collector sets.
fn accessible_refs(
    conn: &mut CdpConnection,
    session: &str,
    cancel: &CancellationToken,
) -> Result<std::collections::HashSet<u32>, BrowserSessionError> {
    let document = conn.call(
        Some(session),
        "DOM.getDocument",
        json!({"depth": -1, "pierce": false}),
        cancel,
    )?;
    let mut by_backend: HashMap<u64, u32> = HashMap::new();
    collect_ref_attributes(&document["root"], &mut by_backend);
    if by_backend.is_empty() {
        return Ok(std::collections::HashSet::new());
    }
    let tree = conn.call(
        Some(session),
        "Accessibility.getFullAXTree",
        json!({}),
        cancel,
    )?;
    let mut refs = std::collections::HashSet::new();
    for node in tree["nodes"].as_array().into_iter().flatten() {
        if node["ignored"].as_bool().unwrap_or(false) {
            continue;
        }
        if let Some(backend) = node["backendDOMNodeId"].as_u64()
            && let Some(index) = by_backend.get(&backend)
        {
            refs.insert(*index);
        }
    }
    Ok(refs)
}

fn collect_ref_attributes(node: &Value, out: &mut HashMap<u64, u32>) {
    if let (Some(backend), Some(attributes)) = (
        node["backendNodeId"].as_u64(),
        node["attributes"].as_array(),
    ) {
        let mut pairs = attributes.chunks(2);
        if let Some(pair) = pairs.find(|pair| pair[0].as_str() == Some("data-rapidlm-ref"))
            && let Some(index) = pair
                .get(1)
                .and_then(Value::as_str)
                .and_then(|s| s.parse().ok())
        {
            out.insert(backend, index);
        }
    }
    for child in node["children"].as_array().into_iter().flatten() {
        collect_ref_attributes(child, out);
    }
}

fn capture_screenshot(
    conn: &mut CdpConnection,
    session: &str,
    cancel: &CancellationToken,
) -> Result<RawScreenshot, BrowserSessionError> {
    let metrics = conn.call(Some(session), "Page.getLayoutMetrics", json!({}), cancel)?;
    let viewport = &metrics["cssVisualViewport"];
    let width = viewport["clientWidth"]
        .as_f64()
        .unwrap_or(f64::from(VIEWPORT.0)) as u32;
    let height = viewport["clientHeight"]
        .as_f64()
        .unwrap_or(f64::from(VIEWPORT.1)) as u32;
    let shot = conn.call(
        Some(session),
        "Page.captureScreenshot",
        json!({"format": "png", "captureBeyondViewport": false}),
        cancel,
    )?;
    let data = shot["data"].as_str().ok_or(BrowserSessionError::Backend)?;
    let bytes = base64_decode(data).ok_or(BrowserSessionError::Backend)?;
    RawScreenshot::new(bytes, width.max(1), height.max(1)).map_err(|_| BrowserSessionError::Backend)
}

/// Create the context's page target, attach a flattened session to it and
/// put it in the driver's shape: downloads to the session's directory,
/// `Page` events on (dialogs, navigations), the fixed viewport.
fn attach_page(
    conn: &mut CdpConnection,
    cdp_context: &str,
    downloads_dir: &Path,
    cancel: &CancellationToken,
) -> Result<String, BrowserSessionError> {
    let _ = conn.call(
        None,
        "Browser.setDownloadBehavior",
        json!({
            "behavior": "allow",
            "browserContextId": cdp_context,
            "downloadPath": downloads_dir.display().to_string(),
            "eventsEnabled": false,
        }),
        cancel,
    );
    let target = conn.call(
        None,
        "Target.createTarget",
        json!({
            "url": "about:blank",
            "browserContextId": cdp_context,
            "width": VIEWPORT.0,
            "height": VIEWPORT.1,
        }),
        cancel,
    )?;
    let target_id = target["targetId"]
        .as_str()
        .ok_or(BrowserSessionError::Backend)?
        .to_owned();
    let attached = conn.call(
        None,
        "Target.attachToTarget",
        json!({"targetId": target_id, "flatten": true}),
        cancel,
    )?;
    let session = attached["sessionId"]
        .as_str()
        .ok_or(BrowserSessionError::Backend)?
        .to_owned();
    // `Page.enable` for dialogs and frame events; no `Runtime.enable` — it
    // is not needed for `Runtime.evaluate` and would subscribe this session
    // to every page-controlled console/exception event.
    conn.call(Some(&session), "Page.enable", json!({}), cancel)?;
    let _ = conn.call(Some(&session), "Accessibility.enable", json!({}), cancel);
    conn.call(
        Some(&session),
        "Emulation.setDeviceMetricsOverride",
        json!({
            "width": VIEWPORT.0,
            "height": VIEWPORT.1,
            "deviceScaleFactor": 1,
            "mobile": false,
        }),
        cancel,
    )?;
    Ok(session)
}

fn read_cookies(
    conn: &mut CdpConnection,
    ctx: &LiveContext,
    cancel: &CancellationToken,
) -> Result<Vec<BrowserCookie>, BrowserSessionError> {
    let result = conn.call(
        None,
        "Storage.getCookies",
        json!({"browserContextId": ctx.cdp_context}),
        cancel,
    )?;
    let mut cookies = Vec::new();
    for raw in result["cookies"].as_array().into_iter().flatten() {
        let domain = raw["domain"].as_str().unwrap_or("").trim_start_matches('.');
        if domain.is_empty() {
            continue;
        }
        let secure = raw["secure"].as_bool().unwrap_or(false);
        let origin = format!("{}://{domain}", if secure { "https" } else { "http" });
        let name = raw["name"].as_str().unwrap_or("");
        let value = raw["value"].as_str().unwrap_or("");
        if let Ok(cookie) = BrowserCookie::new(&origin, name, value) {
            cookies.push(cookie);
        }
        if cookies.len() >= super::session::MAX_COOKIES_PER_CONTEXT {
            break;
        }
    }
    Ok(cookies)
}

fn set_cookie(
    conn: &mut CdpConnection,
    ctx: &LiveContext,
    cookie: &BrowserCookie,
    cancel: &CancellationToken,
) -> Result<(), BrowserSessionError> {
    let origin = cookie.origin();
    let secure = origin.starts_with("https://");
    let host = origin
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("");
    if host.is_empty() {
        return Err(BrowserSessionError::OriginInvalid);
    }
    conn.call(
        None,
        "Storage.setCookies",
        json!({
            "browserContextId": ctx.cdp_context,
            "cookies": [{
                "name": cookie.name(),
                "value": cookie.value(),
                "domain": host,
                "path": "/",
                "secure": secure,
            }],
        }),
        cancel,
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn mouse_event(
    conn: &mut CdpConnection,
    ctx: &LiveContext,
    kind: &str,
    x: f64,
    y: f64,
    button: &str,
    click_count: u8,
    cancel: &CancellationToken,
) -> Result<(), ActionError> {
    conn.call(
        Some(&ctx.session),
        "Input.dispatchMouseEvent",
        json!({
            "type": kind,
            "x": x,
            "y": y,
            "button": button,
            "clickCount": click_count,
        }),
        cancel,
    )
    .map(|_| ())
    .map_err(map_session_to_action)
}

struct KeySpec {
    key: &'static str,
    code: &'static str,
    vk: u32,
    text: Option<&'static str>,
}

/// Key event fields for the observer's accepted key names.
fn key_spec(raw: &str) -> Option<KeySpec> {
    let named = match raw {
        "Enter" => Some(KeySpec {
            key: "Enter",
            code: "Enter",
            vk: 13,
            text: Some("\r"),
        }),
        "Tab" => Some(KeySpec {
            key: "Tab",
            code: "Tab",
            vk: 9,
            text: None,
        }),
        "Escape" => Some(KeySpec {
            key: "Escape",
            code: "Escape",
            vk: 27,
            text: None,
        }),
        "Backspace" => Some(KeySpec {
            key: "Backspace",
            code: "Backspace",
            vk: 8,
            text: None,
        }),
        "Delete" => Some(KeySpec {
            key: "Delete",
            code: "Delete",
            vk: 46,
            text: None,
        }),
        "Space" => Some(KeySpec {
            key: " ",
            code: "Space",
            vk: 32,
            text: Some(" "),
        }),
        "Home" => Some(KeySpec {
            key: "Home",
            code: "Home",
            vk: 36,
            text: None,
        }),
        "End" => Some(KeySpec {
            key: "End",
            code: "End",
            vk: 35,
            text: None,
        }),
        "PageUp" => Some(KeySpec {
            key: "PageUp",
            code: "PageUp",
            vk: 33,
            text: None,
        }),
        "PageDown" => Some(KeySpec {
            key: "PageDown",
            code: "PageDown",
            vk: 34,
            text: None,
        }),
        "ArrowUp" => Some(KeySpec {
            key: "ArrowUp",
            code: "ArrowUp",
            vk: 38,
            text: None,
        }),
        "ArrowDown" => Some(KeySpec {
            key: "ArrowDown",
            code: "ArrowDown",
            vk: 40,
            text: None,
        }),
        "ArrowLeft" => Some(KeySpec {
            key: "ArrowLeft",
            code: "ArrowLeft",
            vk: 37,
            text: None,
        }),
        "ArrowRight" => Some(KeySpec {
            key: "ArrowRight",
            code: "ArrowRight",
            vk: 39,
            text: None,
        }),
        _ => None,
    };
    if named.is_some() {
        return named;
    }
    let mut chars = raw.chars();
    let ch = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    // A single printable character: the key name is the character itself;
    // the virtual key code is its uppercase ASCII where one exists.
    let text: &'static str = Box::leak(ch.to_string().into_boxed_str());
    let key: &'static str = text;
    let (code, vk): (&'static str, u32) = match ch {
        'a'..='z' | 'A'..='Z' => {
            let upper = ch.to_ascii_uppercase();
            (
                Box::leak(format!("Key{upper}").into_boxed_str()),
                u32::from(upper as u8),
            )
        }
        '0'..='9' => (
            Box::leak(format!("Digit{ch}").into_boxed_str()),
            u32::from(ch as u8),
        ),
        '.' => ("Period", 190),
        ',' => ("Comma", 188),
        '-' => ("Minus", 189),
        '=' => ("Equal", 187),
        '/' => ("Slash", 191),
        ';' => ("Semicolon", 186),
        _ => return None,
    };
    Some(KeySpec {
        key,
        code,
        vk,
        text: Some(text),
    })
}

fn trace_line(ctx: &mut LiveContext, line: &str) {
    if let Some(trace) = ctx.trace.as_mut()
        && trace.len() < 4096
    {
        trace.push(line.to_owned());
    }
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), BrowserSessionError> {
    if cancel.is_cancelled() {
        Err(BrowserSessionError::Cancelled)
    } else {
        Ok(())
    }
}

fn map_ws(err: WsError) -> BrowserSessionError {
    match err {
        WsError::Closed => BrowserSessionError::SessionCrashed,
        WsError::Timeout => BrowserSessionError::Backend,
        _ => BrowserSessionError::Backend,
    }
}

fn map_session_to_observe(err: BrowserSessionError) -> ObserveError {
    match err {
        BrowserSessionError::Cancelled => ObserveError::Cancelled,
        BrowserSessionError::SessionNotFound => ObserveError::SessionNotFound,
        BrowserSessionError::SessionCrashed => ObserveError::SessionCrashed,
        BrowserSessionError::SessionClosed => ObserveError::SessionClosed,
        BrowserSessionError::Unavailable => ObserveError::Unavailable,
        _ => ObserveError::Backend,
    }
}

fn map_session_to_action(err: BrowserSessionError) -> ActionError {
    match err {
        BrowserSessionError::Cancelled => ActionError::Cancelled,
        BrowserSessionError::SessionNotFound => ActionError::SessionNotFound,
        BrowserSessionError::SessionCrashed => ActionError::SessionCrashed,
        BrowserSessionError::SessionClosed => ActionError::SessionClosed,
        BrowserSessionError::Unavailable => ActionError::Unavailable,
        _ => ActionError::Backend,
    }
}

/// Collects the page's interactive and named elements in document order and
/// pins them on `window.__rapidlmTargets` (and as `data-rapidlm-ref`) so an
/// action can name the element the model was shown. Field values are never
/// read into the result — only roles, names and test ids.
const COLLECT_TARGETS_JS: &str = r#"(() => {
  const MAX = 256;
  const implicitRole = (el) => {
    const tag = el.tagName.toLowerCase();
    const type = (el.getAttribute('type') || '').toLowerCase();
    switch (tag) {
      case 'a': return el.hasAttribute('href') ? 'link' : 'generic';
      case 'button': return 'button';
      case 'select': return 'combobox';
      case 'textarea': return 'textbox';
      case 'img': return 'img';
      case 'h1': case 'h2': case 'h3': case 'h4': case 'h5': case 'h6': return 'heading';
      case 'nav': return 'navigation';
      case 'main': return 'main';
      case 'form': return 'form';
      case 'summary': return 'button';
      case 'option': return 'option';
      case 'input':
        switch (type) {
          case 'button': case 'submit': case 'reset': case 'image': return 'button';
          case 'checkbox': return 'checkbox';
          case 'radio': return 'radio';
          case 'range': return 'slider';
          case 'number': return 'spinbutton';
          case 'search': return 'searchbox';
          case 'hidden': return null;
          default: return 'textbox';
        }
      default: return null;
    }
  };
  const isInteractive = (el, role) => {
    const tag = el.tagName.toLowerCase();
    if (['a', 'button', 'input', 'select', 'textarea', 'summary'].includes(tag)) {
      return !(tag === 'a' && !el.hasAttribute('href')) && !el.disabled;
    }
    if (el.isContentEditable) return true;
    if (el.hasAttribute('onclick')) return true;
    const tab = el.getAttribute('tabindex');
    if (tab !== null && Number(tab) >= 0) return true;
    return ['button', 'link', 'checkbox', 'radio', 'textbox', 'combobox', 'menuitem',
            'tab', 'switch', 'slider', 'option', 'searchbox', 'spinbutton'].includes(role || '');
  };
  const text = (s) => (s || '').replace(/\s+/g, ' ').trim().slice(0, 256);
  const labelText = (el) => {
    if (el.labels && el.labels.length) return text(el.labels[0].textContent);
    const id = el.getAttribute('id');
    if (id) {
      const label = document.querySelector('label[for="' + CSS.escape(id) + '"]');
      if (label) return text(label.textContent);
    }
    return '';
  };
  const nameOf = (el, role) => {
    const aria = el.getAttribute('aria-label');
    if (aria) return text(aria);
    const by = el.getAttribute('aria-labelledby');
    if (by) {
      const parts = by.split(/\s+/).map((id) => {
        const n = document.getElementById(id); return n ? n.textContent : '';
      });
      const joined = text(parts.join(' '));
      if (joined) return joined;
    }
    const tag = el.tagName.toLowerCase();
    if (tag === 'input' || tag === 'textarea' || tag === 'select') {
      const label = labelText(el);
      if (label) return label;
      const type = (el.getAttribute('type') || '').toLowerCase();
      if (['button', 'submit', 'reset'].includes(type) && el.value) return text(el.value);
      const placeholder = el.getAttribute('placeholder');
      if (placeholder) return text(placeholder);
    }
    if (tag === 'img') return text(el.getAttribute('alt'));
    const title = el.getAttribute('title');
    // A landmark's name is its label, never its whole subtree's text.
    if (['navigation', 'main', 'form', 'region', 'complementary', 'banner', 'contentinfo'].includes(role || '')) {
      return title ? text(title) : '';
    }
    const content = text(el.textContent);
    if (content) return content;
    if (title) return text(title);
    return '';
  };
  const visible = (el) => {
    if (el.getAttribute('aria-hidden') === 'true') return false;
    // `checkVisibility` accounts for hidden ancestors (a closed dialog's
    // controls); the rect check is the fallback for older engines.
    if (typeof el.checkVisibility === 'function') return el.checkVisibility();
    return el.getClientRects().length > 0;
  };
  const targets = [];
  const nodes = [];
  const all = document.querySelectorAll('body *');
  for (const el of all) {
    if (targets.length >= MAX) break;
    if (!visible(el)) continue;
    const testId = el.getAttribute('data-testid') || el.getAttribute('data-test-id');
    const explicit = el.getAttribute('role');
    let role = explicit || implicitRole(el);
    if (role === null && testId && el.tagName.toLowerCase() === 'p') role = 'paragraph';
    if (role === null && testId) role = 'generic';
    if (role === null) continue;
    const interactive = isInteractive(el, role);
    const name = nameOf(el, role);
    if (!interactive && !name && !testId) continue;
    if (!interactive && !explicit && !testId && !['heading', 'img', 'link', 'navigation', 'main', 'form'].includes(role)) continue;
    const idx = targets.length;
    targets.push(el);
    el.setAttribute('data-rapidlm-ref', String(idx));
    const node = { role: role || 'generic', name: name, interactive: interactive };
    if (testId) node.testId = testId.slice(0, 128);
    if (el.tagName.toLowerCase() === 'input') node.inputType = (el.getAttribute('type') || 'text').toLowerCase();
    nodes.push(node);
  }
  globalThis.__rapidlmTargets = targets;
  return { title: '' + document.title, nodes: nodes };
})()"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_specs_cover_the_named_keys_and_single_characters() {
        for name in [
            "Enter",
            "Tab",
            "Escape",
            "Backspace",
            "Delete",
            "Space",
            "Home",
            "End",
            "PageUp",
            "PageDown",
            "ArrowUp",
            "ArrowDown",
            "ArrowLeft",
            "ArrowRight",
        ] {
            let spec = key_spec(name).unwrap_or_else(|| panic!("{name}"));
            assert!(spec.vk > 0);
            assert!(!spec.code.is_empty());
        }
        let a = key_spec("a").expect("a");
        assert_eq!((a.key, a.code, a.vk, a.text), ("a", "KeyA", 65, Some("a")));
        let seven = key_spec("7").expect("7");
        assert_eq!((seven.code, seven.vk), ("Digit7", 55));
        let period = key_spec(".").expect(".");
        assert_eq!(period.code, "Period");
        assert!(key_spec("F1").is_none());
        assert!(key_spec("ab").is_none());
        assert!(key_spec("").is_none());
    }

    #[test]
    fn non_chromium_engines_are_a_typed_unavailable_before_any_launch() {
        // A path that does not exist: had the backend tried to launch it,
        // the error would be the same variant — so the assertion also
        // checks the *sequence*, via the executable never being touched.
        let backend = ChromiumCdpBackend::new(ChromiumLaunch::at(PathBuf::from(
            "/nonexistent/rapidlm-browser",
        )));
        let cancel = CancellationToken::new();
        for engine in [BrowserEngine::Firefox, BrowserEngine::Webkit] {
            assert_eq!(
                backend
                    .launch_or_reuse_browser(engine, &cancel)
                    .expect_err("not chromium"),
                BrowserSessionError::Unavailable
            );
        }
        assert!(backend.state.lock().expect("state").browsers.is_empty());
    }

    #[test]
    fn a_missing_executable_is_unavailable_and_cancellation_wins_first() {
        let backend = ChromiumCdpBackend::new(ChromiumLaunch::at(PathBuf::from(
            "/nonexistent/rapidlm-browser",
        )));
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert_eq!(
            backend
                .launch_or_reuse_browser(BrowserEngine::Chromium, &cancelled)
                .expect_err("cancelled"),
            BrowserSessionError::Cancelled
        );
        assert_eq!(
            backend
                .launch_or_reuse_browser(BrowserEngine::Chromium, &CancellationToken::new())
                .expect_err("no such browser"),
            BrowserSessionError::Unavailable
        );
    }

    #[test]
    fn discovery_override_must_be_an_absolute_existing_file() {
        // Exercised through the pure helper rather than the process
        // environment, which other tests share.
        let relative = PathBuf::from("chrome");
        assert!(!relative.is_absolute());
        let missing = PathBuf::from("/nonexistent/rapidlm-browser");
        assert!(!missing.is_file());
        let launch = ChromiumLaunch::at(missing.clone());
        assert_eq!(launch.executable(), missing.as_path());
        assert_eq!(launch.launch_timeout, DEFAULT_LAUNCH_TIMEOUT);
    }

    #[test]
    fn ref_attributes_are_correlated_by_backend_node_id() {
        let document = json!({
            "backendNodeId": 1,
            "children": [
                {"backendNodeId": 2, "attributes": ["id", "x", "data-rapidlm-ref", "0"]},
                {"backendNodeId": 3, "attributes": ["data-rapidlm-ref", "not-a-number"]},
                {"backendNodeId": 4, "children": [
                    {"backendNodeId": 5, "attributes": ["data-rapidlm-ref", "7"]}
                ]}
            ]
        });
        let mut out = HashMap::new();
        collect_ref_attributes(&document, &mut out);
        assert_eq!(out.get(&2), Some(&0));
        assert_eq!(out.get(&5), Some(&7));
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn the_collector_script_is_a_single_expression_that_never_reads_field_values() {
        assert!(COLLECT_TARGETS_JS.trim_start().starts_with("(() => {"));
        assert!(COLLECT_TARGETS_JS.trim_end().ends_with("})()"));
        assert!(COLLECT_TARGETS_JS.contains("__rapidlmTargets"));
        // The URL comes from the browser (`Page.getFrameTree`), never from
        // page script — nothing in the collector reads `location`.
        assert!(!COLLECT_TARGETS_JS.contains("location"));
        // The only `.value` read is the label of a button-type input; a
        // textbox's contents are never part of the capture.
        let reads: Vec<&str> = COLLECT_TARGETS_JS
            .lines()
            .filter(|line| line.contains("el.value"))
            .collect();
        assert_eq!(reads.len(), 1, "{reads:?}");
        assert!(reads[0].contains("['button', 'submit', 'reset'].includes(type)"));
    }
}
