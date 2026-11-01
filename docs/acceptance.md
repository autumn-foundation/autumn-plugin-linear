# Acceptance criteria and evidence

Each row names an AC from `docs/planning.md` and its evidence. Test names
are in `src/<module>/tests.rs` (`module::test`). `intake::` tests are in
`examples/intake-desk/src/tests.rs` and run against Postgres. `triage::`
tests are in `examples/triage_app.rs`. Status on 2026-11-01: 48 of 49 met
with local evidence. AC-49 is met in code. Its Verus run is a CI job,
and that job has not run yet (see "Gaps").

## Config

| AC | Criterion | Evidence |
|---|---|---|
| AC-01 | Keys and defaults; unknown keys fail | `config::defaults_are_the_documented_values`, `the_toml_section_sets_every_field`, `a_missing_section_gives_defaults_and_scopes_take_csv`, `unknown_keys_and_bad_toml_fail` |
| AC-02 | Secret keys in the file fail; the error names the key only | `config::secret_keys_in_the_file_fail_closed` (8 keys; the value never shows) |
| AC-03 | Secrets only from the env lookup; `Debug` redacts | `config::env_overrides_the_file_and_carries_secrets`, `debug_redacts_secrets`, `verify::debug_redacts_the_secrets`, `plugin::debug_shows_keys_but_no_secrets`, `tokens::debug_redacts_both_tokens`, `client::debug_redacts_the_credential`, `graphql::debug_hides_the_webhook_secret_and_variables` |
| AC-04 | Precedence; `env_lookup` replaces the process env | `config::env_overrides_the_file_and_carries_secrets`, `blank_env_values_count_as_unset`, `plugin::configure_closures_win_over_env`, `env_lookup_replaces_the_process_environment`, `env_lookup_is_the_only_source_of_linear_variables` |
| AC-05 | `validate` rules | `config::validate_checks_the_oauth_fields`, `validate_bounds_numbers_and_paths`, `bad_env_numbers_and_flags_fail`, `a_full_oauth_config_validates`, `an_api_key_only_config_validates`, `plugin::handlers_need_the_webhook_secret`, `oauth::dot_segments_cannot_make_an_open_redirect` |
| AC-06 | A bad config stops the startup | `plugin::start_fails_on_a_bad_config`, `a_bad_config_fails_the_startup_hook` (real `TestApp` startup hook), `build_runtime_needs_a_valid_config` |

## Types and policy (verified core)

| AC | Criterion | Evidence |
|---|---|---|
| AC-07 | Anchored id, type, and action rules | `types::organization_id_accepts_uuids_and_refuses_hostile_values`, `event_type_parses_pascal_case_only`, `action_parses_lower_camel_letters_only`, `delivery_id_rules`; property tests `parsed_organization_ids_are_always_safe`, `every_valid_organization_id_parses`, `parsed_delivery_ids_never_hold_separators` |
| AC-08 | Replay window | `policy::replay_window_edges`; property tests `fresh_matches_the_spec` (i128 reference), `fresh_is_symmetric`, `a_wider_window_accepts_more`; Verus `spec_fresh` and 4 lemmas in `verus/policy.rs` |
| AC-09 | Refresh decision | `policy::refresh_edges`, `refresh_matches_the_spec`, `time_only_moves_toward_a_refresh`; Verus `spec_needs_refresh` and 3 lemmas |
| AC-10 | Retry decision | `policy::retry_edges`, `retry_matches_the_spec`, `a_retry_loop_always_stops`; Verus `spec_may_retry` and 3 lemmas |
| AC-11 | Claim state machine | `policy::claim_state_machine_walk`, property test `claim_invariants_hold` (random operation runs); Verus `spec_claim`/`spec_complete`/`spec_release` and 6 lemmas; the in-memory store uses these functions (`delivery::*`) |

## Webhook verification

| AC | Criterion | Evidence |
|---|---|---|
| AC-12 | Hex HMAC on raw bytes, constant time, previous secret | `verify::sign_matches_a_reference_vector` (Python `hmac` reference), `a_valid_signature_verifies`, `the_previous_secret_verifies_during_rotation`, property tests `signed_bodies_always_verify`, `a_flipped_bit_never_verifies`; constant time: `Mac::verify_slice` in `verify::verify_signature`; `webhooks::the_previous_secret_verifies_during_rotation` |
| AC-13 | Every signature failure is 401, before parsing | `webhooks::signature_failures_are_401_before_any_parsing` (missing, wrong secret, changed byte, garbage body), `verify::bad_signatures_fail_with_signature_errors` |
| AC-14 | Stale, future, or missing `webhookTimestamp` is 401 | `webhooks::stale_future_and_missing_times_are_401` (edges at ±60 000 ms pass), `the_window_follows_the_config`, `event::a_missing_or_bad_timestamp_is_a_replay_error` |
| AC-15 | Over 1 MiB is 413 | `webhooks::an_oversized_body_is_413` |
| AC-16 | Missing delivery id or bad envelope is 400; routing uses the signed type | `webhooks::a_missing_delivery_id_or_a_bad_envelope_is_400`, `routing_uses_the_signed_type_not_the_header`, `event::a_bad_envelope_is_an_invalid_request` |

## Webhook dispatch

| AC | Criterion | Evidence |
|---|---|---|
| AC-17 | `(type, action)` handler, else `type` handler; 200 | `webhooks::a_valid_delivery_runs_the_exact_handler_with_full_context`, `the_type_handler_takes_other_actions` |
| AC-18 | Handler error is non-2xx and releases the claim | `webhooks::a_failed_handler_is_500_and_its_retry_runs_again` |
| AC-19 | Done duplicate is 200 and runs nothing; in flight is 409 | `webhooks::a_done_duplicate_is_200_and_runs_nothing`, `an_in_flight_duplicate_is_409`, `delivery::a_new_id_is_fresh_then_in_flight_then_done` |
| AC-20 | No handler is 200 | `webhooks::an_event_with_no_handler_is_200_and_runs_nothing` |
| AC-21 | Bad or repeated key stops the startup | `webhooks::the_registry_refuses_bad_and_repeated_keys`, `plugin::build_runtime_refuses_bad_and_repeated_keys` |
| AC-22 | Revoke deletes the token (not a newer install), then runs the handler | `webhooks::a_revoke_deletes_the_token_then_runs_the_handler`, `the_revoke_handler_sees_no_token` (order), `a_revoke_keeps_a_token_from_a_later_install`, `a_revoke_for_another_client_keeps_the_token`, `a_revoke_without_a_time_keeps_a_working_token`, `a_revoke_without_a_time_deletes_a_dead_token` |
| AC-23 | Own task; a dropped request does not stop it; a panic is 500 and releases | `webhooks::a_dropped_request_does_not_stop_the_handler` (the caller is aborted mid-handler; `Notify`, no sleeps), `a_panic_is_500_and_releases_the_claim`, `a_store_failure_is_500_and_runs_nothing` |
| AC-24 | `WebhookContext` contents and helpers | `webhooks::a_valid_delivery_runs_the_exact_handler_with_full_context`, `a_context_built_in_a_test_reaches_the_client`, `event::parse_reads_every_envelope_field`, `issue_view_reads_the_data_and_ignores_new_fields`, `comment_view_reads_the_data`, `updated_from_helpers_find_changes_and_label_moves`, `a_create_with_the_label_counts_as_added`, `entity_key_uses_signed_fields_only` |
| AC-25 | `VerifiedWebhook` and `process` on a custom route | `webhooks::the_extractor_and_process_work_on_a_custom_route` (Handled, Duplicate, Unhandled, 401) |
| AC-26 | Agent session view | `event::agent_session_view_reads_created_events`, `agent_session_view_reads_prompts_and_the_stop_signal` (`content.body` and `body`) |

## GraphQL and client

| AC | Criterion | Evidence |
|---|---|---|
| AC-27 | Transport-free operations; the parser reads data, errors, rate limits | `graphql::operation_bodies_name_their_inputs`, `data_comes_back_on_success`, `errors_win_over_partial_data`, `rate_limits_are_found_by_status_and_by_code`, `a_rate_limit_code_next_to_data_is_not_retried`, `the_parser_never_panics` (property test); the same operations go through camel and San Mateo in `intake::one_issue_gives_one_workflow_and_one_receipt` |
| AC-28 | Endpoint; API key raw, OAuth as Bearer | `client::an_api_key_goes_raw_and_an_oauth_token_as_bearer`, `the_default_endpoint_is_linear` |
| AC-29 | GraphQL and HTTP errors | `graphql::errors_win_over_partial_data`, `a_400_with_graphql_errors_gives_the_messages`, `http_errors_keep_the_status_and_truncate_the_body`, `bad_success_bodies_are_unexpected` |
| AC-30 | Only rate limits retry, with a capped wait | `client::a_rate_limit_retries_after_the_wait_then_succeeds`, `retries_run_out_and_keep_the_wait`, `a_wait_longer_than_the_cap_stops_at_once`, `zero_retries_fail_on_the_first_limit`, `server_errors_and_graphql_errors_do_not_retry`, `a_missing_retry_after_uses_the_default_wait_against_the_cap`, `the_reset_header_sets_the_wait_when_retry_after_is_missing` |
| AC-31 | Typed helpers | `client::typed_helpers_send_their_operations` (all 13 operation names in order), `create_comment_once_finds_an_earlier_comment`, `graphql::connections_report_the_next_cursor`, `extract_checks_mutation_success_and_reads_the_payload_field`, `constructors_for_app_code`; every document checked against the Linear SDK schema by the conformance review |
| AC-32 | No token in `Debug` or errors; no redirects | `client::debug_redacts_the_credential`, `a_redirect_is_not_followed`, `a_transport_failure_is_an_http_error_without_the_token`, `health::a_failing_key_reports_down_without_the_key` |

## OAuth

| AC | Criterion | Evidence |
|---|---|---|
| AC-33 | Authorize URL and state cookie | `oauth::install_redirects_to_linear_with_every_parameter_and_a_cookie`, `the_nonce_comes_from_the_app_entropy`, `pkce_values_follow_rfc_7636` (RFC 7636 appendix B vector), `authorize_url_leaves_out_prompt_when_consent_is_off` |
| AC-34 | Callback checks, exchange, store, handler, redirect; 502 with no secret | `oauth::the_callback_exchanges_the_code_stores_the_token_and_runs_the_handler` (form has `code_verifier`), `the_callback_checks_run_before_any_token_call`, `an_exchange_failure_is_502_without_secrets`, `a_bad_organization_or_token_answer_is_502`, `a_failed_install_handler_is_500_and_the_token_stays`, `an_array_scope_from_an_old_app_is_accepted`, `the_install_handler_gets_the_request_headers`, `cookie_value_refuses_a_repeated_cookie`, `dot_segments_cannot_make_an_open_redirect` |
| AC-35 | Refresh rules | `plugin::a_token_near_expiry_refreshes_and_stores_the_rotated_pair`, `a_fresh_token_makes_no_refresh`, `concurrent_callers_share_one_refresh` (8 callers, 1 call), `a_cancelled_caller_does_not_lose_the_rotated_pair`, `a_refused_refresh_deletes_the_pair_and_asks_for_a_reinstall`, `a_refused_refresh_uses_and_keeps_a_pair_another_replica_stored`, `a_refused_client_secret_keeps_the_tokens`, `a_transient_failure_keeps_a_valid_old_token_and_backs_off`, `an_expired_token_backs_off_after_a_failed_refresh`, `an_expired_token_with_a_transient_failure_fails`, `an_expired_token_without_a_refresh_token_needs_a_reinstall`, `a_refresh_without_a_new_refresh_token_keeps_the_old_one`, `a_refresh_answer_without_scope_or_expiry_keeps_safe_values` |
| AC-36 | OAuth routes are 404 when off | `oauth::oauth_routes_are_404_when_oauth_is_off` |
| AC-37 | `TokenStore` trait, replaceable | `tokens::the_in_memory_store_puts_gets_and_deletes`, `delete_if_removes_only_the_seen_token`; custom stores in `plugin::a_refused_refresh_uses_and_keeps_a_pair_another_replica_stored` |

## Plugin

| AC | Criterion | Evidence |
|---|---|---|
| AC-38 | Section, mount, contract, conformance, public routes | `plugin::the_plugin_passes_the_conformance_harness`, `the_plugin_declares_the_linear_section_and_public_routes` (`RouteClassification::Public` on all 3), `the_plugin_name_and_contract_are_stable`, `supported_autumn_web_matches_the_manifest` |
| AC-39 | `Linear` extractor | `plugin::the_extractor_is_503_without_the_runtime`, `the_api_key_client_serves_every_workspace_when_oauth_is_off`, `client_needs_an_api_key`, `an_uninstalled_workspace_is_not_installed`, `install_context_new_carries_the_handle` |
| AC-40 | Health-only indicator in API-key mode | `health::a_working_key_reports_up_with_the_viewer_name`, `a_failing_key_reports_down_without_the_key`, `plugin::the_indicator_is_in_health_and_not_in_ready`, `start_installs_the_runtime_and_the_health_indicator`, `start_skips_health_without_an_api_key` |
| AC-41 | Startup warnings | `plugin::warnings_name_the_csrf_exemption_and_the_unused_api_key` |
| AC-42 | Test kit | `testing::a_fixture_signs_what_the_verifier_accepts`; every webhook test and both examples use `EventFixture` |

## Examples

| AC | Criterion | Evidence |
|---|---|---|
| AC-43 | `triage_app` builds; a test runs its handler over HTTP | `triage::a_new_issue_gets_a_priority_and_one_comment` (TestApp + local axum stub), `triage::priorities_follow_the_title` |
| AC-44 | `intake-desk` runs the full chain, checks itself, has failure tests, CI job | `intake::one_issue_gives_one_workflow_and_one_receipt`; `cargo run` prints the checked summary (401, 200 ×4; 4 Linear operations; `auth_added_by_san_mateo: true`; the sent ids and priority). Failure tests: `a_graphql_error_fails_the_workflow_with_no_retry`, `a_rate_limit_is_retried_by_harvest`, `an_http_429_is_retried_by_harvest`, `an_existing_receipt_is_not_posted_again`, `a_refused_create_finds_the_receipt_an_earlier_attempt_made`, `a_wrong_summary_fails_the_check`, `events_without_the_intake_label_are_skipped` (also another workspace). CI: `.github/workflows/ci.yml` job `intake-desk` with Postgres 16 |
| AC-45 | Repeated deliveries start one workflow and post one comment | `intake::one_issue_gives_one_workflow_and_one_receipt`: the same event three times (a new delivery id, then the same id). One workflow, one `AutumnCommentCreate`, with the stable id |

## Quality

| AC | Criterion | Evidence |
|---|---|---|
| AC-46 | fmt, clippy pedantic + nursery `-D warnings`, MSRV 1.88 | Local runs of `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings` (stable 1.97 and 1.88), the same for `examples/intake-desk`; CI jobs `check`, `msrv`, `intake-desk` |
| AC-47 | Line coverage ≥ 85 % | `cargo llvm-cov --summary-only`: 96.82 % lines, 95.74 % regions, 94.81 % functions; CI job `coverage` (`--fail-under-lines 85`) |
| AC-48 | Docs in ASD-STE100; `missing_docs` clean | README, CLAUDE.md, CHANGELOG, `docs/planning.md`, 4 ADRs, `docs/tdd-log.md`; `cargo doc --no-deps` gives no warnings; `missing_docs = "warn"` with `-D warnings` in clippy |
| AC-49 | Verus spec, drift test, CI job | `verus/policy.rs` (6 exec functions with `ensures`, 16 lemmas); `policy::verus_bodies_match` (bodies and the 6 constants); CI job `verus` (pinned Verus build, z3 4.16) |

## Review findings and how each was closed

Five reviews ran: security, correctness and concurrency, Linear API
conformance, API and docs and tests, and the ecosystem example. The
example review used two lenses, with two skeptics per finding. Each fix
has a test written first (batch 5 in `docs/tdd-log.md`).

| Finding | Fix | Test |
|---|---|---|
| A retried revoke could delete the token of a later reinstall | Signed `createdAt` (stable on retries); no time: probe the token; other `oauthClientId`: no change | `webhooks::a_revoke_*` (5 tests) |
| Expired token + failing refresh: one slow call per waiting caller | `RefreshBackoff` (503) for 10 s | `plugin::an_expired_token_backs_off_after_a_failed_refresh` |
| `invalid_client` deleted every workspace token | Config error; tokens stay; only `invalid_grant` is terminal | `plugin::a_refused_client_secret_keeps_the_tokens` |
| Cross-replica check-then-delete | `TokenStore::delete_if`, atomic in memory, documented for shared stores | `tokens::delete_if_removes_only_the_seen_token` |
| The full store or a short window dropped a running claim | Evict only done or expired claims; prune stops at a running claim; full and all running gives 409; lease ≥ 1 ms | `delivery::a_full_store_never_drops_a_running_claim`, `a_short_window_never_drops_a_running_claim`, `an_expired_lease_makes_room_in_a_full_store`, `a_zero_lease_is_clamped` |
| `RATELIMITED` next to data could re-run a mutation | Rate limit only with no data | `graphql::a_rate_limit_code_next_to_data_is_not_retried` |
| Refresh answer could wipe the scope and the expiry | Keep the old scope; no `expires_in` uses Linear's 24 h | `plugin::a_refresh_answer_without_scope_or_expiry_keeps_safe_values` |
| Open redirect through dot segments | Refuse `.`/`..` segments; the final path must stay local | `oauth::dot_segments_cannot_make_an_open_redirect` |
| Webhook secret in `Debug` of `WebhookCreate` and `Operation` | Manual `Debug` | `graphql::debug_hides_the_webhook_secret_and_variables` |
| The install was not bound to the app user; the org id in the redirect is unsigned | `InstallContext::headers`; README tells apps to bind there, not on the redirect page | `oauth::the_install_handler_gets_the_request_headers` |
| One global secret for per-workspace webhooks | README: a secret per workspace for API-made webhooks | docs |
| Inputs and contexts hard to build from app code | Input structs constructible; `Organization::new`, `IssueCreate::with_stable_id`, `create_comment_once`, `create_agent_activity_with_id`; wider re-exports; test recipe in README | `graphql::constructors_for_app_code`, `client::create_comment_once_finds_an_earlier_comment` |
| `Retry-After` missing: fixed 1 s wait | Wait from `X-RateLimit-Requests-Reset` | `client::the_reset_header_sets_the_wait_when_retry_after_is_missing` |
| `first: u32` beyond GraphQL `Int` | Clamp to `i32::MAX` | `graphql::constructors_for_app_code` |
| Weak tests for AC-04/06/22/23/31/35/38/40 | Stronger tests (see the AC rows) | see above |
| Docs out of step; STE violations; rustdoc links | Fixed; `thiserror::Error as ThisError` | `cargo doc` clean |
| Example: 3 fast attempts lost the issue on a rate limit | 8 attempts, exponential from 500 ms; the module doc gives the failure model and the recovery path | `intake::an_http_429_is_retried_by_harvest`, `a_rate_limit_is_retried_by_harvest` |
| Example: events of any workspace used one connection | The handler skips other workspaces; the workflow id and the receipt key carry the org | `intake::events_without_the_intake_label_are_skipped` |
| Example: a refused create after a lost reply failed the run | Find again after a failed create | `intake::a_refused_create_finds_the_receipt_an_earlier_attempt_made` |
| Example: `check()` did not inspect request variables | The summary has the sent ids, issue, and priority | `intake::one_issue_gives_one_workflow_and_one_receipt` |
| Example: shutdown order and timeouts | Stop the worker before a join error; workflow wait 60 s inside the 120 s run; `ackTimeout=4000` | `intake::*` |

## Gaps

- **AC-49, Verus run.** The proofs have not run on this machine: the
  build of Verus from source was not allowed here. The CI job `verus`
  builds the same pinned Verus as san-mateo and runs `verus
  verus/policy.rs`. The drift test and the property tests run locally.
  If the CI job finds a syntax or proof issue, fix it in a follow-up.
- **Not in the example, by choice.** Harvest drops `Retry-After` between
  camel and the activity. The connector dead-letter sink needs the
  harvest plugin migrations. A FAILED run needs a reconciler or the
  harvest reset path. The module doc of `intake-desk` states all three.
- **Out of scope** (see `docs/planning.md`, "Not in this release"):
  client-credentials tokens, webhook source IP checks, and agent plan
  updates.
