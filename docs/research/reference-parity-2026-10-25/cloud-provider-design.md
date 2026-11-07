# Additional cloud protocols (REF-010 task-specific acceptance complete)

The progress sections below retain their historical checkpoint status. Current Azure,
Bedrock and Copilot local implementation acceptance is recorded in
`validation.copilot_segment`; provider-account login and final platform/quality
acceptance remain separate pending tasks.

## Pinned reference evidence

OpenCode fork @77bba6a provider/provider.ts imports bundled @ai-sdk/azure and
@ai-sdk/amazon-bedrock, with versions pinned in its package.json/bun.lock. Azure's
custom loader defaults to sdk.responses(modelID), selecting sdk.chat(modelID) when
useCompletionUrls is true. azure-cognitive-services supplies a cognitive-services
resource prefix when configured. The exact SDK packages have now been inspected as static source (no execution); their lockfile SHA-512 integrity was verified. Provenance is recorded in azure-sdk-provenance.json and amazon-bedrock-sdk-provenance.json. Azure 2.0.73 defaults to a v1 prefix plus explicit api-version=v1, with an independent useDeploymentBasedUrls option. The repository imports these SDKs rather than containing their Azure/Bedrock implementations.

The Bedrock loader checks AWS_PROFILE, AWS_ACCESS_KEY_ID, AWS_BEARER_TOKEN_BEDROCK and
AWS_REGION, then delegates to the AWS provider chain. It rewrites selected model ids
into regional inference profiles (US/EU/APAC/AU), preserves global-prefixed ids and
avoids selected GovCloud rewriting. Do not guess new model availability or silently
rewrite user ids in RapidLM. Expose explicit region and full inference-profile/model
identity, and document migration instead. Native implementation must preserve canonical
text/images/tools, reasoning signatures, usage, egress, cancellation and retry classes.

Copilot and enterprise choose Responses for ids containing codex, otherwise chat.
Their locally bundled SDK appends /responses or /chat/completions. Authentication is
loaded by the external plugin opencode-copilot-auth@0.0.9 (plugin/index.ts); it must not
be inferred from ordinary OpenAI endpoints. Account/device flow belongs to REF-011.
Implement explicit protocol selection rather than a model-substring heuristic; inspect
plugin/package evidence and official service documentation for required bindings.

Kimi @743f1409 protocol registry has four real wire protocols; vertexai is an option
on its Google adapter. Azure/Bedrock/Copilot are OpenCode-derived differences, not
additional native Kimi protocols. Source tests explicitly reject Azure as a wire id.

## Primary documentation checked 2026-10-26

- https://learn.microsoft.com/en-us/azure/ai-foundry/model-inference/concepts/endpoints
- https://learn.microsoft.com/en-us/rest/api/aifoundry/azureopenai/responses
- https://learn.microsoft.com/azure/cognitive-services/openai/quickstart
- https://docs.aws.amazon.com/bedrock/latest/userguide/api-keys-use.html
- https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_InvokeModelWithResponseStream.html
- https://docs.aws.amazon.com/bedrock/latest/userguide/model-parameters-anthropic-claude-messages-request-response.html

Azure supports the current /openai/v1 route with deployment name in model, plus legacy
versioned deployment URLs. api-key is a fixed wire authentication header; Entra uses
bearer credentials. The Responses reference also permits an explicit api-version query
(v1/preview), while the endpoint guide says v1 needs no version query. Model this as an
explicit validated API mode, not an inferred hostname or arbitrary appended query.
Deployment identifiers and versions must be path/query-safe and part of replay scope.
Keep the native provider selection explicit, reusing the existing OpenAI encoder/parser.
Never send a first-party credential to a changed custom origin by preset inference.

The pinned Bedrock **3.0.57** chat implementation calls **ConverseStream**, not
Anthropic InvokeModel. Native implementation should target the shared Converse content
model (including reasoning signatures and tool results) for all its supported model
families. The earlier InvokeModel documentation is a useful alternate API reference,
but does not establish what this pinned SDK does. Bedrock supports bearer API keys and
SigV4 with session credentials. It uses the AWS binary event-stream envelope, requiring
framing/CRC/error handling before canonical text/tool parsing. Current HttpTransport
streams UTF-8 text callbacks, so add a shared binary callback boundary for true native
streaming. A generic SSE gateway does not close this gap. Native AWS signing binds
method, URI/query, host, body and region/service at the wire, with session-token support,
secret-free debug and deterministic official-vector tests. Keep the existing HTTP,
scheduler and credential authorities. AWS provider-chain behavior is delegated by the
fork; RapidLM should expose an explicit authenticated chain/process and test it rather
than inventing ambient role assumptions.

## Next implementation acceptance

Add focused routing/auth/envelope unit tests and configured-binary loopback fixtures.
Demonstrate live fragments before stream completion, split-frame/UTF-8 correctness,
private signed tool replay after restart, images, usage, typed provider errors,
cancellation and egress-before-auth. For Azure cover both deployment/version and v1
modes with API-key/Entra binding. For Bedrock cover explicit bearer and signed/session
credentials, malformed/truncated/checksum-invalid event frames and no leaked auth.
Managed ceilings and offline doctor/setup must apply to every new provider path.
Document exact native limits and any separately tested compatibility path. Keep this
task pending until all required branches have evidence. Real service/account checks
remain in REF-027 when usable credentials are available; no login has been started.


## Azure implementation segment (REF-010 remains pending)

Native azure-chat and azure-responses now use the shared OpenAI encoder/parser and
transport with an explicit URL mode independent of api_version. `model` is the
configured deployment (including effort_ids selection). Fixed api-key or Entra bearer
headers are selected by configuration; no hostname inference. The native setup presets
require a supplied resource prefix and deployment. Entra supports the existing bounded
credential process under the same managed ceiling as Vertex, without account login.
Modern Chat output limits use max_completion_tokens; older deployments may explicitly
choose azure_legacy_max_tokens. No model-name heuristic is used. Local fixtures cover
both APIs, URL modes and auth kinds, lazy helpers, private Responses restart, split-UTF8
incremental streaming and offline setup. Focused native tests and workspace clippy have passed. Final Azure library regression evidence passed (see validation.azure_segment);
Bedrock and Copilot are still pending and REF-010 must not be marked complete yet.


Additional authoritative framing/protocol sources checked:

- https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_ConverseStream.html
- https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_ContentBlockDeltaEvent.html
- https://smithy.io/2.0/aws/amazon-eventstream.html

Implementation notes for the next segment: ConverseStream posts to the model's
converse-stream resource, with native system/messages/inferenceConfig/toolConfig.
Frame prelude and entire-message CRC32 must both verify before delivery. Parse typed
headers with unique names; keep event types and exception types out of display text.
Use the existing total-body bound, checked arithmetic and bounded partial buffers;
never trust a declared length enough to allocate it unchecked. Preserve partial tool
arguments and reasoning signatures until block/message completion; refuse malformed
unions, truncated frames and unfinished tool calls before tool execution. Header
metadata identifies the event type; its JSON payload is the event object rather than
an SSE envelope. Include ARN/inference-profile identifiers in path encoding and replay
scope; do not conflate API region with a model id's routing prefix.


## Bedrock library segment (REF-010 still pending)

Implemented shared fallible `HttpTransport::execute_streaming_bytes` with Box
forwarding, incremental reads within HTTP chunks, and immediate socket close on a
binary decoder error. Existing text streaming uses the same path with split-UTF8
handling. Provider auth gains a fixed SigV4 mode: egress planning precedes signing;
reserved caller signing headers are refused. Only fixed signed header fields are
materialized at write. Generic raw-client API-key behavior is preserved.

`aws_eventstream.rs` bounds partial/total bytes, verifies prelude/message CRC32,
parses typed unique headers with a 64-header resource bound and rejects truncated
or malformed frames. `aws_sigv4.rs` uses the already-locked ring implementation;
13 pinned AWS C Auth vectors cover header/body signing, normalized paths, query
ordering, UTF8 and session tokens. Test data and Apache license/provenance are in
crates/llm-router/tests/fixtures/aws-signing. No vendor implementation was copied.

`bedrock.rs` implements the native ConverseStream endpoint and text/image/tool
conversions, binary content block assembly, private reasoning/redacted bytes,
typed errors, cache usage and explicit native additional-model fields. Region and
authentication mode join the endpoint in replay scope. A private context fingerprint
binds signed reasoning to the exact native system/messages prefix; changed prefixes
omit incompatible reasoning while preserving visible tool-turn text. JSON tool
arguments normalize as native objects before canonical call binding. Redacted blob
fragments decode/concatenate as bytes and re-encode once. Interleaved text blocks
render in content order; many small deltas coalesce under the canonical event bound.
AWS prompt-caching documentation confirms that ordinary input, cache-read and
cache-write counts are separate; normalized uncached input includes newly written
cache tokens. Request paths permit long encoded resources while configured base
URLs retain 512 bytes and request queries retain 2048 bytes.

This is a library segment, **not a completed selectable CLI provider**. Configured
AWS credential acquisition/refresh, managed/profile/setup/doctor plumbing and actual
ConfiguredModel/SuspendedHistory end-to-end checks are next. Canonical reasoning
effort currently fails explicitly in this adapter; add explicit model-family mapping
before task completion. No login, AWS CLI command or real provider call occurred.
`validation.bedrock_adapter_segment` records 369 passing selected regression cases,
workspace clippy, formatting, snapshots and remaining work.

Additional primary references checked during implementation:

- https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_sigv-create-signed-request.html
- https://github.com/awslabs/aws-c-auth/tree/84a26ea184d9ccd322c8164759510b3604ab13fd/tests/aws-signing-test-suite/v4
- https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_ReasoningTextBlock.html
- https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_TokenUsage.html
- https://docs.aws.amazon.com/bedrock/latest/userguide/prompt-caching.html


## Bedrock CLI wiring (current segment)

Bedrock is selectable from model configuration and the common backend dispatcher.
`BedrockModelOptions` keeps region/auth/resource/extra fields/effort mode explicit.
Setup requires region and model/inference-profile identity; it never invents a regional
prefix. Optional native resource handles long ARNs. Setup defaults to AWS environment
signing on the standard regional origin; custom origins require explicit credential
selection. Bearer tokens and SigV4 authority cannot be mixed. Both setup and doctor
use the native adapter; offline commands do not execute a helper. Connectivity probes
cap output at 16 and omit optional explicit thinking budgets, preserving canonical
managed effort floors and leaving the actual configured model unchanged.

`CredentialKind::AwsSigning` is metadata for signing authority, not an API key.
`AwsWireAuth` resolves environment signing keys or an explicit bounded process only
after egress approval. `aws_profile` normalizes to the fixed installed AWS CLI argv
`configure export-credentials --profile <name> --format process --no-cli-pager
--no-cli-auto-prompt`. The existing preconfigured chain may supply SSO/role credentials;
RapidLM never starts login. Explicit processes print strict AWS Version1 process JSON.
Only explicit future expiry is cached (max1h, refresh30s early), otherwise credentials
are reread each request. Empty/malformed/expired outputs fail with redacted errors.
Session keys never enter public output or durable tool history. Managed helper opt-in
applies to normalized profiles, primary/fallback/phase paths and direct model builds.

Explicit `bedrock_effort` selects Anthropic output_config/beta or Nova reasoningConfig
mapping. Supported effort levels are validated without model-name guessing. Anthropic
thinking enabled/adaptive requires reasoning and Anthropic mode; enabled budgets must
fit the actual request output cap. Native extra fields cannot replace output caps or
message/tool fields. Default native output cap is always sent explicitly.

Primary credential/control references checked during wiring:

- https://docs.aws.amazon.com/cli/latest/reference/configure/export-credentials.html
- https://docs.aws.amazon.com/cli/latest/userguide/cli-configure-sourcing-external.html
- https://docs.aws.amazon.com/nova/latest/nova2-userguide/using-converse-api.html
- https://docs.aws.amazon.com/nova/latest/nova2-userguide/request-response-schema.html
- https://docs.aws.amazon.com/bedrock/latest/userguide/model-parameters-anthropic-claude-messages-request-response.html

## Copilot exact plugin investigation (implementation still pending)

Downloaded exactly opencode-copilot-auth@0.0.9 as a static npm archive and verified its
registry SHA512 integrity; the fork selects this exact version in plugin/index.ts,
not an integrity-pinned dependency in its lockfile. Metadata/file hashes are recorded
in copilot-auth-provenance.json. No install, plugin execution, credential lookup, login
or account request occurred. Extracted analysis source is under
/tmp/rapid-reference-sdk-sources/copilot-auth-0.0.9/package/index.mjs.

The pinned plugin uses an explicit GitHub domain and derives api.<domain> token
exchange and copilot-api.<enterprise-domain> inference origins; public inference uses
api.githubcopilot.com. A stored GitHub OAuth token (called refresh in that plugin) is
exchanged via /copilot_internal/v2/token for a separate expiring Copilot access token.
The plugin installs fixed product/editor/integration headers, Openai-Intent,
X-Initiator user/agent derived from request history, and a vision indicator. It deletes
competing lowercase authorization/x-api-key headers. Responses function/tool/reasoning
items affect initiator detection. Auth refresh and inference use distinct origins and
must each obey egress in RapidLM. Device-flow polling is account work in REF011.

The reference identifies as VS Code and reuses a VS Code OAuth client id. Do not copy
those product identities into RapidLM or assume the current service accepts them.
Current primary GitHub documentation describes SDK authentication with a consumer's
own OAuth app and integration identifier; OpenCode's support was announced as a
specific partnership. A RapidLM compatibility implementation needs explicit protocol,
origin/client/integration binding and tested expiry/cancel/redaction semantics. Do not
label arbitrary OpenAI endpoint support as Copilot authentication parity, infer API
style from a codex substring, or infer subscription prices as zero from this plugin.

Current primary sources consulted for the next implementation:

- https://docs.github.com/en/copilot/how-tos/copilot-sdk/auth/authenticate
- https://docs.github.com/en/enterprise-cloud%40latest/copilot/how-tos/copilot-sdk/setup/multi-tenancy
- https://github.blog/changelog/2026-01-16-github-copilot-now-supports-opencode/


## Copilot implementation segment

Native copilot-chat and copilot-responses profiles select the shared OpenAI APIs
explicitly. `CopilotOptions` binds auth mode, secure token URL, product integration and
native request headers; private Responses scope includes all those choices. GitHub
source credentials are never sent to inference. `CopilotWireAuth` exchanges them only
after the inference egress plan succeeds and after a second independent auth-origin
egress check, using the common HTTP/TLS/proxy path. Its response limit is16KiB, token
limit8KiB, timeout10s. Only token/expires_at are consumed; returned endpoint metadata is
ignored. Explicit future expiry is cached<=1h with30s early refresh; near-expiry tokens
are used once. Failed/malformed/expired/redirect responses never connect inference.
Cancellation closes an in-progress token socket, releasing no partial credential.

The shared authenticated GET writes Authorization only at send and refuses injected
reserved headers. Credential source helpers remain bounded and run after the auth
origin is permitted. Direct opaque API helper tokens use an explicit printable-token
mode; existing Vertex/Azure helper parsing stays strict. Copilot source credentials
use OAuthAccess metadata and binding checks. ModelEntry/ResolvedCredential Debug now
redacts plaintext values; the old derived Debug exposed them (caught during review).

Product headers identify RapidLM rather than the reference plugin's VS Code identity.
Integration ID is explicit/configurable, with rapidlm-cli as the product default.
Initiator/vision headers come from canonical history, including synthetic tool turns.
Shared text/images/tools/usage and private encrypted reasoning stay on the existing
runtime path; no zero-cost subscription inference is introduced. Native setup presets
require an available model ID and explicit source credentials for GitHub-token/custom
origins. No account OAuth login or real Copilot/provider call occurred; account login
remains REF011 and credentialed quality/platform acceptance remains REF027.

`ModelEgress` combines at most two existing exact endpoint gates for live probes,
checks the target identity before selecting either, and aggregates their receipts.
Existing single-endpoint policy/proxy/SSRF behavior is retained. It does not follow
provider-advertised endpoints or widen origins. A configured custom DialGate also
sees both inference and token exchanges from the model builder.
