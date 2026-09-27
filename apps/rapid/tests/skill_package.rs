//! The skill package (`skill/`) teaches other hosts to drive `rapid`
//! headlessly. Every command it shows is run against the binary's own
//! `--help` here, so a documented subcommand or flag cannot drift from what
//! the binary accepts (SEAM-06 AC-06).

use std::borrow::Cow;
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

/// Every `rapid …` command the file shows: each line of a code block, and
/// each inline code span, that starts with `rapid `.
fn command_lines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut in_block = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_block = !in_block;
            continue;
        }
        if in_block {
            if trimmed.starts_with("rapid ") {
                lines.push(trimmed.to_owned());
            }
            continue;
        }
        // Inline spans: the text between each pair of backticks.
        for (at, span) in line.split('`').enumerate() {
            if at % 2 == 1 && span.starts_with("rapid ") {
                lines.push(span.to_owned());
            }
        }
    }
    lines
}

/// A command line's words, as a POSIX shell would split them (double and
/// single quotes; no expansion), each with whether any of it was quoted.
fn split(line: &str) -> Vec<(String, bool)> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut quoted = false;
    for c in line.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => word.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                quoted = true;
            }
            (None, c) if c.is_whitespace() => {
                if quoted || !word.is_empty() {
                    words.push((std::mem::take(&mut word), quoted));
                }
                quoted = false;
            }
            (None, c) => word.push(c),
        }
    }
    assert!(quote.is_none(), "unbalanced quote in {line:?}");
    if quoted || !word.is_empty() {
        words.push((word, quoted));
    }
    words
}

fn words(line: &str) -> Vec<String> {
    split(line).into_iter().map(|(word, _)| word).collect()
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

/// The help entry for `command` when the help lists it as its own entry (a
/// line indented two spaces that starts with it, and the deeper-indented
/// lines that continue it); otherwise the whole help.
fn entry<'a>(help: &'a str, command: &str) -> Cow<'a, str> {
    let lines: Vec<&str> = help.lines().collect();
    let indent = |line: &str| line.len() - line.trim_start().len();
    let Some(start) = lines.iter().position(|line| {
        indent(line) == 2
            && line
                .trim_start()
                .strip_prefix(command)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
    }) else {
        return Cow::Borrowed(help);
    };
    let end = lines[start + 1..]
        .iter()
        .position(|line| !line.trim().is_empty() && indent(line) <= 2)
        .map_or(lines.len(), |offset| start + 1 + offset);
    Cow::Owned(lines[start..end].join("\n"))
}

/// Why `line` is not a command the binary documents, if it is not.
fn check(line: &str, helps: &mut std::collections::BTreeMap<String, String>) -> Option<String> {
    let words = split(line);
    let subcommand = words.get(1).map(|(word, _)| word.clone())?;
    let help = helps
        .entry(subcommand.clone())
        .or_insert_with(|| help(&subcommand));
    // The unquoted lowercase words after the subcommand, up to the first
    // flag or argument, name a nested command: each must be in the help, and
    // a flag must be in that command's own entry when the help gives it one.
    let nested: Vec<&str> = words[2..]
        .iter()
        .take_while(|(word, quoted)| {
            !quoted
                && !word.is_empty()
                && !word.starts_with('-')
                && word.chars().all(|c| c.is_ascii_lowercase() || c == '-')
        })
        .map(|(word, _)| word.as_str())
        .collect();
    if let Some(word) = nested.iter().find(|word| !names(help, word)) {
        return Some(format!(
            "`{line}` uses `{word}`, which `rapid {subcommand} --help` does not name"
        ));
    }
    // The longest nested command the help gives its own entry
    // (`evidence record` before `evidence`); the whole help when none.
    let scope = (1..=nested.len())
        .rev()
        .map(|depth| nested[..depth].join(" "))
        .find_map(|command| match entry(help, &command) {
            Cow::Owned(scoped) => Some(Cow::Owned(scoped)),
            Cow::Borrowed(_) => None,
        })
        .unwrap_or(Cow::Borrowed(help.as_str()));
    for (word, quoted) in &words[2..] {
        let Some(flag) = word.strip_prefix("--").filter(|_| !quoted) else {
            continue;
        };
        if flag.contains('=') {
            return Some(format!(
                "`{line}`: the binary takes `--flag value`, not `--flag=value`"
            ));
        }
        let flag = format!("--{flag}");
        if !names(&scope, &flag) {
            return Some(format!(
                "`{line}` uses {flag}, which `rapid {subcommand} --help` does not give {}",
                nested
                    .first()
                    .map_or("it".to_owned(), |command| format!("`{command}`"))
            ));
        }
    }
    None
}

#[test]
fn every_command_in_the_skill_package_is_one_the_binary_documents() {
    let files = skill_files();
    assert!(files.len() >= 2, "the skill and its sub-skill: {files:?}");
    let mut helps = std::collections::BTreeMap::new();
    let mut checked = 0;
    for file in &files {
        let text = std::fs::read_to_string(file).expect("read skill");
        for line in command_lines(&text) {
            if let Some(problem) = check(&line, &mut helps) {
                panic!("{}: {problem}", file.display());
            }
            checked += 1;
        }
    }
    assert!(checked >= 20, "only {checked} commands found");
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
fn the_check_catches_what_it_is_for() {
    let mut helps = std::collections::BTreeMap::new();
    // Documented: passes.
    assert_eq!(check(r#"rapid exec "x" --jsonl"#, &mut helps), None);
    assert_eq!(
        check(
            r#"rapid goal claim --summary "s" --check "t=x""#,
            &mut helps
        ),
        None
    );
    // A made-up flag, a flag of another nested command, a made-up nested
    // word at any depth, and the `=` form the parser does not take.
    for bad in [
        r#"rapid exec "x" --no-such-flag"#,
        r#"rapid exec "x" --json"#,
        r#"rapid goal create "s" --check "t=x""#,
        "rapid goal suspend",
        "rapid goal evidence recrod --kind test",
        "rapid goal evidence list --kind test",
        r#"rapid goal create "s" --criterion=t"#,
    ] {
        assert!(check(bad, &mut helps).is_some(), "{bad} passed");
    }
    // A quoted single word is an argument, not a nested command.
    assert_eq!(check(r#"rapid exec "x""#, &mut helps), None);
    assert_eq!(
        words(r#"rapid goal create "a b" --criterion 'x=y z'"#),
        ["rapid", "goal", "create", "a b", "--criterion", "x=y z"]
    );
    // Inline spans are commands too.
    assert_eq!(
        command_lines("run `rapid exec \"x\" --jsonl` and `ls`\n"),
        [r#"rapid exec "x" --jsonl"#]
    );
}
