//! Shared harness for **system** Matrix HTTP E2E (Phase A: env gate, HTTP helpers, `sqlx` cleanup /
//! row assertions, [`bootstrap`] / [`MatrixE2eCtx`]).
//!
//! This module is the extracted equivalent of the historical monolithic
//! `tests/system_matrix_http_e2e.rs` setup; journey logic lives in `journey_*.rs`. See
//! `docs/testing/system-e2e-matrix.md` for the capability ↔ route ↔ test mapping.

use std::sync::Arc;
use std::sync::OnceLock;

use astra_core::config::AppSettings;
use astra_runtime::{MemoriaForwarder, build_app, build_server_state};
use async_trait::async_trait;
use axum::{
    Router,
    body::{self, Body},
    http::{Request, StatusCode},
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use sqlx::Row;
use sqlx::mysql::MySqlRow;
use tokio::sync::Mutex;
use tower::util::ServiceExt;
use uuid::Uuid;

static E2E_ENV_INIT: OnceLock<()> = OnceLock::new();

pub fn require_system_e2e_env() {
    assert_eq!(
        std::env::var("MO_AGENT_SYSTEM_MATRIX_E2E").as_deref(),
        Ok("1"),
        "set MO_AGENT_SYSTEM_MATRIX_E2E=1 to run this ignored test"
    );
    E2E_ENV_INIT.get_or_init(|| {
        let secret = std::env::var("MO_AGENT_BRIDGE_TEST_SECRET")
            .unwrap_or_else(|_| "system-matrix-e2e-secret".to_string());
        // SAFETY: set once before parallel test threads (idempotent for all E2E tests).
        unsafe {
            std::env::set_var("MO_AGENT_BRIDGE_TEST_SECRET", &secret);
        }
    });
}

#[derive(Clone, Default)]
pub struct E2eMemoriaStub {
    pub calls: Arc<Mutex<Vec<(String, Value)>>>,
}

#[async_trait]
impl MemoriaForwarder for E2eMemoriaStub {
    async fn forward(&self, endpoint: &str, body: Value) -> Result<Value, String> {
        self.calls.lock().await.push((endpoint.to_string(), body));
        if endpoint.contains("retrieve") {
            return Ok(json!({ "memories": [] }));
        }
        Ok(json!({ "memory_id": "e2e-stub-memory" }))
    }
}

pub async fn get_json(
    app: &Router,
    path: &str,
    auth: Option<&str>,
    extra_headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut req = Request::builder().method("GET").uri(path);
    if let Some(t) = auth {
        req = req.header("authorization", t);
    }
    for (k, v) in extra_headers {
        req = req.header(*k, *v);
    }
    let req = req.body(Body::empty()).expect("request");
    let response = app.clone().oneshot(req).await.expect("oneshot");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .expect("body");
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));
    (status, json)
}

pub async fn put_json(
    app: &Router,
    path: &str,
    auth: Option<&str>,
    payload: Value,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method("PUT")
        .uri(path)
        .header("content-type", "application/json");
    if let Some(t) = auth {
        req = req.header("authorization", t);
    }
    let req = req.body(Body::from(payload.to_string())).expect("request");
    let response = app.clone().oneshot(req).await.expect("oneshot");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .expect("body");
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));
    (status, json)
}

pub async fn delete_no_content(app: &Router, path: &str, auth: Option<&str>) -> StatusCode {
    let mut req = Request::builder().method("DELETE").uri(path);
    if let Some(t) = auth {
        req = req.header("authorization", t);
    }
    let req = req.body(Body::empty()).expect("request");
    let response = app.clone().oneshot(req).await.expect("oneshot");
    response.status()
}

pub async fn delete_json(app: &Router, path: &str, auth: Option<&str>) -> (StatusCode, Value) {
    let mut req = Request::builder().method("DELETE").uri(path);
    if let Some(t) = auth {
        req = req.header("authorization", t);
    }
    let req = req.body(Body::empty()).expect("request");
    let response = app.clone().oneshot(req).await.expect("oneshot");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .expect("body");
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));
    (status, json)
}

/// POST with empty body (routes that have no `Json` extractor).
pub async fn post_empty(app: &Router, path: &str, auth: Option<&str>) -> (StatusCode, Value) {
    let mut req = Request::builder().method("POST").uri(path);
    if let Some(t) = auth {
        req = req.header("authorization", t);
    }
    let req = req.body(Body::empty()).expect("request");
    let response = app.clone().oneshot(req).await.expect("oneshot");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .expect("body");
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));
    (status, json)
}

/// POST JSON and collect the response body as UTF-8 (for small buffered SSE bodies).
pub async fn post_json_collect_body_text(
    app: &Router,
    path: &str,
    auth: Option<&str>,
    payload: &Value,
    max_bytes: usize,
) -> (StatusCode, String) {
    let mut req = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json");
    if let Some(t) = auth {
        req = req.header("authorization", t);
    }
    let req = req.body(Body::from(payload.to_string())).expect("request");
    let response = app.clone().oneshot(req).await.expect("oneshot");
    let status = response.status();
    if !status.is_success() {
        let bytes = body::to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap_or_default();
        return (status, String::from_utf8_lossy(&bytes).to_string());
    }
    let mut stream = response.into_body().into_data_stream();
    let mut acc = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("body chunk");
        acc.extend_from_slice(&chunk);
        if acc.len() >= max_bytes {
            break;
        }
        let preview = String::from_utf8_lossy(&acc);
        if preview.contains("\"run_id\"") && preview.contains("session_info") {
            break;
        }
    }
    (status, String::from_utf8_lossy(&acc).to_string())
}

/// Parse SSE `data: {...}` blocks; return the first JSON object whose `type` matches.
pub fn sse_first_data_json_with_type(body: &str, want_type: &str) -> Option<Value> {
    for block in body.split("\n\n") {
        let line = block.lines().find(|l| l.starts_with("data: "));
        let Some(l) = line else {
            continue;
        };
        let rest = l.strip_prefix("data: ")?;
        let v: Value = serde_json::from_str(rest.trim()).ok()?;
        if v.get("type").and_then(|t| t.as_str()) == Some(want_type) {
            return Some(v);
        }
    }
    None
}

pub async fn post_json(
    app: &Router,
    path: &str,
    auth: Option<&str>,
    payload: Value,
) -> (StatusCode, Value) {
    post_json_with_headers(app, path, auth, &[], payload).await
}

pub async fn post_json_with_headers(
    app: &Router,
    path: &str,
    auth: Option<&str>,
    extra_headers: &[(&str, &str)],
    payload: Value,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json");
    if let Some(t) = auth {
        req = req.header("authorization", t);
    }
    for (k, v) in extra_headers {
        req = req.header(*k, *v);
    }
    let req = req.body(Body::from(payload.to_string())).expect("request");
    let response = app.clone().oneshot(req).await.expect("oneshot");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .expect("body");
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));
    (status, json)
}

pub async fn cleanup_session_data(pool: &sqlx::MySqlPool, session_id: &str) {
    let _ = sqlx::query("DELETE FROM ctx_decision_audits WHERE session_id = ?")
        .bind(session_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM ctx_snapshots WHERE session_id = ?")
        .bind(session_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM agent_events WHERE session_id = ?")
        .bind(session_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM agent_sessions WHERE session_id = ?")
        .bind(session_id)
        .execute(pool)
        .await;
}

pub async fn cleanup_edge_registry(pool: &sqlx::MySqlPool, user_id: &str, edge_agent_id: &str) {
    let _ = sqlx::query("DELETE FROM edge_agent_registry WHERE user_id = ? AND edge_agent_id = ?")
        .bind(user_id)
        .bind(edge_agent_id)
        .execute(pool)
        .await;
}

pub async fn cleanup_task_rows(pool: &sqlx::MySqlPool, user_id: &str, task_id: &str) {
    let _ = sqlx::query("DELETE FROM task_leases WHERE user_id = ? AND task_id = ?")
        .bind(user_id)
        .bind(task_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM agent_tasks WHERE task_id = ?")
        .bind(task_id)
        .execute(pool)
        .await;
}

pub fn row_get_str(r: &MySqlRow, col: &str) -> String {
    r.try_get::<String, _>(col)
        .unwrap_or_else(|_| panic!("missing string column {col}"))
}

pub fn row_get_opt_str(r: &MySqlRow, col: &str) -> Option<String> {
    r.try_get::<Option<String>, _>(col).ok().flatten()
}

pub fn row_get_opt_i64(r: &MySqlRow, col: &str) -> Option<i64> {
    r.try_get::<Option<i64>, _>(col).ok().flatten()
}

/// Poll until `agent_events` has at least one row per `event_type` for this session, or `timeout`.
/// Avoids fixed `sleep` after `/chat/turn` SSE (faster on hot DB, less flaky on cold).
pub async fn wait_for_agent_event_types(
    pool: &sqlx::MySqlPool,
    session_id: &str,
    types: &[&str],
    timeout: std::time::Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let mut ok = true;
        for et in types {
            let n: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM agent_events WHERE session_id = ? AND event_type = ?",
            )
            .bind(session_id)
            .bind(*et)
            .fetch_one(pool)
            .await
            .unwrap_or(0);
            if n < 1 {
                ok = false;
                break;
            }
        }
        if ok {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "timeout ({timeout:?}) waiting for agent_events types {types:?} for session_id={session_id}"
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Shared Matrix E2E context after app build + user registration + session creation.
pub struct MatrixE2eCtx {
    pub app: Router,
    pub pool: sqlx::MySqlPool,
    pub memoria: Arc<E2eMemoriaStub>,
    pub user_id: String,
    pub session_id: String,
    pub edge_agent_id: String,
    pub suffix: String,
}

pub struct BootstrapResult {
    pub ctx: MatrixE2eCtx,
    pub auth_header: String,
    pub refresh_token: String,
}

const E2E_PASSWORD: &str = "E2e-matrix-pass-9";

/// Build app, connect pool, register user, refresh token, create session (with cleanup of stale rows).
pub async fn bootstrap() -> BootstrapResult {
    dotenvy::dotenv().ok();

    let settings = AppSettings::from_env().expect("AppSettings::from_env (see astra-server env)");
    let url = settings.matrixone.database_url();
    let pool = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect MatrixOne for assertions");

    let memoria = Arc::new(E2eMemoriaStub::default());
    let state = build_server_state(settings)
        .await
        .expect("build_server_state")
        .with_memoria_forwarder(memoria.clone());

    let app = build_app(state);

    let suffix = Uuid::new_v4().simple().to_string();
    let username = format!("prod_matrix_{suffix}");
    let email = format!("prod_matrix_{suffix}@e2e.test");
    let edge_agent_id = format!("edge-{suffix}");

    let (st_reg, reg) = post_json(
        &app,
        "/auth/register",
        None,
        json!({
            "username": username,
            "email": email,
            "password": E2E_PASSWORD,
            "display_name": "Product Matrix E2E"
        }),
    )
    .await;
    assert_eq!(st_reg, StatusCode::CREATED, "register: {reg}");
    let access = reg["access_token"].as_str().expect("access_token");
    let user_id = reg["user_id"].as_str().expect("user_id").to_string();
    let mut refresh_token = reg["refresh_token"]
        .as_str()
        .expect("refresh_token")
        .to_string();
    let mut auth_header = format!("Bearer {access}");

    let auth_row = sqlx::query("SELECT username, email FROM auth_users WHERE user_id = ?")
        .bind(&user_id)
        .fetch_optional(&pool)
        .await
        .expect("auth_users lookup");
    let auth_row = auth_row.expect("auth_users row after register");
    assert_eq!(
        auth_row.try_get::<String, _>("username").ok().as_deref(),
        Some(username.as_str())
    );

    let (st_login, login_j) = post_json(
        &app,
        "/auth/login",
        None,
        json!({ "username": username, "password": E2E_PASSWORD }),
    )
    .await;
    assert_eq!(st_login, StatusCode::OK, "login: {login_j}");

    let (st_me, me) = get_json(&app, "/auth/me", Some(&auth_header), &[]).await;
    assert_eq!(st_me, StatusCode::OK, "me: {me}");
    assert_eq!(me["user_id"].as_str(), Some(user_id.as_str()));

    let (st_ref, ref_j) = post_json(
        &app,
        "/auth/refresh",
        None,
        json!({ "refresh_token": refresh_token }),
    )
    .await;
    assert_eq!(st_ref, StatusCode::OK, "refresh: {ref_j}");
    let access2 = ref_j["access_token"]
        .as_str()
        .expect("post-refresh access_token");
    refresh_token = ref_j["refresh_token"]
        .as_str()
        .expect("post-refresh refresh_token")
        .to_string();
    auth_header = format!("Bearer {access2}");

    let (st_me2, me2) = get_json(&app, "/auth/me", Some(&auth_header), &[]).await;
    assert_eq!(st_me2, StatusCode::OK, "me after refresh: {me2}");
    assert_eq!(me2["user_id"].as_str(), Some(user_id.as_str()));

    let (st_learn_h, learn_h) = get_json(&app, "/api/v1/learning/health", None, &[]).await;
    assert_eq!(st_learn_h, StatusCode::OK, "learning health: {learn_h}");

    let (st_sess, sess) = post_json(
        &app,
        "/sessions",
        Some(&auth_header),
        json!({ "title": "product matrix session", "metadata": { "suite": "matrix" } }),
    )
    .await;
    assert_eq!(st_sess, StatusCode::CREATED, "create session: {sess}");
    let session_id = sess["session_id"].as_str().expect("session_id").to_string();

    // Do not call `cleanup_session_data` here — it would delete the row we just created via POST
    // /sessions, breaking list/get/cancel and the full product journey.
    cleanup_edge_registry(&pool, &user_id, &edge_agent_id).await;

    BootstrapResult {
        ctx: MatrixE2eCtx {
            app,
            pool,
            memoria,
            user_id,
            session_id,
            edge_agent_id,
            suffix,
        },
        auth_header,
        refresh_token,
    }
}
