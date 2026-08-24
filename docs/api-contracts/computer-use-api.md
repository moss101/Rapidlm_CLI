# API Contract — Computer, Browser and Mobile API

Normalized Observation/action model across structured and pixel surfaces.

## Types

### `Observation`
surface/generation/viewport/scale/semantic targets/screenshot

### `ComputerAction`
move/click/down/up/drag/scroll/type/key/chord/wait/screenshot/cursor/app/window

### `BrowserAction`
navigate/query/DOM/AX/form/console/network/JS/download/upload

### `MobileAction`
install/launch/tap/type/key/swipe/observe/log

## Operations

- `computer.observe(surface)`
- `computer.act(observation,action)`
- `browser.act(session,action)`
- `mobile.act(device,action)`
- `control.take/release(domain)`
- `record.start/stop`

## Error/recovery semantics

Stale generation rejects action. Target transform mismatch rejects coordinate action. Sensitive action can require approval/takeover.

## Versioning/compatibility

Action enums versioned additively; backend capabilities negotiated.

## Security
All calls are evaluated in the caller/session scope. API availability never implies authorization; privileged execution requires current policy/lease enforcement. External/untrusted payloads retain trust metadata.
