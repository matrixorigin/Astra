use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    body,
    http::{HeaderMap, Request, StatusCode},
};
use mo_agent_runtime::triggers::{TriggerCreateRequestData, WebhookFireData};
use mo_agent_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ErrorResponse, HealthChecker, ServiceInfo, TriggerRecord,
    TriggerService, build_app,
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
                user_id: "trigger-user-id".to_string(),
                username: "trigger-user".to_string(),
                email: "trigger-user@test.com".to_string(),
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
struct StubTriggerService;

#[async_trait]
impl TriggerService for StubTriggerService {
    async fn create_trigger(
        &self,
        user_id: String,
        request: TriggerCreateRequestData,
    ) -> Result<TriggerRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        Ok(TriggerRecord {
            trigger_id: "trig-001".to_string(),
            user_id,
            agent_id: request.agent_id,
            trigger_type: request.trigger_type,
            name: request.name,
            user_input: request.user_input,
            context: request.context,
            cron_expr: request.cron_expr,
            session_id: request.session_id,
            is_active: true,
            created_at: "2026-01-01T00:00:00".to_string(),
            secret: Some("webhook-secret-abc".to_string()),
        })
    }

    async fn list_triggers(
        &self,
        user_id: String,
    ) -> Result<Vec<TriggerRecord>, (StatusCode, axum::Json<ErrorResponse>)> {
        Ok(vec![TriggerRecord {
            trigger_id: "trig-001".to_string(),
            user_id,
            agent_id: "agent-1".to_string(),
            trigger_type: "webhook".to_string(),
            name: "My Trigger".to_string(),
            user_input: "run analysis".to_string(),
            context: None,
            cron_expr: None,
            session_id: None,
            is_active: true,
            created_at: "2026-01-01T00:00:00".to_string(),
            secret: None,
        }])
    }

    async fn delete_trigger(
        &self,
        trigger_id: String,
        _user_id: String,
    ) -> Result<(), (StatusCode, axum::Json<ErrorResponse>)> {
        if trigger_id == "trig-001" {
            Ok(())
        } else {
            Err((
                StatusCode::NOT_FOUND,
                axum::Json(ErrorResponse {
                    detail: format!("Trigger {} not found", trigger_id),
                }),
            ))
        }
    }

    async fn fire_webhook(
        &self,
        trigger_id: String,
        request: WebhookFireData,
    ) -> Result<serde_json::Value, (StatusCode, axum::Json<ErrorResponse>)> {
        if trigger_id == "trig-001" && request.secret == "valid-secret" {
            Ok(serde_json::json!({"triggered": true}))
        } else {
            Err((
                StatusCode::FORBIDDEN,
                axum::Json(ErrorResponse {
                    detail: "Invalid secret".to_string(),
                }),
            ))
        }
    }
}

fn build_test_app() -> axum::Router {
    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
        .with_auth_service(Arc::new(StubAuthService))
        .with_trigger_service(Arc::new(StubTriggerService));
    build_app(state)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_trigger_returns_ok() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/triggers")
                .header("authorization", "Bearer test-token")
                .header("content-type", "application/json")
                .body(body::Body::from(r#"{"trigger_type":"webhook","name":"My Trigger","agent_id":"agent-1","user_input":"run analysis"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["trigger_id"], "trig-001");
    assert_eq!(json["trigger_type"], "webhook");
    assert_eq!(json["is_active"], true);
}

#[tokio::test]
async fn list_triggers_returns_array() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/triggers")
                .header("authorization", "Bearer test-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(json.as_array().unwrap().len() == 1);
    assert_eq!(json[0]["trigger_id"], "trig-001");
}

#[tokio::test]
async fn delete_trigger_returns_ok() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/triggers/trig-001")
                .header("authorization", "Bearer test-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn delete_trigger_not_found() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/triggers/nonexistent")
                .header("authorization", "Bearer test-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn fire_webhook_returns_ok() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/triggers/trig-001/fire")
                .header("content-type", "application/json")
                .body(body::Body::from(
                    r#"{"secret":"valid-secret","payload":{"key":"value"}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["triggered"], true);
}

#[tokio::test]
async fn fire_webhook_forbidden() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/triggers/trig-001/fire")
                .header("content-type", "application/json")
                .body(body::Body::from(r#"{"secret":"wrong-secret"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}
