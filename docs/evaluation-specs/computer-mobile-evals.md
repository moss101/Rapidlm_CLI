# Computer-Use and Mobile Evaluation Specification

## Browser tasks
Login-less web navigation, form filling, local dev-app testing, download handling, multi-tab flows, SPA state changes, accessibility-poor pages and prompt-injection pages. Record Playwright trace, screenshots and action log.

Metrics: completion, semantic-target rate vs coordinate fallback, actions/task, invalid-action recovery, origin-policy violations (target 0), screenshot bytes/tokens, wall time.

## Desktop tasks
Controlled fixture apps on macOS/Windows/Linux where supported: menus, dialogs, file chooser, clipboard-denied flow, accessibility tree changes, multi-window state. Coordinates are intentionally perturbed between observations to test stale-coordinate rejection.

## Mobile tasks
Android Emulator snapshot boot/reset, app install, deep link, rotation, permission dialog, text entry, network policy and screenshot verification. iOS tests run on macOS workers via `simctl`; Linux CI must mark iOS capability unavailable rather than silently emulate it.

## Deterministic evidence
Every successful task has a machine-verifiable terminal observation: URL/text/state, app data assertion, screenshot hash region, accessibility node condition, or test endpoint. Model prose does not count as task completion evidence.
