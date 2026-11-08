# RapidLM: evaluation, agent harness, and product parity

Assessment date: 2026-09-15. Reviewed checkout: `7816caa`.

## Verdict

**RapidLM has a substantial agent-runtime foundation, but its evaluations are not strong enough to certify agent quality, and overall parity with Kimi Code, Qwen Code, and Grok Build is not established.** There are also concrete integration gaps that rule out full feature parity today.

This is a source-based review with local tests and targeted probes. Competitor capabilities come from current official documentation; competitor implementations were not exhaustively audited or rerun. The historical live comparison is retained evidence, not a fresh benchmark of this checkout. No application source was changed by this review.

## 1. Evaluation findings, in priority order

### P1 — Graders accept incorrect solutions

The eight test-writing cases invoke `python3 -B test_<module>.py`. They do not establish that tests were discovered, that any assertions ran, or that every requested function was tested. In isolated temporary directories, I seeded each case's original module and wrote an **empty test file**. All eight verification commands exited successfully.

This is a demonstrated grading defect, not evidence that a particular historical agent exploited it. The bugfix cases also run editable test files without checking that the agent respected the prompt's instruction to preserve them. Workflow cases check compilation, rather than asserting the required two-node structure and dependency.

Evidence: [test-writing example](../eval/suite/tests-020.json), [suite generator](../eval/gen-suite.py), [live grading](../apps/rapid/src/eval_serve.rs#L901).

**Required correction:** keep judges outside the editable workspace; validate required outputs and prohibited changes; use test discovery and mutation checks for test-authoring tasks. Add intentionally wrong submissions that the grader must reject.

### P1 — Live runner can manufacture timeouts

`run_live_agent` pipes stdout and stderr, polls `try_wait()` until exit, and only then reads the pipes. A verbose child can fill either pipe and block before exiting. The runner then reports a 600-second timeout. The timeout path kills the immediate child and returns without collecting usage or reaping it there; it does not explicitly clean up the entire process group.

A local subprocess probe reproduced the pipe mechanism: a child writing 2 MiB could not finish while the parent waited without reading; draining the output allowed it to finish immediately. This demonstrates the mechanism; it does **not** establish that either historical workflow timeout was caused by it. Retained traces are insufficient to assign that cause.

Evidence: [runner](../apps/rapid/src/eval_serve.rs#L797), particularly the polling and subsequent read at lines 857–887. The production shell executor already drains concurrently; the evaluation runner does not share that implementation.

**Required correction:** drain both pipes concurrently, retain bounded output while continuing to consume excess, kill and reap the process group on timeout, preserve traces and partial usage.

### P1 — Efficiency reporting excludes failed attempts

`summarize()` filters to passed tasks before summing token and cost usage. Its reported metric is therefore average usage **among successful attempts**, not total evaluation spend divided by verified successes. Missing token measurements also remain in the token denominator.

For the retained equal-window run:

| Metric | RapidLM | Grok |
|---|---:|---:|
| Recorded grader passes | 35/40 | 40/40 |
| Published tokens per success, successful attempts only | 48,614 | 165,643 |
| All reported attempt tokens / successes | at least 49,795 | 165,644 |
| Attempts with token measurements | 38/40 | 40/40 |

RapidLM's two timeout attempts have no usage, so a complete efficiency ratio cannot be reconstructed. Its smaller observed token use is promising, but a precise total-spend advantage is not established. Cost estimates and provider-reported charges must also remain separate.

Evidence: [aggregation](../apps/rapid/src/eval_serve.rs#L932), [raw run](../eval/results/live-1789415241.json).

The separate library collector has a related semantic problem: `VerifiedSuccessPerToken` treats either `turn.completed` or `goal.completed` as verified success, without consulting assertion verdicts. Ordinary terminal output can complete a turn; it is not proof that code is correct. Evidence: [collector](../crates/harness/src/graders.rs#L94), [turn completion](../crates/agent-runtime/src/turn.rs#L1500).

### P1 — Skips can conceal failures and incomplete comparisons

The live preflight executes the first actual task. Any error text containing `non-zero`, `model`, or `provider` can mark the entire agent arm as unavailable. A real tool/agent failure on that task can therefore become 40 skips. Separately, an all-skipped run exits zero because the exit decision checks only for `failed`.

Missing or version-mismatched competitor binaries are announced as “recorded as skipped,” but are omitted from the report's agent list. Only RapidLM and Grok have invocation recipes; Kimi and Qwen are not configured competitors. Model/configuration hashes and complete version provenance are not embedded in each retained result.

Evidence: [recipes](../apps/rapid/src/eval_serve.rs#L497), [selection/preflight](../apps/rapid/src/eval_serve.rs#L531), [exit policy](../apps/rapid/src/eval_serve.rs#L1152).

**Required correction:** use a dedicated provider health probe with typed infrastructure errors; report every requested arm; make incomplete comparisons non-passing; record commit, binary version, model, settings, budgets, suite hash, and environment identity.

### P2 — Forty cases provide much less than forty independent task types

The seven recovery tasks have one identical prompt and the same pipeline repair. The seven workflow tasks share one prompt and differ mostly by names. All eight “multifile” gold solutions edit only `config.py`. Every case in the entire suite has a one-file gold patch.

These are useful smoke tests, but they do not establish repository-scale reasoning, large refactors, sustained autonomous work, multi-agent conflict handling, or recovery after hours of accumulated context. The offline run supplies the gold edits itself, and correctly labels itself mechanical validation rather than model-quality evaluation.

Evidence: [generator](../eval/gen-suite.py), [offline execution](../apps/rapid/src/eval_serve.rs#L340).

For perspective, Qwen's current repository documents an evaluation using 500 SWE-bench Verified cases and three trials per version. Its scores cannot be compared numerically with RapidLM's different tasks and model configuration. [Official Qwen repository](https://github.com/QwenLM/qwen-code).

## 2. Agent harness: substantial strengths, incomplete integration

### Strengths verified in source

- **Bounded execution:** the turn loop has model/tool/token budgets, cancellation, duplicate-call rejection, and loop detection. [Turn runtime](../crates/agent-runtime/src/turn.rs).
- **Recoverable tool errors:** malformed arguments and unknown tools produce handled, model-visible failures rather than always killing the turn. Permissions and hooks are applied on the execution path. [Tool dispatch](../apps/rapid/src/exec_tools.rs#L2098).
- **Host-owned context recovery:** the live host calls the recovery-capable executor and requires a smaller rebuilt packet before retrying. Explicit provider fallback is also implemented. [Host](../apps/rapid/src/host.rs#L635).
- **Real subagent isolation:** write-capable children receive Git worktrees; setup failure refuses delegation. Children inherit narrowed permissions, hooks, and shared resource budgets. [Live child runner](../apps/rapid/src/interactive.rs#L2896), [worktree manager](../apps/rapid/src/agent_views.rs).
- **Durable goals and verification:** goal completion checks evidence; claim verification executes commands and records ledger-backed evidence. [Goal host](../apps/rapid/src/goal_host.rs#L325), [claim verifier](../apps/rapid/src/goal_claim.rs#L585).
- **Meaningful recovery testing:** a test kills a process after durable `tool.started`, restarts against the same database, and requires paused recovery without autonomous replay. [Crash test](../crates/kernel/tests/crash_recovery.rs).
- **Useful product plumbing:** MCP registration, background shell jobs, interactive approval/resume, session continuation, headless output, and a TypeScript SDK have concrete implementations.

### Material limitations

- **Subagents block the parent tool call until their report returns.** Multiple calls can run concurrently within a batch, but this is not a detached, resumable child-session API that lets the parent continue reasoning independently. [Spawn execution](../apps/rapid/src/exec_tools.rs#L3620).
- **Computer use is not connected to interactive agent work.** The interactive dispatcher explicitly reports it unavailable; graph/computer/resource panel routes also have no rendered content. Existing backend modules do not establish delivered end-to-end capability. [Interactive dispatcher](../apps/rapid/src/interactive.rs#L1748), [panel rendering](../crates/tui/src/compositor.rs#L175).
- **ACP mode switching is unsupported.** Both adapter test suites require `METHOD_NOT_FOUND` for `session/set_mode`. The CLI uses the V1 adapter. [V1](../crates/acp/src/v1.rs#L1511), [V2](../crates/acp/src/v2.rs#L891).
- **Other everyday interactive actions are incomplete.** The dispatcher reports mid-session model switching and plugin install/trust management as unwired. [Unsupported actions](../apps/rapid/src/interactive.rs#L1786). Grok documents switching models inside the TUI, and Kimi documents interactive plugin installation; these are concrete product gaps rather than benchmark uncertainties.
- **Advanced mechanisms are not uniformly connected.** The gateway repair implementation has tests, but this review found no production caller of its `repair()` function; live tools instead provide ordinary handled argument errors. Model requests also specify `ModelPurpose::Chat`. Do not count every library primitive as a delivered automatic repair/routing feature. [Repair module](../crates/tool-gateway/src/repair.rs#L197), [request construction](../apps/rapid/src/model.rs#L650).
- **Anthropic streaming remains buffered at the adapter boundary.** It requests SSE, but calls `transport.execute`, then parses the returned whole response body. This is different from progressive delivery during the request. [Adapter](../crates/llm-router/src/providers/anthropic.rs#L166).

The appropriate assessment is **credible runtime engineering with incomplete operational proof**, not a blanket “strong production harness” certification.

## 3. Product parity

| Comparator | Assessment | Evidence and remaining gap |
|---|---|---|
| Grok Build | Partial core overlap; quality parity unproven | Both cover terminal/headless coding, tools, MCP, hooks, plans, and isolated children. Grok documents background subagents while conversation continues; RapidLM's child call waits. The retained run records 35 versus 40 grader passes, subject to the grading and runner defects above. |
| Kimi Code | Core coding overlap; overall parity unproven | Kimi documents parallel isolated subagents, ACP, video input, conversational MCP configuration, and a plugin marketplace. RapidLM has no Kimi evaluation recipe; equivalent end-to-end media/ecosystem workflows were not established. |
| Qwen Code | Overall feature parity not reached; quality comparison unavailable | Qwen documents desktop/web/editor/chat surfaces, multiple SDK languages, agent teams, computer use, and a broader evaluation program. RapidLM is primarily CLI/TUI plus TypeScript SDK, with computer use unconnected in the interactive path. |

Competitor sources: [Grok overview](https://docs.x.ai/build/overview), [Grok background tasks](https://docs.x.ai/build/features/background-tasks), [Kimi repository](https://github.com/moonshotai/kimi-code), [Qwen repository](https://github.com/QwenLM/qwen-code). These establish documented capabilities, not an independent guarantee of their reliability.

RapidLM's graph/evidence/recovery design could differentiate it, but differentiation requires measured task outcomes and working user flows. A single overall parity percentage would be misleading without a weighted acceptance matrix and common trials.

## 4. Recommended order of work

1. Fix graders, output draining/process cleanup, skip policy, accounting, and experiment provenance before rerunning comparisons.
2. Add independent repository tasks, real multi-file edits, hidden verification, test-quality checks, long-context recovery, permissions, and multi-agent merge scenarios. Run repeated trials with matched model/settings/tools/budgets where possible.
3. Add pinned Kimi and Qwen runners. Separate same-model harness experiments from each product's default-model experience.
4. Finish the missing user flows: detached/resumable subagents, ACP modes, progressive provider streaming, and any computer-use capability intended for release.
5. Make CI cover all existing SDK test files and make supported-platform gates reliable. Keep implementation specifications separate from verified delivery status.

## 5. Local validation

The original modified `.rapidlm/goal.json` was outside this review's changes. No new paid-model evaluation was run.

- Empty-test grader probe: **8/8 incorrect empty submissions accepted**.
- Pipe saturation probe: **blocking mechanism reproduced**, then cleared by draining output.
- `pnpm test`: **8 tests passed**. The configured script selects only `generated.test.ts`.
- Explicit run of all four SDK test files: **28 tests passed**. Thus **20 existing tests are omitted by the normal SDK CI command**. [SDK script](../sdk/typescript/package.json#L22).
- `pnpm typecheck`: **passed**.
- `cargo fmt --all -- --check`: **failed**, including evaluation-runner and TUI formatting differences.
- `cargo test --workspace --locked --no-fail-fast`: **build succeeded; partial test execution passed**. Completed groups: ACP 46, ACP compatibility 6, agent pool 17, agent runtime 280 — **349 passed, zero failed**. The full workspace suite was stopped after repeated long executable-startup delays. A process sample during the first delay showed only `_dyld_start`, before test execution; this is not being attributed to an application test failure. Remaining tests, including the real crash/restart test, were not verified in this run.
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: **incomplete**; stopped during dependency checking after the same slow local validation window. Compilation emitted unused/dead-code warnings, but a completed lint verdict is not claimed.

Local command logs: `/tmp/rapidlm-review-tests-20260915.log`, `/tmp/rapidlm-review-format-20260915.log`, `/tmp/rapidlm-review-clippy-20260915.log`, `/tmp/rapidlm-review-sdk-all-20260915.log`. These are temporary audit artifacts, not checked-in benchmark evidence.
