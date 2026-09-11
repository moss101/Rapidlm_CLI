#![forbid(unsafe_code)]

pub mod desktop {
    pub mod backend;
    pub mod linux;
    pub mod macos;
    pub mod windows;
    pub use backend::{
        AccessibilityNodeRef, AccessibilitySnapshotRef, ActionKind, ActionReceiptId, ActionStatus,
        AppRef, DEFAULT_ACT_TIMEOUT, DEFAULT_OBSERVE_TIMEOUT, DesktopAction, DesktopActionReceipt,
        DesktopActionRequest, DesktopActor, DesktopBackend, DesktopCapabilities, DesktopCapture,
        DesktopError, DesktopHealth, DesktopHealthReason, DesktopNodeCapture, DesktopObservation,
        DesktopObservationView, DesktopObserveRequest, DesktopPlatform, DesktopSessionId,
        DesktopTargetRef, DesktopWindowCapture, DisplayGeometry, FakeDesktopBackend, KeyCode,
        MAX_ACT_TIMEOUT, MAX_NODES, MAX_OBSERVE_TIMEOUT, MAX_WINDOWS, MouseButton, ObservationId,
        Point, Rect, ResolvedDesktopTarget, ScreenshotMeta, SecretAwareString, SemanticSource,
        SemanticTarget, SemanticTargetView, WindowInfo, WindowRef, supports_action,
    };
    pub use linux::{
        AtspiActionClass, LinuxAtspiAction, LinuxAtspiHost, LinuxAtspiNode, LinuxAtspiProbe,
        LinuxAtspiSnapshot, LinuxAtspiWindow, LinuxDesktopBackend, LiveLinuxAtspiHost,
        ScriptedLinuxAtspiHost, atspi_action_supports_action, atspi_role_is_password,
        classify_atspi_action, map_atspi_actions, normalize_atspi_role,
    };
    pub use macos::{
        LiveMacosAxHost, MacosAxAction, MacosAxHost, MacosAxNode, MacosAxProbe, MacosAxSnapshot,
        MacosAxWindow, MacosDesktopBackend, NODE_REF_PREFIX, ScriptedMacosAxHost,
        WINDOW_REF_PREFIX, ax_role_is_secure, normalize_ax_role,
    };
    pub use windows::{
        LiveWindowsUiaHost, ScriptedWindowsUiaHost, UiaPatternClass, WindowsDesktopBackend,
        WindowsUiaHost, WindowsUiaNode, WindowsUiaPattern, WindowsUiaProbe, WindowsUiaSnapshot,
        WindowsUiaWindow, classify_uia_pattern, map_uia_patterns, normalize_uia_control_type,
        uia_element_is_password, uia_pattern_supports_action,
    };
}

pub mod browser {
    pub mod action;
    pub mod fence;
    pub mod observe;
    pub mod policy;
    pub mod security;
    pub mod session;
    pub mod takeover;
    pub mod trace;
    pub mod verify;
    pub use action::{
        ActRequest, ActionError, ActionKind, ActionReceipt, ActionReceiptId, ActionStatus,
        BrowserActor, DEFAULT_ACT_TIMEOUT, FakePageActor, KeyCode, MAX_ACT_TIMEOUT,
        MAX_CLICK_COUNT, MAX_SCROLL_ABS, MAX_STABLE_REF_BYTES, MAX_TYPE_BYTES, MouseButton,
        PageActor, ResolvedTarget, SecretAwareString, TargetSelector, UiAction, act,
    };
    pub use fence::{FenceError, FencedContent, SurfaceSource, TrustClass};
    pub use observe::{
        AccessibilitySnapshotRef, BrowserObserver, DomSnapshotRef, FakePage, MAX_NAME_BYTES,
        MAX_OBSERVE_TIMEOUT, MAX_ROLE_BYTES, MAX_SCREENSHOT_BYTES, MAX_SCREENSHOT_HEIGHT,
        MAX_SCREENSHOT_WIDTH, MAX_TARGETS, MAX_TEST_ID_BYTES, MAX_TITLE_BYTES, MAX_URL_BYTES,
        Observation, ObservationId, ObservationView, ObserveError, ObserveRequest, PageCapture,
        PageNode, PageSnapshot, SCREENSHOT_MEDIA_TYPE, ScreenshotMeta, SemanticSource,
        SemanticTarget, SemanticTargetView, observe,
    };
    pub use policy::{BatchDecision, SettlePolicy, batchable, settle_policy};
    pub use security::{
        BrowserGateAction, CapabilityIntent, ClipboardOp, DOWNLOAD_STAGING_PATH, DownloadDest,
        MAX_FILE_PATH_BYTES, SecurityError, SensitiveClass, UploadSource, authorize_browser_action,
        authorize_intents, classify_browser_action,
    };
    pub use session::{
        BrowserCookie, BrowserEngine, BrowserManager, BrowserProfile, BrowserSession,
        BrowserSessionError, BrowserSessionId, BrowserSpec, MAX_COOKIE_NAME_BYTES,
        MAX_COOKIE_VALUE_BYTES, MAX_COOKIES_PER_CONTEXT, MAX_LIVE_SESSIONS, MAX_ORIGIN_BYTES,
        MAX_PROFILE_NAME_BYTES, MAX_TRACE_BYTES, NewContextRequest, PersistentProfileName,
        PersistentProfilePolicy, PlaywrightBackend, PlaywrightBrowserId, PlaywrightContextId,
        SessionCleanup, SessionState, TRACE_MEDIA_TYPE,
    };
    pub use trace::{
        BrowserTraceBundle, BrowserTraceLog, MANIFEST_MEDIA_TYPE, MAX_MANIFEST_BYTES,
        MAX_METHOD_BYTES, MAX_TRACE_ENTRIES, MAX_ZIP_BYTES, NetworkMetadata, TRACE_MANIFEST_SCHEMA,
        TRACE_ZIP_MEDIA_TYPE, TraceError, TraceWarning, TraceWarningCode, finish_trace,
    };
    pub use verify::{
        AssertionResult, BrowserVerifier, DEFAULT_VERIFY_TIMEOUT, MAX_PREDICATES,
        MAX_VERIFY_TIMEOUT, VerificationClause, VerificationPredicate, VerificationResult,
        VerificationStatus, VerifyError, verify,
    };
}

pub use browser::{
    AccessibilitySnapshotRef, ActRequest, ActionError, ActionKind, ActionReceipt, ActionReceiptId,
    ActionStatus, AssertionResult, BrowserActor, BrowserCookie, BrowserEngine, BrowserGateAction,
    BrowserManager, BrowserObserver, BrowserProfile, BrowserSession, BrowserSessionError,
    BrowserSessionId, BrowserSpec, BrowserTraceBundle, BrowserTraceLog, BrowserVerifier,
    CapabilityIntent, ClipboardOp, DomSnapshotRef, DownloadDest, FakePageActor, KeyCode,
    MANIFEST_MEDIA_TYPE, MAX_LIVE_SESSIONS, MAX_SCREENSHOT_BYTES, MAX_TARGETS, MouseButton,
    NetworkMetadata, NewContextRequest, Observation, ObservationId, ObservationView, ObserveError,
    ObserveRequest, PageActor, PageCapture, PersistentProfileName, PersistentProfilePolicy,
    PlaywrightBackend, ResolvedTarget, ScreenshotMeta, SecretAwareString, SecurityError,
    SemanticSource, SemanticTarget, SensitiveClass, SessionCleanup, SessionState,
    TRACE_ZIP_MEDIA_TYPE, TargetSelector, TraceError, TraceWarning, TraceWarningCode, UiAction,
    UploadSource, VerificationClause, VerificationPredicate, VerificationResult,
    VerificationStatus, VerifyError, act, authorize_browser_action, classify_browser_action,
    finish_trace, observe, verify,
};
