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
