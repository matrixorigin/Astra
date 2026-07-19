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

fn register_event_ingestion_metrics(registry: &astra_turn_core::pipeline_metrics::MetricsRegistry) {
    registry.register_gauge(
        "astra_event_ingestion_config_batch_size",
        "Configured event ingestion batch size for this runtime process.",
    );
    registry.register_gauge(
        "astra_event_ingestion_config_flush_interval_secs",
        "Configured event ingestion flush interval in seconds for this runtime process.",
    );
    registry.register_gauge(
        "astra_event_ingestion_config_channel_capacity",
        "Configured event ingestion channel capacity for this runtime process.",
    );
    registry.register_gauge(
        "astra_event_ingestion_config_max_retries",
        "Configured event ingestion max retries for this runtime process.",
    );
    registry.register_counter(
        "astra_event_ingestion_events_received_total",
        "Events accepted by the event ingestion worker.",
    );
    registry.register_counter(
        "astra_event_ingestion_events_flushed_total",
        "Events handled by successful event ingestion flushes.",
    );
    registry.register_counter(
        "astra_event_ingestion_events_dropped_permanent_total",
        "Events permanently dropped by the ingestion worker after acceptance.",
    );
    registry.register_counter(
        "astra_event_ingestion_flushes_total",
        "Successful event ingestion flush attempts.",
    );
    registry.register_counter(
        "astra_event_ingestion_errors_total",
        "Event ingestion worker errors.",
    );
    registry.register_counter(
        "astra_event_ingestion_enqueue_overflows_total",
        "Producer enqueue attempts that found the ingestion channel full or closed.",
    );
    registry.register_counter(
        "astra_event_ingestion_events_dropped_before_acceptance_total",
        "Events dropped before the ingestion worker accepted them.",
    );
    registry.register_counter(
        "astra_event_ingestion_events_dropped_before_acceptance_by_priority_total",
        "Events dropped before worker acceptance, split by ingestion priority.",
    );
}

fn scrape_event_ingestion_metrics(state: &AppState) {
    let registry = state.metrics_registry();
    register_event_ingestion_metrics(&registry);
    let Some(runtime) = state.matrix_cloud_runtime.as_ref() else {
        return;
    };

    let config = runtime.ingestion_config();
    registry.set_gauge(
        "astra_event_ingestion_config_batch_size",
        &[],
        config.batch_size as f64,
    );
    registry.set_gauge(
        "astra_event_ingestion_config_flush_interval_secs",
        &[],
        config.flush_interval_secs as f64,
    );
    registry.set_gauge(
        "astra_event_ingestion_config_channel_capacity",
        &[],
        config.channel_capacity as f64,
    );
    registry.set_gauge(
        "astra_event_ingestion_config_max_retries",
        &[],
        config.max_retries as f64,
    );
    registry.set_counter_absolute(
        "astra_event_ingestion_enqueue_overflows_total",
        &[],
        runtime.ingestion_overflow_count(),
    );
    registry.set_counter_absolute(
        "astra_event_ingestion_events_dropped_before_acceptance_total",
        &[],
        runtime.ingestion_dropped_before_acceptance_count(),
    );
    registry.set_counter_absolute(
        "astra_event_ingestion_events_dropped_before_acceptance_by_priority_total",
        &[("priority", "critical")],
        runtime.ingestion_dropped_critical_before_acceptance_count(),
    );
    registry.set_counter_absolute(
        "astra_event_ingestion_events_dropped_before_acceptance_by_priority_total",
        &[("priority", "telemetry")],
        runtime.ingestion_dropped_telemetry_before_acceptance_count(),
    );

    let Some(stats) = runtime.ingestion_stats() else {
        return;
    };
    registry.set_counter_absolute(
        "astra_event_ingestion_events_received_total",
        &[],
        stats.events_received,
    );
    registry.set_counter_absolute(
        "astra_event_ingestion_events_flushed_total",
        &[],
        stats.events_flushed,
    );
    registry.set_counter_absolute(
        "astra_event_ingestion_events_dropped_permanent_total",
        &[],
        stats.events_dropped_permanent,
    );
    registry.set_counter_absolute(
        "astra_event_ingestion_flushes_total",
        &[],
        stats.flush_count,
    );
    registry.set_counter_absolute("astra_event_ingestion_errors_total", &[], stats.errors);
}

pub(super) async fn root_handler(State(state): State<AppState>) -> Json<RootResponse> {
    Json(RootResponse {
        name: state.service_info.name,
        version: state.service_info.version,
        docs: state.service_info.docs,
    })
}

pub(super) async fn current_health(state: &AppState) -> HealthResponse {
    let database_health = state.health_checker.database_health().await;
    let memoria_health = state.cached_memoria_health();

    let status = if !database_health.is_healthy() {
        "unhealthy"
    } else if memoria_health.is_degraded() {
        "degraded"
    } else {
        "healthy"
    };

    HealthResponse {
        status: status.to_string(),
        database: database_health.database_label().to_string(),
        memoria: memoria_health.label().to_string(),
        persist_ok: PERSIST_OK_COUNT.load(Ordering::Relaxed),
        persist_fail: PERSIST_FAIL_COUNT.load(Ordering::Relaxed),
    }
}

pub(super) async fn health_handler(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(current_health(&state).await)
}

#[derive(Serialize)]
pub(super) struct LivenessResponse {
    status: &'static str,
}

#[derive(Serialize)]
pub(super) struct ReadinessResponse {
    status: &'static str,
    database: &'static str,
}

/// Process liveness never performs dependency I/O. Orchestrators should only
/// restart the process when this endpoint itself is unreachable.
pub(super) async fn live_handler() -> Json<LivenessResponse> {
    Json(LivenessResponse { status: "alive" })
}

/// Traffic readiness is owned by core dependencies only. Optional capability
/// failures remain visible on `/health` without evicting every replica.
pub(super) async fn ready_handler(State(state): State<AppState>) -> impl IntoResponse {
    let database = state.health_checker.database_health().await;
    let status = if database.is_healthy() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(ReadinessResponse {
            status: if database.is_healthy() {
                "ready"
            } else {
                "not_ready"
            },
            database: database.database_label(),
        }),
    )
}

/// `GET /metrics` — Prometheus text format 0.0.4.
///
/// Renders the shared `MetricsRegistry` owned by [`AppState`]. DB-backed
/// multi-agent gauges are refreshed by a background task, so this handler does
/// not issue database queries on the scrape path.
pub(super) async fn metrics_handler(State(state): State<AppState>) -> impl IntoResponse {
    let bridge = MetricsRegistryBridge(state.metrics_registry().clone());
    state.multi_agent_metrics.register_with(&bridge);
    state.multi_agent_metrics.scrape_to(&bridge);
    crate::server::interaction_metrics::register_interaction_metrics(&state.metrics_registry());
    crate::server::ws_handler::register_ws_run_stream_poll_metrics(&state.metrics_registry());
    crate::capacity_model::scrape_capacity_metrics_from_env(&state.metrics_registry());
    scrape_event_ingestion_metrics(&state);
    crate::turn::bridge::llm_stream::rate_limit_cooldown()
        .scrape_metrics(&state.metrics_registry());
    let body = state.metrics_registry().render_prometheus();
    (
        [(CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AppState, HealthChecker, MemoriaForwarder, MemoriaHealth, ServiceInfo};
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone)]
    struct AlwaysHealthy;

    #[async_trait]
    impl HealthChecker for AlwaysHealthy {
        async fn database_healthy(&self) -> bool {
            true
        }
    }

    #[derive(Clone)]
    struct DatabaseUnavailable;

    #[async_trait]
    impl HealthChecker for DatabaseUnavailable {
        async fn database_healthy(&self) -> bool {
            false
        }
    }

    struct UnavailableMemoria;

    #[async_trait]
    impl MemoriaForwarder for UnavailableMemoria {
        async fn forward(
            &self,
            _method: reqwest::Method,
            _endpoint: &str,
            _body: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            Err("memoria unavailable".to_string())
        }

        async fn health(&self) -> MemoriaHealth {
            MemoriaHealth::Unavailable("shared database unavailable".to_string())
        }
    }

    struct CountingMemoria {
        probes: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl MemoriaForwarder for CountingMemoria {
        async fn forward(
            &self,
            _method: reqwest::Method,
            _endpoint: &str,
            _body: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            Err("unused".to_string())
        }

        async fn health(&self) -> MemoriaHealth {
            self.probes.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            MemoriaHealth::Unavailable("optional dependency down".to_string())
        }
    }

    #[tokio::test]
    async fn configured_dependency_failure_is_reported_as_degraded() {
        let state = AppState::new(ServiceInfo::default(), Arc::new(AlwaysHealthy))
            .with_memoria_forwarder(Arc::new(UnavailableMemoria));

        let health = current_health(&state).await;

        assert_eq!(health.status, "degraded");
        assert_eq!(health.database, "connected");
        assert_eq!(health.memoria, "unavailable");
    }

    #[tokio::test]
    async fn primary_database_failure_remains_unhealthy_when_dependency_also_fails() {
        let state = AppState::new(ServiceInfo::default(), Arc::new(DatabaseUnavailable))
            .with_memoria_forwarder(Arc::new(UnavailableMemoria));

        let health = current_health(&state).await;

        assert_eq!(health.status, "unhealthy");
        assert_eq!(health.database, "unavailable");
        assert_eq!(health.memoria, "unavailable");
    }

    #[tokio::test]
    async fn capability_probe_is_cached_singleflight_and_does_not_block_health_requests() {
        let probes = Arc::new(AtomicUsize::new(0));
        let state = AppState::new(ServiceInfo::default(), Arc::new(AlwaysHealthy))
            .with_memoria_forwarder(Arc::new(CountingMemoria {
                probes: probes.clone(),
            }));
        let callers = (0..8)
            .map(|_| {
                let state = state.clone();
                tokio::spawn(async move {
                    state
                        .refresh_memoria_health_if_stale(std::time::Duration::from_secs(60))
                        .await
                })
            })
            .collect::<Vec<_>>();
        for caller in callers {
            assert!(matches!(
                caller.await.unwrap(),
                MemoriaHealth::Unavailable(_)
            ));
        }
        assert_eq!(probes.load(Ordering::SeqCst), 1);

        let health = current_health(&state).await;
        assert_eq!(health.status, "degraded");
        assert_eq!(
            probes.load(Ordering::SeqCst),
            1,
            "request path must use cache"
        );

        let ready = ready_handler(State(state)).await.into_response();
        assert_eq!(ready.status(), StatusCode::OK);
        assert_eq!(probes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn readiness_rejects_only_core_database_failure() {
        let state = AppState::new(ServiceInfo::default(), Arc::new(DatabaseUnavailable))
            .with_memoria_forwarder(Arc::new(UnavailableMemoria));
        let response = ready_handler(State(state)).await.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn metrics_handler_scrapes_capacity_metrics() {
        let state = AppState::new(ServiceInfo::default(), Arc::new(AlwaysHealthy));
        let cooldown = crate::turn::bridge::llm_stream::rate_limit_cooldown();
        cooldown.reset_for_tests();
        cooldown.with("metrics-model", |rl| {
            rl.record_429(None, false);
        });

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
        assert!(
            text.contains(
                "astra_llm_provider_rate_limit_errors_total{model=\"metrics-model\",status=\"429\"} 1"
            ),
            "{text}"
        );
        assert!(
            text.contains("# TYPE astra_edge_dispatch_pending_rows gauge"),
            "{text}"
        );
        assert!(
            text.contains("# TYPE astra_edge_dispatch_deliver_misses_total counter"),
            "{text}"
        );
        assert!(
            text.contains("# TYPE astra_interaction_ask_user_wait_total counter"),
            "{text}"
        );
        assert!(
            text.contains("# TYPE astra_interaction_approval_lookup_total counter"),
            "{text}"
        );
        assert!(
            text.contains("# TYPE astra_interaction_approval_resolution_total counter"),
            "{text}"
        );
        assert!(
            text.contains("# TYPE astra_ws_run_stream_poll_attempts_total counter"),
            "{text}"
        );
        assert!(
            text.contains("# TYPE astra_ws_run_stream_poll_errors_total counter"),
            "{text}"
        );
        assert!(
            text.contains("# TYPE astra_event_ingestion_events_received_total counter"),
            "{text}"
        );
        assert!(
            text.contains(
                "# TYPE astra_event_ingestion_events_dropped_before_acceptance_total counter"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "# TYPE astra_event_ingestion_events_dropped_before_acceptance_by_priority_total counter"
            ),
            "{text}"
        );
    }
}
