use std::sync::Arc;

use astra_runtime::streaming::{StreamChatRequestData, StreamChatResponse, StreamingService};
use astra_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ErrorResponse, HealthChecker, ServiceInfo, build_app,
};
use async_trait::async_trait;
use axum::{
    Json, body,
    http::{HeaderMap, Request, StatusCode},
};
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
struct InMemoryStreamingService;

#[async_trait]
impl StreamingService for InMemoryStreamingService {
    async fn stream_chat(
        &self,
        _user_id: String,
        _request: StreamChatRequestData,
    ) -> Result<StreamChatResponse, (StatusCode, Json<ErrorResponse>)> {
        Ok(StreamChatResponse {
            status: "ok".to_string(),
            message: "test response".to_string(),
        })
    }
}

fn build_test_app() -> axum::Router {
    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
        .with_auth_service(Arc::new(StubAuthService))
        .with_streaming_service(Arc::new(InMemoryStreamingService));
    build_app(state)
}

async fn response_json(resp: axum::http::Response<body::Body>) -> serde_json::Value {
    let bytes = body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn stream_chat_returns_deprecation_response_shape() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/streaming/chat")
                .header("X-User-Id", "user-1")
                .header("content-type", "application/json")
                .body(body::Body::from(
                    r#"{"session_id":"session-1","message":"hello","context":{},"max_candidates":3}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = response_json(resp).await;
    assert_eq!(json["status"], "ok");
    assert_eq!(json["message"], "test response");
}

#[tokio::test]
async fn missing_user_id_returns_401() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/streaming/chat")
                .header("content-type", "application/json")
                .body(body::Body::from(
                    r#"{"session_id":"session-1","message":"hello"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
