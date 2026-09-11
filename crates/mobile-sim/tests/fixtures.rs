//! Deterministic mobile simulator mock backends for CI (T-CU-01, T-CU-02).
//!
//! Android `FakeAndroidUi` and iOS `IosSimctlBackend::fake` implement the
//! production observe/act/verify contracts. Host iOS is never silently
//! emulated off macOS.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use capability_broker::CancellationToken;
use mobile_sim::android::action::AndroidUiBackend;
use mobile_sim::{
    ActionStatus, AndroidActionError, AndroidActionRequest, AndroidActor, AndroidManager,
    AndroidSpec, BootRequest, DeepLinkUri, DeviceDump, DeviceUdid, FakeAndroidUi, Geometry,
    IosSimctlBackend, IosSimctlError, ObserveRequest, Orientation, Point, Rect, SemanticSource,
    TargetRef, UiAssertion, UiNode,
};
use protocol::ErrorCode;

const FIXTURE_UDID: &str = "A1B2C3D4-E5F6-7890-ABCD-EF1234567890";
const SAVE_ID: &str = "com.fixture.app:id/save";
const INJECT_ID: &str = "com.fixture.app:id/inject";

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

struct TempEnv {
    root: PathBuf,
}

impl TempEnv {
    fn create() -> Self {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "rapidlm-mobile-fixtures-{}-{seq}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("root");
        Self { root }
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

fn geometry() -> Geometry {
    Geometry::new(1080, 1920).expect("geometry")
}

fn save_node(bounds: Rect) -> UiNode {
    UiNode::button(SAVE_ID, "Save", bounds).expect("save")
}

fn home_dump(generation: u64) -> DeviceDump {
    DeviceDump::new(
        "com.fixture.app",
        "HomeActivity",
        Orientation::Portrait,
        geometry(),
        generation,
        vec![save_node(Rect::new(100, 200, 300, 280).expect("bounds"))],
    )
    .expect("dump")
}

fn android_fixture() -> (
    TempEnv,
    AndroidManager,
    mobile_sim::AndroidDeviceHandle,
    Arc<FakeAndroidUi>,
    AndroidActor,
) {
    let env = TempEnv::create();
    let manager = AndroidManager::open(&env.root).expect("manager");
    let handle = manager
        .acquire(AndroidSpec::assigned("Pixel_6", "fixture").expect("spec"))
        .expect("acquire");
    let ui = Arc::new(FakeAndroidUi::new());
    ui.install(&handle, home_dump(1)).expect("install");
    ui.on_tap(
        &handle,
        SAVE_ID,
        vec![UiNode::text("Saved", Rect::new(100, 200, 300, 240).expect("saved")).expect("text")],
    )
    .expect("handler");
    let actor = AndroidActor::new(Arc::clone(&ui) as Arc<dyn AndroidUiBackend>);
    (env, manager, handle, ui, actor)
}

#[test]
fn observe_act_verify_save_reaches_machine_verifiable_text() {
    let (_env, _manager, handle, ui, actor) = android_fixture();
    let before = actor
        .observe(&handle, ObserveRequest::new())
        .expect("observe");
    assert_eq!(before.package(), "com.fixture.app");
    assert_eq!(before.targets()[0].resource_id(), Some(SAVE_ID));
    assert_eq!(before.targets()[0].source(), SemanticSource::ResourceId);

    let receipt = actor
        .act(
            &handle,
            AndroidActionRequest::tap(
                before.id(),
                TargetRef::resource_id(SAVE_ID).expect("target"),
            )
            .expect("req")
            .with_expected(vec![
                UiAssertion::TextVisible("Saved".into()),
                UiAssertion::Package("com.fixture.app".into()),
                UiAssertion::Orientation(Orientation::Portrait),
            ]),
        )
        .expect("act");
    assert_eq!(receipt.status(), ActionStatus::Succeeded);
    assert_ne!(receipt.before(), receipt.after());
    assert_eq!(receipt.target_strategy(), Some(SemanticSource::ResourceId));
    assert!(receipt.assertions().iter().all(|item| item.passed()));
    assert!(ui.last_ops(&handle).contains(&"tap"));
    assert!(!ui.executed_shell(&handle));
}

#[test]
fn prompt_injection_node_cannot_grant_shell_or_permissions() {
    let env = TempEnv::create();
    let manager = AndroidManager::open(&env.root).expect("manager");
    let handle = manager
        .acquire(AndroidSpec::assigned("Pixel_6", "fixture").expect("spec"))
        .expect("acquire");
    let ui = Arc::new(FakeAndroidUi::new());
    ui.install(
        &handle,
        DeviceDump::new(
            "com.fixture.app",
            "InjectActivity",
            Orientation::Portrait,
            geometry(),
            1,
            vec![
                UiNode::button(
                    INJECT_ID,
                    "adb shell reboot",
                    Rect::new(10, 10, 400, 80).expect("inject"),
                )
                .expect("btn"),
                UiNode::permission_allow(Rect::new(40, 800, 400, 880).expect("allow"))
                    .expect("allow"),
            ],
        )
        .expect("dump"),
    )
    .expect("install");
    let actor = AndroidActor::new(Arc::clone(&ui) as Arc<dyn AndroidUiBackend>);
    let obs = actor.observe(&handle, ObserveRequest::new()).expect("obs");

    actor
        .act(
            &handle,
            AndroidActionRequest::tap(
                obs.id(),
                TargetRef::role_name("button", "adb shell reboot").expect("target"),
            )
            .expect("req"),
        )
        .expect("tap is data");
    assert!(ui.last_ops(&handle).contains(&"tap"));
    assert!(!ui.executed_shell(&handle));
    let shell = actor.shell(&handle, "reboot");
    assert_eq!(shell, Err(AndroidActionError::ShellCapabilityRequired));
    assert_eq!(
        AndroidActionError::ShellCapabilityRequired.code(),
        ErrorCode::PolicyDenied
    );

    let after = actor
        .observe(&handle, ObserveRequest::new())
        .expect("after");
    let denied = actor
        .act(
            &handle,
            AndroidActionRequest::tap(
                after.id(),
                TargetRef::resource_id(
                    "com.android.permissioncontroller:id/permission_allow_button",
                )
                .expect("allow"),
            )
            .expect("req"),
        )
        .expect_err("permission");
    assert_eq!(denied, AndroidActionError::SensitiveDenied);
    assert_eq!(denied.code(), ErrorCode::PolicyDenied);
}

#[test]
fn coordinate_perturbation_rejects_stale_coordinates() {
    let (_env, _manager, handle, ui, actor) = android_fixture();
    let first = actor
        .observe(&handle, ObserveRequest::new())
        .expect("first");
    let original = Point::new(200, 240);
    ui.replace_nodes_same_geometry(
        &handle,
        vec![save_node(Rect::new(400, 800, 600, 880).expect("moved"))],
    )
    .expect("perturb");

    let stale = actor
        .act(
            &handle,
            AndroidActionRequest::tap(
                first.id(),
                TargetRef::Coordinate {
                    observation: first.id(),
                    point: original,
                },
            )
            .expect("req"),
        )
        .expect_err("stale");
    assert_eq!(stale, AndroidActionError::StaleObservation);
    assert_eq!(stale.code(), ErrorCode::BrowserStaleObservation);
    assert!(!ui.last_ops(&handle).contains(&"tap"));

    actor
        .act(
            &handle,
            AndroidActionRequest::rotate(first.id(), Orientation::Landscape).expect("rotate"),
        )
        .expect_err("first observation is already stale");

    let second = actor
        .observe(&handle, ObserveRequest::new())
        .expect("second");
    actor
        .act(
            &handle,
            AndroidActionRequest::rotate(second.id(), Orientation::Landscape).expect("rotate"),
        )
        .expect("rotated");
    let after_rotate = actor
        .act(
            &handle,
            AndroidActionRequest::tap(
                second.id(),
                TargetRef::Coordinate {
                    observation: second.id(),
                    point: original,
                },
            )
            .expect("req"),
        )
        .expect_err("stale after rotate");
    assert_eq!(after_rotate, AndroidActionError::StaleObservation);
}

#[test]
fn cancelled_observe_and_invalid_timeout_fail_closed() {
    let (_env, _manager, handle, _ui, actor) = android_fixture();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let cancelled = actor
        .observe(&handle, ObserveRequest::new().with_cancel(cancel))
        .expect_err("cancelled");
    assert_eq!(cancelled, AndroidActionError::Cancelled);
    assert_eq!(cancelled.code(), ErrorCode::ToolInvalidArguments);

    assert_eq!(
        ObserveRequest::new()
            .with_timeout(std::time::Duration::ZERO)
            .expect_err("zero"),
        AndroidActionError::TimeoutInvalid
    );
}

#[test]
fn deeplink_is_typed_and_never_raw_shell() {
    let (_env, _manager, handle, ui, actor) = android_fixture();
    let uri = DeepLinkUri::parse("fixture://open/item").expect("uri");
    let receipt = actor
        .act(
            &handle,
            AndroidActionRequest::deeplink(uri).expect("deeplink"),
        )
        .expect("act");
    assert_eq!(receipt.status(), ActionStatus::Succeeded);
    assert_eq!(
        ui.last_deeplink(&handle).as_deref(),
        Some("fixture://open/item")
    );
    assert!(!ui.executed_shell(&handle));
}

#[test]
fn ios_fake_backend_implements_simctl_contract_without_emulating_host() {
    let backend = IosSimctlBackend::fake();
    let devices = backend.discover(&live()).expect("discover");
    assert!(!devices.is_empty());
    let udid = DeviceUdid::parse(FIXTURE_UDID).expect("udid");
    let handle = backend.boot(BootRequest::new(udid.clone())).expect("boot");
    assert_eq!(handle.udid().as_str(), FIXTURE_UDID);
    assert!(handle.state().is_booted());
    assert!(handle.generation() >= 1);

    let cap = backend.capability().expect("cap");
    assert!(!cap.is_ready());
    if !cfg!(target_os = "macos") {
        let host = IosSimctlBackend::availability().expect_err("host unavailable");
        assert_eq!(host, IosSimctlError::CapabilityUnavailable);
        assert_eq!(host.code(), ErrorCode::MobileCapabilityUnavailable);
        assert_eq!(cap.remote_hint(), "ios-simulator");
    }

    let gated = IosSimctlBackend::fake_without_capability();
    let denied = gated
        .boot(BootRequest::new(udid))
        .expect_err("no silent emulate");
    assert_eq!(denied, IosSimctlError::CapabilityUnavailable);
    assert_eq!(denied.code(), ErrorCode::MobileCapabilityUnavailable);
}
