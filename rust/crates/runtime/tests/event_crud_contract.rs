use std::sync::{Arc, Mutex};

use astra_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ErrorResponse, EventCreateRequestData, EventListFilter,
    EventListRecord, EventRecord, EventService, HealthChecker, ServiceInfo, build_app,
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
            Some("Bearer contract-event-token") => Ok(AuthUserRecord {
                user_id: "contract-event-user-id".to_string(),
                username: "contract-event-user".to_string(),
                email: "event-user@test.com".to_string(),
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
struct StubEventService {
    state: Arc<Mutex<Vec<EventRecord>>>,
}

impl StubEventService {
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(vec![EventRecord {
                event_id: "contract-event-1".to_string(),
                user_id: "contract-event-user-id".to_string(),
                session_id: "contract-session-1".to_string(),
                event_type: "user_query".to_string(),
                content: "Hello world".to_string(),
                agent_id: Some("system".to_string()),
                agent_version: Some("1.0.0".to_string()),
                parent_event_id: None,
                causal_chain_id: "chain-1".to_string(),
                metadata: serde_json::json!({}),
                created_at: "2026-01-01T00:00:00".to_string(),
            }])),
        }
    }
}

#[async_trait]
impl EventService for StubEventService {
    async fn create_event(
        &self,
        user_id: String,
        request: EventCreateRequestData,
    ) -> Result<EventRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        let record = EventRecord {
            event_id: "contract-created-event".to_string(),
            user_id,
            session_id: request.session_id,
            event_type: request.event_type,
            content: request.content,
            agent_id: request.agent_id.or(Some("system".to_string())),
            agent_version: request.agent_version.or(Some("1.0.0".to_string())),
            parent_event_id: request.parent_event_id,
            causal_chain_id: request.causal_chain_id.unwrap_or("chain-new".to_string()),
            metadata: request.metadata.unwrap_or(serde_json::json!({})),
            created_at: "2026-01-01T00:00:00".to_string(),
        };
        self.state.lock().unwrap().push(record.clone());
        Ok(record)
    }

    async fn list_events(
        &self,
        filter: EventListFilter,
    ) -> Result<EventListRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        let events: Vec<_> = self
            .state
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.user_id == filter.user_id)
            .cloned()
            .collect();
        let total = events.len() as i64;
        Ok(EventListRecord {
            events,
            total,
            limit: filter.limit,
            offset: filter.offset,
        })
    }

    async fn get_event(
        &self,
        event_id: String,
        user_id: String,
    ) -> Result<EventRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        let events = self.state.lock().unwrap();
        events
            .iter()
            .find(|e| e.event_id == event_id && e.user_id == user_id)
            .cloned()
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    axum::Json(ErrorResponse {
                        detail: format!("Event {} not found", event_id),
                    }),
                )
            })
    }

    async fn get_causal_chain(
        &self,
        causal_chain_id: String,
        user_id: String,
    ) -> Result<Vec<EventRecord>, (StatusCode, axum::Json<ErrorResponse>)> {
        let events: Vec<_> = self
            .state
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.causal_chain_id == causal_chain_id && e.user_id == user_id)
            .cloned()
            .collect();
        Ok(events)
    }

    async fn get_session_events(
        &self,
        session_id: String,
        user_id: String,
        limit: u32,
        offset: u32,
    ) -> Result<EventListRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        let events: Vec<_> = self
            .state
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.session_id == session_id && e.user_id == user_id)
            .cloned()
            .collect();
        let total = events.len() as i64;
        Ok(EventListRecord {
            events,
            total,
            limit,
            offset,
        })
    }

    async fn delete_event(
        &self,
        event_id: String,
        user_id: String,
    ) -> Result<(), (StatusCode, axum::Json<ErrorResponse>)> {
        let mut events = self.state.lock().unwrap();
        let idx = events
            .iter()
            .position(|e| e.event_id == event_id && e.user_id == user_id)
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    axum::Json(ErrorResponse {
                        detail: format!("Event {} not found", event_id),
                    }),
                )
            })?;
        events.remove(idx);
        Ok(())
    }
}

fn build_test_app() -> axum::Router {
    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
        .with_auth_service(Arc::new(StubAuthService))
        .with_event_service(Arc::new(StubEventService::new()));
    build_app(state)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn events_require_auth() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/events")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn create_event_returns_201() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/events")
                .header("authorization", "Bearer contract-event-token")
                .header("content-type", "application/json")
                .body(body::Body::from(
                    r#"{"session_id":"s1","event_type":"user_query","content":"hi"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["content"], "hi");
    assert_eq!(json["event_type"], "user_query");
}

#[tokio::test]
async fn list_events_matches_contract() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/events")
                .header("authorization", "Bearer contract-event-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["total"], 1);
    assert_eq!(json["events"][0]["event_type"], "user_query");
}

#[tokio::test]
async fn get_event_matches_contract() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/events/contract-event-1")
                .header("authorization", "Bearer contract-event-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn get_event_not_found() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/events/nonexistent")
                .header("authorization", "Bearer contract-event-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_causal_chain_returns_list() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/events/causal-chain/chain-1")
                .header("authorization", "Bearer contract-event-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(!json.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn get_session_events_returns_list() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/events/session/contract-session-1")
                .header("authorization", "Bearer contract-event-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["total"], 1);
}

#[tokio::test]
async fn delete_event_returns_204() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/events/contract-event-1")
                .header("authorization", "Bearer contract-event-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}
