//! The live browser driver against a real headless Chromium.
//!
//! Every GitHub-hosted runner (Ubuntu, macOS, Windows) ships Google Chrome,
//! as does any developer machine this product targets, so this runs for
//! real rather than against the in-process stand-in: a page is served from
//! an in-test HTTP listener, observed through `BrowserObserver`, driven
//! through `BrowserActor` (typing, clicking, keys, scrolling, navigation),
//! verified through `verify`, and its cookies, storage, screenshot and
//! trace go through the same `BrowserManager` paths production uses.
//!
//! A host with no Chromium-family browser fails the test with the
//! discovery error — set `RAPIDLM_BROWSER_PATH`, or
//! `RAPIDLM_SKIP_LIVE_BROWSER=1` to opt out loudly.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use capability_broker::{
    ActionRequest, ApprovalChoice, ApprovalResolution, ApprovalScopeId, BrowserScope,
    CancellationToken, CanonicalAction, Capability, CapabilityLease, LeaseIssuer, Origin,
    PolicyDocument, PolicySource, PolicyStack, PrincipalRef, ResourceDescriptor, evaluate, issue,
    request_approval,
};
use computer_use::browser::{
    ActionStatus, BrowserActor, BrowserCookie, BrowserDiscoveryError, BrowserEngine,
    BrowserManager, BrowserObserver, BrowserSpec, ChromiumCdpBackend, KeyCode, ObserveRequest,
    PageActor, PageCapture, PlaywrightBackend, SecretAwareString, TargetSelector, UiAction,
    VerificationClause, VerificationPredicate, VerificationStatus, act, observe, verify,
};
use event_ledger::artifact_store::ArtifactStore;
use protocol::SessionId;

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

const PAGE: &str = r#"<!doctype html>
<html><head><title>Greeter fixture</title></head>
<body>
  <h1>Greeter</h1>
  <nav><a href="/second" data-testid="second-link">Second page</a></nav>
  <form onsubmit="return false">
    <label for="who">Your name</label>
    <input id="who" data-testid="who" type="text" placeholder="name">
    <label for="pw">Password</label>
    <input id="pw" type="password">
    <button type="button" data-testid="greet"
      onclick="document.getElementById('out').textContent = 'Hello, ' + document.getElementById('who').value + '!'">Greet</button>
  </form>
  <p id="out" data-testid="out">nobody yet</p>
  <div style="height: 3000px"></div>
  <p data-testid="bottom">bottom marker</p>
</body></html>
"#;

const SECOND: &str = r#"<!doctype html>
<html><head><title>Second fixture</title></head>
<body><h1>Second</h1><a href="/" data-testid="home-link">Home</a></body></html>
"#;

/// A one-thread HTTP/1.1 server for the fixture pages, serving until dropped.
struct FixtureSite {
    origin: String,
    _thread: std::thread::JoinHandle<()>,
}

impl FixtureSite {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        listener.set_nonblocking(false).expect("blocking");
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                while !request.ends_with(b"\r\n\r\n") && request.len() < 64 * 1024 {
                    match stream.read(&mut byte) {
                        Ok(1) => request.push(byte[0]),
                        _ => break,
                    }
                }
                let text = String::from_utf8_lossy(&request);
                let path = text
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_owned();
                let (status, body) = match path.as_str() {
                    "/" | "/index.html" => ("200 OK", PAGE),
                    "/second" => ("200 OK", SECOND),
                    _ => ("404 Not Found", "<html><body>missing</body></html>"),
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
Content-Length: {}\r\nSet-Cookie: fixture=served; Path=/\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self {
            origin: format!("http://127.0.0.1:{port}"),
            _thread: thread,
        }
    }
}

struct TempEnv {
    root: PathBuf,
    artifacts: ArtifactStore,
}

impl TempEnv {
    fn create() -> Self {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("rapidlm-cu-live-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let artifacts = ArtifactStore::create(root.join("artifacts")).expect("artifact store");
        Self { root, artifacts }
    }
}

impl Drop for TempEnv {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn live() -> CancellationToken {
    CancellationToken::new()
}

fn lease_for(origin: &str) -> CapabilityLease {
    let capability = Capability::BrowserNavigate;
    let resource = ResourceDescriptor::Browser(BrowserScope::navigate(
        Origin::parse(origin).expect("origin"),
    ));
    let source = PolicySource::user("live-chromium").expect("source");
    let policies = PolicyStack::new([PolicyDocument::parse_toml(
        &format!(
            "[[rules]]\nid = \"allow\"\neffect = \"ask\"\nsubjects = [\"*\"]\ncapability = \"{}\"\n",
            capability.as_str()
        ),
        source,
        &live(),
    )
    .expect("policy")])
    .expect("stack");
    let actual = CanonicalAction::Resource {
        capability,
        resource: resource.clone(),
    };
    let request = ActionRequest::new(
        PrincipalRef::parse("agent").expect("principal"),
        SessionId::new(),
        capability,
        resource,
        actual,
        "live-chromium",
    )
    .expect("request");
    let now = Instant::now();
    let decision = evaluate(&policies, &request, &live()).expect("evaluate");
    let approval = request_approval(&request, &decision, now, &live()).expect("approval");
    let approved = match approval
        .resolve(
            ApprovalChoice::Approve(ApprovalScopeId::Once),
            &request,
            now,
            &live(),
        )
        .expect("resolve")
    {
        ApprovalResolution::Approved(approved) => approved,
        ApprovalResolution::Denied => panic!("expected approved"),
    };
    let issuer = LeaseIssuer::from_key([0x42; 32]).expect("issuer");
    issue(&issuer, &approved, &policies, now, &live()).expect("issue")
}

fn backend() -> Option<Arc<ChromiumCdpBackend>> {
    match ChromiumCdpBackend::discover() {
        Ok(backend) => Some(Arc::new(backend)),
        Err(err) => {
            if std::env::var_os("RAPIDLM_SKIP_LIVE_BROWSER").is_some() {
                eprintln!("SKIPPED live browser test: {err} (RAPIDLM_SKIP_LIVE_BROWSER is set)");
                return None;
            }
            panic!("{err}; install a Chromium-family browser or set RAPIDLM_SKIP_LIVE_BROWSER=1");
        }
    }
}

fn target_by_test_id(
    observation: &computer_use::browser::Observation,
    test_id: &str,
) -> TargetSelector {
    let found = observation
        .targets()
        .iter()
        .find(|item| item.test_id() == Some(test_id))
        .unwrap_or_else(|| {
            panic!(
                "no target with test id {test_id:?} among {:?}",
                observation
                    .targets()
                    .iter()
                    .map(|t| (
                        t.role().map(str::to_owned),
                        t.name().map(str::to_owned),
                        t.test_id().map(str::to_owned)
                    ))
                    .collect::<Vec<_>>()
            )
        });
    TargetSelector::from_target(found).expect("selector")
}

#[test]
fn a_real_chromium_is_observed_driven_and_verified_end_to_end() {
    let Some(backend) = backend() else { return };
    let site = FixtureSite::start();
    let env = TempEnv::create();
    let manager = BrowserManager::open_with_backend(
        &env.root,
        env.artifacts.clone(),
        Box::new(Arc::clone(&backend)),
    )
    .expect("manager");
    let session = manager
        .create(BrowserSpec::ephemeral(BrowserEngine::Chromium).with_trace(true))
        .expect("a real browser launches");
    let observer = BrowserObserver::new(
        env.artifacts.clone(),
        Arc::clone(&backend) as Arc<dyn PageCapture>,
    );
    let actor = BrowserActor::new(observer, Arc::clone(&backend) as Arc<dyn PageActor>);
    let lease = lease_for(&site.origin);

    // Navigate through the production action path, then observe.
    let blank = observe(&session, actor.observer()).expect("observe about:blank");
    assert_eq!(blank.url(), "about:blank");
    let home = format!("{}/", site.origin);
    let navigated = act(
        &session,
        blank.id(),
        UiAction::navigate(&home).expect("nav"),
        &lease,
        &actor,
    )
    .expect("navigate");
    assert_eq!(navigated.status(), ActionStatus::Executed);

    let first = observe(&session, actor.observer()).expect("observe page");
    assert_eq!(first.url(), home);
    assert_eq!(first.title(), "Greeter fixture");
    let who = first
        .targets()
        .iter()
        .find(|t| t.test_id() == Some("who"))
        .expect("the labelled input is a target");
    assert_eq!(who.role(), Some("textbox"));
    assert_eq!(who.name(), Some("Your name"));
    assert!(who.is_interactive());
    let greet = first
        .targets()
        .iter()
        .find(|t| t.test_id() == Some("greet"))
        .expect("button");
    assert_eq!(greet.role(), Some("button"));
    assert_eq!(greet.name(), Some("Greet"));
    // The password field is listed as a locator but its name is withheld
    // (sensitive), and it can only be addressed by path.
    let password = first
        .targets()
        .iter()
        .find(|t| t.is_sensitive())
        .expect("password field is listed as a locator");
    assert_eq!(password.role(), Some("textbox"));
    assert_eq!(password.name(), None);
    assert!(password.stable_ref().starts_with("node:"));

    // Type through real input events, click through real mouse events.
    let typed = act(
        &session,
        first.id(),
        UiAction::type_text(
            target_by_test_id(&first, "who"),
            SecretAwareString::literal("Ada").expect("literal"),
        ),
        &lease,
        &actor,
    )
    .expect("type");
    assert_eq!(typed.status(), ActionStatus::Executed);
    let clicked = act(
        &session,
        first.id(),
        UiAction::click(target_by_test_id(&first, "greet")),
        &lease,
        &actor,
    )
    .expect("click");
    let verified = verify(
        &session,
        VerificationPredicate::new(
            clicked,
            vec![
                VerificationClause::url_equals(&home).expect("url"),
                VerificationClause::dom_state("paragraph", "Hello, Ada!").expect("text"),
                VerificationClause::accessible_node("heading", "Greeter").expect("node"),
                VerificationClause::dom_test_id("out").expect("dom"),
            ],
        )
        .expect("predicate"),
        actor.observer(),
    )
    .expect("verify");
    assert_eq!(
        verified.status(),
        VerificationStatus::Passed,
        "{:?}",
        verified
            .after_observation()
            .targets()
            .iter()
            .map(|t| (t.role().map(str::to_owned), t.name().map(str::to_owned)))
            .collect::<Vec<_>>()
    );

    // A key press reaches the focused field: Backspace shortens the name.
    let after = verified.after_observation().clone();
    let keyed = act(
        &session,
        after.id(),
        UiAction::key_on(
            target_by_test_id(&after, "who"),
            KeyCode::parse("Backspace").expect("key"),
        ),
        &lease,
        &actor,
    )
    .expect("key");
    assert_eq!(keyed.status(), ActionStatus::Executed);
    let reclicked = act(
        &session,
        after.id(),
        UiAction::click(target_by_test_id(&after, "greet")),
        &lease,
        &actor,
    )
    .expect("click again");
    let shortened = verify(
        &session,
        VerificationPredicate::new(
            reclicked,
            vec![VerificationClause::dom_state("paragraph", "Hello, Ad!").expect("text")],
        )
        .expect("predicate"),
        actor.observer(),
    )
    .expect("verify");
    assert_eq!(shortened.status(), VerificationStatus::Passed);

    // Scrolling is a real wheel event, dispatched and acknowledged by the
    // browser (the offset it produces is not part of an observation).
    let latest = shortened.after_observation().clone();
    let scrolled = act(
        &session,
        latest.id(),
        UiAction::scroll(0, 1200).expect("scroll"),
        &lease,
        &actor,
    )
    .expect("scroll");
    assert_eq!(scrolled.status(), ActionStatus::Executed);

    // A screenshot is a bounded artifact, not bytes in the model view — and
    // with a password field on this page it is the redacted marker, never
    // the pixels (T-CU-03).
    let shot = actor
        .observer()
        .observe_with(&session, ObserveRequest::new().with_screenshot(true))
        .expect("observe with screenshot");
    let meta = shot.screenshot().expect("screenshot metadata");
    assert!(meta.width() > 0 && meta.height() > 0);
    assert!(meta.masked(), "a page with a password field is masked");
    let bytes = env
        .artifacts
        .get(
            &meta.artifact().id,
            &event_ledger::artifact_store::CancellationToken::new(),
        )
        .expect("screenshot artifact");
    assert!(
        !bytes.starts_with(b"\x89PNG"),
        "the pixels of a sensitive page must not be persisted"
    );

    // Link navigation through a click changes the document *by the page's
    // own doing*: the click is acknowledged before the navigation commits,
    // and nothing re-observes in between.
    let link = observe(&session, actor.observer()).expect("observe");
    let followed = act(
        &session,
        link.id(),
        UiAction::click(target_by_test_id(&link, "second-link")),
        &lease,
        &actor,
    )
    .expect("follow link");
    std::thread::sleep(std::time::Duration::from_millis(750));
    // `link` is still the latest observation the ledger knows, so this is
    // the document-generation check and nothing else: the generation is
    // the browser's loader identity, which moved when the page navigated
    // itself (T-CU-02). A driver that only counted its own navigations
    // would click "Greet" on a page that no longer has it.
    let acted_on_old_document = act(
        &session,
        link.id(),
        UiAction::click(target_by_test_id(&link, "greet")),
        &lease,
        &actor,
    )
    .expect_err("an observation of the previous document is stale");
    assert!(matches!(
        acted_on_old_document,
        computer_use::browser::ActionError::StaleObservation
    ));
    let second = verify(
        &session,
        VerificationPredicate::new(
            followed,
            vec![
                VerificationClause::url_equals(&format!("{}/second", site.origin)).expect("url"),
                VerificationClause::accessible_node("heading", "Second").expect("node"),
            ],
        )
        .expect("predicate")
        .with_wait(),
        actor.observer(),
    )
    .expect("verify second");
    assert_eq!(second.status(), VerificationStatus::Passed);
    // No sensitive field here: the screenshot is the real PNG.
    let shot = actor
        .observer()
        .observe_with(&session, ObserveRequest::new().with_screenshot(true))
        .expect("observe second page with screenshot");
    let meta = shot.screenshot().expect("screenshot metadata");
    assert!(!meta.masked());
    assert_eq!((meta.width(), meta.height()), (1280, 720));
    let bytes = env
        .artifacts
        .get(
            &meta.artifact().id,
            &event_ledger::artifact_store::CancellationToken::new(),
        )
        .expect("screenshot artifact");
    assert!(bytes.starts_with(b"\x89PNG"), "a real PNG was persisted");
    // Cookies and storage go through the browser, not a map.
    let cookies = session.cookies(&live()).expect("cookies");
    assert!(
        cookies
            .iter()
            .any(|c| c.name() == "fixture" && c.value() == "served"),
        "the server's Set-Cookie landed in the context: {cookies:?}"
    );
    session
        .put_cookie(
            &BrowserCookie::new(&site.origin, "agent", "placed").expect("cookie"),
            &live(),
        )
        .expect("put cookie");
    let cookies = session.cookies(&live()).expect("cookies");
    assert!(
        cookies
            .iter()
            .any(|c| c.name() == "agent" && c.value() == "placed")
    );
    session
        .storage_put(&site.origin, "rapidlm-key", "rapidlm-value", &live())
        .expect("storage put");
    assert_eq!(
        session
            .storage_get(&site.origin, "rapidlm-key", &live())
            .expect("storage get")
            .as_deref(),
        Some("rapidlm-value")
    );

    // Close: the requested trace is preserved as an artifact.
    let cleanup = session.close(&live()).expect("close");
    let trace = cleanup.trace().expect("a trace was requested");
    let body = env
        .artifacts
        .get(
            &trace.id,
            &event_ledger::artifact_store::CancellationToken::new(),
        )
        .expect("trace artifact");
    let text = String::from_utf8_lossy(&body);
    assert!(text.starts_with("rapidlm.playwright.trace.v1\n"));
    assert!(text.contains("\"driver\":\"chromium-cdp\""));
    assert!(text.contains("\"event\":\"click\""));
    assert!(text.contains("\"event\":\"navigate\""));
}

#[test]
fn a_second_engine_request_is_a_typed_unavailable_not_a_chromium_in_disguise() {
    let Some(backend) = backend() else { return };
    let cancel = live();
    assert_eq!(
        backend
            .launch_or_reuse_browser(BrowserEngine::Firefox, &cancel)
            .expect_err("firefox"),
        computer_use::browser::BrowserSessionError::Unavailable
    );
    let env = TempEnv::create();
    let manager = BrowserManager::open_with_backend(
        &env.root,
        env.artifacts.clone(),
        Box::new(Arc::clone(&backend)),
    )
    .expect("manager");
    assert!(matches!(
        manager.create(BrowserSpec::ephemeral(BrowserEngine::Webkit)),
        Err(computer_use::browser::BrowserSessionError::Unavailable)
    ));
}

#[test]
fn discovery_reports_a_typed_reason_when_the_override_is_wrong() {
    // Pure: does not touch the process environment. The override contract
    // is "absolute path of an existing file".
    assert_eq!(
        BrowserDiscoveryError::OverrideInvalid.to_string(),
        "RAPIDLM_BROWSER_PATH must be the absolute path of an existing browser executable"
    );
}
