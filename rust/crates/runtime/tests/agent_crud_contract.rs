use std::sync::{Arc, Mutex};

use astra_runtime::{
    AgentCreateRequestData, AgentListRecord, AgentRecord, AgentService, AgentUpdateRequestData,
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ErrorResponse, HealthChecker, ServiceInfo, build_app,
};
use async_trait::async_trait;
use axum::{
    body,
    http::{HeaderMap, Request, StatusCode},
};
use tower::util::ServiceExt;

// ── Stubs ────────────────────────────────────────────────────────────────────

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
        _: AuthRegisterRequestData,
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }
    async fn login(
        &self,
        _: AuthLoginRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }
    async fn refresh(
        &self,
        _: AuthRefreshRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }
    async fn logout(
        &self,
        _: AuthRefreshRequestData,
    ) -> Result<(), (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }

    async fn current_user(
        &self,
        headers: &HeaderMap,
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        match headers.get("authorization").and_then(|v| v.to_str().ok()) {
            Some("Bearer contract-agent-token") => Ok(AuthUserRecord {
                user_id: "contract-agent-user-id".to_string(),
                username: "contract-agent-user".to_string(),
                email: "agent-user@test.com".to_string(),
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
struct StubAgentService {
    state: Arc<Mutex<Vec<AgentRecord>>>,
}

impl StubAgentService {
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(vec![AgentRecord {
                agent_id: "contract-agent-1".to_string(),
                name: "Alpha Agent".to_string(),
                agent_type: "general".to_string(),
                owner_user_id: "contract-agent-user-id".to_string(),
                agent_config: serde_json::json!({"model": "gpt-4"}),
                data_source: serde_json::json!({"type": "matrixone"}),
                is_active: true,
                created_at: "2026-01-01T00:00:00".to_string(),
                updated_at: Some("2026-01-01T00:05:00".to_string()),
            }])),
        }
    }
}

#[async_trait]
impl AgentService for StubAgentService {
    async fn create_agent(
        &self,
        user_id: String,
        request: AgentCreateRequestData,
    ) -> Result<AgentRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        let record = AgentRecord {
            agent_id: "contract-created-agent".to_string(),
            name: request.name,
            agent_type: "general".to_string(),
            owner_user_id: user_id,
            agent_config: request.agent_config.unwrap_or(serde_json::json!({})),
            data_source: request.data_source.unwrap_or(serde_json::json!({})),
            is_active: true,
            created_at: "2026-01-01T00:00:00".to_string(),
            updated_at: None,
        };
        self.state.lock().unwrap().push(record.clone());
        Ok(record)
    }

    async fn list_agents(
        &self,
        user_id: String,
    ) -> Result<AgentListRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        let agents: Vec<_> = self
            .state
            .lock()
            .unwrap()
            .iter()
            .filter(|a| a.owner_user_id == user_id)
            .cloned()
            .collect();
        let total = agents.len() as i64;
        Ok(AgentListRecord { agents, total })
    }

    async fn get_agent(
        &self,
        agent_id: String,
        user_id: String,
    ) -> Result<AgentRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        let agents = self.state.lock().unwrap();
        agents
            .iter()
            .find(|a| a.agent_id == agent_id && a.owner_user_id == user_id)
            .cloned()
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    axum::Json(ErrorResponse {
                        detail: format!("Agent {} not found", agent_id),
                    }),
                )
            })
    }

    async fn update_agent(
        &self,
        agent_id: String,
        user_id: String,
        request: AgentUpdateRequestData,
    ) -> Result<AgentRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        let mut agents = self.state.lock().unwrap();
        let agent = agents
            .iter_mut()
            .find(|a| a.agent_id == agent_id && a.owner_user_id == user_id)
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    axum::Json(ErrorResponse {
                        detail: format!("Agent {} not found", agent_id),
                    }),
                )
            })?;
        if let Some(name) = request.name {
            agent.name = name;
        }
        if let Some(config) = request.agent_config {
            agent.agent_config = config;
        }
        if let Some(source) = request.data_source {
            agent.data_source = source;
        }
        if let Some(active) = request.is_active {
            agent.is_active = active;
        }
        agent.updated_at = Some("2026-01-01T00:10:00".to_string());
        Ok(agent.clone())
    }

    async fn delete_agent(
        &self,
        agent_id: String,
        user_id: String,
    ) -> Result<(), (StatusCode, axum::Json<ErrorResponse>)> {
        let mut agents = self.state.lock().unwrap();
        let idx = agents
            .iter()
            .position(|a| a.agent_id == agent_id && a.owner_user_id == user_id)
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    axum::Json(ErrorResponse {
                        detail: format!("Agent {} not found", agent_id),
                    }),
                )
            })?;
        agents.remove(idx);
        Ok(())
    }
}

fn build_test_app() -> axum::Router {
    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
        .with_auth_service(Arc::new(StubAuthService))
        .with_agent_service(Arc::new(StubAgentService::new()));
    build_app(state)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn agents_require_auth() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/agents")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn create_agent_returns_201() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agents")
                .header("authorization", "Bearer contract-agent-token")
                .header("content-type", "application/json")
                .body(body::Body::from(r#"{"name":"Test Agent","agent_config":{"model":"gpt-4"},"data_source":{"type":"matrixone"}}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["name"], "Test Agent");
    assert_eq!(json["agent_type"], "general");
    assert_eq!(json["owner_user_id"], "contract-agent-user-id");
}

#[tokio::test]
async fn list_agents_matches_contract() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/agents")
                .header("authorization", "Bearer contract-agent-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["total"], 1);
    assert_eq!(json["agents"][0]["name"], "Alpha Agent");
}

#[tokio::test]
async fn get_agent_matches_contract() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/agents/contract-agent-1")
                .header("authorization", "Bearer contract-agent-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["agent_id"], "contract-agent-1");
    assert_eq!(json["name"], "Alpha Agent");
}

#[tokio::test]
async fn get_agent_not_found() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/agents/nonexistent")
                .header("authorization", "Bearer contract-agent-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn update_agent_matches_contract() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/agents/contract-agent-1")
                .header("authorization", "Bearer contract-agent-token")
                .header("content-type", "application/json")
                .body(body::Body::from(r#"{"name":"Updated Agent"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["name"], "Updated Agent");
}

#[tokio::test]
async fn delete_agent_returns_204() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/agents/contract-agent-1")
                .header("authorization", "Bearer contract-agent-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn delete_agent_not_found() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/agents/nonexistent")
                .header("authorization", "Bearer contract-agent-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
