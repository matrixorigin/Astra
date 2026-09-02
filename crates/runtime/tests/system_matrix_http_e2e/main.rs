//! System HTTP end-to-end: real Axum app + MatrixOne + full `build_server_state` wiring.
//!
//! ## Tests in this binary (ignored tests)
//! - **`product_matrix_api_journey_hits_multiple_tables`** — full product journey (sessions → agents →
//!   events → server-owned `chat/stream` SSE + `agent_events` assertions → logout), including
//!   `GET /platform/snapshot` after session activity.
//! - **`e2e_matrix_chat_run_pause_resume_http`** — `POST /chat` (background run), immediate
//!   pause/resume + `GET /chat/runs/{run_id}` (run state is in-memory + optional engine; no Matrix
//!   table assertion today).
//! - **`e2e_matrix_session_cancel_delete`** — `POST .../cancel` + DB `cancelled`, then `DELETE` + 404.
//! - **`e2e_matrix_chat_stream_session_info`** — `POST /chat/stream` buffered SSE; first `session_info`
//!   event contains `run_id`.
//! - **`e2e_matrix_approval_respond_invalid_session_id`** — `POST /approval/respond` rejects unsafe
//!   `session_id` values with `400` instead of writing durable approval data for an invalid path.
//! - **`e2e_matrix_edge_callback_http_boundary_failures`** — callback routes reject unauthenticated and
//!   malformed `/tools/result` / `/approval/respond` requests with client/auth errors instead of
//!   accepting transport-boundary garbage.
//! - **`e2e_matrix_duplicate_tool_result_idempotency`** — `POST /tools/result` twice for the same
//!   `request_id` during one server-owned stream; assert one request, one durable result, and one
//!   final model round/terminal.
//! - **`e2e_matrix_duplicate_approval_response_idempotency`** — `POST /approval/respond` twice for the
//!   same `request_id`; assert the run event log records one terminal decision.
//! - **`e2e_matrix_server_stream_partial_batch_failure`** — one server-owned stream emits two
//!   `tool_request` callbacks; post one success and one failure and assert the same stream reaches
//!   its final model round and one server terminal.
//! - **`e2e_matrix_server_stream_out_of_order_tool_results`** — one server-owned stream emits two
//!   callbacks, accepts the second result before the first, and still reaches one final terminal.
//! - **`e2e_matrix_auth_session_negative_paths`** — `GET /sessions` without auth (401), duplicate
//!   register, bad login, and successful login after negative calls (replaces stub `auth_contract` /
//!   `session_contract` negative coverage).
//! - **`e2e_matrix_memory_proxy_user_isolation`** — unauthenticated memory returns 401; spoofed
//!   `user_id` / `session_id` in body are overwritten to JWT user on forward (replaces `memory_contract`
//!   isolation tests).
//! - **`e2e_matrix_models_admin_crud`** — grant `astra_admin`, `POST/PUT/DELETE /models` with
//!   `provider: mock` + `infra_llm_models` SQL checks (replaces `model_crud_contract`).
//! - **`e2e_matrix_stream_session_and_run_status`** — `POST /chat/stream` with mock LLM → verify
//!   `agent_sessions` row persisted, run transitions to `completed`, `events_count > 0`.
//! - **`e2e_matrix_stream_context_trace_persistence`** — `POST /chat/stream` → verify
//!   `context_trace_signal` event written to `agent_events` with valid causal chain.
//! - **`e2e_matrix_stream_multi_turn_persistence`** — two sequential `POST /chat/stream` to same
//!   session → verify event counts increment, distinct causal chains, both runs completed.
//! - **`e2e_matrix_stream_structured_fanout_has_one_parent_synthesis_and_durable_tree`** — real
//!   `/chat/stream` + mock parent/children + MatrixOne: three durable child runs settle before one
//!   parent synthesis, with no detached reconciliation or orphan transcript run identity.
//! - **`e2e_matrix_stream_failed_fanout_settles_once_without_orphaning_children`** — the same live
//!   path with all child providers failing: causes stay inspectable, no replacement runs appear,
//!   and the parent receives one fixed-size terminal aggregate.
//! - **`e2e_matrix_stream_concurrent_fanout_isolates_users_sessions_and_group_ids`** — four live
//!   fanouts reuse the same group/slot labels across two users and two sessions each; successful and
//!   provider-deadline groups remain stream-, registry-, and MatrixOne-isolated under concurrency.
//! - **`e2e_matrix_stream_root_cancel_settles_slow_fanout_without_late_synthesis`** — cancelling a
//!   live root while three child providers are delayed yields one cancellation terminal per fixed
//!   slot, a four-row cancelled tree, and no late child output or parent synthesis.
//! - **`e2e_matrix_team_crud_and_db`** — `POST/GET/DELETE /teams`, list + detail + empty executions,
//!   upsert second `POST`, `team_definitions` SQL assertions (`user_id`, `name`, delete removes row).
//! - **`e2e_matrix_team_snapshots_and_db`** — `POST/GET .../snapshots`, `DELETE /teams/snapshots/{id}`,
//!   `team_snapshots` SQL; cleans up team row after.
//! - **`e2e_matrix_team_http_negative_paths`** — `GET /teams` without auth (401), unknown team GET/DELETE
//!   (404), `POST /teams` empty members + duplicate roles → 400 (`validate_team`), invalid budget +
//!   adversarial member-count validation.
//! - **`e2e_matrix_team_http_db_fidelity`** — HTTP `GET /teams/{name}` matches `team_definitions`
//!   JSON columns (`coordination`, `members_json`, `context_json`, `budget_json`, …); `GET /teams` count =
//!   SQL `COUNT(*)`; snapshot `team_definition_json` + list fields vs DB; `GET .../executions?limit=`.
//! - **`e2e_matrix_team_cross_user_isolation`** — second registered user cannot GET/DELETE another user's
//!   team (404); team name absent from foreigner's list.
//! - **`e2e_matrix_meta_health`** — `GET /` + `GET /health` (service metadata, DB connected, persist counters).
//! - **`e2e_matrix_session_http_db`** — `GET`/`PUT /sessions/{id}` vs `agent_sessions` (`title`, `user_id`).
//! - **`e2e_matrix_session_artifact_http_db`** — authenticated session artifact list/get routes align with
//!   `session_artifacts`, including session scoping and cross-user isolation.
//! - **`e2e_matrix_published_artifact_http_round_trip`** — a real runtime publish path (`/chat/stream`
//!   success → `llm_capture` artifact) lands in `session_artifacts` and is readable back through the
//!   authenticated session artifact HTTP routes.
//! - **`e2e_matrix_session_artifact_latest_and_download`** — authenticated latest/download session
//!   artifact routes expose a direct latest-by-kind read plus attachment download for published
//!   `llm_capture` artifacts.
//! - **`e2e_matrix_session_artifact_latest_tiebreaker`** — authenticated latest route stays
//!   deterministic when multiple artifacts share the same `created_at`, returning the newest
//!   artifact consistently instead of relying on database tie behavior.
//! - **`e2e_matrix_evaluation_reads`** — evaluation GETs (`x-user-id`), optional agent seed for trust/SLO/
//!   observability, plus memory health/metrics.
//! - **`e2e_matrix_context_decision_chain`** — `POST /events` → `/context` → `/decisions` + `ctx_snapshots` /
//!   `ctx_decision_audits` SQL.
//! - **`e2e_matrix_models`** — `GET /models` and model-access projections.
//! - **`e2e_matrix_branches_cost_estimate_http`** — `POST /branches/cost-estimate` (JWT, numeric
//!   estimate fields); 401 without auth (no DDL branch ops).
//! - **`e2e_matrix_delegate_http_boundaries`** — `POST /chat` → `run_id`; `GET .../delegations`;
//!   `POST .../delegate` fails at typed validation (`422`) without executing sub-runs.
//! - **`e2e_matrix_admin_control_plane_rbac`** — global token and model-gateway control-plane
//!   routes reject normal users, then work after `grant_astra_admin_role`.
//!
//! Session list/get/put, close/resume, activity, and DB checks for close/resume live only in the full
//! journey (not duplicated in a separate test).
//!
//! External dependencies remain mocked where the product already allows it:
//! - LLM: `context.test_llm_rounds` + the legacy `e2e-hooks` mock-inference hook on
//!   server-owned `/chat/stream` (no external model server).
//!   `ASTRA_TEST_E2E_SECRET` is test-hook configuration, not route authorization.
//! - Memoria: [`astra_runtime::MemoriaForwarder`] stub (memory proxy routes only).
//!
//! ## How to run
//! ```text
//! ASTRA_TEST_DB_IT=1 \
//! ASTRA_TEST_E2E_SECRET=system-matrix-e2e-secret \
//! ASTRA_BACKEND_SERVICE_KEY=test-service-key-e2e \
//! ASTRA_LLM_RETRY_BASE_MS=10 ASTRA_DEFAULT_RETRY_AFTER_MS=10 ASTRA_BCRYPT_COST=4 \
//! RUST_MIN_STACK=16777216 \
//! cargo test -p astra-runtime --test system_matrix_http_e2e --features e2e-hooks -- \
//!   --ignored --nocapture
//! ```
//!
//! Requires the same env as `astra-server` startup: `MATRIXONE_*`, `ASTRA_JWT_SECRET`, etc.
//! from [`astra_core::AppSettings::from_env`]. Load `.env` if you use one for development.
//!
//! See `docs/testing/system-e2e-matrix.md` for the capability ↔ route ↔ test mapping, DB isolation, and
//! parallelism (`make test` / `ASTRA_TEST_DB_IT_TEST_THREADS`).

mod harness;
mod journey_admin_smoke_matrix;
mod journey_branches_matrix;
mod journey_context_decision_chain_matrix;
mod journey_delegate_http_matrix;
mod journey_evaluation_reads_matrix;
mod journey_extended;
mod journey_full;
mod journey_full_capture_matrix;
mod journey_meta_matrix;
mod journey_models_matrix;
mod journey_phase0_production_baseline;
mod journey_phase0_production_topologies;
mod journey_saas_negative_matrix;
mod journey_saas_platform_matrix;
mod journey_session_artifacts_matrix;
mod journey_session_http_db_matrix;
mod journey_stream_persistence;
mod journey_tasks_runs;
mod journey_team_crud_matrix;
mod journey_team_data_fidelity_matrix;
mod journey_team_http_negatives_matrix;
mod journey_team_isolation_matrix;
mod journey_team_snapshots_matrix;

use harness::require_system_e2e_env;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn product_matrix_api_journey_hits_multiple_tables() {
    require_system_e2e_env();
    let b = harness::bootstrap().await;
    journey_full::run_product_matrix_full_journey(&b.ctx, &b.auth_header, &b.refresh_token).await;
    b.ctx.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_chat_run_pause_resume_http() {
    require_system_e2e_env();
    journey_tasks_runs::run_chat_run_pause_resume_http().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live MatrixOne; production orphan claim x HTTP DELETE cancellation race"]
async fn e2e_matrix_orphan_cancel_claim_race_http() {
    require_system_e2e_env();
    journey_tasks_runs::run_orphan_cancel_claim_race_http().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + mock LLM; paused accounting generation fence"]
async fn e2e_matrix_paused_accounting_generation_fence_http() {
    require_system_e2e_env();
    journey_tasks_runs::run_paused_accounting_generation_fence_http().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live MatrixOne + mock LLM; pause wins the post-loop settlement race"]
async fn e2e_matrix_live_pause_wins_post_loop_settlement_accounting() {
    require_system_e2e_env();
    journey_tasks_runs::run_live_pause_wins_post_loop_settlement_accounting().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_session_cancel_delete() {
    require_system_e2e_env();
    journey_extended::run_session_cancel_then_delete().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_chat_stream_session_info() {
    require_system_e2e_env();
    journey_extended::run_chat_stream_session_info_smoke().await;
}

// This journey asserts local journal isolation while the stream handler and
// turn flush run across worker threads; the journey installs a process-scoped
// journal root for that single nextest process.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_stream_session_metadata_enables_full_llm_exchange_journaling() {
    require_system_e2e_env();
    journey_full_capture_matrix::run_stream_session_metadata_enables_full_llm_exchange_journaling()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live MatrixOne + mock LLM; overlapping fixture cleanup isolation gate"]
async fn e2e_matrix_stream_bootstrap_cleanup_preserves_live_fixture() {
    require_system_e2e_env();
    journey_stream_persistence::run_stream_bootstrap_cleanup_preserves_live_fixture().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_approval_respond_invalid_session_id() {
    require_system_e2e_env();
    journey_extended::run_approval_respond_invalid_session_id_rejected().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_edge_callback_http_boundary_failures() {
    require_system_e2e_env();
    journey_extended::run_edge_callback_http_boundary_failures().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_duplicate_tool_result_idempotency() {
    require_system_e2e_env();
    journey_extended::run_duplicate_tool_result_server_stream_is_idempotent().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_duplicate_approval_response_idempotency() {
    require_system_e2e_env();
    journey_extended::run_duplicate_approval_response_is_idempotent().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_server_stream_partial_batch_failure() {
    require_system_e2e_env();
    journey_extended::run_server_stream_partial_batch_failure().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_server_stream_out_of_order_tool_results() {
    require_system_e2e_env();
    journey_extended::run_server_stream_out_of_order_tool_results().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_auth_session_negative_paths() {
    require_system_e2e_env();
    journey_extended::run_auth_and_session_negative_paths().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_memory_proxy_user_isolation() {
    require_system_e2e_env();
    journey_extended::run_memory_proxy_user_isolation().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_models_admin_crud() {
    require_system_e2e_env();
    journey_extended::run_models_admin_crud_with_db().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_stream_session_and_run_status() {
    require_system_e2e_env();
    journey_stream_persistence::run_stream_session_and_run_status().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_stream_context_trace_persistence() {
    require_system_e2e_env();
    journey_stream_persistence::run_stream_context_trace_persistence().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_stream_multi_turn_persistence() {
    require_system_e2e_env();
    journey_stream_persistence::run_stream_multi_turn_persistence().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + three real provider Offerings; dedicated Phase-0 ServerOnly baseline"]
#[serial_test::serial(phase0_production_baseline)]
async fn e2e_matrix_phase0_server_only_production_baseline() {
    require_system_e2e_env();
    journey_phase0_production_baseline::run_server_only_production_baseline().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live MatrixOne + real provider + explicit production binary paths; complete Phase-0 topology baseline"]
#[serial_test::serial(phase0_production_baseline)]
async fn e2e_matrix_phase0_external_production_topologies() {
    require_system_e2e_env();
    journey_phase0_production_topologies::run_external_production_topologies().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live MatrixOne + real provider + explicit production binary paths; exact Edge+Server × 1M diagnostic"]
#[serial_test::serial(phase0_production_baseline)]
async fn e2e_matrix_phase0_external_edge_server_m1() {
    require_system_e2e_env();
    journey_phase0_production_topologies::run_external_edge_server_m1().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live MatrixOne + mock parent/child LLM; structured fan-in online gate"]
async fn e2e_matrix_stream_structured_fanout_has_one_parent_synthesis_and_durable_tree() {
    require_system_e2e_env();
    journey_stream_persistence::run_stream_structured_fanout_has_one_parent_synthesis_and_durable_tree()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "live MatrixOne + mock parent/child LLM; concurrent fanout ownership/isolation gate"]
async fn e2e_matrix_stream_concurrent_fanout_isolates_users_sessions_and_group_ids() {
    require_system_e2e_env();
    journey_stream_persistence::run_stream_concurrent_fanout_isolates_users_sessions_and_group_ids(
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
#[ignore = "live MatrixOne + delayed mock child LLM; root/fanout cancellation race gate"]
async fn e2e_matrix_stream_root_cancel_settles_slow_fanout_without_late_synthesis() {
    require_system_e2e_env();
    journey_stream_persistence::run_stream_root_cancel_settles_slow_fanout_without_late_synthesis()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live MatrixOne + mock parent/child LLM; canonical Work scheduler online gate"]
async fn e2e_matrix_stream_canonical_work_scheduler_prevents_decorative_plan() {
    require_system_e2e_env();
    journey_stream_persistence::run_stream_canonical_work_scheduler_prevents_decorative_plan()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live MatrixOne + mock LLM; deferred canonical Work online gate"]
async fn e2e_matrix_stream_deferred_work_does_not_start_an_attempt() {
    require_system_e2e_env();
    journey_stream_persistence::run_stream_deferred_work_does_not_start_an_attempt().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live MatrixOne + failing mock child LLM; structured fan-in unhappy-path gate"]
async fn e2e_matrix_stream_failed_fanout_settles_once_without_orphaning_children() {
    require_system_e2e_env();
    journey_stream_persistence::run_stream_failed_fanout_settles_once_without_orphaning_children()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_team_crud_and_db() {
    require_system_e2e_env();
    journey_team_crud_matrix::run_team_crud_db().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_team_snapshots_and_db() {
    require_system_e2e_env();
    journey_team_snapshots_matrix::run_team_snapshots_db().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_team_http_negative_paths() {
    require_system_e2e_env();
    journey_team_http_negatives_matrix::run_team_http_negative_paths().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_team_http_db_fidelity() {
    require_system_e2e_env();
    journey_team_data_fidelity_matrix::run_team_http_db_fidelity().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_team_cross_user_isolation() {
    require_system_e2e_env();
    journey_team_isolation_matrix::run_team_cross_user_isolation().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_meta_health() {
    require_system_e2e_env();
    journey_meta_matrix::run_meta_root_and_health().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_session_http_db() {
    require_system_e2e_env();
    journey_session_http_db_matrix::run_session_http_matches_agent_sessions_row().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_session_artifact_http_db() {
    require_system_e2e_env();
    journey_session_artifacts_matrix::run_session_artifact_http_matches_session_artifacts_rows()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_published_artifact_http_round_trip() {
    require_system_e2e_env();
    journey_session_artifacts_matrix::run_published_session_artifact_round_trip().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_session_artifact_latest_and_download() {
    require_system_e2e_env();
    journey_session_artifacts_matrix::run_session_artifact_latest_and_download_routes().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_failed_session_artifact_latest_and_download() {
    require_system_e2e_env();
    journey_session_artifacts_matrix::run_failed_session_artifact_latest_and_download_routes()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_server_loop_block_parse_preserves_partial_without_replay() {
    require_system_e2e_env();
    journey_session_artifacts_matrix::run_server_loop_block_parse_preserves_partial_without_replay_routes()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_server_loop_transport_preserves_partial_without_replay() {
    require_system_e2e_env();
    journey_session_artifacts_matrix::run_server_loop_transport_preserves_partial_without_replay_routes()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_server_loop_idle_preserves_partial_without_replay() {
    require_system_e2e_env();
    journey_session_artifacts_matrix::run_server_loop_idle_preserves_partial_without_replay_routes(
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_server_loop_rate_limit_failure_session_artifact_latest_and_download() {
    require_system_e2e_env();
    journey_session_artifacts_matrix::run_server_loop_rate_limit_failure_session_artifact_latest_and_download_routes()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_server_loop_rate_limit_retry_success_session_artifact_latest_and_download() {
    require_system_e2e_env();
    journey_session_artifacts_matrix::run_server_loop_rate_limit_retry_success_session_artifact_latest_and_download_routes()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_session_artifact_latest_tiebreaker() {
    require_system_e2e_env();
    journey_session_artifacts_matrix::run_session_artifact_latest_route_uses_stable_tiebreaker()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_evaluation_reads() {
    require_system_e2e_env();
    journey_evaluation_reads_matrix::run_evaluation_read_http_smoke().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_context_decision_chain() {
    require_system_e2e_env();
    journey_context_decision_chain_matrix::run_context_decision_chain_db().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_models() {
    require_system_e2e_env();
    journey_models_matrix::run_models_smoke().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_branches_cost_estimate_http() {
    require_system_e2e_env();
    journey_branches_matrix::run_branches_cost_estimate_http().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_delegate_http_boundaries() {
    require_system_e2e_env();
    journey_delegate_http_matrix::run_delegate_http_boundaries().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — see module doc"]
async fn e2e_matrix_admin_control_plane_rbac() {
    require_system_e2e_env();
    journey_admin_smoke_matrix::run_admin_control_plane_rbac().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §5.3"]
async fn e2e_matrix_saas_resource_limits_read_and_admin_override() {
    require_system_e2e_env();
    journey_saas_platform_matrix::run_saas_resource_limits_read_and_admin_override().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §5.3"]
async fn e2e_matrix_saas_resource_daily_session_cap_denies_chat() {
    require_system_e2e_env();
    journey_saas_platform_matrix::run_saas_resource_daily_session_cap_denies_chat().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §5.3"]
async fn e2e_matrix_saas_resource_concurrent_session_cap_denies_chat() {
    require_system_e2e_env();
    journey_saas_platform_matrix::run_saas_resource_concurrent_session_cap_denies_chat().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §5.2"]
async fn e2e_matrix_saas_admin_config_crud_rbac() {
    require_system_e2e_env();
    journey_saas_platform_matrix::run_saas_admin_config_crud_rbac().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §5.2"]
async fn e2e_matrix_saas_admin_grant_revoke_rbac_flow() {
    require_system_e2e_env();
    journey_saas_platform_matrix::run_saas_admin_grant_revoke_rbac_flow().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §5.3"]
async fn e2e_matrix_saas_resource_usage_per_user_isolation() {
    require_system_e2e_env();
    journey_saas_platform_matrix::run_saas_resource_usage_per_user_isolation().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §5.1"]
async fn e2e_matrix_saas_auth_refresh_cycle() {
    require_system_e2e_env();
    journey_saas_platform_matrix::run_saas_auth_refresh_cycle().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §5.4"]
async fn e2e_matrix_saas_session_cross_user_isolation() {
    require_system_e2e_env();
    journey_saas_platform_matrix::run_saas_session_cross_user_isolation().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §5.4"]
async fn e2e_matrix_saas_events_and_audit_cross_user_isolation() {
    require_system_e2e_env();
    journey_saas_platform_matrix::run_saas_events_and_audit_cross_user_isolation().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §5.1 auth negatives"]
async fn e2e_matrix_saas_auth_negative_paths() {
    require_system_e2e_env();
    journey_saas_negative_matrix::run_saas_auth_negative_paths().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §5.3 resource negatives"]
async fn e2e_matrix_saas_resource_governance_negative_paths() {
    require_system_e2e_env();
    journey_saas_negative_matrix::run_saas_resource_governance_negative_paths().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §5.3 concurrent cap recovery"]
async fn e2e_matrix_saas_resource_concurrent_cap_recovery() {
    require_system_e2e_env();
    journey_saas_negative_matrix::run_saas_resource_concurrent_cap_recovery().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §5.1 logout/expired JWT"]
async fn e2e_matrix_saas_auth_logout_and_expired_token() {
    require_system_e2e_env();
    journey_saas_negative_matrix::run_saas_auth_logout_and_expired_token().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §5.3 bash/disk limits contract"]
async fn e2e_matrix_saas_resource_limits_extended_fields() {
    require_system_e2e_env();
    journey_saas_negative_matrix::run_saas_resource_limits_extended_fields().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §4.2 edge callback success"]
async fn e2e_matrix_saas_edge_tool_result_success_path() {
    require_system_e2e_env();
    journey_saas_negative_matrix::run_saas_edge_tool_result_success_path().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §5.7 Memoria degradation"]
async fn e2e_matrix_saas_memoria_proxy_degradation() {
    require_system_e2e_env();
    journey_saas_negative_matrix::run_saas_memoria_proxy_degradation().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §4.3 run isolation"]
async fn e2e_matrix_saas_run_cross_user_isolation() {
    require_system_e2e_env();
    journey_saas_negative_matrix::run_saas_run_cross_user_isolation().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §4.3 run state conflict"]
async fn e2e_matrix_saas_run_double_pause_conflict() {
    require_system_e2e_env();
    journey_saas_negative_matrix::run_saas_run_double_pause_conflict().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §4.2 edges/status"]
async fn e2e_matrix_saas_edges_status_smoke() {
    require_system_e2e_env();
    journey_saas_negative_matrix::run_saas_edges_status_smoke().await;
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §4.2 service/edges/status auth gate"]
async fn e2e_matrix_saas_service_edges_status_smoke() {
    require_system_e2e_env();
    journey_saas_negative_matrix::run_saas_service_edges_status_smoke().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS platform §4.2 approval callback"]
async fn e2e_matrix_saas_approval_respond_success_path() {
    require_system_e2e_env();
    journey_saas_negative_matrix::run_saas_approval_respond_success_path().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §5.1/§7.4 health+me"]
async fn e2e_matrix_saas_platform_health_and_auth_me() {
    require_system_e2e_env();
    journey_saas_platform_matrix::run_saas_platform_health_and_auth_me().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §5.1 refresh rotation"]
async fn e2e_matrix_saas_auth_refresh_token_rotation() {
    require_system_e2e_env();
    journey_saas_platform_matrix::run_saas_auth_refresh_token_rotation().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §5.4/§5.7 memory isolation"]
async fn e2e_matrix_saas_memory_proxy_user_isolation() {
    require_system_e2e_env();
    journey_saas_platform_matrix::run_saas_memory_proxy_user_isolation().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §5.6 models list+encryption"]
async fn e2e_matrix_saas_models_list_and_key_encryption() {
    require_system_e2e_env();
    journey_saas_platform_matrix::run_saas_models_list_and_key_encryption().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §5.1/§6.1 session CRUD"]
async fn e2e_matrix_saas_session_lifecycle_positive() {
    require_system_e2e_env();
    journey_saas_platform_matrix::run_saas_session_lifecycle_positive().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §5.3/§6.6 usage increment"]
async fn e2e_matrix_saas_resource_usage_increments_after_chat() {
    require_system_e2e_env();
    journey_saas_platform_matrix::run_saas_resource_usage_increments_after_chat().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §4.3 run cancel isolation"]
async fn e2e_matrix_saas_run_cancel_cross_user_and_owner() {
    require_system_e2e_env();
    journey_saas_platform_matrix::run_saas_run_cancel_cross_user_and_owner().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §4.2 approval deny"]
async fn e2e_matrix_saas_approval_respond_deny_path() {
    require_system_e2e_env();
    journey_saas_platform_matrix::run_saas_approval_respond_deny_path().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §4.3 pause/resume"]
async fn e2e_matrix_saas_chat_run_pause_resume_positive() {
    require_system_e2e_env();
    journey_saas_platform_matrix::run_saas_chat_run_pause_resume_positive().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §5.2 admin tokens"]
async fn e2e_matrix_saas_admin_tokens_rbac_smoke() {
    require_system_e2e_env();
    journey_saas_platform_matrix::run_saas_admin_tokens_rbac_smoke().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §5.1 register/login"]
async fn e2e_matrix_saas_auth_register_login_positive() {
    require_system_e2e_env();
    journey_saas_platform_matrix::run_saas_auth_register_login_positive().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §5.1 duplicate email"]
async fn e2e_matrix_saas_auth_duplicate_email_register() {
    require_system_e2e_env();
    journey_saas_platform_matrix::run_saas_auth_duplicate_email_register().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §4.3 GET /runs"]
async fn e2e_matrix_saas_runs_list_pagination_positive() {
    require_system_e2e_env();
    journey_saas_platform_matrix::run_saas_runs_list_pagination_positive().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §4.2 edge register"]
async fn e2e_matrix_saas_edge_agent_registration_smoke() {
    require_system_e2e_env();
    journey_saas_platform_matrix::run_saas_edge_agent_registration_smoke().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §5.2 admin cleanup"]
async fn e2e_matrix_saas_admin_cleanup_rbac_smoke() {
    require_system_e2e_env();
    journey_saas_platform_matrix::run_saas_admin_cleanup_rbac_smoke().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §5.2 admin audit"]
async fn e2e_matrix_saas_admin_audit_rbac_smoke() {
    require_system_e2e_env();
    journey_saas_platform_matrix::run_saas_admin_audit_rbac_smoke().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §5.4 skills isolation"]
async fn e2e_matrix_saas_skills_cross_user_isolation() {
    require_system_e2e_env();
    journey_saas_platform_matrix::run_saas_skills_cross_user_isolation().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §5.4 team isolation"]
async fn e2e_matrix_saas_team_cross_user_isolation() {
    require_system_e2e_env();
    journey_saas_platform_matrix::run_saas_team_cross_user_isolation().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §6.1 replay compare guardrail"]
async fn e2e_matrix_saas_session_replay_compare_unavailable_guardrail() {
    require_system_e2e_env();
    journey_saas_platform_matrix::run_saas_session_replay_compare_unavailable_guardrail().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §6.1 replay POST guardrail"]
async fn e2e_matrix_saas_session_replay_post_unavailable_guardrail() {
    require_system_e2e_env();
    journey_saas_platform_matrix::run_saas_session_replay_post_unavailable_guardrail().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §5.2 admin feedback stats"]
async fn e2e_matrix_saas_admin_feedback_stats_rbac() {
    require_system_e2e_env();
    journey_saas_platform_matrix::run_saas_admin_feedback_stats_rbac().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §4.3 run projection"]
async fn e2e_matrix_saas_run_projection_smoke() {
    require_system_e2e_env();
    journey_saas_platform_matrix::run_saas_run_projection_smoke().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §5.4 session audit smoke"]
async fn e2e_matrix_saas_session_audit_after_chat_smoke() {
    require_system_e2e_env();
    journey_saas_platform_matrix::run_saas_session_audit_after_chat_smoke().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §5.1 platform snapshot"]
async fn e2e_matrix_saas_platform_snapshot_smoke() {
    require_system_e2e_env();
    journey_saas_platform_matrix::run_saas_platform_snapshot_smoke().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §5.1 session activity/transcript"]
async fn e2e_matrix_saas_session_activity_transcript_artifacts_smoke() {
    require_system_e2e_env();
    journey_saas_platform_matrix::run_saas_session_activity_transcript_artifacts_smoke().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §5.4 events/session positive"]
async fn e2e_matrix_saas_events_session_after_chat_positive() {
    require_system_e2e_env();
    journey_saas_platform_matrix::run_saas_events_session_after_chat_positive().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_TEST_DB_IT=1 — SaaS §4.3 delegate HTTP"]
async fn e2e_matrix_saas_delegate_http_boundaries() {
    require_system_e2e_env();
    journey_saas_platform_matrix::run_saas_delegate_http_boundaries().await;
}
