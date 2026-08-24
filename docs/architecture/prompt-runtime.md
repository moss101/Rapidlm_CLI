# Architecture — Prompt Runtime and Instruction Composition

## 1. Responsibility
Compose deterministic, cache-friendly, versioned system/role/repository/skill/node/context instructions while preserving authority hierarchy.

## 2. Non-negotiable design rules
- Higher-authority instructions cannot be overridden by retrieved/user/project data.
- Stable prefixes and small tool surfaces improve cache/token efficiency.
- Prompt text is versioned/hashes recorded for eval reproducibility.

## 3. Components
- **PromptRegistry** — versioned prompt artifacts
- **PromptComposer** — layer precedence
- **RulesLoader** — AGENTS hierarchy/project rules
- **SkillLoader** — progressive selected skills
- **PromptBudgeter** — static/dynamic token accounting

## 4. Canonical contracts
`PromptLayer`, `PromptVersion`, `PromptComposition`, `RuleRef`, `SkillRef`.

## 5. Failure and recovery
Missing/malformed lower-authority project instruction is reported without weakening host policy. Compaction never removes core constitution/current goal contract.

## 6. Security and trust
Retrieved content is delimited/tagged as untrusted. Secret values do not enter prompt when handles suffice.

## 7. Implementation notes
See `system-prompts/system-prompt-composition.md`; prompts synthesize researched principles but are independently authored.

## 8. Required verification
- Unit/contract tests for typed boundaries.
- At least one negative/recovery test for every mutable or privileged path.
- Event/evidence assertions for user-visible state.
- No release claim without the acceptance evidence named by the implementation task.
