# Configuration Schema and Precedence

Layers: compiled defaults/safety → organization managed policy → user global → trusted project → session/CLI overrides where permitted. Lower layers cannot broaden higher security ceilings.

Configuration sections: runtime/daemon, providers/models/router, context/indexing, graph/scheduler, agents/prompts/skills, tools/repair, workspace/VCS, policy/approvals, sandbox/resources, process/jobs/triggers, computer/browser/mobile/preview, MCP/ACP/plugins/hooks, memory/knowledge/preferences, telemetry/eval, update/release.

Every config load produces `ConfigGeneration` and source metadata; changes invalidate dependent caches/projections. Executable project extensions remain disabled until trust.


Native cloud profiles are specified in [model configuration](../reference/model-configuration.md).
`vertex-genai` and `vertex-anthropic` require explicit project/location and OAuth
credentials. `[policy] allow_credential_processes` is a managed boolean ceiling,
default false. It controls ADC/configured executable credential helpers for every
primary, fallback and phase model; it does not enable agent shell tools.
`[provider_accounts.<id>]` holds explicit OAuth registrations; `[model.<id>] account`
binds one to the same provider and resource. Managed `[policy] allow_provider_accounts`
defaults to false and gates account login, refresh, and model use. Credentials remain
in the OS keychain. See the model configuration reference for the registration fields.

Azure Chat/Responses profiles explicitly bind a resource API prefix, deployment in
`model`, `api_version`, `azure_auth` and `azure_url_mode`. Azure Entra helpers share
the managed process ceiling. `azure_legacy_max_tokens` selects the older Chat output
limit field when configured. See the same model configuration reference for bounds.

Bedrock uses native ConverseStream with explicit `region` and `aws_auth` (SigV4 by
default). `aws_profile` delegates an installed CLI profile and shares the managed
credential-process ceiling; otherwise signing reads explicit AWS environment keys.
Bearer auth uses an explicit token source. `bedrock_resource` carries long native
ARNs without changing canonical model-id bounds. `bedrock_additional_fields` and
`bedrock_effort` carry bounded native controls. Signing authority has dedicated
`aws_signing` credential metadata and cannot be resolved as a bearer token.

Copilot Chat/Responses profiles bind an explicit API style, source-vs-inference token
mode, token endpoint and integration ID. `copilot_auth = "github-token"` exchanges a
configured source token lazily through a separately guarded GET; `api-token` sends an
already exchanged token. Model-private replay includes this binding. Bounded helpers
share the same managed ceiling. Setup/doctor permit only the finite configured model
and auth origins, and never run a helper or exchange credentials while offline.
