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

/// `json_ct`: send `content-type: application/json` (only for POST bodies that had it originally).
async fn oneshot_eval(
    app: axum::Router,
    method: &str,
    uri: &str,
    body: body::Body,
    json_ct: bool,
) -> axum::response::Response {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-user-id", "u1");
    if json_ct {
        req = req.header("content-type", "application/json");
    }
    app.oneshot(req.body(body).unwrap()).await.unwrap()
}

#[tokio::test]
async fn unconfigured_evaluation_routes_return_503() {
    let app = build_unconfigured_app();
    let get_uris = [
        "/evaluation/quality/trend",
        "/evaluation/drift",
        "/evaluation/gates",
        "/evaluation/calibration",
        "/evaluation/sessions/scores",
        "/evaluation/trust-report?agent_id=agent-1",
        "/evaluation/slo/dashboard",
        "/evaluation/slo/agent-1/history",
        "/evaluation/observability/metrics?agent_id=agent-1",
        "/evaluation/memory-health",
        "/evaluation/memory-metrics",
        "/evaluation/training-data/ds-001/export",
    ];
    for uri in get_uris {
        let resp = oneshot_eval(app.clone(), "GET", uri, body::Body::empty(), false).await;
        assert_eq!(
            resp.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "GET {uri}"
        );
    }

    let post_cases: [(&str, body::Body, bool); 4] = [
        (
            "/evaluation/gate/validate",
            body::Body::from(r#"{"change_type":"prompt","change_id":"c1","change_content":{}}"#),
            true,
        ),
        ("/evaluation/drift/run", body::Body::empty(), false),
        ("/evaluation/loop", body::Body::empty(), false),
        (
            "/evaluation/training-data/extract",
            body::Body::from(r#"{}"#),
            true,
        ),
    ];
    for (uri, b, json_ct) in post_cases {
        let resp = oneshot_eval(app.clone(), "POST", uri, b, json_ct).await;
        assert_eq!(
            resp.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "POST {uri}"
        );
    }
}

#[tokio::test]
async fn memory_health_and_metrics_use_mock_memoria() {
    let memoria_base_url = start_mock_memoria_health().await;
    let app = build_memoria_backed_app(memoria_base_url);

    let resp = oneshot_eval(
        app.clone(),
        "GET",
        "/evaluation/memory-health",
        body::Body::empty(),
        false,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["total_memories"], 12);
    assert_eq!(json["active_memories"], 9);
    assert_eq!(json["inactive_memories"], 3);
    assert_eq!(json["stale_working_memories"], 2);
    assert_eq!(json["orphaned_records"], 0);
    assert_eq!(json["healthy"], false);

    let resp = oneshot_eval(
        app,
        "GET",
        "/evaluation/memory-metrics",
        body::Body::empty(),
        false,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["total_memories"], 12);
    assert_eq!(json["stale_count"], 2);
    assert_eq!(json["avg_confidence"], 0.6666666666666666);
}

#[tokio::test]
async fn memoria_stubbed_evaluation_routes_surface_expected_errors() {
    let memoria_base_url = start_mock_memoria_health().await;
    let app = build_memoria_backed_app(memoria_base_url);

    let cases = [
        (
            "GET",
            "/evaluation/drift",
            body::Body::empty(),
            false,
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        (
            "GET",
            "/evaluation/quality/trend?model=gpt-4",
            body::Body::empty(),
            false,
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        (
            "GET",
            "/evaluation/slo/dashboard",
            body::Body::empty(),
            false,
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    ];
    for (method, uri, b, json_ct, expected_status) in cases {
        let resp = oneshot_eval(app.clone(), method, uri, b, json_ct).await;
        assert_eq!(resp.status(), expected_status, "{method} {uri}");
    }
}
