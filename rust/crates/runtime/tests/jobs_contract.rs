use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    body,
    http::{HeaderMap, Request, StatusCode},
};
use mo_agent_runtime::jobs::JobWebhookData;
use mo_agent_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ErrorResponse, HealthChecker, JobRecord, JobService,
    JobSubmitRequestData, ServiceInfo, build_app,
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
                user_id: "job-user-id".to_string(),
                username: "job-user".to_string(),
                email: "job-user@test.com".to_string(),
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
struct StubJobService;

#[async_trait]
impl JobService for StubJobService {
    async fn submit_job(
        &self,
        _user_id: String,
        _request: JobSubmitRequestData,
    ) -> Result<JobRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        Ok(JobRecord {
            job_id: "job-001".to_string(),
            status: "pending".to_string(),
            result: None,
            error: None,
            progress: 0.0,
        })
    }

    async fn get_job(
        &self,
        job_id: String,
    ) -> Result<JobRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        if job_id == "job-001" {
            Ok(JobRecord {
                job_id: "job-001".to_string(),
                status: "running".to_string(),
                result: None,
                error: None,
                progress: 0.5,
            })
        } else {
            Err((
                StatusCode::NOT_FOUND,
                axum::Json(ErrorResponse {
                    detail: format!("Job {} not found", job_id),
                }),
            ))
        }
    }

    async fn cancel_job(
        &self,
        job_id: String,
    ) -> Result<JobRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        if job_id == "job-001" {
            Ok(JobRecord {
                job_id: "job-001".to_string(),
                status: "cancelled".to_string(),
                result: None,
                error: None,
                progress: 0.0,
            })
        } else if job_id == "job-done" {
            Err((
                StatusCode::CONFLICT,
                axum::Json(ErrorResponse {
                    detail: "Job already completed".to_string(),
                }),
            ))
        } else {
            Err((
                StatusCode::NOT_FOUND,
                axum::Json(ErrorResponse {
                    detail: format!("Job {} not found", job_id),
                }),
            ))
        }
    }

    async fn job_webhook(
        &self,
        _payload: JobWebhookData,
    ) -> Result<serde_json::Value, (StatusCode, axum::Json<ErrorResponse>)> {
        Ok(serde_json::json!({"ok": true}))
    }
}

fn build_test_app() -> axum::Router {
    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
        .with_auth_service(Arc::new(StubAuthService))
        .with_job_service(Arc::new(StubJobService));
    build_app(state)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn submit_job_returns_ok() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/jobs")
                .header("authorization", "Bearer test-token")
                .header("content-type", "application/json")
                .body(body::Body::from(r#"{"job_type":"train","inputs":{"lr":0.01},"gpu_required":true,"timeout_seconds":3600}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["job_id"], "job-001");
    assert_eq!(json["status"], "pending");
}

#[tokio::test]
async fn get_job_returns_ok() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/jobs/job-001")
                .header("authorization", "Bearer test-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["job_id"], "job-001");
    assert_eq!(json["status"], "running");
    assert_eq!(json["progress"], 0.5);
}

#[tokio::test]
async fn get_job_not_found() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/jobs/nonexistent")
                .header("authorization", "Bearer test-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cancel_job_returns_ok() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/jobs/job-001")
                .header("authorization", "Bearer test-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["status"], "cancelled");
}

#[tokio::test]
async fn cancel_job_conflict() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/jobs/job-done")
                .header("authorization", "Bearer test-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn job_webhook_returns_ok() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/jobs/webhook")
                .header("content-type", "application/json")
                .body(body::Body::from(
                    r#"{"job_id":"job-001","status":"completed","result":{"accuracy":0.95}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["ok"], true);
}
