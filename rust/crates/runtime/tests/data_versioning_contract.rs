use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    Json, body,
    http::{HeaderMap, Request, StatusCode},
};
use mo_agent_runtime::data_versioning::{
    CheckpointResponse, CreateCheckpointData, DataVersioningService, EventAtCheckpoint,
    LineageNode, SandboxCheckpointData, StatusResponse,
};
use mo_agent_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ErrorResponse, HealthChecker, ServiceInfo, build_app,
};
use tokio::sync::Mutex;
use tower::util::ServiceExt;

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
        let user_id = headers
            .get("X-User-Id")
            .and_then(|v| v.to_str().ok())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                (
                    StatusCode::UNAUTHORIZED,
                    axum::Json(ErrorResponse {
                        detail: "Missing X-User-Id header".to_string(),
                    }),
                )
            })?;

        Ok(AuthUserRecord {
            user_id: user_id.to_string(),
            username: format!("user-{user_id}"),
            email: format!("{user_id}@example.test"),
            display_name: None,
        })
    }
}

#[derive(Clone)]
struct InMemoryDataVersioningService {
    checkpoints: Arc<Mutex<HashMap<String, CheckpointResponse>>>,
}

impl InMemoryDataVersioningService {
    fn new() -> Self {
        Self {
            checkpoints: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[async_trait]
impl DataVersioningService for InMemoryDataVersioningService {
    async fn create_checkpoint(
        &self,
        _user_id: String,
        request: CreateCheckpointData,
    ) -> Result<CheckpointResponse, (StatusCode, Json<ErrorResponse>)> {
        let cp = CheckpointResponse {
            checkpoint_name: request.name.clone(),
            timestamp: "2026-01-01T00:00:00".to_string(),
            description: request.description,
        };
        self.checkpoints
            .lock()
            .await
            .insert(request.name, cp.clone());
        Ok(cp)
    }

    async fn list_checkpoints(
        &self,
        _user_id: String,
    ) -> Result<Vec<CheckpointResponse>, (StatusCode, Json<ErrorResponse>)> {
        let mut checkpoints: Vec<CheckpointResponse> =
            self.checkpoints.lock().await.values().cloned().collect();
        checkpoints.sort_by(|a, b| a.checkpoint_name.cmp(&b.checkpoint_name));
        Ok(checkpoints)
    }

    async fn get_events_at_checkpoint(
        &self,
        _user_id: String,
        checkpoint_name: String,
    ) -> Result<Vec<EventAtCheckpoint>, (StatusCode, Json<ErrorResponse>)> {
        if !self.checkpoints.lock().await.contains_key(&checkpoint_name) {
            return Err((
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    detail: format!("Checkpoint '{}' not found", checkpoint_name),
                }),
            ));
        }

        Ok(vec![EventAtCheckpoint {
            event_id: "evt-1".to_string(),
            session_id: "session-1".to_string(),
            event_type: "user_message".to_string(),
            content: "hello".to_string(),
            created_at: "2026-01-01T00:00:00".to_string(),
        }])
    }

    async fn get_causal_chain(
        &self,
        _user_id: String,
        event_id: String,
    ) -> Result<Vec<LineageNode>, (StatusCode, Json<ErrorResponse>)> {
        if event_id == "missing-event" {
            return Err((
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    detail: "Event not found".to_string(),
                }),
            ));
        }

        Ok(vec![LineageNode {
            event_id,
            event_type: "assistant_response".to_string(),
            content: "ok".to_string(),
            parent_event_id: Some("evt-1".to_string()),
            causal_chain_id: Some("chain-1".to_string()),
            created_at: "2026-01-01T00:00:00".to_string(),
        }])
    }

    async fn trace_upstream(
        &self,
        _user_id: String,
        event_id: String,
    ) -> Result<Vec<LineageNode>, (StatusCode, Json<ErrorResponse>)> {
        if event_id == "missing-event" {
            return Err((
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    detail: "Event not found".to_string(),
                }),
            ));
        }

        Ok(vec![LineageNode {
            event_id,
            event_type: "tool_call".to_string(),
            content: "trace".to_string(),
            parent_event_id: None,
            causal_chain_id: Some("chain-1".to_string()),
            created_at: "2026-01-01T00:00:00".to_string(),
        }])
    }

    async fn sandbox_checkpoint(
        &self,
        _user_id: String,
        sandbox_name: String,
        request: SandboxCheckpointData,
    ) -> Result<CheckpointResponse, (StatusCode, Json<ErrorResponse>)> {
        let full_name = format!("{}__{}", sandbox_name, request.checkpoint_name);
        let cp = CheckpointResponse {
            checkpoint_name: full_name.clone(),
            timestamp: "2026-01-01T00:00:00".to_string(),
            description: Some(format!("Sandbox checkpoint for {}", sandbox_name)),
        };
        self.checkpoints.lock().await.insert(full_name, cp.clone());
        Ok(cp)
    }

    async fn sandbox_restore(
        &self,
        _user_id: String,
        sandbox_name: String,
        request: SandboxCheckpointData,
    ) -> Result<StatusResponse, (StatusCode, Json<ErrorResponse>)> {
        let full_name = format!("{}__{}", sandbox_name, request.checkpoint_name);
        if !self.checkpoints.lock().await.contains_key(&full_name) {
            return Err((
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    detail: format!("Checkpoint '{}' not found", full_name),
                }),
            ));
        }

        Ok(StatusResponse {
            status: "restored".to_string(),
        })
    }
}

fn build_test_app() -> axum::Router {
    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
        .with_auth_service(Arc::new(StubAuthService))
        .with_data_versioning_service(Arc::new(InMemoryDataVersioningService::new()));
    build_app(state)
}

async fn response_json(resp: axum::http::Response<body::Body>) -> serde_json::Value {
    let bytes = body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn create_and_list_checkpoints_return_success() {
    let app = build_test_app();

    let create_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/data-versioning/checkpoints")
                .header("X-User-Id", "user-1")
                .header("content-type", "application/json")
                .body(body::Body::from(r#"{"name":"cp-1","description":"first"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(create_resp.status(), StatusCode::CREATED);
    let create_json = response_json(create_resp).await;
    assert_eq!(create_json["checkpoint_name"], "cp-1");

    let list_resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/data-versioning/checkpoints")
                .header("X-User-Id", "user-1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(list_resp.status(), StatusCode::OK);
    let list_json = response_json(list_resp).await;
    assert_eq!(list_json.as_array().unwrap().len(), 1);
    assert_eq!(list_json[0]["checkpoint_name"], "cp-1");
}

#[tokio::test]
async fn get_events_not_found_returns_404() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/data-versioning/checkpoints/missing/events")
                .header("X-User-Id", "user-1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn sandbox_checkpoint_and_restore_return_success() {
    let app = build_test_app();

    let checkpoint_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/data-versioning/sandbox/dev/checkpoint")
                .header("X-User-Id", "user-1")
                .header("content-type", "application/json")
                .body(body::Body::from(r#"{"checkpoint_name":"cp-a"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(checkpoint_resp.status(), StatusCode::CREATED);
    let checkpoint_json = response_json(checkpoint_resp).await;
    assert_eq!(checkpoint_json["checkpoint_name"], "dev__cp-a");

    let restore_resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/data-versioning/sandbox/dev/restore")
                .header("X-User-Id", "user-1")
                .header("content-type", "application/json")
                .body(body::Body::from(r#"{"checkpoint_name":"cp-a"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(restore_resp.status(), StatusCode::OK);
    let restore_json = response_json(restore_resp).await;
    assert_eq!(restore_json["status"], "restored");
}

#[tokio::test]
async fn lineage_endpoints_return_ok() {
    let app = build_test_app();

    let chain_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/data-versioning/lineage/evt-1/chain")
                .header("X-User-Id", "user-1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(chain_resp.status(), StatusCode::OK);

    let upstream_resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/data-versioning/lineage/evt-1/upstream")
                .header("X-User-Id", "user-1")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(upstream_resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn missing_user_id_returns_401() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/data-versioning/checkpoints")
                .header("content-type", "application/json")
                .body(body::Body::from(r#"{"name":"cp-1"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
