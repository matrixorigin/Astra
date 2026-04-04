use std::collections::HashMap;
use std::sync::Arc;

use astra_runtime::replay::{
    ComparisonResponse, ReplayResponse, ReplayService, ReplaySessionRequestData,
};
use astra_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ErrorResponse, HealthChecker, ServiceInfo, build_app,
};
use async_trait::async_trait;
use axum::{
    Json, body,
    http::{HeaderMap, Request, StatusCode},
};
use tokio::sync::Mutex;
use tower::util::ServiceExt;

#[derive(Clone)]
struct StubHealthChecker;

#[async_trait]
impl HealthChecker for StubHealthChecker {
    async fn database_healthy(&self) -> bool {
        true
    }
}

#[derive(Clone)]
struct StubAuthService;

#[async_trait]
impl AuthService for StubAuthService {
    async fn register(
        &self,
        _: AuthRegisterRequestData,
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }

    async fn login(
        &self,
        _: AuthLoginRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }

    async fn refresh(
        &self,
        _: AuthRefreshRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }

    async fn logout(
        &self,
        _: AuthRefreshRequestData,
    ) -> Result<(), (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }

    async fn current_user(
        &self,
        headers: &HeaderMap,
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        let user_id = headers
            .get("X-User-Id")
            .and_then(|v| v.to_str().ok())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                (
                    StatusCode::UNAUTHORIZED,
                    axum::Json(ErrorResponse {
                        detail: "Missing X-User-Id header".to_string(),
                    }),
                )
            })?;

        Ok(AuthUserRecord {
            user_id: user_id.to_string(),
            username: format!("user-{user_id}"),
            email: format!("{user_id}@example.test"),
            display_name: None,
        })
    }
}

#[derive(Clone)]
struct InMemoryReplayService {
    sessions: Arc<Mutex<HashMap<String, i64>>>,
}

impl InMemoryReplayService {
    fn new() -> Self {
        let mut sessions = HashMap::new();
        sessions.insert("session-1".to_string(), 3);
        Self {
            sessions: Arc::new(Mutex::new(sessions)),
        }
    }
}

#[async_trait]
impl ReplayService for InMemoryReplayService {
    async fn replay_session(
        &self,
        _user_id: String,
        session_id: String,
        request: ReplaySessionRequestData,
    ) -> Result<ReplayResponse, (StatusCode, Json<ErrorResponse>)> {
        let events_replayed = self
            .sessions
            .lock()
            .await
            .get(&session_id)
            .copied()
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    Json(ErrorResponse {
                        detail: "Session not found".to_string(),
                    }),
                )
            })?;

        Ok(ReplayResponse {
            replay_id: format!("replay-{session_id}"),
            session_id,
            status: "completed".to_string(),
            events_replayed,
            sandbox_name: request.sandbox_name,
            mock_mode: request.mock_mode,
            created_at: "2026-01-01T00:00:00".to_string(),
        })
    }

    async fn compare_replay(
        &self,
        _user_id: String,
        session_id: String,
    ) -> Result<ComparisonResponse, (StatusCode, Json<ErrorResponse>)> {
        let original_event_count = self
            .sessions
            .lock()
            .await
            .get(&session_id)
            .copied()
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    Json(ErrorResponse {
                        detail: "Session not found".to_string(),
                    }),
                )
            })?;

        Ok(ComparisonResponse {
            session_id,
            original_event_count,
            replay_event_count: original_event_count,
            difference: 0,
            is_match: true,
            compared_at: "2026-01-01T00:00:00".to_string(),
        })
    }
}

fn build_test_app() -> axum::Router {
    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
        .with_auth_service(Arc::new(StubAuthService))
        .with_replay_service(Arc::new(InMemoryReplayService::new()));
    build_app(state)
}

async fn response_json(resp: axum::http::Response<body::Body>) -> serde_json::Value {
    let bytes = body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn replay_session_returns_ok() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/sessions/session-1/replay")
                .header("X-User-Id", "user-1")
                .header("content-type", "application/json")
                .body(body::Body::from(
                    r#"{"sandbox_name":"my-sandbox","mock_mode":false}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = response_json(resp).await;
    assert_eq!(json["replay_id"], "replay-session-1");
    assert_eq!(json["mock_mode"], false);
    assert_eq!(json["events_replayed"], 3);
}

#[tokio::test]
async fn replay_session_defaults_mock_mode_true() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/sessions/session-1/replay")
                .header("X-User-Id", "user-1")
                .header("content-type", "application/json")
                .body(body::Body::from(r#"{}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = response_json(resp).await;
    assert_eq!(json["mock_mode"], true);
}

#[tokio::test]
async fn compare_replay_returns_ok() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/sessions/session-1/replay/compare")
                .header("X-User-Id", "user-1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = response_json(resp).await;
    assert_eq!(json["is_match"], true);
    assert_eq!(json["difference"], 0);
}

#[tokio::test]
async fn replay_session_not_found_returns_404() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/sessions/missing/replay")
                .header("X-User-Id", "user-1")
                .header("content-type", "application/json")
                .body(body::Body::from(r#"{}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn missing_user_id_returns_401() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/sessions/session-1/replay/compare")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
