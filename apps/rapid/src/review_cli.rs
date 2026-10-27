//! `rapid review` — a first-party review of the working tree's changes.
//!
//! The project instructions may carry a `## Review rules` (or
//! `## Code Review Rules`) section: house rules the review reads. The diff
//! comes from git — the uncommitted changes by default, a base ref's
//! range, or one commit — and one model call over the rules and the diff
//! answers with findings, or says there are none. The reviewer runs on
//! the session's own model, resolved the way every other auxiliary call
//! resolves.

use std::path::Path;
use std::process::Command;

/// Longest diff accepted (bytes) — a review reads a change, not a
/// history.
pub const MAX_DIFF_BYTES: usize = 256 * 1024;
/// Longest one finding kept.
pub const MAX_FINDING_BYTES: usize = 2 * 1024;
/// Most findings one review reports.
pub const MAX_FINDINGS: usize = 32;

/// What one review found.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Finding {
    pub file: String,
    pub line: Option<u32>,
    pub severity: String,
    pub finding: String,
}

/// The reviewer's standing instructions: what a finding is, and the shape
/// of the answer.
const REVIEW_INSTRUCTIONS: &str = "\
You are reviewing a change to a codebase. The project's own review rules \
(if any) come first, then the diff. Report only real problems a maintainer \
would want fixed before merge: bugs, security issues, broken invariants, \
misleading names or comments, and tests that do not test what they claim. \
Style the project did not ask for is not a finding; missing perfection is \
not a finding. For each finding name the file, the line in the *new* file \
when you can, a severity (`high`, `medium`, `low`), and one to three \
sentences that say what is wrong and why it matters.

Reply with one JSON array and nothing else: \
[{\"file\":\"path\",\"line\":12,\"severity\":\"high\",\
\"finding\":\"what is wrong and why\"}, …] — an empty array when the \
change is clean.";

/// The user-role request the diff is attached to.
const REVIEW_TASK: &str =
    "Review the change that follows, as instructed. Reply with the JSON array only.";

/// Extract the project's review rules from its composed instructions: the
/// `## Review rules` (or `## Code Review Rules`) section, heading to the
/// next heading of the same or higher rank. `None` when the instructions
/// carry no such section — the review proceeds with the standing
/// instructions alone.
pub fn review_rules_section(instructions: &str) -> Option<String> {
    let mut in_section = false;
    let mut collected: Vec<&str> = Vec::new();
    for line in instructions.lines() {
        let trimmed = line.trim_start();
        let is_rules = trimmed.eq_ignore_ascii_case("## review rules")
            || trimmed.eq_ignore_ascii_case("## code review rules");
        if is_rules {
            in_section = true;
            continue;
        }
        if in_section {
            // The next heading of this rank or higher closes the section;
            // a deeper sub-heading stays inside it.
            if trimmed.starts_with('#') && !trimmed.starts_with("###") {
                break;
            }
            collected.push(line);
        }
    }
    if !in_section {
        return None;
    }
    let joined = collected.join("\n").trim().to_owned();
    (!joined.is_empty()).then_some(joined)
}

/// Collect the diff under `root` for the requested scope. `None` when the
/// scope cannot be collected: not a git repository, or git itself failed —
/// the reason says which.
pub fn collect_diff(root: &Path, scope: ReviewScope) -> Result<String, String> {
    let (program, args): (&str, Vec<&str>) = match &scope {
        ReviewScope::Uncommitted => ("diff", vec!["diff", "HEAD"]),
        ReviewScope::Base(base) => ("diff", vec!["diff", "--merge-base", base.as_str(), "HEAD"]),
        ReviewScope::Commit(commit) => ("show", vec!["show", commit.as_str()]),
    };
    let output = Command::new("git")
        .args(&args)
        .current_dir(root)
        .output()
        .map_err(|err| format!("git could not run: {err}"))?;
    if !output.status.success() {
        let reason = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(format!(
            "git {program} failed{}",
            if reason.is_empty() {
                String::new()
            } else {
                format!(": {}", reason.chars().take(200).collect::<String>())
            }
        ));
    }
    let mut diff = String::from_utf8_lossy(&output.stdout).into_owned();
    if diff.len() > MAX_DIFF_BYTES {
        let cut: String = diff.chars().take(MAX_DIFF_BYTES).collect();
        diff = format!("{cut}\n… (diff truncated at {MAX_DIFF_BYTES} bytes)");
    }
    Ok(diff)
}

/// Which change a review reads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReviewScope {
    Uncommitted,
    Base(String),
    Commit(String),
}

/// What the review prompt carries: the rules (if any) and the diff, as
/// the task's memory — pure, so the composition is testable.
pub fn review_blocks<'a>(
    rules: Option<&'a str>,
    diff: &'a str,
) -> Vec<context_engine::compile::CompileInput> {
    let mut inputs = Vec::new();
    if let Some(rules) = rules.filter(|rules| !rules.trim().is_empty()) {
        inputs.push(context_engine::compile::CompileInput::new(
            "review/rules",
            rules.to_owned(),
        ));
    }
    inputs.push(context_engine::compile::CompileInput::new(
        "review/diff",
        diff.to_owned(),
    ));
    inputs
}

/// Parse the reviewer's reply: the instructed JSON array, or — a model
/// being a model — a fenced array. Anything else is `None`, and the
/// caller shows the bounded text rather than guessing findings from it.
pub fn parse_findings(text: &str) -> Option<Vec<Finding>> {
    let trimmed = text.trim();
    let body = trimmed
        .strip_prefix("```")
        .and_then(|rest| rest.strip_suffix("```"))
        .map(str::trim)
        .map(|inside| match inside.split_once('\n') {
            Some((tag, rest)) if !tag.is_empty() && tag.chars().all(char::is_alphanumeric) => {
                rest.trim()
            }
            _ => inside,
        })
        .unwrap_or(trimmed);
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let entries = value.as_array()?;
    let mut findings = Vec::new();
    for entry in entries.iter().take(MAX_FINDINGS) {
        let object = entry.as_object()?;
        let file = object.get("file")?.as_str()?.to_owned();
        let finding = object
            .get("finding")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_owned();
        if file.is_empty() || finding.trim().is_empty() {
            return None;
        }
        findings.push(Finding {
            file,
            line: object
                .get("line")
                .and_then(serde_json::Value::as_u64)
                .and_then(|line| u32::try_from(line).ok()),
            severity: object
                .get("severity")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("medium")
                .chars()
                .take(16)
                .collect(),
            finding: finding.chars().take(MAX_FINDING_BYTES).collect(),
        });
    }
    Some(findings)
}

/// One review model call: the standing instructions as the system block,
/// the rules and the diff as the task's memory, one step, the reply.
/// Mirrors `approval_review::run_review`'s shape.
pub fn run_review_model<B: crate::host::LiveModelCall>(
    backing: &mut B,
    rules: Option<&str>,
    diff: &str,
    context_limit: u32,
    output_reserve: u32,
    cancel: &agent_runtime::CancellationToken,
) -> Result<String, String> {
    use agent_runtime::ModelStepInput;
    use context_engine::compile::{CompileContext, CompileInput, CompileLimits};
    let limits = CompileLimits::new()
        .retrieved_share_bps(0)
        .memory_share_bps(10_000)
        .read_set_share_bps(0);
    let mut ctx = CompileContext::new(context_limit, output_reserve)
        .limits(limits)
        .system(CompileInput::new(
            "system/review",
            REVIEW_INSTRUCTIONS.to_owned(),
        ))
        .task(REVIEW_TASK);
    for input in review_blocks(rules, diff) {
        ctx = ctx.memory(input);
    }
    let packet = context_engine::compile::compile(&ctx)
        .map_err(|err| format!("the review request did not compile: {err}"))?;
    if cancel.is_cancelled() {
        return Err("cancelled".to_owned());
    }
    let stepped = backing.step(packet.blocks(), &ModelStepInput::without_tools(0), cancel);
    let _ = backing.take_continuations();
    match stepped {
        Ok(agent_runtime::ModelStepOutput::Terminal { text, .. }) => Ok(text),
        Ok(agent_runtime::ModelStepOutput::ToolCalls { .. }) => {
            Err("the reviewer proposed tool calls, which it was not offered".to_owned())
        }
        Err(err) => Err(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_review_rules_section_is_extracted_between_headings() {
        let instructions = "\
# AGENTS.md

House rules for everyone.

## Review rules

- No new dependencies without a note in the ledger.
- Every fix names its test.

## Other section

Unrelated prose.";
        assert_eq!(
            review_rules_section(instructions).expect("rules"),
            "- No new dependencies without a note in the ledger.\n- Every fix names its test."
        );
        // Case-insensitive heading, either spelling; a section that runs
        // to the end still closes; and no section means none.
        assert!(review_rules_section("## Code Review Rules\n\none").is_some());
        assert!(review_rules_section("## Review Rules\n\nruns to the end").is_some());
        assert_eq!(review_rules_section("# AGENTS.md\n\nnothing marked"), None);
    }

    #[test]
    fn findings_parse_from_the_instructed_array_and_nothing_else() {
        let reply =
            r#"[{"file":"src/lib.rs","line":12,"severity":"high","finding":"reads past the end"}]"#;
        let findings = parse_findings(reply).expect("findings");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].file, "src/lib.rs");
        assert_eq!(findings[0].line, Some(12));
        assert_eq!(findings[0].severity, "high");

        // A fenced array is tolerated; a missing line or severity gets
        // defaults; prose is not guessed into findings.
        let fenced = parse_findings("```json\n[]\n```").expect("fenced");
        assert!(fenced.is_empty());
        let partial =
            parse_findings(r#"[{"file":"a.rs","finding":"unclear name"}]"#).expect("partial");
        assert_eq!(partial[0].line, None);
        assert_eq!(partial[0].severity, "medium");
        assert!(parse_findings("looks fine overall").is_none());
        assert!(parse_findings(r#"[{"file":"","finding":"x"}]"#).is_none());
    }

    #[test]
    fn the_diff_comes_from_git_for_each_scope() {
        let dir = std::env::temp_dir().join(format!("rapidlm-review-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let git = |args: &[&str]| -> String {
            let output = Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .expect("git");
            assert!(
                output.status.success(),
                "git {:?}: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).into_owned()
        };
        git(&["init", "--quiet"]);
        git(&["config", "user.email", "review@test"]);
        git(&["config", "user.name", "Review Test"]);
        std::fs::write(dir.join("a.rs"), "fn one() {}\n").expect("write");
        git(&["add", "."]);
        git(&["commit", "--quiet", "-m", "first"]);
        std::fs::write(dir.join("a.rs"), "fn one() { panic!() }\n").expect("edit");
        git(&["add", "."]);
        git(&["commit", "--quiet", "-m", "second"]);
        std::fs::write(dir.join("a.rs"), "fn one() { panic!() } // uncommitted\n").expect("edit");

        let uncommitted = collect_diff(&dir, ReviewScope::Uncommitted).expect("uncommitted");
        assert!(uncommitted.contains("uncommitted"), "{uncommitted}");
        let commit = collect_diff(&dir, ReviewScope::Commit("HEAD".to_owned())).expect("commit");
        assert!(commit.contains("panic!"), "{commit}");
        let base = collect_diff(&dir, ReviewScope::Base("HEAD~1".to_owned())).expect("base");
        assert!(base.contains("panic!"), "{base}");

        assert!(collect_diff(&dir, ReviewScope::Commit("nope".to_owned())).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A backing whose one step returns `text`.
    struct Reply(String);

    impl crate::host::LiveModelCall for Reply {
        fn step(
            &mut self,
            _blocks: &[context_engine::compile::ContextBlock],
            _input: &agent_runtime::ModelStepInput<'_>,
            _cancel: &agent_runtime::CancellationToken,
        ) -> Result<agent_runtime::ModelStepOutput, agent_runtime::ModelStepError> {
            Ok(agent_runtime::ModelStepOutput::Terminal {
                text: self.0.clone(),
                tokens: 1,
                cost_usd_micros: None,
            })
        }
    }

    #[test]
    fn the_review_model_call_parses_its_reply() {
        let mut backing = Reply(
            r#"[{"file":"a.rs","line":3,"severity":"low","finding":"name says list, code sums"}]"#
                .to_owned(),
        );
        let reply = run_review_model(
            &mut backing,
            Some("- house rule"),
            "diff --git a/a.rs b/a.rs",
            crate::user_config::DEFAULT_CONTEXT_WINDOW,
            crate::user_config::DEFAULT_MAX_OUTPUT_TOKENS,
            &agent_runtime::CancellationToken::new(),
        )
        .expect("reply");
        let findings = parse_findings(&reply).expect("findings");
        assert_eq!(findings[0].finding, "name says list, code sums");
    }
}
