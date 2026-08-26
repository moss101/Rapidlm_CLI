#![forbid(unsafe_code)]

pub mod android;
pub mod ios;
pub mod trace;

pub use android::action::{
    AccessibilitySnapshotRef, ActionKind, ActionStatus, AndroidAction, AndroidActionError,
    AndroidActionReceipt, AndroidActionRequest, AndroidActor, AndroidKey, AndroidObservation,
    AndroidObservationView, AssertionResult, DEFAULT_ACTION_TIMEOUT, DEFAULT_OBSERVE_TIMEOUT,
    DeepLinkUri, DeviceDump, DeviceStateId, FakeAndroidUi, Geometry, HostAdbUi, MAX_ACTION_TIMEOUT,
    MAX_DEEPLINK_BYTES, MAX_HIERARCHY_BYTES, MAX_NODES, MAX_OBSERVE_TIMEOUT, MAX_SCREENSHOT_BYTES,
    MAX_TEXT_BYTES, ObservationId, ObserveRequest, Orientation, Point, Rect, SCREENSHOT_MEDIA_TYPE,
    ScreenshotMeta, SecretAwareText, SemanticSource, SemanticTarget, SemanticTargetView, TargetRef,
    TypedAdbCommand, UiAssertion, UiNode, adb_argv, encode_input_text,
};
pub use android::manager::{
    AndroidCapability, AndroidDeviceHandle, AndroidDeviceId, AndroidManager, AndroidManagerError,
    AndroidSpec, AvdInventoryEntry, AvdName, DEFAULT_START_TIMEOUT, DeviceSelection, DeviceSerial,
    DiscoveredEmulator, DiscoveredOwnership, EmulatorProcessId, MAX_LIVE_DEVICES,
    MAX_START_TIMEOUT, MIN_CONSOLE_PORT, NetworkAdbPolicy, OwnerId, OwnershipKind,
};
pub use android::snapshot::{
    AndroidSnapshotBackend, AndroidSnapshotError, AndroidSnapshots, BASELINE_SNAPSHOT_NAME,
    DEFAULT_SNAPSHOT_TIMEOUT, FakeAndroidSnapshots, HostAndroidSnapshots, MAX_EVIDENCE_EVENTS,
    MAX_SNAPSHOT_NAME_BYTES, MAX_SNAPSHOT_TIMEOUT, MAX_SNAPSHOTS_PER_DEVICE,
    MobileSnapshotCapability, SnapshotEvidence, SnapshotFailure, SnapshotId, SnapshotName,
    SnapshotOp, SnapshotReceipt, SnapshotRequest, SnapshotStatus, SnapshotTarget,
    TypedSnapshotCommand, snapshot_adb_argv,
};
pub use ios::remote::{
    DEFAULT_REMOTE_TIMEOUT, DelegatedOperation, IOS_SIMULATOR_CAPABILITY, IosRemoteDelegator,
    IosRemoteError, IosRemoteWorkLease, LocalIosAvailability, MAX_REMOTE_ARTIFACTS,
    MAX_REMOTE_OUTPUT_BYTES, MAX_REMOTE_TIMEOUT, MacosWorkerId, MacosWorkerProfile,
    RemoteActionKind, RemoteActionStatus, RemoteDelegationConfig, RemoteDelegationPolicy,
    RemoteDeviceLease, RemoteKey, RemoteLeaseParties, RemoteMobileAction, RemoteMobileActionResult,
    RemoteMobileObservation, RemoteObservationId, RemoteObserveRequest, RemoteSecretAwareText,
    RemoteTargetRef, RemoteWorkLeaseIssuer, UntrustedRemoteEnvelope, WorkerPlatform,
};
pub use ios::simctl::{
    AppBundlePath, BootRequest, BundleId, DEFAULT_BOOT_TIMEOUT, DEFAULT_OP_TIMEOUT, DeviceUdid,
    DiscoveredSimulator, IosCapability, IosDeviceHandle, IosSimctlBackend, IosSimctlError,
    LaunchReceipt, MAX_BOOT_TIMEOUT, MAX_BUNDLE_ID_BYTES, MAX_HOST_OUTPUT_BYTES, MAX_NAME_BYTES,
    MAX_OP_TIMEOUT, MAX_PATH_BYTES, RuntimeName, ScreenshotPath, ScreenshotReceipt, SimulatorName,
    SimulatorState, TypedSimctlCommand, XCRUN_PATH, simctl_argv,
};
pub use trace::{
    CanonicalAction, DeviceLogChunk, DeviceLogKind, MANIFEST_MEDIA_TYPE, MAX_LOG_CHUNK_BYTES,
    MAX_MANIFEST_BYTES, MAX_TRACE_ENTRIES, MobilePlatform, MobileTraceBundle, MobileTraceId,
    MobileTraceLog, RedactionLabel, TraceError, TraceScreenshot, TraceWarning, TraceWarningCode,
    WorkerPlacement, finish_trace,
};
