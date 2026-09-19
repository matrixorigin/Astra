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

mod execution_fixture {
    use astra_services as services;
    include!("../../services/tests/fixtures/evaluation_execution_config.rs");
}

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
    MatrixOneSettings::mock()
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

fn generic_experiment_create_value() -> serde_json::Value {
    let hash = |letter: char| format!("sha256:{}", letter.to_string().repeat(64));
    json!({
        "submission_idempotency_key": "contract-submission",
        "spec": {
            "schema_version": 1,
            "experiment_id": "contract-experiment",
            "measurement_profile": "instruction-only.v1",
            "target": {
                "kind": "prompt",
                "baseline": {"revision_id": "base", "content_hash": hash('a')},
                "candidate": {"revision_id": "candidate", "content_hash": hash('b')}
            },
            "cases": [{
                "case_id": "case-1",
                "input_snapshot_ref": "input://case-1",
                "input_content_hash": hash('c'),
                "task_verifier": astra_services::evaluation::task_verifier::TaskVerifierSpec::freeze(
                    astra_services::evaluation::task_verifier::JsonValueEqualsConfig { expected: json!({"ok": true}) },
                ).expect("freeze contract task verifier"),
                "holdout": false
            }],
            "repetitions": 1,
            "order": {"kind": "baseline_first"},
            "conditions": {
                "execution_config": execution_fixture::execution_config("model", "provider", "case-1"),
                "isolation_profile": "prompt_only_private",
                "model_binding": "model",
                "provider_binding": "provider",
                "context_snapshot_hash": hash('d'),
                "tool_policy_hash": hash('e'),
                "cache_policy": "provider_default_recorded",
                "memory_isolation": {"kind": "disabled"},
                "data_isolation": {"kind": "disabled"}
            },
            "budget": {
                "max_trials": 2,
                "max_concurrency": 1,
                "max_wall_time_secs": 60
            }
        }
    })
}

fn generic_experiment_create_body() -> body::Body {
    body::Body::from(
        serde_json::to_vec(&generic_experiment_create_value())
            .expect("serialize generic experiment request"),
    )
}

fn generic_experiment_create_with_prepare_marker_body() -> body::Body {
    let mut value = generic_experiment_create_value();
    value["spec"]["adapter_profile_version"] =
        json!(astra_services::evaluation::EVALUATION_ADAPTER_PROFILE_VERSION);
    body::Body::from(
        serde_json::to_vec(&value).expect("serialize marked generic experiment request"),
    )
}

fn prepared_experiment_body() -> body::Body {
    body::Body::from(
        serde_json::to_vec(&json!({
            "submission_idempotency_key": "prepare-contract-submission",
            "target": {
                "kind": "prompt",
                "baseline": {"revision_id": "base", "content": "baseline instructions"},
                "candidate": {"revision_id": "candidate", "content": "candidate instructions"}
            },
            "case": {
                "case_id": "case-1",
                "message": "fixed input",
                "verifier_config": {"expected": {"ok": true}},
                "holdout": false
            },
            "model_offering_id": "model",
            "max_concurrency": 1,
            "max_wall_time_secs": 60
        }))
        .expect("serialize prepared evaluation request"),
    )
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
async fn unconfigured_evaluation_routes_return_errors() {
    let app = build_unconfigured_app();
    let generic_get_uris = [
        "/evaluation/experiments/exp-1",
        "/evaluation/experiments/exp-1/report",
    ];
    for uri in generic_get_uris {
        let resp = oneshot_eval(app.clone(), "GET", uri, body::Body::empty(), false).await;
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED, "GET {uri}");
    }

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
    let generic_create = oneshot_eval(
        app.clone(),
        "POST",
        "/evaluation/experiments",
        generic_experiment_create_body(),
        true,
    )
    .await;
    // Authentication is unconfigured here; the authenticated fixture below
    // separately exercises the database-availability boundary.
    assert_eq!(generic_create.status(), StatusCode::NOT_IMPLEMENTED);

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
async fn generic_evaluation_control_plane_requires_database() {
    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
        .with_auth_service(Arc::new(StubAuthService));
    let app = build_app(state);

    let projection = oneshot_eval(
        app.clone(),
        "GET",
        "/evaluation/experiments/exp-1",
        body::Body::empty(),
        false,
    )
    .await;
    assert_eq!(projection.status(), StatusCode::SERVICE_UNAVAILABLE);

    let create = oneshot_eval(
        app.clone(),
        "POST",
        "/evaluation/experiments",
        generic_experiment_create_body(),
        true,
    )
    .await;
    assert_eq!(create.status(), StatusCode::SERVICE_UNAVAILABLE);

    let marked_create = oneshot_eval(
        app,
        "POST",
        "/evaluation/experiments",
        generic_experiment_create_with_prepare_marker_body(),
        true,
    )
    .await;
    assert_eq!(marked_create.status(), StatusCode::BAD_REQUEST);

    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
        .with_auth_service(Arc::new(StubAuthService));
    let prepare = oneshot_eval(
        build_app(state),
        "POST",
        "/evaluation/experiments/prepare",
        prepared_experiment_body(),
        true,
    )
    .await;
    assert_eq!(prepare.status(), StatusCode::SERVICE_UNAVAILABLE);
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
    assert_eq!(json["noise_filtered_avg_confidence"], 0.6666666666666666);
    assert_eq!(json["noise_filtered_confidence_samples"], 12);
}

// Drift/quality/slo routes require a DB and are not backed by Memoria.
// Verify they return 500 using the unconfigured stub (no TCP attempt).
#[tokio::test]
async fn db_dependent_evaluation_routes_return_error_without_db() {
    let app = build_unconfigured_app();

    let cases = [
        ("GET", "/evaluation/drift", body::Body::empty(), false),
        (
            "GET",
            "/evaluation/quality/trend?model=gpt-4",
            body::Body::empty(),
            false,
        ),
        (
            "GET",
            "/evaluation/slo/dashboard",
            body::Body::empty(),
            false,
        ),
    ];
    for (method, uri, b, json_ct) in cases {
        let resp = oneshot_eval(app.clone(), method, uri, b, json_ct).await;
        assert_eq!(
            resp.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "{method} {uri}"
        );
    }
}
