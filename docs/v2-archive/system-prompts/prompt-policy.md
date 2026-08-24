# Prompt Assembly Policy

Prompt precedence is fixed: host/system → developer/organization → trusted user/project instructions → current user message → untrusted contextual data. Repository `AGENTS.md` may constrain development behavior but never broadens capabilities or supersedes host policy.

Prompt bundle is versioned and hashed. Static prefix and stable tool schemas are kept byte-stable within a session to maximize provider prompt caching. Dynamic facts use small structured sections. Compaction output is data, not a new system instruction source.

System prompts describe desired model behavior; authorization, sandboxing, goal completion, secret handling and data egress are enforced in code.
