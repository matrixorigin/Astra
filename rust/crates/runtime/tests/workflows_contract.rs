use std::sync::Arc;

use astra_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ErrorResponse, HealthChecker, ServiceInfo, WorkflowDefRecord,
    WorkflowListItem, WorkflowRunRecord, WorkflowService, build_app,
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
            Some("Bearer test-token") => Ok(AuthUserRecord {
                user_id: "wf-user-id".to_string(),
                username: "wf-user".to_string(),
                email: "wf-user@test.com".to_string(),
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
struct StubWorkflowService;

#[async_trait]
impl WorkflowService for StubWorkflowService {
    async fn list_workflows(
        &self,
    ) -> Result<Vec<WorkflowListItem>, (StatusCode, axum::Json<ErrorResponse>)> {
        Ok(vec![WorkflowListItem {
            workflow_id: "wf-001".to_string(),
            name: "ETL Pipeline".to_string(),
            version: "1.0".to_string(),
            description: Some("Extract-Transform-Load".to_string()),
            is_active: true,
        }])
    }

    async fn get_workflow(
        &self,
        workflow_id: String,
    ) -> Result<WorkflowDefRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        if workflow_id == "wf-001" {
            Ok(WorkflowDefRecord {
                workflow_id: "wf-001".to_string(),
                name: "ETL Pipeline".to_string(),
                version: "1.0".to_string(),
                description: Some("Extract-Transform-Load".to_string()),
                definition: serde_json::json!({"steps": []}),
                is_active: true,
            })
        } else {
            Err((
                StatusCode::NOT_FOUND,
                axum::Json(ErrorResponse {
                    detail: format!("Workflow {} not found", workflow_id),
                }),
            ))
        }
    }

    async fn get_workflow_run(
        &self,
        run_id: String,
    ) -> Result<WorkflowRunRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        if run_id == "run-001" {
            Ok(WorkflowRunRecord {
                run_id: "run-001".to_string(),
                workflow_id: "wf-001".to_string(),
                agent_run_id: Some("agent-run-1".to_string()),
                status: "waiting".to_string(),
                waiting_for: Some("human_approval".to_string()),
                current_step_idx: 2,
                step_results: serde_json::json!([]),
                error: None,
            })
        } else {
            Err((
                StatusCode::NOT_FOUND,
                axum::Json(ErrorResponse {
                    detail: format!("Run {} not found", run_id),
                }),
            ))
        }
    }

    async fn resolve_workflow_wait(
        &self,
        run_id: String,
        _result: serde_json::Value,
    ) -> Result<serde_json::Value, (StatusCode, axum::Json<ErrorResponse>)> {
        if run_id == "run-001" {
            Ok(serde_json::json!({"resumed": true}))
        } else if run_id == "run-not-waiting" {
            Err((
                StatusCode::CONFLICT,
                axum::Json(ErrorResponse {
                    detail: "Run is not waiting".to_string(),
                }),
            ))
        } else {
            Err((
                StatusCode::NOT_FOUND,
                axum::Json(ErrorResponse {
                    detail: format!("Run {} not found", run_id),
                }),
            ))
        }
    }
}

fn build_test_app() -> axum::Router {
    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
        .with_auth_service(Arc::new(StubAuthService))
        .with_workflow_service(Arc::new(StubWorkflowService));
    build_app(state)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_workflows_returns_ok() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/workflows")
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
    assert_eq!(json[0]["workflow_id"], "wf-001");
    assert_eq!(json[0]["name"], "ETL Pipeline");
    assert!(json[0].get("definition").is_none());
}

#[tokio::test]
async fn get_workflow_returns_detail() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/workflows/wf-001")
                .header("authorization", "Bearer test-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["workflow_id"], "wf-001");
    assert!(json.get("definition").is_some());
}

#[tokio::test]
async fn get_workflow_run_returns_ok() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/workflows/runs/run-001")
                .header("authorization", "Bearer test-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["run_id"], "run-001");
    assert_eq!(json["status"], "waiting");
    assert_eq!(json["waiting_for"], "human_approval");
}

#[tokio::test]
async fn get_workflow_run_not_found() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/workflows/runs/nonexistent")
                .header("authorization", "Bearer test-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn resolve_workflow_returns_ok() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/workflows/runs/run-001/resolve")
                .header("authorization", "Bearer test-token")
                .header("content-type", "application/json")
                .body(body::Body::from(r#"{"approved": true}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["resumed"], true);
}

#[tokio::test]
async fn resolve_workflow_not_waiting() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/workflows/runs/run-not-waiting/resolve")
                .header("authorization", "Bearer test-token")
                .header("content-type", "application/json")
                .body(body::Body::from(r#"{"approved": true}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}
