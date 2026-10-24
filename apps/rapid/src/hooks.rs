//! Project-settings hooks (headless scope).
//!
//! `pre_tool_use` commands run before a matched tool executes. A hook that
//! prints nothing structured keeps the v1 contract: a non-zero exit DENIES
//! the call with the hook's stderr as the model-visible detail. A hook that
//! prints a [`protocol::HookResult`] (`rapidlm.hook_result` v2) on stdout
//! decides in words — `allow`, `deny`, `ask`, `defer` — and each such
//! decision is recorded as a [`HookDecisionRecord`] for the ledger (ADR
//! 0022). `post_tool_use` commands run after execution and their output is
//! recorded on the result. Hook commands are project settings
//! (trusted-project gate), receive the tool call as JSON on stdin, and are
//! bounded by a timeout — a hung hook denies rather than hangs the turn.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use protocol::{HookDecision, HookResult, HookResultError, MAX_HOOK_RESULT_BYTES};

/// Default per-hook wall-clock budget.
pub const HOOK_TIMEOUT: Duration = Duration::from_secs(5);
/// Maximum hooks per stage.
pub const MAX_HOOKS_PER_STAGE: usize = 8;
/// Maximum bytes of one hook command line.
pub const MAX_HOOK_COMMAND_BYTES: usize = 512;
/// Hard byte cap on captured hook stderr.
pub const MAX_HOOK_STDERR_BYTES: usize = 2048;

/// How many entries of one stage's array are read from one file, kept or not.
pub const MAX_ENTRIES_EXAMINED_PER_STAGE: usize = 32;

/// Maximum bytes of a hook `matcher` expression.
pub const MAX_HOOK_MATCHER_BYTES: usize = 256;

/// Compile `pattern` with a compile-size bound well under the crate's
/// default, so a hostile expression cannot cost a turn much to load.
fn build_regex(pattern: &str) -> Result<regex::Regex, regex::Error> {
    regex::RegexBuilder::new(pattern)
        .size_limit(256 * 1024)
        .dfa_size_limit(256 * 1024)
        .build()
}

/// A stage's anchored whole-subject regular expression. Equality is the
/// expression's text, which is what deduplication and the settings merge
/// compare.
#[derive(Clone, Debug)]
pub struct HookMatcher {
    source: String,
    regex: regex::Regex,
}

impl HookMatcher {
    /// Compile `source` anchored to the whole subject. The workspace `regex`
    /// crate matches in linear time, so a hostile expression cannot stall a
    /// turn.
    pub fn new(source: &str) -> Result<Self, String> {
        if source.is_empty() {
            return Err("`matcher` is empty".to_owned());
        }
        if source.len() > MAX_HOOK_MATCHER_BYTES {
            return Err(format!(
                "`matcher` is {} bytes, over the {MAX_HOOK_MATCHER_BYTES}-byte limit",
                source.len()
            ));
        }
        let invalid = || "`matcher` is not a valid regular expression".to_owned();
        // On its own first: wrapped in the anchors an unbalanced expression
        // such as `a)|(b` would compile and match more than the whole subject.
        build_regex(source).map_err(|_| invalid())?;
        let regex = build_regex(&format!("^(?:{source})$")).map_err(|_| invalid())?;
        Ok(Self {
            source: source.to_owned(),
            regex,
        })
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn is_match(&self, subject: &str) -> bool {
        self.regex.is_match(subject)
    }
}

impl PartialEq for HookMatcher {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source
    }
}

impl Eq for HookMatcher {}

/// One configured hook: a command line and, in the object form, the
/// narrowing the entry asked for. A plain string in settings is an entry
/// with none of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookEntry {
    pub command: String,
    pub matcher: Option<HookMatcher>,
    /// Wall-clock budget for this hook; `None` is the caller's default
    /// ([`HOOK_TIMEOUT`]).
    pub timeout_ms: Option<u64>,
    /// `stop` only: a `deny` from this hook may send the turn back for another
    /// section (EXT-03 §3.5). Parsed and kept; the stop stage consults it
    /// once the continuation lands (EXT-03-4).
    pub continue_turn: bool,
}

impl HookEntry {
    pub fn plain(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            matcher: None,
            timeout_ms: None,
            continue_turn: false,
        }
    }
}

/// A plain entry is its command line: lets a test compare a parsed stage with
/// the strings it was written from.
impl PartialEq<String> for HookEntry {
    fn eq(&self, other: &String) -> bool {
        self == &HookEntry::plain(other.clone())
    }
}

impl From<String> for HookEntry {
    fn from(command: String) -> Self {
        Self::plain(command)
    }
}

impl From<&str> for HookEntry {
    fn from(command: &str) -> Self {
        Self::plain(command)
    }
}

/// What a stage runner needs to know about one hook. Implemented for a bare
/// command line and for [`HookEntry`], so the runners serve both and a
/// string-only configuration runs exactly as it always did.
pub trait HookSpec {
    fn command(&self) -> &str;
    /// Whether the hook applies to `subject` (the tool name, agent type,
    /// stop reason or notification kind of the stage). No matcher matches
    /// everything.
    fn applies_to(&self, subject: Option<&str>) -> bool;
    /// The hook's own wall-clock budget, when it set one.
    fn timeout_override(&self) -> Option<Duration>;
}

impl HookSpec for String {
    fn command(&self) -> &str {
        self
    }
    fn applies_to(&self, _subject: Option<&str>) -> bool {
        true
    }
    fn timeout_override(&self) -> Option<Duration> {
        None
    }
}

impl HookSpec for HookEntry {
    fn command(&self) -> &str {
        &self.command
    }
    fn applies_to(&self, subject: Option<&str>) -> bool {
        match (&self.matcher, subject) {
            (None, _) => true,
            (Some(matcher), Some(subject)) => matcher.is_match(subject),
            // A matcher on a stage with no subject is rejected when the
            // entry is parsed; an entry that got here anyway does not run.
            (Some(_), None) => false,
        }
    }
    fn timeout_override(&self) -> Option<Duration> {
        self.timeout_ms.map(Duration::from_millis)
    }
}

/// Why one object-form entry was rejected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookRejection {
    pub stage: &'static str,
    /// Index in the stage's array, in the file it came from.
    pub position: usize,
    pub reason: String,
}

impl std::fmt::Display for HookRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "hooks.{}[{}]: {}",
            self.stage, self.position, self.reason
        )
    }
}

/// What a stage's `matcher` is matched against, or `None` for a stage with no
/// subject (a `matcher` there is rejected).
pub fn stage_subject(stage: &str) -> Option<&'static str> {
    match stage {
        "pre_tool_use" | "post_tool_use" | "post_tool_use_failure" => Some("tool name"),
        "subagent_start" | "subagent_stop" => Some("agent type"),
        "stop_cancelled" => Some("stop reason"),
        _ => None,
    }
}

/// Whether a timed-out hook's whole process group is terminated on this
/// platform. Where it is not, a hook could outlive its timeout, so an entry
/// asking for more than the default budget is refused rather than leaking
/// children ([`timeout_allowed`]).
pub const PROCESS_GROUP_TERMINATION: bool = cfg!(any(unix, windows));

/// Whether `timeout_ms` may be set on this platform given whether it can
/// terminate a hook's process group.
fn timeout_allowed(timeout_ms: u64, group_termination: bool) -> Result<(), String> {
    if !group_termination && timeout_ms > HOOK_TIMEOUT.as_millis() as u64 {
        return Err(format!(
            "`timeout_ms` above {} is not available on this platform: a timed-out hook's \
child processes cannot be ended with it",
            HOOK_TIMEOUT.as_millis()
        ));
    }
    Ok(())
}

/// The largest `timeout_ms` a stage accepts, by how the stage blocks the
/// user (EXT-03 §3.3). A stage that is not named blocks little and takes the
/// observer ceiling.
pub fn stage_max_timeout_ms(stage: &str) -> u64 {
    match stage {
        "pre_tool_use" | "subagent_stop" => 120_000,
        "stop" => 600_000,
        _ => 30_000,
    }
}

/// Hook commands parsed from project settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HooksConfig {
    pub pre_tool_use: Vec<HookEntry>,
    pub post_tool_use: Vec<HookEntry>,
    /// Fires once per `rapid exec` run, after hooks/settings load, before the
    /// turn starts. Notification-style: output is logged, never gates.
    pub session_start: Vec<HookEntry>,
    /// Fires once per `rapid exec` run, on every exit path (success, typed
    /// failure, or early return) — see `SessionEndHookGuard` in
    /// `interactive.rs`. Notification-style, same as `session_start`.
    pub session_end: Vec<HookEntry>,
    /// Fires when `task_spawn` is about to run a child agent.
    /// Notification-style, same as `session_start`.
    pub subagent_start: Vec<HookEntry>,
    /// Fires when a `task_spawn` child finishes (success or failure).
    /// Notification-style, same as `session_start`.
    pub subagent_stop: Vec<HookEntry>,
    /// Fires before a `/compact` asks the model for its summary, with how
    /// many turns are about to be folded. Notification-style: it observes,
    /// it never gates.
    pub pre_compact: Vec<HookEntry>,
    /// Fires after a `/compact` recorded its summary, with the turns folded
    /// and the summary's size. Notification-style, same as `pre_compact`.
    pub post_compact: Vec<HookEntry>,
    /// Fires when a prompt is submitted, before a turn starts (headless
    /// `rapid exec`, the TUI composer, a queued message). A v2 `deny`
    /// blocks the prompt with its reason; a queued prompt that is blocked
    /// stays queued, held (ADR 0022 §7).
    pub user_prompt_submit: Vec<HookEntry>,
    /// Fires when a turn completes. Exclusive with `stop_cancelled`: one
    /// turn-end site chooses which. Observes; never gates.
    pub stop: Vec<HookEntry>,
    /// Fires *instead of* `stop` when a turn ends without completing —
    /// interrupted, paused for approval, out of budget, a failed model or
    /// tool step — with the reason. Observes; never gates.
    pub stop_cancelled: Vec<HookEntry>,
    /// Fires when a tool call failed (a dispatch failure or an MCP error
    /// result). May add context after the failure; cannot block.
    pub post_tool_use_failure: Vec<HookEntry>,
}

/// Number of hook stages [`HooksConfig`] carries.
pub const HOOK_STAGE_COUNT: usize = 12;

/// Collect command lines into entries: for building a [`HooksConfig`] from
/// plain commands.
pub fn entries<I, S>(commands: I) -> Vec<HookEntry>
where
    I: IntoIterator<Item = S>,
    S: Into<HookEntry>,
{
    commands.into_iter().map(Into::into).collect()
}

impl HooksConfig {
    /// Parse the `hooks` object from a settings document value. Entries the
    /// object form rejects are dropped here; [`Self::parse_reporting`] says
    /// which and why.
    pub fn parse(value: &serde_json::Value) -> Option<HooksConfig> {
        Self::parse_reporting(value).map(|(config, _)| config)
    }

    /// [`Self::parse`] with the object-form entries that were rejected. A
    /// malformed *string* entry is skipped silently, as it always was; an
    /// object entry with an unknown key, a missing command or a value out of
    /// range is rejected with its position — a gating hook that silently
    /// vanished would be worse than a loud error.
    pub fn parse_reporting(value: &serde_json::Value) -> Option<(HooksConfig, Vec<HookRejection>)> {
        let object = value.get("hooks")?.as_object()?;
        let mut config = HooksConfig::default();
        let mut rejections = Vec::new();
        for (key, target) in config.named_stages_mut() {
            let Some(entries) = object.get(key).and_then(serde_json::Value::as_array) else {
                continue;
            };
            for (position, entry) in entries.iter().enumerate() {
                // Rejected entries are not kept, so the per-stage cap on kept
                // entries does not bound the work a long array costs; this
                // does (each object may compile an expression).
                if position >= MAX_ENTRIES_EXAMINED_PER_STAGE {
                    rejections.push(HookRejection {
                        stage: key,
                        position,
                        reason: format!(
                            "more than {MAX_ENTRIES_EXAMINED_PER_STAGE} entries; the rest are not read"
                        ),
                    });
                    break;
                }
                let parsed = match entry {
                    serde_json::Value::String(command) => {
                        if command.is_empty() || command.len() > MAX_HOOK_COMMAND_BYTES {
                            continue;
                        }
                        HookEntry::plain(command.clone())
                    }
                    serde_json::Value::Object(map) => match parse_object_entry(key, map) {
                        Ok(entry) => entry,
                        Err(reason) => {
                            rejections.push(HookRejection {
                                stage: key,
                                position,
                                reason,
                            });
                            continue;
                        }
                    },
                    _ => continue,
                };
                if target.len() >= MAX_HOOKS_PER_STAGE {
                    break;
                }
                target.push(parsed);
            }
        }
        Some((config, rejections))
    }

    /// Every stage, in declaration order — the one list a merge or a cap
    /// walks, so a stage added to the struct cannot be left out of either
    /// (which is how `pre_compact`/`post_compact` parsed and then vanished
    /// in the project-settings merge).
    pub fn stages_mut(&mut self) -> [&mut Vec<HookEntry>; HOOK_STAGE_COUNT] {
        [
            &mut self.pre_tool_use,
            &mut self.post_tool_use,
            &mut self.session_start,
            &mut self.session_end,
            &mut self.subagent_start,
            &mut self.subagent_stop,
            &mut self.pre_compact,
            &mut self.post_compact,
            &mut self.user_prompt_submit,
            &mut self.stop,
            &mut self.stop_cancelled,
            &mut self.post_tool_use_failure,
        ]
    }

    /// Every stage with its settings key, mutably (for a gate that drops a
    /// stage by name).
    pub fn stages_named_mut(&mut self) -> [(&'static str, &mut Vec<HookEntry>); HOOK_STAGE_COUNT] {
        self.named_stages_mut()
    }

    fn named_stages_mut(&mut self) -> [(&'static str, &mut Vec<HookEntry>); HOOK_STAGE_COUNT] {
        let names = HooksConfig::default().named_stages().map(|(name, _)| name);
        let stages = self.stages_mut();
        let mut index = 0;
        stages.map(|stage| {
            let name = names[index];
            index += 1;
            (name, stage)
        })
    }

    /// Every stage with its settings key, in declaration order — what a
    /// reader (`rapid doctor`, a managed gate) walks, so a new stage cannot
    /// be listed in one place and forgotten in another.
    pub fn named_stages(&self) -> [(&'static str, &Vec<HookEntry>); HOOK_STAGE_COUNT] {
        [
            ("pre_tool_use", &self.pre_tool_use),
            ("post_tool_use", &self.post_tool_use),
            ("session_start", &self.session_start),
            ("session_end", &self.session_end),
            ("subagent_start", &self.subagent_start),
            ("subagent_stop", &self.subagent_stop),
            ("pre_compact", &self.pre_compact),
            ("post_compact", &self.post_compact),
            ("user_prompt_submit", &self.user_prompt_submit),
            ("stop", &self.stop),
            ("stop_cancelled", &self.stop_cancelled),
            ("post_tool_use_failure", &self.post_tool_use_failure),
        ]
    }

    /// Append every stage of `other` to this config.
    pub fn extend(&mut self, mut other: HooksConfig) {
        for (stage, more) in self.stages_mut().into_iter().zip(other.stages_mut()) {
            stage.append(more);
        }
    }

    /// After a merge: entries identical in every field run once (the first
    /// wins), then each stage is held to [`MAX_HOOKS_PER_STAGE`].
    pub fn dedupe_and_cap(&mut self) {
        self.dedupe_only();
        for stage in self.stages_mut() {
            stage.truncate(MAX_HOOKS_PER_STAGE);
        }
    }

    /// Entries identical in every field run once; the first wins.
    pub fn dedupe_only(&mut self) {
        for stage in self.stages_mut() {
            let mut kept: Vec<HookEntry> = Vec::with_capacity(stage.len());
            for entry in stage.drain(..) {
                if !kept.contains(&entry) {
                    kept.push(entry);
                }
            }
            *stage = kept;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.named_stages()
            .iter()
            .all(|(_, commands)| commands.is_empty())
    }
}

const OBJECT_ENTRY_KEYS: [&str; 4] = ["command", "matcher", "timeout_ms", "continue_turn"];

/// One object-form entry, or why it is rejected.
pub fn parse_object_entry(
    stage: &'static str,
    map: &serde_json::Map<String, serde_json::Value>,
) -> Result<HookEntry, String> {
    parse_object_entry_for(stage, map, PROCESS_GROUP_TERMINATION)
}

/// [`parse_object_entry`] for a platform that can or cannot end a timed-out
/// hook's children, so the refusal on one that cannot is exercised on all.
fn parse_object_entry_for(
    stage: &'static str,
    map: &serde_json::Map<String, serde_json::Value>,
    group_termination: bool,
) -> Result<HookEntry, String> {
    if let Some(unknown) = map
        .keys()
        .find(|key| !OBJECT_ENTRY_KEYS.contains(&key.as_str()))
    {
        return Err(format!(
            "unknown key {:?}; an entry takes command, matcher, timeout_ms and continue_turn",
            truncate(unknown.as_bytes(), 40)
        ));
    }
    let command = match map.get("command") {
        Some(serde_json::Value::String(command))
            if !command.trim().is_empty() && command.len() <= MAX_HOOK_COMMAND_BYTES =>
        {
            command.clone()
        }
        Some(serde_json::Value::String(command)) if command.trim().is_empty() => {
            return Err("`command` is empty".to_owned());
        }
        Some(serde_json::Value::String(_)) => {
            return Err(format!(
                "`command` is over the {MAX_HOOK_COMMAND_BYTES}-byte limit"
            ));
        }
        Some(_) => return Err("`command` is not a string".to_owned()),
        None => return Err("`command` is missing".to_owned()),
    };
    let timeout_ms = match map.get("timeout_ms") {
        None => None,
        Some(value) => {
            let max = stage_max_timeout_ms(stage);
            match value.as_u64() {
                Some(ms) if (1..=max).contains(&ms) => {
                    timeout_allowed(ms, group_termination)?;
                    Some(ms)
                }
                Some(_) => {
                    return Err(format!(
                        "`timeout_ms` must be between 1 and {max} for the {stage} stage"
                    ));
                }
                None => return Err("`timeout_ms` is not a positive integer".to_owned()),
            }
        }
    };
    let continue_turn = match map.get("continue_turn") {
        None => false,
        Some(serde_json::Value::Bool(flag)) => {
            if *flag && stage != "stop" {
                return Err("`continue_turn` applies to the stop stage only".to_owned());
            }
            *flag
        }
        Some(_) => return Err("`continue_turn` is not a boolean".to_owned()),
    };
    // Last: compiling the expression is the costly check, so a rejection for
    // any other reason never pays for it.
    let matcher = match map.get("matcher") {
        None => None,
        Some(serde_json::Value::String(source)) => {
            if stage_subject(stage).is_none() {
                return Err(format!(
                    "the {stage} stage has no subject, so `matcher` does not apply"
                ));
            }
            Some(HookMatcher::new(source)?)
        }
        Some(_) => return Err("`matcher` is not a string".to_owned()),
    };
    Ok(HookEntry {
        command,
        matcher,
        timeout_ms,
        continue_turn,
    })
}

/// Outcome of a pre-tool-use hook stage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreHookOutcome {
    /// Every hook allowed (or deferred on) the call.
    Allowed,
    /// A hook denied the call; carries its bounded reason (a v2 `reason`,
    /// or the v1 stderr).
    Denied { reason: String },
    /// A v2 hook asked for a human decision and no hook denied. `hook`
    /// names the asking hook (`pre_tool_use[<index>]`); `command_digest`
    /// identifies its command line, so an approval answers this hook and
    /// not whichever hook later sits at its position.
    Ask {
        hook: String,
        command_digest: String,
        reason: String,
    },
}

/// One v2 decision a hook made, in the shape the ledger records
/// (`hook.decided`, `rapidlm.hook.decision/v1`). v1 hooks — exit code only,
/// nothing structured on stdout — produce no record, so a project whose
/// hooks never print a result has a ledger identical to before.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookDecisionRecord {
    /// `pre_tool_use[<index>]` — the stage and the hook's position in it.
    pub hook: String,
    /// The stage that ran (`pre_tool_use`).
    pub event: &'static str,
    /// First twelve hex digits of the SHA-256 of the hook command line, so
    /// a reader can tell which command a position referred to after the
    /// settings file changed.
    pub command_digest: String,
    pub decision: HookDecision,
    pub reason: Option<String>,
    /// The result carried a grant-shaped key (ignored; recorded).
    pub grant_attempted: bool,
}

/// A pre-tool stage's outcome together with every v2 decision made on the
/// way to it, for the caller to record, and the input rewrite that survived
/// the stage (ADR 0022 §5): the last rewriting hook wins, a denial discards
/// every rewrite, and an `ask` keeps the rewrite so the human is shown the
/// call that would actually run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreHookReport {
    pub outcome: PreHookOutcome,
    pub decisions: Vec<HookDecisionRecord>,
    pub rewrite: Option<HookRewrite>,
    /// Every `additional_context` the stage's hooks returned, in hook order
    /// (ADR 0022 §6). Delivered after the tool result, fenced as untrusted;
    /// a denial delivers none — there is no result to follow.
    pub context: Vec<HookContext>,
}

/// An `additional_context` a hook returned: bounded at parse
/// ([`protocol::MAX_HOOK_CONTEXT_BYTES`]), delivered to the model only inside
/// the untrusted-content fence, naming the hook.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookContext {
    /// `<stage>[<index>]`.
    pub hook: String,
    pub text: String,
}

impl HookContext {
    /// The fenced block the model sees — the same fence retrieved untrusted
    /// context already uses (`<untrusted_context locator="…">`), so the
    /// boundary is structural and source-independent: instructions inside
    /// are data.
    pub fn fenced(&self) -> String {
        crate::model::fence_untrusted(&format!("hook:{}", self.hook), &self.text)
    }
}

/// A post-tool stage's report: the v1 text (every plain line the hooks
/// printed, as before) and the v2 context and decisions.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct PostHookReport {
    pub output: String,
    pub context: Vec<HookContext>,
    /// v2 decisions the stage's hooks made. Whether a `deny` has an effect is
    /// the stage's own rule: none for `post_tool_use`, `post_tool_use_failure`,
    /// `stop` and `stop_cancelled`; a block for `user_prompt_submit` and
    /// `subagent_stop`.
    pub decisions: Vec<HookDecisionRecord>,
    /// Hooks that failed (non-zero exit, crash, timeout, unreadable result):
    /// these stages fail open, so each is a recorded warning (`hook.failed`),
    /// never an effect.
    pub failures: Vec<HookFailure>,
}

impl PostHookReport {
    /// The first `deny` the stage's hooks returned, as `(hook, reason)`.
    pub fn first_deny(&self) -> Option<(String, String)> {
        self.decisions
            .iter()
            .find(|decision| decision.decision == HookDecision::Deny)
            .map(|decision| {
                let reason = decision
                    .reason
                    .clone()
                    .unwrap_or_else(|| format!("denied by {} hook", decision.hook));
                (decision.hook.clone(), reason)
            })
    }
}

/// A hook in a fail-open stage that did not produce a usable result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookFailure {
    /// `<stage>[<index>]`.
    pub hook: String,
    pub event: &'static str,
    pub command_digest: String,
    /// Bounded: what went wrong (the hook's stderr, "hook timed out", the
    /// parse error).
    pub detail: String,
}

/// How a turn ended, for the exclusive `stop` / `stop_cancelled` choice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnEnd {
    Completed,
    /// Ended without completing; `reason` is a stable token
    /// (`cancelled`, `approval_required`, `budget_exhausted`, …; `failed` for
    /// an outcome with no stop reason, `error` when the turn failed before
    /// producing one).
    Cancelled {
        reason: String,
    },
}

/// An `updated_input` a hook returned: the hook that returned it and the
/// object that replaces the call's arguments. Validation against the tool's
/// own argument parser is the caller's (the stage does not know the tools).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookRewrite {
    /// `pre_tool_use[<index>]` — the last hook that rewrote.
    pub hook: String,
    pub command_digest: String,
    pub input: serde_json::Map<String, serde_json::Value>,
    /// Every hook whose rewrite shaped `input`, in order (each saw the call
    /// as the hooks before it left it).
    pub contributors: Vec<String>,
}

/// What one hook run produced: whether it exited zero, its stdout (the
/// result channel, bounded by [`MAX_HOOK_RESULT_BYTES`]) and its stderr
/// (the detail channel, bounded by [`MAX_HOOK_STDERR_BYTES`]).
#[derive(Clone, Debug, PartialEq, Eq)]
struct HookRun {
    ok: bool,
    /// The hook was killed at the timeout. Whatever it printed is at best
    /// partial: never a decision, still a diagnostic.
    timed_out: bool,
    stdout: Vec<u8>,
    stderr: String,
}

impl HookRun {
    fn failed(reason: String) -> Self {
        Self {
            ok: false,
            timed_out: false,
            stdout: Vec::new(),
            stderr: reason,
        }
    }

    /// The v1 detail: stderr, or stdout when the hook wrote its reason there
    /// (both streams used to land in one capture, so a hook that spoke on
    /// stdout keeps being heard).
    fn detail(&self) -> String {
        if !self.stderr.is_empty() {
            return self.stderr.clone();
        }
        truncate(&self.stdout, MAX_HOOK_STDERR_BYTES)
    }

    /// Both streams as one bounded text, stdout first — the shape the
    /// notification and post-tool stages have always recorded.
    fn combined(&self) -> String {
        let mut text = truncate(&self.stdout, MAX_HOOK_STDERR_BYTES);
        if !self.stderr.is_empty() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&self.stderr);
        }
        truncate(text.as_bytes(), MAX_HOOK_STDERR_BYTES)
    }
}

/// `pre_tool_use[<index>]`.
fn hook_name(stage: &str, index: usize) -> String {
    format!("{stage}[{index}]")
}

/// First twelve hex digits of the SHA-256 of the hook command line.
fn command_digest(command: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(command.as_bytes());
    let mut hex = String::with_capacity(12);
    for byte in &digest[..6] {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// The shell a hook line runs under, with the environment cleared down to
/// what a shell needs to find programs and a temp directory. Which shell
/// and which variables is the only platform difference in running a hook.
fn hook_shell(command: &str) -> Command {
    #[cfg(unix)]
    let (shell, flag, keep): (&str, &str, &[&str]) =
        ("sh", "-c", &["PATH", "HOME", "LANG", "TMPDIR"]);
    #[cfg(not(unix))]
    let (shell, flag, keep): (&str, &str, &[&str]) = ("cmd", "/C", &["PATH"]);
    let mut builder = Command::new(shell);
    builder.arg(flag).arg(command).env_clear();
    for key in keep {
        if let Ok(value) = std::env::var(key) {
            let _ = builder.env(key, value);
        }
    }
    // What the OS itself needs to start a child (`SystemRoot`, `COMSPEC`,
    // `TEMP`, … on Windows; nothing on Unix), stated once in `host_env`.
    for (key, value) in protocol::host_env::platform_base_env() {
        let _ = builder.env(key, value);
    }
    for (key, value) in crate::run_identity::env() {
        let _ = builder.env(key, value);
    }
    builder
}

/// Run one hook command with `input_json` on stdin. A missing/failed spawn
/// counts as failed with a static reason (never a panic).
///
/// `own_group` puts the hook in a process group of its own so a timeout can
/// end the whole tree it started. It is for a hook whose entry set its own
/// `timeout_ms`: a plain hook stays in the caller's group exactly as it
/// always did, so a terminal's Ctrl-C, a group kill by a supervisor, or a hook
/// that reads the terminal behave as before. A hook in its own group is
/// bounded by its timeout and is not reached by a terminal signal.
fn run_hook_once(command: &str, input_json: &str, timeout: Duration, own_group: bool) -> HookRun {
    // Output goes to temp files rather than our pipes: a hook that
    // backgrounds its own children (`sleep 30 &`) would otherwise hold the
    // pipe write-end open past the kill, blocking EOF collection. Two files,
    // because stdout is the result channel and stderr the detail channel
    // (a v2 result printed beside a warning must still parse).
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let stdout_path =
        std::env::temp_dir().join(format!("rapidlm-hook-out-{}-{seq}", std::process::id()));
    let stderr_path =
        std::env::temp_dir().join(format!("rapidlm-hook-err-{}-{seq}", std::process::id()));
    let cleanup = || {
        let _ = std::fs::remove_file(&stdout_path);
        let _ = std::fs::remove_file(&stderr_path);
    };
    let stdout_file = match std::fs::File::create(&stdout_path) {
        Ok(file) => file,
        Err(err) => return HookRun::failed(format!("hook output file failed: {err}")),
    };
    let stderr_file = match std::fs::File::create(&stderr_path) {
        Ok(file) => file,
        Err(err) => {
            cleanup();
            return HookRun::failed(format!("hook output file failed: {err}"));
        }
    };
    // The stdio wiring is the contract — input JSON on stdin, everything the
    // hook prints into the files — and is the same on every platform. Only
    // the shell differs. (The Windows arm used to spawn bare: no stdin, so
    // the hook never received its payload, and no redirection, so its
    // output went to the TUI's own terminal and the file read back empty.)
    let mut builder = hook_shell(command);
    if own_group {
        process_signal::isolate_process_group(&mut builder);
    }
    let spawn = builder
        .stdin(Stdio::piped())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .spawn();
    let mut child = match spawn {
        Ok(child) => child,
        Err(err) => {
            cleanup();
            return HookRun::failed(format!("hook spawn failed: {err}"));
        }
    };
    // Write stdin on its own thread rather than blocking here: `write_all`
    // on a piped child stdin has no timeout of its own, so a payload larger
    // than the OS pipe buffer combined with a hook that never reads stdin
    // (the overwhelmingly common case — most hooks only care about argv/the
    // command's own output) would otherwise block synchronously, before the
    // timeout clock below even starts, hanging the turn despite this
    // module's own "bounded by a timeout" contract (see the module doc).
    // The child's stdin handle is moved into the thread, so dropping it
    // there still gives the hook EOF; if the child is killed on timeout
    // while the write is still blocked, closing its stdin fd unblocks the
    // writer thread with a broken-pipe error, which is ignored below.
    if let Some(mut stdin) = child.stdin.take() {
        let input_json = input_json.to_owned();
        std::thread::spawn(move || {
            let _ = stdin.write_all(input_json.as_bytes());
            let _ = stdin.flush();
        });
    }
    let collect = |ok: bool| {
        let stdout = crate::exec_tools::read_capped_bytes(&stdout_path, MAX_HOOK_RESULT_BYTES + 1);
        let stderr = crate::exec_tools::read_capped_bytes(&stderr_path, MAX_HOOK_STDERR_BYTES);
        cleanup();
        HookRun {
            ok,
            timed_out: false,
            stdout,
            stderr: truncate(&stderr, MAX_HOOK_STDERR_BYTES),
        }
    };
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return collect(status.success()),
            Ok(None) => {
                if started.elapsed() > timeout {
                    if own_group {
                        process_signal::terminate_process_group_default(&mut child);
                    } else {
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(err) => {
                cleanup();
                return HookRun::failed(format!("hook wait failed: {err}"));
            }
        }
    }
    let mut run = collect(false);
    run.timed_out = true;
    if run.stderr.is_empty() && run.stdout.is_empty() {
        run.stderr = "hook timed out".to_owned();
    }
    run
}

/// Run every `pre_tool_use` hook for one tool call. Input JSON:
/// `{"tool": name, "arguments": <raw arguments value>}`. The first denial
/// wins. Kept for callers that only need the outcome; the stage's decision
/// records are in [`run_pre_tool_stage`].
pub fn run_pre_tool_hooks<H: HookSpec>(
    hooks: &[H],
    tool: &str,
    arguments: &str,
    timeout: Duration,
) -> PreHookOutcome {
    run_pre_tool_stage(hooks, tool, arguments, timeout).outcome
}

/// Run every `pre_tool_use` hook for one tool call and report every v2
/// decision made on the way (ADR 0022 §1–4).
///
/// Per hook, in declaration order:
/// - a non-zero exit, a crash or a timeout **denies** with the hook's stderr
///   (v1 contract, unchanged), whatever its stdout says;
/// - stdout with no structured result is v1: exit zero allows;
/// - a v2 `deny` denies with its `reason`; a v2 result the binary cannot read
///   (unknown decision, wrong schema, newer version) denies naming the hook —
///   a hook speaking an unknown contract cannot be assumed to have allowed;
/// - `allow` and `defer` continue to the next hook (`defer` states no
///   opinion: the normal permission flow decides);
/// - `ask` is remembered and reported only if no later hook denies.
///
/// A denial ends the stage: later hooks do not run (they never did).
pub fn run_pre_tool_stage<H: HookSpec>(
    hooks: &[H],
    tool: &str,
    arguments: &str,
    timeout: Duration,
) -> PreHookReport {
    const STAGE: &str = "pre_tool_use";
    let mut decisions = Vec::new();
    let mut ask: Option<(String, String, String)> = None;
    let mut rewrite: Option<HookRewrite> = None;
    let mut context: Vec<HookContext> = Vec::new();
    for (index, hook) in hooks.iter().enumerate() {
        // A hook whose matcher does not select this tool is skipped without
        // a record; its position is still its place in the configured list.
        if !hook.applies_to(Some(tool)) {
            continue;
        }
        let command = hook.command();
        let timeout = hook.timeout_override().unwrap_or(timeout);
        let name = hook_name(STAGE, index);
        // Each hook sees the call as it currently stands: an earlier hook's
        // rewrite is what a later hook evaluates (and may ask about), not
        // the model's original the later hook would otherwise judge blind.
        let current_arguments = match &rewrite {
            Some(rewrite) => serde_json::Value::Object(rewrite.input.clone()).to_string(),
            None => arguments.to_owned(),
        };
        let input = format!(r#"{{"tool":"{tool}","arguments":{current_arguments}}}"#);
        let run = run_hook_once(command, &input, timeout, hook.timeout_override().is_some());
        if !run.ok {
            // Fail-closed whatever stdout says. A v2 result printed beside
            // the non-zero exit is still a decision the hook made, so it is
            // recorded — and a `deny` lends its reason — but only the exit
            // code decides the outcome, and it says deny. A timed-out hook
            // printed nothing that counts as a decision; its stdout is a
            // diagnostic like its stderr.
            let result = if run.timed_out {
                None
            } else {
                HookResult::from_stdout(&run.stdout).ok().flatten()
            };
            let mut reason = None;
            if let Some(result) = result {
                if result.decision == HookDecision::Deny {
                    reason = result.reason.clone().filter(|r| !r.is_empty());
                } else {
                    reason = Some(format!(
                        "hook exited non-zero (its result said {}; the exit code decides)",
                        result.decision
                    ));
                }
                decisions.push(HookDecisionRecord {
                    hook: name.clone(),
                    event: STAGE,
                    command_digest: command_digest(command),
                    decision: result.decision,
                    reason: result.reason.filter(|r| !r.is_empty()),
                    grant_attempted: result.grant_attempted,
                });
            }
            let reason = reason.unwrap_or_else(|| {
                let detail = run.detail();
                if detail.is_empty() {
                    // A silent failure still denies with a static reason.
                    "hook exited non-zero".to_owned()
                } else if run.timed_out {
                    format!("hook timed out: {detail}")
                } else {
                    detail
                }
            });
            // A denial discards every rewrite: nothing runs.
            return PreHookReport {
                outcome: PreHookOutcome::Denied {
                    reason: truncate(reason.as_bytes(), MAX_HOOK_STDERR_BYTES),
                },
                decisions,
                rewrite: None,
                context: Vec::new(),
            };
        }
        let result = match HookResult::from_stdout(&run.stdout) {
            Ok(None) => continue,
            Ok(Some(result)) => result,
            Err(err) => {
                return PreHookReport {
                    outcome: PreHookOutcome::Denied {
                        reason: unreadable_result_reason(&name, &err),
                    },
                    decisions,
                    rewrite: None,
                    context: Vec::new(),
                };
            }
        };
        decisions.push(HookDecisionRecord {
            hook: name.clone(),
            event: STAGE,
            command_digest: command_digest(command),
            decision: result.decision,
            reason: result.reason.clone().filter(|r| !r.is_empty()),
            grant_attempted: result.grant_attempted,
        });
        if let Some(text) = result
            .additional_context
            .clone()
            .filter(|text| !text.trim().is_empty())
        {
            context.push(HookContext {
                hook: name.clone(),
                text,
            });
        }
        // The last rewriting hook wins (a later hook that rewrites nothing
        // leaves an earlier rewrite standing); a `deny` below discards it.
        if let Some(input) = result.updated_input.clone() {
            // An `updated_input` equal to what the hook was shown (the
            // pass-through idiom) is not a rewrite: nothing to journal or
            // mark.
            let unchanged = serde_json::from_str::<serde_json::Value>(&current_arguments)
                .ok()
                .is_some_and(|shown| shown == serde_json::Value::Object(input.clone()));
            if !unchanged {
                let mut contributors = rewrite
                    .take()
                    .map(|earlier| earlier.contributors)
                    .unwrap_or_default();
                contributors.push(name.clone());
                rewrite = Some(HookRewrite {
                    hook: name.clone(),
                    command_digest: command_digest(command),
                    input,
                    contributors,
                });
            }
        }
        match result.decision {
            HookDecision::Allow | HookDecision::Defer => {}
            HookDecision::Deny => {
                let reason = result
                    .reason
                    .filter(|r| !r.is_empty())
                    .unwrap_or_else(|| format!("denied by {name} hook"));
                return PreHookReport {
                    outcome: PreHookOutcome::Denied { reason },
                    decisions,
                    rewrite: None,
                    context: Vec::new(),
                };
            }
            HookDecision::Ask => {
                if ask.is_none() {
                    let reason = result
                        .reason
                        .filter(|r| !r.is_empty())
                        .unwrap_or_else(|| format!("{name} hook asked for approval"));
                    ask = Some((name, command_digest(command), reason));
                }
            }
        }
    }
    let outcome = match ask {
        Some((hook, command_digest, reason)) => PreHookOutcome::Ask {
            hook,
            command_digest,
            reason,
        },
        None => PreHookOutcome::Allowed,
    };
    // A chain of rewrites that ends where it started is no rewrite.
    let original = serde_json::from_str::<serde_json::Value>(arguments).ok();
    let rewrite = rewrite.filter(|rewrite| {
        original.as_ref() != Some(&serde_json::Value::Object(rewrite.input.clone()))
    });
    PreHookReport {
        outcome,
        decisions,
        rewrite,
        context,
    }
}

/// The denial reason for a hook whose stdout declared itself a result this
/// binary cannot read.
fn unreadable_result_reason(hook: &str, err: &HookResultError) -> String {
    truncate(
        format!("{hook} hook printed an unreadable result ({err}); the call is denied").as_bytes(),
        MAX_HOOK_STDERR_BYTES,
    )
}

/// Run every `post_tool_use` hook; returns their combined v1 output
/// (bounded). Kept for callers that only need the text; the v2 context and
/// decisions are in [`run_post_tool_stage`].
pub fn run_post_tool_hooks<H: HookSpec>(
    hooks: &[H],
    tool: &str,
    summary: &str,
    timeout: Duration,
) -> String {
    run_post_tool_stage(hooks, tool, summary, timeout).output
}

/// Run every `post_tool_use` hook for a completed call. Input JSON:
/// `{"tool": name, "summary": <the result the model will see>}`. A hook
/// that prints plain text has it recorded on the result as before (v1); a
/// hook that prints a v2 result has its `additional_context` delivered
/// fenced and its decision recorded — a post hook observes, it cannot block
/// (ADR 0022 §6–7).
pub fn run_post_tool_stage<H: HookSpec>(
    hooks: &[H],
    tool: &str,
    summary: &str,
    timeout: Duration,
) -> PostHookReport {
    // Proper JSON serialization: a hand-rolled `"{escaped}"` format only
    // escaped `\` and `"`, so a summary containing a raw newline (e.g.
    // `execute_shell`'s `"exit {code}\n{output}"`) produced invalid JSON.
    // `serde_json` escapes every control character RFC 8259 requires.
    let input = serde_json::json!({ "tool": tool, "summary": summary }).to_string();
    run_observer_stage("post_tool_use", hooks, Some(tool), &input, timeout)
}

/// Run every `post_tool_use_failure` hook for a failed call. Input JSON:
/// `{"event":"post_tool_use_failure","tool":name,"error":<the failure the
/// model will see>}`. May add context after the failure; cannot block.
pub fn run_post_tool_failure_stage<H: HookSpec>(
    hooks: &[H],
    tool: &str,
    error: &str,
    timeout: Duration,
) -> PostHookReport {
    let input = serde_json::json!({
        "event": "post_tool_use_failure",
        "tool": tool,
        "error": error,
    })
    .to_string();
    run_observer_stage("post_tool_use_failure", hooks, Some(tool), &input, timeout)
}

/// Run every `user_prompt_submit` hook for a prompt about to start a turn.
/// Input JSON: `{"event":"user_prompt_submit","prompt":text}`. A v2 `deny`
/// blocks the prompt ([`PostHookReport::first_deny`]); `allow`/`defer`
/// let it through; `ask` has no one to ask — the prompt's author is the
/// human — and is recorded without effect. A hook that fails lets the prompt
/// through with a recorded warning (fail open, like every non-tool stage).
pub fn run_prompt_submit_stage<H: HookSpec>(
    hooks: &[H],
    prompt: &str,
    timeout: Duration,
) -> PostHookReport {
    let input = serde_json::json!({ "event": "user_prompt_submit", "prompt": prompt }).to_string();
    run_observer_stage("user_prompt_submit", hooks, None, &input, timeout)
}

/// Run the one turn-end stage `end` selects — `stop` for a completed turn,
/// `stop_cancelled` otherwise, never both. Input JSON:
/// `{"event":"stop"|"stop_cancelled","reason"?:…,"tool_calls":n,"tokens":n}`.
/// Observes; never gates.
pub fn run_turn_end_stage(
    hooks: &HooksConfig,
    end: &TurnEnd,
    tool_calls: u32,
    tokens: u64,
    timeout: Duration,
) -> PostHookReport {
    let (stage, commands, subject, input) = match end {
        TurnEnd::Completed => (
            "stop",
            &hooks.stop,
            None,
            serde_json::json!({
                "event": "stop",
                "tool_calls": tool_calls,
                "tokens": tokens,
            }),
        ),
        TurnEnd::Cancelled { reason } => (
            "stop_cancelled",
            &hooks.stop_cancelled,
            Some(reason.as_str()),
            serde_json::json!({
                "event": "stop_cancelled",
                "reason": reason,
                "tool_calls": tool_calls,
                "tokens": tokens,
            }),
        ),
    };
    run_observer_stage(stage, commands, subject, &input.to_string(), timeout)
}

/// Run every `subagent_stop` hook for a finished `task_spawn` child. Input
/// JSON: `{"event":"subagent_stop","agent_type":…,"status":…,"ok":bool}`.
/// A v2 `deny` blocks the child's completion: the parent receives a failure
/// naming the hook instead of the child's report
/// ([`PostHookReport::first_deny`]).
pub fn run_subagent_stop_stage<H: HookSpec>(
    hooks: &[H],
    agent_type: &str,
    status: &str,
    ok: bool,
    timeout: Duration,
) -> PostHookReport {
    let input = serde_json::json!({
        "event": "subagent_stop",
        "agent_type": agent_type,
        "status": status,
        "ok": ok,
    })
    .to_string();
    run_observer_stage("subagent_stop", hooks, Some(agent_type), &input, timeout)
}

/// The fail-open stage runner every observing stage shares: each hook gets
/// `input` on stdin; a v2 result (from a hook that exited zero and was not
/// killed) contributes its context and decision; plain text is the v1
/// output; a hook that failed or printed an unreadable result is a
/// [`HookFailure`] — a recorded warning, never an effect.
fn run_observer_stage<H: HookSpec>(
    stage: &'static str,
    hooks: &[H],
    subject: Option<&str>,
    input: &str,
    timeout: Duration,
) -> PostHookReport {
    let mut report = PostHookReport::default();
    for (index, hook) in hooks.iter().enumerate() {
        if !hook.applies_to(subject) {
            continue;
        }
        let command = hook.command();
        let timeout = hook.timeout_override().unwrap_or(timeout);
        let name = hook_name(stage, index);
        let run = run_hook_once(command, input, timeout, hook.timeout_override().is_some());
        if !run.ok {
            let detail = run.detail();
            let detail = if detail.is_empty() {
                "hook exited non-zero".to_owned()
            } else {
                detail
            };
            report.failures.push(HookFailure {
                hook: name,
                event: stage,
                command_digest: command_digest(command),
                detail: truncate(detail.as_bytes(), MAX_HOOK_STDERR_BYTES),
            });
            // A failing hook's text is still what it printed: recorded like
            // any v1 output, as it always was for `post_tool_use`.
            append_output(&mut report.output, &run.combined());
            continue;
        }
        match HookResult::from_stdout(&run.stdout) {
            Ok(Some(result)) => {
                if let Some(text) = result
                    .additional_context
                    .filter(|text| !text.trim().is_empty())
                {
                    report.context.push(HookContext {
                        hook: name.clone(),
                        text,
                    });
                }
                report.decisions.push(HookDecisionRecord {
                    hook: name,
                    event: stage,
                    command_digest: command_digest(command),
                    decision: result.decision,
                    reason: result.reason.filter(|r| !r.is_empty()),
                    grant_attempted: result.grant_attempted,
                });
            }
            Ok(None) => append_output(&mut report.output, &run.combined()),
            Err(err) => {
                report.failures.push(HookFailure {
                    hook: name,
                    event: stage,
                    command_digest: command_digest(command),
                    detail: truncate(
                        format!("unreadable result ({err})").as_bytes(),
                        MAX_HOOK_STDERR_BYTES,
                    ),
                });
                // What it printed is still recorded, as v1 recorded it.
                append_output(&mut report.output, &run.combined());
            }
        }
    }
    report
}

fn append_output(combined: &mut String, output: &str) {
    if !output.is_empty() && combined.len() < MAX_HOOK_STDERR_BYTES {
        if !combined.is_empty() {
            combined.push_str("; ");
        }
        combined.push_str(output);
    }
}

/// Run every hook for a notification-style event (`session_start`,
/// `subagent_start`, `subagent_stop`, ...): fire-and-collect, never gates —
/// unlike `pre_tool_use`, a non-zero exit here is not a denial. `payload` is
/// serialized as the hook's stdin JSON (only `event` is added to it here, so
/// callers pass their own event-specific fields already, matching
/// `run_post_tool_hooks`'s `{"tool":...,"summary":...}` shape rather than a
/// generic wrapper).
pub fn run_notify_hooks<H: HookSpec>(
    hooks: &[H],
    event: &str,
    subject: Option<&str>,
    payload: serde_json::Value,
    timeout: Duration,
) -> String {
    let mut input = match payload {
        serde_json::Value::Object(map) => map,
        other => {
            let mut map = serde_json::Map::new();
            map.insert("payload".to_owned(), other);
            map
        }
    };
    input.insert(
        "event".to_owned(),
        serde_json::Value::String(event.to_owned()),
    );
    let input = serde_json::Value::Object(input).to_string();
    let mut combined = String::new();
    for hook in hooks {
        if !hook.applies_to(subject) {
            continue;
        }
        let timeout = hook.timeout_override().unwrap_or(timeout);
        let output = run_hook_once(
            hook.command(),
            &input,
            timeout,
            hook.timeout_override().is_some(),
        )
        .combined();
        if !output.is_empty() && combined.len() < MAX_HOOK_STDERR_BYTES {
            if !combined.is_empty() {
                combined.push_str("; ");
            }
            combined.push_str(&output);
        }
    }
    combined
}

fn truncate(bytes: &[u8], cap: usize) -> String {
    let mut end = bytes.len().min(cap);
    while end > 0 && std::str::from_utf8(&bytes[..end]).is_err() {
        end -= 1;
    }
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(dir: &std::path::Path, name: &str, body: &str) -> String {
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        // The hook line is handed to `sh -c` on Unix and `cmd /C` on
        // Windows. Unquoted: `cmd /C` re-escapes double quotes on their
        // way into `sh` (Windows CI, 2026-09-17: `sh: \C:/…/ok.sh": No such
        // file`), and the CI temp path has no spaces. Forward slashes so
        // `sh` on Windows does not read the backslashes as escapes. A
        // profile with a space in its temp path is a known limitation of
        // these fixtures, not of hooks (a user's hook line is their own).
        format!("sh {}", test_fixtures::slash_path(&path))
    }

    #[test]
    fn parse_settings_hooks_object() {
        let value: serde_json::Value = serde_json::from_str(
            r#"{"hooks": {"pre_tool_use": ["a", "b"], "post_tool_use": ["c"], "junk": 1}}"#,
        )
        .expect("json");
        let hooks = HooksConfig::parse(&value).expect("hooks");
        assert_eq!(hooks.pre_tool_use, vec!["a".to_owned(), "b".to_owned()]);
        assert_eq!(hooks.post_tool_use, vec!["c".to_owned()]);
        // No hooks key → None; empty object → empty config.
        assert!(HooksConfig::parse(&serde_json::json!({})).is_none());
        let empty = HooksConfig::parse(&serde_json::json!({"hooks": {}})).expect("hooks");
        assert!(empty.is_empty());
    }

    #[test]
    fn zero_exit_allows_and_nonzero_denies() {
        let dir = std::env::temp_dir().join(format!("hook-ok-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let ok = script(&dir, "ok.sh", "exit 0");
        assert_eq!(
            run_pre_tool_hooks(&[ok], "repo_read", "{}", HOOK_TIMEOUT),
            PreHookOutcome::Allowed
        );

        let deny = script(
            &dir,
            "deny.sh",
            r#"if [ "$1" = "check" ]; then true; fi
read line
case "$line" in
  *shell_exec*) echo "shell is not allowed here" >&2; exit 2 ;;
esac
exit 0"#,
        );
        match run_pre_tool_hooks(&[deny], "shell_exec", r#"{"argv":["ls"]}"#, HOOK_TIMEOUT) {
            PreHookOutcome::Denied { reason } => {
                assert!(reason.contains("shell is not allowed"), "{reason}");
            }
            other => panic!("denial expected, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hook_stdin_receives_the_tool_call_json() {
        let dir = std::env::temp_dir().join(format!("hook-stdin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let capture = dir.join("captured.json");
        let hook = script(
            &dir,
            "capture.sh",
            &format!("cat > {}", test_fixtures::sh_quote(&capture)),
        );
        let input = r#"{"tool":"repo_read","arguments":{"path":"a.txt"}}"#;
        assert_eq!(
            run_pre_tool_hooks(&[hook], "repo_read", r#"{"path":"a.txt"}"#, HOOK_TIMEOUT),
            PreHookOutcome::Allowed
        );
        let captured = std::fs::read_to_string(capture).expect("captured");
        assert_eq!(captured, input);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hung_hook_is_denied_after_the_bounded_timeout() {
        let dir = std::env::temp_dir().join(format!("hook-hang-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let hang = script(&dir, "hang.sh", "sleep 30");
        let started = std::time::Instant::now();
        match run_pre_tool_hooks(&[hang], "shell_exec", "{}", Duration::from_millis(250)) {
            PreHookOutcome::Denied { reason } => {
                assert!(reason.contains("timed out"), "{reason}");
            }
            other => panic!("hung hook must deny, got {other:?}"),
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the hook budget must bound the wait"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hook_subprocess_does_not_inherit_ambient_environment() {
        let dir = std::env::temp_dir().join(format!("hook-env-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let capture = dir.join("env.txt");
        // Cargo sets `CARGO_MANIFEST_DIR` (and the rest of `CARGO_*`) in
        // every test binary's environment: a variable this process is
        // guaranteed to have and no hook is ever forwarded. If it reaches
        // the hook, the ambient environment leaked — the same assertion on
        // every platform.
        const CANARY_NAME: &str = "CARGO_MANIFEST_DIR";
        assert!(
            std::env::var_os(CANARY_NAME).is_some(),
            "the test binary itself carries the canary"
        );
        // The redirect lives inside a script `sh` parses on every host;
        // `cmd /C` would parse a bare `env > file` line itself on Windows
        // and does not understand POSIX quoting.
        let hook = script(
            &dir,
            "env.sh",
            &format!(
                "{} > {}",
                test_fixtures::sh_quote(&test_fixtures::tool("env")),
                test_fixtures::sh_quote(&capture)
            ),
        );
        assert_eq!(
            run_pre_tool_hooks(&[hook], "repo_read", "{}", HOOK_TIMEOUT),
            PreHookOutcome::Allowed
        );
        let captured = std::fs::read_to_string(&capture).expect("captured env");
        assert!(
            !captured.lines().any(|line| line.starts_with(CANARY_NAME)),
            "hook subprocess must not inherit the ambient environment: {captured}"
        );
        // On Unix the exact set is known: the four we deliberately forward,
        // plus what `sh` itself injects even under `env -i` (confirmed via
        // `env -i PATH=/usr/bin:/bin sh -c 'env'`: PWD, SHLVL, and `_`) —
        // not something our own spawn code passes through. Anything outside
        // this set had to come from the real process environment, which
        // env_clear() must stop. (`cmd.exe` and the MSYS runtime add their
        // own set on Windows — COMSPEC, PATHEXT, the `=D:` drive cwd
        // pseudo-variables, … — which is why the canary is the assertion
        // that holds everywhere.)
        #[cfg(unix)]
        {
            const ALLOWED: &[&str] = &["PATH", "HOME", "LANG", "TMPDIR", "PWD", "SHLVL", "_"];
            for line in captured.lines() {
                let Some((key, _)) = line.split_once('=') else {
                    continue;
                };
                assert!(
                    ALLOWED.contains(&key),
                    "hook subprocess must not inherit ambient env var {key:?}: {captured}"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_large_stdin_payload_does_not_block_past_the_hook_timeout() {
        // `hang.sh` never touches stdin. If `run_hook_once` writes stdin
        // synchronously before starting its timeout clock, a payload larger
        // than the OS pipe buffer blocks the write until the child exits on
        // its own (here, after its full 30s sleep) rather than being bounded
        // by `timeout` — directly contradicting this module's doc comment
        // ("bounded by a timeout — a hung hook denies rather than hangs the
        // turn"). A small payload wouldn't reach the pipe buffer's capacity
        // and would pass either way, so this needs a payload comfortably
        // past any realistic OS pipe buffer size.
        let dir = std::env::temp_dir().join(format!("hook-bigstdin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let hang = script(&dir, "hang.sh", "sleep 30");
        let big_argument = format!("\"{}\"", "x".repeat(4_000_000));
        let started = std::time::Instant::now();
        match run_pre_tool_hooks(
            &[hang],
            "shell_exec",
            &big_argument,
            Duration::from_millis(250),
        ) {
            PreHookOutcome::Denied { reason } => {
                assert!(reason.contains("timed out"), "{reason}");
            }
            other => panic!("hung hook must deny, got {other:?}"),
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a large stdin payload must not block past the hook timeout, took {:?}",
            started.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn post_hooks_record_their_output() {
        let dir = std::env::temp_dir().join(format!("hook-post-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let note = script(&dir, "note.sh", "echo post-ran-ok");
        let output = run_post_tool_hooks(&[note], "repo_read", "the summary", HOOK_TIMEOUT);
        assert!(output.contains("post-ran-ok"), "{output}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hook_output_survives_a_trailing_invalid_utf8_byte() {
        // A `read_to_string`-based collection fails validity for the *whole*
        // captured file the instant any byte anywhere is invalid UTF-8,
        // discarding an otherwise perfectly good output rather than just the
        // offending tail. `read_capped_bytes` + the manual boundary-trimming
        // `truncate` above must instead preserve the valid prefix.
        let dir = std::env::temp_dir().join(format!("hook-badutf8-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let hook = script(&dir, "badutf8.sh", "printf 'ok-output'; printf '\\377'");
        let output = run_post_tool_hooks(&[hook], "repo_read", "the summary", HOOK_TIMEOUT);
        assert!(
            output.contains("ok-output"),
            "expected the valid prefix to survive a trailing invalid UTF-8 byte, got {output:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn notify_hooks_never_gate_and_receive_the_event_name() {
        let dir = std::env::temp_dir().join(format!("hook-notify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let capture = dir.join("captured.json");
        // Exits non-zero: notification hooks must not turn that into a
        // denial the way pre_tool_use does.
        let hook = script(
            &dir,
            "notify.sh",
            &format!("cat > {}\nexit 7", test_fixtures::sh_quote(&capture)),
        );
        let output = run_notify_hooks(
            &[hook],
            "subagent_start",
            Some("explore"),
            serde_json::json!({"agent_type": "explore"}),
            HOOK_TIMEOUT,
        );
        // Non-gating: the caller gets logged output, not a Denied variant —
        // there is no such variant for this call at all, which is the point.
        let _ = output;
        let captured = std::fs::read_to_string(capture).expect("captured");
        let value: serde_json::Value = serde_json::from_str(&captured).expect("json");
        assert_eq!(value["event"], "subagent_start");
        assert_eq!(value["agent_type"], "explore");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A hook script whose stdout is exactly `json` (single-quoted for `sh`,
    /// so the braces and double quotes survive) and whose exit code is
    /// `exit_code`.
    fn result_script(dir: &std::path::Path, name: &str, json: &str, exit_code: u8) -> String {
        script(dir, name, &format!("echo '{json}'\nexit {exit_code}"))
    }

    fn temp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("hook-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    #[test]
    fn a_v2_deny_result_denies_with_its_reason_and_is_recorded() {
        let dir = temp("v2-deny");
        let deny = result_script(
            &dir,
            "deny.sh",
            r#"{"schema":"rapidlm.hook_result","version":2,"decision":"deny","reason":"writes to docs/ are reviewed"}"#,
            0,
        );
        let report = run_pre_tool_stage(
            std::slice::from_ref(&deny),
            "workspace_write",
            "{}",
            HOOK_TIMEOUT,
        );
        assert_eq!(
            report.outcome,
            PreHookOutcome::Denied {
                reason: "writes to docs/ are reviewed".to_owned()
            }
        );
        assert_eq!(report.decisions.len(), 1);
        let record = &report.decisions[0];
        assert_eq!(record.hook, "pre_tool_use[0]");
        assert_eq!(record.event, "pre_tool_use");
        assert_eq!(record.decision, HookDecision::Deny);
        assert_eq!(
            record.reason.as_deref(),
            Some("writes to docs/ are reviewed")
        );
        assert_eq!(record.command_digest, command_digest(&deny));
        assert_eq!(record.command_digest.len(), 12);
        assert!(!record.grant_attempted);
        // The thin wrapper agrees with the stage.
        assert_eq!(
            run_pre_tool_hooks(&[deny], "workspace_write", "{}", HOOK_TIMEOUT),
            report.outcome
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn v2_allow_and_defer_continue_to_the_next_hook_and_are_recorded() {
        let dir = temp("v2-allow-defer");
        let allow = result_script(&dir, "allow.sh", r#"{"decision":"allow"}"#, 0);
        let defer = result_script(
            &dir,
            "defer.sh",
            r#"{"decision":"defer","reason":"not my call","Capabilities":["fs.write"]}"#,
            0,
        );
        let report = run_pre_tool_stage(&[allow, defer], "repo_read", "{}", HOOK_TIMEOUT);
        assert_eq!(report.outcome, PreHookOutcome::Allowed);
        let decisions: Vec<(String, HookDecision, bool)> = report
            .decisions
            .iter()
            .map(|d| (d.hook.clone(), d.decision, d.grant_attempted))
            .collect();
        assert_eq!(
            decisions,
            vec![
                ("pre_tool_use[0]".to_owned(), HookDecision::Allow, false),
                ("pre_tool_use[1]".to_owned(), HookDecision::Defer, true),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_v2_ask_is_reported_unless_a_later_hook_denies() {
        let dir = temp("v2-ask");
        let ask = result_script(
            &dir,
            "ask.sh",
            r#"{"decision":"ask","reason":"a human should look at this"}"#,
            0,
        );
        let deny = result_script(&dir, "deny.sh", r#"{"decision":"deny","reason":"no"}"#, 0);
        let alone =
            run_pre_tool_stage(std::slice::from_ref(&ask), "shell_exec", "{}", HOOK_TIMEOUT);
        assert_eq!(
            alone.outcome,
            PreHookOutcome::Ask {
                hook: "pre_tool_use[0]".to_owned(),
                command_digest: command_digest(&ask),
                reason: "a human should look at this".to_owned()
            }
        );
        assert_eq!(alone.decisions.len(), 1);
        assert_eq!(alone.decisions[0].decision, HookDecision::Ask);
        // A later denial wins over an earlier ask; both are recorded.
        let denied = run_pre_tool_stage(&[ask, deny], "shell_exec", "{}", HOOK_TIMEOUT);
        assert_eq!(
            denied.outcome,
            PreHookOutcome::Denied {
                reason: "no".to_owned()
            }
        );
        assert_eq!(
            denied
                .decisions
                .iter()
                .map(|d| d.decision)
                .collect::<Vec<_>>(),
            vec![HookDecision::Ask, HookDecision::Deny]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_v2_result_denies_naming_the_hook() {
        let dir = temp("v2-unreadable");
        let unknown = result_script(&dir, "maybe.sh", r#"{"decision":"maybe"}"#, 0);
        let future = result_script(
            &dir,
            "v9.sh",
            r#"{"schema":"rapidlm.hook_result","version":9,"decision":"allow"}"#,
            0,
        );
        for (hook, expected) in [
            (unknown, "is not one of allow, deny, ask, defer"),
            (future, "version 9 is not supported"),
        ] {
            let report = run_pre_tool_stage(&[hook], "repo_read", "{}", HOOK_TIMEOUT);
            match &report.outcome {
                PreHookOutcome::Denied { reason } => {
                    assert!(
                        reason.starts_with("pre_tool_use[0] hook printed an unreadable result"),
                        "{reason}"
                    );
                    assert!(reason.contains(expected), "{reason}");
                }
                other => panic!("an unreadable result must deny, got {other:?}"),
            }
            // Nothing readable was decided, so nothing is recorded.
            assert!(report.decisions.is_empty());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plain_stdout_text_keeps_v1_semantics_and_records_nothing() {
        let dir = temp("v1-text");
        // A v1 hook that chats on stdout and exits zero: allowed, silent.
        let chatty = script(&dir, "chatty.sh", "echo checking the call\nexit 0");
        let report = run_pre_tool_stage(&[chatty], "repo_read", "{}", HOOK_TIMEOUT);
        assert_eq!(report.outcome, PreHookOutcome::Allowed);
        assert!(report.decisions.is_empty());
        // A v1 hook that prints its reason on stdout (not stderr) and exits
        // non-zero: the reason is still the detail, as it was when both
        // streams shared one capture.
        let stdout_deny = script(&dir, "stdout-deny.sh", "echo not here\nexit 3");
        let report = run_pre_tool_stage(&[stdout_deny], "repo_read", "{}", HOOK_TIMEOUT);
        assert_eq!(
            report.outcome,
            PreHookOutcome::Denied {
                reason: "not here\n".to_owned()
            }
        );
        assert!(report.decisions.is_empty());
        // stderr wins over stdout as the v1 detail when both are written.
        let both = script(&dir, "both.sh", "echo progress\necho refused >&2\nexit 1");
        let report = run_pre_tool_stage(&[both], "repo_read", "{}", HOOK_TIMEOUT);
        assert_eq!(
            report.outcome,
            PreHookOutcome::Denied {
                reason: "refused\n".to_owned()
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_v2_deny_beside_a_nonzero_exit_lends_its_reason_and_a_v2_allow_does_not_rescue_it() {
        let dir = temp("v2-nonzero");
        let deny = result_script(
            &dir,
            "deny1.sh",
            r#"{"decision":"deny","reason":"typed refusal"}"#,
            1,
        );
        let report = run_pre_tool_stage(&[deny], "repo_read", "{}", HOOK_TIMEOUT);
        assert_eq!(
            report.outcome,
            PreHookOutcome::Denied {
                reason: "typed refusal".to_owned()
            }
        );
        assert_eq!(report.decisions.len(), 1);
        assert_eq!(report.decisions[0].decision, HookDecision::Deny);
        // Exit code still rules: `allow` printed by a failing hook denies
        // (fail-closed) with a reason that says so — not the echoed JSON,
        // which would read as an allow — and the decision is still recorded.
        let allow = result_script(&dir, "allow1.sh", r#"{"decision":"allow"}"#, 1);
        let report = run_pre_tool_stage(&[allow], "repo_read", "{}", HOOK_TIMEOUT);
        assert_eq!(
            report.outcome,
            PreHookOutcome::Denied {
                reason: "hook exited non-zero (its result said allow; the exit code decides)"
                    .to_owned()
            }
        );
        assert_eq!(report.decisions.len(), 1);
        assert_eq!(report.decisions[0].decision, HookDecision::Allow);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_timed_out_hooks_partial_stdout_is_not_a_decision() {
        let dir = temp("v2-timeout");
        // Prints a deny with a distinctive reason, then hangs. If the
        // timed-out run's stdout were parsed, that deny would be recorded
        // and its reason would surface; instead the timeout is the reason
        // and nothing is recorded — what a killed hook printed is a
        // diagnostic, never a decision.
        let hang = script(
            &dir,
            "hang.sh",
            "echo '{\"decision\":\"deny\",\"reason\":\"partial-verdict\"}'\nsleep 30",
        );
        let report = run_pre_tool_stage(&[hang], "repo_read", "{}", Duration::from_millis(250));
        match &report.outcome {
            PreHookOutcome::Denied { reason } => {
                assert!(reason.starts_with("hook timed out"), "{reason}");
                // The stdout diagnostic still reaches the detail.
                assert!(reason.contains("partial-verdict"), "{reason}");
            }
            other => panic!("a hung hook must deny, got {other:?}"),
        }
        assert!(report.decisions.is_empty(), "{:?}", report.decisions);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_malformed_result_denies_and_a_chatty_v1_hook_of_any_size_allows() {
        let dir = temp("v2-malformed");
        // A deny whose reason broke the JSON: denied, naming the hook — not
        // silently read as a v1 hook that allowed.
        let broken = script(
            &dir,
            "broken.sh",
            "echo '{\"decision\":\"deny\",\"reason\":\"a \"quoted\" word\"}'\nexit 0",
        );
        let report = run_pre_tool_stage(&[broken], "repo_read", "{}", HOOK_TIMEOUT);
        match &report.outcome {
            PreHookOutcome::Denied { reason } => {
                assert!(
                    reason.starts_with("pre_tool_use[0] hook printed an unreadable result"),
                    "{reason}"
                );
                assert!(reason.contains("not exactly one JSON object"), "{reason}");
            }
            other => panic!("a malformed result must deny, got {other:?}"),
        }
        // A result followed by a log line is malformed too.
        let trailing = script(
            &dir,
            "trailing.sh",
            "echo '{\"decision\":\"deny\"}'\necho checked 3 files\nexit 0",
        );
        assert!(matches!(
            run_pre_tool_stage(&[trailing], "repo_read", "{}", HOOK_TIMEOUT).outcome,
            PreHookOutcome::Denied { .. }
        ));
        // A v1 hook that prints far more than the result ceiling and exits
        // zero is still allowed: the ceiling is for results, not for text.
        let chatty = script(
            &dir,
            "chatty.sh",
            "i=0\nwhile [ $i -lt 1200 ]; do echo 'lint: file ok ...................................................'; i=$((i+1)); done\nexit 0",
        );
        let report = run_pre_tool_stage(&[chatty], "repo_read", "{}", HOOK_TIMEOUT);
        assert_eq!(report.outcome, PreHookOutcome::Allowed);
        assert!(report.decisions.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn post_hooks_record_both_streams_stdout_first() {
        let dir = temp("post-both");
        let both = script(&dir, "both.sh", "echo out-line\necho err-line >&2");
        let output = run_post_tool_hooks(&[both], "repo_read", "s", HOOK_TIMEOUT);
        let out = output.find("out-line").expect("stdout recorded");
        let err = output.find("err-line").expect("stderr recorded");
        assert!(out < err, "{output}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_last_rewrite_wins_a_deny_discards_them_and_an_ask_keeps_one() {
        let dir = temp("v2-rewrite");
        let first = result_script(
            &dir,
            "first.sh",
            r#"{"decision":"allow","updated_input":{"path":"first.txt","content":"a"}}"#,
            0,
        );
        let second = result_script(
            &dir,
            "second.sh",
            r#"{"decision":"defer","updated_input":{"path":"second.txt","content":"b"}}"#,
            0,
        );
        let plain_allow = result_script(&dir, "plain.sh", r#"{"decision":"allow"}"#, 0);
        let deny = result_script(&dir, "deny.sh", r#"{"decision":"deny","reason":"no"}"#, 0);
        let ask = result_script(
            &dir,
            "ask.sh",
            r#"{"decision":"ask","reason":"look","updated_input":{"path":"asked.txt","content":"c"}}"#,
            0,
        );
        // Two rewriters, then a hook that rewrites nothing: the second
        // rewrite stands (the last *rewriting* hook wins).
        let report = run_pre_tool_stage(
            &[first.clone(), second.clone(), plain_allow],
            "workspace_write",
            r#"{"path":"orig.txt","content":"x"}"#,
            HOOK_TIMEOUT,
        );
        assert_eq!(report.outcome, PreHookOutcome::Allowed);
        let rewrite = report.rewrite.expect("a rewrite survives");
        assert_eq!(rewrite.hook, "pre_tool_use[1]");
        assert_eq!(rewrite.command_digest, command_digest(&second));
        assert_eq!(rewrite.input["path"], "second.txt");
        // A denial discards every rewrite.
        let report = run_pre_tool_stage(
            &[first.clone(), deny],
            "workspace_write",
            "{}",
            HOOK_TIMEOUT,
        );
        assert!(matches!(report.outcome, PreHookOutcome::Denied { .. }));
        assert_eq!(report.rewrite, None);
        // An ask keeps the rewrite: the human is shown what would run.
        let report = run_pre_tool_stage(&[first, ask], "workspace_write", "{}", HOOK_TIMEOUT);
        assert!(matches!(report.outcome, PreHookOutcome::Ask { .. }));
        assert_eq!(report.rewrite.expect("kept").input["path"], "asked.txt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_later_hook_sees_the_earlier_hooks_rewrite() {
        let dir = temp("v2-chain");
        let capture = dir.join("seen.json");
        let first = result_script(
            &dir,
            "first.sh",
            r#"{"decision":"allow","updated_input":{"path":"redirected.txt","content":"a"}}"#,
            0,
        );
        // The second hook records what it was shown and allows.
        let second = script(
            &dir,
            "second.sh",
            &format!(
                "cat > {}\necho '{{\"decision\":\"allow\"}}'\nexit 0",
                test_fixtures::sh_quote(&capture)
            ),
        );
        let report = run_pre_tool_stage(
            &[first, second],
            "workspace_write",
            r#"{"path":"orig.txt","content":"a"}"#,
            HOOK_TIMEOUT,
        );
        assert_eq!(report.outcome, PreHookOutcome::Allowed);
        let seen: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(capture).expect("captured"))
                .expect("json");
        assert_eq!(seen["arguments"]["path"], "redirected.txt", "{seen}");
        assert_eq!(
            report.rewrite.expect("kept").input["path"],
            "redirected.txt"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_updated_input_equal_to_the_shown_input_is_not_a_rewrite() {
        let dir = temp("v2-identity");
        let same = result_script(
            &dir,
            "same.sh",
            r#"{"decision":"allow","updated_input":{"path":"a.txt","content":"x"}}"#,
            0,
        );
        let report = run_pre_tool_stage(
            std::slice::from_ref(&same),
            "workspace_write",
            r#"{"content":"x","path":"a.txt"}"#,
            HOOK_TIMEOUT,
        );
        assert_eq!(report.outcome, PreHookOutcome::Allowed);
        assert_eq!(
            report.rewrite, None,
            "same object (key order aside) is no rewrite"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn additional_context_is_collected_from_pre_and_post_hooks_and_fenced() {
        let dir = temp("v2-context");
        let pre = result_script(
            &dir,
            "pre.sh",
            r#"{"decision":"allow","additional_context":"the docs are generated nightly"}"#,
            0,
        );
        let blank = result_script(
            &dir,
            "blank.sh",
            r#"{"decision":"defer","additional_context":"   "}"#,
            0,
        );
        let report = run_pre_tool_stage(&[pre, blank], "repo_read", "{}", HOOK_TIMEOUT);
        assert_eq!(report.outcome, PreHookOutcome::Allowed);
        assert_eq!(report.context.len(), 1, "blank context is not delivered");
        assert_eq!(report.context[0].hook, "pre_tool_use[0]");
        assert_eq!(
            report.context[0].fenced(),
            "<untrusted_context locator=\"hook:pre_tool_use[0]\">\nthe docs are generated nightly\n</untrusted_context>"
        );
        // A deny delivers none, even from an earlier hook.
        let deny = result_script(&dir, "deny.sh", r#"{"decision":"deny","reason":"no"}"#, 0);
        let pre2 = result_script(
            &dir,
            "pre2.sh",
            r#"{"decision":"allow","additional_context":"c"}"#,
            0,
        );
        let denied = run_pre_tool_stage(&[pre2, deny], "repo_read", "{}", HOOK_TIMEOUT);
        assert!(matches!(denied.outcome, PreHookOutcome::Denied { .. }));
        assert!(denied.context.is_empty());
        // Post hooks: a v2 result's context is delivered, its decision
        // recorded (and ignored — a post hook cannot block); plain text is
        // the v1 output it always was.
        let post_v2 = result_script(
            &dir,
            "post.sh",
            r#"{"decision":"deny","reason":"too late","additional_context":"lint: 2 warnings"}"#,
            0,
        );
        let post_v1 = script(&dir, "note.sh", "echo post-ran-ok");
        let post = run_post_tool_stage(&[post_v2, post_v1], "repo_read", "s", HOOK_TIMEOUT);
        assert_eq!(post.output, "post-ran-ok\n");
        assert_eq!(post.context.len(), 1);
        assert_eq!(post.context[0].hook, "post_tool_use[0]");
        assert_eq!(post.context[0].text, "lint: 2 warnings");
        assert_eq!(post.decisions.len(), 1);
        assert_eq!(post.decisions[0].event, "post_tool_use");
        assert_eq!(post.decisions[0].decision, HookDecision::Deny);
        // The v1 wrapper still returns only the text.
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_new_stages_parse_and_every_stage_is_named_once() {
        let value: serde_json::Value = serde_json::from_str(
            r#"{"hooks": {"user_prompt_submit": ["a"], "stop": ["b"], "stop_cancelled": ["c"], "post_tool_use_failure": ["d"]}}"#,
        )
        .expect("json");
        let hooks = HooksConfig::parse(&value).expect("hooks");
        assert_eq!(hooks.user_prompt_submit, vec!["a".to_owned()]);
        assert_eq!(hooks.stop, vec!["b".to_owned()]);
        assert_eq!(hooks.stop_cancelled, vec!["c".to_owned()]);
        assert_eq!(hooks.post_tool_use_failure, vec!["d".to_owned()]);
        assert!(!hooks.is_empty());
        // One list, no duplicates, every stage: a stage added to the struct
        // but not to `named_stages` would drop out of doctor and the gates.
        let mut every = HooksConfig::default();
        for stage in every.stages_mut() {
            stage.push(HookEntry::plain("x"));
        }
        let names: Vec<&str> = every.named_stages().iter().map(|(n, _)| *n).collect();
        let unique: std::collections::BTreeSet<&str> = names.iter().copied().collect();
        assert_eq!(unique.len(), HOOK_STAGE_COUNT, "{names:?}");
        assert!(every.named_stages().iter().all(|(_, c)| c.len() == 1));
        // A settings key for every named stage round-trips through parse.
        let doc = serde_json::json!({
            "hooks": names.iter().map(|n| (n.to_string(), serde_json::json!(["y"]))).collect::<serde_json::Map<_, _>>()
        });
        let parsed = HooksConfig::parse(&doc).expect("hooks");
        assert!(
            parsed
                .named_stages()
                .iter()
                .all(|(_, c)| **c == vec!["y".to_owned()])
        );
    }

    #[test]
    fn a_prompt_submit_deny_blocks_and_a_failing_hook_fails_open_with_a_warning() {
        let dir = temp("prompt-submit");
        let capture = dir.join("prompt.json");
        let deny = script(
            &dir,
            "deny.sh",
            &format!(
                "cat > {}\necho '{{\"decision\":\"deny\",\"reason\":\"no secrets in prompts\"}}'\nexit 0",
                test_fixtures::sh_quote(&capture)
            ),
        );
        let report =
            run_prompt_submit_stage(std::slice::from_ref(&deny), "fix the bug", HOOK_TIMEOUT);
        assert_eq!(
            report.first_deny(),
            Some((
                "user_prompt_submit[0]".to_owned(),
                "no secrets in prompts".to_owned()
            ))
        );
        let seen: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&capture).expect("captured"))
                .expect("json");
        assert_eq!(seen["event"], "user_prompt_submit");
        assert_eq!(seen["prompt"], "fix the bug");
        // A crashing hook lets the prompt through and is a recorded failure.
        let crash = script(&dir, "crash.sh", "echo boom >&2\nexit 9");
        let report = run_prompt_submit_stage(&[crash], "fix the bug", HOOK_TIMEOUT);
        assert_eq!(report.first_deny(), None);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].hook, "user_prompt_submit[0]");
        assert!(report.failures[0].detail.contains("boom"));
        // `ask` has no one to ask: recorded, no block.
        let ask = result_script(&dir, "ask.sh", r#"{"decision":"ask"}"#, 0);
        let report = run_prompt_submit_stage(&[ask], "x", HOOK_TIMEOUT);
        assert_eq!(report.first_deny(), None);
        assert_eq!(report.decisions[0].decision, HookDecision::Ask);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_turn_end_stage_fires_stop_or_stop_cancelled_never_both() {
        let dir = temp("turn-end");
        let stop_capture = dir.join("stop.json");
        let cancelled_capture = dir.join("cancelled.json");
        let hooks = HooksConfig {
            stop: crate::hooks::entries(vec![script(
                &dir,
                "stop.sh",
                &format!("cat > {}", test_fixtures::sh_quote(&stop_capture)),
            )]),
            stop_cancelled: crate::hooks::entries(vec![script(
                &dir,
                "cancelled.sh",
                &format!("cat > {}", test_fixtures::sh_quote(&cancelled_capture)),
            )]),
            ..Default::default()
        };
        run_turn_end_stage(&hooks, &TurnEnd::Completed, 3, 42, HOOK_TIMEOUT);
        let stop: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&stop_capture).expect("stop fired"))
                .expect("json");
        assert_eq!(stop["event"], "stop");
        assert_eq!(stop["tool_calls"], 3);
        assert_eq!(stop["tokens"], 42);
        assert!(
            !cancelled_capture.exists(),
            "stop_cancelled must not fire for a completed turn"
        );
        std::fs::remove_file(&stop_capture).expect("reset");
        run_turn_end_stage(
            &hooks,
            &TurnEnd::Cancelled {
                reason: "budget_exhausted".to_owned(),
            },
            1,
            7,
            HOOK_TIMEOUT,
        );
        let cancelled: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&cancelled_capture).expect("stop_cancelled fired"),
        )
        .expect("json");
        assert_eq!(cancelled["event"], "stop_cancelled");
        assert_eq!(cancelled["reason"], "budget_exhausted");
        assert!(
            !stop_capture.exists(),
            "stop must not fire when stop_cancelled does"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_chained_rewrite_credits_every_contributor_and_a_round_trip_is_no_rewrite() {
        let dir = temp("v2-contributors");
        // hook0 changes the path; hook1 (seeing hook0's call) changes the content.
        let path = script(
            &dir,
            "path.sh",
            "echo '{\"decision\":\"allow\",\"updated_input\":{\"path\":\"b.txt\",\"content\":\"x\"}}'",
        );
        let content = script(
            &dir,
            "content.sh",
            "echo '{\"decision\":\"allow\",\"updated_input\":{\"path\":\"b.txt\",\"content\":\"y\"}}'",
        );
        let report = run_pre_tool_stage(
            &[path.clone(), content],
            "workspace_write",
            r#"{"path":"a.txt","content":"x"}"#,
            HOOK_TIMEOUT,
        );
        let rewrite = report.rewrite.expect("rewritten");
        assert_eq!(rewrite.hook, "pre_tool_use[1]");
        assert_eq!(
            rewrite.contributors,
            vec!["pre_tool_use[0]".to_owned(), "pre_tool_use[1]".to_owned()]
        );
        // hook1 puts it back exactly as the model proposed: no rewrite at all.
        let back = script(
            &dir,
            "back.sh",
            "echo '{\"decision\":\"allow\",\"updated_input\":{\"path\":\"a.txt\",\"content\":\"x\"}}'",
        );
        let report = run_pre_tool_stage(
            &[path, back],
            "workspace_write",
            r#"{"path":"a.txt","content":"x"}"#,
            HOOK_TIMEOUT,
        );
        assert_eq!(report.rewrite, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_settings_reads_the_new_notification_hook_keys() {
        let value: serde_json::Value = serde_json::from_str(
            r#"{"hooks": {"session_start": ["a"], "session_end": ["d"], "subagent_start": ["b"], "subagent_stop": ["c"]}}"#,
        )
        .expect("json");
        let hooks = HooksConfig::parse(&value).expect("hooks");
        assert_eq!(hooks.session_start, vec!["a".to_owned()]);
        assert_eq!(hooks.session_end, vec!["d".to_owned()]);
        assert_eq!(hooks.subagent_start, vec!["b".to_owned()]);
        assert_eq!(hooks.subagent_stop, vec!["c".to_owned()]);
        assert!(!hooks.is_empty());
    }

    fn parsed(settings: &str) -> (HooksConfig, Vec<HookRejection>) {
        let value: serde_json::Value = serde_json::from_str(settings).expect("json");
        HooksConfig::parse_reporting(&value).expect("hooks object")
    }

    fn rejected_reasons(settings: &str) -> Vec<String> {
        parsed(settings)
            .1
            .into_iter()
            .map(|rejection| rejection.to_string())
            .collect()
    }

    #[test]
    fn object_form_entries_parse_with_every_key() {
        let (config, rejected) = parsed(
            r#"{"hooks": {
                "pre_tool_use": [
                    "plain.sh",
                    {"command": "a.sh"},
                    {"command": "b.sh", "matcher": "shell_exec|workspace_patch", "timeout_ms": 20000}
                ],
                "stop": [{"command": "gate.sh", "continue_turn": true, "timeout_ms": 600000}],
                "stop_cancelled": [{"command": "r.sh", "matcher": "budget_exhausted"}]
            }}"#,
        );
        assert!(rejected.is_empty(), "{rejected:?}");
        assert_eq!(config.pre_tool_use.len(), 3);
        assert_eq!(config.pre_tool_use[0], HookEntry::plain("plain.sh"));
        assert_eq!(config.pre_tool_use[1], HookEntry::plain("a.sh"));
        let third = &config.pre_tool_use[2];
        assert_eq!(third.command, "b.sh");
        assert_eq!(
            third.matcher.as_ref().map(HookMatcher::source),
            Some("shell_exec|workspace_patch")
        );
        assert_eq!(third.timeout_ms, Some(20_000));
        assert!(!third.continue_turn);
        assert!(config.stop[0].continue_turn);
        assert_eq!(config.stop[0].timeout_ms, Some(600_000));
        assert_eq!(
            config.stop_cancelled[0]
                .matcher
                .as_ref()
                .map(HookMatcher::source),
            Some("budget_exhausted")
        );
    }

    #[test]
    fn every_bad_object_entry_is_rejected_with_its_position_not_skipped() {
        let cases: [(&str, &str); 14] = [
            (r#"{"command": "a", "extra": 1}"#, "unknown key"),
            (r#"{"matcher": "x"}"#, "`command` is missing"),
            (r#"{"command": ""}"#, "`command` is empty"),
            (r#"{"command": 7}"#, "`command` is not a string"),
            (
                r#"{"command": "a", "matcher": "("}"#,
                "not a valid regular expression",
            ),
            (
                r#"{"command": "a", "matcher": 3}"#,
                "`matcher` is not a string",
            ),
            (r#"{"command": "a", "matcher": ""}"#, "`matcher` is empty"),
            (r#"{"command": "a", "timeout_ms": 0}"#, "between 1 and"),
            (
                r#"{"command": "a", "timeout_ms": 120001}"#,
                "between 1 and 120000",
            ),
            (
                r#"{"command": "a", "timeout_ms": "5"}"#,
                "not a positive integer",
            ),
            (
                r#"{"command": "a", "timeout_ms": -4}"#,
                "not a positive integer",
            ),
            (
                r#"{"command": "a", "continue_turn": true}"#,
                "stop stage only",
            ),
            (
                r#"{"command": "a", "continue_turn": "yes"}"#,
                "not a boolean",
            ),
            (
                r#"{"command": "a", "timeout_ms": 1.5}"#,
                "not a positive integer",
            ),
        ];
        for (entry, expect) in cases {
            let settings = format!(r#"{{"hooks": {{"pre_tool_use": ["ok.sh", {entry}]}}}}"#);
            let reasons = rejected_reasons(&settings);
            assert_eq!(reasons.len(), 1, "{entry}: {reasons:?}");
            assert!(
                reasons[0].starts_with("hooks.pre_tool_use[1]: "),
                "position is reported: {}",
                reasons[0]
            );
            assert!(reasons[0].contains(expect), "{entry}: {}", reasons[0]);
            let (config, _) = parsed(&settings);
            assert_eq!(config.pre_tool_use.len(), 1, "the good entry still loads");
        }
    }

    #[test]
    fn a_matcher_needs_a_stage_with_a_subject_and_a_long_one_is_refused() {
        for stage in [
            "session_start",
            "session_end",
            "pre_compact",
            "post_compact",
            "user_prompt_submit",
            "stop",
        ] {
            let reasons = rejected_reasons(&format!(
                r#"{{"hooks": {{"{stage}": [{{"command": "a", "matcher": "x"}}]}}}}"#
            ));
            assert_eq!(reasons.len(), 1, "{stage}");
            assert!(reasons[0].contains("no subject"), "{stage}: {}", reasons[0]);
        }
        for stage in [
            "pre_tool_use",
            "post_tool_use",
            "post_tool_use_failure",
            "subagent_start",
            "subagent_stop",
            "stop_cancelled",
        ] {
            assert!(
                rejected_reasons(&format!(
                    r#"{{"hooks": {{"{stage}": [{{"command": "a", "matcher": "x"}}]}}}}"#
                ))
                .is_empty(),
                "{stage} has a subject"
            );
        }
        let long = "a".repeat(MAX_HOOK_MATCHER_BYTES + 1);
        let reasons = rejected_reasons(&format!(
            r#"{{"hooks": {{"pre_tool_use": [{{"command": "a", "matcher": "{long}"}}]}}}}"#
        ));
        assert!(
            reasons[0].contains("over the 256-byte limit"),
            "{reasons:?}"
        );
        let exactly = "a".repeat(MAX_HOOK_MATCHER_BYTES);
        assert!(
            rejected_reasons(&format!(
                r#"{{"hooks": {{"pre_tool_use": [{{"command": "a", "matcher": "{exactly}"}}]}}}}"#
            ))
            .is_empty(),
            "256 bytes is the limit, inclusive"
        );
    }

    #[test]
    fn the_stage_class_decides_the_longest_timeout() {
        for (stage, max) in [
            ("user_prompt_submit", 30_000u64),
            ("pre_tool_use", 120_000),
            ("subagent_stop", 120_000),
            ("stop", 600_000),
            ("post_tool_use", 30_000),
            ("session_end", 30_000),
        ] {
            let at =
                format!(r#"{{"hooks": {{"{stage}": [{{"command": "a", "timeout_ms": {max}}}]}}}}"#);
            assert!(rejected_reasons(&at).is_empty(), "{stage} accepts {max}");
            let over = format!(
                r#"{{"hooks": {{"{stage}": [{{"command": "a", "timeout_ms": {}}}]}}}}"#,
                max + 1
            );
            assert_eq!(
                rejected_reasons(&over).len(),
                1,
                "{stage} refuses {}",
                max + 1
            );
            assert_eq!(stage_max_timeout_ms(stage), max);
        }
    }

    #[test]
    fn a_matcher_matches_the_whole_subject_not_a_substring() {
        let matcher = HookMatcher::new("shell_exec|workspace_patch").expect("matcher");
        assert!(matcher.is_match("shell_exec"));
        assert!(matcher.is_match("workspace_patch"));
        assert!(!matcher.is_match("shell_exec2"), "anchored at the end");
        assert!(!matcher.is_match("my_shell_exec"), "anchored at the start");
        let mcp = HookMatcher::new("mcp__github__.*").expect("matcher");
        assert!(mcp.is_match("mcp__github__create_issue"));
        assert!(!mcp.is_match("mcp__gitlab__create_issue"));
        let reasons = HookMatcher::new("a{1000}{1000}").map(|_| ()).unwrap_err();
        assert!(reasons.contains("not a valid regular expression"));
    }

    fn marker_hook(dir: &std::path::Path, name: &str, marker: &str) -> String {
        script(
            dir,
            name,
            &format!(
                "cat > /dev/null\necho ran >> {}\n",
                test_fixtures::sh_quote(&dir.join(marker))
            ),
        )
    }

    #[test]
    fn a_hook_whose_matcher_does_not_select_the_tool_does_not_run_at_all() {
        let dir = std::env::temp_dir().join(format!("hook-matcher-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let command = marker_hook(&dir, "m.sh", "ran.txt");
        let shell_only = HookEntry {
            command: command.clone(),
            matcher: Some(HookMatcher::new("shell_exec").expect("matcher")),
            timeout_ms: None,
            continue_turn: false,
        };
        let outcome = run_pre_tool_stage(
            std::slice::from_ref(&shell_only),
            "workspace_write",
            "{}",
            HOOK_TIMEOUT,
        );
        assert_eq!(outcome.outcome, PreHookOutcome::Allowed);
        assert!(outcome.decisions.is_empty(), "no record for a skipped hook");
        assert!(!dir.join("ran.txt").exists(), "no process was started");
        run_pre_tool_stage(&[shell_only], "shell_exec", "{}", HOOK_TIMEOUT);
        assert!(
            dir.join("ran.txt").exists(),
            "the selected tool runs the hook"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_skipped_hook_keeps_its_place_so_the_ones_after_it_keep_their_names() {
        let dir = std::env::temp_dir().join(format!("hook-identity-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let first = HookEntry {
            command: script(&dir, "a.sh", "cat > /dev/null\n"),
            matcher: Some(HookMatcher::new("other_tool").expect("matcher")),
            timeout_ms: None,
            continue_turn: false,
        };
        let second = HookEntry::plain(script(
            &dir,
            "b.sh",
            "cat > /dev/null\necho '{\"decision\":\"allow\",\"reason\":\"fine\"}'\n",
        ));
        let report = run_pre_tool_stage(&[first, second], "shell_exec", "{}", HOOK_TIMEOUT);
        assert_eq!(report.decisions.len(), 1);
        assert_eq!(
            report.decisions[0].hook, "pre_tool_use[1]",
            "the position is the entry's place in the configured list, so a matcher never renames a hook (and never orphans an approval that names it)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn adding_a_matcher_and_timeout_does_not_change_which_approval_a_hook_answers() {
        let dir = std::env::temp_dir().join(format!("hook-approval-id-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let command = script(
            &dir,
            "ask.sh",
            "cat > /dev/null\necho '{\"decision\":\"ask\",\"reason\":\"look\"}'\n",
        );
        let ask =
            |entry: HookEntry| match run_pre_tool_stage(&[entry], "shell_exec", "{}", HOOK_TIMEOUT)
                .outcome
            {
                PreHookOutcome::Ask {
                    hook,
                    command_digest,
                    ..
                } => (hook, command_digest),
                other => panic!("expected an ask, got {other:?}"),
            };
        let bare = ask(HookEntry::plain(command.clone()));
        let decorated = ask(HookEntry {
            command,
            matcher: Some(HookMatcher::new("shell_exec").expect("matcher")),
            timeout_ms: Some(9_000),
            continue_turn: false,
        });
        assert_eq!(
            bare, decorated,
            "an approval recorded against the bare hook still answers it once a matcher and timeout are added"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_per_hook_timeout_replaces_the_default_for_that_hook_only() {
        let dir = std::env::temp_dir().join(format!("hook-own-timeout-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let slow = script(
            &dir,
            "slow.sh",
            &format!(
                "cat > /dev/null\nsleep 2\necho done > {}\n",
                test_fixtures::sh_quote(&dir.join("done.txt"))
            ),
        );
        let patient = HookEntry {
            command: slow.clone(),
            matcher: None,
            timeout_ms: Some(10_000),
            continue_turn: false,
        };
        // The stage default is far too short for the hook; its own budget wins.
        let report = run_pre_tool_stage(&[patient], "shell_exec", "{}", Duration::from_millis(100));
        assert_eq!(report.outcome, PreHookOutcome::Allowed, "{report:?}");
        // The same command with no budget of its own is held to the default.
        let report = run_pre_tool_stage(
            &[HookEntry::plain(slow)],
            "shell_exec",
            "{}",
            Duration::from_millis(100),
        );
        assert!(
            matches!(&report.outcome, PreHookOutcome::Denied { reason } if reason.contains("timed out")),
            "{report:?}"
        );
        // And a short budget of its own bounds a hook under a generous default.
        let impatient = HookEntry {
            command: script(&dir, "hang.sh", "sleep 30"),
            matcher: None,
            timeout_ms: Some(200),
            continue_turn: false,
        };
        let started = Instant::now();
        let report = run_pre_tool_stage(&[impatient], "shell_exec", "{}", Duration::from_secs(60));
        assert!(matches!(report.outcome, PreHookOutcome::Denied { .. }));
        assert!(started.elapsed() < Duration::from_secs(10));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn observer_and_notify_stages_honour_the_matcher_and_the_timeout_too() {
        let dir = std::env::temp_dir().join(format!("hook-observers-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let selective = |name: &str, marker: &str, pattern: &str| HookEntry {
            command: marker_hook(&dir, name, marker),
            matcher: Some(HookMatcher::new(pattern).expect("matcher")),
            timeout_ms: None,
            continue_turn: false,
        };
        // Subagent type for the child stages.
        run_notify_hooks(
            &[selective("s1.sh", "start.txt", "explore")],
            "subagent_start",
            Some("coder"),
            serde_json::json!({}),
            HOOK_TIMEOUT,
        );
        assert!(!dir.join("start.txt").exists());
        run_notify_hooks(
            &[selective("s2.sh", "start.txt", "explore")],
            "subagent_start",
            Some("explore"),
            serde_json::json!({}),
            HOOK_TIMEOUT,
        );
        assert!(dir.join("start.txt").exists());
        // Stop reason for stop_cancelled.
        let config = HooksConfig {
            stop_cancelled: vec![selective("s3.sh", "cancel.txt", "budget_exhausted")],
            ..Default::default()
        };
        run_turn_end_stage(
            &config,
            &TurnEnd::Cancelled {
                reason: "cancelled".to_owned(),
            },
            0,
            0,
            HOOK_TIMEOUT,
        );
        assert!(!dir.join("cancel.txt").exists());
        run_turn_end_stage(
            &config,
            &TurnEnd::Cancelled {
                reason: "budget_exhausted".to_owned(),
            },
            0,
            0,
            HOOK_TIMEOUT,
        );
        assert!(dir.join("cancel.txt").exists());
        // Tool name for post_tool_use.
        run_post_tool_stage(
            &[selective("s4.sh", "post.txt", "mcp__.*")],
            "shell_exec",
            "x",
            HOOK_TIMEOUT,
        );
        assert!(!dir.join("post.txt").exists());
        run_post_tool_stage(
            &[selective("s5.sh", "post.txt", "mcp__.*")],
            "mcp__srv__tool",
            "x",
            HOOK_TIMEOUT,
        );
        assert!(dir.join("post.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merging_dedupes_identical_entries_and_caps_the_result() {
        let mut merged = HooksConfig::default();
        let (one, _) = parsed(
            r#"{"hooks": {"pre_tool_use": ["same.sh", {"command": "same.sh", "matcher": "shell_exec"}]}}"#,
        );
        let (two, _) = parsed(
            r#"{"hooks": {"pre_tool_use": ["same.sh", {"command": "same.sh", "matcher": "shell_exec"}, "other.sh"]}}"#,
        );
        merged.extend(one);
        merged.extend(two);
        assert_eq!(merged.pre_tool_use.len(), 5);
        merged.dedupe_and_cap();
        let commands: Vec<(&str, Option<&str>)> = merged
            .pre_tool_use
            .iter()
            .map(|entry| {
                (
                    entry.command.as_str(),
                    entry.matcher.as_ref().map(HookMatcher::source),
                )
            })
            .collect();
        assert_eq!(
            commands,
            [
                ("same.sh", None),
                ("same.sh", Some("shell_exec")),
                ("other.sh", None)
            ],
            "identical entries run once; the same command with a different matcher is a different hook"
        );
        let mut many = HooksConfig::default();
        for index in 0..(MAX_HOOKS_PER_STAGE + 3) {
            many.stop.push(HookEntry::plain(format!("h{index}.sh")));
        }
        many.dedupe_and_cap();
        assert_eq!(many.stop.len(), MAX_HOOKS_PER_STAGE);
        // Timeout and continue_turn also tell two entries apart.
        let mut tuned = HooksConfig::default();
        tuned.stop.push(HookEntry {
            timeout_ms: Some(10),
            ..HookEntry::plain("x")
        });
        tuned.stop.push(HookEntry::plain("x"));
        tuned.stop.push(HookEntry {
            continue_turn: true,
            ..HookEntry::plain("x")
        });
        tuned.dedupe_and_cap();
        assert_eq!(tuned.stop.len(), 3);
    }

    #[test]
    fn a_settings_file_of_strings_only_parses_exactly_as_before() {
        let (config, rejected) = parsed(
            r#"{"hooks": {"pre_tool_use": ["a", "", 5, null, "b"], "stop": "not-an-array", "post_tool_use": [["nested"]]}}"#,
        );
        assert!(
            rejected.is_empty(),
            "malformed strings are still skipped silently: {rejected:?}"
        );
        assert_eq!(config.pre_tool_use, vec!["a".to_owned(), "b".to_owned()]);
        assert!(config.stop.is_empty() && config.post_tool_use.is_empty());
        let too_long = "x".repeat(MAX_HOOK_COMMAND_BYTES + 1);
        let (config, _) = parsed(&format!(
            r#"{{"hooks": {{"pre_tool_use": ["{too_long}", "ok"]}}}}"#
        ));
        assert_eq!(config.pre_tool_use, vec!["ok".to_owned()]);
    }

    #[cfg(unix)]
    #[test]
    fn a_timed_out_hook_leaves_no_child_process_behind() {
        let dir = std::env::temp_dir().join(format!("hook-group-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let plain = dir.join("plain.pid");
        let stubborn = dir.join("stubborn.pid");
        // Two grandchildren: one that dies on TERM, one that ignores it. Both
        // outlive the hook's own shell unless the whole group is ended.
        let hook = script(
            &dir,
            "spawner.sh",
            &format!(
                "cat > /dev/null\nsleep 30 &\necho $! > {}\nsh -c 'trap \"\" TERM; sleep 30' &\necho $! > {}\nwait\n",
                test_fixtures::sh_quote(&plain),
                test_fixtures::sh_quote(&stubborn),
            ),
        );
        let entry = HookEntry {
            command: hook,
            matcher: None,
            timeout_ms: Some(1_500),
            continue_turn: false,
        };
        let report = run_pre_tool_stage(&[entry], "shell_exec", "{}", Duration::from_secs(60));
        assert!(
            matches!(&report.outcome, PreHookOutcome::Denied { reason } if reason.contains("timed out")),
            "{report:?}"
        );
        let read_pid = |path: &std::path::Path| -> u32 {
            std::fs::read_to_string(path)
                .expect("the hook wrote its child's pid")
                .trim()
                .parse()
                .expect("pid")
        };
        let pids = [read_pid(&plain), read_pid(&stubborn)];
        let deadline = Instant::now() + Duration::from_secs(5);
        while pids.iter().any(|pid| process_signal::process_exists(*pid))
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        let survivors: Vec<u32> = pids
            .iter()
            .copied()
            .filter(|pid| process_signal::process_exists(*pid))
            .collect();
        for pid in &survivors {
            let _ = process_signal::kill_process(*pid);
        }
        assert!(
            survivors.is_empty(),
            "children of a timed-out hook must not outlive it: {survivors:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_long_timeout_needs_a_platform_that_can_end_a_hooks_children() {
        // Every supported platform ends the group (Unix signals the group,
        // Windows ends the tree); a target that does not is a compile-time fact.
        const { assert!(PROCESS_GROUP_TERMINATION) };
        assert!(
            timeout_allowed(5_000, false).is_ok(),
            "the default budget needs nothing extra"
        );
        let refused = timeout_allowed(5_001, false).expect_err("typed unavailability");
        assert!(
            refused.contains("not available on this platform"),
            "{refused}"
        );
        assert!(timeout_allowed(120_000, true).is_ok());
        // And the parser applies it: on a platform that cannot, an entry
        // asking for more than the default is rejected, not accepted and leaked.
        let entry = |ms: u64| {
            serde_json::json!({"command": "a", "timeout_ms": ms})
                .as_object()
                .cloned()
                .expect("object")
        };
        let refused = parse_object_entry_for("pre_tool_use", &entry(20_000), false)
            .expect_err("no group termination, no long timeout");
        assert!(
            refused.contains("not available on this platform"),
            "{refused}"
        );
        assert!(parse_object_entry_for("pre_tool_use", &entry(5_000), false).is_ok());
        assert!(parse_object_entry_for("pre_tool_use", &entry(20_000), true).is_ok());
    }

    #[test]
    fn an_unbalanced_matcher_cannot_escape_its_anchors() {
        // Wrapped in `^(?:…)$` as text, `shell_exec)|(x` would read as
        // `^(?:shell_exec)|(x)$` and match any subject starting `shell_exec`.
        let reasons = rejected_reasons(
            r#"{"hooks": {"pre_tool_use": [{"command": "a", "matcher": "shell_exec)|(x"}]}}"#,
        );
        assert_eq!(reasons.len(), 1, "{reasons:?}");
        assert!(
            reasons[0].contains("not a valid regular expression"),
            "{reasons:?}"
        );
        assert!(HookMatcher::new("shell_exec)|(x").is_err());
        // A flag group and anchors the author wrote themselves stay inside the whole-subject match.
        let insensitive = HookMatcher::new("(?i)shell_exec").expect("matcher");
        assert!(insensitive.is_match("SHELL_EXEC"));
        assert!(!insensitive.is_match("shell_exec_extra"));
        let anchored = HookMatcher::new("^shell_exec$").expect("matcher");
        assert!(anchored.is_match("shell_exec") && !anchored.is_match("xshell_exec"));
    }

    #[test]
    fn a_cheap_reason_to_reject_is_found_before_the_expression_is_compiled() {
        // Both the expression and `continue_turn` are wrong; the cheap check speaks.
        let reasons = rejected_reasons(
            r#"{"hooks": {"pre_tool_use": [{"command": "a", "matcher": "(", "continue_turn": true}]}}"#,
        );
        assert!(reasons[0].contains("stop stage only"), "{reasons:?}");
        let reasons = rejected_reasons(
            r#"{"hooks": {"pre_tool_use": [{"command": "a", "matcher": "(", "timeout_ms": 0}]}}"#,
        );
        assert!(reasons[0].contains("between 1 and"), "{reasons:?}");
    }

    #[test]
    fn a_long_array_is_read_only_so_far_and_says_so() {
        let entries = (0..(MAX_ENTRIES_EXAMINED_PER_STAGE + 10))
            .map(|_| r#"{"command": "x", "oops": 1}"#)
            .collect::<Vec<_>>()
            .join(",");
        let (config, rejected) =
            parsed(&format!(r#"{{"hooks": {{"pre_tool_use": [{entries}]}}}}"#));
        assert!(config.pre_tool_use.is_empty());
        assert_eq!(
            rejected.len(),
            MAX_ENTRIES_EXAMINED_PER_STAGE + 1,
            "{}",
            rejected.len()
        );
        let last = rejected.last().expect("last");
        assert!(last.reason.contains("the rest are not read"), "{last:?}");
        assert_eq!(last.position, MAX_ENTRIES_EXAMINED_PER_STAGE);
    }

    #[test]
    fn a_command_of_only_spaces_is_not_a_command() {
        let reasons = rejected_reasons(r#"{"hooks": {"stop": [{"command": "   \t "}]}}"#);
        assert!(reasons[0].contains("`command` is empty"), "{reasons:?}");
        let (config, _) = parsed(r#"{"hooks": {"stop": ["   "]}}"#);
        assert_eq!(
            config.stop,
            vec!["   ".to_owned()],
            "a blank string entry is still kept as it always was (S11); only the object form is new"
        );
    }

    #[cfg(unix)]
    #[test]
    fn only_a_hook_with_its_own_timeout_leaves_the_callers_process_group() {
        let dir = std::env::temp_dir().join(format!("hook-pgid-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let group_of = |pid: &str| -> String {
            let out = std::process::Command::new("ps")
                .args(["-o", "pgid=", "-p", pid])
                .output()
                .expect("ps");
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        let mine = group_of(&std::process::id().to_string());
        let record = |name: &str| {
            let file = dir.join(format!("{name}.pgid"));
            let command = script(
                &dir,
                &format!("{name}.sh"),
                &format!(
                    "cat > /dev/null\nps -o pgid= -p $$ > {}\n",
                    test_fixtures::sh_quote(&file)
                ),
            );
            (command, file)
        };
        let (plain_command, plain_file) = record("plain");
        run_pre_tool_stage(
            &[HookEntry::plain(plain_command)],
            "shell_exec",
            "{}",
            HOOK_TIMEOUT,
        );
        assert_eq!(
            std::fs::read_to_string(&plain_file).expect("pgid").trim(),
            mine,
            "a hook with no timeout of its own stays in the caller's group, as it always did, so a terminal's Ctrl-C still reaches it"
        );
        let (own_command, own_file) = record("own");
        run_pre_tool_stage(
            &[HookEntry {
                timeout_ms: Some(5_000),
                ..HookEntry::plain(own_command)
            }],
            "shell_exec",
            "{}",
            HOOK_TIMEOUT,
        );
        let own = std::fs::read_to_string(&own_file).expect("pgid");
        assert_ne!(
            own.trim(),
            mine,
            "a hook with its own timeout is in a group of its own"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_timeout_of_its_own_bounds_an_observer_and_a_notify_hook_too() {
        let dir =
            std::env::temp_dir().join(format!("hook-observer-timeout-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let hang = HookEntry {
            timeout_ms: Some(200),
            ..HookEntry::plain(script(&dir, "hang.sh", "sleep 30"))
        };
        let started = Instant::now();
        let report = run_post_tool_failure_stage(
            std::slice::from_ref(&hang),
            "shell_exec",
            "boom",
            Duration::from_secs(60),
        );
        assert_eq!(
            report.failures.len(),
            1,
            "a timed-out observer is a recorded failure"
        );
        run_notify_hooks(
            std::slice::from_ref(&hang),
            "session_start",
            None,
            serde_json::json!({}),
            Duration::from_secs(60),
        );
        let report = run_subagent_stop_stage(
            std::slice::from_ref(&hang),
            "explore",
            "done",
            true,
            Duration::from_secs(60),
        );
        assert_eq!(report.failures.len(), 1);
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{:?}",
            started.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_failure_and_subagent_stop_stages_match_on_their_own_subjects() {
        let dir = std::env::temp_dir().join(format!("hook-subjects-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let selective = |name: &str, marker: &str, pattern: &str| HookEntry {
            matcher: Some(HookMatcher::new(pattern).expect("matcher")),
            ..HookEntry::plain(marker_hook(&dir, name, marker))
        };
        run_post_tool_failure_stage(
            &[selective("f1.sh", "fail.txt", "mcp__.*")],
            "shell_exec",
            "e",
            HOOK_TIMEOUT,
        );
        assert!(!dir.join("fail.txt").exists());
        run_post_tool_failure_stage(
            &[selective("f2.sh", "fail.txt", "mcp__.*")],
            "mcp__s__t",
            "e",
            HOOK_TIMEOUT,
        );
        assert!(
            dir.join("fail.txt").exists(),
            "the tool name is the subject"
        );
        run_subagent_stop_stage(
            &[selective("g1.sh", "stop.txt", "explore")],
            "coder",
            "done",
            true,
            HOOK_TIMEOUT,
        );
        assert!(!dir.join("stop.txt").exists());
        run_subagent_stop_stage(
            &[selective("g2.sh", "stop.txt", "explore")],
            "explore",
            "done",
            true,
            HOOK_TIMEOUT,
        );
        assert!(
            dir.join("stop.txt").exists(),
            "the agent type is the subject"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
