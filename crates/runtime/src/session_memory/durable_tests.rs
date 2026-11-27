//! Real MatrixOne journal plus a strict loopback backend at public HTTP paths.
use super::*;
use crate::isolated_database;
use astra_core::{MatrixOneSettings, SharedPool};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::{delete, get, post},
};
use std::{collections::BTreeMap, sync::Mutex};

#[derive(Default)]
struct Backend {
    owner: String,
    session: String,
    snapshot: Value,
    inventory: Option<Vec<Value>>,
    working_active: bool,
    writes: BTreeMap<String, Value>,
    calls: BTreeMap<String, usize>,
    lose_episode_receipt: bool,
    lose_scene_receipt: bool,
    lose_delete_receipt: bool,
    fail_reflection: bool,
    unsupported_idempotency: bool,
}
fn assert_authority(headers: &HeaderMap, backend: &Backend) {
    assert_eq!(
        headers["authorization"],
        "Memoria-Owner fixture-deployment-key"
    );
    assert_eq!(headers["x-user-id"], backend.owner);
}
async fn store_memory(
    State(state): State<Arc<Mutex<Backend>>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let mut backend = state.lock().unwrap();
    assert_authority(&headers, &backend);
    assert_eq!(body["session_id"], backend.session);
    assert_eq!(body["branch"], "main");
    if backend.unsupported_idempotency {
        return (StatusCode::NOT_FOUND, Json(json!({})));
    }
    let kind = body["memory_type"].as_str().unwrap().to_string();
    *backend.calls.entry(kind.clone()).or_default() += 1;
    let key = body["idempotency_key"].as_str().unwrap();
    let digest = Sha256::digest(
        serde_json::to_vec(&json!(["memoria.store.v1", backend.owner, "main", key])).unwrap(),
    );
    let id: String = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let memory = json!({"memory_id":id,"user_id":backend.owner,"session_id":backend.session,"memory_type":kind,"content":body["content"]});
    if let Some(existing) = backend.writes.get(&id) {
        assert_eq!(existing, &memory);
    } else {
        backend.writes.insert(id.clone(), memory.clone());
        if kind == "working"
            && let Some(inventory) = backend.inventory.as_mut()
        {
            inventory.push(memory.clone());
            inventory.sort_by(|a, b| b["memory_id"].as_str().cmp(&a["memory_id"].as_str()));
        }
    }
    if (kind == "episodic" && std::mem::take(&mut backend.lose_episode_receipt))
        || (kind == "semantic" && std::mem::take(&mut backend.lose_scene_receipt))
    {
        return (StatusCode::CREATED, Json(json!({})));
    }
    (StatusCode::CREATED, Json(memory))
}
async fn list_memory(
    State(state): State<Arc<Mutex<Backend>>>,
    Query(query): Query<BTreeMap<String, String>>,
    headers: HeaderMap,
) -> Json<Value> {
    let mut backend = state.lock().unwrap();
    assert_authority(&headers, &backend);
    assert_eq!(query.get("branch").map(String::as_str), Some("main"));
    *backend.calls.entry("list".into()).or_default() += 1;
    if let Some(inventory) = backend.inventory.as_ref() {
        assert_eq!(query["session_id"], backend.session);
        assert_eq!(query["memory_type"], "working");
        let offset = query
            .get("cursor")
            .map(|cursor| {
                inventory
                    .iter()
                    .position(|row| row["memory_id"] == *cursor)
                    .unwrap()
                    + 1
            })
            .unwrap_or(0);
        let limit: usize = query["limit"].parse().unwrap();
        let items: Vec<_> = inventory.iter().skip(offset).take(limit).cloned().collect();
        let next = if offset + items.len() < inventory.len() {
            items.last().unwrap()["memory_id"].clone()
        } else {
            Value::Null
        };
        return Json(json!({"items":items,"next_cursor":next}));
    }
    Json(
        json!({"items":if backend.working_active {vec![backend.snapshot.clone()]} else {vec![]},"next_cursor":null}),
    )
}
async fn delete_memory(
    State(state): State<Arc<Mutex<Backend>>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<BTreeMap<String, String>>,
) -> StatusCode {
    let mut backend = state.lock().unwrap();
    assert_authority(&headers, &backend);
    assert_eq!(query.get("branch").map(String::as_str), Some("main"));
    *backend.calls.entry("delete".into()).or_default() += 1;
    let removed = if let Some(inventory) = backend.inventory.as_mut() {
        if let Some(position) = inventory
            .iter()
            .position(|memory| memory["memory_id"] == id)
        {
            inventory.remove(position);
            true
        } else {
            false
        }
    } else {
        assert_eq!(id, "working-final");
        std::mem::replace(&mut backend.working_active, false)
    };
    if !removed {
        return StatusCode::NOT_FOUND;
    }
    if std::mem::take(&mut backend.lose_delete_receipt) {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::NO_CONTENT
    }
}
async fn reflect(
    State(state): State<Arc<Mutex<Backend>>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let mut backend = state.lock().unwrap();
    assert_authority(&headers, &backend);
    assert_eq!(body["branch"], "main");
    *backend.calls.entry("reflect".into()).or_default() += 1;
    if std::mem::take(&mut backend.fail_reflection) {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({})));
    }
    (
        StatusCode::OK,
        Json(
            json!({"candidates":[{"signal":"pattern","importance":0.8,"memories":[{"memory_id":"source-1","content":"Recurring final-turn evidence"}]}]}),
        ),
    )
}
fn snapshot(owner: &str, session: &str) -> Value {
    snapshot_at_turn(owner, session, 3)
}
fn snapshot_at_turn(owner: &str, session: &str, turn: u32) -> Value {
    let facts = astra_turn_types::session_facts::SessionFacts {
        turn,
        ..Default::default()
    };
    let detail = serde_json::to_string(&crate::session_memory::runner::SessionMemorySnapshot {
        schema_version: 2,
        session_id: session.into(),
        updated_turn: turn,
        facts,
        narrative: crate::session_memory::runner::SessionNarrative {
            task_spec: "Complete the final turn".into(),
            current_state: vec!["Final turn is durable".into()],
            ..Default::default()
        },
    })
    .unwrap();
    let content = astra_prompts::memory_proto::MemoryEntry::new_layered(
        "session",
        "active",
        "Final turn is durable",
        None,
        Some(&detail),
    )
    .encode();
    json!({"memory_id":"working-final","user_id":owner,"session_id":session,"memory_type":"working","content":content})
}

struct Fixture {
    pool: SharedPool,
    store: SessionMemoryLifecycleStore,
    user: String,
    session: String,
    backend: Arc<Mutex<Backend>>,
    inner: Arc<dyn MemoriaPort>,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
async fn fixture() -> Fixture {
    let settings = MatrixOneSettings::from_env();
    isolated_database::require_isolated_database(&settings.database);
    static SCHEMA: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    SCHEMA
        .get_or_init(|| async {
            astra_services::storage::ensure_core_schema(&settings, "mysql")
                .await
                .unwrap();
        })
        .await;
    let pool = SharedPool::new(&settings).await.unwrap();
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let user = format!("astra-{nonce}");
    let session = format!("session-{nonce}");
    let owner = format!("memoria-{nonce}");
    sqlx::query("INSERT INTO agent_sessions (user_id, session_id, status) VALUES (?, ?, 'active')")
        .bind(&user)
        .bind(&session)
        .execute(pool.get())
        .await
        .unwrap();
    let backend = Arc::new(Mutex::new(Backend {
        owner: owner.clone(),
        session: session.clone(),
        snapshot: snapshot(&owner, &session),
        working_active: true,
        ..Default::default()
    }));
    let app = Router::new()
        .route("/v1/memories/idempotent", post(store_memory))
        .route("/v1/memories", get(list_memory))
        .route("/v1/memories/{id}", delete(delete_memory))
        .route("/v1/reflect", post(reflect))
        .with_state(backend.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let inner = Arc::new(HttpMemoriaPort::from_transport(
        astra_memoria::MemoriaToolTransport {
            base_url: base,
            credential: "fixture-deployment-key".into(),
            owner_user_id: owner,
            owner_scoped_master: true,
        },
    )) as Arc<dyn MemoriaPort>;
    let store = SessionMemoryLifecycleStore::new(pool.clone());
    store.publish(&user, &session, json!({"session_facts":astra_turn_types::session_facts::SessionFacts {turn:3,..Default::default()}}), "generation-3".into()).await.unwrap();
    store.request_extraction(&user, &session).await.unwrap();
    let claim = store.claim(&user, &session).await.unwrap().unwrap();
    store.finish_extraction(&claim, true, 3).await.unwrap();
    Fixture {
        pool,
        store,
        user,
        session,
        backend,
        inner,
        server,
    }
}
async fn retry_now(f: &Fixture) {
    sqlx::query("UPDATE agent_session_memory_work SET next_attempt_at = NOW(6) WHERE user_id = ? AND session_id = ?").bind(&f.user).bind(&f.session).execute(f.pool.get()).await.unwrap();
}
async fn attempt(f: &Fixture) -> crate::turn::cloud::session_end_governance::SessionEndReport {
    retry_now(f).await;
    // Each attempt constructs a fresh service and transport to model restart.
    let store = SessionMemoryLifecycleStore::new(f.pool.clone());
    let claim = store.claim(&f.user, &f.session).await.unwrap().unwrap();
    let mut port = DurableMemoriaPort {
        inner: f.inner.clone(),
        store,
        claim,
    };
    let plan = port.prepare_plan().await.unwrap();
    port.claim.work.plan = Some(serde_json::to_value(&plan).unwrap());
    let report = crate::turn::cloud::session_end_governance::run_session_end_governance(
        &plan.facts,
        &f.session,
        &port,
    )
    .await
    .unwrap();
    port.finish_governance(&report).await.unwrap();
    report
}

#[tokio::test]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn restart_retries_lost_write_and_purge_receipts_without_duplicate_episodes_or_scenes() {
    let f = fixture().await;
    {
        let mut backend = f.backend.lock().unwrap();
        backend.lose_episode_receipt = true;
        backend.lose_delete_receipt = true;
        backend.fail_reflection = true;
        backend.lose_scene_receipt = true;
    }
    f.store
        .request_finalization(&f.user, &f.session)
        .await
        .unwrap();
    let first = attempt(&f).await;
    assert!(first.working_retained_due_to_episode_failure);
    assert!(first.reflection_failed);
    assert!(f.backend.lock().unwrap().working_active);
    let second = attempt(&f).await;
    assert!(second.working_purge_failed);
    assert_eq!(second.scene_persist_failures, 1);
    let third = attempt(&f).await;
    assert!(third.consolidation_complete());
    assert_eq!(third.scene_persist_failures, 0);
    let status = f.store.status(&f.user, &f.session).await.unwrap();
    assert_eq!(status["pending"], false);
    assert_eq!(status["consolidation_complete"], true);
    assert_eq!(status["optional_complete"], true);
    let sqlx::types::Json(work): sqlx::types::Json<SessionMemoryWork> = sqlx::query_scalar(
        "SELECT state_json FROM agent_session_memory_work WHERE user_id=? AND session_id=?",
    )
    .bind(&f.user)
    .bind(&f.session)
    .fetch_one(f.pool.get())
    .await
    .unwrap();
    assert!(work.plan.is_none());
    assert!(work.reflection.is_none());
    assert!(work.receipts.is_empty());
    assert!(work.purged_ids.is_empty());
    assert!(work.active_request.is_none());
    assert!(work.pending_request.is_none());
    assert!(work.latest_facts.is_some());
    assert!(work.last_report.is_some());
    // A repeated close/exit must not need the retired inventory or repeat HTTP.
    f.store
        .request_finalization(&f.user, &f.session)
        .await
        .unwrap();
    assert!(f.store.claim(&f.user, &f.session).await.unwrap().is_none());
    let backend = f.backend.lock().unwrap();
    assert_eq!(backend.writes.len(), 2);
    assert_eq!(backend.calls["episodic"], 2);
    assert_eq!(backend.calls["semantic"], 2);
    assert_eq!(backend.calls["delete"], 2);
    assert_eq!(backend.calls["reflect"], 2);
    assert_eq!(backend.calls["list"], 1);
}

#[tokio::test]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn optional_retry_preserves_consolidation_and_follow_up_invalidates_old_purge_plan() {
    let f = fixture().await;
    f.backend.lock().unwrap().fail_reflection = true;
    f.store
        .request_finalization(&f.user, &f.session)
        .await
        .unwrap();
    let report = attempt(&f).await;
    assert!(report.consolidation_complete());
    assert!(report.reflection_failed);
    let status = f.store.status(&f.user, &f.session).await.unwrap();
    assert_eq!(status["consolidation_complete"], true);
    assert_eq!(status["optional_complete"], false);
    let sqlx::types::Json(work): sqlx::types::Json<SessionMemoryWork> = sqlx::query_scalar(
        "SELECT state_json FROM agent_session_memory_work WHERE user_id=? AND session_id=?",
    )
    .bind(&f.user)
    .bind(&f.session)
    .fetch_one(f.pool.get())
    .await
    .unwrap();
    assert!(
        work.plan.is_some(),
        "optional retry still needs its frozen inputs"
    );
    assert!(!work.receipts.is_empty());
    let report = attempt(&f).await;
    assert!(report.consolidation_complete());
    {
        let backend = f.backend.lock().unwrap();
        assert_eq!(backend.calls["episodic"], 1);
        assert_eq!(backend.calls["delete"], 1);
    }
    let f = fixture().await;
    f.store
        .request_finalization(&f.user, &f.session)
        .await
        .unwrap();
    let claim = f.store.claim(&f.user, &f.session).await.unwrap().unwrap();
    let mut port = DurableMemoriaPort {
        inner: f.inner.clone(),
        store: f.store.clone(),
        claim,
    };
    let plan = port.prepare_plan().await.unwrap();
    port.claim.work.plan = Some(serde_json::to_value(&plan).unwrap());
    f.store
        .publish(
            &f.user,
            &f.session,
            json!({"session_facts":{"turn":4}}),
            "generation-4".into(),
        )
        .await
        .unwrap();
    assert!(port.purge_working(&f.session).await.is_err());
    assert!(f.backend.lock().unwrap().working_active);
    assert!(!f.backend.lock().unwrap().calls.contains_key("delete"));
}

struct Healthy;
#[async_trait::async_trait]
impl crate::HealthChecker for Healthy {
    async fn database_healthy(&self) -> bool {
        true
    }
}

#[tokio::test]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn public_exit_close_and_status_routes_preserve_ownership_and_durable_handoff() {
    use astra_services::{
        AuthService, DatabaseAuthService, SessionService,
        auth::{
            AuthLoginRequestData, AuthRegisterRequestData, DatabaseSessionService,
            SessionCreateRequestData,
        },
    };
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use tower::ServiceExt;
    let mut f = fixture().await;
    let transport = f.inner.resolve_tool_transport(true).await.unwrap().unwrap();
    let settings = MatrixOneSettings::from_env();
    let auth = Arc::new(
        DatabaseAuthService::new(
            settings.clone(),
            astra_core::JwtSettings {
                secret_key: "fixture-jwt-secret".into(),
                algorithm: "HS256".into(),
                access_token_expire_minutes: 15,
                refresh_token_expire_days: 7,
            },
        )
        .with_pool(f.pool.clone())
        .with_encryptor(
            astra_services::FernetTokenEncryptor::new("fixture-token-encryption").unwrap(),
        )
        .with_memoria_settings(&astra_core::MemoriaSettings {
            base_url: transport.base_url.clone(),
            master_key: Some("fixture-deployment-key".into()),
            self_hosted_master_access: true,
            issuer: None,
            web_url: None,
        })
        .unwrap(),
    );
    let username = format!("user_{}", uuid::Uuid::new_v4().simple());
    let user = auth
        .register(AuthRegisterRequestData {
            username: username.clone(),
            email: format!("{username}@example.test"),
            password: "fixture-passphrase".into(),
            display_name: None,
        })
        .await
        .unwrap();
    let tokens = auth
        .login(AuthLoginRequestData {
            username,
            password: "fixture-passphrase".into(),
        })
        .await
        .unwrap();
    let sessions = Arc::new(DatabaseSessionService::new(settings).with_pool(f.pool.clone()));
    let session = sessions
        .create_session(
            user.user_id.clone(),
            SessionCreateRequestData {
                agent_id: None,
                title: Some("Finalization regression".into()),
                metadata: None,
            },
        )
        .await
        .unwrap();
    f.user = user.user_id.clone();
    f.session = session.session_id.clone();
    {
        let mut backend = f.backend.lock().unwrap();
        backend.owner = f.user.clone();
        backend.session = f.session.clone();
        backend.snapshot = snapshot(&f.user, &f.session);
    }
    f.store.publish(&f.user,&f.session,json!({"session_facts":astra_turn_types::session_facts::SessionFacts {turn:3,..Default::default()}}),"generation-3".into()).await.unwrap();
    f.store
        .request_extraction(&f.user, &f.session)
        .await
        .unwrap();
    let claim = f.store.claim(&f.user, &f.session).await.unwrap().unwrap();
    f.store.finish_extraction(&claim, true, 3).await.unwrap();
    let state = crate::AppState::new(crate::ServiceInfo::default(), Arc::new(Healthy))
        .with_shared_pool(f.pool.clone())
        .with_auth_service(auth)
        .with_session_service(sessions.clone())
        .with_memoria_config(transport.base_url, Some("fixture-deployment-key".into()))
        .with_self_hosted_memoria_fallback(true);
    let app = crate::build_app(state.clone());
    let request = Request::builder()
        .method("POST")
        .uri(format!("/sessions/{}/memory/finalize", f.session))
        .header("authorization", format!("Bearer {}", tokens.access_token))
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(body["accepted"], true);
    assert_eq!(
        sessions
            .get_session(f.session.clone(), f.user.clone())
            .await
            .unwrap()
            .status,
        "active"
    );
    let request = Request::builder()
        .method("POST")
        .uri(format!("/sessions/{}/close", f.session))
        .header("authorization", format!("Bearer {}", tokens.access_token))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(request).await.unwrap().status(),
        StatusCode::OK
    );
    assert_eq!(
        sessions
            .get_session(f.session.clone(), f.user.clone())
            .await
            .unwrap()
            .status,
        "closed"
    );
    crate::server::session_memory_runtime::drive_session_memory_work(&state, &f.user, &f.session)
        .await
        .unwrap();
    let request = Request::builder()
        .uri(format!("/sessions/{}/memory/status", f.session))
        .header("authorization", format!("Bearer {}", tokens.access_token))
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(body["pending"], false);
    assert_eq!(body["consolidation_complete"], true);
    let request = Request::builder()
        .method("POST")
        .uri(format!("/sessions/{}/memory/finalize", f.session))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.oneshot(request).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
}

struct ReadGrantPort {
    inner: Arc<dyn MemoriaPort>,
    read_allowed: std::sync::atomic::AtomicBool,
    resolutions: Mutex<Vec<bool>>,
}

#[async_trait::async_trait]
impl MemoriaPort for ReadGrantPort {
    async fn resolve_tool_transport(
        &self,
        write: bool,
    ) -> Result<Option<astra_memoria::MemoriaToolTransport>, String> {
        self.resolutions.lock().unwrap().push(write);
        if !write && !self.read_allowed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("memory.read is disabled".into());
        }
        self.inner.resolve_tool_transport(write).await
    }

    async fn retrieve_ext(
        &self,
        _: &str,
        _: Option<&str>,
        _: usize,
        _: bool,
    ) -> Result<Vec<MemoriaMemory>, MemoriaOperationError> {
        panic!("finalization reads through the resolved HTTP transport")
    }
    async fn store(
        &self,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<String, String> {
        panic!("plan preparation must not write memories")
    }
    async fn purge_working(&self, _: &str) -> Result<u64, String> {
        panic!("plan preparation must not purge memories")
    }
}

#[tokio::test]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn finalization_snapshot_requires_read_grant_before_inventory_and_cached_plan() {
    use std::sync::atomic::Ordering;
    let f = fixture().await;
    f.store
        .request_finalization(&f.user, &f.session)
        .await
        .unwrap();
    let claim = f.store.claim(&f.user, &f.session).await.unwrap().unwrap();
    let authority = Arc::new(ReadGrantPort {
        inner: f.inner.clone(),
        read_allowed: false.into(),
        resolutions: Mutex::new(Vec::new()),
    });
    // A write-only owner retains its write transport; it cannot use that
    // owner-scoped deployment credential for an implicit snapshot read.
    assert!(
        authority
            .resolve_tool_transport(true)
            .await
            .unwrap()
            .is_some()
    );
    authority.resolutions.lock().unwrap().clear();
    let port = DurableMemoriaPort {
        inner: authority.clone(),
        store: f.store.clone(),
        claim,
    };
    assert!(
        port.prepare_plan().await.is_err(),
        "write-only access cannot read an inventory"
    );
    assert!(f.backend.lock().unwrap().calls.is_empty());
    assert!(
        f.store
            .load_claim_work(&port.claim)
            .await
            .unwrap()
            .plan
            .is_none()
    );
    assert_eq!(*authority.resolutions.lock().unwrap(), vec![false]);

    authority.read_allowed.store(true, Ordering::SeqCst);
    let plan = port.prepare_plan().await.unwrap();
    assert_eq!(plan.memories.len(), 1);
    assert!(
        f.store
            .load_claim_work(&port.claim)
            .await
            .unwrap()
            .plan
            .is_some()
    );
    assert_eq!(f.backend.lock().unwrap().calls.get("list"), Some(&1));

    authority.read_allowed.store(false, Ordering::SeqCst);
    assert!(
        port.prepare_plan().await.is_err(),
        "a frozen plan cannot bypass a revoked read grant"
    );
    assert_eq!(f.backend.lock().unwrap().calls.get("list"), Some(&1));
    assert!(port.reflect_session(&f.session, false).await.is_err());
    assert!(!f.backend.lock().unwrap().calls.contains_key("reflect"));
    assert_eq!(
        *authority.resolutions.lock().unwrap(),
        vec![false, false, false, false]
    );
}

#[tokio::test]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn stale_canonical_watermark_and_old_backend_never_authorize_working_cleanup() {
    let f = fixture().await;
    f.store
        .request_finalization(&f.user, &f.session)
        .await
        .unwrap();
    sqlx::query("INSERT INTO session_context_heads (isolation_domain,owner_user_id,session_id,branch_id,completed_turn) VALUES ('server', ?, ?, 'main', 4)")
        .bind(&f.user).bind(&f.session).execute(f.pool.get()).await.unwrap();
    let claim = f.store.claim(&f.user, &f.session).await.unwrap().unwrap();
    let port = DurableMemoriaPort {
        inner: f.inner.clone(),
        store: f.store.clone(),
        claim,
    };
    assert!(port.prepare_plan().await.is_err());
    assert!(f.backend.lock().unwrap().working_active);
    assert!(f.backend.lock().unwrap().writes.is_empty());
    let f = fixture().await;
    f.backend.lock().unwrap().unsupported_idempotency = true;
    f.store
        .request_finalization(&f.user, &f.session)
        .await
        .unwrap();
    let report = attempt(&f).await;
    assert!(report.working_retained_due_to_episode_failure);
    assert!(f.backend.lock().unwrap().working_active);
    assert!(f.backend.lock().unwrap().writes.is_empty());
}

#[tokio::test]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn finalization_freezes_all_pages_with_final_receipt_on_either_page() {
    for receipt_index in [0, 500] {
        let f = fixture().await;
        let receipt = {
            let mut backend = f.backend.lock().unwrap();
            let inventory: Vec<_> = (0..501)
                .rev()
                .map(|index| {
                    let mut memory = backend.snapshot.clone();
                    memory["memory_id"] = json!(format!("{index:032x}"));
                    memory
                })
                .collect();
            let receipt = inventory[receipt_index]["memory_id"]
                .as_str()
                .unwrap()
                .to_string();
            backend.inventory = Some(inventory);
            receipt
        };
        f.store
            .with_fenced_state(&f.user, &f.session, None, true, move |work| {
                Box::pin(async move {
                    work.working_memory_id = Some(receipt);
                    Ok(())
                })
            })
            .await
            .unwrap();
        f.store
            .request_finalization(&f.user, &f.session)
            .await
            .unwrap();
        let claim = f.store.claim(&f.user, &f.session).await.unwrap().unwrap();
        let port = DurableMemoriaPort {
            inner: f.inner.clone(),
            store: f.store.clone(),
            claim,
        };
        let plan = port.prepare_plan().await.unwrap();
        assert_eq!(plan.memories.len(), 501);
        let persisted = f.store.load_claim_work(&port.claim).await.unwrap();
        let frozen: FinalizationPlan = serde_json::from_value(persisted.plan.unwrap()).unwrap();
        assert_eq!(frozen.memories.len(), 501);
        assert!(
            frozen
                .memories
                .iter()
                .any(|memory| Some(&memory.memory_id) == persisted.working_memory_id.as_ref())
        );
        assert!(!persisted.consolidation_complete);
        let backend = f.backend.lock().unwrap();
        assert_eq!(backend.calls["list"], 2);
        assert!(!backend.calls.contains_key("delete"));
    }
}

#[tokio::test]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn inventory_overflow_keeps_plan_unfrozen_and_working_memory_pending() {
    let f = fixture().await;
    {
        let mut backend = f.backend.lock().unwrap();
        backend.inventory = Some(
            (0..4097)
                .rev()
                .map(|index| {
                    let mut memory = backend.snapshot.clone();
                    memory["memory_id"] = json!(format!("{index:032x}"));
                    memory
                })
                .collect(),
        );
    }
    f.store
        .request_finalization(&f.user, &f.session)
        .await
        .unwrap();
    let claim = f.store.claim(&f.user, &f.session).await.unwrap().unwrap();
    let port = DurableMemoriaPort {
        inner: f.inner.clone(),
        store: f.store.clone(),
        claim,
    };
    let error = port
        .prepare_plan()
        .await
        .err()
        .expect("overflow must retain working memory");
    assert!(error.contains("exceeds bounded inventory"));
    let work = f.store.load_claim_work(&port.claim).await.unwrap();
    assert!(work.plan.is_none());
    assert!(!work.consolidation_complete);
    assert!(work.pending());
    let backend = f.backend.lock().unwrap();
    assert!(backend.working_active);
    assert_eq!(backend.calls["list"], 9);
    assert!(!backend.calls.contains_key("delete"));
    assert!(backend.writes.is_empty());
}

async fn stale_cleanup_preserves_receipt_through_close_and_restart(lose_response: bool) {
    use crate::session_memory::runner::{ExtractionArtifacts, run_extraction};
    use astra_services::auth::{DatabaseSessionService, SessionService, SessionUpdateRequestData};
    let f = fixture().await;
    let stale_receipt = format!("{:032x}", u128::MAX);
    // The failure case fills the cleanup page with stale rows. The happy
    // control needs only one old row and avoids racing the one-second cleanup
    // budget with many database-fenced deletes on a loaded test host.
    let oldest_stale_turn = if lose_response { 1 } else { 16 };
    {
        let mut backend = f.backend.lock().unwrap();
        backend.inventory = Some(
            (oldest_stale_turn..=16)
                .rev()
                .map(|turn| {
                    let mut memory = snapshot_at_turn(&backend.owner, &backend.session, turn);
                    memory["memory_id"] =
                        json!(format!("{:032x}", u128::MAX - u128::from(16 - turn)));
                    memory
                })
                .collect(),
        );
        backend.lose_delete_receipt = lose_response;
    }
    let facts = astra_turn_types::session_facts::SessionFacts {
        turn: 17,
        ..Default::default()
    };
    f.store
        .publish(
            &f.user,
            &f.session,
            json!({"session_facts": facts}),
            "generation-17".into(),
        )
        .await
        .unwrap();
    f.store
        .request_extraction(&f.user, &f.session)
        .await
        .unwrap();
    let claim = f.store.claim(&f.user, &f.session).await.unwrap().unwrap();
    let port = Arc::new(DurableMemoriaPort {
        inner: f.inner.clone(),
        store: f.store.clone(),
        claim,
    });
    f.store
        .with_fenced_state(
            &f.user,
            &f.session,
            Some(&port.claim.token),
            false,
            |work| {
                Box::pin(async move {
                    work.working_memory_id = Some("previous-generation-receipt".into());
                    Ok(())
                })
            },
        )
        .await
        .unwrap();
    // Before this attempt writes, a typed read must still replace the previous
    // generation's receipt with the highest-turn observed snapshot.
    port.retrieve_scoped_typed("session memory", &f.session, 16, &["working"])
        .await
        .unwrap();
    assert_eq!(
        f.store
            .load_claim_work(&port.claim)
            .await
            .unwrap()
            .working_memory_id
            .as_deref(),
        Some(stale_receipt.as_str())
    );
    let memoria = port.clone() as Arc<dyn MemoriaPort>;
    let scope = astra_turn_types::InferenceInvocationScope::Session {
        session_id: f.session.clone(),
        turn: 17,
        round: 0,
        operation_id: "memory_extraction".into(),
        logical_attempt: 0,
    };
    // Execute the real canonical writer and its best-effort cleanup, using
    // the supported rule fallback rather than a paid inference provider.
    let result = run_extraction(
        &memoria,
        &scope,
        &[json!({"role":"user", "content":"Complete the final turn"})],
        17,
        "",
        &facts,
        &[] as &[crate::memory_hooks::DirectMemoryInferenceClient],
        Duration::from_secs(1),
        1024,
    )
    .await;
    let ExtractionArtifacts::Persisted {
        cleanup_error_reason,
        ..
    } = result
    else {
        panic!("a confirmed working write remains successful after cleanup failure");
    };
    assert_eq!(
        cleanup_error_reason,
        lose_response.then_some(
            astra_services::session_journal::SessionMemoryExtractionErrorReason::PurgeFailed
        ),
    );
    let actual_receipt = {
        let backend = f.backend.lock().unwrap();
        let id = backend
            .writes
            .values()
            .find(|memory| memory["memory_type"] == "working")
            .unwrap()["memory_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            id.as_str() < "fffffffffffffffffffffffffffffff0",
            "new receipt lies outside the 16-row cleanup page"
        );
        assert!(
            backend
                .inventory
                .as_ref()
                .unwrap()
                .iter()
                .any(|memory| memory["memory_id"] == id)
        );
        assert!(
            !backend
                .inventory
                .as_ref()
                .unwrap()
                .iter()
                .any(|memory| memory["memory_id"] == stale_receipt)
        );
        assert_eq!(backend.calls["delete"], 1);
        id
    };
    f.store
        .finish_extraction(&port.claim, true, 17)
        .await
        .unwrap();
    let work = f.store.status(&f.user, &f.session).await.unwrap();
    assert_eq!(work["extraction_pending"], false);
    DatabaseSessionService::new(MatrixOneSettings::from_env())
        .with_pool(f.pool.clone())
        .update_session(
            f.session.clone(),
            f.user.clone(),
            SessionUpdateRequestData {
                title: None,
                metadata: None,
                metadata_patch: None,
                status: Some("closed".into()),
            },
        )
        .await
        .unwrap();
    let restarted = SessionMemoryLifecycleStore::new(f.pool.clone());
    for _ in 0..3 {
        let claim = restarted.claim(&f.user, &f.session).await.unwrap().unwrap();
        assert!(claim.work.active_request.is_none());
        assert!(claim.work.prepared_working.is_none());
        assert_eq!(
            claim.work.working_memory_id.as_deref(),
            Some(actual_receipt.as_str())
        );
        let mut recovered = DurableMemoriaPort {
            inner: f.inner.clone(),
            store: restarted.clone(),
            claim,
        };
        let plan = recovered.prepare_plan().await.unwrap();
        assert!(
            plan.memories
                .iter()
                .any(|memory| memory.memory_id == actual_receipt)
        );
        recovered.claim.work.plan = Some(serde_json::to_value(&plan).unwrap());
        // Frozen inventory reads also leave the confirmed receipt unchanged.
        recovered
            .retrieve_scoped_typed("session memory", &f.session, 16, &["working"])
            .await
            .unwrap();
        assert_eq!(
            restarted
                .load_claim_work(&recovered.claim)
                .await
                .unwrap()
                .working_memory_id
                .as_deref(),
            Some(actual_receipt.as_str())
        );
        restarted.release(&recovered.claim, true).await.unwrap();
    }
    let report = attempt(&f).await;
    assert!(report.consolidation_complete());
    assert_eq!(
        f.store.status(&f.user, &f.session).await.unwrap()["pending"],
        false
    );
    assert!(
        f.backend
            .lock()
            .unwrap()
            .inventory
            .as_ref()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn lost_stale_delete_response_preserves_confirmed_receipt_on_close_and_restart() {
    stale_cleanup_preserves_receipt_through_close_and_restart(true).await;
}

#[tokio::test]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn successful_stale_cleanup_preserves_confirmed_receipt_on_close_and_restart() {
    stale_cleanup_preserves_receipt_through_close_and_restart(false).await;
}
