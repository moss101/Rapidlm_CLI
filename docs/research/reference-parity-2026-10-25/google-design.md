# Native Google implementation notes (REF-008, verified)

Primary docs checked 2026-10-26:

- https://ai.google.dev/api/generate-content
- https://ai.google.dev/gemini-api/docs/function-calling
- https://ai.google.dev/gemini-api/docs/thinking

The last page now primarily documents Interactions, which is a different protocol;
it explicitly distinguishes generateContent signatures attached to arbitrary parts.
The old thought-signatures URL redirects to it. Follow generateContent wire contracts,
not Interactions step envelopes. Do not assume a model capability from its name.

Implement a `google-genai` ConfigProvider and append its setup preset (existing menu
ordinals must remain stable). Google endpoint derives
`/models/{validated-model}:streamGenerateContent?alt=sse` from a validated v1beta base.
Use `x-goog-api-key` at the existing transport credential boundary. No key in query,
no credential in `ProviderHttpRequest` debug, no second socket/HTTP implementation.
Extend the fixed wire authentication style, prohibit caller header overrides, and
keep GET/raw HTTP behavior and proxy credential separation intact. Existing
`Http1Transport::{execute,execute_streaming}` share the HTTP decoder.

Map system messages to text-only systemInstruction, user/model contents with ordered
parts, inlineData images, functionDeclarations using parametersJsonSchema (verify API
support), functionCall/functionResponse batches, usageMetadata including cached tokens,
and typed STOP/MAX_TOKENS/block/refusal/malformed-function outcomes. Guard candidates,
body/event limits, cancellation and provider errors; discard raw error body content.
Progressive text callbacks must exclude `thought:true` parts and signature-only parts.

REF-028 reserves `ReplayProtocol::GoogleGenAi` and call-bound `GoogleSignature` but
has no Google extractor. Extend it for signed text/other required parts before shipping:
signatures are not restricted to function calls. Preserve exact signed part boundaries
and order (including a signature in a final otherwise-empty streaming part), and bind
it to the original canonical calls and provider/model/profile/endpoint scope. Avoid
introducing private thoughts into answer text or public exports. Tool results must be
paired to their original function name/id. Where a provider omits a call id, keep the
internal synthetic id separate from wire optional-id semantics. Use fixtures for this.

Exercise actual configured binary selection, API-key header (no bearer or URL key),
text and images, signed multi-tool replay after serialization/restart, streaming before
completion, cancellation and classified errors. Credentialed live provider checks are
separate evidence and have not been performed. Existing reference sources are behavior
samples only: do not execute or copy reference code.

## Implementation checkpoint

Implemented in `crates/llm-router/src/providers/google_genai.rs`. GoogleCall metadata
keeps original optional ids and signatures; GoogleText keeps signed/private text parts.
All native acceptance checks passed; exact logs/source hashes in validation.google_batch.
The initial binary attempt exposed the shared transport's query prohibition; request-only
query support now preserves configured-base validation and all origin/dial/header guards.
Cancellation testing also found and corrected the shared watcher starting in Drop.
The fixture project now grants its own read trust, preserving production trust checks.
No real provider account was contacted. REF-009 remains separate.
