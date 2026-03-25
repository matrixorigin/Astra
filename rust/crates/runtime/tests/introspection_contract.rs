use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    Json, body,
    http::{HeaderMap, Request, StatusCode},
};
use mo_agent_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ErrorResponse, HealthChecker, ServiceInfo, build_app,
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
    ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
        unreachable!()
    }

    async fn login(
        &self,
        _: AuthLoginRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
        unreachable!()
    }

    async fn refresh(
        &self,
        _: AuthRefreshRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
        unreachable!()
    }

    async fn logout(
        &self,
        _: AuthRefreshRequestData,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        unreachable!()
    }

    async fn current_user(
        &self,
        headers: &HeaderMap,
    ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
        let user_id = headers
            .get("X-User-Id")
            .and_then(|v| v.to_str().ok())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                (
                    StatusCode::UNAUTHORIZED,
                    Json(ErrorResponse {
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

/// Build an app with stub auth but default (unconfigured) introspection service.
fn build_unconfigured_app() -> axum::Router {
    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
        .with_auth_service(Arc::new(StubAuthService));
    build_app(state)
}

#[tokio::test]
async fn memory_introspection_returns_503() {
    let app = build_unconfigured_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/introspection/memory?session_id=sess-1")
                .header("X-User-Id", "u1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn skills_introspection_returns_503() {
    let app = build_unconfigured_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/introspection/skills")
                .header("X-User-Id", "u1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn context_trend_returns_503() {
    let app = build_unconfigured_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/introspection/context/trend?session_id=sess-1")
                .header("X-User-Id", "u1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn context_snapshot_returns_503() {
    let app = build_unconfigured_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/introspection/context/snapshot?session_id=sess-1")
                .header("X-User-Id", "u1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn retrieval_quality_returns_503() {
    let app = build_unconfigured_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/introspection/context/retrieval_quality?session_id=sess-1")
                .header("X-User-Id", "u1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn memory_recall_returns_503() {
    let app = build_unconfigured_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/introspection/memory/recall?session_id=sess-1&query=test")
                .header("X-User-Id", "u1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}
