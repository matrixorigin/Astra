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
//! - **`e2e_matrix_chat_turn_empty_session_id`** — `POST /chat/turn` rejects whitespace-only
//!   `session_id` values with `400` before any SSE stream starts.
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
mod journey_audit_cross_session;
mod journey_extended;
mod journey_full;
mod journey_remote_skills;
mod journey_stream_persistence;
mod journey_tasks_runs;
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
async fn e2e_matrix_chat_turn_empty_session_id() {
    require_system_e2e_env();
    journey_extended::run_chat_turn_empty_session_id_rejected().await;
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
