# SEAMS-RAPIDLM-01 — session prompt

Paste the block below as the first message of the next session (or as the `/loop` goal). It is
self-contained: it names the governing document, the order of work, the operating rules and the
stop conditions. Re-use it verbatim on every later session of this goal; the worklist and the
delivery records carry the state, not the prompt.

Governing document: [`seams-governing-principles.md`](seams-governing-principles.md).

---

```text
You are continuing work in the RapidLM CLI repository (a Rust workspace: `apps/rapid` is the
composition root, `crates/*` own the domain logic). Your standing goal for this session is
SEAMS-RAPIDLM-01 — "Extension seams, background work, agent control and operability".

READ FIRST, IN THIS ORDER, IN FULL
1. docs/goals/seams-governing-principles.md — the governing document. Its §4 catalog is the
   scope, its acceptance criteria are the definition of done, its §2 naming rules and §3
   invariants are absolute, its §5 protocol is how you work.
2. 00-README.md — the fifteen core invariants and the normative hierarchy.
3. agents.md and skills.md, then agents/*.md for the profiles §5.5 assigns.
4. newtask.md §0a (the meta-finding: mature crates exist but are not wired into apps/rapid —
   wire before you build), and the most recent docs/goal-delivery-*.md for the current shape
   of a delivery record.
5. docs/goals/gvs5h-phase0-baseline-2026-09-17.md and docs/goals/gvs5h-implementation-tasks.json
   — the exact shapes your Phase 0 outputs must take.

ABSOLUTE RULES (from the governing document)
- Never name a peer coding-agent product, CLI, IDE or a vendor's agent product — not in code,
  identifiers, comments, tests, fixtures, docs, ADRs, worklist entries, delivery records or
  commit messages. Say "peer tools", "external hosts". Describe features by what they do.
  Upstream model providers may be named only inside the provider modules and the preset table.
  When you edit a file that already contains such a name, remove it in that change; do not
  purge files you are not otherwise editing.
- Clean room: adapt designs, never code, prompts, fixtures or tests from any external project.
- Every new behaviour is a graph node, a ledger event or a policy gate — never a side channel.
  Nothing prompts on a TTY that could not survive a restart. No silent rewrites of tool input
  or model context. External content is fenced, bounded and provenance-tagged. Policy only
  narrows. Views derive from the ledger. Egress goes through the broker with a receipt and is
  opt-in. Defaults, JSONL and exit codes stay as they are for a user who changes nothing.

PHASE 0 — CHECK THE CODE (do this before any feature code)
a) Confirm the baseline: `git rev-parse HEAD`, `git status --short` (preserve every pre-existing
   local change and untracked file untouched), toolchain from rust-toolchain.toml, the required
   checks and the platform contract from .github/workflows/ci.yml.
b) For every SEAM-01 … SEAM-15 item, establish ground truth from source with file:line — what
   exists, what is reachable from apps/rapid, what is missing — and confirm or correct every
   "today" claim in §4. Where the real remaining work is smaller or differently shaped than
   written, say so; where it is larger, say so. Effort labels come from this audit only.
c) Write docs/goals/seams-phase0-baseline-<today>.md in the shape of the GVS baseline audit.
d) Write the ADR(s) the contract changes need (at minimum: hook result v2, inbox edges and
   delivery modes, plan proposals) at the next free number under docs/adrs/.
e) Write docs/goals/seams-implementation-tasks.json with the same schema as the GVS worklist:
   one or more tasks per item (ids SEAM-<item>-<n>), explicit depends_on, acceptance_criteria
   referencing the governing document's AC ids, owners, implementation, validation, evidence.
   Tier 1 first; within a tier, catalog order unless the audit justifies a reorder.
f) Add the program's entries to docs/development-ledger.md.
g) Commit (message names the audit), push to origin/main, then start PHASE 1 in this session.

PHASE 1+ — PLAN, EXECUTE, IMPLEMENT (repeat until the worklist has no non-terminal task)
1. Pick the next ready task by dependency and priority. Restate its contract: affected modules
   and schemas, migration impact, the named acceptance evidence. Spend the first minutes
   grepping for the primitive before writing new code.
2. Implement the smallest valid slice through existing seams. No second scheduler, registry,
   store, policy engine or context authority. New schemas are versioned with a fixture under
   crates/protocol.
3. Tests for happy, negative, boundary, cancellation and recovery cases at system boundaries;
   typed-unavailability tests for paths a platform cannot run (Windows is a test gate).
4. Revert-cycle every fix and acceptance test: reverted → fails for the stated reason;
   applied → passes. Never weaken an assertion. Never run two cargo suites concurrently.
5. Run the required checks serially and green:
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets --locked -- -D warnings
   cargo test --workspace --locked --no-fail-fast
   cargo test --locked -p protocol --test schema_fixtures
   pnpm generate:check && pnpm typecheck && pnpm test
6. Record the slice in docs/goal-delivery-<today>.md as a criterion table (criterion / status /
   evidence with test names); update the task's status and evidence in the worklist and the
   development ledger in the same commit.
7. Commit with a conventional prefix and a body naming the SEAM task and the AC ids it
   satisfies; push to origin/main.
8. Launch a background adversarial self-review of that commit; its confirmed findings become
   the next task before any new feature work.
9. If a task needs a product decision (a default, a name, a scope question), record "decision
   needed" with options and a recommendation in the delivery record and move to the next ready
   task. Never widen policy, change a default or name a product to get unblocked.

REPORTING AND STOPPING
- In a long autonomous run, write a user-facing status at least hourly: done / in progress /
  blocked / decisions needed. Silence reads as "stuck".
- Most turns in an autonomous loop are system wake-ups, not user messages; never treat them
  as approval or new direction.
- Stop only when the worklist has no non-terminal task, or when every remaining task is
  blocked on a recorded decision — then say exactly which decisions are needed.
```
