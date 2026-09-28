//! Six-mode permission lattice for the exec tool path (gaps.md §4 parity).
//!
//! Both reference CLIs converged on the same lattice: modes named exactly
//! `default | plan | acceptEdits | auto | dontAsk | bypassPermissions`, plus
//! `Tool(arg-glob)` allow/ask/deny rules where deny wins, remembered
//! per-project allow grants consulted before asking, and a typed reason on
//! every decision. This module is pure decision logic: no I/O, no prompts.
//! Headless exec cannot ask, so an `Ask` decision renders as a typed
//! model-visible denial at the driver — never a silent pass.

use std::collections::BTreeMap;
use std::fmt;

/// Hard ceiling on rules loaded for one decision context.
pub const MAX_RULES: usize = 128;
/// Hard ceiling on persisted grants consulted for one project.
pub const MAX_GRANTS: usize = 128;
/// Maximum UTF-8 bytes for one rule or grant pattern.
pub const MAX_PATTERN_BYTES: usize = 256;
/// Maximum UTF-8 bytes for the settings document.
pub const MAX_SETTINGS_BYTES: usize = 64 * 1024;
/// Maximum persisted projects in the grants file.
pub const MAX_GRANT_RECORDS: usize = 4096;

/// The six permission modes, named exactly as the reference CLIs name them.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum PermissionMode {
    Default,
    Plan,
    AcceptEdits,
    Auto,
    DontAsk,
    BypassPermissions,
}

pub const MODE_NAMES: [&str; 6] = [
    "default",
    "plan",
    "acceptEdits",
    "auto",
    "dontAsk",
    "bypassPermissions",
];

impl PermissionMode {
    /// Case-sensitive parse over the fixed name table (the mode names are
    /// wire-compatible identifiers, not free text).
    pub fn parse(raw: &str) -> Option<Self> {
        MODE_NAMES
            .iter()
            .position(|name| *name == raw)
            .map(|index| Self::ALL[index])
    }

    pub const fn as_str(self) -> &'static str {
        MODE_NAMES[self as usize]
    }

    const ALL: [Self; 6] = [
        Self::Default,
        Self::Plan,
        Self::AcceptEdits,
        Self::Auto,
        Self::DontAsk,
        Self::BypassPermissions,
    ];

    /// Permissiveness order for managed-policy ceiling gates (Modbit
    /// `CAP-001`: lower-trust layers may only restrict, never widen).
    /// `Plan` is deliberately the strictest of all six — it denies every
    /// write-classified call outright (`evaluate`'s `PlanModeDeny` arm),
    /// stricter than `Default`'s "ask" — not a position in `Self::ALL`'s
    /// declaration order, which exists only to match `MODE_NAMES`'s lookup
    /// table and carries no permissiveness meaning.
    pub const fn permissiveness_rank(self) -> u8 {
        match self {
            Self::Plan => 0,
            Self::Default => 1,
            Self::AcceptEdits => 2,
            Self::Auto => 3,
            Self::DontAsk => 4,
            Self::BypassPermissions => 5,
        }
    }
}

impl fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Effect of a `Tool(pattern)` rule.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RuleEffect {
    Allow,
    Ask,
    Deny,
}

/// How a call is classified for the mode table.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ToolClass {
    /// Pure read: auto-allowed in every mode once rules/grants had their say.
    ReadOnly,
    /// Bounded file edit inside the workspace (`acceptEdits`/`auto` allow).
    FileEdit,
    /// Any other state-mutating call (process execution, …).
    Other,
}

/// A `Tool(arg-glob)` rule: `Name` matches every call of the tool,
/// `Name(pattern)` additionally matches the call subject (path for file
/// tools, joined argv for `shell_exec`) with `*`/`?` glob semantics.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ToolRule {
    pub effect: RuleEffect,
    pub pattern: ToolPattern,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ToolPattern {
    tool: String,
    arg_glob: Option<String>,
}

impl ToolPattern {
    /// Parse `Name` or `Name(glob)`. Unknown tools are accepted as patterns:
    /// a deny rule for a tool this build does not know still reads as intent.
    pub fn parse(raw: &str) -> Option<Self> {
        if raw.is_empty() || raw.len() > MAX_PATTERN_BYTES {
            return None;
        }
        let (tool, arg_glob) = match raw.split_once('(') {
            Some((tool, rest)) => {
                let arg = rest.strip_suffix(')')?;
                if tool.is_empty() || arg.is_empty() {
                    return None;
                }
                (tool, Some(arg.to_owned()))
            }
            None => (raw, None),
        };
        if !valid_rule_ident(tool) {
            return None;
        }
        Some(Self {
            tool: tool.to_owned(),
            arg_glob,
        })
    }

    pub fn tool(&self) -> &str {
        &self.tool
    }

    pub fn arg_glob(&self) -> Option<&str> {
        self.arg_glob.as_deref()
    }

    /// The exact text [`Self::parse`] accepts back. The inverse lives beside
    /// the parser so no writer re-derives the `Tool` / `Tool(glob)` syntax —
    /// `render_grants` and `rapid permissions` both go through this.
    pub fn render(&self) -> String {
        match &self.arg_glob {
            Some(glob) => format!("{}({glob})", self.tool),
            None => self.tool.clone(),
        }
    }

    fn matches(&self, tool: &str, subject: &str) -> bool {
        if self.tool != tool {
            return false;
        }
        match &self.arg_glob {
            None => true,
            // Domain names are case-insensitive by spec (DNS) — the same
            // normalization `web_fetch::classify_fetch`'s own allowlist
            // already applies via `eq_ignore_ascii_case`. Without it here,
            // a plain case change in the request URL's host (attacker- or
            // redirect-controlled) silently bypasses a `deny`/`ask` rule,
            // or even an admin `denied_tools` ceiling documented as
            // un-overridable by any setting.
            Some(glob) => match (
                glob.strip_prefix("domain:"),
                subject.strip_prefix("domain:"),
            ) {
                (Some(pattern_domain), Some(subject_domain)) => glob_match(
                    &pattern_domain.to_ascii_lowercase(),
                    &subject_domain.to_ascii_lowercase(),
                ),
                // Path-shaped subjects (`workspace_write`/`workspace_read`/
                // `repo_read`/`workspace_patch`/`repo_glob`) also need
                // case-insensitive matching: the two most common desktop
                // filesystems this tool actually runs against — macOS's
                // default APFS and Windows' default NTFS — are both
                // case-insensitive (case-preserving, but insensitive for
                // lookups), so `Secrets/x` and `secrets/x` are the same file
                // on disk even though a plain-string glob comparison sees
                // them as different subjects. Without this, a deny rule (or
                // an admin `denied_tools` ceiling) written for `secrets/*`
                // never matches a model-supplied `Secrets/x`, even though
                // the write lands in the identical protected file. Shell
                // argv (`shell_exec`) is deliberately excluded — Unix
                // program-name lookup really is case-sensitive.
                _ if PATH_SUBJECT_TOOLS.contains(&tool) => {
                    glob_match(&glob.to_ascii_lowercase(), &subject.to_ascii_lowercase())
                }
                _ => glob_match(glob, subject),
            },
        }
    }
}

/// Tool names whose rule subject is a workspace-relative path, mirroring
/// `exec_tools.rs::rule_subject`'s own classification exactly (kept as
/// literal strings here, not a shared import, per this module's own "pure
/// decision logic" doc comment — it has no dependency on `exec_tools`).
const PATH_SUBJECT_TOOLS: &[&str] = &[
    "workspace_write",
    "workspace_read",
    "repo_read",
    "workspace_patch",
    "repo_glob",
];

/// `true` when `value` matches `pattern` with `*` (any run) and `?` (one
/// char). Iterative single-pass matcher, no regex dependency.
pub fn glob_match(pattern: &str, value: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let value: Vec<char> = value.chars().collect();
    let (mut p, mut v) = (0usize, 0usize);
    let (mut star, mut star_v) = (None::<usize>, 0usize);
    while v < value.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == value[v]) {
            p += 1;
            v += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            star_v = v;
            p += 1;
        } else if let Some(star_p) = star {
            p = star_p + 1;
            star_v += 1;
            v = star_v;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

/// Is `subject` equal to `scope` or a path under it? Segment-aware: `scope`
/// = "src" must not match `subject` = "src-other/file.rs" (a naive string
/// prefix would).
fn path_within_scope(scope: &str, subject: &str) -> bool {
    let scope = scope.trim_end_matches('/');
    if scope.is_empty() {
        return true;
    }
    subject == scope
        || subject
            .strip_prefix(scope)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn valid_rule_ident(tool: &str) -> bool {
    if tool.is_empty() || tool.len() > MAX_PATTERN_BYTES {
        return false;
    }
    let mut bytes = tool.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    tool.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
}

/// Typed reason carried by every decision. `as_str` is a stable identifier;
/// `explanation` is the model-facing one-liner.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum DecisionReason {
    DenyRule,
    AskRule,
    AllowRule,
    PersistedGrant,
    /// Refused by a persisted "never allow" answer for this project — the
    /// user's own standing refusal, ranked above the project's rules.
    PersistedDeny,
    /// Refused because the persisted answers could not be read: a "never
    /// allow" in them cannot be honoured, so nothing runs until the store
    /// is fixed.
    PersistedAnswersUnreadable,
    ReadOnlyAutoAllow,
    /// `auto` mode: a shell script made only of fixed safe commands
    /// (directory creation, `touch`, listing), with no redirect.
    AutoSafeCommand,
    EditModeAllow,
    BypassAllow,
    ModeAsk,
    PlanModeDeny,
    /// Plan mode's one write: the plan itself (`.rapidlm/plan.md`, or a
    /// markdown file under `.rapidlm/plans/`), where a plan is written
    /// (ADR 0024 §1).
    PlanFileCarveOut,
    DontAskDeny,
    UntrustedProject,
    /// Denied by a lattice-level `write_scope` ceiling (Modbit `CAP-008`/
    /// `AgentExecutionCapsule`'s narrow write scope) — a subagent confined
    /// to a subtree tried to mutate something outside it. Checked before
    /// every rule/grant/mode, since a scope ceiling is not something a
    /// lower-trust layer (a rule the child's own prompt could talk the
    /// model into proposing) may widen.
    WriteScopeViolation,
    /// Denied by an admin/managed-policy tool ban (Modbit `CAP-001`'s Policy
    /// Compiler: a hard, enterprise-level ceiling lower-trust layers — a
    /// project's rules, a user's grants, even `bypassPermissions` — may
    /// never widen past). Checked before every rule/grant/mode, same
    /// precedence as `WriteScopeViolation`.
    AdminToolDenied,
    /// Denied by an admin/managed-policy write-scope ceiling (Modbit
    /// `CAP-001`) — distinct from `WriteScopeViolation` (a `task_spawn`
    /// subagent's own narrower scope) so a denial reads as "the deployment
    /// confines every write here" rather than "this specific delegated
    /// task was scoped down." Checked before `write_scope`, every rule,
    /// grant, and mode.
    AdminWriteScopeViolation,
}

impl DecisionReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DenyRule => "deny_rule",
            Self::AskRule => "ask_rule",
            Self::AllowRule => "allow_rule",
            Self::PersistedGrant => "persisted_grant",
            Self::PersistedDeny => "persisted_deny",
            Self::PersistedAnswersUnreadable => "persisted_answers_unreadable",
            Self::ReadOnlyAutoAllow => "read_only_auto_allow",
            Self::AutoSafeCommand => "auto_safe_command",
            Self::EditModeAllow => "edit_mode_allow",
            Self::BypassAllow => "mode_allow",
            Self::ModeAsk => "mode_ask",
            Self::PlanModeDeny => "plan_mode_deny",
            Self::PlanFileCarveOut => "plan_file_carve_out",
            Self::DontAskDeny => "dont_ask_deny",
            Self::UntrustedProject => "untrusted_project",
            Self::WriteScopeViolation => "write_scope_violation",
            Self::AdminToolDenied => "admin_tool_denied",
            Self::AdminWriteScopeViolation => "admin_write_scope_violation",
        }
    }

    /// Model-facing one-liner. Static text only.
    pub const fn explanation(self) -> &'static str {
        match self {
            Self::DenyRule => "denied by an explicit deny rule",
            Self::AskRule => {
                "an ask rule requires approval, which no surface in this build can prompt for \
yet; pre-approve it with `rapid permissions allow <tool>` or a `permissions.allow` entry \
in .rapidlm/settings.json"
            }
            Self::AllowRule => "allowed by an explicit allow rule",
            Self::PersistedGrant => "allowed by a persisted per-project grant",
            Self::PersistedDeny => {
                "refused: the user chose never to allow this in this project \
(`rapid permissions revoke` undoes it)"
            }
            Self::PersistedAnswersUnreadable => {
                "refused: the user's saved permission answers cannot be read, so a \
\"never allow\" among them cannot be honoured; `rapid permissions list` names the file to fix"
            }
            Self::ReadOnlyAutoAllow => "allowed: read-only calls run without approval",
            Self::AutoSafeCommand => {
                "allowed: auto mode runs directory creation, touch and listing without approval"
            }
            Self::EditModeAllow => "allowed: the current mode auto-approves workspace edits",
            Self::BypassAllow => "allowed by bypassPermissions mode",
            // Reached in the *interactive* TUI as well as headless exec —
            // `PermissionMode::Default` is the out-of-box mode for both, and
            // `ExecTools` turns any non-`Allow` decision into a denial
            // because nothing anywhere can prompt for an approval yet (see
            // `newtask.md`'s "the interactive TUI cannot ask" entry). The
            // previous wording, "headless exec cannot ask", was therefore
            // false half the time it was shown, and named no way forward.
            Self::ModeAsk => {
                "requires approval, which no surface in this build can prompt for yet; \
pre-approve it with `rapid permissions allow <tool>`, or a `permissions.allow` entry in \
.rapidlm/settings.json, or set RAPIDLM_PERMISSION_MODE (acceptEdits allows file edits)"
            }
            Self::PlanModeDeny => "plan mode is read-only; this call mutates state",
            Self::PlanFileCarveOut => "plan mode allows writing the plan file",
            Self::DontAskDeny => "dontAsk mode silently refuses calls that are not pre-approved",
            Self::UntrustedProject => "the project is not trusted; every tool call is refused",
            Self::WriteScopeViolation => "outside the write scope this subagent was confined to",
            Self::AdminToolDenied => {
                "this tool is banned by managed policy; no setting can re-enable it"
            }
            Self::AdminWriteScopeViolation => {
                "outside the write scope managed policy confines this deployment to"
            }
        }
    }
}

/// Outcome of one permission evaluation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Decision {
    Allow(DecisionReason),
    Ask(DecisionReason),
    Deny(DecisionReason),
}

impl Decision {
    pub const fn is_allowed(self) -> bool {
        matches!(self, Self::Allow(_))
    }

    pub const fn is_denied(self) -> bool {
        matches!(self, Self::Deny(_))
    }

    pub const fn reason(self) -> DecisionReason {
        match self {
            Self::Allow(reason) | Self::Ask(reason) | Self::Deny(reason) => reason,
        }
    }
}

/// The lattice: one mode, ordered rules, and persisted per-project grants.
/// Evaluation is total and deterministic — every call gets a typed decision.
#[derive(Clone, Debug)]
pub struct PermissionLattice {
    mode: PermissionMode,
    rules: Vec<ToolRule>,
    grants: Vec<ToolPattern>,
    /// Persisted per-project "never allow" answers: refused before any
    /// project rule, grant or mode (after the managed ceilings).
    denials: Vec<ToolPattern>,
    /// The persisted answers exist but could not be read: every call is
    /// refused rather than a "never allow" silently dropped.
    denials_unreadable: bool,
    /// Workspace-relative path prefix a write-classified call's subject
    /// must fall under (Modbit `CAP-008`: a narrow write scope for a
    /// subagent, checked before every rule/grant/mode — see
    /// `DecisionReason::WriteScopeViolation`). `None`: no additional
    /// restriction, today's unscoped behavior.
    write_scope: Option<String>,
    /// Tools an admin/managed policy has banned outright (Modbit `CAP-001`
    /// Policy Compiler: an enterprise-level ceiling), checked before every
    /// rule/grant/mode — see `DecisionReason::AdminToolDenied`. Empty:
    /// no additional restriction, today's unmanaged behavior.
    denied_tools: Vec<ToolPattern>,
    /// Workspace-relative path prefix an admin/managed policy confines
    /// every write-classified call to (Modbit `CAP-001`), independent of
    /// — and checked before — the per-`task_spawn` `write_scope` above.
    /// Deliberately a *separate* field rather than reusing `write_scope`:
    /// `with_write_scope` overwrites (by design, for `task_spawn`'s "each
    /// call sets this subagent's own scope" use), and a subagent's own
    /// scope argument overwriting an admin ceiling instead of narrowing
    /// within it would silently defeat the ceiling. `None`: no additional
    /// restriction, today's unmanaged behavior.
    admin_write_scope: Option<String>,
}

/// Whether `subject` is where a plan is written: `.rapidlm/plan.md`, or a
/// markdown file under `.rapidlm/plans/` — no `..`, no other extension.
pub(crate) fn is_plan_file(subject: &str) -> bool {
    let path = subject.replace('\\', "/");
    if path.split('/').any(|part| part == ".." || part.is_empty()) {
        return false;
    }
    path == ".rapidlm/plan.md"
        || (path.starts_with(".rapidlm/plans/")
            && path.ends_with(".md")
            && path.len() > ".rapidlm/plans/.md".len())
}

/// The tool whose calls [`PermissionLattice::evaluate_shell`] judges.
const SHELL_TOOL: &str = "shell_exec";

/// Commands `auto` mode runs without asking when a whole script is made of
/// them with no redirect: directory creation, `touch`, listing.
pub const AUTO_SAFE_COMMANDS: &[&str] = &["mkdir", "touch", "ls", "pwd"];

/// How deep a `sh -c` inside a `sh -c` is read before the script counts
/// as unknown.
const MAX_SHELL_NESTING: usize = 4;

/// A `shell_exec` argv as the lattice judges it: each simple command the
/// shell will run, joined like a direct argv subject (assignments before a
/// command set apart), whether all of it could be read (`known`), whether
/// every command is one `auto` runs unasked, and the commands a wrapper
/// runs for it (`env rm …`, `sudo -u x rm …`, `find … -exec rm …`) —
/// matched by deny and ask rules only, never counted toward an allow.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ShellParts {
    pub commands: Vec<String>,
    pub wrapped: Vec<String>,
    pub known: bool,
    pub auto_safe: bool,
}

/// Commands that run another command given in their arguments.
const WRAPPERS: &[&str] = &[
    "env",
    "sudo",
    "doas",
    "su",
    "runuser",
    "nice",
    "nohup",
    "timeout",
    "time",
    "command",
    "exec",
    "builtin",
    "xargs",
    "stdbuf",
    "ionice",
    "chrt",
    "taskset",
    "setsid",
    "unbuffer",
    "caffeinate",
    "watch",
    "flock",
    "chroot",
    "strace",
    "ltrace",
    "script",
];

/// How a shell is invoked: with a readable `-c` script, in a way that
/// cannot be read with certainty, or not as a `-c` shell at all.
enum ShellInvocation<'a> {
    Script(&'a str),
    Unclear,
    NotAShell,
}

impl ShellParts {
    pub fn of(argv: &[String]) -> Self {
        let mut parts = Self {
            commands: Vec::new(),
            wrapped: Vec::new(),
            known: true,
            auto_safe: true,
        };
        parts.add(argv, false, 0, false);
        parts.auto_safe &= parts.known && !parts.commands.is_empty();
        parts
    }

    /// Add one command's argv: a shell's `-c` script is read into its own
    /// commands; anything else is one command as it stands. A `wrapped`
    /// command (one a wrapper runs) only feeds [`Self::wrapped`].
    fn add(&mut self, argv: &[String], redirected: bool, depth: usize, wrapped: bool) {
        let joined = argv.join(" ");
        let unknown = |parts: &mut Self| {
            if !wrapped {
                parts.known = false;
                parts.auto_safe = false;
            }
        };
        match shell_invocation(argv) {
            ShellInvocation::Script(script) if depth < MAX_SHELL_NESTING => {
                match security::parse_shell_script(script) {
                    Ok(parsed) => {
                        if parsed.opaque {
                            unknown(self);
                        }
                        for command in parsed.commands {
                            // An assignment, or a redirect of its own
                            // (`(ls) > out`), is not a safe command.
                            if !command.assignments.is_empty()
                                || (command.argv.is_empty() && command.redirected)
                            {
                                self.auto_safe = false;
                            }
                            if command.argv.is_empty() {
                                continue;
                            }
                            if matches!(
                                basename(&command.argv[0]),
                                "eval" | "source" | "." | "exec" | "command" | "builtin"
                            ) {
                                unknown(self);
                            }
                            self.add(&command.argv, command.redirected, depth + 1, wrapped);
                        }
                        return;
                    }
                    // Not readable whole: rules still see the joined argv.
                    Err(_) => unknown(self),
                }
            }
            ShellInvocation::Script(_) | ShellInvocation::Unclear => unknown(self),
            ShellInvocation::NotAShell => {}
        }
        if wrapped {
            self.wrapped.push(joined);
        } else {
            // A bare name only: `./ls` or `/tmp/x/touch` is whatever file
            // that is.
            let safe = !redirected
                && argv
                    .first()
                    .is_some_and(|name| AUTO_SAFE_COMMANDS.contains(&name.as_str()));
            self.auto_safe &= safe;
            self.commands.push(joined);
        }
        // What a wrapper runs: every tail of its arguments (its own options
        // are not known here), and what `find` runs after `-exec`.
        if depth < MAX_SHELL_NESTING
            && let Some(name) = argv.first().map(|name| basename(name))
        {
            let tails_from = if WRAPPERS.contains(&name) {
                Some(1)
            } else if name == "find" {
                argv.iter()
                    .position(|arg| matches!(arg.as_str(), "-exec" | "-execdir" | "-ok" | "-okdir"))
                    .map(|at| at + 1)
            } else {
                None
            };
            if let Some(from) = tails_from {
                for start in from..argv.len() {
                    self.add(&argv[start..], false, depth + 1, true);
                }
            }
        }
    }
}

/// How `argv` invokes a shell (see [`ShellInvocation`]). The script is the
/// argument after the first short-option cluster naming `c` (`-c`, `-ec`,
/// `-lc`), with only argument-free options before it; an option that takes
/// an argument (`-o`, `-O`), a word before the cluster, or an option after
/// it leave where the script is uncertain.
fn shell_invocation(argv: &[String]) -> ShellInvocation<'_> {
    let Some(first) = argv.first() else {
        return ShellInvocation::NotAShell;
    };
    let name = basename(first);
    if !matches!(
        name,
        "sh" | "bash" | "zsh" | "dash" | "ksh" | "mksh" | "ash" | "busybox"
    ) {
        return ShellInvocation::NotAShell;
    }
    let mut rest = &argv[1..];
    if name == "busybox" {
        if rest.first().map(String::as_str) != Some("sh") {
            return ShellInvocation::NotAShell;
        }
        rest = &rest[1..];
    }
    let is_cluster = |arg: &str| {
        (arg.starts_with('-') && !arg.starts_with("--") || arg.starts_with('+')) && arg.len() > 1
    };
    let Some(at) = rest
        .iter()
        .position(|arg| is_cluster(arg) && arg[1..].contains('c'))
    else {
        // No `-c`: a script file or stdin, judged as the argv it is.
        return ShellInvocation::NotAShell;
    };
    let before_is_plain = rest[..at].iter().all(|arg| {
        (arg.starts_with('-') || arg.starts_with('+')) && !arg[1..].contains(['o', 'O'])
    });
    if !before_is_plain || rest[at][1..].contains(['o', 'O']) {
        return ShellInvocation::Unclear;
    }
    match rest.get(at + 1) {
        Some(script) if !script.starts_with('-') && !script.starts_with('+') => {
            ShellInvocation::Script(script)
        }
        _ => ShellInvocation::Unclear,
    }
}

fn basename(raw: &str) -> &str {
    let base = raw
        .rsplit(['/', '\\'])
        .next()
        .filter(|part| !part.is_empty())
        .unwrap_or(raw);
    base.strip_suffix(".exe").unwrap_or(base)
}

impl PermissionLattice {
    pub fn new(mode: PermissionMode) -> Self {
        Self {
            mode,
            rules: Vec::new(),
            grants: Vec::new(),
            denials: Vec::new(),
            denials_unreadable: false,
            write_scope: None,
            denied_tools: Vec::new(),
            admin_write_scope: None,
        }
    }

    /// The same lattice — rules, grants, ceilings — in Plan mode: what a
    /// turn in plan mode is judged by (ADR 0024 §1).
    pub fn in_plan_mode(&self) -> Self {
        Self {
            mode: PermissionMode::Plan,
            ..self.clone()
        }
    }

    /// Add rules (deny/ask/allow stay distinct effects; matching order is
    /// deny > ask > allow). Excess rules beyond [`MAX_RULES`] are dropped —
    /// bounds are enforced at load, never panic.
    pub fn with_rules(mut self, rules: Vec<ToolRule>) -> Self {
        self.rules.truncate(MAX_RULES);
        self.rules
            .extend(rules.into_iter().take(MAX_RULES - self.rules.len()));
        self
    }

    /// Add persisted allow grants consulted before the mode would ask.
    pub fn with_grants(mut self, grants: Vec<ToolPattern>) -> Self {
        self.grants
            .extend(grants.into_iter().take(MAX_GRANTS - self.grants.len()));
        self
    }

    /// Add persisted "never allow" answers, refused before every project
    /// rule, grant and mode.
    pub fn with_denials(mut self, denials: Vec<ToolPattern>) -> Self {
        self.denials
            .extend(denials.into_iter().take(MAX_GRANTS - self.denials.len()));
        self
    }

    pub fn denials(&self) -> &[ToolPattern] {
        &self.denials
    }

    /// The persisted answers could not be read: refuse every call (fail
    /// closed) rather than run without the user's "never allow" answers.
    pub fn with_unreadable_denials(mut self) -> Self {
        self.denials_unreadable = true;
        self
    }

    /// Confine every write-classified call to `scope` (a workspace-relative
    /// path prefix) or its descendants — read-classified calls are
    /// unaffected. Rules, grants, and mode can only make a write *harder* to
    /// get inside the scope; none of them can widen past it.
    pub fn with_write_scope(mut self, scope: impl Into<String>) -> Self {
        self.write_scope = Some(scope.into());
        self
    }

    /// Ban every tool matching `patterns` outright (Modbit `CAP-001`): no
    /// rule, grant, or mode checked afterward — including `bypassPermissions`
    /// — can re-allow one. Additive with any existing bans, never replaces
    /// them, so an admin ceiling set once at construction can't be narrowed
    /// away by a later call.
    pub fn with_denied_tools(mut self, patterns: impl IntoIterator<Item = ToolPattern>) -> Self {
        self.denied_tools.extend(patterns);
        self
    }

    /// Confine every write-classified call, deployment-wide, to `scope` or
    /// its descendants (Modbit `CAP-001`) — independent of `with_write_scope`,
    /// which a `task_spawn` call may still set its own (narrower-in-intent)
    /// scope through without affecting this one. See `admin_write_scope`'s
    /// own doc comment for why the two must not share a field.
    pub fn with_admin_write_scope(mut self, scope: impl Into<String>) -> Self {
        self.admin_write_scope = Some(scope.into());
        self
    }

    pub const fn mode(&self) -> PermissionMode {
        self.mode
    }

    pub fn rules(&self) -> &[ToolRule] {
        &self.rules
    }

    pub fn write_scope(&self) -> Option<&str> {
        self.write_scope.as_deref()
    }

    pub fn denied_tools(&self) -> &[ToolPattern] {
        &self.denied_tools
    }

    pub fn admin_write_scope(&self) -> Option<&str> {
        self.admin_write_scope.as_deref()
    }

    /// Lattice for a `task_spawn` child. The child is the model's own choice
    /// of what to delegate and with what prompt — never the human's direct
    /// action — so it must not silently wield authority the human never
    /// reviewed. Rules and persisted grants carry over unchanged (a child can
    /// still *consume* what was already approved, so it isn't reduced to
    /// asking for everything from scratch), but `BypassPermissions` — the one
    /// mode that allows every call with no `Ask` step at all, per
    /// [`Self::evaluate`]'s mode table — is capped to `AcceptEdits`. Every
    /// other mode already denies (never silently allows) a non-file-edit call
    /// for a subagent, since subagents run headless-style with no interactive
    /// channel and `Ask` renders as a denial there; `BypassPermissions` is the
    /// only mode where that safety net doesn't already apply.
    pub fn for_subagent(&self) -> Self {
        Self {
            mode: match self.mode {
                PermissionMode::BypassPermissions => PermissionMode::AcceptEdits,
                other => other,
            },
            rules: self.rules.clone(),
            grants: self.grants.clone(),
            denials: self.denials.clone(),
            denials_unreadable: self.denials_unreadable,
            write_scope: self.write_scope.clone(),
            denied_tools: self.denied_tools.clone(),
            admin_write_scope: self.admin_write_scope.clone(),
        }
    }

    /// Evaluate one call. `tool` is the gateway tool name, `subject` the
    /// rule-matching context (workspace-relative path for file tools, joined
    /// argv for `shell_exec`).
    pub fn evaluate(&self, tool: &str, subject: &str, class: ToolClass) -> Decision {
        self.judge(tool, subject, class, None)
    }

    /// Evaluate one `shell_exec` call by its argv. A shell's `-c` script is
    /// judged command by command ([`ShellParts`]): a deny or ask rule — or a
    /// managed ban or a "never allow" — matching any one command decides;
    /// an allow rule or a grant allows only when every command is covered;
    /// a script that cannot be read whole (a substitution, a compound
    /// command, a heredoc, …) is never allowed by a rule or grant, only by
    /// the mode. Rules are still matched against the joined argv too, so a
    /// rule written against it keeps denying. In `auto` mode a script made
    /// only of [`AUTO_SAFE_COMMANDS`], with no redirect or assignment, runs.
    pub fn evaluate_shell(&self, argv: &[String]) -> Decision {
        let parts = ShellParts::of(argv);
        self.judge(SHELL_TOOL, &argv.join(" "), ToolClass::Other, Some(&parts))
    }

    fn judge(
        &self,
        tool: &str,
        subject: &str,
        class: ToolClass,
        shell: Option<&ShellParts>,
    ) -> Decision {
        // A pattern hits the call when it matches the subject or, for a
        // shell script, any one of its commands.
        let hits = |pattern: &ToolPattern| {
            pattern.matches(tool, subject)
                || shell.is_some_and(|parts| {
                    parts
                        .commands
                        .iter()
                        .chain(&parts.wrapped)
                        .any(|command| pattern.matches(tool, command))
                })
        };
        // A set of patterns covers the call when one matches the subject —
        // or, for a shell script read whole, when each command is matched
        // by one of them.
        let covers = |patterns: &mut dyn Iterator<Item = &ToolPattern>| {
            let patterns: Vec<&ToolPattern> = patterns.collect();
            match shell {
                None => patterns
                    .iter()
                    .any(|pattern| pattern.matches(tool, subject)),
                Some(parts) => {
                    parts.known
                        && !parts.commands.is_empty()
                        && parts.commands.iter().all(|command| {
                            patterns
                                .iter()
                                .any(|pattern| pattern.matches(tool, command))
                        })
                }
            }
        };
        // -1. Admin/managed-policy tool ban, checked before absolutely
        // everything else, including the write-scope ceiling below — the
        // one restriction nothing downstream (a rule, a grant, any mode,
        // including bypassPermissions) may ever widen past.
        if self.denied_tools.iter().any(hits) {
            return Decision::Deny(DecisionReason::AdminToolDenied);
        }
        // -0.5. Admin/managed-policy write-scope ceiling — same precedence
        // as the tool ban above, and deliberately checked *before* the
        // per-`task_spawn` write_scope below: a subagent's own scope
        // argument must never be able to widen past a deployment-wide
        // confinement, only add a further restriction inside it.
        if class == ToolClass::FileEdit
            && let Some(scope) = &self.admin_write_scope
            && !path_within_scope(scope, subject)
        {
            return Decision::Deny(DecisionReason::AdminWriteScopeViolation);
        }
        // 0. Write-scope ceiling, checked before everything else — a rule,
        // grant, or mode may only make a write *harder* to get inside the
        // scope, never widen past it. Scoped to genuine file-edit calls
        // only: `shell_exec`'s `subject` is joined argv, not a workspace
        // path, so applying a path-prefix check to it would be meaningless.
        if class == ToolClass::FileEdit
            && let Some(scope) = &self.write_scope
            && !path_within_scope(scope, subject)
        {
            return Decision::Deny(DecisionReason::WriteScopeViolation);
        }
        // 0.5. The user's persisted "never allow" answers: a standing
        // refusal the user made for this project outranks the project's own
        // rules (a lower-trust layer) and every grant and mode — and holds
        // for read-classified calls too (a fetch domain, an MCP tool).
        if self.denials_unreadable {
            return Decision::Deny(DecisionReason::PersistedAnswersUnreadable);
        }
        if self.denials.iter().any(hits) {
            return Decision::Deny(DecisionReason::PersistedDeny);
        }
        // 0.75. Plan mode's absolute write floor, checked before any rule:
        // `permissiveness_rank`'s own doc comment describes Plan as denying
        // "every write-classified call outright... stricter than Default's
        // 'ask'" — a ceiling, not a default that a lower-trust project
        // settings file's ordinary `allow` rule should be able to widen past
        // (the same property the admin ceilings above already enforce for
        // denied tools/write scope). Without this check, an `allow` rule
        // matched at step 1 below would return `Decision::Allow` before the
        // mode table ever saw the call, silently defeating both a plain
        // `"plan"` mode and an admin `max_permission_mode` ceiling that
        // forced it. Scoped to non-`ReadOnly` calls only: the model still
        // needs to read files to produce a plan.
        if self.mode == PermissionMode::Plan && class != ToolClass::ReadOnly {
            // Its one write is the plan itself — a file edit, to a plan
            // path, that no deny rule forbids.
            if class == ToolClass::FileEdit && is_plan_file(subject) {
                let matches = |effect: RuleEffect| {
                    self.rules
                        .iter()
                        .any(|rule| rule.effect == effect && rule.pattern.matches(tool, subject))
                };
                // Deny rules win; an ask rule still asks.
                return if matches(RuleEffect::Deny) {
                    Decision::Deny(DecisionReason::DenyRule)
                } else if matches(RuleEffect::Ask) {
                    Decision::Ask(DecisionReason::AskRule)
                } else {
                    Decision::Allow(DecisionReason::PlanFileCarveOut)
                };
            }
            return Decision::Deny(DecisionReason::PlanModeDeny);
        }
        // 1. Rules, by precedence not insertion order: deny wins, then ask,
        // then allow.
        for rule in &self.rules {
            if rule.effect == RuleEffect::Deny && hits(&rule.pattern) {
                return Decision::Deny(DecisionReason::DenyRule);
            }
        }
        for rule in &self.rules {
            if rule.effect == RuleEffect::Ask && hits(&rule.pattern) {
                return Decision::Ask(DecisionReason::AskRule);
            }
        }
        if covers(
            &mut self
                .rules
                .iter()
                .filter(|rule| rule.effect == RuleEffect::Allow)
                .map(|rule| &rule.pattern),
        ) {
            return Decision::Allow(DecisionReason::AllowRule);
        }
        // 2. Read-only calls run without approval in every mode.
        if class == ToolClass::ReadOnly {
            return Decision::Allow(DecisionReason::ReadOnlyAutoAllow);
        }
        // 3. Persisted per-project grants suppress the ask.
        if covers(&mut self.grants.iter()) {
            return Decision::Allow(DecisionReason::PersistedGrant);
        }
        // 3.5. `auto` runs a script of fixed safe commands without asking.
        if self.mode == PermissionMode::Auto && shell.is_some_and(|parts| parts.auto_safe) {
            return Decision::Allow(DecisionReason::AutoSafeCommand);
        }
        // 4. Mode table.
        match self.mode {
            PermissionMode::Default => Decision::Ask(DecisionReason::ModeAsk),
            PermissionMode::Plan => Decision::Deny(DecisionReason::PlanModeDeny),
            PermissionMode::AcceptEdits | PermissionMode::Auto => {
                if class == ToolClass::FileEdit {
                    Decision::Allow(DecisionReason::EditModeAllow)
                } else {
                    Decision::Ask(DecisionReason::ModeAsk)
                }
            }
            PermissionMode::DontAsk => Decision::Deny(DecisionReason::DontAskDeny),
            PermissionMode::BypassPermissions => Decision::Allow(DecisionReason::BypassAllow),
        }
    }

    /// The persisted grant that would answer this call from now on — what
    /// an approve-and-remember answer to its ask records — or `None` when
    /// no grant can. `subject` is the call's rule subject, `None` for a tool
    /// whose calls carry none (every call of it is the same to the lattice).
    ///
    /// The grant names this tool and this subject, so it covers no other
    /// subject — but paths compare case-insensitively, as every path pattern
    /// does (on a case-sensitive filesystem it also answers the same path in
    /// another case), and for a tool whose calls carry no subject (an MCP tool)
    /// or a fixed one (`task_spawn`, `todo_write`) it answers every call of the
    /// tool. There is none for `shell_exec` (its subject is a joined argv,
    /// which `["a b"]` and `["a", "b"]` share, so no pattern names one command;
    /// standing shell approvals stay the user's own `rapid permissions allow`
    /// patterns), for an empty subject or one holding a glob character (the
    /// pattern would match more than this call), for a project already at
    /// [`MAX_GRANTS`] (the store refuses one more), or when a layer ranked
    /// above grants would still decide the call — a managed ban or write
    /// confinement, a write scope, plan mode, a deny or ask rule: a grant never
    /// outranks them, so remembering would not stop the next ask.
    pub fn standing_grant_for(
        &self,
        tool: &str,
        subject: Option<&str>,
        class: ToolClass,
    ) -> Option<ToolPattern> {
        if tool == "shell_exec" || self.grants.len() >= MAX_GRANTS {
            return None;
        }
        let grant = match subject {
            None => ToolPattern::parse(tool)?,
            Some(subject) if subject.is_empty() || subject.contains(['*', '?']) => return None,
            Some(subject) => ToolPattern::parse(&format!("{tool}({subject})"))?,
        };
        let remembered = Self {
            grants: vec![grant.clone()],
            ..self.clone()
        };
        (remembered.evaluate(tool, subject.unwrap_or_default(), class)
            == Decision::Allow(DecisionReason::PersistedGrant))
        .then_some(grant)
    }
}

/// A parsed project settings document (`permissions` rules + default mode).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProjectSettings {
    pub mode: Option<PermissionMode>,
    pub rules: Vec<ToolRule>,
}

/// Typed settings failure. Never echoes document content.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettingsError {
    TooLarge,
    InvalidJson,
    InvalidRule,
    TooManyRules,
}

impl SettingsError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TooLarge => "settings document exceeds the size bound",
            Self::InvalidJson => "settings document is not valid JSON",
            Self::InvalidRule => "a permission rule is not a valid Tool(pattern)",
            Self::TooManyRules => "settings document exceeds the rule count bound",
        }
    }
}

/// Parse a settings document. Accepts both shapes the ecosystem writes:
/// `{"mode": "...", "permissions": {...}}` and the Claude-compatible
/// `{"permissions": {"defaultMode": "...", "allow": [...], "ask": [...],
/// "deny": [...]}}`. Unknown keys are ignored; bound violations fail typed.
pub fn parse_settings(text: &str) -> Result<ProjectSettings, SettingsError> {
    if text.len() > MAX_SETTINGS_BYTES {
        return Err(SettingsError::TooLarge);
    }
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|_| SettingsError::InvalidJson)?;
    let object = value.as_object().ok_or(SettingsError::InvalidJson)?;
    let mut settings = ProjectSettings::default();
    if let Some(mode) = object.get("mode").and_then(serde_json::Value::as_str) {
        settings.mode = Some(PermissionMode::parse(mode).ok_or(SettingsError::InvalidRule)?);
    }
    let Some(permissions) = object.get("permissions") else {
        return Ok(settings);
    };
    let permissions = permissions.as_object().ok_or(SettingsError::InvalidJson)?;
    if settings.mode.is_none()
        && let Some(mode) = permissions
            .get("defaultMode")
            .and_then(serde_json::Value::as_str)
    {
        settings.mode = Some(PermissionMode::parse(mode).ok_or(SettingsError::InvalidRule)?);
    }
    for (key, effect) in [
        ("deny", RuleEffect::Deny),
        ("ask", RuleEffect::Ask),
        ("allow", RuleEffect::Allow),
    ] {
        let Some(entries) = permissions.get(key).and_then(serde_json::Value::as_array) else {
            continue;
        };
        for entry in entries {
            let raw = entry.as_str().ok_or(SettingsError::InvalidRule)?;
            let pattern = ToolPattern::parse(raw).ok_or(SettingsError::InvalidRule)?;
            settings.rules.push(ToolRule { effect, pattern });
            if settings.rules.len() > MAX_RULES {
                return Err(SettingsError::TooManyRules);
            }
        }
    }
    Ok(settings)
}

/// Persisted per-project allow grants, keyed by canonical project root.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PermissionGrants {
    records: BTreeMap<String, Vec<ToolPattern>>,
    /// "Never allow" answers, keyed the same way.
    denials: BTreeMap<String, Vec<ToolPattern>>,
}

/// Typed grants-file failure. Corrupt input fails closed: callers treat it as
/// "no grants", never as "grant everything".
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GrantsError {
    TooLarge,
    InvalidJson,
    InvalidGrant,
    TooManyRecords,
}

impl PermissionGrants {
    /// Allow patterns recorded for `canonical_root` (empty when absent).
    pub fn for_root(&self, canonical_root: &str) -> Vec<ToolPattern> {
        self.records
            .get(canonical_root)
            .cloned()
            .unwrap_or_default()
    }

    /// Record `pattern` for `canonical_root`. Returns whether anything
    /// changed — granting an already-granted pattern is idempotent, the same
    /// contract `rapid trust grant` has.
    ///
    /// Bounded by [`MAX_GRANTS`] per project and [`MAX_GRANT_RECORDS`]
    /// projects, the same limits [`parse_grants`] enforces on the way in: a
    /// writer that could produce a document its own reader would reject (or
    /// silently truncate) is how a grant "disappears" with no diagnostic.
    pub fn allow(
        &mut self,
        canonical_root: &str,
        pattern: ToolPattern,
    ) -> Result<bool, GrantsError> {
        if !self.knows_root(canonical_root) && self.root_count() >= MAX_GRANT_RECORDS {
            return Err(GrantsError::TooManyRecords);
        }
        // The newer answer stands: a "never allow" of the same pattern goes.
        let dropped_denial = Self::remove_from(&mut self.denials, canonical_root, &pattern);
        let entry = self.records.entry(canonical_root.to_owned()).or_default();
        if entry.iter().any(|existing| existing == &pattern) {
            return Ok(dropped_denial);
        }
        if entry.len() >= MAX_GRANTS {
            return Err(GrantsError::InvalidGrant);
        }
        entry.push(pattern);
        entry.sort_by_key(ToolPattern::render);
        Ok(true)
    }

    /// "Never allow" patterns recorded for `canonical_root`.
    pub fn denials_for(&self, canonical_root: &str) -> Vec<ToolPattern> {
        self.denials
            .get(canonical_root)
            .cloned()
            .unwrap_or_default()
    }

    /// Record a "never allow" `pattern` for `canonical_root`; an allow grant
    /// of the same pattern is dropped (the newer answer stands). Returns
    /// whether anything changed. Same bounds as [`Self::allow`].
    pub fn deny(
        &mut self,
        canonical_root: &str,
        pattern: ToolPattern,
    ) -> Result<bool, GrantsError> {
        if !self.knows_root(canonical_root) && self.root_count() >= MAX_GRANT_RECORDS {
            return Err(GrantsError::TooManyRecords);
        }
        let dropped_allow = Self::remove_from(&mut self.records, canonical_root, &pattern);
        let entry = self.denials.entry(canonical_root.to_owned()).or_default();
        if entry.iter().any(|existing| existing == &pattern) {
            return Ok(dropped_allow);
        }
        if entry.len() >= MAX_GRANTS {
            return Err(GrantsError::InvalidGrant);
        }
        entry.push(pattern);
        entry.sort_by_key(ToolPattern::render);
        Ok(true)
    }

    /// Remove `pattern` from `canonical_root`, as a grant or a "never allow"
    /// answer. Returns whether anything changed. A root left with nothing is
    /// dropped entirely rather than persisted as an empty record.
    pub fn revoke(&mut self, canonical_root: &str, pattern: &ToolPattern) -> bool {
        let allow = Self::remove_from(&mut self.records, canonical_root, pattern);
        let deny = Self::remove_from(&mut self.denials, canonical_root, pattern);
        allow || deny
    }

    fn knows_root(&self, canonical_root: &str) -> bool {
        self.records.contains_key(canonical_root) || self.denials.contains_key(canonical_root)
    }

    /// Every project with a grant or a "never allow" answer.
    fn roots(&self) -> std::collections::BTreeSet<&String> {
        self.records.keys().chain(self.denials.keys()).collect()
    }

    fn root_count(&self) -> usize {
        self.roots().len()
    }

    fn remove_from(
        map: &mut BTreeMap<String, Vec<ToolPattern>>,
        canonical_root: &str,
        pattern: &ToolPattern,
    ) -> bool {
        let Some(entry) = map.get_mut(canonical_root) else {
            return false;
        };
        let before = entry.len();
        entry.retain(|existing| existing != pattern);
        let changed = entry.len() != before;
        if entry.is_empty() {
            map.remove(canonical_root);
        }
        changed
    }
}

/// Serialize the persisted grants document — the exact inverse of
/// [`parse_grants`], and the only writer of this format.
///
/// Nothing wrote this file before: `parse_grants` was a reader with no
/// counterpart, which made `Decision::PersistedGrant` — step 3 of
/// [`PermissionLattice::evaluate`], "persisted per-project grants suppress
/// the ask" — unreachable in production. `rapid permissions` is the writer.
pub fn render_grants(grants: &PermissionGrants) -> Result<String, GrantsError> {
    let projects: Vec<serde_json::Value> = grants
        .roots()
        .into_iter()
        .map(|root| {
            let allow = grants.for_root(root);
            let deny = grants.denials_for(root);
            let mut project = serde_json::json!({
                "root": root,
                "allow": allow.iter().map(ToolPattern::render).collect::<Vec<_>>(),
            });
            // Only when there is one: a store with no refusals is written
            // exactly as before they existed.
            if !deny.is_empty() {
                project["deny"] = deny.iter().map(ToolPattern::render).collect();
            }
            project
        })
        .collect();
    // Schema 2 exactly when a refusal is recorded: a binary that predates
    // refusals reads only schema 1, so it refuses such a store (and its
    // writer will not overwrite it) instead of rewriting it without the
    // refusals. A store with none stays schema 1, readable by every build.
    let schema = if grants.denials.is_empty() { 1 } else { 2 };
    let document = serde_json::json!({ "schema": schema, "projects": projects });
    let text = serde_json::to_string_pretty(&document).map_err(|_| GrantsError::InvalidJson)?;
    // `+ 1` for the trailing newline every writer appends. Bounding the
    // pre-newline text let a document rendering to exactly
    // `MAX_SETTINGS_BYTES` be written one byte over the limit its own
    // loader enforces, at which point `parse_grants` rejects it, the
    // run-time reader fails closed, and *every* project's grants disappear
    // with no diagnostic.
    if text.len() + 1 > MAX_SETTINGS_BYTES {
        return Err(GrantsError::TooLarge);
    }
    Ok(text)
}

/// Parse the persisted grants document:
/// `{"schema": 1, "projects": [{"root": "...", "allow": ["Tool(glob)"]}]}.
pub fn parse_grants(text: &str) -> Result<PermissionGrants, GrantsError> {
    if text.len() > MAX_SETTINGS_BYTES {
        return Err(GrantsError::TooLarge);
    }
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|_| GrantsError::InvalidJson)?;
    let object = value.as_object().ok_or(GrantsError::InvalidJson)?;
    let schema = object.get("schema").and_then(serde_json::Value::as_u64);
    if !matches!(schema, Some(1 | 2)) {
        return Err(GrantsError::InvalidJson);
    }
    let mut grants = PermissionGrants::default();
    let Some(projects) = object.get("projects").and_then(serde_json::Value::as_array) else {
        return Ok(grants);
    };
    if projects.len() > MAX_GRANT_RECORDS {
        return Err(GrantsError::TooManyRecords);
    }
    for project in projects {
        let project = project.as_object().ok_or(GrantsError::InvalidJson)?;
        let root = project
            .get("root")
            .and_then(serde_json::Value::as_str)
            .ok_or(GrantsError::InvalidJson)?;
        let mut allow = Vec::new();
        for entry in project
            .get("allow")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
        {
            let raw = entry.as_str().ok_or(GrantsError::InvalidGrant)?;
            let pattern = ToolPattern::parse(raw).ok_or(GrantsError::InvalidGrant)?;
            allow.push(pattern);
            if allow.len() > MAX_GRANTS {
                break;
            }
        }
        let mut deny = Vec::new();
        // Read whatever the schema: a refusal found is honoured (a store
        // from a build that wrote them before schema 2 existed keeps them).
        for entry in project
            .get("deny")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
        {
            let raw = entry.as_str().ok_or(GrantsError::InvalidGrant)?;
            let pattern = ToolPattern::parse(raw).ok_or(GrantsError::InvalidGrant)?;
            deny.push(pattern);
            if deny.len() > MAX_GRANTS {
                break;
            }
        }
        if !allow.is_empty() || deny.is_empty() {
            grants.records.insert(root.to_owned(), allow);
        }
        if !deny.is_empty() {
            grants.denials.insert(root.to_owned(), deny);
        }
    }
    Ok(grants)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell(script: &str) -> Vec<String> {
        vec!["bash".to_owned(), "-c".to_owned(), script.to_owned()]
    }

    fn lattice_with(mode: PermissionMode, rules: &[(RuleEffect, &str)]) -> PermissionLattice {
        PermissionLattice::new(mode).with_rules(
            rules
                .iter()
                .map(|(effect, raw)| ToolRule {
                    effect: *effect,
                    pattern: ToolPattern::parse(raw).expect("rule"),
                })
                .collect(),
        )
    }

    #[test]
    fn shell_rules_match_the_parsed_script_command_by_command() {
        use RuleEffect::{Allow, Deny};
        let allow = Decision::Allow(DecisionReason::AllowRule);
        let ask = Decision::Ask(DecisionReason::ModeAsk);
        let deny = Decision::Deny(DecisionReason::DenyRule);
        let git = lattice_with(PermissionMode::Default, &[(Allow, "shell_exec(git *)")]);
        let argv = |words: &[&str]| words.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>();
        // A direct argv, as before.
        assert_eq!(git.evaluate_shell(&argv(&["git", "status"])), allow);
        // `;` and `&&`: every command must be covered.
        assert_eq!(git.evaluate_shell(&shell("git status && git diff")), allow);
        assert_eq!(git.evaluate_shell(&shell("git status; rm -rf ~")), ask);
        // Quoting neither bypasses nor over-prompts: a quoted `;` is an
        // argument, not a second command.
        assert_eq!(
            git.evaluate_shell(&shell("git commit -m 'fix; rm -rf /'")),
            allow
        );
        assert_eq!(git.evaluate_shell(&shell("git commit -m \"$MSG\"")), allow);
        // Pipelines and subshells.
        assert_eq!(git.evaluate_shell(&shell("git log | head -5")), ask);
        let git_head = lattice_with(
            PermissionMode::Default,
            &[
                (Allow, "shell_exec(git *)"),
                (Allow, "shell_exec(head *)"),
                (Allow, "shell_exec(cd *)"),
            ],
        );
        assert_eq!(git_head.evaluate_shell(&shell("git log | head -5")), allow);
        assert_eq!(
            git_head.evaluate_shell(&shell("(cd src && git status)")),
            allow
        );
        // Unknown constructs are never allowed by a rule: a substitution, a
        // heredoc, a compound command, eval.
        let cat = lattice_with(
            PermissionMode::Default,
            &[(Allow, "shell_exec(git *)"), (Allow, "shell_exec(cat *)")],
        );
        for script in [
            "git log $(rm -rf ~)",
            "git log `rm -rf ~`",
            "cat <<EOF\nx\nEOF",
            "if true; then git status; fi",
            "eval git status",
        ] {
            assert_eq!(cat.evaluate_shell(&shell(script)), ask, "{script}");
        }
        // Where the script is uncertain, no rule covers it: options after
        // `-c`, an option taking an argument, `$'…'`, a continuation, a
        // command named by a variable.
        let everything = lattice_with(PermissionMode::Default, &[(Allow, "shell_exec(*)")]);
        assert_eq!(everything.evaluate_shell(&shell("git status")), allow);
        for call in [
            argv(&["bash", "-c", "-e", "rm -rf x"]),
            argv(&["bash", "-o", "pipefail", "-c", "rm -rf x"]),
            shell("$'rm' -rf x"),
            shell("r\\\nm -rf x"),
            shell("X=rm; $X -rf x"),
        ] {
            assert_eq!(everything.evaluate_shell(&call), ask, "{call:?}");
        }
        // The old bypass: a rule written against the joined argv of a
        // shell no longer allows what follows the covered command.
        let joined = lattice_with(
            PermissionMode::Default,
            &[(Allow, "shell_exec(bash -c git *)")],
        );
        assert_ne!(joined.evaluate_shell(&shell("git status; rm -rf ~")), allow);
        // A deny rule hits any one command — behind an assignment, a
        // keyword, a nested shell — in every mode.
        for mode in [PermissionMode::Default, PermissionMode::BypassPermissions] {
            let rm = lattice_with(
                mode,
                &[(Allow, "shell_exec(*)"), (Deny, "shell_exec(rm *)")],
            );
            for script in [
                "echo hi; rm -rf x",
                "FOO=1 rm -rf x",
                "if true; then rm -rf x; fi",
                "sh -c 'rm -rf x'",
                "ls | xargs echo && (rm -rf x)",
                // A `#` inside a word is not a comment.
                "mkdir a#;rm -rf x",
                // What a wrapper runs.
                "env FOO=1 rm -rf x",
                "sudo -u root rm -rf x",
                "timeout 5 sh -c 'rm -rf x'",
                "find . -name '*.o' -exec rm -rf {} ;",
                "ls | xargs rm -rf",
            ] {
                assert_eq!(rm.evaluate_shell(&shell(script)), deny, "{mode:?} {script}");
            }
            // The same wrappers as a direct argv.
            assert_eq!(
                rm.evaluate_shell(&argv(&["env", "rm", "-rf", "x"])),
                deny,
                "{mode:?}"
            );
            // A deny written against the joined argv still denies.
            let joined = lattice_with(mode, &[(Deny, "shell_exec(bash -c *rm*)")]);
            assert_eq!(joined.evaluate_shell(&shell("rm -rf x")), deny);
        }
    }

    #[test]
    fn auto_mode_runs_only_the_safe_list_and_only_in_auto() {
        let argv = |words: &[&str]| words.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>();
        let auto = PermissionLattice::new(PermissionMode::Auto);
        let safe = Decision::Allow(DecisionReason::AutoSafeCommand);
        assert_eq!(auto.evaluate_shell(&argv(&["mkdir", "-p", "a/b"])), safe);
        assert_eq!(
            auto.evaluate_shell(&shell("mkdir -p a && touch a/b && ls a")),
            safe
        );
        assert_eq!(auto.evaluate_shell(&argv(&["pwd"])), safe);
        // Only a bare name: a path is whatever file it names.
        assert_eq!(
            auto.evaluate_shell(&argv(&["./ls"])),
            Decision::Ask(DecisionReason::ModeAsk)
        );
        for script in [
            "ls > listing.txt",
            "mkdir a; rm -rf a",
            "touch $(whoami)",
            "FOO=1 ls",
            "ls | sh",
            "(ls) > listing.txt",
            "mkdir a#;rm -rf ~",
            "./ls",
            "/tmp/evil/touch x",
        ] {
            assert_eq!(
                auto.evaluate_shell(&shell(script)),
                Decision::Ask(DecisionReason::ModeAsk),
                "{script}"
            );
        }
        // Only in auto.
        for mode in [PermissionMode::Default, PermissionMode::AcceptEdits] {
            assert_eq!(
                PermissionLattice::new(mode).evaluate_shell(&argv(&["mkdir", "a"])),
                Decision::Ask(DecisionReason::ModeAsk),
                "{mode:?}"
            );
        }
        // A deny rule still wins.
        let denied = lattice_with(
            PermissionMode::Auto,
            &[(RuleEffect::Deny, "shell_exec(mkdir *)")],
        );
        assert_eq!(
            denied.evaluate_shell(&argv(&["mkdir", "a"])),
            Decision::Deny(DecisionReason::DenyRule)
        );
    }

    #[test]
    fn a_persisted_never_allow_outranks_rules_grants_and_every_mode() {
        let never = ToolPattern::parse("web_fetch(domain:evil.example)").expect("pattern");
        let allow_rule = ToolRule {
            effect: RuleEffect::Allow,
            pattern: ToolPattern::parse("web_fetch").expect("rule"),
        };
        for mode in [
            PermissionMode::Default,
            PermissionMode::AcceptEdits,
            PermissionMode::BypassPermissions,
        ] {
            let lattice = PermissionLattice::new(mode)
                .with_rules(vec![allow_rule.clone()])
                .with_grants(vec![ToolPattern::parse("web_fetch").expect("grant")])
                .with_denials(vec![never.clone()]);
            // Even a read-classified call: a fetch domain, an MCP tool.
            assert_eq!(
                lattice.evaluate("web_fetch", "domain:evil.example", ToolClass::ReadOnly),
                Decision::Deny(DecisionReason::PersistedDeny),
                "{mode:?}"
            );
            assert_eq!(
                lattice.evaluate("web_fetch", "domain:fine.example", ToolClass::ReadOnly),
                Decision::Allow(DecisionReason::AllowRule)
            );
            // It survives into a subagent's lattice.
            assert_eq!(
                lattice.for_subagent().evaluate(
                    "web_fetch",
                    "domain:evil.example",
                    ToolClass::ReadOnly
                ),
                Decision::Deny(DecisionReason::PersistedDeny)
            );
        }
        // A managed ban still ranks first.
        let banned = PermissionLattice::new(PermissionMode::Default)
            .with_denials(vec![never.clone()])
            .with_denied_tools([ToolPattern::parse("web_fetch").expect("ban")]);
        assert_eq!(
            banned.evaluate("web_fetch", "domain:evil.example", ToolClass::ReadOnly),
            Decision::Deny(DecisionReason::AdminToolDenied)
        );
        // Unreadable answers refuse everything rather than drop one.
        let unreadable =
            PermissionLattice::new(PermissionMode::BypassPermissions).with_unreadable_denials();
        assert_eq!(
            unreadable.evaluate("repo_read", "a.rs", ToolClass::ReadOnly),
            Decision::Deny(DecisionReason::PersistedAnswersUnreadable)
        );
    }

    #[test]
    fn never_and_always_answers_persist_replace_each_other_and_revoke() {
        let root = "/p";
        let pattern = ToolPattern::parse("mcp__srv__drop").expect("pattern");
        let mut grants = PermissionGrants::default();
        assert!(grants.allow(root, pattern.clone()).expect("allow"));
        // A store with no refusal is written exactly as before they existed.
        let before = render_grants(&grants).expect("render");
        assert!(!before.contains("deny"), "{before}");
        assert!(before.contains("\"schema\": 1"), "{before}");
        // The newer answer stands.
        assert!(grants.deny(root, pattern.clone()).expect("deny"));
        assert!(grants.for_root(root).is_empty());
        assert_eq!(grants.denials_for(root), vec![pattern.clone()]);
        assert!(!grants.deny(root, pattern.clone()).expect("again"));
        let text = render_grants(&grants).expect("render");
        // A store with a refusal is schema 2: a build that predates
        // refusals refuses it rather than rewrite it without them.
        assert!(text.contains("\"schema\": 2"), "{text}");
        // A schema-1 store carrying refusals keeps them.
        let legacy = parse_grants(&text.replace("\"schema\": 2", "\"schema\": 1")).expect("legacy");
        assert_eq!(legacy.denials_for(root), vec![pattern.clone()]);
        let read = parse_grants(&text).expect("parse");
        assert_eq!(read, grants);
        assert_eq!(read.denials_for(root), vec![pattern.clone()]);
        assert!(grants.allow(root, pattern.clone()).expect("allow again"));
        assert!(grants.denials_for(root).is_empty());
        assert!(grants.deny(root, pattern.clone()).expect("deny again"));
        assert!(grants.revoke(root, &pattern));
        assert_eq!(grants, PermissionGrants::default());
    }

    #[test]
    fn a_grant_can_never_widen_past_an_admin_ceiling() {
        // `rapid permissions` and its help text both claim a grant "can only
        // narrow the gap between ask and allow" and cannot beat a
        // managed-policy tool ban or write-scope ceiling. That claim is only
        // as good as `evaluate`'s ordering, so it is asserted here rather
        // than trusted.
        let granted = ToolPattern::parse("workspace_write").expect("pattern");

        // Without a ceiling the grant does what it says.
        let allowed =
            PermissionLattice::new(PermissionMode::Default).with_grants(vec![granted.clone()]);
        assert_eq!(
            allowed.evaluate("workspace_write", "a.rs", ToolClass::FileEdit),
            Decision::Allow(DecisionReason::PersistedGrant)
        );

        // A managed-policy tool ban beats it.
        let banned = PermissionLattice::new(PermissionMode::Default)
            .with_grants(vec![granted.clone()])
            .with_denied_tools([ToolPattern::parse("workspace_write").expect("pattern")]);
        assert_eq!(
            banned.evaluate("workspace_write", "a.rs", ToolClass::FileEdit),
            Decision::Deny(DecisionReason::AdminToolDenied)
        );

        // So does a managed write-scope ceiling, for a path outside it.
        let confined = PermissionLattice::new(PermissionMode::Default)
            .with_grants(vec![granted])
            .with_admin_write_scope("src");
        assert_eq!(
            confined.evaluate("workspace_write", "elsewhere/a.rs", ToolClass::FileEdit),
            Decision::Deny(DecisionReason::AdminWriteScopeViolation)
        );
    }

    #[test]
    fn a_standing_grant_names_exactly_its_call_and_only_where_it_would_answer_it() {
        let lattice = PermissionLattice::new(PermissionMode::Default);
        let grant = |lattice: &PermissionLattice, tool: &str, subject: Option<&str>| {
            lattice
                .standing_grant_for(tool, subject, ToolClass::FileEdit)
                .map(|grant| grant.render())
        };
        // This tool, this subject: the grant answers the same call and no
        // other path (another case of the same path aside: path patterns
        // compare case-insensitively).
        let exact = lattice
            .standing_grant_for("workspace_write", Some("first.txt"), ToolClass::FileEdit)
            .expect("a plain write is rememberable");
        assert_eq!(exact.render(), "workspace_write(first.txt)");
        let remembered = PermissionLattice::new(PermissionMode::Default).with_grants(vec![exact]);
        assert_eq!(
            remembered.evaluate("workspace_write", "first.txt", ToolClass::FileEdit),
            Decision::Allow(DecisionReason::PersistedGrant)
        );
        for other in ["first.txt.bak", "second.txt", "dir/first.txt"] {
            assert_eq!(
                remembered.evaluate("workspace_write", other, ToolClass::FileEdit),
                Decision::Ask(DecisionReason::ModeAsk),
                "{other}"
            );
        }
        // A tool whose calls carry no subject: the bare tool is exact.
        assert_eq!(
            grant(&lattice, "mcp__server__lookup", None).as_deref(),
            Some("mcp__server__lookup")
        );
        // A subject a pattern cannot state exactly, or none where the tool
        // has one: no grant, never a wider one.
        for subject in ["", "a*.txt", "a?.txt"] {
            assert_eq!(
                grant(&lattice, "workspace_write", Some(subject)),
                None,
                "{subject:?}"
            );
        }
        // A joined argv names no one command.
        assert_eq!(
            lattice.standing_grant_for("shell_exec", Some("git status"), ToolClass::Other),
            None
        );
        // Every layer ranked above grants still decides, so remembering
        // would not stop the next ask — or would widen past it.
        let ruled = PermissionLattice::new(PermissionMode::Default).with_rules(vec![ToolRule {
            effect: RuleEffect::Ask,
            pattern: ToolPattern::parse("workspace_write(first*)").expect("rule"),
        }]);
        let banned = PermissionLattice::new(PermissionMode::Default)
            .with_denied_tools([ToolPattern::parse("workspace_write").expect("pattern")]);
        let confined =
            PermissionLattice::new(PermissionMode::Default).with_admin_write_scope("src");
        let planning = PermissionLattice::new(PermissionMode::Plan);
        for (name, above) in [
            ("ask rule", &ruled),
            ("managed ban", &banned),
            ("managed write confinement", &confined),
            ("plan mode", &planning),
        ] {
            assert_eq!(
                grant(above, "workspace_write", Some("first.txt")),
                None,
                "{name}"
            );
        }
        // A project at its grant bound: the store refuses one more.
        let full = PermissionLattice::new(PermissionMode::Default).with_grants(
            (0..MAX_GRANTS)
                .map(|n| ToolPattern::parse(&format!("workspace_write(f{n})")).expect("pattern"))
                .collect(),
        );
        assert_eq!(grant(&full, "workspace_write", Some("first.txt")), None);
    }

    #[test]
    fn a_rendered_grants_document_parses_back_to_the_same_grants() {
        // The writer and the reader must agree exactly: a document this
        // build writes but its own loader rejects would make a grant vanish
        // silently, which is the whole failure mode `Decision::
        // PersistedGrant` exists to avoid.
        let mut grants = PermissionGrants::default();
        for raw in ["workspace_write", "shell_exec(git *)", "repo_read"] {
            let pattern = ToolPattern::parse(raw).expect("pattern");
            assert!(grants.allow("/proj/a", pattern).expect("allow"));
        }
        assert!(
            grants
                .allow("/proj/b", ToolPattern::parse("workspace_patch").expect("p"))
                .expect("allow")
        );
        let text = render_grants(&grants).expect("render");
        let parsed = parse_grants(&text).expect("the writer's own output must parse");
        assert_eq!(parsed, grants);
        assert_eq!(
            parsed
                .for_root("/proj/a")
                .iter()
                .map(ToolPattern::render)
                .collect::<Vec<_>>(),
            vec![
                "repo_read".to_owned(),
                "shell_exec(git *)".to_owned(),
                "workspace_write".to_owned()
            ]
        );
        assert_eq!(parsed.for_root("/proj/unknown"), Vec::new());
    }

    #[test]
    fn granting_is_idempotent_and_revoking_drops_an_emptied_root() {
        let mut grants = PermissionGrants::default();
        let pattern = ToolPattern::parse("workspace_write").expect("pattern");
        assert!(grants.allow("/proj", pattern.clone()).expect("first"));
        assert!(
            !grants.allow("/proj", pattern.clone()).expect("second"),
            "granting an existing pattern must report no change, like `rapid trust grant`"
        );
        assert!(grants.revoke("/proj", &pattern));
        assert!(
            !grants.revoke("/proj", &pattern),
            "revoking twice changes nothing"
        );
        // A root left with no grants is dropped rather than persisted as an
        // empty record — visible in the rendered document, which is the only
        // thing that actually reaches disk.
        let rendered = render_grants(&grants).expect("render");
        assert!(
            !rendered.contains("/proj"),
            "an emptied root must not be persisted: {rendered}"
        );
    }

    #[test]
    fn the_writer_enforces_the_same_bounds_the_reader_does() {
        let mut grants = PermissionGrants::default();
        for index in 0..MAX_GRANTS {
            let pattern = ToolPattern::parse(&format!("tool_{index}")).expect("pattern");
            assert!(grants.allow("/proj", pattern).expect("under the cap"));
        }
        let over = ToolPattern::parse("one_too_many").expect("pattern");
        assert_eq!(
            grants.allow("/proj", over),
            Err(GrantsError::InvalidGrant),
            "the reader silently truncates past MAX_GRANTS, so the writer must refuse"
        );
    }

    #[test]
    fn a_document_that_would_be_one_byte_over_once_written_is_refused() {
        // Every writer appends a trailing newline, so the bound has to cover
        // it. A document rendering to exactly `MAX_SETTINGS_BYTES` used to
        // pass this check and land on disk one byte over the limit its own
        // loader enforces — bricking the whole store, every other project's
        // grants included, with no diagnostic.
        //
        // The boundary is found byte-exactly: bulk-fill to within a pattern's
        // length of the limit, then grow one final pattern's name one
        // character at a time, since each character adds exactly one byte to
        // the rendered JSON. The largest accepted document is therefore
        // *exactly* at whatever threshold this function enforces, which is
        // what makes the off-by-one detectable at all.
        // Small enough that the fill stops with less free space than one
        // maximum-length pattern would consume, so the one-character growth
        // below actually reaches the limit; large enough that appending to
        // an *existing* root (no per-root JSON overhead) still fits.
        let headroom = 64usize;
        let mut grants = PermissionGrants::default();
        // The root the variable-length pattern is appended to, created up
        // front so growing it costs only the pattern's own bytes.
        grants
            .allow("/final", ToolPattern::parse("seed").expect("pattern"))
            .expect("seed root");
        let mut added = 0usize;
        loop {
            let mut candidate = grants.clone();
            let pattern = ToolPattern::parse(&format!("bulk{added:06}")).expect("pattern");
            // One pattern at a time, rotating roots as each fills, so the
            // fill stops within a few bytes of the target rather than
            // overshooting by a whole root.
            candidate
                .allow(&format!("/pad/{:06}", added / MAX_GRANTS), pattern)
                .expect("capacity");
            match render_grants(&candidate) {
                Ok(text) if text.len() < MAX_SETTINGS_BYTES - headroom => {
                    grants = candidate;
                    added += 1;
                }
                _ => break,
            }
            assert!(added < 200_000, "the document never approached the limit");
        }

        // One character at a time: `accepted` ends up holding the largest
        // document this function will accept.
        let mut accepted = grants.clone();
        let mut length = 1usize;
        while length <= MAX_PATTERN_BYTES {
            let mut candidate = grants.clone();
            let pattern =
                ToolPattern::parse(&format!("p{}", "x".repeat(length - 1))).expect("pattern");
            candidate.allow("/final", pattern).expect("existing root");
            match render_grants(&candidate) {
                Ok(_) => {
                    accepted = candidate;
                    length += 1;
                }
                Err(GrantsError::TooLarge) => break,
                Err(other) => panic!("unexpected render failure: {other:?}"),
            }
        }
        assert!(
            length > 1 && length <= MAX_PATTERN_BYTES,
            "the fill did not bracket the limit (stopped at {length})"
        );

        // The invariant: anything this function accepts must still parse
        // once the trailing newline every writer appends is on it.
        let rendered = render_grants(&accepted).expect("accepted");
        let mut written = rendered.into_bytes();
        written.push(b'\n');
        assert!(
            written.len() <= MAX_SETTINGS_BYTES,
            "an accepted document is {} bytes once written, over the {MAX_SETTINGS_BYTES}-byte \
limit its own loader enforces",
            written.len()
        );
        parse_grants(&String::from_utf8(written).expect("utf8"))
            .expect("what the writer accepts must parse back");
    }

    #[test]
    fn the_writer_refuses_to_exceed_the_project_record_bound() {
        let mut grants = PermissionGrants::default();
        let pattern = || ToolPattern::parse("workspace_write").expect("pattern");
        for index in 0..MAX_GRANT_RECORDS {
            assert!(
                grants
                    .allow(&format!("/proj/{index}"), pattern())
                    .expect("under")
            );
        }
        assert_eq!(
            grants.allow("/one/too/many", pattern()),
            Err(GrantsError::TooManyRecords),
            "the reader rejects a document with too many project records, so the writer \
must never produce one"
        );
        // An existing root is still writable at the bound.
        assert!(
            grants
                .allow("/proj/0", ToolPattern::parse("repo_read").expect("pattern"))
                .expect("existing root at the record bound")
        );
    }

    #[test]
    fn a_pattern_renders_back_to_exactly_what_parse_accepts() {
        for raw in [
            "workspace_write",
            "shell_exec(git *)",
            "repo_read(src/**)",
            "tool-with-dash",
        ] {
            let parsed = ToolPattern::parse(raw).expect("parse");
            assert_eq!(parsed.render(), raw);
            assert_eq!(ToolPattern::parse(&parsed.render()), Some(parsed));
        }
    }

    #[test]
    fn mode_names_round_trip_exactly() {
        for name in MODE_NAMES {
            let mode = PermissionMode::parse(name).expect("mode");
            assert_eq!(mode.as_str(), name);
        }
        assert_eq!(PermissionMode::parse("Default"), None, "case-sensitive");
        assert_eq!(PermissionMode::parse("yolo"), None);
        assert_eq!(MODE_NAMES.len(), 6);
        // The exact Claude/Grok-compatible spelling of the accept-edits mode.
        assert_eq!(
            PermissionMode::parse("acceptEdits"),
            Some(PermissionMode::AcceptEdits)
        );
    }

    #[test]
    fn read_only_calls_auto_allow_in_default_mode() {
        let lattice = PermissionLattice::new(PermissionMode::Default);
        for tool in ["repo_read", "repo_search", "workspace_read"] {
            assert_eq!(
                lattice.evaluate(tool, "src/lib.rs", ToolClass::ReadOnly),
                Decision::Allow(DecisionReason::ReadOnlyAutoAllow),
                "{tool} must auto-allow"
            );
        }
        // A write in default mode asks (a headless denial at the driver).
        assert_eq!(
            lattice.evaluate("workspace_patch", "src/lib.rs", ToolClass::FileEdit),
            Decision::Ask(DecisionReason::ModeAsk)
        );
        assert_eq!(
            lattice.evaluate("shell_exec", "git status", ToolClass::Other),
            Decision::Ask(DecisionReason::ModeAsk)
        );
    }

    #[test]
    fn plan_mode_allows_only_the_plan_file_and_only_by_a_file_edit() {
        let lattice = PermissionLattice::new(PermissionMode::Plan);
        for plan in [
            ".rapidlm/plan.md",
            ".rapidlm/plans/p1.md",
            ".rapidlm/plans/nested/x.md",
        ] {
            assert_eq!(
                lattice.evaluate("workspace_write", plan, ToolClass::FileEdit),
                Decision::Allow(DecisionReason::PlanFileCarveOut),
                "{plan}"
            );
        }
        for not_a_plan in [
            ".rapidlm/plans/../settings.json",
            ".rapidlm/plans/../../src/escape.md",
            ".rapidlm/plans/p1.sh",
            ".rapidlm/settings.json",
            "src/plan.md",
            ".rapidlm/plans",
        ] {
            assert_eq!(
                lattice.evaluate("workspace_write", not_a_plan, ToolClass::FileEdit),
                Decision::Deny(DecisionReason::PlanModeDeny),
                "{not_a_plan}"
            );
        }
        // Only a file edit: a command naming the plan path is still refused.
        assert_eq!(
            lattice.evaluate("shell_exec", ".rapidlm/plan.md", ToolClass::Other),
            Decision::Deny(DecisionReason::PlanModeDeny)
        );
        // A deny rule still wins.
        let guarded = PermissionLattice::new(PermissionMode::Plan).with_rules(vec![ToolRule {
            effect: RuleEffect::Deny,
            pattern: ToolPattern::parse("workspace_write(.rapidlm/plans/*)").expect("pattern"),
        }]);
        assert_eq!(
            guarded.evaluate(
                "workspace_write",
                ".rapidlm/plans/p1.md",
                ToolClass::FileEdit
            ),
            Decision::Deny(DecisionReason::DenyRule)
        );
        // An ask rule still asks.
        let asking = PermissionLattice::new(PermissionMode::Plan).with_rules(vec![ToolRule {
            effect: RuleEffect::Ask,
            pattern: ToolPattern::parse("workspace_write(.rapidlm/*)").expect("pattern"),
        }]);
        assert_eq!(
            asking.evaluate(
                "workspace_write",
                ".rapidlm/plans/p1.md",
                ToolClass::FileEdit
            ),
            Decision::Ask(DecisionReason::AskRule)
        );
    }

    #[test]
    fn plan_mode_denies_writes_even_when_an_allow_rule_matches() {
        // Plan's own doc comment (`permissiveness_rank`): denies every
        // write-classified call outright, a ceiling a lower-trust project
        // settings file's ordinary `allow` rule must not be able to widen
        // past -- the same property an admin `max_permission_mode` ceiling
        // relies on when it forces the lattice into Plan mode.
        let allow_everything = ToolRule {
            effect: RuleEffect::Allow,
            pattern: ToolPattern::parse("workspace_write(*)").expect("pattern"),
        };
        let lattice =
            PermissionLattice::new(PermissionMode::Plan).with_rules(vec![allow_everything]);
        assert_eq!(
            lattice.evaluate("workspace_write", "src/lib.rs", ToolClass::FileEdit),
            Decision::Deny(DecisionReason::PlanModeDeny),
            "an allow rule must never bypass Plan mode's absolute write floor"
        );
        // Reads still work in Plan mode -- the model needs to read files to
        // produce a plan -- confirming this doesn't over-deny.
        assert_eq!(
            lattice.evaluate("workspace_read", "src/lib.rs", ToolClass::ReadOnly),
            Decision::Allow(DecisionReason::ReadOnlyAutoAllow)
        );
    }

    #[test]
    fn path_subject_deny_rules_are_case_insensitive() {
        // The two most common desktop filesystems this tool runs against
        // (macOS's default APFS, Windows' default NTFS) are both
        // case-insensitive, so `Secrets/x` and `secrets/x` are the same
        // file on disk -- a deny rule (or an admin `denied_tools` ceiling)
        // written for one case must still catch the other.
        let deny_secrets = ToolRule {
            effect: RuleEffect::Deny,
            pattern: ToolPattern::parse("workspace_write(secrets/*)").expect("pattern"),
        };
        let lattice = PermissionLattice::new(PermissionMode::BypassPermissions)
            .with_rules(vec![deny_secrets]);
        for path in [
            "secrets/config.json",
            "Secrets/config.json",
            "SECRETS/config.json",
        ] {
            assert_eq!(
                lattice.evaluate("workspace_write", path, ToolClass::FileEdit),
                Decision::Deny(DecisionReason::DenyRule),
                "{path} must be denied regardless of case"
            );
        }
        // shell_exec's argv subject deliberately stays case-sensitive (Unix
        // program-name lookup really is case-sensitive) -- confirming the
        // fix is scoped to path-shaped tools, not a blanket change.
        let deny_rm = ToolRule {
            effect: RuleEffect::Deny,
            pattern: ToolPattern::parse("shell_exec(rm *)").expect("pattern"),
        };
        let shell_lattice =
            PermissionLattice::new(PermissionMode::BypassPermissions).with_rules(vec![deny_rm]);
        assert_eq!(
            shell_lattice.evaluate("shell_exec", "RM -rf /tmp/x", ToolClass::Other),
            Decision::Allow(DecisionReason::BypassAllow),
            "shell argv case-sensitivity must be unchanged"
        );
    }

    #[test]
    fn write_calls_ask_or_deny_per_mode() {
        let subject = "src/lib.rs";
        let cases = [
            (
                PermissionMode::Default,
                Decision::Ask(DecisionReason::ModeAsk),
            ),
            (
                PermissionMode::Plan,
                Decision::Deny(DecisionReason::PlanModeDeny),
            ),
            (
                PermissionMode::AcceptEdits,
                Decision::Allow(DecisionReason::EditModeAllow),
            ),
            (
                PermissionMode::Auto,
                Decision::Allow(DecisionReason::EditModeAllow),
            ),
            (
                PermissionMode::DontAsk,
                Decision::Deny(DecisionReason::DontAskDeny),
            ),
            (
                PermissionMode::BypassPermissions,
                Decision::Allow(DecisionReason::BypassAllow),
            ),
        ];
        for (mode, expected) in cases {
            let lattice = PermissionLattice::new(mode);
            assert_eq!(
                lattice.evaluate("workspace_patch", subject, ToolClass::FileEdit),
                expected,
                "{mode} file-edit decision"
            );
        }
        // Non-edit writes still ask under acceptEdits/auto.
        for mode in [PermissionMode::AcceptEdits, PermissionMode::Auto] {
            let lattice = PermissionLattice::new(mode);
            assert_eq!(
                lattice.evaluate("shell_exec", "rm -rf build", ToolClass::Other),
                Decision::Ask(DecisionReason::ModeAsk),
                "{mode} shell decision"
            );
        }
        // Plan mode allows reads.
        let plan = PermissionLattice::new(PermissionMode::Plan);
        assert!(
            plan.evaluate("repo_read", "src/lib.rs", ToolClass::ReadOnly)
                .is_allowed()
        );
    }

    #[test]
    fn subagent_lattice_caps_bypass_but_leaves_every_other_mode_and_rules_alone() {
        let rule = ToolRule {
            effect: RuleEffect::Allow,
            pattern: ToolPattern::parse("shell_exec(*)").expect("pattern"),
        };
        let grant = ToolPattern::parse("workspace_patch(*)").expect("pattern");

        let bypass = PermissionLattice::new(PermissionMode::BypassPermissions)
            .with_rules(vec![rule.clone()])
            .with_grants(vec![grant.clone()]);
        let child = bypass.for_subagent();
        assert_eq!(child.mode(), PermissionMode::AcceptEdits);
        // A capable child still auto-allows file edits (it just lost the
        // "allow literally everything, never ask" ceiling), and the parent's
        // rules/grants carried over rather than starting from nothing.
        assert!(
            child
                .evaluate("workspace_write", "src/lib.rs", ToolClass::FileEdit)
                .is_allowed()
        );
        assert_eq!(
            child.evaluate("shell_exec", "cargo test", ToolClass::Other),
            Decision::Allow(DecisionReason::AllowRule),
            "carried-over allow rule still applies"
        );
        assert!(
            child
                .evaluate("workspace_patch", "src/lib.rs", ToolClass::FileEdit)
                .is_allowed(),
            "carried-over persisted grant still applies"
        );

        // Every other mode passes through unchanged: they were already safe
        // for a subagent (non-file-edit calls hit `Ask`, which a headless-style
        // subagent turn treats as a denial — see `evaluate`'s doc comment).
        for mode in [
            PermissionMode::Default,
            PermissionMode::Plan,
            PermissionMode::AcceptEdits,
            PermissionMode::Auto,
            PermissionMode::DontAsk,
        ] {
            let lattice = PermissionLattice::new(mode);
            assert_eq!(lattice.for_subagent().mode(), mode, "{mode} passes through");
        }
    }

    #[test]
    fn write_scope_confines_file_edits_but_never_shell_exec() {
        // BypassPermissions would allow everything unconditionally — the
        // scope ceiling must still win over even the most permissive mode.
        let lattice = PermissionLattice::new(PermissionMode::BypassPermissions)
            .with_write_scope("src/feature");

        assert_eq!(
            lattice.evaluate("workspace_write", "src/feature/mod.rs", ToolClass::FileEdit),
            Decision::Allow(DecisionReason::BypassAllow),
            "inside the scope: unaffected"
        );
        assert_eq!(
            lattice.evaluate("workspace_write", "src/feature", ToolClass::FileEdit),
            Decision::Allow(DecisionReason::BypassAllow),
            "the scope root itself counts as inside"
        );
        assert_eq!(
            lattice.evaluate("workspace_write", "src/other.rs", ToolClass::FileEdit),
            Decision::Deny(DecisionReason::WriteScopeViolation),
            "outside the scope: denied even under bypassPermissions"
        );
        assert_eq!(
            lattice.evaluate(
                "workspace_write",
                "src/feature-other/x.rs",
                ToolClass::FileEdit
            ),
            Decision::Deny(DecisionReason::WriteScopeViolation),
            "segment-aware: a sibling directory sharing the prefix string must not match"
        );
        // shell_exec's subject is joined argv, not a path — the scope must
        // never apply to it, or a legitimate command would be misdenied for
        // merely containing the scope string as a substring coincidence.
        assert_eq!(
            lattice.evaluate("shell_exec", "rm -rf src/other.rs", ToolClass::Other),
            Decision::Allow(DecisionReason::BypassAllow),
            "shell_exec is never subject to the write scope"
        );
    }

    #[test]
    fn write_scope_survives_for_subagent_narrowing() {
        let parent = PermissionLattice::new(PermissionMode::Default).with_write_scope("src");
        let child = parent.for_subagent();
        assert_eq!(child.write_scope(), Some("src"));
        assert_eq!(
            child.evaluate("workspace_write", "docs/readme.md", ToolClass::FileEdit),
            Decision::Deny(DecisionReason::WriteScopeViolation)
        );
    }

    #[test]
    fn admin_denied_tools_win_over_bypass_permissions_and_allow_rules() {
        // BypassPermissions plus an explicit allow rule would allow this
        // call by every other mechanism the lattice has — the admin ban
        // must still win over both.
        let lattice = PermissionLattice::new(PermissionMode::BypassPermissions)
            .with_rules(vec![ToolRule {
                effect: RuleEffect::Allow,
                pattern: ToolPattern::parse("shell_exec").expect("pattern"),
            }])
            .with_denied_tools([ToolPattern::parse("shell_exec").expect("pattern")]);

        assert_eq!(
            lattice.evaluate("shell_exec", "rm -rf /", ToolClass::Other),
            Decision::Deny(DecisionReason::AdminToolDenied),
            "an admin tool ban wins over bypassPermissions and an explicit allow rule alike"
        );
        // An unrelated tool is unaffected.
        assert_eq!(
            lattice.evaluate("workspace_read", "src/lib.rs", ToolClass::ReadOnly),
            Decision::Allow(DecisionReason::ReadOnlyAutoAllow),
        );
    }

    #[test]
    fn admin_denied_tools_survive_for_subagent_narrowing() {
        let parent = PermissionLattice::new(PermissionMode::Default)
            .with_denied_tools([ToolPattern::parse("task_spawn").expect("pattern")]);
        let child = parent.for_subagent();
        assert_eq!(child.denied_tools().len(), 1);
        assert_eq!(
            child.evaluate("task_spawn", "explore", ToolClass::Other),
            Decision::Deny(DecisionReason::AdminToolDenied)
        );
    }

    #[test]
    fn admin_denied_tools_respect_their_own_arg_glob() {
        // A pattern with an arg glob only bans matching arguments, not the
        // tool outright — same semantics as an ordinary deny rule's glob.
        let lattice = PermissionLattice::new(PermissionMode::BypassPermissions)
            .with_denied_tools([ToolPattern::parse("shell_exec(rm *)").expect("pattern")]);

        assert_eq!(
            lattice.evaluate("shell_exec", "rm -rf /tmp/x", ToolClass::Other),
            Decision::Deny(DecisionReason::AdminToolDenied)
        );
        assert_eq!(
            lattice.evaluate("shell_exec", "ls -la", ToolClass::Other),
            Decision::Allow(DecisionReason::BypassAllow),
            "a non-matching argv for the same tool is unaffected"
        );
    }

    #[test]
    fn admin_write_scope_wins_over_bypass_permissions_and_write_scope_never_overwrites_it() {
        let lattice =
            PermissionLattice::new(PermissionMode::BypassPermissions).with_admin_write_scope("src");

        assert_eq!(
            lattice.evaluate("workspace_write", "docs/readme.md", ToolClass::FileEdit),
            Decision::Deny(DecisionReason::AdminWriteScopeViolation),
            "outside the admin ceiling: denied even under bypassPermissions"
        );
        assert_eq!(
            lattice.evaluate("workspace_write", "src/lib.rs", ToolClass::FileEdit),
            Decision::Allow(DecisionReason::BypassAllow),
            "inside the admin ceiling: unaffected"
        );

        // The exact scenario this field exists to prevent: a `task_spawn`
        // call setting its own (unrelated, wider) write_scope must never be
        // able to widen past the admin ceiling — the two fields are
        // independent, so `with_write_scope` cannot overwrite
        // `admin_write_scope` the way it would if they shared one field.
        let subagent_widened = lattice.with_write_scope("docs");
        assert_eq!(
            subagent_widened.evaluate("workspace_write", "docs/readme.md", ToolClass::FileEdit),
            Decision::Deny(DecisionReason::AdminWriteScopeViolation),
            "a subagent's own write_scope must not escape the admin ceiling"
        );
        assert_eq!(
            subagent_widened.evaluate("workspace_write", "src/lib.rs", ToolClass::FileEdit),
            Decision::Deny(DecisionReason::WriteScopeViolation),
            "still confined by its own narrower write_scope too — both must hold"
        );
    }

    #[test]
    fn admin_write_scope_survives_for_subagent_narrowing() {
        let parent = PermissionLattice::new(PermissionMode::Default).with_admin_write_scope("src");
        let child = parent.for_subagent();
        assert_eq!(child.admin_write_scope(), Some("src"));
        assert_eq!(
            child.evaluate("workspace_write", "docs/readme.md", ToolClass::FileEdit),
            Decision::Deny(DecisionReason::AdminWriteScopeViolation)
        );
    }

    #[test]
    fn dont_ask_silently_denies_and_bypass_still_honors_deny_rules() {
        // dontAsk denies everything not pre-approved (rules/grants/read-only).
        let lattice = PermissionLattice::new(PermissionMode::DontAsk);
        assert_eq!(
            lattice.evaluate("shell_exec", "cargo test", ToolClass::Other),
            Decision::Deny(DecisionReason::DontAskDeny)
        );
        // ...but a pre-approved grant still allows.
        let lattice = lattice.with_grants(vec![
            ToolPattern::parse("shell_exec(cargo *)").expect("grant"),
        ]);
        assert_eq!(
            lattice.evaluate("shell_exec", "cargo test", ToolClass::Other),
            Decision::Allow(DecisionReason::PersistedGrant)
        );

        // bypassPermissions allows, except where a deny rule fires.
        let lattice =
            PermissionLattice::new(PermissionMode::BypassPermissions).with_rules(vec![ToolRule {
                effect: RuleEffect::Deny,
                pattern: ToolPattern::parse("shell_exec(rm *)").expect("rule"),
            }]);
        assert!(
            lattice
                .evaluate("workspace_patch", "any.rs", ToolClass::FileEdit)
                .is_allowed()
        );
        assert_eq!(
            lattice.evaluate("shell_exec", "rm -rf /", ToolClass::Other),
            Decision::Deny(DecisionReason::DenyRule)
        );
    }

    #[test]
    fn deny_rule_beats_allow_rule_and_ask_rule() {
        let lattice = PermissionLattice::new(PermissionMode::BypassPermissions).with_rules(vec![
            ToolRule {
                effect: RuleEffect::Allow,
                pattern: ToolPattern::parse("shell_exec(git *)").expect("allow"),
            },
            ToolRule {
                effect: RuleEffect::Ask,
                pattern: ToolPattern::parse("shell_exec(git push*)").expect("ask"),
            },
            ToolRule {
                effect: RuleEffect::Deny,
                pattern: ToolPattern::parse("shell_exec(git push --force*)").expect("deny"),
            },
        ]);
        assert_eq!(
            lattice.evaluate(
                "shell_exec",
                "git push --force origin main",
                ToolClass::Other
            ),
            Decision::Deny(DecisionReason::DenyRule),
            "deny must win over allow and ask"
        );
        assert_eq!(
            lattice.evaluate("shell_exec", "git push origin main", ToolClass::Other),
            Decision::Ask(DecisionReason::AskRule),
            "ask must win over allow"
        );
        assert_eq!(
            lattice.evaluate("shell_exec", "git status", ToolClass::Other),
            Decision::Allow(DecisionReason::AllowRule)
        );
        // A deny rule beats read-only auto-allow too.
        let lattice = lattice.with_rules(vec![ToolRule {
            effect: RuleEffect::Deny,
            pattern: ToolPattern::parse("repo_read(.env*)").expect("deny"),
        }]);
        assert_eq!(
            lattice.evaluate("repo_read", ".env.local", ToolClass::ReadOnly),
            Decision::Deny(DecisionReason::DenyRule)
        );
    }

    #[test]
    fn persisted_grant_suppresses_the_mode_ask() {
        let lattice = PermissionLattice::new(PermissionMode::Default).with_grants(vec![
            ToolPattern::parse("shell_exec(git *)").expect("grant"),
            ToolPattern::parse("workspace_patch").expect("grant"),
        ]);
        assert_eq!(
            lattice.evaluate("shell_exec", "git diff", ToolClass::Other),
            Decision::Allow(DecisionReason::PersistedGrant)
        );
        assert_eq!(
            lattice.evaluate("workspace_patch", "src/a.rs", ToolClass::FileEdit),
            Decision::Allow(DecisionReason::PersistedGrant)
        );
        // Un-granted calls still ask.
        assert_eq!(
            lattice.evaluate("shell_exec", "make all", ToolClass::Other),
            Decision::Ask(DecisionReason::ModeAsk)
        );
    }

    #[test]
    fn every_decision_carries_a_typed_reason() {
        let lattice = PermissionLattice::new(PermissionMode::Default).with_rules(vec![ToolRule {
            effect: RuleEffect::Deny,
            pattern: ToolPattern::parse("shell_exec(sudo *)").expect("rule"),
        }]);
        let decisions = [
            lattice.evaluate("shell_exec", "sudo rm x", ToolClass::Other),
            lattice.evaluate("shell_exec", "ls", ToolClass::Other),
            lattice.evaluate("repo_read", "a.rs", ToolClass::ReadOnly),
            lattice.evaluate("workspace_patch", "a.rs", ToolClass::FileEdit),
        ];
        for decision in decisions {
            let reason = decision.reason();
            assert!(!reason.as_str().is_empty());
            assert!(!reason.explanation().is_empty());
        }
    }

    #[test]
    fn glob_matcher_handles_star_question_and_literals() {
        assert!(glob_match("*", "anything at all"));
        assert!(glob_match("git *", "git push origin"));
        assert!(glob_match("git ?tatus", "git status"));
        assert!(!glob_match("git status", "git status --short"));
        assert!(!glob_match("cargo *", "cargo"));
        assert!(glob_match("cargo*", "cargo"));
        assert!(glob_match("a*b*c", "a-x-b-y-c"));
        assert!(!glob_match("a*b*c", "a-x-b-y-d"));
        assert!(glob_match("*.rs", "src/main.rs"));
        assert!(!glob_match("*.rs", "src/main.rs.bak"));
        // Multi-byte subjects match on chars, never split a code point.
        assert!(glob_match("é*", "ééé"));
    }

    #[test]
    fn tool_pattern_parse_rejects_malformed_rules() {
        assert!(ToolPattern::parse("").is_none());
        assert!(ToolPattern::parse("(").is_none());
        assert!(ToolPattern::parse("shell_exec(").is_none());
        assert!(ToolPattern::parse("shell_exec()").is_none());
        assert!(ToolPattern::parse("(git *)").is_none());
        assert!(ToolPattern::parse("shell exec(git)").is_none());
        let pattern = ToolPattern::parse("shell_exec(git *)").expect("ok");
        assert_eq!(pattern.tool(), "shell_exec");
        assert_eq!(pattern.arg_glob(), Some("git *"));
        let bare = ToolPattern::parse("workspace_patch").expect("ok");
        assert_eq!(bare.arg_glob(), None);
    }

    #[test]
    fn settings_parse_reads_both_document_shapes() {
        let rapidlm = parse_settings(
            r#"{"mode": "acceptEdits", "permissions": {"deny": ["shell_exec(rm *)"]}}"#,
        )
        .expect("rapidlm shape");
        assert_eq!(rapidlm.mode, Some(PermissionMode::AcceptEdits));
        assert_eq!(rapidlm.rules.len(), 1);
        assert_eq!(rapidlm.rules[0].effect, RuleEffect::Deny);

        let claude = parse_settings(
            r#"{"permissions": {"defaultMode": "plan", "allow": ["Read(*)", "repo_read"],
               "ask": ["Bash(git push*)"]}}"#,
        )
        .expect("claude shape");
        assert_eq!(claude.mode, Some(PermissionMode::Plan));
        assert_eq!(claude.rules.len(), 3);
        assert_eq!(claude.rules[0].effect, RuleEffect::Ask, "ask before allow");

        // Unknown tools parse as patterns (deny intent survives), unknown keys
        // are ignored, and an empty document parses to defaults.
        let compat =
            parse_settings(r#"{"permissions": {"deny": ["WebFetch(domain:x)"]}, "extra": 1}"#)
                .expect("compat");
        assert_eq!(compat.rules.len(), 1);
        let empty = parse_settings("{}").expect("empty");
        assert_eq!(empty, ProjectSettings::default());
    }

    #[test]
    fn settings_parse_fails_typed_on_corrupt_or_out_of_bounds_input() {
        assert_eq!(parse_settings("nope"), Err(SettingsError::InvalidJson));
        assert_eq!(parse_settings("[1]"), Err(SettingsError::InvalidJson));
        assert_eq!(
            parse_settings(r#"{"mode": "yolo"}"#),
            Err(SettingsError::InvalidRule)
        );
        assert_eq!(
            parse_settings(r#"{"permissions": {"allow": ["bad rule!!"]}}"#),
            Err(SettingsError::InvalidRule)
        );
        let many: Vec<String> = (0..=MAX_RULES)
            .map(|index| format!("tool{index}"))
            .collect();
        let document = format!(r#"{{"permissions": {{"allow": {many:?}}}}}"#);
        assert_eq!(parse_settings(&document), Err(SettingsError::TooManyRules));
        let oversized = format!("\"{}\"", "x".repeat(MAX_SETTINGS_BYTES + 1));
        assert_eq!(parse_settings(&oversized), Err(SettingsError::TooLarge));
    }

    #[test]
    fn grants_parse_is_scoped_per_root_and_fails_closed() {
        let document = r#"{"schema": 1, "projects": [
            {"root": "/work/a", "allow": ["shell_exec(cargo *)"]},
            {"root": "/work/b", "allow": []}
        ]}"#;
        let grants = parse_grants(document).expect("grants");
        assert_eq!(grants.for_root("/work/a").len(), 1);
        assert!(grants.for_root("/work/b").is_empty());
        assert!(grants.for_root("/work/missing").is_empty());

        assert_eq!(
            parse_grants(r#"{"schema": 3, "projects": []}"#),
            Err(GrantsError::InvalidJson),
            "unsupported schema version must fail closed"
        );
        assert_eq!(
            parse_grants("{"),
            Err(GrantsError::InvalidJson),
            "corrupt file yields no grants"
        );
        assert_eq!(
            parse_grants(r#"{"schema": 1, "projects": [{"root": "/a", "allow": ["!!"]}]}"#),
            Err(GrantsError::InvalidGrant)
        );
    }

    #[test]
    fn rule_and_grant_bounds_are_enforced_at_load() {
        let lattice = PermissionLattice::new(PermissionMode::Default);
        let rules: Vec<ToolRule> = (0..MAX_RULES + 10)
            .map(|index| ToolRule {
                effect: RuleEffect::Allow,
                pattern: ToolPattern::parse(&format!("tool{index}")).expect("rule"),
            })
            .collect();
        let lattice = lattice.with_rules(rules);
        assert_eq!(lattice.rules().len(), MAX_RULES);
    }
}
