use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use axum::{
    Router, body,
    http::{HeaderMap, Request, StatusCode},
};
use mo_agent_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ErrorResponse, HealthChecker, ServiceInfo,
    SessionActivityRecord, SessionCreateRequestData, SessionListFilter, SessionListRecord,
    SessionRecord, SessionService, SessionUpdateRequestData, build_app,
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
struct DeleteContract {
    status: u16,
}

#[derive(Deserialize)]
struct SessionContract {
    auth_error: ResponseContract,
    create_session: RequestContract,
    list_sessions: ResponseContract,
    list_sessions_filtered: ResponseContract,
    get_session: ResponseContract,
    get_session_not_found: ResponseContract,
    get_session_unauthorized: ResponseContract,
    update_session: RequestContract,
    update_session_not_found: RequestContract,
    update_session_unauthorized: RequestContract,
    delete_session: DeleteContract,
    delete_session_not_found: ResponseContract,
    close_session: ResponseContract,
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
        unreachable!("register is not used in session contract tests")
    }

    async fn login(
        &self,
        _request: AuthLoginRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!("login is not used in session contract tests")
    }

    async fn refresh(
        &self,
        _request: AuthRefreshRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!("refresh is not used in session contract tests")
    }

    async fn logout(
        &self,
        _request: AuthRefreshRequestData,
    ) -> Result<(), (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!("logout is not used in session contract tests")
    }

    async fn current_user(
        &self,
        headers: &HeaderMap,
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        match headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
        {
            Some("Bearer contract-session-token") => Ok(AuthUserRecord {
                user_id: "contract-session-user-id".to_string(),
                username: "contract-session-user".to_string(),
                email: "contract-session-user@test.com".to_string(),
                display_name: Some("Contract Session User".to_string()),
            }),
            Some("Bearer contract-other-token") => Ok(AuthUserRecord {
                user_id: "contract-other-session-user-id".to_string(),
                username: "contract-other-session-user".to_string(),
                email: "contract-other-session-user@test.com".to_string(),
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
struct StubSessionService {
    state: Arc<Mutex<SessionState>>,
}

#[derive(Default)]
struct SessionState {
    sessions: HashMap<String, SessionRecord>,
    next_created_id: usize,
}

impl StubSessionService {
    fn new() -> Self {
        let sessions = HashMap::from([
            (
                "contract-session-active".to_string(),
                SessionRecord {
                    session_id: "contract-session-active".to_string(),
                    user_id: "contract-session-user-id".to_string(),
                    agent_id: Some("contract-agent-alpha".to_string()),
                    title: Some("Contract Session Alpha".to_string()),
                    metadata: serde_json::Map::from_iter([
                        ("source".to_string(), serde_json::Value::from("contract")),
                        ("topic".to_string(), serde_json::Value::from("alpha")),
                    ]),
                    status: "active".to_string(),
                    event_count: 1,
                    created_at: "2026-01-02T08:00:00".to_string(),
                    updated_at: Some("2026-01-02T08:15:00".to_string()),
                    ended_at: None,
                },
            ),
            (
                "contract-session-closed".to_string(),
                SessionRecord {
                    session_id: "contract-session-closed".to_string(),
                    user_id: "contract-session-user-id".to_string(),
                    agent_id: Some("contract-agent-beta".to_string()),
                    title: Some("Contract Session Beta".to_string()),
                    metadata: serde_json::Map::from_iter([
                        ("source".to_string(), serde_json::Value::from("contract")),
                        ("topic".to_string(), serde_json::Value::from("beta")),
                    ]),
                    status: "closed".to_string(),
                    event_count: 2,
                    created_at: "2026-01-03T09:30:00".to_string(),
                    updated_at: Some("2026-01-03T10:00:00".to_string()),
                    ended_at: None,
                },
            ),
            (
                "contract-foreign-session".to_string(),
                SessionRecord {
                    session_id: "contract-foreign-session".to_string(),
                    user_id: "contract-other-session-user-id".to_string(),
                    agent_id: Some("contract-agent-foreign".to_string()),
                    title: Some("Foreign Session".to_string()),
                    metadata: serde_json::Map::from_iter([(
                        "source".to_string(),
                        serde_json::Value::from("foreign"),
                    )]),
                    status: "active".to_string(),
                    event_count: 4,
                    created_at: "2026-01-04T11:00:00".to_string(),
                    updated_at: Some("2026-01-04T11:05:00".to_string()),
                    ended_at: None,
                },
            ),
        ]);

        Self {
            state: Arc::new(Mutex::new(SessionState {
                sessions,
                next_created_id: 1,
            })),
        }
    }
}

#[async_trait]
impl SessionService for StubSessionService {
    async fn create_session(
        &self,
        user_id: String,
        request: SessionCreateRequestData,
    ) -> Result<SessionRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        let mut state = self.state.lock().unwrap();
        let session_id = format!("contract-created-session-{}", state.next_created_id);
        state.next_created_id += 1;

        let record = SessionRecord {
            session_id: session_id.clone(),
            user_id,
            agent_id: request.agent_id,
            title: request
                .title
                .or(Some("Session 2026-01-04 14:00".to_string())),
            metadata: request.metadata.unwrap_or_default(),
            status: "active".to_string(),
            event_count: 0,
            created_at: "2026-01-04T14:00:00".to_string(),
            updated_at: Some("2026-01-04T14:00:00".to_string()),
            ended_at: None,
        };

        state.sessions.insert(session_id, record.clone());
        Ok(record)
    }

    async fn list_sessions(
        &self,
        filter: SessionListFilter,
    ) -> Result<SessionListRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        let state = self.state.lock().unwrap();
        let mut sessions = state
            .sessions
            .values()
            .filter(|record| record.user_id == filter.user_id)
            .cloned()
            .collect::<Vec<_>>();

        if let Some(agent_id) = filter.agent_id {
            sessions.retain(|record| record.agent_id.as_deref() == Some(agent_id.as_str()));
        }
        if let Some(status) = filter.status {
            sessions.retain(|record| record.status == status);
        }

        sessions.sort_by(|left, right| right.created_at.cmp(&left.created_at));

        let total = sessions.len() as i64;
        let sessions = sessions
            .into_iter()
            .skip(filter.offset as usize)
            .take(filter.limit as usize)
            .collect::<Vec<_>>();

        Ok(SessionListRecord {
            sessions,
            total,
            limit: filter.limit,
            offset: filter.offset,
        })
    }

    async fn get_session(
        &self,
        session_id: String,
        user_id: String,
    ) -> Result<SessionRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        let state = self.state.lock().unwrap();
        let session = state.sessions.get(&session_id).cloned().ok_or_else(|| {
            session_error(
                StatusCode::NOT_FOUND,
                format!("Session {session_id} 不存在"),
            )
        })?;

        if session.user_id != user_id {
            return Err(session_error(
                StatusCode::NOT_FOUND,
                format!("无权限访问 Session {session_id}"),
            ));
        }

        Ok(session)
    }

    async fn update_session(
        &self,
        session_id: String,
        user_id: String,
        request: SessionUpdateRequestData,
    ) -> Result<SessionRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        let mut state = self.state.lock().unwrap();
        let session = state.sessions.get_mut(&session_id).ok_or_else(|| {
            session_error(
                StatusCode::NOT_FOUND,
                format!("Session {session_id} 不存在"),
            )
        })?;

        if session.user_id != user_id {
            return Err(session_error(
                StatusCode::NOT_FOUND,
                format!("无权限修改 Session {session_id}"),
            ));
        }

        if let Some(title) = request.title {
            session.title = Some(title);
        }
        if let Some(metadata) = request.metadata {
            session.metadata = metadata;
        }
        if let Some(status) = request.status {
            if status == "ended" {
                session.ended_at = Some("2026-01-05T08:15:00".to_string());
            }
            session.status = status;
        }
        session.updated_at = Some("2026-01-05T08:15:00".to_string());

        Ok(session.clone())
    }

    async fn delete_session(
        &self,
        session_id: String,
        user_id: String,
    ) -> Result<(), (StatusCode, axum::Json<ErrorResponse>)> {
        let mut state = self.state.lock().unwrap();
        let existing = state.sessions.get(&session_id).cloned().ok_or_else(|| {
            session_error(
                StatusCode::NOT_FOUND,
                format!("Session {session_id} 不存在"),
            )
        })?;

        if existing.user_id != user_id {
            return Err(session_error(
                StatusCode::NOT_FOUND,
                format!("无权限删除 Session {session_id}"),
            ));
        }

        state.sessions.remove(&session_id);
        Ok(())
    }

    async fn get_session_activity(
        &self,
        session_id: String,
        _user_id: String,
        _limit: u32,
        _offset: u32,
    ) -> Result<SessionActivityRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        Ok(SessionActivityRecord {
            session_id,
            activities: vec![],
            total: 0,
        })
    }
}

fn session_error(
    status: StatusCode,
    detail: impl Into<String>,
) -> (StatusCode, axum::Json<ErrorResponse>) {
    (
        status,
        axum::Json(ErrorResponse {
            detail: detail.into(),
        }),
    )
}

fn load_contract() -> SessionContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/session_contract.json");
    let content = fs::read_to_string(path).expect("session contract fixture should exist");
    serde_json::from_str(&content).expect("session contract fixture should be valid JSON")
}

fn build_app_with_sessions() -> Router {
    build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_auth_service(Arc::new(StubAuthService))
            .with_session_service(Arc::new(StubSessionService::new())),
    )
}

async fn read_json(
    app: Router,
    path: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, serde_json::Value) {
    let response = app
        .oneshot(build_request("GET", path, headers, None))
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
        .oneshot(build_request("POST", path, headers, Some(payload)))
        .await
        .unwrap();
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap();
    (status, json)
}

async fn put_json(
    app: Router,
    path: &str,
    headers: &[(&str, &str)],
    payload: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let response = app
        .oneshot(build_request("PUT", path, headers, Some(payload)))
        .await
        .unwrap();
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap();
    (status, json)
}

async fn delete_status(app: Router, path: &str, headers: &[(&str, &str)]) -> StatusCode {
    let response = app
        .oneshot(build_request("DELETE", path, headers, None))
        .await
        .unwrap();
    response.status()
}

fn build_request(
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    payload: Option<serde_json::Value>,
) -> Request<body::Body> {
    let mut builder = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    if matches!(method, "POST" | "PUT") {
        builder = builder.header("content-type", "application/json");
    }
    let body = payload
        .map(|value| body::Body::from(value.to_string()))
        .unwrap_or_else(body::Body::empty);
    builder.body(body).unwrap()
}

fn assert_session_subset(actual: &serde_json::Value, expected: &serde_json::Value) {
    let expected = expected.as_object().expect("expected contract object");
    for (key, value) in expected {
        assert_eq!(actual.get(key), Some(value), "field {key} should match");
    }
}

#[tokio::test]
async fn sessions_require_auth() {
    let contract = load_contract();

    let (status, json) = read_json(build_app_with_sessions(), "/sessions", &[]).await;

    assert_eq!(status.as_u16(), contract.auth_error.status);
    assert_eq!(json, contract.auth_error.json);
}

#[tokio::test]
async fn create_session_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = post_json(
        build_app_with_sessions(),
        "/sessions",
        &[("authorization", "Bearer contract-session-token")],
        contract.create_session.request.clone(),
    )
    .await;

    assert_eq!(status.as_u16(), contract.create_session.status);
    assert_session_subset(&json, &contract.create_session.json);
    assert!(
        json["session_id"]
            .as_str()
            .unwrap()
            .starts_with("contract-created-session-")
    );
    assert_eq!(
        json["created_at"],
        serde_json::Value::String("2026-01-04T14:00:00".into())
    );
    assert_eq!(
        json["updated_at"],
        serde_json::Value::String("2026-01-04T14:00:00".into())
    );
}

#[tokio::test]
async fn list_sessions_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = read_json(
        build_app_with_sessions(),
        "/sessions",
        &[("authorization", "Bearer contract-session-token")],
    )
    .await;

    assert_eq!(status.as_u16(), contract.list_sessions.status);
    assert_eq!(json, contract.list_sessions.json);
}

#[tokio::test]
async fn list_sessions_filters_match_shared_contract() {
    let contract = load_contract();

    let (status, json) = read_json(
        build_app_with_sessions(),
        "/sessions?agent_id=contract-agent-alpha&session_status=active&limit=1&offset=0",
        &[("authorization", "Bearer contract-session-token")],
    )
    .await;

    assert_eq!(status.as_u16(), contract.list_sessions_filtered.status);
    assert_eq!(json, contract.list_sessions_filtered.json);
}

#[tokio::test]
async fn get_session_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = read_json(
        build_app_with_sessions(),
        "/sessions/contract-session-active",
        &[("authorization", "Bearer contract-session-token")],
    )
    .await;

    assert_eq!(status.as_u16(), contract.get_session.status);
    assert_eq!(json, contract.get_session.json);
}

#[tokio::test]
async fn get_session_not_found_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = read_json(
        build_app_with_sessions(),
        "/sessions/contract-missing-session",
        &[("authorization", "Bearer contract-session-token")],
    )
    .await;

    assert_eq!(status.as_u16(), contract.get_session_not_found.status);
    assert_eq!(json, contract.get_session_not_found.json);
}

#[tokio::test]
async fn get_session_unauthorized_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = read_json(
        build_app_with_sessions(),
        "/sessions/contract-foreign-session",
        &[("authorization", "Bearer contract-session-token")],
    )
    .await;

    assert_eq!(status.as_u16(), contract.get_session_unauthorized.status);
    assert_eq!(json, contract.get_session_unauthorized.json);
}

#[tokio::test]
async fn update_session_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = put_json(
        build_app_with_sessions(),
        "/sessions/contract-session-active",
        &[("authorization", "Bearer contract-session-token")],
        contract.update_session.request.clone(),
    )
    .await;

    assert_eq!(status.as_u16(), contract.update_session.status);
    assert_session_subset(&json, &contract.update_session.json);
    assert_eq!(
        json["updated_at"],
        serde_json::Value::String("2026-01-05T08:15:00".into())
    );
}

#[tokio::test]
async fn update_session_not_found_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = put_json(
        build_app_with_sessions(),
        "/sessions/contract-missing-session",
        &[("authorization", "Bearer contract-session-token")],
        contract.update_session_not_found.request.clone(),
    )
    .await;

    assert_eq!(status.as_u16(), contract.update_session_not_found.status);
    assert_eq!(json, contract.update_session_not_found.json);
}

#[tokio::test]
async fn update_session_unauthorized_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = put_json(
        build_app_with_sessions(),
        "/sessions/contract-foreign-session",
        &[("authorization", "Bearer contract-session-token")],
        contract.update_session_unauthorized.request.clone(),
    )
    .await;

    assert_eq!(status.as_u16(), contract.update_session_unauthorized.status);
    assert_eq!(json, contract.update_session_unauthorized.json);
}

#[tokio::test]
async fn delete_session_matches_shared_contract() {
    let contract = load_contract();
    let app = build_app_with_sessions();

    let status = delete_status(
        app.clone(),
        "/sessions/contract-session-active",
        &[("authorization", "Bearer contract-session-token")],
    )
    .await;
    let (get_status, _) = read_json(
        app,
        "/sessions/contract-session-active",
        &[("authorization", "Bearer contract-session-token")],
    )
    .await;

    assert_eq!(status.as_u16(), contract.delete_session.status);
    assert_eq!(get_status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_session_not_found_matches_shared_contract() {
    let contract = load_contract();
    let app = build_app_with_sessions();

    let status = delete_status(
        app.clone(),
        "/sessions/contract-missing-session",
        &[("authorization", "Bearer contract-session-token")],
    )
    .await;

    let (get_status, json) = read_json(
        app,
        "/sessions/contract-missing-session",
        &[("authorization", "Bearer contract-session-token")],
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        get_status.as_u16(),
        contract.delete_session_not_found.status
    );
    assert_eq!(json, contract.delete_session_not_found.json);
}

#[tokio::test]
async fn close_session_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = post_json(
        build_app_with_sessions(),
        "/sessions/contract-session-active/close",
        &[("authorization", "Bearer contract-session-token")],
        serde_json::json!({}),
    )
    .await;

    assert_eq!(status.as_u16(), contract.close_session.status);
    assert_session_subset(&json, &contract.close_session.json);
    assert_eq!(
        json["updated_at"],
        serde_json::Value::String("2026-01-05T08:15:00".into())
    );
    assert_eq!(json["ended_at"], serde_json::Value::Null);
}
