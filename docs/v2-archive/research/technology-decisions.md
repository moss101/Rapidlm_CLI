# Technology Research and Design Implications

## Rust runtime

Baseline pin: Rust 1.97.1 (released 2026-07-16). The runtime’s process, terminal, SQLite, parser, and sandbox responsibilities favor a memory-safe native implementation and single-binary distribution.

Source: https://blog.rust-lang.org/2026/07/16/Rust-1.97.1/

## TypeScript SDK

Node 24.19.0 LTS is the baseline runtime for the public SDK/tooling. pnpm 11.20 is the package manager baseline; the repository should pin an exact patch and use supply-chain protections including lockfile integrity and delayed adoption of newly published packages where compatible.

Sources:

- https://nodejs.org/en/blog/release/v24.19.0
- https://pnpm.io/blog/releases/11.20
- https://pnpm.io/supply-chain-security

## Tree-sitter + LSP

Tree-sitter provides incremental concrete syntax trees, which makes it appropriate for fast local symbol/chunk maintenance. LSP provides richer language-specific definition/reference/type relationships and should be an enrichment layer, not a hard dependency.

Sources:

- https://tree-sitter.github.io/
- https://microsoft.github.io/language-server-protocol/

## SQLite FTS5 + rebuildable vector cache

SQLite is chosen as the durable local metadata/event/projection store because it provides mature transactional semantics and FTS5. Vector search is intentionally a rebuildable side index keyed by canonical chunk IDs; corruption/loss must not corrupt sessions. The embedding provider is pluggable and optional.

## Playwright for browser control

Playwright browser contexts provide isolated sessions and its trace tooling gives reproducible browser evidence. RapidLM wraps Playwright behind its own `BrowserSupervisor` to preserve policy and artifact contracts.

Sources:

- https://playwright.dev/docs/browser-contexts
- https://playwright.dev/docs/trace-viewer

## Android/iOS simulator control

Android Emulator CLI supports AVD lifecycle and snapshots; ADB is the device-control substrate. Apple documents `xcrun simctl` as the CLI to control Simulator. Because iOS Simulator is tied to Xcode/macOS, non-mac hosts must use a remote macOS worker instead of pretending local parity.

Sources:

- https://developer.android.com/studio/run/emulator-commandline
- https://developer.apple.com/documentation/xcode/xcode-command-line-tool-reference

## Sandbox ladder

gVisor interposes a user-space application kernel and supports KVM/Systrap platforms, giving a stronger Linux isolation option than ordinary containers with different compatibility/performance trade-offs. Firecracker uses KVM microVMs plus a jailer and is appropriate for remote/hosted high-risk workloads, not a mandatory local dependency.

Sources:

- https://gvisor.dev/docs/architecture_guide/intro/
- https://firecracker-microvm.github.io/

## MCP and ACP

The 2026-07-28 MCP specification moves to a stateless core and deterministic/cacheable list responses. RapidLM targets it while normalizing all tool invocations through internal policy. ACP uses JSON-RPC and supports editor-agent interactions; RapidLM should negotiate v1/v2 for ecosystem compatibility.

Sources:

- https://modelcontextprotocol.io/specification/2026-07-28
- https://agentclientprotocol.com/protocol/v2/overview
- https://agentclientprotocol.com/protocol/v2/migration

## V2 managed-agent and trajectory decisions

### Persistent background agents + clean managed workers

Muse Code's published architecture motivates a session-long pool for specialized async background agents, while Devin's managed-agent workflow motivates clean-slate isolated workers for bounded implementation tasks. RapidLM deliberately implements **both classes**: persistent agents amortize exploration/read state; managed workers preserve task focus and write isolation.

Sources:
- https://research.meta.ai/blog/introducing-muse-code-and-muse-spark-1-2
- https://cognition.ai/blog/devin-can-now-manage-devins

### Execution handoff

Devin CLI demonstrates the product value of moving local context/branch/uncommitted changes into a remote cloud session. RapidLM generalizes the idea to local/daemon/remote workers and adds a durable generation-fenced execution lease so transfer failure cannot create two write owners.

Source: https://docs.devin.ai/cli/handoff

### Full desktop Computer Use

Devin's public Computer Use material validates full desktop operation (mouse, keyboard, screenshots and native/web applications), not browser-only automation, and its testing/Android material combines visual interaction with video evidence and deterministic ADB side channels. RapidLM uses platform accessibility APIs as the semantic layer, vision/coordinates only as fallback, and binds visual results into the Goal/Evidence DAG.

Sources:
- https://docs.devin.ai/work-with-devin/computer-use
- https://docs.devin.ai/work-with-devin/testing-and-recordings
- https://docs.devin.ai/onboard-devin/environment/android-emulation

### Training-ready observable trajectories

Muse Code / Muse Spark's published harness co-training and long-horizon work motivates preserving versioned observable trajectories from the beginning. RapidLM does not require or attempt to recover hidden chain-of-thought; it stores observable model/tool boundaries, selected context, changes, evidence, metrics and data-policy labels and gates any experimental promotion on held-out evaluation.

Source: https://research.meta.ai/blog/introducing-muse-code-and-muse-spark-1-2
