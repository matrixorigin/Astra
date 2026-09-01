use std::sync::Arc;

use astra_harness::{DecisionRecord, HookPoint, RuntimeSnapshot, SnapshotSink};
use astra_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ErrorResponse, HealthChecker, ServiceInfo, build_app,
};
use astra_services::auth::{
    SessionActivityCursor, SessionActivityRecord, SessionCreateRequestData, SessionListFilter,
    SessionListRecord, SessionRecord, SessionService, SessionUpdateRequestData,
};
use async_trait::async_trait;
use axum::{
    Router,
    body::{self, Body},
    http::{HeaderMap, Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::util::ServiceExt;

#[derive(Clone)]
struct Healthy;

#[async_trait]
impl HealthChecker for Healthy {
    async fn database_healthy(&self) -> bool {
        true
    }
}

#[derive(Clone)]
struct TestAuth;

fn not_configured() -> (StatusCode, axum::Json<ErrorResponse>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        axum::Json(ErrorResponse::new("not configured".to_string())),
    )
}

#[async_trait]
impl AuthService for TestAuth {
    async fn register(
        &self,
        _request: AuthRegisterRequestData,
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        Err(not_configured())
    }

    async fn login(
        &self,
        _request: AuthLoginRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        Err(not_configured())
    }

    async fn refresh(
        &self,
        _request: AuthRefreshRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        Err(not_configured())
    }

    async fn logout(
        &self,
        _request: AuthRefreshRequestData,
    ) -> Result<(), (StatusCode, axum::Json<ErrorResponse>)> {
        Err(not_configured())
    }

    async fn current_user(
        &self,
        headers: &HeaderMap,
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        match headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
        {
            Some("Bearer owner") => Ok(AuthUserRecord {
                user_id: "test-user".to_string(),
                username: "owner".to_string(),
                email: "owner@test.local".to_string(),
                display_name: None,
            }),
            Some("Bearer foreign") => Ok(AuthUserRecord {
                user_id: "foreign-user".to_string(),
                username: "foreign".to_string(),
                email: "foreign@test.local".to_string(),
                display_name: None,
            }),
            _ => Err((
                StatusCode::UNAUTHORIZED,
                axum::Json(ErrorResponse::new("not authenticated".to_string())),
            )),
        }
    }
}

#[derive(Clone)]
struct OwnedSessionService;

fn missing_session() -> (StatusCode, axum::Json<ErrorResponse>) {
    (
        StatusCode::NOT_FOUND,
        axum::Json(ErrorResponse::new("session not found".to_string())),
    )
}

fn owned_session() -> SessionRecord {
    SessionRecord {
        session_id: "s1".to_string(),
        user_id: "test-user".to_string(),
        agent_id: None,
        title: Some("harness test".to_string()),
        metadata: Default::default(),
        status: "active".to_string(),
        event_count: 0,
        created_at: "2026-01-01T00:00:00".to_string(),
        updated_at: None,
        ended_at: None,
    }
}

#[async_trait]
impl SessionService for OwnedSessionService {
    async fn create_session(
        &self,
        _user_id: String,
        _request: SessionCreateRequestData,
    ) -> Result<SessionRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        Err(missing_session())
    }

    async fn list_sessions(
        &self,
        _filter: SessionListFilter,
    ) -> Result<SessionListRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        Err(missing_session())
    }

    async fn get_session(
        &self,
        session_id: String,
        user_id: String,
    ) -> Result<SessionRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        if session_id == "s1" && user_id == "test-user" {
            Ok(owned_session())
        } else {
            Err(missing_session())
        }
    }

    async fn update_session(
        &self,
        _session_id: String,
        _user_id: String,
        _request: SessionUpdateRequestData,
    ) -> Result<SessionRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        Err(missing_session())
    }

    async fn delete_session(
        &self,
        _session_id: String,
        _user_id: String,
    ) -> Result<(), (StatusCode, axum::Json<ErrorResponse>)> {
        Err(missing_session())
    }

    async fn get_session_activity(
        &self,
        _session_id: String,
        _user_id: String,
        _limit: u32,
        _cursor: Option<SessionActivityCursor>,
    ) -> Result<SessionActivityRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        Err(missing_session())
    }
}

fn snapshot(turn: u32, timestamp: u64, tokens: u64) -> RuntimeSnapshot {
    snapshot_with_tools(turn, timestamp, tokens, &["bash"])
}

fn snapshot_with_tools(turn: u32, timestamp: u64, tokens: u64, tools: &[&str]) -> RuntimeSnapshot {
    RuntimeSnapshot {
        session_id: "s1".to_string(),
        turn_number: turn,
        model: Some("secret-model".to_string()),
        tokens_used_session: tokens,
        tool_calls_this_session: turn,
        unique_tools_used: tools.iter().map(|tool| (*tool).to_string()).collect(),
        last_tool_called: tools.last().map(|tool| (*tool).to_string()),
        turns_used: turn,
        captured_at_unix_millis: timestamp,
        ..RuntimeSnapshot::empty()
    }
}

fn record(snapshot: RuntimeSnapshot) -> DecisionRecord {
    DecisionRecord {
        session_id: "s1".to_string(),
        turn: snapshot.turn_number,
        point: HookPoint::PostTurn,
        wall_time_unix_millis: snapshot.captured_at_unix_millis,
        monotonic_millis_since_session: snapshot.captured_at_unix_millis,
        snapshot,
    }
}

fn build_test_app() -> (
    Router,
    Arc<astra_runtime::server::harness::server_sink::ServerSnapshotSink>,
) {
    let state = AppState::new(ServiceInfo::default(), Arc::new(Healthy))
        .with_auth_service(Arc::new(TestAuth))
        .with_session_service(Arc::new(OwnedSessionService));
    let sink = Arc::new(
        astra_runtime::server::harness::server_sink::ServerSnapshotSink::new(
            "s1".to_string(),
            "test-user".to_string(),
        ),
    );
    state.harness_registry.register_with_broadcast(
        "s1".to_string(),
        sink.clone(),
        sink.broadcaster_sender(),
    );
    (build_app(state), sink)
}

fn request(path: &str, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method("GET").uri(path);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    builder.body(Body::empty()).expect("request should build")
}

async fn json_response(app: Router, path: &str, token: Option<&str>) -> (StatusCode, Value) {
    let response = app
        .oneshot(request(path, token))
        .await
        .expect("router response");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body");
    (
        status,
        serde_json::from_slice(&bytes).expect("harness response should be JSON"),
    )
}

async fn response_status(app: Router, path: &str, token: Option<&str>) -> StatusCode {
    app.oneshot(request(path, token))
        .await
        .expect("router response")
        .status()
}

fn update_sink(
    sink: &astra_runtime::server::harness::server_sink::ServerSnapshotSink,
    snapshot: RuntimeSnapshot,
) {
    sink.update(&record(snapshot));
}

#[tokio::test]
async fn harness_http_owner_snapshot_history_and_diff_are_typed_and_sanitized() {
    let (app, sink) = build_test_app();
    update_sink(&sink, snapshot(1, 1_000, 5_000));
    update_sink(
        &sink,
        snapshot_with_tools(2, 2_000, 15_000, &["bash", "read_file"]),
    );

    let (status, latest) =
        json_response(app.clone(), "/sessions/s1/harness/snapshot", Some("owner")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(latest["turn_number"], 2);
    assert_eq!(
        latest["model"],
        Value::Null,
        "non-admin reads must redact model"
    );
    assert_eq!(latest["unique_tools_used"], serde_json::json!([]));
    assert_eq!(latest["last_tool_called"], Value::Null);

    let (status, history) = json_response(
        app.clone(),
        "/sessions/s1/harness/history?n=2",
        Some("owner"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(history[0]["turn_number"], 2);
    assert_eq!(history[1]["turn_number"], 1);
    assert_eq!(history[0]["model"], Value::Null);

    let (status, diff) = json_response(app, "/sessions/s1/harness/diff", Some("owner")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(diff["from_turn"], 1);
    assert_eq!(diff["to_turn"], 2);
    assert_eq!(diff["tokens_delta"], 10_000);
    assert_eq!(diff["new_tools"], serde_json::json!([]));
}

#[tokio::test]
async fn harness_http_auth_and_owner_checks_fail_closed() {
    let (app, sink) = build_test_app();
    update_sink(&sink, snapshot(1, 1_000, 5_000));

    for suffix in ["/snapshot", "/history?n=2", "/diff", "/stream"] {
        let owned_path = format!("/sessions/s1/harness{suffix}");
        let missing_path = format!("/sessions/unknown/harness{suffix}");

        assert_eq!(
            response_status(app.clone(), &owned_path, None).await,
            StatusCode::UNAUTHORIZED,
            "missing auth must be rejected for {owned_path}"
        );
        assert_eq!(
            response_status(app.clone(), &owned_path, Some("foreign")).await,
            StatusCode::NOT_FOUND,
            "foreign owner must not read {owned_path}"
        );
        assert_eq!(
            response_status(app.clone(), &missing_path, Some("owner")).await,
            StatusCode::NOT_FOUND,
            "missing session must be rejected for {missing_path}"
        );
    }
}

async fn next_sse_json(body: &mut axum::body::Body) -> Value {
    let frame = tokio::time::timeout(std::time::Duration::from_secs(2), body.frame())
        .await
        .expect("SSE frame should arrive")
        .expect("SSE stream should not end")
        .expect("SSE frame should be readable");
    let data = frame.into_data().expect("SSE frame should contain data");
    let line = String::from_utf8(data.to_vec()).expect("SSE data should be UTF-8");
    let payload = line
        .strip_prefix("data: ")
        .and_then(|value| value.strip_suffix("\n\n"))
        .expect("SSE frame should use the data contract");
    serde_json::from_str(payload).expect("SSE data should contain JSON")
}

#[tokio::test]
async fn harness_http_stream_seeds_once_and_deduplicates_seed_before_next_update() {
    let (app, sink) = build_test_app();
    let first = snapshot(1, 1_000, 5_000);
    update_sink(&sink, first.clone());

    let response = app
        .oneshot(request("/sessions/s1/harness/stream", Some("owner")))
        .await
        .expect("router response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );
    let mut body = response.into_body();
    let seed = next_sse_json(&mut body).await;
    assert_eq!(seed["turn_number"], 1);

    // This is the broadcast copy of the already-sent seed. The stream must
    // suppress it and expose the next actual snapshot as the next frame.
    update_sink(&sink, first);
    update_sink(&sink, snapshot(2, 2_000, 15_000));
    let next = next_sse_json(&mut body).await;
    assert_eq!(next["turn_number"], 2);
}
