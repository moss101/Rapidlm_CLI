//! The durable approval flow, end to end across its layers:
//!
//! 1. An `Ask` decision with a live sink suspends the turn (`ApprovalRequired`)
//!    and records a pending approval — action, scope, diff — in the ledger.
//! 2. The suspension carries every completed exchange, so a resume never
//!    repeats a committed side effect; the pending call executes exactly once.
//! 3. A restarted process (a fresh client over the same ledger) still sees
//!    the pending approval and can load the suspended state and resolve it.
//! 4. Denial / managed-policy-deny / no-sink-denial all hold, and a pending
//!    clarification rides the same machinery.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agent_runtime::{
    ModelDriver, ModelStepError, ModelStepInput, ModelStepOutput, ProposedToolCall, ToolDriver,
    ToolKind, ToolStepError, TurnSpec, TurnStatus, TurnStopReason, TurnSuspension, run_turn,
    run_turn_seeded,
};
use event_ledger::event::{ActorKind, ActorRef};
use kernel::{
    ApprovalDecision, CreateSession, InProcessKernelClient, KernelClient, RecordApproval,
    ResolveApproval,
};
use protocol::{AgentId, ProjectId, SessionId, TraceId, TurnId};
use rapid::approvals::ApprovalSink as _;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Kernel calls here are synchronous in-process SQLite; poll once.
fn call<T, E>(future: impl Future<Output = Result<T, E>>) -> Result<T, E>
where
    E: std::fmt::Display,
{
    let mut future = Box::pin(future);
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    match future.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(result) => result,
        std::task::Poll::Pending => panic!("kernel call went async under the test"),
    }
}

use std::future::Future;

fn actor() -> ActorRef {
    // Actor ids are EventIds; mint a fresh one per call like the CLI does.
    ActorRef::new(ActorKind::Human, &protocol::EventId::new().to_string()).expect("actor")
}

struct Project {
    root: PathBuf,
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn project(name: &str) -> Project {
    let root = std::env::temp_dir().join(format!(
        "rapid-approvals-{name}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&root).expect("mkdir");
    Project { root }
}

fn ledger_path(root: &Path) -> PathBuf {
    let path = root.join(".rapidlm").join("sessions.db");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("ledger dir");
    path
}

fn open_session(root: &Path) -> (InProcessKernelClient, SessionId) {
    let client = InProcessKernelClient::open(ledger_path(root)).expect("ledger opens");
    let snapshot = call(client.create_session(CreateSession::new(
        ProjectId::new(),
        actor(),
        TraceId::new(),
    )))
    .expect("session");
    (client, snapshot.id())
}

/// One scripted model: emits the queued outputs and records the result kinds
/// it was shown, so a test can assert the model saw the resumed history.
struct ScriptedModel {
    outputs: Mutex<VecDeque<Result<ModelStepOutput, ModelStepError>>>,
    seen_exchanges: Mutex<Vec<Vec<String>>>,
}

impl ScriptedModel {
    fn new(outputs: Vec<Result<ModelStepOutput, ModelStepError>>) -> Self {
        Self {
            outputs: Mutex::new(outputs.into()),
            seen_exchanges: Mutex::new(Vec::new()),
        }
    }

    fn propose(
        call_id: &str,
        tool: &str,
        arguments: &str,
    ) -> Result<ModelStepOutput, ModelStepError> {
        Ok(ModelStepOutput::ToolCalls {
            calls: vec![ProposedToolCall::new(call_id, tool, arguments).expect("valid proposal")],
            tokens: 1,
            cost_usd_micros: None,
        })
    }

    fn terminal(text: &str) -> Result<ModelStepOutput, ModelStepError> {
        Ok(ModelStepOutput::Terminal {
            text: text.to_owned(),
            tokens: 1,
            cost_usd_micros: None,
        })
    }

    fn seen(&self) -> Vec<Vec<String>> {
        self.seen_exchanges
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

fn result_kind(result: &agent_runtime::ToolStepResult) -> String {
    match result {
        agent_runtime::ToolStepResult::Succeeded { summary, .. } => format!("succeeded: {summary}"),
        agent_runtime::ToolStepResult::Failed { .. } => "failed".to_owned(),
        agent_runtime::ToolStepResult::Denied { detail, .. } => {
            format!("denied: {}", detail.as_deref().unwrap_or(""))
        }
        agent_runtime::ToolStepResult::ApprovalRequired { .. } => "approval_required".to_owned(),
        agent_runtime::ToolStepResult::ContextRequired { question, .. } => {
            format!("question: {question}")
        }
    }
}

impl ModelDriver for ScriptedModel {
    fn step(
        &mut self,
        input: &ModelStepInput<'_>,
        _cancel: &agent_runtime::CancellationToken,
    ) -> Result<ModelStepOutput, ModelStepError> {
        self.seen_exchanges
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(
                input
                    .history()
                    .iter()
                    .map(|exchange| {
                        exchange
                            .results()
                            .iter()
                            .map(result_kind)
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>(),
            );
        self.outputs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .unwrap_or(Err(ModelStepError::Failed))
    }
}

/// A workspace driver with an explicit lattice and (optionally) the ledger
/// approval sink — the same assembly the interactive turn builds, at test
/// scale.
struct TestTools {
    inner: rapid::exec_tools::WorkspaceTools,
}

impl TestTools {
    fn open(root: &Path, lattice: rapid::permissions::PermissionLattice) -> Self {
        Self {
            inner: rapid::exec_tools::WorkspaceTools::open_with_permissions(root, lattice)
                .expect("workspace tools"),
        }
    }

    fn with_sink(mut self, client: InProcessKernelClient, session: SessionId, root: &Path) -> Self {
        self.inner
            .set_approval_source(Arc::new(rapid::approvals::LedgerApprovalSink::new(
                client,
                session,
                actor(),
                root.to_path_buf(),
            )));
        self
    }
}

impl ToolDriver for TestTools {
    fn validate(
        &mut self,
        call: &ProposedToolCall,
        cancel: &agent_runtime::CancellationToken,
    ) -> Result<agent_runtime::ValidatedToolCall, ToolStepError> {
        self.inner.validate(call, cancel)
    }

    fn execute(
        &mut self,
        call: &agent_runtime::ValidatedToolCall,
        cancel: &agent_runtime::CancellationToken,
    ) -> Result<agent_runtime::ToolStepResult, ToolStepError> {
        self.inner.execute(call, cancel)
    }

    fn tool_kind(&self, tool: &str) -> ToolKind {
        self.inner.tool_kind(tool)
    }
}

fn default_lattice() -> rapid::permissions::PermissionLattice {
    // `Default` asks for every write-classified call — the out-of-box mode.
    rapid::permissions::PermissionLattice::new(rapid::permissions::PermissionMode::Default)
}

fn budget() -> agent_runtime::TurnBudget {
    agent_runtime::TurnBudget::new(4, Some(8), None).expect("budget")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The core flow: an Ask with a sink suspends the turn, records the pending
/// approval with a diff, and the file is NOT written; a fresh client (a
/// restarted process) sees the pending approval, approves it, and the
/// continuation executes the write exactly once and completes the turn.
#[test]
fn ask_with_a_sink_suspends_records_and_resumes_the_exact_call() {
    let project = project("resume");
    let (client, session) = open_session(&project.root);

    // Turn one: the model proposes a write; the lattice asks; the sink records.
    let mut model = ScriptedModel::new(vec![
        ScriptedModel::propose(
            "c1",
            "workspace_write",
            r#"{"path":"notes/hello.txt","content":"hello from the agent"}"#,
        ),
        ScriptedModel::terminal("unreachable — the turn must stop first"),
    ]);
    let mut tools = TestTools::open(&project.root, default_lattice()).with_sink(
        client.clone(),
        session,
        &project.root,
    );
    let cancel = agent_runtime::CancellationToken::new();
    let result = run_turn(
        TurnSpec::new(
            TurnId::new(),
            session,
            AgentId::new(),
            budget(),
            &mut model,
            &mut tools,
            &mut Vec::new(),
        ),
        &cancel,
    )
    .expect("turn runs");
    assert_eq!(result.reason(), Some(TurnStopReason::ApprovalRequired));
    let suspension = result.suspension().expect("suspends with state");
    assert_eq!(suspension.call_id(), "c1");
    // The placeholder result is inside the recorded history.
    assert!(
        suspension
            .history()
            .iter()
            .any(|exchange| exchange.results().iter().any(|result| matches!(
                result,
                agent_runtime::ToolStepResult::ApprovalRequired { call_id } if call_id == "c1"
            )))
    );

    // The turn did not write the file.
    assert!(!project.root.join("notes/hello.txt").exists());

    // The pending approval is durable and shows the action and scope.
    let pendings = call(client.pending_approvals(session)).expect("pendings");
    assert_eq!(pendings.len(), 1);
    let payload = pendings[0].payload();
    assert_eq!(payload.tool, "workspace_write");
    assert_eq!(payload.scope, vec!["notes/hello.txt".to_owned()]);
    assert!(payload.summary.contains("create notes/hello.txt"));
    assert!(payload.diff.contains("hello from the agent"));

    // A restarted process sees the same pending approval and can load the
    // suspension once the host recorded it.
    let reopened = InProcessKernelClient::open(ledger_path(&project.root)).expect("reopens");
    let pendings = call(reopened.pending_approvals(session)).expect("pendings");
    assert_eq!(pendings.len(), 1);
    let token = pendings[0].payload().id.clone();

    // Approve once — durable resolution, and a duplicate fails closed.
    call(
        reopened.approve(
            ResolveApproval::new(
                session,
                pendings[0].seq(),
                ApprovalDecision::Approved,
                actor(),
                TraceId::new(),
            )
            .with_wait_token(&token),
        ),
    )
    .expect("resolves");
    assert!(
        call(reopened.pending_approvals(session))
            .expect("pendings")
            .is_empty()
    );
    assert!(
        call(
            reopened.approve(
                ResolveApproval::new(
                    session,
                    pendings[0].seq(),
                    ApprovalDecision::Approved,
                    actor(),
                    TraceId::new()
                )
                .with_wait_token(&token),
            )
        )
        .is_err()
    );

    // The pending call executes exactly once, through the preapproved path.
    let proposed = ProposedToolCall::replay(
        "c1",
        "workspace_write",
        r#"{"path":"notes/hello.txt","content":"hello from the agent"}"#,
    );
    let mut fresh_tools = TestTools::open(&project.root, default_lattice());
    let validated =
        agent_runtime::ToolDriver::validate(&mut fresh_tools, &proposed, &cancel).expect("valid");
    let executed = fresh_tools
        .inner
        .execute_preapproved(&validated, &cancel)
        .expect("executes");
    assert!(matches!(
        executed,
        agent_runtime::ToolStepResult::Succeeded { .. }
    ));
    assert!(project.root.join("notes/hello.txt").exists());

    // The continuation turn starts from the recorded history with the real
    // result in the placeholder's slot, and completes on it.
    let mut seed = rapid::approvals::SuspendedHistory::from_agent(suspension.history());
    {
        let (exchange, _) = seed.find_result_mut("c1").expect("placeholder present");
        for result in exchange.results.iter_mut() {
            if result.call_id == "c1" {
                result.kind = "succeeded".to_owned();
                result.summary = "wrote notes/hello.txt".to_owned();
            }
        }
    }
    let history = seed.to_agent();
    let mut continuation_model = ScriptedModel::new(vec![ScriptedModel::terminal("done")]);
    let continuation = run_turn_seeded(
        TurnSpec::new(
            TurnId::new(),
            session,
            AgentId::new(),
            budget(),
            &mut continuation_model,
            &mut TestTools::open(&project.root, default_lattice()),
            &mut Vec::new(),
        ),
        history,
        &cancel,
    )
    .expect("continuation completes");
    assert_eq!(continuation.status(), TurnStatus::Completed);
    let seen = continuation_model.seen();
    assert!(seen[0].iter().any(|kind| kind.starts_with("succeeded:")));
    assert!(!seen[0].iter().any(|kind| kind == "approval_required"));
}

/// A hook's `ask` rides the same wait (ADR 0022 §3): with the lattice
/// permissive, a v2 hook that asks suspends the turn, records the pending
/// approval naming the hook as its source, and writes nothing; a restarted
/// process sees the approval, approves it, and the preapproved resume runs
/// the call exactly once even though the hook asks again.
#[test]
fn a_hook_ask_with_a_sink_suspends_records_its_source_and_a_restart_resumes_the_call_once() {
    let project = project("hook-ask");
    let (client, session) = open_session(&project.root);
    let hook = project.root.join("ask.sh");
    std::fs::write(
        &hook,
        "echo '{\"schema\":\"rapidlm.hook_result\",\"version\":2,\"decision\":\"ask\",\"reason\":\"notes need a reviewer\"}'\nexit 0\n",
    )
    .expect("hook script");
    let hooks = rapid::hooks::HooksConfig {
        pre_tool_use: vec![format!("sh {}", test_fixtures::slash_path(&hook))],
        ..Default::default()
    };
    let permissive = || {
        rapid::permissions::PermissionLattice::new(
            rapid::permissions::PermissionMode::BypassPermissions,
        )
    };

    let mut model = ScriptedModel::new(vec![
        ScriptedModel::propose(
            "c1",
            "workspace_write",
            r#"{"path":"notes/plan.txt","content":"the plan"}"#,
        ),
        ScriptedModel::terminal("unreachable — the turn must stop first"),
    ]);
    let mut tools = TestTools::open(&project.root, permissive()).with_sink(
        client.clone(),
        session,
        &project.root,
    );
    tools.inner.set_hooks(hooks.clone());
    let cancel = agent_runtime::CancellationToken::new();
    let result = run_turn(
        TurnSpec::new(
            TurnId::new(),
            session,
            AgentId::new(),
            budget(),
            &mut model,
            &mut tools,
            &mut Vec::new(),
        ),
        &cancel,
    )
    .expect("turn runs");
    assert_eq!(result.reason(), Some(TurnStopReason::ApprovalRequired));
    assert_eq!(result.suspension().expect("suspends").call_id(), "c1");
    assert!(!project.root.join("notes/plan.txt").exists());

    // The pending approval names the hook as its source and in the summary,
    // after the call's own action; scope and diff are the call's.
    let pendings = call(client.pending_approvals(session)).expect("pendings");
    assert_eq!(pendings.len(), 1);
    let payload = pendings[0].payload();
    assert_eq!(payload.tool, "workspace_write");
    assert!(
        payload
            .source
            .as_deref()
            .is_some_and(|s| s.starts_with("hook:pre_tool_use[0]#"))
    );
    // The call comes first — a human approving must see what runs — then
    // the hook and its reason.
    assert_eq!(
        payload.summary,
        "create notes/plan.txt — pre_tool_use[0] hook asks: notes need a reviewer"
    );
    assert_eq!(payload.scope, vec!["notes/plan.txt".to_owned()]);
    assert!(payload.diff.contains("the plan"));

    // A restarted process (a fresh client over the same ledger) sees it and
    // decides; the source survived the round trip through the ledger.
    let reopened = InProcessKernelClient::open(ledger_path(&project.root)).expect("reopens");
    let pendings = call(reopened.pending_approvals(session)).expect("pendings");
    assert_eq!(pendings.len(), 1);
    assert!(
        pendings[0]
            .payload()
            .source
            .as_deref()
            .is_some_and(|s| s.starts_with("hook:pre_tool_use[0]#"))
    );
    let token = pendings[0].payload().id.clone();
    call(
        reopened.approve(
            ResolveApproval::new(
                session,
                pendings[0].seq(),
                ApprovalDecision::Approved,
                actor(),
                TraceId::new(),
            )
            .with_wait_token(&token),
        ),
    )
    .expect("resolves");

    // The preapproved resume runs the call once, with the same asking hook
    // still configured: the answered ask is not raised again, and the sink
    // (installed again, as the resuming surface would) receives nothing.
    let proposed = ProposedToolCall::replay(
        "c1",
        "workspace_write",
        r#"{"path":"notes/plan.txt","content":"the plan"}"#,
    );
    let mut fresh_tools = TestTools::open(&project.root, permissive()).with_sink(
        reopened.clone(),
        session,
        &project.root,
    );
    fresh_tools.inner.set_hooks(hooks);
    let validated =
        agent_runtime::ToolDriver::validate(&mut fresh_tools, &proposed, &cancel).expect("valid");
    // The resuming surface passes the approval's recorded source, so the
    // hook's (identical) ask is the one the human answered.
    let approved = rapid::approvals::recorded_request(&reopened, session, &token)
        .map(|payload| rapid::approvals::ApprovedAsk::from_payload(&payload))
        .expect("the resolved approval's record is still readable");
    assert!(
        approved
            .source
            .as_deref()
            .is_some_and(|s| s.starts_with("hook:pre_tool_use[0]#")),
        "{approved:?}"
    );
    assert!(approved.covers(proposed.arguments()));
    let executed = fresh_tools
        .inner
        .execute_preapproved_from(&validated, &cancel, Some(&approved))
        .expect("executes");
    assert!(matches!(
        executed,
        agent_runtime::ToolStepResult::Succeeded { .. }
    ));
    assert_eq!(
        std::fs::read_to_string(project.root.join("notes/plan.txt")).expect("written"),
        "the plan"
    );
    assert!(
        call(reopened.pending_approvals(session))
            .expect("pendings")
            .is_empty(),
        "the answered ask must not be asked again"
    );
}

/// Asks recorded concurrently through *independent* sinks — the interactive
/// turn's sink and the one a continuation's suspension record creates, or two
/// surfaces on one session — all land: each sink serialises its own requests,
/// and across sinks the retry re-reads the session tip on a sequence conflict
/// rather than telling the loser there is no approval surface. (Two sinks, so
/// the conflicts are real: one shared sink would serialise everything and the
/// retry would never run.)
#[test]
fn concurrent_approval_requests_both_land_despite_sequence_conflicts() {
    let project = project("concurrent-asks");
    let (client, session) = open_session(&project.root);
    let sinks: Vec<Arc<rapid::approvals::LedgerApprovalSink>> = (0..2)
        .map(|_| {
            Arc::new(rapid::approvals::LedgerApprovalSink::new(
                client.clone(),
                session,
                actor(),
                project.root.clone(),
            ))
        })
        .collect();
    let rounds = 12;
    let mut handles = Vec::new();
    for round in 0..rounds {
        for (side, sink) in sinks.iter().enumerate() {
            let sink = Arc::clone(sink);
            handles.push(std::thread::spawn(move || {
                sink.request(&rapid::approvals::ApprovalRequest {
                    tool: "repo_read".to_owned(),
                    call_id: format!("c{round}-{side}"),
                    summary: format!("read file {round}-{side}"),
                    scope: Vec::new(),
                    diff: String::new(),
                    source: Some("hook:pre_tool_use[0]#0123456789ab".to_owned()),
                    arguments_digest: None,
                })
            }));
        }
    }
    let outcomes: Vec<Result<String, String>> = handles
        .into_iter()
        .map(|h| h.join().expect("thread"))
        .collect();
    let failures: Vec<&String> = outcomes.iter().filter_map(|o| o.as_ref().err()).collect();
    assert!(
        failures.is_empty(),
        "every request must be recorded: {failures:?}"
    );
    let pendings = call(client.pending_approvals(session)).expect("pendings");
    assert_eq!(pendings.len(), rounds * 2);
    assert!(
        pendings
            .iter()
            .all(|p| p.payload().source.as_deref() == Some("hook:pre_tool_use[0]#0123456789ab"))
    );
}

/// A denial feeds the paused turn a typed denial, so the model sees the
/// refusal rather than the turn simply vanishing; nothing is written.
#[test]
fn a_denied_approval_continues_with_a_typed_denial() {
    let project = project("deny");
    let (client, session) = open_session(&project.root);

    let call_id = ProposedToolCall::new(
        "d1",
        "workspace_write",
        r#"{"path":"blocked.txt","content":"no"}"#,
    )
    .expect("valid");
    // The pending approval is recorded the way the sink records it.
    call(
        client.record_approval(
            RecordApproval::new(
                session,
                1,
                actor(),
                TraceId::new(),
                "approval-test-deny",
                call_id.call_id(),
                "workspace_write",
                "create blocked.txt",
            )
            .with_scope(vec!["blocked.txt".to_owned()]),
        ),
    )
    .expect("recorded");

    let denial = agent_runtime::ToolStepResult::Denied {
        call_id: call_id.call_id().to_owned(),
        detail: Some("denied by the operator".to_owned()),
    };
    let history = vec![agent_runtime::ToolStepExchange::new(
        vec![call_id],
        vec![denial],
    )];

    let cancel = agent_runtime::CancellationToken::new();
    let mut model = ScriptedModel::new(vec![ScriptedModel::terminal("acknowledged")]);
    let result = run_turn_seeded(
        TurnSpec::new(
            TurnId::new(),
            session,
            AgentId::new(),
            budget(),
            &mut model,
            &mut TestTools::open(&project.root, default_lattice()),
            &mut Vec::new(),
        ),
        history,
        &cancel,
    )
    .expect("continuation completes");
    assert_eq!(result.status(), TurnStatus::Completed);
    assert!(!project.root.join("blocked.txt").exists());
    let seen = model.seen();
    assert!(
        seen[0]
            .iter()
            .any(|kind| kind.contains("denied by the operator"))
    );
}

/// Without a sink — headless exec — an Ask stays a typed denial, fail-closed,
/// and the model continues past it.
#[test]
fn ask_without_a_sink_is_the_fail_closed_denial() {
    let project = project("nosink");

    let mut model = ScriptedModel::new(vec![
        ScriptedModel::propose(
            "c1",
            "workspace_write",
            r#"{"path":"no.txt","content":"no"}"#,
        ),
        ScriptedModel::terminal("worked around the denial"),
    ]);
    let mut tools = TestTools::open(&project.root, default_lattice());
    let cancel = agent_runtime::CancellationToken::new();
    let result = run_turn(
        TurnSpec::new(
            TurnId::new(),
            SessionId::new(),
            AgentId::new(),
            budget(),
            &mut model,
            &mut tools,
            &mut Vec::new(),
        ),
        &cancel,
    )
    .expect("turn runs");
    assert!(!project.root.join("no.txt").exists());
    assert_eq!(result.status(), TurnStatus::Completed);
}

/// A managed-policy tool ban is never widened by an approval: even with a
/// live sink, a banned tool is denied outright, never asked about.
#[test]
fn managed_policy_deny_beats_the_approval_surface() {
    let project = project("managed");
    let (client, session) = open_session(&project.root);

    let lattice = default_lattice()
        .with_denied_tools([rapid::permissions::ToolPattern::parse("workspace_write").unwrap()]);
    let mut tools =
        TestTools::open(&project.root, lattice).with_sink(client.clone(), session, &project.root);
    let pending_call = ProposedToolCall::new(
        "m1",
        "workspace_write",
        r#"{"path":"banned.txt","content":"no"}"#,
    )
    .expect("valid");
    let cancel = agent_runtime::CancellationToken::new();
    let validated =
        agent_runtime::ToolDriver::validate(&mut tools, &pending_call, &cancel).expect("valid");
    let outcome =
        agent_runtime::ToolDriver::execute(&mut tools, &validated, &cancel).expect("executes");
    assert!(matches!(
        outcome,
        agent_runtime::ToolStepResult::Denied { .. }
    ));
    assert!(
        call(client.pending_approvals(session))
            .expect("pendings")
            .is_empty()
    );
}

/// The suspension the loop records is exactly what the resume needs, and it
/// round-trips through its wire form (what the ledger payload carries).
#[test]
fn suspension_round_trips_through_its_wire_form() {
    let calls = vec![
        ProposedToolCall::replay("a", "repo_read", r#"{"path":"x"}"#),
        ProposedToolCall::replay("b", "workspace_read", r#"{"path":"y"}"#),
    ];
    let outcomes = vec![
        Ok(agent_runtime::ToolStepResult::Succeeded {
            call_id: "a".into(),
            summary: "read x".into(),
        }),
        Ok(agent_runtime::ToolStepResult::ApprovalRequired {
            call_id: "b".into(),
        }),
    ];
    let suspension = TurnSuspension::new(
        TurnStopReason::ApprovalRequired,
        "b".into(),
        &[agent_runtime::ToolStepExchange::new(
            vec![ProposedToolCall::replay(
                "z",
                "repo_read",
                r#"{"path":"z"}"#,
            )],
            vec![agent_runtime::ToolStepResult::Succeeded {
                call_id: "z".into(),
                summary: "read z".into(),
            }],
        )],
        &calls,
        &outcomes,
    );
    assert_eq!(suspension.reason(), TurnStopReason::ApprovalRequired);
    assert_eq!(suspension.call_id(), "b");
    assert_eq!(suspension.omitted_earlier_steps(), 0);

    let mut wire = rapid::approvals::SuspendedHistory::from_agent(suspension.history());
    let json = serde_json::to_string(&wire).expect("serializes");
    let back: rapid::approvals::SuspendedHistory = serde_json::from_str(&json).expect("parses");
    let exchanges = back.to_agent();
    assert_eq!(exchanges.len(), 2);
    assert_eq!(exchanges[0].calls()[0].call_id(), "z");
    assert_eq!(exchanges[1].calls()[0].call_id(), "a");
    assert_eq!(exchanges[1].results()[1].call_id(), "b");
    assert!(wire.find_result_mut("b").is_some());
    assert!(wire.find_result_mut("missing").is_none());
}

/// An `ask_user` clarification rides the same durable machinery: the question
/// is pending, the answer resolves it, and the continuation carries the
/// answer to the model.
#[test]
fn a_clarification_is_pending_until_answered_then_continues() {
    let project = project("clarify");
    let (client, session) = open_session(&project.root);

    let mut model = ScriptedModel::new(vec![
        ScriptedModel::propose(
            "q1",
            "ask_user",
            r#"{"question":"Which flavor?","options":["vanilla","cherry"]}"#,
        ),
        ScriptedModel::terminal("unreachable until answered"),
    ]);
    let mut tools = TestTools::open(&project.root, default_lattice()).with_sink(
        client.clone(),
        session,
        &project.root,
    );
    let cancel = agent_runtime::CancellationToken::new();
    let result = run_turn(
        TurnSpec::new(
            TurnId::new(),
            session,
            AgentId::new(),
            budget(),
            &mut model,
            &mut tools,
            &mut Vec::new(),
        ),
        &cancel,
    )
    .expect("turn runs");
    assert_eq!(result.reason(), Some(TurnStopReason::ContextRequired));
    let suspension = result.suspension().expect("suspends with state");
    assert_eq!(suspension.call_id(), "q1");

    // The host-level record: pending approval + suspension, then the answer.
    let sink = rapid::approvals::LedgerApprovalSink::new(
        client.clone(),
        session,
        actor(),
        project.root.clone(),
    );
    let token = sink
        .request(&rapid::approvals::ApprovalRequest {
            tool: "ask_user".to_owned(),
            call_id: "q1".to_owned(),
            summary: "Which flavor?".to_owned(),
            scope: Vec::new(),
            diff: String::new(),
            source: None,
            arguments_digest: None,
        })
        .expect("records");
    let suspended = rapid::approvals::SuspendedTurn {
        task: "pick a flavor".to_owned(),
        call_id: "q1".to_owned(),
        reason: "context_required".to_owned(),
        omitted_earlier_steps: 0,
        history: rapid::approvals::SuspendedHistory::from_agent(suspension.history()),
    };
    rapid::approvals::record_suspension(
        &client,
        session,
        &actor(),
        &token,
        "q1",
        "ask_user",
        &suspended,
    )
    .expect("records");
    assert_eq!(
        call(client.pending_approvals(session))
            .expect("pendings")
            .len(),
        1
    );
    // The resolution appends at the session's CURRENT tip — exactly what the
    // session loop's resolver does.
    let tip = call(client.get_session(session)).expect("session").seq();
    call(
        client.approve(
            ResolveApproval::new(
                session,
                tip,
                ApprovalDecision::Approved,
                actor(),
                TraceId::new(),
            )
            .with_wait_token(&token),
        ),
    )
    .expect("resolves");

    // The continuation re-runs `ask_user` with a one-shot answer source; the
    // model-visible result carries the answer.
    let answered: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(Some("cherry".to_owned())));
    let mut fresh_tools = TestTools::open(&project.root, default_lattice());
    let slot = Arc::clone(&answered);
    fresh_tools
        .inner
        .set_ask_source(Arc::new(move |_prompt, _options, _timeout| {
            let mut guard = slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard
                .take()
                .ok_or_else(|| "the recorded answer was already used".to_owned())
        }));
    let proposed = ProposedToolCall::replay(
        "q1",
        "ask_user",
        r#"{"question":"Which flavor?","options":["vanilla","cherry"]}"#,
    );
    let validated =
        agent_runtime::ToolDriver::validate(&mut fresh_tools, &proposed, &cancel).expect("valid");
    let outcome = agent_runtime::ToolDriver::execute(&mut fresh_tools, &validated, &cancel)
        .expect("executes");
    match outcome {
        agent_runtime::ToolStepResult::Succeeded { summary, .. } => {
            assert!(summary.contains("cherry"), "{summary}");
        }
        other => panic!("expected the answer to satisfy the question, got {other:?}"),
    }
}
