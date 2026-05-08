use std::sync::Arc;
use std::time::{Duration, Instant};

use astra_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ErrorResponse, HealthChecker, ServiceInfo,
    SessionActivityRecord, SessionCreateRequestData, SessionListFilter, SessionListRecord,
    SessionRecord, SessionService, SessionUpdateRequestData, build_app,
    server::run_engine::RunEngine,
};
use astra_services::runs::{
    CancelRunRecord, ChatRequestData, ChatRunRecord, ChatStreamRecord, DatabaseRunStateStore,
    DurableRunRecord, RunInputData, RunInputRecord, RunLifecycleService, RunListRecord,
    RunMutationRecord, RunStateStore, RunStatusRecord, SSE_HEARTBEAT_INTERVAL_SECS,
    ToolOutputBatchItem,
};
use async_trait::async_trait;
use axum::{
    Json, Router,
    body::{self, Body},
    http::{HeaderMap, Request, StatusCode},
};
use serde_json::{Value, json};
use sqlx::Row;
use tokio::sync::RwLock;
use tower::util::ServiceExt;
use uuid::Uuid;

const HTTP_TOKEN: &str = "Bearer phase1-http-token";

fn require_db_it_env() -> astra_core::MatrixOneSettings {
    assert_eq!(
        std::env::var("ASTRA_TEST_DB_IT").as_deref(),
        Ok("1"),
        "set ASTRA_TEST_DB_IT=1 for ignored integration tests"
    );
    astra_core::MatrixOneSettings::from_env()
}

static SHARED_BOOTSTRAP: tokio::sync::OnceCell<astra_core::SharedPool> =
    tokio::sync::OnceCell::const_new();

async fn setup_pool() -> astra_core::SharedPool {
    SHARED_BOOTSTRAP
        .get_or_init(|| async {
            let settings = require_db_it_env();
            let catalog = std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG")
                .unwrap_or_else(|_| "mysql".into());
            astra_services::ensure_core_schema(&settings, &catalog)
                .await
                .expect("ensure_core_schema; is MatrixOne up?");
            astra_core::SharedPool::new(&settings)
                .await
                .expect("SharedPool::new")
        })
        .await
        .clone()
}

fn test_ids() -> (String, String, String) {
    let suffix = Uuid::new_v4();
    (
        format!("run-{suffix}"),
        format!("session-{suffix}"),
        format!("user-{suffix}"),
    )
}

fn durable_record(run_id: &str, session_id: &str, user_id: &str) -> DurableRunRecord {
    DurableRunRecord {
        run_id: run_id.to_string(),
        user_id: user_id.to_string(),
        session_id: session_id.to_string(),
        parent_run_id: None,
        root_run_id: Some(run_id.to_string()),
        ancestor_path: Some(run_id.to_string()),
        depth: 0,
        delegation_id: None,
        agent_id: Some("phase1-agent".to_string()),
        retry_of: None,
        retry_scope: Some("node".to_string()),
        status: "running".to_string(),
        waiting_for: None,
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
        events: vec![json!({"event_type": "run_started", "data": {}})],
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
    }
}

#[derive(Clone)]
struct Phase1HttpHealth;

#[async_trait]
impl HealthChecker for Phase1HttpHealth {
    async fn database_healthy(&self) -> bool {
        true
    }
}

#[derive(Clone)]
struct Phase1HttpAuth {
    user_id: String,
}

#[async_trait]
impl AuthService for Phase1HttpAuth {
    async fn current_user(
        &self,
        headers: &HeaderMap,
    ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
        if headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            != Some(HTTP_TOKEN)
        {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse::new("unauthorized")),
            ));
        }
        Ok(AuthUserRecord {
            user_id: self.user_id.clone(),
            username: "phase1-http".to_string(),
            email: "phase1-http@example.test".to_string(),
            display_name: None,
        })
    }

    async fn register(
        &self,
        _request: AuthRegisterRequestData,
    ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }

    async fn login(
        &self,
        _request: AuthLoginRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }

    async fn refresh(
        &self,
        _request: AuthRefreshRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }

    async fn logout(
        &self,
        _request: AuthRefreshRequestData,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }
}

#[derive(Clone)]
struct Phase1HttpSession {
    user_id: String,
}

#[async_trait]
impl SessionService for Phase1HttpSession {
    async fn create_session(
        &self,
        _user_id: String,
        _request: SessionCreateRequestData,
    ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }

    async fn get_session(
        &self,
        session_id: String,
        _user_id: String,
    ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
        Ok(SessionRecord {
            session_id,
            user_id: self.user_id.clone(),
            agent_id: Some("phase1-http-agent".to_string()),
            title: Some("phase1 http".to_string()),
            status: "active".to_string(),
            metadata: Default::default(),
            event_count: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: Some(chrono::Utc::now().to_rfc3339()),
            ended_at: None,
        })
    }

    async fn update_session(
        &self,
        _session_id: String,
        _user_id: String,
        _request: SessionUpdateRequestData,
    ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }

    async fn list_sessions(
        &self,
        _filter: SessionListFilter,
    ) -> Result<SessionListRecord, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }

    async fn delete_session(
        &self,
        _session_id: String,
        _user_id: String,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }

    async fn get_session_activity(
        &self,
        _session_id: String,
        _user_id: String,
        _limit: u32,
        _offset: u32,
    ) -> Result<SessionActivityRecord, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }
}

#[derive(Clone)]
struct Phase1HttpRunLifecycle {
    store: Arc<RwLock<DatabaseRunStateStore>>,
}

impl Phase1HttpRunLifecycle {
    async fn store(&self) -> DatabaseRunStateStore {
        self.store.read().await.clone()
    }
}

#[async_trait]
impl RunLifecycleService for Phase1HttpRunLifecycle {
    async fn create_run(
        &self,
        _user_id: String,
        _request: ChatRequestData,
    ) -> Result<ChatRunRecord, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }

    async fn stream_chat(
        &self,
        _user_id: String,
        _request: ChatRequestData,
    ) -> Result<ChatStreamRecord, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }

    async fn get_run_status(
        &self,
        run_id: String,
        user_id: String,
    ) -> Result<RunStatusRecord, (StatusCode, Json<ErrorResponse>)> {
        let run = self
            .store()
            .await
            .load_run(&run_id)
            .await
            .map_err(|error| {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(ErrorResponse::new(error)),
                )
            })?
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    Json(ErrorResponse::new("run not found")),
                )
            })?;
        if run.user_id != user_id {
            return Err((
                StatusCode::FORBIDDEN,
                Json(ErrorResponse::new("access denied")),
            ));
        }
        Ok(RunStatusRecord {
            run_id,
            session_id: run.session_id,
            status: run.status,
            waiting_for: run.waiting_for,
            events_count: run.events.len() as i64,
        })
    }

    async fn stream_run(
        &self,
        run_id: String,
        user_id: String,
        last_index: u32,
    ) -> Result<Vec<Value>, (StatusCode, Json<ErrorResponse>)> {
        let run = self
            .store()
            .await
            .load_run(&run_id)
            .await
            .map_err(|error| {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(ErrorResponse::new(error)),
                )
            })?
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    Json(ErrorResponse::new("run not found")),
                )
            })?;
        if run.user_id != user_id {
            return Err((
                StatusCode::FORBIDDEN,
                Json(ErrorResponse::new("access denied")),
            ));
        }
        Ok(run.events.into_iter().skip(last_index as usize).collect())
    }

    async fn cancel_run(
        &self,
        run_id: String,
        _user_id: String,
    ) -> Result<CancelRunRecord, (StatusCode, Json<ErrorResponse>)> {
        Ok(CancelRunRecord {
            run_id,
            status: "cancelled".to_string(),
        })
    }

    async fn list_runs(
        &self,
        _user_id: String,
        limit: u32,
        offset: u32,
    ) -> Result<RunListRecord, (StatusCode, Json<ErrorResponse>)> {
        Ok(RunListRecord {
            runs: Vec::new(),
            total: 0,
            limit,
            offset,
        })
    }

    async fn submit_run_input(
        &self,
        run_id: String,
        user_id: String,
        input: RunInputData,
    ) -> Result<RunInputRecord, (StatusCode, Json<ErrorResponse>)> {
        let store = self.store().await;
        let run = store
            .load_run(&run_id)
            .await
            .map_err(|error| {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(ErrorResponse::new(error)),
                )
            })?
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    Json(ErrorResponse::new("run not found")),
                )
            })?;
        if run.user_id != user_id {
            return Err((
                StatusCode::FORBIDDEN,
                Json(ErrorResponse::new("access denied")),
            ));
        }
        let duplicate = run.events.iter().any(|event| {
            event.get("idempotency_key").and_then(Value::as_str)
                == Some(input.idempotency_key.as_str())
        });
        if !duplicate {
            store
                .append_event(
                    &run_id,
                    json!({
                        "event_type": "user_input",
                        "idempotency_key": input.idempotency_key,
                        "data": {"input": input.input},
                    }),
                )
                .await
                .map_err(|error| {
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(ErrorResponse::new(error)),
                    )
                })?;
            store
                .update_run_status(&run_id, "running", None, None)
                .await
                .map_err(|error| {
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(ErrorResponse::new(error)),
                    )
                })?;
            store
                .append_event(
                    &run_id,
                    json!({"event_type": "run_resumed", "data": {"source": "approval_input"}}),
                )
                .await
                .map_err(|error| {
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(ErrorResponse::new(error)),
                    )
                })?;
        }
        Ok(RunInputRecord {
            run_id,
            accepted: true,
            duplicate,
        })
    }

    async fn pause_run(
        &self,
        run_id: String,
        _user_id: String,
    ) -> Result<RunMutationRecord, (StatusCode, Json<ErrorResponse>)> {
        self.store()
            .await
            .update_run_status(&run_id, "waiting", Some("user"), None)
            .await
            .map_err(|error| {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(ErrorResponse::new(error)),
                )
            })?;
        Ok(RunMutationRecord {
            run_id,
            status: "waiting".to_string(),
            previous_status: "running".to_string(),
        })
    }
}

fn build_phase1_http_app(
    pool: astra_core::SharedPool,
    _session_id: String,
    user_id: String,
    store: Arc<RwLock<DatabaseRunStateStore>>,
) -> Router {
    let state = AppState::new(ServiceInfo::default(), Arc::new(Phase1HttpHealth))
        .with_shared_pool(pool)
        .with_auth_service(Arc::new(Phase1HttpAuth {
            user_id: user_id.clone(),
        }))
        .with_session_service(Arc::new(Phase1HttpSession { user_id }))
        .with_run_lifecycle_service(Arc::new(Phase1HttpRunLifecycle { store }));
    build_app(state)
}

fn parse_sse_events(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|json| serde_json::from_str::<Value>(json).ok())
        .collect()
}

async fn http_get_run_stream(app: &Router, run_id: &str, last_index: u32) -> Vec<Value> {
    let request = Request::builder()
        .method("GET")
        .uri(format!(
            "/chat/runs/{run_id}/stream?last_index={last_index}"
        ))
        .header("authorization", HTTP_TOKEN)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = body::to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    parse_sse_events(&String::from_utf8_lossy(&bytes))
}

async fn http_post_run_input(app: &Router, run_id: &str, key: &str, input: Value) {
    let request = Request::builder()
        .method("POST")
        .uri(format!("/chat/runs/{run_id}/input"))
        .header("authorization", HTTP_TOKEN)
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"idempotency_key": key, "input": input}).to_string(),
        ))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn l2_lease_race_has_single_owner() {
    let pool = setup_pool().await;
    let (run_id, session_id, user_id) = test_ids();
    let store = DatabaseRunStateStore::new(pool.clone()).with_owner_pod_id("owner-a");
    store
        .insert_run(durable_record(&run_id, &session_id, &user_id))
        .await
        .unwrap();

    let a = DatabaseRunStateStore::new(pool.clone()).with_owner_pod_id("owner-a");
    let b = DatabaseRunStateStore::new(pool).with_owner_pod_id("owner-b");
    let (won_a, won_b) = tokio::join!(
        a.acquire_owner_lease(&run_id, "owner-a", Duration::from_secs(30)),
        b.acquire_owner_lease(&run_id, "owner-b", Duration::from_secs(30))
    );
    let wins = [won_a.unwrap(), won_b.unwrap()]
        .into_iter()
        .filter(|won| *won)
        .count();
    assert_eq!(wins, 1, "exactly one pod may own a live lease");
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn l2_event_idx_and_idempotency_use_run_counters() {
    let pool = setup_pool().await;
    let (run_id, session_id, user_id) = test_ids();
    let store = DatabaseRunStateStore::new(pool.clone());
    store
        .insert_run(durable_record(&run_id, &session_id, &user_id))
        .await
        .unwrap();
    store
        .append_event(
            &run_id,
            json!({"event_type": "user_input", "idempotency_key": "same-key", "data": {"text": "one"}}),
        )
        .await
        .unwrap();
    store
        .append_event(
            &run_id,
            json!({"event_type": "user_input", "idempotency_key": "same-key", "data": {"text": "one"}}),
        )
        .await
        .unwrap();
    store
        .append_event(&run_id, json!({"event_type": "tool_result", "data": {}}))
        .await
        .unwrap();

    let rows = sqlx::query(
        "SELECT event_idx FROM agent_run_events WHERE run_id = ? ORDER BY event_idx ASC",
    )
    .bind(&run_id)
    .fetch_all(pool.get())
    .await
    .unwrap();
    let idx = rows
        .into_iter()
        .map(|row| row.try_get::<i64, _>("event_idx").unwrap())
        .collect::<Vec<_>>();
    assert_eq!(idx, [0, 1, 2]);

    let counter = sqlx::query("SELECT next_event_idx FROM run_counters WHERE run_id = ?")
        .bind(&run_id)
        .fetch_one(pool.get())
        .await
        .unwrap()
        .try_get::<i64, _>("next_event_idx")
        .unwrap();
    assert_eq!(counter, 3);
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn l2_graceful_checkpoint_recovers_as_waiting() {
    let pool = setup_pool().await;
    let (run_id, session_id, user_id) = test_ids();
    let store = Arc::new(DatabaseRunStateStore::new(pool));
    store
        .insert_run(durable_record(&run_id, &session_id, &user_id))
        .await
        .unwrap();

    assert!(
        store
            .save_checkpoint(
                &run_id,
                &json!({
                    "version": "checkpoint_v1",
                    "graceful": true,
                    "last_batch_id": "batch-1",
                    "extra": {
                        "partial_progress": {
                            "step_index": 1,
                            "total_steps": 3,
                            "resumable_marker": "after-step-1"
                        }
                    }
                })
                .to_string(),
            )
            .await
            .unwrap()
    );
    assert!(
        store
            .save_checkpoint(&run_id, r#"{"version":"bad"}"#)
            .await
            .is_err()
    );

    let engine = RunEngine::new(store);
    let recovered = engine.recover_active_runs().await.unwrap();
    assert!(recovered.iter().any(|run| run.run_id == run_id));
    let loaded = engine.load_run(&run_id).await.unwrap().unwrap();
    assert_eq!(loaded.status, "waiting");
    assert_eq!(loaded.waiting_for.as_deref(), Some("restart_resume"));
    assert!(loaded.events.iter().any(|event| {
        event.get("event_type").and_then(serde_json::Value::as_str)
            == Some("run_resumed_after_restart")
    }));
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn l2_crash_recovery_marks_running_failed() {
    let pool = setup_pool().await;
    let (run_id, session_id, user_id) = test_ids();
    let store = Arc::new(DatabaseRunStateStore::new(pool));
    store
        .insert_run(durable_record(&run_id, &session_id, &user_id))
        .await
        .unwrap();

    let engine = RunEngine::new(store);
    let recovered = engine.recover_active_runs().await.unwrap();
    assert!(recovered.iter().any(|run| run.run_id == run_id));
    let loaded = engine.load_run(&run_id).await.unwrap().unwrap();
    assert_eq!(loaded.status, "failed");
    assert_eq!(
        loaded.error_message.as_deref(),
        Some("recovered from crash")
    );
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn l2_retry_scope_and_batch_contracts_hold() {
    let pool = setup_pool().await;
    let (run_id, session_id, user_id) = test_ids();
    let store = DatabaseRunStateStore::new(pool.clone());
    let mut record = durable_record(&run_id, &session_id, &user_id);
    record.retry_scope = Some("siblings".to_string());
    store.insert_run(record).await.unwrap();
    let loaded = store.load_run(&run_id).await.unwrap().unwrap();
    assert_eq!(loaded.retry_scope.as_deref(), Some("siblings"));

    let mut invalid = durable_record(&format!("{run_id}-invalid"), &session_id, &user_id);
    invalid.retry_scope = Some("branch".to_string());
    assert!(store.insert_run(invalid).await.is_err());

    let batch_id = format!("batch-{}", Uuid::new_v4());
    let items = (0..500)
        .map(|idx| ToolOutputBatchItem {
            output_id: format!("out-{idx}-{}", Uuid::new_v4()),
            tool_call_id: Some(format!("call-{idx}")),
            tool_name: "bash".to_string(),
            output_json: json!({"idx": idx, "stdout": "ok"}),
        })
        .collect::<Vec<_>>();
    store
        .insert_tool_output_batch(&batch_id, &session_id, &run_id, &user_id, &items)
        .await
        .unwrap();

    let oversized = (0..501)
        .map(|idx| ToolOutputBatchItem {
            output_id: format!("oversized-{idx}-{}", Uuid::new_v4()),
            tool_call_id: None,
            tool_name: "bash".to_string(),
            output_json: json!({"idx": idx}),
        })
        .collect::<Vec<_>>();
    assert!(
        store
            .insert_tool_output_batch(
                &format!("batch-{}", Uuid::new_v4()),
                &session_id,
                &run_id,
                &user_id,
                &oversized,
            )
            .await
            .is_err()
    );
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn l2_one_thousand_tool_outputs_insert_under_500ms() {
    let pool = setup_pool().await;
    let (run_id, session_id, user_id) = test_ids();
    let store = DatabaseRunStateStore::new(pool);
    store
        .insert_run(durable_record(&run_id, &session_id, &user_id))
        .await
        .unwrap();
    let started = Instant::now();
    for chunk in 0..2 {
        let items = (0..500)
            .map(|idx| ToolOutputBatchItem {
                output_id: format!("l2-out-{chunk}-{idx}-{}", Uuid::new_v4()),
                tool_call_id: Some(format!("call-{chunk}-{idx}")),
                tool_name: "bash".to_string(),
                output_json: json!({"chunk": chunk, "idx": idx, "stdout": "ok"}),
            })
            .collect::<Vec<_>>();
        store
            .insert_tool_output_batch(
                &format!("l2-batch-{chunk}-{}", Uuid::new_v4()),
                &session_id,
                &run_id,
                &user_id,
                &items,
            )
            .await
            .unwrap();
    }
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "1000 tool outputs should insert in under 500ms"
    );
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn l2_sse_heartbeat_contract_is_15_seconds() {
    assert_eq!(SSE_HEARTBEAT_INTERVAL_SECS, 15);
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn l3_s04_reconnect_replays_monotonic_events() {
    let pool = setup_pool().await;
    let (run_id, session_id, user_id) = test_ids();
    let store = DatabaseRunStateStore::new(pool);
    store
        .insert_run(durable_record(&run_id, &session_id, &user_id))
        .await
        .unwrap();
    for idx in 0..17 {
        store
            .append_event(
                &run_id,
                json!({"event_type": "text_delta", "data": {"idx": idx}}),
            )
            .await
            .unwrap();
    }
    for idx in 0..2 {
        store
            .append_event(
                &run_id,
                json!({"event_type": "approval_decision", "data": {"idx": idx}}),
            )
            .await
            .unwrap();
    }
    let loaded = store.load_run(&run_id).await.unwrap().unwrap();
    let indexes = loaded
        .events
        .iter()
        .map(|event| {
            event
                .get("index")
                .and_then(serde_json::Value::as_i64)
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(indexes, (0..20).collect::<Vec<_>>());
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn l3_s04_t01_t17_full_reconnect_survives_restart_and_approvals() {
    let pool = setup_pool().await;
    let (run_id, session_id, user_id) = test_ids();
    let store_a = DatabaseRunStateStore::new(pool.clone()).with_owner_pod_id("phase1-pod-a");
    store_a
        .insert_run(durable_record(&run_id, &session_id, &user_id))
        .await
        .unwrap();
    assert!(
        store_a
            .acquire_owner_lease(&run_id, "phase1-pod-a", Duration::from_secs(30))
            .await
            .unwrap()
    );
    let active_store = Arc::new(RwLock::new(store_a.clone()));
    let app = build_phase1_http_app(
        pool.clone(),
        session_id.clone(),
        user_id.clone(),
        active_store.clone(),
    );

    let mut next_index = 0_u32;
    let mut client_indexes = Vec::new();
    for disconnect in 0..17 {
        active_store
            .read()
            .await
            .append_event(
                &run_id,
                json!({"event_type": "text_delta", "data": {"disconnect": disconnect}}),
            )
            .await
            .unwrap();
        let events = http_get_run_stream(&app, &run_id, next_index).await;
        let new_events = events
            .iter()
            .filter_map(|event| event.get("index").and_then(Value::as_u64))
            .collect::<Vec<_>>();
        assert_eq!(
            new_events.len(),
            if disconnect == 0 { 2 } else { 1 },
            "each HTTP reconnect should replay only the missing SSE gap"
        );
        client_indexes.extend(new_events.iter().map(|idx| *idx as i64));
        next_index = new_events.last().copied().unwrap() as u32 + 1;
    }

    active_store
        .read()
        .await
        .save_checkpoint(
            &run_id,
            &json!({
                "version": "checkpoint_v1",
                "graceful": true,
                "extra": {"partial_progress": {"step_index": 17, "total_steps": 17}}
            })
            .to_string(),
        )
        .await
        .unwrap();
    let engine = RunEngine::new(Arc::new(active_store.read().await.clone()));
    let recovered = engine.recover_active_runs().await.unwrap();
    assert!(recovered.iter().any(|run| run.run_id == run_id));

    sqlx::query(
        "UPDATE run_counters
         SET owner_lease_expires_at = DATE_SUB(NOW(6), INTERVAL 1 SECOND)
         WHERE run_id = ?",
    )
    .bind(&run_id)
    .execute(pool.get())
    .await
    .unwrap();
    let store_b = DatabaseRunStateStore::new(pool).with_owner_pod_id("phase1-pod-b");
    assert!(
        store_b
            .acquire_owner_lease(&run_id, "phase1-pod-b", Duration::from_secs(30))
            .await
            .unwrap(),
        "new pod should take over the durable run_counters lease after restart"
    );
    *active_store.write().await = store_b.clone();

    for (idx, decision) in ["approve-read", "approve-write"].into_iter().enumerate() {
        active_store
            .read()
            .await
            .append_event(
                &run_id,
                json!({"event_type": "run_paused", "data": {"waiting_for": "user"}}),
            )
            .await
            .unwrap();
        active_store
            .read()
            .await
            .append_event(
                &run_id,
                json!({"event_type": "approval_required", "data": {"request_id": format!("approval-{idx}")}}),
            )
            .await
            .unwrap();
        active_store
            .read()
            .await
            .update_run_status(&run_id, "waiting", Some("user"), None)
            .await
            .unwrap();
        let waiting = http_get_run_stream(&app, &run_id, next_index).await;
        let waiting_indexes = waiting
            .iter()
            .filter_map(|event| event.get("index").and_then(Value::as_u64))
            .collect::<Vec<_>>();
        client_indexes.extend(waiting_indexes.iter().map(|idx| *idx as i64));
        next_index = waiting_indexes.last().copied().unwrap() as u32 + 1;

        http_post_run_input(
            &app,
            &run_id,
            &format!("approval-key-{idx}"),
            json!({"approval": decision}),
        )
        .await;
        let resumed = http_get_run_stream(&app, &run_id, next_index).await;
        let resumed_indexes = resumed
            .iter()
            .filter_map(|event| event.get("index").and_then(Value::as_u64))
            .collect::<Vec<_>>();
        assert_eq!(
            resumed_indexes.len(),
            2,
            "approval input should append user_input and run_resumed over HTTP"
        );
        client_indexes.extend(resumed_indexes.iter().map(|idx| *idx as i64));
        next_index = resumed_indexes.last().copied().unwrap() as u32 + 1;
    }
    let loaded = active_store
        .read()
        .await
        .load_run(&run_id)
        .await
        .unwrap()
        .unwrap();
    let indexes = loaded
        .events
        .iter()
        .map(|event| {
            event
                .get("index")
                .and_then(serde_json::Value::as_i64)
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        client_indexes, indexes,
        "client-side HTTP SSE replay should receive every event exactly once"
    );
    assert_eq!(
        indexes,
        (0..27).collect::<Vec<_>>(),
        "run_started + 17 reconnect fragments + restart resume + 2 approval cycles stay monotonic"
    );
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn l3_s08_one_thousand_tool_outputs_split_under_two_seconds() {
    let pool = setup_pool().await;
    let (run_id, session_id, user_id) = test_ids();
    let store = DatabaseRunStateStore::new(pool);
    store
        .insert_run(durable_record(&run_id, &session_id, &user_id))
        .await
        .unwrap();
    let started = Instant::now();
    for chunk in 0..2 {
        let items = (0..500)
            .map(|idx| ToolOutputBatchItem {
                output_id: format!("out-{chunk}-{idx}-{}", Uuid::new_v4()),
                tool_call_id: Some(format!("call-{chunk}-{idx}")),
                tool_name: "bash".to_string(),
                output_json: json!({"chunk": chunk, "idx": idx, "stdout": "ok"}),
            })
            .collect::<Vec<_>>();
        store
            .insert_tool_output_batch(
                &format!("batch-{chunk}-{}", Uuid::new_v4()),
                &session_id,
                &run_id,
                &user_id,
                &items,
            )
            .await
            .unwrap();
    }
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "1000 tool outputs should insert in under 2s"
    );
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn l3_s10_retry_scope_is_persisted_for_retry_runs() {
    let pool = setup_pool().await;
    let (run_id, session_id, user_id) = test_ids();
    let original = format!("{run_id}-original");
    let store = DatabaseRunStateStore::new(pool);
    store
        .insert_run(durable_record(&original, &session_id, &user_id))
        .await
        .unwrap();
    let mut retry = durable_record(&run_id, &session_id, &user_id);
    retry.retry_of = Some(original);
    retry.retry_scope = Some("subtree".to_string());
    retry.depth = 1;
    retry.ancestor_path = Some(format!("{run_id}/child"));
    store.insert_run(retry).await.unwrap();
    let loaded = store.load_run(&run_id).await.unwrap().unwrap();
    assert_eq!(loaded.retry_scope.as_deref(), Some("subtree"));
    assert!(loaded.retry_of.is_some());
}
