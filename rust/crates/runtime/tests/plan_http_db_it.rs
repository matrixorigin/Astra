//! Live HTTP integration tests for the plan REST surface.
//!
//! Exercises the real Axum router + real MatrixOne via `CloudPlanRepository`,
//! with `StubAuthService` supplying `user_id="test-user"` on any bearer token.
//!
//! Each test:
//!   - Builds an `AppState` with the real DB-backed plan repo
//!   - Mounts the full router via `build_app`
//!   - Drives requests with `tower::ServiceExt::oneshot`
//!   - Asserts both the JSON response body **and** actual rows in MatrixOne
//!
//! ```text
//! ASTRA_DB_IT=1 cargo test -p astra-runtime --test plan_http_db_it -- --ignored
//! ```

use std::sync::Arc;

use astra_core::{DEV_MATRIXONE_PASSWORD, MatrixOneSettings, SharedPool, resolve_database_name};
use astra_plan::CloudPlanRepository;
use astra_runtime::{AppState, HealthChecker, ServiceInfo, build_app};
use astra_services::ensure_core_schema;
use async_trait::async_trait;
use axum::{
    Router, body,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use sqlx::Row;
use tower::util::ServiceExt;
use uuid::Uuid;

// ── Harness ──────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct HealthyStub;

#[async_trait]
impl HealthChecker for HealthyStub {
    async fn database_healthy(&self) -> bool {
        true
    }
}

fn require_db_it_env() -> MatrixOneSettings {
    assert_eq!(
        std::env::var("ASTRA_DB_IT").as_deref(),
        Ok("1"),
        "set ASTRA_DB_IT=1 for ignored plan_http_db_it tests"
    );
    dotenvy::dotenv().ok();
    MatrixOneSettings {
        host: std::env::var("MATRIXONE_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
        port: std::env::var("MATRIXONE_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(6001),
        user: std::env::var("MATRIXONE_USER").unwrap_or_else(|_| "root".into()),
        password: std::env::var("MATRIXONE_PASSWORD")
            .unwrap_or_else(|_| DEV_MATRIXONE_PASSWORD.to_string()),
        database: resolve_database_name(&|k| std::env::var(k).ok()),
    }
}

async fn setup_app() -> (Router, sqlx::Pool<sqlx::MySql>) {
    let settings = require_db_it_env();
    ensure_core_schema(&settings)
        .await
        .expect("ensure_core_schema; is MatrixOne up?");
    let shared = SharedPool::new(&settings).await.expect("SharedPool::new");
    let pool = shared.get().clone();
    let state = AppState::new(ServiceInfo::default(), Arc::new(HealthyStub))
        .with_auth_service(Arc::new(astra_services::auth::StubAuthService))
        .with_plan_repository(Arc::new(CloudPlanRepository::new(pool.clone())));
    (build_app(state), pool)
}

fn auth_bearer() -> &'static str {
    // StubAuthService accepts any bearer and returns user_id="test-user".
    "test-token"
}

async fn request_json(
    app: Router,
    method: &str,
    path: &str,
    body_json: Option<Value>,
) -> (StatusCode, Value) {
    let has_body = body_json.is_some();
    let body = match body_json {
        Some(v) => body::Body::from(v.to_string()),
        None => body::Body::empty(),
    };
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {}", auth_bearer()));
    if has_body {
        builder = builder.header("content-type", "application/json");
    }
    let req = builder.body(body).expect("build request");
    let resp = app.oneshot(req).await.expect("oneshot");
    let status = resp.status();
    let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            panic!(
                "parse JSON response body failed: {e} — raw: {}",
                String::from_utf8_lossy(&bytes)
            )
        })
    };
    (status, json)
}

async fn cleanup_plan(pool: &sqlx::Pool<sqlx::MySql>, plan_id: &str) {
    let _ = sqlx::query("DELETE FROM plan_step_runs WHERE plan_id = ?")
        .bind(plan_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM plans WHERE plan_id = ?")
        .bind(plan_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("UPDATE agent_sessions SET active_plan_id = NULL WHERE active_plan_id = ?")
        .bind(plan_id)
        .execute(pool)
        .await;
}

async fn ensure_session(pool: &sqlx::Pool<sqlx::MySql>, session_id: &str) {
    let _ = sqlx::query(
        "INSERT IGNORE INTO agent_sessions \
             (session_id, user_id, status, event_count, created_at, updated_at, last_active_at) \
         VALUES (?, 'test-user', 'active', 0, NOW(6), NOW(6), NOW(6))",
    )
    .bind(session_id)
    .execute(pool)
    .await;
}

async fn seed_plan_with_subtasks(app: &Router, goal: &str, subtasks: &[&str]) -> (String, u64) {
    let (status, body) =
        request_json(app.clone(), "POST", "/plans", Some(json!({ "goal": goal }))).await;
    assert_eq!(status, StatusCode::CREATED, "create: {body}");
    let plan_id = body["plan_id"].as_str().expect("plan_id").to_string();
    let _v0 = body["version"].as_u64().expect("version");

    // Load, mutate subtasks in-place via repo-style update: we can't call repo
    // directly here because we want to exercise the HTTP path. Use
    // PUT /plans/{id} to trigger an edit-save so the version bumps, then
    // update subtasks directly in the DB for a deterministic fixture.
    // (Astra's decompose LLM path is out of scope for these invariants tests.)
    let (status, get_body) =
        request_json(app.clone(), "GET", &format!("/plans/{plan_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    let version = get_body["version"].as_u64().expect("get version");

    // Patch plan_json via direct DB UPDATE so subtasks exist for execute/rewind.
    let settings = require_db_it_env();
    let shared = SharedPool::new(&settings).await.unwrap();
    let pool = shared.get();
    let subtask_json: Vec<Value> = subtasks
        .iter()
        .map(|id| {
            json!({
                "id": *id,
                "title": format!("step {id}"),
                "description": null,
                "depends_on": [],
                "status": "pending",
            })
        })
        .collect();
    let plan_json: String = sqlx::query_scalar("SELECT plan_json FROM plans WHERE plan_id = ?")
        .bind(&plan_id)
        .fetch_one(pool)
        .await
        .unwrap();
    let mut doc: Value = serde_json::from_str(&plan_json).unwrap();
    doc["plan"]["subtasks"] = Value::Array(subtask_json);
    let new_json = doc.to_string();
    sqlx::query("UPDATE plans SET plan_json = ? WHERE plan_id = ?")
        .bind(&new_json)
        .bind(&plan_id)
        .execute(pool)
        .await
        .unwrap();

    (plan_id, version)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "ASTRA_DB_IT=1 and live MatrixOne"]
async fn post_plans_creates_row_owned_by_authenticated_user() {
    let (app, pool) = setup_app().await;

    let goal = format!("http-create-{}", Uuid::new_v4().simple());
    let (status, body) =
        request_json(app.clone(), "POST", "/plans", Some(json!({ "goal": goal }))).await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let plan_id = body["plan_id"].as_str().expect("plan_id").to_string();
    assert_eq!(body["phase"], "planning");
    assert_eq!(body["goal"], goal);

    // Row exists and is owned by the stub user.
    let row = sqlx::query("SELECT user_id, goal, phase FROM plans WHERE plan_id = ?")
        .bind(&plan_id)
        .fetch_one(&pool)
        .await
        .expect("row");
    let user_id: String = row.try_get("user_id").unwrap();
    let stored_goal: String = row.try_get("goal").unwrap();
    assert_eq!(user_id, "test-user");
    assert_eq!(stored_goal, goal);

    cleanup_plan(&pool, &plan_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_DB_IT=1 and live MatrixOne"]
async fn get_plan_invalid_id_returns_400_not_500() {
    let (app, _pool) = setup_app().await;
    let (status, body) = request_json(app, "GET", "/plans/..%2Fetc%2Fpasswd", None).await;
    // Path traversal in plan_id must be rejected by validate_plan_id → 400.
    assert!(
        status == StatusCode::BAD_REQUEST || status == StatusCode::NOT_FOUND,
        "expected 400/404, got {status}: {body}"
    );
}

#[tokio::test]
#[ignore = "ASTRA_DB_IT=1 and live MatrixOne"]
async fn get_plan_unknown_id_returns_404() {
    let (app, _pool) = setup_app().await;
    let (status, _) = request_json(app, "GET", "/plans/doesnotexist-xyz", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
#[ignore = "ASTRA_DB_IT=1 and live MatrixOne"]
async fn put_plan_with_stale_expected_version_returns_409() {
    let (app, pool) = setup_app().await;
    let (plan_id, version) = seed_plan_with_subtasks(&app, "http-ver-conflict", &["a", "b"]).await;

    // First edit with the correct version → 200.
    let (s, b) = request_json(
        app.clone(),
        "PUT",
        &format!("/plans/{plan_id}"),
        Some(json!({
            "instruction": "first edit",
            "expected_version": version
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "first edit: {b}");
    let v_after_first = b["version"].as_u64().unwrap();
    assert!(v_after_first > version);

    // Second edit with the stale version → 409.
    let (s, _b) = request_json(
        app.clone(),
        "PUT",
        &format!("/plans/{plan_id}"),
        Some(json!({
            "instruction": "stale edit",
            "expected_version": version
        })),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "stale edit must 409");

    cleanup_plan(&pool, &plan_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_DB_IT=1 and live MatrixOne"]
async fn rewind_resets_suffix_and_records_timeline_event() {
    let (app, pool) = setup_app().await;
    let (plan_id, _) = seed_plan_with_subtasks(&app, "http-rewind", &["a", "b", "c"]).await;

    // Mark all three as completed so rewind from 2 resets b+c.
    sqlx::query(
        r#"UPDATE plans
              SET plan_json = JSON_REPLACE(plan_json,
                  '$.plan.subtasks[0].status', 'completed',
                  '$.plan.subtasks[1].status', 'completed',
                  '$.plan.subtasks[2].status', 'completed')
              WHERE plan_id = ?"#,
    )
    .bind(&plan_id)
    .execute(&pool)
    .await
    .ok();
    // Fallback for MatrixOne JSON path support: direct JSON rewrite via load/save.
    let plan_json: String = sqlx::query_scalar("SELECT plan_json FROM plans WHERE plan_id = ?")
        .bind(&plan_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let mut doc: Value = serde_json::from_str(&plan_json).unwrap();
    if let Some(arr) = doc["plan"]["subtasks"].as_array_mut() {
        for st in arr.iter_mut() {
            st["status"] = Value::String("completed".into());
        }
    }
    sqlx::query("UPDATE plans SET plan_json = ? WHERE plan_id = ?")
        .bind(doc.to_string())
        .bind(&plan_id)
        .execute(&pool)
        .await
        .unwrap();

    // Now rewind to anchor=2 (1-based) → b + c reset to pending.
    let (s, body) = request_json(
        app.clone(),
        "POST",
        &format!("/plans/{plan_id}/rewind"),
        Some(json!({ "anchor": "2", "reason": "test rewind" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["reset_count"], 2);
    let subtasks = body["plan"]["subtasks"].as_array().unwrap();
    assert_eq!(subtasks[0]["status"], "completed", "a stays");
    assert_eq!(subtasks[1]["status"], "pending", "b resets");
    assert_eq!(subtasks[2]["status"], "pending", "c resets");

    // Timeline gained a SubtaskRewound event (inside plan_json).
    let plan_json: String = sqlx::query_scalar("SELECT plan_json FROM plans WHERE plan_id = ?")
        .bind(&plan_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let doc: Value = serde_json::from_str(&plan_json).unwrap();
    let timeline = doc["timeline"]["events"].as_array().expect("timeline");
    assert!(
        timeline
            .iter()
            .any(|e| e["event"]["type"] == "subtask_rewound"),
        "timeline must carry a subtask_rewound event, got {timeline:?}"
    );

    cleanup_plan(&pool, &plan_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_DB_IT=1 and live MatrixOne"]
async fn redo_step_resets_single_subtask_only() {
    let (app, pool) = setup_app().await;
    let (plan_id, _) = seed_plan_with_subtasks(&app, "http-redo", &["a", "b"]).await;

    // Mark a + b as completed.
    let plan_json: String = sqlx::query_scalar("SELECT plan_json FROM plans WHERE plan_id = ?")
        .bind(&plan_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let mut doc: Value = serde_json::from_str(&plan_json).unwrap();
    for st in doc["plan"]["subtasks"].as_array_mut().unwrap().iter_mut() {
        st["status"] = Value::String("completed".into());
    }
    sqlx::query("UPDATE plans SET plan_json = ? WHERE plan_id = ?")
        .bind(doc.to_string())
        .bind(&plan_id)
        .execute(&pool)
        .await
        .unwrap();

    let (s, body) = request_json(
        app.clone(),
        "POST",
        &format!("/plans/{plan_id}/redo-step"),
        Some(json!({ "subtask_id": "a" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["subtask_id"], "a");
    assert_eq!(body["attempt"], 1, "first redo is attempt 1");

    // Verify DB: a is back to pending, b is still completed.
    let plan_json: String = sqlx::query_scalar("SELECT plan_json FROM plans WHERE plan_id = ?")
        .bind(&plan_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let doc: Value = serde_json::from_str(&plan_json).unwrap();
    let subtasks = doc["plan"]["subtasks"].as_array().unwrap();
    assert_eq!(subtasks[0]["status"], "pending", "a reset");
    assert_eq!(subtasks[1]["status"], "completed", "b untouched");

    cleanup_plan(&pool, &plan_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_DB_IT=1 and live MatrixOne"]
async fn start_and_finish_step_run_round_trips_through_plan_step_runs_table() {
    let (app, pool) = setup_app().await;
    let (plan_id, _) = seed_plan_with_subtasks(&app, "http-runs", &["s1", "s2"]).await;

    let session_id = format!("sit-http-{}", Uuid::new_v4().simple());
    ensure_session(&pool, &session_id).await;

    // Start a run.
    let (s, body) = request_json(
        app.clone(),
        "POST",
        &format!("/plans/{plan_id}/step-runs"),
        Some(json!({
            "subtask_id": "s1",
            "session_id": session_id,
            "request_id": "req-http-1",
            "attempt": 1
        })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    let run_id = body["run_id"].as_str().unwrap().to_string();
    assert_eq!(body["subtask_id"], "s1");
    assert_eq!(body["attempt"], 1);

    // Row is in DB with correct trace metadata.
    let row = sqlx::query(
        "SELECT status, session_id, request_id, finished_at, attempt \
         FROM plan_step_runs WHERE run_id = ?",
    )
    .bind(&run_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let status_str: String = row.try_get("status").unwrap();
    let sess: String = row.try_get("session_id").unwrap();
    let reqid: String = row.try_get("request_id").unwrap();
    let finished_at: Option<chrono::DateTime<chrono::Utc>> = row.try_get("finished_at").unwrap();
    let attempt: i32 = row.try_get("attempt").unwrap();
    assert_eq!(status_str, "in_progress");
    assert_eq!(sess, session_id);
    assert_eq!(reqid, "req-http-1");
    assert_eq!(attempt, 1);
    assert!(finished_at.is_none(), "must be unfinished");

    // Finish it successfully.
    let (s, body) = request_json(
        app.clone(),
        "POST",
        &format!("/plans/{plan_id}/step-runs/{run_id}/finish"),
        Some(json!({ "status": "completed" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "completed");

    // Second finish of the same run_id must be rejected (append-only).
    let (s, _) = request_json(
        app.clone(),
        "POST",
        &format!("/plans/{plan_id}/step-runs/{run_id}/finish"),
        Some(json!({ "status": "failed" })),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "second finish must 404");

    // GET /plans/{id}/step-runs lists the attempt.
    let (s, body) = request_json(
        app.clone(),
        "GET",
        &format!("/plans/{plan_id}/step-runs"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let runs = body["runs"].as_array().unwrap();
    assert!(runs.iter().any(|r| r["run_id"] == run_id));

    cleanup_plan(&pool, &plan_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_DB_IT=1 and live MatrixOne"]
async fn execute_pins_active_plan_id_on_session() {
    let (app, pool) = setup_app().await;
    let (plan_id, v0) = seed_plan_with_subtasks(&app, "http-exec", &["s1"]).await;
    let session_id = format!("sit-exec-{}", Uuid::new_v4().simple());
    ensure_session(&pool, &session_id).await;

    // Initially the session has no active plan.
    let active_before: Option<String> =
        sqlx::query_scalar("SELECT active_plan_id FROM agent_sessions WHERE session_id = ?")
            .bind(&session_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(active_before.is_none());

    // GET current version so we pass expected_version correctly.
    let (_, get_body) = request_json(app.clone(), "GET", &format!("/plans/{plan_id}"), None).await;
    let current_version = get_body["version"].as_u64().unwrap();
    assert!(current_version >= v0);

    let (s, body) = request_json(
        app.clone(),
        "POST",
        &format!("/plans/{plan_id}/execute"),
        Some(json!({
            "session_id": session_id,
            "step_by_step": true,
            "expected_version": current_version
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["phase"], "executing");

    let active_after: Option<String> =
        sqlx::query_scalar("SELECT active_plan_id FROM agent_sessions WHERE session_id = ?")
            .bind(&session_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(active_after.as_deref(), Some(plan_id.as_str()));

    cleanup_plan(&pool, &plan_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_DB_IT=1 and live MatrixOne"]
async fn delete_plan_clears_active_plan_id_on_any_session() {
    let (app, pool) = setup_app().await;
    let (plan_id, _) = seed_plan_with_subtasks(&app, "http-del", &["s1"]).await;
    let session_id = format!("sit-del-{}", Uuid::new_v4().simple());
    ensure_session(&pool, &session_id).await;

    // Pin the plan to the session via execute.
    let (_, get_body) = request_json(app.clone(), "GET", &format!("/plans/{plan_id}"), None).await;
    let v = get_body["version"].as_u64().unwrap();
    let _ = request_json(
        app.clone(),
        "POST",
        &format!("/plans/{plan_id}/execute"),
        Some(json!({ "session_id": session_id, "expected_version": v })),
    )
    .await;

    // Delete.
    let (s, _) = request_json(app.clone(), "DELETE", &format!("/plans/{plan_id}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);

    // Plan row gone, active_plan_id cleared.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM plans WHERE plan_id = ?")
        .bind(&plan_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let active: Option<String> =
        sqlx::query_scalar("SELECT active_plan_id FROM agent_sessions WHERE session_id = ?")
            .bind(&session_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(active.is_none(), "delete must clear active_plan_id");
}

#[tokio::test]
#[ignore = "ASTRA_DB_IT=1 and live MatrixOne"]
async fn post_completed_step_run_persists_finalized_row_in_one_call() {
    // The CLI executor's happy path (subtask completed) only ever needs to
    // record a terminal-state attempt. The start+finish pair costs 2 HTTP
    // round-trips; the one-shot `POST /plans/{id}/step-runs/completed` must
    // create the row already finalized (status set, finished_at populated)
    // in a single request.
    let (app, pool) = setup_app().await;
    let (plan_id, _) = seed_plan_with_subtasks(&app, "cli-oneshot", &["s1"]).await;
    let session_id = format!("sit-oneshot-{}", Uuid::new_v4().simple());
    ensure_session(&pool, &session_id).await;

    let (status, body) = request_json(
        app.clone(),
        "POST",
        &format!("/plans/{plan_id}/step-runs/completed"),
        Some(json!({
            "subtask_id": "s1",
            "session_id": session_id,
            "request_id": "req-oneshot",
            "attempt": 1,
            "status": "completed",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let run_id = body["run_id"].as_str().expect("run_id").to_string();

    // DB row must land already-finalized: finished_at is NOT NULL, status is
    // the requested terminal state.
    let row = sqlx::query(
        "SELECT status, finished_at, attempt, session_id, request_id \
         FROM plan_step_runs WHERE run_id = ?",
    )
    .bind(&run_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let row_status: String = row.try_get("status").unwrap();
    let finished: Option<chrono::DateTime<chrono::Utc>> = row.try_get("finished_at").unwrap();
    let attempt: i32 = row.try_get("attempt").unwrap();
    let sess: String = row.try_get("session_id").unwrap();
    let reqid: String = row.try_get("request_id").unwrap();
    assert_eq!(row_status, "completed");
    assert!(
        finished.is_some(),
        "one-shot must set finished_at; a row without finished_at means the handler \
         skipped the finalize step"
    );
    assert_eq!(attempt, 1);
    assert_eq!(sess, session_id);
    assert_eq!(reqid, "req-oneshot");

    cleanup_plan(&pool, &plan_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_DB_IT=1 and live MatrixOne"]
async fn post_completed_step_run_rejects_in_progress_status() {
    // The one-shot endpoint is for terminal states only — an attempt that
    // ended in_progress shouldn't exist. The handler must 400 so callers
    // route that case through POST /step-runs instead.
    let (app, pool) = setup_app().await;
    let (plan_id, _) = seed_plan_with_subtasks(&app, "cli-oneshot-bad", &["s1"]).await;
    let session_id = format!("sit-oneshot-bad-{}", Uuid::new_v4().simple());
    ensure_session(&pool, &session_id).await;

    let (status, _body) = request_json(
        app.clone(),
        "POST",
        &format!("/plans/{plan_id}/step-runs/completed"),
        Some(json!({
            "subtask_id": "s1",
            "session_id": session_id,
            "request_id": "req-bad",
            "attempt": 1,
            "status": "in_progress",
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "in_progress is not a terminal state; one-shot must reject"
    );

    cleanup_plan(&pool, &plan_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_DB_IT=1 and live MatrixOne"]
async fn end_to_end_thin_client_posts_step_run_pair_and_persists_row() {
    // This test mirrors what the CLI executor does at the completed path:
    // it uses the real `ThinClient` against a real HTTP server to post a
    // step-run start + finish pair, and verifies the DB row has all the
    // trace fields populated correctly.
    let (app, pool) = setup_app().await;
    let (plan_id, _) = seed_plan_with_subtasks(&app, "cli-e2e-step-run", &["s1"]).await;
    let session_id = format!("sit-cli-{}", Uuid::new_v4().simple());
    ensure_session(&pool, &session_id).await;

    // Spawn the axum app on a random local port so we can point ThinClient at it.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app_for_server = app.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, app_for_server).await.unwrap();
    });

    let base = format!("http://{}", addr);
    let client = astra_thin_client::ThinClient::new(&base, None).expect("thin client");

    let start_body = json!({
        "subtask_id": "s1",
        "session_id": session_id,
        "request_id": "req-cli-e2e",
        "attempt": 2
    });
    let resp = client
        .post_plan_step_run_start("test-token", &plan_id, &start_body)
        .await
        .expect("start POST");
    let resp_json: Value = serde_json::from_str(&resp).expect("start body is JSON");
    let run_id = resp_json["run_id"].as_str().expect("run_id").to_string();
    assert_eq!(resp_json["attempt"], 2);

    // DB row exists with all the trace fields.
    let row = sqlx::query(
        "SELECT status, session_id, request_id, attempt, finished_at \
         FROM plan_step_runs WHERE run_id = ?",
    )
    .bind(&run_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let status: String = row.try_get("status").unwrap();
    let sess: String = row.try_get("session_id").unwrap();
    let reqid: String = row.try_get("request_id").unwrap();
    let attempt: i32 = row.try_get("attempt").unwrap();
    let finished: Option<chrono::DateTime<chrono::Utc>> = row.try_get("finished_at").unwrap();
    assert_eq!(status, "in_progress");
    assert_eq!(sess, session_id);
    assert_eq!(reqid, "req-cli-e2e");
    assert_eq!(attempt, 2);
    assert!(finished.is_none());

    // Finish it.
    let finish_body = json!({ "status": "completed" });
    let _ = client
        .post_plan_step_run_finish("test-token", &plan_id, &run_id, &finish_body)
        .await
        .expect("finish POST");

    let row = sqlx::query("SELECT status, finished_at FROM plan_step_runs WHERE run_id = ?")
        .bind(&run_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let status: String = row.try_get("status").unwrap();
    let finished: Option<chrono::DateTime<chrono::Utc>> = row.try_get("finished_at").unwrap();
    assert_eq!(status, "completed");
    assert!(
        finished.is_some(),
        "finished_at must be set after the finish call"
    );

    // List the runs via the client too, to prove the full CLI round-trip.
    let listing = client
        .get_plan_step_runs_text("test-token", &plan_id, Some("s1"), Some(10))
        .await
        .expect("runs list");
    let listing_json: Value = serde_json::from_str(&listing).unwrap();
    let runs = listing_json["runs"].as_array().unwrap();
    assert!(
        runs.iter().any(|r| r["run_id"] == run_id),
        "listing must include the run just created: {runs:?}"
    );

    server.abort();
    cleanup_plan(&pool, &plan_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_DB_IT=1 and live MatrixOne"]
async fn exit_plan_mode_approved_clears_session_active_plan_id() {
    // Regression: exit_plan_mode_handler previously only flipped the phase
    // hint in the response body — it did NOT clear agent_sessions.active_plan_id.
    // That left the write-tool guard (`plan_mode_authoring_active`) blocking
    // every bash/write_file call on the session even after user approval,
    // because the guard reads active_plan_id. The server-tool path
    // (`tool_exit_plan_mode`) clears active_plan_id on approve; the REST
    // handler must mirror that for web-agent parity.
    let (app, pool) = setup_app().await;
    let (plan_id, _) = seed_plan_with_subtasks(&app, "http-exit-clears", &["s1"]).await;
    let session_id = format!("sit-exit-{}", Uuid::new_v4().simple());
    ensure_session(&pool, &session_id).await;

    // Pin the plan to the session via execute so active_plan_id is non-null.
    let (_, get_body) = request_json(app.clone(), "GET", &format!("/plans/{plan_id}"), None).await;
    let v = get_body["version"].as_u64().unwrap();
    let (s, _b) = request_json(
        app.clone(),
        "POST",
        &format!("/plans/{plan_id}/execute"),
        Some(json!({
            "session_id": session_id,
            "step_by_step": true,
            "expected_version": v
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "execute must pin active_plan_id");
    let active_after_execute: Option<String> =
        sqlx::query_scalar("SELECT active_plan_id FROM agent_sessions WHERE session_id = ?")
            .bind(&session_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(active_after_execute.as_deref(), Some(plan_id.as_str()));

    // Approve → active_plan_id must be cleared so the write guard lifts.
    let (_, get_body) = request_json(app.clone(), "GET", &format!("/plans/{plan_id}"), None).await;
    let v = get_body["version"].as_u64().unwrap();
    let (s, body) = request_json(
        app.clone(),
        "POST",
        &format!("/plans/{plan_id}/exit-plan-mode"),
        Some(json!({ "approved": true, "expected_version": v })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["phase"], "refining");

    let active_after_approve: Option<String> =
        sqlx::query_scalar("SELECT active_plan_id FROM agent_sessions WHERE session_id = ?")
            .bind(&session_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        active_after_approve.is_none(),
        "approving the plan must clear active_plan_id so the write-tool guard lifts; \
         was still {active_after_approve:?}"
    );

    cleanup_plan(&pool, &plan_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_DB_IT=1 and live MatrixOne"]
async fn exit_plan_mode_rejected_leaves_active_plan_id_pinned() {
    // Control: rejecting keeps the plan pinned so the next authoring pass
    // still benefits from the write guard.
    let (app, pool) = setup_app().await;
    let (plan_id, _) = seed_plan_with_subtasks(&app, "http-exit-keeps", &["s1"]).await;
    let session_id = format!("sit-reject-{}", Uuid::new_v4().simple());
    ensure_session(&pool, &session_id).await;

    let (_, get_body) = request_json(app.clone(), "GET", &format!("/plans/{plan_id}"), None).await;
    let v = get_body["version"].as_u64().unwrap();
    let _ = request_json(
        app.clone(),
        "POST",
        &format!("/plans/{plan_id}/execute"),
        Some(json!({ "session_id": session_id, "expected_version": v })),
    )
    .await;

    let (_, get_body) = request_json(app.clone(), "GET", &format!("/plans/{plan_id}"), None).await;
    let v = get_body["version"].as_u64().unwrap();
    let (s, _) = request_json(
        app.clone(),
        "POST",
        &format!("/plans/{plan_id}/exit-plan-mode"),
        Some(json!({ "approved": false, "expected_version": v })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    let active_after_reject: Option<String> =
        sqlx::query_scalar("SELECT active_plan_id FROM agent_sessions WHERE session_id = ?")
            .bind(&session_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        active_after_reject.as_deref(),
        Some(plan_id.as_str()),
        "rejecting must keep the plan pinned for another authoring pass"
    );

    cleanup_plan(&pool, &plan_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_DB_IT=1 and live MatrixOne"]
async fn exit_plan_mode_records_lifecycle_decision() {
    let (app, pool) = setup_app().await;
    let (plan_id, _) = seed_plan_with_subtasks(&app, "http-exit", &["s1"]).await;

    let (_, get_body) = request_json(app.clone(), "GET", &format!("/plans/{plan_id}"), None).await;
    let v = get_body["version"].as_u64().unwrap();

    let (s, body) = request_json(
        app.clone(),
        "POST",
        &format!("/plans/{plan_id}/exit-plan-mode"),
        Some(json!({
            "approved": true,
            "plan_md": "# approved plan\n- step s1",
            "expected_version": v
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["phase"], "refining");

    // Reject path also works.
    let (_, get_body) = request_json(app.clone(), "GET", &format!("/plans/{plan_id}"), None).await;
    let v = get_body["version"].as_u64().unwrap();
    let (s, body) = request_json(
        app.clone(),
        "POST",
        &format!("/plans/{plan_id}/exit-plan-mode"),
        Some(json!({ "approved": false, "expected_version": v })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["phase"], "planning");

    cleanup_plan(&pool, &plan_id).await;
}

// ── Round-3 input-validation regressions ────────────────────────────────────

/// `start_step_run_handler` used to accept any client-provided `attempt`
/// including 0, negative, or near-i32::MAX. Those values silently land in
/// `plan_step_runs.attempt`, poison `max(attempt) + 1` redo logic, or wrap
/// on overflow. Handler must reject with 400 before the INSERT.
#[tokio::test]
#[ignore = "ASTRA_DB_IT=1 and live MatrixOne"]
async fn start_step_run_rejects_attempt_out_of_range() {
    let (app, pool) = setup_app().await;
    let (plan_id, _) = seed_plan_with_subtasks(&app, "http-attempt-range", &["s1"]).await;
    let session_id = format!("sit-attempt-{}", Uuid::new_v4().simple());
    ensure_session(&pool, &session_id).await;

    for bad in [0, -1, -999, i32::MIN, i32::MAX] {
        let (status, body) = request_json(
            app.clone(),
            "POST",
            &format!("/plans/{plan_id}/step-runs"),
            Some(json!({
                "subtask_id": "s1",
                "session_id": session_id,
                "request_id": "req-1",
                "attempt": bad
            })),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "attempt={bad} must be rejected, got {status} body={body}"
        );
    }

    // And no row was inserted for any of those attempts.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM plan_step_runs WHERE plan_id = ?")
        .bind(&plan_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "no step_runs must persist for rejected attempts");

    cleanup_plan(&pool, &plan_id).await;
}

/// Same rule applies to the one-shot completed-step-run endpoint.
#[tokio::test]
#[ignore = "ASTRA_DB_IT=1 and live MatrixOne"]
async fn post_completed_step_run_rejects_attempt_out_of_range() {
    let (app, pool) = setup_app().await;
    let (plan_id, _) = seed_plan_with_subtasks(&app, "http-cattempt", &["s1"]).await;
    let session_id = format!("sit-cattempt-{}", Uuid::new_v4().simple());
    ensure_session(&pool, &session_id).await;

    for bad in [0, -1] {
        let (status, body) = request_json(
            app.clone(),
            "POST",
            &format!("/plans/{plan_id}/step-runs/completed"),
            Some(json!({
                "subtask_id": "s1",
                "session_id": session_id,
                "request_id": "req-1",
                "attempt": bad,
                "status": "completed"
            })),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "attempt={bad} must be rejected, got {status} body={body}"
        );
    }

    cleanup_plan(&pool, &plan_id).await;
}

/// `rewind.reason`, `finish.error`, and `finish.artifact_ref` have no
/// backpressure: a malicious client can stuff a 10MB string into the journal
/// or the `plan_step_runs.error` column. Handlers must cap them.
#[tokio::test]
#[ignore = "ASTRA_DB_IT=1 and live MatrixOne"]
async fn rewind_rejects_oversized_reason_string() {
    let (app, pool) = setup_app().await;
    let (plan_id, _) = seed_plan_with_subtasks(&app, "http-rewind-big", &["a", "b"]).await;

    // 20k > the 5k cap we'll install.
    let huge = "A".repeat(20_000);
    let (status, body) = request_json(
        app.clone(),
        "POST",
        &format!("/plans/{plan_id}/rewind"),
        Some(json!({ "anchor": "2", "reason": huge })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "huge reason must be rejected: {body}"
    );

    cleanup_plan(&pool, &plan_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_DB_IT=1 and live MatrixOne"]
async fn finish_step_run_rejects_oversized_error_and_artifact_ref() {
    let (app, pool) = setup_app().await;
    let (plan_id, _) = seed_plan_with_subtasks(&app, "http-finish-big", &["s1"]).await;
    let session_id = format!("sit-finish-big-{}", Uuid::new_v4().simple());
    ensure_session(&pool, &session_id).await;

    // Start a run legitimately.
    let (_, body) = request_json(
        app.clone(),
        "POST",
        &format!("/plans/{plan_id}/step-runs"),
        Some(json!({
            "subtask_id": "s1",
            "session_id": session_id,
            "request_id": "req-1",
            "attempt": 1
        })),
    )
    .await;
    let run_id = body["run_id"].as_str().unwrap().to_string();

    let huge_err = "E".repeat(20_000);
    let (status, body) = request_json(
        app.clone(),
        "POST",
        &format!("/plans/{plan_id}/step-runs/{run_id}/finish"),
        Some(json!({ "status": "failed", "error": huge_err })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "huge error must be rejected: {body}"
    );

    let huge_art = "/".to_string() + &"a".repeat(2000);
    let (status, body) = request_json(
        app.clone(),
        "POST",
        &format!("/plans/{plan_id}/step-runs/{run_id}/finish"),
        Some(json!({ "status": "failed", "artifact_ref": huge_art })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "huge artifact_ref must be rejected: {body}"
    );

    // Run must still be open (nothing finalized).
    let row = sqlx::query("SELECT finished_at FROM plan_step_runs WHERE run_id = ?")
        .bind(&run_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let finished: Option<chrono::DateTime<chrono::Utc>> = row.try_get("finished_at").unwrap();
    assert!(
        finished.is_none(),
        "rejected finalize must not persist finished_at"
    );

    cleanup_plan(&pool, &plan_id).await;
}

/// Regression for round-2 review finding: rewind was resetting subtasks to
/// Pending but leaving open `plan_step_runs` rows with `finished_at IS NULL`.
/// The orphaned audit rows would skew attempt counters and make stall
/// detectors think the subtask was still executing. Handler must now cancel
/// any open runs for the reset suffix.
#[tokio::test]
#[ignore = "ASTRA_DB_IT=1 and live MatrixOne"]
async fn rewind_cancels_open_step_runs_for_reset_subtasks() {
    let (app, pool) = setup_app().await;
    let (plan_id, _) = seed_plan_with_subtasks(&app, "http-rewind-abort", &["a", "b", "c"]).await;
    let session_id = format!("sit-rewind-abort-{}", Uuid::new_v4().simple());
    ensure_session(&pool, &session_id).await;

    // Start open runs on b and c (they are the ones rewind will touch).
    // Leave a with a completed run so we can verify it stays intact.
    let (_s, body_a) = request_json(
        app.clone(),
        "POST",
        &format!("/plans/{plan_id}/step-runs"),
        Some(json!({ "subtask_id": "a", "session_id": session_id, "request_id": "req-a", "attempt": 1 })),
    )
    .await;
    let run_a = body_a["run_id"].as_str().unwrap().to_string();
    let _ = request_json(
        app.clone(),
        "POST",
        &format!("/plans/{plan_id}/step-runs/{run_a}/finish"),
        Some(json!({ "status": "completed" })),
    )
    .await;

    let (_, body_b) = request_json(
        app.clone(),
        "POST",
        &format!("/plans/{plan_id}/step-runs"),
        Some(json!({ "subtask_id": "b", "session_id": session_id, "request_id": "req-b", "attempt": 1 })),
    )
    .await;
    let run_b = body_b["run_id"].as_str().unwrap().to_string();
    let (_, body_c) = request_json(
        app.clone(),
        "POST",
        &format!("/plans/{plan_id}/step-runs"),
        Some(json!({ "subtask_id": "c", "session_id": session_id, "request_id": "req-c", "attempt": 1 })),
    )
    .await;
    let run_c = body_c["run_id"].as_str().unwrap().to_string();

    // Rewind from anchor=2 → reset suffix (b, c); a stays completed.
    let (s, body) = request_json(
        app.clone(),
        "POST",
        &format!("/plans/{plan_id}/rewind"),
        Some(json!({ "anchor": "2", "reason": "restart b+c" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");

    // b and c's runs must be cancelled (finalized).
    let row_b = sqlx::query("SELECT status, finished_at FROM plan_step_runs WHERE run_id = ?")
        .bind(&run_b)
        .fetch_one(&pool)
        .await
        .unwrap();
    let b_status: String = row_b.try_get("status").unwrap();
    let b_finished: Option<chrono::DateTime<chrono::Utc>> = row_b.try_get("finished_at").unwrap();
    assert_eq!(
        b_status, "cancelled",
        "b's open run must be cancelled by rewind"
    );
    assert!(b_finished.is_some(), "b's open run must gain finished_at");

    let row_c = sqlx::query("SELECT status, finished_at FROM plan_step_runs WHERE run_id = ?")
        .bind(&run_c)
        .fetch_one(&pool)
        .await
        .unwrap();
    let c_status: String = row_c.try_get("status").unwrap();
    let c_finished: Option<chrono::DateTime<chrono::Utc>> = row_c.try_get("finished_at").unwrap();
    assert_eq!(
        c_status, "cancelled",
        "c's open run must be cancelled by rewind"
    );
    assert!(c_finished.is_some(), "c's open run must gain finished_at");

    // a's already-finalized run must not be re-touched.
    let row_a = sqlx::query("SELECT status FROM plan_step_runs WHERE run_id = ?")
        .bind(&run_a)
        .fetch_one(&pool)
        .await
        .unwrap();
    let a_status: String = row_a.try_get("status").unwrap();
    assert_eq!(a_status, "completed", "a's run must stay completed");

    cleanup_plan(&pool, &plan_id).await;
}
