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
use astra_services::{
    DatabaseRunStateStore, DatabaseSessionService, DurableRunRecord, RunStateStore, SessionService,
};
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

pub fn require_system_e2e_env() {
    assert_eq!(
        std::env::var("ASTRA_TEST_DB_IT").as_deref(),
        Ok("1"),
        "set ASTRA_TEST_DB_IT=1 to run this ignored test"
    );
    E2E_ENV_INIT.get_or_init(|| {
        let secret = std::env::var("ASTRA_TEST_BRIDGE_SECRET")
            .unwrap_or_else(|_| "system-matrix-e2e-secret".to_string());
        // SAFETY: These tests MUST run under nextest (per-process isolation).
        // Under `cargo test` with shared threads this is technically UB on
        // Rust 2024 edition. OnceLock guarantees this block runs exactly once
        // before any test logic reads the env vars.
        unsafe {
            std::env::set_var("ASTRA_TEST_BRIDGE_SECRET", &secret);
            if std::env::var_os("ASTRA_LLM_RETRY_BASE_MS").is_none() {
                std::env::set_var("ASTRA_LLM_RETRY_BASE_MS", "10");
            }
            if std::env::var_os("ASTRA_DEFAULT_RETRY_AFTER_MS").is_none() {
                std::env::set_var("ASTRA_DEFAULT_RETRY_AFTER_MS", "10");
            }
            if std::env::var_os("ASTRA_BCRYPT_COST").is_none() {
                std::env::set_var("ASTRA_BCRYPT_COST", "4");
            }
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

/// Normalized JWT secret matching [`astra_core::config`] / auth service.
pub fn e2e_normalized_jwt_secret() -> String {
    let secret =
        std::env::var("ASTRA_JWT_SECRET").unwrap_or_else(|_| "change-me-in-production".to_string());
    if secret.len() >= 32 {
        secret
    } else {
        let mut padded = secret;
        padded.extend(std::iter::repeat_n('0', 32 - padded.len()));
        padded
    }
}

/// Build a local-JWT access token for negative-path tests (expired / custom claims).
pub fn build_e2e_access_token(user_id: &str, username: &str, exp_unix: u64) -> String {
    build_hs256_jwt(
        &e2e_normalized_jwt_secret(),
        json!({
            "sub": user_id,
            "username": username,
            "token_type": "access",
            "exp": exp_unix,
            "iat": exp_unix.saturating_sub(3600),
            "jti": Uuid::new_v4().to_string()
        }),
    )
}

async fn build_state(memoria: Arc<E2eMemoriaStub>) -> (astra_runtime::AppState, String, String) {
    let settings = AppSettings::from_env().expect("AppSettings::from_env (see astra-server env)");
    let matrixone_database = settings.matrixone.database.clone();
    let url = settings.matrixone.database_url_with_password();
    let state = build_server_state(settings).await;

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
    /// When true, [`MemoriaForwarder::forward`] returns an error (simulates Memoria outage).
    pub fail_forward: Arc<std::sync::atomic::AtomicBool>,
}

impl E2eMemoriaStub {
    pub fn set_fail_forward(&self, fail: bool) {
        use std::sync::atomic::Ordering;
        self.fail_forward.store(fail, Ordering::Relaxed);
    }
}

#[async_trait]
impl MemoriaForwarder for E2eMemoriaStub {
    async fn forward(
        &self,
        _method: reqwest::Method,
        endpoint: &str,
        body: Value,
    ) -> Result<Value, String> {
        use std::sync::atomic::Ordering;
        if self.fail_forward.load(Ordering::Relaxed) {
            return Err(format!("memoria unavailable (e2e stub): {endpoint}"));
        }
        let response = if endpoint.ends_with("/retrieve") {
            json!({ "memories": [] })
        } else if endpoint.ends_with("/purge") {
            let purged = body
                .get("memory_ids")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            json!({ "purged": purged })
        } else {
            json!({ "memory_id": "e2e-stub-memory" })
        };
        self.calls.lock().await.push((endpoint.to_string(), body));
        Ok(response)
    }

    async fn health(&self) -> astra_runtime::MemoriaHealth {
        use std::sync::atomic::Ordering;
        if self.fail_forward.load(Ordering::Relaxed) {
            astra_runtime::MemoriaHealth::Unavailable("e2e stub unavailable".to_string())
        } else {
            astra_runtime::MemoriaHealth::Connected
        }
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
        return (status, String::from_utf8_lossy(&bytes).into_owned());
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
    (status, String::from_utf8_lossy(&acc).into_owned())
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

pub fn tool_result_payload(parts: astra_thin_client::ToolResultRequestParts) -> Value {
    serde_json::to_value(astra_thin_client::ToolResultRequest::new_with_hash(parts))
        .expect("tool result payload serializes")
}

pub fn maybe_tool_result_payload_from_sse(
    raw_sse: &str,
    request_id: &str,
    edge_agent_id: &str,
    status: &str,
    output: &str,
    duration_ms: u64,
) -> Option<Value> {
    let event = raw_sse
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .find(|event| {
            event.get("type").and_then(Value::as_str) == Some("tool_request")
                && event.get("request_id").and_then(Value::as_str) == Some(request_id)
        })?;
    let field = |name: &str| {
        event
            .get(name)
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("tool_request {request_id} missing {name}"))
    };
    Some(tool_result_payload(
        astra_thin_client::ToolResultRequestParts {
            session_id: field("session_id").to_string(),
            run_id: field("run_id").to_string(),
            turn_chain_id: field("turn_chain_id").to_string(),
            request_id: request_id.to_string(),
            edge_agent_id: edge_agent_id.to_string(),
            status: status.to_string(),
            output: output.to_string(),
            duration_ms,
            tool_result_fields: None,
        },
    ))
}

pub fn model_selection(offering_id: impl Into<String>) -> Value {
    json!({ "offering_id": offering_id.into() })
}

pub fn seeded_model_selection(ctx: &MatrixE2eCtx) -> Value {
    model_selection(ctx.model_offering_id.clone())
}

pub fn offering_id_from_model_response(response: &Value) -> &str {
    response["model_id"]
        .as_str()
        .expect("model creation response must contain model_id")
}

pub async fn cleanup_session_data(shared_pool: &SharedPool, user_id: &str, session_id: &str) {
    let service =
        DatabaseSessionService::new(shared_pool.settings().clone()).with_pool(shared_pool.clone());
    match service
        .delete_session(session_id.to_string(), user_id.to_string())
        .await
    {
        Ok(()) => {}
        Err((StatusCode::NOT_FOUND, _)) => {}
        Err((status, body)) => {
            panic!(
                "cleanup_session_data failed for user_id={user_id} session_id={session_id}: status={status} body={:?}",
                body.0
            );
        }
    }
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
    let _ = sqlx::query("DELETE FROM agent_tasks WHERE user_id = ? AND task_id = ?")
        .bind(user_id)
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
    user_id: &str,
    session_id: &str,
    types: &[&str],
    timeout: std::time::Duration,
) {
    let expected_counts = types
        .iter()
        .map(|event_type| (*event_type, 1_i64))
        .collect::<Vec<_>>();
    wait_for_agent_event_type_counts(pool, user_id, session_id, &expected_counts, timeout).await;
}

/// Poll until every `event_type` reaches its expected row count for this session, or `timeout`.
///
/// Bridge turns persist some observability events asynchronously after the SSE response completes.
/// Callers that assert a session-wide projection must wait for every expected turn event, rather
/// than merely observing one row of each type.
pub async fn wait_for_agent_event_type_counts(
    pool: &sqlx::MySqlPool,
    user_id: &str,
    session_id: &str,
    expected_counts: &[(&str, i64)],
    timeout: std::time::Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let mut ok = true;
        for (event_type, expected_count) in expected_counts {
            let n: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM agent_events \
                 WHERE session_id = ? AND user_id = ? AND event_type = ?",
            )
            .bind(session_id)
            .bind(user_id)
            .bind(*event_type)
            .fetch_one(pool)
            .await
            .unwrap_or(0);
            if n < *expected_count {
                ok = false;
                break;
            }
        }
        if ok {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "timeout ({timeout:?}) waiting for agent_events counts {expected_counts:?} for user_id={user_id} session_id={session_id}"
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Poll until a run reaches the specified status, or timeout.
/// Returns the final status observed.
pub async fn wait_for_run_status(
    app: &Router,
    run_id: &str,
    auth: &str,
    target_status: &str,
    timeout: std::time::Duration,
) -> String {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let (st, body) = get_json(app, &format!("/chat/runs/{run_id}"), Some(auth), &[]).await;
        if st == StatusCode::OK {
            let status = body["status"].as_str().unwrap_or("unknown");
            if status == target_status {
                return status.to_string();
            }
            // If we hit a terminal state that's not our target, bail early
            if matches!(status, "completed" | "delegated" | "failed" | "cancelled")
                && status != target_status
            {
                return status.to_string();
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "timeout ({timeout:?}) waiting for run {run_id} to reach status '{target_status}'"
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Shared Matrix E2E context after app build + user registration + session creation.
pub struct MatrixE2eCtx {
    pub app: Router,
    pub app_state: astra_runtime::AppState,
    pub pool: sqlx::MySqlPool,
    pub shared_pool: SharedPool,
    /// Logical MatrixOne database (includes `ASTRA_DATABASE_PREFIX` + `ASTRA_DATABASE`).
    pub matrixone_database: String,
    pub memoria: Arc<E2eMemoriaStub>,
    pub user_id: String,
    /// Registered username (for duplicate-register negative tests).
    pub username: String,
    pub session_id: String,
    pub edge_agent_id: String,
    pub model_offering_id: String,
    pub suffix: String,
}

pub struct BootstrapResult {
    pub ctx: MatrixE2eCtx,
    pub auth_header: String,
    pub refresh_token: String,
}

/// Seed the durable precondition for `/approval/respond` through the same run
/// store used by production. A callback is not a free-standing journal write:
/// it must resolve an existing owner-scoped run interaction.
pub async fn seed_pending_approval(
    ctx: &MatrixE2eCtx,
    run_id: &str,
    request_id: &str,
    tool: &str,
    approval_kind: &str,
) {
    let now = chrono::Utc::now().to_rfc3339();
    let store = DatabaseRunStateStore::new(ctx.shared_pool.clone());
    store
        .insert_run(DurableRunRecord {
            run_id: run_id.to_string(),
            user_id: ctx.user_id.clone(),
            session_id: ctx.session_id.clone(),
            parent_run_id: None,
            root_run_id: Some(run_id.to_string()),
            ancestor_path: Some(run_id.to_string()),
            depth: 0,
            delegation_id: None,
            agent_id: Some("system-matrix-approval-fixture".to_string()),
            retry_of: None,
            retry_scope: Some("node".to_string()),
            status: "waiting".to_string(),
            waiting_for: Some("tool_approval".to_string()),
            owner_pod_id: None,
            owner_lease_expires_at: None,
            run_generation: 0,
            last_event_idx: -1,
            checkpoint_version: None,
            checkpoint_json: None,
            error_code: None,
            error_message: None,
            retry_count: 0,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_tool_calls: 0,
            agent_binding_id: None,
            agent_binding_name: None,
            agent_binding_schema_version: None,
            model_offering_id: None,
            resolved_model_name: None,
            runtime_profile: None,
            provider_request_fingerprint: None,
            events: vec![
                json!({"event_type": "run_started", "data": {}}),
                json!({
                    "event_type": "approval_required",
                    "data": {
                        "request_id": request_id,
                        "tool": tool,
                        "approval_kind": approval_kind,
                        "delivery": "durable",
                    }
                }),
            ],
            created_at: now.clone(),
            updated_at: now,
        })
        .await
        .expect("seed durable pending approval");
}

pub async fn load_durable_interaction_event(
    ctx: &MatrixE2eCtx,
    run_id: &str,
    request_id: &str,
    event_type: &str,
) -> Value {
    DatabaseRunStateStore::new(ctx.shared_pool.clone())
        .load_run_interaction_event(&ctx.user_id, run_id, request_id, event_type)
        .await
        .expect("load durable interaction event")
        .unwrap_or_else(|| {
            panic!(
                "missing durable interaction event type={event_type} run={run_id} request={request_id}"
            )
        })
}

pub async fn durable_interaction_event_count(
    ctx: &MatrixE2eCtx,
    run_id: &str,
    request_id: &str,
    event_type: &str,
) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_run_events \
         WHERE user_id = ? AND run_id = ? AND interaction_request_id = ? AND event_type = ?",
    )
    .bind(&ctx.user_id)
    .bind(run_id)
    .bind(request_id)
    .bind(event_type)
    .fetch_one(&ctx.pool)
    .await
    .expect("count durable interaction events")
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
    // Use low bcrypt cost in tests to avoid multi-second hashing in debug builds.
    // SAFETY: set before any code reads this variable; the tokio runtime has
    // worker threads but none are reading ASTRA_BCRYPT_COST yet.
    unsafe { std::env::set_var("ASTRA_BCRYPT_COST", "4") };
    let memoria = Arc::new(E2eMemoriaStub::default());
    let (state, matrixone_database, url) = build_state(memoria.clone()).await;

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
    let session_lifecycle_pool = evaluation_pool.clone();
    let memoria_health_base_url = start_mock_memoria_health().await;
    let state = state.with_evaluation_service(Arc::new(
        DatabaseEvaluationService::new(matrixone_settings)
            .with_pool(evaluation_pool)
            .with_memoria_config(
                memoria_health_base_url,
                Some("system-e2e-mock-master-key".to_string()),
            ),
    ));

    // `build_app` is used in-process here, so the real server startup hook that
    // primes capability health does not run. Mirror that production boundary
    // once; request handlers must continue to consume the cache without remote
    // dependency I/O.
    let _ = state
        .refresh_memoria_health_if_stale(std::time::Duration::ZERO)
        .await;
    let app_state = state.clone();
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

    let (st_memory_h, memory_h) =
        get_json(&app, "/memory/health", Some(auth_header.as_str()), &[]).await;
    assert_eq!(st_memory_h, StatusCode::OK, "memory health: {memory_h}");

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
    let (st_mdl, model) = post_json(
        &app,
        "/models",
        Some(&auth_header),
        json!({
            "name": mock_model,
            "provider": "mock",
            "context_window": 200000,
            "api_key": "unused",
            "base_url": "http://127.0.0.1:1",
            "context_window": 200000
        }),
    )
    .await;
    assert_eq!(st_mdl, StatusCode::CREATED, "seed mock model: {model}");
    let model_offering_id = offering_id_from_model_response(&model).to_string();

    BootstrapResult {
        ctx: MatrixE2eCtx {
            app,
            app_state,
            pool,
            shared_pool: session_lifecycle_pool,
            matrixone_database,
            memoria,
            user_id,
            username,
            session_id,
            edge_agent_id,
            model_offering_id,
            suffix,
        },
        auth_header,
        refresh_token,
    }
}
