use std::sync::{Arc, Mutex};

use astra_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ContextService, ErrorResponse, HealthChecker, ServiceInfo,
    SnapshotCreateRequestData, SnapshotListFilter, SnapshotListItem, SnapshotListRecord,
    SnapshotRecord, build_app,
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
            Some("Bearer contract-context-token") => Ok(AuthUserRecord {
                user_id: "contract-context-user-id".to_string(),
                username: "contract-context-user".to_string(),
                email: "context-user@test.com".to_string(),
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
struct StubContextService {
    state: Arc<Mutex<Vec<SnapshotRecord>>>,
}

impl StubContextService {
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(vec![SnapshotRecord {
                context_capture_id: "contract-snapshot-1".to_string(),
                session_id: "contract-session-1".to_string(),
                event_id: "contract-event-1".to_string(),
                context_data: serde_json::json!({"key": "value"}),
                created_at: "2026-01-01T00:00:00".to_string(),
            }])),
        }
    }
}

#[async_trait]
impl ContextService for StubContextService {
    async fn create_snapshot(
        &self,
        _user_id: String,
        request: SnapshotCreateRequestData,
    ) -> Result<SnapshotRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        let record = SnapshotRecord {
            context_capture_id: "contract-created-snapshot".to_string(),
            session_id: request.session_id,
            event_id: request.event_id,
            context_data: request.context_data,
            created_at: "2026-01-01T00:00:00".to_string(),
        };
        self.state.lock().unwrap().push(record.clone());
        Ok(record)
    }

    async fn list_snapshots(
        &self,
        filter: SnapshotListFilter,
    ) -> Result<SnapshotListRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        let snapshots = self.state.lock().unwrap().clone();
        let total = snapshots.len() as i64;
        let snapshots = snapshots
            .into_iter()
            .skip(filter.offset as usize)
            .take(filter.limit as usize)
            .map(|snapshot| SnapshotListItem {
                context_capture_id: snapshot.context_capture_id,
                session_id: snapshot.session_id,
                event_id: snapshot.event_id,
                created_at: snapshot.created_at,
            })
            .collect();
        Ok(SnapshotListRecord {
            snapshots,
            total,
            limit: filter.limit,
            offset: filter.offset,
        })
    }

    async fn get_snapshot(
        &self,
        context_capture_id: String,
        _user_id: String,
    ) -> Result<SnapshotRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        self.state
            .lock()
            .unwrap()
            .iter()
            .find(|s| s.context_capture_id == context_capture_id)
            .cloned()
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    axum::Json(ErrorResponse {
                        detail: format!("Snapshot {} not found", context_capture_id),
                    }),
                )
            })
    }
}

fn build_test_app() -> axum::Router {
    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
        .with_auth_service(Arc::new(StubAuthService))
        .with_context_service(Arc::new(StubContextService::new()));
    build_app(state)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_snapshot_returns_201() {
    let app = build_test_app();
    let payload = serde_json::json!({
        "session_id": "s1",
        "event_id": "e1",
        "context_data": {"files": ["main.rs"]}
    });
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/context")
                .header("authorization", "Bearer contract-context-token")
                .header("content-type", "application/json")
                .body(body::Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["session_id"], "s1");
    assert_eq!(json["event_id"], "e1");
}

#[tokio::test]
async fn list_snapshots_returns_ok() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/context?limit=10&offset=0")
                .header("authorization", "Bearer contract-context-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["total"], 1);
    assert!(!json["snapshots"].as_array().unwrap().is_empty());
    assert!(json["snapshots"][0].get("context_data").is_none());
}

#[tokio::test]
async fn get_snapshot_by_id() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/context/contract-snapshot-1")
                .header("authorization", "Bearer contract-context-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["context_capture_id"], "contract-snapshot-1");
}

#[tokio::test]
async fn get_snapshot_not_found() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/context/nonexistent")
                .header("authorization", "Bearer contract-context-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
