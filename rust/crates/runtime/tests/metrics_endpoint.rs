//! TDD: `/metrics` endpoint exposes Prometheus text format backed by
//! `AppState::metrics_registry` (an `Arc<MetricsRegistry>` from astra-turn-core).

use astra_runtime::{AppState, ServiceInfo};
use astra_turn_core::pipeline_metrics::MetricsRegistry;
use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use std::sync::Arc;
use tower::ServiceExt; // oneshot

struct AlwaysHealthy;

#[async_trait]
impl astra_runtime::HealthChecker for AlwaysHealthy {
    async fn database_healthy(&self) -> bool {
        true
    }
}

fn test_state() -> AppState {
    let info = ServiceInfo::new("test", "0.0.0", "");
    AppState::new(info, Arc::new(AlwaysHealthy))
}

#[tokio::test]
async fn app_state_exposes_metrics_registry() {
    let state = test_state();
    // Must be reachable via a public accessor.
    let reg: Arc<MetricsRegistry> = state.metrics_registry();
    reg.register_counter("unit_test_counter_total", "unit test counter");
    reg.increment_counter("unit_test_counter_total", &[], 1);
    let out = reg.render_prometheus();
    assert!(out.contains("unit_test_counter_total 1"));
}

#[tokio::test]
async fn get_metrics_returns_prometheus_text() {
    let state = test_state();
    let reg = state.metrics_registry();
    reg.register_counter("astra_test_requests_total", "test counter");
    reg.increment_counter("astra_test_requests_total", &[("route", "/ping")], 3);

    let router = astra_runtime::build_test_router(state);
    let response = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router oneshot");

    assert_eq!(response.status(), StatusCode::OK);
    let ctype = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        ctype.starts_with("text/plain"),
        "content-type must be text/plain, got {ctype}"
    );
    assert!(
        ctype.contains("version=0.0.4"),
        "content-type must declare prometheus version 0.0.4, got {ctype}"
    );

    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        text.contains("# HELP astra_test_requests_total"),
        "missing HELP line in:\n{text}"
    );
    assert!(
        text.contains("# TYPE astra_test_requests_total counter"),
        "missing TYPE line in:\n{text}"
    );
    assert!(
        text.contains(r#"astra_test_requests_total{route="/ping"} 3"#),
        "missing counter value in:\n{text}"
    );
}
