use std::{collections::HashMap, fs, path::PathBuf, sync::Arc};

use astra_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, CancelRunRecord, ChatRequestData, ChatRunRecord,
    ChatStreamRecord, ErrorResponse, HealthChecker, RunLifecycleService, RunListRecord,
    RunStatusRecord, ServiceInfo, build_app,
};
use async_trait::async_trait;
use axum::{
    Router, body,
    http::{HeaderMap, Request, Response, StatusCode},
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
struct SseRequestContract {
    request: serde_json::Value,
    status: u16,
    headers: HashMap<String, String>,
    events: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct SseResponseContract {
    status: u16,
    headers: HashMap<String, String>,
    events: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct ChatLifecycleContract {
    auth_error: ResponseContract,
    create_run_auto_session: RequestContract,
    create_run_existing_session: RequestContract,
    create_run_missing_session: RequestContract,
    stream_chat_success: SseRequestContract,
    stream_chat_missing_session: SseRequestContract,
    get_run_status_success: ResponseContract,
    get_run_status_not_found: ResponseContract,
    get_run_status_forbidden: ResponseContract,
    stream_run_success: SseResponseContract,
    stream_run_not_found: SseResponseContract,
    stream_run_forbidden: SseResponseContract,
    cancel_run_success: ResponseContract,
    cancel_run_finished: ResponseContract,
    cancel_run_forbidden: ResponseContract,
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
        unreachable!("register is not used in lifecycle contract tests")
    }

    async fn login(
        &self,
        _request: AuthLoginRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!("login is not used in lifecycle contract tests")
    }

    async fn refresh(
        &self,
        _request: AuthRefreshRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!("refresh is not used in lifecycle contract tests")
    }

    async fn logout(
        &self,
        _request: AuthRefreshRequestData,
    ) -> Result<(), (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!("logout is not used in lifecycle contract tests")
    }

    async fn current_user(
        &self,
        headers: &HeaderMap,
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        match headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
        {
            Some("Bearer contract-chat-token") => Ok(AuthUserRecord {
                user_id: "contract-chat-user-id".to_string(),
                username: "contract-chat-user".to_string(),
                email: "contract-chat-user@test.com".to_string(),
                display_name: Some("Contract Chat User".to_string()),
            }),
            Some("Bearer contract-other-chat-token") => Ok(AuthUserRecord {
                user_id: "contract-other-chat-user-id".to_string(),
                username: "contract-other-chat-user".to_string(),
                email: "contract-other-chat-user@test.com".to_string(),
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
struct StubRunLifecycleService {
    stream_events: Vec<serde_json::Value>,
}

impl StubRunLifecycleService {
    fn new() -> Self {
        Self {
            stream_events: vec![
                serde_json::json!({
                    "event_type": "run_started",
                    "data": {},
                    "run_id": "contract-run-live",
                }),
                serde_json::json!({
                    "event_type": "text_delta",
                    "data": { "chunk": "partial reply" },
                    "run_id": "contract-run-live",
                }),
                serde_json::json!({
                    "event_type": "text_done",
                    "data": { "full_text": "partial reply complete" },
                    "run_id": "contract-run-live",
                }),
                serde_json::json!({
                    "event_type": "run_finished",
                    "data": {},
                    "run_id": "contract-run-live",
                }),
            ],
        }
    }
}

#[async_trait]
impl RunLifecycleService for StubRunLifecycleService {
    async fn create_run(
        &self,
        _user_id: String,
        request: ChatRequestData,
    ) -> Result<ChatRunRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        match request.session_id.as_deref() {
            Some("contract-missing-session") => Err((
                StatusCode::NOT_FOUND,
                axum::Json(ErrorResponse {
                    detail: "Session not found".to_string(),
                }),
            )),
            Some("contract-existing-session") => Ok(ChatRunRecord {
                session_id: "contract-existing-session".to_string(),
                run_id: "contract-run-existing".to_string(),
                status: "pending".to_string(),
                explain: None,
            }),
            None => Ok(ChatRunRecord {
                session_id: "contract-created-session-1".to_string(),
                run_id: "contract-run-auto".to_string(),
                status: "pending".to_string(),
                explain: None,
            }),
            _ => unreachable!("unexpected session_id for create_run"),
        }
    }

    async fn stream_chat(
        &self,
        _user_id: String,
        request: ChatRequestData,
    ) -> Result<ChatStreamRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        match request.session_id.as_deref() {
            Some("contract-missing-session") => Err((
                StatusCode::NOT_FOUND,
                axum::Json(ErrorResponse {
                    detail: "Session not found".to_string(),
                }),
            )),
            None => Ok(ChatStreamRecord {
                session_id: "contract-created-session-1".to_string(),
                run_id: "contract-run-stream".to_string(),
                events: self.stream_events.clone(),
            }),
            _ => unreachable!("unexpected session_id for stream_chat"),
        }
    }

    async fn get_run_status(
        &self,
        run_id: String,
        user_id: String,
    ) -> Result<RunStatusRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        match run_id.as_str() {
            "contract-run-live" => Ok(RunStatusRecord {
                run_id,
                session_id: "contract-existing-session".to_string(),
                status: "pending".to_string(),
                waiting_for: None,
                events_count: self.stream_events.len() as i64,
            }),
            "contract-run-foreign" => {
                debug_assert_eq!(user_id, "contract-chat-user-id");
                Err((
                    StatusCode::FORBIDDEN,
                    axum::Json(ErrorResponse {
                        detail: "Not authorized to view this run".to_string(),
                    }),
                ))
            }
            "contract-run-missing" => Err((
                StatusCode::NOT_FOUND,
                axum::Json(ErrorResponse {
                    detail: "Run not found".to_string(),
                }),
            )),
            _ => unreachable!("unexpected run_id for get_run_status"),
        }
    }

    async fn stream_run(
        &self,
        run_id: String,
        user_id: String,
        last_index: u32,
    ) -> Result<Vec<serde_json::Value>, (StatusCode, axum::Json<ErrorResponse>)> {
        match run_id.as_str() {
            "contract-run-live" => {
                debug_assert_eq!(user_id, "contract-chat-user-id");
                Ok(self
                    .stream_events
                    .iter()
                    .skip(last_index as usize)
                    .cloned()
                    .collect())
            }
            "contract-run-foreign" => Err((
                StatusCode::FORBIDDEN,
                axum::Json(ErrorResponse {
                    detail: "Not authorized to view this run".to_string(),
                }),
            )),
            "contract-run-missing" => Err((
                StatusCode::NOT_FOUND,
                axum::Json(ErrorResponse {
                    detail: "Run not found".to_string(),
                }),
            )),
            _ => unreachable!("unexpected run_id for stream_run"),
        }
    }

    async fn cancel_run(
        &self,
        run_id: String,
        user_id: String,
    ) -> Result<CancelRunRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        match run_id.as_str() {
            "contract-run-cancellable" => {
                debug_assert_eq!(user_id, "contract-chat-user-id");
                Ok(CancelRunRecord {
                    run_id,
                    status: "cancelled".to_string(),
                })
            }
            "contract-run-finished" => Err((
                StatusCode::CONFLICT,
                axum::Json(ErrorResponse {
                    detail: "Run already finished".to_string(),
                }),
            )),
            "contract-run-foreign" => Err((
                StatusCode::FORBIDDEN,
                axum::Json(ErrorResponse {
                    detail: "Not authorized to cancel this run".to_string(),
                }),
            )),
            _ => unreachable!("unexpected run_id for cancel_run"),
        }
    }

    async fn list_runs(
        &self,
        _user_id: String,
        _limit: u32,
        _offset: u32,
    ) -> Result<RunListRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        Ok(RunListRecord {
            runs: vec![],
            total: 0,
            limit: 50,
            offset: 0,
        })
    }
}

fn load_contract() -> ChatLifecycleContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_lifecycle_contract.json");
    let content = fs::read_to_string(path).expect("chat lifecycle contract fixture should exist");
    serde_json::from_str(&content).expect("chat lifecycle contract fixture should be valid JSON")
}

fn build_app_with_services() -> Router {
    build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_auth_service(Arc::new(StubAuthService))
            .with_run_lifecycle_service(Arc::new(StubRunLifecycleService::new())),
    )
}

async fn request_json(
    app: Router,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    payload: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let response = app
        .oneshot(build_request(method, path, headers, payload))
        .await
        .unwrap();
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap();
    (status, json)
}

async fn request_sse(
    app: Router,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    payload: Option<serde_json::Value>,
) -> (StatusCode, HeaderMap, Vec<serde_json::Value>) {
    let response = app
        .oneshot(build_request(method, path, headers, payload))
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response_to_string(response).await;
    (status, headers, parse_sse_events(&body))
}

fn build_request(
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    payload: Option<serde_json::Value>,
) -> Request<body::Body> {
    let mut builder = Request::builder().method(method).uri(path);
    if payload.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }

    let body = match payload {
        Some(payload) => body::Body::from(payload.to_string()),
        None => body::Body::empty(),
    };
    builder.body(body).unwrap()
}

async fn response_to_string(response: Response<body::Body>) -> String {
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn parse_sse_events(body: &str) -> Vec<serde_json::Value> {
    astra_runtime::turn::sse_data_lines::parse_sse_data_json_events(body)
}

fn assert_sse_headers(headers: &HeaderMap, expected: &HashMap<String, String>) {
    let content_type = headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap();
    assert!(content_type.contains("text/event-stream"));

    for (name, expected_value) in expected {
        let actual = headers
            .get(name.as_str())
            .and_then(|value| value.to_str().ok());
        assert_eq!(actual, Some(expected_value.as_str()));
    }
}

#[tokio::test]
async fn chat_requires_auth() {
    let contract = load_contract();

    let (status, json) = request_json(
        build_app_with_services(),
        "POST",
        "/chat",
        &[],
        Some(serde_json::json!({ "message": "hello" })),
    )
    .await;

    assert_eq!(status.as_u16(), contract.auth_error.status);
    assert_eq!(json, contract.auth_error.json);
}

#[tokio::test]
async fn create_run_auto_session_matches_shared_contract() {
    let contract = load_contract();
    let case = contract.create_run_auto_session;

    let (status, json) = request_json(
        build_app_with_services(),
        "POST",
        "/chat",
        &[("authorization", "Bearer contract-chat-token")],
        Some(case.request),
    )
    .await;

    assert_eq!(status.as_u16(), case.status);
    assert_eq!(json, case.json);
}

#[tokio::test]
async fn create_run_existing_session_matches_shared_contract() {
    let contract = load_contract();
    let case = contract.create_run_existing_session;

    let (status, json) = request_json(
        build_app_with_services(),
        "POST",
        "/chat",
        &[("authorization", "Bearer contract-chat-token")],
        Some(case.request),
    )
    .await;

    assert_eq!(status.as_u16(), case.status);
    assert_eq!(json, case.json);
}

#[tokio::test]
async fn create_run_missing_session_matches_shared_contract() {
    let contract = load_contract();
    let case = contract.create_run_missing_session;

    let (status, json) = request_json(
        build_app_with_services(),
        "POST",
        "/chat",
        &[("authorization", "Bearer contract-chat-token")],
        Some(case.request),
    )
    .await;

    assert_eq!(status.as_u16(), case.status);
    assert_eq!(json, case.json);
}

#[tokio::test]
async fn stream_chat_success_matches_shared_contract() {
    let contract = load_contract();
    let case = contract.stream_chat_success;

    let (status, headers, events) = request_sse(
        build_app_with_services(),
        "POST",
        "/chat/stream",
        &[("authorization", "Bearer contract-chat-token")],
        Some(case.request),
    )
    .await;

    assert_eq!(status.as_u16(), case.status);
    assert_sse_headers(&headers, &case.headers);
    assert_eq!(events, case.events);
}

#[tokio::test]
async fn stream_chat_missing_session_matches_shared_contract() {
    let contract = load_contract();
    let case = contract.stream_chat_missing_session;

    let (status, headers, events) = request_sse(
        build_app_with_services(),
        "POST",
        "/chat/stream",
        &[("authorization", "Bearer contract-chat-token")],
        Some(case.request),
    )
    .await;

    assert_eq!(status.as_u16(), case.status);
    assert_sse_headers(&headers, &case.headers);
    assert_eq!(events, case.events);
}

#[tokio::test]
async fn get_run_status_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = request_json(
        build_app_with_services(),
        "GET",
        "/chat/runs/contract-run-live",
        &[("authorization", "Bearer contract-chat-token")],
        None,
    )
    .await;

    assert_eq!(status.as_u16(), contract.get_run_status_success.status);
    assert_eq!(json, contract.get_run_status_success.json);
}

#[tokio::test]
async fn get_run_status_not_found_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = request_json(
        build_app_with_services(),
        "GET",
        "/chat/runs/contract-run-missing",
        &[("authorization", "Bearer contract-chat-token")],
        None,
    )
    .await;

    assert_eq!(status.as_u16(), contract.get_run_status_not_found.status);
    assert_eq!(json, contract.get_run_status_not_found.json);
}

#[tokio::test]
async fn get_run_status_forbidden_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = request_json(
        build_app_with_services(),
        "GET",
        "/chat/runs/contract-run-foreign",
        &[("authorization", "Bearer contract-chat-token")],
        None,
    )
    .await;

    assert_eq!(status.as_u16(), contract.get_run_status_forbidden.status);
    assert_eq!(json, contract.get_run_status_forbidden.json);
}

#[tokio::test]
async fn stream_run_matches_shared_contract() {
    let contract = load_contract();

    let (status, headers, events) = request_sse(
        build_app_with_services(),
        "GET",
        "/chat/runs/contract-run-live/stream?last_index=1",
        &[("authorization", "Bearer contract-chat-token")],
        None,
    )
    .await;

    assert_eq!(status.as_u16(), contract.stream_run_success.status);
    assert_sse_headers(&headers, &contract.stream_run_success.headers);
    assert_eq!(events, contract.stream_run_success.events);
}

#[tokio::test]
async fn stream_run_not_found_matches_shared_contract() {
    let contract = load_contract();

    let (status, headers, events) = request_sse(
        build_app_with_services(),
        "GET",
        "/chat/runs/contract-run-missing/stream",
        &[("authorization", "Bearer contract-chat-token")],
        None,
    )
    .await;

    assert_eq!(status.as_u16(), contract.stream_run_not_found.status);
    assert_sse_headers(&headers, &contract.stream_run_not_found.headers);
    assert_eq!(events, contract.stream_run_not_found.events);
}

#[tokio::test]
async fn stream_run_forbidden_matches_shared_contract() {
    let contract = load_contract();

    let (status, headers, events) = request_sse(
        build_app_with_services(),
        "GET",
        "/chat/runs/contract-run-foreign/stream",
        &[("authorization", "Bearer contract-chat-token")],
        None,
    )
    .await;

    assert_eq!(status.as_u16(), contract.stream_run_forbidden.status);
    assert_sse_headers(&headers, &contract.stream_run_forbidden.headers);
    assert_eq!(events, contract.stream_run_forbidden.events);
}

#[tokio::test]
async fn cancel_run_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = request_json(
        build_app_with_services(),
        "DELETE",
        "/chat/runs/contract-run-cancellable",
        &[("authorization", "Bearer contract-chat-token")],
        None,
    )
    .await;

    assert_eq!(status.as_u16(), contract.cancel_run_success.status);
    assert_eq!(json, contract.cancel_run_success.json);
}

#[tokio::test]
async fn cancel_finished_run_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = request_json(
        build_app_with_services(),
        "DELETE",
        "/chat/runs/contract-run-finished",
        &[("authorization", "Bearer contract-chat-token")],
        None,
    )
    .await;

    assert_eq!(status.as_u16(), contract.cancel_run_finished.status);
    assert_eq!(json, contract.cancel_run_finished.json);
}

#[tokio::test]
async fn cancel_run_forbidden_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = request_json(
        build_app_with_services(),
        "DELETE",
        "/chat/runs/contract-run-foreign",
        &[("authorization", "Bearer contract-chat-token")],
        None,
    )
    .await;

    assert_eq!(status.as_u16(), contract.cancel_run_forbidden.status);
    assert_eq!(json, contract.cancel_run_forbidden.json);
}
