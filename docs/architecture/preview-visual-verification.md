# Architecture — Preview and Visual Verification

## 1. Responsibility
Own development preview lifecycle and convert application/runtime failures into structured graph evidence before raw Computer Use.

## 2. Non-negotiable design rules
- Preview is a supervised process/resource, not an ad-hoc shell convention.
- HTTP/compile/console/network failures become typed evidence.
- Visual verification requires fresh observation and criterion-specific assertions.

## 3. Components
- **PreviewSupervisor** — build/serve/readiness lifecycle
- **PortAllocator** — scoped dynamic port
- **BrowserBridge** — browser session binding
- **DiagnosticCollector** — compile/console/network/HMR
- **VisualEvidenceRecorder** — screenshots/video/structured states

## 4. Canonical contracts
`PreviewSpec`, `PreviewHandle`, `PreviewHealth`, `DiagnosticEvent`, `VisualAssertion`.

## 5. Failure and recovery
Failed startup returns exact failed phase and artifact log. HMR/proxy/redirect loops have dedicated detection. Reconnect after process/browser restart invalidates stale surface generation.

## 6. Security and trust
Preview-origin network access follows sandbox policy; browser content untrusted. User processes are never commandeered without explicit attach.

## 7. Implementation notes
Integrate with graph repair so compile/console errors can create diagnosis nodes automatically. Do not declare a visual criterion satisfied solely because a page loaded.

## 8. Required verification
- Unit/contract tests for typed boundaries.
- At least one negative/recovery test for every mutable or privileged path.
- Event/evidence assertions for user-visible state.
- No release claim without the acceptance evidence named by the implementation task.
