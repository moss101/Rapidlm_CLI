# Event Catalog

Durable families and examples:
- session/run: `session.created`, `run.started|paused|resumed|completed|cancelled`;
- graph: `graph.revision.committed`, `node.ready|started|waiting|succeeded|failed|invalidated|superseded`;
- goal: `goal.created|updated|paused|blocked|completed|cleared`, `criterion.changed`;
- context: `context.compiled|compacted|invalidated`, `read.observed`;
- turn: `turn.started|interrupted|completed|failed`, `message.queued|state` (a message accepted while the model slot was busy, and its later state — `submitted`, `cancelled`, or `held` with the blocking hook's reason);
- model: `model.requested|completed|failed`, `model.continued` (one per request of a step that carried a length-truncated answer forward — `continuation_of` the step's request, `index` 0 for its first request, `tokens` that request's own; the step's `model.completed` carries their sum);
- agent mail: `agent.mail.sent` (a message to a running subagent, recorded before it is queued — `rapidlm.agent.mail/v1`: `message_id`, `to` (agent id), `from` (`user`), `delivery` (`interject`, `steer` or `queue`), `body` bounded to 16 KiB), `agent.mail.delivered` (`message_id`, `at`: `wait`, `turn_boundary` or `after_completion`), `agent.mail.dropped` (`message_id`, `reason`: `bounded`, `terminal_without_continue` or `unknown_agent`);
- tool: `tool.started|repaired|completed|failed`, `tool.context_required` (the call needs the human's answer; the turn pauses);
- policy: `approval.requested|resolved`, `lease.issued|revoked`;
- workspace: `transaction.proposed|applied|conflict|rejected`, `external_mutation.detected`;
- process/resource: `process.*`, `monitor.*`, `trigger.*`, `resource.phase.*`;
- computer: `observation.captured`, `computer.action.*`, `control.transferred`;
- evidence: `evidence.recorded|invalidated`, `verification.*`;
- handoff/remote: `handoff.*`, `worker.*`;
- mcp: `mcp.tools_loaded` (the model brought tools of a deferred MCP server into its tool list — `rapidlm.mcp.tools_loaded/v1`: `server`, `names[]` (the server's own tool names), `schema_digests[]` (SHA-256 of each canonical input schema), `definition_bytes[]`, `origin` (`model`|`restore`), `loaded_set_size`; names and digests only, never a description, schema or argument; appended before the next tool list is built; filed under the `tool` family) and `mcp.tools_unloaded` (`rapidlm.mcp.tools_unloaded/v1`: `server`, `names[]`, `reason` (`evicted`|`stale`|`server_offline`|`undeferred`));
- agent: `agent.selected` (the agent a session runs as — `rapidlm.agent.selected/v1`: `id`, `definition_digest` (SHA-256 of the definition, the same in either syntax), `mode` (`primary`|`both`); appended when a session begins as an agent, and again when a resumed session's agent has changed since it was recorded — the newest one is the session's agent; EXT-05);
- skill: `skill.activated` (a skill's instructions entered the conversation — `rapidlm.skill.activated/v1`: `id`, `root_class` (`user`|`project`|`plugin`), `body_digest`, `args_digest` (absent without arguments), `origin` (`model`|`user`), `generation`; appended before the text reaches the context; filed under the `tool` family);
- extension: `hook.decided` (a v2 hook decision — `rapidlm.hook.decision/v1`: hook, stage, decision, reason digest, tool, call), `hook.input_rewritten` (a hook replaced a call's arguments — `rapidlm.hook.rewrite/v1`: hook, contributors, tool, call, both inputs and their digests), `hook.failed` (a hook in a fail-open stage failed and was ignored — `rapidlm.hook.failure/v1`: hook, stage, detail digest; ADR 0022), `hook.stop_continued` (a `stop` hook that opted in with `continue_turn` sent the turn back for another section — `rapidlm.hook.stop_continued/v1`: `hook` (positions, e.g. `stop[0]`), `continuation_index` (1 for the first), `limit`, `reason_digest`, `reason` (at most 2 KiB), `answer` (what the section ended with, scrubbed, at most 4 KiB) and `answer_truncated`; appended before the next section starts), `hook.stop_continuation_exhausted` (a bound ended the loop with a hook still asking to go on — `rapidlm.hook.stop_continuation_exhausted/v1`: `hook`, `limit`, `reason` `count` or `repeat`), `hook.dropped` (observer-stage events were discarded because the bounded queue of 32 was full — `rapidlm.hook.dropped/v1`: `stage`, `count` since the last such record; recorded in the ledger of the session whose events were lost — the first drop at once, later ones together at most once per 60 seconds, and what is left when the process exits), `plugin.*`, `mcp.*`, `acp.*`;
- notification: `notification.recorded` (something the user is told outside the transcript — a loop's result: `source`, `text` bounded to 1 KiB, and the `loop_id`/`outcome` it came from; projected by the TUI as notices, never into the conversation);
- eval/release: `eval.*`, `experiment.*`, `release.*`.

Token streaming deltas need not be durable events unless export/replay policy requires them.
