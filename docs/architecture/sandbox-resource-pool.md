# Architecture — Sandbox, Resource Pool and Execution Environments

## 1. Responsibility
Provide policy-selected isolation plus warm resource leasing for low-latency agents/builds/browser/mobile work.

## 2. Non-negotiable design rules
- Required isolation never silently downgrades.
- Warm resources are leases, not trusted peers.
- Release sanitizes or destroys according to prior trust/workload.

## 3. Components
- **SandboxManager** — backend selection
- **ResourcePool** — warm acquire/release
- **Image/SnapshotStore** — digest-pinned bases
- **WorkerRegistry** — remote identities/health
- **ResourceTelemetry** — provision phases and capacity

## 4. Canonical contracts
`SandboxSpec`, `ResourceSpec`, `EnvironmentLease {identity,generation,digests,expires}`, `ReleaseDisposition`, `WorkLease`.

## 5. Failure and recovery
Pool miss can provision if policy/budget allows. Worker disconnect marks owned nodes waiting/failed according idempotency. Interrupted package/install state is not returned healthy to pool.

## 6. Security and trust
No ambient host credentials; secret handles resolved per lease. Network/filesystem/env limits explicit. Remote workers authenticated and results digest-verified.

## 7. Implementation notes
Backends may include host-restricted, rootless container, platform OS sandbox, gVisor-like isolation, microVM and dedicated remote/mac/GPU workers. Capability discovery determines availability.

## 8. Required verification
- Unit/contract tests for typed boundaries.
- At least one negative/recovery test for every mutable or privileged path.
- Event/evidence assertions for user-visible state.
- No release claim without the acceptance evidence named by the implementation task.
