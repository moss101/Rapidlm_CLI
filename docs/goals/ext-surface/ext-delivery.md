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
