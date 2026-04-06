//! System HTTP end-to-end: real Axum app + MatrixOne + full `build_server_state` wiring.
//!
//! ## Tests in this binary
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
mod journey_extended;
mod journey_full;
mod journey_tasks_runs;

use harness::require_system_e2e_env;

#[tokio::test]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn product_matrix_api_journey_hits_multiple_tables() {
    require_system_e2e_env();
    let mut b = harness::bootstrap().await;
    journey_full::run_product_matrix_full_journey(&b.ctx, &mut b.auth_header, &mut b.refresh_token)
        .await;
    b.ctx.pool.close().await;
}

#[tokio::test]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_tasks_lease_and_db_assertions() {
    require_system_e2e_env();
    journey_tasks_runs::run_tasks_lease_with_db_assertions().await;
}

#[tokio::test]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_chat_run_pause_resume_http() {
    require_system_e2e_env();
    journey_tasks_runs::run_chat_run_pause_resume_http().await;
}

#[tokio::test]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_session_cancel_delete() {
    require_system_e2e_env();
    journey_extended::run_session_cancel_then_delete().await;
}

#[tokio::test]
#[ignore = "live MatrixOne + full secrets; ASTRA_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_chat_stream_session_info() {
    require_system_e2e_env();
    journey_extended::run_chat_stream_session_info_smoke().await;
}
