/// Verify that all expected routes are registered in the router.
/// This catches route registration regressions — if a route is accidentally removed,
/// the test will fail with 404 instead of the expected status.
use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    Router, body,
    http::{Request, StatusCode},
};
use mo_agent_runtime::{AppState, HealthChecker, ServiceInfo, build_app};
use tower::util::ServiceExt;

#[derive(Clone)]
struct StubHealthChecker;

#[async_trait]
impl HealthChecker for StubHealthChecker {
    async fn database_healthy(&self) -> bool {
        true
    }
}

fn build_test_app() -> Router {
    build_app(AppState::new(
        ServiceInfo::default(),
        Arc::new(StubHealthChecker),
    ))
}

/// Send a request and return the status code. We don't care about the body —
/// we just want to verify the route is registered (not 404/405).
async fn route_status(app: Router, method: &str, path: &str) -> StatusCode {
    let mut builder = Request::builder().method(method).uri(path);
    let body = if method == "POST" {
        builder = builder.header("content-type", "application/json");
        body::Body::from("{}")
    } else {
        body::Body::empty()
    };
    app.oneshot(builder.body(body).unwrap())
        .await
        .unwrap()
        .status()
}

/// A registered route returns 401 (auth required) or 200, never 404.
fn assert_route_registered(status: StatusCode, method: &str, path: &str) {
    assert_ne!(
        status,
        StatusCode::NOT_FOUND,
        "Route {method} {path} returned 404 — not registered"
    );
    assert_ne!(
        status,
        StatusCode::METHOD_NOT_ALLOWED,
        "Route {method} {path} returned 405 — wrong method"
    );
}

// ── Core routes ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn route_root_registered() {
    let status = route_status(build_test_app(), "GET", "/").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn route_health_registered() {
    let status = route_status(build_test_app(), "GET", "/health").await;
    assert_eq!(status, StatusCode::OK);
}

// ── Auth routes ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn route_auth_register() {
    let s = route_status(build_test_app(), "POST", "/auth/register").await;
    assert_route_registered(s, "POST", "/auth/register");
}

#[tokio::test]
async fn route_auth_login() {
    let s = route_status(build_test_app(), "POST", "/auth/login").await;
    assert_route_registered(s, "POST", "/auth/login");
}

#[tokio::test]
async fn route_auth_me() {
    let s = route_status(build_test_app(), "GET", "/auth/me").await;
    assert_route_registered(s, "GET", "/auth/me");
}

// ── Chat routes ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn route_chat() {
    let s = route_status(build_test_app(), "POST", "/chat").await;
    assert_route_registered(s, "POST", "/chat");
}

#[tokio::test]
async fn route_chat_stream() {
    let s = route_status(build_test_app(), "POST", "/chat/stream").await;
    assert_route_registered(s, "POST", "/chat/stream");
}

#[tokio::test]
async fn route_chat_turn() {
    let s = route_status(build_test_app(), "POST", "/chat/turn").await;
    assert_route_registered(s, "POST", "/chat/turn");
}

#[tokio::test]
async fn route_chat_route() {
    let s = route_status(build_test_app(), "POST", "/chat/route").await;
    assert_route_registered(s, "POST", "/chat/route");
}

// ── Reflect / decision-trace (newly added) ───────────────────────────────────

#[tokio::test]
async fn route_reflect_registered() {
    let s = route_status(build_test_app(), "GET", "/chat/session/test-sess/reflect").await;
    assert_route_registered(s, "GET", "/chat/session/{session_id}/reflect");
}

#[tokio::test]
async fn route_decision_trace_registered() {
    let s = route_status(
        build_test_app(),
        "GET",
        "/chat/session/test-sess/decision-trace",
    )
    .await;
    assert_route_registered(s, "GET", "/chat/session/{session_id}/decision-trace");
}

// ── Learning feedback (newly added) ──────────────────────────────────────────

#[tokio::test]
async fn route_learning_feedback_registered() {
    let s = route_status(build_test_app(), "POST", "/api/v1/learning/feedback").await;
    assert_route_registered(s, "POST", "/api/v1/learning/feedback");
}

// ── Other learning routes ────────────────────────────────────────────────────

#[tokio::test]
async fn route_learning_health() {
    let s = route_status(build_test_app(), "GET", "/api/v1/learning/health").await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn route_learning_signals() {
    let s = route_status(build_test_app(), "GET", "/api/v1/learning/signals").await;
    assert_route_registered(s, "GET", "/api/v1/learning/signals");
}

// ── Memory proxy routes ──────────────────────────────────────────────────────

#[tokio::test]
async fn route_memory_store() {
    let s = route_status(build_test_app(), "POST", "/memory/store").await;
    assert_route_registered(s, "POST", "/memory/store");
}

#[tokio::test]
async fn route_memory_retrieve() {
    let s = route_status(build_test_app(), "POST", "/memory/retrieve").await;
    assert_route_registered(s, "POST", "/memory/retrieve");
}

// ── Admin routes ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn route_admin_init() {
    let s = route_status(build_test_app(), "POST", "/admin/init").await;
    assert_route_registered(s, "POST", "/admin/init");
}

#[tokio::test]
async fn route_admin_audit() {
    let s = route_status(build_test_app(), "GET", "/admin/audit").await;
    assert_route_registered(s, "GET", "/admin/audit");
}

// ── Session routes ───────────────────────────────────────────────────────────

#[tokio::test]
async fn route_sessions_crud() {
    let s = route_status(build_test_app(), "POST", "/sessions").await;
    assert_route_registered(s, "POST", "/sessions");
    let s = route_status(build_test_app(), "GET", "/sessions").await;
    assert_route_registered(s, "GET", "/sessions");
}

// ── Evaluation routes ────────────────────────────────────────────────────────

#[tokio::test]
async fn route_evaluation_quality_trend() {
    let s = route_status(build_test_app(), "GET", "/evaluation/quality/trend").await;
    assert_route_registered(s, "GET", "/evaluation/quality/trend");
}

// ── Introspection routes ─────────────────────────────────────────────────────

#[tokio::test]
async fn route_introspection_memory() {
    let s = route_status(build_test_app(), "GET", "/introspection/memory").await;
    assert_route_registered(s, "GET", "/introspection/memory");
}

#[tokio::test]
async fn route_introspection_skills() {
    let s = route_status(build_test_app(), "GET", "/introspection/skills").await;
    assert_route_registered(s, "GET", "/introspection/skills");
}

// ── Marketplace routes ───────────────────────────────────────────────────────

#[tokio::test]
async fn route_marketplace_install() {
    let s = route_status(build_test_app(), "POST", "/marketplace/install").await;
    assert_route_registered(s, "POST", "/marketplace/install");
}

// ── Skills routes ────────────────────────────────────────────────────────────

#[tokio::test]
async fn route_skills_crud() {
    let s = route_status(build_test_app(), "POST", "/skills").await;
    assert_route_registered(s, "POST", "/skills");
    let s = route_status(build_test_app(), "GET", "/skills").await;
    assert_route_registered(s, "GET", "/skills");
}

// ── Data versioning routes ───────────────────────────────────────────────────

#[tokio::test]
async fn route_data_versioning_checkpoints() {
    let s = route_status(build_test_app(), "POST", "/data-versioning/checkpoints").await;
    assert_route_registered(s, "POST", "/data-versioning/checkpoints");
}

// ── Replay routes ────────────────────────────────────────────────────────────

#[tokio::test]
async fn route_replay() {
    let s = route_status(build_test_app(), "POST", "/sessions/test-sess/replay").await;
    assert_route_registered(s, "POST", "/sessions/{session_id}/replay");
}
