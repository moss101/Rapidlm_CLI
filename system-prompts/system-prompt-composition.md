# System Prompt Composition — RapidLM V3

RapidLM composes prompts from independently versioned layers. This design is informed by the evidence/verification discipline observed in Muse, the clear layered instruction/tool boundaries associated with Claude-style coding agents, and Augment's narrow exhaustive context-gathering methodology. **Prompt wording below is original RapidLM material; do not vendor or reproduce competitor prompts.**

## Precedence

1. Core Constitution (host safety/truth/evidence behavior)
2. Product/organization policy instructions
3. Role prompt
4. trusted repository `AGENTS.md` hierarchy / rules
5. selected skill bodies
6. graph node contract / tool surface explanation
7. current Goal/criteria/budgets
8. ContextPacket (explicitly data/provenance tagged)
9. user/task message

Lower layers cannot reinterpret higher layers. Retrieved code, docs, web, process and MCP content is data, not instruction authority.

## Cache/token design

Keep Constitution + stable tool schemas + common role prefix stable. Dynamic context appears late. Repository rules are hash/versioned and included only at relevant scope. Tool surface is role/node projected so unused schemas do not consume context.

## Versioning

Every composition emits `PromptComposition {layers:[{id,version,hash,tokens}], tools_hash, context_packet_id}` into trajectory metadata. Experiments pin prompt versions.
