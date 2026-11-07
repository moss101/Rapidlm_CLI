# Vertex implementation notes (REF-009, implemented and locally verified)

Goal scope requires native Vertex project/location routes and credential handling for
Google GenAI and Anthropic-on-Vertex. A generic API-key gateway is not native parity.
REF-008 is complete; use its encoder/parser and the existing Anthropic adapter.

Primary sources inspected 2026-10-26:

- https://cloud.google.com/vertex-ai/generative-ai/docs/reference/rest/v1beta1/projects.locations.publishers.models
- https://docs.cloud.google.com/workflows/docs/reference/googleapis/aiplatform/v1/projects.locations.publishers.models/streamGenerateContent
- https://cloud.google.com/vertex-ai/generative-ai/docs/partner-models/claude/use-claude
  (redirects to https://docs.cloud.google.com/gemini-enterprise-agent-platform/models/partner-models/claude/use-claude)
- https://docs.cloud.google.com/docs/authentication/provide-credentials-adc

Google native publisher path is project/location/publishers/google/models/<id> with
streamGenerateContent. Regional endpoints use <location>-aiplatform.googleapis.com;
global uses aiplatform.googleapis.com; US/EU use aiplatform.<location>.rep.googleapis.com, verified against current primary locations and partner-model docs.
API-key developer auth differs from Vertex's OAuth bearer credentials.

The Claude source explicitly documents:
`POST https://LOCATION-aiplatform.googleapis.com/v1/projects/PROJECT/locations/LOCATION/publishers/anthropic/models/MODEL:streamRawPredict`
Body has `anthropic_version = "vertex-2023-10-16"`, messages/max_tokens/stream/tools;
model belongs in the URL. Retain existing thinking/cache/private-replay conversions.
Check actual supported model identifiers/versions and locations rather than inventing
one. Some legacy Claude Vertex identifiers use @version: canonical ModelId currently
rejects @ and URL validation rejects raw @. If required, support the model identifier
explicitly and percent-encode its @ in the request path; do not rewrite it to a colon.

A configured credential process meets the worklist's ADC-or-process acceptance branch.
An explicit gcloud application-default print-access-token command can use ADC, but it
is not an in-process implementation of the ADC search/refresh chain. Never silently
read service-account files or start an account login. Implement any process with argv
(no shell), time/output bounds, cancellation, token redaction, no stderr echo and
appropriate managed/trust refusal. Audit configuration provenance and managed policy
before permitting a helper: do not introduce an arbitrary-command policy bypass.
Auth crate already has OauthAccess/OauthRefresh kinds; current model credential source
only supports inline API key/env/keychain/keyless. Extend existing authority rather
than adding a second credential database, egress stack or retry loop.

GoogleGenAiConfig currently always sends ProviderAuthStyle::GoogleApiKey. Make Vertex's
explicit native configuration select bearer style and native path; do not infer this
from host names. AnthropicEndpoint currently appends /messages with no model argument;
its Vertex route/envelope needs an explicit mode. Replay scope must include project,
location and full native endpoint, and must not cross accounts/profiles/origins.

Acceptance should drive real configured binaries against loopback fixtures for both
protocols, bearer header placement, body/path differences, signed tool replay, helper
refresh/cancel/bounds/redaction, invalid project/location, managed refusal and egress.
Credentialed live checks only with usable credentials and explicit recorded limits;
no real provider request has been made in this goal yet. Clean platform/release checks
remain for REF-027. Use the pinned reference clones as behavioral evidence only.


Implementation and focused verification are recorded in validation.vertex_batch.
Credentialed live verification and clean Linux/Windows acceptance remain REF-027.
