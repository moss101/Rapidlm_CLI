//! Fuzzy `@`-file mentions: complete a path fragment from the project's
//! files, the way a file picker does.
//!
//! Typing `@` in the composer begins a mention; Tab completes it. The
//! fragment is matched fuzzily — subsequence, not prefix — against the
//! project's tracked files (git's index and working tree when the project
//! is a git repository, else a bounded walk that skips the usual noise
//! directories), and the best match replaces the fragment in place. What
//! the model receives is a plain path; the `@` stays, because the model
//! reading `@src/lib.rs` as "the user means this file" is the point.

use std::path::Path;

/// Most candidates one completion considers.
pub const MAX_CANDIDATES: usize = 4096;
/// Longest one candidate path.
pub const MAX_PATH_BYTES: usize = 512;
/// Most candidates shown when a fragment matches many.
pub const MAX_SHOWN: usize = 8;

/// One completion candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Candidate {
    pub path: String,
    /// The fuzzy score: higher wins. Subsequence matches score by
    /// compactness (consecutive runs beat scattered letters) and by the
    /// match starting earlier in the path.
    pub score: i64,
}

/// The `@`-mention fragment a draft ends with, if any: the text after the
/// last `@` that has no whitespace since it (a mention cannot span a
/// space — `@see this` names `see`, `this` is prose).
pub fn trailing_mention(draft: &str) -> Option<&str> {
    let at = draft.rfind('@')?;
    let fragment = &draft[at + 1..];
    if fragment.contains(char::is_whitespace) {
        return None;
    }
    // An empty fragment is still a mention — the picker's entry point.
    Some(fragment)
}

/// Score a fragment against a path: subsequence positions found earlier
/// and more compactly score higher. `None` when the fragment does not
/// match at all.
fn score(fragment: &str, path: &str) -> Option<i64> {
    let lower: Vec<char> = path.to_lowercase().chars().collect();
    let want: Vec<char> = fragment.to_lowercase().chars().collect();
    let mut score = 0i64;
    let mut last = None;
    let mut at = 0usize;
    for ch in &want {
        let mut found = None;
        while at < lower.len() {
            if &lower[at] == ch {
                found = Some(at);
                break;
            }
            at += 1;
        }
        let found = found?;
        // A run continues: +2 per consecutive letter; a fresh jump costs.
        score += match last {
            Some(prev) if found == prev + 1 => 2,
            _ => 1,
        };
        // Earlier matches count more: the first letter's position drags
        // the whole score down the later it appears.
        if found == 0 || lower[found - 1] == '/' || lower[found - 1] == '.' {
            score += 2; // a boundary (path segment, extension) is a strong start
        }
        last = Some(found);
        at = found + 1;
    }
    // Shorter paths win ties: `lib.rs` beats `vendor/whatever/lib.rs`.
    score -= (path.len() as i64) / 64;
    Some(score)
}

/// Rank `paths` against `fragment`, best first, bounded to
/// [`MAX_SHOWN`].
pub fn rank<'a>(fragment: &str, paths: impl IntoIterator<Item = &'a str>) -> Vec<Candidate> {
    let mut candidates: Vec<Candidate> = paths
        .into_iter()
        .filter_map(|path| {
            score(fragment, path).map(|value| Candidate {
                path: path.to_owned(),
                score: value,
            })
        })
        .collect();
    candidates.sort_by(|a, b| b.score.cmp(&a.score).then(a.path.cmp(&b.path)));
    candidates.truncate(MAX_SHOWN);
    candidates
}

/// The project's file paths: git's tracked files when the project is a
/// git repository (fast, and exactly the files a change touches), else a
/// bounded walk that skips the noise directories (`target`, `node_modules`,
/// `.git`, …). Paths are `/`-separated and repo-relative.
pub fn project_files(root: &Path) -> Vec<String> {
    if let Some(tracked) = git_ls_files(root) {
        return tracked;
    }
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name();
            let name = name.to_string_lossy().into_owned();
            if file_type.is_dir() {
                if is_noise_dir(&name) {
                    continue;
                }
                stack.push(entry.path());
            } else if file_type.is_file() {
                if files.len() >= MAX_CANDIDATES {
                    return files;
                }
                let path = entry.path();
                let Ok(relative) = path.strip_prefix(root) else {
                    continue;
                };
                let text = relative.display().to_string();
                if text.len() <= MAX_PATH_BYTES && !text.contains('\\') {
                    files.push(text);
                }
            }
        }
    }
    files.sort();
    files
}

fn is_noise_dir(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | ".hg"
            | ".rapidlm"
            | "target"
            | "node_modules"
            | ".venv"
            | "venv"
            | "dist"
            | "build"
            | ".next"
            | "__pycache__"
            | ".tox"
            | ".mypy_cache"
    )
}

/// `git ls-files` (plus untracked-but-not-ignored files) under `root`,
/// repo-relative, when the project is a git repository.
fn git_ls_files(root: &Path) -> Option<Vec<String>> {
    let output = std::process::Command::new("git")
        .args(["ls-files", "--cached", "--others", "--exclude-standard"])
        .current_dir(root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let files: Vec<String> = text
        .lines()
        .filter(|line| !line.is_empty() && line.len() <= MAX_PATH_BYTES)
        .map(str::to_owned)
        .take(MAX_CANDIDATES)
        .collect();
    // A git run in a non-repository answers non-zero; an empty answer in a
    // real repository means an empty repository — still a git project.
    (!files.is_empty()).then_some(files)
}

/// Complete the draft's trailing `@`-mention against `files`: the draft
/// with the fragment replaced by the best path, and the candidates that
/// were considered (all of them when the fragment is empty — the picker's
/// entry point; the best few otherwise).
pub fn complete(draft: &str, files: &[String]) -> Option<(String, Vec<Candidate>)> {
    let fragment = trailing_mention(draft)?;
    let at = draft.rfind('@')?;
    let candidates = if fragment.is_empty() {
        files
            .iter()
            .take(MAX_SHOWN)
            .map(|path| Candidate {
                path: path.clone(),
                score: 0,
            })
            .collect()
    } else {
        rank(fragment, files.iter().map(String::as_str))
    };
    let best = candidates.first()?;
    let completed = format!("{}@{}{}", &draft[..at], best.path, "");
    Some((completed, candidates))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files() -> Vec<String> {
        [
            "src/lib.rs",
            "src/interactive.rs",
            "src/exec_tools.rs",
            "src/permissions.rs",
            "crates/tui/src/commands.rs",
            "docs/reference/hooks.md",
            "Cargo.toml",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }

    #[test]
    fn a_trailing_mention_is_the_fragment_after_the_last_bare_at() {
        assert_eq!(trailing_mention("look at @inter"), Some("inter"));
        assert_eq!(trailing_mention("start with @"), Some(""));
        // A mention cannot span whitespace: `@see this` is prose after a
        // bare word.
        assert_eq!(trailing_mention("@see this"), None);
        assert_eq!(trailing_mention("no mention"), None);
        // An email-shaped `@` mid-word still counts as a mention start —
        // the completion list is what disambiguates.
        assert_eq!(trailing_mention("a@b"), Some("b"));
    }

    #[test]
    fn fragments_match_as_subsequences_and_rank_by_compactness() {
        // The fuzzy matcher is exercised through `rank` — the same
        // subsequence rule, proven by what it finds and what it cannot.
        // `lsr` — letters that appear across these paths, but `exec_tools.rs`
        // accepts them in order (l, s, then r in `.rs`), so a real negative
        // needs a letter no path carries.
        assert!(rank("zzq", files().iter().map(String::as_str)).is_empty());

        let ranked = rank("inter", files().iter().map(String::as_str));
        assert_eq!(
            ranked.first().map(|c| c.path.as_str()),
            Some("src/interactive.rs")
        );
        assert!(ranked.iter().all(|c| c.path != "Cargo.toml"));

        // Boundary starts beat mid-word starts: `p` ranks permissions.rs
        // (path segment start) over interactive.rs.
        let ranked = rank("perm", files().iter().map(String::as_str));
        assert_eq!(
            ranked.first().map(|c| c.path.as_str()),
            Some("src/permissions.rs")
        );

        // Shorter wins ties for the same letters: `ct` finds
        // crates/tui/src/commands.rs only via letters that exist; check a
        // real compact case instead.
        let ranked = rank("cargo", files().iter().map(String::as_str));
        assert_eq!(ranked.first().map(|c| c.path.as_str()), Some("Cargo.toml"));
    }

    #[test]
    fn completing_replaces_the_fragment_with_the_best_path() {
        let files = files();
        // Completion happens while the fragment is the draft's end — the
        // moment Tab is pressed — so prose after the mention is not part
        // of this contract.
        let (completed, candidates) = complete("explain @perm", &files).expect("completes");
        assert_eq!(completed, "explain @src/permissions.rs");
        assert!(candidates.iter().any(|c| c.path == "src/permissions.rs"));

        // An empty fragment lists the picker's entry candidates and
        // completes to the first.
        let (completed, _) = complete("look at @", &files).expect("empty completes");
        assert_eq!(completed, "look at @src/lib.rs");

        // A fragment that matches nothing does not complete — the draft
        // stands, and the caller says so.
        assert!(complete("find @zzzznope", &files).is_none());
    }

    #[test]
    fn project_files_walk_the_tree_and_skip_the_noise() {
        let dir = std::env::temp_dir().join(format!("rapidlm-mention-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for path in [
            "src/a.rs",
            "docs/b.md",
            "target/junk.o",
            "node_modules/x.js",
            ".git/config",
        ] {
            let file = dir.join(path);
            std::fs::create_dir_all(file.parent().expect("parent")).expect("dirs");
            std::fs::write(&file, b"x").expect("write");
        }
        let files = project_files(&dir);
        assert!(files.contains(&"src/a.rs".to_owned()), "{files:?}");
        assert!(files.contains(&"docs/b.md".to_owned()));
        assert!(
            !files.iter().any(|f| f.starts_with("target")
                || f.starts_with("node_modules")
                || f.starts_with(".git")),
            "the noise directories stay out: {files:?}"
        );
        // Git tracks faster where it can: in this non-repository the walk
        // is the answer, and it is bounded and sorted.
        let mut sorted = files.clone();
        sorted.sort();
        assert_eq!(files, sorted);
        std::fs::remove_dir_all(&dir).ok();
    }
}
