# Computer Use — Browser, Desktop, TUI, Mobile and Remote Surfaces

## 1. Architecture

Computer Use is a kernel subsystem and typed Node Executor, not a special prompt-only loop. It normalizes structured browser, native accessibility, TUI semantics, mobile simulators and pixel/coordinate fallback.

Target order: **DOM/test-id → accessibility/native control → TUI semantic region → bounded vision target → raw coordinate**.

## 2. Core contracts

```rust
pub struct Observation {
    pub observation_id: ObservationId,
    pub surface: SurfaceRef,
    pub generation: u64,
    pub viewport: Rect,
    pub scale: ScaleInfo,
    pub structured: Vec<SemanticTarget>,
    pub screenshot: Option<ArtifactId>,
    pub captured_at: Instant,
}

pub enum ComputerAction {
    Move { target: PointerTarget },
    Click { target: PointerTarget, button: MouseButton, count: u8 },
    Down { button: MouseButton }, Up { button: MouseButton },
    Drag { from: PointerTarget, to: PointerTarget, duration_ms: u32 },
    Scroll { dx: i32, dy: i32, target: Option<PointerTarget> },
    Type { text: SecretOrPlainText },
    Key { key: KeyCode }, Chord { keys: Vec<KeyCode> },
    Wait { condition: WaitCondition, timeout_ms: u32 },
    Screenshot { region: Option<Rect> },
    CursorPosition,
    App { action: AppAction }, Window { action: WindowAction },
}
```

Every state-changing action references an Observation/generation; stale generations reject and force reobserve.

## 3. Driver architecture

Dedicated computer worker/process abstracts Browser/CDP/Playwright-compatible, macOS AX, Windows UI Automation, Linux AT-SPI plus platform pointer backend, Android ADB/emulator and remote-desktop transports. Captured Cursor implementation research informs coordinate scaling, batching, cursor state, settle delays and screenshot-on-failure patterns; V3 does not assume a single global backend.

Coordinate scaler validates source/target dimensions/aspect and maps logical/model coordinates to device pixels; impossible/unsafe transforms fail.

## 4. Browser structured surface

Browser tools include navigate/back/forward/reload/tabs, DOM/AX snapshot/query, get text/read page, click/type/select/form, upload, download with policy, JS execution when policy permits, console/network capture, screenshot, viewport, cookies/storage through constrained interfaces and CDP allowlist. Page content is untrusted data and prompt-injection tagged.

## 5. Settle and batching

Pure pointer moves/compatible key sequences may batch. After navigation, click/type/drag or other state-changing action, a settle policy waits for a structured condition (DOM/network/app idle or bounded delay) and captures a fresh observation before dependent action. On failure, capture screenshot + structured state + console/log excerpt if policy permits.

## 6. Preview Supervisor

For web/UI implementation RapidLM owns a Preview node that starts/attaches to dev server, discovers/allocates port, monitors process/HTTP readiness, captures compiler/HMR/console/network errors and emits evidence. A preview failure may automatically create a diagnosis/repair branch without spending model turns polling.

## 7. Mobile

Android: emulator snapshot pool, ADB install/launch/logcat, accessibility/UIAutomator where available, coordinate fallback. iOS: `simctl` and accessibility/automation on macOS/remote mac worker. Mobile actions/events use the same Observation/Action/Evidence abstractions.

## 8. Human takeover

`ControlLease` is separate from security CapabilityLease. Taking control of terminal/desktop/mobile input pauses conflicting agent actions; read-only watchers may continue. On return RapidLM checks workspace mutations, invalidates stale observations and reobserves. MFA/CAPTCHA is escalated to human; no bypass automation.

## 9. Evidence

Before/after screenshots, DOM/AX snapshots, video segments, console/network logs, app logs and deterministic assertions can attach to criteria. The verifier chooses evidence appropriate to the criterion rather than treating screenshots as universally sufficient.

## 10. Commands

See `docs/reference/computer-use-command-reference.md`. Required families: observe, click/double-click/move/down/up/drag, scroll, type, key/chord, wait, screenshot, cursor, app/window, browser structured actions, mobile, record, take-control/release-control.
