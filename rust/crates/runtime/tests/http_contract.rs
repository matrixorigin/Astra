use std::{fs, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use axum::{
    Router, body,
    http::{Request, StatusCode},
};
use chrono::DateTime;
use mo_agent_runtime::{AppState, HealthChecker, ServiceInfo, build_app};
use serde::Deserialize;
use tower::util::ServiceExt;

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
    assert_eq!(json, contract.root.json);
}

#[tokio::test]
async fn healthy_state_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = read_json(build_test_app(true), "/health").await;

    assert_eq!(status.as_u16(), contract.health.healthy.status);
    assert_eq!(json, contract.health.healthy.json);
}

#[tokio::test]
async fn unhealthy_state_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = read_json(build_test_app(false), "/health").await;

    assert_eq!(status.as_u16(), contract.health.unhealthy.status);
    assert_eq!(json, contract.health.unhealthy.json);
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
async fn learning_signals_require_auth() {
    let contract = load_contract();

    let (status, json) = read_json(build_test_app(true), "/api/v1/learning/signals").await;

    assert_eq!(status.as_u16(), contract.auth_error.status);
    assert_eq!(json, contract.auth_error.json);
}

#[tokio::test]
async fn learning_stats_require_auth() {
    let contract = load_contract();

    let (status, json) = read_json(build_test_app(true), "/api/v1/learning/stats").await;

    assert_eq!(status.as_u16(), contract.auth_error.status);
    assert_eq!(json, contract.auth_error.json);
}

#[tokio::test]
async fn learning_trigger_requires_auth() {
    let contract = load_contract();

    let (status, json) = post_json(
        build_test_app(true),
        "/api/v1/learning/trigger",
        &[],
        contract.learning_trigger.request.clone(),
    )
    .await;

    assert_eq!(status.as_u16(), contract.auth_error.status);
    assert_eq!(json, contract.auth_error.json);
}

#[tokio::test]
async fn learning_signals_match_shared_contract() {
    let contract = load_contract();

    let (status, json) = read_json_with_headers(
        build_test_app(true),
        "/api/v1/learning/signals",
        &[("authorization", "Bearer test-token")],
    )
    .await;

    assert_eq!(status.as_u16(), contract.learning_signals.status);
    assert_eq!(json, contract.learning_signals.json);
}

#[tokio::test]
async fn learning_stats_match_shared_contract() {
    let contract = load_contract();

    let (status, json) = read_json_with_headers(
        build_test_app(true),
        "/api/v1/learning/stats",
        &[("authorization", "Bearer test-token")],
    )
    .await;

    assert_eq!(status.as_u16(), contract.learning_stats.status);
    assert_eq!(json, contract.learning_stats.json);
}

#[tokio::test]
async fn learning_trigger_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = post_json(
        build_test_app(true),
        "/api/v1/learning/trigger",
        &[("authorization", "Bearer test-token")],
        contract.learning_trigger.request.clone(),
    )
    .await;

    assert_eq!(status.as_u16(), contract.learning_trigger.status);
    assert_eq!(json, contract.learning_trigger.json);
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
