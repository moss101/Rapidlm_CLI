# Model configuration

`rapid` drives a real model when a user config selects one, and fails typed
when it does not. The configuration surface is one small user TOML, env
overrides on top, and per-model tables that pin the provider, model id,
endpoint, and credential.

## Setting it up

`rapid setup` writes this file for you: it plans the change, sends one short
request to the endpoint to verify it (at most 16 output tokens), and only then
writes — atomically, readable by you only, with a copy of the previous file when
it changes and nothing at all when it would not:

```sh
rapid setup --preset <id> --key-env <VAR>            # a key kept in an environment variable
printf '%s' "$KEY" | rapid setup --preset <id> --key-stdin   # a key kept in the OS keychain
rapid setup --base-url http://127.0.0.1:8080/v1 --model local-model   # any compatible endpoint
rapid setup --preset <id> --dry-run --output json   # the plan only: no request, no write
```

`rapid setup --help` lists the presets and every exit code (a failed
verification exits 11–15 by class and changes no file). A key is never accepted
on the command line. `rapid doctor --live` later probes every configured profile
the same way, one row each; plain `rapid doctor` stays offline.

## File locations and precedence

The config file is resolved from the environment in this order:

1. `RAPIDLM_CONFIG` — explicit config file path. If set, the file must exist;
   a missing file is a typed error (never silently ignored).
2. `RAPIDLM_HOME/config.toml` — verbatim home-root override.
3. `$HOME/.rapidlm/config.toml`
4. `$USERPROFILE/.rapidlm/config.toml` (Windows)

If none of these exist, `rapid exec` stays in its typed unconfigured fallback:
it prints where it looked, runs the turn with the unconfigured model, and exits
`5` (the turn failed). It never fabricates a completion.

Within a resolved config, values apply in this precedence:

```
env override (RAPIDLM_MODEL, RAPIDLM_PROXY, credential env vars) > config file > typed fallback
```

## Schema

### `[models]`

| Key | Type | Required | Meaning |
|---|---|---|---|
| `default` | string | one of `default`/`RAPIDLM_MODEL` | Profile id of the model a run uses |
| `fallback` | array of profile ids | no | Models to move to, in order, when the default's provider fails for a class the chain moves on for (up to 7); never inferred from the other tables |

`RAPIDLM_MODEL` overrides `[models].default` without editing the file; a managed
policy's `locked_default` overrides both.

| Key | Type | Meaning |
|---|---|---|
| `catalog_url` | URL | A remote model catalog, fetched as `GET <base>/models?client_version=<version>` when the URL does not already end in `/models`. Entries become `[model.<id>]` rows the user did not define — a row the user *did* define always wins, and credentials are never discovered (`api_key`/`env_key`/`keychain` are ignored on the way in). One fetch per process per ten minutes, through the same audited HTTP path every other request uses, authorized for exactly the catalog's own origin |

Beside it, a local `models.json` next to the resolved config file serves the
same shape without any fetch — and wins over the remote catalog. Both accept
`{"models":[…]}` (the `[model.<id>]` fields), a bare array, or the
OpenAI-compatible `{"data":[{"id":…}]}` shape whose wire ids ride the
catalog's own origin. A catalog that cannot be read is a warning on stderr;
the user's configuration stands exactly as it was.

### `[phases]`

| Key | Type | Meaning |
|---|---|---|
| `compact` | profile id | The model `/compact` summarises with; the default model when absent (in-turn context recovery always uses the turn's own model) |
| `review` | profile id | The model `rapid review` reviews on; the default model when absent |

Other request purposes (`chat`, …) are accepted as keys; a run builds a separate
model only for `compact`. A phase naming a profile with no table is an error.

### `[model.<profile-id>]`

One table per model. `<profile-id>` must be lowercase letters, digits, and
single dashes (it becomes the router profile id and credential handle name).

| Key | Type | Required | Meaning |
|---|---|---|---|
| `provider` | string | yes | `"openai-compatible"` or `"anthropic"` |
| `model` | string | yes | The model id sent on the wire (e.g. `small-model`, `vendor/model:tag`) |
| `base_url` | string | yes | `http://` or `https://` origin, e.g. `http://127.0.0.1:8080/v1` |
| `name` | string | no | Display name |
| `api_key` | string | no | Inline credential; wins over `env_key` |
| `env_key` | string or array | no | Env var name(s); the first set, non-empty value wins |
| `keychain` | string | no | OS keychain alias the key is kept under (`rapid setup --key-stdin` writes `rapidlm-model-<id>-<digest>`, the digest naming this config file and endpoint); read when the model client is built |
| `effort_ids` | table | no | Model id sent at a given reasoning effort, e.g. `{ high = "m-think", low = "m-fast" }` (keys: `none`…`ultra`); the effort after every floor picks it, else `model` |
| `retry` | table | no | This model's step retry policy (below); replaces the built-in one |
| `continue_on_length` | 0–8 | no (default 0) | When an answer ends at its output limit, up to this many follow-on requests hand the answer so far back and ask for the rest; the parts are one message. Each request is a `model.continued` ledger record and counts against the turn's token budget; `rapid exec` says how many follow-on requests a completed answer took, on stderr. A continuation is sent only while the answer can still grow by another part as long as the last within what one message holds (64 KiB); a part that would overflow it, or a reply proposing tool calls, leaves the answer as it was (its request still counts). `0`: the answer ends where the limit cut it |
| `max_tokens` | positive integer | no | Output cap; default `4096` when the provider needs one |
| `context_window` | positive integer | no | Documented context pin; default `32768` |
| `reasoning_effort` | `none`…`ultra` | no | Effort requested (managed and reminder floors may raise it) |
| `vision` | bool | no | The model reads images (default off) |
| `caching` | bool | no | The provider caches prompts (default off) |
| `reasoning` | bool | no | The model exposes reasoning (default off) |

Credential precedence: `api_key` > first set, non-empty
`env_key` entry > `keychain` > keyless. A `keychain` key is read from the OS
keychain (macOS Keychain, Windows Credential Manager, the Secret Service on
Linux) only when the client is built; where no keychain is available, or it
holds nothing under the alias, the model fails to build with an error naming
the alias, before anything is sent. Keyless configs (typical for local servers) send an
empty bearer token. Config keys that are not part of the schema are reported as
stderr warnings (`warning: unknown config key '…'`) and ignored.

### `retry = { … }`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `max_attempts` | 1–11 | 6 | Attempts in all, the first included (`1`: never retry) |
| `base_ms` | 0–600000 | 1000 | Wait before the first retry, doubled for each one after; `RAPIDLM_RETRY_BASE_MS` overrides it |
| `max_ms` | 1–600000 | none | Longest wait; a provider asking (retry-after) for longer ends the retries |
| `on` | array | all four | Classes retried: `rate_limit` (429), `server` (5xx, a dropped stream), `network` (unreachable, connection broke), `rejected` (other 4xx) |

Authentication, quota and proxy refusals are never retried; naming them in `on`
is an error, as is an unknown key in the table. A provider's retry-after is
honoured when longer than the backoff. An empty reply counts as a `server`
failure (and is retried at most twice). Without a `retry` table the built-in
policy applies: six attempts, every class, 1 s doubling, no longest wait. In a
`[models] fallback` chain a model with a `retry` table is retried by it before
the chain moves on (the chain's own two same-model retries are for models
without one), and the chain is then not run again as a whole; moving between
models stays the chain's decision.

### `[network]`

| Key | Type | Required | Meaning |
|---|---|---|---|
| `proxy` | `"environment"` or `"none"` | no (default `"none"`) | Whether model connections go through the proxy the environment names |

`RAPIDLM_PROXY=environment|none` overrides it without editing the file.

By default model connections are dialled directly and the proxy variables are
not read, so a shell that already exports `HTTPS_PROXY` behaves as it always
did. With `proxy = "environment"` every model a run builds — the default, the
`[models] fallback` chain and the `[phases]` models — and the `rapid setup`
probe go through the same proxy:

- `https_proxy` (then `HTTPS_PROXY`) for `https://` endpoints, reached through
  a `CONNECT` tunnel with TLS to the endpoint inside it: the proxy never sees
  the request or the key.
- `http_proxy` for `http://` endpoints — lower case only, since the upper-case
  spelling is also a request header name some servers export into the
  environment (on Windows, where variable names ignore case, either spelling).
  The proxy receives the whole request, the key included, as it would travel
  to a plain-`http://` endpoint anyway.
- `no_proxy` (then `NO_PROXY`): names (and their subdomains), addresses and
  ranges such as `10.0.0.0/8`, each with an optional port, dialled directly;
  `*` turns the proxy off. Loopback endpoints are always dialled directly.
- Only `http://` proxies are supported; credentials in the proxy URL are sent
  as `Proxy-Authorization` and never printed. A variable set but empty turns
  its proxy off. An unusable proxy URL is a configuration error naming the
  variable and where the opt-in came from, never the URL; an unusable
  `no_proxy` entry is quoted (up to 64 characters, anything before an `@`
  withheld).
- A proxy that refuses its own credentials (`407`) ends the turn without
  trying a fallback model (every model goes through the same proxy);
  `rapid setup` reports it as a network failure (exit 13).

### `[context]`

| Key | Type | Required | Meaning |
|---|---|---|---|
| `auto_compact_tokens` | 1024–1000000 | no | Once the estimated tokens the next turn would carry reach this, the session folds its earlier turns into a summary on its own — the same compaction `/compact` runs (same `[phases] compact` model, same `pre_compact`/`post_compact` hooks with `"trigger": "auto"`, same `context.compacted` record) — between turns, never inside one. Absent: the session never folds on its own |
| `auto_compact_scope` | `"conversation"` or `"total"` | no (default `"conversation"`) | What the estimate covers: the summary and turn history (the part compaction folds), or additionally the standing prefix every turn carries regardless (system prompt, project rules, memory, todos, reminders) |

The estimate is bytes-over-four on what the next turn would carry — a bound
for the fold/no-fold decision, never a billing figure. The check runs once per
ledger advance (a turn settled, a compaction recorded) while the model slot is
free, and holds the slot for its whole life; a turn or compaction in flight
defers the check to the next tick. Under the threshold it spends and says
nothing. Over it, the TUI shows one `auto-compacted N turn(s) in T tokens`
line; a model that fails to resolve or answer is reported as
`auto-compact skipped: …` and the history is left as it was. A session's
first tick checks once, so a resumed session with an oversized history folds
before its first turn rather than after it.

### `[shell_env]`

The environment a model-spawned shell command starts with — foreground
`shell_exec`, background and monitor jobs, and sandboxed runs alike. `MCP`
servers keep their own per-server `env` configuration and are not governed
by this section.

| Key | Type | Required | Meaning |
|---|---|---|---|
| `inherit` | `"all"`, `"core"` or `"none"` | no (default `"core"`) | What passes from the parent environment: everything; the core set (`PATH`, `HOME`, `LANG`, `TMPDIR`, plus on Windows the loader base set); or nothing |
| `include` | array of names | no | Names read from the parent environment and passed through on top of `inherit` — how a project says "this build needs `NPM_TOKEN`" without writing the value anywhere |
| `exclude` | array of names | no | Names dropped, whatever inherited them. Deny wins over `inherit` and `include` alike |
| `set` (table) | name → literal | no | Literal values the command starts with, applied last. For constants (`CI = "true"`), never secrets: a value here is config, readable by anything that reads this file. A secret belongs in the parent environment behind an `include` name |

Precedence, in order: `inherit`, then `include` from the parent
environment, then `exclude`, then `set` (an explicit literal survives an
`exclude` — it is config, not inheritance). The run identity
(`RAPIDLM_SESSION_ID`/`RAPIDLM_TURN_ID`) is added after everything and
always wins. Sandboxed runs bound every value at 4096 bytes — the same
bound `[shell_env.set]` validates — and at most 128 variables; a policy
that builds cannot be one the sandbox refuses.

The default is exactly the behavior from before this section existed: core
inheritance, nothing included, excluded or set. An unreadable configuration
file also reads as the default — no command fails because a policy could
not be read; `rapid doctor` reports the broken file.

```toml
[shell_env]
inherit = "core"
include = ["NPM_TOKEN", "GITHUB_TOKEN"]
exclude = ["TMPDIR"]

[shell_env.set]
CI = "true"
```

### `[approvals]`

| Key | Type | Required | Meaning |
|---|---|---|---|
| `auto_review` | bool | no (default off) | A permission `Ask` is first judged by one model call against a written risk policy (`approval_review`): exfiltration, credential and account material, destructive actions, security weakening. An approval proceeds, journaled as the same request-then-answer pair a human approval leaves; a denial — or a reviewer that cannot be built, cannot answer, or answers something unreadable — still raises the approval to the human, annotated with the verdict and its risk tier |

The reviewer can only ever answer an `Ask`: deny rules, managed bans, write
scopes and plan mode's floor all still decide first, and a reviewer approval
never widens past any of them. The human is the override for every denial.
A circuit breaker suspends the reviewer for the session after three
consecutive denials or ten of fifty verdicts — every ask goes back to the
human, with a notice saying so. The reviewer runs on the session's own
model; every verdict and its reason lands in the transcript as a notice.

### `[egress_proxy]`

The domain rules a sandboxed command's network is routed through. Absent or
empty `allow`: no proxy is started, the network posture is exactly what it
was.

| Key | Type | Required | Meaning |
|---|---|---|---|
| `allow` | array of host patterns | no (empty = off) | What a sandboxed command may reach: `registry.npmjs.org` (exact), `*.npmjs.org` (subdomains, not the apex), `**.github.com` (the apex and everything under it), `*` (everything — a policy that is really a deny list) |
| `deny` | array of host patterns | no | Refused whatever `allow` says. Deny wins |
| `allow_local_binding` | bool | no (default off) | Loopback and private destinations are deliberate (a local dev server), not refused |

When configured, every sandboxed `shell_exec` starts a loopback proxy this
process owns, and the command's `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`
variables point at it (`NO_PROXY` keeps loopback direct). Every connection
is judged against the rules before it leaves the machine: an allow tunnels
through — for `https` inside a `CONNECT` tunnel, so the proxy sees the
destination, never the bytes — and a denial is refused with a stable reason
(`deny-rule`, `not-allowlisted`, `local-binding`, `unresolvable`). Both
sides leave a receipt in the project's egress log
(`.rapidlm/egress-receipts.jsonl`, the same one the tools write). Unless
`allow_local_binding` is set, a destination that is — or resolves to — a
loopback, private or link-local address is refused: the cheap half of a
DNS-rebinding guard (every address a name offers must be public).

Truth in tiers: on the host-restricted tier this is routing, not
confinement — a command that ignores the proxy variables dials as that
tier always allowed, and the receipts show what went *through*, never a
claim about what did not. The seatbelt tier's confinement is unchanged.

```toml
[egress_proxy]
allow = ["registry.npmjs.org", "*.pypi.org", "**.github.com"]
deny = ["*.internal.example.com"]
```

## Examples

A server on this machine that needs no key:

```toml
[models]
default = "local"

[model.local]
provider = "openai-compatible"
model = "local-model"
base_url = "http://127.0.0.1:8080/v1"
```

A gateway speaking the second dialect, its key in one of two variables, with a
fallback and a cheaper model for compaction:

```toml
[models]
default = "gateway"
fallback = ["local"]

[phases]
compact = "local"

[model.gateway]
provider = "anthropic"
model = "large-model"
base_url = "https://gateway.internal"
env_key = ["GW_API_KEY", "TEAM_API_KEY"]
max_tokens = 8192
retry = { max_attempts = 3, on = ["rate_limit", "network"] }
continue_on_length = 2
```

Select a different model for one invocation:

```sh
RAPIDLM_MODEL=gateway rapid exec "summarize the diff"
```

## Honest constraints

These are typed, fail-closed properties of the current transport — surfaced at
configuration time or as typed provider failures, never silent downgrades:

- **Transport.** HTTP/1.1 over plain TCP (local/gateway servers) or over TLS
  for `https://` origins, verified against the static Mozilla root set
  (rustls; no custom CAs, no dynamic trust store). Cloud metadata /
  link-local targets stay rejected fail-closed.
- **Bearer auth only.** Both dialects send `Authorization: Bearer <key>`; a
  keyless profile sends the header with an empty token, which a server that
  takes no key ignores. The second dialect suits gateways that accept bearer
  auth.
- **Canonical model id alphabet.** `model` (and `effort_ids`) allow
  alphanumerics with single `-`, `_`, `.`, `/`, `:` separators — ids like
  `vendor/model:tag` are carried verbatim.

## Errors

Configuration and provider failures are typed; `rapid exec` exits `2` for a
configuration it cannot use, `4` when the provider failed or refused, and `5`
when no model is configured (the full table is in
[getting started](../getting-started.md#exit-codes)):

- `RAPIDLM_CONFIG points at a missing config file: <path>`
- `config key has the wrong type: model.<id>.max_tokens`
- `default model 'x' has no [model.x] table; defined models: …`
- `config base_url '…' is invalid: expected an http:// or https:// origin without userinfo or metadata hosts`
- `model configuration error: …` (before any request is sent)
- `agent turn failed: failed` (provider-side failure after a typed attempt)

Secret values never appear in errors, warnings, or debug output.
