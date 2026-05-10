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
    let db_healthy = state.health_checker.database_healthy().await;

    Json(HealthResponse {
        status: if db_healthy { "healthy" } else { "unhealthy" }.to_string(),
        database: if db_healthy {
            "connected"
        } else {
            "disconnected"
        }
        .to_string(),
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
