use super::*;
use crate::bridge::side_effects::{PERSIST_FAIL_COUNT, PERSIST_OK_COUNT};
use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;
use std::sync::atomic::Ordering;

pub(super) async fn root_handler(State(state): State<AppState>) -> Json<RootResponse> {
    Json(RootResponse {
        name: state.service_info.name,
        version: state.service_info.version,
        docs: state.service_info.docs,
    })
}

pub(super) async fn health_handler(State(state): State<AppState>) -> Json<HealthResponse> {
    let database_health = state.health_checker.database_health().await;

    Json(HealthResponse {
        status: database_health.overall_status().to_string(),
        database: database_health.database_label().to_string(),
        persist_ok: PERSIST_OK_COUNT.load(Ordering::Relaxed),
        persist_fail: PERSIST_FAIL_COUNT.load(Ordering::Relaxed),
    })
}

/// `GET /metrics` — Prometheus text format 0.0.4.
///
/// Renders the shared `MetricsRegistry` owned by [`AppState`]. Scrapers must
/// see a stable content-type so content negotiation and schema diffing work.
pub(super) async fn metrics_handler(State(state): State<AppState>) -> impl IntoResponse {
    let body = state.metrics_registry().render_prometheus();
    (
        [(CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        body,
    )
}
