# RapidLM V3 Development Ledger

Update rows only after observing evidence. `NOT_STARTED → IN_PROGRESS → BLOCKED|VERIFIED` (or `SUPERSEDED`). A task is not VERIFIED because code was written; acceptance evidence must be recorded.

| Task | Phase | Status | Owner/Agent | Started | Completed | Commit/PR | Evidence | Notes |
|---|---|---|---|---|---|---|---|---|
| P0-001 | P0 | NOT_STARTED |  |  |  |  |  | Audit repository tree and current V2 implementation |
| P0-002 | P0 | NOT_STARTED |  |  |  |  |  | Map crates/modules to V3 ownership |
| P0-003 | P0 | NOT_STARTED |  |  |  |  |  | Inventory CLI commands and TUI surfaces |
| P0-004 | P0 | NOT_STARTED |  |  |  |  |  | Inventory persistence schemas and migrations |
| P0-005 | P0 | NOT_STARTED |  |  |  |  |  | Inventory Event Ledger and side-effect journal behavior |
| P0-006 | P0 | NOT_STARTED |  |  |  |  |  | Inventory tool contracts and schema versions |
| P0-007 | P0 | NOT_STARTED |  |  |  |  |  | Inventory policy/approval/lease enforcement points |
| P0-008 | P0 | NOT_STARTED |  |  |  |  |  | Inventory sandbox/process/remote worker backends |
| P0-009 | P0 | NOT_STARTED |  |  |  |  |  | Inventory context/index/search implementation |
| P0-010 | P0 | NOT_STARTED |  |  |  |  |  | Inventory goal/agent/workflow/DAG implementation |
| P0-011 | P0 | NOT_STARTED |  |  |  |  |  | Inventory workspace/VCS/patch paths |
| P0-012 | P0 | NOT_STARTED |  |  |  |  |  | Inventory browser/computer/mobile implementation |
| P0-013 | P0 | NOT_STARTED |  |  |  |  |  | Inventory provider/model routing |
| P0-014 | P0 | NOT_STARTED |  |  |  |  |  | Inventory MCP/ACP/hooks/plugins/skills |
| P0-015 | P0 | NOT_STARTED |  |  |  |  |  | Inventory daemon/background/cron/handoff implementation |
| P0-016 | P0 | NOT_STARTED |  |  |  |  |  | Inventory build/test/lint/security/release configuration |
| P0-017 | P0 | NOT_STARTED |  |  |  |  |  | Run baseline unit/integration/eval suites |
| P0-018 | P0 | NOT_STARTED |  |  |  |  |  | Capture baseline startup/token/context/tool-repair metrics |
| P0-019 | P0 | NOT_STARTED |  |  |  |  |  | Create KEEP/ADAPT/REPLACE/DELETE migration matrix |
| P0-020 | P0 | NOT_STARTED |  |  |  |  |  | Create V2→V3 ownership/dependency ADR |
| P0-GATE | P0 | NOT_STARTED |  |  |  |  |  | Pass Repository Audit & V2→V3 Migration phase gate |
| P1-001 | P1 | NOT_STARTED |  |  |  |  |  | Define canonical V3 IDs and error envelopes |
| P1-002 | P1 | NOT_STARTED |  |  |  |  |  | Version EventEnvelope and event-kind registry |
| P1-003 | P1 | NOT_STARTED |  |  |  |  |  | Implement transactional session sequence allocation |
| P1-004 | P1 | NOT_STARTED |  |  |  |  |  | Implement event append + projection transaction |
| P1-005 | P1 | NOT_STARTED |  |  |  |  |  | Implement projection checkpoints and rebuild |
| P1-006 | P1 | NOT_STARTED |  |  |  |  |  | Implement content-addressed Artifact Store |
| P1-007 | P1 | NOT_STARTED |  |  |  |  |  | Implement Artifact metadata/provenance registry |
| P1-008 | P1 | NOT_STARTED |  |  |  |  |  | Implement Operation Journal state machine |
| P1-009 | P1 | NOT_STARTED |  |  |  |  |  | Implement stable effect fingerprints |
| P1-010 | P1 | NOT_STARTED |  |  |  |  |  | Implement idempotency classes and replay policy |
| P1-011 | P1 | NOT_STARTED |  |  |  |  |  | Implement uncertain-effect reconciliation interface |
| P1-012 | P1 | NOT_STARTED |  |  |  |  |  | Implement hash-linked egress attempt receipts |
| P1-013 | P1 | NOT_STARTED |  |  |  |  |  | Implement run/session lifecycle service |
| P1-014 | P1 | NOT_STARTED |  |  |  |  |  | Implement graph/run checkpoint envelope |
| P1-015 | P1 | NOT_STARTED |  |  |  |  |  | Implement session recovery procedure |
| P1-016 | P1 | NOT_STARTED |  |  |  |  |  | Implement active-goal recovery-to-paused rule |
| P1-017 | P1 | NOT_STARTED |  |  |  |  |  | Implement session fork lineage |
| P1-018 | P1 | NOT_STARTED |  |  |  |  |  | Implement checkpoint/rewind metadata |
| P1-019 | P1 | NOT_STARTED |  |  |  |  |  | Implement retention/GC roots for artifacts |
| P1-020 | P1 | NOT_STARTED |  |  |  |  |  | Implement redaction classes and secure logging |
| P1-021 | P1 | NOT_STARTED |  |  |  |  |  | Implement durable approval/wait records |
| P1-022 | P1 | NOT_STARTED |  |  |  |  |  | Implement kernel health snapshots |
| P1-023 | P1 | NOT_STARTED |  |  |  |  |  | Implement structured shutdown/quiesce |
| P1-024 | P1 | NOT_STARTED |  |  |  |  |  | Add crash-at-every-transition durability suite |
| P1-GATE | P1 | NOT_STARTED |  |  |  |  |  | Pass Kernel, Durability, Artifacts & Sessions phase gate |
| P2-001 | P2 | NOT_STARTED |  |  |  |  |  | Define RuntimeGraph/Node/Edge IR |
| P2-002 | P2 | NOT_STARTED |  |  |  |  |  | Define NodeKind and EdgeKind registries |
| P2-003 | P2 | NOT_STARTED |  |  |  |  |  | Define node state machine |
| P2-004 | P2 | NOT_STARTED |  |  |  |  |  | Define GraphProposal contract |
| P2-005 | P2 | NOT_STARTED |  |  |  |  |  | Implement graph proposal schema validation |
| P2-006 | P2 | NOT_STARTED |  |  |  |  |  | Implement dependency cycle validation |
| P2-007 | P2 | NOT_STARTED |  |  |  |  |  | Implement graph revision persistence |
| P2-008 | P2 | NOT_STARTED |  |  |  |  |  | Implement immutable graph diff model |
| P2-009 | P2 | NOT_STARTED |  |  |  |  |  | Implement host graph mutation service |
| P2-010 | P2 | NOT_STARTED |  |  |  |  |  | Implement deterministic ready-set computation |
| P2-011 | P2 | NOT_STARTED |  |  |  |  |  | Implement dependency condition evaluation |
| P2-012 | P2 | NOT_STARTED |  |  |  |  |  | Implement joins and barriers |
| P2-013 | P2 | NOT_STARTED |  |  |  |  |  | Implement shard/fan-out nodes |
| P2-014 | P2 | NOT_STARTED |  |  |  |  |  | Implement resource/capability readiness constraints |
| P2-015 | P2 | NOT_STARTED |  |  |  |  |  | Implement workspace write-conflict readiness |
| P2-016 | P2 | NOT_STARTED |  |  |  |  |  | Implement node attempt lifecycle |
| P2-017 | P2 | NOT_STARTED |  |  |  |  |  | Implement node retry classes/backoff |
| P2-018 | P2 | NOT_STARTED |  |  |  |  |  | Implement cancellation propagation |
| P2-019 | P2 | NOT_STARTED |  |  |  |  |  | Implement waiting/suspend/resume nodes |
| P2-020 | P2 | NOT_STARTED |  |  |  |  |  | Implement approval node executor |
| P2-021 | P2 | NOT_STARTED |  |  |  |  |  | Implement AskUser node executor |
| P2-022 | P2 | NOT_STARTED |  |  |  |  |  | Implement trigger/timer node executor |
| P2-023 | P2 | NOT_STARTED |  |  |  |  |  | Implement graph checkpoints/resume |
| P2-024 | P2 | NOT_STARTED |  |  |  |  |  | Implement repair/replan revision semantics |
| P2-025 | P2 | NOT_STARTED |  |  |  |  |  | Implement supersession semantics |
| P2-026 | P2 | NOT_STARTED |  |  |  |  |  | Implement invalidation engine |
| P2-027 | P2 | NOT_STARTED |  |  |  |  |  | Implement bounded invalidation propagation |
| P2-028 | P2 | NOT_STARTED |  |  |  |  |  | Implement graph budget accounting |
| P2-029 | P2 | NOT_STARTED |  |  |  |  |  | Implement scheduler fairness across runs |
| P2-030 | P2 | NOT_STARTED |  |  |  |  |  | Implement graph why-ready/why-blocked explain |
| P2-031 | P2 | NOT_STARTED |  |  |  |  |  | Implement graph export JSON/DOT/Mermaid |
| P2-032 | P2 | NOT_STARTED |  |  |  |  |  | Implement Graph Inspector kernel API |
| P2-GATE | P2 | NOT_STARTED |  |  |  |  |  | Pass Dynamic Runtime Graph phase gate |
| P3-001 | P3 | NOT_STARTED |  |  |  |  |  | Define InformationNeed and ContextPacket contracts |
| P3-002 | P3 | NOT_STARTED |  |  |  |  |  | Implement content hashing/file watcher generations |
| P3-003 | P3 | NOT_STARTED |  |  |  |  |  | Implement language detection and chunking |
| P3-004 | P3 | NOT_STARTED |  |  |  |  |  | Integrate ripgrep exact/regex search |
| P3-005 | P3 | NOT_STARTED |  |  |  |  |  | Implement SQLite FTS/BM25 index |
| P3-006 | P3 | NOT_STARTED |  |  |  |  |  | Integrate Tree-sitter parser/symbol extractor |
| P3-007 | P3 | NOT_STARTED |  |  |  |  |  | Integrate LSP definitions/references/types/diagnostics |
| P3-008 | P3 | NOT_STARTED |  |  |  |  |  | Implement git/manifest/build/test link extractor |
| P3-009 | P3 | NOT_STARTED |  |  |  |  |  | Implement deterministic code graph projection |
| P3-010 | P3 | NOT_STARTED |  |  |  |  |  | Implement bounded graph neighbor/impact queries |
| P3-011 | P3 | NOT_STARTED |  |  |  |  |  | Implement optional embedding/vector adapter |
| P3-012 | P3 | NOT_STARTED |  |  |  |  |  | Implement candidate merge/score normalization |
| P3-013 | P3 | NOT_STARTED |  |  |  |  |  | Implement MMR/diversity reranking |
| P3-014 | P3 | NOT_STARTED |  |  |  |  |  | Implement scope/trust/freshness filters |
| P3-015 | P3 | NOT_STARTED |  |  |  |  |  | Implement token estimator/model tokenizer adapter |
| P3-016 | P3 | NOT_STARTED |  |  |  |  |  | Implement context budget allocator |
| P3-017 | P3 | NOT_STARTED |  |  |  |  |  | Implement context packer with protected categories |
| P3-018 | P3 | NOT_STARTED |  |  |  |  |  | Implement Context Scout agent contract |
| P3-019 | P3 | NOT_STARTED |  |  |  |  |  | Implement semantic-first scout retrieval flow |
| P3-020 | P3 | NOT_STARTED |  |  |  |  |  | Implement exhaustive reference enumeration mode |
| P3-021 | P3 | NOT_STARTED |  |  |  |  |  | Implement broaden-on-zero-hit policy |
| P3-022 | P3 | NOT_STARTED |  |  |  |  |  | Implement checked negative findings |
| P3-023 | P3 | NOT_STARTED |  |  |  |  |  | Implement open-question handoff |
| P3-024 | P3 | NOT_STARTED |  |  |  |  |  | Implement read tool line/byte/per-line/token ceilings |
| P3-025 | P3 | NOT_STARTED |  |  |  |  |  | Implement structured continuation cursors |
| P3-026 | P3 | NOT_STARTED |  |  |  |  |  | Implement ReadObservation ledger |
| P3-027 | P3 | NOT_STARTED |  |  |  |  |  | Implement context visibility generations |
| P3-028 | P3 | NOT_STARTED |  |  |  |  |  | Implement context-aware dedup |
| P3-029 | P3 | NOT_STARTED |  |  |  |  |  | Implement post-write context invalidation |
| P3-030 | P3 | NOT_STARTED |  |  |  |  |  | Implement deterministic compaction fallback |
| P3-031 | P3 | NOT_STARTED |  |  |  |  |  | Implement model-assisted structured compaction |
| P3-032 | P3 | NOT_STARTED |  |  |  |  |  | Implement /context explainability metrics |
| P3-GATE | P3 | NOT_STARTED |  |  |  |  |  | Pass Context Fabric, Code Intelligence & Knowledge phase gate |
| P4-001 | P4 | NOT_STARTED |  |  |  |  |  | Define WorkspaceView/WorkspaceTransaction contracts |
| P4-002 | P4 | NOT_STARTED |  |  |  |  |  | Implement direct workspace backend |
| P4-003 | P4 | NOT_STARTED |  |  |  |  |  | Implement git-worktree backend |
| P4-004 | P4 | NOT_STARTED |  |  |  |  |  | Implement overlay/sandbox backend |
| P4-005 | P4 | NOT_STARTED |  |  |  |  |  | Implement remote snapshot backend |
| P4-006 | P4 | NOT_STARTED |  |  |  |  |  | Implement preimage/hash guarded patch apply |
| P4-007 | P4 | NOT_STARTED |  |  |  |  |  | Implement semantic patch metadata/intent |
| P4-008 | P4 | NOT_STARTED |  |  |  |  |  | Implement external mutation detector |
| P4-009 | P4 | NOT_STARTED |  |  |  |  |  | Implement symbol-level change attribution |
| P4-010 | P4 | NOT_STARTED |  |  |  |  |  | Implement child-view transactional integration |
| P4-011 | P4 | NOT_STARTED |  |  |  |  |  | Implement conflict detection/staging rollback |
| P4-012 | P4 | NOT_STARTED |  |  |  |  |  | Implement read-before-write freshness invariant |
| P4-013 | P4 | NOT_STARTED |  |  |  |  |  | Define stable model-visible Tool Gateway |
| P4-014 | P4 | NOT_STARTED |  |  |  |  |  | Define versioned tool schema registry |
| P4-015 | P4 | NOT_STARTED |  |  |  |  |  | Implement validate-original-first tool parsing |
| P4-016 | P4 | NOT_STARTED |  |  |  |  |  | Implement validator issue-path extraction |
| P4-017 | P4 | NOT_STARTED |  |  |  |  |  | Implement optional-null repair rule |
| P4-018 | P4 | NOT_STARTED |  |  |  |  |  | Implement stringified-array/object repair rule |
| P4-019 | P4 | NOT_STARTED |  |  |  |  |  | Implement singleton-array repair rule |
| P4-020 | P4 | NOT_STARTED |  |  |  |  |  | Implement numeric/boolean/enum safe repair rules |
| P4-021 | P4 | NOT_STARTED |  |  |  |  |  | Implement semantic-ambiguity repair refusal |
| P4-022 | P4 | NOT_STARTED |  |  |  |  |  | Implement repair telemetry/model feedback |
| P4-023 | P4 | NOT_STARTED |  |  |  |  |  | Implement universal ToolOutcome recovery contract |
| P4-024 | P4 | NOT_STARTED |  |  |  |  |  | Implement cross-tool invariant engine |
| P4-025 | P4 | NOT_STARTED |  |  |  |  |  | Implement Capability vocabulary/normalization |
| P4-026 | P4 | NOT_STARTED |  |  |  |  |  | Implement policy precedence engine |
| P4-027 | P4 | NOT_STARTED |  |  |  |  |  | Implement CapabilityLease issuance/signature |
| P4-028 | P4 | NOT_STARTED |  |  |  |  |  | Implement executor-side lease verification |
| P4-029 | P4 | NOT_STARTED |  |  |  |  |  | Implement durable approval queue |
| P4-030 | P4 | NOT_STARTED |  |  |  |  |  | Implement dont-ask automation semantics |
| P4-031 | P4 | NOT_STARTED |  |  |  |  |  | Implement Capability Projection Engine |
| P4-032 | P4 | NOT_STARTED |  |  |  |  |  | Implement SecretHandle store |
| P4-033 | P4 | NOT_STARTED |  |  |  |  |  | Implement Credential Broker ephemeral credentials |
| P4-034 | P4 | NOT_STARTED |  |  |  |  |  | Implement environment-identity-safe credential cache |
| P4-GATE | P4 | NOT_STARTED |  |  |  |  |  | Pass Workspace, Tool Gateway, Repair, Policy & Secrets phase gate |
| P5-001 | P5 | NOT_STARTED |  |  |  |  |  | Define AgentExecutionContext and AgentResult |
| P5-002 | P5 | NOT_STARTED |  |  |  |  |  | Implement node AgentExecutor interface |
| P5-003 | P5 | NOT_STARTED |  |  |  |  |  | Implement role registry and capability profiles |
| P5-004 | P5 | NOT_STARTED |  |  |  |  |  | Implement main agent role |
| P5-005 | P5 | NOT_STARTED |  |  |  |  |  | Implement Context Scout role |
| P5-006 | P5 | NOT_STARTED |  |  |  |  |  | Implement planner/architect role |
| P5-007 | P5 | NOT_STARTED |  |  |  |  |  | Implement coder/debugger roles |
| P5-008 | P5 | NOT_STARTED |  |  |  |  |  | Implement reviewer/tester roles |
| P5-009 | P5 | NOT_STARTED |  |  |  |  |  | Implement independent verifier role |
| P5-010 | P5 | NOT_STARTED |  |  |  |  |  | Implement security/performance reviewer roles |
| P5-011 | P5 | NOT_STARTED |  |  |  |  |  | Implement browser/computer operator role |
| P5-012 | P5 | NOT_STARTED |  |  |  |  |  | Implement release-manager role |
| P5-013 | P5 | NOT_STARTED |  |  |  |  |  | Implement clean-context TaskEnvelope |
| P5-014 | P5 | NOT_STARTED |  |  |  |  |  | Implement typed agent mailbox/result refs |
| P5-015 | P5 | NOT_STARTED |  |  |  |  |  | Implement isolated subagent lifecycle |
| P5-016 | P5 | NOT_STARTED |  |  |  |  |  | Implement persistent read-only specialist lifecycle |
| P5-017 | P5 | NOT_STARTED |  |  |  |  |  | Implement background specialist bounded summaries |
| P5-018 | P5 | NOT_STARTED |  |  |  |  |  | Implement delegation utility scoring |
| P5-019 | P5 | NOT_STARTED |  |  |  |  |  | Implement nested delegation depth/budget bounds |
| P5-020 | P5 | NOT_STARTED |  |  |  |  |  | Implement agent cancellation/timeouts |
| P5-021 | P5 | NOT_STARTED |  |  |  |  |  | Implement empty-response retry guard |
| P5-022 | P5 | NOT_STARTED |  |  |  |  |  | Implement repeated tool-call loop detector |
| P5-023 | P5 | NOT_STARTED |  |  |  |  |  | Implement repeated-message/stream loop detector |
| P5-024 | P5 | NOT_STARTED |  |  |  |  |  | Implement compact-before-context-overflow retry |
| P5-025 | P5 | NOT_STARTED |  |  |  |  |  | Implement per-subtask model routing |
| P5-026 | P5 | NOT_STARTED |  |  |  |  |  | Implement provider/model capability profiles |
| P5-027 | P5 | NOT_STARTED |  |  |  |  |  | Implement routing fallback/circuit breakers |
| P5-028 | P5 | NOT_STARTED |  |  |  |  |  | Implement PromptRegistry version/hash model |
| P5-029 | P5 | NOT_STARTED |  |  |  |  |  | Implement layered PromptComposer |
| P5-030 | P5 | NOT_STARTED |  |  |  |  |  | Implement AGENTS hierarchy loader |
| P5-031 | P5 | NOT_STARTED |  |  |  |  |  | Implement skill metadata registry |
| P5-032 | P5 | NOT_STARTED |  |  |  |  |  | Implement skill progressive disclosure |
| P5-GATE | P5 | NOT_STARTED |  |  |  |  |  | Pass Agent Harness, Prompts, Models & Skills phase gate |
| P6-001 | P6 | NOT_STARTED |  |  |  |  |  | Define GoalContract and lifecycle |
| P6-002 | P6 | NOT_STARTED |  |  |  |  |  | Implement active/paused/blocked goal state |
| P6-003 | P6 | NOT_STARTED |  |  |  |  |  | Implement goal create/replace/clear rules |
| P6-004 | P6 | NOT_STARTED |  |  |  |  |  | Implement goal turn-boundary context injection |
| P6-005 | P6 | NOT_STARTED |  |  |  |  |  | Implement explicit turn/token/wall-clock budgets |
| P6-006 | P6 | NOT_STARTED |  |  |  |  |  | Implement budget convergence guidance |
| P6-007 | P6 | NOT_STARTED |  |  |  |  |  | Implement goal crash recovery behavior |
| P6-008 | P6 | NOT_STARTED |  |  |  |  |  | Define Criterion/Claim/Evidence schemas |
| P6-009 | P6 | NOT_STARTED |  |  |  |  |  | Implement evidence provenance/freshness |
| P6-010 | P6 | NOT_STARTED |  |  |  |  |  | Implement claim support/contradiction edges |
| P6-011 | P6 | NOT_STARTED |  |  |  |  |  | Implement evidence invalidation propagation |
| P6-012 | P6 | NOT_STARTED |  |  |  |  |  | Define VerificationRecord/verdict |
| P6-013 | P6 | NOT_STARTED |  |  |  |  |  | Implement deterministic verifier adapters |
| P6-014 | P6 | NOT_STARTED |  |  |  |  |  | Implement independent verifier Agent node |
| P6-015 | P6 | NOT_STARTED |  |  |  |  |  | Implement verifier context isolation |
| P6-016 | P6 | NOT_STARTED |  |  |  |  |  | Implement CompletionCandidate contract |
| P6-017 | P6 | NOT_STARTED |  |  |  |  |  | Implement host GoalCompletionGate |
| P6-018 | P6 | NOT_STARTED |  |  |  |  |  | Implement false-completion rejection path |
| P6-019 | P6 | NOT_STARTED |  |  |  |  |  | Implement rejection gap→repair graph expansion |
| P6-020 | P6 | NOT_STARTED |  |  |  |  |  | Implement visual vs deterministic proof precedence |
| P6-021 | P6 | NOT_STARTED |  |  |  |  |  | Implement goal pause/resume/cancel kernel APIs |
| P6-022 | P6 | NOT_STARTED |  |  |  |  |  | Implement goal/evidence TUI inspector |
| P6-023 | P6 | NOT_STARTED |  |  |  |  |  | Implement goal export/attestation |
| P6-024 | P6 | NOT_STARTED |  |  |  |  |  | Add adversarial false-completion eval suite |
| P6-GATE | P6 | NOT_STARTED |  |  |  |  |  | Pass Goals, Evidence & Independent Verification phase gate |
| P7-001 | P7 | NOT_STARTED |  |  |  |  |  | Define ProcessSpec/ProcessHandle/ProcessOutcome |
| P7-002 | P7 | NOT_STARTED |  |  |  |  |  | Implement process group/tree ownership |
| P7-003 | P7 | NOT_STARTED |  |  |  |  |  | Implement PTY sessions and input generation guards |
| P7-004 | P7 | NOT_STARTED |  |  |  |  |  | Implement bounded stdout/stderr head-tail results |
| P7-005 | P7 | NOT_STARTED |  |  |  |  |  | Implement full process log Artifact spooling |
| P7-006 | P7 | NOT_STARTED |  |  |  |  |  | Implement foreground→daemon detach policy |
| P7-007 | P7 | NOT_STARTED |  |  |  |  |  | Implement process timeout/cancellation |
| P7-008 | P7 | NOT_STARTED |  |  |  |  |  | Implement protected-user-process safeguards |
| P7-009 | P7 | NOT_STARTED |  |  |  |  |  | Define MonitorSpec and event predicates |
| P7-010 | P7 | NOT_STARTED |  |  |  |  |  | Implement exit-code monitor |
| P7-011 | P7 | NOT_STARTED |  |  |  |  |  | Implement regex/log-event monitor |
| P7-012 | P7 | NOT_STARTED |  |  |  |  |  | Implement port/readiness monitor |
| P7-013 | P7 | NOT_STARTED |  |  |  |  |  | Implement file/event monitor |
| P7-014 | P7 | NOT_STARTED |  |  |  |  |  | Implement graph wake-on-monitor events |
| P7-015 | P7 | NOT_STARTED |  |  |  |  |  | Define TriggerSpec and durable cursors |
| P7-016 | P7 | NOT_STARTED |  |  |  |  |  | Implement cron trigger parser/scheduler |
| P7-017 | P7 | NOT_STARTED |  |  |  |  |  | Implement deterministic per-job jitter |
| P7-018 | P7 | NOT_STARTED |  |  |  |  |  | Implement JobLease generation/heartbeat |
| P7-019 | P7 | NOT_STARTED |  |  |  |  |  | Implement missed one-shot semantics |
| P7-020 | P7 | NOT_STARTED |  |  |  |  |  | Define SandboxSpec/backend trait |
| P7-021 | P7 | NOT_STARTED |  |  |  |  |  | Implement host-restricted backend |
| P7-022 | P7 | NOT_STARTED |  |  |  |  |  | Implement rootless/container backend |
| P7-023 | P7 | NOT_STARTED |  |  |  |  |  | Implement stronger kernel/microVM adapter seam |
| P7-024 | P7 | NOT_STARTED |  |  |  |  |  | Define ResourcePool/EnvironmentLease |
| P7-025 | P7 | NOT_STARTED |  |  |  |  |  | Implement warm acquire/release |
| P7-026 | P7 | NOT_STARTED |  |  |  |  |  | Implement environment phase telemetry |
| P7-027 | P7 | NOT_STARTED |  |  |  |  |  | Implement pool sanitation/quarantine |
| P7-028 | P7 | NOT_STARTED |  |  |  |  |  | Implement pool miss fallback provisioning |
| P7-029 | P7 | NOT_STARTED |  |  |  |  |  | Implement remote worker resource backend |
| P7-030 | P7 | NOT_STARTED |  |  |  |  |  | Add disconnect/install-interruption chaos tests |
| P7-GATE | P7 | NOT_STARTED |  |  |  |  |  | Pass Process, Sandbox, Resource Pool, Jobs & Triggers phase gate |
| P8-001 | P8 | NOT_STARTED |  |  |  |  |  | Define SurfaceRef/Observation/generation contracts |
| P8-002 | P8 | NOT_STARTED |  |  |  |  |  | Define ComputerAction schema |
| P8-003 | P8 | NOT_STARTED |  |  |  |  |  | Implement coordinate normalization/scaling |
| P8-004 | P8 | NOT_STARTED |  |  |  |  |  | Implement pointer state/move/click/down/up |
| P8-005 | P8 | NOT_STARTED |  |  |  |  |  | Implement double-click and drag |
| P8-006 | P8 | NOT_STARTED |  |  |  |  |  | Implement keyboard key/chord/type |
| P8-007 | P8 | NOT_STARTED |  |  |  |  |  | Implement scroll and wait conditions |
| P8-008 | P8 | NOT_STARTED |  |  |  |  |  | Implement screenshot/region/cursor actions |
| P8-009 | P8 | NOT_STARTED |  |  |  |  |  | Implement action batching rules |
| P8-010 | P8 | NOT_STARTED |  |  |  |  |  | Implement settle/reobserve policy |
| P8-011 | P8 | NOT_STARTED |  |  |  |  |  | Implement screenshot-on-failure diagnostics |
| P8-012 | P8 | NOT_STARTED |  |  |  |  |  | Implement action metrics/trace events |
| P8-013 | P8 | NOT_STARTED |  |  |  |  |  | Implement BrowserSession lifecycle |
| P8-014 | P8 | NOT_STARTED |  |  |  |  |  | Implement DOM/test-id locator actions |
| P8-015 | P8 | NOT_STARTED |  |  |  |  |  | Implement accessibility-tree browser actions |
| P8-016 | P8 | NOT_STARTED |  |  |  |  |  | Implement CDP allowlist bridge |
| P8-017 | P8 | NOT_STARTED |  |  |  |  |  | Implement browser console/network capture |
| P8-018 | P8 | NOT_STARTED |  |  |  |  |  | Implement browser JS execution policy gate |
| P8-019 | P8 | NOT_STARTED |  |  |  |  |  | Implement download/upload policy paths |
| P8-020 | P8 | NOT_STARTED |  |  |  |  |  | Implement prompt-injection fencing for page content |
| P8-021 | P8 | NOT_STARTED |  |  |  |  |  | Implement macOS AX backend |
| P8-022 | P8 | NOT_STARTED |  |  |  |  |  | Implement Windows UIA backend |
| P8-023 | P8 | NOT_STARTED |  |  |  |  |  | Implement Linux AT-SPI/platform pointer seam |
| P8-024 | P8 | NOT_STARTED |  |  |  |  |  | Implement PreviewSupervisor lifecycle |
| P8-025 | P8 | NOT_STARTED |  |  |  |  |  | Implement dynamic port/readiness checks |
| P8-026 | P8 | NOT_STARTED |  |  |  |  |  | Implement compiler/HMR/HTTP diagnostics |
| P8-027 | P8 | NOT_STARTED |  |  |  |  |  | Implement preview console/network feedback |
| P8-028 | P8 | NOT_STARTED |  |  |  |  |  | Implement visual evidence recorder |
| P8-029 | P8 | NOT_STARTED |  |  |  |  |  | Implement Android emulator/ADB adapter |
| P8-030 | P8 | NOT_STARTED |  |  |  |  |  | Implement iOS simctl remote-mac adapter |
| P8-031 | P8 | NOT_STARTED |  |  |  |  |  | Implement ControlLease human takeover |
| P8-032 | P8 | NOT_STARTED |  |  |  |  |  | Implement post-takeover mutation/surface reconciliation |
| P8-GATE | P8 | NOT_STARTED |  |  |  |  |  | Pass Browser, Computer Use, Mobile & Preview phase gate |
| P9-001 | P9 | NOT_STARTED |  |  |  |  |  | Define MCP catalog revision model |
| P9-002 | P9 | NOT_STARTED |  |  |  |  |  | Implement MCP client discovery/cache |
| P9-003 | P9 | NOT_STARTED |  |  |  |  |  | Implement MCP invocation middleware chain |
| P9-004 | P9 | NOT_STARTED |  |  |  |  |  | Implement MCP output trust fencing |
| P9-005 | P9 | NOT_STARTED |  |  |  |  |  | Implement explicit RapidLM MCP server surface |
| P9-006 | P9 | NOT_STARTED |  |  |  |  |  | Implement ACP stdio framing |
| P9-007 | P9 | NOT_STARTED |  |  |  |  |  | Implement ACP session/progress/diff/permission mapping |
| P9-008 | P9 | NOT_STARTED |  |  |  |  |  | Implement versioned headless JSONL schema |
| P9-009 | P9 | NOT_STARTED |  |  |  |  |  | Implement JSONL stdout purity tests |
| P9-010 | P9 | NOT_STARTED |  |  |  |  |  | Implement TypeScript SDK generated contracts |
| P9-011 | P9 | NOT_STARTED |  |  |  |  |  | Implement SDK reconnect/event cursors |
| P9-012 | P9 | NOT_STARTED |  |  |  |  |  | Define lifecycle HookEvent registry |
| P9-013 | P9 | NOT_STARTED |  |  |  |  |  | Implement hook filter/matcher engine |
| P9-014 | P9 | NOT_STARTED |  |  |  |  |  | Implement hook timeouts/process-group cleanup |
| P9-015 | P9 | NOT_STARTED |  |  |  |  |  | Implement per-event fail-open/fail-closed semantics |
| P9-016 | P9 | NOT_STARTED |  |  |  |  |  | Implement hook capability restrictions |
| P9-017 | P9 | NOT_STARTED |  |  |  |  |  | Define WASM plugin manifest/WIT host API |
| P9-018 | P9 | NOT_STARTED |  |  |  |  |  | Implement WASM capability intersection |
| P9-019 | P9 | NOT_STARTED |  |  |  |  |  | Implement plugin lifecycle/update/disable |
| P9-020 | P9 | NOT_STARTED |  |  |  |  |  | Implement skill/plugin marketplace metadata seam |
| P9-021 | P9 | NOT_STARTED |  |  |  |  |  | Implement Playbook→initial graph compiler |
| P9-022 | P9 | NOT_STARTED |  |  |  |  |  | Implement external ACP agent adapter |
| P9-023 | P9 | NOT_STARTED |  |  |  |  |  | Implement external CLI agent adapter |
| P9-024 | P9 | NOT_STARTED |  |  |  |  |  | Implement external agent result/evidence normalization |
| P9-025 | P9 | NOT_STARTED |  |  |  |  |  | Define HandoffBundle |
| P9-026 | P9 | NOT_STARTED |  |  |  |  |  | Implement SessionExecutionLease generation transfer |
| P9-027 | P9 | NOT_STARTED |  |  |  |  |  | Implement local↔remote handoff state machine |
| P9-028 | P9 | NOT_STARTED |  |  |  |  |  | Implement fresh target lease/credential/observation issuance |
| P9-GATE | P9 | NOT_STARTED |  |  |  |  |  | Pass MCP, ACP, SDK, Plugins, Hooks, External Agents & Handoff phase gate |
| P10-001 | P10 | NOT_STARTED |  |  |  |  |  | Implement rapid composition root/startup |
| P10-002 | P10 | NOT_STARTED |  |  |  |  |  | Implement daemon IPC/authentication |
| P10-003 | P10 | NOT_STARTED |  |  |  |  |  | Implement KernelClient in-process/IPC transports |
| P10-004 | P10 | NOT_STARTED |  |  |  |  |  | Implement interactive prompt/transcript reducer |
| P10-005 | P10 | NOT_STARTED |  |  |  |  |  | Implement streaming coalescing/backpressure |
| P10-006 | P10 | NOT_STARTED |  |  |  |  |  | Implement 80-column/NO_COLOR rendering |
| P10-007 | P10 | NOT_STARTED |  |  |  |  |  | Implement command palette/slash command registry |
| P10-008 | P10 | NOT_STARTED |  |  |  |  |  | Implement graph inspector UI |
| P10-009 | P10 | NOT_STARTED |  |  |  |  |  | Implement context inspector UI |
| P10-010 | P10 | NOT_STARTED |  |  |  |  |  | Implement agent inspector UI |
| P10-011 | P10 | NOT_STARTED |  |  |  |  |  | Implement files/diff WorkspaceTransaction UI |
| P10-012 | P10 | NOT_STARTED |  |  |  |  |  | Implement terminal/process monitor UI |
| P10-013 | P10 | NOT_STARTED |  |  |  |  |  | Implement approvals UI |
| P10-014 | P10 | NOT_STARTED |  |  |  |  |  | Implement evidence/verification UI |
| P10-015 | P10 | NOT_STARTED |  |  |  |  |  | Implement browser/computer panel |
| P10-016 | P10 | NOT_STARTED |  |  |  |  |  | Implement resources/sandbox panel |
| P10-017 | P10 | NOT_STARTED |  |  |  |  |  | Implement memory/knowledge/preference inspector |
| P10-018 | P10 | NOT_STARTED |  |  |  |  |  | Implement trace/events inspector |
| P10-019 | P10 | NOT_STARTED |  |  |  |  |  | Implement model/router selector/status |
| P10-020 | P10 | NOT_STARTED |  |  |  |  |  | Implement goal card/budget controls |
| P10-021 | P10 | NOT_STARTED |  |  |  |  |  | Implement human-control ownership indicator |
| P10-022 | P10 | NOT_STARTED |  |  |  |  |  | Implement session list/search/resume |
| P10-023 | P10 | NOT_STARTED |  |  |  |  |  | Implement timeline/checkpoint UI |
| P10-024 | P10 | NOT_STARTED |  |  |  |  |  | Implement conversation/graph/workspace rewind |
| P10-025 | P10 | NOT_STARTED |  |  |  |  |  | Implement non-destructive fork from checkpoint |
| P10-026 | P10 | NOT_STARTED |  |  |  |  |  | Implement headless rapid exec/run |
| P10-027 | P10 | NOT_STARTED |  |  |  |  |  | Implement rapid inspect/export |
| P10-028 | P10 | NOT_STARTED |  |  |  |  |  | Implement rapid doctor diagnostics |
| P10-029 | P10 | NOT_STARTED |  |  |  |  |  | Implement shell completion/man/help generation |
| P10-030 | P10 | NOT_STARTED |  |  |  |  |  | Benchmark startup/10k-event replay/render |
| P10-GATE | P10 | NOT_STARTED |  |  |  |  |  | Pass CLI, TUI, Daemon, Inspectors & Time Travel phase gate |
| P11-001 | P11 | NOT_STARTED |  |  |  |  |  | Define eval Scenario/Fixture schemas |
| P11-002 | P11 | NOT_STARTED |  |  |  |  |  | Implement FixtureManager CAS repos/services |
| P11-003 | P11 | NOT_STARTED |  |  |  |  |  | Implement ScriptedModel |
| P11-004 | P11 | NOT_STARTED |  |  |  |  |  | Implement ReplayProvider |
| P11-005 | P11 | NOT_STARTED |  |  |  |  |  | Implement LiveProvider experiment pinning |
| P11-006 | P11 | NOT_STARTED |  |  |  |  |  | Implement production KernelRunner modes |
| P11-007 | P11 | NOT_STARTED |  |  |  |  |  | Implement FaultInjector framework |
| P11-008 | P11 | NOT_STARTED |  |  |  |  |  | Implement graph assertions |
| P11-009 | P11 | NOT_STARTED |  |  |  |  |  | Implement file/workspace assertions |
| P11-010 | P11 | NOT_STARTED |  |  |  |  |  | Implement policy/capability assertions |
| P11-011 | P11 | NOT_STARTED |  |  |  |  |  | Implement process/resource assertions |
| P11-012 | P11 | NOT_STARTED |  |  |  |  |  | Implement context/evidence assertions |
| P11-013 | P11 | NOT_STARTED |  |  |  |  |  | Implement browser/computer assertions |
| P11-014 | P11 | NOT_STARTED |  |  |  |  |  | Implement deterministic grader registry |
| P11-015 | P11 | NOT_STARTED |  |  |  |  |  | Implement optional rubric JudgeAdapter |
| P11-016 | P11 | NOT_STARTED |  |  |  |  |  | Implement MetricCollector registry |
| P11-017 | P11 | NOT_STARTED |  |  |  |  |  | Implement FailureBundler |
| P11-018 | P11 | NOT_STARTED |  |  |  |  |  | Implement TrajectoryCollector |
| P11-019 | P11 | NOT_STARTED |  |  |  |  |  | Implement trajectory privacy classification |
| P11-020 | P11 | NOT_STARTED |  |  |  |  |  | Implement CandidateRanker |
| P11-021 | P11 | NOT_STARTED |  |  |  |  |  | Implement ExperimentRegistry |
| P11-022 | P11 | NOT_STARTED |  |  |  |  |  | Define FeedbackEvent accept/reject/edit |
| P11-023 | P11 | NOT_STARTED |  |  |  |  |  | Implement PreferenceCandidate extraction |
| P11-024 | P11 | NOT_STARTED |  |  |  |  |  | Implement preference confidence/scope/decay |
| P11-025 | P11 | NOT_STARTED |  |  |  |  |  | Implement preference conflict handling |
| P11-026 | P11 | NOT_STARTED |  |  |  |  |  | Implement Session Insights analyzers |
| P11-027 | P11 | NOT_STARTED |  |  |  |  |  | Implement held-out promotion gates |
| P11-028 | P11 | NOT_STARTED |  |  |  |  |  | Implement 1h/4h/12h/24h endurance harness |
| P11-029 | P11 | NOT_STARTED |  |  |  |  |  | Implement >=1000-tool-call release scenario |
| P11-030 | P11 | NOT_STARTED |  |  |  |  |  | Implement verified-success-per-token benchmark |
| P11-GATE | P11 | NOT_STARTED |  |  |  |  |  | Pass Eval Harness, Trajectories, Preferences & Optimization phase gate |
| P12-001 | P12 | NOT_STARTED |  |  |  |  |  | Create V3 STRIDE/data-flow threat model |
| P12-002 | P12 | NOT_STARTED |  |  |  |  |  | Implement project trust gating |
| P12-003 | P12 | NOT_STARTED |  |  |  |  |  | Implement symlink/realpath path escape tests |
| P12-004 | P12 | NOT_STARTED |  |  |  |  |  | Implement prompt-injection trust tagging |
| P12-005 | P12 | NOT_STARTED |  |  |  |  |  | Implement MCP/plugin/hook privilege-escalation tests |
| P12-006 | P12 | NOT_STARTED |  |  |  |  |  | Implement secret redaction/exfiltration tests |
| P12-007 | P12 | NOT_STARTED |  |  |  |  |  | Implement egress policy/receipt tests |
| P12-008 | P12 | NOT_STARTED |  |  |  |  |  | Implement approval bypass negative tests |
| P12-009 | P12 | NOT_STARTED |  |  |  |  |  | Implement dont-ask no-hang tests |
| P12-010 | P12 | NOT_STARTED |  |  |  |  |  | Implement stale CapabilityLease tests |
| P12-011 | P12 | NOT_STARTED |  |  |  |  |  | Implement stale Observation coordinate tests |
| P12-012 | P12 | NOT_STARTED |  |  |  |  |  | Implement stale ReadObservation write tests |
| P12-013 | P12 | NOT_STARTED |  |  |  |  |  | Implement uncertain-effect double-send tests |
| P12-014 | P12 | NOT_STARTED |  |  |  |  |  | Implement sandbox downgrade refusal tests |
| P12-015 | P12 | NOT_STARTED |  |  |  |  |  | Implement resource pool cross-tenant contamination tests |
| P12-016 | P12 | NOT_STARTED |  |  |  |  |  | Implement credential cache identity isolation tests |
| P12-017 | P12 | NOT_STARTED |  |  |  |  |  | Implement remote worker forged-result tests |
| P12-018 | P12 | NOT_STARTED |  |  |  |  |  | Implement handoff split-brain partition tests |
| P12-019 | P12 | NOT_STARTED |  |  |  |  |  | Implement plugin WASM resource exhaustion tests |
| P12-020 | P12 | NOT_STARTED |  |  |  |  |  | Implement process tree escape/cancellation tests |
| P12-021 | P12 | NOT_STARTED |  |  |  |  |  | Implement archive/path traversal tests |
| P12-022 | P12 | NOT_STARTED |  |  |  |  |  | Implement dependency/lockfile integrity scans |
| P12-023 | P12 | NOT_STARTED |  |  |  |  |  | Implement secret/dependency/SAST scanner nodes |
| P12-024 | P12 | NOT_STARTED |  |  |  |  |  | Implement fuzzers for graph/event/tool decoders |
| P12-025 | P12 | NOT_STARTED |  |  |  |  |  | Implement persistence corruption recovery tests |
| P12-026 | P12 | NOT_STARTED |  |  |  |  |  | Run full adversarial security release gate |
| P12-GATE | P12 | NOT_STARTED |  |  |  |  |  | Pass Security, Reliability & Adversarial Hardening phase gate |
| P13-001 | P13 | NOT_STARTED |  |  |  |  |  | Audit/lock Rust toolchain and Cargo workspace |
| P13-002 | P13 | NOT_STARTED |  |  |  |  |  | Audit/lock Node/TypeScript SDK toolchain |
| P13-003 | P13 | NOT_STARTED |  |  |  |  |  | Implement targeted CI crate/package matrix |
| P13-004 | P13 | NOT_STARTED |  |  |  |  |  | Implement format/lint/static analysis gates |
| P13-005 | P13 | NOT_STARTED |  |  |  |  |  | Implement unit/integration/e2e test matrix |
| P13-006 | P13 | NOT_STARTED |  |  |  |  |  | Implement deterministic replay CI gate |
| P13-007 | P13 | NOT_STARTED |  |  |  |  |  | Implement token/performance regression gates |
| P13-008 | P13 | NOT_STARTED |  |  |  |  |  | Implement cross-platform macOS/Linux/Windows builds |
| P13-009 | P13 | NOT_STARTED |  |  |  |  |  | Implement optional remote mac/iOS test lane |
| P13-010 | P13 | NOT_STARTED |  |  |  |  |  | Generate SBOM and dependency/license reports |
| P13-011 | P13 | NOT_STARTED |  |  |  |  |  | Implement artifact signing/provenance attestations |
| P13-012 | P13 | NOT_STARTED |  |  |  |  |  | Implement release manifest/update channels |
| P13-013 | P13 | NOT_STARTED |  |  |  |  |  | Implement updater signature/digest verification |
| P13-014 | P13 | NOT_STARTED |  |  |  |  |  | Implement rollback/failed-update recovery |
| P13-015 | P13 | NOT_STARTED |  |  |  |  |  | Implement migration of V2 Event/Goal/Workspace data |
| P13-016 | P13 | NOT_STARTED |  |  |  |  |  | Implement migration compatibility fixtures |
| P13-017 | P13 | NOT_STARTED |  |  |  |  |  | Implement CLI/API schema compatibility fixtures |
| P13-018 | P13 | NOT_STARTED |  |  |  |  |  | Write operator incident-recovery runbook |
| P13-019 | P13 | NOT_STARTED |  |  |  |  |  | Write observability/SLO runbook |
| P13-020 | P13 | NOT_STARTED |  |  |  |  |  | Write complete user CLI/TUI reference |
| P13-021 | P13 | NOT_STARTED |  |  |  |  |  | Write extension/SDK developer guide |
| P13-022 | P13 | NOT_STARTED |  |  |  |  |  | Run requirements traceability audit |
| P13-023 | P13 | NOT_STARTED |  |  |  |  |  | Run clean-install/upgrade/rollback acceptance |
| P13-024 | P13 | NOT_STARTED |  |  |  |  |  | Execute GA release gate and archive evidence |
| P13-GATE | P13 | NOT_STARTED |  |  |  |  |  | Pass Build, Release, Documentation & GA Migration phase gate |
