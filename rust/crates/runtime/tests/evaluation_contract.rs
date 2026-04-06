use std::sync::Arc;

use astra_runtime::{
    AppState, DatabaseEvaluationService, ErrorResponse, HealthChecker, MatrixOneSettings,
    ServiceInfo, build_app,
};
use axum::{
    Json, Router, body,
    http::{HeaderMap, Request, StatusCode},
    routing::get,
};
use serde_json::json;
use tokio::net::TcpListener;
use tower::util::ServiceExt;

#[derive(Clone)]
struct StubHealthChecker;

#[async_trait::async_trait]
impl HealthChecker for StubHealthChecker {
    async fn database_healthy(&self) -> bool {
        true
    }
}

#[derive(Clone)]
struct StubAuthService;

#[async_trait::async_trait]
impl astra_runtime::AuthService for StubAuthService {
    async fn register(
        &self,
        _: astra_runtime::AuthRegisterRequestData,
    ) -> Result<astra_runtime::AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }

    async fn login(
        &self,
        _: astra_runtime::AuthLoginRequestData,
    ) -> Result<astra_runtime::AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }

    async fn refresh(
        &self,
        _: astra_runtime::AuthRefreshRequestData,
    ) -> Result<astra_runtime::AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }

    async fn logout(
        &self,
        _: astra_runtime::AuthRefreshRequestData,
    ) -> Result<(), (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }

    async fn current_user(
        &self,
        headers: &HeaderMap,
    ) -> Result<astra_runtime::AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        let user_id = headers
            .get("x-user-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("u1");
        Ok(astra_runtime::AuthUserRecord {
            user_id: user_id.to_string(),
            username: user_id.to_string(),
            email: format!("{user_id}@example.test"),
            display_name: None,
        })
    }
}

/// Build an app with default (unconfigured) evaluation service.
fn build_unconfigured_app() -> axum::Router {
    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker));
    build_app(state)
}

fn dummy_matrixone() -> MatrixOneSettings {
    MatrixOneSettings {
        host: "127.0.0.1".to_string(),
        port: 6001,
        user: "root".to_string(),
        password: "111".to_string(),
        database: "astra".to_string(),
    }
}

async fn start_mock_memoria_health() -> String {
    let app = Router::new()
        .route(
            "/v1/health/storage",
            get(|headers: HeaderMap| async move {
                assert_eq!(
                    headers.get("x-user-id").and_then(|v| v.to_str().ok()),
                    Some("u1")
                );
                Json(json!({
                    "total": 12,
                    "active": 9,
                    "inactive": 3
                }))
            }),
        )
        .route(
            "/v1/health/analyze",
            get(|headers: HeaderMap| async move {
                assert_eq!(
                    headers.get("x-user-id").and_then(|v| v.to_str().ok()),
                    Some("u1")
                );
                Json(json!({
                    "semantic": {
                        "total": 4,
                        "avg_confidence": 0.8
                    },
                    "profile": {
                        "total": 8,
                        "avg_confidence": 0.6
                    }
                }))
            }),
        )
        .route(
            "/v1/health/hygiene",
            get(|headers: HeaderMap| async move {
                let user_id = headers.get("x-user-id").and_then(|v| v.to_str().ok());
                assert_eq!(user_id, Some("u1"));
                Json(json!({
                    "inactive_memories": 0,
                    "stale_working_memories": 2,
                    "orphan_memory_entity_links": 0,
                    "orphan_entity_links": 0,
                    "orphan_graph_nodes": 0
                }))
            }),
        );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::task::yield_now().await;
    format!("http://{addr}")
}

fn build_memoria_backed_app(memoria_base_url: String) -> axum::Router {
    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
        .with_auth_service(Arc::new(StubAuthService))
        .with_evaluation_service(Arc::new(
            DatabaseEvaluationService::new(dummy_matrixone())
                .with_memoria_config(memoria_base_url, Some("test-master-key".to_string())),
        ));
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
async fn memory_health_uses_memoria_storage_and_hygiene() {
    let memoria_base_url = start_mock_memoria_health().await;
    let app = build_memoria_backed_app(memoria_base_url);
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
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["total_memories"], 12);
    assert_eq!(json["active_memories"], 9);
    assert_eq!(json["inactive_memories"], 3);
    assert_eq!(json["stale_working_memories"], 2);
    assert_eq!(json["orphaned_records"], 0);
    assert_eq!(json["healthy"], false);
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
async fn memory_metrics_uses_memoria_health_endpoints() {
    let memoria_base_url = start_mock_memoria_health().await;
    let app = build_memoria_backed_app(memoria_base_url);
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
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["total_memories"], 12);
    assert_eq!(json["stale_count"], 2);
    assert_eq!(json["avg_confidence"], 0.6666666666666666);
}

#[tokio::test]
async fn drift_returns_501_when_service_is_stubbed() {
    let memoria_base_url = start_mock_memoria_health().await;
    let app = build_memoria_backed_app(memoria_base_url);
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
    assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
}

#[tokio::test]
async fn quality_trend_model_filter_returns_501_until_supported() {
    let memoria_base_url = start_mock_memoria_health().await;
    let app = build_memoria_backed_app(memoria_base_url);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/evaluation/quality/trend?model=gpt-4")
                .header("x-user-id", "u1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
}

#[tokio::test]
async fn slo_dashboard_returns_501_when_service_is_stubbed() {
    let memoria_base_url = start_mock_memoria_health().await;
    let app = build_memoria_backed_app(memoria_base_url);
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
    assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
}

#[tokio::test]
async fn training_data_extract_returns_501_when_service_is_stubbed() {
    let memoria_base_url = start_mock_memoria_health().await;
    let app = build_memoria_backed_app(memoria_base_url);
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
    assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
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
