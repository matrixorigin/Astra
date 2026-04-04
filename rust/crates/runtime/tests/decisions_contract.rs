use std::sync::{Arc, Mutex};

use astra_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, DecisionCreateRequestData, DecisionListFilter,
    DecisionListRecord, DecisionRecord, DecisionService, DecisionWithContextRecord, ErrorResponse,
    HealthChecker, ServiceInfo, build_app,
};
use async_trait::async_trait;
use axum::{
    body,
    http::{HeaderMap, Request, StatusCode},
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
            Some("Bearer contract-decision-token") => Ok(AuthUserRecord {
                user_id: "contract-decision-user-id".to_string(),
                username: "contract-decision-user".to_string(),
                email: "decision-user@test.com".to_string(),
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
struct StubDecisionService {
    state: Arc<Mutex<Vec<DecisionRecord>>>,
}

impl StubDecisionService {
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(vec![DecisionRecord {
                decision_id: "contract-decision-1".to_string(),
                session_id: "contract-session-1".to_string(),
                event_id: "contract-event-1".to_string(),
                context_capture_id: "contract-context-1".to_string(),
                decision_type: "tool_selection".to_string(),
                decision_output: serde_json::json!({"tool": "bash"}),
                model_params: serde_json::json!({"temperature": 0.7}),
                created_at: "2026-01-01T00:00:00".to_string(),
            }])),
        }
    }
}

#[async_trait]
impl DecisionService for StubDecisionService {
    async fn record_decision(
        &self,
        _user_id: String,
        request: DecisionCreateRequestData,
    ) -> Result<DecisionRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        let record = DecisionRecord {
            decision_id: "contract-created-decision".to_string(),
            session_id: request.session_id,
            event_id: request.event_id,
            context_capture_id: request.context_capture_id,
            decision_type: request.decision_type,
            decision_output: request.decision_output,
            model_params: request.model_params.unwrap_or(serde_json::json!({})),
            created_at: "2026-01-01T00:00:00".to_string(),
        };
        self.state.lock().unwrap().push(record.clone());
        Ok(record)
    }

    async fn list_decisions(
        &self,
        filter: DecisionListFilter,
    ) -> Result<DecisionListRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        let decisions = self.state.lock().unwrap().clone();
        let total = decisions.len() as i64;
        let decisions = decisions
            .into_iter()
            .skip(filter.offset as usize)
            .take(filter.limit as usize)
            .collect();
        Ok(DecisionListRecord {
            decisions,
            total,
            limit: filter.limit,
            offset: filter.offset,
        })
    }

    async fn get_decision(
        &self,
        decision_id: String,
        _user_id: String,
    ) -> Result<DecisionRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        self.state
            .lock()
            .unwrap()
            .iter()
            .find(|d| d.decision_id == decision_id)
            .cloned()
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    axum::Json(ErrorResponse {
                        detail: format!("Decision {} not found", decision_id),
                    }),
                )
            })
    }

    async fn get_decision_with_context(
        &self,
        decision_id: String,
        _user_id: String,
    ) -> Result<DecisionWithContextRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        let decisions = self.state.lock().unwrap();
        let decision = decisions
            .iter()
            .find(|d| d.decision_id == decision_id)
            .cloned()
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    axum::Json(ErrorResponse {
                        detail: format!("Decision {} not found", decision_id),
                    }),
                )
            })?;
        Ok(DecisionWithContextRecord {
            decision,
            context: Some(serde_json::json!({"files": ["main.rs"]})),
        })
    }
}

fn build_test_app() -> axum::Router {
    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
        .with_auth_service(Arc::new(StubAuthService))
        .with_decision_service(Arc::new(StubDecisionService::new()));
    build_app(state)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn record_decision_returns_201() {
    let app = build_test_app();
    let payload = serde_json::json!({
        "session_id": "s1",
        "event_id": "e1",
        "context_capture_id": "ctx1",
        "decision_type": "tool_selection",
        "decision_output": {"tool": "grep"},
        "model_params": {"temperature": 0.5}
    });
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/decisions")
                .header("authorization", "Bearer contract-decision-token")
                .header("content-type", "application/json")
                .body(body::Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["decision_type"], "tool_selection");
    assert_eq!(json["session_id"], "s1");
}

#[tokio::test]
async fn list_decisions_returns_ok() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/decisions")
                .header("authorization", "Bearer contract-decision-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["total"], 1);
    assert!(!json["decisions"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn get_decision_by_id() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/decisions/contract-decision-1")
                .header("authorization", "Bearer contract-decision-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["decision_id"], "contract-decision-1");
}

#[tokio::test]
async fn get_decision_not_found() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/decisions/nonexistent")
                .header("authorization", "Bearer contract-decision-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn audit_decision_with_context() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/decisions/contract-decision-1/audit")
                .header("authorization", "Bearer contract-decision-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["decision_id"], "contract-decision-1");
    assert!(json["context"].is_object());
}

#[tokio::test]
async fn audit_decision_not_found() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/decisions/nonexistent/audit")
                .header("authorization", "Bearer contract-decision-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
