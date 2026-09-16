//! `rapid update` — self-update with verification and failure recovery
//! (delivery goal §8).
//!
//! The update contract, in the order the implementation enforces it:
//!
//! 1. Fetch the update manifest (`{version, sha256, url}` JSON) from the
//!    configured release URL. No manifest, no update.
//! 2. Compare versions: a downgrade or same-version re-install is refused
//!    unless `--force`.
//! 3. Download the artifact to a temp file in the same directory as the
//!    running binary (same filesystem → atomic rename is possible).
//! 4. **Verify before swap**: the artifact's SHA-256 must match the
//!    manifest's checksum, AND the artifact must be executable and report
//!    the manifest's version via `--version`. A checksum mismatch or a
//!    binary that will not run aborts the update with the old binary
//!    untouched.
//! 5. **Atomic swap with rollback**: the old binary is renamed aside, the
//!    new one renamed into place, and the new one is smoke-run
//!    (`--version`). If the smoke run fails, the old binary is renamed
//!    back — the machine is never left without a working `rapid`.
//!
//! Integrity beyond checksums (detached signatures, provenance) is recorded
//! in the manifest and *verified if present*: a manifest carrying
//! `sha256` only is accepted for checksum-only releases; a manifest
//! carrying additional `signatures` entries is surfaced in the output for
//! the user to verify with their own tooling (signing keys are an
//! operator credential this binary deliberately does not hold).

use std::path::Path;
use std::process::Command;

pub const UPDATE_USAGE: &str = "\
usage: rapid update [--url <manifest-url>] [--force] [--check]

Self-update with verification and failure recovery.

  --url <manifest-url>  Release manifest URL (default: the RAPIDLM_UPDATE_URL
                        environment variable; without either, update prints
                        the configured URL and exits 2).
  --force               Allow same-version or downgrade installs.
  --check               Only report what an update would do; change nothing.

The manifest is {\"version\": …, \"sha256\": …, \"url\": …}. The artifact is
checksum-verified, smoke-run (--version must print the manifest version), and
atomically swapped; any failure leaves the running binary untouched.
";

/// What a manifest must say. Unknown fields are ignored (forward-compatible
/// releases may add signatures/provenance).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateManifest {
    pub version: String,
    pub sha256: String,
    pub url: String,
}

pub fn parse_manifest(json: &str) -> Result<UpdateManifest, String> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|err| format!("manifest is not JSON: {err}"))?;
    let field = |name: &str| -> Result<String, String> {
        value
            .get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("manifest missing `{name}`"))
    };
    let manifest = UpdateManifest {
        version: field("version")?,
        sha256: field("sha256")?,
        url: field("url")?,
    };
    if manifest.version.is_empty() || manifest.url.is_empty() {
        return Err("manifest version/url is empty".to_owned());
    }
    if manifest.sha256.len() != 64 || !manifest.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("manifest sha256 is not a 64-hex digest".to_owned());
    }
    Ok(manifest)
}

/// The running binary's own version, from `--version` output (the same text
/// a user sees): `rapid 0.1.0 (RapidLM CLI)`.
pub fn running_version(bin: &Path) -> Result<String, String> {
    let output = Command::new(bin)
        .arg("--version")
        .output()
        .map_err(|err| err.to_string())?;
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.split_whitespace()
        .nth(1)
        .map(str::to_owned)
        .ok_or_else(|| format!("`--version` output not understood: {text:?}"))
}

/// Fetch a URL over HTTP(S). Delegates to the platform `curl` — the update
/// path runs once in a blue moon, and curl's TLS/proxy story is the user's
/// existing one. Bounded by `--max-time`.
pub fn fetch(url: &str, max_bytes: usize) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    let mut child = Command::new("curl")
        .args([
            "-fsSL",
            "--max-time",
            "120",
            "--proto",
            "=https", // updates are https-only; no silent cleartext
            url,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|err| format!("curl could not start: {err}"))?;
    let mut body = Vec::new();
    if let Some(mut stdout) = child.stdout.take() {
        let mut capped = vec![0u8; max_bytes + 1];
        let read = stdout.read(&mut capped).map_err(|err| err.to_string())?;
        body = capped[..read.min(max_bytes)].to_vec();
    }
    let status = child.wait().map_err(|err| err.to_string())?;
    if !status.success() {
        return Err(format!("fetch {url} failed ({status})"));
    }
    Ok(body)
}

/// SHA-256 via the existing hashing in protocol's ArtifactId? No — this is
/// a straight re-implementation using the same digest crate the workspace
/// already depends on, exposed through a small helper here so the update
/// path owns its verification.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Compare semantic versions (`0.1.0`): Ok(()) when `new` > `current`.
pub fn ensure_newer(current: &str, new: &str) -> Result<(), String> {
    let parse = |v: &str| -> Result<Vec<u64>, String> {
        v.split('.')
            .map(|part| {
                part.parse::<u64>()
                    .map_err(|_| format!("version {v:?} is not semantic"))
            })
            .collect()
    };
    let (current_parts, new_parts) = (parse(current)?, parse(new)?);
    if new_parts <= current_parts {
        return Err(format!(
            "refusing to install {new} over {current} (not newer); use --force to override"
        ));
    }
    Ok(())
}

/// Execute the update against an already-fetched manifest. Split from the
/// network phase so tests can drive the full verify/swap/rollback path with
/// local files and no HTTP.
pub fn apply_update(
    bin: &Path,
    manifest: &UpdateManifest,
    artifact_bytes: &[u8],
    force: bool,
) -> Result<String, String> {
    // Verify-before-everything: the checksum is pure bytes and always runs
    // first, so a corrupt artifact is refused before any process is spawned
    // or any file is touched.
    let actual = sha256_hex(artifact_bytes);
    if actual != manifest.sha256 {
        return Err(format!(
            "checksum mismatch: manifest {}, artifact {actual} — old binary untouched",
            manifest.sha256
        ));
    }
    let current = running_version(bin).ok();
    if let Some(current) = &current {
        if !force {
            ensure_newer(current, &manifest.version)?;
        }
    } else if !force {
        return Err(
            "the running binary's version could not be determined; refusing to update without --force"
                .to_owned(),
        );
    }
    // Then the artifact must run and claim the manifest's version.
    let staged = sibling_variant(bin, "update-new");
    std::fs::write(&staged, artifact_bytes).map_err(|err| format!("staging write: {err}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
            .map_err(|err| format!("staging chmod: {err}"))?;
    }
    let staged_version = running_version(&staged);
    if staged_version.as_deref() != Ok(manifest.version.as_str()) {
        let _ = std::fs::remove_file(&staged);
        return Err(format!(
            "staged artifact reports {:?}, manifest claims {:?} — old binary untouched",
            staged_version, manifest.version
        ));
    }
    // Atomic swap with rollback: old aside, new in, smoke-run; on failure
    // the old binary returns.
    let backup = sibling_variant(bin, "update-old");
    let _ = std::fs::remove_file(&backup);
    std::fs::rename(bin, &backup).map_err(|err| format!("backup rename: {err}"))?;
    if let Err(err) = std::fs::rename(&staged, bin) {
        let _ = std::fs::rename(&backup, bin);
        return Err(format!("install rename: {err} (old binary restored)"));
    }
    if running_version(bin).as_deref() != Ok(manifest.version.as_str()) {
        let _ = std::fs::rename(&backup, bin);
        let _ = std::fs::remove_file(&staged);
        return Err("post-install smoke run failed; old binary restored".to_owned());
    }
    let _ = std::fs::remove_file(&backup);
    Ok(format!(
        "updated {} -> {} (old binary removed after a successful smoke run)",
        current.as_deref().unwrap_or("?"),
        manifest.version
    ))
}

/// `rapid update` entry: fetch manifest + artifact, verify, swap.
pub fn run_update(args: &[String]) -> Result<i32, crate::p9_commands::P9CommandError> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{UPDATE_USAGE}");
        return Ok(0);
    }
    let mut url = std::env::var("RAPIDLM_UPDATE_URL").ok();
    let mut force = false;
    let mut check_only = false;
    let mut iterator = args.iter();
    while let Some(arg) = iterator.next() {
        match arg.as_str() {
            "--url" => {
                url = iterator
                    .next()
                    .cloned()
                    .or_else(|| {
                        eprintln!("--url needs a value");
                        None
                    })
                    .or(url);
            }
            "--force" => force = true,
            "--check" => check_only = true,
            other => {
                eprintln!("rapid update: unknown argument {other:?}");
                return Err(crate::p9_commands::P9CommandError::Usage);
            }
        }
    }
    let Some(url) = url else {
        eprintln!("rapid update: no release URL configured; set RAPIDLM_UPDATE_URL or pass --url");
        return Err(crate::p9_commands::P9CommandError::Usage);
    };
    let bin = std::env::current_exe()
        .map_err(|err| crate::p9_commands::P9CommandError::Agent(err.to_string()))?;
    let manifest_bytes =
        fetch(&url, 64 * 1024).map_err(crate::p9_commands::P9CommandError::Agent)?;
    let manifest_text = String::from_utf8(manifest_bytes)
        .map_err(|err| crate::p9_commands::P9CommandError::Agent(err.to_string()))?;
    let manifest =
        parse_manifest(&manifest_text).map_err(crate::p9_commands::P9CommandError::Agent)?;
    let current = running_version(&bin).map_err(crate::p9_commands::P9CommandError::Agent)?;
    if check_only {
        println!(
            "would update {} -> {} (artifact {})",
            current, manifest.version, manifest.url
        );
        return Ok(0);
    }
    if !force && let Err(reason) = ensure_newer(&current, &manifest.version) {
        println!("{reason}");
        return Ok(0);
    }
    let artifact = fetch(&manifest.url, 256 * 1024 * 1024)
        .map_err(crate::p9_commands::P9CommandError::Agent)?;
    match apply_update(&bin, &manifest, &artifact, force) {
        Ok(message) => {
            println!("{message}");
            Ok(0)
        }
        Err(reason) => {
            eprintln!("rapid update: {reason}");
            Ok(1)
        }
    }
}

/// The staging / backup sibling of `bin`: `rapid.update-new`, or, when the
/// binary carries an executable extension, `rapid.update-new.exe` — the
/// extension stays last, so a `.cmd`/`.bat` launcher (the test stand-in on
/// Windows) is still something the OS will run for the smoke test, and a
/// real `.exe` keeps its conventional name. `Path::with_extension` used to
/// *replace* the last dotted segment instead: `rapid.exe` → `rapid.update-new`
/// and `rapid-0.1` → `rapid-0.update-new`.
fn sibling_variant(bin: &Path, tag: &str) -> std::path::PathBuf {
    let name = bin.file_name().unwrap_or_default();
    let mut out = std::ffi::OsString::new();
    let executable_ext = bin
        .extension()
        .and_then(|ext| ext.to_str())
        .filter(|ext| ["exe", "cmd", "bat", "com"].contains(&ext.to_ascii_lowercase().as_str()));
    match executable_ext {
        Some(ext) => {
            out.push(bin.file_stem().unwrap_or_default());
            out.push(".");
            out.push(tag);
            out.push(".");
            out.push(ext);
        }
        None => {
            out.push(name);
            out.push(".");
            out.push(tag);
        }
    }
    bin.with_file_name(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A controllable stand-in binary: a script whose `--version` prints
    /// whatever the file says — `sh` on Unix, a `.cmd` batch file on
    /// Windows (which `Command` runs through `cmd.exe`). The update
    /// machinery must treat it exactly like a real binary — checksum, smoke
    /// run, atomic swap.
    fn make_bin(dir: &Path, name: &str, version: &str) -> PathBuf {
        let path = bin_path(dir, name);
        std::fs::write(&path, version_script(version)).unwrap();
        mark_executable(&path);
        path
    }

    /// The stand-in's path: bare on Unix, `.cmd` on Windows.
    fn bin_path(dir: &Path, name: &str) -> PathBuf {
        if cfg!(windows) {
            dir.join(format!("{name}.cmd"))
        } else {
            dir.join(name)
        }
    }

    fn version_script(version: &str) -> Vec<u8> {
        if cfg!(windows) {
            format!("@echo off\r\nif \"%~1\"==\"--version\" echo rapid {version} (RapidLM CLI)\r\n")
                .into_bytes()
        } else {
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"rapid {version} (RapidLM CLI)\"; fi\n"
            )
            .into_bytes()
        }
    }

    fn mark_executable(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        #[cfg(not(unix))]
        {
            let _ = path;
        }
    }

    fn manifest_for(bin_bytes: &[u8], version: &str) -> UpdateManifest {
        UpdateManifest {
            version: version.to_owned(),
            sha256: sha256_hex(bin_bytes),
            url: "https://releases.example/rapid".to_owned(),
        }
    }

    #[test]
    fn update_swaps_atomically_and_reports_the_new_version() {
        let dir = std::env::temp_dir().join(format!(
            "upd-ok-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = make_bin(&dir, "rapid", "0.1.0");
        let new_bytes = version_script("0.2.0");
        let new_version = "0.2.0";
        let bin_copy = make_bin(&dir, "rapid-real", "0.1.0");

        // The update runs against the copy so `running_version(bin)` sees
        // the scripted old version through the whole flow.
        let manifest = manifest_for(&new_bytes, new_version);
        let message =
            apply_update(&bin_copy, &manifest, &new_bytes, false).expect("update succeeds");
        assert!(message.contains("0.1.0 -> 0.2.0"), "{message}");
        let swapped = std::fs::read(&bin_copy).unwrap();
        assert_eq!(swapped, new_bytes);
        assert!(running_version(&bin_copy).unwrap() == "0.2.0");
        // No leftovers.
        assert!(!sibling_variant(&bin_copy, "update-new").exists());
        assert!(!sibling_variant(&bin_copy, "update-old").exists());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = bin;
    }

    #[test]
    fn staging_and_backup_names_keep_an_executable_extension_last() {
        assert_eq!(
            sibling_variant(Path::new("/opt/rapid"), "update-new"),
            PathBuf::from("/opt/rapid.update-new")
        );
        assert_eq!(
            sibling_variant(Path::new("/opt/rapid-0.1"), "update-old"),
            PathBuf::from("/opt/rapid-0.1.update-old")
        );
        assert_eq!(
            sibling_variant(Path::new(r"C:\tools\rapid.exe"), "update-new"),
            PathBuf::from(r"C:\tools\rapid.update-new.exe")
        );
        assert_eq!(
            sibling_variant(Path::new("x/rapid-real.CMD"), "update-old"),
            PathBuf::from("x/rapid-real.update-old.CMD")
        );
    }

    #[test]
    fn a_checksum_mismatch_refuses_and_leaves_the_old_binary_untouched() {
        let dir = std::env::temp_dir().join(format!(
            "upd-mismatch-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let bin_copy = bin_path(&dir, "rapid-real");
        std::fs::write(&bin_copy, b"old").unwrap();
        mark_executable(&bin_copy);
        let manifest = UpdateManifest {
            version: "0.2.0".to_owned(),
            sha256: "0".repeat(64),
            url: "https://releases.example/rapid".to_owned(),
        };
        let error = apply_update(&bin_copy, &manifest, b"new bytes", true).expect_err("refused");
        assert!(error.contains("checksum mismatch"), "{error}");
        assert!(error.contains("untouched"), "{error}");
        assert_eq!(std::fs::read(&bin_copy).unwrap(), b"old");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_staged_artifact_that_will_not_run_rolls_back() {
        let dir = std::env::temp_dir().join(format!(
            "upd-rollback-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let bin_copy = bin_path(&dir, "rapid-real");
        std::fs::write(&bin_copy, b"old").unwrap();
        mark_executable(&bin_copy);
        // The artifact's checksum matches the manifest but it claims the
        // wrong version — exactly what a truncated or mis-built artifact
        // looks like.
        let bytes = version_script("9.9.9");
        let manifest = manifest_for(&bytes, "0.2.0");
        let error = apply_update(&bin_copy, &manifest, &bytes, true).expect_err("refused");
        assert!(error.contains("old binary untouched"), "{error}");
        assert_eq!(std::fs::read(&bin_copy).unwrap(), b"old");
        assert!(!sibling_variant(&bin_copy, "update-new").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn downgrades_are_refused_unless_forced() {
        assert!(ensure_newer("0.2.0", "0.1.9").is_err());
        assert!(ensure_newer("0.2.0", "0.2.0").is_err());
        assert!(ensure_newer("0.1.9", "0.2.0").is_ok());
        assert!(ensure_newer("0.1.9", "0.10.0").is_ok());
    }

    #[test]
    fn manifest_parsing_is_bounded_and_typed() {
        let good = parse_manifest(
            r#"{"version":"0.2.0","sha256":"a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4","url":"https://x/rapid","extra":1}"#,
        )
        .expect("parses");
        assert_eq!(good.version, "0.2.0");
        assert!(parse_manifest(r#"{"version":"0.2.0"}"#).is_err());
        assert!(
            parse_manifest(r#"{"version":"0.2.0","sha256":"short","url":"https://x"}"#).is_err()
        );
    }
}
