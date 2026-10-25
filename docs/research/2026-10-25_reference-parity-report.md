# RapidLM comparison with OpenCode v2 and Kimi Code

Analysis date: 2026-10-25. RapidLM baseline: `569317e`.

**The overall parity goal is still in progress.** The current Rust implementation already
provides much of the shared coding-agent surface. The important remaining differences
include native provider protocols and authentication, media handling, safe edit semantics,
plugin installation, and browser/remote product surfaces. This report records actual
dispatch paths and their limits, rather than treating a module or a README entry as proof
that a feature works. Implementation status and acceptance checks live in
[the worklist](../goals/reference-parity-2026-10-25.json).

## Scope and reproducible evidence

The user's request authorizes analysis of the two public repositories and implementation
in this checkout. The references were cloned into temporary directories; their code was
not executed, installed, or copied into RapidLM. Both repositories identify their root
license as MIT; the inventory records license-file hashes. The changes here implement
behaviors through RapidLM's own authorities and contracts.

| Evidence | Exact source | What it establishes |
| --- | --- | --- |
| E-01 | [agustif/opencode-v2 at `77bba6a`](https://github.com/agustif/opencode-v2/tree/77bba6a46ac6aeb98362a55d017c8f83ba6f22eb) | The requested fork, whose latest commit is 2026-01-02; it must not be silently replaced with current upstream OpenCode |
| E-02 | [MoonshotAI/kimi-code at `743f140`](https://github.com/MoonshotAI/kimi-code/tree/743f140978bddaf8b6aa524765fcd578f7c22fc9) | The TypeScript Kimi Code tree, with a 2026-10-25 changelog update for 2.3.2; the older Python `kimi-cli` is a different reference |
| E-03 | [inventory.json](reference-parity-2026-10-25/inventory.json) | Pinned revisions, file inventory, source-line counts including tests/generated code, and license hashes |
| E-04 | RapidLM `apps/rapid/src/interactive.rs`, `exec_tools.rs`, `model.rs`, `user_config.rs`; workspace crates | Real command, tool, provider, policy, context and persistence paths inspected in this checkout |
| E-05 | [validation.json](reference-parity-2026-10-25/validation.json) | Baseline and implementation validation, including command identity and output digest |

`AGENTS.md` and `agents.md` are the same inode on this filesystem and contain only a
link back to themselves. There is no second governance document to read. The existing
working-tree modification to `.rapidlm/goal.json` and unrelated untracked assessment/eval
files were preserved. The Homebrew compiler fails to load an LLVM symbol; validation
uses the already-installed rustup Rust 1.97.1 instead. No system installation was changed.
An independent commit, `eeed559`, landed during this run. It includes Unix-only test-helper
gates and captured the first media/content edits from this work. Those edits and the
independent fixes were retained. E-03/E-05 distinguish the initial baseline from the later
shared working tree; the baseline run is local workspace evidence, not an atomic clean
reproduction of the later commit.

The source review follows entry points through assembly, dispatch, transport and state
storage in each project. File counts are an inventory, not a quality score. Upstream
README claims, upstream test filenames and source inspection do not establish a successful
live comparison. No same-model performance or task-success superiority is claimed.

## How the three systems work

### OpenCode: a server and terminal client around a shared session loop

The fork's `packages/opencode/src/session/prompt.ts` assembles prompts and tools;
`session/processor.ts` consumes model events, records tool states, snapshots changes,
detects repeated calls and coordinates retries. `agent/agent.ts` assembles primary and
subagent profiles with per-agent model, tool and permission overlays. The build/plan split
changes the available actions and approval behavior, rather than creating a second agent
runtime. `tool/registry.ts` is the authoritative source of the model-visible built-ins and
custom tools.

Configuration and provider selection live under `config/` and `provider/`; the provider
layer integrates multiple provider SDKs. `mcp/index.ts` connects local/remote servers and
manages OAuth. `lsp/` and `format/` attach feedback to file operations. The HTTP server,
PTY endpoint, ACP command, SDK, app and desktop packages are different clients of this
product. Git snapshots, session revert, export/import and sharing provide substantial
session portability.

An important reachability correction: this snapshot contains `tool/lsp-hover.ts`,
`tool/lsp-diagnostics.ts` and `tool/multiedit.ts`, but `tool/registry.ts` does **not** include
them in its returned built-in list. They cannot be counted as default model tools merely
because the files exist. Its code-search integration is conditional on provider/feature
configuration. RapidLM should adopt useful capabilities without inheriting those claims.

Useful source paths: `packages/opencode/src/{session,agent,tool,provider,mcp,lsp,format,
permission,server,snapshot,share,acp}` and `packages/{app,desktop,sdk}` in E-01.

### Kimi Code: a modular core with terminal, server and protocol hosts

`packages/agent-core-v2` owns the current core. Runtime interfaces and local/fake hosts,
session services and policy gates, agent lifecycle, model/protocol adapters and feature
assembly are separate modules. The `human/` subtree is part of this implementation;
inspection restricted to `tool/` would miss much of its actual tool and protocol behavior.

The protocol registry routes Chat Completions, Responses, Anthropic, Google GenAI and
Vertex variants. Provider connection, catalog and credential logic remain separate from
the model conversation. Persistence includes append logs, atomic documents, blob stores,
query storage and session locks. The session layer has subagent identities, continuation,
usage, context and token counting. Features add tools such as goals, reminders and cron.

The current tool documentation describes complete-line reads with same-line continuation,
read-before-write checks, regex/path/glob search, image/video input, background commands,
focused subagents and an item-based swarm. The app adds terminal interaction, plugins,
configuration, provider management and browser-hosted sessions. Remote control adds a
relay and account authentication; it is not equivalent to an unauthenticated HTTP bind.

Useful source paths in E-02: `packages/agent-core-v2/src/{agent,session,runtime,persistence,
features,llm-adapter,human}`, `packages/{acp-server,kap-server,node-sdk,remote-control}`,
`apps/kimi-code/src/{cli,tui}` and `docs/en/`.

### RapidLM: retain the kernel and expose working capabilities through it

`apps/rapid` composes the Rust crates. Interactive, headless, daemon and ACP routes use
the same live model/tool composition. The kernel, event ledger, scheduler, workspace,
capability broker, context engine and agent runtime already own their respective state.
The TypeScript SDK is a client. New features should extend these boundaries; replacing
them with an upstream session engine would create conflicting authorities.

The older `gaps.md` and `gap_analysis.md` describe earlier revisions. Their assertions that
interactive prompts are discarded, trust cannot be granted, the context window is always
8,192 tokens, only stdio MCP exists, or the SDK has no daemon cannot be carried forward
without rechecking. Current source, tests, command references and prior delivery manifests
show substantial remediation. Conversely, the existence of an adapter in `llm-router`
did not prove it could be chosen by a real RapidLM configuration: Responses was one such
unreachable path.

## Capability and gap matrix

“Present” means a source-backed production path exists; it is not an upstream live-test
claim. “Implemented here” names work verified by the new focused checks. “Partial” and
“open” remain work, even where shell commands or MCP can provide a workaround.

| Capability | Reference evidence | RapidLM disposition and owning path |
| --- | --- | --- |
| Interactive model turns | Both session runtimes | Present: `interactive.rs`, `host.rs`, `model.rs` |
| Headless text, streaming protocol records and structured output | Both CLI run/print surfaces | Present: headless JSONL and `structured_output.rs` |
| Session resume, fork and workspace undo | OpenCode sessions/snapshots; Kimi session services | Present: kernel ledger, `turn_undo.rs`, `resume_recap.rs` |
| Tool batches and per-call results | Both runtimes | Present: agent-runtime turn loop and `batch_dispatch` |
| Complete model tool payloads | Both tool-result paths | Implemented here: successful MCP/web/PDF and language-server feedback no longer uses the 256-byte display-detail cut |
| PNG/JPEG/GIF/WebP tool reads | Kimi media; OpenCode reads | Implemented here: shared byte-signature detection, complete base64, 3 MiB refusal, model vision gate |
| Paginated reads of long lines | Kimi `Read` | Implemented here: whole lines or same-line UTF-8 continuation; tail offsets and configurable byte budget |
| UTF-16 and explicitly lossy text decoding | Kimi `Read` | Open: current model reads decode with UTF-8 replacement and have no UTF-16/BOM policy or page-level loss warning |
| Regex, case-insensitive and scoped lexical search | Both search tools | Implemented here through the existing Context Fabric engine; ignore files, every matching line, stable page order and byte-cap continuation |
| Search context, modes and very large output spooling | Kimi `Grep` and `Glob` | Partial: current search remains bounded content output; context/count/files modes and readback artifacts remain open |
| File overwrite based on a fresh read | Both file-time/read guards | Open: serialize writes and check paths today; no general session read fingerprint guard on existing-file replacement |
| Atomic multi-file or multi-edit tool operation | OpenCode patch support; Kimi edit workflows | Partial: transactional workspace primitives exist; no model-visible atomic multi-edit surface |
| Process supervision, cancellation, background jobs and monitors | Both process runtimes | Present: `exec_tools.rs`, process-supervisor, `job_recovery.rs`, loops |
| Main-agent and subagent profiles | Both agent registries | Present: `agent_defs`, `primary_agent.rs`, `agent_types.rs` |
| Continue/message/wait for subagents | Kimi subagent lifecycle | Present: registry/inbox/session lineage; bounded admission |
| Item-template swarm operation | Kimi `AgentSwarm` | Partial: parallel primitives exist; equivalent validated item fan-out/resume aggregation is open |
| Plans and approval-driven continuation | Both | Present: plan proposals, graph compilation, durable approval waits |
| Questions with several fields and multiple selections | Kimi `AskUserQuestion` | Partial: `ask_user` exists; multi-question/multi-select form semantics remain open |
| Host-owned goals, budgets and evidence | Kimi goal feature | Present: goal host and evidence-gated completion; independent-verification expansion has its own existing GVS worklist |
| Automatic and manual compaction | Both context layers | Present: Context Fabric and compaction routes; empirical retention quality remains unmeasured |
| Search/code graph and focused context | Both retrieval paths | Present: context-engine FTS/graph/scout production retrieval |
| Skills, instructions and custom commands | Both | Present: skills/rules loaders, `skills_runtime.rs`, `custom_commands.rs` |
| Lifecycle hooks and tool gates | Kimi hook features; OpenCode plugins | Present: hooks v2, observer events and managed hook limits |
| MCP local/HTTP, OAuth and token renewal | Both | Present: MCP registry and auth/refresh paths; new content-payload check added here |
| Deferred MCP tool discovery/loading | Kimi tool selection and MCP infrastructure | Present: `mcp_deferred.rs` with bounded schema projection |
| MCP input elicitation | Current Kimi MCP core | Present with documented transport limits; no claim that every MCP capability is model-facing |
| ACP editor integration | Both ACP packages | Present: `acp_serve.rs` and ACP compatibility tests |
| Native Chat Completions and Anthropic | Both protocol layers | Present: provider adapters and setup profiles |
| Native Responses selection and basic replay | Kimi Responses protocol | Core path implemented here: `openai-responses` profile/preset, JSON output, argument reconciliation, image tool results and effort encoding; bounded encrypted reasoning now survives tool turns and durable approvals |
| Provider caching and reasoning-specific wire state | Both provider transformations; Kimi protocol implementations | Implemented here: explicit Anthropic adaptive/budgeted thinking, automatic ephemeral cache breakpoints, and private typed Responses/Anthropic replay state with restart/export isolation |
| Incremental HTTP text delivery | Both | Implemented here: boxed transport forwarding and shared chunked/UTF-8 decoder; callback-before-completion regression |
| Native Google GenAI and Vertex | Kimi protocol registry; OpenCode provider SDKs | Native Google GenAI implemented with key-header auth, signed part replay and configured-binary checks. Vertex project/location/credential routing remains open (REF-009). |
| Additional provider-specific SDK/auth protocols | OpenCode provider layer | Open: inventory and implement explicit protocols; keep endpoint-specific auth at the wire boundary |
| Provider account OAuth and refresh | Both authentication products | Partial: API keys/keychain and MCP OAuth exist; they are not provider-account OAuth |
| Catalog import/refresh/model migration guidance | Kimi model catalog/provider manager | Open: current presets/configured profiles are not an imported, refreshable provider registry |
| Native video input | Kimi media upload | Open: canonical model content currently has text and image parts, not video/upload handles |
| Image crops, adaptive compression and full-resolution controls | Kimi media tool | Open: complete bounded images now work; these transformations require their own deterministic media path |
| Plugin package install/update/remove/marketplace | Both plugin ecosystems | Partial: manifests, trust and WASM primitives exist; package lifecycle and model tool integration require work |
| Full browser chat, file/diff and approval UI | Both web/app surfaces | Open: daemon RPC/dashboard is not a full browser product |
| Authenticated remote control and reconnect | Kimi relay; OpenCode client/server | Open: local IPC and loopback service are not a remote relay, identity or takeover protocol |
| Session import and share/revoke | OpenCode import/export/share; Kimi exports | Partial: export exists; validated import and explicitly published, revocable sharing remain open |
| GitHub issue/PR workflow commands | OpenCode CLI GitHub/PR commands | Partial: shell can operate GitHub; a configured integration with its own CLI and CI workflow remains open |
| Themes and input customization | Both terminal apps | Present for configured themes/keys; custom theme authoring and accessibility comparison need direct UX review |
| Distribution and updates | Both installers/bundles | Present as installer/release/updater code; actual release-channel and clean-machine verification is a separate open gate |
| Cost, usage and telemetry | Both | Present with unknown costs represented explicitly; no same-model cost advantage demonstrated |
| Cross-platform behavior and performance | Both distributions | Open empirical gate: local macOS validation is not Linux/Windows/runtime/startup evidence |

## Findings and implementation paths

| Finding | Evidence → finding | Concrete path |
| --- | --- | --- |
| F-01: ordinary tool-read images were corrupt | E-04: `execute_read` encoded the image then called `bounded_detail` (256 bytes) | P-01: recognize formats with the existing file-reference detector → enforce the image bound before encoding → keep a complete data URL → gate image parts on the model's vision flag |
| F-02: a read could permanently skip a long-line suffix | E-04: `execute_repo_read` counted a byte-cut partial line as delivered and suggested the next line | P-02: choose complete lines → fragment only a line too large to fit → return same-line byte continuation → reconstruct Unicode content in a regression test |
| F-03: model content used a UI detail bound | E-04: MCP, fetch, PDF and write-feedback success branches used `bounded_detail` | P-03: retain a bounded content payload → redact before cutting → mark external content as untrusted → neutralize fake image/fence markers |
| F-04: Responses was implemented but unselectable | E-04: `ConfigProvider` had two variants and model construction always chose Chat Completions | P-04: add an explicit protocol value/preset → reuse endpoint/auth/egress paths → repair dormant encoding/parsing → drive the actual binary against a fixture |
| F-05: the model's search bypassed richer existing retrieval | E-04: direct tool used a separate literal walker and returned one line per file | P-05: expose the Context Fabric grep engine with a narrower path filter → keep ignore rules and cancellation there → sort and paginate actual delivered hits |
| F-06: live streaming disappeared at type erasure | E-04: `Box<dyn HttpTransport>` forwarded only `execute` | P-06: forward the streaming method → reuse the bounded HTTP decoder → retain split UTF-8 bytes and decode HTTP chunks → prove the callback runs before the server sends completion |

These paths preserve one context authority, one credential boundary, one policy lattice and
one event/approval authority. They add no provider credential to prompts or command lines.

## Validation and delivery gates

The clean post-change workspace run passed **5,593 tests, 0 failed, 4 ignored** across 93
test-result groups. Final source hashes were rechecked against the tested snapshot.
Formatting and clippy with warnings denied passed. SDK generation and typechecking
passed, and all 29 SDK tests passed. SDK checks used available Node 26.11.1 rather than
the project pin of 24.19.0; pinned-Node release validation remains open.

Baseline command: `cargo +1.97.1 test --workspace --locked --no-fail-fast`, using the rustup
binary at `/Users/mohsin/.cargo/bin/cargo`. **5,580 passed, 0 failed, 4 ignored** across 93
test-result groups. The ignored tests remain unverified. Focused implementation checks,
formatting, clippy and SDK checks are recorded separately in E-05. The first post-change
attempt had two failures, both corrected before the clean run above.

The new checks cover image byte-for-byte round trips for four formats and oversize refusal;
Unicode read reassembly; whole-line and tail reads; regex/case/path/glob/ignore behavior;
search page reconstruction; useful fetched/MCP text beyond byte 256; the tenth language-server diagnostic; Responses JSON and
stream snapshots; image tool-result encoding; actual binary protocol selection; and a
boxed HTTP transport receiving chunked, split-UTF-8 text before completion.

The worklist remains open until the remaining capabilities have production wiring,
focused behavioral evidence, updated references and final repository checks. External
provider credentials, proprietary relays, package signing/publication and other platforms
need explicit evidence when reached; their absence must never be converted into a pass.

For Responses wire semantics, the implementation was checked against the
[official function-calling documentation](https://developers.openai.com/api/docs/guides/function-calling).
Upstream comparison links above pin both requested repositories; subsequent upstream
changes require a new comparison baseline.

## Private provider continuation batch — 2026-10-26

REF-028 has focused acceptance evidence. A bounded typed contract in
`crates/protocol/src/provider_replay.rs` binds opaque provider items to original calls
and a protocol/provider/model/profile/endpoint scope. Responses encrypted reasoning
and Anthropic signed/redacted thinking survive tool batches and approval persistence.
Driver wrappers forward only the current step's state. Restored model instances use
the stable credential alias; switching model/profile/origin omits incompatible state.
Mismatched call bindings fail before execution. Whole-exchange history pruning keeps
state with the calls it belongs to. No raw provider reasoning is rendered as an answer.
Debug and public event/JSONL exports exclude replay contents; internal ledger recovery
retains the exact original. Signed thinking precedes assistant continuation text.

Explicit Anthropic `thinking`/`thinking_budget_tokens` controls require declared
reasoning support. `caching = true` enables automatic ephemeral breakpoints. These
use the existing transport, credential and usage paths. Model names grant no implicit
capabilities. See [thinking](https://platform.claude.com/docs/en/build-with-claude/thinking),
[effort](https://platform.claude.com/docs/en/build-with-claude/effort),
[prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching), and
[Responses reasoning](https://developers.openai.com/api/docs/guides/reasoning).

Core replay checks passed (622 tests), the initial approval/configured-model integration
batch passed (51 tests, 1 ignored), and the final router checks passed (188 tests),
including the continuation-order regression. A fresh native Anthropic model fixture
passed after that correction. SDK generation/typechecking and 29 SDK tests passed.
The workspace run and exact snapshot notes are recorded in E-05. These are offline
protocol fixtures, not credentialed provider quality measurements. Eight worklist tasks
have focused evidence; 21 remain open. Native Google work follows REF-028.

The replay batch workspace regression finished with **5,604 passed, 0 failed, 4 ignored**
across 93 groups (exit 0). Compilation preceded the final provider ordering correction;
the subsequent 188-test router run, fresh Anthropic restart fixture and final clippy
check cover that correction. Final replay source hashes matched the recorded snapshot.
REF-027 remains the final whole-source acceptance gate after the remaining features.

## Native Google GenAI batch — 2026-10-26

REF-008 has focused acceptance evidence. `google-genai` is an explicit provider/profile
and an appended setup preset. Native requests use the validated v1beta
`models/<id>:streamGenerateContent?alt=sse` route and fixed `x-goog-api-key` wire auth.
Google text/system instructions, inline images, function declarations, parallel calls
and paired function responses use their native envelopes. Original optional call ids
remain separate from internal synthetic ids. Bounded private replay captures signed
text/thought/function-call parts and signature-only streaming tails, survives a fresh
model instance and suspension serialization, and stays out of answer text and exports.
Only unsigned text chunks are coalesced; signed boundaries are retained.

The adapter reuses the HTTP transport, proxy/egress gates, byte limits, credential
resolution and stream decoder. Request-only query parsing supports Google's `alt=sse`
without permitting queries in configured base URLs. Caller credential-header overrides
remain forbidden. Refusals, malformed/incomplete calls and unexpected finish conditions
fail before tool execution; usage includes cached, thought and tool-prompt tokens.

The cancellation fixture exposed a shared defect: `ProviderCancelWatch::start` did not
start its watcher; `Drop` spawned it after retiring the router token. The watcher now
runs during the request and is retired on drop. Cancelling a hanging native Google
request returns promptly with a typed cancellation and no continuation metadata.

Native binary checks cover parallel file reads, API-key header placement, byte-framed
streaming before completion with split UTF-8, private-thought omission, fresh-model
replay, images, classified HTTP errors and refusal by the existing dial gate. The broader
regression passed 2,248 rapid library tests, 49 configured-model tests, 193 router tests
and 94 protocol tests; the subsequently added egress check also passed. Three ignored
tests in those targets remain unverified. Formatting, workspace clippy, final added-test
clippy, SDK generation/typechecking and 29 SDK tests passed. See E-05 for log digests and
source hashes. No credentialed Google request or same-model quality comparison was run.

Wire semantics were checked against [generateContent](https://ai.google.dev/api/generate-content),
[thinking](https://ai.google.dev/gemini-api/docs/thinking), and the
[current model list](https://ai.google.dev/gemini-api/docs/models). The public thinking
and function-calling guides now primarily demonstrate Interactions, so their Interactions
step envelopes were not substituted for generateContent's part-based protocol. The
new preset uses the model list's stable `gemini-3.8-flash`; existing defaults are unchanged.
Nine worklist tasks are complete and 20 remain open; Vertex is the next provider task.
