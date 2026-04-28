//! Shared harness for **system** Matrix HTTP E2E (Phase A: env gate, HTTP helpers, `sqlx` cleanup /
//! row assertions, [`bootstrap`] / [`MatrixE2eCtx`]).
//!
//! This module is the extracted equivalent of the historical monolithic
//! `tests/system_matrix_http_e2e.rs` setup; journey logic lives in `journey_*.rs`. See
//! `docs/testing/system-e2e-matrix.md` for the capability ↔ route ↔ test mapping.

use std::sync::Arc;
use std::sync::OnceLock;

use astra_core::SharedPool;
use astra_core::config::AppSettings;
use astra_runtime::{DatabaseEvaluationService, MemoriaForwarder, build_app, build_server_state};
use async_trait::async_trait;
use axum::{
    Json, Router,
    body::{self, Body},
    http::{Request, StatusCode},
    routing::get,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::StreamExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::Row;
use sqlx::mysql::MySqlRow;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tower::util::ServiceExt;
use uuid::Uuid;

static E2E_ENV_INIT: OnceLock<()> = OnceLock::new();
static SERVER_STATE_ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

const TRUSTED_MOI_E2E_SECRET: &str = "trusted_moi_system_e2e_secret_key_123456";
const TRUSTED_MOI_E2E_EXP: u64 = 4_102_444_800; // 2100-01-01T00:00:00Z

fn server_state_env_lock() -> &'static Mutex<()> {
    SERVER_STATE_ENV_LOCK.get_or_init(|| Mutex::new(()))
}

pub fn require_system_e2e_env() {
    assert_eq!(
        std::env::var("ASTRA_DB_IT").as_deref(),
        Ok("1"),
        "set ASTRA_DB_IT=1 to run this ignored test"
    );
    E2E_ENV_INIT.get_or_init(|| {
        let secret = std::env::var("ASTRA_BRIDGE_TEST_SECRET")
            .unwrap_or_else(|_| "system-matrix-e2e-secret".to_string());
        // SAFETY: set once before parallel test threads (idempotent for all E2E tests).
        unsafe {
            std::env::set_var("ASTRA_BRIDGE_TEST_SECRET", &secret);
        }
    });
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK_SIZE: usize = 64;
    let mut normalized_key = [0u8; BLOCK_SIZE];
    if key.len() > BLOCK_SIZE {
        let digest = Sha256::digest(key);
        normalized_key[..32].copy_from_slice(&digest);
    } else {
        normalized_key[..key.len()].copy_from_slice(key);
    }

    let mut inner_pad = [0x36u8; BLOCK_SIZE];
    let mut outer_pad = [0x5cu8; BLOCK_SIZE];
    for i in 0..BLOCK_SIZE {
        inner_pad[i] ^= normalized_key[i];
        outer_pad[i] ^= normalized_key[i];
    }

    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(message);
    let inner_digest = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner_digest);
    let digest = outer.finalize();

    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

fn build_hs256_jwt(secret: &str, claims: Value) -> String {
    let header = json!({
        "alg": "HS256",
        "typ": "JWT"
    });
    let header_b64 = URL_SAFE_NO_PAD.encode(header.to_string().as_bytes());
    let claims_b64 = URL_SAFE_NO_PAD.encode(claims.to_string().as_bytes());
    let signing_input = format!("{header_b64}.{claims_b64}");
    let signature = hmac_sha256(secret.as_bytes(), signing_input.as_bytes());
    let signature_b64 = URL_SAFE_NO_PAD.encode(signature);
    format!("{signing_input}.{signature_b64}")
}

fn set_env_var_for_e2e(name: &str, value: &str) {
    // SAFETY: this is guarded by `SERVER_STATE_ENV_LOCK`, so no concurrent env mutation/read among
    // system E2E bootstrap paths.
    unsafe {
        std::env::set_var(name, value);
    }
}

fn restore_env_var_for_e2e(name: &str, old_value: Option<String>) {
    // SAFETY: this is guarded by `SERVER_STATE_ENV_LOCK`, so no concurrent env mutation/read among
    // system E2E bootstrap paths.
    unsafe {
        if let Some(v) = old_value {
            std::env::set_var(name, v);
        } else {
            std::env::remove_var(name);
        }
    }
}

async fn build_state_with_mode(
    memoria: Arc<E2eMemoriaStub>,
    trusted_moi_mode: bool,
) -> (astra_runtime::AppState, String, String) {
    let _env_guard = server_state_env_lock().lock().await;
    dotenvy::dotenv().ok();

    let prev_auth_mode = std::env::var("ASTRA_AUTH_MODE").ok();
    let prev_trusted_secret = std::env::var("TRUSTED_MOI_JWT_SECRET_KEY").ok();
    let prev_trusted_algorithm = std::env::var("TRUSTED_MOI_JWT_ALGORITHM").ok();
    let prev_trusted_issuer = std::env::var("TRUSTED_MOI_JWT_ISSUER").ok();
    let prev_trusted_audience = std::env::var("TRUSTED_MOI_JWT_AUDIENCE").ok();
    let prev_trusted_leeway = std::env::var("TRUSTED_MOI_JWT_LEEWAY_SECS").ok();

    if trusted_moi_mode {
        set_env_var_for_e2e("ASTRA_AUTH_MODE", "trusted_moi");
        set_env_var_for_e2e("TRUSTED_MOI_JWT_SECRET_KEY", TRUSTED_MOI_E2E_SECRET);
        set_env_var_for_e2e("TRUSTED_MOI_JWT_ALGORITHM", "HS256");
        restore_env_var_for_e2e("TRUSTED_MOI_JWT_ISSUER", None);
        restore_env_var_for_e2e("TRUSTED_MOI_JWT_AUDIENCE", None);
        set_env_var_for_e2e("TRUSTED_MOI_JWT_LEEWAY_SECS", "30");
    } else {
        // Force local mode for this bootstrap path so service auth mode is deterministic.
        set_env_var_for_e2e("ASTRA_AUTH_MODE", "local_jwt");
    }

    let settings = AppSettings::from_env().expect("AppSettings::from_env (see astra-server env)");
    let matrixone_database = settings.matrixone.database.clone();
    let url = settings.matrixone.database_url_with_password();
    let state = build_server_state(settings).await;

    restore_env_var_for_e2e("ASTRA_AUTH_MODE", prev_auth_mode);
    restore_env_var_for_e2e("TRUSTED_MOI_JWT_SECRET_KEY", prev_trusted_secret);
    restore_env_var_for_e2e("TRUSTED_MOI_JWT_ALGORITHM", prev_trusted_algorithm);
    restore_env_var_for_e2e("TRUSTED_MOI_JWT_ISSUER", prev_trusted_issuer);
    restore_env_var_for_e2e("TRUSTED_MOI_JWT_AUDIENCE", prev_trusted_audience);
    restore_env_var_for_e2e("TRUSTED_MOI_JWT_LEEWAY_SECS", prev_trusted_leeway);

    (
        state
            .expect("build_server_state")
            .with_memoria_forwarder(memoria),
        matrixone_database,
        url,
    )
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

async fn start_mock_memoria_health() -> String {
    let app = Router::new()
        .route(
            "/v1/health/storage",
            get(|| async move {
                Json(json!({
                    "total": 12,
                    "active": 9,
                    "inactive": 3
                }))
            }),
        )
        .route(
            "/v1/health/analyze",
            get(|| async move {
                Json(json!({
                    "semantic": {
                        "total": 4,
                        "avg_confidence": 0.8
                    },
                    "profile": {
                        "total": 8,
                        "avg_confidence": 0.6
                    }
                }))
            }),
        )
        .route(
            "/v1/health/hygiene",
            get(|| async move {
                Json(json!({
                    "inactive_memories": 0,
                    "stale_working_memories": 2,
                    "orphan_memory_entity_links": 0,
                    "orphan_entity_links": 0,
                    "orphan_graph_nodes": 0
                }))
            }),
        );
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock memoria health");
    let addr = listener.local_addr().expect("mock memoria local addr");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve mock memoria health");
    });
    tokio::task::yield_now().await;
    format!("http://{addr}")
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
pub async fn collect_sse_body_text(
    app: &Router,
    req: Request<Body>,
    max_bytes: usize,
) -> (StatusCode, String) {
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
    let mut matched = false;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while let Ok(Some(chunk)) = tokio::time::timeout_at(deadline, stream.next()).await {
        let chunk = chunk.expect("body chunk");
        acc.extend_from_slice(&chunk);
        if acc.len() >= max_bytes {
            matched = true;
            break;
        }
        let preview = String::from_utf8_lossy(&acc);
        if preview.contains("\"run_id\"") && preview.contains("session_info") {
            matched = true;
            break;
        }
        if preview.contains("\"type\":\"error\"") {
            matched = true;
            break;
        }
    }
    if !matched {
        let preview = String::from_utf8_lossy(&acc);
        panic!(
            "SSE stream timed out (5s) without expected events (run_id/session_info/error). \
             Collected {} bytes, preview: {}",
            acc.len(),
            &preview[..preview.len().min(500)]
        );
    }
    (status, String::from_utf8_lossy(&acc).to_string())
}

/// Parse SSE `data: {...}` blocks; return the first JSON object whose `type` matches.
#[allow(dead_code)]
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
    let _ = sqlx::query(
        "DELETE edge FROM agent_event_edges edge \
         JOIN agent_events ev ON edge.child_event_id = ev.event_id \
         WHERE ev.session_id = ?",
    )
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
    /// Logical MatrixOne database (includes `ASTRA_DATABASE_PREFIX` + `ASTRA_DATABASE`).
    pub matrixone_database: String,
    pub memoria: Arc<E2eMemoriaStub>,
    pub user_id: String,
    /// Registered username (for duplicate-register negative tests).
    pub username: String,
    pub session_id: String,
    pub edge_agent_id: String,
    pub suffix: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum E2eAuthMode {
    LocalJwt,
    TrustedMoi,
}

pub fn current_auth_mode() -> E2eAuthMode {
    dotenvy::dotenv().ok();
    let mode = std::env::var("ASTRA_AUTH_MODE")
        .unwrap_or_else(|_| "local_jwt".to_string())
        .trim()
        .to_ascii_lowercase();
    match mode.as_str() {
        "" | "local_jwt" | "local" | "database" => E2eAuthMode::LocalJwt,
        "trusted_moi" => E2eAuthMode::TrustedMoi,
        other => panic!("unsupported ASTRA_AUTH_MODE in e2e: {other}"),
    }
}

pub struct BootstrapResult {
    pub ctx: MatrixE2eCtx,
    pub auth_header: String,
    pub refresh_token: String,
    pub auth_mode: E2eAuthMode,
}

pub struct TrustedMoiBootstrapResult {
    pub ctx: MatrixE2eCtx,
    pub auth_header: String,
}

pub const E2E_PASSWORD: &str = "E2e-matrix-pass-9";

/// Ensure `user_id` has the `astra_admin` role (idempotent). Used for admin-only HTTP paths in E2E.
pub async fn grant_astra_admin_role(pool: &sqlx::MySqlPool, user_id: &str) {
    sqlx::query(
        "INSERT IGNORE INTO auth_user_roles (user_id, role_id) \
         SELECT ?, r.role_id FROM auth_roles r WHERE r.role_name = 'astra_admin' LIMIT 1",
    )
    .bind(user_id)
    .execute(pool)
    .await
    .expect("grant_astra_admin_role insert");

    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM auth_user_roles ur \
         INNER JOIN auth_roles r ON ur.role_id = r.role_id \
         WHERE ur.user_id = ? AND r.role_name = 'astra_admin'",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .expect("grant_astra_admin_role verify");
    assert!(
        n >= 1,
        "user {user_id} should have astra_admin (auth_roles seeded?)"
    );
}

/// Remove the `astra_admin` role from `user_id` if present (admin smoke tests start non-admin).
pub async fn revoke_astra_admin_role(pool: &sqlx::MySqlPool, user_id: &str) {
    sqlx::query(
        "DELETE ur FROM auth_user_roles ur \
         INNER JOIN auth_roles r ON ur.role_id = r.role_id \
         WHERE ur.user_id = ? AND r.role_name = 'astra_admin'",
    )
    .bind(user_id)
    .execute(pool)
    .await
    .expect("revoke_astra_admin_role delete");
}

/// Build app, connect pool, register user, refresh token, create session (with cleanup of stale rows).
pub async fn bootstrap() -> BootstrapResult {
    match current_auth_mode() {
        E2eAuthMode::LocalJwt => bootstrap_local_jwt().await,
        E2eAuthMode::TrustedMoi => {
            let trusted = bootstrap_trusted_moi().await;
            BootstrapResult {
                ctx: trusted.ctx,
                auth_header: trusted.auth_header,
                refresh_token: String::new(),
                auth_mode: E2eAuthMode::TrustedMoi,
            }
        }
    }
}

async fn bootstrap_local_jwt() -> BootstrapResult {
    // Use low bcrypt cost in tests to avoid multi-second hashing in debug builds.
    // SAFETY: set before any code reads this variable; the tokio runtime has
    // worker threads but none are reading ASTRA_BCRYPT_COST yet.
    unsafe { std::env::set_var("ASTRA_BCRYPT_COST", "4") };
    let memoria = Arc::new(E2eMemoriaStub::default());
    let (state, matrixone_database, url) = build_state_with_mode(memoria.clone(), false).await;

    let pool = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect MatrixOne for assertions");

    let matrixone_settings = AppSettings::from_env()
        .expect("AppSettings::from_env (see astra-server env)")
        .matrixone;
    let evaluation_pool = SharedPool::new(&matrixone_settings)
        .await
        .expect("connect MatrixOne shared pool for evaluation");
    let memoria_health_base_url = start_mock_memoria_health().await;
    let state = state.with_evaluation_service(Arc::new(
        DatabaseEvaluationService::new(matrixone_settings)
            .with_pool(evaluation_pool)
            .with_memoria_config(
                memoria_health_base_url,
                Some("system-e2e-mock-master-key".to_string()),
            ),
    ));

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

    // Register a mock model so run-lifecycle tests don't need a real LLM.
    grant_astra_admin_role(&pool, &user_id).await;
    let mock_model = format!("mock-{suffix}");
    let (st_mdl, _) = post_json(
        &app,
        "/models",
        Some(&auth_header),
        json!({
            "name": mock_model,
            "provider": "mock",
            "api_key": "unused",
            "base_url": "http://127.0.0.1:1"
        }),
    )
    .await;
    assert_eq!(st_mdl, StatusCode::CREATED, "seed mock model");

    BootstrapResult {
        ctx: MatrixE2eCtx {
            app,
            pool,
            matrixone_database,
            memoria,
            user_id,
            username,
            session_id,
            edge_agent_id,
            suffix,
        },
        auth_header,
        refresh_token,
        auth_mode: E2eAuthMode::LocalJwt,
    }
}

/// Build app in `trusted_moi` mode and bootstrap an external user token/session.
pub async fn bootstrap_trusted_moi() -> TrustedMoiBootstrapResult {
    let memoria = Arc::new(E2eMemoriaStub::default());
    let (state, matrixone_database, url) = build_state_with_mode(memoria.clone(), true).await;

    let pool = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect MatrixOne for assertions");
    let matrixone_settings = AppSettings::from_env()
        .expect("AppSettings::from_env (see astra-server env)")
        .matrixone;
    let evaluation_pool = SharedPool::new(&matrixone_settings)
        .await
        .expect("connect MatrixOne shared pool for evaluation");
    let memoria_health_base_url = start_mock_memoria_health().await;
    let state = state.with_evaluation_service(Arc::new(
        DatabaseEvaluationService::new(matrixone_settings)
            .with_pool(evaluation_pool)
            .with_memoria_config(
                memoria_health_base_url,
                Some("system-e2e-mock-master-key".to_string()),
            ),
    ));
    let app = build_app(state);

    let suffix = Uuid::new_v4().simple().to_string();
    let username = format!("moi_matrix_{suffix}");
    let email = format!("moi_matrix_{suffix}@e2e.test");
    // Keep external user id within 36 chars so it can fit service tables that use VARCHAR(36).
    let user_id = format!("moi_{}", &suffix[..28]);
    let edge_agent_id = format!("edge-{suffix}");

    let token = build_hs256_jwt(
        TRUSTED_MOI_E2E_SECRET,
        json!({
            "sub": user_id,
            "username": username,
            "email": email,
            "name": "Trusted Moi E2E",
            "exp": TRUSTED_MOI_E2E_EXP
        }),
    );
    let auth_header = format!("Bearer {token}");

    let (st_me, me) = get_json(&app, "/auth/me", Some(auth_header.as_str()), &[]).await;
    assert_eq!(st_me, StatusCode::OK, "trusted_moi me: {me}");
    let user_id = me["user_id"]
        .as_str()
        .expect("trusted_moi /auth/me user_id")
        .to_string();
    let username = me["username"]
        .as_str()
        .expect("trusted_moi /auth/me username")
        .to_string();

    let (st_sess, sess) = post_json(
        &app,
        "/sessions",
        Some(auth_header.as_str()),
        json!({ "title": "trusted moi matrix session", "metadata": { "suite": "trusted_moi" } }),
    )
    .await;
    assert_eq!(
        st_sess,
        StatusCode::CREATED,
        "trusted_moi create session: {sess}"
    );
    let session_id = sess["session_id"].as_str().expect("session_id").to_string();

    let row = sqlx::query("SELECT user_id FROM agent_sessions WHERE session_id = ?")
        .bind(&session_id)
        .fetch_optional(&pool)
        .await
        .expect("trusted_moi session owner select");
    let row = row.expect("trusted_moi session row");
    assert_eq!(
        row.try_get::<String, _>("user_id").ok().as_deref(),
        Some(user_id.as_str()),
        "trusted_moi session should be owned by external user id"
    );

    cleanup_edge_registry(&pool, &user_id, &edge_agent_id).await;

    let (st_learn_h, learn_h) = get_json(&app, "/api/v1/learning/health", None, &[]).await;
    assert_eq!(
        st_learn_h,
        StatusCode::OK,
        "trusted_moi learning health: {learn_h}"
    );

    // Seed a mock model directly in DB so run-lifecycle tests don't depend on admin auth mode.
    let mock_model = format!("mock-{suffix}");
    let model_id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO infra_llm_models (model_id, model_name, provider, base_url, is_active) \
         VALUES (?, ?, 'mock', 'http://127.0.0.1:1', 1)",
    )
    .bind(&model_id)
    .bind(&mock_model)
    .execute(&pool)
    .await
    .expect("trusted_moi seed mock model");

    TrustedMoiBootstrapResult {
        ctx: MatrixE2eCtx {
            app,
            pool,
            matrixone_database,
            memoria,
            user_id,
            username,
            session_id,
            edge_agent_id,
            suffix,
        },
        auth_header,
    }
}
