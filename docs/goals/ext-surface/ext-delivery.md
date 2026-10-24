# EXT-SURFACE-01 — delivery record

Worklist: [`ext-implementation-tasks.json`](ext-implementation-tasks.json). Specs: [`README.md`](README.md). Rules: SEAMS §2 (naming, clean room), §3 (invariants), §5 (delivery protocol). Baseline commit for the specs: `05ad9d6`.

Each slice records: the Phase 0 corrections found before code was written, a criterion table (criterion / status / evidence with test names), the revert cycles, and the checks run.

---

## EXT-02-0 — the remote transport works

Spec: [EXT-02](EXT-02-mcp-remote-authorization.md) Part A (AC-01…05).

### Phase 0 corrections to the spec's "Today"

| Claim | Finding |
|---|---|
| Defect A: `tools/call` over HTTP cannot work | **Narrowed.** `tools_list` and the plain `tools_call` already worked over HTTP through the transport's `exchange_skipping_notifications` override. What failed was `tools_call_with` (the production tool-call path) and the acknowledgement of `notifications/initialized`, because `send_frame` discarded the reply and `recv_frame` always returned `InvalidFrame`; a `202` was a `HandshakeFailed`. |
| Defect B: a configured `Authorization` header cannot work | Confirmed. Also found: the shared writer emitted `Authorization: Bearer ` (empty) on every request to a server with no credential configured. It now sends no header. |
| Frame bounds | Confirmed: production HTTP io bound is 64 KiB per frame (`IoBounds::new(64 * 1024, …)`), 256 KiB for a whole stream. `MAX_FRAME_BYTES` itself is 256 KiB. |
| "Stdio only" in five places | Confirmed (`cli-command-reference.md`, `mcp_admin.rs` usage, `mcp_config.rs` message, `interactive.rs` `/mcp auth` message, `transport.rs` comment) plus two tests; all corrected. |
| `RapidHttpIo` watcher | Not in the spec: each exchange spawned a cancel-bridge thread that polled every 20 ms until the process ended (a leak). It now ends with the exchange. |
| Redaction of the bearer value | The spec says "registered with the redaction registry". The registry's snapshot is built once by the caller and cannot take a later value, so the connection instead carries its credential values and replaces them in MCP tool results (`credential_values`, `redact_secrets`); error texts never contain them by construction. |
| `McpSession::exchange_skipping_notifications` | Dead private duplicate of the trait default; removed (its rationale moved to the trait). |

### Criteria

| Criterion | Status | Evidence |
|---|---|---|
| AC-01 JSON and event-stream `tools/call`, `202` notification | met | `crates/mcp`: `http_tools_call_with_returns_a_json_reply`, `http_tools_call_with_reads_an_event_stream_reply`, `http_tools_call_over_an_event_stream_skips_other_events`, `http_server_request_inside_a_stream_is_answered_with_a_post`, `http_unread_frames_of_an_earlier_exchange_do_not_leak_into_the_next_call`, `http_reply_without_a_matching_response_is_an_invalid_frame`, `http_frame_bounds_apply_to_json_bodies_and_to_each_stream_event`, `event_stream_parsing_follows_the_line_and_event_rules`, `content_type_decides_framing_not_the_body_shape`. `rapid`: `tools_call_over_http_returns_a_json_reply_and_an_event_stream_reply` drives `connect_mcp_server` against a loopback stub over a real socket |
| AC-02 `Authorization` verbatim; `bearer_token_env`; redaction; typed missing variable | met | `llm-router`: `post_raw_authorized_*`, `post_raw_keeps_writing_one_bearer_header`. `rapid`: `a_bearer_token_from_the_environment_is_sent_once_and_never_printed`, `a_configured_authorization_header_is_sent_verbatim_and_without_one_none_is_invented`, `a_missing_bearer_variable_is_a_typed_error_that_names_the_variable`, `an_mcp_result_that_echoes_a_credential_does_not_carry_it_to_the_model`, `redaction_replaces_longest_values_first_and_leaves_short_text_alone`. Config: `a_remote_entry_may_name_the_variables_that_hold_its_credentials`, `a_bad_bearer_variable_name_is_rejected_by_kind`, `two_credentials_for_one_server_are_refused`, `headers_env_is_validated_entry_by_entry` |
| AC-03 401/403 distinguished; challenge kept, parsed, bounded | met | `http_401_is_unauthorized_and_keeps_a_parsed_challenge`, `http_403_is_insufficient_scope_only_when_the_challenge_says_so`, `www_authenticate_parsing_is_bounded_and_tolerant` (oversized, malformed, control characters, duplicate, case); `rapid`: `a_401_is_reported_as_authorization_required_not_as_a_broken_handshake` |
| AC-04 every remote connection leaves a receipt; denied host refused and receipted | met | `every_remote_dial_leaves_an_egress_receipt` (one receipt per POST in `.rapidlm/egress-receipts.jsonl`); `a_configured_remote_name_cannot_reach_a_private_address` (refusal receipted for loopback, private and link-local answers to a public name) |
| AC-05 install output loads; `add --url`; no secret in argv; stdio-only statements gone | met | `an_installed_remote_entry_loads_under_the_project_loader`; `add_url_writes_a_remote_entry_that_loads_and_holds_no_secret`, `add_url_refuses_what_the_loader_or_a_secret_policy_would_reject_and_writes_nothing`, `add_url_accepts_plain_http_only_for_a_local_address`, `add_url_is_refused_in_an_untrusted_project_and_by_the_managed_policy`, `a_stdio_entry_is_listed_exactly_as_before`, `an_unsupported_remote_kind_names_both_supported_transports` |

S11: a stdio entry's `rapid mcp list` line is byte-identical (`a_stdio_entry_is_listed_exactly_as_before`); a remote entry gains ` transport=http auth=…` naming credential sources only.

### Revert cycles (each mutation applied to the fixed code, the named test failed, the file restored and touched)

| Mutation | Test that failed |
|---|---|
| `recv_frame` back to always `InvalidFrame` | `http_tools_call_with_returns_a_json_reply`, `…_reads_an_event_stream_reply`, `http_server_request_inside_a_stream_is_answered_with_a_post` |
| `202` not a success | eight transport tests, including `http_tools_call_with_returns_a_json_reply` |
| `401` a plain handshake failure | `http_401_is_unauthorized_and_keeps_a_parsed_challenge` |
| event streams read as JSON | `http_tools_call_with_reads_an_event_stream_reply`, `content_type_decides_framing_not_the_body_shape` |
| any `403` is insufficient scope | `http_403_is_insufficient_scope_only_when_the_challenge_says_so` |
| queue not cleared on a new request | `http_unread_frames_of_an_earlier_exchange_do_not_leak_into_the_next_call` (the first version of the suite did not catch this; the test was added after the mutation survived) |
| router ignores the given `Authorization` | `post_raw_authorized_sends_the_given_value_verbatim_and_once` |
| router invents a bearer for none | `post_raw_authorized_without_a_value_sends_no_authorization_header` |
| `Accept: application/json` only | `tools_call_over_http_returns_a_json_reply_and_an_event_stream_reply` |
| bearer dropped | `a_bearer_token_from_the_environment_is_sent_once_and_never_printed` |
| receipts not recorded | `every_remote_dial_leaves_an_egress_receipt` |
| gate always allows local addresses | `a_configured_remote_name_cannot_reach_a_private_address` |
| install omits `type` | `an_installed_remote_entry_loads_under_the_project_loader` |
| missing variable ignored | `a_missing_bearer_variable_is_a_typed_error_that_names_the_variable` |
| two credentials allowed | `two_credentials_for_one_server_are_refused` |
| redaction removed / Authorization values skipped / shortest first | the two redaction tests |

### Checks

`cargo fmt --all -- --check` clean; `cargo clippy --workspace --all-targets --locked -- -D warnings` clean; `cargo test --workspace --locked --no-fail-fast`: 4792 passed, 2 failed — `llm-router`'s `the_address_is_resolved_and_guarded_before_the_credential_is_looked_at` (a real regression of this slice: the credential check had moved ahead of address planning; fixed, `cargo test -p llm-router` 179/179) and `interactive::tests::computer_observe_reports_the_typed_platform_gate_not_a_stub` (the host-dependent failure already recorded under HYG-001 and in the SEAMS delivery record: the real desktop host answers "backend"; it fails the same way on committed HEAD and alone). After the fix `cargo test -p rapid --lib mcp_` 104/104; `cargo test -p protocol --test schema_fixtures`, `pnpm generate:check`, `pnpm typecheck`, `pnpm test` green.

### Decisions and follow-ups

- `rapid mcp add --header-env NAME=VAR` needed a settings key for a header value taken from the environment; this slice adds `headers_env` (header name → variable name) beside `bearer_token_env`. Both are resolved at connect; a missing or empty variable is a typed `credential unavailable` error naming the variable.
- `bearer_token_env` names are upper-case `[A-Z_][A-Z0-9_]{0,63}`, so a typo cannot silently name an unrelated variable such as `path`.
- Left for EXT-01 slice 2: the per-call watchdog thread in `execute_mcp_tool` (R-6) is still detached for its full 30 s; it moves with the timeout work.


### Review follow-up for EXT-02-0 (self-review of `c5469d6`)

The background adversarial review of the commit found real defects; each was verified in the code and fixed in the follow-up commit, with its own revert cycle (15 mutations).

| Finding | Fix | Test |
|---|---|---|
| **HIGH** `bearer_token_env`/`headers_env` let a trusted project file send any upper-case environment variable (an API key) to a project-chosen URL | A project-declared server may read only `MCP_`-prefixed variables, or ones the user lists in `RAPIDLM_MCP_CREDENTIAL_ENV` in their own environment; refused before any request; `rapid mcp add` applies the same rule | `a_project_cannot_read_an_arbitrary_variable_for_a_credential`, `add_url_accepts_a_variable_the_user_lists_in_their_own_environment` |
| **MEDIUM** plain `http://` to a non-local host carried a credential | the loader rejects a credential on plain `http` unless the host is `localhost` or a loopback/private IPv4 address | `a_credential_for_a_plain_http_url_off_this_network_is_refused` |
| `rapid mcp add` echoed a token pasted into `--bearer-env` / `--header-env` | none of the refusals repeats its operand | `add_url_refuses_…` (no-echo cases) |
| A remote server's `elicitation/create` was put in front of the user although the session never declares it | answered with an error for HTTP sessions | `a_remote_servers_request_for_input_is_refused_not_put_to_the_user` |
| A refusal by the transport's own egress policy (or an off-origin redirect) left no receipt; the gate wiring was unpinned | a refusal receipt for policy denials; gate construction extracted (`http_io_for`) and tested | `a_redirect_off_the_configured_origin_is_refused_and_leaves_a_refusal_receipt`, `a_remote_gate_built_for_a_configured_name_refuses_private_answers` |
| Redaction gaps: a credential straddling a cap kept its prefix; only `Authorization`/env headers counted; tool descriptions unredacted | redact before every cut; every configured header value counts; descriptions redacted | `a_credential_straddling_a_result_cap_is_redacted_before_it_is_cut`, `redaction_replaces_longest_values_first_and_leaves_short_text_alone`, `a_tool_description_that_carries_a_credential_does_not_reach_the_model` |
| `add --url` accepted URLs `connect` cannot use (IPv6 literals, odd host characters, `x.localhost`) | `add` and the local-literal rule now match what the transport reaches (`localhost`, IPv4 loopback/private) | `add_url_refuses_…`, `a_configured_local_literal_is_dialled_and_drain_forgets_what_it_returned` |
| A second `WWW-Authenticate` line (e.g. the `insufficient_scope` one) was lost; oversize-challenge comments untrue | every challenge line is joined; comments corrected; bounds pinned at 1024/2048 inclusive | `every_challenge_line_counts_when_a_403_names_the_scope_on_the_second`, `the_challenge_bounds_are_inclusive` |
| An SSE event with an empty `data:` dispatched an empty frame | not dispatched | `an_event_with_an_empty_data_field_is_not_dispatched` |
| `list`/`get` printed a URL's query | shown as `?…` | `a_remote_url_is_shown_without_its_query_or_fragment`, `a_report_shows_a_url_without_the_part_that_can_carry_a_token` |

**Recorded, not changed here.**
- *R-3 is confirmed.* An MCP success is returned through `bounded_detail`, so the model sees at most 256 bytes of a tool result (the 20 KiB `MCP_RESULT_CAP` is never reached). Other tools use larger bounds (`read` 4 KiB, `shell_exec` 16 KiB). Raising it changes what every MCP user's model sees and costs, so it is a decision for the maintainer; the straddle test covers both caps so it stays meaningful when it changes.
- A server that keeps an event stream open after sending its response costs the full tool timeout, because the whole body is read before frames are returned (the MCP specification says a server SHOULD close the stream). True incremental reading belongs with EXT-01 slice 2's timeout work.
- `.claude/settings.json` remains named in files this work touches: it is a real path the loader reads for compatibility, not a product reference. The `Co-Authored-By` trailer is the harness-required attribution.

---

## EXT-03-1 and EXT-03-2 — hook object form, matcher, timeout, dedupe, managed ceiling; process-group termination

Spec: [EXT-03](EXT-03-hook-stage-extensions.md) §3.1–3.3, §3.6 (partly), AC-01…06, 17 (partly). The two slices share `hooks.rs` and landed in one commit.

### Phase 0 corrections to the spec's "Today"

| Claim | Finding |
|---|---|
| Twelve stages; three parallel lists | Confirmed. `named_stages_mut` is now the one place a stage is named for mutation; `stages_mut`/`named_stages` remain. |
| Both project settings files merged by plain append | Confirmed, plus: the managed policy's own hooks are combined in `gate_hooks`, a second merge point. Deduplication runs at both (`dedupe_and_cap` in the loader, `dedupe_only` in the gate), so an entry the organisation and the project both declare runs once, the organisation's first. |
| `regex` available to `apps/rapid` | Confirmed (direct dependency). |
| The notify-style runner (`run_notify_hooks`) | Needed a subject: `subagent_start` matches on the agent type. The other notify stages (`session_*`, `pre/post_compact`) have none and now pass `None`. |
| Managed `max_timeout_ms` | The spec lowers "every ceiling above"; the 5 s default is also a ceiling when the managed value is lower, so an entry with no `timeout_ms` is clamped too (and counted in the report). |
| Platform without group termination | Both supported families (Unix, Windows) end the group, so the refusal is reachable only through `timeout_allowed(.., false)`; it is tested through `parse_object_entry_for(.., false)` rather than a target `cfg`. |
| D-1 (older binaries skip object entries) | Implemented as recommended: documented, and the doctor row notes how many entries use the form. It is a note on a passing row, not a warning, because a warning that cannot be silenced would fire for every project that uses the feature. |

### Criteria

| Criterion | Status | Evidence |
|---|---|---|
| AC-01 string-only configuration is byte-identical | met | every pre-existing hook test passes unchanged (the 52 struct-literal sites changed only by wrapping in `entries(...)`); `a_settings_file_of_strings_only_parses_exactly_as_before`; the rapid package run (1575 lib + integration) is green |
| AC-02 object-form parsing and rejections, with position, in the per-turn warning and `rapid doctor`; cap counts the merged list | met | `object_form_entries_parse_with_every_key`, `every_bad_object_entry_is_rejected_with_its_position_not_skipped` (14 cases), `a_matcher_needs_a_stage_with_a_subject_and_a_long_one_is_refused`, `the_project_loader_merges_both_settings_files_dedupes_and_reports_rejections`, `a_turn_warns_once_per_rejected_hook_entry_and_bounds_the_report` (bounded at 16 plus a count), `the_hooks_check_lists_rejected_entries_and_notes_object_form_ones` |
| AC-03 matcher semantics; no record and no process for a skipped hook | met | `a_matcher_matches_the_whole_subject_not_a_substring`, `a_hook_whose_matcher_does_not_select_the_tool_does_not_run_at_all` (marker file), `observer_and_notify_stages_honour_the_matcher_and_the_timeout_too` (tool name, agent type, stop reason) |
| AC-04 per-hook timeouts; stage maxima; managed clamp reports field, origin, remediation | met | `a_per_hook_timeout_replaces_the_default_for_that_hook_only`, `the_stage_class_decides_the_longest_timeout`, `a_managed_ceiling_on_hook_time_clamps_what_exceeds_it_and_says_so` |
| AC-05 a grandchild does not outlive the timeout (Unix); typed refusal where group termination is missing | met | `a_timed_out_hook_leaves_no_child_process_behind` (a plain and a TERM-ignoring grandchild), `a_long_timeout_needs_a_platform_that_can_end_a_hooks_children` |
| AC-06 dedupe across both files and the policy; an approval for the bare hook still answers the decorated one | met | `merging_dedupes_identical_entries_and_caps_the_result`, `identical_managed_and_project_entries_run_once_with_the_managed_one_first`, `adding_a_matcher_and_timeout_does_not_change_which_approval_a_hook_answers`, `a_skipped_hook_keeps_its_place_so_the_ones_after_it_keep_their_names` |
| AC-17 managed (partly) | partly | object-form managed hooks run first and a bad one refuses the policy (`the_organisations_hooks_may_use_the_object_form_and_a_bad_one_refuses_the_policy`); `denied_events` for the new stage names arrives with slice 3, `max_stop_continuations` with slice 4 |

### Revert cycles

Slice 1 (17): unanchored matcher; matcher always applies; per-hook timeout ignored; unknown keys accepted; one global timeout maximum; matcher accepted on a subject-less stage; no deduplication; skipped hooks renumbered; `continue_turn` accepted off `stop`; notify stages ignore the matcher; `stop_cancelled` matched without its reason; managed clamp removed; managed object entries unvalidated; managed/project duplicates both kept; loader dedupe replaced by truncation; warning cap removed; doctor omits rejected entries. Slice 2 (3): leader-only kill; TERM without the final group KILL (in `process-signal`, restored); platform guard removed. Each fails the test named in the criteria table above.

### Checks

`cargo fmt --all -- --check` clean; `cargo clippy --workspace --all-targets --locked -- -D warnings` clean; `cargo test -p rapid --locked --no-fail-fast` green (1575 lib, plus every integration binary); the change touches only `apps/rapid` and docs. `docs/reference/hooks.md` gains the object-form section; the six drift corrections wait for EXT-03-5.

### Review follow-up for EXT-03-1 and EXT-03-2 (self-review of `79b61d9`)

The background adversarial review found real defects; each was verified and fixed with a revert cycle (11 mutations).

| Finding | Fix | Test |
|---|---|---|
| **HIGH** every hook (plain ones included) left the caller's process group: a terminal Ctrl-C, a supervisor's group kill and terminal-reading hooks behaved differently; the commit's "unchanged" claim was false | only a hook whose entry sets its own `timeout_ms` is isolated (and its whole group ended on timeout); a plain hook is exactly as before | `only_a_hook_with_its_own_timeout_leaves_the_callers_process_group` |
| **MEDIUM** regex compile cost not bounded by the stage cap (a rejected object still compiled its expression; rejections unbounded) | cheap checks first, compile last; 256 KiB compile bound; at most 32 entries read per stage per file; at most 64 rejections kept | `a_cheap_reason_to_reject_is_found_before_the_expression_is_compiled`, `a_long_array_is_read_only_so_far_and_says_so` |
| an unbalanced matcher (`shell_exec)\|(x`) escaped its anchors | the expression must compile on its own before it is wrapped | `an_unbalanced_matcher_cannot_escape_its_anchors` |
| managed: a blank object `command` was accepted; entries differing only above the ceiling both survived (dedupe ran before the clamp) | blank rejected; clamp first, then dedupe | `a_command_of_only_spaces_is_not_a_command`, `a_blank_object_command_in_the_policy_refuses_it`, `entries_that_differ_only_above_the_ceiling_are_one_hook_after_the_clamp` |
| doctor listed 3 rejections while the warning said "run `rapid doctor` for the full report"; the `continue_turn` comment claimed behaviour that is not built yet | doctor lists the same 16 the turn warns about; wording made true | `the_hooks_check_lists_as_many_rejections_as_the_turn_warns_about` |
| vacuous tests: the observer test never set a timeout; `post_tool_use_failure` and `subagent_stop` matchers untested; a 400 ms timeout raced the pid files; a test wrote `done.txt` into the crate directory | timeouts and both subjects now tested; 1.5 s budget; the file goes in the temp dir | `a_timeout_of_its_own_bounds_an_observer_and_a_notify_hook_too`, `the_failure_and_subagent_stop_stages_match_on_their_own_subjects` |
| docs: dedupe also applies to plain strings; positions in warnings and ledger names are different index spaces; older binaries refuse a managed policy that uses the new keys | all three stated in `docs/reference/hooks.md` | — |

**Recorded, not changed.** Hook execution still takes no cancellation: Ctrl-C and `--max-wall-time` do not interrupt a running hook, and the larger ceilings (120 s `pre_tool_use`, 600 s `stop`) make that more visible; the cancellable wait arrives with the `stop` continuation (EXT-03-4), which needs it. A rejected `pre_tool_use` entry fails open with a warning and a doctor line, as the spec's "reject, not skip" defines it (the managed path refuses the whole policy). Windows has no test of hook group termination.


---

## EXT-03-3 — observer stages: `permission_requested`, `permission_resolved`, `notification`

Spec: [EXT-03](EXT-03-hook-stage-extensions.md) §3.4, AC-07…09.

### Phase 0 corrections to the spec's "Today"

| Claim | Finding |
|---|---|
| Six approval request sites | Confirmed, and they reduce to **two shapes**: four inside the tool runner (lattice ask, hook ask, plan, MCP request for input — all in `exec_tools.rs`, where the session's hook configuration is at hand) and two outside it (the clarification recorded in `record_outcome_suspension`, now `record_clarification`, and the workflow human wait). |
| Four resolution surfaces | Three code sites serve four surfaces: the terminal UI's `resolve_pending`, `acp_resolve_and_continue` (ACP and the daemon — each now passes its own `surface`) and `rapid run --resolve` (`resolve_human_wait`). The daemon's other `ResolveApproval` constructions are test code. |
| `notification` kinds | Six kinds and six sites, all found: `turn_complete` (both turn-end paths), `agent_finished` (a continued child's report, and a detached child's end), `job_finished` and `monitor_flooded` (the job recorder), `loop_result` (`record_loop_notification`), `goal_changed` (`/goal pause` and the autonomous goal loop's pause/block/complete). None had to be dropped. |
| "Observer hooks run on a bounded worker" | One process-wide queue and worker thread started on first use. A process that exits with events queued would lose them, so `main` waits up to ten seconds for the queue to drain (`a_hook_ask_reaches_the_permission_requested_hook_before_the_process_exits` runs the real binary to prove it). |
| `session_id` in the payload | The thread that fires an event often has no run identity (a slash command on the UI thread, the worker), so the recorder supplies it (`HookEvents::session`); the thread's identity adds `turn_id`. |
| Resolved request lookup | The request is already resolved when the resolution is observed, so the tool name is read from the record (`recorded_request`), not from the pending set — the first version of the terminal-UI test found this. |

### Criteria

| Criterion | Status | Evidence |
|---|---|---|
| AC-07 `permission_requested` fires once per request with the documented payload and `source` at each site; `permission_resolved` from each surface with the right `surface` | met except the plan site's own firing | lattice, hook ask, MCP, clarification, workflow: one test each (names in the worklist). The plan site calls the same helper as the other three; its `source` label is tested (`a_plan_proposal_request_is_labelled_plan`), but no unit test drives a plan proposal to approval. Resolutions: `the_tui_reports_an_approval_and_a_denial_to_the_resolved_stage`, `the_acp_and_daemon_paths_each_report_their_own_surface`, `a_workflow_resolution_reports_the_workflow_surface` |
| AC-08 `notification` fires once per kind at its site; the matcher selects by kind; 100 monitor lines fire nothing; `notification.recorded` unchanged | met | `each_notification_kind_fires_once_at_its_site` (all six, once each), `a_notification_matcher_selects_by_kind`, `a_monitor_that_prints_a_hundred_lines_fires_no_notification`, `a_turn_that_did_not_complete_is_not_a_turn_complete_notification`, `a_detached_child_ending_fires_an_agent_finished_notification`, `slash_goal_pause_fires_a_goal_changed_notification`; the existing `notification.recorded` tests pass unchanged |
| AC-09 a slow observer does not delay the emitter; overflow drops the oldest and records `hook.dropped` with the count | met | `a_slow_observer_does_not_delay_the_emitter_and_order_is_kept` (ordering assertion, no wall-clock threshold), `a_full_queue_drops_the_oldest_and_records_how_many`, `drops_are_recorded_at_most_once_per_interval`; `an_observer_that_hangs_does_not_hold_up_the_run…` runs the binary |
| AC-17 (managed, observer part) | met | `the_observer_stages_can_be_denied_and_are_dropped_with_a_report`; managed observer hooks run first |

Observers run only for a trusted project (`an_untrusted_project_runs_no_observer_hook`). The `summary` and `title` fields are bounded to 512 bytes with control characters flattened; the spec's "redacted" is satisfied by the ledger's own redaction of the underlying record, not by a second redactor here.

### Revert cycles

24 mutations: no applicability check; no drop on overflow; newest dropped instead of oldest; no interval; failures not recorded; the digest left in `source`; control characters kept; no session id; each of the four runner request sites, both resolution paths, the clarification, the workflow request and resolution, and each notification kind's site (job, monitor flood, detached agent, loop, turn-complete for a non-completed turn, goal) removed or mislabelled; trust ignored for notifications and for resolutions. Each fails the test named for it.

### Recorded, not changed

The observer queue is process-wide (32 events across all sessions of a daemon, not per session). Observer hooks have no cancellation beyond their own timeout. A plan proposal reaching approval is not driven by a unit test.


## EXT-04-0 — skills wired: listing block, `skill` tool, `/skill`, `--skill`, `rapid skills list|show`, `skill.activated`

Spec: [EXT-04](EXT-04-skills.md) §3.0, AC-01…05, AC-11, AC-12.

### Phase 0 corrections to the spec's "Today"

| Claim | Finding |
|---|---|
| "Activating a skill already active in the current context generation returns `already active`; compaction starts a new generation" | A tool exchange does not outlive its turn here: later turns are given the user's message and the model's final answer only (`ConversationHistory`). So a skill the **model** loaded is in front of it for that turn alone, and "already active" is per turn for the model. What the **user** loaded (`/skill`, `--skill`) is part of their message, so it stays until a compaction folds it into a summary, and the model's own call for it answers `already active`. The generation is derived from the compaction summary each turn is handed (`SharedSkills::observe_compaction`), so manual compaction, in-turn recovery and a resumed session all count. |
| "Skill resources the loader lists are read through the normal read tool" | A user-root skill lives outside the workspace, which the workspace tools cannot read. The loader's own bounded inlining of resources (prompt text, never executed) is used instead; resources beyond the 16 KiB activation bound are cut with the body. |
| Skill body "placed in the context as a block … prompt data only" | Not fenced as untrusted: that fence tells the model to treat the text as data, which would defeat a skill. The block is labelled (`[skill:<id> source=… sha256=…]` … `[/skill:<id>]`), a body line that would close it or turn into an image part (`DATA_URL:`) is defused, and the lattice decides every call the text leads to (negative test: a body that claims write authority in plan mode changes nothing). |
| Event family | `EventFamily` has no `Skill` variant. Filed under `tool` (a skill activation is the result of a tool call or of a typed command); no new family. Registered like `hook.dropped` (event table, wire catalog, regenerated SDK, `WIRE_SCHEMA_SHA256`, event catalog, SDK pinned count 120 → 121). |
| Roots | The plugin/marketplace root has no counterpart in the app (no marketplace concept), so the roots are the user's `<home>/skills` and a trusted project's `.rapidlm/skills`. The `plugin` root class exists in the record vocabulary and is unreachable until a marketplace is. |
| "Discovery cached until a root's directory metadata changed" | Discovery is read once per turn (and again for a typed `/skill` or Tab), not fingerprinted: it is bounded (64 skills) and a no-skills session costs two failed `read_dir` calls. |
| `user_invocable` / `model_invocable` (`SK:573-589`) | Confirmed: a named request checked only `user_invocable`. Added `ActivationQuery::model_requested`, which needs `model_invocable` and no `paths`; `activate_matching` treats either named request as required (an over-budget one is an error, not a silent skip). |
| Unlisted / `model_invocable: false` | `skill_unknown` (the model is not told the skill exists); a typed `/skill` of a model-only skill is `skill_not_invocable`. |
| `ToolKind` | Not set for `skill`: nothing observes the difference between read and write kinds for a session-local tool, and an unproven classification is not shipped. `ToolClass::ReadOnly` is what lets it run in plan mode. |

### Criteria

| Criterion | Status | Evidence |
|---|---|---|
| AC-01 with no skills nothing changes | met | `with_no_skills_nothing_about_skills_reaches_the_model` (binary: no `skill` tool, no listing in any request), `with_no_skills_there_is_nothing_to_list_or_offer`, `the_skill_tool_is_offered_only_with_a_model_invocable_skill…`, `a_skill_for_people_alone_offers_the_model_nothing`; the existing seventeen-tool surface test passes unchanged |
| AC-02 user and project skills listed in order; untrusted project not loaded, doctor says why | met | `user_and_project_skills_are_listed_and_the_tool_is_offered_but_an_untrusted_project_adds_none` (also: an untrusted exec has no skill tool and no listing), `an_untrusted_projects_skills_are_not_loaded`, `the_skills_check_counts_skills_and_says_when_a_projects_were_left_out`, `an_untrusted_projects_skills_are_not_listed_and_the_listing_says_why` |
| AC-03 activation: body as `skill:<id>` block, event first, transcript marker, repeat is `already active`, new generation inserts again | met | `the_model_activates_a_skill_once_the_event_comes_first_and_a_repeat_is_already_active` (ledger order against `tool.completed`), `activation_reports_first_then_a_second_one_is_already_active`, `a_typed_activation_always_inserts_and_stays_loaded_until_compaction`, `a_compaction_makes_the_model_load_a_typed_skill_again`, `a_skill_activation_leaves_one_marker_line_and_a_malformed_one_leaves_none` |
| AC-04 plan mode and every mode without prompting; a body cannot change a decision | met | `the_tool_works_in_plan_mode_and_a_body_that_tells_the_model_to_write_changes_nothing` (the write is `denied:` by the lattice, `notes.txt` absent) |
| AC-05 `/skill`, `rapid exec --skill`, `rapid skills list|show`, `SUBCOMMANDS`, reference consistency | met | `slash_skill_loads_a_skill_as_the_users_message_names_the_unknown_and_completes`, `slash_skill_takes_an_id_and_optional_arguments`, `exec_skill_leads_the_prompt_is_recorded_as_the_users_and_makes_a_model_call_already_active`, `exec_skill_alone_can_be_the_prompt_and_an_unknown_one_is_a_usage_error`, `rapid_skills_lists_and_shows_offline`, the width and reference-doc tests over `SUBCOMMANDS` |
| AC-11 privilege keys rejected as today | met | the loader's tests are unchanged; `a_privilege_key_is_still_refused_at_discovery` through the runtime |
| AC-12 listing bounds | met | `the_listing_is_bounded_and_summarises_the_rest` (≤ 4 KiB, `+N more`), `a_description_is_one_line_and_cut_to_200_bytes` |

Also covered: arguments over 4 KiB or with a NUL are refused (`arguments_are_bounded_and_may_not_hold_a_nul`, `the_skill_tools_arguments_are_bounded_and_closed`); a body is cut at a line with `[skill truncated]` and the digest covers the whole (`a_long_body_is_cut_at_a_line…`); a read that fails does not leave the skill marked loaded (`a_skill_that_cannot_be_read_is_not_left_claimed`); the model cannot load a `model_invocable: false` skill (`the_model_cannot_load_a_skill_marked_for_people_alone…`).

### Revert cycles

43 mutations, each applied to the fixed code, the named test failed, the file restored and touched: three on the loader (a model request not gated on `model_invocable`; a refusal not mapped to `NotInvocable`; a named model request not treated as required); the listing's reserve, description cut and model-invocable test; the two flag gates and the `NotUserInvocable` refusal; the idempotence claim, the per-turn release of the model's entries, the generation clear and the release on a failed read; both defusing rules in a body; the line cut and the digest of the cut text; the argument bounds; the near-match distance; the compaction observation (two forms) and the per-project session; the trust gate on project roots; the refresh between turns; the typed activation's turn; the offer gate, the tool class, the argument parser's three checks, the model origin, the dispatch arm and `begin_turn` in the tool; the listing's trust gate and `bind_skills`; `--skill`'s session sharing, its ledger replay and its argument check; the ledger append; the transcript fold and the `/skill` parse arm; `rapid skills list`'s count and the doctor's untrusted note and severity. Three first attempts were weak and were corrected: the near-match test could not tell a close id from a far one (rewritten with only one close id); a `tool_kind` entry nothing observed (removed); two anchors that rustfmt had moved.

### Checks

`rustfmt --check` on every changed file, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --no-fail-fast` (4 905 passed after the doctor row lists were updated for the new `skills` row; `exec_tools::tests::twice_the_ceiling_queues_in_order_rather_than_erroring` failed once under load and passes alone 3/3 — it is already in the known-flake list), the SDK's `generate:check`, `typecheck` and tests (29), and `rapid doctor`'s row-order tests.

### Recorded, not changed

*(Superseded in part by EXT-04-0-R below: a typed `/skill` now records only once its message is certain to go out, and the listing is given only where the run has the tool.)*

- A typed `/skill` appends `skill.activated` and then submits the message; if a `user_prompt_submit` hook blocks the message, the event was already recorded (the body did not enter the conversation). The event says the activation was decided, not that a turn ran.
- `skills/selected` carries the listing for a trusted project only (an untrusted project has no tools to load a skill with).
- The skill session is created at the first turn that knows the project; a project switch mid-session starts another.
- Subagents get no `skill` tool and no listing (invariant 12); EXT-05 may offer skills to a definition later.
- D-3's link fix (`Skills.md` → `skills.md`) is not made: the sibling `AGENTS.md` points the same way and both are governance files left to the maintainer.


## EXT-03-3-R — review follow-up for the observer stages (self-review of `4518b26`)

The background review of the commit returned twelve findings. Each was read against the code before it was acted on.

| # | Finding | Verdict | What changed |
|---|---|---|---|
| 1 | Observer hooks lost `RAPIDLM_SESSION_ID` / `RAPIDLM_TURN_ID` (the identity is thread-local and the worker is another thread) | confirmed | the job carries the run identity (the recorder's session, the thread's turn) and the worker scopes it around the hooks: `the_hook_is_given_the_session_and_turn_in_its_environment` |
| 2 | `hook.dropped` recorded the first drop of an interval and lost the rest (nothing flushed an idle queue or the exit; counts vanished with no sink) | confirmed | drops are tallied per stage and session; the first is recorded at once, later ones together at most once per interval, an idle worker reports a waiting tally when the interval is up, and the exit drain records the remainder; a run with no ledger says it on stderr |
| 3 | One FIFO for every session and stage: a drop was recorded in the wrong ledger; a slow notification delayed a permission event; a notification flood evicted permission events | confirmed, in part | tallies are keyed by session and recorded in that session's ledger; the oldest notification is evicted first (a notification never evicts a permission event; a permission event only for another); a waiting permission event runs before a waiting notification. **Not changed:** a hook already running still holds the single worker for its own budget (5 s by default) — documented |
| 4 | Duplicate kinds for one end; `agent_finished` outcome always `continued`; "a hundred lines fires nothing" is false for a burst; `agent_finished` told before the job ended | confirmed, in part | each kind is true on its own (a background child is a job; a flooded monitor then ends cancelled; a loop's turn completes and the loop reports), so they stay and `hooks.md` now says so; `agent_finished` carries `ok`/`failed`/`cancelled` from the continuation's end; the detached child's notice is told after its report is spooled and its job is terminal; the documentation no longer overstates the flood case |
| 5 | "Redacted" was not true; title bound 512 against the spec's 200; the approval summary was not scrubbed | confirmed | the approval summary goes through the tools' redaction; free text is scrubbed of the active model's credential **before** it is cut (a secret straddling the bound leaves no prefix); `title` is bounded to 200; `hooks.md` says the fields are untrusted data a hook must not interpolate into a shell. **Not changed:** only known secrets are scrubbed (no pattern redactor exists) |
| 6 | `goal_changed`: a budget-exhausted block never notified; a pause during an autonomous run fired twice; a doc comment sat on the wrong function | confirmed | the block notifies `blocked` when it lands; the command skips its notice when the loop will see the pause; comments moved |
| 7 | Settings and managed policy read per event; a false "nothing is read" comment; a full-session export to name a resolved request's tool; the managed gate report dropped; mixed snapshot and reload | confirmed, in part | project hooks are cached for two seconds (`project_hooks`), the comment is true; **not changed:** the export (it runs only when a `permission_resolved` hook exists), the gate report (turn start prints it) and the process-environment dependence of the tests that load project integrations |
| 8 | Vacuous or missing tests | confirmed | the hang test is now two runs compared (same exit code and output, the hook did start, the wait is its own 400 ms and not 30 s or the 5 s default); the exit drain is proven with an observer that writes a second after the run ends; a parked run fires no `turn_complete`; the sink wiring of a recorded turn's tools, a clarification through a session turn, the `remember` flag both ways, the long-hook give-up message. **Not added:** `tool`/`approval_id` assertions on the ACP and daemon paths, a plan proposal driven to approval |
| 9 | Worker robustness and exit semantics | confirmed | a panicking runner costs that event (`catch_unwind`); a failed thread spawn lets the next event retry (3 lines, not testable without failing a spawn); the exit wait reports what it gave up, and the error is printed before it |
| 10 | An unrecorded run recorded or warned of nothing | confirmed | a failed hook and dropped events are said on stderr when there is no ledger (the drop line is tested; the failure line is not) |
| 11 | "Managed observer hooks work through the existing gate" overreaches for an untrusted project | confirmed | `hooks.md` says observers run for a trusted project only, the managed policy's own included |
| 12 | Outcome vocabulary, `remember` semantics, `agent_finished` ordering | confirmed | the per-kind outcome table and the `remember` meaning (a standing answer recorded with the answer; false when it already stood) are documented; the ordering is fixed (#4) |

### Revert cycles

24 mutations: no run identity in the job or its scope; no exit flush, no idle flush, a tally not keyed by session, a notification not preferred as the victim, the incoming notification not dropped, a permission event not run first, no panic guard, no stderr line for a run with no ledger, no scrub, a 512-byte title, no cache interval; the approval summary not scrubbed; the budget block and the pause not told once; the continuation outcome; the agent notice moved before the job's end; the sink wiring; the exit-wait message. Two first attempts could not tell the mutated code from the fixed (the session-attribution test used a zero interval, so every drop was recorded alone; the priority test's two scenarios ended in the same queue); both were rewritten and the mutations re-run.

### Checks

`rustfmt --check` on the changed files, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --no-fail-fast`: 4 922 passed, 0 failed. `a_goal_run_while_the_model_slot_is_busy_waits_instead_of_starting_a_second_turn` failed on this machine for a while during the work and passes with `RAPIDLM_CONFIG` pointed at a missing file: its second iteration reaches the configured real provider, so it depends on that provider being unreachable quickly. Not a regression.


## EXT-04-0-R — review follow-up for the skills wiring (self-review of `081396a`)

The background review returned eleven findings. Each was read against the code before it was acted on.

| # | Finding | Verdict | What changed |
|---|---|---|---|
| 1 | A `/skill` message over 16 KiB makes the terminal UI's session view refuse `turn.started` (strict 16 KiB display bound against the kernel's 32 KiB turn text) and block every later event | confirmed — and older than the commit: any typed prompt of 16–32 KiB did it; a loaded skill made it routine | `turn.started`'s text is cut for display (`… (cut)`), tolerantly, like `notification.recorded`; the model has all of it: `a_long_turn_text_is_cut_for_display_and_blocks_nothing` |
| 2 | The message's text is scanned for `@path`, so a skill's text (or arguments) attached files with the person's authority | confirmed | skill blocks are left out of the scan; the person's own `@`s outside them attach: `a_loaded_skills_at_signs_attach_nothing_but_the_persons_own_do`, `what_a_skill_says_is_set_apart_from_what_a_person_typed` |
| 3 | Subagents (and a workflow step, and a run whose tools failed to build) were told of skills they have no tool for; the doc said otherwise | confirmed | the listing is passed in only where the run has the tool (`build_live_context(…, skills)`): a trusted project's own turn, and a recorded `exec`; the `skill` tool is offered, and run, only where an activation can be recorded. **Not changed:** the listing and the tool each read the roots (one discovery per use), so a skill edited between the two can show in one and not the other |
| 4 | A typed activation was claimed and recorded before its message was known to go out (turn running, hook blocking, view blocked) | confirmed | `/skill` is read first (`prepare_user`), refused while a turn runs, gated by `user_prompt_submit`, and only then claimed and recorded (`commit_user`), released if the turn does not start: `a_skill_typed_while_a_turn_runs_is_refused_…`, `a_skill_a_prompt_hook_blocks_is_not_activated`. **Unproven:** the release when the turn fails to start (the failure cannot be produced in a unit test) |
| 5 | A `/skill` typed right after a compaction was cleared by the next turn's compaction check | confirmed | the compaction summary is observed at the command, before the claim: `a_skill_typed_right_after_a_compaction_is_still_loaded_for_the_next_turn` |
| 6 | A workflow step got the listing and tool but no sink; `exec --skill` recorded its event after `turn.started`; an unrecorded run recorded nothing | confirmed, in part | a workflow step is offered no skills; `exec` lists skills only once its turn is recorded; **the order stays**: the turn is submitted at the seq the run read the session at, so nothing of the run's own can be appended before it (a first attempt to do so made the run unrecordable — `session.conflict` — and was reverted). The event is appended before any model step, which is what the spec's "before the body enters the context" means for a model; the test asserts that |
| 7 | `body_digest` covered the arguments; arguments were cut with the body and still counted as delivered; a repeat call with new arguments was silently "already active" | confirmed | the digest is of the skill alone; arguments follow the cut text; other arguments make a new activation, none or the same are "already active" |
| 8 | Tab could not complete ids with capitals | confirmed | `complete_id`, case-insensitive on the typed part, ids as written |
| 9 | A custom command named `skill` is shadowed; the doctor row is missing from the reference; `--skill` tokens leave an unquoted prompt | confirmed | recorded in `skills.md` (`/commands` already lists a shadowed command); the doctor list in the CLI reference has `skills` |
| 10 | `rapid skills show` built the directory from the id and printed the body raw | confirmed | the directory is the file's; control characters other than newline and tab print as `?` |
| 11 | Tests: no concurrency test; some read the developer's real home; the plan-mode negative test cannot fail today | confirmed | an eight-thread test with a barrier (exactly one insertion; **claim-before-read has no effect on the result** — a claim that is not atomic does, and that mutation fails the test); a session takes its home from the session (`SharedSkills::set_home`, set from the resolved home at start and by the test harness), so tests are hermetic and a session started with an explicit home looks there; the plan-mode test stays as a regression guard |

### Revert cycles

17 mutations, each failing the test named for it: no display cut; the `@` scan not skipping blocks; the offer not needing a sink; the run not needing one; no refusal while busy; no hook gate; no compaction observation at the command; arguments ignored by idempotence; the digest covering the arguments; case-sensitive completion; raw body in `show`; the environment's home over the session's; the typed turn number; a typed claim that records nothing; a release that gives nothing back; the exec replay; a non-atomic claim. One first attempt (a thread test with sixteen-byte skills) could not overlap its readers and one mutation of it, claim-after-read, turned out to change nothing observable; the test was rebuilt around a slow skill and a barrier and proven with the non-atomic claim instead.

### Checks

`rustfmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --no-fail-fast`: 4 937 passed, 0 failed.


## EXT-04-1 — skill roots and visibility: `.agents/skills`, `extra_dirs`, `discover`, shadowing, rejections

Spec: [EXT-04](EXT-04-skills.md) §3.1, AC-06, AC-07.

### Phase 0 corrections to the spec's "Today"

| Claim | Finding |
|---|---|
| §3.1.2 "Precedence: project roots > user root > extra roots > plugin root" | contradicts §3.1.4 and decision D-1 in the same document (user over project, as the loader and its test have it). Resolved as D-1 says: **user > extra > project (`.rapidlm/skills`) > project convention (`.agents/skills`)**. The extra directories are the user's own, so they rank with the user's. The loader's existing order (user < marketplace < project) is kept for the roots that were there. |
| §3.1.5 "a symlinked directory or `SKILL.md` is skipped with a reason; `name` must equal the directory name" | both now reject, in the loader, for the quiet `discover` too (it still drops quietly; `discover_report` says what it dropped). The 120 loader tests that existed pass unchanged. |
| §3.1.6 "keeps the first 64 by precedence and name and reports the rest, instead of failing" | `discover_report` does that and a directory over its entry cap is read to the cap and counted; the plain `discover` still fails with `TooManySkills`, so a library caller that relied on it is unchanged. |
| §3.1.7 plugin skills namespaced `<plugin id>:<skill>` | **not done**: no plugin carries skills and nothing in the app has a marketplace root to name them under, so there is nothing to namespace. The id grammar allows `:`; when a consumer exists the root's layout decides it. |
| §3.1.3 `discover` | scans the home and configuration roots of `rapid mcp install --discover` (`RAPIDLM_CONFIG_ROOTS` overrides, `--roots` replaces) to three levels, each folder of a candidate holding a `SKILL.md` that parses and is named for its folder; a candidate is not entered; symlinked directories are not followed. |

### Criteria

| Criterion | Status | Evidence |
|---|---|---|
| AC-06 `rapid skills list` reports shadowed and rejected skills with typed reasons incl. `NameMismatch`, `PrivilegeInjection`, `Symlink`, `TooLarge`; past 64 keeps the first 64 and reports the rest | met | loader: `a_report_says_what_it_left_out_and_why`, `a_skill_hidden_by_a_higher_root_is_reported_with_who_hides_it`, `past_the_cap_the_first_by_precedence_and_name_stay_and_the_rest_are_counted`, `a_directory_with_too_many_entries_is_read_to_the_cap_and_counted`; app: `list_says_what_a_higher_root_hides_and_what_it_refused_and_why`, `past_the_cap_the_first_sixty_four_are_kept_and_the_rest_are_counted`, the doctor test, and the binary test `agents_and_extra_directories_load_at_their_rank_and_list_says_what_it_hid_and_refused` |
| AC-07 `.agents/skills` and `extra_dirs` load at the stated precedence; an entry under the user root or missing is rejected with a reason; `discover` finds a neutral fixture by shape and writes nothing | met | `extra_dirs_are_checked_one_by_one_and_each_problem_is_named` (eight at most, `~`, missing, not a directory, inside the user skills directory, twice, not a string), `every_root_has_its_rank_and_a_hidden_skill_is_reported_not_lost`, `a_root_named_twice_is_read_once`, `discover_finds_a_skills_directory_by_its_shape_and_changes_nothing` (tree identical before and after; a decoy, a mismatched name and a too-deep directory not found; `already read` marked), the binary tests through `exec`, `rapid skills list` and `discover` |

Also: the user config's `[skills]` table is read from `<home>/config.toml` for a session started with an explicit home, else from the file the environment points at; a session says once, as it starts, when skills were left out or an `extra_dirs` entry was ignored (`a_session_says_at_its_start_when_a_skill_was_left_out`); `rapid doctor` counts what is hidden and what was refused and warns with the way out.

### Revert cycles

23 mutations, each failing the test named for it: the extra roots not scanned; the convention directory not scanned; the extra rank above the user's; the convention directory above `.rapidlm/skills`; no name check; a symlinked folder dropped instead of refused; a folder with no `SKILL.md` refused instead of ignored; no cap on the kept; a root read twice; the entry cap an error; a hidden skill not reported; the ninth `extra_dirs` entry read; an entry inside the user directory accepted; a duplicate accepted; `~/` not expanded; an explicit home's config not read; `name_mismatch` shown as `malformed`; no `already read` mark; a candidate with a mismatched name accepted; the depth bound off by one; the doctor not warning; no session notice, and its wiring. Three first attempts needed a better fixture (a too-deep directory at depth five, which no bound would find; an unmarked `already read`; an anchor that rustfmt had moved).

### Checks

`rustfmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --no-fail-fast`: 4 952 passed, 0 failed.

### Recorded, not changed

`rapid skills discover` names no product and has no table of product directories (SEAMS §2.5): it finds by shape only. The extra directories are read each turn from the config (a changed list makes a new skill session, which forgets what a typed `/skill` loaded in the old one).


## Second review round — the observer and skills fixes (self-review of `aacf25a` and `a7367fc`)

The background review of the two fix commits returned fourteen findings. Each was read against the code.

| # | Finding | Verdict | What changed |
|---|---|---|---|
| 1 | A `/skill` claim survives `/undo`, `/fork`, `/rewind`, `/resume` (the claim is cleared only by a changed compaction summary) | confirmed | switching conversations forgets what the user loaded: `a_skill_loaded_before_a_session_switch_is_not_still_claimed_after_it` |
| 2 | `skills.md` said nothing is recorded for a message that is not sent; a failure after the commit leaves the record | confirmed | the documentation now says what is true: recorded just before the message goes out, once the checks that could stop it have passed; the claim is given back and the record stays if the turn then fails to start |
| 3 | `hooks.md` said the job is not terminal when `agent_finished` fires — the opposite of the fix | confirmed | corrected |
| 4 | `without_skill_blocks` swallowed every later `@path` after any line starting `[skill:` | confirmed | a block is a header line of exactly the shape this module writes (`source=` one of four words, 16 hex digits) **and** a closing line after it; anything else is the person's text, a reference-style link included. The arguments of `/skill` are inside the block and attach nothing; that is documented, not changed |
| 5 | `drain` could return while a `hook.dropped` flush was being recorded (idle branch cleared `running` first) | confirmed | the idle flush runs as work: `a_drain_waits_for_a_tally_that_is_being_recorded` |
| 6 | The worker could die if a recorder or `eprintln!` panicked outside the guard | confirmed | the reports are guarded, stderr writes cannot panic (`say`): `a_recorder_that_panics_costs_that_report_not_the_worker`. **Not changed:** a thread-spawn failure while another thread has queued a job runs it at the next submit |
| 7 | `/goal pause` skipped its notice during an autonomous run; a stop before the loop's next pass lost it | confirmed | the command always tells it and marks the run so the loop does not tell it again: `a_pause_typed_in_an_autonomous_run_is_told_once_even_if_the_loop_stops_first` |
| 8 | The listing was offered when the interactive turn's tools failed to build | confirmed | one helper (`skill_roots_for_turn`) decides at the three turn sites: `a_turn_lists_skills_only_where_its_tools_were_built` |
| 9 | ACP, daemon, loop and approval-continuation turns use a fresh `SharedSkills` (environment home, generation 0); the production `set_home` call is untested | confirmed in part | recorded, not changed: those surfaces have no resolved home of their own and use the environment's, as the listing does; a continuation after an approval forgets earlier loads in its turn (the model still has them in its replayed history). The `set_home` call is now covered by the session-notice test (it finds the broken skill only under the session's home) |
| 10 | Mutations covered less than stated; timing-sensitive tests | confirmed in part | the bounds are tested at their call sites (`the_documented_bounds_hold_at_the_call_sites`); the wall-clock margin is 4 s, the file-time comparison has a 100 ms tolerance, the slow hook is 14 s. **Unproven, named:** `register_secret`'s production call; the release when a typed turn fails to start |
| 11 | The scrubber swallowed failures, grew without bound, rebuilt per add, held its lock through a scan, and the approval summary is cut upstream before it is scrubbed | confirmed | placeholders under 8 bytes are ignored, at most 64 secrets are scrubbed for, a failure is said once on stderr, the scan runs outside the lock: `a_scrubber_ignores_placeholders_caps_its_secrets_and_says_when_it_cannot`. **Not changed:** the 512-byte cut of an approval summary happens where the summary is made, before any scrubber, and the ledger's `approval.requested` carries the same cut text |
| 12 | The scrub and JSON work ran for everyone with no hook configured | confirmed | the four observer entry points return first when the stage has no hook (no observable difference, so no test) |
| 13 | A plain-text warning followed the JSON error envelope; the interrupt path skips the exit wait; a long `exec --skill` prompt can exceed the turn limit | confirmed in part | the warning is a `{"warning": …}` line in JSON mode (not tested: it needs an eleven-second run); the interrupt path's skipping the wait is documented; the turn-limit case is recorded (the run is then unrecorded and says so, and the tool is not offered) |
| 14 | `source=<user|project>` and an older delivery section describing superseded behavior | confirmed | corrected, with a pointer in the older section |

### Revert cycles

13 mutations, each failing the test named for it: the session switch not forgetting; the pause not marking the run; the command not telling a pause during a run; the listing not checking the tools; a header of any shape; a header with no closing line; the idle flush not counting as work; the report unguarded; the placeholder floor; the cap; each of the two call-site bounds swapped; the session's home not set at start. One test (the drain during a flush) was rebuilt after its first form made the slow record on the submitting thread, where it could not overlap.

### Checks

`rustfmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --no-fail-fast`: 4 959 passed, 0 failed.


## EXT-04-2 — skill arguments: the shared expander and the `arguments` key

Restated contract (spec §3.2, AC-08): a skill may declare up to nine argument names; an activation's argument string is filled into the skill's text with the grammar custom commands use (`$ARGUMENTS`, `$1`…`$9`, `$$`, `$<name>`), an unknown `$word` is literal, a skill that names none gets the arguments after its text, and custom commands behave exactly as before. Nothing in the arguments is run or looked up (S4).

### Phase 0 corrections to the spec

| Claim | Finding |
|---|---|
| §3.2 "The expander moves to a shared module taking `(&str body, &[&str] declared_names, &str raw)`", owner `crates/protocol` | it lives in `apps/rapid/src/arguments.rs`: both consumers (custom commands and the skill session) are in `apps/rapid`, and EXT-05 shares the frontmatter tokenizer, not this. It takes the names as `&[String]` (what the loader holds) and a `limit`, and returns the text with the offset of the first placeholder (below). `custom_commands::split_args` is re-exported, so its callers and tests are untouched. |
| §3.2 "a body using none of these gets the raw arguments appended after a blank line" | true for a custom command (`fill`, unchanged). For a skill it is not enough: a template that names `$ARGUMENTS` only **past the 16 KiB cut** put the arguments into text that was then thrown away, and the model got neither. The expander now reports where the first placeholder landed (`first_use`); the arguments follow the text unless one landed in the part that is kept (`arguments_named_only_past_the_cut_follow_the_text`, including a placeholder that begins on the first byte past the cut). |
| §3.2 "a scalar … block lists are also accepted" | both are read (`arguments: version channel` and a block list); more than nine, a duplicate, a name that is not `[a-z][a-z0-9_]*` (64 bytes at most) and the name `arguments` make the skill `malformed`. |
| §3.2 "Idempotence key `(skill id, args digest)`" | already the behavior of slice 0 (`other_arguments_make_a_new_activation…`); the digest of the *skill* is of its text as written, placeholders included, so it does not depend on the arguments. |
| §3.2 "Bounds: arguments ≤ 4 KiB, no NUL" | unchanged from slice 0 (`MAX_ARGUMENT_BYTES`, checked before the skill is read). |

### Criteria

| Criterion | Status | Evidence |
|---|---|---|
| AC-08 `$ARGUMENTS`, `$1…$9` with quoting and escapes, `$$`, declared `$name`, unknown `$word` literal, append rule, bound | met | `arguments::tests::{the_placeholders_fill_from_the_raw_string, a_declared_name_is_the_word_at_its_position, a_template_that_uses_none_gets_the_arguments_after_a_blank_line, an_unknown_dollar_word_stays_literal, the_first_use_is_where_the_first_placeholder_was_filled_in, the_output_is_bounded_however_often_the_template_repeats_an_argument, a_limit_changes_only_how_much_of_the_end_is_left_out}`; skill side `a_skill_names_its_arguments_and_the_text_is_filled_in_place`, `arguments_named_only_past_the_cut_follow_the_text`, `a_template_that_repeats_the_arguments_is_cut_like_any_other_skill`; loader `the_arguments_key_names_up_to_nine_arguments`; binary `named_arguments_are_filled_in_for_a_typed_run_and_for_the_model` |
| The same fixtures pass through custom commands unchanged | met | the pre-existing `custom_commands::tests` argument tests pass without edits (the `used` flag became `first_use`, and `custom_commands` calls `arguments::fill`) |

### Revert cycles

19 mutations, each failing the test named for it: declared names not recognised; a declared name matched as a prefix of a longer word; the append rule always on; `$$` left as two dollars; the written text shown in place of the filled text; the declared names not handed to the expander (in the library and in the binary path); arguments appended whatever landed in the kept text; a placeholder at the very byte of the cut counted as kept; the early stop removed; the recorded offset wrong for each of the three placeholder kinds, and "first" replaced by "last"; in the loader, a duplicate name accepted, more than nine accepted, the name `arguments` accepted, and the `arguments` key not allowed. The first cycle left three mutations unproven (a cut at a character boundary, an early exit inside the loop, and the call-site limit): the expander had its own final cut, which the caller's cut made redundant. The final cut was removed; the early stop was kept because a skill file of 64 KiB can repeat `$1` 32 000 times and a 4 KiB argument would otherwise be a 128 MiB string. It is proven by the string's capacity, not its length (`the_output_is_bounded…`). Two first attempts needed a better fixture (`first_use` "last instead of first" had no test with two placeholders; the boundary case needed `session.refresh()` to see a second skill).

### Checks

`rustfmt --check` on the changed files, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --no-fail-fast`: 4 972 passed, 0 failed.

### Recorded, not changed

- The limit `read_skill` passes (`BODY_MAX_BYTES`) is one token at one call site and its effect is visible only in allocation: mutating it to "no limit" gives the same text. The function's own bound is proven; the call site is not.
- Arguments are never validated against the declared names: a skill that declares two and is given five has the other three in `$ARGUMENTS` and `$3`…`$5` only. Declared names beyond the words given are empty.
- An argument string is taken as typed for `/skill` and `--skill-args`; `$` in it is not expanded again (a placeholder in the arguments is literal).


## EXT-04-1-R — review follow-up for the skill roots (self-review of `242dfd4`)

The background review of the roots-and-visibility commit returned fourteen findings. Each was read against the code; all but three are confirmed and fixed.

| # | Finding | Verdict | What changed |
|---|---|---|---|
| 1 | `[skills]` is not a known key of the user config parser, so `rapid doctor` warned that the feature's own table was "unrecognized and ignored" | confirmed | `skills` is a known table; a key in it other than `extra_dirs` is named (`skills.<key>`): `the_skills_table_is_known_and_a_key_it_does_not_have_is_named`, and the binary test now reads the doctor's `config` row |
| 2 | A relative `extra_dirs` entry resolved against the working directory, so an untrusted repository's `skills/` could be read at `extra` rank outside the trust gate; with no `HOME` the "user config" fell back to the relative `.rapidlm/config.toml` of the project | confirmed | an entry must be absolute or start with `~/`; the config path is `None` when it would be relative: `extra_dirs_are_checked_one_by_one_and_each_problem_is_named`, `the_config_is_the_one_every_command_reads_and_never_a_relative_path` |
| 3 | A terminal session read `<home>/config.toml` while `list`, `doctor` and `exec` honoured `RAPIDLM_CONFIG` | confirmed | one function decides: the environment's file wins, then the session's explicit home, then the conventions |
| 4 | `Roots` (with the extra directories and their problems) was the session's identity, so any change in the config made a new skill session that forgot what the user had loaded and restarted the generation | confirmed | identity is the home, the project and the trust; the extra directories are followed in place: `a_config_change_is_followed_by_the_session_not_a_new_one` |
| 5 | The session notice could read "skills: ; `rapid skills list` says why" when only a directory was crowded; the doctor row ignored it; which 256 entries are read depends on the filesystem's order | confirmed | the notice, `list` and the doctor row name a crowded directory; the order is documented as the filesystem's: `a_crowded_directory_is_named_even_when_no_skill_was_refused`, `the_doctor_row_says_when_a_directory_holds_more_than_is_read` |
| 6 | A folder with no `SKILL.md` was refused as `invalid_name` (a space or non-ASCII letter in its name), and any link as `symlink`; the documentation said such folders are not reported | confirmed | a folder with no `SKILL.md` is ignored whatever it is called; a link is reported only if its name could be a skill's and it points at a directory (looking at the target reads nothing): `a_report_says_what_it_left_out_and_why` (its `Bad Name` fixture had no `SKILL.md`, which pinned the old behavior) |
| 7 | "Inside the user skills directory" used a prefix test, but the loader reads direct children only: a folder of skills in it could not be added, with a false reason; `discover` marked such folders "already read" | confirmed | only the directory itself is refused / marked: `only_a_directory_that_is_read_is_read_already_not_one_inside_it` |
| 8 | `discover` could offer `<repo>/.agents/skills` as an `extra_dirs` line, which reads an untrusted repository's skills with the user's rank | confirmed | a project's own skills directory is shown with the reason and no line: `a_projects_own_skills_directory_is_not_offered_as_an_extra_directory` |
| 9 | `rapid skills list|show|discover` created `$HOME/.rapidlm` on a fresh machine, against "only reads" | confirmed | the read-only commands resolve the home without making it: `the_commands_do_not_make_a_home_that_is_not_there` and the binary test `the_read_only_commands_do_not_make_a_home_that_is_not_there` |
| 10 | The 1 s clock now spans up to eleven roots and a timeout empties the catalog, user skills included | **recorded, not changed** | the failure is not silent (the session line says "discovery failed", `rapid skills list` exits 1, the doctor warns), and a partial result needs a design of its own; no deterministic test exists for a wall-clock bound |
| 11 | Strict `discover` changed behavior (a name mismatch now drops a skill; the convention directory is scanned for a trusted project) though the commit message said "unchanged" | confirmed, **recorded** | the delivery record of EXT-04-1 says so; the commit message stands |
| 12 | The limit of eight counted raw entries, so three bad entries followed by eight good ones read five | confirmed | eight that can be read are read, the rest named: the capped case of `extra_dirs_are_checked_one_by_one_and_each_problem_is_named` now starts with three that cannot be |
| 13 | `rapid skills show` and the activation error gave the trust hint for `.rapidlm/skills` only | confirmed | both directories: `show_points_at_the_untrusted_convention_directory_too`, `the_untrusted_hint_covers_the_convention_directory_too` |
| 14 | Stale text: "dropped if it does not parse", the doctor row's description, a command comment, the module doc; `rejected` printed in directory order; a symlinked project root directory is followed | confirmed in part | all text corrected; `list` sorts what it refused by directory (**unproven**: this filesystem lists in name order already); a symlinked `.agents` / `.agents/skills` of a *trusted* project is followed as the old `.rapidlm/skills` was, **recorded** |

Test claims in the review also held and were met: a root spelled with `..` or through a link is read once (`a_root_named_twice_is_read_once_however_the_path_is_spelled`); an invalid entry followed by valid ones; a notice with only a crowded directory; the doctor `config` row with a `[skills]` table. The app-level test cannot tell `.rapidlm/skills` above `.agents/skills`, because both are the `project` class; the loader's own tests do.

### Revert cycles

22 mutations, each failing the test named for it: `skills` not a known table; its keys not checked; the read-only home made; the commands still using the creating resolution; "already read" by prefix; a project's directory offered; only one of the two project directories recognised; the hint for one project directory only (admin and loader); the crowded directory missing from the doctor row and from the session notice; the session identity including the extra directories; the config change not followed; the explicit home beating `RAPIDLM_CONFIG`; a relative config path read; a relative entry accepted; a prefix refusal; the limit counting raw entries; a folder with no `SKILL.md` and an invalid name refused; every link refused; a link to a file refused; a root's spelling deciding whether it was read twice.

### Checks

`rustfmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --no-fail-fast`: 4 984 passed, 0 failed.

### Recorded, not changed

Items 10, 11 and the symlinked project root of 14 above; and that the sort of `list`'s refused skills is not provable here.


## EXT-04-2-R — review follow-up for skill arguments (self-review of `8d1d64b`)

The background review of the arguments commit found no high-severity problem and confirmed custom commands unchanged (300 000 random templates against the removed code, no difference). Its two medium and four low-medium findings were each reproduced by reading the code and are fixed.

| # | Finding | Verdict | What changed |
|---|---|---|---|
| 1 | The test for "the arguments are in the kept text" asked only whether the placeholder *starts* before the cut. A multi-line argument the cut falls inside was kept in part and not re-appended | confirmed | the expander reports where the arguments *end* (`landed_end`); they count as placed only if that end is inside the kept text. Otherwise the whole string follows the text: `an_argument_the_cut_falls_inside_follows_the_text_whole` |
| 2 | The early stop could leave the text exactly at the bound after trimming, so no `[skill truncated]` marker was shown and the rest of the template was silently gone; a resource could follow the stopped text | confirmed | `Expanded::stopped`: the block says it was cut whenever the expansion stopped, and no resource follows stopped text: `text_the_expansion_stopped_in_still_says_it_was_cut` (the fixture trims back to under the bound and has a resource that would fit) |
| 3 | An earlier `$1` that was kept hid a `$ARGUMENTS` that lay past the cut: the words after the first never reached the model | confirmed | a template that has `$ARGUMENTS` anywhere (even past a stop) counts on it: that placeholder decides, and only a template that takes words by position alone is decided by its first word: `a_word_taken_early_does_not_hide_arguments_named_after_the_cut`, `a_stop_before_the_whole_string_is_reached_lands_nothing` |
| 4 | A skill that declares no argument names and is given no arguments had its `$1`, `$$` and `$ARGUMENTS` rewritten (`awk '{print $1}'` lost its `$1`), which it never was before | confirmed | such a skill is inserted exactly as written; a skill that declares names, or any activation that gives arguments, is expanded. **This refines the spec** (§3.2 gives the same grammar to every skill): the compatibility rule "defaults preserve today's behavior" decides. Documented in `skills.md`: `a_skill_with_no_argument_names_and_none_given_is_inserted_as_written` |
| 5 | A placeholder that filled in nothing at the end of the text counted as inside the kept text only when no resource followed it (the offset was compared with the untrimmed length) | confirmed | compared with the length of the text itself: `a_word_no_one_gave_is_a_use_with_or_without_a_resource` |
| 6 | `$versionX` was the declared name `version` followed by `X` | confirmed | any letter, digit or `_` after a name makes a longer word, in any alphabet (`$ARGUMENTSX` keeps its old reading — "the arguments, then X" — because custom commands always read it so) |
| 7 | Gaps: the 64-byte name bound untested; a bad `arguments:` key is shown only as `malformed`; "a list on one line" over-promised; resources are not expanded and the docs were silent; the overshoot comment; the mutation count; a change to `arguments:` is not in the digest | confirmed in part | a 64/65-byte test; the list wording and the resource rule are documented; the comment corrected; the record's count recounted. **Recorded, not changed:** the reason `malformed` carries no detail (it is the same for every frontmatter fault); the digest covers the body only, as for every other frontmatter key |

### Revert cycles

16 mutations, each failing the test named for it: a stop before the whole string was reached counting as landed; the first word preferred to the whole string; the offsets taken before the value is added (`$ARGUMENTS`, `$1`, a declared name); a name ending only at lower-case letters; the stop not recorded; the rest of the template added after a stop; the marker not forced after a stop; a resource following stopped text; the end not clamped to the text; the end compared with `<` instead of `<=`; "any placeholder at all" counted as placed; the verbatim rule off, and widened to "no arguments given" alone; a 65-byte name accepted.

### Checks

`rustfmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --no-fail-fast`: 4 990 passed, 0 failed.


## EXT-04-3 — skill rules and limits: `skill(...)` rules, the managed `[skills]` table, the model's bounds

Restated contract (spec §3.3, AC-09 and AC-10): `skill` is a tool name in the permission rules; `deny` hides a skill and refuses it for the model and for a typed `/skill`, `ask` makes only a model's load wait for an approval, `allow` is the default. The managed policy adds `[skills] denied_ids` and `project_skills = false`, narrow-only and reported. The user config bounds how many skills the model may load by itself in a turn (default 8) and in a session (default 64); past a bound the load returns `skill_limit`. A skill never carries a permission, and policy only narrows.

### Phase 0 corrections to the spec

| Claim | Finding |
|---|---|
| §3.3 `ask` creates an approval with `source: skill:<id>` | the approval's `source` is for asks raised by something other than the lattice (a hook, a plan); a `skill(...)` ask rule *is* a lattice ask, so `source` stays unset. The request names the skill by what a reviewer reads: summary `load skill <id>`, scope `skill:<id>`, the arguments it was given, and the standard digest of the call's arguments (the JSON, which includes the name) rather than of the argument string alone |
| §3.3 `deny` refuses with `skill_denied` | the typed `skill_denied` is what a typed `/skill`, `rapid exec --skill`, and a model call refused by the managed `denied_ids` get. A model call refused by a *rule* never reaches the skill code: the tool runner's generic denial answers it first, as for every tool. In every case nothing is read and nothing is recorded |
| §3.3 managed `denied_ids` and `project_skills` "reported" | `rapid skills list` names each refused skill and by what; `rapid skills show` says it; `rapid doctor`'s row counts them and, for `project_skills = false`, says the project's skills are not loaded and why; the session line says when a skills setting could not be applied |
| §4 the bounds live in the user config | kept there. The spec gives the managed policy no ceiling on them, and none was added (a managed policy narrowing a user's own bound is a further, separate key) |
| rules come from "the existing grammar" | the lists that hide a skill and refuse a typed load are built where the listing is made, without the turn's lattice: a trusted project's settings files read from the project (not from the working directory), the managed `denied_tools`, and the user's standing "never allow" answers under the home. A project that is not trusted contributes no rule. The model's own call is judged by the turn's real lattice as well |

### Criteria

| Criterion | Status | Evidence |
|---|---|---|
| AC-09 `deny` hides and refuses for model and user; `ask` creates an approval for a model activation and not for a typed one; managed `denied_ids` and `project_skills = false` narrow and report | met | `a_skill_the_policy_refuses_is_hidden_and_refused_to_the_model_and_the_person`, `a_trusted_projects_deny_rule_refuses_the_skills_it_names_and_an_untrusted_ones_does_not` (including: a typed load of an `ask` skill is not asked), `the_managed_policy_can_turn_a_trusted_projects_own_skills_off`, `a_skill_rule_names_the_skill_deny_refuses_ask_waits_and_the_rest_load` (the runner: denied, an approval with the skill, scope and arguments, and a plain load), `list_show_and_the_doctor_row_say_what_the_policy_refuses_and_turns_off`, the policy loader's tests, `the_skills_table_is_closed_and_typed`; binary `a_skill_the_managed_policy_refuses_is_hidden_from_the_model_and_refused_to_everyone` (the listing, the model's refusal, `exec --skill` exiting 3 before any model call) |
| AC-10 `max_model_activations_per_turn` and `_per_session` return `skill_limit` at the bound | met | `the_model_may_load_only_so_many_skills_by_itself_and_a_person_is_not_counted`, `a_skill_that_could_not_be_read_does_not_use_up_the_models_bound`, `a_changed_bound_or_rule_is_followed_by_the_running_session`, `the_bounds_come_from_the_user_config_and_a_bad_one_is_named`; binary `a_project_rule_refuses_a_skill_and_the_model_may_load_only_so_many_by_itself` |

### Revert cycles

32 mutations, each failing the test named for it: the managed ids compared with case; the rule check discarded; a project's rule applied while untrusted; project skills defaulting to off; a managed policy that cannot be read leaving project skills on; the bound not capped at 1000; the default changed; an unreadable standing-refusal store not refusing; the managed tool ban not applied; the refusal missing from activation; refused skills still listed, still counted for the tool offer; the managed project switch ignored; the model's count not counted, off by one, not reset each turn, not given back when a read fails, and counting a person's load; a changed policy not followed by the session; the notice missing; the list's refused section, `show`'s refusal, the doctor's project-off line, the roots note and the bounds line; the rule subject; the approval description; the exit code; the managed table not accepted, its list not bounded; the new config keys not known.

### Checks

`rustfmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --no-fail-fast`: 5 012 passed, 0 failed.

### Recorded, not changed

- The lists that hide a skill are built from files, not from the turn's lattice, so a mode or an `--allow` for this run cannot change them; the model's own call is still judged by the turn's lattice.
- Rule patterns match the id exactly, upper and lower case apart; the managed `denied_ids` ignore case.
- No managed ceiling on the bounds (above).


## EXT-04-R3 — second review round for the roots and arguments commits (self-review of `32d537c` and `a8e2d55`)

The reviews of the two fix commits returned eleven and seven findings. Each was read against the code. Nothing was high severity, and none showed an untrusted project's skills being read without the user naming the directory.

### The roots fixes (`32d537c`)

| # | Finding | Verdict | What changed |
|---|---|---|---|
| 1 | `discover` withheld the person's own `~/.agents/skills` (it judged by the last two path components) | confirmed | the person's home is not a project: `…/.agents/skills` under it is offered. `the_persons_own_agents_directory_is_offered_and_a_link_to_a_projects_is_not` |
| 2 | `discover` judged and offered the spelled path: a relative path was offered (and then refused by `extra_dirs`), and a link to a project's directory was offered under its link name; `.Agents/Skills` on a case-insensitive filesystem escaped the name test | confirmed | both the test and the printed line use where the directory really is, and the names compare without case |
| 3 | A relative `RAPIDLM_CONFIG` was still read for `[skills]`, against "never a relative path" | confirmed | not read; a test with a relative value |
| 4 | `roots_for` read the process environment: a user's exported `RAPIDLM_CONFIG` or managed policy leaked into tests that set a home | confirmed | `SharedSkills::set_env` (test builds) and an empty environment in every test that sets a home |
| 5 | "One config file for every command" was false for a session whose `RAPIDLM_HOME` held no `config.toml` while the conventions did | confirmed | the session's own `<home>/config.toml` is used when it is there, otherwise the conventions, as every other command: `the_config_is_the_one_every_command_reads_and_never_a_relative_path` |
| 6 | A crowded directory was counted, not named | confirmed | the report carries the directories; `list` and the doctor row name them (the session line counts): `a_directory_with_too_many_entries…`, `the_doctor_row_says_when_a_directory_holds_more_than_is_read` |
| 7 | `extra_dirs` examined every entry, unbounded, from an unbounded read | confirmed | the first 32 entries are examined and a config over 256 KiB is not read for this: `only_so_many_entries_are_examined_and_a_huge_config_is_not_read` |
| 8 | Links the old code reported became silent, including a dangling one with a valid name; the stat was described as no-follow | confirmed in part | a link to a directory or to nothing is reported; one to a file or with an invalid name is not; the doc says it is a stat. **Recorded:** a non-UTF-8 name is not reported |
| 9 | `user_home_path_from` canonicalised with the std function, not the portable one | confirmed | `protocol::host_path::canonicalize`. **Unproven here** (it differs only on Windows) |
| 10 | Documentation: the doctor row "warns when any of that is so" (hidden-only is a pass), the module doc of `skills_runtime`, spec §3.1.2 and AC-07 (still "under the user root"), `~` alone, the relative-home message | confirmed | all corrected, spec text amended with a pointer to this section |
| 11 | `show`'s trust hint followed links and joined an unvalidated id (`../skills`); `roots_of` opens the trust store, which writes when an identity went stale | confirmed in part | the hint needs a valid id naming a real folder, no link: `the_trust_hint_needs_a_real_skill_folder_and_not_a_path_or_a_link`. **Recorded:** the trust-store write is the store's own behavior for every command that asks |

### The arguments fixes (`a8e2d55`)

| # | Finding | Verdict | What changed |
|---|---|---|---|
| 1 | **Medium.** A whitespace-only argument string defeated "inserted as written" (only the typed path trimmed it), expanded the text, and made a second digest | confirmed | blank arguments are none at every entry (`activate`, `prepare_user`, `commit_user`): `a_skill_with_no_argument_names_and_none_given_is_inserted_as_written` (the model, a typed load, three kinds of blank, and the repeat is `already active`) |
| 2 | `$$ARGUMENTS` after an early stop counted as `$ARGUMENTS`, so the first word was appended twice | confirmed | the rest of the template past the limit is now read with the same grammar and kept out of the text, so an escaped dollar is what it is: `an_escaped_dollar_is_not_the_arguments_after_a_stop` and the expander test |
| 3 | The early stop could show `[skill truncated]` and drop a resource when the left-out remainder was only blanks or empty values | confirmed | `stopped` is true only if the remainder would have added more than blanks: `a_remainder_of_blanks_is_not_a_cut_and_the_digest_still_covers_the_resources` (which also pins that the digest covers a resource the block leaves out) |
| 4 | A skill that asks for `$ARGUMENTS` showed the literal when loaded bare, unlike a custom command; the spec was not amended for the verbatim rule | confirmed | a skill whose text contains `$ARGUMENTS` is filled in even with none (blank). `$1` and `$$` still survive a bare load. Spec §3.2 and AC-08 amended |
| 5 | A name followed by a letter of another alphabet stayed literal (`$version을`), with no way to write one | design | ASCII letters, digits and `_` extend a name (the name grammar is ASCII); anything else ends it. Documented |
| 6 | Mutants surviving the first tests: a digit after a name, `$$ARGUMENTS`, blank arguments, the digest of stopped text | confirmed | tests for each |
| 7 | Documentation: "exactly as written", `$ARGUMENTSX` and `$10` having no boundary, what the marker means, the overshoot comment, the mutation count | confirmed | corrected |

### Revert cycles

20 mutations, each failing the test named for it. Roots: the explicit-home config used without being there; a relative `RAPIDLM_CONFIG` read; the entry cap and the size cap off; `discover` offering the spelled path, judging by the spelled path, losing the person's home exemption and the case-insensitive name; the crowded directories not named (report and list); the hint without a valid id; a dangling link ignored. Arguments: blank arguments counted as given (the predicate, and at each of the two entry points); `$ARGUMENTS` not asking for expansion; a blank remainder counted as left out; the dry read missing a later `$ARGUMENTS`; the tail not checked against the limit; a name extended by non-ASCII letters.

### Checks

`rustfmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --no-fail-fast`: 5 018 passed, 0 failed.

### Recorded, not changed

A non-UTF-8 folder name is not reported (the loader needs a name to report); the trust store's write on a stale identity; the `Windows` canonicalisation is unproven on this host.


## EXT-01-1 — MCP tool filters: `enabled_tools`, `disabled_tools`, the effective set and the dispatch gate

Restated contract (spec §3.1, AC-01…04): a server entry may carry `enabled_tools` (an allowlist; absent means every tool) and `disabled_tools` (applied after it), patterns matched against the server's own tool name. The effective set is cached in the surface and offered; a call to any other tool returns `mcp_tool_not_enabled` without contacting the server; a pattern that matches nothing is visible; a filter can only remove, so managed bans still hold; an invalid list rejects the entry with a reason in `rapid mcp list`, the doctor row and the turn warning. Entries with none of the new keys behave exactly as before.

### Phase 0 recheck

| Claim | Finding |
|---|---|
| §2 "a name the model was never offered is still callable" (dispatch does not check the surface) | confirmed: `execute_mcp_tool` forwards any `mcp__<server>__<tool>` of a live server. It still does for a server with no filter (S11); with one, the gate answers first |
| §2 "unknown keys are ignored silently" | unchanged for every key but the two new ones; `a_bad_tool_list_rejects…` pins that an unrelated key (`enabled_tool`) is still ignored |
| §3.1 empty list | the spec does not say: an **empty** `enabled_tools` allows nothing (it is a list, not "absent"); an empty `disabled_tools` removes nothing |
| §3.4 `rapid mcp get` prints "12 of 40 tools enabled" | `get` is offline and cannot know the reported count; it prints each pattern. `probe`, which starts the server, prints `tools=<offered> of=<reported>` and the patterns that match nothing. The estimated sizes and `get`'s count from a live registry belong to slice 4 |

### Criteria

| Criterion | Status | Evidence |
|---|---|---|
| AC-01 with none of the new keys nothing changes | met | `with_no_filter_every_tool_is_offered_and_callable_as_before`; `list`, `get` and `probe` print no new text for an unfiltered server (`tool_filters_are_listed_got…`, `probe_reports_the_effective_tools…`); every existing MCP test passes unedited apart from the new struct field |
| AC-02 patterns (`*`, `?`, literal, anchoring, case, 64 patterns, 128 bytes, characters); a filtered tool is absent and a call returns `mcp_tool_not_enabled` with **zero** `tools/call` requests at the stub | met | `tool_lists_are_read_and_filter_by_the_servers_own_tool_name`, `a_bad_tool_list_rejects_the_entry_and_says_which_pattern_and_why`, `a_filtered_tool_is_not_offered_and_a_call_to_it_never_reaches_the_server` (the stub's call log does not exist) |
| AC-03 a filter cannot re-enable what the managed policy bans | met | `an_allowlist_cannot_bring_back_a_tool_the_managed_policy_bans` (the tool is offered by the filter and denied by the ban; the stub is not asked). A denied server is not registered at all, so it has no tools to filter |
| AC-04 an invalid list rejects the entry with a reason in `list`, the doctor row and the turn warning | met | `tool_filters_are_listed_got_and_a_bad_one_is_a_rejection_with_its_reason`, `the_mcp_check_names_a_bad_tool_filter_and_why_the_server_will_not_run`; the turn warning is the existing per-rejection warning, which prints the same text |

Also: `rapid mcp add` takes `--enabled-tool` and `--disabled-tool` (repeatable), checks the patterns by the loader's own function before writing, and re-reads through the loader (`add_writes_tool_filters_the_loader_accepts_and_refuses_a_bad_pattern_before_writing`).

### Revert cycles

19 mutations, each failing the test named for it: the denylist ignored; the allowlist ignored; the registration not filtering; the dispatch gate off; the connection not keeping the filter; the denylist missing from the unmatched-pattern report; the default test wrong; `disabled_tools` not read; the character check, the 128-byte bound (off by one), the 64-pattern bound (in the parser and in the shared check, off by one) and the empty-pattern check removed; `list`'s suffix, `get`'s lines, `probe`'s `of=` and its unmatched report, `add`'s validation and `probe`'s filtering removed. One expectation was too strict, not a gap: the shared bound mutation fails the config test and not the `add` test, which uses a handful of patterns.

### Checks

`rustfmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --no-fail-fast`: 5 028 passed, 0 failed.

### Recorded, not changed

- A registry that already holds a server keeps the filter it was registered with: a settings edit during a session takes effect at the next session (the registry is not re-synchronised; slice 3 does that for the deferred set).
- The filter is by the tool's own name as the server reports it; a server that reports the same name twice has both filtered alike.


## EXT-04-3-R — review follow-up for skill rules and limits (self-review of `f9fcee4`)

The review of the rules-and-limits commit found no way to activate a refused skill through a different spelling, `already active`, a subagent or workflow step, the listing, completion or `skills show`. Its ten findings were read against the code.

| # | Finding | Verdict | What changed |
|---|---|---|---|
| 1 | **Medium.** A user's `extra_dirs` naming the project's skills directory (or any directory inside the project) defeated the managed `project_skills = false` | confirmed | when the switch is off, extra directories inside the project are not read, and each is named as ignored: `extra_dirs_cannot_bring_back_the_project_skills_the_managed_policy_turns_off` |
| 2 | **Medium.** An unreadable managed policy left `denied_ids` unenforced on the user-side paths (`skills show` printed a refused skill; a typed `/skill` journaled and sent its text before the turn failed on the same policy) | confirmed | a policy that cannot be read cannot say what it refuses: no skill is loaded until it can be (`Denial::Unreadable`), and project skills stay off: `a_managed_policy_that_cannot_be_read…` |
| 3 | **Medium.** A new test wrote its store key with `std::fs::canonicalize`, which differs from the reader's on Windows (a CI gate) | confirmed | the test uses the reader's own `host_path::canonicalize`. **Unproven here** (it differs only on Windows) |
| 4 | `ask` was judged against the settings file read relative to the working directory while `deny` was caught from the project root, so launched from a subdirectory an `ask` rule did not ask | confirmed, **recorded** | the cwd-relative read is `exec_permission_lattice`'s and affects every rule of every tool; a `deny` is still enforced by the skill code. Not changed here; stated in the delivery notes below |
| 5 | The per-turn bound restarts when a turn resumes after an approval (the resume binds the skills again); `forget_loaded` leaves the session count alone, so "per session" is per process | confirmed, **recorded** | the session bound still holds; the per-turn count is per model turn including a resumed one |
| 6 | The `ask` prompt came before any check that could refuse the load (an unknown name, a refused one, the bound), and showed the raw name | confirmed in part | the name is shown as the tool reads it (trimmed, control characters flattened, 256 bytes at most): `a_skill_ask_names_the_skill_as_the_tool_reads_it…`. **Recorded:** the prompt is not pre-resolved |
| 7 | `/approvals approve N remember` on a skill ask recorded a bare `skill` grant, which answers nothing (a rule outranks it) while saying "remembered" | confirmed | not offered for a skill ask: `remember_is_not_offered_for_a_skill_ask…` |
| 8 | Rule patterns matched the id exactly, so `skill(deploy)` did not refuse `Deploy` | confirmed | the `skill` tool's rules are matched without regard to case (a narrowing; other tools unchanged): `a_skill_rule_names_the_skill_in_any_case` |
| 9 | Documentation: "refuses as `skill_denied`" for a rule, the second settings file unnamed, `ask` in a headless run, the description of a managed ban as "a skill(...) deny rule", no remedy for a managed refusal; the mutation count | confirmed | corrected in `skills.md`; the description reads "a permission rule, a managed tool ban or a standing refusal"; the list says a managed refusal changes only in the managed policy file. The count of EXT-04-3's mutations is 32 |
| 10 | Test gaps: `unclaim` not decrementing the session count, the `denied_ids` length bounds, the real process environment in the follow test | confirmed | each has a test (`a_skill_that_could_not_be_read…` sets both bounds; `the_skills_table_is_closed_and_typed` pins 256 entries of 256 bytes and 257 of either); the follow test and its neighbours set an empty environment |

### Revert cycles

13 mutations, each failing the test named for it: an unreadable policy refusing nothing (the refusal and the flag); extra directories inside the project read under the switch (the filter, the problem, the loader roots); the rule match case-sensitive again; a bare grant offered for a skill; the approval name untrimmed and unbounded; the session count not given back; the two `denied_ids` bounds off by one; the managed-refusal remedy missing.

### Checks

`rustfmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --no-fail-fast`: 5 032 passed, 0 failed.

### Recorded, not changed

- Findings 4, 5 and the prompt order of 6, above.
- A skill's `SKILL.md` stays readable by the file tools: a refusal is of the loader, not of the file system.


## EXT-01-1-R — review follow-up for MCP tool filters (self-review of `f973e5c`)

The review found no way to call a tool the filter removes by a different spelling, case, a leading or trailing character, the offline marker, a duplicate-reported tool, or the HTTP transport, and confirmed the gate checks the pair it sends. Its ten findings were read against the code.

| # | Finding | Verdict | What changed |
|---|---|---|---|
| 1 | **Medium.** A tool list rejected in the first settings file left its name free, so a plain entry of the same name in the second file ran with every tool | confirmed | a name whose entry was rejected for its tool list still belongs to that file; the later one is `Shadowed`: `a_rejected_tool_list_still_claims_its_name…` (other rejections keep behaving as before) |
| 2 | **Medium.** With a filter, the gate matched patterns only: a name the server never listed (`purge_issue` against `disabled_tools: ["delete_issue"]`) was forwarded, so "closes the gap that a name never offered is callable" was too strong | confirmed | a server with a filter is called only for tools it listed and the filter allows (`McpConnection::permits`); a server with no filter is forwarded names as before (S11). Docs say so: `a_name_the_server_never_listed_is_not_callable_once_there_is_a_filter` |
| 3 | **Medium.** A live registry kept the filter it was registered with; a settings edit mid-session did nothing, and an edit that made the list invalid left the server running with all tools | confirmed | the connection keeps every tool the server reported; a changed filter rebuilds what is offered from them (tightening or loosening, no restart, the surface order kept): `a_changed_filter_is_followed_by_a_server_that_is_already_up`. An entry that became invalid keeps what it had until the session ends (**recorded** in the docs) |
| 4 | `add --force` silently dropped the old filters | confirmed | the output names each list it did not carry over (and does not when the command line gave one): `a_forced_replacement_says…` |
| 5 | The gate ran after the permission check and the `pre_tool_use` hooks: a filtered call could raise an approval prompt and run hooks, then be refused | confirmed | refused first, before either: `a_filtered_call_is_refused_before_anything_asks_about_it_or_hooks_run` (default mode, a recording sink and a hook that would leave a file) |
| 6 | An empty allowlist looked unfiltered in `get`, bare in `list`, and `probe` said "server advertises none" for a server that advertises three | confirmed | `(none: the allowlist is empty…)`, `enabled_tools=(none)`, `(no tool enabled)`: `an_empty_allowlist_says_so…`, `probe_reports…` |
| 7 | `add --disabled-tool --force` took `--force` for a pattern | confirmed | a value starting with `-` is refused: `an_empty_allowlist_says_so_and_a_flag_is_not_taken_for_a_pattern` (nothing written) |
| 8 | Two servers named `x` and `x_` reporting `_y` and `y`: a filter on one makes the ambiguous wire name `mcp__x___y` resolvable | confirmed, **recorded** | needs server names differing by a trailing underscore and a filter on exactly one of them; the fix is to match connection names instead of splitting at the first `__`, which is a change to the routing itself and its tests. Left, named here |
| 9 | `rapid mcp install` drops the filter lists without saying so | confirmed | it prints a note |
| 10 | Test and record gaps: AC-03 is checked through the lattice and not the managed policy path, `get` does not list non-matching patterns (spec §3.1.3 says "get and probe"), the exhaustive issue-rendering test lacked the new variants, `Default` derived on a type whose doc says a config is never empty | confirmed in part | the issue test lists every `ToolListInvalid` fault; `Default` is not derived any more. **Recorded:** `get` is offline and cannot know what the server reports, so only `probe` names a pattern that matches nothing; AC-03's managed path is the same lattice call the turn makes |

The reviewer also noted that an entry in the compatibility settings file that already carries `enabled_tools: null` (or another invalid value) is now rejected; that is what the spec says and is visible in `rapid mcp list`.

### Revert cycles

11 mutations, each failing the test named for it: the gate not checking what the server listed; the early refusal missing; a changed filter not followed; a rejected list not claiming its name (the claim, and its trigger); a flag taken for a pattern; the forced-replacement note and the install note missing; the empty-allowlist lines missing in `get`, `list` and `probe`.

### Checks

`rustfmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --no-fail-fast`: 5038 passed, 0 failed (rustfmt and clippy `-D warnings` clean).

## EXT-01-2 — MCP timeouts: start-up and call limits, the `[mcp]` table, the managed ceiling and the timer's lifetime

Restated contract (spec §3.2, AC-05): a server entry may carry `startup_timeout_ms` (1 000–120 000) and `tool_timeout_ms` (1 000–600 000); a `[mcp]` table in the user's `config.toml` and `RAPIDLM_MCP_STARTUP_TIMEOUT_MS` / `RAPIDLM_MCP_TOOL_TIMEOUT_MS` set the same for every server; the highest layer that says anything wins (managed ceiling, environment, user config, the server's own key, built-in); a managed `[mcp] max_tool_timeout_ms` lowers a larger call limit and is reported; a call that outlives its limit reads `mcp_timeout`, the server is sent a cancellation notice, and the call's timer is gone when the call returns; the per-frame bound follows the limit; a server that does not finish starting is offline with `startup_timeout`. With none of it set, nothing changes.

### Phase 0 recheck

| Claim | Finding |
|---|---|
| §2 "a fixed 30 s watchdog thread that is never stopped early" | confirmed: `execute_mcp_tool` spawned a thread that slept in 50 ms steps for thirty seconds and dropped its handle; a call answered in a millisecond left a thread alive for half a minute. It is now `CallTimer`: woken and joined when the call returns, on every path |
| §2 "30 s per frame" and the Phase 0 "current handshake bound" | confirmed: both connect paths built their bounds with a fixed thirty seconds, for `initialize` and again for `tools/list`. That value is the built-in for both limits; each start-up step still gets the whole start-up limit, so a default build is unchanged (a first version that shared one limit across the two steps tightened a 20 s + 20 s server and was dropped before commit) |
| §3.2 "the frame bound follows the limit" | `IoBounds` could not be changed after it was built, and `MAX_IO_TIMEOUT` was 120 s, so a 600 s call limit could not be expressed. `set_io_timeout` is now on the transport (stdio, HTTP, loopback) and the session; the cap is 600 s |
| remote servers | the router's HTTP client reports a read that ran out of time as `ProviderError::Connection`, which the adapter mapped to `HandshakeFailed`: a slow remote server could never have been a `mcp_timeout` or a `startup_timeout`. An exchange that fails only after using its whole allowance (50 ms of slack) is now `Timeout` (found by the first HTTP test, not by reading) |
| §3.2 "best-effort cancellation notification" | nothing was sent; the server went on working. `tools_call_with` now sends `notifications/cancelled` with the request id when a call times out or is stopped, within a one-second deadline of its own, and puts the call's deadline back |

### Criteria

| Criterion | Status | Evidence |
|---|---|---|
| AC-01 with none of the new keys set the timeouts and every existing MCP test are unchanged | met | `with_no_limit_set_the_built_ins_apply_as_before` (30 s and 30 s, every layer `built-in`); `each_start_up_step_gets_the_start_up_limit` and the HTTP twin (a 0.7 s + 0.7 s start is up under one second, as each step always had its own limit); a managed policy without `max_tool_timeout_ms` parses as before (`a_managed_ceiling_on_the_call_limit_is_read_and_held_to_its_bounds`) |
| AC-04 an invalid timeout rejects the entry, with a reason in `rapid mcp list`, the doctor row and the turn warning | met | `the_timeout_keys_are_read_and_held_to_their_bounds` (13 invalid values, both keys, both ends), `list_shows_the_keys_a_server_sets…`, `a_rejected_timeout_still_claims_its_name`, `every_issue_renders_a_non_empty_reason`; the doctor and turn text is the existing rejection text |
| AC-05 a slow stub call fails at the limit with `mcp_timeout` and the cancellation notice is sent | met | `a_call_past_its_limit_fails_with_mcp_timeout_and_the_server_is_told` (stdio: the stub logs `cancelled <the request's id>`), `a_remote_call_past_its_limit_times_out_and_the_server_is_told` (HTTP), the four transport tests (`a_call_that_runs_out_of_time_is_followed_by_a_cancelled_notice_for_its_request`, `…the_caller_stops…`, `…is_answered_sends_no_notice…`, `the_calls_own_deadline_is_back_after_the_notice_went_out`) |
| AC-05 a longer limit really allows a longer call (the frame bound follows the limit) | met with a smaller scale: the limits cannot be below a second, so the proof is a 1 s start-up limit and a 10 s call limit with a 2.5 s call | `a_call_is_not_cut_short_by_the_shorter_start_up_limit`, `a_connected_server_answers_within_the_calls_limit_not_the_start_up_limit`, `a_remote_call_may_take_longer_than_the_handshake_was_given`, `a_changed_limit_is_followed_by_a_server_that_is_already_up`, `a_later_deadline_lets_a_slow_server_answer…` |
| AC-05 the call's timer is observably cancelled and exited when the call returns | met | `CallTimerState` (stopped, exited, timed out) is recorded per call under test: `no_timer_outlives_its_call` (a 600 s limit, a call answered at once and one the caller stops), `a_server_that_keeps_talking_is_still_ended_at_the_calls_limit` (a server that sends a frame every 0.3 s never lets a single wait run out: only the timer ends it, and it is gone) |
| AC-05 a slow handshake is the offline reason `startup_timeout` | met | `a_server_slow_to_start_is_offline_with_startup_timeout` (also the offline marker's failure), `probe_gives_a_slow_server_its_start_up_limit_and_no_more`, `a_remote_server_slow_to_answer_the_handshake_is_a_startup_timeout` |
| AC-05 precedence is tested layer by layer; a managed ceiling lowers a larger value and is reported | met | `the_highest_layer_that_says_anything_wins`, `the_managed_ceiling_lowers_a_larger_call_limit_and_says_from_what`, `get_shows_each_limit_with_the_layer_it_came_from` (all five layers, `lowered-from=`), `the_managed_ceiling_lowers_the_call_limit_the_call_runs_under`, `a_turns_mcp_servers_start_under_the_limit_layers_and_a_lowered_limit_is_warned` (the layers reach the servers a turn starts, and a lowered limit is warned), `the_mcp_check_names_a_lowered_call_limit…` |

### Decisions

- `rapid mcp add` also takes `--startup-timeout-ms`; the spec lists only `--tool-timeout-ms`. Both are checked by the loader's own function before anything is written.
- An unusable value in the environment or the user's `[mcp]` table is **left out and named** (`warning: MCP limits: …` in `list`, `get`, `probe`, the turn warnings and the doctor row), not a config error: a limit that silently did not apply would leave a server on a bound the person did not choose, and the rest of the config still loads (the same as `[skills]`). An unusable value in a server's own entry rejects that entry, as the spec says.
- A call limit the managed ceiling lowered is reported like a managed gate (field, origin, remediation) in the turn warnings and the doctor row, but not through `mcp_gates`, whose wording is "blocked by policy".
- A rejected timeout keeps its name (a plain entry of the same name in the second file is shadowed), as a rejected tool list does.
- D-2 (precedence): S11 as written; the project's per-server key is the lowest layer.

### Recorded, not done

- The cancellation notice is written with a fresh token and a short deadline, but a write to a stdio pipe that is full cannot be interrupted: a server that never reads its input and times out many times in a row could block the notice. The request itself has always been written the same way.
- *(superseded by EXT-01-2-R #2)* A remote server's timeout was first recognised by the exchange having used its whole allowance; the exchange's watcher now enforces the limit and says so.
- *(superseded by EXT-01-2-R #11)* Time a call spends waiting for a person to answer a server's request for input is no longer charged to its limit.
- `MAX_IO_TIMEOUT` is now 600 s for any caller of `IoBounds::new`; outside the transport's own tests the only caller is `mcp_io_bounds`, which both connect paths use.

### Revert cycles

31 mutations, each failing the test named for it (checked, restored, touched): the environment below the user layer; the entry's own key above the user layer; the ceiling not applied; the environment value's range check dropped; the user value's range check dropped; the user layer not read; the loader's range check dropped (the same check `add` uses); a rejected timeout not claiming its name; the managed ceiling's range check dropped; the timer not woken and joined; the timer never reaching its deadline; the call path not setting the frame bound; the stdio connect not setting it after the handshake; the listing given a shorter bound than the start-up limit (stdio and HTTP); a start-up timeout not read as one; a timer-ended call not read as a timeout; the cancellation notice never sent; sent for a failed call too; the call's deadline not put back after it; the HTTP exchange that used its whole allowance not read as a timeout; the HTTP connect not setting the call bound; `get` without the limit lines; `list` without the problems; `list` without a server's own keys; `add` not writing the key; no install note; the doctor row without the findings; the turn not given the layers; the turn not warning of a lowered limit; the connect bound not following the start-up limit. One mutation first read NOT-PROVEN because the cycle's test filter did not name the test that had been renamed; with the filter corrected it fails the test.

### Checks

`rustfmt --check` on the changed files, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --no-fail-fast`: 5078 passed, 0 failed. The full run itself had one failure, `interactive::tests::computer_observe_reports_the_typed_platform_gate_not_a_stub` ("computer-use observation failed: backend": the screen-capture backend was unavailable during that run); it passes alone and does not touch MCP or the timeouts.

## EXT-01-1-R2 — second review round for tool filters (self-review of `3f40dbf`)

The review found no way to call a tool the filter removes by case, prefix or suffix, a duplicate reported name, a hand-built wire name, the offline marker of an online server, or either transport, and confirmed that a server with no filter behaves as before, the refusal is the first thing after the cancel check, the locks are never held together, and the loader's caps come before any allocation. Its nine findings and one note were read against the code.

| # | Finding | Verdict | What changed |
|---|---|---|---|
| 1 | **Medium.** Only a rejected *tool list* held its name, and the lists are validated last: `{command, args: "x", enabled_tools: [...]}` was rejected for `args`, held nothing, and a plain twin in the second file ran with every tool. The `Shadowed` text ("already configured by an earlier file") was also false for a held name | confirmed | an entry that has `enabled_tools`, `disabled_tools`, `startup_timeout_ms` or `tool_timeout_ms` and is rejected for any reason holds its name; the later one is `HeldByRejected`, naming the file: `a_rejected_tool_list_still_claims_its_name…` (the reviewer's case and a timeout-only case added), `a_rejected_timeout_still_claims_its_name` |
| 2 | An entry that became invalid mid-session leaves the connection running with its old filter while the warning says "not registered" | confirmed, **recorded** | unchanged and documented: the session keeps what it had until it ends; making the warning say so needs the registry to know which names were rejected, which it does not |
| 3 | The connection of a server that did not come up had no filter, so a call to a tool the filter removes (`mcp__demo__delete_issue` while `demo` is down) could raise an approval and run hooks, then fail "offline" | confirmed | the offline connection keeps the filter; for a down server the filter alone decides (nothing was reported) and the marker tool always passes, because it says why the server is down: `a_filtered_tool_of_a_server_that_is_down_is_refused_before_anything_asks` |
| 4 | `add --disabled-tool -x` is refused though `-` is a legal pattern character | confirmed, **documented** | a pattern starting with `-` can only be written by hand; the flag check exists so that `--disabled-tool --force` does not take `--force` for a pattern (the docs say so) |
| 5 | Loosening a server that had nothing offered appended its block last, changing which server wins the 20 KiB cap although the notes said "surface order kept" | confirmed | the block goes before the first server registered after it: `a_loosened_server_gets_its_block_back_in_registration_order` |
| 6 | The gate inside `execute_mcp_tool` is unreachable in tests past the early one, so reverting `permits` there failed nothing | confirmed (test gap) | `the_call_path_itself_refuses_a_name_the_server_never_listed` calls dispatch directly with a name the server never listed |
| 7 | `get` printed `enabled-tool=(none: …)` in the key namespace of real patterns | confirmed | `enabled-tools=(none: …)` |
| 8 | `a_changed_filter_is_followed…` would pass if the server were killed and started again | confirmed (test gap) | the stub records each start; the test asserts one |
| 9 | The `pre_tool_use` assertion was only meaningful with the approval check | confirmed (test gap) | `a_filtered_call_runs_no_hook_in_a_permissive_mode_either`, with a control that the hook does run for a tool in the set |
| note | The collision report prints a server's tool names raw (control characters, any length) into a warning | confirmed (older code) | the names go through `label` (bounded, control characters flattened): `a_collision_report_bounds_and_flattens_the_names_a_server_chose` |

### Revert cycles

11 mutations, each failing the test named for it: the claim keyed on nothing; the held name reported as shadowed; the offline connection without its filter; the marker not passing for a down server; the early gate skipping a down server; the block placed after the later servers; dispatch checking the pattern only; the old `get` key; the collision names raw; the server restarted on a filter change; the early gate removed (the hook then runs).

### Checks

`rustfmt --check` on the changed files, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --no-fail-fast`: 5083 passed, 0 failed.

## EXT-01-2-R — review follow-up for MCP timeouts (self-review of `a21fe58`)

The review found no ceiling bypass, no input to the layers that panics, and no path out of a call that leaves the timer running; it confirmed that a late reply to an abandoned call is skipped by id and that every stdio failure path kills the child. Its eleven findings were read against the code.

| # | Finding | Verdict | What changed |
|---|---|---|---|
| 1 | **Medium.** `/mcp` in the terminal UI read only the managed variables, so `get` and `probe` ran under different limits than a turn, and a bad `RAPIDLM_MCP_*` value went unnamed | confirmed | `mcp_env` uses the same environment as a turn (`integration_env`); recorded without a test of its own (it is a one-line use of a function that reads the process environment, which a test must not set) |
| 2 | **Medium.** A remote exchange was called a timeout when it failed after `T − 50 ms` of total time, name lookup and connecting included: a refused connection or a certificate failure on a slow path read `startup_timeout`, and a slow connect plus a silent server could run to twice the limit | confirmed | the elapsed-time guess is gone. The exchange's watcher enforces the limit as a whole and says it was the limit that ended the exchange; the router's own limit is set half a second past it so that the limit, not the router's, ends a slow exchange: `a_remote_call_past_its_limit…`, `a_remote_server_slow_to_answer_the_handshake…`, `a_refused_connection_is_a_failure_not_a_timeout_whatever_the_limit` |
| 3 | **Medium.** The cancellation notice's one-second deadline is not a bound: the stdio send never reads the transport's timeout and a write to a full pipe cannot be interrupted; over HTTP the notice can take a lookup, a connect and a read; and an interrupt (Ctrl-C) now waited for it | confirmed in part | the notice is no longer sent for an interrupt (what a person does is not announced, as before); it is sent only for a timeout, by the caller (`McpSession::notify_cancelled`), and only for a call that was sent. The docs no longer say "within one second". **Recorded:** a write to a pipe the server has stopped reading cannot be interrupted, as the request's own write never could; fixing that is a writer thread in the stdio transport |
| 4 | The registry's lock was held for the whole call, now up to 600 s | confirmed | the session is shared (`Arc`) and the registry's lock is released before the call: `the_registry_is_free_while_a_call_runs` |
| 5 | The notice's reason read "the client cancelled the call" for a timeout, and it could go out for a request never written | confirmed | the reason is "the client gave up waiting"; a call stopped before it was written is not spoken of: `nothing_is_said_of_a_call_that_was_answered_failed_or_never_sent` |
| 6 | Environment and user-config problems were warned every turn, in a project with no MCP server | confirmed | warned only when the project has a server: `a_project_with_no_mcp_server_is_not_warned_about_mcp_limits` |
| 7 | An offline server is retried every turn, so a permanently slow one costs up to twice its start-up limit each time | confirmed, **recorded** | the existing rule ("one bad start must not be a session-long outage") unchanged; the cost is the limit the person chose |
| 8 | Test gaps: one-second limits with a python stub; no test of an abandoned call's late answer; the HTTP arm of `set_io_timeout` and the timer were not exercised through dispatch | confirmed | steps of 0.6 s and wider margins; `the_answer_to_a_call_given_up_on_is_not_taken_for_the_next_calls`; `a_remote_servers_call_is_held_to_its_limit_through_dispatch_too` |
| 9 | A managed ceiling outside 1 000–600 000 ms fails the whole policy (and so every project hook and server) instead of clamping | confirmed | any positive number loads (one above 10 minutes lowers nothing): `a_managed_ceiling_on_the_call_limit_is_read_as_a_positive_number` |
| 10 | The user config was opened without a check that it is a file (a pipe would block), and a relative `RAPIDLM_CONFIG` silently gave no user layer | confirmed | only a regular file is read, and a relative path is named: `a_config_that_is_not_a_file_is_not_read_and_does_not_block`, `a_relative_config_path_is_not_read_and_says_so` |
| 11 | Time spent waiting for a person's answer to a server's request for input counted against the limit, so with the one-second floor a person could not answer in time | confirmed | the timer is paused while the person is asked: `time_spent_waiting_on_a_person_is_not_charged_to_the_calls_limit`. **Recorded:** a server that goes on running after its request for input was cancelled delays the approval prompt by up to the limit, as it did by thirty seconds |

### Revert cycles

11 mutations, each failing the test named for it (one expected a third test that passes because the call timer also ends a remote call at the dispatch level): the notice marked sent after the send; an interrupt announced; the registry held; no pause while a person answers; the exchange's limit not reported as a timeout; a zero ceiling accepted; the config read without the file check; the relative-path note dropped; the limit warnings sent for a project with no server; the notice sent more than once; the answered call still spoken of.

### Checks

`rustfmt --check` on the changed files, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --no-fail-fast`: 5092 passed, 0 failed.

## EXT-01-3 — Deferred MCP definitions: the manifest, `mcp_find`, `mcp_load`, the loaded set, its records and its staleness

Restated contract (spec §3.3, AC-06…AC-13): a server entry may carry `deferred: true` (and the user's `[mcp] default_deferred` sets the default); the tools in a deferred server's effective set are not in the per-step tool list, and the context carries a manifest of them instead (one line per tool, 4 KiB a server, 8 KiB in all, fenced as untrusted); two read-only tools, `mcp_find` and `mcp_load`, exist only while a deferred server is connected; a loaded tool is in the tool list from the next step; a call to one that is not loaded is `mcp_tool_not_loaded` and does not reach the server; the loaded set is bounded by `max_loaded_tools` and the provider, evicts the least recently called, goes stale with the server's schema, and is a projection of the ledger. A project with no `deferred` anywhere sees nothing change.

### Phase 0 recheck

| Claim | Finding |
|---|---|
| §3.3 "the loop already rebuilds the surface every step" | confirmed: `ModelStepInput::tool_surface` is built each step; the end-to-end test sees the loaded tool appear at step 2 of one turn |
| §3.3.1 "a sanitizer … `model::fence_untrusted`" | the request builder already fences every block whose trust is `Untrusted` with the block's locator; the manifest is therefore compiled as `retrieved` blocks named `mcp-manifest:<server>` and needs no fence of its own. The sanitizer (control characters other than newline and tab become spaces) is applied to every server text before it is shown |
| §3.3.9 "a projection of the ledger (S6)" | the session's history is already read back by following `session.forked` into the parent up to `source_seq` (`conversation_through`); the loaded set is read the same way (`loaded_set_for`), folding `mcp.tools_loaded`, `mcp.tools_unloaded` and `tool.requested` for `mcp__` tools (which restores the order of calls, so the next eviction is the same one). A `/rewind` is a fork at an earlier seq, so it needs nothing of its own |
| `EventFamily` has no `Mcp` | the two kinds are filed under `tool` (as `skill.activated` was): adding a family changes every exhaustive match and the SDK's family union for no reader's benefit. Edits: `event.rs`, `wire.v1.json`, the regenerated `index.ts` and its test count, `WIRE_SCHEMA_SHA256` in `daemon_serve.rs`, `event-catalog.md` |
| §6 "a schema larger than 16 KiB or a description larger than 4096 bytes … fails the whole model step as it can today" | confirmed in the surface code (`tool_surface` pushed every registered tool): for a server that is not deferred such a tool is now left out of the list; for a deferred one it is `too_large` |
| `mcp_load` in a batch | tools of class ReadOnly with kind Write get a group each, so two loads in one batch ran concurrently on one set; loads are given one group and run in the order asked |
| staleness needs `tools/list` again | the registry listed a server's tools once, at connect; a turn now lists each deferred server's tools again (`resync_deferred`), which is also what finds a tool that vanished |

### Criteria

| Criterion | Status | Evidence |
|---|---|---|
| AC-06 a deferred server's tools are not in the step's tool list; `mcp_find`, `mcp_load` and the fenced manifest are; with no deferred server none of the three exists | met | `a_deferred_servers_tools_stay_out_of_the_tool_list_and_the_finders_and_manifest_come` (both cases), `a_deferred_server_is_loaded_within_a_turn…` (the first step's list and the manifest block in the context), `a_deferred_servers_manifest_is_compiled_as_untrusted_text_under_its_own_locator`, `nothing_is_deferred_where_a_load_could_not_be_recorded` |
| AC-07 `mcp_load` makes the tools appear in the next step's list within the turn; unknown, filtered and oversize names get their own typed result without aborting the batch | met | `mcp_load_answers_each_name_and_the_tools_join_the_next_tool_list`, `a_tool_the_provider_would_refuse_is_left_out_not_the_whole_step`, the end-to-end test (step 2 of one turn), `loading_answers_each_name_and_loads_what_it_can` |
| AC-08 a deferred tool that is not loaded is `mcp_tool_not_loaded` and the stub records zero `tools/call` | met | `a_deferred_tool_that_is_not_loaded_is_refused_without_contacting_the_server_or_asking` (through the executor and straight into dispatch; no approval asked, no call log) |
| AC-09 a restart, a fork at an earlier seq and a rewind rebuild the loaded set from the ledger alone | met | `a_deferred_server_is_loaded_within_a_turn_and_the_load_outlives_a_restart_and_follows_a_fork` (a fresh registry in the same session lists and calls the loaded tool at its first step; a fork at the tip has it, one before the load does not, one before an unload still has the tool), `what_was_called_decides_what_is_evicted_next_after_the_set_is_rebuilt`, `the_set_is_rebuilt_from_the_records_and_the_calls` |
| AC-10 a changed digest or a vanished tool unloads with `stale`, writes `mcp.tools_unloaded`, regenerates the manifest and adds the transcript line | met | `a_changed_schema_a_vanished_tool_and_a_server_that_went_down_unload_what_was_loaded` (digest, vanished, then a server that exits: `server_offline` and its marker in the manifest), `tools_that_left_the_model_because_the_server_changed_leave_a_line_each` (the terminal UI's line comes from the ledger record, so a resumed session shows it too) |
| AC-11 over random load/call/evict sequences the tool list never exceeds the provider ceiling and the loaded set never exceeds `max_loaded_tools`; eviction is least-recently-called | met | `over_random_load_call_and_unload_sequences_the_bounds_hold_and_eviction_is_by_call_order` (200 seeds, 120 steps each: count, slots and bytes bounds, no oversize tool, no victim younger than one left), `a_full_set_unloads_the_least_recently_called_and_reports_it`, `the_loaded_set_never_outgrows_what_the_provider_and_the_cap_take_and_evicts_by_call_order`, `the_loaded_set_leaves_room_for_the_tools_of_servers_that_are_not_deferred` |
| AC-12 a manifest or finder description with a closing fence or a `DATA_URL:` line is neutralised and a control character is a space; the bounds hold | met | `a_manifest_description_cannot_close_the_fence_or_carry_a_control_character`, `a_control_character_becomes_a_space_and_newline_and_tab_stay`, `a_server_takes_at_most_4_kib_and_the_rest_is_counted`, `all_servers_together_take_at_most_8_kib_and_each_still_says_what_it_left_out`, `a_find_result_is_the_wire_name_and_a_description_cut_to_200_bytes`, `mcp_find_returns_fenced_ranked_results…` |
| AC-13 `mcp_find` scoring and order match §3.3 on a fixed corpus; plan mode permits both tools and still denies or asks for the loaded tool's call | met | `find_scores_three_for_a_token_in_the_name_and_one_in_the_description`, `a_name_is_worth_three_descriptions_and_a_description_one`, `an_empty_query_browses_in_wire_name_order_and_a_server_narrows_it`, `plan_mode_permits_the_finders_and_still_holds_back_the_loaded_tools_call` (the loaded tool is denied in plan mode and the server is not called) |

### Decisions

- The records carry names, schema digests, `definition_bytes[]` (a size, not text) and `loaded_set_size`; `origin` is always `model`, because restoring a set from the ledger writes nothing.
- A query token is "found" when the lowercased name or description contains it (so `issue` finds `search_issues`); a repeated token counts once; the default `limit` is 10; "name" is the server's own tool name.
- The manifest is rebuilt at the start of each turn, which is the only time it can change (staleness and a server going down are both found then), and enters the context as `retrieved` blocks scored above retrieval.
- `max_loaded_tools` accepts 1–1024 as written; the provider's room (64 tools less the built-in ones, the two finder tools and the plain servers' tools) lowers it, and `rapid doctor` says when it did.
- Where no ledger sink is attached (a workflow step, a child, a run that is not recorded) nothing is deferred and the server's tools are listed in full.
- The offline marker of a deferred server that is down is shown in the manifest, not in the tool list.

### Recorded, not done

- `rapid mcp get` does not print "N of M tools enabled" and the estimated definition sizes (`list`, `probe`, doctor, `/mcp`): that is slice 4 (AC-14).
- The loaded set is read from at most the newest 65 536 events of each session in the fork chain; a session longer than that, with a load only in its older part, restores without that load.
- A listing failure for a deferred server at the start of a turn leaves its loaded tools as they were (it cannot be told whether they are stale).
- The `tools/list` pagination decision D-1 is unchanged; a deferred server is the one that benefits from it.

### Revert cycles

39 mutations, each failing the test named for it (three first read NOT-PROVEN and were strengthened: a name outweighing two descriptions, a repeated query token, a control character on a manifest line's first line): the weights of the score; the order of equal scores; the repeated token; the limit clamp; the eviction order and a batch evicting its own loads; a changed digest not being stale; the per-server and total manifest bounds; the sanitizer; the oversize refusal; the byte budget; a deferred server's tools staying in the list; the finder tools without a deferred server; deferral without a record sink; the refusal for want of a load (before the permission check, and in dispatch); the load and unload records; the touch that orders evictions; the stale unload record and the re-listing; an oversize tool left out of the list; the loads serialized; the finders' read-only class; a down server's marker in the tool list; the byte and slot budget; the fork chain; the unload fold; the restore on a new session; the manifest in the context; the calls folded into the order; the `deferred` key's validation, `add --deferred`, the default and the doctor's note; the terminal UI's line.

### Checks

`rustfmt --check` on the changed files, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --no-fail-fast`: 5139 passed, 0 failed; the SDK's generated-types test (`node --test test/generated.test.ts`) passes with the regenerated file.

## EXT-01-4 — MCP visibility and the reference page

Restated contract (spec §3.4, AC-14): `rapid mcp get|list|probe` and the offline doctor row show tool counts, resolved timeouts with their source layer and the *estimated* size of the definitions; doctor starts no server; `/mcp` shows the same plus, per session, the loaded tools.

### Phase 0 recheck

| Claim | Finding |
|---|---|
| §3.4 `get` prints "12 of 40 tools enabled" | `get` and `list` read the settings and start nothing, so the reported count is not known to them (said in EXT-01-1). The counts are printed where a server has been asked: `rapid mcp probe` (`tools=<offered> of=<reported>`) and `/mcp`, which reads the session's live registry. `list` and the doctor row say plainly that they did not measure (S9) |
| §3.4 "estimated definition size (bytes / 4)" | the surface counts a tool as its wire name, description and serialized schema; that is the byte count, divided by four and always labelled an estimate. A deferred server also gets the estimate of its manifest, so the saving is visible |
| §11 "a new `docs/reference/mcp.md`; the MCP section still says stdio only in places" | the command reference no longer says stdio only (EXT-02-0 fixed it); the new page holds the rules and the command reference keeps the commands and a short pointer for filters, deferral and timeouts |

### Criteria

| Criterion | Status | Evidence |
|---|---|---|
| AC-14 `rapid mcp get\|list\|probe` and the offline doctor row show effective counts, resolved timeouts with their source layer and the estimated size; doctor starts no server | met, with `get` and the doctor row saying "not measured" where nothing was started | `get_shows_each_limit_with_the_layer_it_came_from`, `deferral_is_set_listed_and_shown_with_who_said_so` (`get`), `probe_sizes_what_a_server_offers_as_an_estimate_and_a_deferred_one_by_its_manifest` (`tools=N of=M estimated-tokens=T`, and for a deferred server `manifest-estimated-tokens=`), `list_says_what_it_does_not_measure`, `the_mcp_row_does_not_claim_a_tool_count_it_did_not_measure`, `the_mcp_check_says_when_the_loaded_set_is_larger_than_the_provider_takes` |
| `/mcp` shows the same plus, per session, the loaded tools | met | `a_deferred_server_is_loaded_within_a_turn_and_the_load_outlives_a_restart_and_follows_a_fork` reads `McpRegistry::live().render()` after the turn: `demo up: 2 tools enabled, definitions about N tokens (estimated); deferred, its manifest about M tokens (estimated)` and `loaded by the model (1):` with the tool |

### Revert cycles

6 mutations, each failing the test named for it: the estimate's divisor; the deferred part of a probe line; the list's note; the loaded tools in `/mcp`; the doctor's wording; the manifest and the full size swapped.

### Checks

`rustfmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --no-fail-fast`: 5142 passed, 0 failed.

## EXT-05-1 — Agent definitions as Markdown: the shared front matter reader, the loader, the commands, the hardening

Restated contract (spec §3.1, AC-01…04, AC-11): a definition is `<id>.md` (front matter, then the instructions as the body) or `<id>.toml`; both become one in-memory definition through one validator; the front matter reader is shared with the skills loader and lives in `crates/protocol`; a definition of an id that is already held is refused; `<id>.md` beside `<id>.toml` refuses both; a symlinked definitions directory is skipped with a reason; `[agents] extra_dirs` adds directories below the user's; `rapid agents list|validate` do not read an untrusted project's directory; `scaffold` writes either form.

### Phase 0 recheck

| Claim | Finding |
|---|---|
| §2 "no frontmatter parser for agents; the skills loader's tokenizer is private" | confirmed; the skills reader (`decode_frontmatter_map`) is moved to `protocol::frontmatter` and the loader reads through it. The privilege-key denylist, the alias check and the cancel stride stay in the skills schema and are handed to the tokenizer as callbacks |
| §2 "a symlinked `agents/` directory is followed" | confirmed (`read_dir` followed it); it is now one refusal naming the directory |
| §2 "`list` and `validate` read the project directory without a trust check" | confirmed; `list` without `--dir` now reads the project's directory only when the project is trusted, `validate` reads nothing and exits 3 |
| §2 "`USER_DEFS_DIR` is unused and names another path" | confirmed (`.rapidlm/agents`, while the loader reads `<home>/agents`); it is now `agents`, and `user_agents_dir` uses it |
| §3.1 "`name` defaults to the file stem and must equal it" | built; the TOML form keeps `id`, which is not tied to the file name (it never was) |
| invalid UTF-8 in a definition | a TOML file that was not UTF-8 failed the whole directory (`read_to_string` was `?`-propagated); it is now one refusal. Listed as a change in the spec's implementation note |
| `list`/`validate` and the RapidLM home | `user_agents_dir` created the home when it was missing; the commands now resolve it without making it (as `rapid skills` does) |

### Criteria

| Criterion | Status | Evidence |
|---|---|---|
| AC-01 existing definition tests pass unchanged; a v1 TOML file loads and behaves identically; the spawn path, the `task_spawn` description and `/agents` panel are unchanged without new keys | met | the 11 existing `agent_defs` tests, the existing `binary_a_defined_agent_type_sets_the_childs_model_effort_instructions_and_tools` and the spawn/panel tests run unchanged; the skills tests run unchanged on the shared reader |
| AC-02 Markdown and the equivalent TOML give identical definitions (property test); the body is in the child's prompt in the overlay section and never replaces the base prompt; a templating attempt is rejected | met | `a_markdown_definition_and_its_table_form_mean_the_same_over_generated_definitions` (30 000 generated definitions, 1 000+ accepted and 1 000+ refused, same outcome and same definition), `binary_a_markdown_agent_definition_shapes_the_child_like_a_table_one` (the body, model, effort and only the narrowed tools in the child's own request), `the_front_matter_key_set_is_closed_and_the_body_cannot_replace_the_base_prompt` (`instructions`, `id`, `role`, `tools`, `system_prompt`, `prompt`, `extends`, `template`) |
| AC-03 rejections have typed reasons shown by `list`/`validate`: unknown key, `name` ≠ stem, `.md`/`.toml` collision, body over 8 KiB, bad pattern, `allow` in `permission`, `max_steps` out of range, non-empty `spawn`, symlinked directory | met for this slice's rows (the others belong to slices 2–3) | `the_front_matter_key_set_is_closed…`, `the_name_is_the_file_stem_when_left_out_and_must_equal_it_when_given`, `a_markdown_and_a_table_definition_of_one_id_are_both_refused_naming_each`, `a_body_over_the_cap_is_refused_naming_the_body`, `a_symlinked_definitions_directory_is_skipped_with_a_reason`, `front_matter_outside_the_subset_is_refused_with_its_line`, `a_directory_loads_both_syntaxes_and_one_bad_file_does_not_hide_the_rest`, `a_directory_named_with_dir_is_read_whatever_its_projects_trust` |
| AC-04 `rapid agents list\|validate` on an untrusted project loads nothing and says why | met | `an_untrusted_projects_directory_is_not_read_and_the_command_says_why` (both commands, then the same after `rapid trust grant`) |
| AC-11 the command table tests pass; `rapid agents scaffold --format md` output validates | met | `both_scaffolds_validate_as_written_to_a_directory`, `both_scaffolds_parse_back_and_validate`, and the docs-consistency tests in the full run |

### Decisions

- The Markdown form is turned into the `[agent]` table the TOML form has and then read by that one validator, so equivalence holds by construction; the property test is what would catch a mapping that drifted.
- `--dir <path>` is read whatever its project's trust: the files are inert, and checking a definition before trusting the project is what `validate` is for. Without it the project's directory is read only when trusted, as a spawn does.
- A Markdown file whose stem is not a valid id (a `README.md`) is refused like any other; the docs say to keep notes outside the directory. (Ignoring files without a front matter fence would hide a mistyped fence.)
- `[agents] extra_dirs` is read only from the user's config, with the `[skills]` reader generalised over the table name (same bounds, same messages), so a project cannot name a directory for its own definitions to come from.
- `tools_key` is carried by the two errors that name it, so a Markdown file is told about `tool_classes` and a table about `tools`.

### Recorded, not done

- The `/agents` panel rows do not show the format yet (slice 3 reworks them).
- No ADR yet: ADR 0026 lands with the narrowing and the primary agent (slices 2 and 4).
- The `permission` nested map is accepted by the tokenizer under `Rules::nested_maps` but not turned on for agents until slice 2.

### Revert cycles

39 mutations, each failing the test named for it: the body trim, the CRLF read, an empty body, the `name` default, the `name` check, the closed key set, the `tool_classes` mapping, the collision rule, the symlinked-directory skip, the extra-directory cap and shadowing, the per-file UTF-8 refusal, the two `tool_classes` messages, the line number in a syntax refusal, `USER_DEFS_DIR`, the body's name in the size refusal, the `.md` extension, the trust lookup and the exit code for an untrusted project, `extra_dirs` and its problems, the `--dir` branch, the one-line output, the format column, the scaffold default and its Markdown form, the home that must not be made, the `[agents]` config table and its keys, the generalised table name and own-directory check, and in the shared tokenizer the `\t` escape, the key-with-a-space rule, the nested-map rule and the closing fence (each against both the tokenizer's own tests and the skills differential tests).

### Checks

`rustfmt --check` on the changed files, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --no-fail-fast`: 5171 passed in the full run and one known flake (`twice_the_ceiling_queues_in_order_rather_than_erroring`, an admission-queue ordering race under load that this slice does not touch; 3 of 3 green alone). A first run of the skills differential test failed once under load: the test shared one one-second clock across 120 000 cases, and the two readers check it at different points; each case now has its own.

## EXT-01-3-R — review follow-up for deferred MCP definitions (self-review of `188d31b`)

The review read the commit and reported eleven findings. Each was read against the code and all were confirmed; every one was fixed or its limit recorded.

| # | Finding | What was done |
|---|---|---|
| 1 | high: `execute_mcp_tool` held the registry while taking the loaded set, `mcp_load` the set while reading the registry — a load and a call on two threads of one step could lock each other out; and `[mcp_load X, call X]` ran its two halves in whichever order the threads got there | the call takes the set only after the registry is let go (it now touches the set when it is *proposed*, see 7), and a step with a load in it runs its MCP calls in the loads' group, in the order asked. `a_call_waiting_to_record_itself_does_not_hold_the_registry_a_load_needs`, `a_load_and_the_calls_of_one_step_run_in_the_order_they_were_asked` |
| 2 | high, older than the commit: the provider layer refused every `mcp__<server>__<tool>` name (two `_` in a row), so any step carrying an MCP tool — or a call of one — failed as `ProviderFailed{Rejected}`; the alphabet, the 128-byte bound and a `null` or non-object schema were not checked either | `ToolName` accepts runs of `_` (the other separators still come alone); a tool whose wire name the provider layer would refuse, or whose schema is not an object, is `unusable`: left out of the list (counted), and for a deferred server out of the manifest, the finder and the load; a `null` schema is read as no parameters. Found by a test that builds the request. `an_mcp_tools_wire_name_is_a_name_the_provider_layer_takes`, `a_tool_name_may_hold_a_run_of_underscores_and_nothing_else_may_repeat`, `a_tool_the_provider_would_refuse_by_name_or_schema_is_never_offered`, `a_null_input_schema_is_no_parameters…` |
| 3 | medium: every turn re-listed each deferred server with the *call* limit (up to 600 s) and no way out; a listing that failed left the server up and its tools loaded | the listing gets the start-up limit; a server that does not answer is down for the turn (its marker in the manifest, its loaded tools unloaded `server_offline`) and the next registration connects it again. `a_server_that_stops_answering_the_listing_is_down_for_the_turn_and_not_waited_on` |
| 4 | medium: a record that could not be written was dropped without a word, and a ledger that could not be read left a session marked as restored with an empty or partial set | the sinks say whether they wrote; the load's result carries a note when one did not; an unreadable ledger gives no set, the session is not marked restored and the next turn says so and tries again. `a_load_the_session_record_could_not_take_says_so…`, `a_ledger_that_could_not_be_read_is_said_once_and_tried_again`, the unreadable-ledger assertion in the end-to-end test |
| 5 | medium-low: nothing held a restored or already-listed set to the budget, the provider's 64 tools were not enforced on the list, and a loaded definition's size was fixed at load time | the list stops at the provider's tool limit; at the start of a turn a loaded tool takes its current size and the set is trimmed to the budget, least recently called first (`evicted`, a line each). `a_plain_server_with_more_tools_than_the_provider_takes_is_listed_up_to_the_ceiling`, `a_loaded_set_that_outgrew_its_budget_is_trimmed_when_the_turn_starts`, `a_description_that_grew_is_counted_at_its_new_size…`, the shrink and refresh unit tests |
| 6 | low-medium: a name answered `already_loaded` could be evicted by a later name of the same request | a name asked for is protected for the rest of the request. `a_name_asked_for_that_is_already_loaded_is_not_unloaded_for_a_later_one` |
| 7 | low-medium: `called_at` differed between a live set (touched when a call ran) and a rebuilt one (touched at every `tool.requested`) | the live set is touched when a call is proposed, in proposal order, which is what `tool.requested` records; the touch at execution is gone. `a_call_counts_as_the_most_recent_when_it_is_proposed_whatever_becomes_of_it` |
| 8 | low: server text in names was written raw; U+2028/2029 and bidi marks passed the sanitizer; a cut line could be 3 bytes over; the manifest could pass its bound by a byte; the parameter list of a name over 64 bytes was lost | a name outside the provider's alphabet is never shown (2); the sanitizer also turns the line and paragraph separators, the zero-width and direction marks and the BOM into spaces; a cut and an echo are within their bound with the ellipsis counted; the overflow reserve is exact; the parameters are looked up by the name asked. `what_changes_a_line_on_a_screen_is_a_space…`, `a_cut_description_and_an_echoed_name_are_within_their_bound…`, `a_manifest_never_goes_past_its_bounds_whatever_the_lengths_and_counts`, `a_loaded_tools_parameters_are_found_by_the_name_asked_not_its_cut` |
| 9 | low: a query with no letter or digit (`?`) browsed like an empty one | it finds nothing. `a_query_of_nothing_but_punctuation_finds_nothing_where_an_empty_one_browses` |
| 10 | low: a server that stopped being deferred had its loaded tools reported as "changed on the server"; the doctor's count left out the `skill` tool | "is no longer deferred"; the count includes the finders and the skill tool whether or not skills are bound (the room is never promised larger than it is). `a_server_that_is_no_longer_deferred_says_so…`, `the_doctors_count_of_built_in_tools_leaves_room…` |
| 11 | tests: the plan-mode test accepted three different outcomes; nothing exercised a load beside a call, a hung listing, a record that could not be written or a ledger that could not be read | the plan-mode test accepts one outcome (denied, for plan mode); the cases above are tested |

Recorded, not changed: the §6 interrupt line (a load is one step with no point to cancel at) holds by construction and is not tested; the 65 536-event window of the restore stays; an offline server is still retried at every turn's registration.

### Revert cycles

29 mutations, each failing the test named for it.

### Checks

`rustfmt --check` on the changed files, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --no-fail-fast`: 5197 passed, 0 failed.

## EXT-05-1-R — review follow-up for Markdown agent definitions (self-review of `39ada54`)

The review reported two medium and six lower findings. Each was read against the code; all were confirmed in substance and fixed or recorded.

| # | Finding | What was done |
|---|---|---|
| 1 | medium: the `.md`/`.toml` clash compared neighbours in the sort, so `x.md`, `x.old.toml`, `x.toml` slipped through (and then dropped the whole directory as a duplicate id) | the files are grouped by name; exactly the twins are refused, naming both. `a_markdown_and_a_table_definition_of_one_id_are_both_refused_naming_each` (now with a file that sorts between them) |
| 2 | medium: the symlinked-directory check looked at the last path component only, so a trusted project committing `.rapidlm` as a link to elsewhere was read; the user's own linked `agents` directory is dropped | a trusted project's directory is read only if it resolves inside the project (`resolves_inside`: every link followed, the root too) — in `rapid agents` and in the spawn inventory, one refusal each. The user's own linked directory stays refused (D-4: every loader refuses links) and the page says to name its target in `[agents] extra_dirs`, which is followed. A check-then-open race remains, as for the file-level check. `a_directory_reached_through_a_link_out_of_the_project_is_not_inside_it`, `a_trusted_projects_link_out_of_the_project_is_not_followed` |
| 3 | low-medium: a role the file names was echoed unbounded; reasons and descriptions were printed raw by `list` and the `/agents` panel; bidi and line-separator characters passed the one-line rule | `shown()` on the role; `rapid agents` and the panel print every word of a file through `plain`/`bounded` (control, line-splitting and reordering characters as `?`, a reason at 400 characters, a description at 200). `a_role_the_file_names_is_echoed_bounded`, `what_a_file_says_is_one_clean_bounded_line`, `a_definitions_own_words_are_one_clean_bounded_row` |
| 4 | low: the opening fence was accepted without a line to itself (`---description: x`), and the refusal's line number was then off by one | for a definition the opening fence is `---` and a line break (the shared reader stays lenient for skills, whose behavior is unchanged). `the_opening_fence_is_a_line_of_its_own` |
| 5 | low: the skills differential test never reached the scalar bound, cancellation or an expired time; the old reader judged an item with no key open before the item itself, the new one after | the generator now goes past the bound, adds long items and runs each case also cancelled and with its time up (counting what each reached); the tokenizer judges the missing key first. The comment on `decode`'s callbacks is corrected. `moving_the_tokenizer_changed_no_answer_over_generated_blocks` |
| 6 | low: the equivalence claim was wider than the corpus; a one-line list split on any Unicode white space; a file name that is not UTF-8 skipped the `name` check | the page lists exactly what one syntax can say and the other cannot; lists split on ASCII white space; a name that is not text is not a definition. `names_in_a_list_are_split_on_ascii_white_space_only`, `a_file_name_that_is_not_text_is_not_a_definition` |
| 7 | low: `rapid agents --help` never reached the command's usage; `validate` of a typo'd or empty directory printed nothing and exited 0; `Scope.named` was written and never read; the page said `extra_dirs` lines came last; scripts see new output | the command prints its own usage; `validate` prints a `summary` line and refuses a named directory that is not there; `named` decides that; the page is corrected and lists the output changes. `binary_agents_help_is_the_commands_own_usage`, `a_named_directory_that_is_not_there_is_refused_and_an_empty_one_says_so` |
| 8 | low: two tests could not fail (a project `extra_dirs` that was never a candidate; an untrusted directory with only a valid file) | the first is replaced by a test of the real guard (a relative config path names no directory); the second has a file that would be refused, so a read would show |

Recorded, not changed: a `README.md` or a notes `*.md` in a definitions directory is a refused definition (the page says so); the 128-file cap counts `.md` files and fails a directory as a whole; `.MD`, `.markdown` and `.yml` files are ignored silently; Windows has no `~\` expansion for `extra_dirs`, as for skills.

### Revert cycles

18 mutations, each failing the test named for it.

### Checks

`rustfmt --check` on the changed files, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --no-fail-fast`: 5208 passed, 0 failed.

## EXT-01-3-R2 — second review round for deferred MCP definitions (self-review of `cbd5679`)

The second review read the follow-up commit and reported eleven findings. Each was read against the code; all were confirmed, and each was fixed or its limit recorded.

| # | Finding | What was done |
|---|---|---|
| 1 | medium: "the provider would refuse" was judged by the router's validator (dots, 128 bytes), not by the providers' own rule, so `get.issue` or a 70-byte name was listed and failed every later step | a name is usable only if every provider takes it: letters, digits, `-` and `_`, 64 bytes with the `mcp__<server>__` prefix (`portable_name`). A tool called `get.issue` is not offered (aliasing a name is a later possibility). `a_name_every_provider_takes…`, `a_tool_the_provider_would_refuse_by_name_or_schema_is_never_offered` (boundary at 64/65 bytes) |
| 2 | medium: the turn-start notices were dropped on every interactive path (a restore that failed, an unload that could not be written) and the terminal UI showed only `stale` and `server_offline` | the turn start returns the unload lines (on the ledger) and the other notices separately; the headless path prints both, an interactive turn posts the others as notices; the terminal UI now also shows `evicted` ("unloaded to make room") and the new `undeferred`. `what_the_turn_start_says_is_told_apart…`, `tools_that_left_the_model_because_the_server_changed_leave_a_line_each` (not tested: the notice reaching the terminal through `notify` itself) |
| 3 | medium: a failed restore kept the previous session's loaded set, so a resumed or forked session offered tools its record never loaded | the set is dropped (empty, the session not marked restored), and the turn says that none is loaded. `a_failed_restore_drops_the_other_sessions_tools_and_a_bounded_one_says_so` |
| 4 | medium: any failed `tools/list` — a "busy" error included — unloaded everything for that server and had its child killed | only a timeout, a closed transport or an I/O error marks a server down; an error answer leaves it as it was. `a_server_that_answers_the_listing_with_an_error_is_left_as_it_was` |
| 5 | medium: the listing was bounded per frame, so a server sending notifications for ever blocked the start of every turn | the whole listing runs under the start-up limit's timer (the call timer), which cancels it. `a_listing_that_never_ends_is_cut_off_as_a_whole_not_frame_by_frame` |
| 6 | medium-low: a partial read of the record could be reported complete (a record past the window, a fork chain deeper than followed, a fork record that names no parent) and the session was never retried | the read says whether it is bounded; a bounded restore holds what was read and the turn says so. The end-to-end test reads with a small window and a shallow chain. `a_deferred_server_is_loaded_within_a_turn…` |
| 7 | medium-low: loaded tools were listed in registration order, so a deferred server registered before a plain one could cut the plain one short, and the budget was circular | plain servers are listed first, then the loaded tools: loads only have what the others leave. `a_plain_server_registered_after_a_deferred_one_is_not_cut_short_by_its_loads` |
| 8 | low-medium: a load in a step serialized every MCP call of the step | only calls to tools of deferred servers go in the load's group. `a_load_only_holds_up_the_calls_of_deferred_servers` |
| 9 | low: a name asked for later in a request could still be evicted by an earlier one | everything already loaded that the request names is protected from the start. `a_name_asked_for_later_in_the_request_is_protected_from_an_earlier_one` |
| 10 | low: live and replayed order differ for an approval continuation (validated twice), a call refused by the structural check, and a call beside the load that enables it | recorded, not changed: the differences reorder eviction among tools only in those cases, and the live order is the model's proposal order |
| 11 | low: the sanitizer turned the zero-width joiner and non-joiner (Persian, Indic scripts, emoji) into spaces and missed the tag characters (U+E0000–E007F), the variation selectors beyond the emoji ones and U+206A–206F — text no screen shows and a model reads; `/mcp` counted tools that are never offered | the joiners stay; the tag characters, the extra variation selectors and the deprecated format characters are spaces; `/mcp` says how many tools the provider would refuse. `what_changes_a_line_on_a_screen_is_a_space_like_a_control_character`, `text_no_screen_shows_cannot_carry_an_instruction_in_a_description`, `the_live_view_says_how_many_tools_the_provider_would_refuse` |

The tests the review found too weak were fixed: the lock-order test looks until the call has ended instead of sleeping once; the manifest half of the line-separator test checks the character, not a line count that cannot see it; the tautology in the ceiling test is gone; the plan-mode test already took one outcome.

Recorded, not changed: the surface-lock/registry-lock inversion between `register_mcp_servers` and `refilter_mcp_surface` (older than the deferral; both run on the turn's thread); a probabilistic stress test of a load beside a call.

### Revert cycles

19 mutations (two first read NOT-PROVEN — the group key was tested beside, not through, the dispatcher, and the bounded-restore notice through the concatenated lines — and were rewritten), each failing the test named for it.

### Checks

`rustfmt --check` on the changed files, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --no-fail-fast`: 5220 passed, 0 failed.

## EXT-05-2 — Narrowing a definition can ask for: tool lists, permission rules, a step cap

Restated contract (spec §3.2, AC-05…07): a definition may carry `allow_tools` and `deny_tools` (tool-name patterns), `permission` (rules that deny or ask, never allow) and `max_steps` (1–32), in both syntaxes (the table form under `rapidlm.agent_defs.v2`); the child's tools are `(base role ∩ tool_classes) ∩ allow_tools − deny_tools`, enforced by not offering a tool and by refusing a call to it as `tool_not_allowed_for_agent`; the rules apply to the child's calls and outrank nothing the managed policy says; the cap ends the child after exactly that many model steps with `budget_exhausted` (detail `steps`) and a usable report.

### Phase 0 recheck

| Claim | Finding |
|---|---|
| §2 "`budget_from` hard-codes `MAX_MODEL_STEPS`; the child spec sets no budget" | confirmed; `AgentBudget` gained `max_steps`, `budget_from` reads it (at most the ceiling), the runner sets it from the definition |
| §3.2 "reaches the cap … exactly as a spent token budget does" | **wrong**: a spent token budget makes the child a bare failure (`subagent turn failed`, worktree discarded, nothing of its work reported). The step cap returns a report instead — status `budget_exhausted`, the last twelve calls and what came of each, the blocker `[budget] steps` — and the child is unfinished like one that needed context (headless its changes are dropped, interactive they are held). The token case is unchanged |
| §3.2 "permission rules are applied … using `with_rules`, `with_denied_tools`" | `with_rules` is additive and ordered `deny > ask > allow`; `with_denied_tools` is the managed ban and is not used (an agent file must never be able to ban). Rules go on the child's lattice, a clone of the parent's, so the parent is untouched |
| §3.2 "a pattern that matches no known tool … rejected" vs "only warn in `validate`" | the two disagree in one paragraph; built as the second (a malformed pattern is refused, one that names no tool is a warning with the closest names) |
| §5 "the `agent_spec` in `wire.v1.json` is checked for new fields" | no agent-definition or budget type is in that catalog; `AgentBudget`'s own JSON form gains `max_steps` only when set, so no golden moved. The tool-gateway's `budget_schema` is another tool's (`agent.spawn`) and is not extended |
| §5 "a `rapidlm.agent_defs.v2` fixture under `crates/protocol`" | `crates/protocol/tests/fixtures/agent_defs/` holds the same agent in both syntaxes with the JSON both must come to, and a v1 table; `a_definition_in_either_form_is_the_golden_one` reads them |

### Criteria

| Criterion | Status | Evidence |
|---|---|---|
| AC-05 effective-surface arithmetic; the tool is hidden and a forced call returns `tool_not_allowed_for_agent` | met | `a_child_holds_to_its_classes_then_its_allow_list_less_its_deny_list` (classes, allow, deny, an allow list that cannot gain a tool, an empty allow list), `a_call_outside_what_the_definition_keeps_is_refused_by_name_and_does_nothing` (a forced write, search and command: refused by name, the write did nothing), `binary_a_definitions_narrowing_holds_for_its_child_and_its_step_cap_ends_it_with_a_report` (the child's own request lists only the kept tools; a scripted model calls the one taken away) |
| AC-06 `permission` rules deny/ask for the child's calls and for no one else; managed policy outranks them | met | `a_definitions_rules_only_ever_refuse_or_ask_and_only_for_its_child` (deny, ask, a file the rule does not name still allowed, the parent unchanged, a managed ban outranking), the end-to-end test (a read of one file refused, the secret never reached the model), `every_bad_narrowing_value_is_refused…` (`allow` refused at load) |
| AC-07 `max_steps = N` ends a child after exactly N model steps with `budget_exhausted` / `steps` and a usable report; N > 32 rejected at load | met | `a_step_cap_ends_the_turn_after_exactly_that_many_model_steps` (3, 1, past the ceiling, none), the end-to-end test (three child requests, then the parent, which is told "Stopped at its step limit (3 model steps) … budget_exhausted (steps)" with the child's three calls), `every_bad_narrowing_value_is_refused…` (0, 33, −4, "12", 2.5) |

### Decisions

- A v1 table keeps its closed key set and is told where the new keys live; a Markdown file has the keys because it has no schema line.
- `permission` is stored in pattern order so a definition written in either syntax is the same value; `allow_tools` absent is "no restriction", present and empty is "no tool".
- The grammar of a `permission` key is the settings' rule grammar, asked of the host through the implementation registry (the loader has no permission module), so a key that is not a rule refuses the definition when it loads.
- The step cap is per run: a child continued by a queued message gets the cap again.
- `AgentOutcome`/`ExecOutcome` gain `model_steps` so the runner can tell a step stop from a token or call stop.

### Recorded, not done

- The `tool_not_allowed_for_agent` text is also the class narrowing's refusal now (its old text was the same fact without the id).
- Slice 3 adds `mode` and `when_to_use` to the `/agents` rows; slice 2 shows the caps.

### Revert cycles

26 mutations (25 proved; one, removing the sort of the rules, is equivalent — the table both syntaxes build is already ordered by key — and is kept as a guard against an ordered map; one mutation first did not compile and was rewritten), each failing the test named for it.

### Checks

`rustfmt --check` on the changed files, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --no-fail-fast`: 5236 passed, 0 failed.

## EXT-05-3 — Steering delegation: `mode`, `when_to_use`, `spawn` and the spawn's label

Restated contract (spec §3.3, AC-08): a definition may say where it is used (`mode`: `subagent` by default, `primary` or `both`), add one line of `when_to_use` to its entry in the types `task_spawn` offers, and — if it can be a main agent — list the agents it may delegate to (`spawn`); a non-empty `spawn` on a subagent-only definition is refused at load; a spawn outside the list is refused as `agent_type_not_allowed` naming the allowed ids; the list the model reads keeps its 2 KiB bound and is unchanged for a configuration that uses none of this; the `description` argument of `task_spawn` is shown in the child's row and recorded on its spawn event.

### Phase 0 recheck

| Claim | Finding |
|---|---|
| §3.3 "`description` … accepted and dropped today" | confirmed (`TaskSpawnArgs` read it and nothing used it); it is now folded to one clean line of at most 200 characters (cut, not refused: it was always accepted) |
| §3.3 "`MAX_TYPES_DESCRIPTION_BYTES`, 2 048 bytes" | confirmed; the new text is added entry by entry inside the same bound, the overflow counted as before |
| §3.3 "for a primary agent it filters the `type` values … checked at the spawn site" | no main agent can be selected yet (slice 4), so the filter is `WorkspaceTools::set_spawn_allowlist`, which slice 4's host calls; until then no agent has one and the tool behaves exactly as before |
| §3.3 "a `primary` definition is selectable as a main agent only" | built as: not a `task_spawn` type (left out of the enum and the description; naming it is "no agent type" with the spawnable ids); `AgentMode::is_selectable` is what slice 4 will ask |
| §5 "`agent.spawned` gains `description`" | additive keys on the existing event (`description`, and `definition_digest`, a SHA-256 of the canonical definition, pinned by the golden fixture); the wire catalog has no payload type for it, so no schema moved |

### Criteria

| Criterion | Status | Evidence |
|---|---|---|
| AC-03 (part) non-empty `spawn` on a subagent-only definition refused | met | `where_an_agent_may_be_used_is_a_mode_and_only_a_main_agent_delegates`, the property test (30 000 definitions, both syntaxes) |
| AC-08 `spawn` filters the offered types and refuses others with `agent_type_not_allowed` | met | `a_main_agent_delegates_only_to_its_list_and_a_main_agent_only_type_is_no_type` (offer and accept, the refusal listing what is allowed; a continuation never reaches the spawn check, so it is exempt by construction, not by a test), `the_offered_types_follow_the_mode_the_when_to_use_and_the_spawn_list` |
| AC-08 `when_to_use` and `mode` shape the description within 2 KiB; without the keys it is the old text | met | `without_the_new_keys_the_description_is_what_it_always_was` (byte-for-byte), `long_hints_do_not_widen_the_list_the_model_reads_past_its_bound`, `when_to_use_is_one_line_of_at_most_256_bytes` |
| AC-08 a `subagent`-mode definition never appears as a selectable primary | met | `where_an_agent_may_be_used_is_a_mode_and_only_a_main_agent_delegates` (`is_selectable`/`is_spawnable` for each mode) |
| §3.3 `description` shown and recorded | met | `the_label_of_a_spawn_is_one_clean_line_of_at_most_200_characters`, `a_spawn_record_carries_the_label_and_the_fingerprint_of_the_definition`, `a_spawn_payload_labels_the_row_and_names_the_definition_it_runs`, `a_main_agents_row_says_so` |

### Decisions

- `mode` is a closed three-value key; absent is `subagent`, which is what every definition was. `spawn` ids are checked for shape, count (32) and repeats at load; an id no agent has is a `validate` warning, not a refusal (an agent file can arrive later).
- The digest is over every field a definition can say and none of where it came from, so the same agent in either syntax has one digest; it moved the golden fixture once, deliberately.
- Continuing a finished child (a message to it) is not a spawn and is not held to the list.
- The list in `rapid agents list` shows `mode=` and `spawn=`; `when_to_use` is free text and stays out of the columns.

### Recorded, not done

- Nothing selects a main agent yet, so `spawn` has no effect on a session until slice 4 (it is held at the tool and tested there).

### Revert cycles

23 mutations (mode parse and the `spawn` rule at load, the spawnable/selectable predicates, the `when_to_use` bound, repeated spawn ids, the digest's `spawn` and `mode`, the Markdown conversion of `spawn`, the hint text, a primary-only type in the offer and by name, the allowlist in the offer and at the spawn, the label's cut and control characters, the digest on the record and the label on the row, the list's `mode=`/`spawn=` columns, the validate warning, the panel's `mode=`), each failing the test named for it; none survived.

### Checks

`rustfmt --check` on the changed files, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --no-fail-fast`: 5247 passed and one failed in the full run — `a_detached_child_ending_fires_an_agent_finished_notification`, a race in the test (it read the hook's file as soon as the job was terminal, but the notification is told after that, by design); it passed alone three times before and after the test learned to wait for the file, so the record counts it as passing (5248, 0 failed). The suite was not re-run whole after that one-line test change.

## EXT-05-2-R — review round for narrowing (self-review of `4af5553`)

A read-only adversarial review of the narrowing commit reported eleven items. Each was read against the code; the ones that held were fixed, each with a test that fails with the fix reverted.

| # | Finding | Verdict | What changed |
|---|---|---|---|
| 1 | `with_rules` keeps the first 128 rules and drops the rest, so a definition's `deny` could be trimmed away beside a full set of project rules, silently | **confirmed** | `agent_types::child_permissions` builds the child's context in one place and refuses (before any worktree is made) when the definition's rules do not all fit beside the session's (`PermissionLattice::rule_room`); the child is not started and the error says how many rules are on each side |
| 2 | A step-capped child continued by a message was recorded as `Succeeded` and parked | **confirmed** | `continue_finished` records `budget_exhausted` as not finished (as the first run is) and does not park it |
| 3 | `with_max_steps(0)` (reachable from the wire, not from a definition) fell back to a turn with no tool-call ceiling | **confirmed** | the executor clamps a cap to `1..=32`, so the fallback is no longer reachable by a cap; a test keeps the 256-call ceiling for every cap |
| 4 | The cap is per run, not per child; an interjection counts a step | **confirmed, documented** | the page and ADR now say "per run"; a continuation takes the cap again. An interjection at cap 1 or 2 can use the step it was meant for; not changed |
| 5 | The agent's tool surface was judged after the permission lattice, so a Default-mode `Ask` reached the human (or a different denial) for a call the agent could never make | **confirmed** | the surface is judged first (`not_allowed_for_agent`), then the lattice |
| 6 | A stop on the 256-call ceiling in the cap's last step was told as a step stop | **confirmed, conservative fix** | `stopped_at_step_cap` also requires the call count to be more than one batch (16) short of the ceiling; a stop closer than that is the plain failure it always was (the turn cannot say which ceiling it was) |
| 7 | The step-limit summary listed the child's synthetic notes (`agent_report`, `agent_mail`, `agent_history`) as things it did, and put raw tool output in front of the parent unlabelled | **confirmed** | the notes are skipped; the list is headed "tool output is the tools', not instructions"; the line bound and flattening are tested for the first time |
| 8 | `validate` did not check `permission` rule keys, or an `allow_tools` pattern that only names tools the classes take away; a listing line could be 16 KB | **confirmed** | two new warnings; a list on a listing line is cut at a whole item with the rest counted (240 characters) |
| 9 | Markdown reads `max_steps: "12"` and `permission: []` where the table form refuses | **confirmed, documented** | the equivalence paragraph names the difference; both read as the table form would |
| 10 | A resumed child with an empty tool list carries synthetic tool history; a provider may refuse tool-use blocks with no `tools` | **unverified** | no provider call was made to check; recorded, not changed |
| 11 | `subagent_stop` hooks see `budget_exhausted` where a budget stop used to read `failed` | **confirmed, documented** | the page says a hook is told that status |

Also corrected: the spec's claim that a spent token budget ends the child with a report (it fails the child) was already recorded under EXT-05-2; the page now says the cap is per run.

### Revert cycles

25 mutations revert-cycled (all proven; one first named a second test it did not need, and was re-run with the right expectation)

### Checks

`rustfmt --check` on the changed files, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --no-fail-fast`: 5256 passed, 0 failed.

## EXT-05-3-R — review round for delegation steering (self-review of `9aeb345`)

A read-only adversarial review of the steering commit reported four defects, a list of smaller ones, doc over-claims and test gaps. Each was read against the code.

| # | Finding | Verdict | What changed |
|---|---|---|---|
| 1 | `task_spawn` refused a `description` that was not text (`null`, a number): a regression against "accepted whatever it held", locked in by a test | **confirmed** | a value that is not text is no label; the test now asserts it parses to none |
| 2 | `when_to_use` was checked for control characters only and reached the model raw: line separators, zero-width and direction characters and tag characters (text no screen shows) got through; a whitespace-only hint was accepted. The description had the same hole, older | **confirmed** | `protocol::text` holds the one rule (`changes_the_line`, `one_clean_line`; `mcp_deferred` now delegates to it); `when_to_use` is refused at load if blank or if it has such a character, in both syntaxes; the types the model reads are put through `one_clean_line` for the description and the hint alike, and a hint with nothing visible is no hint |
| 3 | `set_spawn_allowlist(vec![])` refused everything (while `spawn = []` means no list), and a list naming nothing spawnable gave `"enum": []` | **confirmed** | an empty list is no list; a list that leaves no type gives no enum and a description that says so, and every spawn is refused with `allowed: none` |
| 4 | The label was replaced on the `/agents` row by the next state change (queued, admitted, in its worktree), so write-capable children never kept it | **confirmed** | the ledger sink remembers each child's label and puts it in front of the status of those changes; forgotten when the child ends |
| 5a | The fixed `task_spawn` text (built-in types, the `explore` example) and the default type ignored the allowlist | **confirmed** | with a list the text defers to the `type` argument; leaving the type out names the default as the reason; without a list the text is byte-for-byte the old one (a test pins it) |
| 5b | "no agent type" listed types the agent may not use | **confirmed** | it lists the types this agent may use |
| 5c | The digest kept the written order of `allow_tools`, `deny_tools` and `spawn`, so reordering moved it (spurious "digest changed" on resume) | **confirmed** | those lists count once each, in any order; the golden fixture's digest moved once, deliberately |
| 5d | A hint too long for the bound dropped its type, and every type after it | **confirmed** | the hint is dropped before the type is |
| 5e | The "N more" suffix lands after the bound check, so the text can pass 2 KiB by a short line | **confirmed, documented** | the page says "2 KiB, and the short count of what is left out after it"; changing it would change the text of a configuration that uses none of the new keys |
| 5f | The allowlist refusal was a tool failure, unlike its sibling policy refusals | **confirmed** | it is a denial; a type nobody has stays a handled failure |
| 5g | `validate` said nothing of a `spawn` naming a main agent only, or of a `when_to_use` that is never shown | **confirmed** | two warnings |
| 6 | The page did not say nothing selects a main agent yet, still said "Nothing in this slice writes a record", and called `list` "what a spawn would resolve against"; the record cited a continuation test that does not exist | **confirmed** | all four corrected |

Tests added for the gaps the review listed: the sink receives the note in the foreground and detached (with a sink that fails on the note-less call), a refused type in the background, exactly 32 spawn ids and 200 label characters, hidden characters in a hint in both syntaxes, a label that is not text, the label surviving each state change, a reordered or repeated list leaving the digest alone, the enum and the description listing the same ids and the text within the bound, and the new warnings. Not done: a generated-definition property over hidden characters (the targeted tests cover the same classes) and a separate batch-dispatch case for the allowlist (batches reach `execute_task_spawn`, where the check is).

### Revert cycles

33 mutations (35 runs: two first survived, one because duplicates were not in the test's data and one because a tab between words was swallowed rather than folded by the mutant, and were proved after the tests were strengthened), each failing the test named for it.

### Checks

`rustfmt --check` on the changed files, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --no-fail-fast`: 5266 passed, 0 failed.

## EXT-05-3-R2 — second review round (self-review of `70504f4` and `61ee7ed`)

A read-only review of the two fix rounds found two defects, a half-done fix, a leak, a narrow filter, doc drift and test gaps. Each was read against the code.

| # | Finding | Verdict | What changed |
|---|---|---|---|
| 1 | `stopped_at_step_cap` refused legitimate step stops: the call ceiling refuses the call that would pass it and still runs those before it (turn.rs: "an accepted prefix still runs"), so a ceiling stop always has exactly 256 calls made — not "within a batch of it" | **confirmed** | the test is `tool_calls < ceiling`; a stop at exactly the ceiling stays the plain failure (it cannot say which ceiling it was); the page, ADR and tests say so |
| 2 | `step_limit_summary` listed `background_jobs` notices (a finished job's output) as the child's calls | **confirmed** | the synthetic names are one list that includes it; the test has such a notice |
| 3 | Only the `allow_tools`/`deny_tools` filter moved ahead of the permission lattice; the class surface (`tool_classes`) and the docs' claim did not | **confirmed** | the class surface is judged in the same early gate. This matters most for a main agent, whose session has an approval sink: a write it could never make is no longer put to a human |
| 4 | `AgentLabels` remembered a label before the spawn record landed, so a spawn that did not land leaked it | **confirmed** | remembered only once the record landed |
| 5 | `changes_the_line` named "variation selectors" but covered one of the two ranges, and not the soft hyphen, grapheme joiner, annotation marks or the fillers that draw nothing | **confirmed** | every variation selector but the two that choose an emoji's presentation (U+FE0E, U+FE0F: an existing decision, and a test of the MCP text keeps them) and those characters are covered |
| 6 | The page claimed unchanged model-facing text for unchanged configurations, which is false for a description with a newline or hidden character; two doc comments sat on the wrong item | **confirmed** | the claim says "for descriptions that were already one visible line"; the comments moved |
| 7a | `can_resume` did not check that the definition's rules still fit, so the refusal came after the kept child was taken | **confirmed** | checked in `can_resume`, with the child still kept (not separately tested: building the live runner in a unit test is not possible) |
| 7b | A continued child that asked for context, or failed integration, is recorded as succeeded and parked | **recorded, not changed** | only the step stop has parity with a first run; the rest is how a resume has always ended |

Tests added for the gaps: a step stop at 241–255 calls (and at 256), a `background_jobs` notice in the summary, a class-only agent under the Default mode with an approval sink, a label not kept for a spawn that did not land, hidden characters of the second range, and an end-to-end case where the session's 128 rules leave no room for a definition's one (the child is refused with the numbers, nothing is asked of the model).

## EXT-05-4 — Running a session as a chosen main agent

Restated contract (spec §3.4, AC-09): a definition with `mode: primary` or `both` can be the session's main agent, chosen with `rapid exec --agent <id>`, `rapid --agent <id>` or `[agents] default_primary` (flag over config); it narrows the main role — instructions in a labelled section, tools, permission rules, a step cap on each run, model and effort where the managed policy allows — and never widens it; the choice is fixed for the session and recorded as `agent.selected`; a resume re-applies the recorded id from the file as it is now and says so in one line if the digest changed; plan mode still wins; an unknown id, a subagent-only id, an untrusted project's definition and a managed-policy refusal each fail before any model request, naming `rapid agents list`.

### Phase 0 recheck

| Claim | Finding |
|---|---|
| §2 "`parse_exec_args` accepts no agent flag; every top-level turn hard-codes `AgentRole::Coder` at four sites" | confirmed. `rapid exec`, every turn of a TUI session, the turn resumed after an approval and the test seam take the agent; the role stays `Coder` (the agent narrows it). ACP and daemon sessions and workflow steps (which build a context of their own) start with none, and say so in the page |
| §3.4 "recorded as `agent.selected { id, definition_digest }` when the session starts" | built with `mode` (§5 says so); family `agent`; registered in the event table, `wire.v1.json`, the generated SDK, the schema hash the daemon names and the SDK's kind count, and documented in the event catalog. For `exec` it is appended after the turn is submitted (the turn is submitted at the seq the run read, so nothing of the run's own can precede it) and before any model step; for the terminal UI at session creation |
| §3.4 "four typed errors" | five: no such agent, subagent-only, untrusted project, model/policy, and rules that do not fit — plus a bad `default_primary`, a switch on resume and an agent asked of a session that began without one |
| §3.4 "model/effort used when managed policy allows" | the definition's profile is the override of the model plan, so a managed lock still wins; `/model select` wins over the agent's model; the effort is the definition's under the reminders' floor and the policy's |
| §3.4 "plan mode still wins" | plan is the lattice's mode and the agent's rules are added on top; `plan_mode_still_wins_over_an_agent_that_can_write` |
| "resume re-applies it" | read from the session's newest `agent.selected` before the run is set up (it decides the tools, prompt and model); an unreadable record stops the run rather than resuming as the main role |

### Criteria

| Criterion | Status | Evidence |
|---|---|---|
| AC-09 the overlay narrows the main role (surface, instructions, model, cap) | met | `binary_exec_runs_as_the_agent_it_names_and_a_resumed_session_keeps_it` (the request's tools, its system prompt, the delegation enum, the rule's refusal, the model's own server, the one-step cap), `every_turn_of_a_session_that_runs_as_an_agent_is_that_agents`, `an_agents_step_cap_bounds_each_turn_of_its_session`, `a_turn_that_resumes_after_an_approval_is_still_the_sessions_agents`, `an_agents_instructions_follow_the_hosts_prompt_and_its_budget_is_its_step_cap`, `an_agents_model_and_effort_are_used_under_the_floors_that_hold_over_them` |
| AC-09 `agent.selected` is recorded | met | the binary test exports the session and reads it; `a_session_started_as_an_agent_records_it_and_a_resume_keeps_it`; `what_a_session_records_of_its_agent_is_read_back_and_folds_into_its_view` |
| AC-09 resume re-applies it and warns on a digest change | met | both of the above (one line, once; the new fingerprint recorded; nothing recorded when unchanged); `a_choice_is_recorded_when_new_or_changed_and_a_changed_one_is_warned_of` |
| AC-09 plan mode still prevents writes | met | `plan_mode_still_wins_over_an_agent_that_can_write` |
| AC-09 each error case fails before any model request | met | the binary test (no id, a subagent, a bad default, a switch, `--agent` before a command: the model's server saw nothing), `an_untrusted_projects_agent_is_named_as_such_and_never_read`, `the_model_and_effort_an_agent_names_must_be_usable_before_the_run_starts`, `each_refusal_says_what_to_do_and_exits_as_a_usage_or_a_policy_error` |
| AC-08 (last clause) a `subagent`-mode definition is never a selectable primary | met | `an_agent_is_found_by_its_id_and_must_be_one_a_session_can_run_as` |

### Decisions

- The agent is fixed for the session by what it recorded: a resumed session runs as that id (current file), a different `--agent` is refused, and one asked of a session that began without an agent is refused too. The configured default applies to sessions that begin after it was set.
- The record is a precondition: a session that cannot say which agent it is would resume as the unnarrowed main role, so the run stops instead.
- `[agents] default_primary` that is not an agent id is a config error naming the key, not a quiet no agent.
- The TUI says `running as agent '<id>'` when it starts, from the very value the turns read.

### Recorded, not done

- `SessionModel::resolve_as` (the TUI's model and effort for an agent) has no test of its own: it reads the process environment, which a unit test cannot set. The same plan code is tested through `exec` (a second model server) and the effort through `apply_effort`.
- The ACP and daemon surfaces, and workflow steps, are the plain main role.
- A resumed child's `can_resume` check of the definition's rules (EXT-05-3-R2 #7a) is not separately tested.

### Revert cycles

46 mutations (the choice and its precedence, each refusal, the rule-fit check, the overlay heading and bound, the budget, the effort and its floor, the spawn allowlist and the narrowing, the record on a new, a changed and an unchanged resume, the newest-record rule, the warning text, every wiring point of `exec` and of the terminal UI's turns and its continuation, the launch flag and its refusal before a command, the config's default, and the second review round's fixes). Four needed a better test or the right command before they failed: the exec budget, the session's shared agent, and two whose first command did not run the test that covers them; none survived.

### Checks

`rustfmt --check` on the changed files, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --no-fail-fast`: 5288 passed, 0 failed.
