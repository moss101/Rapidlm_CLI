//! Deterministic event-driven TUI snapshot and frame-latency harness.
//!
//! Fixture kernel/local event streams fold through [`tui::replay`] and paint
//! major panels into fixed 80x24 / 120x40 cell buffers. The same stream always
//! yields the same cells. Input-to-render samples record p50/p95. Transcript
//! virtualization is checked so a frame never relayouts the full history.

use std::time::{Duration, Instant};

use agent_runtime::PatchSummary;
use context_engine::{
    CompileContext, CompileInput, CompileReason, Freshness, MemoryScopeKind, MemorySourceKind,
    compile,
};
use event_ledger::event::{ActorKind, ActorRef, EventEnvelope, EventKind, RecordedAt};
use llm_router::{
    CatalogConfig, CatalogModelSpec, CatalogRevision, DataPolicyTag, LatencyClass,
    ModelCapabilities, ModelCatalog, ModelId, ModelPrices, ModelPurpose, ModelRef, PrivacyClass,
    ProviderCapEntry, ProviderCapIndex, ProviderCapabilities, ProviderId, ReasoningSupport, Region,
    RouteRequest, UsageFieldSet,
};
use protocol::{AgentId, ArtifactId, ArtifactRef, EventId, JobId, RedactionClass, SpanId, TraceId};
use serde_json::Value;
use tui::state::{ApprovalKey, ApprovalLifecycle, JobLifecycle, LocalUiEvent, UiEvent, UiRoute};
use tui::{
    AgentHandoffObservation, AgentMergeState, AgentsSelection, AgentsViewModel, ApprovalActionSpec,
    ApprovalClock, ApprovalPolicySpec, ApprovalPrompt, ApprovalRiskSpec, ApprovalScopeSpec,
    ApprovalViewModel, CancellationToken, CapabilityView, CommandPreviewInput, ComposerCommand,
    ComposerModel, ContextUsage, ContextViewModel, DiffSelection, DiffViewModel, JobClass,
    JobObservation, MemoryObservation, MemorySelection, MemoryViewModel, ModelViewModel,
    PolicyLayerView, PolicyMode, Rect, RenderBlockKind, RiskClassView, SandboxMode,
    SessionActionsViewModel, SessionLifecycleObservation, SessionLifecycleRequest, SpanObservation,
    SpanStatus, StatusChrome, TraceJobsViewModel, Transcript, TranscriptViewport, UiMode,
    compute_layout, reduce, render_status_with, replay,
};

const SIZE_80X24: (u16, u16) = (80, 24);
const SIZE_120X40: (u16, u16) = (120, 40);

const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000010";
const PROJECT_ID: &str = "019c0000-0000-7000-8000-000000000011";
const TURN_ID: &str = "019c0000-0000-7000-8000-000000000012";
const GOAL_ID: &str = "019c0000-0000-7000-8000-000000000014";
const AGENT_ID: &str = "019c0000-0000-7000-8000-000000000015";
const ACTOR_ID: &str = "019c0000-0000-7000-8000-000000000016";
const TRACE_ID: &str = "8f000000-0000-7000-8000-000000000017";
const CHILD_ID: &str = "019c0000-0000-7000-8000-000000000018";
const JOB_ID: &str = "019c0000-0000-7000-8000-000000000019";
const APPROVAL_ID: &str = "019c0000-0000-7000-8000-00000000001a";
const SIBLING_ID: &str = "019c0000-0000-7000-8000-00000000001b";
const VIEW_ID: &str = "019c0000-0000-7000-8000-000000000021";
const EVIDENCE_ID: &str = "019c0000-0000-7000-8000-000000000030";
const SPAN_A: &str = "018f3c8a-7e2b-7a11-8c4d-0123456789ab";
const SPAN_B: &str = "018f3c8a-7e2b-7a13-8c4d-0123456789ab";
const CREATED_AT: &str = "2026-08-14T15:20:04.123Z";
const UPDATED_AT: &str = "2026-08-14T15:21:00.000Z";
const FINGERPRINT: &str = "aabbccddeeff00112233445566778899";

const LATENCY_SAMPLES: usize = 64;
const MAX_P95_INPUT_TO_RENDER: Duration = Duration::from_millis(150);

const AGENTS_GOLDEN: &str = "\
agents control:human
  000000000015 persistent paused
  > 000000000018 managed running write merge:conflict:1 TAKEOVER
    00000000001b unspecified paused
selected:000000000018
  state:running action:edit src elapsed:1500ms tokens:4 cost:1
  view:000000000021 write
  diff:+2/-1 files:1
  blocker:-
  evidence:000000000030
  merge:conflict:1
  control:human TAKEOVER";

const APPROVAL_GOLDEN: &str = "\
approval:019c0000-0000-7000-8000-00000000001a requested
capability:fs.read
action:fs.read repo:src/main.rs
risk:low
reason:read source
policy:trusted_project:.rapidlm/policy.toml
rules:repo-ask
scopes:
> once one-shot
  session-exact session-exact
cmd: argv /usr/bin/git cwd=/repo git status
env: HOME=[REDACTED] TOKEN=[REDACTED]";

const CONTEXT_GOLDEN: &str = "\
context tokens:65/500 reserved:50 safety:0
partitions:
  system 8/8
  user 10/10
  goal 5/5
  diff 12/12
  retrieved 20/250
  memory 6/103
  read_set 4/62
mandatory:
> sys tokens:8 source:system reason:system trust:project freshness:unknown pin:no
  user tokens:10 source:user reason:user trust:project freshness:unknown pin:no
  goal tokens:5 source:goal reason:goal trust:project freshness:unknown pin:no
  src/lib.rs:10-20 tokens:12 source:diff reason:error trust:untrusted freshness:fresh pin:no UNTRUSTED
retrieved:
  src/main.rs:1-40 tokens:20 source:retrieved reason:retrieved trust:untrusted freshness:stale pin:no STALE UNTRUSTED
memory:
  prefers-exact-ranges tokens:6 source:memory reason:memory trust:project freshness:unknown pin:no
read_set:
  src/lib.rs:1-8 tokens:4 source:read_set reason:read_set trust:untrusted freshness:fresh pin:no UNTRUSTED
dropped:
  (empty)";

const MEMORY_GOLDEN: &str = "\
memory writes:enabled
user:
> pref-exact source:user confidence:0.90 expiry:none prefers exact ranges
project:
  build-cmd source:agent confidence:0.70 expiry:2026-12-31T00:00:00Z cargo test -p tui
session:
  last-error source:tool confidence:0.40 expiry:2026-08-15T00:00:00Z EXPIRED rustc failed
  api-token source:user confidence:1.00 expiry:none [REDACTED]";

const TRACE_JOBS_GOLDEN: &str = "\
trace/jobs tab:jobs
jobs:
> 019c0000-0000-7000-8000-000000000019 class:supervised state:started exit:- cursor:40 artifact:sha256:2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae truncated
  019c0000-0000-7000-8000-00000000001b class:background state:completed exit:0 cursor:0
traces:
  018f3c8a-7e2b-7a11-8c4d-0123456789ab trace:8f000000-0000-7000-8000-000000000017 parent:- name:tool.exec status:ok 12ms
  018f3c8a-7e2b-7a13-8c4d-0123456789ab trace:8f000000-0000-7000-8000-000000000017 parent:018f3c8a-7e2b-7a11-8c4d-0123456789ab name:[REDACTED] status:error -
log page:1/3 cursor:0 more:yes
  job-a line 00
  job-a line 01
  job-a line 02
  job-a line 03
  job-a line 04
  job-a line 05
  job-a line 06
  job-a line 07
  job-a line 08
  job-a line 09
  job-a line 10
  job-a line 11
  job-a line 12
  job-a line 13
  job-a line 14
  job-a line 15";

const DIFF_GOLDEN: &str = "\
summary files:1 +2 -1 conflicts:0
(empty)
--";

#[derive(Clone, Copy)]
struct TermSize {
    width: u16,
    height: u16,
}

impl TermSize {
    const fn new(width: u16, height: u16) -> Self {
        Self { width, height }
    }

    fn area(self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }
}

struct CellGrid {
    width: usize,
    height: usize,
    rows: Vec<String>,
}

impl CellGrid {
    fn blank(width: u16, height: u16) -> Self {
        let width = usize::from(width);
        let height = usize::from(height);
        Self {
            width,
            height,
            rows: vec![" ".repeat(width); height],
        }
    }

    fn paint_lines(&mut self, rect: Rect, lines: &[String]) {
        if rect.is_empty() || self.width == 0 || self.height == 0 {
            return;
        }
        let x = usize::from(rect.x());
        let y = usize::from(rect.y());
        let w = usize::from(rect.width());
        let h = usize::from(rect.height());
        for row in 0..h {
            let dest_y = y + row;
            if dest_y >= self.height {
                break;
            }
            let src = lines.get(row).map(String::as_str).unwrap_or("");
            let fitted = fit_width(src, w);
            let dest = &mut self.rows[dest_y];
            let dest_x = x.min(self.width);
            let end = (dest_x + w).min(self.width);
            if dest_x >= end {
                continue;
            }
            let mut chars: Vec<char> = dest.chars().collect();
            let piece: Vec<char> = fitted.chars().take(end - dest_x).collect();
            for (offset, ch) in piece.into_iter().enumerate() {
                if dest_x + offset < chars.len() {
                    chars[dest_x + offset] = ch;
                }
            }
            self.rows[dest_y] = chars.into_iter().collect();
        }
    }

    fn snapshot(&self) -> String {
        self.rows.join("\n")
    }
}

struct LatencyStats {
    p50: Duration,
    p95: Duration,
    samples: usize,
}

impl LatencyStats {
    fn from_samples(mut samples: Vec<Duration>) -> Self {
        samples.sort_unstable();
        let n = samples.len();
        let p50 = percentile(&samples, 50);
        let p95 = percentile(&samples, 95);
        Self {
            p50,
            p95,
            samples: n,
        }
    }
}

struct FixtureSet {
    events: Vec<UiEvent>,
    transcript: Transcript,
    composer: ComposerModel,
    chrome: StatusChrome,
}

fn cancel() -> CancellationToken {
    CancellationToken::new()
}

fn router_cancel() -> llm_router::CancellationToken {
    llm_router::CancellationToken::new()
}

fn event_id_for_seq(seq: u64) -> EventId {
    format!("019c0000-0000-7000-8000-{seq:012x}")
        .parse()
        .expect("event id")
}

fn envelope(seq: u64, kind: EventKind, payload: Value) -> EventEnvelope<Value> {
    let at = if seq == 1 { CREATED_AT } else { UPDATED_AT };
    EventEnvelope::new(
        event_id_for_seq(seq),
        SESSION_ID.parse().expect("session"),
        seq,
        at.parse::<RecordedAt>().expect("recorded_at"),
        ActorRef::new(ActorKind::System, ACTOR_ID).expect("actor"),
        TRACE_ID.parse::<TraceId>().expect("trace"),
        kind,
        RedactionClass::Project,
        payload,
    )
}

fn kernel(seq: u64, kind: EventKind, payload: Value) -> UiEvent {
    UiEvent::Kernel(envelope(seq, kind, payload))
}

fn child_id() -> AgentId {
    CHILD_ID.parse().expect("child")
}

fn fixture_events() -> Vec<UiEvent> {
    vec![
        kernel(
            1,
            EventKind::SessionCreated,
            serde_json::json!({"project_id": PROJECT_ID}),
        ),
        kernel(
            2,
            EventKind::GoalCreated,
            serde_json::json!({
                "goal_id": GOAL_ID,
                "statement": "ship the projection",
                "budget": {"max_turns": 8, "max_tokens": 1000}
            }),
        ),
        kernel(
            3,
            EventKind::TurnStarted,
            serde_json::json!({"turn_id": TURN_ID}),
        ),
        kernel(
            4,
            EventKind::AgentSpawned,
            serde_json::json!({
                "agent_id": AGENT_ID,
                "role": "coder",
                "worker_class": "persistent",
                "state": "paused",
                "stats": {"tokens": 12, "cost": 3, "active_ms": 40}
            }),
        ),
        kernel(
            5,
            EventKind::AgentSpawned,
            serde_json::json!({
                "agent_id": CHILD_ID,
                "parent_id": AGENT_ID,
                "worker_class": "managed",
                "workspace_view_id": VIEW_ID
            }),
        ),
        kernel(
            6,
            EventKind::AgentStateChanged,
            serde_json::json!({
                "agent_id": CHILD_ID,
                "state": "running",
                "current_operation": "edit src",
                "evidence_id": EVIDENCE_ID,
                "stats": {"tokens": 4, "cost": 1, "active_ms": 1500}
            }),
        ),
        kernel(
            7,
            EventKind::AgentSpawned,
            serde_json::json!({
                "agent_id": SIBLING_ID,
                "parent_id": AGENT_ID,
                "state": "paused"
            }),
        ),
        kernel(
            8,
            EventKind::JobStarted,
            serde_json::json!({"job_id": JOB_ID}),
        ),
        kernel(
            9,
            EventKind::ApprovalRequested,
            serde_json::json!({"approval_id": APPROVAL_ID}),
        ),
        kernel(
            10,
            EventKind::ControlTransferredToHuman,
            serde_json::json!({}),
        ),
        UiEvent::Local(LocalUiEvent::SelectAgent(Some(child_id()))),
    ]
}

fn fixture_transcript() -> Transcript {
    let mut transcript = Transcript::new();
    transcript.push(RenderBlockKind::System, "session ready");
    transcript.push(RenderBlockKind::User, "review the agents panel");
    transcript.push(
        RenderBlockKind::Assistant,
        "parent is paused; child is editing src",
    );
    transcript.push(RenderBlockKind::Tool, "git status");
    transcript.push(RenderBlockKind::Evidence, "goal budget remaining");
    transcript
}

fn fixture_composer() -> ComposerModel {
    let mut composer = ComposerModel::new();
    composer
        .apply(ComposerCommand::Insert("follow up on the child".into()))
        .expect("insert");
    composer
}

fn fixture_chrome() -> StatusChrome {
    StatusChrome::new()
        .with_model("claude-sonnet")
        .with_provider("anthropic")
        .with_sandbox(SandboxMode::HostRestricted)
        .with_policy(PolicyMode::Ask)
        .with_context(ContextUsage::new(65, Some(500)))
}

fn fixtures() -> FixtureSet {
    FixtureSet {
        events: fixture_events(),
        transcript: fixture_transcript(),
        composer: fixture_composer(),
        chrome: fixture_chrome(),
    }
}

fn replay_state(events: &[UiEvent]) -> tui::AppState {
    replay(events, &cancel()).expect("replay")
}

fn child_handoff() -> AgentHandoffObservation {
    AgentHandoffObservation::new(
        child_id(),
        Some(PatchSummary::new(1, 2, 1)),
        Some(AgentMergeState::Conflict { conflicts: 1 }),
    )
}

fn agents_model(state: &tui::AppState) -> AgentsViewModel {
    AgentsViewModel::from_state(
        state,
        &[child_handoff()],
        AgentsSelection::default(),
        &cancel(),
    )
    .expect("agents")
}

fn approval_model() -> ApprovalViewModel {
    let prompt = ApprovalPrompt::new(
        ApprovalKey::parse(APPROVAL_ID).expect("id"),
        ApprovalLifecycle::Requested,
        ApprovalActionSpec::new(
            CapabilityView::FsRead,
            "fs.read repo:src/main.rs",
            FINGERPRINT,
        )
        .expect("action"),
        ApprovalRiskSpec::new(RiskClassView::Low, "read source").expect("risk"),
        ApprovalPolicySpec::new(
            PolicyLayerView::TrustedProject,
            ".rapidlm/policy.toml",
            ["repo-ask"],
        )
        .expect("policy"),
        ApprovalScopeSpec::default_safe(),
        ApprovalClock::new(1_000, 1_060).expect("clock"),
    )
    .expect("prompt")
    .with_command(
        CommandPreviewInput::argv(
            "/usr/bin/git",
            ["git", "status"],
            "/repo",
            ["HOME", "TOKEN"],
        )
        .expect("command"),
    )
    .with_env_assignments([("TOKEN", "super-secret-env-value")])
    .expect("env discarded");
    ApprovalViewModel::new(prompt, &cancel()).expect("approval")
}

fn context_model() -> ContextViewModel {
    let req = CompileContext::new(500, 50)
        .safety_margin(0)
        .system(CompileInput::new("sys", "sys").tokens(8))
        .user(CompileInput::new("user", "user").tokens(10))
        .goal_block(CompileInput::new("goal", "goal").tokens(5))
        .diff(
            CompileInput::new("src/lib.rs:10-20", "src/lib.rs:10-20")
                .tokens(12)
                .reason(CompileReason::Error)
                .freshness(Freshness::Fresh),
        )
        .retrieved(
            CompileInput::new("src/main.rs:1-40", "src/main.rs:1-40")
                .tokens(20)
                .score(80)
                .freshness(Freshness::Stale),
        )
        .memory(
            CompileInput::new("prefers-exact-ranges", "prefers-exact-ranges")
                .tokens(6)
                .score(40),
        )
        .read_set(
            CompileInput::new("src/lib.rs:1-8", "src/lib.rs:1-8")
                .tokens(4)
                .freshness(Freshness::Fresh),
        );
    ContextViewModel::from_packet(&compile(&req).expect("compile"), &cancel()).expect("context")
}

fn memory_model() -> MemoryViewModel {
    let items = vec![
        MemoryObservation::new(
            "pref-exact",
            MemoryScopeKind::User,
            MemorySourceKind::User,
            "user-1",
            0.9,
            "prefers exact ranges",
        )
        .expect("user"),
        MemoryObservation::new(
            "build-cmd",
            MemoryScopeKind::Project,
            MemorySourceKind::Agent,
            "agent-1",
            0.7,
            "cargo test -p tui",
        )
        .expect("project")
        .with_project("proj-1")
        .with_expires_at("2026-12-31T00:00:00Z"),
        MemoryObservation::new(
            "last-error",
            MemoryScopeKind::Session,
            MemorySourceKind::Tool,
            "tool-1",
            0.4,
            "rustc failed",
        )
        .expect("session")
        .with_project("proj-1")
        .with_session("sess-1")
        .with_expires_at("2026-08-15T00:00:00Z"),
        MemoryObservation::new(
            "api-token",
            MemoryScopeKind::Session,
            MemorySourceKind::User,
            "user-1",
            1.0,
            "sk-super-secret-token",
        )
        .expect("secret")
        .with_project("proj-1")
        .with_session("sess-1")
        .with_redaction(RedactionClass::Secret),
    ];
    MemoryViewModel::with_clock(
        &items,
        true,
        Some("2026-08-16T00:00:00Z"),
        MemorySelection::default(),
        &cancel(),
    )
    .expect("memory")
}

fn provider_caps() -> ProviderCapabilities {
    ProviderCapabilities::new(
        true,
        true,
        true,
        true,
        ReasoningSupport::Exposed,
        true,
        200_000,
        16_384,
        UsageFieldSet::new(true, true, true, true, true, false, false),
    )
    .expect("caps")
}

fn model_selector() -> ModelViewModel {
    let caps = provider_caps();
    let spec = CatalogModelSpec::new(
        ProviderId::parse("anthropic").expect("provider"),
        ModelId::parse("claude-sonnet").expect("model"),
        true,
        caps.clone(),
        ModelPrices::new(
            Some(3_000_000),
            Some(15_000_000),
            None,
            Some(llm_router::PriceTableVersion::parse("table-v1").expect("price table")),
        ),
        vec![Region::parse("us").expect("region")],
        vec![DataPolicyTag::parse("no-training").expect("tag")],
        LatencyClass::Interactive,
    );
    let mut index = ProviderCapIndex::new();
    index
        .insert(spec.provider().clone(), ProviderCapEntry::Available(caps))
        .expect("index");
    let config =
        CatalogConfig::new(CatalogRevision::new(3).expect("rev"), vec![spec]).expect("config");
    let catalog = ModelCatalog::build(&config, &index, &router_cancel())
        .expect("catalog")
        .snapshot()
        .clone();
    let request = RouteRequest::new(
        ModelPurpose::Code,
        PrivacyClass::NoTraining,
        None,
        8_000,
        1_000,
        500,
        ModelCapabilities::new(true, false, false, false, false, false),
        None,
        None,
        Some(ModelRef::new(
            ProviderId::parse("anthropic").expect("p"),
            ModelId::parse("claude-sonnet").expect("m"),
        )),
    )
    .expect("request");
    ModelViewModel::from_catalog(&catalog, &request, &cancel()).expect("models")
}

fn diff_model() -> DiffViewModel {
    DiffViewModel::from_summary(
        &PatchSummary::new(1, 2, 1),
        &[],
        DiffSelection::default(),
        &cancel(),
    )
    .expect("diff")
}

fn jobs_model(_state: &tui::AppState) -> TraceJobsViewModel {
    let spans = vec![
        SpanObservation::new(
            SPAN_A.parse::<SpanId>().expect("span a"),
            TRACE_ID.parse().expect("trace"),
            "tool.exec",
            SpanStatus::Ok,
        )
        .expect("span a")
        .with_duration_ms(12),
        SpanObservation::new(
            SPAN_B.parse::<SpanId>().expect("span b"),
            TRACE_ID.parse().expect("trace"),
            "secret.op",
            SpanStatus::Error,
        )
        .expect("span b")
        .with_parent(SPAN_A.parse().expect("parent"))
        .with_redaction(RedactionClass::Secret),
    ];
    let jobs = vec![
        JobObservation::new(
            JOB_ID.parse::<JobId>().expect("job a"),
            JobClass::Supervised,
            JobLifecycle::Started,
        )
        .expect("job a")
        .with_log(
            ArtifactRef::new(
                ArtifactId::from_bytes(b"foo"),
                "text/plain",
                4096,
                RedactionClass::Project,
            ),
            40,
            (0..40)
                .map(|i| format!("job-a line {i:02}"))
                .collect::<Vec<_>>()
                .join("\n"),
            true,
        )
        .expect("job a log"),
        JobObservation::new(
            SIBLING_ID.parse::<JobId>().expect("job b"),
            JobClass::Background,
            JobLifecycle::Completed,
        )
        .expect("job b")
        .with_exit_status(0),
    ];
    TraceJobsViewModel::from_observations(&spans, &jobs, &cancel()).expect("jobs")
}

fn goal_model(state: &tui::AppState) -> SessionActionsViewModel {
    let observation = SessionLifecycleObservation::from_app_state(state)
        .expect("session")
        .with_recovered(true);
    SessionActionsViewModel::plan(
        SessionLifecycleRequest::Resume { session: None },
        &observation,
        &cancel(),
    )
    .expect("goal")
}

fn sidebar_lines(route: UiRoute, state: &tui::AppState, width: u16, height: u16) -> Vec<String> {
    match route {
        UiRoute::Transcript => Vec::new(),
        UiRoute::Agents => agents_model(state).render(width, height).lines().to_vec(),
        UiRoute::Diff => diff_model().render(width, height).lines().to_vec(),
        UiRoute::Context => context_model().render(width, height).lines().to_vec(),
        UiRoute::Memory => memory_model().render(width, height).lines().to_vec(),
        UiRoute::Jobs => jobs_model(state).render(width, height).lines().to_vec(),
        UiRoute::Approvals => approval_model().render(width, height).lines().to_vec(),
        UiRoute::Goals => Vec::new(),
        // P10 routes render main-panel content only; no sidebar model yet.
        UiRoute::Graph | UiRoute::Computer | UiRoute::Resources | UiRoute::Models => Vec::new(),
    }
}

fn paint_screen(
    state: &tui::AppState,
    fixtures: &FixtureSet,
    size: TermSize,
    modal: bool,
) -> CellGrid {
    let mut composer = fixtures.composer.clone();
    let _ = composer.apply(ComposerCommand::SetWidth(size.width));
    let layout = compute_layout_for(state, size, composer.preferred_height(), modal);
    let mut grid = CellGrid::blank(size.width, size.height);

    let transcript_rect = layout.transcript();
    let mut viewport = TranscriptViewport::from_rect(transcript_rect);
    viewport.follow_end();
    let window = viewport.visible(&fixtures.transcript);
    let transcript_lines: Vec<String> = window
        .rows()
        .iter()
        .map(|row| format!("{}|{}", row.kind().as_str(), row.text()))
        .collect();

    let route = state.route();
    if layout.sidebar().is_empty() && route != UiRoute::Transcript {
        grid.paint_lines(
            transcript_rect,
            &sidebar_lines(
                route,
                state,
                transcript_rect.width(),
                transcript_rect.height(),
            ),
        );
    } else {
        grid.paint_lines(transcript_rect, &transcript_lines);
        if !layout.sidebar().is_empty() {
            grid.paint_lines(
                layout.sidebar(),
                &sidebar_lines(
                    route,
                    state,
                    layout.sidebar().width(),
                    layout.sidebar().height(),
                ),
            );
        }
    }

    let composer_view = composer.render(layout.composer().width(), layout.composer().height());
    grid.paint_lines(layout.composer(), composer_view.lines());

    let status = render_status_with(state, &fixtures.chrome, layout.status().width());
    grid.paint_lines(layout.status(), &[status.text()]);

    if modal && !layout.modal().is_empty() {
        grid.paint_lines(
            layout.modal(),
            approval_model()
                .render(layout.modal().width(), layout.modal().height())
                .lines(),
        );
    }
    grid
}

fn compute_layout_for(
    state: &tui::AppState,
    size: TermSize,
    composer_lines: u16,
    modal: bool,
) -> tui::LayoutRects {
    let mode = match (state.route() != UiRoute::Transcript, modal) {
        (false, false) => UiMode::Transcript,
        (true, false) => UiMode::Sidebar,
        (false, true) => UiMode::Modal,
        (true, true) => UiMode::SidebarModal,
    };
    tui::compute_layout_with_composer(size.area(), mode, composer_lines)
}

fn routed_state(events: &[UiEvent], route: UiRoute, size: TermSize) -> tui::AppState {
    let mut state = replay_state(events);
    state = reduce(
        state,
        &UiEvent::Local(LocalUiEvent::SetViewport {
            width: size.width,
            height: size.height,
        }),
    );
    reduce(state, &UiEvent::Local(LocalUiEvent::SetRoute(route)))
}

fn fit_width(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let cols = text.chars().count();
    if cols == width {
        return text.to_owned();
    }
    if cols < width {
        let mut out = text.to_owned();
        out.extend(std::iter::repeat_n(' ', width - cols));
        return out;
    }
    text.chars().take(width).collect()
}

fn percentile(sorted: &[Duration], pct: u32) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let rank = (u128::from(pct) * (sorted.len() as u128 - 1) + 50) / 100;
    sorted[rank as usize]
}

fn assert_exact_geometry(snapshot: &str, width: u16, height: u16) {
    let lines: Vec<&str> = snapshot.split('\n').collect();
    assert_eq!(lines.len(), usize::from(height), "height {height}");
    assert!(
        lines
            .iter()
            .all(|line| line.chars().count() == usize::from(width)),
        "every row must be {width} cells"
    );
}

fn render_all_panels(state: &tui::AppState, fixtures: &FixtureSet, size: TermSize) {
    let _ = agents_model(state).render(size.width, size.height);
    let _ = approval_model().render(size.width, size.height);
    let _ = context_model().render(size.width, size.height);
    let _ = memory_model().render(size.width, size.height);
    let _ = model_selector().render(size.width, size.height);
    let _ = diff_model().render(size.width, size.height);
    let _ = jobs_model(state).render(size.width, size.height);
    let _ = goal_model(state).render(size.width, size.height);
    let _ = render_status_with(state, &fixtures.chrome, size.width);
    let mut viewport = TranscriptViewport::new(size.width, size.height.saturating_sub(4));
    viewport.follow_end();
    let _ = viewport.visible(&fixtures.transcript);
    let _ = fixtures.composer.render(size.width, 3);
}

#[test]
fn snapshots_cover_80x24_and_120x40() {
    let fixtures = fixtures();
    for (width, height) in [SIZE_80X24, SIZE_120X40] {
        let size = TermSize::new(width, height);
        let state = routed_state(&fixtures.events, UiRoute::Agents, size);
        let first = paint_screen(&state, &fixtures, size, false).snapshot();
        let second = paint_screen(&state, &fixtures, size, false).snapshot();
        assert_eq!(first, second, "cell snapshot must be deterministic");
        assert_exact_geometry(&first, width, height);
        assert!(first.contains("agents control:human") || first.contains("system|session ready"));
    }
}

#[test]
fn fixture_event_streams_render_stable_panel_goldens() {
    let fixtures = fixtures();
    let state = replay_state(&fixtures.events);

    for (width, height) in [SIZE_80X24, SIZE_120X40] {
        assert_eq!(
            agents_model(&state).render(width, height).golden(),
            AGENTS_GOLDEN
        );
        assert_eq!(
            approval_model().render(width, height).golden(),
            APPROVAL_GOLDEN
        );
        assert_eq!(
            context_model().render(width, height).golden(),
            CONTEXT_GOLDEN
        );
        assert_eq!(memory_model().render(width, height).golden(), MEMORY_GOLDEN);
        assert_eq!(
            jobs_model(&state).render(width, height).golden(),
            TRACE_JOBS_GOLDEN
        );
        assert_eq!(diff_model().render(width, height).golden(), DIFF_GOLDEN);
        assert_eq!(
            agents_model(&state)
                .render(width, height)
                .text()
                .lines()
                .count(),
            usize::from(height)
        );
        assert!(
            agents_model(&state)
                .render(width, height)
                .text()
                .lines()
                .all(|line| line.chars().count() == usize::from(width))
        );
    }

    let models = model_selector();
    let model_80 = models.render(80, 24).golden();
    let model_120 = models.render(120, 40).golden();
    assert_eq!(model_80, model_120);
    assert!(model_80.contains("claude-sonnet"));
    assert!(model_80.contains("provider:anthropic"));
    assert!(model_80.contains("privacy:no_training"));

    let goal = goal_model(&state);
    let goal_80 = goal.render(80, 24).golden();
    assert_eq!(goal_80, goal.render(120, 40).golden());
    assert!(goal_80.contains("resume preview"));
    assert!(goal_80.contains("auto-continue:no"));
}

#[test]
fn layout_goldens_match_fixed_terminal_dimensions() {
    assert_eq!(
        compute_layout(Rect::new(0, 0, 80, 24), UiMode::Transcript).golden(),
        "transcript=0+0,80x20 composer=0+20,80x3 status=0+23,80x1 sidebar=- modal=-"
    );
    assert_eq!(
        compute_layout(Rect::new(0, 0, 80, 24), UiMode::Sidebar).golden(),
        "transcript=0+0,80x20 composer=0+20,80x3 status=0+23,80x1 sidebar=- modal=-"
    );
    assert_eq!(
        compute_layout(Rect::new(0, 0, 120, 40), UiMode::Transcript).golden(),
        "transcript=0+0,120x36 composer=0+36,120x3 status=0+39,120x1 sidebar=- modal=-"
    );
    assert_eq!(
        compute_layout(Rect::new(0, 0, 120, 40), UiMode::Sidebar).golden(),
        "transcript=0+0,84x36 composer=0+36,120x3 status=0+39,120x1 sidebar=84+0,36x36 modal=-"
    );
}

#[test]
fn replay_same_event_stream_yields_identical_cells() {
    let fixtures = fixtures();
    let size = TermSize::new(120, 40);
    let left = routed_state(&fixtures.events, UiRoute::Agents, size);
    let right = routed_state(&fixtures.events, UiRoute::Agents, size);
    assert_eq!(left, right);
    assert_eq!(
        paint_screen(&left, &fixtures, size, true).snapshot(),
        paint_screen(&right, &fixtures, size, true).snapshot()
    );
}

#[test]
fn major_routes_paint_distinct_cell_snapshots() {
    let fixtures = fixtures();
    let size = TermSize::new(120, 40);
    let routes = [
        UiRoute::Transcript,
        UiRoute::Agents,
        UiRoute::Diff,
        UiRoute::Context,
        UiRoute::Memory,
        UiRoute::Jobs,
        UiRoute::Approvals,
    ];
    let mut seen = Vec::new();
    for route in routes {
        let state = routed_state(&fixtures.events, route, size);
        let cells = paint_screen(&state, &fixtures, size, matches!(route, UiRoute::Approvals));
        let snapshot = cells.snapshot();
        assert_exact_geometry(&snapshot, size.width, size.height);
        assert!(
            !seen.iter().any(|prior: &String| prior == &snapshot),
            "route {route:?} collided with another snapshot"
        );
        seen.push(snapshot);
    }
}

#[test]
fn input_to_render_records_p50_p95() {
    let fixtures = fixtures();
    let size = TermSize::new(80, 24);
    let base = routed_state(&fixtures.events, UiRoute::Agents, size);
    let mut samples = Vec::with_capacity(LATENCY_SAMPLES);

    for i in 0..LATENCY_SAMPLES {
        let started = Instant::now();
        let mut state = reduce(
            base.clone(),
            &kernel(
                11,
                EventKind::AgentStateChanged,
                serde_json::json!({
                    "agent_id": CHILD_ID,
                    "state": "running",
                    "current_operation": format!("edit src {i}"),
                    "evidence_id": EVIDENCE_ID,
                    "stats": {"tokens": 4 + i as u64, "cost": 1, "active_ms": 1500 + i as u64}
                }),
            ),
        );
        state = reduce(
            state,
            &UiEvent::Local(LocalUiEvent::SetComposerText(format!("note {i}"))),
        );
        let _ = paint_screen(&state, &fixtures, size, false);
        render_all_panels(&state, &fixtures, size);
        samples.push(started.elapsed());
    }

    let stats = LatencyStats::from_samples(samples);
    eprintln!(
        "input-to-render p50={:?} p95={:?} n={}",
        stats.p50, stats.p95, stats.samples
    );
    assert_eq!(stats.samples, LATENCY_SAMPLES);
    assert!(stats.p50 <= stats.p95, "p50 must not exceed p95");
    assert!(
        stats.p95 < MAX_P95_INPUT_TO_RENDER,
        "p95 {:?} exceeded {:?}",
        stats.p95,
        MAX_P95_INPUT_TO_RENDER
    );
}

#[test]
fn transcript_frame_does_not_relayout_full_history() {
    let mut transcript = Transcript::new();
    const BLOCKS: usize = 400;
    for i in 0..BLOCKS {
        transcript.push(
            RenderBlockKind::Assistant,
            &format!("block {i:03} {}", "lorem ".repeat(24)),
        );
    }
    let height = 20;
    let mut viewport = TranscriptViewport::new(80, height);
    viewport.follow_end();
    let first = viewport.visible(&transcript);
    assert!(
        first.work().is_bounded(height),
        "first frame work {:?}",
        first.work()
    );
    assert!(
        first.work().blocks_wrapped() < BLOCKS / 4,
        "wrapped {} of {BLOCKS}",
        first.work().blocks_wrapped()
    );
    assert!(first.work().rows_emitted() <= usize::from(height));

    let last_id = transcript.blocks().last().expect("last").id();
    transcript
        .append_delta(last_id, " streamed tail")
        .expect("delta");
    let after_delta = viewport.visible(&transcript);
    assert!(
        after_delta.work().is_bounded(height),
        "delta frame work {:?}",
        after_delta.work()
    );
    assert!(after_delta.work().blocks_wrapped() < BLOCKS / 4);

    transcript.push(RenderBlockKind::User, "next prompt");
    viewport.follow_end();
    let after_push = viewport.visible(&transcript);
    assert!(
        after_push.work().is_bounded(height),
        "push frame work {:?}",
        after_push.work()
    );
    assert_eq!(after_push.rows().len(), usize::from(height));
}

#[test]
fn approval_modal_cells_sanitize_untrusted_and_redact_env() {
    let fixtures = fixtures();
    let size = TermSize::new(80, 24);
    let state = routed_state(&fixtures.events, UiRoute::Approvals, size);
    let cells = paint_screen(&state, &fixtures, size, true).snapshot();
    assert_exact_geometry(&cells, 80, 24);
    assert!(cells.contains("capability:fs.read"));
    assert!(cells.contains("TOKEN=[REDACTED]"));
    assert!(!cells.contains("super-secret-env-value"));
}

#[test]
fn agents_snapshot_keeps_hierarchy_after_streaming_event() {
    let fixtures = fixtures();
    let size = TermSize::new(120, 40);
    let before = routed_state(&fixtures.events, UiRoute::Agents, size);
    let before_cells = paint_screen(&before, &fixtures, size, false).snapshot();
    let after = reduce(
        before,
        &kernel(
            11,
            EventKind::AgentStateChanged,
            serde_json::json!({
                "agent_id": CHILD_ID,
                "state": "waiting_tool",
                "current_operation": "run tests",
                "stats": {"tokens": 40, "cost": 9, "active_ms": 9000}
            }),
        ),
    );
    let after_cells = paint_screen(&after, &fixtures, size, false).snapshot();
    assert!(before_cells.contains("000000000015"));
    assert!(after_cells.contains("000000000018"));
    assert!(after_cells.contains("waiting_tool") || after_cells.contains("run tests"));
    assert_ne!(before_cells, after_cells);
}
