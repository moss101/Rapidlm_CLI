# Model configuration

`rapid exec` drives a real model when a user config selects one, and fails
typed when it does not. The configuration surface is one small user TOML,
env overrides on top, and per-model tables that pin the provider, model id,
endpoint, and credential.

## File locations and precedence

The config file is resolved from the environment in this order:

1. `RAPIDLM_CONFIG` — explicit config file path. If set, the file must exist;
   a missing file is a typed error (never silently ignored).
2. `RAPIDLM_HOME/config.toml` — verbatim home-root override.
3. `$HOME/.rapidlm/config.toml`
4. `$USERPROFILE/.rapidlm/config.toml` (Windows)

If none of these exist, `rapid exec` stays in its typed unconfigured fallback:
it prints where it looked, runs the turn with the unconfigured model (a typed
provider failure), and exits `1`. It never fabricates a completion.

Within a resolved config, values apply in this precedence:

```
env override (RAPIDLM_MODEL, RAPIDLM_PROXY, credential env vars) > config file > typed fallback
```

## Schema

### `[models]`

| Key | Type | Required | Meaning |
|---|---|---|---|
| `default` | string | one of `default`/`RAPIDLM_MODEL` | Profile id of the model used by `rapid exec` |

`RAPIDLM_MODEL` overrides `[models].default` without editing the file.

### `[model.<profile-id>]`

One table per model. `<profile-id>` must be lowercase letters, digits, and
single dashes (it becomes the router profile id and credential handle name).

| Key | Type | Required | Meaning |
|---|---|---|---|
| `provider` | string | yes | `"openai-compatible"` or `"anthropic"` |
| `model` | string | yes | Provider-side model id sent on the wire (e.g. `llama3.2`, `gpt-4.1`) |
| `base_url` | string | yes | Plain-HTTP origin, e.g. `http://127.0.0.1:11434/v1` |
| `name` | string | no | Display name |
| `api_key` | string | no | Inline credential; wins over `env_key` |
| `env_key` | string or array | no | Env var name(s); the first set, non-empty value wins |
| `keychain` | string | no | OS keychain alias the key is kept under (`rapid setup --key-stdin` writes `rapidlm-model-<id>-<digest>`, the digest naming this config file and endpoint); read when the model client is built |
| `effort_ids` | table | no | Model id sent at a given reasoning effort, e.g. `{ high = "m-think", low = "m-fast" }` (keys: `none`…`ultra`); the effort after every floor picks it, else `model` |
| `retry` | table | no | This model's step retry policy (below); replaces the built-in one |
| `continue_on_length` | 0–8 | no (default 0) | When an answer ends at its output limit, up to this many follow-on requests hand the answer so far back and ask for the rest; the parts are one message. Each request is a `model.continued` ledger record and counts against the turn's token budget; `rapid exec` says how many follow-on requests a completed answer took, on stderr. A continuation is sent only while the answer can still grow by another part as long as the last within what one message holds (64 KiB); a part that would overflow it, or a reply proposing tool calls, leaves the answer as it was (its request still counts). `0`: the answer ends where the limit cut it |
| `max_tokens` | positive integer | no | Output cap; default `4096` when the provider needs one |
| `context_window` | positive integer | no | Documented context pin; default `32768` |

Credential precedence: `api_key` > first set, non-empty
`env_key` entry > `keychain` > keyless. A `keychain` key is read from the OS
keychain (macOS Keychain, Windows Credential Manager, the Secret Service on
Linux) only when the client is built; where no keychain is available, or it
holds nothing under the alias, the model fails to build with an error naming
the alias, before anything is sent. Keyless configs (typical for local servers) send no
bearer token. Config keys that are not part of the schema are reported as
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

## Examples

Local OpenAI-compatible server (Ollama):

```toml
[models]
default = "ollama-local"

[model.ollama-local]
provider = "openai-compatible"
model = "llama3.2"
base_url = "http://127.0.0.1:11434/v1"
env_key = "OLLAMA_API_KEY"
```

Anthropic-compatible gateway with an env credential:

```toml
[models]
default = "gateway"

[model.gateway]
provider = "anthropic"
model = "claude-3-5-sonnet"
base_url = "http://gateway.internal:8080"
env_key = ["GW_API_KEY", "ANTHROPIC_API_KEY"]
max_tokens = 8192
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
- **Bearer auth only.** Requests carry `Authorization: Bearer <key>` (or no
  auth header token when keyless). The Anthropic path suits gateways that
  accept bearer auth.
- **Canonical model id alphabet.** `model` allows alphanumerics with single
  `-`, `_`, `.`, `/`, `:` separators — provider-side ids like
  `vendor/model:tag` (OpenRouter, Ollama tags) are carried verbatim.
- **Exec is one turn with no tools.** `rapid exec` runs a single agent turn
  over the live context with no tool schemas advertised; models that answer
  with tool calls produce a typed failure instead of a fabricated completion.

## Errors

Configuration and provider failures are typed and exit `1`:

- `RAPIDLM_CONFIG points at a missing config file: <path>`
- `config key has the wrong type: model.<id>.max_tokens`
- `default model 'x' has no [model.x] table; defined models: …`
- `config base_url '…' is invalid: expected an http:// or https:// origin without userinfo or metadata hosts`
- `model configuration error: …` (before any request is sent)
- `agent turn failed: failed` (provider-side failure after a typed attempt)

Secret values never appear in errors, warnings, or debug output.
