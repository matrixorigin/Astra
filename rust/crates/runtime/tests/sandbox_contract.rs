use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    body,
    http::{HeaderMap, Request, StatusCode},
};
use mo_agent_runtime::sandbox::SandboxCreateRequestData;
use mo_agent_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ErrorResponse, HealthChecker, SandboxRecord, SandboxService,
    ServiceInfo, build_app,
};
use tower::util::ServiceExt;

// ── Stubs ────────────────────────────────────────────────────────────────────

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
        match headers.get("authorization").and_then(|v| v.to_str().ok()) {
            Some("Bearer test-token") => Ok(AuthUserRecord {
                user_id: "sandbox-user-id".to_string(),
                username: "sandbox-user".to_string(),
                email: "sandbox-user@test.com".to_string(),
                display_name: None,
            }),
            _ => Err((
                StatusCode::UNAUTHORIZED,
                axum::Json(ErrorResponse {
                    detail: "Not authenticated".to_string(),
                }),
            )),
        }
    }
}

#[derive(Clone)]
struct StubSandboxService;

#[async_trait]
impl SandboxService for StubSandboxService {
    async fn create_sandbox(
        &self,
        user_id: String,
        request: SandboxCreateRequestData,
    ) -> Result<SandboxRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        Ok(SandboxRecord {
            sandbox_name: request.name,
            description: request.description,
            created_by: user_id.clone(),
            created_at: "2026-01-01T00:00:00".to_string(),
            status: Some("active".to_string()),
            user_id: Some(user_id),
        })
    }

    async fn list_sandboxes(
        &self,
        user_id: String,
        _pattern: Option<String>,
    ) -> Result<Vec<SandboxRecord>, (StatusCode, axum::Json<ErrorResponse>)> {
        Ok(vec![SandboxRecord {
            sandbox_name: "my-sandbox".to_string(),
            description: "Test sandbox".to_string(),
            created_by: user_id.clone(),
            created_at: "2026-01-01T00:00:00".to_string(),
            status: Some("active".to_string()),
            user_id: Some(user_id),
        }])
    }

    async fn get_sandbox(
        &self,
        name: String,
        user_id: String,
    ) -> Result<SandboxRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        if name == "my-sandbox" {
            Ok(SandboxRecord {
                sandbox_name: "my-sandbox".to_string(),
                description: "Test sandbox".to_string(),
                created_by: user_id.clone(),
                created_at: "2026-01-01T00:00:00".to_string(),
                status: Some("active".to_string()),
                user_id: Some(user_id),
            })
        } else {
            Err((
                StatusCode::NOT_FOUND,
                axum::Json(ErrorResponse {
                    detail: format!("Sandbox {} not found", name),
                }),
            ))
        }
    }

    async fn delete_sandbox(
        &self,
        name: String,
        _user_id: String,
    ) -> Result<(), (StatusCode, axum::Json<ErrorResponse>)> {
        if name == "my-sandbox" {
            Ok(())
        } else {
            Err((
                StatusCode::NOT_FOUND,
                axum::Json(ErrorResponse {
                    detail: format!("Sandbox {} not found", name),
                }),
            ))
        }
    }
}

fn build_test_app() -> axum::Router {
    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
        .with_auth_service(Arc::new(StubAuthService))
        .with_sandbox_service(Arc::new(StubSandboxService));
    build_app(state)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_sandbox_returns_201() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/sandbox")
                .header("authorization", "Bearer test-token")
                .header("content-type", "application/json")
                .body(body::Body::from(
                    r#"{"name":"new-sandbox","description":"A new sandbox"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let bytes = body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["sandbox_name"], "new-sandbox");
    assert_eq!(json["description"], "A new sandbox");
    assert_eq!(json["created_by"], "sandbox-user-id");
}

#[tokio::test]
async fn list_sandboxes_returns_ok() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/sandbox")
                .header("authorization", "Bearer test-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["total"], 1);
    assert_eq!(json["sandboxes"][0]["sandbox_name"], "my-sandbox");
}

#[tokio::test]
async fn get_sandbox_returns_ok() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/sandbox/my-sandbox")
                .header("authorization", "Bearer test-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["sandbox_name"], "my-sandbox");
    assert_eq!(json["description"], "Test sandbox");
}

#[tokio::test]
async fn get_sandbox_not_found() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/sandbox/nonexistent")
                .header("authorization", "Bearer test-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_sandbox_returns_204() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/sandbox/my-sandbox")
                .header("authorization", "Bearer test-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn delete_sandbox_not_found() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/sandbox/nonexistent")
                .header("authorization", "Bearer test-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
