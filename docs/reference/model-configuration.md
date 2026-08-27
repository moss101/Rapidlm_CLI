# Model configuration (Grok Build style)

`rapid exec` drives a real model when a user config selects one, and fails
typed when it does not. The configuration surface mirrors the Grok Build CLI:
one small user TOML, env overrides on top, and per-model tables that pin the
provider, model id, endpoint, and credential.

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

Within a resolved config, values apply with Grok Build precedence:

```
env override (RAPIDLM_MODEL, credential env vars) > config file > typed fallback
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
| `max_tokens` | positive integer | no | Output cap; default `4096` when the provider needs one |
| `context_window` | positive integer | no | Documented context pin; default `32768` |

Credential precedence follows Grok Build: `api_key` > first set, non-empty
`env_key` entry > keyless. Keyless configs (typical for local servers) send no
bearer token. Config keys that are not part of the schema are reported as
stderr warnings (`warning: unknown config key '…'`) and ignored.

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

- **Plain HTTP only.** The workspace has no TLS stack; the HTTP/1.1 transport
  rejects `https://` (and cloud metadata / link-local targets) fail-closed.
  Point `base_url` at local or gateway plain-HTTP endpoints.
- **Bearer auth only.** Requests carry `Authorization: Bearer <key>` (or no
  auth header token when keyless). The Anthropic path suits gateways that
  accept bearer auth over plain HTTP.
- **Canonical model id alphabet.** `model` allows alphanumerics with single
  `-`, `_`, `.` separators. Ollama tags use `:` (e.g. `gpt-oss:20b`); alias
  the model first (`ollama cp gpt-oss:20b gpt-oss-20b`) and configure the
  alias.
- **Exec is one turn with no tools.** `rapid exec` runs a single agent turn
  over the live context with no tool schemas advertised; models that answer
  with tool calls produce a typed failure instead of a fabricated completion.

## Errors

Configuration and provider failures are typed and exit `1`:

- `RAPIDLM_CONFIG points at a missing config file: <path>`
- `config key has the wrong type: model.<id>.max_tokens`
- `default model 'x' has no [model.x] table; defined models: …`
- `config base_url '…' is invalid: the transport has no TLS; only plain http:// origins are supported`
- `model configuration error: …` (before any request is sent)
- `agent turn failed: failed` (provider-side failure after a typed attempt)

Secret values never appear in errors, warnings, or debug output.
