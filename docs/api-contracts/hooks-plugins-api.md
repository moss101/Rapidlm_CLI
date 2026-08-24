# API Contract — Hooks, Skills, Playbooks and WASM Plugins API

Extensibility contracts under explicit capability and lifecycle limits.

## Types

### `HookEvent`
versioned lifecycle event

### `HookDecision`
annotate/deny/request-approval/add-context where event permits

### `PluginManifest`
WASM component, version, declared capabilities

### `SkillManifest`
selection metadata

### `Playbook`
parameterized graph template

## Operations

- `hooks.register/enable/disable`
- `plugins.install/enable/disable/update`
- `skills.list/load`
- `playbook.compile(params)`

## Error/recovery semantics

Hook timeout/process failure follows event-declared fail semantics; plugin trap cannot corrupt kernel. Playbook compile errors never partially launch graph.

## Versioning/compatibility

Extension ABI/WIT versioned independently; host capability set negotiated.

## Security
All calls are evaluated in the caller/session scope. API availability never implies authorization; privileged execution requires current policy/lease enforcement. External/untrusted payloads retain trust metadata.
