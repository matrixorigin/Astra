use std::{fs, sync::Arc};

use astra_runtime::{AppState, HealthChecker, ServiceInfo, build_app};
use async_trait::async_trait;
use axum::{
    Router, body,
    http::{Request, StatusCode},
};
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
}

#[derive(Deserialize)]
struct HealthContractVariants {
    healthy: ResponseContract,
    unhealthy: ResponseContract,
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
    let content = fs::read_to_string(astra_core::test_paths::workspace_path(
        "fixtures/contracts/http_shell_contract.json",
    ))
    .expect("contract fixture should exist");
    serde_json::from_str(&content).expect("contract fixture should be valid JSON")
}

fn build_test_app(healthy: bool) -> Router {
    build_app(
        AppState::new(
            ServiceInfo::default(),
            Arc::new(StubHealthChecker { healthy }),
        )
        .with_auth_service(Arc::new(astra_services::auth::StubAuthService)),
    )
}

async fn read_json(app: Router, path: &str) -> (StatusCode, serde_json::Value) {
    read_json_with_headers(app, path, &[]).await
}

async fn read_status(app: Router, method: &str, path: &str) -> StatusCode {
    app.oneshot(build_request(method, path, &[], body::Body::empty()))
        .await
        .unwrap()
        .status()
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

fn assert_health_contract_json(
    actual: &serde_json::Value,
    expected: &serde_json::Value,
    label: &str,
) {
    let expected_sha = expected
        .as_object()
        .and_then(|object| object.get("build_git_sha"))
        .and_then(serde_json::Value::as_str);
    assert_eq!(
        expected_sha,
        Some("<dynamic-build-git-sha>"),
        "{label}: health contract must use the dynamic build SHA sentinel"
    );

    let mut resolved_expected = expected.clone();
    resolved_expected
        .as_object_mut()
        .expect("health contract should be a JSON object")
        .insert(
            "build_git_sha".to_string(),
            serde_json::Value::String(astra_core::history_work_baseline::BUILD_GIT_SHA.to_string()),
        );
    assert_contract_json(actual, &resolved_expected, label);
}

fn assert_root_contract_json(actual: &serde_json::Value, expected: &serde_json::Value) {
    let expected_version = expected
        .as_object()
        .and_then(|object| object.get("version"))
        .and_then(serde_json::Value::as_str);
    assert_eq!(
        expected_version,
        Some("<dynamic-package-version>"),
        "root contract must use the dynamic package version sentinel"
    );

    let mut resolved_expected = expected.clone();
    resolved_expected
        .as_object_mut()
        .expect("root contract should be a JSON object")
        .insert(
            "version".to_string(),
            serde_json::Value::String(env!("CARGO_PKG_VERSION").to_string()),
        );
    assert_contract_json(actual, &resolved_expected, "root");
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
    assert_root_contract_json(&json, &contract.root.json);
}

#[tokio::test]
async fn healthy_state_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = read_json(build_test_app(true), "/health").await;

    assert_eq!(status.as_u16(), contract.health.healthy.status);
    assert_health_contract_json(&json, &contract.health.healthy.json, "health_healthy");
}

#[tokio::test]
async fn unhealthy_state_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = read_json(build_test_app(false), "/health").await;

    assert_eq!(status.as_u16(), contract.health.unhealthy.status);
    assert_health_contract_json(&json, &contract.health.unhealthy.json, "health_unhealthy");
}

#[tokio::test]
async fn removed_learning_routes_are_not_public_contracts() {
    let app = build_test_app(true);

    for (method, path) in [
        ("GET", "/api/v1/learning/health"),
        ("GET", "/api/v1/learning/signals"),
        ("GET", "/api/v1/learning/stats"),
        ("POST", "/api/v1/learning/trigger"),
        ("POST", "/api/v1/learning/feedback"),
    ] {
        let status = read_status(app.clone(), method, path).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{method} {path} must stay removed instead of returning a stub"
        );
    }
}
