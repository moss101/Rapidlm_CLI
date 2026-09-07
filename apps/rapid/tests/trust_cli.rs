//! End-to-end coverage for `rapid trust grant|status|revoke` — the real
//! production binary, not the store unit tests in
//! `crates/kernel/src/project/trust.rs`.
//!
//! Proves the actual gap this task closes: before this command existed,
//! nothing in the production binary could ever call
//! `ProjectTrustStore::set` (every prior test — `exec_diagnosability.rs`,
//! `goal_concurrency.rs` — hand-wrote the catalog JSON directly to simulate
//! a grant that no reachable command could perform). These tests use only
//! the real `rapid trust` command, and (for the security-gate proof) prove
//! it actually flips a real trust-gated operation from denied to allowed.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;

const TERMINAL_BODY: &str = r#"{"choices":[{"message":{"role":"assistant","content":"hello from scripted model"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":5}}"#;

fn config_doc(base_url: &str) -> String {
    format!(
        "[models]\ndefault = \"local\"\n\
         \n\
         [model.local]\n\
         provider = \"openai-compatible\"\n\
         model = \"test-model\"\n\
         base_url = \"{base_url}\"\n\
         api_key = \"scripted-key\"\n"
    )
}

fn spawn_scripted_server(body: String) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let _ = read_request(&mut stream);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
        let _ = stream.shutdown(Shutdown::Both);
    });
    addr
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk).unwrap_or(0);
        if read == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..read]);
        let Ok(text) = std::str::from_utf8(&buf) else {
            continue;
        };
        if text.contains("\r\n\r\n") {
            break;
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn write_config(home: &Path, addr: std::net::SocketAddr) -> PathBuf {
    let config = home.join("config.toml");
    std::fs::write(&config, config_doc(&format!("http://{addr}/v1"))).expect("config");
    config
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rapidlm-trustcli-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// A project directory with a real git repo (commit identity configured),
/// deliberately carrying **no** trust record — every test here proves the
/// real `rapid trust` command itself, not a hand-written catalog fixture.
fn git_project(project: &Path) {
    std::fs::create_dir_all(project).expect("project");
    let init = Command::new("git")
        .arg("init")
        .arg("-q")
        .current_dir(project)
        .output()
        .expect("git init");
    assert!(init.status.success(), "git init failed");
    for (key, value) in [("user.email", "trustcli@example.com"), ("user.name", "trustcli")] {
        let _ = Command::new("git")
            .args(["config", key, value])
            .current_dir(project)
            .output()
            .expect("git config");
    }
}

/// Run `rapid trust <args>` against `project`/`home`.
fn run_trust(project: &Path, home: &Path, args: &[&str]) -> (Option<i32>, String, String) {
    let mut all: Vec<String> = vec!["trust".to_owned()];
    all.extend(args.iter().map(|s| s.to_string()));
    let output = Command::new(env!("CARGO_BIN_EXE_rapid"))
        .args(&all)
        .current_dir(project)
        .env("HOME", home)
        .env_remove("RAPIDLM_HOME")
        .output()
        .expect("run rapid trust");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Run `rapid exec <prompt>` against `project`/`home`/`config` — used only
/// as the real, safely-observable trust-gated operation for the
/// security-gate proof (workspace tools stay withheld until trusted).
fn run_exec(project: &Path, home: &Path, config: &Path, prompt: &str) -> (Option<i32>, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_rapid"))
        .args(["exec", prompt])
        .current_dir(project)
        .env("HOME", home)
        .env("RAPIDLM_CONFIG", config)
        .env("RAPIDLM_PERMISSION_MODE", "bypassPermissions")
        .env_remove("RAPIDLM_HOME")
        .env_remove("RAPIDLM_MODEL")
        .output()
        .expect("run rapid exec");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// The catalog's `records` array length — used to prove idempotency creates
/// no duplicate record.
fn catalog_record_count(home: &Path) -> usize {
    let raw = std::fs::read_to_string(home.join(".rapidlm").join("project-trust.json"))
        .expect("catalog must exist");
    let value: serde_json::Value = serde_json::from_str(&raw).expect("catalog must be valid JSON");
    value["records"].as_array().expect("records array").len()
}

#[test]
fn status_reports_untrusted_for_a_fresh_project() {
    let home = temp_dir("status-fresh");
    let project = home.join("project");
    git_project(&project);

    let (code, stdout, _stderr) = run_trust(&project, &home, &["status"]);
    assert_eq!(code, Some(0));
    assert!(stdout.starts_with("untrusted:"), "{stdout}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn grant_then_status_reports_trusted() {
    let home = temp_dir("grant-status");
    let project = home.join("project");
    git_project(&project);

    let (code, stdout, _stderr) = run_trust(&project, &home, &["grant"]);
    assert_eq!(code, Some(0));
    assert!(stdout.starts_with("trust granted:"), "{stdout}");

    let (code, stdout, _stderr) = run_trust(&project, &home, &["status"]);
    assert_eq!(code, Some(0));
    assert!(stdout.starts_with("trusted:"), "{stdout}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn grant_is_idempotent_and_creates_no_duplicate_record() {
    let home = temp_dir("grant-idempotent");
    let project = home.join("project");
    git_project(&project);

    let (code, stdout, _) = run_trust(&project, &home, &["grant"]);
    assert_eq!(code, Some(0));
    assert!(stdout.starts_with("trust granted:"), "{stdout}");
    assert_eq!(catalog_record_count(&home), 1);

    let (code, stdout, _) = run_trust(&project, &home, &["grant"]);
    assert_eq!(code, Some(0));
    assert!(stdout.starts_with("already trusted:"), "{stdout}");
    assert_eq!(catalog_record_count(&home), 1, "a repeated grant must not duplicate the record");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn revoke_after_grant_returns_to_untrusted_and_is_itself_idempotent() {
    let home = temp_dir("revoke");
    let project = home.join("project");
    git_project(&project);

    run_trust(&project, &home, &["grant"]);
    let (code, stdout, _) = run_trust(&project, &home, &["revoke"]);
    assert_eq!(code, Some(0));
    assert!(stdout.starts_with("trust revoked:"), "{stdout}");

    let (code, stdout, _) = run_trust(&project, &home, &["status"]);
    assert_eq!(code, Some(0));
    assert!(stdout.starts_with("untrusted:"), "{stdout}");

    let (code, stdout, _) = run_trust(&project, &home, &["revoke"]);
    assert_eq!(code, Some(0));
    assert!(stdout.starts_with("already untrusted:"), "{stdout}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn grant_from_a_nested_subdirectory_resolves_the_same_project_root() {
    let home = temp_dir("nested");
    let project = home.join("project");
    let nested = project.join("src").join("deep");
    git_project(&project);
    std::fs::create_dir_all(&nested).expect("nested dir");

    let (code, stdout, _) = run_trust(&nested, &home, &["grant"]);
    assert_eq!(code, Some(0));
    assert!(stdout.starts_with("trust granted:"), "{stdout}");

    // Queried from the project root, not the nested directory the grant
    // itself ran from — both must resolve to the identical canonical
    // identity via the shared `.git`-marker walk-up.
    let (code, stdout, _) = run_trust(&project, &home, &["status"]);
    assert_eq!(code, Some(0));
    assert!(stdout.starts_with("trusted:"), "{stdout}");
    assert_eq!(catalog_record_count(&home), 1);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn unrelated_sibling_project_stays_untrusted_after_granting_another() {
    let home = temp_dir("sibling");
    let project_a = home.join("project-a");
    let project_b = home.join("project-b");
    git_project(&project_a);
    git_project(&project_b);

    run_trust(&project_a, &home, &["grant"]);

    let (code, stdout, _) = run_trust(&project_b, &home, &["status"]);
    assert_eq!(code, Some(0));
    assert!(
        stdout.starts_with("untrusted:"),
        "granting project A must never trust an unrelated sibling B: {stdout}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[cfg(unix)]
#[test]
fn grant_via_a_symlink_and_status_via_the_real_path_agree() {
    let home = temp_dir("symlink");
    let real = home.join("real-project");
    let link = home.join("via-symlink");
    git_project(&real);
    std::os::unix::fs::symlink(&real, &link).expect("symlink project");

    // Grant through the symlinked path...
    let (code, stdout, _) = run_trust(&link, &home, &["grant"]);
    assert_eq!(code, Some(0));
    assert!(stdout.starts_with("trust granted:"), "{stdout}");

    // ...and confirm the real path (never named explicitly, only ever
    // reached via canonicalization) reports trusted too: both sides
    // resolve to the same canonical identity, so there is exactly one
    // record either way, never two conflicting ones for the same project.
    let (code, stdout, _) = run_trust(&real, &home, &["status"]);
    assert_eq!(code, Some(0));
    assert!(stdout.starts_with("trusted:"), "{stdout}");
    assert_eq!(
        catalog_record_count(&home),
        1,
        "the symlink and its real target must collapse to one identity, not two"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn trust_bogus_verb_is_a_usage_error() {
    let home = temp_dir("bogus-verb");
    let project = home.join("project");
    git_project(&project);

    let (code, _stdout, stderr) = run_trust(&project, &home, &["bogus"]);
    assert_ne!(code, Some(0));
    assert!(stderr.contains("usage: rapid trust"), "{stderr}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn trust_help_documents_all_three_verbs() {
    let home = temp_dir("help");
    let project = home.join("project");
    git_project(&project);

    let (code, stdout, _stderr) = run_trust(&project, &home, &["--help"]);
    assert_eq!(code, Some(0));
    for verb in ["grant", "status", "revoke"] {
        assert!(stdout.contains(verb), "usage text missing {verb}: {stdout}");
    }
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn malformed_catalog_fails_closed_for_status_and_leaves_the_file_untouched() {
    let home = temp_dir("malformed");
    let project = home.join("project");
    git_project(&project);
    let trust_dir = home.join(".rapidlm");
    std::fs::create_dir_all(&trust_dir).expect("rapidlm home");
    let catalog = trust_dir.join("project-trust.json");
    std::fs::write(&catalog, "{not-valid-json").expect("corrupt catalog");
    let before = std::fs::read_to_string(&catalog).expect("read corrupt catalog");

    let (code, _stdout, stderr) = run_trust(&project, &home, &["status"]);
    assert_ne!(code, Some(0), "a corrupt catalog must never silently report untrusted/trusted");
    assert!(!stderr.is_empty(), "a corrupt-catalog failure must be reported");

    let after = std::fs::read_to_string(&catalog).expect("read catalog after failed status");
    assert_eq!(before, after, "a failed read must never rewrite/replace the corrupt file");
    let _ = std::fs::remove_dir_all(&home);
}

/// The end-to-end security-gate proof the task calls for: a real
/// trust-gated operation (headless `rapid exec`'s workspace tools) denied
/// before any grant, and allowed after the real production `rapid trust
/// grant` — not the store's own unit tests, and not a hand-written catalog
/// fixture (`exec_diagnosability.rs`'s `trusted_project` helper, which
/// existed only because no reachable grant command existed before this
/// task).
#[test]
fn security_gate_exec_workspace_tools_flip_from_denied_to_allowed_after_grant() {
    let home = temp_dir("security-gate");
    let project = home.join("project");
    git_project(&project);

    let server = spawn_scripted_server(TERMINAL_BODY.to_owned());
    let config = write_config(&home, server);
    let (code, stdout, stderr) = run_exec(&project, &home, &config, "say pong");
    assert_eq!(code, Some(0));
    assert!(stdout.contains("hello from scripted model"));
    assert!(
        stderr.contains("workspace tools are disabled"),
        "gate must deny before any grant: {stderr}"
    );

    let (code, stdout, _stderr) = run_trust(&project, &home, &["grant"]);
    assert_eq!(code, Some(0));
    assert!(stdout.starts_with("trust granted:"), "{stdout}");

    let server = spawn_scripted_server(TERMINAL_BODY.to_owned());
    let config = write_config(&home, server);
    let (code, stdout, stderr) = run_exec(&project, &home, &config, "say pong again");
    assert_eq!(code, Some(0));
    assert!(stdout.contains("hello from scripted model"));
    assert!(
        !stderr.contains("workspace tools are disabled"),
        "gate must allow after the real production grant: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// Same gate, the other direction: `rapid trust revoke` must flip it back.
#[test]
fn security_gate_revoke_disables_workspace_tools_again() {
    let home = temp_dir("security-gate-revoke");
    let project = home.join("project");
    git_project(&project);
    run_trust(&project, &home, &["grant"]);

    let server = spawn_scripted_server(TERMINAL_BODY.to_owned());
    let config = write_config(&home, server);
    let (code, stdout, stderr) = run_exec(&project, &home, &config, "say pong");
    assert_eq!(code, Some(0), "exec must actually reach the model, not fail before the gate: {stderr}");
    assert!(stdout.contains("hello from scripted model"), "{stdout}");
    assert!(!stderr.contains("workspace tools are disabled"), "{stderr}");

    run_trust(&project, &home, &["revoke"]);

    let server = spawn_scripted_server(TERMINAL_BODY.to_owned());
    let config = write_config(&home, server);
    let (code, stdout, stderr) = run_exec(&project, &home, &config, "say pong");
    assert_eq!(code, Some(0), "exec must actually reach the model, not fail before the gate: {stderr}");
    assert!(stdout.contains("hello from scripted model"), "{stdout}");
    assert!(
        stderr.contains("workspace tools are disabled"),
        "revoke must re-close the gate: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&home);
}
