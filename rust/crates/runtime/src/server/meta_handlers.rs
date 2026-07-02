use super::*;
use crate::bridge::side_effects::{PERSIST_FAIL_COUNT, PERSIST_OK_COUNT};
use astra_services::multi_agent::MetricTarget;
use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;
use std::sync::Arc;
use std::sync::atomic::Ordering;

// ─── MetricTarget bridge ────────────────────────────────────────────────────
// Connect the services layer's MetricTarget trait to the runtime's
// MetricsRegistry via a newtype wrapper (orphan rule prevents direct impl).

struct MetricsRegistryBridge(Arc<astra_turn_core::pipeline_metrics::MetricsRegistry>);

impl MetricTarget for MetricsRegistryBridge {
    fn register_counter(&self, name: &str, help: &str) {
        self.0.register_counter(name, help);
    }

    fn register_gauge(&self, name: &str, help: &str) {
        self.0.register_gauge(name, help);
    }

    fn set_gauge(&self, name: &str, value: f64) {
        astra_turn_core::pipeline_metrics::MetricsRegistry::set_gauge(&self.0, name, &[], value);
    }

    fn set_counter(&self, name: &str, value: u64) {
        astra_turn_core::pipeline_metrics::MetricsRegistry::set_counter_absolute(
            &self.0,
            name,
            &[],
            value,
        );
    }
}

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
/// Renders the shared `MetricsRegistry` owned by [`AppState`]. Before rendering,
/// scrapes the latest multi-agent metrics snapshot into the registry so all
/// metrics are exposed through a single endpoint.
pub(super) async fn metrics_handler(State(state): State<AppState>) -> impl IntoResponse {
    let bridge = MetricsRegistryBridge(state.metrics_registry().clone());
    state.multi_agent_metrics.register_with(&bridge);
    state.multi_agent_metrics.scrape_to(&bridge);
    crate::capacity_model::scrape_capacity_metrics_from_env(&state.metrics_registry());
    let body = state.metrics_registry().render_prometheus();
    (
        [(CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AppState, HealthChecker, ServiceInfo};
    use async_trait::async_trait;
    use std::sync::Arc;

    #[derive(Clone)]
    struct AlwaysHealthy;

    #[async_trait]
    impl HealthChecker for AlwaysHealthy {
        async fn database_healthy(&self) -> bool {
            true
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn metrics_handler_scrapes_capacity_metrics() {
        let state = AppState::new(ServiceInfo::default(), Arc::new(AlwaysHealthy));

        let response = metrics_handler(State(state)).await.into_response();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("metrics body");
        let text = String::from_utf8(body.to_vec()).expect("metrics utf8");

        assert!(text.contains("# TYPE astra_capacity_run_slots_total gauge"));
        assert!(text.contains("# TYPE astra_capacity_rollout_allowed gauge"));
        assert!(
            text.contains(
                "astra_capacity_limit_mode{env_var=\"ASTRA_ENDPOINT_RPC_CONCURRENCY\",limit=\"registered_endpoint_rpc\",mode=\"reject\",scope=\"per_endpoint_per_pod\"} 1"
            ),
            "{text}"
        );
    }
}
