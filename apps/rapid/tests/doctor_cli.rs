//! End-to-end coverage for `rapid doctor` against the **real compiled
//! binary**, driven with isolated homes, projects, and configuration.
//!
//! What these prove, beyond "the command exits 0":
//!
//!   - a healthy controlled configuration reports every mandatory check;
//!   - a broken model configuration exits non-zero and names the fault;
//!   - an untrusted project still produces a full report, warns with the
//!     real `rapid trust grant` remediation, and does **not** mutate trust;
//!   - outside a project, project-scoped checks skip rather than crash;
//!   - a secret-bearing configuration never leaks the key on stdout or
//!     stderr, even when the provider entry is malformed enough to error;
//!   - the default command performs **no network access**: a provider is
//!     configured pointing at a listener that would record any connection,
//!     and the listener must stay untouched;
//!   - the crossed fallback chain reports the same safe `(minimum
//!     context_limit, maximum max_output)` pair execution derives.

use std::io::Read;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

static SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rapidlm-doctor-{name}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// An isolated home + project pair. `home` is the parent of `.rapidlm`, so
/// `HOME=<home>` makes the binary resolve `<home>/.rapidlm` exactly as it
/// does for a real user.
struct Fixture {
    home: PathBuf,
    project: PathBuf,
}

fn fixture(name: &str) -> Fixture {
    let root = temp_dir(name);
    let home = root.join("home");
    let project = root.join("project");
    std::fs::create_dir_all(home.join(".rapidlm")).expect("home");
    std::fs::create_dir_all(project.join(".rapidlm")).expect("project");
    Fixture { home, project }
}

fn write_config(fixture: &Fixture, body: &str) -> PathBuf {
    let path = fixture.home.join(".rapidlm").join("config.toml");
    std::fs::write(&path, body).expect("write config");
    path
}

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Run {
    fn row(&self, id: &str) -> String {
        self.stdout
            .lines()
            // Only status-prefixed lines are rows; an indented `-> …`
            // remediation line must never be mistaken for one.
            .filter(|line| {
                ["PASS", "FAIL", "WARN", "SKIP"]
                    .iter()
                    .any(|label| line.starts_with(label))
            })
            .find(|line| line.split_whitespace().nth(1) == Some(id))
            .unwrap_or_else(|| panic!("no `{id}` row in:\n{}", self.stdout))
            .to_owned()
    }

    fn status(&self, id: &str) -> String {
        self.row(id)
            .split_whitespace()
            .next()
            .expect("status")
            .to_owned()
    }
}

fn run_doctor_in(cwd: &Path, home: &Path, config: Option<&Path>) -> Run {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rapid"));
    command
        .arg("doctor")
        .current_dir(cwd)
        .env("HOME", home)
        .env_remove("RAPIDLM_HOME")
        .env_remove("RAPIDLM_MODEL")
        .env_remove("RAPIDLM_CONFIG")
        // A managed policy inherited from the developer's own environment
        // would rewrite the resolved model and add `managed gate:` notes,
        // failing these tests for a reason that has nothing to do with them.
        .env_remove("RAPIDLM_MANAGED_CONFIG");
    if let Some(config) = config {
        command.env("RAPIDLM_CONFIG", config);
    }
    let output = command.output().expect("run rapid doctor");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn healthy_config(base_url: &str) -> String {
    format!(
        "[models]\ndefault = \"local\"\n\n\
         [model.local]\n\
         provider = \"openai-compatible\"\n\
         model = \"test-model\"\n\
         base_url = \"{base_url}\"\n\
         api_key = \"doctor-e2e-secret-key\"\n\
         context_window = 200000\n\
         max_tokens = 8192\n"
    )
}

/// A listener that accepts nothing and only records whether anything ever
/// connected. Used to prove the default command performs no network I/O.
struct Tripwire {
    addr: std::net::SocketAddr,
    connections: Arc<Mutex<usize>>,
}

fn tripwire() -> Tripwire {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("addr");
    let connections = Arc::new(Mutex::new(0usize));
    let counter = Arc::clone(&connections);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            *counter.lock().expect("lock") += 1;
            // Drain a little so a client that did connect is definitely
            // recorded before the connection drops.
            let mut sink = [0u8; 64];
            let _ = stream.read(&mut sink);
        }
    });
    Tripwire { addr, connections }
}

/// Every row the report always carries, in the order it renders them. Not
/// all of these are *mandatory* checks — several are optional integrations
/// that skip — but the set and its order are invariant, which is what makes
/// the output diffable and scriptable.
const EXPECTED_CHECKS: [&str; 21] = [
    "environment",
    "home",
    "config",
    "model",
    "credentials",
    "context-budget",
    "project",
    "project-trust",
    "trust-store",
    "workspace-tools",
    "sandbox",
    "sandbox-probe",
    "git",
    "scanner",
    "hooks",
    "mcp",
    "plugins",
    "credential-store",
    "project-config-exposure",
    "security-policy",
    "release-signature",
];

#[test]
fn a_healthy_controlled_configuration_succeeds_and_reports_every_check() {
    let fixture = fixture("healthy");
    let config = write_config(&fixture, &healthy_config("http://127.0.0.1:9/v1"));
    let run = run_doctor_in(&fixture.project, &fixture.home, Some(&config));

    assert_eq!(
        run.code,
        Some(0),
        "stdout:\n{}\nstderr:\n{}",
        run.stdout,
        run.stderr
    );
    for id in EXPECTED_CHECKS {
        assert!(
            run.stdout
                .lines()
                .any(|line| line.split_whitespace().nth(1) == Some(id)),
            "missing `{id}` row in:\n{}",
            run.stdout
        );
    }
    assert_eq!(run.status("config"), "PASS");
    assert_eq!(run.status("model"), "PASS");
    assert_eq!(run.status("credentials"), "PASS");
    assert_eq!(run.status("context-budget"), "PASS");
    // The model row must describe the real resolved provider, not a guess.
    assert!(run.row("model").contains("openai-compatible"));
    assert!(run.row("model").contains("test-model"));
    // Connectivity was never tested, and the report says so rather than
    // implying the endpoint was reached.
    assert!(run.row("model").contains("connectivity not tested"));
    // Budget comes from this config's own explicit capabilities.
    assert!(run.row("context-budget").contains("context_window=200000"));
    assert!(run.row("context-budget").contains("output_reserve=8192"));
    assert!(run.row("context-budget").contains("input_budget=191808"));
}

#[test]
fn the_rows_render_in_a_stable_order_across_runs() {
    let fixture = fixture("order");
    let config = write_config(&fixture, &healthy_config("http://127.0.0.1:9/v1"));
    let first = run_doctor_in(&fixture.project, &fixture.home, Some(&config));
    let second = run_doctor_in(&fixture.project, &fixture.home, Some(&config));
    let ids = |run: &Run| -> Vec<String> {
        run.stdout
            .lines()
            .filter(|line| {
                ["PASS", "FAIL", "WARN", "SKIP"]
                    .iter()
                    .any(|label| line.starts_with(label))
            })
            .filter_map(|line| line.split_whitespace().nth(1).map(str::to_owned))
            .collect()
    };
    assert_eq!(ids(&first), ids(&second));
    assert_eq!(ids(&first), EXPECTED_CHECKS.to_vec());
}

#[test]
fn a_broken_model_configuration_exits_nonzero_and_names_the_failure() {
    let fixture = fixture("broken-model");
    // `base_url` is not an http(s) origin: `ConfiguredModel::build` rejects
    // it eagerly and locally, the same way a real turn would.
    let config = write_config(
        &fixture,
        "[models]\ndefault = \"local\"\n\n\
         [model.local]\n\
         provider = \"openai-compatible\"\n\
         model = \"test-model\"\n\
         base_url = \"not-a-url\"\n\
         api_key = \"doctor-e2e-secret-key\"\n",
    );
    let run = run_doctor_in(&fixture.project, &fixture.home, Some(&config));

    assert_eq!(run.code, Some(1), "stdout:\n{}", run.stdout);
    assert_eq!(run.status("model"), "FAIL");
    assert!(
        run.row("model").contains("base_url"),
        "the failure must name what is wrong: {}",
        run.row("model")
    );
    // A prerequisite failure must not cascade into fake failures.
    assert_eq!(run.status("context-budget"), "SKIP");
    // Unrelated checks still ran.
    assert_eq!(run.status("environment"), "PASS");
    assert!(run.stdout.contains("1 failed"));
}

#[test]
fn a_malformed_config_file_fails_with_the_real_path_and_skips_nothing_else() {
    let fixture = fixture("malformed");
    let config = write_config(&fixture, "this is not = = toml [[[");
    let run = run_doctor_in(&fixture.project, &fixture.home, Some(&config));

    assert_eq!(run.code, Some(1));
    assert_eq!(run.status("config"), "FAIL");
    assert!(
        run.row("config").contains(&config.display().to_string()),
        "the failing config path must be named: {}",
        run.row("config")
    );
    assert_eq!(run.status("model"), "FAIL");
    assert_eq!(run.status("project"), "PASS");
}

#[test]
fn a_rapidlm_config_pointing_at_a_missing_file_fails_and_says_it_is_missing() {
    let fixture = fixture("explicit-missing");
    let absent = fixture.home.join("does-not-exist.toml");
    let run = run_doctor_in(&fixture.project, &fixture.home, Some(&absent));

    assert_eq!(run.code, Some(1));
    assert_eq!(run.status("config"), "FAIL");
    let row = run.row("config");
    assert!(row.contains("does not exist"), "{row}");
    assert!(
        !row.contains("is invalid"),
        "a missing file must not be described as invalid: {row}"
    );
}

#[test]
fn an_unknown_config_key_warns_only_on_the_config_row_and_leaves_the_model_passing() {
    // The `config` row already reports unrecognized keys with the right
    // remediation; repeating it on the `model` row cost that row its PASS
    // and pointed the user at fallback entries that do not exist here.
    let fixture = fixture("unknown-key");
    let mut body = healthy_config("http://127.0.0.1:9/v1");
    body.push_str("typo_key = \"oops\"\n");
    let config = write_config(&fixture, &body);
    let run = run_doctor_in(&fixture.project, &fixture.home, Some(&config));

    assert_eq!(run.code, Some(0));
    assert_eq!(run.status("config"), "WARN", "{}", run.row("config"));
    assert!(run.row("config").contains("unknown key"));
    assert_eq!(
        run.status("model"),
        "PASS",
        "the model resolved and constructed: {}",
        run.row("model")
    );
    assert!(!run.row("model").contains("unknown config key"));
}

#[test]
fn an_untrusted_project_reports_fully_warns_with_the_real_command_and_never_grants_trust() {
    let fixture = fixture("untrusted");
    let config = write_config(&fixture, &healthy_config("http://127.0.0.1:9/v1"));
    let catalog = fixture.home.join(".rapidlm").join("project-trust.json");
    assert!(!catalog.exists());

    let run = run_doctor_in(&fixture.project, &fixture.home, Some(&config));

    assert_eq!(run.code, Some(0), "an untrusted project is not a failure");
    assert_eq!(run.status("project-trust"), "WARN");
    assert!(run.stdout.contains("rapid trust grant"));
    assert_eq!(run.status("workspace-tools"), "WARN");

    // The mandatory read-only guarantee: doctor must not have granted trust.
    let granted = std::fs::read_to_string(&catalog).unwrap_or_default();
    assert!(
        !granted.contains("\"trusted\"") && !granted.contains("Trusted"),
        "doctor must never grant trust; catalog now reads: {granted}"
    );
    let after = run_doctor_in(&fixture.project, &fixture.home, Some(&config));
    assert_eq!(
        after.status("project-trust"),
        "WARN",
        "a second run must still see an untrusted project"
    );
}

#[test]
fn a_trusted_project_reports_trusted_and_enables_workspace_tools() {
    let fixture = fixture("trusted");
    let config = write_config(&fixture, &healthy_config("http://127.0.0.1:9/v1"));
    // Grant through the real, human-only control plane, not by hand.
    let grant = Command::new(env!("CARGO_BIN_EXE_rapid"))
        .args(["trust", "grant"])
        .current_dir(&fixture.project)
        .env("HOME", &fixture.home)
        .env_remove("RAPIDLM_HOME")
        .output()
        .expect("rapid trust grant");
    assert!(
        grant.status.success(),
        "{}",
        String::from_utf8_lossy(&grant.stderr)
    );

    let run = run_doctor_in(&fixture.project, &fixture.home, Some(&config));
    assert_eq!(run.status("project-trust"), "PASS");
    assert_eq!(run.status("workspace-tools"), "PASS");
    assert_eq!(run.code, Some(0));
}

#[test]
fn outside_a_project_the_global_checks_still_run_and_project_checks_skip_cleanly() {
    let root = temp_dir("outside");
    let home = root.join("home");
    // A bare directory: no `.rapidlm`, no `.git`, and nothing above it
    // inside the temp tree.
    let bare = root.join("bare");
    std::fs::create_dir_all(home.join(".rapidlm")).expect("home");
    std::fs::create_dir_all(&bare).expect("bare");
    let config = home.join(".rapidlm").join("config.toml");
    std::fs::write(&config, healthy_config("http://127.0.0.1:9/v1")).expect("config");

    let run = run_doctor_in(&bare, &home, Some(&config));

    // Global checks still ran.
    assert_eq!(run.status("environment"), "PASS");
    assert_eq!(run.status("config"), "PASS");
    assert_eq!(run.status("model"), "PASS");
    assert_eq!(run.status("context-budget"), "PASS");
    // Project-scoped integrations skip rather than fail or crash. (The
    // system temp directory may itself sit under a marker on some hosts, so
    // this asserts the checks are non-fatal, which is the actual contract.)
    for id in ["scanner", "hooks", "mcp", "plugins"] {
        let status = run.status(id);
        assert!(
            status == "SKIP" || status == "PASS" || status == "WARN",
            "`{id}` must never fail outside a project, got {status}"
        );
    }
    assert_eq!(run.code, Some(0), "stdout:\n{}", run.stdout);
}

#[test]
fn no_configured_model_warns_but_still_reports_a_real_context_budget() {
    let fixture = fixture("unconfigured");
    // No config file at all: the documented, supported default state.
    let run = run_doctor_in(&fixture.project, &fixture.home, None);

    assert_eq!(run.code, Some(0), "an absent config is not a failure");
    assert_eq!(run.status("config"), "WARN");
    assert_eq!(run.status("model"), "WARN");
    assert_eq!(run.status("credentials"), "SKIP");
    // The typed no-model fallback still has a real, production-derived
    // budget — `context_budget_for`'s own Unconfigured arm.
    assert_eq!(run.status("context-budget"), "PASS");
    assert!(run.row("context-budget").contains("context_window=32768"));
    assert!(run.row("context-budget").contains("output_reserve=4096"));
    assert!(
        run.row("context-budget")
            .contains("source=default (no model configured)")
    );
}

#[test]
fn a_crossed_fallback_chain_reports_the_same_safe_budget_execution_derives() {
    // The regression fixture for the context-budget safety rule: backend A
    // has the smaller context window but the smaller output cap, backend B
    // the larger of both. The safe pair is the *minimum* context_limit with
    // the *maximum* max_output — 100000 and 16384 — never (100000, 4096),
    // which would under-reserve headroom for whichever backend actually
    // serves the turn.
    let fixture = fixture("crossed-chain");
    let config = write_config(
        &fixture,
        "[models]\ndefault = \"small\"\nfallback = [\"large\"]\n\n\
         [model.small]\n\
         provider = \"openai-compatible\"\n\
         model = \"small-model\"\n\
         base_url = \"http://127.0.0.1:9/v1\"\n\
         api_key = \"doctor-e2e-secret-key\"\n\
         context_window = 100000\n\
         max_tokens = 4096\n\n\
         [model.large]\n\
         provider = \"openai-compatible\"\n\
         model = \"large-model\"\n\
         base_url = \"http://127.0.0.1:9/v1\"\n\
         api_key = \"doctor-e2e-secret-key\"\n\
         context_window = 400000\n\
         max_tokens = 16384\n",
    );
    let run = run_doctor_in(&fixture.project, &fixture.home, Some(&config));

    assert_eq!(run.code, Some(0), "stdout:\n{}", run.stdout);
    let row = run.row("context-budget");
    assert!(row.contains("context_window=100000"), "{row}");
    assert!(row.contains("output_reserve=16384"), "{row}");
    assert!(row.contains("input_budget=83616"), "{row}");
    assert!(
        row.contains("chain: minimum context_limit / maximum max_output"),
        "{row}"
    );
    // And the model row must show the chain it actually resolved.
    assert!(
        run.row("model").contains("small -> large"),
        "{}",
        run.row("model")
    );
}

#[test]
fn a_secret_bearing_configuration_never_leaks_the_key_on_stdout_or_stderr() {
    const SECRET: &str = "sk-doctor-must-never-print-this-0123456789";
    let fixture = fixture("secret");
    // Deliberately malformed enough that the provider construction path
    // errors while still carrying the key: the error-formatting path is
    // exactly where a leak would happen.
    let config = write_config(
        &fixture,
        &format!(
            "[models]\ndefault = \"local\"\n\n\
             [model.local]\n\
             provider = \"openai-compatible\"\n\
             model = \"test-model\"\n\
             base_url = \"http://user:{SECRET}@example.invalid/v1\"\n\
             api_key = \"{SECRET}\"\n"
        ),
    );
    let run = run_doctor_in(&fixture.project, &fixture.home, Some(&config));

    assert!(
        !run.stdout.contains(SECRET),
        "the credential leaked to stdout:\n{}",
        run.stdout
    );
    assert!(
        !run.stderr.contains(SECRET),
        "the credential leaked to stderr:\n{}",
        run.stderr
    );
    // The failure itself must still be reported — redaction must not hide
    // the diagnosis, only the secret.
    assert_eq!(run.status("model"), "FAIL");
    assert_eq!(run.code, Some(1));
}

#[test]
fn a_credential_embedded_only_in_the_base_url_is_also_never_printed() {
    // The harder case than the one above: the entry is *keyless*, so the
    // resolved-credential list is empty and the naive "scrub the resolved
    // key" approach would print this verbatim on the model row and in the
    // provider-construction error that quotes the URL.
    const SECRET: &str = "pw-doctor-url-only-must-never-print-4242";
    let fixture = fixture("secret-url");
    let config = write_config(
        &fixture,
        &format!(
            "[models]\ndefault = \"local\"\n\n\
             [model.local]\n\
             provider = \"openai-compatible\"\n\
             model = \"test-model\"\n\
             base_url = \"http://svc:{SECRET}@example.invalid/v1\"\n"
        ),
    );
    let run = run_doctor_in(&fixture.project, &fixture.home, Some(&config));

    assert!(
        !run.stdout.contains(SECRET),
        "the URL credential leaked to stdout:\n{}",
        run.stdout
    );
    assert!(
        !run.stderr.contains(SECRET),
        "the URL credential leaked to stderr:\n{}",
        run.stderr
    );
    assert_eq!(run.status("model"), "FAIL");
}

#[test]
fn the_default_command_makes_no_network_request_to_the_configured_provider() {
    // A future change that made doctor "verify connectivity" by contacting
    // the provider — a potentially billable request — would trip this.
    let wire = tripwire();
    let fixture = fixture("offline");
    let config = write_config(
        &fixture,
        &healthy_config(&format!("http://{}/v1", wire.addr)),
    );
    let run = run_doctor_in(&fixture.project, &fixture.home, Some(&config));

    assert_eq!(run.code, Some(0), "stdout:\n{}", run.stdout);
    assert_eq!(run.status("model"), "PASS");
    // Give a stray connection a moment to be recorded before asserting.
    thread::sleep(std::time::Duration::from_millis(200));
    assert_eq!(
        *wire.connections.lock().expect("lock"),
        0,
        "rapid doctor contacted the configured provider; it must stay offline by default"
    );
}

#[test]
fn help_is_printed_without_running_any_check_and_exits_zero() {
    let fixture = fixture("help");
    let output = Command::new(env!("CARGO_BIN_EXE_rapid"))
        .args(["doctor", "--help"])
        .current_dir(&fixture.project)
        .env("HOME", &fixture.home)
        .env_remove("RAPIDLM_HOME")
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("usage: rapid doctor"));
    assert!(
        !stdout.contains("PASS "),
        "help must not run checks:\n{stdout}"
    );
}

#[test]
fn configured_hooks_scanners_mcp_and_plugins_are_reported_without_being_executed() {
    let fixture = fixture("integrations");
    let config = write_config(&fixture, &healthy_config("http://127.0.0.1:9/v1"));

    // A hook whose script would create a marker file if it ever ran, plus
    // one whose path does not exist at all.
    let marker = fixture.project.join("HOOK-RAN");
    let script = fixture.project.join("hook.sh");
    std::fs::write(&script, format!("#!/bin/sh\ntouch {}\n", marker.display()))
        .expect("hook script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    // The script path is JSON-encoded: a Windows path's backslashes are
    // escapes to the settings parser, and a settings file that does not
    // parse reads as "no hooks configured".
    let script_json = serde_json::to_string(&script.display().to_string()).expect("json");
    std::fs::write(
        fixture.project.join(".rapidlm").join("settings.json"),
        format!(
            r#"{{"hooks":{{"pre_tool_use":[{script_json},"./definitely-absent-hook.sh"]}},
                "mcpServers":{{"demo":{{"command":"/bin/echo","args":["hi"]}}}}}}"#
        ),
    )
    .expect("settings");

    // A scanner whose executable does not exist.
    std::fs::write(
        fixture.project.join(".rapidlm").join("scanners.json"),
        r#"{"scanners":[{"id":"absent-scanner","kind":"sast","argv":["definitely-not-a-real-scanner-xyz","."]}]}"#,
    )
    .expect("scanners");

    let run = run_doctor_in(&fixture.project, &fixture.home, Some(&config));

    // Reported, not fatal — these gate optional capabilities only.
    assert_eq!(run.code, Some(0), "stdout:\n{}", run.stdout);
    assert_eq!(run.status("hooks"), "WARN", "{}", run.row("hooks"));
    assert!(run.row("hooks").contains("definitely-absent-hook.sh"));
    assert_eq!(run.status("scanner"), "WARN", "{}", run.row("scanner"));
    assert!(run.row("scanner").contains("absent-scanner"));
    // MCP is configured but the project is untrusted, so it is truthfully
    // reported as not registered rather than as working.
    assert_eq!(run.status("mcp"), "WARN", "{}", run.row("mcp"));
    assert!(run.row("mcp").contains("demo"));
    assert!(run.row("mcp").contains("untrusted"));

    // The mandatory guarantee: nothing was executed.
    assert!(
        !marker.exists(),
        "doctor executed a project hook; it must never do that"
    );
}

#[test]
fn the_sandbox_probe_executes_the_real_backend_and_leaves_the_project_untouched() {
    let fixture = fixture("sandbox");
    let config = write_config(&fixture, &healthy_config("http://127.0.0.1:9/v1"));
    let marker = fixture.project.join("source.txt");
    std::fs::write(&marker, b"untouched").expect("marker");
    let before: Vec<_> = std::fs::read_dir(&fixture.project)
        .expect("read")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .collect();

    let run = run_doctor_in(&fixture.project, &fixture.home, Some(&config));

    let status = run.status("sandbox-probe");
    assert!(
        status == "PASS" || status == "WARN",
        "the probe must never fail the command: {}",
        run.row("sandbox-probe")
    );
    // The probe runs in a scratch directory, never the project.
    assert_eq!(
        std::fs::read_to_string(&marker).expect("marker"),
        "untouched"
    );
    let after: Vec<_> = std::fs::read_dir(&fixture.project)
        .expect("read")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .collect();
    assert_eq!(
        before.len(),
        after.len(),
        "the project gained or lost files"
    );
}

/// A loopback model server answering every request with `status` and `body`,
/// recording each request it read.
fn model_server(status: u16, body: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
    use std::io::Write;
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let origin = format!("http://{}", listener.local_addr().expect("addr"));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            while let Ok(read) = stream.read(&mut chunk) {
                if read == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..read]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some(end) = text.find("\r\n\r\n") {
                    let length = text[..end]
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if buf.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            log.lock()
                .expect("lock")
                .push(String::from_utf8_lossy(&buf).to_string());
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
    (origin, seen)
}

/// `rapid doctor --live` in `fixture` with `config` (and a managed policy).
fn run_live(fixture: &Fixture, config: &Path, policy: Option<&Path>) -> Run {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rapid"));
    command
        .args(["doctor", "--live"])
        .current_dir(&fixture.project)
        .env("HOME", &fixture.home)
        .env("RAPIDLM_CONFIG", config)
        .env_remove("RAPIDLM_HOME")
        .env_remove("RAPIDLM_MODEL")
        .env_remove("RAPIDLM_MANAGED_CONFIG")
        .env_remove("RAPIDLM_PROXY");
    if let Some(policy) = policy {
        command.env("RAPIDLM_MANAGED_CONFIG", policy);
    }
    let output = command.output().expect("run rapid doctor --live");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

const GOOD_BODY: &str = r#"{"choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1}}"#;

/// A `[model.<id>]` table for `origin`, with `key` inline when given.
fn live_entry(id: &str, origin: &str, key: Option<&str>) -> String {
    let key = key
        .map(|key| format!("api_key = \"{key}\"\n"))
        .unwrap_or_default();
    format!(
        "[model.{id}]\nprovider = \"openai-compatible\"\nmodel = \"test-model\"\n\
         base_url = \"{origin}/v1\"\n{key}\n"
    )
}

/// The status-prefixed rows of a report.
fn rows(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter(|line| {
            ["PASS", "FAIL", "WARN", "SKIP"]
                .iter()
                .any(|label| line.starts_with(label))
        })
        .map(str::to_owned)
        .collect()
}

#[test]
fn live_probes_every_profile_once_and_reports_one_typed_row_each() {
    // SEAM-02 AC-07: one typed check per profile; the offline rows are the
    // same rows, in the same order, with the same text; the live ones follow.
    let (good, good_seen) = model_server(200, GOOD_BODY);
    let (refusing, refusing_seen) = model_server(401, r#"{"error":{"message":"bad key"}}"#);
    let (keyless, keyless_seen) = model_server(401, r#"{"error":{"message":"key needed"}}"#);
    let (limited, limited_seen) = model_server(429, r#"{"error":{"message":"slow down"}}"#);
    let down = {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let origin = format!("http://{}", listener.local_addr().expect("addr"));
        drop(listener);
        origin
    };
    let fixture = fixture("live");
    let config = write_config(
        &fixture,
        &format!(
            "[models]\ndefault = \"good\"\n\n{}{}{}{}{}",
            live_entry("good", &good, Some("doctor-live-secret-good")),
            live_entry("refusing", &refusing, Some("doctor-live-secret-refusing")),
            live_entry("down", &down, Some("doctor-live-secret-down")),
            live_entry("keyless", &keyless, None),
            // A placeholder key a local server ignores: too short to redact,
            // it must not blank an "x" out of every other row.
            live_entry("limited", &limited, Some("x")),
        ),
    );
    let run = run_live(&fixture, &config, None);

    assert_eq!(run.code, Some(1), "a probe failed:\n{}", run.stdout);
    assert_eq!(run.status("live:good"), "PASS");
    assert!(
        run.row("live:good")
            .contains(&format!("answered; egress: allowed {good}")),
        "{}",
        run.stdout
    );
    assert_eq!(run.status("live:refusing"), "FAIL");
    assert!(run.row("live:refusing").contains("auth:"), "{}", run.stdout);
    assert_eq!(run.status("live:down"), "FAIL");
    assert!(run.row("live:down").contains("network:"), "{}", run.stdout);
    assert_eq!(run.status("live:limited"), "FAIL");
    assert!(run.row("live:limited").contains("quota:"), "{}", run.stdout);
    assert_eq!(run.status("live:keyless"), "FAIL");
    assert!(
        run.row("live:keyless")
            .contains("requires a key and none was sent"),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout
            .contains("[model.keyless] names no key: give one with rapid setup --profile keyless"),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("rapid doctor --live"),
        "a failure names the next command:\n{}",
        run.stdout
    );
    // The offline rows first, word for word; then one row per profile
    // (sorted), and nothing else.
    let offline = run_doctor_in(&fixture.project, &fixture.home, Some(&config));
    let live_rows = rows(&run.stdout);
    let offline_rows = rows(&offline.stdout);
    assert_eq!(
        live_rows[..offline_rows.len()],
        offline_rows[..],
        "--live changed an offline row"
    );
    let ids: Vec<&str> = live_rows
        .iter()
        .filter_map(|line| line.split_whitespace().nth(1))
        .collect();
    let mut expected: Vec<&str> = EXPECTED_CHECKS.to_vec();
    expected.extend([
        "live:down",
        "live:good",
        "live:keyless",
        "live:limited",
        "live:refusing",
    ]);
    assert_eq!(ids, expected);
    // One bounded request each; no key ever printed.
    for seen in [&good_seen, &refusing_seen, &keyless_seen, &limited_seen] {
        let requests = seen.lock().expect("lock").clone();
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert!(requests[0].contains("\"max_tokens\":16"), "{}", requests[0]);
    }
    for id in ["good", "refusing", "down"] {
        let secret = format!("doctor-live-secret-{id}");
        assert!(!run.stdout.contains(&secret) && !run.stderr.contains(&secret));
    }
    // Without --live: no request.
    assert!(!offline.stdout.contains("live:"), "{}", offline.stdout);
    assert_eq!(good_seen.lock().expect("lock").len(), 1);
}

#[test]
fn live_under_a_locked_default_probes_the_other_profiles_as_a_run_dials_them() {
    // Locked to corp: a run dials corp, its fallback spare and its compact
    // model cheap — never backup, the file's own default.
    let (corp, corp_seen) = model_server(200, GOOD_BODY);
    let (spare, spare_seen) = model_server(200, GOOD_BODY);
    let (cheap, cheap_seen) = model_server(200, GOOD_BODY);
    let (backup, backup_seen) = model_server(200, GOOD_BODY);
    let fixture = fixture("live-lock");
    let config = write_config(
        &fixture,
        &format!(
            "[models]\ndefault = \"backup\"\nfallback = [\"corp\", \"spare\"]\n\n\
             [phases]\ncompact = \"cheap\"\n\n{}{}{}{}",
            live_entry("corp", &corp, Some("doctor-live-secret-corp")),
            live_entry("spare", &spare, Some("doctor-live-secret-spare")),
            live_entry("cheap", &cheap, Some("doctor-live-secret-cheap")),
            live_entry("backup", &backup, Some("doctor-live-secret-backup")),
        ),
    );
    let policy = fixture.home.join("managed.toml");
    std::fs::write(
        &policy,
        "schema = \"rapidlm.managed_config.v1\"\n[policy]\nlocked_default = \"corp\"\n",
    )
    .expect("policy");
    let run = run_live(&fixture, &config, Some(&policy));
    assert_eq!(run.code, Some(0), "{}{}", run.stdout, run.stderr);
    assert_eq!(run.status("live:corp"), "PASS");
    assert!(!run.row("live:corp").contains("locks"), "{}", run.stdout);
    assert_eq!(run.status("live:spare"), "PASS");
    assert!(
        run.row("live:spare")
            .contains("locks the default to corp; probed as a [models] fallback"),
        "{}",
        run.stdout
    );
    assert_eq!(run.status("live:cheap"), "PASS");
    assert!(
        run.row("live:cheap")
            .contains("probed as the [phases] compact model"),
        "{}",
        run.stdout
    );
    assert_eq!(run.status("live:backup"), "SKIP");
    assert!(
        run.row("live:backup")
            .contains("a run does not dial this profile"),
        "{}",
        run.stdout
    );
    for (seen, expected) in [
        (&corp_seen, 1),
        (&spare_seen, 1),
        (&cheap_seen, 1),
        (&backup_seen, 0),
    ] {
        assert_eq!(seen.lock().expect("lock").len(), expected, "{}", run.stdout);
    }

    // A lock on a provider the policy refuses: the refusal is the locked
    // profile's; the others are judged as what a run dials them as.
    std::fs::write(
        &policy,
        "schema = \"rapidlm.managed_config.v1\"\n[policy]\nlocked_default = \"corp\"\n\
         allowed_providers = [\"anthropic\"]\n",
    )
    .expect("policy");
    let run = run_live(&fixture, &config, Some(&policy));
    assert_eq!(run.status("live:corp"), "SKIP");
    assert!(
        run.row("live:corp").contains("refuses it"),
        "{}",
        run.stdout
    );
    assert_eq!(run.status("live:spare"), "SKIP");
    assert!(
        run.row("live:spare")
            .contains("refuses this profile as a [models] fallback"),
        "{}",
        run.stdout
    );
    assert!(
        run.row("live:backup")
            .contains("a run does not dial this profile"),
        "{}",
        run.stdout
    );
    assert_eq!(
        spare_seen.lock().expect("lock").len(),
        1,
        "nothing more sent"
    );
}

#[test]
fn a_profile_name_cannot_put_control_characters_on_the_report() {
    let (good, _seen) = model_server(200, GOOD_BODY);
    let fixture = fixture("live-id");
    let hostile = format!("x{}", "y".repeat(100));
    let config = write_config(
        &fixture,
        &format!(
            "[models]\ndefault = \"good\"\n\n{}[model.\"a\\u001b[2J\"]\nprovider = \
             \"openai-compatible\"\nmodel = \"m\"\nbase_url = \"{good}/v1\"\n\n{}",
            live_entry("good", &good, None),
            live_entry(&hostile, &good, None),
        ),
    );
    let run = run_live(&fixture, &config, None);
    assert!(!run.stdout.contains('\u{1b}'), "{:?}", run.stdout);
    assert!(run.stdout.contains("live:a?[2J"), "{}", run.stdout);
    // A long profile id is cut, not allowed to pad every row.
    assert!(
        run.stdout.lines().all(|line| line.len() < 400),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("live:xyyy"), "{}", run.stdout);
}

#[test]
fn live_with_no_model_configured_warns_and_points_at_setup() {
    let fixture = fixture("live-none");
    let output = Command::new(env!("CARGO_BIN_EXE_rapid"))
        .args(["doctor", "--live"])
        .current_dir(&fixture.project)
        .env("HOME", &fixture.home)
        .env_remove("RAPIDLM_HOME")
        .env_remove("RAPIDLM_MODEL")
        .env_remove("RAPIDLM_CONFIG")
        .env_remove("RAPIDLM_MANAGED_CONFIG")
        .output()
        .expect("run");
    let run = Run {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    };
    assert_eq!(run.code, Some(0), "{}{}", run.stdout, run.stderr);
    assert_eq!(run.status("live"), "WARN");
    assert!(run.stdout.contains("-> run rapid setup"), "{}", run.stdout);
}
