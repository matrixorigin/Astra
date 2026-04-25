//! System HTTP end-to-end: real Axum app + MatrixOne + full `build_server_state` wiring.
//!
//! ## Tests in this binary (ignored tests)
//! - **`product_matrix_api_journey_hits_multiple_tables`** — full product journey (sessions → agents →
//!   events → jobs → `chat/turn` SSE + `agent_events` assertions → logout), including
//!   `GET /platform/snapshot` after session activity.
//! - **`e2e_matrix_tasks_lease_and_db_assertions`** — `POST /tasks`, `GET /tasks`, `GET /tasks/{id}`,
//!   `GET .../progress`, edge register, lease claim / `GET` lease / renew / release, `task_leases` +
//!   `PUT /tasks/{id}/status` + `agent_tasks`.
//! - **`e2e_matrix_chat_run_pause_resume_http`** — `POST /chat` (background run), immediate
//!   pause/resume + `GET /chat/runs/{run_id}` (run state is in-memory + optional engine; no Matrix
//!   table assertion today).
//! - **`e2e_matrix_session_cancel_delete`** — `POST .../cancel` + DB `cancelled`, then `DELETE` + 404.
//! - **`e2e_matrix_chat_stream_session_info`** — `POST /chat/stream` buffered SSE; first `session_info`
//!   event contains `run_id`.
//! - **`e2e_matrix_approval_respond_invalid_session_id`** — `POST /approval/respond` rejects unsafe
//!   `session_id` values with `400` instead of writing approval journal data for an invalid path.
//! - **`e2e_matrix_edge_callback_http_boundary_failures`** — callback routes reject unauthenticated and
//!   malformed `/tools/result` / `/approval/respond` requests with client/auth errors instead of
//!   accepting transport-boundary garbage.
//! - **`e2e_matrix_duplicate_tool_result_idempotency`** — `POST /tools/result` twice for the same
//!   `request_id` during a live handoff; assert the initial `chat/turn` SSE still emits one
//!   `tool_request` and ends with `has_tool_calls=true`.
//! - **`e2e_matrix_duplicate_approval_response_idempotency`** — `POST /approval/respond` twice for the
//!   same `request_id`; assert the session journal records one `approval_decision`.
//! - **`e2e_matrix_chat_turn_partial_batch_failure`** — one `chat/turn` round emits two `tool_request`
//!   callbacks; post one success and one failure and assert the initial SSE handoff still ends with
//!   `has_tool_calls=true`.
//! - **`e2e_matrix_chat_turn_out_of_order_tool_results`** — one `chat/turn` round emits two
//!   `tool_request` callbacks, then accepts the second callback result before the first while the
//!   initial SSE handoff still ends with `has_tool_calls=true`.
//! - **`e2e_matrix_same_session_concurrent_turns_isolated`** — two concurrent `POST /chat/turn`
//!   requests target the same session; assert both complete with distinct persisted `event_id` and
//!   `causal_chain_id` values instead of cross-wiring each other.
//! - **`e2e_matrix_same_session_waiting_turn_overlap_isolated`** — a same-session tool-backed
//!   handoff can overlap a second plain turn without leaking the second turn's response into the
//!   first stream (or vice versa).
//! - **`e2e_matrix_auth_session_negative_paths`** — `GET /sessions` without auth (401), plus
//!   mode-aware auth negatives: in `local_jwt` validates duplicate register/bad login; in
//!   `trusted_moi` validates local auth endpoints are disabled (replaces stub `auth_contract` /
//!   `session_contract` negative coverage).
//! - **`e2e_matrix_memory_proxy_user_isolation`** — unauthenticated memory returns 401; spoofed
//!   `user_id` / `session_id` in body are overwritten to JWT user on forward (replaces `memory_contract`
//!   isolation tests).
//! - **`e2e_matrix_models_admin_crud`** — grant `astra_admin`, `POST/PUT/DELETE /models` with
//!   `provider: mock` + `infra_llm_models` SQL checks (replaces `model_crud_contract`).
//! - **`e2e_matrix_audit_cross_session_analytics_http`** — SQL-seeded `agent_events` / `ctx_decision_audits`,
//!   then `GET /audit/stats`, `/audit/mutations`, `/audit/promotions` with JWT auth (cross-session audit).
//! - **`e2e_matrix_trusted_moi_user_system_integration`** — run server in `trusted_moi` mode,
//!   authenticate via external JWT claims, verify local auth endpoints are disabled, and assert
//!   session/memory ownership maps to upstream user id.
//! - **`e2e_matrix_remote_skill_registration_user_system_integration`** — register remote skill via
//!   `/skills` with mode-aware bootstrap (`local_jwt` or `trusted_moi`), verify validation behavior,
//!   list/get/version discoverability, and `skills_registry.created_by` ownership mapping.
//! - **`e2e_matrix_stream_session_and_run_status`** — `POST /chat/stream` with mock LLM → verify
//!   `agent_sessions` row persisted, run transitions to `completed`, `events_count > 0`.
//! - **`e2e_matrix_stream_context_trace_persistence`** — `POST /chat/stream` → verify
//!   `context_trace_signal` event written to `agent_events` with valid causal chain.
//! - **`e2e_matrix_stream_multi_turn_persistence`** — two sequential `POST /chat/stream` to same
//!   session → verify event counts increment, distinct causal chains, both runs completed.
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
//! - **`e2e_matrix_evaluation_reads`** — evaluation GETs (`x-user-id`), optional agent seed for trust/SLO/
//!   observability, plus learning health/signals.
//! - **`e2e_matrix_context_decision_chain`** — `POST /events` → `/context` → `/decisions` + `ctx_snapshots` /
//!   `ctx_decision_audits` SQL.
//! - **`e2e_matrix_chat_route_models`** — `POST /chat/route` shape + `GET /models`.
//! - **`e2e_matrix_branches_cost_estimate_http`** — `POST /branches/cost-estimate` (JWT, numeric
//!   estimate fields); 401 without auth (no DDL branch ops).
//! - **`e2e_matrix_delegate_http_boundaries`** — `POST /chat` → `run_id`; `GET .../delegations`;
//!   `POST .../delegate` fails at validation (`400`) without executing sub-runs.
//! - **`e2e_matrix_admin_tokens_smoke`** — `GET /admin/tokens`: `403` without `astra_admin`,
//!   then `200` + JSON array after `grant_astra_admin_role`.
//!
//! Session list/get/put, close/resume, activity, and DB checks for close/resume live only in the full
//! journey (not duplicated in a separate test).
//!
//! External dependencies remain mocked where the product already allows it:
//! - LLM: `test_llm_rounds` + `bridge-e2e-hooks` on `/chat/turn` (no external model server).
//! - Memoria: [`astra_runtime::MemoriaForwarder`] stub (memory proxy routes only).
//!
//! ## How to run
//! ```text
//! ASTRA_SYSTEM_MATRIX_E2E=1 \
//! ASTRA_BRIDGE_TEST_SECRET=system-matrix-e2e-secret \
//! cargo test -p astra-runtime --test system_matrix_http_e2e --features bridge-e2e-hooks -- \
//!   --ignored --nocapture
//! ```
//!
//! Requires the same env as `astra-server` startup: `MATRIXONE_*`, `JWT_SECRET_KEY` / `SECRET_KEY`
//! from [`astra_core::AppSettings::from_env`], etc. Load `.env` if you use one for development.
//!
//! See `docs/testing/system-e2e-matrix.md` for the capability ↔ route ↔ test mapping, DB isolation, and
//! parallelism (`make test` / `ASTRA_SYSTEM_MATRIX_E2E_TEST_THREADS`).

mod harness;
mod journey_admin_smoke_matrix;
mod journey_audit_cross_session;
mod journey_branches_matrix;
mod journey_chat_route_models_matrix;
mod journey_context_decision_chain_matrix;
mod journey_delegate_http_matrix;
mod journey_evaluation_reads_matrix;
mod journey_extended;
mod journey_full;
mod journey_meta_matrix;
mod journey_remote_skills;
mod journey_session_artifacts_matrix;
mod journey_session_http_db_matrix;
mod journey_stream_persistence;
mod journey_tasks_runs;
mod journey_team_crud_matrix;
mod journey_team_data_fidelity_matrix;
mod journey_team_http_negatives_matrix;
mod journey_team_isolation_matrix;
mod journey_team_snapshots_matrix;
mod journey_trusted_moi;

use harness::require_system_e2e_env;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn product_matrix_api_journey_hits_multiple_tables() {
    require_system_e2e_env();
    let mut b = harness::bootstrap().await;
    journey_full::run_product_matrix_full_journey(
        &b.ctx,
        &mut b.auth_header,
        &mut b.refresh_token,
        b.auth_mode,
    )
    .await;
    b.ctx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_tasks_lease_and_db_assertions() {
    require_system_e2e_env();
    journey_tasks_runs::run_tasks_lease_with_db_assertions().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_chat_run_pause_resume_http() {
    require_system_e2e_env();
    journey_tasks_runs::run_chat_run_pause_resume_http().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_session_cancel_delete() {
    require_system_e2e_env();
    journey_extended::run_session_cancel_then_delete().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_chat_stream_session_info() {
    require_system_e2e_env();
    journey_extended::run_chat_stream_session_info_smoke().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_approval_respond_invalid_session_id() {
    require_system_e2e_env();
    journey_extended::run_approval_respond_invalid_session_id_rejected().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_edge_callback_http_boundary_failures() {
    require_system_e2e_env();
    journey_extended::run_edge_callback_http_boundary_failures().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_duplicate_tool_result_idempotency() {
    require_system_e2e_env();
    journey_extended::run_duplicate_tool_result_is_idempotent().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_duplicate_approval_response_idempotency() {
    require_system_e2e_env();
    journey_extended::run_duplicate_approval_response_is_idempotent().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_chat_turn_partial_batch_failure() {
    require_system_e2e_env();
    journey_extended::run_chat_turn_partial_batch_failure().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_chat_turn_out_of_order_tool_results() {
    require_system_e2e_env();
    journey_extended::run_chat_turn_out_of_order_tool_results().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_same_session_concurrent_turns_isolated() {
    require_system_e2e_env();
    journey_extended::run_same_session_concurrent_turns_isolated().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_same_session_waiting_turn_overlap_isolated() {
    require_system_e2e_env();
    journey_extended::run_same_session_waiting_turn_overlap_isolated().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_auth_session_negative_paths() {
    require_system_e2e_env();
    journey_extended::run_auth_and_session_negative_paths().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_memory_proxy_user_isolation() {
    require_system_e2e_env();
    journey_extended::run_memory_proxy_user_isolation().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_models_admin_crud() {
    require_system_e2e_env();
    journey_extended::run_models_admin_crud_with_db().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_audit_cross_session_analytics_http() {
    require_system_e2e_env();
    journey_audit_cross_session::run_audit_cross_session_analytics_http().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_trusted_moi_user_system_integration() {
    require_system_e2e_env();
    journey_trusted_moi::run_trusted_moi_user_system_integration().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_remote_skill_registration_user_system_integration() {
    require_system_e2e_env();
    journey_remote_skills::run_remote_skill_registration_user_system_integration().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_stream_session_and_run_status() {
    require_system_e2e_env();
    journey_stream_persistence::run_stream_session_and_run_status().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_stream_context_trace_persistence() {
    require_system_e2e_env();
    journey_stream_persistence::run_stream_context_trace_persistence().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_stream_multi_turn_persistence() {
    require_system_e2e_env();
    journey_stream_persistence::run_stream_multi_turn_persistence().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_team_crud_and_db() {
    require_system_e2e_env();
    journey_team_crud_matrix::run_team_crud_db().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_team_snapshots_and_db() {
    require_system_e2e_env();
    journey_team_snapshots_matrix::run_team_snapshots_db().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_team_http_negative_paths() {
    require_system_e2e_env();
    journey_team_http_negatives_matrix::run_team_http_negative_paths().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_team_http_db_fidelity() {
    require_system_e2e_env();
    journey_team_data_fidelity_matrix::run_team_http_db_fidelity().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_team_cross_user_isolation() {
    require_system_e2e_env();
    journey_team_isolation_matrix::run_team_cross_user_isolation().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_meta_health() {
    require_system_e2e_env();
    journey_meta_matrix::run_meta_root_and_health().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_session_http_db() {
    require_system_e2e_env();
    journey_session_http_db_matrix::run_session_http_matches_agent_sessions_row().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_session_artifact_http_db() {
    require_system_e2e_env();
    journey_session_artifacts_matrix::run_session_artifact_http_matches_session_artifacts_rows()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_published_artifact_http_round_trip() {
    require_system_e2e_env();
    journey_session_artifacts_matrix::run_published_session_artifact_round_trip().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_session_artifact_latest_and_download() {
    require_system_e2e_env();
    journey_session_artifacts_matrix::run_session_artifact_latest_and_download_routes().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_evaluation_reads() {
    require_system_e2e_env();
    journey_evaluation_reads_matrix::run_evaluation_read_http_smoke().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_context_decision_chain() {
    require_system_e2e_env();
    journey_context_decision_chain_matrix::run_context_decision_chain_db().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_chat_route_models() {
    require_system_e2e_env();
    journey_chat_route_models_matrix::run_chat_route_and_models_smoke().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_branches_cost_estimate_http() {
    require_system_e2e_env();
    journey_branches_matrix::run_branches_cost_estimate_http().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_delegate_http_boundaries() {
    require_system_e2e_env();
    journey_delegate_http_matrix::run_delegate_http_boundaries().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_admin_tokens_smoke() {
    require_system_e2e_env();
    journey_admin_smoke_matrix::run_admin_tokens_smoke().await;
}
