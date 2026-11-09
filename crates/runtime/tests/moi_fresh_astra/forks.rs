//! New Session fork consumer contract. The ordinary MOI chat baseline stays frozen.
use super::*;
use astra_runtime::server::provider_test_support::{
    ProviderGateway, ProviderResponse, ProviderScript,
};
use astra_services::{
    DatabaseSessionContextCoordinator, DatabaseSessionForkCoordinator, DatabaseSessionService,
    SessionContextCoordinator, SessionService,
};
use astra_turn_types::SessionKeyV1;

async fn control(
    app: &Router,
    method: &str,
    path: &str,
    route: &str,
    body: &str,
) -> (StatusCode, Value) {
    response(app, signed_http_request(method, path, route, body.into())).await
}

async fn response(app: &Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (
        status,
        serde_json::from_slice(&body).expect("control API JSON"),
    )
}

fn signed_in_scope(
    method: &str,
    path: &str,
    route: &str,
    body: &str,
    scope: &str,
) -> Request<Body> {
    let mut request = signed_http_request(method, path, route, body.into());
    let token = request.headers()["authorization"]
        .to_str()
        .unwrap()
        .strip_prefix("Bearer moi-provider-v1.")
        .unwrap();
    let encoded = token.split('.').next().unwrap();
    let mut claims: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(encoded).unwrap()).unwrap();
    claims["scope"] = json!(scope);
    let encoded = URL_SAFE_NO_PAD.encode(claims.to_string());
    let mut signer = Hmac::<Sha256>::new_from_slice(KEY.as_bytes()).unwrap();
    signer.update(encoded.as_bytes());
    let token = format!(
        "Bearer moi-provider-v1.{encoded}.{}",
        URL_SAFE_NO_PAD.encode(signer.finalize().into_bytes())
    );
    request
        .headers_mut()
        .insert("authorization", token.parse().unwrap());
    request
}

async fn cursor(app: &Router, session: &str, run: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let (status, value) = control(
                app,
                "GET",
                &format!("/sessions/{session}/runs/{run}/cursor"),
                "/sessions/{session_id}/runs/{run_id}/cursor",
                "",
            )
            .await;
            if value["error_code"] == "session_fork_cursor_not_ready" {
                tokio::time::sleep(Duration::from_millis(20)).await;
                continue;
            }
            assert_eq!(status, StatusCode::OK, "cursor: {value}");
            return value;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("committed projection unavailable: session {session}, run {run}"))
}

async fn fork(app: &Router, parent: &str, cursor: &Value, key: &str) -> (Value, String) {
    let path = format!("/sessions/{parent}/forks");
    let payload = json!({"idempotency_key":key,"expected_parent_cursor":cursor,"reason":"MOI selected completed turn"}).to_string();
    let (status, manifest) =
        control(app, "POST", &path, "/sessions/{session_id}/forks", &payload).await;
    assert_eq!(status, StatusCode::OK, "prepare: {manifest}");
    let (status, replay) =
        control(app, "POST", &path, "/sessions/{session_id}/forks", &payload).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        manifest, replay,
        "same intent cannot allocate a second fork"
    );
    let id = manifest["fork_id"].as_str().unwrap();
    let activate = format!("/sessions/{parent}/forks/{id}/activate-for-continuation");
    let (status, active) = control(
        app,
        "POST",
        &activate,
        "/sessions/{session_id}/forks/{fork_id}/activate-for-continuation",
        "{}",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "activate: {active}");
    assert_eq!(active["manifest"]["state"], "active");
    assert!(
        active.get("writer_lease").is_none(),
        "activation must not grant execution authority"
    );
    let child = manifest["child_key"]["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(active["child_head"]["cursor"]["session_id"], child);
    (active, child)
}

fn answer(text: &str) -> ProviderResponse {
    ProviderResponse::OpenAi(json!({"id":"fork-answer","model":"moi-contract-model",
        "choices":[{"index":0,"message":{"role":"assistant","content":text},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":100,"completion_tokens":10,"total_tokens":110}}))
}

struct ReviewFixture {
    state: astra_runtime::AppState,
    app: Router,
    moi: MoiClient,
    provider: ProviderGateway,
    sessions: DatabaseSessionService,
    admin: MySqlConnection,
    database: String,
    gateway_task: tokio::task::JoinHandle<()>,
    parent: String,
    parent_run: String,
    parent_cursor: Value,
}

impl ReviewFixture {
    async fn new() -> Self {
        assert_eq!(std::env::var("ASTRA_TEST_DB_IT").as_deref(), Ok("1"));
        let mut settings = AppSettings::from_explicit_env().unwrap();
        settings.matrixone.database = "mysql".into();
        let mut admin = MySqlConnection::connect(&settings.matrixone.database_url_with_password())
            .await
            .unwrap();
        let database = format!("astra_test_moi_fork_{}", Uuid::new_v4().simple());
        eprintln!("MOI review fixture database: {database}");
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE `{database}`")))
            .execute(&mut admin)
            .await
            .unwrap();
        settings.matrixone.database = database.clone();
        settings.provider_request_auth = vec![ProviderRequestAuthConfig {
            provider: "moi".into(),
            auth_type: "hmac".into(),
            key: KEY.into(),
        }];
        settings.external_providers.clear();
        let gateway_state = Arc::new(Mutex::new(GatewayState::default()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        settings.memoria.base_url = endpoint.clone();
        let gateway_app = gateway_router(gateway_state.clone());
        let gateway_task = tokio::spawn(async move {
            axum::serve(listener, gateway_app).await.unwrap();
        });
        let session_settings = settings.matrixone.clone();
        let state = build_server_state(settings).await.unwrap();
        let sessions = DatabaseSessionService::new(session_settings)
            .with_pool(state.shared_pool.as_ref().unwrap().clone());
        let app = build_app(state.clone());
        let mut moi = MoiClient::new(false, endpoint, "fork-review", gateway_state);
        moi.repair_session(&app).await;
        moi.repair_bindings(&app).await;
        moi.history.clear();
        let provider = ProviderGateway::start(vec![ProviderScript::new(
            "fork review",
            |r| r.path == "/v1/chat/completions",
            (0..5).map(|_| answer("REVIEW-ANSWER")).collect(),
        )])
        .await;
        moi.model_endpoint = Some(format!("{}/v1/chat/completions", provider.base_url));
        let parent = moi.session.clone().unwrap();
        let run = moi
            .chat(&app, "WORKSPACE-A-PRIVATE-HISTORY", "REVIEW-ANSWER")
            .await;
        let parent_cursor = cursor(&app, &parent, &run).await;
        Self {
            state,
            app,
            moi,
            provider,
            sessions,
            admin,
            database,
            gateway_task,
            parent,
            parent_run: run,
            parent_cursor,
        }
    }

    async fn close(mut self) {
        assert!(
            self.state
                .drain_background_runs(Duration::from_secs(10))
                .await
        );
        self.state.close_database_pools().await;
        self.gateway_task.abort();
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE `{}`",
            self.database
        )))
        .execute(&mut self.admin)
        .await
        .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "real disposable MatrixOne; provider run stream and cancel scope"]
async fn provider_fork_review_rejects_cross_workspace_run_stream_and_cancel() {
    let fixture = ReviewFixture::new().await;
    let run = fixture.parent_run.clone();
    let stream_path = format!("/chat/runs/{run}/stream");
    let replay_path = format!("{stream_path}?replay_only=true");
    let mut rejected_streams = Vec::new();
    for path in [&stream_path, &replay_path] {
        let mut request = signed_in_scope(
            "GET",
            &stream_path,
            "/chat/runs/{run_id}/stream",
            "",
            "other-workspace",
        );
        *request.uri_mut() = path.parse().unwrap();
        let response = fixture.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        rejected_streams.push((status, body));
    }
    let cancel_path = format!("/chat/runs/{run}");
    let (cancel_status, cancel_body) = self::response(
        &fixture.app,
        signed_in_scope(
            "DELETE",
            &cancel_path,
            "/chat/runs/{run_id}",
            "",
            "other-workspace",
        ),
    )
    .await;
    // The same-workspace replay must remain usable after a rejected cancellation.
    let mut request =
        signed_http_request("GET", &stream_path, "/chat/runs/{run_id}/stream", "".into());
    *request.uri_mut() = replay_path.parse().unwrap();
    let response = fixture.app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();
    fixture.close().await;
    for (status, body) in rejected_streams {
        assert!(
            status == StatusCode::FORBIDDEN
                || events(&body)
                    .iter()
                    .any(|event| event["error_code"] == "session_fork_forbidden"),
            "{status} {body}"
        );
        assert!(!body.contains("WORKSPACE-A-PRIVATE-HISTORY"));
    }
    assert_eq!(cancel_status, StatusCode::FORBIDDEN, "{cancel_body}");
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        events(&body)
            .iter()
            .any(|event| event["type"] == "run_finished"),
        "{body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "real disposable MatrixOne; PR review workspace regression"]
async fn provider_fork_review_rejects_cross_workspace_chat() {
    let mut fixture = ReviewFixture::new().await;
    let (active, child) = fork(
        &fixture.app,
        &fixture.parent,
        &fixture.parent_cursor,
        "scope-child",
    )
    .await;
    fixture.moi.session = Some(child.clone());
    let owner = fixture.parent_cursor["owner_id"]
        .as_str()
        .unwrap()
        .to_owned();
    fixture
        .sessions
        .update_session(
            child.clone(),
            owner.clone(),
            astra_services::SessionUpdateRequestData {
                title: None,
                status: None,
                metadata_patch: None,
                metadata: Some(serde_json::Map::from_iter([(
                    "provider_owner".into(),
                    json!({"provider_id":"moi", "provider_scope_id":"other-workspace"}),
                )])),
            },
        )
        .await
        .unwrap();
    let body = fixture.moi.payload("WORKSPACE-B-REQUEST").to_string();
    let response = fixture
        .app
        .clone()
        .oneshot(signed_in_scope(
            "POST",
            "/chat/stream",
            "/chat/stream",
            &body,
            "other-workspace",
        ))
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();
    let requests = fixture.provider.requests.lock().await.len();
    let (background_status, background_error) = self::response(
        &fixture.app,
        signed_in_scope(
            "POST",
            "/chat",
            "/chat",
            &fixture.moi.payload("B-BACKGROUND").to_string(),
            "other-workspace",
        ),
    )
    .await;
    assert_eq!(
        background_status,
        StatusCode::FORBIDDEN,
        "{background_error}"
    );
    let mut replay_request = fixture.moi.payload("SAME-WORKSPACE-CONTROL");
    replay_request["context"] = json!({"task_ref":"fork-scope-replay"});
    let (same_status, same_body) = post(&fixture.app, "/chat/stream", &replay_request).await;
    assert_eq!(same_status, StatusCode::OK);
    assert!(
        events(&same_body)
            .iter()
            .any(|event| event["type"] == "run_finished"),
        "{same_body}"
    );
    // Model a pre-upgrade provider fork: even exact Run replay must not bypass
    // its missing immutable scope proof.
    let mut old_manifest = active["manifest"].clone();
    old_manifest
        .as_object_mut()
        .unwrap()
        .remove("provider_owner");
    sqlx::query(
        "UPDATE session_forks SET manifest_json = ? WHERE owner_user_id = ? AND fork_id = ?",
    )
    .bind(old_manifest.to_string())
    .bind(&owner)
    .bind(old_manifest["fork_id"].as_str().unwrap())
    .execute(fixture.state.shared_pool.as_ref().unwrap().get())
    .await
    .unwrap();
    let (old_status, old_body) = post(&fixture.app, "/chat/stream", &replay_request).await;
    assert!(
        old_status == StatusCode::FORBIDDEN
            || events(&old_body)
                .iter()
                .any(|event| event["error_code"] == "session_fork_forbidden"),
        "{old_body}"
    );
    assert_eq!(fixture.provider.requests.lock().await.len(), 2);
    fixture.close().await;
    assert!(
        status == StatusCode::FORBIDDEN
            || events(&body)
                .iter()
                .any(|event| event["error_code"] == "session_fork_forbidden"),
        "cross-workspace continuation: {status} {body}"
    );
    assert_eq!(
        requests, 1,
        "rejected continuation must never send inherited history to a model"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "real disposable MatrixOne; PR review retained-head regression"]
async fn provider_fork_review_reforks_inherited_head_after_middle_deletion() {
    let mut fixture = ReviewFixture::new().await;
    let (_, middle_id) = fork(
        &fixture.app,
        &fixture.parent,
        &fixture.parent_cursor,
        "middle",
    )
    .await;
    fixture.moi.session = Some(middle_id.clone());
    let middle_run = fixture
        .moi
        .chat(&fixture.app, "MIDDLE-HISTORY", "REVIEW-ANSWER")
        .await;
    let middle_cursor = cursor(&fixture.app, &middle_id, &middle_run).await;
    let (child, child_id) = fork(&fixture.app, &middle_id, &middle_cursor, "child").await;
    let (_, control_id) = fork(
        &fixture.app,
        &child_id,
        &child["child_head"]["cursor"],
        "before-delete",
    )
    .await;
    let owner = fixture.parent_cursor["owner_id"].as_str().unwrap();
    fixture
        .sessions
        .delete_session(control_id, owner.into())
        .await
        .unwrap();
    fixture
        .sessions
        .delete_session(middle_id, owner.into())
        .await
        .unwrap();
    let payload = json!({"idempotency_key":"after-delete", "expected_parent_cursor":child["child_head"]["cursor"], "reason":"retained exact head"}).to_string();
    let (status, value) = control(
        &fixture.app,
        "POST",
        &format!("/sessions/{child_id}/forks"),
        "/sessions/{session_id}/forks",
        &payload,
    )
    .await;
    fixture.close().await;
    assert_eq!(
        status,
        StatusCode::OK,
        "retained inherited head must remain authorized: {value}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "real disposable MatrixOne; ordinary parent scope laundering regression"]
async fn provider_fork_review_rejects_history_laundering_through_ordinary_parent() {
    let fixture = ReviewFixture::new().await;
    let response = fixture
        .app
        .clone()
        .oneshot(signed_in_scope(
            "POST",
            "/chat/stream",
            "/chat/stream",
            &fixture.moi.payload("B-PARENT-CONTINUATION").to_string(),
            "other-workspace",
        ))
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    let parsed = events(&body);
    let mut fork_status = None;
    if let Some(run) = parsed
        .iter()
        .find(|event| event["type"] == "session_info")
        .and_then(|event| event["run_id"].as_str())
    {
        let path = format!("/sessions/{}/runs/{run}/cursor", fixture.parent);
        let (cursor_status, cursor) = self::response(
            &fixture.app,
            signed_in_scope(
                "GET",
                &path,
                "/sessions/{session_id}/runs/{run_id}/cursor",
                "",
                "other-workspace",
            ),
        )
        .await;
        assert_eq!(cursor_status, StatusCode::OK, "{cursor}");
        let payload = json!({"idempotency_key":"laundered-history", "expected_parent_cursor":cursor, "reason":"B inherited A history"}).to_string();
        fork_status = Some(
            self::response(
                &fixture.app,
                signed_in_scope(
                    "POST",
                    &format!("/sessions/{}/forks", fixture.parent),
                    "/sessions/{session_id}/forks",
                    &payload,
                    "other-workspace",
                ),
            )
            .await
            .0,
        );
    }
    let leaked = fixture
        .provider
        .requests
        .lock()
        .await
        .iter()
        .skip(1)
        .any(|request| {
            request.body["messages"]
                .to_string()
                .contains("WORKSPACE-A-PRIVATE-HISTORY")
        });
    fixture.close().await;
    assert!(
        status == StatusCode::FORBIDDEN
            || parsed
                .iter()
                .any(|event| event["error_code"] == "session_fork_forbidden"),
        "B continued A parent: {status}; B then prepared fork: {fork_status:?}; model received A history: {leaked}"
    );
    assert!(!leaked);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "real disposable MatrixOne; invalid ordinary provider scope proof"]
async fn provider_fork_review_rejects_ordinary_session_with_invalid_scope_proof() {
    let fixture = ReviewFixture::new().await;
    let owner = fixture.parent_cursor["owner_id"].as_str().unwrap();
    let pool = fixture.state.shared_pool.as_ref().unwrap();
    let raw: String = sqlx::query_scalar(
        "SELECT CAST(`metadata` AS CHAR) FROM agent_sessions WHERE user_id = ? AND session_id = ?",
    )
    .bind(owner)
    .bind(&fixture.parent)
    .fetch_one(pool.get())
    .await
    .unwrap();
    let mut metadata: Value = serde_json::from_str(&raw).unwrap();
    metadata["__astra_provider_scope_proof_v1"] = Value::Null;
    sqlx::query("UPDATE agent_sessions SET `metadata` = ? WHERE user_id = ? AND session_id = ?")
        .bind(metadata.to_string())
        .bind(owner)
        .bind(&fixture.parent)
        .execute(pool.get())
        .await
        .unwrap();
    let response = fixture
        .app
        .clone()
        .oneshot(signed_http_request(
            "POST",
            "/chat/stream",
            "/chat/stream",
            fixture.moi.payload("MISSING-PROOF").to_string(),
        ))
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();
    let requests = fixture.provider.requests.lock().await.len();
    let interaction_path = "/provider-interactions/respond";
    let interaction = json!({"run_id": &fixture.parent_run, "session_id": &fixture.parent, "request_id": "not-created", "cancelled": true});
    let (interaction_status, interaction_body) = self::response(
        &fixture.app,
        signed_http_request(
            "POST",
            interaction_path,
            interaction_path,
            interaction.to_string(),
        ),
    )
    .await;
    fixture.close().await;
    assert!(
        status == StatusCode::FORBIDDEN
            || events(&body)
                .iter()
                .any(|event| event["error_code"] == "session_fork_forbidden"),
        "{status} {body}"
    );
    assert_eq!(
        requests, 1,
        "invalid proof must not start another model request"
    );
    assert_eq!(
        interaction_status,
        StatusCode::FORBIDDEN,
        "{interaction_body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "real disposable MatrixOne; existing unbound chat is not a fork source"]
async fn provider_fork_review_preserves_unbound_chat_without_claiming_its_history() {
    let mut fixture = ReviewFixture::new().await;
    let owner = fixture.parent_cursor["owner_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let pool = fixture.state.shared_pool.as_ref().unwrap().clone();
    let raw: String = sqlx::query_scalar(
        "SELECT CAST(`metadata` AS CHAR) FROM agent_sessions WHERE user_id = ? AND session_id = ?",
    )
    .bind(&owner)
    .bind(&fixture.parent)
    .fetch_one(pool.get())
    .await
    .unwrap();
    // Model the persisted data from before scope binding was recorded.
    let mut metadata: Value = serde_json::from_str(&raw).unwrap();
    metadata
        .as_object_mut()
        .unwrap()
        .remove("__astra_provider_scope_proof_v1");
    sqlx::query("UPDATE agent_sessions SET `metadata` = ? WHERE user_id = ? AND session_id = ?")
        .bind(metadata.to_string())
        .bind(&owner)
        .bind(&fixture.parent)
        .execute(pool.get())
        .await
        .unwrap();
    let run = fixture
        .moi
        .chat(
            &fixture.app,
            "EXISTING-CONVERSATION-CONTINUES",
            "REVIEW-ANSWER",
        )
        .await;
    let (cursor_status, cursor_error) = control(
        &fixture.app,
        "GET",
        &format!("/sessions/{}/runs/{run}/cursor", fixture.parent),
        "/sessions/{session_id}/runs/{run_id}/cursor",
        "",
    )
    .await;
    let payload = json!({"idempotency_key":"unbound-history", "expected_parent_cursor":fixture.parent_cursor, "reason":"cannot claim old history"});
    let (fork_status, fork_error) = control(
        &fixture.app,
        "POST",
        &format!("/sessions/{}/forks", fixture.parent),
        "/sessions/{session_id}/forks",
        &payload.to_string(),
    )
    .await;
    let stored: String = sqlx::query_scalar(
        "SELECT CAST(`metadata` AS CHAR) FROM agent_sessions WHERE user_id = ? AND session_id = ?",
    )
    .bind(&owner)
    .bind(&fixture.parent)
    .fetch_one(pool.get())
    .await
    .unwrap();
    let forks: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM session_forks WHERE owner_user_id = ?")
            .bind(&owner)
            .fetch_one(pool.get())
            .await
            .unwrap();
    fixture.close().await;
    assert_eq!(cursor_status, StatusCode::FORBIDDEN, "{cursor_error}");
    assert_eq!(fork_status, StatusCode::FORBIDDEN, "{fork_error}");
    assert!(
        serde_json::from_str::<Value>(&stored)
            .unwrap()
            .get("__astra_provider_scope_proof_v1")
            .is_none()
    );
    assert_eq!(
        forks, 0,
        "a later Run must not make unbound history forkable"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "real disposable MatrixOne; first provider chat creates a scoped session"]
async fn provider_fork_review_scopes_new_ordinary_chat_session() {
    let fixture = ReviewFixture::new().await;
    let mut first = fixture.moi.payload("NEW-ORDINARY-HISTORY");
    first.as_object_mut().unwrap().remove("session_id");
    let (status, body) = post(&fixture.app, "/chat/stream", &first).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let parsed = events(&body);
    assert!(
        parsed.iter().any(|event| event["type"] == "run_finished"),
        "{body}"
    );
    let session_id = parsed
        .iter()
        .find(|event| event["type"] == "session_info")
        .and_then(|event| event["session_id"].as_str())
        .expect("new session id");
    let mut other = fixture.moi.payload("OTHER-WORKSPACE");
    other["session_id"] = json!(session_id);
    let response = fixture
        .app
        .clone()
        .oneshot(signed_in_scope(
            "POST",
            "/chat/stream",
            "/chat/stream",
            &other.to_string(),
            "other-workspace",
        ))
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();
    let requests = fixture.provider.requests.lock().await.len();
    fixture.close().await;
    assert!(
        status == StatusCode::FORBIDDEN
            || events(&body)
                .iter()
                .any(|event| event["error_code"] == "session_fork_forbidden"),
        "{status} {body}"
    );
    assert_eq!(requests, 2, "other workspace must not invoke the model");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "real disposable MatrixOne; PR review prepare cancellation regression"]
async fn provider_fork_review_cancels_interrupted_prepare() {
    let fixture = ReviewFixture::new().await;
    let payload = json!({"idempotency_key":"cancel-prepare", "expected_parent_cursor":fixture.parent_cursor, "reason":"cancel pending continuation"}).to_string();
    let (status, manifest) = control(
        &fixture.app,
        "POST",
        &format!("/sessions/{}/forks", fixture.parent),
        "/sessions/{session_id}/forks",
        &payload,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{manifest}");
    let fork_id = manifest["fork_id"].as_str().unwrap();
    let (status, value) = control(
        &fixture.app,
        "POST",
        &format!("/sessions/{}/forks/{fork_id}/abort", fixture.parent),
        "/sessions/{session_id}/forks/{fork_id}/abort",
        r#"{"detail":"request cancelled","grace_seconds":1}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "provider can cancel a prepared child: {value}"
    );
    assert_eq!(value["state"], "aborted");
    let path = format!("/sessions/{}/forks", fixture.parent);
    let abort_path = format!("{path}/{fork_id}/abort");
    assert_eq!(
        control(
            &fixture.app,
            "POST",
            &abort_path,
            "/sessions/{session_id}/forks/{fork_id}/abort",
            r#"{"detail":"request cancelled","grace_seconds":1}"#
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, retry) = control(
        &fixture.app,
        "POST",
        &path,
        "/sessions/{session_id}/forks",
        &payload,
    )
    .await;
    assert_eq!(status, StatusCode::GONE, "{retry}");
    assert_eq!(retry["error_code"], "session_fork_aborted");
    let activate_path = format!("{path}/{fork_id}/activate-for-continuation");
    assert_eq!(
        control(
            &fixture.app,
            "POST",
            &activate_path,
            "/sessions/{session_id}/forks/{fork_id}/activate-for-continuation",
            "{}"
        )
        .await
        .0,
        StatusCode::GONE
    );

    let pool = fixture.state.shared_pool.as_ref().unwrap();
    let owner = fixture.parent_cursor["owner_id"].as_str().unwrap();
    let child_id = manifest["child_key"]["session_id"].as_str().unwrap();
    let coordinator = DatabaseSessionForkCoordinator::new(
        pool.clone(),
        Arc::new(DatabaseSessionContextCoordinator::new(pool.clone())),
    );
    // Model a completely lost prepare response. Its persisted pin deadline is
    // the recovery authority; a caller need not retain the returned fork ID.
    let lost_payload = json!({"idempotency_key":"lost-prepare", "expected_parent_cursor":fixture.parent_cursor, "reason":"lost response"}).to_string();
    let (status, lost) = control(
        &fixture.app,
        "POST",
        &path,
        "/sessions/{session_id}/forks",
        &lost_payload,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{lost}");
    sqlx::query("UPDATE conversation_manifest_pins SET grace_expires_at_ms = 0 WHERE owner_user_id = ? AND pin_id IN (?, ?)")
        .bind(owner).bind(fork_id).bind(lost["fork_id"].as_str().unwrap()).execute(pool.get()).await.unwrap();
    assert_eq!(
        coordinator
            .abort_expired_provider_prepares(100)
            .await
            .unwrap(),
        1
    );
    let (status, expired) = control(
        &fixture.app,
        "POST",
        &path,
        "/sessions/{session_id}/forks",
        &lost_payload,
    )
    .await;
    assert_eq!(status, StatusCode::GONE, "{expired}");
    assert_eq!(expired["error_code"], "session_fork_aborted");

    sqlx::query("UPDATE agent_session_lifecycle_fences SET delete_requested_at = DATE_SUB(NOW(6), INTERVAL 1 SECOND) WHERE user_id = ? AND delete_requested_at IS NOT NULL")
        .bind(owner).execute(pool.get()).await.unwrap();
    sqlx::query("UPDATE conversation_manifest_pins SET grace_expires_at_ms = 0 WHERE owner_user_id = ? AND pin_state = 'grace'")
        .bind(owner).execute(pool.get()).await.unwrap();
    let swept = astra_services::runtime_maintenance::maintain_runtime_storage(
        pool,
        Some(&coordinator),
        &astra_services::runtime_maintenance::RuntimeMaintenancePolicy {
            batch_limit: 100,
            delete_intent_grace_secs: 0,
        },
        &mut Default::default(),
    )
    .await;
    assert!(
        swept.cleanup_errors.is_empty(),
        "{:?}",
        swept.cleanup_errors
    );
    let children: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_sessions WHERE user_id = ? AND session_id IN (?, ?)",
    )
    .bind(owner)
    .bind(child_id)
    .bind(lost["child_key"]["session_id"].as_str().unwrap())
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(children, 0, "both cancelled children must be reclaimed");
    let pins: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM conversation_manifest_pins WHERE owner_user_id = ?",
    )
    .bind(owner)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(
        pins, 0,
        "cancelled preparations cannot pin history indefinitely"
    );
    assert_eq!(
        control(
            &fixture.app,
            "POST",
            &path,
            "/sessions/{session_id}/forks",
            &payload
        )
        .await
        .0,
        StatusCode::GONE
    );
    assert_eq!(
        control(
            &fixture.app,
            "POST",
            &abort_path,
            "/sessions/{session_id}/forks/{fork_id}/abort",
            r#"{"detail":"request cancelled","grace_seconds":1}"#
        )
        .await
        .0,
        StatusCode::OK
    );
    // Active children are never cancelled by this recovery owner.
    let (active, active_child) = fork(
        &fixture.app,
        &fixture.parent,
        &fixture.parent_cursor,
        "active-control",
    )
    .await;
    let active_id = active["manifest"]["fork_id"].as_str().unwrap();
    assert_eq!(
        control(
            &fixture.app,
            "POST",
            &format!("{path}/{active_id}/abort"),
            "/sessions/{session_id}/forks/{fork_id}/abort",
            r#"{"detail":"must not delete active","grace_seconds":1}"#
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        fixture
            .sessions
            .get_session(active_child.clone(), owner.into())
            .await
            .unwrap()
            .status,
        "active"
    );
    fixture
        .sessions
        .delete_session(active_child, owner.into())
        .await
        .unwrap();
    fixture
        .sessions
        .delete_session(fixture.parent.clone(), owner.into())
        .await
        .unwrap();
    coordinator
        .collect_orphaned_fork_records(100)
        .await
        .unwrap();
    let receipts: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM session_forks WHERE owner_user_id = ?")
            .bind(owner)
            .fetch_one(pool.get())
            .await
            .unwrap();
    assert_eq!(
        receipts, 0,
        "parent deletion must release its cancelled-fork receipts"
    );
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "real disposable MatrixOne; PR review fork quota regression"]
async fn provider_fork_review_charges_creation_once() {
    let fixture = ReviewFixture::new().await;
    let owner = fixture.parent_cursor["owner_id"].as_str().unwrap();
    sqlx::query("INSERT INTO resource_limits (user_id, max_sessions_per_day) VALUES (?, 2)")
        .bind(owner)
        .execute(fixture.state.shared_pool.as_ref().unwrap().get())
        .await
        .unwrap();
    fork(
        &fixture.app,
        &fixture.parent,
        &fixture.parent_cursor,
        "quota-child",
    )
    .await;
    let payload = json!({"idempotency_key":"over-quota", "expected_parent_cursor":fixture.parent_cursor, "reason":"second fork must exceed daily limit"}).to_string();
    let (status, value) = control(
        &fixture.app,
        "POST",
        &format!("/sessions/{}/forks", fixture.parent),
        "/sessions/{session_id}/forks",
        &payload,
    )
    .await;
    let charged = fixture
        .state
        .resource_governor
        .get_usage(owner)
        .await
        .sessions_created;
    fixture.close().await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "fork must share the configured daily limit: {value}"
    );
    assert_eq!(charged, 2, "prepare replay cannot charge creation twice");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "real disposable MatrixOne; provider-signed Session fork contract"]
async fn provider_signed_forks_continue_from_exact_history_without_inherited_authority() {
    assert_eq!(std::env::var("ASTRA_TEST_DB_IT").as_deref(), Ok("1"));
    let mut settings = AppSettings::from_explicit_env().expect("explicit test settings");
    let database = format!("astra_test_moi_fork_{}", Uuid::new_v4().simple());
    eprintln!("MOI fork fixture database: {database}");
    settings.matrixone.database = "mysql".into();
    let mut admin = MySqlConnection::connect(&settings.matrixone.database_url_with_password())
        .await
        .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE `{database}`")))
        .execute(&mut admin)
        .await
        .unwrap();
    settings.matrixone.database = database.clone();
    settings.provider_request_auth = vec![ProviderRequestAuthConfig {
        provider: "moi".into(),
        auth_type: "hmac".into(),
        key: KEY.into(),
    }];
    settings.external_providers.clear();
    let gateway_state = Arc::new(Mutex::new(GatewayState::default()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    settings.memoria.base_url = endpoint.clone();
    settings.edge_token_auth = astra_core::EdgeTokenAuthConfig {
        key: super::fork_auth_compat::USER_TOKEN_KEY.into(),
        check_endpoint: format!("{endpoint}/check"),
    };
    let gateway_app = gateway_router(gateway_state.clone());
    let gateway_task = tokio::spawn(async move {
        axum::serve(listener, gateway_app).await.unwrap();
    });
    let session_settings = settings.matrixone.clone();
    let state = build_server_state(settings)
        .await
        .expect("fresh Astra bootstrap");
    let app = build_app(state.clone());
    let mut moi = MoiClient::new(false, endpoint, "fork-contract", gateway_state.clone());
    moi.repair_session(&app).await;
    moi.repair_bindings(&app).await;
    moi.history.clear();
    let parent = moi.session.clone().unwrap();
    let provider = ProviderGateway::start(vec![ProviderScript::new(
        "fork turns",
        |r| r.path == "/v1/chat/completions",
        [
            "FIRST-ANSWER",
            "SECOND-ANSWER",
            "THIRD-ANSWER",
            "CHILD-ANSWER",
            "PARENT-ANSWER",
            "SIBLING-ANSWER",
            "CHILD-NEXT",
            "NESTED-ANSWER",
            "AFTER-CHILD-DELETE-ANSWER",
            "AFTER-ANCESTOR-DELETE-ANSWER",
        ]
        .into_iter()
        .map(answer)
        .collect(),
    )])
    .await;
    moi.model_endpoint = Some(format!("{}/v1/chat/completions", provider.base_url));
    let first = moi.chat(&app, "FIRST-FACT", "FIRST-ANSWER").await;
    let first_cursor = cursor(&app, &parent, &first).await;
    assert_eq!(first_cursor["completed_turn"], 1);
    let second = moi.chat(&app, "SECOND-FACT", "SECOND-ANSWER").await;
    moi.chat(&app, "THIRD-FACT", "THIRD-ANSWER").await;
    assert_eq!(
        cursor(&app, &parent, &first).await,
        first_cursor,
        "historical run cannot resolve to latest head"
    );
    let path = format!("/sessions/{parent}/forks");
    let payload = json!({"idempotency_key":"tamper-control","expected_parent_cursor":first_cursor,"reason":"test"}).to_string();
    let mut tampered_body = signed_http_request(
        "POST",
        &path,
        "/sessions/{session_id}/forks",
        payload.clone(),
    );
    *tampered_body.body_mut() = Body::from(payload.replace("tamper-control", "tampered"));
    assert_eq!(
        response(&app, tampered_body).await.0,
        StatusCode::UNAUTHORIZED
    );
    let cursor_path = format!("/sessions/{parent}/runs/{first}/cursor");
    let cursor_route = "/sessions/{session_id}/runs/{run_id}/cursor";
    let mut tampered_path = signed_http_request("GET", &cursor_path, cursor_route, String::new());
    *tampered_path.uri_mut() = format!("/sessions/{parent}/runs/{second}/cursor")
        .parse()
        .unwrap();
    assert_eq!(
        response(&app, tampered_path).await.0,
        StatusCode::UNAUTHORIZED
    );
    let request = signed_http_request("GET", &cursor_path, cursor_route, String::new());
    let headers = request.headers().clone();
    assert_eq!(response(&app, request).await.0, StatusCode::OK);
    let mut replay = Request::builder()
        .method("GET")
        .uri(&cursor_path)
        .body(Body::empty())
        .unwrap();
    *replay.headers_mut() = headers;
    assert_eq!(
        response(&app, replay).await.0,
        StatusCode::UNAUTHORIZED,
        "request ID replay is not an idempotency key"
    );
    let (active, child) = fork(&app, &parent, &first_cursor, "fork-child").await;
    let mut mismatch: Value = serde_json::from_str(&payload).unwrap();
    mismatch["idempotency_key"] = json!("fork-child");
    mismatch["reason"] = json!("different intent");
    let (status, error) = control(
        &app,
        "POST",
        &path,
        "/sessions/{session_id}/forks",
        &mismatch.to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error["error_code"], "session_fork_idempotency_mismatch");
    let (status, error) = control(
        &app,
        "GET",
        &format!("/sessions/{child}/runs/{first}/cursor"),
        "/sessions/{session_id}/runs/{run_id}/cursor",
        "",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(error["error_code"], "session_fork_not_found");
    let fork_id = active["manifest"]["fork_id"].as_str().unwrap();
    // A genuinely authenticated legacy credential for the same MOI subject
    // still belongs to a different owner namespace; it cannot reach this fork
    // through legacy Session reads or controls.
    let now = chrono::Utc::now().timestamp();
    let legacy_token = super::fork_auth_compat::token(json!({
        "sub":"moi-contract-user", "workspace_id":"other-workspace",
        "iss":"moi-core", "iat":now, "exp":now+3600, "purpose":"runtime"
    }));
    let own_list = Request::builder()
        .uri("/sessions")
        .header("authorization", format!("Bearer {legacy_token}"))
        .body(Body::empty())
        .unwrap();
    let (legacy_status, legacy_sessions) = response(&app, own_list).await;
    assert_eq!(
        legacy_status,
        StatusCode::OK,
        "legacy credential is valid: {legacy_sessions}"
    );
    assert!(
        legacy_sessions["sessions"].as_array().unwrap().is_empty(),
        "legacy cannot list HMAC-owned Sessions"
    );
    for (method, target, body) in [
        ("GET", format!("/sessions/{parent}/forks"), String::new()),
        (
            "POST",
            format!("/sessions/{child}/context/segments:batchGet"),
            json!({"manifest_root": first_cursor["canonical_root_hash"]}).to_string(),
        ),
        ("POST", format!("/sessions/{child}/cancel"), String::new()),
        ("DELETE", format!("/sessions/{child}"), String::new()),
        (
            "POST",
            format!("/sessions/{parent}/forks/{fork_id}/activate"),
            json!({"placement":"server","writer_ttl_seconds":30}).to_string(),
        ),
    ] {
        let request = Request::builder()
            .method(method)
            .uri(target)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {legacy_token}"))
            .body(Body::from(body))
            .unwrap();
        let (status, error) = response(&app, request).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "legacy cannot own HMAC fork: {error}"
        );
    }
    for (method, path, route, body) in [
        (
            "GET",
            format!("/sessions/{parent}/runs/{first}/cursor"),
            "/sessions/{session_id}/runs/{run_id}/cursor",
            String::new(),
        ),
        (
            "POST",
            format!("/sessions/{parent}/forks"),
            "/sessions/{session_id}/forks",
            payload.clone(),
        ),
        (
            "POST",
            format!("/sessions/{parent}/forks/{fork_id}/activate-for-continuation"),
            "/sessions/{session_id}/forks/{fork_id}/activate-for-continuation",
            "{}".into(),
        ),
    ] {
        let (status, error) = response(
            &app,
            signed_in_scope(method, &path, route, &body, "other-workspace"),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "cross-scope request: {error}"
        );
        assert_eq!(error["error_code"], "session_fork_forbidden");
    }
    let missing_run_path = format!("/sessions/{parent}/runs/{}/cursor", Uuid::new_v4());
    let (status, error) = control(
        &app,
        "GET",
        &missing_run_path,
        "/sessions/{session_id}/runs/{run_id}/cursor",
        "",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(error["error_code"], "session_fork_not_found");
    for (method, path, route, body) in [
        (
            "GET",
            format!("/sessions/{parent}/forks"),
            "/sessions/{session_id}/forks",
            "",
        ),
        (
            "GET",
            format!("/sessions/{parent}/forks/{fork_id}"),
            "/sessions/{session_id}/forks/{fork_id}",
            "",
        ),
        (
            "POST",
            format!("/sessions/{parent}/forks/{fork_id}/activate"),
            "/sessions/{session_id}/forks/{fork_id}/activate",
            r#"{"placement":"server","writer_ttl_seconds":30}"#,
        ),
    ] {
        let (status, error) = control(&app, method, &path, route, body).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "legacy control must retain its existing authentication: {error}"
        );
    }
    let (_, sibling) = fork(&app, &parent, &first_cursor, "fork-sibling").await;
    assert_ne!(child, sibling);
    assert_eq!(
        provider.requests.lock().await.len(),
        3,
        "fork creates no model calls"
    );
    moi.session = Some(child.clone());
    let child_run = moi.chat(&app, "CHILD-FACT", "CHILD-ANSWER").await;
    let child_cursor = cursor(&app, &child, &child_run).await;
    assert_eq!(
        child_cursor["completed_turn"], 2,
        "child canonical cursor starts after its shared prefix"
    );
    moi.session = Some(parent.clone());
    moi.chat(&app, "PARENT-FACT", "PARENT-ANSWER").await;
    moi.session = Some(sibling.clone());
    moi.chat(&app, "SIBLING-FACT", "SIBLING-ANSWER").await;
    moi.session = Some(child.clone());
    let next = moi.chat(&app, "CHILD-NEXT-FACT", "CHILD-NEXT").await;
    assert_eq!(cursor(&app, &child, &next).await["completed_turn"], 3);
    let id = active["manifest"]["fork_id"].as_str().unwrap();
    let (status, replay) = control(
        &app,
        "POST",
        &format!("/sessions/{parent}/forks/{id}/activate-for-continuation"),
        "/sessions/{session_id}/forks/{fork_id}/activate-for-continuation",
        "{}",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        replay["child_head"]["cursor"]["completed_turn"], 3,
        "activation replay must not rewind child"
    );
    let (_, nested) = fork(&app, &child, &child_cursor, "fork-nested").await;
    moi.session = Some(nested.clone());
    let nested_run = moi.chat(&app, "NESTED-FACT", "NESTED-ANSWER").await;
    assert_eq!(
        cursor(&app, &nested, &nested_run).await["completed_turn"],
        3
    );
    // Each chat consumes terminal + [DONE] and its cursor proves canonical
    // settlement. drain_background_runs now shuts admission down, so it is
    // reserved for final fixture teardown after all continuation assertions.

    // Persisted adversarial states must never resolve to the Session's latest head.
    let pool = state.shared_pool.as_ref().unwrap().get();
    let owner = first_cursor["owner_id"].as_str().unwrap();
    let first_path = format!("/sessions/{parent}/runs/{first}/cursor");
    let cursor_route = "/sessions/{session_id}/runs/{run_id}/cursor";
    sqlx::query(
        "UPDATE agent_runs SET parent_run_id = ?, depth = 1 WHERE user_id = ? AND run_id = ?",
    )
    .bind("delegating-run")
    .bind(owner)
    .bind(&first)
    .execute(pool)
    .await
    .unwrap();
    let (status, value) = control(&app, "GET", &first_path, cursor_route, "").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(value["error_code"], "session_fork_cursor_unsupported");
    sqlx::query("UPDATE agent_runs SET parent_run_id = NULL, depth = 0, status = 'running' WHERE user_id = ? AND run_id = ?")
        .bind(owner).bind(&first).execute(pool).await.unwrap();
    let (status, value) = control(&app, "GET", &first_path, cursor_route, "").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(value["error_code"], "session_fork_cursor_not_ready");
    let (status, value) = response(
        &app,
        signed_in_scope("GET", &first_path, cursor_route, "", "other-workspace"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "scope is checked before readiness"
    );
    assert_eq!(value["error_code"], "session_fork_forbidden");
    sqlx::query("UPDATE agent_runs SET status = 'completed' WHERE user_id = ? AND run_id = ?")
        .bind(owner)
        .bind(&first)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE session_transcript_items SET canonical_completed_turn = NULL, canonical_conversation_seq = NULL, canonical_root_hash = NULL WHERE user_id = ? AND run_id = ?")
        .bind(owner).bind(&first).execute(pool).await.unwrap();
    let (status, value) = control(&app, "GET", &first_path, cursor_route, "").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(value["error_code"], "session_fork_cursor_not_ready");
    sqlx::query("UPDATE session_transcript_items SET canonical_completed_turn = ?, canonical_conversation_seq = ?, canonical_root_hash = ? WHERE user_id = ? AND run_id = ?")
        .bind(first_cursor["completed_turn"].as_i64().unwrap()).bind(first_cursor["conversation_seq"].as_i64().unwrap())
        .bind(first_cursor["canonical_root_hash"].as_str().unwrap()).bind(owner).bind(&first).execute(pool).await.unwrap();
    sqlx::query("UPDATE conversation_manifest_nodes SET reachable = 0 WHERE isolation_domain = 'server' AND owner_user_id = ? AND session_id = ? AND manifest_root = ?")
        .bind(owner).bind(&parent).bind(first_cursor["canonical_root_hash"].as_str().unwrap()).execute(pool).await.unwrap();
    let (status, value) = control(&app, "GET", &first_path, cursor_route, "").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(value["error_code"], "session_fork_cursor_unavailable");
    sqlx::query("UPDATE conversation_manifest_nodes SET reachable = 1 WHERE isolation_domain = 'server' AND owner_user_id = ? AND session_id = ? AND manifest_root = ?")
        .bind(owner).bind(&parent).bind(first_cursor["canonical_root_hash"].as_str().unwrap()).execute(pool).await.unwrap();
    assert_eq!(cursor(&app, &parent, &first).await, first_cursor);
    // A scoped Session can contain an older completed Run with no canonical
    // history. Session authorization must not invent its missing fork cursor.
    let (status, legacy) = control(
        &app,
        "POST",
        "/sessions",
        "/sessions",
        &json!({"client_session_ref":format!("legacy-{}", Uuid::new_v4())}).to_string(),
    )
    .await;
    assert!(status == StatusCode::OK || status == StatusCode::CREATED);
    let legacy_session = legacy["session_id"].as_str().unwrap();
    let legacy_run = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO agent_runs (run_id, user_id, session_id, root_run_id, ancestor_path, status) VALUES (?, ?, ?, ?, ?, 'completed')")
        .bind(&legacy_run).bind(owner).bind(legacy_session).bind(&legacy_run).bind(&legacy_run)
        .execute(pool).await.unwrap();
    let legacy_path = format!("/sessions/{legacy_session}/runs/{legacy_run}/cursor");
    let (status, value) = control(&app, "GET", &legacy_path, cursor_route, "").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(value["error_code"], "session_fork_cursor_unsupported");
    // A diagnostic event cannot supply the missing canonical commit either.
    let diagnostic_started = json!({"event_type":"run_started", "data":{
        "provider_run_owner":{"provider_id":"moi","provider_scope_id":"moi-contract-workspace"}
    }})
    .to_string();
    let diagnostic_hash = format!("{:x}", Sha256::digest(diagnostic_started.as_bytes()));
    let diagnostic_insert = sqlx::query("INSERT INTO agent_run_events (id, run_id, event_idx, user_id, session_id, event_type, event_id, event_hash, payload_json) VALUES (?, ?, 0, ?, ?, 'run_started', ?, ?, ?)")
        .bind(Uuid::new_v4().to_string()).bind(&legacy_run).bind(owner).bind(legacy_session)
        .bind(Uuid::new_v4().to_string()).bind(diagnostic_hash).bind(diagnostic_started)
        .execute(pool).await.unwrap();
    assert_eq!(diagnostic_insert.rows_affected(), 1);
    let (status, value) = control(&app, "GET", &legacy_path, cursor_route, "").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(value["error_code"], "session_fork_cursor_unsupported");

    // The middle Session is ordinary mutable state; its descendants retain
    // immutable history without retaining its live head or execution authority.
    let shared_pool = state.shared_pool.as_ref().unwrap().clone();
    let context = Arc::new(DatabaseSessionContextCoordinator::new(shared_pool.clone()));
    let fork_owner = DatabaseSessionForkCoordinator::new(shared_pool.clone(), context.clone());
    let sessions = DatabaseSessionService::new(session_settings).with_pool(shared_pool);
    let nested_key: SessionKeyV1 = serde_json::from_value(json!({
        "schema_version": 1, "isolation_domain": "server", "owner_user_id": owner,
        "session_id": nested, "branch_id": "main"
    }))
    .unwrap();
    let nested_head = context.load_head(&nested_key).await.unwrap().unwrap();
    let exact_nested = context.materialize(&nested_head).await.unwrap();
    assert_eq!(
        context
            .materialize_after(&nested_head, None)
            .await
            .unwrap()
            .messages,
        exact_nested.messages,
        "first observation must include the validated inherited fork prefix"
    );
    sessions
        .delete_session(child.clone(), owner.into())
        .await
        .unwrap();
    let middle_heads: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM session_context_heads WHERE owner_user_id = ? AND session_id = ?",
    )
    .bind(owner)
    .bind(&child)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(
        middle_heads, 0,
        "deletion must remove the middle live context head"
    );
    assert_eq!(
        context
            .materialize(&nested_head)
            .await
            .expect("nested history survives middle Session deletion"),
        exact_nested,
        "deleting a middle Session must preserve the exact descendant prefix"
    );
    let middle_key: SessionKeyV1 =
        serde_json::from_value(active["manifest"]["child_key"].clone()).unwrap();
    let retained_manifest: astra_turn_types::SessionForkManifestV1 =
        serde_json::from_value(active["manifest"].clone()).unwrap();
    assert_eq!(
        context.load_fork_prefix(&middle_key).await.unwrap(),
        Some(retained_manifest.shared_prefix())
    );
    for wrong_key in [
        SessionKeyV1 {
            owner_user_id: "another-owner".into(),
            ..middle_key.clone()
        },
        SessionKeyV1 {
            isolation_domain: "another-domain".into(),
            ..middle_key.clone()
        },
        SessionKeyV1 {
            branch_id: "another-branch".into(),
            ..middle_key.clone()
        },
    ] {
        assert!(
            context
                .load_fork_prefix(&wrong_key)
                .await
                .unwrap()
                .is_none(),
            "retained prefix must not escape the complete child key"
        );
    }
    assert_eq!(
        sessions
            .get_session(child.clone(), owner.into())
            .await
            .unwrap_err()
            .0,
        StatusCode::NOT_FOUND
    );
    let middle_runs: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_runs WHERE user_id = ? AND session_id = ?")
            .bind(owner)
            .bind(&child)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(
        middle_runs, 0,
        "retaining immutable lineage must not retain execution runs"
    );
    let incoming_pin: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversation_manifest_pins WHERE owner_user_id = ? AND pin_id = ? AND pin_state = 'active'")
        .bind(owner).bind(&retained_manifest.fork_id).fetch_one(pool).await.unwrap();
    assert_eq!(
        incoming_pin, 1,
        "the upstream retention pin must survive middle deletion"
    );
    moi.session = Some(nested.clone());
    let after_middle = moi
        .chat(&app, "AFTER-CHILD-DELETE-FACT", "AFTER-CHILD-DELETE-ANSWER")
        .await;
    cursor(&app, &nested, &after_middle).await;

    // Release the independent sibling before deleting A. Otherwise its A pin
    // could hide a broken ancestor pin on the B -> C lineage.
    sessions
        .delete_session(sibling.clone(), owner.into())
        .await
        .unwrap();
    let after_middle_head = context.load_head(&nested_key).await.unwrap().unwrap();
    let exact_after_middle = context.materialize(&after_middle_head).await.unwrap();
    let nested_suffix = context
        .materialize_after(&after_middle_head, Some(&nested_head))
        .await
        .unwrap();
    assert_eq!(
        nested_suffix.loaded_manifest_count, 1,
        "an acknowledged fork prefix must not be reloaded for each child turn"
    );
    assert_eq!(
        nested_suffix.messages,
        exact_after_middle.messages[exact_nested.messages.len()..]
    );
    sessions
        .delete_session(parent.clone(), owner.into())
        .await
        .unwrap();
    for _ in 0..4 {
        fork_owner.collect_orphaned_fork_records(100).await.unwrap();
        fork_owner
            .collect_unpinned_orphan_manifests(100)
            .await
            .unwrap();
    }
    assert_eq!(
        sessions
            .get_session(parent.clone(), owner.into())
            .await
            .unwrap_err()
            .0,
        StatusCode::NOT_FOUND
    );
    let after_middle_history = context
        .materialize(&after_middle_head)
        .await
        .expect("nested history survives ancestor deletion and GC");
    assert_eq!(
        after_middle_history, exact_after_middle,
        "ancestor deletion and GC preserve the complete exact child history"
    );
    assert_eq!(
        context
            .materialize_after(&after_middle_head, Some(&nested_head))
            .await
            .unwrap()
            .messages,
        nested_suffix.messages,
        "suffix recovery survives ancestor deletion and collection"
    );
    let after_ancestor = moi
        .chat(
            &app,
            "AFTER-ANCESTOR-DELETE-FACT",
            "AFTER-ANCESTOR-DELETE-ANSWER",
        )
        .await;
    cursor(&app, &nested, &after_ancestor).await;

    let requests = provider.requests.lock().await;
    for (index, included, excluded) in [
        (
            3,
            vec!["FIRST-FACT", "FIRST-ANSWER", "CHILD-FACT"],
            vec!["SECOND-FACT", "THIRD-FACT"],
        ),
        (
            4,
            vec!["FIRST-FACT", "SECOND-FACT", "THIRD-FACT", "PARENT-FACT"],
            vec!["CHILD-FACT"],
        ),
        (
            5,
            vec!["FIRST-FACT", "SIBLING-FACT"],
            vec!["SECOND-FACT", "CHILD-FACT", "PARENT-FACT"],
        ),
        (
            6,
            vec!["FIRST-FACT", "CHILD-FACT", "CHILD-NEXT-FACT"],
            vec!["SIBLING-FACT", "PARENT-FACT"],
        ),
        (
            7,
            vec!["FIRST-FACT", "CHILD-FACT", "NESTED-FACT"],
            vec!["CHILD-NEXT-FACT", "SIBLING-FACT", "PARENT-FACT"],
        ),
        (
            8,
            vec![
                "FIRST-FACT",
                "CHILD-FACT",
                "NESTED-FACT",
                "AFTER-CHILD-DELETE-FACT",
            ],
            vec!["CHILD-NEXT-FACT", "SIBLING-FACT", "PARENT-FACT"],
        ),
        (
            9,
            vec![
                "FIRST-FACT",
                "CHILD-FACT",
                "NESTED-FACT",
                "AFTER-CHILD-DELETE-FACT",
                "AFTER-ANCESTOR-DELETE-FACT",
            ],
            vec!["CHILD-NEXT-FACT", "SIBLING-FACT", "PARENT-FACT"],
        ),
    ] {
        let messages = requests[index].body["messages"].to_string();
        for value in included {
            assert!(messages.contains(value), "input {index} missing {value}");
        }
        for value in excluded {
            assert!(!messages.contains(value), "input {index} leaked {value}");
        }
    }
    drop(requests);
    provider.assert_complete();
    sessions.delete_session(nested, owner.into()).await.unwrap();
    for _ in 0..4 {
        fork_owner.collect_orphaned_fork_records(100).await.unwrap();
        fork_owner
            .collect_unpinned_orphan_manifests(100)
            .await
            .unwrap();
    }
    for table in [
        "session_forks",
        "session_fork_events",
        "conversation_manifest_pins",
        "conversation_manifest_nodes",
    ] {
        let remaining: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*) FROM `{table}` WHERE owner_user_id = ?"
        )))
        .bind(owner)
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(
            remaining, 0,
            "retained lineage must converge after the last descendant is deleted: {table}"
        );
    }
    assert!(state.drain_background_runs(Duration::from_secs(10)).await);
    state.close_database_pools().await;
    gateway_task.abort();
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP DATABASE `{database}`")))
        .execute(&mut admin)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "real disposable MatrixOne; signed Session memory lifecycle"]
async fn provider_session_memory_lifecycle_preserves_owner_scope_and_grant() {
    use astra_core::{EdgeTokenAuthConfig, MemoriaSettings, config::UcNativeSettings};
    use astra_memoria::MemoriaPort;
    use astra_runtime::turn::cloud::memoria_compact::UserScopedMemoriaPort;
    use astra_services::{
        AuthService, DatabaseAuthService, FernetTokenEncryptor, auth::uc::UcNativeProvider,
    };
    use std::sync::atomic::{AtomicBool, Ordering};

    let fixture = ReviewFixture::new().await;
    let read_only = Arc::new(AtomicBool::new(true));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let callback_issuer = base.clone();
    let grant = read_only.clone();
    let bridge = Router::new()
        .route("/callback", route_post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let issuer = callback_issuer.clone(); let grant = grant.clone();
            async move {
                assert_eq!(headers["authorization"], "Bearer lifecycle-callback-key");
                assert_eq!(body["action"], "resolve_memory_identity");
                Json(json!({"code":0,"data":{"external_subject":body["external_subject"],
                    "issuer":issuer,"uc_subject":"lifecycle-uc-owner",
                    "memory_access":if grant.load(Ordering::SeqCst) {"read_only"} else {"read_write"}}}))
            }
        }))
        .route("/protocol/openid-connect/token", route_post(|| async {
            Json(json!({"access_token":"lifecycle-service","expires_in":300,"token_type":"Bearer"}))
        }))
        .route("/api/v1/uc/internal/users/{subject}/status", axum::routing::get(|| async {
            Json(json!({"code":"OK","data":{"uc_sub":"lifecycle-uc-owner","status":"active"}}))
        }))
        .route("/check", route_post(|| async {Json(json!({"code":0,"data":{"ok":true}}))}));
    let bridge = tokio::spawn(async move { axum::serve(listener, bridge).await.unwrap() });
    let mut settings = AppSettings::from_explicit_env().unwrap();
    settings.matrixone.database = fixture.database.clone();
    let auth = Arc::new(
        DatabaseAuthService::new(settings.matrixone, settings.jwt)
            .with_pool(fixture.state.shared_pool.clone().unwrap())
            .with_encryptor(FernetTokenEncryptor::new("lifecycle-test-encryption").unwrap())
            .with_provider_request_auth(vec![ProviderRequestAuthConfig {
                provider: "moi".into(),
                auth_type: "hmac".into(),
                key: KEY.into(),
            }])
            .with_edge_token_auth(EdgeTokenAuthConfig {
                key: super::fork_auth_compat::USER_TOKEN_KEY.into(),
                check_endpoint: format!("{base}/check"),
            })
            .with_external_providers(vec![astra_services::ExternalAuthProviderConfig {
                id: "moi".into(),
                display_name: "MOI".into(),
                external_auth_endpoint: format!("{base}/callback"),
                auth_key: "lifecycle-callback-key".into(),
            }])
            .with_uc_native(Some(
                UcNativeProvider::new(UcNativeSettings {
                    issuer: base.clone(),
                    adapter_url: base.clone(),
                    moi_api_url: base.clone(),
                    genesis_url: base.clone(),
                    client_secret: "lifecycle-uc-secret".into(),
                    builtin_memory: true,
                })
                .unwrap(),
            ))
            .with_memoria_settings(&MemoriaSettings {
                base_url: base.clone(),
                master_key: Some("lifecycle-memory-key".into()),
                self_hosted_master_access: false,
                issuer: None,
                web_url: None,
            })
            .unwrap(),
    );
    let app = build_app(fixture.state.clone().with_auth_service(auth.clone()));
    let owner = fixture.parent_cursor["owner_id"].as_str().unwrap();
    let port = UserScopedMemoriaPort::new(auth.memoria_credentials().unwrap(), owner.into());
    // Actual explicit-port failure, before contacting Memoria, not just an enum assertion.
    let denied = port
        .store("test memory", "semantic", None, None)
        .await
        .unwrap_err();
    assert!(
        denied.contains("read-only") && denied.contains("administrator"),
        "{denied}"
    );
    assert!(!denied.contains("Connected apps") && !denied.contains("Memoria Settings"));
    let gateway = astra_tools::memoria::MemoriaToolGateway::new(None, None)
        .require_composition_port()
        .with_memoria_port(Arc::new(port));
    let tool_error = gateway
        .call(
            "remember",
            &json!({"content":"synthetic release preference","memory_type":"semantic"}),
        )
        .await;
    assert!(
        tool_error.contains("read-only") && tool_error.contains("administrator"),
        "{tool_error}"
    );
    assert!(!tool_error.contains("Connected apps") && !tool_error.contains("Memoria Settings"));
    let finalize_path = format!("/sessions/{}/memory/finalize", fixture.parent);
    let finalize_route = "/sessions/{session_id}/memory/finalize";
    let (status, _) = control(&app, "POST", &finalize_path, finalize_route, "").await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "read-only must not authorize maintenance"
    );
    read_only.store(false, Ordering::SeqCst);
    let (status, accepted) = control(&app, "POST", &finalize_path, finalize_route, "").await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    assert_eq!(accepted["accepted"], true);
    let close_path = format!("/sessions/{}/close", fixture.parent);
    let close_route = "/sessions/{session_id}/close";
    for (path, route) in [(&finalize_path, finalize_route), (&close_path, close_route)] {
        let (status, _) = response(
            &app,
            signed_in_scope("POST", path, route, "", "other-workspace"),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "another workspace cannot finalize or close this Session"
        );
        let now = chrono::Utc::now().timestamp();
        let subject = owner.strip_prefix("provider_authorized:moi:").unwrap();
        let token = super::fork_auth_compat::token(
            json!({"sub":subject,"workspace_id":"moi-contract-workspace", "iss":"moi-backend", "iat":now,"exp":now+300,
            "purpose":"edge_registration","edge_agent_id":"lifecycle-edge","jti":Uuid::new_v4().to_string()}),
        );
        let request = Request::builder()
            .method("POST")
            .uri(path)
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let (status, _) = response(&app, request).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "same IAM subject is a different Session owner namespace"
        );
    }
    let mut tampered = signed_http_request("POST", &close_path, close_route, "".into());
    *tampered.body_mut() = Body::from("{}");
    let (status, _) = response(&app, tampered).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "lifecycle must verify the signed body digest"
    );
    let (status, closed) = control(&app, "POST", &close_path, close_route, "").await;
    assert_eq!(status, StatusCode::OK, "{closed}");
    assert_eq!(closed["status"], "closed");
    bridge.abort();
    fixture.close().await;
}
