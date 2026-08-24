# Testing Strategy

Test pyramid: pure types/state machines/property tests; service contract tests; SQLite/crash/replay; sandbox/process/workspace integration; scripted-model agent/eval scenarios; browser/computer fake + real supported integrations; protocol compatibility fixtures; adversarial security; long-horizon endurance. Every bug fix gets a regression proof when feasible. Avoid tests that merely restate implementation internals.
