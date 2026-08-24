# Computer Use and Mobile Automation API Contract — V2

Both desktop/browser/TUI and mobile surfaces use `observe → act → verify`. Stateful actions require a current Observation ID and surface generation.

```rust
pub struct SurfaceRef {
    pub session_id: ComputerSessionId,
    pub surface_id: SurfaceId,
    pub kind: SurfaceKind,
    pub generation: u64,
}

pub enum SurfaceKind {
    BrowserPage,
    Desktop,
    Window,
    Tui,
    AndroidEmulator,
    IosSimulator,
    RemoteDesktop,
}

pub struct Observation {
    pub id: ObservationId,
    pub surface: SurfaceRef,
    pub state_hash: Digest,
    pub accessibility: Option<AccessibilitySnapshotRef>,
    pub dom: Option<DomSnapshotRef>,
    pub screenshot: Option<ArtifactRef>,
    pub visual_delta: Option<VisualDeltaRef>,
    pub focused_window: Option<WindowRef>,
    pub captured_at: DateTime<Utc>,
}

pub enum TargetRef {
    Dom(SemanticSelector),
    Accessibility(AccessibilityNodeRef),
    Tui(TuiRegionRef),
    Visual { observation: ObservationId, region: Rect, label: String },
    Coordinate { observation: ObservationId, point: Point },
}

pub enum ComputerAction {
    Click { button: MouseButton, count: u8 },
    Drag { to: TargetRef },
    Scroll { dx: i32, dy: i32 },
    TypeText { value: SecretAwareString },
    Key(KeyCode),
    Chord(Vec<KeyCode>),
    FocusWindow,
    ResizeWindow { width: u32, height: u32 },
    LaunchApp(AppRef),
    Navigate(Url),
    UploadFile(ArtifactRef),
    Download(DownloadExpectation),
}

pub struct ComputerActionRequest {
    pub observation_id: ObservationId,
    pub action: ComputerAction,
    pub target: Option<TargetRef>,
    pub preconditions: Vec<UiAssertion>,
    pub expected: Vec<UiAssertion>,
    pub timeout_ms: u64,
}

pub struct ActionResult {
    pub action_event: EventRef,
    pub status: ActionStatus,
    pub after: Observation,
    pub assertions: Vec<AssertionResult>,
    pub evidence: Vec<EvidenceId>,
}
```

Contract rules:

- target resolution order is DOM/test-id/accessibility → TUI semantics → visual region → coordinate fallback;
- coordinate target is invalid after surface generation/resolution/window geometry changes;
- secrets are resolved from `SecretHandle` only at executor boundary and are redacted from recordings/events;
- sensitive actions map to dedicated Capability classes;
- downloads/uploads are staged through Artifact Store rather than arbitrary host paths;
- CAPTCHA/MFA may trigger human takeover; agent must not bypass challenge controls;
- Android may combine ADB side-channel operations with the same visual SurfaceRef; iOS may combine `simctl`/XCTest with simulator-window Computer Use.
