//! System HTTP end-to-end: real Axum app + MatrixOne + full `build_server_state` wiring.
//!
//! ## Tests in this binary
//! - **`product_matrix_api_journey_hits_multiple_tables`** — full product journey (sessions → agents →
//!   events → jobs → `chat/turn` SSE + `agent_events` assertions → logout). Same coverage as the
//!   historical monolithic test.
//! - **`e2e_matrix_basic_auth_session_lifecycle`** — register/bootstrap, session list/get/put,
//!   close/resume + `agent_sessions` status, activity, logout.
//! - **`e2e_matrix_tasks_lease_and_db_assertions`** — `POST /tasks`, `agent_tasks` row, edge
//!   register, lease claim/release, `task_leases` + `PUT /tasks/{id}/status` + `agent_tasks`.
//! - **`e2e_matrix_chat_run_pause_resume_http`** — `POST /chat` (background run), immediate
//!   pause/resume + `GET /chat/runs/{run_id}` (run state is in-memory + optional engine; no Matrix
//!   table assertion today).
//!
//! External dependencies remain mocked where the product already allows it:
//! - LLM: `test_llm_rounds` + `bridge-e2e-hooks` on `/chat/turn` (no external model server).
//! - Memoria: [`astra_runtime::MemoriaForwarder`] stub (memory proxy routes only).
//!
//! ## How to run
//! ```text
//! MO_AGENT_SYSTEM_MATRIX_E2E=1 \
//! MO_AGENT_BRIDGE_TEST_SECRET=system-matrix-e2e-secret \
//! cargo test -p astra-runtime --test system_matrix_http_e2e --features bridge-e2e-hooks -- \
//!   --ignored --nocapture
//! ```
//!
//! Requires the same env as `astra-server` startup: `MATRIXONE_*`, JWT / Fernet secrets from
//! [`astra_core::AppSettings::from_env`], etc. Load `.env` if you use one for development.
//!
//! See `docs/testing/system-e2e-matrix.md` for the capability ↔ route ↔ test mapping.

mod harness;
mod journey_basic;
mod journey_full;
mod journey_tasks_runs;

use harness::require_system_e2e_env;

#[tokio::test]
#[ignore = "live MatrixOne + full secrets; MO_AGENT_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn product_matrix_api_journey_hits_multiple_tables() {
    require_system_e2e_env();
    let mut b = harness::bootstrap().await;
    journey_full::run_product_matrix_full_journey(&b.ctx, &mut b.auth_header, &mut b.refresh_token)
        .await;
    b.ctx.pool.close().await;
}

#[tokio::test]
#[ignore = "live MatrixOne + full secrets; MO_AGENT_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_basic_auth_session_lifecycle() {
    require_system_e2e_env();
    journey_basic::run_basic_auth_session_lifecycle().await;
}

#[tokio::test]
#[ignore = "live MatrixOne + full secrets; MO_AGENT_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_tasks_lease_and_db_assertions() {
    require_system_e2e_env();
    journey_tasks_runs::run_tasks_lease_with_db_assertions().await;
}

#[tokio::test]
#[ignore = "live MatrixOne + full secrets; MO_AGENT_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn e2e_matrix_chat_run_pause_resume_http() {
    require_system_e2e_env();
    journey_tasks_runs::run_chat_run_pause_resume_http().await;
}
