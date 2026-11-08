# EXT — Extension-surface behavior specs (draft)

**Date:** 2026-10-24  
**Baseline commit:** `05ad9d6`  
**Status:** draft behavior specs, **not a program**. No worklist, no ADR and no task has been created from them; no code has been changed. Turning them into a program (a worklist in the `seams-implementation-tasks.json` shape, Phase 0 audit, ADRs for the contract changes) is a decision for the maintainer.  
**Rules applied:** [`seams-governing-principles.md`](../seams-governing-principles.md) §2 (naming, provenance, clean room), §3 (invariants), §5 (delivery protocol) and [`docs/research/adoption-not-copying.md`](../../research/adoption-not-copying.md).

## 1. What this is

Five specs for capabilities that RapidLM's peer tools document publicly and that fit RapidLM's own architecture. Each spec describes **behavior and contracts in RapidLM's terms**: what a user sees, what is recorded, what is bounded, what policy can narrow, and how to tell it works. None prescribes an implementation beyond naming the existing seam to reach.

| ID | Spec | Size | Depends on |
|---|---|---|---|
| [EXT-01](EXT-01-mcp-tool-surface-control.md) | MCP tool-surface control: per-server filters, timeouts, deferred definitions | M | — |
| [EXT-02](EXT-02-mcp-remote-authorization.md) | Remote MCP servers: a working HTTP transport and user-initiated authorization | L | — (slice 0 alone is a bug fix) |
| [EXT-03](EXT-03-hook-stage-extensions.md) | Hook stage extensions: `stop` continuation, approval/notification observers, per-hook matcher and timeout | M | — |
| [EXT-04](EXT-04-skills.md) | Skills: wire the existing loader, add roots, arguments and rules | M | — |
| [EXT-05](EXT-05-agent-definitions-markdown.md) | Agent definitions as Markdown, tool lists, narrowing, step cap, selectable main agent | M–L | EXT-01 slice 1 (for slice 5 only) |

## 2. Provenance and clean room

- The specs were written from **publicly documented behavior** of peer tools (feature level: what a setting does, what a command prints) and from reading RapidLM's own tree. **No upstream source code was read.** No text, identifier set, prompt, fixture or test was copied. Where a standard is involved (the MCP authorization profile, OAuth 2.1 with PKCE, RFC 7591/8414/9728/8707, the open `SKILL.md` format) the standard is the reference, not any product.
- Per SEAMS §2.1 **no peer product is named** in these files, and per §2.5 no table maps a product name to a path (EXT-04 finds skill directories by shape for that reason).
- **Implementers must not consult upstream source for these features.** If a spec is ambiguous, the answer is a recorded decision here or in the delivery record — not a lookup. This keeps SEAMS §2.3 true.
- Both upstream projects are permissively licensed, so nothing here is *legally* constrained; the clean-room posture is the repository's own rule.

## 3. How the specs are grounded

A first survey leaned on older gap analyses and reference pages, and RapidLM's documents turned out to be out of date in several places (e.g. they still say remote MCP is "stdio only"; the SEAMS program has since delivered hook result v2 and file-based agent overlays). So each spec's **Today** section was produced by reading the code at `05ad9d6` and then **spot-checking** the load-bearing claims by hand (marked *spot-checked*). A separate read-only audit then checked about 230 cited claims against the tree: about 218 were correct, and every wrong, off-by-many-lines or incomplete one it found (a wrong dependency location, a sixth approval request site, a notification payload with no kind token, the stdio-only count, and others) was corrected in the specs. Line numbers will drift; per SEAMS §5.1 a Phase 0 recheck is the first task of any program built from these.

## 4. Defects found while grounding

These are independent of the features. Several block them; all are worth fixing whether or not the specs are adopted. "Live" means reachable in a shipped build today; "latent" means the code is not reached yet.

| # | Defect | Evidence | Where handled |
|---|---|---|---|
| R-1 | `tools/call` over an HTTP MCP server cannot succeed: HTTP `recv_frame` always returns `InvalidFrame`, `send_frame` discards the reply; untested | `crates/mcp/src/transport.rs:862-881`, `exec_tools.rs:6533, 9616` — **live** | EXT-02 slice 0 |
| R-2 | A configured `Authorization` header is rejected by the shared HTTP writer, which also always emits its own `Bearer` header; `docs/gap-to-delivery.md:162` says it works | `openai_compatible.rs:437-442, 2460` — **live** | EXT-02 §3 A4 |
| R-3 | An MCP success is passed through `bounded_detail` (256 bytes); `structuredContent` and images are dropped; output is prefixed but not fenced as untrusted. *Phase 0 must confirm the model-visible effect.* | `exec_tools.rs:220, 6586-6596, 7882`; `transport.rs:1339-1362` — **live (to confirm)** | adjacent to EXT-01; recommend its own fix |
| R-4 | `rapid mcp install` writes remote entries without `type`, which this loader rejects | `mcp_install.rs:84-91` — **live** | EXT-02 §3 A6 |
| R-5 | "Stdio only" is stated in five places (and asserted by two tests) while `type:"http"` is accepted; `/mcp auth` answers "only stdio"; `rapid mcp add` has no way to create a remote entry | see EXT-02 §2 — **live** | EXT-02 §3 A4, A7 |
| R-6 | The per-call MCP watchdog thread is never stopped early | `exec_tools.rs:6468-6482` — **live** | EXT-01 §3.2 |
| R-7 | `docs/reference/hooks.md` disagrees with the code in six places (failure records, exit-0 rule, `hook_specific`, settings files, env variables, "exactly one stop") | EXT-03 §2 — **live (docs)** | EXT-03 slice 5 |
| R-8 | The skills loader is complete and tested but unreachable from the app; the development ledger's "verified" entries for it reportedly rest on library tests (grounding pass; not re-checked) | `crates/plugin-host/src/skills.rs`; no caller under `apps/rapid/src` (spot-checked) — **latent** | EXT-04 slice 0 |
| R-9 | `DELEGATION_MAX_DEPTH` is documented as the single nesting boundary but never called; depth is hard-coded to 1 elsewhere; `USER_DEFS_DIR` is unused and names a different path from the real one | `delegation.rs:15`; `AD:40` | EXT-05 §11 |
| R-10 | `rapid agents list|validate` read an untrusted project's agents directory without a trust check; a symlinked `agents/` directory is followed | `p9_commands.rs:2790-2890`, `agent_defs.rs:682-720` — **live** | EXT-05 slice 1 |
| R-11 | `task_spawn.description` is accepted and dropped | `exec_tools.rs:7998` | EXT-05 §3.3 |
| R-12 | The notify-stage hooks record no `hook.failed` although the docs say they do | `interactive.rs:3385` | EXT-03 D-2 |
| R-13 | `event-catalog.md` lists families that are not event kinds (`mcp.*`, and also `process.*`, `monitor.*`, `trigger.*`, `plugin.*`, `acp.*`); `job.*` kinds exist (`event.rs:212-215`) but the catalog never mentions them | `event-ledger/src/event.rs:168-288`, `event-catalog.md:18` | each spec's contract section |

## 5. Shared work (do once, use in several specs)

1. **A shared closed-subset frontmatter tokenizer** in `crates/protocol` (the one crate both `plugin-host` and `agent-runtime` depend on). Move the skills loader's tokenizer (`SK:1333`) there, add the one nested-map construct EXT-05 needs, keep every existing skill test green. Each consumer keeps its own closed key set — the skills privilege-key denylist stays in the skills schema. EXT-05 slice 1 does the move; EXT-04 slices 0–1 use the loader's existing tokenizer in place and migrate when the shared one lands.
2. **A shared argument expander** (`$ARGUMENTS`, `$1`…`$9`, `$$`, declared names) taking a string, used by custom commands (unchanged behavior) and skills.
3. **A shared `*`/`?` name-pattern matcher** with the bounds in EXT-01 §3.1, used by MCP filters and agent tool lists. (The permission-rule grammar stays exact-name; extending it to globs over tool names is a separate decision.)
4. **Event-kind registration checklist** — `define_event_kinds!`, `wire.v1.json`, SDK regeneration, `WIRE_SCHEMA_SHA256`, event catalog, and `EventFamily`: the enum has `Agent`, `Hook` and `Notification` but no `Mcp` or `Skill` variant (`event.rs:100-127`), so Phase 0 decides once whether to add them or file the kinds under `Tool`/`Agent`. Ten new kinds across the five specs; register them in as few commits as the slices allow.
5. **Doc-consistency discipline.** `rapid skills` and any new subcommand must pass the test that compares the CLI reference with `SUBCOMMANDS`.

## 6. Suggested order

1. **Bug fixes first**, each small and independently valuable: R-1, R-2, R-4 and R-5 (= EXT-02 slice 0), R-6, and the R-3 decision.
2. **EXT-03 slices 1–3** (object form, matcher, timeout, process-group termination, observers): isolated, no new dependencies.
3. **EXT-04 slices 0–1**: wiring an existing, tested loader is the cheapest user-visible win.
4. **EXT-05 slices 1–2**, which move the shared tokenizer (shared work 1) and migrate the skills loader onto it.
5. **EXT-01 slices 1–3.**
6. **EXT-02 slices 1–4** (authorization), after slice 0 has been exercised.
7. **EXT-03 slice 4** (`stop` continuation): the largest change — a completed run must return its history and the host must own cumulative budgets — so it goes last among the hook slices.
8. **EXT-04 slices 2–3, EXT-05 slices 3–5.**

## 7. Decisions needed from the maintainer

Each spec ends with its own table; these are the ones that change what is built, with the recommendation in each spec.

| Spec | Decision | Recommendation |
|---|---|---|
| EXT-01 D-1 | Page `tools/list` for deferred servers | Yes, bounded, as its own AC set |
| EXT-02 D-1 | Credential store unavailable | Refuse; no plaintext fallback |
| EXT-02 D-3 | Authorization as a durable approval wait | Marker tool + separate command first |
| EXT-02 D-5 | Token scope | Per user, keyed by URL + issuer + client id |
| EXT-03 D-1 | Older binaries silently skip object-form hooks | Accept, document, doctor-warn |
| EXT-03 D-2 | Make notify stages record failures as the docs claim | Yes |
| EXT-04 D-1 | Skill precedence (user over project, as the loader has it) | Keep; make shadowing visible |
| EXT-04 D-2 | Peer-product skill directories | None by name; `.agents/skills`, `extra_dirs`, shape-based `discover` |
| EXT-05 D-1 | Frontmatter parser | Share the skills subset |
| EXT-05 D-2 | MCP tools for subagents | Yes, only when a definition names them |
| EXT-05 D-3 | Nested spawning | Keep depth 1 |

## 8. Not specified here

Items from the same survey that were ranked lower and are not covered: an HTTP/SSE daemon with attach, an LSP navigation tool, video input by frame sampling, `rapid pr` and a workflow generator, mid-session model-switch choices and stale-tool-output pruning, shell-mode and sub-session views in the terminal UI, declarative command tools, session export sanitizing/import, goal queues. Also deliberately rejected: hosted share links, a hosted relay for remote control, telemetry and rating prompts, in-process JavaScript plugins, fail-open policy hooks and auto-installing updates. Each can get a spec in this format on request.

## 9. How to adopt

1. Read SEAMS §0–§3 and §5; read each spec's **Today** section against the tree you have.
2. Run a Phase 0 recheck for the specs you take: confirm or correct every cited line, record decisions, open ADRs for the contract changes (suggested: ADR 0025 for EXT-03's `stop` decision, ADR 0026 for EXT-05's definition schema v2, one for EXT-02's authorization storage and pinning).
3. Create worklist tasks named `EXT-<spec>-<slice>` in the SEAMS schema with the spec's acceptance criteria as the task criteria.
4. Deliver slice by slice under §5.2–§5.6, including the revert cycle for every AC and the background self-review of every commit.
