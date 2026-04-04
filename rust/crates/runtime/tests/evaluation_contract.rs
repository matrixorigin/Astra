use std::sync::Arc;

use astra_runtime::{AppState, HealthChecker, ServiceInfo, build_app};
use axum::{
    body,
    http::{Request, StatusCode},
};
use tower::util::ServiceExt;

#[derive(Clone)]
struct StubHealthChecker;

#[async_trait::async_trait]
impl HealthChecker for StubHealthChecker {
    async fn database_healthy(&self) -> bool {
        true
    }
}

/// Build an app with default (unconfigured) evaluation service.
fn build_unconfigured_app() -> axum::Router {
    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker));
    build_app(state)
}

// ── GET endpoints ────────────────────────────────────────────────────────────

#[tokio::test]
async fn quality_trend_returns_503() {
    let app = build_unconfigured_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/evaluation/quality/trend")
                .header("x-user-id", "u1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn drift_returns_503() {
    let app = build_unconfigured_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/evaluation/drift")
                .header("x-user-id", "u1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn gate_history_returns_503() {
    let app = build_unconfigured_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/evaluation/gates")
                .header("x-user-id", "u1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn calibration_returns_503() {
    let app = build_unconfigured_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/evaluation/calibration")
                .header("x-user-id", "u1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn session_scores_returns_503() {
    let app = build_unconfigured_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/evaluation/sessions/scores")
                .header("x-user-id", "u1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn trust_report_returns_503() {
    let app = build_unconfigured_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/evaluation/trust-report?agent_id=agent-1")
                .header("x-user-id", "u1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn slo_dashboard_returns_503() {
    let app = build_unconfigured_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/evaluation/slo/dashboard")
                .header("x-user-id", "u1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn slo_history_returns_503() {
    let app = build_unconfigured_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/evaluation/slo/agent-1/history")
                .header("x-user-id", "u1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn observability_metrics_returns_503() {
    let app = build_unconfigured_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/evaluation/observability/metrics?agent_id=agent-1")
                .header("x-user-id", "u1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn memory_health_returns_503() {
    let app = build_unconfigured_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/evaluation/memory-health")
                .header("x-user-id", "u1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn memory_metrics_returns_503() {
    let app = build_unconfigured_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/evaluation/memory-metrics")
                .header("x-user-id", "u1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn training_data_export_returns_503() {
    let app = build_unconfigured_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/evaluation/training-data/ds-001/export")
                .header("x-user-id", "u1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

// ── POST endpoints ───────────────────────────────────────────────────────────

#[tokio::test]
async fn gate_validate_returns_503() {
    let app = build_unconfigured_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/evaluation/gate/validate")
                .header("x-user-id", "u1")
                .header("content-type", "application/json")
                .body(body::Body::from(
                    r#"{"change_type":"prompt","change_id":"c1","change_content":{}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn drift_run_returns_503() {
    let app = build_unconfigured_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/evaluation/drift/run")
                .header("x-user-id", "u1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn closed_loop_returns_503() {
    let app = build_unconfigured_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/evaluation/loop")
                .header("x-user-id", "u1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn training_data_extract_returns_503() {
    let app = build_unconfigured_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/evaluation/training-data/extract")
                .header("x-user-id", "u1")
                .header("content-type", "application/json")
                .body(body::Body::from(r#"{}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}
