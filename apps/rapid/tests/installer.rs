//! The installer script, run against a release laid out on local disk.
//!
//! `scripts/test-install.sh` builds a fake release (an archive, its checksum)
//! and drives `scripts/install.sh` through the happy path, an upgrade, a bad or
//! malformed checksum, a checksum for another file, a missing release, a
//! symlinked binary, hostile version strings and an unsupported system. It is a
//! POSIX shell script, so it runs where there is a shell; Windows has its own
//! installer.

#![cfg(unix)]

use std::path::Path;
use std::process::Command;

#[test]
fn the_installer_script_passes_its_own_tests() {
    let scripts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts");
    let output = Command::new("sh")
        .arg(scripts.join("test-install.sh"))
        .output()
        .expect("sh runs");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stdout:\n{text}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(text.contains("all passed"), "{text}");
    assert!(!text.contains("FAIL"), "{text}");
}
