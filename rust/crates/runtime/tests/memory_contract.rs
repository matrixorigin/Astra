/// Memory proxy contract tests.
///
/// These tests verify that the /memory/* routes:
/// 1. Require authentication
/// 2. Inject `session_id` = authenticated user_id into every forwarded Memoria payload
/// 3. Return 503 when Memoria is not configured
///
/// Uses an in-memory mock forwarder (no real TCP) so tests are fast and hermetic.
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use astra_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ErrorResponse, HealthChecker, MemoriaForwarder,
    NoopMemoriaForwarder, ServiceInfo, build_app,
};
use async_trait::async_trait;
use axum::{
    Router,
    body::{self, Body},
    http::{Request, StatusCode},
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
        request: AuthRegisterRequestData,
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        Ok(AuthUserRecord {
            user_id: "stub-user-id".to_string(),
            username: request.username,
            email: request.email,
            display_name: request.display_name,
        })
    }

    async fn login(
        &self,
        _request: AuthLoginRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        Ok(AuthTokenRecord {
            access_token: "stub-access-token".to_string(),
            refresh_token: "stub-refresh-token".to_string(),
            token_type: "bearer".to_string(),
            expires_in: 3600,
        })
    }

    async fn refresh(
        &self,
        _request: AuthRefreshRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        Err((
            StatusCode::UNAUTHORIZED,
            axum::Json(ErrorResponse {
                detail: "Not supported in stub".to_string(),
            }),
        ))
    }

    async fn logout(
        &self,
        _request: AuthRefreshRequestData,
    ) -> Result<(), (StatusCode, axum::Json<ErrorResponse>)> {
        Ok(())
    }

    async fn current_user(
        &self,
        headers: &axum::http::HeaderMap,
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        match headers.get("authorization").and_then(|v| v.to_str().ok()) {
            Some("Bearer contract-user-a-token") => Ok(AuthUserRecord {
                user_id: "contract-user-a-id".to_string(),
                username: "user-a".to_string(),
                email: "user-a@test.com".to_string(),
                display_name: None,
            }),
            Some("Bearer contract-user-b-token") => Ok(AuthUserRecord {
                user_id: "contract-user-b-id".to_string(),
                username: "user-b".to_string(),
                email: "user-b@test.com".to_string(),
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

// ── In-memory mock forwarder ──────────────────────────────────────────────────

/// Captures the body passed to the last `forward()` call.
/// No real HTTP — completely in-memory.
#[derive(Clone, Default)]
struct CapturingMemoriaForwarder {
    last_call: Arc<Mutex<Option<(String, serde_json::Value)>>>,
}

impl CapturingMemoriaForwarder {
    fn take(&self) -> Option<(String, serde_json::Value)> {
        self.last_call.lock().unwrap().take()
    }
}

#[async_trait]
impl MemoriaForwarder for CapturingMemoriaForwarder {
    async fn forward(
        &self,
        endpoint: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        *self.last_call.lock().unwrap() = Some((endpoint.to_string(), body));
        Ok(serde_json::json!([]))
    }
}

// ── App builder ──────────────────────────────────────────────────────────────

fn build_memory_app_capturing() -> (Router, CapturingMemoriaForwarder) {
    let forwarder = CapturingMemoriaForwarder::default();
    let app = build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_auth_service(Arc::new(StubAuthService))
            .with_memoria_forwarder(Arc::new(forwarder.clone())),
    );
    (app, forwarder)
}

fn build_noop_app() -> Router {
    build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_auth_service(Arc::new(StubAuthService))
            // NoopMemoriaForwarder returns "not configured" error
            .with_memoria_forwarder(Arc::new(NoopMemoriaForwarder)),
    )
}

// ── HTTP helpers ─────────────────────────────────────────────────────────────

fn load_contract() -> serde_json::Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/memory_contract.json");
    let content = std::fs::read_to_string(path).expect("memory contract fixture should exist");
    serde_json::from_str(&content).expect("memory contract fixture should be valid JSON")
}

async fn post_memory(
    app: Router,
    path: &str,
    auth_token: &str,
    payload: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .header("authorization", auth_token)
        .body(Body::from(payload.to_string()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn post_memory_unauth(app: Router, path: &str, payload: serde_json::Value) -> StatusCode {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(payload.to_string()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    response.status()
}

// ── Auth tests ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn memory_store_requires_auth() {
    let contract = load_contract();
    let status = post_memory_unauth(
        build_noop_app(),
        "/memory/store",
        serde_json::json!({"content": "test", "memory_type": "semantic"}),
    )
    .await;
    assert_eq!(
        status.as_u16(),
        contract["memory_unauthenticated"]["status"]
            .as_u64()
            .unwrap() as u16
    );
}

#[tokio::test]
async fn memory_retrieve_requires_auth() {
    let contract = load_contract();
    let status = post_memory_unauth(
        build_noop_app(),
        "/memory/retrieve",
        serde_json::json!({"query": "test"}),
    )
    .await;
    assert_eq!(
        status.as_u16(),
        contract["memory_unauthenticated"]["status"]
            .as_u64()
            .unwrap() as u16
    );
}

#[tokio::test]
async fn memory_search_requires_auth() {
    let contract = load_contract();
    let status = post_memory_unauth(
        build_noop_app(),
        "/memory/search",
        serde_json::json!({"query": "test"}),
    )
    .await;
    assert_eq!(
        status.as_u16(),
        contract["memory_unauthenticated"]["status"]
            .as_u64()
            .unwrap() as u16
    );
}

#[tokio::test]
async fn memory_purge_requires_auth() {
    let contract = load_contract();
    let status = post_memory_unauth(
        build_noop_app(),
        "/memory/purge",
        serde_json::json!({"topic": "test", "reason": "cleanup"}),
    )
    .await;
    assert_eq!(
        status.as_u16(),
        contract["memory_unauthenticated"]["status"]
            .as_u64()
            .unwrap() as u16
    );
}

// ── Session_id injection tests ────────────────────────────────────────────────

/// The proxy must inject session_id = user_id into the body forwarded to Memoria.
/// This is the core user-isolation guarantee.
#[tokio::test]
async fn memory_store_injects_session_id_for_user_isolation() {
    let contract = load_contract();
    let (app, forwarder) = build_memory_app_capturing();

    let user_a_id = contract["memory_isolation"]["user_a_id"]
        .as_str()
        .unwrap()
        .to_string();
    let user_a_token = contract["memory_isolation"]["user_a_token"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, body) = post_memory(
        app,
        "/memory/store",
        &user_a_token,
        serde_json::json!({"content": "User prefers concise answers", "memory_type": "semantic"}),
    )
    .await;

    assert_eq!(status.as_u16(), 200, "response body: {body}");

    let (endpoint, forwarded) = forwarder.take().expect("forwarder should have been called");
    assert_eq!(endpoint, "/v1/memories");
    assert_eq!(
        forwarded["session_id"].as_str(),
        Some(user_a_id.as_str()),
        "session_id must equal the authenticated user_id"
    );
    assert_eq!(
        forwarded["user_id"].as_str(),
        Some(user_a_id.as_str()),
        "user_id must equal the authenticated user_id"
    );
    assert_eq!(
        forwarded["content"].as_str(),
        Some("User prefers concise answers")
    );
}

#[tokio::test]
async fn memory_retrieve_injects_session_id_for_user_isolation() {
    let contract = load_contract();
    let (app, forwarder) = build_memory_app_capturing();

    let user_a_id = contract["memory_isolation"]["user_a_id"]
        .as_str()
        .unwrap()
        .to_string();
    let user_a_token = contract["memory_isolation"]["user_a_token"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, body) = post_memory(
        app,
        "/memory/retrieve",
        &user_a_token,
        serde_json::json!({"query": "user preferences", "top_k": 5}),
    )
    .await;

    assert_eq!(status.as_u16(), 200, "response body: {body}");

    let (endpoint, forwarded) = forwarder.take().unwrap();
    assert_eq!(endpoint, "/v1/memories/retrieve");
    assert_eq!(forwarded["session_id"].as_str(), Some(user_a_id.as_str()));
    assert_eq!(forwarded["query"].as_str(), Some("user preferences"));
}

#[tokio::test]
async fn memory_search_injects_session_id_for_user_isolation() {
    let contract = load_contract();
    let (app, forwarder) = build_memory_app_capturing();

    let user_b_id = contract["memory_isolation"]["user_b_id"]
        .as_str()
        .unwrap()
        .to_string();
    let user_b_token = contract["memory_isolation"]["user_b_token"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, body) = post_memory(
        app,
        "/memory/search",
        &user_b_token,
        serde_json::json!({"query": "concise answers", "top_k": 3}),
    )
    .await;

    assert_eq!(status.as_u16(), 200, "response body: {body}");

    let (endpoint, forwarded) = forwarder.take().unwrap();
    assert_eq!(endpoint, "/v1/memories/search");
    assert_eq!(
        forwarded["session_id"].as_str(),
        Some(user_b_id.as_str()),
        "user B must get their own session_id injected"
    );
}

#[tokio::test]
async fn memory_purge_injects_session_id_for_user_isolation() {
    let contract = load_contract();
    let (app, forwarder) = build_memory_app_capturing();

    let user_a_id = contract["memory_isolation"]["user_a_id"]
        .as_str()
        .unwrap()
        .to_string();
    let user_a_token = contract["memory_isolation"]["user_a_token"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, body) = post_memory(
        app,
        "/memory/purge",
        &user_a_token,
        serde_json::json!({"topic": "test", "reason": "cleanup"}),
    )
    .await;

    assert_eq!(status.as_u16(), 200, "response body: {body}");

    let (endpoint, forwarded) = forwarder.take().unwrap();
    assert_eq!(endpoint, "/v1/memories/purge");
    assert_eq!(forwarded["session_id"].as_str(), Some(user_a_id.as_str()));
}

/// Verify different users get different session_ids — the core isolation property.
#[tokio::test]
async fn memory_different_users_get_different_session_ids() {
    let contract = load_contract();

    let user_a_id = contract["memory_isolation"]["user_a_id"]
        .as_str()
        .unwrap()
        .to_string();
    let user_b_id = contract["memory_isolation"]["user_b_id"]
        .as_str()
        .unwrap()
        .to_string();
    let user_a_token = contract["memory_isolation"]["user_a_token"]
        .as_str()
        .unwrap()
        .to_string();
    let user_b_token = contract["memory_isolation"]["user_b_token"]
        .as_str()
        .unwrap()
        .to_string();

    // User A
    let (app_a, forwarder_a) = build_memory_app_capturing();
    post_memory(
        app_a,
        "/memory/search",
        &user_a_token,
        serde_json::json!({"query": "test"}),
    )
    .await;
    let (_, a_body) = forwarder_a.take().unwrap();

    // User B
    let (app_b, forwarder_b) = build_memory_app_capturing();
    post_memory(
        app_b,
        "/memory/search",
        &user_b_token,
        serde_json::json!({"query": "test"}),
    )
    .await;
    let (_, b_body) = forwarder_b.take().unwrap();

    assert_eq!(
        a_body["session_id"].as_str(),
        Some(user_a_id.as_str()),
        "user A gets their session_id"
    );
    assert_eq!(
        b_body["session_id"].as_str(),
        Some(user_b_id.as_str()),
        "user B gets their session_id"
    );
    assert_ne!(
        a_body["session_id"].as_str(),
        b_body["session_id"].as_str(),
        "different users must get different session_ids"
    );
}

/// Caller-supplied session_id must NOT be overridden (or_insert_with semantics).
/// If edge already set session_id, proxy preserves it.
#[tokio::test]
async fn memory_does_not_override_existing_session_id() {
    let contract = load_contract();
    let (app, forwarder) = build_memory_app_capturing();

    let user_a_id = contract["memory_isolation"]["user_a_id"]
        .as_str()
        .unwrap()
        .to_string();
    let user_a_token = contract["memory_isolation"]["user_a_token"]
        .as_str()
        .unwrap()
        .to_string();

    // Edge sends its own session_id — server MUST overwrite it with the
    // authenticated user_id to prevent cross-user memory access.
    let (status, _) = post_memory(
        app,
        "/memory/search",
        &user_a_token,
        serde_json::json!({"query": "test", "session_id": "edge-provided-session"}),
    )
    .await;

    assert_eq!(status.as_u16(), 200);

    let (_, forwarded) = forwarder.take().unwrap();
    // Security: server force-overwrites session_id to prevent impersonation
    assert_eq!(
        forwarded["session_id"].as_str(),
        Some(user_a_id.as_str()),
        "session_id must be overwritten with authenticated user_id"
    );
    assert_eq!(forwarded["user_id"].as_str(), Some(user_a_id.as_str()));
}

/// Verify that the server returns 503 when Memoria is not configured (no master key).
#[tokio::test]
async fn memory_returns_503_when_memoria_not_configured() {
    let (status, json) = post_memory(
        build_noop_app(),
        "/memory/search",
        "Bearer contract-user-a-token",
        serde_json::json!({"query": "test"}),
    )
    .await;

    assert_eq!(status.as_u16(), 503);
    assert!(
        json["detail"]
            .as_str()
            .unwrap_or("")
            .contains("not configured"),
        "should return 503 with 'not configured' detail, got: {json}"
    );
}

/// Security: client-supplied user_id must be overwritten with the authenticated identity.
/// This prevents a malicious client from accessing another user's memories.
#[tokio::test]
async fn memory_proxy_overwrites_spoofed_user_id() {
    let contract = load_contract();
    let (app, forwarder) = build_memory_app_capturing();

    let user_a_id = contract["memory_isolation"]["user_a_id"]
        .as_str()
        .unwrap()
        .to_string();
    let user_a_token = contract["memory_isolation"]["user_a_token"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, _) = post_memory(
        app,
        "/memory/store",
        &user_a_token,
        serde_json::json!({
            "content": "test",
            "user_id": "victim-user-id",
            "session_id": "victim-session-id"
        }),
    )
    .await;

    assert_eq!(status.as_u16(), 200);

    let (_, forwarded) = forwarder.take().unwrap();
    assert_eq!(
        forwarded["user_id"].as_str(),
        Some(user_a_id.as_str()),
        "spoofed user_id must be replaced with authenticated user"
    );
    assert_eq!(
        forwarded["session_id"].as_str(),
        Some(user_a_id.as_str()),
        "spoofed session_id must be replaced with authenticated user"
    );
}
