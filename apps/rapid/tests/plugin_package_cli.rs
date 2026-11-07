#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "rapid-plugin-cli-{}-{}",
            std::process::id(),
            protocol::EventId::new()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn run(&self, args: &[&str], policy: Option<&Path>) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rapid"));
        command
            .current_dir(&self.0)
            .env("HOME", &self.0)
            .env("RAPIDLM_HOME", &self.0)
            .env_remove("RAPIDLM_MANAGED_CONFIG")
            .env_remove("RAPIDLM_MANAGED_TRUSTED_KEYS")
            .env_remove("RAPIDLM_MANAGED_REQUIRE_SIGNATURE");
        if let Some(policy) = policy {
            command.env("RAPIDLM_MANAGED_CONFIG", policy);
        }
        command.args(args).output().unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_package(root: &Path, version: &str, bytes: &[u8]) -> (PathBuf, PathBuf) {
    let manifest = root.join("manifest.json");
    let package = root.join("package.bin");
    std::fs::write(&package, bytes).unwrap();
    std::fs::write(&manifest, format!(r#"{{"schema":"rapidlm.plugin_manifest","schema_version":1,"id":"acme.fmt","version":"{version}","publisher":"acme","digest":"{}","entrypoint":"plugin.wasm","wit_version":"1.0.0","compatibility":{{"min":"1.0.0","max":"2.0.0"}},"requested_caps":[{{"capability":{{"schema":"rapidlm.capability","schema_version":1,"family":"plugin","action":"invoke"}},"resource":{{"schema":"rapidlm.resource_descriptor","schema_version":1,"kind":"plugin","plugin":"acme.fmt","capability":"format"}}}}]}}"#, protocol::ArtifactId::from_bytes(bytes))).unwrap();
    (manifest, package)
}

#[test]
fn package_cli_installs_updates_and_removes_with_hash_and_trust_checks() {
    let fixture = Fixture::new();
    let catalog = fixture.0.join("store/plugin-trust.json");
    let (manifest, package) = write_package(&fixture.0, "1.0.0", b"package-one");
    let catalog = catalog.to_str().unwrap();
    let manifest = manifest.to_str().unwrap();
    let package = package.to_str().unwrap();
    let invoke = |action: &str, paths: &[&str]| {
        let mut args = vec!["plugins", "--catalog", catalog, action];
        args.extend_from_slice(paths);
        fixture.run(&args, None)
    };
    let installed = invoke("install", &[manifest, package]);
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    assert!(String::from_utf8_lossy(&installed.stdout).contains("executable_enabled=false"));
    assert_eq!(
        std::fs::read(fixture.0.join("store/installed/acme.fmt/package.bin")).unwrap(),
        b"package-one"
    );
    assert!(!invoke("install", &[manifest, package]).status.success());
    std::fs::write(package, b"wrong bytes").unwrap();
    assert!(!invoke("update", &[manifest, package]).status.success());
    assert_eq!(
        std::fs::read(fixture.0.join("store/installed/acme.fmt/package.bin")).unwrap(),
        b"package-one"
    );
    write_package(&fixture.0, "2.0.0", b"package-two");
    let updated = invoke("update", &[manifest, package]);
    assert!(
        updated.status.success(),
        "{}",
        String::from_utf8_lossy(&updated.stderr)
    );
    assert_eq!(
        std::fs::read(fixture.0.join("store/installed/acme.fmt/package.bin")).unwrap(),
        b"package-two"
    );
    let removed = invoke("remove", &["acme.fmt"]);
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
    assert!(!fixture.0.join("store/installed/acme.fmt").exists());
    let trust: serde_json::Value =
        serde_json::from_slice(&std::fs::read(catalog).unwrap()).unwrap();
    assert_eq!(trust["records"][0]["status"], "untrusted");
}

#[test]
fn package_cli_checks_policy_for_manifest_and_package_separately() {
    let fixture = Fixture::new();
    let allowed = fixture.0.join("allowed");
    let outside = fixture.0.join("outside");
    std::fs::create_dir_all(&allowed).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let (manifest, package) = write_package(&allowed, "1.0.0", b"package");
    let outside_package = outside.join("package.bin");
    std::fs::copy(&package, &outside_package).unwrap();
    let policy = fixture.0.join("policy.toml");
    let allowed = std::fs::canonicalize(allowed).unwrap();
    std::fs::write(&policy, format!("schema = \"rapidlm.managed_config.v1\"\n[policy]\n[plugins]\nallowed_sources = ['{}/*']\n", allowed.display())).unwrap();
    let catalog = fixture.0.join("store/plugin-trust.json");
    let run = |pkg: &Path| {
        fixture.run(
            &[
                "plugins",
                "--catalog",
                catalog.to_str().unwrap(),
                "install",
                manifest.to_str().unwrap(),
                pkg.to_str().unwrap(),
            ],
            Some(&policy),
        )
    };
    assert!(!run(&outside_package).status.success());
    assert!(!catalog.exists());
    let accepted = run(&package);
    assert!(
        accepted.status.success(),
        "{}",
        String::from_utf8_lossy(&accepted.stderr)
    );
}

#[test]
fn package_cli_refuses_a_catalog_locked_by_another_process() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let (manifest, package) = write_package(&fixture.0, "1.0.0", b"package");
    let store = fixture.0.join("store");
    let lock = store.join(".plugin-operation-lock");
    std::fs::create_dir_all(&lock).unwrap();
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o700)).unwrap();
    let held = workspace::multi_edit_journal::JournalStore::lock(&lock).unwrap();
    let catalog = store.join("plugin-trust.json");
    let run = || {
        fixture.run(
            &[
                "plugins",
                "--catalog",
                catalog.to_str().unwrap(),
                "install",
                manifest.to_str().unwrap(),
                package.to_str().unwrap(),
            ],
            None,
        )
    };
    let refused = run();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("busy"));
    assert!(!catalog.exists());
    drop(held);
    assert!(run().status.success());
}

#[test]
fn failed_update_with_corrupt_trust_catalog_keeps_the_old_package() {
    let fixture = Fixture::new();
    let catalog = fixture.0.join("store/plugin-trust.json");
    let (manifest, package) = write_package(&fixture.0, "1.0.0", b"original");
    let run = |action: &str| {
        fixture.run(
            &[
                "plugins",
                "--catalog",
                catalog.to_str().unwrap(),
                action,
                manifest.to_str().unwrap(),
                package.to_str().unwrap(),
            ],
            None,
        )
    };
    assert!(run("install").status.success());
    std::fs::write(&catalog, b"corrupt catalog").unwrap();
    write_package(&fixture.0, "2.0.0", b"replacement");
    let failed = run("update");
    assert!(!failed.status.success());
    assert_eq!(
        std::fs::read(fixture.0.join("store/installed/acme.fmt/package.bin")).unwrap(),
        b"original"
    );
}

#[test]
fn package_update_revokes_trust_when_the_manifest_changes() {
    let fixture = Fixture::new();
    let catalog = fixture.0.join("store/plugin-trust.json");
    let (manifest, package) = write_package(&fixture.0, "1.0.0", b"package");
    let run = |args: &[&str]| {
        let mut all = vec!["plugins", "--catalog", catalog.to_str().unwrap()];
        all.extend_from_slice(args);
        fixture.run(&all, None)
    };
    assert!(
        run(&[
            "install",
            manifest.to_str().unwrap(),
            package.to_str().unwrap()
        ])
        .status
        .success()
    );
    let approved = run(&[
        "approve",
        "acme.fmt",
        "--capability",
        "plugin.invoke",
        "--resource",
        "plugin:acme.fmt:format",
    ]);
    assert!(
        approved.status.success(),
        "{}",
        String::from_utf8_lossy(&approved.stderr)
    );
    let changed = std::fs::read_to_string(&manifest)
        .unwrap()
        .replace("plugin.wasm", "other.wasm");
    std::fs::write(&manifest, changed).unwrap();
    let updated = run(&[
        "update",
        manifest.to_str().unwrap(),
        package.to_str().unwrap(),
    ]);
    assert!(
        updated.status.success(),
        "{}",
        String::from_utf8_lossy(&updated.stderr)
    );
    let trust: serde_json::Value =
        serde_json::from_slice(&std::fs::read(catalog).unwrap()).unwrap();
    assert_eq!(trust["records"][0]["status"], "untrusted");
    assert!(trust["records"][0]["manifest_hash"].as_str().is_some());
}

#[test]
fn manifest_registration_revokes_changed_grants_and_legacy_records_require_review() {
    let fixture = Fixture::new();
    let catalog = fixture.0.join("store/plugin-trust.json");
    let (manifest, _) = write_package(&fixture.0, "1.0.0", b"package");
    let run = |args: &[&str]| {
        let mut all = vec!["plugins", "--catalog", catalog.to_str().unwrap()];
        all.extend_from_slice(args);
        fixture.run(&all, None)
    };
    let register = || run(&["register", manifest.to_str().unwrap()]);
    let approve = || {
        run(&[
            "approve",
            "acme.fmt",
            "--capability",
            "plugin.invoke",
            "--resource",
            "plugin:acme.fmt:format",
            "--manifest",
            manifest.to_str().unwrap(),
        ])
    };
    assert!(register().status.success());
    let no_manifest = run(&[
        "approve",
        "acme.fmt",
        "--capability",
        "plugin.invoke",
        "--resource",
        "plugin:acme.fmt:format",
    ]);
    assert!(!no_manifest.status.success());
    assert!(String::from_utf8_lossy(&no_manifest.stderr).contains("supply --manifest"));
    assert!(approve().status.success());
    let changed = std::fs::read_to_string(&manifest)
        .unwrap()
        .replace("plugin.wasm", "other.wasm");
    std::fs::write(&manifest, changed).unwrap();
    assert!(register().status.success());
    let trust: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&catalog).unwrap()).unwrap();
    assert_eq!(trust["records"][0]["status"], "untrusted");
    assert_eq!(
        trust["records"][0]["granted_capabilities"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert!(approve().status.success());
    let mut legacy: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&catalog).unwrap()).unwrap();
    legacy["records"][0]
        .as_object_mut()
        .unwrap()
        .remove("manifest_hash");
    std::fs::write(&catalog, serde_json::to_vec(&legacy).unwrap()).unwrap();
    let refused = approve();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("register the manifest again"));
    assert!(register().status.success());
    let rebound: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&catalog).unwrap()).unwrap();
    assert_eq!(rebound["records"][0]["status"], "untrusted");
    assert!(approve().status.success());
}

#[test]
fn package_review_shows_requested_scopes_and_rejects_unrequested_grants() {
    let fixture = Fixture::new();
    let catalog = fixture.0.join("store/plugin-trust.json");
    let (manifest, package) = write_package(&fixture.0, "1.0.0", b"package");
    let run = |args: &[&str]| {
        let mut all = vec!["plugins", "--catalog", catalog.to_str().unwrap()];
        all.extend_from_slice(args);
        fixture.run(&all, None)
    };
    assert!(
        run(&[
            "install",
            manifest.to_str().unwrap(),
            package.to_str().unwrap()
        ])
        .status
        .success()
    );
    let trust_before = std::fs::read(&catalog).unwrap();
    let inspect = run(&["inspect", "acme.fmt"]);
    assert!(
        inspect.status.success(),
        "{}",
        String::from_utf8_lossy(&inspect.stderr)
    );
    let text = String::from_utf8(inspect.stdout).unwrap();
    assert!(
        text.contains("trust_binding=matched")
            && text.contains("manifest_hash=sha256:")
            && text.contains("requested capability=plugin.invoke")
            && text.contains("\"capability\":\"format\""),
        "{text}"
    );
    assert_eq!(std::fs::read(&catalog).unwrap(), trust_before);
    let refused = run(&[
        "approve",
        "acme.fmt",
        "--capability",
        "fs.read",
        "--resource",
        "repo:src/**",
    ]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("does not request"));
    assert_eq!(std::fs::read(&catalog).unwrap(), trust_before);
    let mismatch = std::fs::read_to_string(&manifest)
        .unwrap()
        .replace("plugin.wasm", "changed.wasm");
    let installed_manifest = fixture.0.join("store/installed/acme.fmt/manifest.json");
    std::fs::write(&installed_manifest, mismatch).unwrap();
    let review = run(&["inspect", "acme.fmt"]);
    assert!(review.status.success());
    assert!(String::from_utf8_lossy(&review.stdout).contains("trust_binding=mismatch"));
    let denied = run(&[
        "approve",
        "acme.fmt",
        "--capability",
        "plugin.invoke",
        "--resource",
        "plugin:acme.fmt:format",
        "--manifest",
        manifest.to_str().unwrap(),
    ]);
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("do not match"));
    assert_eq!(std::fs::read(&catalog).unwrap(), trust_before);
}

#[test]
fn package_inspection_refuses_pending_rollback_without_recovery_writes() {
    let fixture = Fixture::new();
    let catalog = fixture.0.join("store/plugin-trust.json");
    let (manifest, package) = write_package(&fixture.0, "1.0.0", b"package");
    let run = |args: &[&str]| {
        let mut all = vec!["plugins", "--catalog", catalog.to_str().unwrap()];
        all.extend_from_slice(args);
        fixture.run(&all, None)
    };
    assert!(
        run(&[
            "install",
            manifest.to_str().unwrap(),
            package.to_str().unwrap()
        ])
        .status
        .success()
    );
    let rollback = fixture.0.join("store/rollback/acme.fmt");
    std::fs::create_dir_all(&rollback).unwrap();
    std::fs::write(rollback.join("sentinel"), "keep").unwrap();
    let result = run(&["inspect", "acme.fmt"]);
    assert!(!result.status.success());
    assert_eq!(
        std::fs::read_to_string(rollback.join("sentinel")).unwrap(),
        "keep"
    );
}

#[test]
fn manifest_only_approval_rejects_a_changed_or_unrequested_manifest() {
    let fixture = Fixture::new();
    let catalog = fixture.0.join("store/plugin-trust.json");
    let (manifest, _) = write_package(&fixture.0, "1.0.0", b"package");
    let run = |args: &[&str]| {
        let mut all = vec!["plugins", "--catalog", catalog.to_str().unwrap()];
        all.extend_from_slice(args);
        fixture.run(&all, None)
    };
    assert!(!run(&["inspect", "acme.fmt"]).status.success());
    assert!(!fixture.0.join("store").exists());
    assert!(
        run(&["register", manifest.to_str().unwrap()])
            .status
            .success()
    );
    let before = std::fs::read(&catalog).unwrap();
    let modified = std::fs::read_to_string(&manifest)
        .unwrap()
        .replace("plugin.wasm", "other.wasm");
    std::fs::write(&manifest, modified).unwrap();
    let result = run(&[
        "approve",
        "acme.fmt",
        "--manifest",
        manifest.to_str().unwrap(),
        "--capability",
        "plugin.invoke",
        "--resource",
        "plugin:acme.fmt:format",
    ]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("does not match"));
    assert_eq!(std::fs::read(&catalog).unwrap(), before);
    write_package(&fixture.0, "1.0.0", b"package");
    let result = run(&[
        "approve",
        "acme.fmt",
        "--manifest",
        manifest.to_str().unwrap(),
        "--capability",
        "fs.read",
        "--resource",
        "repo:src/**",
    ]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("does not request"));
    assert_eq!(std::fs::read(&catalog).unwrap(), before);
}

#[test]
fn approval_removes_stale_grants_not_requested_by_the_current_manifest() {
    let fixture = Fixture::new();
    let catalog = fixture.0.join("store/plugin-trust.json");
    let (manifest, package) = write_package(&fixture.0, "1.0.0", b"package");
    let run = |args: &[&str]| {
        let mut all = vec!["plugins", "--catalog", catalog.to_str().unwrap()];
        all.extend_from_slice(args);
        fixture.run(&all, None)
    };
    assert!(
        run(&[
            "install",
            manifest.to_str().unwrap(),
            package.to_str().unwrap()
        ])
        .status
        .success()
    );
    let approve = || {
        run(&[
            "approve",
            "acme.fmt",
            "--capability",
            "plugin.invoke",
            "--resource",
            "plugin:acme.fmt:format",
        ])
    };
    assert!(approve().status.success());
    let mut trust: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&catalog).unwrap()).unwrap();
    trust["records"][0]["granted_capabilities"].as_array_mut().unwrap().push(serde_json::json!({
        "capability":{"schema":"rapidlm.capability","schema_version":1,"family":"fs","action":"read"},
        "resource":{"schema":"rapidlm.resource_descriptor","schema_version":1,"kind":"filesystem","root":"repo","glob":"src/**"}
    }));
    std::fs::write(&catalog, serde_json::to_vec(&trust).unwrap()).unwrap();
    assert!(approve().status.success());
    let updated: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&catalog).unwrap()).unwrap();
    let grants = updated["records"][0]["granted_capabilities"]
        .as_array()
        .unwrap();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0]["capability"]["family"], "plugin");
}

#[test]
fn signed_identity_requires_verification_before_cli_approval() {
    let fixture = Fixture::new();
    let catalog = fixture.0.join("store/plugin-trust.json");
    let (manifest, package) = write_package(&fixture.0, "1.0.0", b"package");
    let run = |args: &[&str]| {
        let mut all = vec!["plugins", "--catalog", catalog.to_str().unwrap()];
        all.extend_from_slice(args);
        fixture.run(&all, None)
    };
    assert!(
        run(&[
            "install",
            manifest.to_str().unwrap(),
            package.to_str().unwrap()
        ])
        .status
        .success()
    );
    let mut trust: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&catalog).unwrap()).unwrap();
    trust["records"][0]["signature"] = serde_json::json!(
        "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
    );
    std::fs::write(&catalog, serde_json::to_vec(&trust).unwrap()).unwrap();
    let before = std::fs::read(&catalog).unwrap();
    let refused = run(&[
        "approve",
        "acme.fmt",
        "--capability",
        "plugin.invoke",
        "--resource",
        "plugin:acme.fmt:format",
    ]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("requires signature verification"));
    assert_eq!(std::fs::read(&catalog).unwrap(), before);
    let inspected = run(&["inspect", "acme.fmt"]);
    assert!(inspected.status.success());
    assert!(String::from_utf8_lossy(&inspected.stdout).contains("trust_binding=mismatch"));
}

#[test]
fn package_enable_disable_preserves_trust_and_disabled_state_across_updates() {
    let fixture = Fixture::new();
    let catalog = fixture.0.join("store/plugin-trust.json");
    let (manifest, package) = write_package(&fixture.0, "1.0.0", b"package-one");
    let run = |args: &[&str]| {
        let mut all = vec!["plugins", "--catalog", catalog.to_str().unwrap()];
        all.extend_from_slice(args);
        fixture.run(&all, None)
    };
    assert!(
        run(&[
            "install",
            manifest.to_str().unwrap(),
            package.to_str().unwrap()
        ])
        .status
        .success()
    );
    assert!(
        run(&[
            "approve",
            "acme.fmt",
            "--capability",
            "plugin.invoke",
            "--resource",
            "plugin:acme.fmt:format"
        ])
        .status
        .success()
    );
    let initial = std::fs::read(&catalog).unwrap();
    let disabled = run(&["disable", "acme.fmt"]);
    assert!(disabled.status.success());
    assert!(String::from_utf8_lossy(&disabled.stdout).contains("package_enabled=false"));
    assert_eq!(std::fs::read(&catalog).unwrap(), initial);
    let inspect = run(&["inspect", "acme.fmt"]);
    let text = String::from_utf8(inspect.stdout).unwrap();
    assert!(
        text.contains("status=trusted")
            && text.contains("package_enabled=false")
            && text.contains("executable_enabled=false"),
        "{text}"
    );
    assert!(run(&["enable", "acme.fmt"]).status.success());
    assert_eq!(std::fs::read(&catalog).unwrap(), initial);
    let text = String::from_utf8(run(&["inspect", "acme.fmt"]).stdout).unwrap();
    assert!(
        text.contains("package_enabled=true") && text.contains("executable_enabled=true"),
        "{text}"
    );
    assert!(run(&["disable", "acme.fmt"]).status.success());
    write_package(&fixture.0, "2.0.0", b"package-two");
    assert!(
        run(&[
            "update",
            manifest.to_str().unwrap(),
            package.to_str().unwrap()
        ])
        .status
        .success()
    );
    let text = String::from_utf8(run(&["inspect", "acme.fmt"]).stdout).unwrap();
    assert!(
        text.contains("status=untrusted")
            && text.contains("package_enabled=false")
            && text.contains("executable_enabled=false"),
        "{text}"
    );
    assert!(run(&["enable", "acme.fmt"]).status.success());
    let text = String::from_utf8(run(&["inspect", "acme.fmt"]).stdout).unwrap();
    assert!(
        text.contains("status=untrusted")
            && text.contains("package_enabled=true")
            && text.contains("executable_enabled=false"),
        "{text}"
    );
}

#[test]
fn skill_bundle_is_validated_and_content_bound_before_publication() {
    let fixture = Fixture::new();
    let catalog = fixture.0.join("store/plugin-trust.json");
    let value = serde_json::json!({"schema":"rapidlm.plugin_bundle","schema_version":1,"skills":[{"id":"fmt","skill_md":"---\nname: fmt\ndescription: Format sources\nresources:\n  - guide.md\n---\nFormat safely.\n","resources":[{"path":"guide.md","text":"Use the project formatter."}]}]});
    let mut bytes = plugin_host::bundle::BUNDLE_MAGIC.to_vec();
    bytes.extend(serde_json::to_vec(&value).unwrap());
    let (manifest, package) = write_package(&fixture.0, "1.0.0", &bytes);
    let mut plugin: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
    plugin["skills"] = serde_json::json!(["fmt"]);
    std::fs::write(&manifest, serde_json::to_vec(&plugin).unwrap()).unwrap();
    let run = |args: &[&str]| {
        let mut all = vec!["plugins", "--catalog", catalog.to_str().unwrap()];
        all.extend_from_slice(args);
        fixture.run(&all, None)
    };
    let installed = run(&[
        "install",
        manifest.to_str().unwrap(),
        package.to_str().unwrap(),
    ]);
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let report = run(&["inspect", "acme.fmt"]);
    let text = String::from_utf8(report.stdout).unwrap();
    assert!(
        report.status.success()
            && text.contains("package_format=skill-bundle-v1")
            && text.contains("contribution skill=fmt"),
        "{text}"
    );
    let original = std::fs::read(fixture.0.join("store/installed/acme.fmt/package.bin")).unwrap();
    let mut bad = value;
    bad["skills"][0]["resources"] = serde_json::json!([]);
    let mut invalid = plugin_host::bundle::BUNDLE_MAGIC.to_vec();
    invalid.extend(serde_json::to_vec(&bad).unwrap());
    std::fs::write(&package, &invalid).unwrap();
    plugin["digest"] = serde_json::json!(protocol::ArtifactId::from_bytes(&invalid).to_string());
    plugin["version"] = serde_json::json!("2.0.0");
    std::fs::write(&manifest, serde_json::to_vec(&plugin).unwrap()).unwrap();
    let update = run(&[
        "update",
        manifest.to_str().unwrap(),
        package.to_str().unwrap(),
    ]);
    assert!(!update.status.success());
    assert!(String::from_utf8_lossy(&update.stderr).contains("resources do not match"));
    assert_eq!(
        std::fs::read(fixture.0.join("store/installed/acme.fmt/package.bin")).unwrap(),
        original
    );
}

#[test]
fn approved_skill_bundle_is_visible_in_the_real_skill_cli_and_obeys_policy() {
    let fixture = Fixture::new();
    std::fs::create_dir_all(fixture.0.join(".rapidlm")).unwrap();
    assert!(
        Command::new("git")
            .arg("init")
            .arg("-q")
            .current_dir(&fixture.0)
            .status()
            .unwrap()
            .success()
    );
    let bundle = serde_json::json!({"schema":"rapidlm.plugin_bundle","schema_version":1,"skills":[{"id":"bundlefmt","skill_md":"---\nname: bundlefmt\ndescription: Format bundled sources\nresources:\n  - guide.md\n---\nUse the bundled formatter instructions.\n","resources":[{"path":"guide.md","text":"Bundled guide."}]}]});
    let mut bytes = plugin_host::bundle::BUNDLE_MAGIC.to_vec();
    bytes.extend(serde_json::to_vec(&bundle).unwrap());
    let (manifest, package) = write_package(&fixture.0, "1.0.0", &bytes);
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
    doc["skills"] = serde_json::json!(["bundlefmt"]);
    std::fs::write(&manifest, serde_json::to_vec(&doc).unwrap()).unwrap();
    let call = |args: &[&str]| fixture.run(args, None);
    assert!(call(&["trust", "grant"]).status.success());
    assert!(
        call(&[
            "plugins",
            "install",
            manifest.to_str().unwrap(),
            package.to_str().unwrap()
        ])
        .status
        .success()
    );
    let before = call(&["skills", "list"]);
    assert!(!String::from_utf8_lossy(&before.stdout).contains("bundlefmt"));
    let review = call(&["plugins", "skill", "acme.fmt", "bundlefmt"]);
    assert!(review.status.success());
    assert!(
        String::from_utf8_lossy(&review.stdout).contains("Use the bundled formatter instructions")
    );
    // An old untrusted record may still contain grants. Content approval must
    // not revive those grants, even when the manifest still requests them.
    assert!(
        call(&[
            "plugins",
            "approve",
            "acme.fmt",
            "--capability",
            "plugin.invoke",
            "--resource",
            "plugin:acme.fmt:format"
        ])
        .status
        .success()
    );
    let trust_path = fixture.0.join(".rapidlm/plugins/plugin-trust.json");
    let mut stale: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&trust_path).unwrap()).unwrap();
    assert_eq!(
        stale["records"][0]["granted_capabilities"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    stale["records"][0]["status"] = serde_json::json!("untrusted");
    std::fs::write(&trust_path, serde_json::to_vec(&stale).unwrap()).unwrap();
    let approved = call(&["plugins", "approve", "acme.fmt", "--content"]);
    assert!(
        approved.status.success(),
        "{}",
        String::from_utf8_lossy(&approved.stderr)
    );
    let listing = call(&["skills", "list"]);
    assert!(listing.status.success());
    assert!(
        String::from_utf8_lossy(&listing.stdout).contains("bundlefmt  plugin"),
        "{}",
        String::from_utf8_lossy(&listing.stdout)
    );
    let shown = call(&["skills", "show", "bundlefmt"]);
    assert!(
        shown.status.success(),
        "{}",
        String::from_utf8_lossy(&shown.stderr)
    );
    assert!(
        String::from_utf8_lossy(&shown.stdout).contains("Use the bundled formatter instructions")
    );
    let trust: serde_json::Value = serde_json::from_slice(
        &std::fs::read(fixture.0.join(".rapidlm/plugins/plugin-trust.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(trust["records"][0]["status"], "trusted");
    assert!(
        trust["records"][0]["granted_capabilities"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(call(&["plugins", "disable", "acme.fmt"]).status.success());
    assert!(!String::from_utf8_lossy(&call(&["skills", "list"]).stdout).contains("bundlefmt"));
    assert!(call(&["plugins", "enable", "acme.fmt"]).status.success());
    let policy = fixture.0.join("managed.toml");
    std::fs::write(
        &policy,
        "schema = \"rapidlm.managed_config.v1\"\n[policy]\n[skills]\nproject_skills = false\n",
    )
    .unwrap();
    let policy_list = fixture.run(&["skills", "list"], Some(&policy));
    assert!(!String::from_utf8_lossy(&policy_list.stdout).contains("bundlefmt"));
    assert!(call(&["plugins", "reject", "acme.fmt"]).status.success());
    assert!(!String::from_utf8_lossy(&call(&["skills", "list"]).stdout).contains("bundlefmt"));
}
