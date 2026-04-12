use std::{fs, path::PathBuf, sync::Arc};

use astra_runtime::{AppState, HealthChecker, ServiceInfo, build_app};
use async_trait::async_trait;
use axum::{
    Router, body,
    http::{Request, StatusCode},
};
use chrono::DateTime;
use serde::Deserialize;
use tower::util::ServiceExt;
use uuid::Uuid;

#[derive(Deserialize)]
struct ResponseContract {
    status: u16,
    json: serde_json::Value,
}

#[derive(Deserialize)]
struct HttpShellContract {
    root: ResponseContract,
    health: HealthContractVariants,
    learning_health: LearningHealthContract,
    auth_error: ResponseContract,
    learning_signals: ResponseContract,
    learning_stats: ResponseContract,
    learning_trigger: TriggerContract,
}

#[derive(Deserialize)]
struct HealthContractVariants {
    healthy: ResponseContract,
    unhealthy: ResponseContract,
}

#[derive(Deserialize)]
struct LearningHealthContract {
    status: u16,
    json: serde_json::Value,
    timestamp_timezone: String,
}

#[derive(Deserialize)]
struct TriggerContract {
    request: serde_json::Value,
    status: u16,
    json: serde_json::Value,
}

#[derive(Clone)]
struct StubHealthChecker {
    healthy: bool,
}

#[async_trait]
impl HealthChecker for StubHealthChecker {
    async fn database_healthy(&self) -> bool {
        self.healthy
    }
}

fn load_contract() -> HttpShellContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/http_shell_contract.json");
    let content = fs::read_to_string(path).expect("contract fixture should exist");
    serde_json::from_str(&content).expect("contract fixture should be valid JSON")
}

fn build_test_app(healthy: bool) -> Router {
    build_app(AppState::new(
        ServiceInfo::default(),
        Arc::new(StubHealthChecker { healthy }),
    ))
}

async fn read_json(app: Router, path: &str) -> (StatusCode, serde_json::Value) {
    read_json_with_headers(app, path, &[]).await
}

async fn read_json_with_headers(
    app: Router,
    path: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, serde_json::Value) {
    let response = app
        .oneshot(build_request("GET", path, headers, body::Body::empty()))
        .await
        .unwrap();
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap();
    (status, json)
}

async fn post_json(
    app: Router,
    path: &str,
    headers: &[(&str, &str)],
    payload: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let response = app
        .oneshot(build_request(
            "POST",
            path,
            headers,
            body::Body::from(payload.to_string()),
        ))
        .await
        .unwrap();
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap();
    (status, json)
}

fn assert_contract_json(actual: &serde_json::Value, expected: &serde_json::Value, label: &str) {
    if let Some(expected_obj) = expected.as_object()
        && expected_obj.contains_key("detail")
        && !expected_obj.contains_key("request_id")
    {
        let actual_obj = actual
            .as_object()
            .unwrap_or_else(|| panic!("{label}: actual response should be a JSON object"));
        let request_id = actual_obj
            .get("request_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| panic!("{label}: error response should include request_id"));
        assert!(
            Uuid::parse_str(request_id).is_ok(),
            "{label}: request_id should be a UUID"
        );

        let mut normalized_actual = actual_obj.clone();
        normalized_actual.remove("request_id");
        assert_eq!(
            serde_json::Value::Object(normalized_actual),
            *expected,
            "{label}"
        );
        return;
    }

    assert_eq!(actual, expected, "{label}");
}

fn build_request(
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: body::Body,
) -> Request<body::Body> {
    let mut builder = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    if method == "POST" {
        builder = builder.header("content-type", "application/json");
    }
    builder.body(body).unwrap()
}

#[tokio::test]
async fn root_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = read_json(build_test_app(true), "/").await;

    assert_eq!(status.as_u16(), contract.root.status);
    assert_contract_json(&json, &contract.root.json, "root");
}

#[tokio::test]
async fn healthy_state_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = read_json(build_test_app(true), "/health").await;

    assert_eq!(status.as_u16(), contract.health.healthy.status);
    assert_contract_json(&json, &contract.health.healthy.json, "health_healthy");
}

#[tokio::test]
async fn unhealthy_state_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = read_json(build_test_app(false), "/health").await;

    assert_eq!(status.as_u16(), contract.health.unhealthy.status);
    assert_contract_json(&json, &contract.health.unhealthy.json, "health_unhealthy");
}

#[tokio::test]
async fn learning_health_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = read_json(build_test_app(true), "/api/v1/learning/health").await;
    let timestamp = json["timestamp"]
        .as_str()
        .expect("timestamp should be present");

    assert_eq!(status.as_u16(), contract.learning_health.status);
    assert_eq!(json["status"], contract.learning_health.json["status"]);
    assert_eq!(json["service"], contract.learning_health.json["service"]);
    assert_eq!(json["version"], contract.learning_health.json["version"]);
    assert!(timestamp.ends_with(&contract.learning_health.timestamp_timezone));
    assert!(DateTime::parse_from_rfc3339(timestamp).is_ok());
}

#[tokio::test]
async fn learning_routes_require_auth() {
    let contract = load_contract();
    let app = build_test_app(true);

    for path in ["/api/v1/learning/signals", "/api/v1/learning/stats"] {
        let (status, json) = read_json(app.clone(), path).await;
        assert_eq!(status.as_u16(), contract.auth_error.status, "{path}");
        assert_contract_json(&json, &contract.auth_error.json, path);
    }

    let (status, json) = post_json(
        app,
        "/api/v1/learning/trigger",
        &[],
        contract.learning_trigger.request.clone(),
    )
    .await;
    assert_eq!(status.as_u16(), contract.auth_error.status);
    assert_contract_json(
        &json,
        &contract.auth_error.json,
        "learning_trigger_auth_error",
    );
}

#[tokio::test]
async fn learning_routes_match_shared_contract_when_authenticated() {
    let contract = load_contract();
    let app = build_test_app(true);
    let auth = &[("authorization", "Bearer test-token")];

    let (status, json) =
        read_json_with_headers(app.clone(), "/api/v1/learning/signals", auth).await;
    assert_eq!(status.as_u16(), contract.learning_signals.status);
    assert_contract_json(&json, &contract.learning_signals.json, "learning_signals");

    let (status, json) = read_json_with_headers(app.clone(), "/api/v1/learning/stats", auth).await;
    assert_eq!(status.as_u16(), contract.learning_stats.status);
    assert_contract_json(&json, &contract.learning_stats.json, "learning_stats");

    let (status, json) = post_json(
        app,
        "/api/v1/learning/trigger",
        auth,
        contract.learning_trigger.request.clone(),
    )
    .await;
    assert_eq!(status.as_u16(), contract.learning_trigger.status);
    assert_contract_json(&json, &contract.learning_trigger.json, "learning_trigger");
}

#[tokio::test]
async fn learning_trigger_validates_days_range() {
    let (status, _) = post_json(
        build_test_app(true),
        "/api/v1/learning/trigger",
        &[("authorization", "Bearer test-token")],
        serde_json::json!({"days": 100}),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}
