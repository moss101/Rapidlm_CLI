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

To use a Responses endpoint, choose the `openai-responses` preset or set
`provider = "openai-responses"` in the model table. Its `base_url` is the API prefix
(for example `https://api.openai.com/v1`); RapidLM appends `/responses`. Existing
`openai-compatible` profiles keep `/chat/completions`. The Responses path requests
streaming with `store = false`, replays the local transcript, and encodes an explicitly
configured effort as `reasoning.effort`. Managed provider allow lists must explicitly
permit `openai-responses` where such a list is configured. Opaque provider reasoning
items are retained as bounded private replay state across tool turns and durable approval
continuations. Public event exports omit this state.

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

### `[phases]`

| Key | Type | Meaning |
|---|---|---|
| `compact` | profile id | The model `/compact` summarises with; the default model when absent (in-turn context recovery always uses the turn's own model) |

Other request purposes (`chat`, …) are accepted as keys; a run builds a separate
model only for `compact`. A phase naming a profile with no table is an error.

### `[model.<profile-id>]`

One table per model. `<profile-id>` must be lowercase letters, digits, and
single dashes (it becomes the router profile id and credential handle name).

| Key | Type | Required | Meaning |
|---|---|---|---|
| `provider` | string | yes | `"openai-compatible"` (Chat Completions), `"openai-responses"` (Responses), `"anthropic"`, or `"google-genai"` |
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
| `thinking` | Anthropic only: `"adaptive"` or `"enabled"`; requires `reasoning = true`. Omitted leaves the provider default. |
| `thinking_budget_tokens` | Required for `thinking = "enabled"`: at least 1024 and less than `max_tokens` (default 4096). Forbidden with adaptive mode. |

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

### `[mcp]`

| Key | Type | Required | Meaning |
|---|---|---|---|
| `startup_timeout_ms` | integer, 1000–120000 | no (default `30000`) | How long an MCP server gets to start, for every server |
| `tool_timeout_ms` | integer, 1000–600000 | no (default `30000`) | How long an MCP server gets to answer one call, for every server |
| `default_deferred` | boolean | no (default `false`) | Whether an MCP server's tools stay out of the tool list until the model loads them, for a server whose entry does not say |
| `max_loaded_tools` | integer, 1–1024 | no (default `24`) | How many deferred tools the model may have loaded at once (held to what the provider takes) |

These are defaults for every server and outrank a server's own `startup_timeout_ms` /
`tool_timeout_ms` in the project's settings; `RAPIDLM_MCP_STARTUP_TIMEOUT_MS` and
`RAPIDLM_MCP_TOOL_TIMEOUT_MS` outrank them in turn, and a managed policy's `[mcp]
max_tool_timeout_ms` caps the call limit. A value that is not usable is left out and named in
`rapid mcp list|get|probe`, `rapid doctor` and the per-turn warnings, not a config error. See
`docs/reference/cli-command-reference.md` (**Timeouts**).

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
- **Bearer auth only.** All three protocol selections send `Authorization: Bearer <key>`; a
  keyless profile sends the header with an empty token, which a server that
  takes no key ignores. A gateway must accept that authentication form.
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

Anthropic `caching = true` sends automatic ephemeral prompt caching, which advances the cache breakpoint as the conversation grows. Explicit `thinking` chooses the wire mode; RapidLM does not infer support from model names. For Anthropic, effort levels low/medium/high/xhigh are sent directly and ultra maps to max; none/minimal are refused. Choose a mode and effort supported by your model. See [thinking](https://platform.claude.com/docs/en/build-with-claude/thinking), [effort](https://platform.claude.com/docs/en/build-with-claude/effort), and [prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching).

Responses encrypted reasoning and Anthropic signed/redacted thinking blocks are private protocol continuation data. They stay paired with their original tool calls, including durable approval waits. They are excluded from answer text, debug output, public event exports and headless JSONL. A profile, model or endpoint change omits incompatible continuation data.

For native Google GenAI choose the `google-genai` setup preset, or set
`provider = "google-genai"`, `base_url = "https://generativelanguage.googleapis.com/v1beta"`
and a Google model id. `env_key = "GEMINI_API_KEY"` keeps the key outside the file.
The native path calls `models/<id>:streamGenerateContent?alt=sse` and sends the key
only in `x-goog-api-key`. `models/` is an optional model-id prefix. Enable `vision = true`
for image-capable models. Inline PNG/JPEG/GIF/WebP inputs and image tool results use
native `inlineData`; internal artifact handles must first be resolved to image data.

Google signed text/function-call parts remain private replay state, including the
original presence or absence of a call id. Thought parts never enter answer text.
With `reasoning = true`, explicit none effort sends `thinkingBudget = 0`, and
minimal/low/medium/high send the corresponding thinking level. xhigh/ultra are
refused. Earlier models may support budgets instead of thinking levels; leave effort
unset when the model does not support the requested level. Caching usage is recorded;
this path does not create persistent cached-content resources. Native Vertex routing
is a separate provider integration. See the [Google wire reference](https://ai.google.dev/api/generate-content)
and [model list](https://ai.google.dev/gemini-api/docs/models).
