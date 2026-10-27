//! Auto-review of approval requests: a written risk policy judged by one
//! model call, before a permission `Ask` is raised to the human.
//!
//! An `Ask` decision today becomes a durable pending approval and pauses
//! the turn for a human. With `[approvals] auto_review = true`, the ask is
//! first handed to a reviewer — one model call over the policy below —
//! which either approves (the call proceeds; the answered pair is
//! journaled so the audit trail shows who decided) or denies (the approval
//! is still raised, annotated with the reviewer's risk and reason — the
//! human is the override, never bypassed). Everything fails closed: a
//! reviewer that cannot be built, cannot answer, or answers something
//! this parser cannot read counts as a denial with the reason attached.
//!
//! A circuit breaker watches the reviewer's own judgments: three
//! consecutive denials (or ten of fifty overall) suspends auto-review for
//! the session and every ask goes back to the human — a reviewer that
//! keeps refusing is either watching a model behave badly or is itself
//! miscalibrated, and in both cases the human should be back in the loop.

use crate::approvals::ApprovalRequest;

/// The reviewer's written risk policy: what the one model call is told to
/// judge. Everything a reviewer approves still runs inside the permission
/// lattice's own ceilings — a reviewer can only ever answer an `Ask`, never
/// widen past a deny rule, a managed ban, or a write scope.
pub const REVIEW_INSTRUCTIONS: &str = "\
You are the auto-reviewer for a coding agent. A tool call the agent wants to \
make needs approval, and you are the first judge. The request below names \
the tool, the action, its scope and the change it would make.

Approve only what an experienced engineer would let a colleague run on \
their machine unattended. Deny, with a reason, anything that:

- exfiltrates data or code: sending secrets, credentials, tokens, private \
keys, environment values, or the repository's contents to any destination \
outside the project (an upload, a paste service, an unfamiliar domain, a \
variable that could hold a secret);
- reaches credentials or account material: reading or writing SSH keys, \
cloud credentials, token stores, password files, or anything under the \
agent's own configuration and governance records;
- destroys work or state: force-pushes, history rewrites, deletes outside \
the project tree, dropping or resetting databases, deleting files the \
project did not create in this session;
- weakens security: turning off protections, widening permissions, \
disabling checks, opening the machine to the network.

When in doubt, deny — a denial only asks the human, it costs nothing. \
Approve ordinary project work: builds, tests, linting, dependency \
installs from the project's own manifests, file edits inside the project, \
reading anything the project already contains that is not credential \
material.

Reply with one JSON object and nothing else: \
{\"decision\":\"approve\"|\"deny\",\"risk\":\"low\"|\"medium\"|\"high\",\
\"reason\":\"one sentence\"}.";

/// The user-role request the judged action is attached to.
pub const REVIEW_TASK: &str =
    "Judge the approval request that follows, as instructed. Reply with the JSON object only.";

/// Risk tier a reviewer assigns. Recorded with every verdict; the riskier
/// an approval, the more the audit trail says.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Risk {
    Low,
    Medium,
    High,
}

impl Risk {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// One reviewer verdict: the decision, its risk tier, and the one-sentence
/// reason the audit trail and the human both read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewVerdict {
    pub approved: bool,
    pub risk: Risk,
    pub reason: String,
}

/// Longest reviewer reason kept — the verdict is one sentence by
/// instruction; anything past this is truncated, not trusted.
pub const MAX_REVIEW_REASON_BYTES: usize = 512;

impl ReviewVerdict {
    /// Parse the reviewer's reply. Anything that is not exactly the
    /// instructed shape is an error, and the caller treats every error as
    /// a denial — fail closed: a reply this parser cannot read was never
    /// an approval.
    pub fn parse_text(text: &str) -> Result<Self, String> {
        let trimmed = text.trim();
        // The instructed shape is a bare JSON object; a fenced reply is
        // tolerated the way every model fence is — including the language
        // tag an opening fence often carries (`​``json`).
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
        let value: serde_json::Value =
            serde_json::from_str(body).map_err(|err| format!("verdict is not JSON: {err}"))?;
        let object = value
            .as_object()
            .ok_or_else(|| "verdict is not a JSON object".to_owned())?;
        let approved = match object.get("decision").and_then(serde_json::Value::as_str) {
            Some("approve") => true,
            Some("deny") => false,
            other => {
                return Err(format!(
                    "decision is not \"approve\" or \"deny\": {}",
                    other.unwrap_or("<missing>")
                ));
            }
        };
        let risk = object
            .get("risk")
            .and_then(serde_json::Value::as_str)
            .and_then(Risk::parse)
            .ok_or_else(|| "risk is not \"low\", \"medium\" or \"high\"".to_owned())?;
        let reason = object
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .chars()
            .take(MAX_REVIEW_REASON_BYTES)
            .collect::<String>();
        Ok(Self {
            approved,
            risk,
            reason,
        })
    }

    /// The fail-closed verdict for a reviewer that could not answer: a
    /// denial with the reason it could not.
    pub fn unreadable(reason: &str) -> Self {
        Self {
            approved: false,
            risk: Risk::High,
            reason: format!("reviewer did not answer: {reason}"),
        }
    }
}

/// Consecutive denials that suspend auto-review for the session.
pub const BREAKER_CONSECUTIVE_DENIALS: u32 = 3;
/// Denials within a window of verdicts that suspend it too — a reviewer
/// that refuses a fifth of everything is miscalibrated or watching a
/// badly behaved model.
pub const BREAKER_WINDOW_DENIALS: u32 = 10;
pub const BREAKER_WINDOW_TOTAL: u32 = 50;

/// The reviewer's circuit breaker: suspends auto-review once its own
/// judgments say the model keeps asking for things that were refused.
/// Per session, in memory — a restart re-arms it, like every other
/// session state.
#[derive(Default)]
pub struct ReviewBreaker {
    consecutive_denials: u32,
    denials: u32,
    verdicts: u32,
    tripped: bool,
}

impl ReviewBreaker {
    /// Whether the reviewer still judges. `false` once tripped, forever
    /// for this session.
    pub fn armed(&self) -> bool {
        !self.tripped
    }

    /// Record one verdict; trips the breaker on the documented thresholds.
    pub fn note(&mut self, verdict: &ReviewVerdict) {
        if self.tripped {
            return;
        }
        self.verdicts += 1;
        if verdict.approved {
            self.consecutive_denials = 0;
            return;
        }
        self.consecutive_denials += 1;
        self.denials += 1;
        if self.consecutive_denials >= BREAKER_CONSECUTIVE_DENIALS
            || (self.verdicts >= BREAKER_WINDOW_TOTAL
                && self.denials * BREAKER_WINDOW_TOTAL >= BREAKER_WINDOW_DENIALS * self.verdicts)
        {
            self.tripped = true;
        }
    }

    /// Why the breaker is open, for the notice that says so.
    pub fn trip_reason(&self) -> &'static str {
        if self.consecutive_denials >= BREAKER_CONSECUTIVE_DENIALS {
            "three consecutive denials"
        } else {
            "ten of fifty denials"
        }
    }
}

/// What the reviewer is shown: the request's own summary, scope and diff,
/// bounded — the reviewer sees what the human would.
pub fn review_request_text(request: &ApprovalRequest) -> String {
    let mut text = format!(
        "Tool: {}\nAction: {}\n",
        request.tool,
        request.summary.lines().next().unwrap_or("")
    );
    if !request.scope.is_empty() {
        text.push_str("Scope: ");
        text.push_str(&request.scope.join("; "));
        text.push('\n');
    }
    let diff: String = request.diff.chars().take(4096).collect();
    if !diff.trim().is_empty() {
        text.push_str("Change:\n");
        text.push_str(&diff);
    }
    text
}

/// One auto-review consultation. Installed on the tool surface when
/// `[approvals] auto_review` is on; the production implementation lives in
/// `interactive.rs` (it resolves a model and journals through the ledger),
/// tests install their own.
pub trait ApprovalReviewer: Send + Sync {
    /// Judge one request. `None`: not armed — unconfigured, or the circuit
    /// breaker tripped — and the approval is raised to the human
    /// unannotated. `Some(Ok(verdict))` with `approved: true`: the call
    /// proceeds (the implementation has journaled the answered pair).
    /// `Some(Ok(verdict))` with `approved: false`: the approval is raised
    /// annotated with the reviewer's risk and reason. `Some(Err(reason))`:
    /// the reviewer could not run — same as a denial, fail closed.
    fn review(&self, request: &ApprovalRequest) -> Option<Result<ReviewVerdict, String>>;
}

/// The one review model call: the policy as the system instructions, the
/// request as the task's memory, one step, the reply parsed. A reply that
/// is not a terminal text (a tool call, where none were offered) or does
/// not parse is an error — the caller turns every error into a denial.
pub fn run_review<B: crate::host::LiveModelCall>(
    backing: &mut B,
    request: &ApprovalRequest,
    context_limit: u32,
    output_reserve: u32,
    cancel: &agent_runtime::CancellationToken,
) -> Result<ReviewVerdict, String> {
    use agent_runtime::ModelStepInput;
    use context_engine::compile::{CompileContext, CompileInput, CompileLimits};
    let limits = CompileLimits::new()
        .retrieved_share_bps(0)
        .memory_share_bps(10_000)
        .read_set_share_bps(0);
    let ctx = CompileContext::new(context_limit, output_reserve)
        .limits(limits)
        .system(CompileInput::new(
            "system/approval-review",
            REVIEW_INSTRUCTIONS.to_owned(),
        ))
        .task(REVIEW_TASK)
        .memory(CompileInput::new(
            "approval/request",
            review_request_text(request),
        ));
    let packet = context_engine::compile::compile(&ctx)
        .map_err(|err| format!("the review request did not compile: {err}"))?;
    if cancel.is_cancelled() {
        return Err("cancelled".to_owned());
    }
    let stepped = backing.step(packet.blocks(), &ModelStepInput::without_tools(0), cancel);
    let _ = backing.take_continuations();
    match stepped {
        Ok(agent_runtime::ModelStepOutput::Terminal { text, .. }) => {
            ReviewVerdict::parse_text(&text)
        }
        Ok(agent_runtime::ModelStepOutput::ToolCalls { .. }) => {
            Err("the reviewer proposed tool calls, which it was not offered".to_owned())
        }
        Err(err) => Err(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> ApprovalRequest {
        ApprovalRequest {
            tool: "shell_exec".to_owned(),
            call_id: "c1".to_owned(),
            summary: "run cargo test".to_owned(),
            scope: vec!["shell".to_owned()],
            diff: String::new(),
            source: None,
            arguments_digest: None,
            remember_as: None,
        }
    }

    #[test]
    fn verdict_parses_the_instructed_shape_and_nothing_else() {
        let approve = ReviewVerdict::parse_text(
            r#"{"decision":"approve","risk":"low","reason":"ordinary build"}"#,
        )
        .expect("approve");
        assert!(approve.approved);
        assert_eq!(approve.risk, Risk::Low);
        assert_eq!(approve.reason, "ordinary build");

        // A fenced reply is tolerated.
        let fenced = ReviewVerdict::parse_text(
            "```json\n{\"decision\":\"deny\",\"risk\":\"high\",\"reason\":\"secret upload\"}\n```",
        )
        .expect("fenced deny");
        assert!(!fenced.approved);
        assert_eq!(fenced.risk, Risk::High);

        // Everything else is an error, and every error reads as a denial
        // at the caller — the fail-closed half of the contract.
        for bad in [
            "",
            "looks fine to me",
            "{\"decision\":\"maybe\"}",
            "{\"decision\":\"approve\"}",
            "{\"decision\":\"approve\",\"risk\":\"severe\",\"reason\":\"x\"}",
            "[\"approve\"]",
        ] {
            assert!(ReviewVerdict::parse_text(bad).is_err(), "{bad:?}");
        }

        let unreadable = ReviewVerdict::unreadable("the model is unconfigured");
        assert!(!unreadable.approved);
        assert_eq!(unreadable.risk, Risk::High);
        assert!(unreadable.reason.contains("unconfigured"));
    }

    #[test]
    fn the_breaker_trips_on_three_consecutive_denials_or_ten_of_fifty() {
        let mut breaker = ReviewBreaker::default();
        let deny = ReviewVerdict {
            approved: false,
            risk: Risk::High,
            reason: String::new(),
        };
        let approve = ReviewVerdict {
            approved: true,
            risk: Risk::Low,
            reason: String::new(),
        };
        // Two denials with an approval between them never trip.
        breaker.note(&deny);
        breaker.note(&approve);
        breaker.note(&deny);
        breaker.note(&approve);
        assert!(breaker.armed());

        // Three in a row do.
        breaker.note(&deny);
        breaker.note(&deny);
        assert!(breaker.armed(), "two are not three");
        breaker.note(&deny);
        assert!(!breaker.armed(), "trips at three consecutive denials");
        assert!(breaker.trip_reason().contains("three consecutive"));

        // And it stays tripped.
        breaker.note(&approve);
        assert!(!breaker.armed());

        // The window rule: ten denials among fifty verdicts, never three
        // consecutive.
        let mut windowed = ReviewBreaker::default();
        for _ in 0..49 {
            windowed.note(&approve);
            windowed.note(&deny);
            if !windowed.armed() {
                break;
            }
        }
        // 25 approvals and 25 denials trip well before 50 verdicts: a
        // reviewer refusing half of everything is miscalibrated.
        assert!(!windowed.armed());
    }

    /// A backing whose one step returns `text` — the reviewer's reply.
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
    fn run_review_parses_the_backings_reply() {
        let mut approve =
            Reply(r#"{"decision":"approve","risk":"low","reason":"project build"}"#.to_owned());
        let verdict = run_review(
            &mut approve,
            &request(),
            crate::user_config::DEFAULT_CONTEXT_WINDOW,
            crate::user_config::DEFAULT_MAX_OUTPUT_TOKENS,
            &agent_runtime::CancellationToken::new(),
        )
        .expect("verdict");
        assert!(verdict.approved);

        let mut nonsense = Reply("sure, go ahead".to_owned());
        let err = run_review(
            &mut nonsense,
            &request(),
            crate::user_config::DEFAULT_CONTEXT_WINDOW,
            crate::user_config::DEFAULT_MAX_OUTPUT_TOKENS,
            &agent_runtime::CancellationToken::new(),
        )
        .expect_err("unreadable");
        assert!(err.contains("not JSON"), "{err}");
    }
}
