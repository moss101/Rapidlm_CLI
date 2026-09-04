//! iOS Simulator backend via `xcrun simctl`.
//!
//! [`IosSimctlBackend::availability`] gates every host operation. Device UDID
//! is a typed handle. Non-macOS returns `mobile.capability_unavailable` and
//! never emulates a local runtime. Commands are typed argv (T-CU-01, T-CU-05).

use std::collections::HashMap;
use std::error::Error;
use std::fmt::{self, Debug};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use capability_broker::CancellationToken;
use protocol::ErrorCode;

/// Absolute `xcrun` used on macOS. PATH is never searched.
pub const XCRUN_PATH: &str = "/usr/bin/xcrun";

/// Maximum UTF-8 bytes for a simulator display name or runtime label.
pub const MAX_NAME_BYTES: usize = 128;

/// Maximum UTF-8 bytes for a bundle identifier.
pub const MAX_BUNDLE_ID_BYTES: usize = 256;

/// Maximum UTF-8 bytes for an `.app` or screenshot path.
pub const MAX_PATH_BYTES: usize = 1024;

/// Maximum screenshot file size accepted after `simctl io screenshot`.
pub const MAX_SCREENSHOT_BYTES: u64 = 2 * 1024 * 1024;

/// Maximum `simctl list` stdout retained.
pub const MAX_HOST_OUTPUT_BYTES: usize = 64 * 1024;

/// Default bound for boot.
pub const DEFAULT_BOOT_TIMEOUT: Duration = Duration::from_secs(60);

/// Absolute upper bound accepted on [`BootRequest::with_timeout`].
pub const MAX_BOOT_TIMEOUT: Duration = Duration::from_secs(180);

/// Default bound for install/launch/screenshot/status/shutdown.
pub const DEFAULT_OP_TIMEOUT: Duration = Duration::from_secs(30);

/// Absolute upper bound for non-boot operations.
pub const MAX_OP_TIMEOUT: Duration = Duration::from_secs(120);

const HOST_POLL: Duration = Duration::from_millis(10);
const UDID_LEN: usize = 36;
const AVAILABILITY_TIMEOUT: Duration = Duration::from_secs(5);
const SCREENSHOT_MARKER: &[u8] = b"rapidlm.ios.screenshot.v1";

/// Host probe for local iOS Simulator control.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct IosCapability {
    macos: bool,
    xcrun: bool,
    simctl: bool,
}

/// Simulator UDID. The implicit `booted` alias and physical IDs are rejected.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct DeviceUdid(String);

/// Reverse-DNS application identifier. Not a shell token.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct BundleId(String);

/// Absolute `.app` bundle path. Not a host shell fragment.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct AppBundlePath(PathBuf);

/// Absolute `.png` destination. Not a host shell fragment.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct ScreenshotPath(PathBuf);

/// Bounded simulator product name from `simctl list`.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct SimulatorName(String);

/// Bounded runtime label (`iOS 17.2`).
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct RuntimeName(String);

/// Lifecycle state reported by `simctl list`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum SimulatorState {
    Shutdown,
    Booted,
    Booting,
    Creating,
    ShuttingDown,
    Unknown,
}

/// Inventory row. Discovery does not grant control.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredSimulator {
    udid: DeviceUdid,
    name: SimulatorName,
    runtime: RuntimeName,
    state: SimulatorState,
}

/// Booted (or already-booted) simulator snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IosDeviceHandle {
    udid: DeviceUdid,
    name: SimulatorName,
    runtime: RuntimeName,
    state: SimulatorState,
    generation: u64,
}

/// Typed boot request. UI/page text cannot construct a UDID.
#[derive(Clone, Debug)]
pub struct BootRequest {
    udid: DeviceUdid,
    timeout: Duration,
    cancel: CancellationToken,
}

/// Successful `simctl launch`. PID is optional host metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchReceipt {
    udid: DeviceUdid,
    bundle: BundleId,
    pid: Option<u32>,
}

/// Successful screenshot write. Bytes stay on disk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScreenshotReceipt {
    udid: DeviceUdid,
    dest: ScreenshotPath,
    bytes: u64,
}

/// Typed `simctl` argv kind. Callers cannot inject a shell script.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TypedSimctlCommand {
    ListDevices,
    Status {
        udid: DeviceUdid,
    },
    Boot {
        udid: DeviceUdid,
    },
    Shutdown {
        udid: DeviceUdid,
    },
    Install {
        udid: DeviceUdid,
        app: AppBundlePath,
    },
    Launch {
        udid: DeviceUdid,
        bundle: BundleId,
    },
    Screenshot {
        udid: DeviceUdid,
        dest: ScreenshotPath,
    },
}

/// Typed simctl failure. Display never echoes UDID, paths, or bundle IDs.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum IosSimctlError {
    Cancelled,
    Timeout,
    TimeoutInvalid,
    CapabilityUnavailable,
    InvalidUdid,
    InvalidBundleId,
    InvalidAppPath,
    InvalidScreenshotPath,
    DeviceNotFound,
    DeviceClosed,
    PhysicalDeviceDenied,
    BootFailed,
    InstallFailed,
    LaunchFailed,
    ScreenshotBound,
    Unavailable,
    Backend,
    Io,
}

/// Discover/boot/control iOS Simulator through typed `xcrun simctl` argv.
pub struct IosSimctlBackend {
    kind: BackendKind,
}

enum BackendKind {
    Host { capability: IosCapability },
    Fake(Mutex<FakeState>),
}

struct FakeState {
    capability: bool,
    devices: HashMap<String, FakeDevice>,
    last_command: Option<TypedSimctlCommand>,
}

struct FakeDevice {
    name: SimulatorName,
    runtime: RuntimeName,
    state: SimulatorState,
    generation: u64,
    installed: Vec<AppBundlePath>,
    launched: Vec<BundleId>,
}

impl IosCapability {
    pub const fn unavailable() -> Self {
        Self {
            macos: false,
            xcrun: false,
            simctl: false,
        }
    }

    pub const fn macos(self) -> bool {
        self.macos
    }

    pub const fn xcrun(self) -> bool {
        self.xcrun
    }

    pub const fn simctl(self) -> bool {
        self.simctl
    }

    pub const fn is_ready(self) -> bool {
        self.macos && self.xcrun && self.simctl
    }

    /// Remote worker capability name. Never implied as a local fallback.
    pub const fn remote_hint(self) -> &'static str {
        "ios-simulator"
    }
}

impl DeviceUdid {
    /// Accept only a CoreSimulator UUID. `booted` and physical IDs fail closed.
    pub fn parse(raw: &str) -> Result<Self, IosSimctlError> {
        if raw.eq_ignore_ascii_case("booted") {
            return Err(IosSimctlError::InvalidUdid);
        }
        if looks_like_uuid(raw) {
            return Ok(Self(raw.to_ascii_uppercase()));
        }
        if looks_like_physical(raw) {
            return Err(IosSimctlError::PhysicalDeviceDenied);
        }
        Err(IosSimctlError::InvalidUdid)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl BundleId {
    /// Reverse-DNS identifier. Shell tokens and option injection fail.
    pub fn parse(raw: &str) -> Result<Self, IosSimctlError> {
        if raw.is_empty() || raw.len() > MAX_BUNDLE_ID_BYTES {
            return Err(IosSimctlError::InvalidBundleId);
        }
        if raw.starts_with('.') || raw.ends_with('.') || raw.contains("..") {
            return Err(IosSimctlError::InvalidBundleId);
        }
        if !raw.contains('.') {
            return Err(IosSimctlError::InvalidBundleId);
        }
        let ok = raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
        if !ok {
            return Err(IosSimctlError::InvalidBundleId);
        }
        Ok(Self(raw.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AppBundlePath {
    /// Absolute `.app` path. Relative, `..`, and shell tokens fail.
    pub fn parse(raw: impl AsRef<Path>) -> Result<Self, IosSimctlError> {
        let path = raw.as_ref();
        let Some(text) = path.to_str() else {
            return Err(IosSimctlError::InvalidAppPath);
        };
        if text.is_empty() || text.len() > MAX_PATH_BYTES {
            return Err(IosSimctlError::InvalidAppPath);
        }
        if !path.is_absolute() || !text.ends_with(".app") {
            return Err(IosSimctlError::InvalidAppPath);
        }
        if !is_safe_host_path(path) {
            return Err(IosSimctlError::InvalidAppPath);
        }
        Ok(Self(path.to_path_buf()))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn as_str(&self) -> &str {
        self.0.to_str().unwrap_or(".app")
    }
}

impl ScreenshotPath {
    /// Absolute `.png` path. Relative, `..`, and shell tokens fail.
    pub fn parse(raw: impl AsRef<Path>) -> Result<Self, IosSimctlError> {
        let path = raw.as_ref();
        let Some(text) = path.to_str() else {
            return Err(IosSimctlError::InvalidScreenshotPath);
        };
        if text.is_empty() || text.len() > MAX_PATH_BYTES {
            return Err(IosSimctlError::InvalidScreenshotPath);
        }
        if !path.is_absolute() || !text.ends_with(".png") {
            return Err(IosSimctlError::InvalidScreenshotPath);
        }
        if !is_safe_host_path(path) {
            return Err(IosSimctlError::InvalidScreenshotPath);
        }
        Ok(Self(path.to_path_buf()))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn as_str(&self) -> &str {
        self.0.to_str().unwrap_or(".png")
    }
}

impl SimulatorName {
    pub fn parse(raw: &str) -> Result<Self, IosSimctlError> {
        bound_label(raw).map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl RuntimeName {
    pub fn parse(raw: &str) -> Result<Self, IosSimctlError> {
        bound_label(raw).map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl SimulatorState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Shutdown => "Shutdown",
            Self::Booted => "Booted",
            Self::Booting => "Booting",
            Self::Creating => "Creating",
            Self::ShuttingDown => "Shutting Down",
            Self::Unknown => "Unknown",
        }
    }

    pub const fn is_booted(self) -> bool {
        matches!(self, Self::Booted)
    }

    fn parse(raw: &str) -> Self {
        match raw {
            "Shutdown" => Self::Shutdown,
            "Booted" => Self::Booted,
            "Booting" => Self::Booting,
            "Creating" => Self::Creating,
            "Shutting Down" => Self::ShuttingDown,
            _ => Self::Unknown,
        }
    }
}

impl DiscoveredSimulator {
    pub fn udid(&self) -> &DeviceUdid {
        &self.udid
    }

    pub fn name(&self) -> &SimulatorName {
        &self.name
    }

    pub fn runtime(&self) -> &RuntimeName {
        &self.runtime
    }

    pub fn state(&self) -> SimulatorState {
        self.state
    }
}

impl IosDeviceHandle {
    pub fn udid(&self) -> &DeviceUdid {
        &self.udid
    }

    pub fn name(&self) -> &SimulatorName {
        &self.name
    }

    pub fn runtime(&self) -> &RuntimeName {
        &self.runtime
    }

    pub fn state(&self) -> SimulatorState {
        self.state
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }
}

impl BootRequest {
    pub fn new(udid: DeviceUdid) -> Self {
        Self {
            udid,
            timeout: DEFAULT_BOOT_TIMEOUT,
            cancel: CancellationToken::new(),
        }
    }

    pub fn parse(udid: &str) -> Result<Self, IosSimctlError> {
        Ok(Self::new(DeviceUdid::parse(udid)?))
    }

    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Result<Self, IosSimctlError> {
        if timeout.is_zero() || timeout > MAX_BOOT_TIMEOUT {
            return Err(IosSimctlError::TimeoutInvalid);
        }
        self.timeout = timeout;
        Ok(self)
    }

    pub fn udid(&self) -> &DeviceUdid {
        &self.udid
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn cancel(&self) -> &CancellationToken {
        &self.cancel
    }
}

impl LaunchReceipt {
    pub fn udid(&self) -> &DeviceUdid {
        &self.udid
    }

    pub fn bundle(&self) -> &BundleId {
        &self.bundle
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }
}

impl ScreenshotReceipt {
    pub fn udid(&self) -> &DeviceUdid {
        &self.udid
    }

    pub fn dest(&self) -> &ScreenshotPath {
        &self.dest
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl IosSimctlBackend {
    /// Host capability gate. Non-macOS and missing Xcode fail closed.
    pub fn availability() -> Result<IosCapability, IosSimctlError> {
        let capability = Self::probe(&CancellationToken::new())?;
        if !capability.is_ready() {
            return Err(IosSimctlError::CapabilityUnavailable);
        }
        Ok(capability)
    }

    /// Diagnostic probe. Never invents a ready capability off macOS.
    pub fn probe(cancel: &CancellationToken) -> Result<IosCapability, IosSimctlError> {
        check_cancel(cancel)?;
        if !cfg!(target_os = "macos") {
            return Ok(IosCapability::unavailable());
        }
        let xcrun = Path::new(XCRUN_PATH);
        if !xcrun.is_file() {
            return Ok(IosCapability {
                macos: true,
                xcrun: false,
                simctl: false,
            });
        }
        match run_xcrun(&["--find", "simctl"], AVAILABILITY_TIMEOUT, cancel) {
            Ok(body) => {
                let found = body.trim();
                let simctl = Path::new(found).is_absolute() && found.ends_with("simctl");
                Ok(IosCapability {
                    macos: true,
                    xcrun: true,
                    simctl,
                })
            }
            Err(IosSimctlError::Cancelled) => Err(IosSimctlError::Cancelled),
            Err(IosSimctlError::TimeoutInvalid) => Err(IosSimctlError::TimeoutInvalid),
            Err(_) => Ok(IosCapability {
                macos: true,
                xcrun: true,
                simctl: false,
            }),
        }
    }

    /// Open the host backend. Fails with [`IosSimctlError::CapabilityUnavailable`]
    /// when [`Self::availability`] is not ready.
    pub fn open() -> Result<Self, IosSimctlError> {
        let capability = Self::availability()?;
        Ok(Self {
            kind: BackendKind::Host { capability },
        })
    }

    /// In-process stand-in. Does not execute `xcrun` or touch a simulator.
    pub fn fake() -> Self {
        Self {
            kind: BackendKind::Fake(Mutex::new(FakeState {
                capability: true,
                devices: default_fake_devices(),
                last_command: None,
            })),
        }
    }

    pub fn fake_without_capability() -> Self {
        let backend = Self::fake();
        if let BackendKind::Fake(state) = &backend.kind
            && let Ok(mut inner) = state.lock()
        {
            inner.capability = false;
        }
        backend
    }

    pub fn capability(&self) -> Result<IosCapability, IosSimctlError> {
        match &self.kind {
            BackendKind::Host { capability } => Ok(*capability),
            BackendKind::Fake(state) => {
                let inner = state.lock().map_err(|_| IosSimctlError::Unavailable)?;
                if inner.capability {
                    Ok(IosCapability {
                        macos: cfg!(target_os = "macos"),
                        xcrun: false,
                        simctl: false,
                    })
                } else {
                    Ok(IosCapability::unavailable())
                }
            }
        }
    }

    pub fn last_command(&self) -> Result<Option<TypedSimctlCommand>, IosSimctlError> {
        match &self.kind {
            BackendKind::Host { .. } => Ok(None),
            BackendKind::Fake(state) => {
                let inner = state.lock().map_err(|_| IosSimctlError::Unavailable)?;
                Ok(inner.last_command.clone())
            }
        }
    }

    /// Read-only inventory. Does not boot or grant a handle.
    pub fn discover(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Vec<DiscoveredSimulator>, IosSimctlError> {
        self.gate(cancel)?;
        match &self.kind {
            BackendKind::Host { .. } => {
                let output =
                    run_typed(&TypedSimctlCommand::ListDevices, DEFAULT_OP_TIMEOUT, cancel)?;
                parse_simctl_list(&output)
            }
            BackendKind::Fake(state) => {
                let mut inner = state.lock().map_err(|_| IosSimctlError::Unavailable)?;
                inner.last_command = Some(TypedSimctlCommand::ListDevices);
                Ok(inner
                    .devices
                    .iter()
                    .map(|(udid, device)| DiscoveredSimulator {
                        udid: DeviceUdid(udid.clone()),
                        name: device.name.clone(),
                        runtime: device.runtime.clone(),
                        state: device.state,
                    })
                    .collect())
            }
        }
    }

    pub fn boot(&self, request: BootRequest) -> Result<IosDeviceHandle, IosSimctlError> {
        self.gate(request.cancel())?;
        if request.timeout.is_zero() || request.timeout > MAX_BOOT_TIMEOUT {
            return Err(IosSimctlError::TimeoutInvalid);
        }
        match &self.kind {
            BackendKind::Host { .. } => self.host_boot(&request),
            BackendKind::Fake(state) => {
                let mut inner = state.lock().map_err(|_| IosSimctlError::Unavailable)?;
                inner.last_command = Some(TypedSimctlCommand::Boot {
                    udid: request.udid.clone(),
                });
                let device = inner
                    .devices
                    .get_mut(request.udid.as_str())
                    .ok_or(IosSimctlError::DeviceNotFound)?;
                device.state = SimulatorState::Booted;
                device.generation = device.generation.saturating_add(1);
                Ok(IosDeviceHandle {
                    udid: request.udid.clone(),
                    name: device.name.clone(),
                    runtime: device.runtime.clone(),
                    state: device.state,
                    generation: device.generation,
                })
            }
        }
    }

    pub fn shutdown(
        &self,
        udid: &DeviceUdid,
        cancel: &CancellationToken,
    ) -> Result<(), IosSimctlError> {
        self.gate(cancel)?;
        let command = TypedSimctlCommand::Shutdown { udid: udid.clone() };
        match &self.kind {
            BackendKind::Host { .. } => match run_typed(&command, DEFAULT_OP_TIMEOUT, cancel) {
                Ok(_) => Ok(()),
                Err(IosSimctlError::Backend) => match self.status(udid, cancel)? {
                    SimulatorState::Shutdown => Ok(()),
                    SimulatorState::Unknown => Err(IosSimctlError::DeviceNotFound),
                    _ => Err(IosSimctlError::Backend),
                },
                Err(err) => Err(err),
            },
            BackendKind::Fake(state) => {
                let mut inner = state.lock().map_err(|_| IosSimctlError::Unavailable)?;
                inner.last_command = Some(command);
                let device = inner
                    .devices
                    .get_mut(udid.as_str())
                    .ok_or(IosSimctlError::DeviceNotFound)?;
                device.state = SimulatorState::Shutdown;
                Ok(())
            }
        }
    }

    pub fn install(
        &self,
        udid: &DeviceUdid,
        app: &AppBundlePath,
        cancel: &CancellationToken,
    ) -> Result<(), IosSimctlError> {
        self.gate(cancel)?;
        let command = TypedSimctlCommand::Install {
            udid: udid.clone(),
            app: app.clone(),
        };
        match &self.kind {
            BackendKind::Host { .. } => run_typed(&command, DEFAULT_OP_TIMEOUT, cancel).map(|_| ()),
            BackendKind::Fake(state) => {
                let mut inner = state.lock().map_err(|_| IosSimctlError::Unavailable)?;
                inner.last_command = Some(command);
                let device = inner
                    .devices
                    .get_mut(udid.as_str())
                    .ok_or(IosSimctlError::DeviceNotFound)?;
                if device.state == SimulatorState::Shutdown {
                    return Err(IosSimctlError::DeviceClosed);
                }
                if !device.installed.iter().any(|item| item == app) {
                    device.installed.push(app.clone());
                }
                Ok(())
            }
        }
    }

    pub fn launch(
        &self,
        udid: &DeviceUdid,
        bundle: &BundleId,
        cancel: &CancellationToken,
    ) -> Result<LaunchReceipt, IosSimctlError> {
        self.gate(cancel)?;
        let command = TypedSimctlCommand::Launch {
            udid: udid.clone(),
            bundle: bundle.clone(),
        };
        match &self.kind {
            BackendKind::Host { .. } => {
                let output = run_typed(&command, DEFAULT_OP_TIMEOUT, cancel)?;
                Ok(LaunchReceipt {
                    udid: udid.clone(),
                    bundle: bundle.clone(),
                    pid: parse_launch_pid(&output),
                })
            }
            BackendKind::Fake(state) => {
                let mut inner = state.lock().map_err(|_| IosSimctlError::Unavailable)?;
                inner.last_command = Some(command);
                let device = inner
                    .devices
                    .get_mut(udid.as_str())
                    .ok_or(IosSimctlError::DeviceNotFound)?;
                if device.state != SimulatorState::Booted {
                    return Err(IosSimctlError::DeviceClosed);
                }
                device.launched.push(bundle.clone());
                Ok(LaunchReceipt {
                    udid: udid.clone(),
                    bundle: bundle.clone(),
                    pid: Some(4242),
                })
            }
        }
    }

    pub fn screenshot(
        &self,
        udid: &DeviceUdid,
        dest: &ScreenshotPath,
        cancel: &CancellationToken,
    ) -> Result<ScreenshotReceipt, IosSimctlError> {
        self.gate(cancel)?;
        let command = TypedSimctlCommand::Screenshot {
            udid: udid.clone(),
            dest: dest.clone(),
        };
        match &self.kind {
            BackendKind::Host { .. } => {
                run_typed(&command, DEFAULT_OP_TIMEOUT, cancel).map(|_| ())?;
                screenshot_receipt(udid, dest)
            }
            BackendKind::Fake(state) => {
                let mut inner = state.lock().map_err(|_| IosSimctlError::Unavailable)?;
                inner.last_command = Some(command);
                let device = inner
                    .devices
                    .get(udid.as_str())
                    .ok_or(IosSimctlError::DeviceNotFound)?;
                if device.state != SimulatorState::Booted {
                    return Err(IosSimctlError::DeviceClosed);
                }
                drop(inner);
                write_screenshot_marker(dest)?;
                screenshot_receipt(udid, dest)
            }
        }
    }

    pub fn status(
        &self,
        udid: &DeviceUdid,
        cancel: &CancellationToken,
    ) -> Result<SimulatorState, IosSimctlError> {
        self.gate(cancel)?;
        let command = TypedSimctlCommand::Status { udid: udid.clone() };
        match &self.kind {
            BackendKind::Host { .. } => {
                let output = run_typed(&command, DEFAULT_OP_TIMEOUT, cancel)?;
                let devices = parse_simctl_list(&output)?;
                devices
                    .into_iter()
                    .find(|item| item.udid == *udid)
                    .map(|item| item.state)
                    .ok_or(IosSimctlError::DeviceNotFound)
            }
            BackendKind::Fake(state) => {
                let mut inner = state.lock().map_err(|_| IosSimctlError::Unavailable)?;
                inner.last_command = Some(command);
                inner
                    .devices
                    .get(udid.as_str())
                    .map(|device| device.state)
                    .ok_or(IosSimctlError::DeviceNotFound)
            }
        }
    }

    fn host_boot(&self, request: &BootRequest) -> Result<IosDeviceHandle, IosSimctlError> {
        let command = TypedSimctlCommand::Boot {
            udid: request.udid.clone(),
        };
        match run_typed(&command, request.timeout, request.cancel()) {
            Ok(_) => {}
            Err(IosSimctlError::Backend) => {
                if self.status(&request.udid, request.cancel())? != SimulatorState::Booted {
                    return Err(IosSimctlError::BootFailed);
                }
            }
            Err(err) => return Err(err),
        }
        self.wait_booted(&request.udid, request.timeout, request.cancel())?;
        let devices = self.discover(request.cancel())?;
        devices
            .into_iter()
            .find(|item| item.udid == request.udid)
            .map(|item| IosDeviceHandle {
                udid: item.udid,
                name: item.name,
                runtime: item.runtime,
                state: item.state,
                generation: 1,
            })
            .ok_or(IosSimctlError::DeviceNotFound)
    }

    fn wait_booted(
        &self,
        udid: &DeviceUdid,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<(), IosSimctlError> {
        let deadline = Instant::now() + timeout;
        loop {
            check_cancel(cancel)?;
            if Instant::now() >= deadline {
                return Err(IosSimctlError::Timeout);
            }
            match self.status(udid, cancel) {
                Ok(state) if state.is_booted() => return Ok(()),
                Ok(_) => std::thread::sleep(HOST_POLL),
                Err(IosSimctlError::DeviceNotFound) => std::thread::sleep(HOST_POLL),
                Err(err) => return Err(err),
            }
        }
    }

    fn gate(&self, cancel: &CancellationToken) -> Result<(), IosSimctlError> {
        check_cancel(cancel)?;
        match &self.kind {
            BackendKind::Host { capability } => {
                if capability.is_ready() {
                    Ok(())
                } else {
                    Err(IosSimctlError::CapabilityUnavailable)
                }
            }
            BackendKind::Fake(state) => {
                let inner = state.lock().map_err(|_| IosSimctlError::Unavailable)?;
                if inner.capability {
                    Ok(())
                } else {
                    Err(IosSimctlError::CapabilityUnavailable)
                }
            }
        }
    }
}

impl IosSimctlError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::TimeoutInvalid => "timeout_invalid",
            Self::CapabilityUnavailable => "capability_unavailable",
            Self::InvalidUdid => "invalid_udid",
            Self::InvalidBundleId => "invalid_bundle_id",
            Self::InvalidAppPath => "invalid_app_path",
            Self::InvalidScreenshotPath => "invalid_screenshot_path",
            Self::DeviceNotFound => "device_not_found",
            Self::DeviceClosed => "device_closed",
            Self::PhysicalDeviceDenied => "physical_device_denied",
            Self::BootFailed => "boot_failed",
            Self::InstallFailed => "install_failed",
            Self::LaunchFailed => "launch_failed",
            Self::ScreenshotBound => "screenshot_bound",
            Self::Unavailable => "unavailable",
            Self::Backend => "backend",
            Self::Io => "io",
        }
    }

    pub const fn code(self) -> ErrorCode {
        match self {
            Self::Cancelled
            | Self::TimeoutInvalid
            | Self::InvalidUdid
            | Self::InvalidBundleId
            | Self::InvalidAppPath
            | Self::InvalidScreenshotPath => ErrorCode::ToolInvalidArguments,
            Self::Timeout => ErrorCode::ProcessTimeout,
            Self::CapabilityUnavailable => ErrorCode::MobileCapabilityUnavailable,
            Self::DeviceNotFound | Self::DeviceClosed => ErrorCode::SessionNotFound,
            Self::PhysicalDeviceDenied => ErrorCode::PolicyDenied,
            Self::BootFailed
            | Self::InstallFailed
            | Self::LaunchFailed
            | Self::ScreenshotBound
            | Self::Unavailable
            | Self::Backend
            | Self::Io => ErrorCode::InternalUnexpected,
        }
    }

    pub const fn remote_worker_hint(self) -> Option<&'static str> {
        match self {
            Self::CapabilityUnavailable => Some("ios-simulator"),
            _ => None,
        }
    }
}

impl fmt::Display for IosSimctlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for IosSimctlError {}

impl Debug for DeviceUdid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("DeviceUdid").field(&self.0).finish()
    }
}

impl Debug for BundleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("BundleId").field(&self.0).finish()
    }
}

impl Debug for AppBundlePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("AppBundlePath").field(&self.0).finish()
    }
}

impl Debug for ScreenshotPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ScreenshotPath").field(&self.0).finish()
    }
}

impl Debug for SimulatorName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SimulatorName").field(&self.0).finish()
    }
}

impl Debug for RuntimeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("RuntimeName").field(&self.0).finish()
    }
}

impl Debug for IosSimctlBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            BackendKind::Host { capability } => f
                .debug_struct("IosSimctlBackend")
                .field("kind", &"host")
                .field("capability", capability)
                .finish(),
            BackendKind::Fake(_) => f
                .debug_struct("IosSimctlBackend")
                .field("kind", &"fake")
                .finish_non_exhaustive(),
        }
    }
}

/// Typed argv for one `xcrun simctl` operation. Never a concatenated shell line.
pub fn simctl_argv(command: &TypedSimctlCommand) -> Vec<String> {
    match command {
        TypedSimctlCommand::ListDevices => {
            vec!["simctl".into(), "list".into(), "devices".into()]
        }
        TypedSimctlCommand::Status { udid } => {
            vec![
                "simctl".into(),
                "list".into(),
                "devices".into(),
                udid.as_str().to_owned(),
            ]
        }
        TypedSimctlCommand::Boot { udid } => {
            vec!["simctl".into(), "boot".into(), udid.as_str().to_owned()]
        }
        TypedSimctlCommand::Shutdown { udid } => {
            vec!["simctl".into(), "shutdown".into(), udid.as_str().to_owned()]
        }
        TypedSimctlCommand::Install { udid, app } => {
            vec![
                "simctl".into(),
                "install".into(),
                udid.as_str().to_owned(),
                app.as_str().to_owned(),
            ]
        }
        TypedSimctlCommand::Launch { udid, bundle } => {
            vec![
                "simctl".into(),
                "launch".into(),
                udid.as_str().to_owned(),
                bundle.as_str().to_owned(),
            ]
        }
        TypedSimctlCommand::Screenshot { udid, dest } => {
            vec![
                "simctl".into(),
                "io".into(),
                udid.as_str().to_owned(),
                "screenshot".into(),
                dest.as_str().to_owned(),
            ]
        }
    }
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), IosSimctlError> {
    if cancel.is_cancelled() {
        Err(IosSimctlError::Cancelled)
    } else {
        Ok(())
    }
}

fn looks_like_uuid(raw: &str) -> bool {
    if raw.len() != UDID_LEN {
        return false;
    }
    let bytes = raw.as_bytes();
    if bytes[8] != b'-' || bytes[13] != b'-' || bytes[18] != b'-' || bytes[23] != b'-' {
        return false;
    }
    bytes.iter().enumerate().all(|(idx, byte)| match idx {
        8 | 13 | 18 | 23 => true,
        _ => byte.is_ascii_hexdigit(),
    })
}

fn looks_like_physical(raw: &str) -> bool {
    if raw.is_empty() || raw.len() > 40 {
        return false;
    }
    let hex_or_dash = raw.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-');
    hex_or_dash && raw.bytes().any(|b| b.is_ascii_hexdigit())
}

fn is_safe_host_path(path: &Path) -> bool {
    let Some(text) = path.to_str() else {
        return false;
    };
    if text.contains('\0') || text.chars().any(char::is_control) {
        return false;
    }
    if text.contains(';')
        || text.contains('&')
        || text.contains('|')
        || text.contains('$')
        || text.contains('`')
        || text.contains('\n')
    {
        return false;
    }
    path.components().all(|part| match part {
        std::path::Component::RootDir | std::path::Component::Normal(_) => true,
        std::path::Component::CurDir
        | std::path::Component::ParentDir
        | std::path::Component::Prefix(_) => false,
    })
}

fn bound_label(raw: &str) -> Result<String, IosSimctlError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_NAME_BYTES {
        return Err(IosSimctlError::Backend);
    }
    if trimmed.contains('\0') || trimmed.chars().any(char::is_control) {
        return Err(IosSimctlError::Backend);
    }
    Ok(trimmed.to_owned())
}

fn default_fake_devices() -> HashMap<String, FakeDevice> {
    let mut devices = HashMap::new();
    let udid = "A1B2C3D4-E5F6-7890-ABCD-EF1234567890".to_owned();
    devices.insert(
        udid,
        FakeDevice {
            name: SimulatorName("iPhone 15".to_owned()),
            runtime: RuntimeName("iOS 17.2".to_owned()),
            state: SimulatorState::Shutdown,
            generation: 0,
            installed: Vec::new(),
            launched: Vec::new(),
        },
    );
    devices
}

fn parse_simctl_list(body: &str) -> Result<Vec<DiscoveredSimulator>, IosSimctlError> {
    let mut devices = Vec::new();
    let mut runtime = RuntimeName("iOS".to_owned());
    for raw in body.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("==") {
            continue;
        }
        if let Some(label) = runtime_header(line) {
            runtime = RuntimeName::parse(label)?;
            continue;
        }
        if line.contains("(unavailable") {
            continue;
        }
        if let Some(device) = parse_device_line(line, &runtime)? {
            devices.push(device);
        }
    }
    Ok(devices)
}

fn runtime_header(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if let Some(inner) = trimmed.strip_prefix("-- ") {
        return inner.strip_suffix(" --");
    }
    None
}

fn parse_device_line(
    line: &str,
    runtime: &RuntimeName,
) -> Result<Option<DiscoveredSimulator>, IosSimctlError> {
    let Some((start, udid)) = find_udid(line) else {
        return Ok(None);
    };
    let name_raw = line.get(..start).unwrap_or("").trim();
    let name_raw = name_raw.strip_suffix('(').unwrap_or(name_raw).trim();
    let after = line.get(start + UDID_LEN..).unwrap_or("");
    let state = parse_state_after_udid(after);
    if name_raw.is_empty() {
        return Ok(None);
    }
    Ok(Some(DiscoveredSimulator {
        udid,
        name: SimulatorName::parse(name_raw)?,
        runtime: runtime.clone(),
        state,
    }))
}

fn parse_state_after_udid(after: &str) -> SimulatorState {
    let rest = after.trim_start_matches(')').trim();
    let rest = rest.strip_prefix('(').unwrap_or(rest);
    let label = rest.split(')').next().unwrap_or(rest).trim();
    if label.is_empty() {
        SimulatorState::Unknown
    } else {
        SimulatorState::parse(label)
    }
}

fn find_udid(line: &str) -> Option<(usize, DeviceUdid)> {
    if line.len() < UDID_LEN {
        return None;
    }
    for start in 0..=line.len() - UDID_LEN {
        let end = start + UDID_LEN;
        // A real UDID is pure ASCII (hex digits and dashes), so a genuine
        // match always has both endpoints on a char boundary — but this
        // loop probes every byte offset, including ones inside a non-ASCII
        // device name (`simctl rename` accepts arbitrary Unicode), where
        // slicing would panic before `looks_like_uuid` ever gets a chance
        // to reject the candidate.
        if !line.is_char_boundary(start) || !line.is_char_boundary(end) {
            continue;
        }
        let candidate = &line[start..end];
        if looks_like_uuid(candidate) && DeviceUdid::parse(candidate).is_ok() {
            return Some((start, DeviceUdid(candidate.to_ascii_uppercase())));
        }
    }
    None
}

fn parse_launch_pid(body: &str) -> Option<u32> {
    let line = body.lines().next()?.trim();
    let pid = line.rsplit(':').next()?.trim();
    pid.parse().ok()
}

fn screenshot_receipt(
    udid: &DeviceUdid,
    dest: &ScreenshotPath,
) -> Result<ScreenshotReceipt, IosSimctlError> {
    let meta = fs::metadata(dest.as_path()).map_err(|_| IosSimctlError::Io)?;
    if !meta.is_file() || meta.len() > MAX_SCREENSHOT_BYTES {
        return Err(IosSimctlError::ScreenshotBound);
    }
    Ok(ScreenshotReceipt {
        udid: udid.clone(),
        dest: dest.clone(),
        bytes: meta.len(),
    })
}

fn write_screenshot_marker(dest: &ScreenshotPath) -> Result<(), IosSimctlError> {
    if let Some(parent) = dest.as_path().parent() {
        fs::create_dir_all(parent).map_err(|_| IosSimctlError::Io)?;
    }
    let mut file = File::create(dest.as_path()).map_err(|_| IosSimctlError::Io)?;
    file.write_all(SCREENSHOT_MARKER)
        .map_err(|_| IosSimctlError::Io)?;
    file.sync_all().map_err(|_| IosSimctlError::Io)?;
    Ok(())
}

fn run_typed(
    command: &TypedSimctlCommand,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<String, IosSimctlError> {
    let argv = simctl_argv(command);
    let args: Vec<&str> = argv.iter().map(String::as_str).collect();
    match run_xcrun(&args, timeout, cancel) {
        Ok(body) => Ok(body),
        Err(IosSimctlError::Backend) if matches!(command, TypedSimctlCommand::Install { .. }) => {
            Err(IosSimctlError::InstallFailed)
        }
        Err(IosSimctlError::Backend) if matches!(command, TypedSimctlCommand::Launch { .. }) => {
            Err(IosSimctlError::LaunchFailed)
        }
        Err(err) => Err(err),
    }
}

fn run_xcrun(
    args: &[&str],
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<String, IosSimctlError> {
    check_cancel(cancel)?;
    if timeout.is_zero() {
        return Err(IosSimctlError::TimeoutInvalid);
    }
    if !cfg!(target_os = "macos") {
        return Err(IosSimctlError::CapabilityUnavailable);
    }
    let buf = crate::host_process::run_bounded_capturing_stdout(
        Path::new(XCRUN_PATH),
        args,
        None,
        timeout,
        MAX_HOST_OUTPUT_BYTES,
        cancel,
    )
    .map_err(|err| match err {
        crate::host_process::HostRunError::TimeoutInvalid => IosSimctlError::TimeoutInvalid,
        crate::host_process::HostRunError::Cancelled => IosSimctlError::Cancelled,
        crate::host_process::HostRunError::Timeout => IosSimctlError::Timeout,
        crate::host_process::HostRunError::NonZeroExit
        | crate::host_process::HostRunError::OutputTooLarge => IosSimctlError::Backend,
        crate::host_process::HostRunError::Spawn | crate::host_process::HostRunError::Wait => {
            IosSimctlError::Io
        }
    })?;
    String::from_utf8(buf).map_err(|_| IosSimctlError::Backend)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    struct TempEnv {
        root: PathBuf,
    }

    impl TempEnv {
        fn create() -> Self {
            let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir()
                .join(format!("rapidlm-ios-simctl-{}-{seq}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).expect("root");
            Self { root }
        }

        fn png(&self, name: &str) -> ScreenshotPath {
            ScreenshotPath::parse(self.root.join(name)).expect("png")
        }

        fn app(&self, name: &str) -> AppBundlePath {
            AppBundlePath::parse(self.root.join(name)).expect("app")
        }
    }

    impl Drop for TempEnv {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    fn fixture_udid() -> DeviceUdid {
        DeviceUdid::parse("A1B2C3D4-E5F6-7890-ABCD-EF1234567890").expect("udid")
    }

    #[test]
    fn availability_is_unavailable_off_macos() {
        let probe = IosSimctlBackend::probe(&live()).expect("probe");
        assert_eq!(probe.remote_hint(), "ios-simulator");
        if cfg!(target_os = "macos") {
            match IosSimctlBackend::availability() {
                Ok(cap) => assert!(cap.is_ready()),
                Err(IosSimctlError::CapabilityUnavailable) => {
                    assert!(!probe.is_ready());
                    assert_eq!(
                        IosSimctlError::CapabilityUnavailable.code(),
                        ErrorCode::MobileCapabilityUnavailable
                    );
                }
                Err(err) => panic!("unexpected availability: {err}"),
            }
        } else {
            assert!(!probe.macos());
            assert!(!probe.is_ready());
            let err = IosSimctlBackend::availability().expect_err("non-mac");
            assert_eq!(err, IosSimctlError::CapabilityUnavailable);
            assert_eq!(err.code(), ErrorCode::MobileCapabilityUnavailable);
            assert_eq!(err.remote_worker_hint(), Some("ios-simulator"));
            assert_eq!(probe.remote_hint(), "ios-simulator");
            assert_eq!(
                IosSimctlBackend::open().expect_err("open"),
                IosSimctlError::CapabilityUnavailable
            );
        }
    }

    #[test]
    fn fake_does_not_emulate_host_availability() {
        let backend = IosSimctlBackend::fake();
        backend.discover(&live()).expect("fake discover");
        if !cfg!(target_os = "macos") {
            let err = IosSimctlBackend::availability().expect_err("host");
            assert_eq!(err.code(), ErrorCode::MobileCapabilityUnavailable);
            assert!(!backend.capability().expect("cap").is_ready());
        }
    }

    #[test]
    fn availability_gates_fake_without_capability() {
        let backend = IosSimctlBackend::fake_without_capability();
        let err = backend.discover(&live()).expect_err("gated");
        assert_eq!(err, IosSimctlError::CapabilityUnavailable);
        assert_eq!(err.code(), ErrorCode::MobileCapabilityUnavailable);
        assert_eq!(
            backend
                .boot(BootRequest::new(fixture_udid()))
                .expect_err("boot"),
            IosSimctlError::CapabilityUnavailable
        );
    }

    #[test]
    fn argv_fixtures_cover_boot_install_launch_screenshot_status() {
        let udid = fixture_udid();
        let app = AppBundlePath::parse("/tmp/Fixture.app").expect("app");
        let dest = ScreenshotPath::parse("/tmp/screen.png").expect("png");
        let bundle = BundleId::parse("com.example.fixture").expect("bundle");

        let boot = simctl_argv(&TypedSimctlCommand::Boot { udid: udid.clone() });
        assert_eq!(boot, ["simctl", "boot", udid.as_str()]);

        let install = simctl_argv(&TypedSimctlCommand::Install {
            udid: udid.clone(),
            app: app.clone(),
        });
        assert_eq!(
            install,
            ["simctl", "install", udid.as_str(), "/tmp/Fixture.app"]
        );

        let launch = simctl_argv(&TypedSimctlCommand::Launch {
            udid: udid.clone(),
            bundle: bundle.clone(),
        });
        assert_eq!(
            launch,
            ["simctl", "launch", udid.as_str(), "com.example.fixture"]
        );

        let shot = simctl_argv(&TypedSimctlCommand::Screenshot {
            udid: udid.clone(),
            dest: dest.clone(),
        });
        assert_eq!(
            shot,
            [
                "simctl",
                "io",
                udid.as_str(),
                "screenshot",
                "/tmp/screen.png"
            ]
        );

        let status = simctl_argv(&TypedSimctlCommand::Status { udid: udid.clone() });
        assert_eq!(status, ["simctl", "list", "devices", udid.as_str()]);

        let list = simctl_argv(&TypedSimctlCommand::ListDevices);
        assert_eq!(list, ["simctl", "list", "devices"]);

        for argv in [boot, install, launch, shot, status, list] {
            assert_eq!(argv.first().map(String::as_str), Some("simctl"));
            assert!(argv.iter().all(|part| !part.contains(';')));
            assert!(argv.iter().all(|part| !part.contains("&&")));
            assert!(argv.iter().all(|part| !part.contains('|')));
            assert!(argv.iter().all(|part| part != "sh" && part != "-c"));
        }
    }

    #[test]
    fn fake_discover_boot_install_launch_screenshot_status() {
        let env = TempEnv::create();
        let backend = IosSimctlBackend::fake();
        let found = backend.discover(&live()).expect("discover");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].state(), SimulatorState::Shutdown);
        assert_eq!(found[0].udid(), &fixture_udid());

        let handle = backend
            .boot(BootRequest::new(fixture_udid()))
            .expect("boot");
        assert_eq!(handle.state(), SimulatorState::Booted);
        assert_eq!(handle.generation(), 1);
        assert_eq!(
            backend.status(&fixture_udid(), &live()).expect("status"),
            SimulatorState::Booted
        );

        let app = env.app("Fixture.app");
        backend
            .install(&fixture_udid(), &app, &live())
            .expect("install");
        let launched = backend
            .launch(
                &fixture_udid(),
                &BundleId::parse("com.example.fixture").expect("bundle"),
                &live(),
            )
            .expect("launch");
        assert_eq!(launched.pid(), Some(4242));

        let dest = env.png("screen.png");
        let shot = backend
            .screenshot(&fixture_udid(), &dest, &live())
            .expect("shot");
        assert!(shot.bytes() > 0);
        assert!(dest.as_path().is_file());

        match backend.last_command().expect("cmd") {
            Some(TypedSimctlCommand::Screenshot { .. }) => {}
            other => panic!("expected screenshot command, got {other:?}"),
        }
    }

    #[test]
    fn install_and_launch_require_booted_fake_device() {
        let env = TempEnv::create();
        let backend = IosSimctlBackend::fake();
        let app = env.app("Fixture.app");
        assert_eq!(
            backend
                .install(&fixture_udid(), &app, &live())
                .expect_err("closed"),
            IosSimctlError::DeviceClosed
        );
        backend
            .boot(BootRequest::new(fixture_udid()))
            .expect("boot");
        backend
            .shutdown(&fixture_udid(), &live())
            .expect("shutdown");
        assert_eq!(
            backend.status(&fixture_udid(), &live()).expect("state"),
            SimulatorState::Shutdown
        );
        assert_eq!(
            backend
                .launch(
                    &fixture_udid(),
                    &BundleId::parse("com.example.fixture").expect("bundle"),
                    &live()
                )
                .expect_err("closed"),
            IosSimctlError::DeviceClosed
        );
    }

    #[test]
    fn udid_rejects_booted_alias_physical_and_shell() {
        assert_eq!(
            DeviceUdid::parse("booted").expect_err("alias"),
            IosSimctlError::InvalidUdid
        );
        assert_eq!(
            DeviceUdid::parse("00008030-001A0D123456802E").expect_err("physical"),
            IosSimctlError::PhysicalDeviceDenied
        );
        assert_eq!(
            DeviceUdid::parse("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").expect_err("legacy"),
            IosSimctlError::PhysicalDeviceDenied
        );
        for raw in [
            "",
            "not-a-udid",
            "A1B2C3D4-E5F6-7890-ABCD-EF1234567890;rm",
            "A1B2C3D4-E5F6-7890-ABCD-EF1234567890 && reboot",
            "../A1B2C3D4-E5F6-7890-ABCD-EF1234567890",
        ] {
            assert_eq!(
                DeviceUdid::parse(raw).expect_err(raw),
                IosSimctlError::InvalidUdid,
                "{raw}"
            );
        }
        DeviceUdid::parse("a1b2c3d4-e5f6-7890-abcd-ef1234567890").expect("case");
    }

    #[test]
    fn bundle_and_paths_reject_shell_tokens() {
        for raw in [
            "",
            "no-dot",
            ".hidden",
            "com.example.",
            "com..example.app",
            "com.example.app;rm",
            "com.example.app && reboot",
            "com.example.$(reboot)",
        ] {
            assert_eq!(
                BundleId::parse(raw).expect_err(raw),
                IosSimctlError::InvalidBundleId,
                "{raw}"
            );
        }
        BundleId::parse("com.example.fixture").expect("ok");

        for raw in [
            "Fixture.app",
            "/tmp/../Fixture.app",
            "/tmp/Fixture.app;rm",
            "/tmp/Fixture.app && wipe",
            "/tmp/Fixture.app|$HOME",
            "/tmp/Fixture",
        ] {
            assert_eq!(
                AppBundlePath::parse(raw).expect_err(raw),
                IosSimctlError::InvalidAppPath,
                "{raw}"
            );
        }
        AppBundlePath::parse("/tmp/Fixture.app").expect("app");

        for raw in [
            "screen.png",
            "/tmp/../screen.png",
            "/tmp/screen.png;rm",
            "/tmp/screen.jpg",
        ] {
            assert_eq!(
                ScreenshotPath::parse(raw).expect_err(raw),
                IosSimctlError::InvalidScreenshotPath,
                "{raw}"
            );
        }
        ScreenshotPath::parse("/tmp/screen.png").expect("png");
    }

    #[test]
    fn parse_simctl_list_skips_unavailable_and_extracts_state() {
        let body = "\
== Devices ==
-- iOS 17.2 --
    iPhone 15 (A1B2C3D4-E5F6-7890-ABCD-EF1234567890) (Shutdown)
    iPad Pro (12.9-inch) (6th generation) (11111111-2222-3333-4444-555555555555) (Booted)
    iPhone 13 (22222222-3333-4444-5555-666666666666) (Shutdown) (unavailable, runtime profile not found)
-- watchOS 10.2 --
    Apple Watch (33333333-4444-5555-6666-777777777777) (Creating)
";
        let devices = parse_simctl_list(body).expect("parse");
        assert_eq!(devices.len(), 3);
        assert_eq!(devices[0].name().as_str(), "iPhone 15");
        assert_eq!(devices[0].state(), SimulatorState::Shutdown);
        assert_eq!(
            devices[1].name().as_str(),
            "iPad Pro (12.9-inch) (6th generation)"
        );
        assert_eq!(devices[1].state(), SimulatorState::Booted);
        assert_eq!(devices[2].runtime().as_str(), "watchOS 10.2");
        assert_eq!(devices[2].state(), SimulatorState::Creating);
    }

    #[test]
    fn parse_simctl_list_does_not_panic_on_a_non_ascii_device_name() {
        // `xcrun simctl rename <udid> "<name>"` accepts arbitrary Unicode,
        // so a real device list line can contain multi-byte UTF-8 well
        // before the UDID itself (which is always pure ASCII). find_udid's
        // sliding byte-window must skip candidate positions that would
        // straddle a character boundary rather than panicking.
        let body = "\
== Devices ==
-- iOS 17.2 --
    Café Test (A1B2C3D4-E5F6-7890-ABCD-EF1234567890) (Shutdown)
    \u{1F4F1} Emoji Phone (11111111-2222-3333-4444-555555555555) (Booted)
";
        let devices = parse_simctl_list(body).expect("parse must not panic");
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].name().as_str(), "Café Test");
        assert_eq!(devices[1].name().as_str(), "\u{1F4F1} Emoji Phone");
    }

    #[test]
    fn cancellation_and_timeout_bounds() {
        let backend = IosSimctlBackend::fake();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            backend.discover(&cancel).expect_err("cancel"),
            IosSimctlError::Cancelled
        );
        assert_eq!(
            BootRequest::new(fixture_udid())
                .with_timeout(Duration::ZERO)
                .expect_err("zero"),
            IosSimctlError::TimeoutInvalid
        );
        assert_eq!(
            BootRequest::new(fixture_udid())
                .with_timeout(MAX_BOOT_TIMEOUT + Duration::from_secs(1))
                .expect_err("max"),
            IosSimctlError::TimeoutInvalid
        );
    }

    #[test]
    fn unknown_device_is_not_found() {
        let backend = IosSimctlBackend::fake();
        let missing = DeviceUdid::parse("FFFFFFFF-EEEE-DDDD-CCCC-BBBBBBBBBBBB").expect("udid");
        assert_eq!(
            backend
                .boot(BootRequest::new(missing.clone()))
                .expect_err("boot"),
            IosSimctlError::DeviceNotFound
        );
        assert_eq!(
            backend.status(&missing, &live()).expect_err("status"),
            IosSimctlError::DeviceNotFound
        );
    }

    #[test]
    fn error_display_does_not_echo_udid_or_path() {
        let err = IosSimctlError::CapabilityUnavailable;
        assert_eq!(err.to_string(), "capability_unavailable");
        assert!(!err.to_string().contains("A1B2"));
        assert!(!err.to_string().contains('/'));
        assert!(!err.to_string().contains("xcrun"));
        assert_eq!(
            IosSimctlError::PhysicalDeviceDenied.to_string(),
            "physical_device_denied"
        );
        assert_eq!(
            IosSimctlError::PhysicalDeviceDenied.code(),
            ErrorCode::PolicyDenied
        );
    }

    #[test]
    fn run_xcrun_never_executes_off_macos() {
        if cfg!(target_os = "macos") {
            return;
        }
        let err = run_xcrun(
            &["simctl", "list", "devices"],
            Duration::from_secs(1),
            &live(),
        )
        .expect_err("no host");
        assert_eq!(err, IosSimctlError::CapabilityUnavailable);
        assert_eq!(err.code(), ErrorCode::MobileCapabilityUnavailable);
    }

    #[test]
    fn parse_launch_pid_reads_last_field() {
        assert_eq!(
            parse_launch_pid("com.example.fixture: 12345\n"),
            Some(12345)
        );
        assert_eq!(parse_launch_pid("not-a-pid"), None);
    }
}
