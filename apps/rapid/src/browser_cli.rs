//! `rapid browser <url> [--step …]…`: one-shot live browser automation.
//!
//! The user-reachable surface of the live Chromium driver
//! (`computer_use::browser::ChromiumCdpBackend`). One invocation launches a
//! headless browser the host already has, opens an isolated context,
//! navigates, runs the requested steps through the same
//! `BrowserObserver`/`BrowserActor`/`verify` path the agent runtime uses,
//! prints the resulting observation, and closes the context. Screenshots
//! and the session trace are persisted as artifacts under the project's
//! `.rapidlm/browser/` directory and named in the output; nothing about the
//! page is trusted beyond what the observation model exposes (locators,
//! roles, names — never field values).
//!
//! Steps: `click:<target>`, `type:<target>=<text>`, `key:<Key>`,
//! `key:<target>=<Key>`, `scroll:<dy>`, `navigate:<url>`,
//! `expect-url:<url>`, `expect-text:<text>`, `expect-node:<role>|<name>`,
//! `expect-state:<role>|<name>`, `expect-testid:<id>`.
//! A `<target>` is a `data-testid` value, a `role|name` pair, or `node:<n>`
//! from the printed observation. Every expectation that fails makes the
//! command exit 1; a browser that cannot be found or driven exits 2 with
//! the typed reason.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use capability_broker::{
    ActionRequest, ApprovalChoice, ApprovalResolution, ApprovalScopeId, BrowserScope,
    CancellationToken, CanonicalAction, Capability, CapabilityLease, LeaseIssuer, Origin,
    PolicyDocument, PolicySource, PolicyStack, PrincipalRef, ResourceDescriptor, evaluate, issue,
    request_approval,
};
use computer_use::browser::{
    BrowserActor, BrowserEngine, BrowserManager, BrowserObserver, BrowserSession, BrowserSpec,
    ChromiumCdpBackend, KeyCode, Observation, ObserveRequest, PageActor, PageCapture,
    SecretAwareString, TargetSelector, UiAction, VerificationClause, VerificationPredicate,
    VerificationStatus, act, observe, verify,
};
use event_ledger::artifact_store::ArtifactStore;
use protocol::SessionId;

use crate::p9_commands::P9CommandError;

pub const BROWSER_USAGE: &str = "\
usage: rapid browser <url> [--step <spec>]... [--screenshot] [--json] [--root <dir>]

Drive a real headless Chromium (Google Chrome, Chromium or Microsoft Edge on
this machine; RAPIDLM_BROWSER_PATH overrides discovery) through the same
observe/act/verify path the agent runtime uses, then print what the page
looks like to the model: URL, title and the semantic targets (locators only;
field values are never read). Screenshots and the session trace are stored
as artifacts under .rapidlm/browser/ and named in the output.

steps (run in order, each followed by a fresh observation):
  click:<target>            click the target
  type:<target>=<text>      focus the target and type text (clears first)
  key:<Key>                 press a key (Enter, Tab, Escape, ArrowDown, a, 1, …)
  key:<target>=<Key>        click the target, then press the key
  scroll:<dy>               wheel-scroll the page by dy CSS pixels
  navigate:<url>            navigate the same context to another http(s) URL
  expect-url:<url>          verify the current URL equals <url>
  expect-text:<text>        verify the accessibility tree exposes <text> as a name
  expect-node:<role>|<name> verify an accessible node with that role and name exists
  expect-state:<role>|<name>
                            verify a DOM node with that role and name exists
  expect-testid:<id>        verify a node with that data-testid exists

a <target> is a data-testid value, `role|name`, or `node:<index>` from the
printed observation. Exit 0 when every expectation held, 1 when one failed,
2 when no browser could be found or driven.
";

const MAX_STEPS: usize = 64;

/// Parsed `--step` specification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Click(String),
    Type(String, String),
    Key(Option<String>, String),
    Scroll(i32),
    Navigate(String),
    ExpectUrl(String),
    ExpectText(String),
    ExpectNode(String, String),
    ExpectState(String, String),
    ExpectTestId(String),
}

impl Step {
    /// Parse one `--step` value. Display of an error never echoes secrets;
    /// the spec itself is the user's own argv.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let (kind, rest) = spec
            .split_once(':')
            .ok_or_else(|| format!("step {spec:?}: expected <kind>:<value>"))?;
        if rest.is_empty() {
            return Err(format!("step {spec:?}: missing value"));
        }
        match kind {
            "click" => Ok(Self::Click(rest.to_owned())),
            "type" => {
                let (target, text) = rest
                    .split_once('=')
                    .ok_or_else(|| format!("step {spec:?}: expected type:<target>=<text>"))?;
                if target.is_empty() {
                    return Err(format!("step {spec:?}: missing target"));
                }
                Ok(Self::Type(target.to_owned(), text.to_owned()))
            }
            "key" => match rest.split_once('=') {
                Some((target, key)) if !target.is_empty() && !key.is_empty() => {
                    Ok(Self::Key(Some(target.to_owned()), key.to_owned()))
                }
                Some(_) => Err(format!("step {spec:?}: expected key:<target>=<Key>")),
                None => Ok(Self::Key(None, rest.to_owned())),
            },
            "scroll" => rest
                .parse::<i32>()
                .map(Self::Scroll)
                .map_err(|_| format!("step {spec:?}: scroll takes an integer")),
            "navigate" => Ok(Self::Navigate(rest.to_owned())),
            "expect-url" => Ok(Self::ExpectUrl(rest.to_owned())),
            "expect-text" => Ok(Self::ExpectText(rest.to_owned())),
            "expect-node" | "expect-state" => {
                let (role, name) = rest
                    .split_once('|')
                    .ok_or_else(|| format!("step {spec:?}: expected {kind}:<role>|<name>"))?;
                if kind == "expect-node" {
                    Ok(Self::ExpectNode(role.to_owned(), name.to_owned()))
                } else {
                    Ok(Self::ExpectState(role.to_owned(), name.to_owned()))
                }
            }
            "expect-testid" => Ok(Self::ExpectTestId(rest.to_owned())),
            other => Err(format!("step {spec:?}: unknown kind {other:?}")),
        }
    }
}

/// Parsed command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserArgs {
    pub url: String,
    pub steps: Vec<Step>,
    pub screenshot: bool,
    pub json: bool,
    pub root: Option<PathBuf>,
}

impl BrowserArgs {
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut url = None;
        let mut steps = Vec::new();
        let mut screenshot = false;
        let mut json = false;
        let mut root = None;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--step" => {
                    let spec = args.get(i + 1).ok_or("--step takes a value")?;
                    if steps.len() >= MAX_STEPS {
                        return Err(format!("at most {MAX_STEPS} steps"));
                    }
                    steps.push(Step::parse(spec)?);
                    i += 2;
                }
                "--screenshot" => {
                    screenshot = true;
                    i += 1;
                }
                "--json" => {
                    json = true;
                    i += 1;
                }
                "--root" => {
                    let dir = args.get(i + 1).ok_or("--root takes a directory")?;
                    root = Some(PathBuf::from(dir));
                    i += 2;
                }
                flag if flag.starts_with('-') => return Err(format!("unknown flag {flag:?}")),
                operand => {
                    if url.is_some() {
                        return Err(format!("unexpected operand {operand:?}"));
                    }
                    url = Some(operand.to_owned());
                    i += 1;
                }
            }
        }
        let url = url.ok_or("a URL is required")?;
        UiAction::navigate(&url).map_err(|err| format!("url {url:?}: {err}"))?;
        Ok(Self {
            url,
            steps,
            screenshot,
            json,
            root,
        })
    }
}

/// `rapid browser` entry: argv in, exit code out.
pub fn run_browser(args: &[String]) -> Result<i32, P9CommandError> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{BROWSER_USAGE}");
        return Ok(0);
    }
    let parsed = match BrowserArgs::parse(args) {
        Ok(parsed) => parsed,
        Err(reason) => {
            eprintln!("rapid browser: {reason}");
            eprint!("{BROWSER_USAGE}");
            return Err(P9CommandError::Usage);
        }
    };
    let root = match &parsed.root {
        Some(root) => root.clone(),
        None => {
            let cwd = std::env::current_dir().map_err(P9CommandError::Io)?;
            crate::interactive::project_marker_dir_in(&cwd).join("browser")
        }
    };
    let mut out = String::new();
    let code = match drive(&parsed, &root, &mut out) {
        Ok(all_held) => {
            if all_held {
                0
            } else {
                1
            }
        }
        Err(reason) => {
            print!("{out}");
            eprintln!("rapid browser: {reason}");
            return Ok(2);
        }
    };
    print!("{out}");
    Ok(code)
}

/// The whole run against a live browser. Returns whether every expectation
/// held; `Err` is a browser/driver failure the caller reports as exit 2.
fn drive(args: &BrowserArgs, root: &Path, out: &mut String) -> Result<bool, String> {
    let backend = Arc::new(ChromiumCdpBackend::discover().map_err(|err| err.to_string())?);
    std::fs::create_dir_all(root).map_err(|err| format!("browser root: {err}"))?;
    let artifacts = ArtifactStore::create(root.join("artifacts"))
        .map_err(|err| format!("artifact store: {err:?}"))?;
    let manager =
        BrowserManager::open_with_backend(root, artifacts.clone(), Box::new(Arc::clone(&backend)))
            .map_err(|err| format!("browser manager: {err}"))?;
    let session = manager
        .create(BrowserSpec::ephemeral(BrowserEngine::Chromium).with_trace(true))
        .map_err(|err| {
            format!(
                "launch ({}): {err}",
                backend.launch().executable().display()
            )
        })?;
    let observer = BrowserObserver::new(
        artifacts.clone(),
        Arc::clone(&backend) as Arc<dyn PageCapture>,
    );
    let actor = BrowserActor::new(observer, Arc::clone(&backend) as Arc<dyn PageActor>);
    let cancel = CancellationToken::new();

    let result = drive_session(args, &session, &actor, out);
    // Close regardless of how the steps went: the trace and the browser's
    // resources must not depend on a successful run.
    let cleanup = session
        .close(&cancel)
        .map_err(|err| format!("close: {err}"))?;
    if let Some(trace) = cleanup.trace() {
        render_line(
            out,
            args.json,
            &format!("{{\"trace\":\"{}\",\"bytes\":{}}}", trace.id, trace.bytes),
            &format!("trace artifact: {} ({} bytes)", trace.id, trace.bytes),
        );
    }
    result
}

fn drive_session(
    args: &BrowserArgs,
    session: &BrowserSession,
    actor: &BrowserActor,
    out: &mut String,
) -> Result<bool, String> {
    let mut all_held = true;
    let mut lease = lease_for_url(&args.url)?;
    let blank = observe(session, actor.observer()).map_err(|err| format!("observe: {err}"))?;
    // The receipt of the most recent action: verification is anchored to
    // it, so an expectation states "after that action, this holds now".
    let mut last_receipt = act(
        session,
        blank.id(),
        UiAction::navigate(&args.url).map_err(|err| format!("url: {err}"))?,
        &lease,
        actor,
    )
    .map_err(|err| format!("navigate: {err}"))?;
    let mut current = observe_page(session, actor, args.screenshot)?;
    render_observation(out, args.json, "open", &current);

    for (index, step) in args.steps.iter().enumerate() {
        let label = format!("step {}", index + 1);
        let action = match step {
            Step::Click(target) => Some(UiAction::click(selector_for(&current, target)?)),
            Step::Type(target, text) => {
                let selector = selector_for(&current, target)?;
                let value = SecretAwareString::literal(text)
                    .map_err(|err| format!("{label} type: {err}"))?;
                Some(UiAction::type_text(selector, value))
            }
            Step::Key(target, key) => {
                let key = KeyCode::parse(key).map_err(|err| format!("{label} key: {err}"))?;
                Some(match target {
                    Some(target) => UiAction::key_on(selector_for(&current, target)?, key),
                    None => UiAction::key(key),
                })
            }
            Step::Scroll(dy) => {
                Some(UiAction::scroll(0, *dy).map_err(|err| format!("{label} scroll: {err}"))?)
            }
            Step::Navigate(url) => {
                lease = lease_for_url(url)?;
                Some(UiAction::navigate(url).map_err(|err| format!("{label} navigate: {err}"))?)
            }
            Step::ExpectUrl(_)
            | Step::ExpectText(_)
            | Step::ExpectNode(_, _)
            | Step::ExpectState(_, _)
            | Step::ExpectTestId(_) => None,
        };
        match action {
            Some(action) => {
                last_receipt = act(session, current.id(), action, &lease, actor)
                    .map_err(|err| format!("{label} {}: {err}", describe_step(step)))?;
            }
            None => {
                let clause = match step {
                    Step::ExpectUrl(url) => VerificationClause::url_equals(url),
                    Step::ExpectText(text) => VerificationClause::accessible_text(text),
                    Step::ExpectNode(role, name) => VerificationClause::accessible_node(role, name),
                    Step::ExpectState(role, name) => VerificationClause::dom_state(role, name),
                    Step::ExpectTestId(id) => VerificationClause::dom_test_id(id),
                    _ => unreachable!("every action step was matched above"),
                }
                .map_err(|err| format!("{label}: {err}"))?;
                let predicate = VerificationPredicate::new(last_receipt.clone(), vec![clause])
                    .map_err(|err| format!("{label}: {err}"))?;
                let verified = verify(session, predicate, actor.observer())
                    .map_err(|err| format!("{label}: {err}"))?;
                let held = verified.status() == VerificationStatus::Passed;
                all_held &= held;
                render_line(
                    out,
                    args.json,
                    &format!(
                        "{{\"step\":{},\"expectation\":{},\"held\":{held}}}",
                        index + 1,
                        json_string(&format!("{step:?}"))
                    ),
                    &format!(
                        "{label}: {} — {}",
                        describe_step(step),
                        if held { "held" } else { "FAILED" }
                    ),
                );
                current = verified.after_observation().clone();
                continue;
            }
        }
        current = observe_page(session, actor, args.screenshot)?;
        render_observation(
            out,
            args.json,
            &format!("after {}", describe_step(step)),
            &current,
        );
    }
    Ok(all_held)
}

fn observe_page(
    session: &BrowserSession,
    actor: &BrowserActor,
    screenshot: bool,
) -> Result<Observation, String> {
    actor
        .observer()
        .observe_with(session, ObserveRequest::new().with_screenshot(screenshot))
        .map_err(|err| format!("observe: {err}"))
}

/// A `data-testid`, `role|name` pair, or `node:<n>` into the observer's
/// stable-ref grammar.
fn selector_for(observation: &Observation, target: &str) -> Result<TargetSelector, String> {
    let stable_ref = if target.starts_with("node:") {
        target.to_owned()
    } else if let Some((role, name)) = target.split_once('|') {
        format!("role:{role}|name:{name}")
    } else if observation
        .targets()
        .iter()
        .any(|t| t.test_id() == Some(target))
    {
        format!("testid:{target}")
    } else {
        return Err(format!(
            "no target with data-testid {target:?} in the current observation; use role|name or node:<n>"
        ));
    };
    TargetSelector::parse(&stable_ref).map_err(|err| format!("target {target:?}: {err}"))
}

fn describe_step(step: &Step) -> String {
    match step {
        Step::Click(target) => format!("click {target}"),
        Step::Type(target, text) => format!("type {} bytes into {target}", text.len()),
        Step::Key(Some(target), key) => format!("key {key} on {target}"),
        Step::Key(None, key) => format!("key {key}"),
        Step::Scroll(dy) => format!("scroll {dy}"),
        Step::Navigate(url) => format!("navigate {url}"),
        Step::ExpectUrl(url) => format!("expect url {url}"),
        Step::ExpectText(text) => format!("expect text {text:?}"),
        Step::ExpectNode(role, name) => format!("expect node {role} {name:?}"),
        Step::ExpectState(role, name) => format!("expect state {role} {name:?}"),
        Step::ExpectTestId(id) => format!("expect testid {id}"),
    }
}

fn render_observation(out: &mut String, json: bool, phase: &str, observation: &Observation) {
    let view = observation.model_view();
    if json {
        let targets: Vec<String> = view
            .targets()
            .iter()
            .map(|t| {
                format!(
                    "{{\"index\":{},\"ref\":{},\"role\":{},\"name\":{},\"testId\":{},\"interactive\":{},\"sensitive\":{}}}",
                    t.index(),
                    json_string(t.stable_ref()),
                    json_opt(t.role()),
                    json_opt(t.name()),
                    json_opt(t.test_id()),
                    t.is_interactive(),
                    t.is_sensitive()
                )
            })
            .collect();
        let screenshot = view
            .screenshot()
            .map(|s| {
                format!(
                    "{{\"artifact\":\"{}\",\"width\":{},\"height\":{}}}",
                    s.artifact().id,
                    s.width(),
                    s.height()
                )
            })
            .unwrap_or_else(|| "null".to_owned());
        out.push_str(&format!(
            "{{\"phase\":{},\"url\":{},\"title\":{},\"generation\":{},\"targets\":[{}],\"screenshot\":{screenshot}}}\n",
            json_string(phase),
            json_string(view.url()),
            json_string(view.title()),
            view.generation(),
            targets.join(",")
        ));
        return;
    }
    out.push_str(&format!("== {phase}\n"));
    out.push_str(&format!("url: {}\ntitle: {}\n", view.url(), view.title()));
    for t in view.targets() {
        let mut line = format!(
            "  [{}] {} {}",
            t.index(),
            t.role().unwrap_or("-"),
            t.name().map(|n| format!("{n:?}")).unwrap_or_default()
        );
        if let Some(test_id) = t.test_id() {
            line.push_str(&format!(" testid={test_id}"));
        }
        if t.is_interactive() {
            line.push_str(" interactive");
        }
        if t.is_sensitive() {
            line.push_str(" sensitive");
        }
        line.push_str(&format!(" ref={}", t.stable_ref()));
        out.push_str(&line);
        out.push('\n');
    }
    if let Some(shot) = view.screenshot() {
        out.push_str(&format!(
            "screenshot artifact: {} ({}x{})\n",
            shot.artifact().id,
            shot.width(),
            shot.height()
        ));
    }
}

fn render_line(out: &mut String, json: bool, json_line: &str, text_line: &str) {
    if json {
        out.push_str(json_line);
    } else {
        out.push_str(text_line);
    }
    out.push('\n');
}

fn json_string(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_owned())
}

fn json_opt(text: Option<&str>) -> String {
    text.map(json_string).unwrap_or_else(|| "null".to_owned())
}

/// A single-use `browser.navigate` lease for the URL's origin, minted the
/// same way `shell_exec`'s sandbox path mints its `proc.exec` lease: a
/// fixed always-`ask` rule the invoking human resolves by running the
/// command. The user typed the URL; that is the approval.
fn lease_for_url(url: &str) -> Result<CapabilityLease, String> {
    let origin_text = origin_of(url)?;
    let origin = Origin::parse(&origin_text).map_err(|err| format!("origin: {err:?}"))?;
    let capability = Capability::BrowserNavigate;
    let resource = ResourceDescriptor::Browser(BrowserScope::navigate(origin));
    let cancel = CancellationToken::new();
    let policy = format!(
        "[[rules]]\nid = \"ask\"\neffect = \"ask\"\nsubjects = [\"*\"]\ncapability = \"{}\"\n",
        capability.as_str()
    );
    let source = PolicySource::user("rapidlm-browser-cli").map_err(|err| format!("{err:?}"))?;
    let document =
        PolicyDocument::parse_toml(&policy, source, &cancel).map_err(|err| format!("{err:?}"))?;
    let policies = PolicyStack::new([document]).map_err(|err| format!("{err:?}"))?;
    let actual = CanonicalAction::Resource {
        capability,
        resource: resource.clone(),
    };
    let principal = PrincipalRef::parse("human").map_err(|err| format!("{err:?}"))?;
    let request = ActionRequest::new(
        principal,
        SessionId::new(),
        capability,
        resource,
        actual,
        "rapid browser",
    )
    .map_err(|err| format!("{err:?}"))?;
    let now = Instant::now();
    let decision = evaluate(&policies, &request, &cancel).map_err(|err| format!("{err:?}"))?;
    let approval =
        request_approval(&request, &decision, now, &cancel).map_err(|err| format!("{err:?}"))?;
    let approved = match approval
        .resolve(
            ApprovalChoice::Approve(ApprovalScopeId::Once),
            &request,
            now,
            &cancel,
        )
        .map_err(|err| format!("{err:?}"))?
    {
        ApprovalResolution::Approved(approved) => approved,
        ApprovalResolution::Denied => return Err("browser navigation was denied".to_owned()),
    };
    let issuer = LeaseIssuer::ephemeral();
    issue(&issuer, &approved, &policies, now, &cancel).map_err(|err| format!("{err:?}"))
}

/// `scheme://host[:port]` of an http(s) URL.
fn origin_of(url: &str) -> Result<String, String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("url {url:?}: no scheme"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        return Err(format!("url {url:?}: no host"));
    }
    Ok(format!("{}://{}", scheme.to_ascii_lowercase(), authority))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn steps_parse_into_typed_specs() {
        assert_eq!(Step::parse("click:greet"), Ok(Step::Click("greet".into())));
        assert_eq!(
            Step::parse("type:who=Ada Lovelace"),
            Ok(Step::Type("who".into(), "Ada Lovelace".into()))
        );
        assert_eq!(
            Step::parse("key:Enter"),
            Ok(Step::Key(None, "Enter".into()))
        );
        assert_eq!(
            Step::parse("key:who=Backspace"),
            Ok(Step::Key(Some("who".into()), "Backspace".into()))
        );
        assert_eq!(Step::parse("scroll:-300"), Ok(Step::Scroll(-300)));
        assert_eq!(
            Step::parse("navigate:http://127.0.0.1:1/x"),
            Ok(Step::Navigate("http://127.0.0.1:1/x".into()))
        );
        assert_eq!(
            Step::parse("expect-node:heading|Greeter"),
            Ok(Step::ExpectNode("heading".into(), "Greeter".into()))
        );
        assert_eq!(
            Step::parse("expect-state:paragraph|Hello, Ada!"),
            Ok(Step::ExpectState("paragraph".into(), "Hello, Ada!".into()))
        );
        assert_eq!(
            Step::parse("expect-testid:out"),
            Ok(Step::ExpectTestId("out".into()))
        );
        for bad in [
            "click",
            "click:",
            "type:who",
            "scroll:fast",
            "expect-node:heading",
            "fly:away",
            "key:=Enter",
        ] {
            assert!(Step::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_command_line_requires_one_http_url_and_bounds_steps() {
        let parsed = BrowserArgs::parse(&args(&[
            "http://127.0.0.1:8080/",
            "--step",
            "click:go",
            "--screenshot",
            "--json",
            "--root",
            "/tmp/x",
        ]))
        .expect("parse");
        assert_eq!(parsed.url, "http://127.0.0.1:8080/");
        assert_eq!(parsed.steps, vec![Step::Click("go".into())]);
        assert!(parsed.screenshot && parsed.json);
        assert_eq!(parsed.root, Some(PathBuf::from("/tmp/x")));

        assert!(BrowserArgs::parse(&args(&[])).is_err());
        assert!(BrowserArgs::parse(&args(&["file:///etc/passwd"])).is_err());
        assert!(BrowserArgs::parse(&args(&["http://a", "http://b"])).is_err());
        assert!(BrowserArgs::parse(&args(&["http://a", "--step"])).is_err());
        assert!(BrowserArgs::parse(&args(&["http://a", "--bogus"])).is_err());
        let mut many = vec!["http://a".to_owned()];
        for _ in 0..=MAX_STEPS {
            many.push("--step".into());
            many.push("key:Enter".into());
        }
        assert!(BrowserArgs::parse(&many).is_err());
    }

    #[test]
    fn origins_come_from_the_url_authority() {
        assert_eq!(
            origin_of("HTTP://127.0.0.1:8080/path?q=1").as_deref(),
            Ok("http://127.0.0.1:8080")
        );
        assert_eq!(
            origin_of("https://example.test").as_deref(),
            Ok("https://example.test")
        );
        assert!(origin_of("example.test").is_err());
        assert!(origin_of("http:///x").is_err());
    }

    #[test]
    fn a_lease_is_minted_for_the_urls_origin() {
        let lease = lease_for_url("http://127.0.0.1:9/index.html").expect("lease");
        assert_eq!(lease.capability(), Capability::BrowserNavigate);
        assert!(lease.remaining_uses() >= 1);
    }

    #[test]
    fn help_and_usage_errors_do_not_launch_a_browser() {
        assert_eq!(run_browser(&args(&["--help"])).expect("help"), 0);
        assert!(matches!(
            run_browser(&args(&["not-a-url"])),
            Err(P9CommandError::Usage)
        ));
        assert!(matches!(
            run_browser(&args(&[])),
            Err(P9CommandError::Usage)
        ));
    }
}
