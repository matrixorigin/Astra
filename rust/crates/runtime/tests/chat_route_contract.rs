use std::{fs, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use axum::{
    Router, body,
    http::{HeaderMap, Request, StatusCode},
};
use mo_agent_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ErrorResponse, HealthChecker, ServiceInfo, build_app,
};
use serde::Deserialize;
use tower::util::ServiceExt;

#[derive(Deserialize)]
struct ResponseContract {
    status: u16,
    json: serde_json::Value,
}

#[derive(Deserialize)]
struct RequestContract {
    request: serde_json::Value,
    status: u16,
    json: serde_json::Value,
}

#[derive(Deserialize)]
struct ChatRouteContract {
    auth_error: ResponseContract,
    conversational: RequestContract,
    preference: RequestContract,
    feedback: RequestContract,
    external_fetch: RequestContract,
    planning: RequestContract,
    debugging: RequestContract,
    code_review: RequestContract,
    command_both: RequestContract,
}

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
        _request: AuthRegisterRequestData,
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!("register is not used in chat route contract tests")
    }

    async fn login(
        &self,
        _request: AuthLoginRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!("login is not used in chat route contract tests")
    }

    async fn refresh(
        &self,
        _request: AuthRefreshRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!("refresh is not used in chat route contract tests")
    }

    async fn logout(
        &self,
        _request: AuthRefreshRequestData,
    ) -> Result<(), (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!("logout is not used in chat route contract tests")
    }

    async fn current_user(
        &self,
        headers: &HeaderMap,
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        match headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
        {
            Some("Bearer contract-route-token") => Ok(AuthUserRecord {
                user_id: "contract-route-user-id".to_string(),
                username: "contract-route-user".to_string(),
                email: "contract-route-user@test.com".to_string(),
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

fn load_contract() -> ChatRouteContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_route_contract.json");
    let content = fs::read_to_string(path).expect("chat route contract fixture should exist");
    serde_json::from_str(&content).expect("chat route contract fixture should be valid JSON")
}

fn build_app_with_auth() -> Router {
    build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_auth_service(Arc::new(StubAuthService)),
    )
}

async fn post_json(
    app: Router,
    path: &str,
    headers: &[(&str, &str)],
    payload: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let response = app
        .oneshot(build_request("POST", path, headers, payload))
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
    payload: serde_json::Value,
) -> Request<body::Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(body::Body::from(payload.to_string())).unwrap()
}

#[tokio::test]
async fn chat_route_requires_auth() {
    let contract = load_contract();

    let (status, json) = post_json(
        build_app_with_auth(),
        "/chat/route",
        &[],
        serde_json::json!({"query": "hello"}),
    )
    .await;

    assert_eq!(status.as_u16(), contract.auth_error.status);
    assert_eq!(json, contract.auth_error.json);
}

#[tokio::test]
async fn chat_route_conversational_matches_shared_contract() {
    assert_chat_route_case(|contract| &contract.conversational).await;
}

#[tokio::test]
async fn chat_route_preference_matches_shared_contract() {
    assert_chat_route_case(|contract| &contract.preference).await;
}

#[tokio::test]
async fn chat_route_feedback_matches_shared_contract() {
    assert_chat_route_case(|contract| &contract.feedback).await;
}

#[tokio::test]
async fn chat_route_external_fetch_matches_shared_contract() {
    assert_chat_route_case(|contract| &contract.external_fetch).await;
}

#[tokio::test]
async fn chat_route_planning_matches_shared_contract() {
    assert_chat_route_case(|contract| &contract.planning).await;
}

#[tokio::test]
async fn chat_route_debugging_matches_shared_contract() {
    assert_chat_route_case(|contract| &contract.debugging).await;
}

#[tokio::test]
async fn chat_route_code_review_matches_shared_contract() {
    assert_chat_route_case(|contract| &contract.code_review).await;
}

#[tokio::test]
async fn chat_route_command_both_matches_shared_contract() {
    assert_chat_route_case(|contract| &contract.command_both).await;
}

async fn assert_chat_route_case(select: impl Fn(&ChatRouteContract) -> &RequestContract) {
    let contract = load_contract();
    let case = select(&contract);

    let (status, json) = post_json(
        build_app_with_auth(),
        "/chat/route",
        &[("authorization", "Bearer contract-route-token")],
        case.request.clone(),
    )
    .await;

    assert_eq!(status.as_u16(), case.status);
    assert_eq!(json, case.json);
}
