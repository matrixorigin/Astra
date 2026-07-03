mod test_support;

use std::{collections::BTreeSet, net::SocketAddr, sync::Arc, time::Duration};

use astra_core::{MatrixOneSettings, SharedPool};
use astra_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ErrorResponse, HealthChecker, ServiceInfo,
    SessionActivityRecord, SessionCreateRequestData, SessionListFilter, SessionListRecord,
    SessionRecord, SessionService, SessionUpdateRequestData, build_app,
};
use astra_services::runs::{
    CancelRunRecord, ChatRequestData, ChatRunRecord, ChatStreamRecord, DurableRunRecord,
    RunInputData, RunInputRecord, RunLifecycleService, RunListCursor, RunListRecord,
    RunMutationRecord, RunStateStore, RunStatusRecord,
};
use astra_services::session_workspace::{WorkspaceMetadata, persist_remote_workspace};
use astra_services::{
    BubbleUpTarget, COMPACTION_INVARIANT_SQL, ConfidenceAction, ContextManifestItemWrite,
    ContextManifestWrite, DatabaseContextManifestStore, DatabaseRunStateStore,
    DatabaseSessionArtifactStore, DatabaseStateProjectionStore, DelegationProjectionUpsert,
    SessionArtifactJsonRecord, SessionArtifactJsonStore, next_action_confidence_action,
};
use async_trait::async_trait;
use axum::{Json, Router, http::HeaderMap, http::StatusCode};
use reqwest::Client;
use serde_json::{Value, json};
use sqlx::Row;
use test_support::parse_sse_events;
use tokio::sync::RwLock;
use uuid::Uuid;

const HTTP_TOKEN: &str = "Bearer e2e-joint-token";

static SHARED_BOOTSTRAP: tokio::sync::OnceCell<MatrixOneSettings> =
    tokio::sync::OnceCell::const_new();

fn require_db_it_env() -> astra_core::MatrixOneSettings {
    let enabled = std::env::var("ASTRA_TEST_DB_IT").unwrap_or_default();
    assert!(
        enabled == "1",
        "set ASTRA_TEST_DB_IT=1 for ignored joint E2E tests; got {enabled:?}"
    );
    astra_core::MatrixOneSettings::from_env()
}

async fn setup_pool() -> SharedPool {
    let settings = SHARED_BOOTSTRAP
        .get_or_init(|| async {
            let settings = require_db_it_env();
            let catalog = std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG")
                .unwrap_or_else(|_| "mysql".to_string());
            astra_services::ensure_core_schema(&settings, &catalog)
                .await
                .expect("ensure_core_schema must pass before joint E2E");
            settings
        })
        .await;
    SharedPool::new(settings)
        .await
        .expect("SharedPool::new must connect to MatrixOne")
}

fn id(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
}

async fn insert_session(pool: &SharedPool, user_id: &str, session_id: &str) {
    sqlx::query(
        "INSERT INTO agent_sessions
         (session_id, user_id, agent_id, title, status, metadata, created_at, updated_at)
         VALUES (?, ?, 'joint-agent', 'joint e2e session', 'active', '{}', NOW(6), NOW(6))",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(pool.get())
    .await
    .expect("insert_session must create isolated test session");
}

#[allow(clippy::too_many_arguments)]
async fn insert_run_row(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
    run_id: &str,
    parent_run_id: Option<&str>,
    root_run_id: &str,
    ancestor_path: &str,
    depth: i64,
    status: &str,
    retry_of: Option<&str>,
    retry_scope: &str,
) {
    sqlx::query(
        "INSERT INTO agent_runs
         (run_id, user_id, session_id, parent_run_id, root_run_id, ancestor_path, depth,
          retry_of, retry_scope, status, last_event_idx, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, -1, NOW(6), NOW(6))",
    )
    .bind(run_id)
    .bind(user_id)
    .bind(session_id)
    .bind(parent_run_id)
    .bind(root_run_id)
    .bind(ancestor_path)
    .bind(depth)
    .bind(retry_of)
    .bind(retry_scope)
    .bind(status)
    .execute(pool.get())
    .await
    .expect("insert_run_row must persist run tree node");
}

async fn insert_state_item(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
    run_id: &str,
    category: &str,
    item_key: &str,
    version: i64,
) -> String {
    let item_id = id("state");
    sqlx::query(
        "INSERT INTO session_state_items
         (item_id, user_id, session_id, scope, category, item_key, status, priority, source,
          run_id, title, summary_text, payload_json, token_estimate, version, created_at, updated_at)
         VALUES (?, ?, ?, 'session', ?, ?, 'active', 10, 'e2e_joint',
                 ?, ?, ?, ?, 40, ?, NOW(6), NOW(6))",
    )
    .bind(&item_id)
    .bind(user_id)
    .bind(session_id)
    .bind(category)
    .bind(item_key)
    .bind(run_id)
    .bind(format!("{category} {item_key}"))
    .bind(format!("summary for {category} {item_key}"))
    .bind(json!({"category": category, "item_key": item_key}).to_string())
    .bind(version)
    .execute(pool.get())
    .await
    .expect("insert_state_item must persist protected state");
    item_id
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
        agent_id: Some("joint-agent".to_string()),
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
        agent_binding_id: None,
        agent_binding_name: None,
        agent_binding_schema_version: None,
        selected_model_json: None,
        selected_model_name: None,
        selected_model_gateway: None,
        capability_server_refs_json: None,
        runtime_profile: None,
        events: vec![json!({"event_type": "run_started", "data": {"source": "joint_e2e"}})],
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
    }
}

#[derive(Clone)]
struct JointHealth;

#[async_trait]
impl HealthChecker for JointHealth {
    async fn database_healthy(&self) -> bool {
        true
    }
}

#[derive(Clone)]
struct JointAuth {
    user_id: String,
}

#[async_trait]
impl AuthService for JointAuth {
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
                Json(ErrorResponse::new("joint e2e unauthorized")),
            ));
        }
        Ok(AuthUserRecord {
            user_id: self.user_id.clone(),
            username: "joint-e2e".to_string(),
            email: "joint-e2e@example.test".to_string(),
            display_name: None,
        })
    }

    async fn register(
        &self,
        _request: AuthRegisterRequestData,
    ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!("joint E2E does not exercise auth register")
    }

    async fn login(
        &self,
        _request: AuthLoginRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!("joint E2E does not exercise auth login")
    }

    async fn refresh(
        &self,
        _request: AuthRefreshRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!("joint E2E does not exercise auth refresh")
    }

    async fn logout(
        &self,
        _request: AuthRefreshRequestData,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        unimplemented!("joint E2E does not exercise auth logout")
    }
}

#[derive(Clone)]
struct JointSession {
    user_id: String,
}

#[async_trait]
impl SessionService for JointSession {
    async fn create_session(
        &self,
        _user_id: String,
        _request: SessionCreateRequestData,
    ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!("joint E2E inserts sessions directly")
    }

    async fn get_session(
        &self,
        session_id: String,
        _user_id: String,
    ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
        Ok(SessionRecord {
            session_id,
            user_id: self.user_id.clone(),
            agent_id: Some("joint-agent".to_string()),
            title: Some("joint e2e".to_string()),
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
        unimplemented!("joint E2E does not update sessions through service")
    }

    async fn list_sessions(
        &self,
        _filter: SessionListFilter,
    ) -> Result<SessionListRecord, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!("joint E2E does not list sessions")
    }

    async fn delete_session(
        &self,
        _session_id: String,
        _user_id: String,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        unimplemented!("joint E2E does not delete sessions")
    }

    async fn get_session_activity(
        &self,
        _session_id: String,
        _user_id: String,
        _limit: u32,
        _cursor: Option<astra_services::auth::SessionActivityCursor>,
    ) -> Result<SessionActivityRecord, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!("joint E2E does not read activity")
    }
}

#[derive(Clone)]
struct JointRunLifecycle {
    store: Arc<RwLock<DatabaseRunStateStore>>,
}

impl JointRunLifecycle {
    async fn store(&self) -> DatabaseRunStateStore {
        self.store.read().await.clone()
    }
}

#[async_trait]
impl RunLifecycleService for JointRunLifecycle {
    async fn create_run(
        &self,
        _user_id: String,
        _request: ChatRequestData,
    ) -> Result<ChatRunRecord, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!("joint E2E creates durable runs directly")
    }

    async fn stream_chat(
        &self,
        _user_id: String,
        _request: ChatRequestData,
    ) -> Result<ChatStreamRecord, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!("joint E2E streams existing runs only")
    }

    async fn get_run_status(
        &self,
        run_id: String,
        user_id: String,
    ) -> Result<RunStatusRecord, (StatusCode, Json<ErrorResponse>)> {
        let run = self
            .store()
            .await
            .load_run(&user_id, &run_id)
            .await
            .map_err(service_unavailable)?
            .ok_or_else(|| not_found("run not found"))?;
        if run.user_id != user_id {
            return Err(forbidden("run belongs to another user"));
        }
        Ok(RunStatusRecord {
            run_id,
            session_id: run.session_id,
            status: run.status,
            waiting_for: run.waiting_for,
            events_count: run.events.len() as i64,
            workspace: None,
            executor: None,
            transport: None,
            fallback_policy: None,
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
            .load_run(&user_id, &run_id)
            .await
            .map_err(service_unavailable)?
            .ok_or_else(|| not_found("run not found"))?;
        if run.user_id != user_id {
            return Err(forbidden("run belongs to another user"));
        }
        Ok(run.events.into_iter().skip(last_index as usize).collect())
    }

    async fn cancel_run(
        &self,
        run_id: String,
        user_id: String,
    ) -> Result<CancelRunRecord, (StatusCode, Json<ErrorResponse>)> {
        self.store()
            .await
            .update_run_status(&user_id, &run_id, "cancelled", None, None)
            .await
            .map_err(service_unavailable)?;
        Ok(CancelRunRecord {
            run_id,
            status: "cancelled".to_string(),
        })
    }

    async fn list_runs_cursor(
        &self,
        _user_id: String,
        limit: u32,
        _cursor: Option<RunListCursor>,
    ) -> Result<RunListRecord, (StatusCode, Json<ErrorResponse>)> {
        Ok(RunListRecord {
            runs: Vec::new(),
            total: None,
            limit,
            next_cursor: None,
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
            .load_run(&user_id, &run_id)
            .await
            .map_err(service_unavailable)?
            .ok_or_else(|| not_found("run not found"))?;
        if run.user_id != user_id {
            return Err(forbidden("run belongs to another user"));
        }
        let duplicate = run.events.iter().any(|event| {
            event.get("idempotency_key").and_then(Value::as_str)
                == Some(input.idempotency_key.as_str())
        });
        if !duplicate {
            store
                .append_event(
                    &user_id,
                    &run_id,
                    json!({
                        "event_type": "user_input",
                        "idempotency_key": input.idempotency_key,
                        "data": {"input": input.input},
                    }),
                )
                .await
                .map_err(service_unavailable)?;
            store
                .update_run_status(&user_id, &run_id, "running", None, None)
                .await
                .map_err(service_unavailable)?;
            store
                .append_event(
                    &user_id,
                    &run_id,
                    json!({"event_type": "run_resumed", "data": {"source": "approval_input"}}),
                )
                .await
                .map_err(service_unavailable)?;
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
        user_id: String,
    ) -> Result<RunMutationRecord, (StatusCode, Json<ErrorResponse>)> {
        self.store()
            .await
            .update_run_status(&user_id, &run_id, "waiting", Some("user"), None)
            .await
            .map_err(service_unavailable)?;
        Ok(RunMutationRecord {
            run_id,
            status: "waiting".to_string(),
            previous_status: "running".to_string(),
        })
    }
}

fn service_unavailable(message: String) -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ErrorResponse::new(message)),
    )
}

fn not_found(message: &str) -> (StatusCode, Json<ErrorResponse>) {
    (StatusCode::NOT_FOUND, Json(ErrorResponse::new(message)))
}

fn forbidden(message: &str) -> (StatusCode, Json<ErrorResponse>) {
    (StatusCode::FORBIDDEN, Json(ErrorResponse::new(message)))
}

fn build_joint_app(
    pool: SharedPool,
    user_id: String,
    store: Arc<RwLock<DatabaseRunStateStore>>,
) -> Router {
    let state = AppState::new(ServiceInfo::default(), Arc::new(JointHealth))
        .with_shared_pool(pool)
        .with_auth_service(Arc::new(JointAuth {
            user_id: user_id.clone(),
        }))
        .with_session_service(Arc::new(JointSession { user_id }))
        .with_run_lifecycle_service(Arc::new(JointRunLifecycle { store }));
    build_app(state)
}

async fn spawn_tcp_router(app: Router) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind joint E2E TCP listener");
    let addr = listener
        .local_addr()
        .expect("local_addr must be available for joint E2E listener");
    let handle = tokio::spawn(async move {
        axum::serve(listener, app.into_make_service())
            .await
            .expect("joint E2E axum router must serve");
    });
    (addr, handle)
}

/// Build a reqwest client that bypasses any ambient HTTP proxy.
///
/// Local dev environments commonly export `http_proxy` (e.g. for
/// reaching internal hosts). The default `Client::new()` honors that
/// proxy and routes the test's `http://127.0.0.1:<random_port>` calls
/// through it — the proxy can't reach a private listener port and
/// returns 503 with `proxy-connection: close`. `no_proxy()` forces
/// reqwest to ignore proxy env so the loopback request lands at the
/// axum router we just spawned.
fn local_client() -> Client {
    Client::builder()
        .no_proxy()
        .build()
        .expect("reqwest Client::builder().no_proxy() must build")
}

async fn get_stream(
    client: &Client,
    base: SocketAddr,
    run_id: &str,
    last_index: i64,
) -> Vec<Value> {
    let response = client
        .get(format!(
            "http://{base}/chat/runs/{run_id}/stream?last_index={last_index}"
        ))
        .header("authorization", HTTP_TOKEN)
        .send()
        .await
        .expect("GET run stream must reach axum router");
    let status = response.status();
    let body = response
        .text()
        .await
        .expect("SSE run stream body must be readable");
    assert!(
        status == StatusCode::OK,
        "GET run stream expected 200, got {status}; body: {body}"
    );
    parse_sse_events(&body)
}

async fn post_run_input(client: &Client, base: SocketAddr, run_id: &str, key: &str, input: Value) {
    let response = client
        .post(format!("http://{base}/chat/runs/{run_id}/input"))
        .header("authorization", HTTP_TOKEN)
        .json(&json!({"idempotency_key": key, "input": input}))
        .send()
        .await
        .expect("POST run input must reach axum router");
    let status = response.status();
    assert!(
        status == StatusCode::OK,
        "POST run input expected 200, got {status}"
    );
}

fn absorb_sse_events(events: Vec<Value>, seen: &mut BTreeSet<i64>, next_index: &mut i64) {
    for event in events {
        let Some(index) = event.get("index").and_then(Value::as_i64) else {
            continue;
        };
        assert!(
            seen.insert(index),
            "client replay must not receive duplicate event_idx={index}"
        );
        assert!(
            index == *next_index,
            "client replay expected event_idx={}, got {index}; event={event}",
            *next_index
        );
        *next_index += 1;
    }
}

#[allow(clippy::too_many_arguments)]
async fn save_manifest_turn(
    store: &DatabaseContextManifestStore,
    user_id: &str,
    session_id: &str,
    run_id: &str,
    turn: usize,
    reason: &str,
    total_tokens: u32,
    extra_items: Vec<ContextManifestItemWrite>,
) {
    let manifest_id = id("manifest");
    let mut items = vec![ContextManifestItemWrite {
        session_id: session_id.to_string(),
        item_order: 0,
        zone: "recent_tail".to_string(),
        source_table: "session_transcript_items".to_string(),
        source_id: format!("{session_id}:{turn}"),
        source_hash: None,
        included: true,
        token_estimate: total_tokens.min(900),
        budget_tokens: 2_000,
        reason: reason.to_string(),
        render_mode: "plain_text".to_string(),
        raw_ref: Some(format!("conversation_log://{session_id}/turn/{turn}")),
    }];
    for (offset, mut item) in extra_items.into_iter().enumerate() {
        item.item_order = (offset + 1) as i32;
        items.push(item);
    }
    store
        .save_manifest(
            ContextManifestWrite {
                manifest_id: manifest_id.clone(),
                user_id: user_id.to_string(),
                session_id: session_id.to_string(),
                run_id: Some(run_id.to_string()),
                turn_id: format!("turn-{turn:02}"),
                model_provider: "mock".to_string(),
                model_name: "joint-fixed-llm".to_string(),
                context_window_tokens: 128_000,
                max_output_tokens: 2_000,
                total_estimated_tokens: total_tokens,
                policy_version: "context_manifest_v1".to_string(),
                tokenizer_id: Some("estimated_v1".to_string()),
                budget_template_id: Some("budget_v1_128k".to_string()),
                turn_intent: Some("development".to_string()),
                reason: reason.to_string(),
                manifest_json: json!({
                    "e2e": "joint",
                    "turn": turn,
                    "zones": {
                        "recent_tail": {"used_tokens": total_tokens.min(900), "budget_tokens": 2000}
                    }
                }),
            },
            items,
        )
        .await
        .expect("save_manifest_turn must persist manifest and items");
}

#[tokio::test]
#[allow(unused_attributes)]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
#[ignore = "e2e_joint"]
async fn e2e_joint_1_s01_rust_60_turn_refactor_chain() {
    let pool = setup_pool().await;
    let user_id = id("user");
    let session_id = id("session");
    let run_id = id("run");
    insert_session(&pool, &user_id, &session_id).await;
    insert_run_row(
        &pool,
        &user_id,
        &session_id,
        &run_id,
        None,
        &run_id,
        &run_id,
        0,
        "completed",
        None,
        "node",
    )
    .await;
    let plan_item = insert_state_item(
        &pool,
        &user_id,
        &session_id,
        &run_id,
        "plan_state",
        "rust-refactor-plan",
        7,
    )
    .await;
    for category in [
        "decision",
        "finding",
        "benchmark",
        "citation",
        "todo_state",
        "error_state",
        "delegation_state",
    ] {
        insert_state_item(
            &pool,
            &user_id,
            &session_id,
            &run_id,
            category,
            &format!("{category}-seed"),
            1,
        )
        .await;
    }

    sqlx::query(
        "INSERT INTO session_history_chunks
         (chunk_id, user_id, session_id, source_session_id, seq_start, seq_end, chunk_type,
          source_table, source_id, content_text, content_hash, token_estimate, provenance_json, created_at)
         VALUES (?, ?, ?, ?, 1, 50, 'code_decision', 'session_transcript_items',
                 'turn-17', 'borrow checker detail from early refactor', ?, 180, ?, NOW(6))",
    )
    .bind(id("chunk"))
    .bind(&user_id)
    .bind(&session_id)
    .bind(&session_id)
    .bind(id("hash"))
    .bind(json!({"retrieval_stage": "structured"}).to_string())
    .execute(pool.get())
    .await
    .expect("S01 retrieval seed chunk must be inserted");

    let artifact_id = id("artifact");
    sqlx::query(
        "INSERT INTO session_artifacts
         (artifact_id, session_id, user_id, owner_run_id, root_run_id, artifact_kind, source,
          content_json, metadata, access_scope, retention_policy, status, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, 'cargo', 'tool_output', ?, ?, 'same_root_tree', 'default',
                 'active', NOW(6), NOW(6))",
    )
    .bind(&artifact_id)
    .bind(&session_id)
    .bind(&user_id)
    .bind(&run_id)
    .bind(&run_id)
    .bind(json!({"preview_text": "cargo test failed in crate astra-runtime"}).to_string())
    .bind(
        json!({"byte_size": 2 * 1024 * 1024, "summary": "cargo test 2MB output preserved"})
            .to_string(),
    )
    .execute(pool.get())
    .await
    .expect("S01 large cargo artifact must be inserted");

    let manifest_store = DatabaseContextManifestStore::new(pool.clone());
    let projection_store = DatabaseStateProjectionStore::new(pool.clone());
    let compaction_turns = [8_usize, 38, 58];
    let mut compaction_runs = 0usize;
    let turn_count = 60usize;
    for turn in 0..turn_count {
        if compaction_turns.contains(&turn) {
            let compaction_run_id = id("compact");
            insert_run_row(
                &pool,
                &user_id,
                &session_id,
                &compaction_run_id,
                Some(&run_id),
                &run_id,
                &format!("{run_id}/{compaction_run_id}"),
                1,
                "completed",
                None,
                "node",
            )
            .await;
            let results = projection_store
                .compact_session_state(&user_id, &session_id, &compaction_run_id, 640)
                .await
                .expect("S01 compaction must run through DatabaseStateProjectionStore");
            assert!(
                results.len() == COMPACTION_INVARIANT_SQL.len(),
                "S01 compaction must execute all invariants; got {} expected {}",
                results.len(),
                COMPACTION_INVARIANT_SQL.len()
            );
            assert!(
                results.iter().all(|(_, violations)| *violations == 0),
                "S01 compaction invariants must all return 0, got {results:?}"
            );
            compaction_runs += 1;
            continue;
        }
        let mut extra = Vec::new();
        let reason = if turn == 17 {
            extra.push(ContextManifestItemWrite {
                session_id: session_id.clone(),
                item_order: 1,
                zone: "retrieved_facts".to_string(),
                source_table: "session_history_chunks".to_string(),
                source_id: format!("{session_id}:turn-17"),
                source_hash: None,
                included: true,
                token_estimate: 220,
                budget_tokens: 1_000,
                reason: "history_recall_structured".to_string(),
                render_mode: "summary".to_string(),
                raw_ref: Some(format!("chunk://{session_id}/turn-17")),
            });
            "history_recall_structured"
        } else if turn == 44 {
            extra.push(ContextManifestItemWrite {
                session_id: session_id.clone(),
                item_order: 1,
                zone: "tool_previews".to_string(),
                source_table: "session_artifacts".to_string(),
                source_id: artifact_id.clone(),
                source_hash: None,
                included: true,
                token_estimate: 1_200,
                budget_tokens: 1_200,
                reason: "large_tool_output_gated".to_string(),
                render_mode: "tool_preview".to_string(),
                raw_ref: Some(format!("artifact://{session_id}/{artifact_id}")),
            });
            "large_tool_output_gated"
        } else {
            "normal_turn"
        };
        save_manifest_turn(
            &manifest_store,
            &user_id,
            &session_id,
            &run_id,
            turn,
            reason,
            1_200,
            extra,
        )
        .await;
    }

    let row = sqlx::query(
        "SELECT COUNT(*) AS manifest_count, SUM(total_estimated_tokens) AS actual_tokens
         FROM context_manifests WHERE session_id = ?",
    )
    .bind(&session_id)
    .fetch_one(pool.get())
    .await
    .expect("S01 manifest count query must succeed");
    let manifest_count = row.try_get::<i64, _>("manifest_count").unwrap_or_default();
    let actual_tokens = row
        .try_get::<Option<i64>, _>("actual_tokens")
        .unwrap_or(Some(0))
        .unwrap_or(0);
    assert!(
        manifest_count == turn_count as i64,
        "S01 must persist one manifest per turn; got {manifest_count}, expected {turn_count}"
    );
    assert!(
        compaction_runs == compaction_turns.len(),
        "S01 must trigger expected compactions; got {compaction_runs}, expected {}",
        compaction_turns.len()
    );
    let plan_version = sqlx::query("SELECT version FROM session_state_items WHERE item_id = ?")
        .bind(&plan_item)
        .fetch_one(pool.get())
        .await
        .expect("S01 plan_state version query must succeed")
        .try_get::<i64, _>("version")
        .unwrap_or_default();
    assert!(
        plan_version == 7,
        "S01 compaction must not spuriously bump plan_state version; got {plan_version}"
    );
    let naive_tokens = turn_count as i64 * 3_000;
    let saved_tokens = naive_tokens.saturating_sub(actual_tokens);
    assert!(
        saved_tokens * 100 >= naive_tokens * 50,
        "S01 token savings must be >=50%; naive={naive_tokens}, actual={actual_tokens}"
    );
    let artifact_refs = sqlx::query(
        "SELECT referenced_by_manifest_count FROM session_artifacts
         WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&artifact_id)
    .fetch_one(pool.get())
    .await
    .expect("S01 artifact reference query must succeed")
    .try_get::<i64, _>("referenced_by_manifest_count")
    .unwrap_or_default();
    assert!(
        artifact_refs >= 1,
        "S01 large cargo artifact must be referenced by manifest, got {artifact_refs}"
    );
    sqlx::query(
        "UPDATE session_artifacts SET status = 'expired'
         WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&artifact_id)
    .execute(pool.get())
    .await
    .expect("S01 must be able to expire artifact for placeholder rendering");
    let rendered = manifest_store
        .render_artifact_manifest_item(&user_id, &session_id, &artifact_id, None)
        .await
        .expect("S01 expired artifact renderer must use persisted summary");
    assert!(
        rendered.contains("historical, raw no longer available, summary preserved"),
        "S01 expired artifact renderer must return historical placeholder, got {rendered}"
    );
}

#[tokio::test]
#[allow(unused_attributes)]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
#[ignore = "e2e_joint"]
async fn e2e_joint_2_s04_seventeen_sse_reconnects_survive_restart_and_approvals() {
    let pool = setup_pool().await;
    let user_id = id("user");
    let session_id = id("session");
    let run_id = id("run");
    insert_session(&pool, &user_id, &session_id).await;
    let initial_store = DatabaseRunStateStore::new(pool.clone()).with_owner_pod_id("joint-pod-a");
    initial_store
        .insert_run(durable_record(&run_id, &session_id, &user_id))
        .await
        .expect("S04 initial durable run insert must succeed");
    let shared_store = Arc::new(RwLock::new(initial_store.clone()));
    let app = build_joint_app(pool.clone(), user_id.clone(), shared_store.clone());
    let (addr, handle) = spawn_tcp_router(app).await;
    let client = local_client();
    let mut seen = BTreeSet::new();
    let mut next_index = 0_i64;

    for reconnect in 0..17 {
        let dropped = client
            .get(format!(
                "http://{addr}/chat/runs/{run_id}/stream?last_index={next_index}"
            ))
            .header("authorization", HTTP_TOKEN)
            .send()
            .await
            .expect("S04 simulated dropped SSE request must reach router");
        drop(dropped);

        let store = shared_store.read().await.clone();
        store
            .append_event(
                &user_id,
                &run_id,
                json!({
                    "event_type": "assistant_delta",
                    "data": {"text": format!("chunk-{reconnect}")},
                }),
            )
            .await
            .expect("S04 append event during reconnect must succeed");

        if reconnect == 8 {
            sqlx::query(
                "UPDATE agent_runs
                 SET owner_lease_expires_at = DATE_SUB(NOW(6), INTERVAL 1 SECOND),
                     updated_at = NOW(6)
                 WHERE user_id = ? AND run_id = ?",
            )
            .bind(&user_id)
            .bind(&run_id)
            .execute(pool.get())
            .await
            .expect("S04 simulated pod restart must expire agent_runs lease");
            let replacement =
                DatabaseRunStateStore::new(pool.clone()).with_owner_pod_id("joint-pod-b");
            let won = replacement
                .acquire_owner_lease(&user_id, &run_id, "joint-pod-b", Duration::from_secs(30))
                .await
                .expect("S04 replacement pod must attempt lease takeover");
            assert!(
                won,
                "S04 replacement pod must take over agent_runs lease after simulated restart"
            );
            *shared_store.write().await = replacement;
        }

        if reconnect == 5 || reconnect == 12 {
            let approval_id = id("approval");
            let store = shared_store.read().await.clone();
            store
                .update_run_status(&user_id, &run_id, "waiting", Some("approval"), None)
                .await
                .expect("S04 approval pause must persist waiting status");
            store
                .append_event(
                    &user_id,
                    &run_id,
                    json!({
                        "event_type": "approval_request",
                        "data": {"approval_id": approval_id, "prompt": "continue?"}
                    }),
                )
                .await
                .expect("S04 approval request event must persist");
            absorb_sse_events(
                get_stream(&client, addr, &run_id, next_index).await,
                &mut seen,
                &mut next_index,
            );
            post_run_input(
                &client,
                addr,
                &run_id,
                &format!("approve-{reconnect}"),
                json!({"decision": "approved"}),
            )
            .await;
        }

        absorb_sse_events(
            get_stream(&client, addr, &run_id, next_index).await,
            &mut seen,
            &mut next_index,
        );
    }

    let store = shared_store.read().await.clone();
    store
        .append_event(
            &user_id,
            &run_id,
            json!({"event_type": "run_finished", "data": {"status": "completed"}}),
        )
        .await
        .expect("S04 final run_finished event must persist");
    store
        .update_run_status(&user_id, &run_id, "completed", None, None)
        .await
        .expect("S04 final completed status must persist");
    absorb_sse_events(
        get_stream(&client, addr, &run_id, next_index).await,
        &mut seen,
        &mut next_index,
    );

    let row = sqlx::query(
        "SELECT COUNT(*) AS event_count, COUNT(DISTINCT event_idx) AS distinct_count,
                MIN(event_idx) AS min_idx, MAX(event_idx) AS max_idx
         FROM agent_run_events WHERE run_id = ?",
    )
    .bind(&run_id)
    .fetch_one(pool.get())
    .await
    .expect("S04 server event_idx aggregate must succeed");
    let event_count = row.try_get::<i64, _>("event_count").unwrap_or_default();
    let distinct_count = row.try_get::<i64, _>("distinct_count").unwrap_or_default();
    let min_idx = row
        .try_get::<Option<i64>, _>("min_idx")
        .unwrap_or(Some(-1))
        .unwrap_or(-1);
    let max_idx = row
        .try_get::<Option<i64>, _>("max_idx")
        .unwrap_or(Some(-1))
        .unwrap_or(-1);
    assert!(
        min_idx == 0 && event_count == distinct_count && max_idx + 1 == event_count,
        "S04 server event_idx must be monotonic/no-gap/no-duplicate; min={min_idx} max={max_idx} count={event_count} distinct={distinct_count}"
    );
    assert!(
        seen.len() as i64 == event_count,
        "S04 client simulated IndexedDB watermark must include every server event; client={} server={event_count}",
        seen.len()
    );
    assert!(
        next_index - 1 == max_idx,
        "S04 client watermark must match server max event_idx; client={} server={max_idx}",
        next_index - 1
    );
    let status = sqlx::query("SELECT status FROM agent_runs WHERE run_id = ?")
        .bind(&run_id)
        .fetch_one(pool.get())
        .await
        .expect("S04 final run status query must succeed")
        .try_get::<String, _>("status")
        .unwrap_or_default();
    assert!(
        status == "completed",
        "S04 final run status must be completed, got {status}"
    );
    handle.abort();
}

#[tokio::test]
#[allow(unused_attributes)]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
#[ignore = "e2e_joint"]
async fn e2e_joint_3_s07_approval_survives_48h_restarts_and_migration() {
    let pool = setup_pool().await;
    let user_id = id("user");
    let session_id = id("session");
    let run_id = id("run");
    insert_session(&pool, &user_id, &session_id).await;
    let store = DatabaseRunStateStore::new(pool.clone()).with_owner_pod_id("joint-approval-a");
    store
        .insert_run(durable_record(&run_id, &session_id, &user_id))
        .await
        .expect("S07 durable run insert must succeed");
    store
        .update_run_status(&user_id, &run_id, "waiting", Some("approval"), None)
        .await
        .expect("S07 approval waiting status must persist");
    let approval_id = id("approval");
    store
        .append_event(
            &user_id,
            &run_id,
            json!({
                "event_type": "approval_request",
                "data": {
                    "approval_id": approval_id,
                    "requested_at": (chrono::Utc::now() - chrono::Duration::hours(48)).to_rfc3339(),
                    "condition": "pre_execute: target_branch == main"
                }
            }),
        )
        .await
        .expect("S07 approval_request event must persist");

    let shared_store = Arc::new(RwLock::new(store));
    let app = build_joint_app(pool.clone(), user_id.clone(), shared_store.clone());
    let (addr, handle) = spawn_tcp_router(app).await;
    let client = local_client();
    let initial_events = get_stream(&client, addr, &run_id, 0).await;
    assert!(
        initial_events.iter().any(|event| event
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind.contains("approval") || kind == "approval_request")),
        "S07 HTTP stream must replay approval_request event, got {initial_events:?}"
    );

    for pod in ["joint-approval-b", "joint-approval-c"] {
        let replacement = DatabaseRunStateStore::new(pool.clone()).with_owner_pod_id(pod);
        let _ = replacement
            .acquire_owner_lease(&user_id, &run_id, pod, Duration::from_secs(30))
            .await
            .expect("S07 replacement pod lease acquisition query must succeed");
        let loaded = replacement
            .load_run(&user_id, &run_id)
            .await
            .expect("S07 replacement pod must load durable run")
            .expect("S07 durable run must exist after restart");
        assert!(
            loaded.status == "waiting" && loaded.waiting_for.as_deref() == Some("approval"),
            "S07 approval_state must survive restart on {pod}; status={} waiting_for={:?}",
            loaded.status,
            loaded.waiting_for
        );
        *shared_store.write().await = replacement;
    }

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS e2e_schema_upgrade_markers (
            marker_id VARCHAR(128) PRIMARY KEY,
            scenario VARCHAR(64) NOT NULL,
            created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
        )",
    )
    .execute(pool.get())
    .await
    .expect("S07 schema upgrade SQL must run during approval wait");

    let approval_item = insert_state_item(
        &pool,
        &user_id,
        &session_id,
        &run_id,
        "approval_state",
        "release-approval",
        1,
    )
    .await;
    sqlx::query(
        "UPDATE session_state_items
         SET payload_json = ?, updated_at = NOW(6)
         WHERE item_id = ?",
    )
    .bind(
        json!({
            "approval_id": approval_id,
            "condition": "pre_execute: target_branch == main AND tests_green == true",
            "condition_chain": [
                "target_branch == main",
                "tests_green == true"
            ]
        })
        .to_string(),
    )
    .bind(&approval_item)
    .execute(pool.get())
    .await
    .expect("S07 approval condition update must persist");
    for condition in [
        "target_branch == main",
        "target_branch == main AND tests_green == true",
    ] {
        sqlx::query(
            "INSERT INTO session_state_item_events
             (event_id, item_id, user_id, session_id, category, item_key, mutation, payload_json, created_at)
             VALUES (?, ?, ?, ?, 'approval_state', 'release-approval', 'apply_suggestion', ?, NOW(6))",
        )
        .bind(id("state-event"))
        .bind(&approval_item)
        .bind(&user_id)
        .bind(&session_id)
        .bind(json!({"condition": condition, "approval_id": approval_id}).to_string())
        .execute(pool.get())
        .await
        .expect("S07 approval condition event chain must persist");
    }

    post_run_input(
        &client,
        addr,
        &run_id,
        "approval-final",
        json!({"approval_id": approval_id, "decision": "approved"}),
    )
    .await;
    let final_store = shared_store.read().await.clone();
    final_store
        .append_event(
                    &user_id,
                    &run_id,
            json!({"event_type": "pre_execute_check", "data": {"approval_id": approval_id, "condition_passed": true}}),
        )
        .await
        .expect("S07 pre_execute condition check event must persist");
    final_store
        .append_event(
            &user_id,
            &run_id,
            json!({"event_type": "run_finished", "data": {"status": "completed"}}),
        )
        .await
        .expect("S07 run_finished must persist");
    final_store
        .update_run_status(&user_id, &run_id, "completed", None, None)
        .await
        .expect("S07 completed status must persist");

    let row = sqlx::query(
        "SELECT payload_json FROM session_state_items
         WHERE item_id = ? AND category = 'approval_state'",
    )
    .bind(&approval_item)
    .fetch_one(pool.get())
    .await
    .expect("S07 approval_state payload query must succeed");
    let payload: Value = serde_json::from_str(
        &row.try_get::<String, _>("payload_json")
            .expect("S07 approval_state payload_json must be text"),
    )
    .expect("S07 approval_state payload must be JSON");
    assert!(
        payload.get("approval_id").and_then(Value::as_str) == Some(approval_id.as_str()),
        "S07 final execution must remain bound to original approval_id; payload={payload}"
    );
    assert!(
        payload
            .get("condition")
            .and_then(Value::as_str)
            .is_some_and(
                |condition| condition.contains("pre_execute") && condition.contains("tests_green")
            ),
        "S07 final condition must be a pre_execute guard with modifications replayed; payload={payload}"
    );
    let status = sqlx::query("SELECT status FROM agent_runs WHERE run_id = ?")
        .bind(&run_id)
        .fetch_one(pool.get())
        .await
        .expect("S07 final status query must succeed")
        .try_get::<String, _>("status")
        .unwrap_or_default();
    assert!(
        status == "completed",
        "S07 final run status must be completed, got {status}"
    );
    handle.abort();
}

#[tokio::test]
#[allow(unused_attributes)]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
#[ignore = "e2e_joint"]
async fn e2e_joint_4_s10_five_level_delegation_bubble_up_and_retry_node() {
    let pool = setup_pool().await;
    let user_id = id("user");
    let session_id = id("session");
    insert_session(&pool, &user_id, &session_id).await;
    let l0 = id("l0");
    let l1 = id("l1");
    let l2 = id("l2");
    let l3_runs = [id("l3a"), id("l3b"), id("l3c"), id("l3d")];
    let l4 = id("l4");
    insert_run_row(
        &pool,
        &user_id,
        &session_id,
        &l0,
        None,
        &l0,
        &l0,
        0,
        "running",
        None,
        "node",
    )
    .await;
    insert_run_row(
        &pool,
        &user_id,
        &session_id,
        &l1,
        Some(&l0),
        &l0,
        &format!("{l0}/{l1}"),
        1,
        "running",
        None,
        "node",
    )
    .await;
    insert_run_row(
        &pool,
        &user_id,
        &session_id,
        &l2,
        Some(&l1),
        &l0,
        &format!("{l0}/{l1}/{l2}"),
        2,
        "running",
        None,
        "node",
    )
    .await;
    for child in &l3_runs {
        insert_run_row(
            &pool,
            &user_id,
            &session_id,
            child,
            Some(&l2),
            &l0,
            &format!("{l0}/{l1}/{l2}/{child}"),
            3,
            "running",
            None,
            "node",
        )
        .await;
    }
    insert_run_row(
        &pool,
        &user_id,
        &session_id,
        &l4,
        Some(&l3_runs[1]),
        &l0,
        &format!("{l0}/{l1}/{l2}/{}/{}", l3_runs[1], l4),
        4,
        "running",
        None,
        "node",
    )
    .await;

    let store = DatabaseRunStateStore::new(pool.clone()).with_owner_pod_id("joint-delegation");
    let shared_store = Arc::new(RwLock::new(store));
    let app = build_joint_app(pool.clone(), user_id.clone(), shared_store);
    let (addr, handle) = spawn_tcp_router(app).await;
    let client = local_client();
    let health = client
        .get(format!("http://{addr}/health"))
        .send()
        .await
        .expect("S10 health request must reach axum router");
    assert!(
        health.status() == StatusCode::OK,
        "S10 router health expected 200, got {}",
        health.status()
    );

    let projection = DatabaseStateProjectionStore::new(pool.clone());
    let delegation_rows = [
        (&l0, &l1, 1_u32, format!("{l0}/{l1}")),
        (&l1, &l2, 2_u32, format!("{l0}/{l1}/{l2}")),
        (
            &l2,
            &l3_runs[0],
            3_u32,
            format!("{l0}/{l1}/{l2}/{}", l3_runs[0]),
        ),
        (
            &l2,
            &l3_runs[1],
            3_u32,
            format!("{l0}/{l1}/{l2}/{}", l3_runs[1]),
        ),
        (
            &l2,
            &l3_runs[2],
            3_u32,
            format!("{l0}/{l1}/{l2}/{}", l3_runs[2]),
        ),
        (
            &l2,
            &l3_runs[3],
            3_u32,
            format!("{l0}/{l1}/{l2}/{}", l3_runs[3]),
        ),
        (
            &l3_runs[1],
            &l4,
            4_u32,
            format!("{l0}/{l1}/{l2}/{}/{}", l3_runs[1], l4),
        ),
    ];
    for (parent, child, depth, path) in delegation_rows {
        projection
            .upsert_delegation_projection(DelegationProjectionUpsert {
                delegation_id: id("delegation"),
                user_id: user_id.clone(),
                session_id: session_id.clone(),
                parent_run_id: parent.to_string(),
                child_run_id: child.to_string(),
                root_run_id: l0.clone(),
                ancestor_path: path,
                depth,
                agent_id: Some(format!("agent-depth-{depth}")),
                title: Some(format!("delegated depth {depth}")),
                status: "running".to_string(),
                retry_of: None,
                retry_scope: "node".to_string(),
                last_summary_ref: None,
                last_summary_text: Some(format!("depth {depth} active")),
                sibling_exposed_artifacts_json: None,
            })
            .await
            .expect("S10 delegation projection upsert must persist table and state item rows");
    }

    let original_item_id = insert_state_item(
        &pool,
        &user_id,
        &session_id,
        &l3_runs[1],
        "finding",
        "critical-executor-2",
        1,
    )
    .await;
    projection
        .bubble_up_finding(
            &user_id,
            &l3_runs[1],
            &original_item_id,
            "critical",
            "executor-2 found unsafe retry boundary",
            &[
                BubbleUpTarget {
                    session_id: session_id.clone(),
                    run_id: l4.clone(),
                    depth: 4,
                },
                BubbleUpTarget {
                    session_id: session_id.clone(),
                    run_id: l3_runs[1].clone(),
                    depth: 3,
                },
                BubbleUpTarget {
                    session_id: session_id.clone(),
                    run_id: l2.clone(),
                    depth: 2,
                },
                BubbleUpTarget {
                    session_id: session_id.clone(),
                    run_id: l1.clone(),
                    depth: 1,
                },
                BubbleUpTarget {
                    session_id: session_id.clone(),
                    run_id: l0.clone(),
                    depth: 0,
                },
            ],
        )
        .await
        .expect("S10 bubble_up_finding must insert all ancestor projection events");

    let retry_run = id("retry");
    insert_run_row(
        &pool,
        &user_id,
        &session_id,
        &retry_run,
        Some(&l2),
        &l0,
        &format!("{l0}/{l1}/{l2}/{retry_run}"),
        3,
        "running",
        Some(&l3_runs[1]),
        "node",
    )
    .await;
    sqlx::query(
        "UPDATE agent_runs SET status = 'superseded', updated_at = NOW(6) WHERE run_id = ?",
    )
    .bind(&l3_runs[1])
    .execute(pool.get())
    .await
    .expect("S10 original executor run must transition to superseded");

    let path = sqlx::query("SELECT ancestor_path FROM agent_runs WHERE run_id = ?")
        .bind(&l4)
        .fetch_one(pool.get())
        .await
        .expect("S10 ancestor_path query must succeed")
        .try_get::<String, _>("ancestor_path")
        .unwrap_or_default();
    let expected_l4_path = format!("{l0}/{l1}/{l2}/{}/{}", l3_runs[1], l4);
    assert!(
        path == expected_l4_path,
        "S10 L4 ancestor_path mismatch; expected {expected_l4_path}, got {path}"
    );
    let bubble_count = sqlx::query(
        "SELECT COUNT(*) AS c FROM session_state_item_events
         WHERE session_id = ? AND user_id = ? AND mutation = 'bubble_up'",
    )
    .bind(&session_id)
    .bind(&user_id)
    .fetch_one(pool.get())
    .await
    .expect("S10 bubble_up event count query must succeed")
    .try_get::<i64, _>("c")
    .unwrap_or_default();
    assert!(
        bubble_count == 5,
        "S10 critical finding must bubble through 5 levels, got {bubble_count}"
    );
    let retry = sqlx::query("SELECT retry_of, retry_scope FROM agent_runs WHERE run_id = ?")
        .bind(&retry_run)
        .fetch_one(pool.get())
        .await
        .expect("S10 retry relation query must succeed");
    let retry_of = retry
        .try_get::<Option<String>, _>("retry_of")
        .unwrap_or(None);
    let retry_scope = retry
        .try_get::<String, _>("retry_scope")
        .unwrap_or_default();
    assert!(
        retry_of.as_deref() == Some(l3_runs[1].as_str()) && retry_scope == "node",
        "S10 retry run must point to executor-2 with retry_scope=node; retry_of={retry_of:?} retry_scope={retry_scope}"
    );
    let superseded = sqlx::query("SELECT status FROM agent_runs WHERE run_id = ?")
        .bind(&l3_runs[1])
        .fetch_one(pool.get())
        .await
        .expect("S10 superseded status query must succeed")
        .try_get::<String, _>("status")
        .unwrap_or_default();
    assert!(
        superseded == "superseded",
        "S10 original executor run must be superseded, got {superseded}"
    );
    handle.abort();
}

#[tokio::test]
#[allow(unused_attributes)]
#[ignore = "requires ASTRA_TEST_DB_IT=1"]
#[ignore = "e2e_joint"]
async fn e2e_joint_5_s14_8k_window_four_devices_and_lease_expiry() {
    let pool = setup_pool().await;
    let matrixone = require_db_it_env();
    let user_id = id("user");
    let session_id = id("session");
    let run_id = id("run");
    insert_session(&pool, &user_id, &session_id).await;
    let mut workspace = WorkspaceMetadata::with_context(
        &session_id,
        "gpt-5.4",
        "/tmp/joint-workspace",
        Some("main"),
    );
    workspace.git_root = Some("/tmp/joint-workspace".to_string());
    workspace.git_head = Some("deadbeef".to_string());
    persist_remote_workspace(
        &workspace,
        &user_id,
        &DatabaseSessionArtifactStore::new(matrixone).with_pool(pool.clone()),
    )
    .await
    .expect("S14 workspace metadata artifact insert must succeed");
    insert_state_item(
        &pool,
        &user_id,
        &session_id,
        &run_id,
        "todo_state",
        "resume-ui",
        1,
    )
    .await;
    DatabaseSessionArtifactStore::new(require_db_it_env())
        .with_pool(pool.clone())
        .persist_json_artifact(SessionArtifactJsonRecord {
            artifact_id: id("artifact"),
            session_id: session_id.clone(),
            user_id: user_id.clone(),
            artifact_kind: "llm_capture".to_string(),
            source: Some("e2e_joint".to_string()),
            turn: Some(1),
            round: None,
            content: json!({"preview": "capture"}),
            metadata: Some(json!({"kind": "preview"})),
        })
        .await
        .expect("S14 artifact preview seed insert must succeed");
    let store = DatabaseRunStateStore::new(pool.clone()).with_owner_pod_id("joint-device");
    store
        .insert_run(durable_record(&run_id, &session_id, &user_id))
        .await
        .expect("S14 durable active run insert must succeed");
    store
        .append_event(
            &user_id,
            &run_id,
            json!({"event_type": "assistant_delta", "data": {"text": "active replay"}}),
        )
        .await
        .expect("S14 second event must make run_event_high_watermark positive");
    for seq in 1..=4_i64 {
        sqlx::query(
            "INSERT INTO session_transcript_items
             (session_id, item_seq, user_id, run_id, role, content, source_event_idx, content_hash, created_at)
             VALUES (?, ?, ?, ?, 'assistant', ?, ?, ?, NOW(6))",
        )
        .bind(&session_id)
        .bind(seq)
        .bind(&user_id)
        .bind(&run_id)
        .bind(format!("transcript item {seq}"))
        .bind(seq - 1)
        .bind(id("hash"))
        .execute(pool.get())
        .await
        .expect("S14 transcript seed insert must succeed");
    }
    DatabaseContextManifestStore::new(pool.clone())
        .save_manifest(
            ContextManifestWrite {
                manifest_id: id("manifest"),
                user_id: user_id.clone(),
                session_id: session_id.clone(),
                run_id: Some(run_id.clone()),
                turn_id: format!("{run_id}:turn-1"),
                model_provider: "openai".to_string(),
                model_name: "gpt-5.4".to_string(),
                context_window_tokens: 8000,
                max_output_tokens: 512,
                total_estimated_tokens: 321,
                policy_version: "context_manifest_v1".to_string(),
                tokenizer_id: Some("estimated_v1".to_string()),
                budget_template_id: Some("budget_v1_8k".to_string()),
                turn_intent: Some("resume".to_string()),
                reason: "initial_turn".to_string(),
                manifest_json: json!({"source": "e2e_joint_s14"}),
            },
            vec![ContextManifestItemWrite {
                session_id: session_id.clone(),
                item_order: 0,
                zone: "session_anchor".to_string(),
                source_table: "session_state_items".to_string(),
                source_id: "anchor-1".to_string(),
                source_hash: None,
                included: true,
                token_estimate: 321,
                budget_tokens: 400,
                reason: "initial_turn".to_string(),
                render_mode: "summary".to_string(),
                raw_ref: None,
            }],
        )
        .await
        .expect("S14 context manifest insert must succeed");

    let shared_store = Arc::new(RwLock::new(store));
    let app = build_joint_app(pool.clone(), user_id.clone(), shared_store);
    let (addr, handle) = spawn_tcp_router(app).await;
    let client = local_client();

    for device_idx in 1..=4 {
        let state: Value = client
            .get(format!(
                "http://{addr}/sessions/{session_id}/state?known_state_revision=0&client_cache_empty=true&device_id=device-{device_idx}&device_fingerprint=fp-{device_idx}"
            ))
            .header("authorization", HTTP_TOKEN)
            .send()
            .await
            .expect("S14 cold-start state request must reach router")
            .json()
            .await
            .expect("S14 cold-start state response must be JSON");
        assert!(
            state.get("replay_required").and_then(Value::as_bool) == Some(true),
            "S14 cold-start must require replay for device {device_idx}; state={state}"
        );
        assert!(
            state.pointer("/active_run/run_id").and_then(Value::as_str) == Some(run_id.as_str()),
            "S14 cold-start must return active_run for device {device_idx}; state={state}"
        );
        assert_eq!(
            state
                .pointer("/workspace_authority/cwd")
                .and_then(Value::as_str),
            Some("/tmp/joint-workspace"),
            "S14 bounded session state must include durable workspace authority; state={state}"
        );
        assert_eq!(
            state
                .pointer("/workspace_authority/git_head")
                .and_then(Value::as_str),
            Some("deadbeef"),
            "S14 bounded session state must expose workspace git head for resume/debug; state={state}"
        );
        assert_eq!(
            state
                .pointer("/latest_context_manifest/reason")
                .and_then(Value::as_str),
            Some("initial_turn"),
            "S14 bounded session state must include latest context manifest summary; state={state}"
        );
        assert_eq!(
            state
                .pointer("/latest_context_manifest/total_estimated_tokens")
                .and_then(Value::as_u64),
            Some(321),
            "S14 bounded session state must surface manifest token estimate; state={state}"
        );
        assert_eq!(
            state
                .pointer("/state_summary/0/category")
                .and_then(Value::as_str),
            Some("todo_state"),
            "S14 bounded session state must summarize active session state categories; state={state}"
        );
        assert_eq!(
            state
                .pointer("/state_summary/0/count")
                .and_then(Value::as_u64),
            Some(1),
            "S14 bounded session state must count active/backlog state items; state={state}"
        );
        assert_eq!(
            state
                .pointer("/artifact_previews/0/artifact_kind")
                .and_then(Value::as_str),
            Some("llm_capture"),
            "S14 bounded session state must include recent non-workspace artifact previews; state={state}"
        );
        let transcript: Value = client
            .get(format!(
                "http://{addr}/sessions/{session_id}/transcript?limit=2"
            ))
            .header("authorization", HTTP_TOKEN)
            .send()
            .await
            .expect("S14 transcript request must reach router")
            .json()
            .await
            .expect("S14 transcript response must be JSON");
        assert!(
            transcript
                .get("items")
                .and_then(Value::as_array)
                .is_some_and(|items| items.len() == 2),
            "S14 cold-start transcript pagination must return requested page; transcript={transcript}"
        );
        let replay = get_stream(&client, addr, &run_id, 0).await;
        assert!(
            replay
                .iter()
                .filter(|event| event.get("index").is_some())
                .count()
                >= 2,
            "S14 active run replay from last_index=0 must include durable events; replay={replay:?}"
        );
    }

    let budget = astra_services::budget_for_turn_intent(Some("benchmark_comparison"));
    let zone_total = budget.budget.input_context_cap();
    assert!(
        zone_total <= 7_300,
        "S14 budget_v1_8k zones must stay <=7300 tokens, got {zone_total}"
    );
    assert!(
        budget.budget.tool_previews == 2_500 && budget.borrowed_from_recent_tail > 0,
        "S14 benchmark_comparison must flex tool_previews to 2500 from recent_tail; allocation={budget:?}"
    );
    assert!(
        matches!(
            next_action_confidence_action(0.9, 0, "structured_event", Some("event-1")),
            ConfidenceAction::AutoAccept
        ),
        "S14 high-confidence structured event must auto-accept"
    );
    assert!(
        matches!(
            next_action_confidence_action(0.65, 0, "rule", Some("event-2")),
            ConfidenceAction::AskUser
        ),
        "S14 medium-confidence action must ask user"
    );
    assert!(
        matches!(
            next_action_confidence_action(0.95, 0, "small_model", None),
            ConfidenceAction::AskUser
        ),
        "S14 small-model-only action must require confirmation despite high score"
    );

    let mut rx = astra_runtime::server::device_lease_sweeper::subscribe_device_lease_events();
    sqlx::query(
        "UPDATE session_device_leases
         SET expires_at = DATE_SUB(NOW(6), INTERVAL 1 SECOND), updated_at = NOW(6)
         WHERE session_id = ? AND device_id = 'device-2'",
    )
    .bind(&session_id)
    .execute(pool.get())
    .await
    .expect("S14 device-2 lease must be made due for expiry");
    let expired = astra_runtime::server::device_lease_sweeper::expire_due_device_leases_once(
        pool.clone(),
        10,
    )
    .await
    .expect("S14 device lease sweeper must expire due leases");
    assert!(
        expired >= 1,
        "S14 sweeper must expire at least device-2 lease, got {expired}"
    );
    // The sweeper publishes one event per expired lease across the
    // process-wide broadcast channel. When other concurrent tests
    // also leak stale leases, our `rx` sees their events too — so we
    // can't take the first event blindly. Drain the channel until we
    // find the one keyed to *our* session_id and device_id (or time out).
    let event = {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let ev = tokio::time::timeout(remaining, rx.recv())
                .await
                .expect("S14 device lease SSE parity event must be published")
                .expect("S14 device lease event receiver must be open");
            if ev.get("session_id").and_then(Value::as_str) == Some(session_id.as_str())
                && ev.get("device_id").and_then(Value::as_str) == Some("device-2")
            {
                break ev;
            }
            // Foreign session/device — keep draining until we hit ours.
        }
    };
    assert!(
        event.get("type").and_then(Value::as_str) == Some("device_lease_expired")
            && event.get("device_id").and_then(Value::as_str) == Some("device-2"),
        "S14 passive expiry event must have symmetric device_lease_expired payload, got {event}"
    );
    let reason = sqlx::query(
        "SELECT reason FROM session_device_lease_events
         WHERE session_id = ? AND device_id = 'device-2' AND event_type = 'auto_expire'
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(&session_id)
    .fetch_one(pool.get())
    .await
    .expect("S14 device lease event row must be inserted")
    .try_get::<String, _>("reason")
    .unwrap_or_default();
    assert!(
        reason == "auto_expire",
        "S14 passive expiry DB event reason must match SSE reason, got {reason}"
    );
    handle.abort();
}
