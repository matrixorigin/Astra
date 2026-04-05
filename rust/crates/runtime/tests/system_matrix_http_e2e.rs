//! System HTTP end-to-end: real Axum app + MatrixOne + full `build_server_state` wiring.
//!
//! ## What this covers
//! - `POST /auth/register` → `POST /sessions` → `POST /chat/turn` (SSE)
//! - Mock LLM via `test_llm_rounds` + `bridge-e2e-hooks` (no external model server)
//! - Mock Memoria HTTP via [`astra_runtime::MemoriaForwarder`] injection (does not affect chat path)
//! - SQL assertions on `agent_events` rows produced by the in-process bridge persistence
//!
//! ## How to run
//! ```text
//! # Terminal 1: MatrixOne with schema (same as local dev)
//! MO_AGENT_SYSTEM_MATRIX_E2E=1 \
//! MO_AGENT_BRIDGE_TEST_SECRET=system-matrix-e2e-secret \
//! cargo test -p astra-runtime --test system_matrix_http_e2e --features bridge-e2e-hooks -- \
//!   --ignored --nocapture
//! ```
//!
//! Requires the same env as `astra-server` startup: `MATRIXONE_*`, JWT / Fernet secrets from
//! [`astra_core::AppSettings::from_env`], etc. Load `.env` if you use one for development.

use std::sync::Arc;
use std::sync::OnceLock;

use async_trait::async_trait;
use axum::{
    Router,
    body::{self, Body},
    http::{Request, StatusCode},
};
use astra_core::config::AppSettings;
use astra_runtime::{MemoriaForwarder, build_app, build_server_state};
use futures_util::StreamExt;
use serde_json::{Value, json};
use sqlx::mysql::MySqlRow;
use sqlx::Row;
use tokio::sync::Mutex;
use tower::util::ServiceExt;
use uuid::Uuid;

static E2E_ENV_INIT: OnceLock<()> = OnceLock::new();

fn require_system_e2e_env() {
    assert_eq!(
        std::env::var("MO_AGENT_SYSTEM_MATRIX_E2E").as_deref(),
        Ok("1"),
        "set MO_AGENT_SYSTEM_MATRIX_E2E=1 to run this ignored test"
    );
    E2E_ENV_INIT.get_or_init(|| {
        let secret = std::env::var("MO_AGENT_BRIDGE_TEST_SECRET").unwrap_or_else(|_| {
            "system-matrix-e2e-secret".to_string()
        });
        // SAFETY: set once before parallel test threads (single ignored test in practice).
        unsafe {
            std::env::set_var("MO_AGENT_BRIDGE_TEST_SECRET", &secret);
        }
    });
}

#[derive(Clone, Default)]
struct E2eMemoriaStub {
    calls: Arc<Mutex<Vec<(String, Value)>>>,
}

#[async_trait]
impl MemoriaForwarder for E2eMemoriaStub {
    async fn forward(
        &self,
        endpoint: &str,
        body: Value,
    ) -> Result<Value, String> {
        self.calls
            .lock()
            .await
            .push((endpoint.to_string(), body));
        if endpoint.contains("retrieve") {
            return Ok(json!({ "memories": [] }));
        }
        Ok(json!({ "memory_id": "e2e-stub-memory" }))
    }
}

async fn post_json(
    app: &Router,
    path: &str,
    auth: Option<&str>,
    payload: Value,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json");
    if let Some(t) = auth {
        req = req.header("authorization", t);
    }
    let req = req
        .body(Body::from(payload.to_string()))
        .expect("request");
    let response = app.clone().oneshot(req).await.expect("oneshot");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .expect("body");
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));
    (status, json)
}

async fn cleanup_session_data(pool: &sqlx::MySqlPool, session_id: &str) {
    let _ = sqlx::query("DELETE FROM agent_events WHERE session_id = ?")
        .bind(session_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM agent_sessions WHERE session_id = ?")
        .bind(session_id)
        .execute(pool)
        .await;
}

#[tokio::test]
#[ignore = "live MatrixOne + full secrets; MO_AGENT_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn register_session_chat_turn_persists_agent_events_with_expected_columns() {
    require_system_e2e_env();
    dotenvy::dotenv().ok();

    let settings = AppSettings::from_env().expect("AppSettings::from_env (see astra-server env)");
    let url = settings.matrixone.database_url();
    let pool = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect MatrixOne for assertions");

    let state = build_server_state(settings.clone())
        .await
        .expect("build_server_state")
        .with_memoria_forwarder(Arc::new(E2eMemoriaStub::default()));

    let app = build_app(state);

    let suffix = Uuid::new_v4().simple().to_string();
    let username = format!("sys_e2e_{suffix}");
    let email = format!("sys_e2e_{suffix}@e2e.test");

    let (st, reg) = post_json(
        &app,
        "/auth/register",
        None,
        json!({
            "username": username,
            "email": email,
            "password": "E2e-test-pass-9",
            "display_name": "System E2E User"
        }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "register: {reg}");
    let access = reg["access_token"].as_str().expect("access_token");
    let user_id = reg["user_id"].as_str().expect("user_id");
    let auth_header = format!("Bearer {access}");

    let (st2, sess) = post_json(
        &app,
        "/sessions",
        Some(&auth_header),
        json!({ "title": "system e2e session", "metadata": {} }),
    )
    .await;
    assert_eq!(st2, StatusCode::CREATED, "create session: {sess}");
    let session_id = sess["session_id"].as_str().expect("session_id").to_string();

    cleanup_session_data(&pool, &session_id).await;

    const LLM_TEXT: &str = "system-matrix-e2e-reply";
    let chat_body = json!({
        "agent_id": "system-e2e-agent",
        "session_id": session_id,
        "messages": [{ "role": "user", "content": "ping for system e2e" }],
        "edge_tools": [],
        "test_llm_rounds": [{
            "full_text": LLM_TEXT,
            "reasoning": "",
            "usage": { "prompt": 10, "completion": 20, "total": 30 }
        }]
    });

    let test_secret = std::env::var("MO_AGENT_BRIDGE_TEST_SECRET").expect("bridge test secret");
    let chat_req = Request::builder()
        .method("POST")
        .uri("/chat/turn")
        .header("authorization", &auth_header)
        .header("content-type", "application/json")
        .header("x-mo-bridge-test-secret", &test_secret)
        .body(Body::from(chat_body.to_string()))
        .expect("chat request");

    let response = app.clone().oneshot(chat_req).await.expect("chat oneshot");
    assert_eq!(response.status(), StatusCode::OK, "chat/turn should return 200");

    let mut stream = response.into_body().into_data_stream();
    let mut acc = Vec::new();
    let mut saw_turn_complete = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("sse chunk");
        acc.extend_from_slice(&chunk);
        if String::from_utf8_lossy(&acc).contains("turn_complete") {
            saw_turn_complete = true;
            break;
        }
    }
    assert!(
        saw_turn_complete,
        "expected turn_complete in SSE, got: {}",
        String::from_utf8_lossy(&acc)
    );

    // Persist hooks run asynchronously on the bridge path.
    tokio::time::sleep(std::time::Duration::from_millis(900)).await;

    let recs = sqlx::query(
        "SELECT event_id, session_id, user_id, event_type, content, parent_event_id, \
         causal_chain_id, token_input, token_output, token_total, llm_model_used, reasoning_content \
         FROM agent_events WHERE session_id = ? ORDER BY created_at ASC",
    )
    .bind(&session_id)
    .fetch_all(&pool)
    .await
    .expect("select agent_events");

    assert!(
        !recs.is_empty(),
        "expected at least one agent_events row for session {session_id}"
    );

    fn row_get_str(r: &MySqlRow, col: &str) -> String {
        r.try_get::<String, _>(col)
            .unwrap_or_else(|_| panic!("missing string column {col}"))
    }
    fn row_get_opt_str(r: &MySqlRow, col: &str) -> Option<String> {
        r.try_get::<Option<String>, _>(col).ok().flatten()
    }
    fn row_get_opt_i64(r: &MySqlRow, col: &str) -> Option<i64> {
        r.try_get::<Option<i64>, _>(col).ok().flatten()
    }

    let user_q = recs
        .iter()
        .find(|r| row_get_str(r, "event_type") == "user_query")
        .expect("user_query event");
    assert_eq!(row_get_str(user_q, "session_id"), session_id);
    assert_eq!(row_get_str(user_q, "user_id"), user_id);
    assert!(!row_get_str(user_q, "event_id").is_empty());
    let cc = row_get_opt_str(user_q, "causal_chain_id").unwrap_or_default();
    assert!(!cc.is_empty(), "causal_chain_id should be set");
    let uq_content = row_get_str(user_q, "content");
    assert!(
        uq_content.contains("ping for system e2e"),
        "user_query content: {uq_content}"
    );

    let llm = recs
        .iter()
        .find(|r| row_get_str(r, "event_type") == "llm_response")
        .expect("llm_response event");
    assert_eq!(row_get_str(llm, "session_id"), session_id);
    assert_eq!(row_get_str(llm, "user_id"), user_id);
    let llm_content = row_get_str(llm, "content");
    assert!(
        llm_content.contains(LLM_TEXT),
        "llm_response content: {llm_content}"
    );
    let uq_event_id = row_get_str(user_q, "event_id");
    assert_eq!(
        row_get_opt_str(llm, "parent_event_id").as_deref(),
        Some(uq_event_id.as_str())
    );
    assert_eq!(row_get_opt_i64(llm, "token_input"), Some(10));
    assert_eq!(row_get_opt_i64(llm, "token_output"), Some(20));
    assert_eq!(row_get_opt_i64(llm, "token_total"), Some(30));
    assert_eq!(
        row_get_opt_str(llm, "llm_model_used").as_deref(),
        Some("bridge-e2e-mock"),
        "e2e mock model name"
    );

    cleanup_session_data(&pool, &session_id).await;
    pool.close().await;
}
