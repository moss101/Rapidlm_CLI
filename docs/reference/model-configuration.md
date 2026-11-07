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
| `provider` | string | yes | `"openai-compatible"` (Chat Completions), `"openai-responses"` (Responses), `"anthropic"`, `"google-genai"`, `"vertex-genai"`, `"vertex-anthropic"`, `"azure-chat"`, `"azure-responses"`, `"bedrock"`, `"copilot-chat"`, or `"copilot-responses"` |
| `model` | string | yes | The model id sent on the wire (e.g. `small-model`, `vendor/model:tag`) |
| `base_url` | string | except native Vertex/Bedrock/Copilot | `http://` or `https://` origin, e.g. `http://127.0.0.1:8080/v1` |
| `name` | string | no | Display name |
| `api_key` | string | no | Inline credential; wins over `env_key` |
| `env_key` | string or array | no | Env var name(s); the first set, non-empty value wins |
| `keychain` | string | no | OS keychain alias the key is kept under (`rapid setup --key-stdin` writes `rapidlm-model-<id>-<digest>`, the digest naming this config file and endpoint); read when the model client is built |
| `account` | string | no | Explicit provider-account registration matching the provider and resource; cannot be combined with other credential sources |
| `catalog` | table | no | Import provenance: source, provider, model and snapshot SHA-256; used for offline catalog diagnostics |
| `effort_ids` | table | no | Model id sent at a given reasoning effort, e.g. `{ high = "m-think", low = "m-fast" }` (keys: `none`…`ultra`); the effort after every floor picks it, else `model` |
| `retry` | table | no | This model's step retry policy (below); replaces the built-in one |
| `continue_on_length` | 0–8 | no (default 0) | When an answer ends at its output limit, up to this many follow-on requests hand the answer so far back and ask for the rest; the parts are one message. Each request is a `model.continued` ledger record and counts against the turn's token budget; `rapid exec` says how many follow-on requests a completed answer took, on stderr. A continuation is sent only while the answer can still grow by another part as long as the last within what one message holds (64 KiB); a part that would overflow it, or a reply proposing tool calls, leaves the answer as it was (its request still counts). `0`: the answer ends where the limit cut it |
| `max_tokens` | positive integer | no | Output cap; default `4096` when the provider needs one |
| `context_window` | positive integer | no | Documented context pin; default `32768` |
| `reasoning_effort` | `none`…`ultra` | no | Effort requested (managed and reminder floors may raise it) |
| `vision` | bool | no | The model reads images (default off) |
| `video_input` | bool | no | Enables video for supported `google-genai` and `vertex-genai` models (default off); large uploads currently require `google-genai` |
| `tool_use` | bool | no | The model accepts tool declarations (default on for existing profiles) |
| `caching` | bool | no | The provider caches prompts (default off) |
| `reasoning` | bool | no | The model exposes reasoning (default off) |
| `thinking` | Anthropic, Vertex Anthropic, and Bedrock with `bedrock_effort = "anthropic"`: `"adaptive"` or `"enabled"`; requires `reasoning = true`. Omitted leaves the provider default. |
| `thinking_budget_tokens` | Required for `thinking = "enabled"`: at least 1024 and less than `max_tokens` (default 4096). Forbidden with adaptive mode. |
| `project` | string | native Vertex only, required | Explicit Cloud project id or number; no path characters |
| `location` | string | native Vertex only, required | Regional location, `global`, or supported `us`/`eu` multi-region; endpoint defaults from this value |
| `adc` | bool | no | Native Vertex only: `true` runs the installed `gcloud auth application-default print-access-token --quiet` helper lazily; does not log in |
| `credential_process` | array of strings | no | Native Vertex/Azure Entra/Bedrock/Copilot: executable and arguments invoked directly, without shell interpolation; absolute executable path or PATH basename |
| `credential_timeout_ms` | integer, 100–30000 | no (default 10000) | Time bound for `credential_process`; forbidden without it, ADC, or an AWS profile |
| `api_version` | string | native Azure, required | Explicit `v1`, `preview`, `YYYY-MM-DD` or `YYYY-MM-DD-preview` query version |
| `azure_auth` | `"api-key"` or `"entra"` | no (default `"api-key"`) | Native Azure only: fixed API-key header or OAuth bearer; Entra also permits an explicit credential process |
| `azure_url_mode` | `"v1"` or `"deployment"` | no (default `"v1"`) | Native Azure only: append `/v1/<API>` or `/deployments/<model>/<API>` to `base_url` |
| `azure_legacy_max_tokens` | bool | no (default `false`) | Native Azure only: use `max_tokens` for older Chat deployments; otherwise Chat uses `max_completion_tokens`. Responses uses `max_output_tokens` |

| `region` | string | native Bedrock, required | AWS signing/routing region; default endpoint derived from it |
| `aws_auth` | `"sigv4"` or `"bearer"` | no (default `"sigv4"`) | Native Bedrock signing keys or explicit bearer token |
| `aws_profile` | string | no | Bedrock SigV4 only: delegate this configured profile to installed AWS CLI `configure export-credentials`; mutually exclusive with `credential_process` |
| `bedrock_resource` | string, up to 2048 bytes | no | Native model/inference-profile resource (including ARN), overrides `model` in the URL; does not widen canonical model-id limits |
| `bedrock_additional_fields` | table, up to 16 KiB | no | Native model-specific additional request fields; cannot replace message/tool fields or output-token caps |
| `bedrock_effort` | `"anthropic"` or `"nova"` | no | Explicit vendor mapping for canonical reasoning effort; omitted mode refuses explicit effort |

| `copilot_auth` | `"api-token"` or `"github-token"` | no (default `"api-token"`) | Pre-exchanged Copilot inference token, or a GitHub token exchanged for an expiring inference token |
| `copilot_token_url` | secure URL | GitHub-token mode only | Explicit enterprise token endpoint; public default is `https://api.github.com/copilot_internal/v2/token`. Required with a custom GitHub-token inference origin |
| `copilot_integration_id` | 1–128 ASCII letters, digits, `-_.` | no (default `rapidlm-cli`) | This product's integration identity; never inferred from another editor |

Cloud helper credentials (`adc = true` for Vertex or `credential_process` for Vertex/Azure Entra/Bedrock, or `aws_profile` for Bedrock) are mutually exclusive with each other and with inline, environment or keychain credentials. Native Vertex and Azure require an explicit credential source. Static Vertex and Azure Entra credentials are OAuth access tokens. Other profiles retain credential precedence: `api_key` > first set, non-empty
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
- **Authentication.** Google GenAI sends `x-goog-api-key`, Azure API-key mode sends `api-key`, and the other protocol selections send `Authorization: Bearer <credential>`. A
  keyless profile sends the header with an empty token, which a server that
  takes no key ignores. A gateway must accept that authentication form.
- **Canonical model id alphabet.** `model` (and `effort_ids`) allow
  alphanumerics with single `-`, `_`, `.`, `/`, `:`, `@` separators — ids like
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


Native Vertex profiles use `provider = "vertex-genai"` or `"vertex-anthropic"`:

```toml
[model.cloud]
provider = "vertex-genai"
model = "gemini-3.8-flash"
project = "my-cloud-project"
location = "global"
adc = true
```

`rapid setup --preset vertex-genai --project <project> --location global --dry-run`
previews this configuration. Remove `--dry-run` to probe and save it. The
`vertex-anthropic` preset uses the native Anthropic publisher and `streamRawPredict`
with the Vertex version field; both protocols put project, location and model in
the resource URL. Model and region availability remain the provider's decision.
A custom `--base-url` requires an explicit credential choice such as `--adc`.
Regional endpoints default to `<location>-aiplatform.googleapis.com`, global to
`aiplatform.googleapis.com`, and US/EU to `aiplatform.<location>.rep.googleapis.com`.
See [locations](https://docs.cloud.google.com/gemini-enterprise-agent-platform/resources/locations)
and [Claude on Vertex](https://docs.cloud.google.com/gemini-enterprise-agent-platform/models/partner-models/claude/use-claude).

ADC delegates the existing application-default credential chain to an installed
`gcloud`; RapidLM does not implement ADC file/metadata discovery or start login.
An alternative `credential_process = ["/absolute/path/helper", "arg"]` may print
one bearer token, or JSON with `access_token`, optional `token_type = "Bearer"`,
and one of `expires_in` (seconds) or `expires_at` (Unix seconds). JSON tokens with
expiry are cached up to one hour and refreshed 30 seconds early; plain tokens are
resolved for each request. Output is limited to 16 KiB and tokens to 8 KiB.
Failure, cancellation, timeout and oversized output terminate the helper process
group. Stderr is discarded and output never becomes a tool result or log entry.
The helper runs from the user's home with a bounded environment of platform,
Cloud SDK, Azure credential/configuration and proxy variables, without repository-relative executable lookup.
Credential-helper configuration is trusted executable user configuration.
Managed installations refuse all helpers by default; administrators must set
`[policy] allow_credential_processes = true` to enable primary, fallback or phase
helpers. Offline doctor and setup dry-run never execute them. The model transport's
egress gate is checked before helper execution; a helper's own credential-refresh
network access is controlled by the helper and the host environment.


For Azure choose `azure-chat` or `azure-responses`. `model` names your deployment,
including values selected by `effort_ids`. `base_url` names the resource API prefix
before `/v1` or `/deployments` (normally `https://<resource>.openai.azure.com/openai`,
or a cognitive-services resource prefix). Both URL modes send the explicit
`api-version` query. The URL mode is independent of the version; it is never inferred
from a model name or hostname. API keys go only in `api-key`. Set `azure_auth = "entra"`
for an OAuth access token in Authorization. Existing environment/keychain precedence,
provider allowlists, fallback/phase ceilings, retries and egress checks apply.

```toml
[model.azure]
provider = "azure-responses"
model = "my-deployment"
base_url = "https://my-resource.openai.azure.com/openai"
api_version = "v1"
azure_url_mode = "v1"
azure_auth = "api-key"
env_key = "AZURE_OPENAI_API_KEY"
max_tokens = 4096
```

`rapid setup --preset azure-responses --base-url <resource-prefix> --model <deployment>
--api-version v1 --key-env AZURE_OPENAI_API_KEY --dry-run` previews the native profile.
The resource prefix and deployment are required; the preset has no usable resource
of its own. Replace `--dry-run` with a normal verified setup to save it. Use
`--azure-url-mode deployment` for deployment URLs, or `--azure-auth entra --key-env
AZURE_OPENAI_AUTH_TOKEN` for Entra. Explicit bearer helpers may be configured manually
with `credential_process`; the same managed helper ceiling, output/time bounds,
refresh cache and offline rules apply as for Vertex. RapidLM does not start an Entra
account login. For example a configured Azure CLI helper may use
`["az", "account", "get-access-token", "--resource", "https://cognitiveservices.azure.com/",
"--query", "accessToken", "--output", "tsv"]` after the user has configured that CLI.
Older Chat models needing `max_tokens` instead of `max_completion_tokens` can opt in
with `azure_legacy_max_tokens = true`; deployment names do not reveal the underlying
model, so RapidLM does not guess this. Responses private reasoning remains bound to
the selected deployment, version, profile and endpoint. See the
[Azure endpoint reference](https://learn.microsoft.com/en-us/azure/ai-foundry/model-inference/concepts/endpoints)
and [Responses reference](https://learn.microsoft.com/en-us/rest/api/aifoundry/azureopenai/responses).

### Native Bedrock ConverseStream

```toml
[models]
default = "aws"

[model.aws]
provider = "bedrock"
model = "your-model-or-inference-profile-id"
region = "us-east-1"
aws_auth = "sigv4"
# Optional: delegate a configured AWS CLI profile instead of reading env keys.
# aws_profile = "work"
# Optional ARN/resource, encoded as one URL path component:
# bedrock_resource = "arn:aws:bedrock:us-east-1:123456789012:inference-profile/your-profile"
max_tokens = 4096
```

Without a profile/helper, SigV4 reads `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`
and optional `AWS_SESSION_TOKEN` only. It does not search credential files or start a
login. An explicit profile delegates the installed AWS CLI's existing credential
chain through `aws configure export-credentials --profile … --format process`.
An explicit helper must print AWS process JSON (`Version = 1`, `AccessKeyId`,
`SecretAccessKey`, optional `SessionToken` and RFC3339 `Expiration`). Expiring results
are cached for at most one hour and refreshed 30 seconds early; no-expiry results
are reread on every request. CLI profiles and helpers obey the same managed
`allow_credential_processes` ceiling as other cloud helpers. Credentials are resolved
and signed only after egress permits the request. Secret keys never become bearer
tokens or enter session records.

For Bedrock bearer auth, select `aws_auth = "bearer"` and an explicit `api_key`,
`env_key`, keychain alias, or token `credential_process`. AWS profile/signing fields
cannot be combined with bearer credentials. Base URLs default from region; custom
endpoints are explicit. Setup requires `--preset bedrock --region … --model …` and
accepts `--aws-auth`, `--aws-profile`, `--bedrock-resource`, `--bedrock-effort`.
A custom setup endpoint requires an explicit credential choice. Dry runs and offline
doctor never execute credential helpers. Live connectivity probes cap output at 16
tokens and omit explicit thinking budgets; managed canonical effort floors remain.

Enable `reasoning = true` before requesting effort. Explicit
`bedrock_effort = "anthropic"` maps low/medium/high to the Anthropic effort field
and beta; it also permits the same enabled/adaptive `thinking` controls described
above. `bedrock_effort = "nova"` maps none to disabled reasoning and low/medium/high
to Nova reasoning configuration. Other levels fail before sending. Omitted effort
uses the model default. Support is never inferred from model names; choose parameters
supported by the selected model. The output cap is always explicit on the wire.
Native signed/redacted reasoning replays privately with its original tool calls and
unchanged message prefix; changing model, region, resource, auth mode, endpoint, or
credential alias drops incompatible private state. RapidLM does not add cross-region
model prefixes automatically; configure an inference-profile id/resource explicitly.

### Native Copilot Chat and Responses

```toml
[models]
default = "copilot"

[model.copilot]
provider = "copilot-responses" # or copilot-chat; protocol is explicit
model = "your-enabled-model"
copilot_auth = "github-token"
env_key = "GITHUB_SOURCE"
copilot_integration_id = "your-product"
max_tokens = 4096
```

A GitHub source token and a Copilot inference token are separate credentials.
`github-token` mode exchanges the configured source token via an authenticated GET
at the explicit token endpoint, then sends only the resulting inference token to
model requests. Expiring inference tokens are cached for at most one hour and
refreshed 30 seconds early. Tokens with less than 30 seconds left are used once;
expired, malformed or oversized responses fail before inference connects. Returned
endpoint metadata cannot redirect credentials. Redirect responses are not followed.
An auth failure is returned without replaying a model request.

For an already exchanged inference token, choose `copilot_auth = "api-token"`
(the default) and `env_key = "GITHUB_COPILOT_API_TOKEN"`, or an explicit keychain,
inline key, or bounded `credential_process`. Helpers use the same plain-token or
`access_token` JSON contract described above and managed opt-in. Copilot API helper
tokens may contain opaque printable signing fields; whitespace/control characters
remain forbidden. Source-token helpers run only when an exchange is needed.
Neither mode imports `gh`/editor credentials or initiates a login.

The public inference origin is `https://api.githubcopilot.com`; the default token
endpoint is `https://api.github.com/copilot_internal/v2/token`. For enterprise or
other owned origins, set `base_url` and, in GitHub-token mode, an explicit
`copilot_token_url`. All credential-bearing endpoints require HTTPS, except explicit
HTTP loopback fixtures. Each auth and inference connection separately passes the
shared DNS, egress, proxy and cancellation checks. Doctor/setup live probes allow
only those configured origins. Offline doctor and setup previews do not exchange
credentials or execute helpers.

Supply an integration ID accepted by your deployment. Fixed product/editor headers
identify RapidLM. The first request is marked user-initiated; assistant/tool history
marks an agent continuation, and image input sets the vision indicator. No API choice
is inferred from a model-name substring. Shared OpenAI encoders preserve text, image,
tool and usage behavior; costs remain unknown without real pricing data. Responses
private replay is additionally bound to auth mode, token URL and integration ID.

Setup offers `--preset copilot-chat` or `--preset copilot-responses` with required
`--model`, optional `--copilot-auth`, `--copilot-token-url` and
`--copilot-integration-id`. Public API-token mode defaults to the inference-token
variable above. GitHub-token mode and custom origins require an explicit `--key-env`
or `--key-stdin` choice. Configuration and resolved credential debug views redact
credential values.
## Provider accounts

An account is an explicit OAuth client registration. Supply a client ID and endpoints
that the provider has registered for your own application; RapidLM does not discover
or reuse another application's client identity. For example, an authorization-code
registration can be connected to an OpenAI-compatible model:

```toml
[provider_accounts.work]
kind = "standard"
flow = "pkce"
encoding = "form"
provider = "openai-compatible"
issuer = "https://login.example"
client_id = "your-registered-client-id"
resource = "https://api.example/v1"
authorize_url = "https://login.example/authorize"
token_url = "https://login.example/token"
redirect_url = "http://127.0.0.1:8765/callback"
scopes = ["inference"]

[model.work]
provider = "openai-compatible"
model = "your-model"
base_url = "https://api.example/v1"
account = "work"
```

For device authorization use `flow = "device"` and `device_url` instead of
`authorize_url` and `redirect_url`. `encoding` is `form` or `json` as required by
the registered issuer. Optional `revoke_url` supports remote logout; optional
`verification_origin` pins a separate device-verification origin. Kinds `anthropic`,
`github`, and `kimi` select their respective account protocols. A GitHub account
requires a Copilot model with `copilot_auth = "github-token"` and an `exchange_url`
matching its Copilot token URL. The model's provider and base URL must match the
account's `provider` and `resource`; account authentication cannot be combined with
inline, environment, keychain, or process credential settings.

Run `rapid auth login work` to start authorization, `rapid auth status work` to
inspect local state, `rapid auth refresh work` to refresh explicitly, and
`rapid auth logout work` to revoke local access. Add `--no-browser` to login to open
the printed address yourself; add `--remote` to logout to request remote revocation
as well. Login is never started by a model request. Tokens are kept in the OS
keychain; account metadata and Kimi's RapidLM device identifier live under the
RapidLM home in `provider-accounts`. A managed policy must explicitly set
`[policy] allow_provider_accounts = true` for login, refresh, or model use. Logout
remains available when accounts are disabled. Provider-side OAuth availability and
client registration are determined by the provider.

If a keychain deletion fails, `rapid auth status` reports cleanup pending. RapidLM
keeps bounded handles for retired or interrupted login bundles and retries deletion
on later account operations. A full cleanup backlog prevents another rotation until
the keychain is available; it does not replace the current credential.

## Provider catalogs

Catalog snapshots are downloaded only by an explicit catalog command. To browse the
public models.dev directory, refresh it and then list its providers or models:

```sh
rapid catalog refresh
rapid catalog sources
rapid catalog list --offline
rapid catalog list --provider openai --offline --json
```

Use `--source https://example.com/api.json` to select another compatible source.
Snapshots are source-bound and stored under the RapidLM home, with their SHA-256 and
fetch time. At most 64 sources can be saved. A fresh snapshot is reused for ten
minutes; a failed online refresh can fall back to the saved snapshot, with the
failure and freshness shown. Offline commands never fetch. Hashes detect accidental
cache corruption; they are not signatures or proof that a catalog is trustworthy.
Use `rapid catalog forget --source URL` to remove a saved snapshot and free a source
slot; imported profiles remain in the configuration.

Import requires the model's wire protocol, destination endpoint and credential source
to be specified explicitly. Review the plan first, then repeat it with `--apply`:

```sh
rapid catalog import openai example-model --profile example \
  --wire openai-compatible --base-url https://api.example.com/v1 \
  --key-env EXAMPLE_API_KEY
rapid catalog import openai example-model --profile example \
  --wire openai-compatible --base-url https://api.example.com/v1 \
  --key-env EXAMPLE_API_KEY --apply
```

Replace the provider, model, endpoint and variable with values appropriate to your
provider. Other credential choices are `--keychain ALIAS`, `--account ID`,
`--keyless`, and `--aws-env`; account and provider policy still apply. A guessed or
unsupported catalog wire needs explicit `--override-wire` after review. Imports use
saved data by default; `--online` permits a refresh, and importing stale data requires
`--allow-stale`. A new profile never replaces an existing profile and leaves the
current default alone unless `--set-default` is supplied. Applying a change makes a
private backup of the previous configuration.

`rapid catalog check` compares imported profiles with the saved source and reports
changed snapshots, removed or retired models, and altered model identifiers. Add
`--online` to permit a refresh. Findings from stale snapshots may be out of date.
Refresh and check do not delete models, change credentials, or migrate defaults.
The same controls are available in the terminal as `/model catalog ...`.
