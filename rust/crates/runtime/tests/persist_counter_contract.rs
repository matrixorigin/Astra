/// Contract tests for fire-and-forget persistence failure counters.
///
/// Verifies that PERSIST_FAIL_COUNT and PERSIST_OK_COUNT are observable
/// and that the health endpoint exposes them.
use axum::{Router, http::StatusCode};
use mo_agent_runtime::{
    AppState, HealthChecker, PERSIST_FAIL_COUNT, PERSIST_OK_COUNT, ServiceInfo, build_app,
};
use std::sync::{Arc, atomic::Ordering};
use tower::util::ServiceExt;

// ── helpers ───────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct StubHealthChecker;

#[async_trait::async_trait]
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

async fn get_health(app: Router) -> serde_json::Value {
    let req = axum::http::Request::builder()
        .uri("/health")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn health_includes_persist_counters() {
    let json = get_health(build_test_app()).await;
    assert!(
        json.get("persist_ok").is_some(),
        "health should include persist_ok"
    );
    assert!(
        json.get("persist_fail").is_some(),
        "health should include persist_fail"
    );
}

#[tokio::test]
async fn persist_counters_are_u64() {
    let json = get_health(build_test_app()).await;
    assert!(
        json["persist_ok"].is_u64(),
        "persist_ok should be u64, got: {}",
        json["persist_ok"]
    );
    assert!(
        json["persist_fail"].is_u64(),
        "persist_fail should be u64, got: {}",
        json["persist_fail"]
    );
}

#[test]
fn persist_fail_counter_increments() {
    // Save current value, increment, verify, restore.
    let before = PERSIST_FAIL_COUNT.load(Ordering::Relaxed);
    PERSIST_FAIL_COUNT.fetch_add(1, Ordering::Relaxed);
    let after = PERSIST_FAIL_COUNT.load(Ordering::Relaxed);
    assert_eq!(after, before + 1);
    // Restore
    PERSIST_FAIL_COUNT.store(before, Ordering::Relaxed);
}

#[test]
fn persist_ok_counter_increments() {
    let before = PERSIST_OK_COUNT.load(Ordering::Relaxed);
    PERSIST_OK_COUNT.fetch_add(1, Ordering::Relaxed);
    let after = PERSIST_OK_COUNT.load(Ordering::Relaxed);
    assert_eq!(after, before + 1);
    PERSIST_OK_COUNT.store(before, Ordering::Relaxed);
}

#[tokio::test]
async fn health_reflects_counter_state() {
    // Set known values, verify health endpoint reflects them.
    PERSIST_OK_COUNT.store(42, Ordering::Relaxed);
    PERSIST_FAIL_COUNT.store(7, Ordering::Relaxed);

    let json = get_health(build_test_app()).await;
    assert_eq!(json["persist_ok"].as_u64(), Some(42));
    assert_eq!(json["persist_fail"].as_u64(), Some(7));

    // Restore
    PERSIST_OK_COUNT.store(0, Ordering::Relaxed);
    PERSIST_FAIL_COUNT.store(0, Ordering::Relaxed);
}
