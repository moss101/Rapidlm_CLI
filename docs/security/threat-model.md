# V3 STRIDE / Data-Flow Threat Model (P12-001)

Trust boundaries: user terminal -> rapid CLI/TUI -> kernel (session/turn/ledger)
-> capability broker/policy -> tool gateway -> executors (process, sandbox,
computer-use, MCP/ACP/plugins, remote workers) -> external world.

| Threat | Vector | Control (verified) |
|---|---|---|
| Spoofing | forged daemon client | auth challenge/proof MAC per connection (ipc server gate) |
| Spoofing | forged remote worker result | signed WorkLease + digest inputs/results; no trust inheritance |
| Tampering | path escape via symlinks | CanonicalHostPath resolution + workspace confinement tests |
| Tampering | ledger corruption | atomic writes + journal reconcile + recovery classify |
| Repudiation | unlogged privileged action | append-before-apply Event Ledger for every transition |
| Info disclosure | secret leakage into model/output | SecretBroker one-use handles, redaction classes, sanitize layer, trajectory drops secrets |
| DoS | oversized frames/payloads | MAX_* byte bounds on every decoder; fuel/memory caps on WASM |
| DoS | process tree escape | process_group isolation + terminate_tree with grace escalation |
| Elevation | undeclared capability grants | broker intersect (grant_not_declared fails), Exclusive deny-by-default intents |
| Elevation | prompt-injection authority | page/tool text is UntrustedContent; FencedContent cannot grant leases |

Data flow: every external input enters as untrusted context labeled
Untrusted/Secret; authority flows only through CapabilityLease/ControlLease
issued by the broker after policy evaluation; evidence is written before apply.
Residual: P4-032 OS-native secure credential storage DEFERRED — file-backed
0600 keychain only; P4-GATE BLOCKED.
