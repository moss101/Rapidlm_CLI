# Configuration Model and Precedence

## Precedence

Highest wins for ordinary settings:

`CLI flags > environment > user config > workspace config > defaults`.

Security is different: lower-trust scopes can only become *more restrictive*. Organization/host policy creates the maximum capability envelope. Workspace config cannot grant what user/organization policy denies.

## Files

- user: `~/.config/rapidlm/config.toml` (platform equivalent on Windows)
- workspace: `.rapidlm/config.toml`
- workspace policy: `.rapidlm/policy.toml`
- repository instructions: nearest `AGENTS.md`
- skills: `.rapidlm/skills/*/SKILL.md`
- local untracked overrides: `.rapidlm/config.local.toml`

Executable workspace config is ignored until project trust is granted.

## Example

```toml
schema = 1

[models]
default_policy = "balanced"

[context]
embeddings = "auto"
max_index_bytes = 2147483648

[agents]
max_parallel = 4
max_write_parallel = 2

[sandbox]
default_tier = "container"
network = "deny"

[telemetry]
mode = "local"
content = "off"

[daemon]
enabled = false
```

## Validation

Unknown keys produce warnings in interactive mode and errors under `rapid config validate --strict`. Secrets MUST be references (`env:NAME`, keychain IDs, provider credential handles), never accepted as cleartext in committed workspace config.

## V2 configuration surface

Example additive configuration (organization/host policy remains the maximum authority envelope):

```toml
[agents.pool]
enabled = true
max_background = 3
background_default_role = "read_only"
mailbox_max_messages = 512
mailbox_max_inline_bytes = 16384

[agents.managed]
max_parallel = 6
max_write_parallel = 2
clean_context = true
max_child_context_tokens = 48000

[handoff]
enabled = true
require_signed_bundle = true
quiesce_timeout_ms = 30000

[knowledge]
enabled = true
max_items_per_context = 8
max_tokens_per_context = 4000
suggestions = "review_required"

[playbooks]
automations_enabled = false
public_repo_event_triggers = "deny"

[trajectory]
local_collection = "evals_and_opt_in_sessions"
training_export = false
retain_days = 30

[computer_use]
semantic_targets_required_when_available = true
coordinate_fallback = "ask_on_sensitive"
recording = "on_for_e2e"
recording_redaction = "required"

[computer_use.desktop]
default_isolation = "gui_sandbox"
clipboard = "deny"
file_chooser_roots = ["${workspace}", "${artifact_staging}"]

[computer_use.vision]
max_full_screenshots_per_minute = 12
prefer_visual_delta = true
```

Validation rules:

- workspace config can lower concurrency, disable handoff/Computer Use/automations or tighten data policy but cannot broaden organization capability policy;
- `trajectory.training_export = true` is ignored/rejected unless the higher-trust data policy explicitly permits export;
- `computer_use.desktop.default_isolation` cannot select a weaker tier than host/organization policy;
- public repository/comment automation triggers remain denied unless a higher-trust policy explicitly enables the event class and filters;
- `clean_context = false` is experimental and cannot mean “clone raw parent transcript”; it may only select a larger bounded ContextPacket strategy.
