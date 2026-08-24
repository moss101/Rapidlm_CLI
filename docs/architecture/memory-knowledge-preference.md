# Architecture — Memory, Knowledge, Rules, Skills, Playbooks and Preference Fabric

## 1. Responsibility
Separate episodic state, engineering facts, applicable instructions, procedures, graph templates, security authority and learned soft preferences.

## 2. Non-negotiable design rules
- Knowledge never grants permission.
- Preference never overrides explicit rules/policy/user intent.
- Durable learned objects carry scope, source, confidence and freshness/decay.

## 3. Components
- **MemoryStore** — episodic/session/user summaries
- **KnowledgeRegistry** — scoped facts/decisions/evidence
- **RuleResolver** — instruction applicability
- **SkillRegistry** — procedures
- **PlaybookRegistry** — parameterized graph templates
- **PreferenceFabric** — accept/reject/edit-derived soft signals

## 4. Canonical contracts
`MemoryItem`, `KnowledgeItem`, `Decision`, `RuleRef`, `SkillManifest`, `Playbook`, `Preference {proposition,evidence,scope,confidence,support,contradiction,last_seen,decay}`.

## 5. Failure and recovery
Conflicting knowledge/preferences remain explicit; do not silently choose latest. Stale knowledge excluded or labeled. Preference candidates require promotion threshold/eval.

## 6. Security and trust
Data-policy scopes govern persistence/export. Secrets/sensitive personal data excluded by default. Playbooks/skills cannot grant capabilities.

## 7. Implementation notes
Context Compiler retrieves each category under distinct token budgets and provenance. Preference affects ranking/planning softly, not completion truth.

## 8. Required verification
- Unit/contract tests for typed boundaries.
- At least one negative/recovery test for every mutable or privileged path.
- Event/evidence assertions for user-visible state.
- No release claim without the acceptance evidence named by the implementation task.
