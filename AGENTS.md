# AGENTS.md — RapidLM Development Agent Governance

## 1. Mission

Implement RapidLM exactly from this dossier while preserving security invariants, module boundaries, versioned contracts, and verification evidence. Optimize for correct composable changes, not maximum diff size.

## 2. Required reading order

For every task:

1. read the task entry in `prompts.md`;
2. read every document listed in its **Reference Documents**;
3. read the nearest implementation-repository `AGENTS.md` if present;
4. inspect existing code/tests at the target boundary;
5. implement only the task scope;
6. run the acceptance commands and record evidence.

Do not read unrelated design documents “just in case”; context discipline is part of the product.

## 3. Hard architectural constraints

- Business state lives in kernel/services, never only in TUI components.
- Every durable state mutation has a typed ledger event.
- Every side-effecting action enters Capability Broker and is enforced again by executor/supervisor.
- A lower-trust policy layer never broadens a higher-trust layer.
- Parallel write-capable agents never share a `WorkspaceView`.
- Tool output sent to models is bounded; large output becomes an artifact.
- Project-controlled executable config is inactive until trust is established.
- Secrets are opaque handles outside the Auth/Secret subsystem.
- Top-level active goals restore as paused after process restart.
- Runtime validates goal completion criteria/evidence; model text cannot override it.
- Public API/event/tool schemas change only with a compatibility decision and tests.
- Do not add model-specific behavior to generic modules; use provider/prompt adapters.


## 3.1 V2 managed-autonomy invariants

- Persistent background agents and bounded managed workers are different runtime classes; do not collapse them into one lifecycle.
- Managed children receive a typed `TaskEnvelope`; never clone the raw parent transcript into a worker context.
- Background exploration is read-only by default. Grant writes only through an explicit worker task and policy.
- Capability leases, secret plaintext, live process handles, browser credentials, and control leases MUST NOT be copied through agent messages or execution handoff bundles.
- Every write-capable managed worker owns a unique transactional `WorkspaceView`. Results integrate only through explicit `AgentResultV2`/ChangeSet verification.
- Execution handoff is generation-fenced: at most one `SessionExecutionLease` generation may own side effects. A handoff uncertainty blocks writes until ownership is resolved.
- Human/agent input ownership is exclusive. `ControlLease` transfer pauses the previous controller, records attribution, and requires a fresh observation before agent resume.
- Memory, Knowledge, Skills, Playbooks and Policy are separate concepts. None of Memory/Knowledge/Skills/Playbooks grants capability.
- Production/user trajectories are not training/export data by default. Never require, infer, reconstruct, log, or persist hidden chain-of-thought. Store observable boundary events, decisions/results, metrics and evidence only.
- Computer Use follows `observe -> resolve target -> authorize -> act -> verify`. Prefer semantic targets; raw coordinates are observation-bound fallback and stale actions are rejected.
- UI/browser/page text is untrusted data and never authority for permission changes. SecretHandle-backed UI entry resolves plaintext only at the final executor and masked regions are excluded/redacted from evidence.

## 4. Repository dependency direction

`protocol` has no RapidLM crate dependencies. High-level crates depend inward on contracts, not vice versa.

```text
protocol
  ↑
event-ledger / auth / telemetry / vcs primitives
  ↑
workspace / process-supervisor / policy / sandbox / context / llm-router
  ↑
tool-gateway / agent-runtime / browser / mobile / mcp / plugin-host
  ↑
kernel
  ↑
tui / acp / headless / app composition
```

Cycles are forbidden. Shared types move to `protocol`, but only if they are truly stable cross-module concepts.

## 5. Rust rules

- Rust 1.97.1 baseline until an explicit toolchain bump task.
- `#![forbid(unsafe_code)]` by default. A crate requiring unsafe must have an ADR and safety module with tests/fuzzing.
- Public errors are typed; no stringly-typed routing.
- Use `CancellationToken`/structured concurrency; never detach untracked tasks.
- Bound channels; document backpressure behavior.
- No `unwrap()`/`expect()` in non-test runtime paths unless invariant is locally proven and message explains it.
- Paths use typed normalized wrappers; do not compare raw strings for security policy.
- Timeouts and output caps are explicit for subprocess/network boundaries.
- Serialization types use explicit schema/version semantics.

## 6. Tests required by change class

| Change | Minimum evidence |
|---|---|
| Pure type/schema | unit + serialization golden |
| Persistence | migration + crash/replay test |
| Policy/security | allow/ask/deny + bypass negative tests |
| Process/sandbox | timeout + cancellation + resource limit tests |
| Context ranking | golden retrieval + token budget regression |
| TUI | reducer/unit + terminal golden/snapshot |
| Provider | mock transport + normalized error/usage tests |
| Agent loop | deterministic scripted-model scenario |
| Browser/mobile | fake adapter unit + one real integration test on supported CI |
| Managed agent mesh | scripted coordinator + isolation + lineage + budget/backpressure tests |
| Handoff/control | generation-fencing + fault injection + takeover/resume reconciliation tests |
| Knowledge/Playbooks | trigger precision + governance + non-escalation + idempotency tests |
| Trajectory/Insights | privacy/redaction + held-out leakage + deterministic metric tests |
| Full Computer Use | semantic target + stale-action + sensitive-UI + evidence/recording tests |
| Public SDK/API | compatibility fixture + generated type check |

## 7. Change discipline

- One prompt task should normally produce one focused commit.
- Do not refactor unrelated code.
- If a referenced contract is insufficient, open/record a design blocker rather than inventing a new incompatible interface.
- New dependency requires rationale: size, license, maintenance, security, alternatives.
- Generated files are never manually edited.
- Keep fixtures free of real credentials/internal identifiers.

## 8. Security workflow

Before finishing a security-sensitive task:

1. identify attacker-controlled inputs;
2. identify the privilege boundary;
3. add at least one negative/bypass test;
4. verify redaction/logging behavior;
5. verify cancellation/timeout/resource caps;
6. record the relevant threat IDs from `threat-model-and-security.md`.

## 9. Agent handoff format

A completed task reports:

```text
Task: RLM-xxx-yyy
Files changed: ...
Contracts changed: none | list
Tests run: command -> result
Security checks: ...
Artifacts/evidence: ...
Known limitations: ...
Follow-up task IDs unblocked: ...
```

Do not include hidden chain-of-thought. Provide decisions, evidence, and unresolved questions only.

## 10. Stop conditions

Stop and mark task blocked if:

- acceptance criteria contradict a higher-precedence document;
- implementation requires broader privileges than specified;
- a public schema must break compatibility without ADR;
- required platform/tool is unavailable and no fake/contract test path exists;
- a test exposes possible sandbox escape, secret leak, or ledger corruption.
