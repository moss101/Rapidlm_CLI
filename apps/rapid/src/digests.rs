//! Real identities for verification evidence (GVS-007).
//!
//! Before this, a goal claim's `WorkspaceIdentity` was a hash of the *goal
//! snapshot* — the statement and criteria the user typed. Two different
//! trees with the same goal hashed identically, so the supervisor's
//! stale-workspace gate could not see a code change at all, and
//! `require_workspace_identity` was left off because turning it on would
//! have compared a constant against itself.
//!
//! The three digests here are what verification is actually *about*:
//!
//! - [`workspace_digest`] — the candidate: what the tree contains right now,
//!   tracked content plus untracked non-ignored files, from git's own object
//!   hashing. Changing any byte of any file changes it.
//! - [`check_definition_digest`] — the test: the command that was run and the
//!   content of the files it names, so editing a test changes the identity of
//!   the check that cited it.
//! - [`environment_digest`] — the environment: the toolchain versions a
//!   result depended on, so an upgraded compiler does not silently inherit
//!   yesterday's green run.
//!
//! Each is a bounded, deterministic string. Every one fails *closed*: when a
//! digest cannot be computed it returns a distinct `unavailable:` value
//! rather than a constant that would compare equal to some other tree. Two
//! unavailable digests never compare equal to each other either, so an
//! uncomputable identity can never be mistaken for a matching one.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use protocol::ArtifactId;

/// Bound on any single command's captured output.
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// Bound on how many files a check definition digests.
const MAX_CHECK_FILES: usize = 64;

/// Bound on one digested file's content.
const MAX_FILE_BYTES: usize = 4 * 1024 * 1024;

/// The tools whose versions make up the environment digest, in a fixed
/// order. A tool that is not installed contributes "absent" — which is
/// itself a stable fact about the environment.
const TOOLCHAIN: &[(&str, &[&str])] = &[
    ("rustc", &["--version"]),
    ("cargo", &["--version"]),
    ("node", &["--version"]),
    ("python3", &["--version"]),
];

/// The candidate's identity: what the working tree contains.
///
/// `git status --porcelain=v1 -z` plus `git rev-parse HEAD` describes the
/// tree exactly — the commit it is based on, and every deviation from it
/// including untracked files. Hashing that description is stable across
/// processes and machines for the same content, and changes whenever any
/// file does.
///
/// Outside a git repository, or when git cannot answer, the result is an
/// `unavailable:` digest unique to this call: an identity we could not
/// establish must never compare equal to one we could, nor to another
/// failure.
pub fn workspace_digest(root: &Path) -> String {
    // `git status` reports paths relative to the repository top level, not
    // to the process's directory. Joining them onto an arbitrary `root`
    // silently missed every file when `root` was a subdirectory, leaving a
    // digest over path names alone — two different edits to the same set of
    // files then hashed identically, which is exactly the false identity
    // match this exists to prevent. Resolve the top level and join there.
    let Some(top) = git_output(root, &["rev-parse", "--show-toplevel"]) else {
        return unavailable("workspace.toplevel");
    };
    let top = PathBuf::from(String::from_utf8_lossy(&top).trim().to_owned());
    let Some(head) = git_output(root, &["rev-parse", "HEAD"]) else {
        return unavailable("workspace.head");
    };
    // `-z` keeps paths NUL-separated, so a filename with a newline or a
    // quote cannot forge a status line and make two different trees hash
    // the same.
    let Some(status) = git_output(
        root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    ) else {
        return unavailable("workspace.status");
    };
    // Status lists *which* paths differ, not their content. Hash the content
    // of every differing path too, or two different edits to one file would
    // share a digest.
    let mut material = Vec::new();
    material.extend_from_slice(b"rapidlm.workspace/v1\0");
    material.extend_from_slice(String::from_utf8_lossy(&head).trim().as_bytes());
    material.push(0);
    for path in status_paths(&status) {
        if is_tool_state(&path) {
            continue;
        }
        // The status codes matter (a deletion differs from an edit), so the
        // per-path entry carries them; the raw blob is not hashed directly
        // because it would re-admit the tool-state paths filtered out above.
        material.extend_from_slice(status_code_for(&status, &path).as_bytes());
        material.push(0);
    }
    for path in status_paths(&status) {
        // RapidLM's own bookkeeping is not the user's work: `goal.json` and
        // `goal-evidence.json` are rewritten by the very commands that
        // record and invalidate evidence, so counting them would make the
        // workspace identity change when nothing but the tool's own state
        // did — and that is precisely what would make a strict
        // stale-workspace gate refuse valid acceptances.
        if is_tool_state(&path) {
            continue;
        }
        material.extend_from_slice(path.as_bytes());
        material.push(0);
        match read_bounded(&top.join(&path)) {
            Some(bytes) => material.extend_from_slice(&bytes),
            // A path in the status that cannot be read (a deletion, a
            // permission error) contributes its absence, not nothing.
            None => material.extend_from_slice(b"<unreadable>"),
        }
        material.push(0);
    }
    format!("sha256:{}", digest_hex(&material))
}

/// The test's identity: the command plus the content of the files it names.
///
/// A check that runs `cargo test -p foo --test bar` and a check whose test
/// file changed are different checks, and evidence citing the old one must
/// not survive the edit. Only paths that exist under `root` are digested;
/// a token that is not a file (a flag, a package name) contributes only as
/// part of the command string.
pub fn check_definition_digest(root: &Path, command: &str) -> String {
    let mut material = Vec::new();
    material.extend_from_slice(b"rapidlm.check/v1\0");
    material.extend_from_slice(command.as_bytes());
    material.push(0);
    let mut digested = 0usize;
    for token in command.split_whitespace() {
        if digested >= MAX_CHECK_FILES {
            break;
        }
        let candidate = root.join(token.trim_matches(|c| c == '"' || c == '\''));
        if !candidate.is_file() {
            continue;
        }
        material.extend_from_slice(token.as_bytes());
        material.push(0);
        match read_bounded(&candidate) {
            Some(bytes) => material.extend_from_slice(&bytes),
            None => material.extend_from_slice(b"<unreadable>"),
        }
        material.push(0);
        digested += 1;
    }
    format!("sha256:{}", digest_hex(&material))
}

/// The environment's identity: the toolchain versions a result depended on.
/// A tool that is absent contributes "absent", which is a stable fact; the
/// digest changes when a tool appears, disappears or is upgraded.
pub fn environment_digest() -> String {
    let mut material = Vec::new();
    material.extend_from_slice(b"rapidlm.environment/v1\0");
    for (program, args) in TOOLCHAIN {
        material.extend_from_slice(program.as_bytes());
        material.push(0);
        match capture(Path::new("."), program, args) {
            Some(version) => material.extend_from_slice(version.trim().as_bytes()),
            None => material.extend_from_slice(b"absent"),
        }
        material.push(0);
    }
    format!("sha256:{}", digest_hex(&material))
}

/// Paths named by a `-z` porcelain status. Each entry is `XY <path>` and
/// a rename carries a second NUL-separated path, which this also yields.
fn status_paths(status: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut expect_rename_source = false;
    for field in status.split(|b| *b == 0) {
        if field.is_empty() {
            continue;
        }
        if expect_rename_source {
            // The rename's source path, with no status prefix of its own.
            expect_rename_source = false;
            out.push(String::from_utf8_lossy(field).into_owned());
            continue;
        }
        // `XY path`: two status columns, a space, then the path.
        if field.len() < 4 {
            continue;
        }
        let (code, path) = field.split_at(3);
        expect_rename_source = code.starts_with(b"R") || code[1] == b'R';
        out.push(String::from_utf8_lossy(path).into_owned());
    }
    out.sort();
    out.dedup();
    out
}

/// RapidLM's own state under the project marker, which changes as a side
/// effect of the very commands that compute this digest.
fn is_tool_state(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    normalized == ".rapidlm" || normalized.starts_with(".rapidlm/")
}

/// The `XY` status code recorded for `path`, or an empty string when the
/// entry cannot be located (it then contributes only its name and content).
fn status_code_for(status: &[u8], wanted: &str) -> String {
    for field in status.split(|b| *b == 0) {
        if field.len() < 4 {
            continue;
        }
        let (code, path) = field.split_at(3);
        if String::from_utf8_lossy(path) == wanted {
            return String::from_utf8_lossy(code).into_owned();
        }
    }
    String::new()
}

fn read_bounded(path: &Path) -> Option<Vec<u8>> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES as u64 {
        return None;
    }
    std::fs::read(path).ok()
}

fn git_output(root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() || output.stdout.len() > MAX_OUTPUT_BYTES {
        return None;
    }
    Some(output.stdout)
}

fn capture(dir: &Path, program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() || output.stdout.len() > MAX_OUTPUT_BYTES {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn digest_hex(material: &[u8]) -> String {
    ArtifactId::from_bytes(material)
        .to_string()
        .rsplit(':')
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// A digest for an identity that could not be established. Unique per call,
/// so it compares equal to nothing — including another failure.
fn unavailable(reason: &str) -> String {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("unavailable:{reason}:{nonce:x}")
}

/// Whether a digest represents an identity that could not be established.
pub fn is_unavailable(digest: &str) -> bool {
    digest.starts_with("unavailable:")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git");
        assert!(status.success(), "git {args:?} failed");
    }

    fn repo(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rapidlm-digest-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        git(&dir, &["init", "-q"]);
        git(&dir, &["config", "user.email", "d@example.com"]);
        git(&dir, &["config", "user.name", "digest"]);
        git(&dir, &["config", "core.autocrlf", "false"]);
        std::fs::write(dir.join("base.txt"), "base\n").expect("write");
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", "base"]);
        dir
    }

    #[test]
    fn the_workspace_digest_changes_with_any_content_change_and_is_stable_otherwise() {
        let dir = repo("workspace");
        let first = workspace_digest(&dir);
        assert!(!is_unavailable(&first), "{first}");
        assert_eq!(
            first,
            workspace_digest(&dir),
            "stable for an unchanged tree"
        );

        // A tracked edit changes it.
        std::fs::write(dir.join("base.txt"), "changed\n").unwrap();
        let edited = workspace_digest(&dir);
        assert_ne!(first, edited, "a tracked edit must change the identity");

        // Two different edits to the same file differ from each other: the
        // status line alone is identical, so content must be digested too.
        std::fs::write(dir.join("base.txt"), "other\n").unwrap();
        assert_ne!(edited, workspace_digest(&dir), "content, not just the path");

        // An untracked file changes it too.
        std::fs::write(dir.join("base.txt"), "base\n").unwrap();
        assert_eq!(
            first,
            workspace_digest(&dir),
            "restored content restores it"
        );
        std::fs::write(dir.join("new.txt"), "new\n").unwrap();
        assert_ne!(first, workspace_digest(&dir), "an untracked file counts");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_digest_sees_content_from_a_subdirectory_too() {
        // `git status` paths are repo-root-relative, so joining them onto a
        // subdirectory `root` missed every file and left a digest over path
        // names alone — two different edits then hashed identically.
        let dir = repo("subdir");
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(dir.join("nested/keep.txt"), "one\n").unwrap();
        let from_top = workspace_digest(&dir);
        let from_sub = workspace_digest(&dir.join("nested"));
        assert!(!is_unavailable(&from_sub), "{from_sub}");
        assert_eq!(
            from_top, from_sub,
            "the same tree, wherever it is read from"
        );

        // And from the subdirectory, a content change still moves it.
        std::fs::write(dir.join("nested/keep.txt"), "two\n").unwrap();
        assert_ne!(
            from_sub,
            workspace_digest(&dir.join("nested")),
            "content must change the identity from a subdirectory too"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_tools_own_state_does_not_move_the_workspace_identity() {
        // `.rapidlm/goal.json` and `goal-evidence.json` are rewritten by the
        // very commands that record and invalidate evidence; counting them
        // would make the identity change when only bookkeeping did.
        let dir = repo("tool-state");
        let before = workspace_digest(&dir);
        std::fs::create_dir_all(dir.join(".rapidlm")).unwrap();
        std::fs::write(dir.join(".rapidlm/goal.json"), "{\"goal\":1}").unwrap();
        std::fs::write(dir.join(".rapidlm/goal-evidence.json"), "{\"records\":[]}").unwrap();
        assert_eq!(
            before,
            workspace_digest(&dir),
            "the tool's own state is not the user's work"
        );
        // A real file still moves it.
        std::fs::write(dir.join("real.txt"), "work\n").unwrap();
        assert_ne!(before, workspace_digest(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_digest_that_cannot_be_established_matches_nothing_including_itself() {
        let dir = std::env::temp_dir().join(format!("rapidlm-digest-nogit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let first = workspace_digest(&dir);
        let second = workspace_digest(&dir);
        assert!(is_unavailable(&first), "{first}");
        assert_ne!(
            first, second,
            "an unestablished identity must never compare equal, even to another failure"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_check_digest_follows_the_command_and_the_files_it_names() {
        let dir = repo("check");
        let plain = check_definition_digest(&dir, "cargo test -p foo");
        assert_eq!(plain, check_definition_digest(&dir, "cargo test -p foo"));
        assert_ne!(
            plain,
            check_definition_digest(&dir, "cargo test -p bar"),
            "a different command is a different check"
        );
        // A named file's content is part of the check's identity.
        let named = check_definition_digest(&dir, "sh base.txt");
        std::fs::write(dir.join("base.txt"), "edited\n").unwrap();
        assert_ne!(
            named,
            check_definition_digest(&dir, "sh base.txt"),
            "editing a file the check names changes the check"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_environment_digest_is_stable_within_one_toolchain() {
        let first = environment_digest();
        assert!(!is_unavailable(&first));
        assert_eq!(first, environment_digest());
    }
}
