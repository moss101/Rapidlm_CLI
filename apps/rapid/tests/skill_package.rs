//! The skill package (`skill/`) teaches other hosts to drive `rapid`
//! headlessly. Every command line it shows is run against the binary's own
//! `--help` here, so a documented subcommand or flag cannot drift from what
//! the binary accepts (SEAM-06 AC-06).

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every `SKILL.md` under `skill/`.
fn skill_files() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("skill dir").flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.file_name().is_some_and(|name| name == "SKILL.md") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(&repo().join("skill"), &mut out);
    out.sort();
    out
}

/// The `rapid …` lines inside the file's shell code blocks.
fn command_lines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut in_shell = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(fence) = trimmed.strip_prefix("```") {
            in_shell = !in_shell && matches!(fence, "bash" | "sh" | "shell");
            continue;
        }
        if in_shell && trimmed.starts_with("rapid ") {
            lines.push(trimmed.to_owned());
        }
    }
    lines
}

/// A command line's words, as a POSIX shell would split them (double and
/// single quotes; no expansion).
fn words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for c in line.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => word.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started || !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
                started = false;
            }
            (None, c) => word.push(c),
        }
    }
    assert!(quote.is_none(), "unbalanced quote in {line:?}");
    if started || !word.is_empty() {
        words.push(word);
    }
    words
}

/// `rapid <subcommand> --help`, run where no project is.
fn help(subcommand: &str) -> String {
    let scratch = std::env::temp_dir().join(format!("rapidlm-skill-help-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).expect("scratch");
    let output = Command::new(env!("CARGO_BIN_EXE_rapid"))
        .args([subcommand, "--help"])
        .current_dir(&scratch)
        .env("HOME", &scratch)
        .output()
        .expect("run rapid");
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success() && !text.trim().is_empty(),
        "`rapid {subcommand} --help` failed: {}{}",
        text,
        String::from_utf8_lossy(&output.stderr)
    );
    text
}

/// `name` in `text` as a whole word (not inside a longer flag or name).
fn names(text: &str, name: &str) -> bool {
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    text.match_indices(name).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + name.len()..].chars().next();
        before.is_none_or(|c| !is_word(c)) && after.is_none_or(|c| !is_word(c))
    })
}

#[test]
fn every_command_in_the_skill_package_is_one_the_binary_documents() {
    let files = skill_files();
    assert!(files.len() >= 2, "the skill and its sub-skill: {files:?}");
    let mut checked = 0;
    let mut helps = std::collections::BTreeMap::new();
    for file in &files {
        let text = std::fs::read_to_string(file).expect("read skill");
        for line in command_lines(&text) {
            let words = words(&line);
            let subcommand = words
                .get(1)
                .unwrap_or_else(|| panic!("{line}: no subcommand"));
            let help = helps
                .entry(subcommand.clone())
                .or_insert_with(|| help(subcommand));
            for (at, word) in words.iter().enumerate().skip(2) {
                if let Some(flag) = word.strip_prefix("--") {
                    let flag = format!("--{}", flag.split('=').next().unwrap_or(flag));
                    assert!(
                        names(help, &flag),
                        "{}: `{line}` uses {flag}, which `rapid {subcommand} --help` does not name",
                        file.display()
                    );
                } else if at == 2
                    && word.chars().all(|c| c.is_ascii_lowercase() || c == '-')
                    && !word.is_empty()
                {
                    assert!(
                        names(help, word),
                        "{}: `{line}` uses `{word}`, which `rapid {subcommand} --help` does not name",
                        file.display()
                    );
                }
            }
            checked += 1;
        }
    }
    assert!(checked >= 20, "only {checked} command lines found");
}

#[test]
fn the_skill_package_is_in_the_open_skill_format() {
    for file in skill_files() {
        let text = std::fs::read_to_string(&file).expect("read skill");
        let front = text
            .strip_prefix("---\n")
            .and_then(|rest| rest.split_once("\n---\n"))
            .map(|(front, _)| front)
            .unwrap_or_else(|| panic!("{}: no frontmatter", file.display()));
        for field in ["name: ", "description: "] {
            assert!(
                front
                    .lines()
                    .any(|line| line.starts_with(field) && line.len() > field.len()),
                "{}: frontmatter needs `{field}`",
                file.display()
            );
        }
    }
}

/// The rows of the exit-code table after `marker`.
fn exit_rows(text: &str, marker: &str) -> Vec<String> {
    let start = text.find(marker).unwrap_or_else(|| panic!("no {marker}"));
    text[start..]
        .lines()
        .skip_while(|line| !line.starts_with("| `"))
        .take_while(|line| line.starts_with("| `"))
        .map(str::to_owned)
        .collect()
}

#[test]
fn the_skills_exit_codes_are_the_documented_ones() {
    // docs/getting-started.md's table is checked against the binary's own
    // exit codes by `getting_started_lists_exactly_the_exit_codes`.
    let skill = std::fs::read_to_string(repo().join("skill/SKILL.md")).expect("skill");
    let guide = std::fs::read_to_string(repo().join("docs/getting-started.md")).expect("guide");
    let rows = exit_rows(&skill, "<!-- exit-codes:");
    assert!(rows.len() >= 10, "{rows:?}");
    assert_eq!(rows, exit_rows(&guide, "<!-- exit-codes:"));
}

#[test]
fn a_flag_the_binary_does_not_have_is_caught() {
    // The check itself: a made-up flag, and a made-up subcommand word.
    let help = help("exec");
    assert!(names(&help, "--jsonl"));
    assert!(
        !names(&help, "--json"),
        "`--json` is only part of `--jsonl`/`--json-schema`"
    );
    assert!(!names(&help, "--no-such-flag"));
    assert_eq!(
        words(r#"rapid goal create "a b" --criterion 'x=y z'"#),
        ["rapid", "goal", "create", "a b", "--criterion", "x=y z"]
    );
}
