use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri},
    response::Response,
    routing::{delete, get, post},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE};
use chrono::Utc;
use tower_http::cors::{AllowHeaders, AllowOrigin, CorsLayer};
use uuid::Uuid;

use super::*;

mod admin_handlers;
mod agent_binding_handlers;
mod agent_binding_skill_runtime;
pub mod artifact_retention_sweeper;
mod audit_handlers;
mod auth_handlers;
mod bridge_prep;
mod chat_handlers;
mod cleanup_retry;
pub mod conflict_resolver;
pub mod delegation;
pub mod device_lease_sweeper;
mod edge;
pub mod harness;
pub(crate) mod header_utils;
mod http_helpers;
mod interaction_metrics;
mod llm_trusted_domains_handlers;
mod mcp_handlers;
mod meta_handlers;
mod model_gateway_handlers;
pub(crate) mod model_gateway_runtime;
mod plan_handlers;
mod platform_handlers;
mod preferences_handlers;
mod product_harness_handlers;
mod provider_runtime_context;
mod reflect_handlers;
mod request_trace;
mod resource_handlers;
mod router_builder;
pub mod run;
pub(crate) mod runtime_mcp;
pub(crate) mod server_bash_execution;
pub mod server_loop_host;
pub mod server_skill_subrun;
pub mod server_tool_executor;
pub(crate) mod session;
pub(crate) mod session_turn;
mod skillify_agent_executor;
mod state_builder;
pub mod sweeper_lease;
mod task_handlers;
pub mod team;
pub(crate) mod tool_agent_info;
pub(crate) mod tool_agent_runtime;
pub(crate) mod tool_approval_preflight;
pub(crate) mod tool_ask_user;
pub(crate) mod tool_binding_projection;
pub(crate) mod tool_database_snapshots;
pub(crate) mod tool_edge_selection;
pub(crate) mod tool_edge_transport;
pub(crate) mod tool_exactly_once;
pub(crate) mod tool_execution_binding;
pub(crate) mod tool_execution_result;
pub(crate) mod tool_execution_service;
pub(crate) mod tool_external_transport;
pub(crate) mod tool_file_runtime;
pub(crate) mod tool_introspect;
pub(crate) mod tool_local_execution;
pub(crate) mod tool_local_transport;

fn external_request_descriptor(
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    route: &'static str,
) -> astra_services::ProviderRequestDescriptor {
    astra_services::ProviderRequestDescriptor {
        method: method.as_str().to_string(),
        path: uri.path().to_string(),
        route: Some(route.to_string()),
        request_id: headers
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned),
        body_digest: None,
    }
}
pub(crate) mod tool_plan_gate;
pub(crate) mod tool_route_boundary;
pub(crate) mod tool_route_runtime;
pub(crate) mod tool_route_selection;
pub(crate) mod tool_session_config;
pub(crate) mod tool_session_history;
pub(crate) mod tool_session_runtime;
pub(crate) mod tool_session_state_rollback;
pub(crate) mod tool_task_runtime;
pub mod tool_transport;
pub(crate) mod tool_transport_errors;
pub(crate) mod tool_transport_metadata;
pub(crate) mod tool_transport_plan;
pub(crate) mod tool_work_surface_events;
pub(crate) mod tool_workspace_path_guard;
mod user_skill_handlers;
mod ws_handler;

use self::{bridge_prep::prepare_chat_turn_bridge_body, http_helpers::*};
use astra_server_types::*;
use astra_server_types::{ChatRouteResponse, classify_chat_route};
mod completions;

pub use request_trace::RequestTrace;
pub use state_builder::build_server_state;

/// Test-only helper: return the raw `Router` without the CORS/body-limit
/// layers, so integration tests can `.oneshot` it without dealing with
/// middleware semantics (e.g. preflight, trace IDs). Production code paths
/// must always go through [`build_app`].
pub fn build_test_router(state: AppState) -> Router {
    router_builder::build_router(state)
}

pub fn build_app(state: AppState) -> Router {
    let allow_origin = match state.cors_origins.as_deref() {
        Some(origins) if !origins.is_empty() && origins != "*" => {
            let parsed: Vec<HeaderValue> = origins
                .split(',')
                .filter_map(|o| o.trim().parse().ok())
                .collect();
            AllowOrigin::list(parsed)
        }
        _ => AllowOrigin::any(),
    };

    let cors = CorsLayer::new()
        .allow_origin(allow_origin)
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers(AllowHeaders::any())
        .expose_headers([HeaderName::from_static("x-request-id")]);

    router_builder::build_router(state)
        .layer(axum::extract::DefaultBodyLimit::max(4 * 1024 * 1024)) // 4 MB
        .layer(axum::middleware::from_fn(
            request_trace::request_trace_middleware,
        ))
        .layer(cors)
}

async fn finish_server_shutdown(
    bg_cancel: tokio_util::sync::CancellationToken,
    bg_handles: Vec<tokio::task::JoinHandle<()>>,
    run_lifecycle: Arc<dyn astra_services::RunLifecycleService>,
    matrix_runtime: Option<Arc<crate::matrix_cloud_runtime::MatrixCloudRuntime>>,
    edge_pool: astra_server_types::edge_connection_pool::EdgeConnectionPool,
) {
    // 0. Drain edge WebSocket connections — send Closing to each edge and wait
    //    for clean disconnect before tearing down background services.
    let edge_count = edge_pool.drain().await;
    if edge_count > 0 {
        tracing::info!(
            target: "astra_runtime::serve",
            edge_count,
            "drained edge WebSocket connections"
        );
    }
    // 1. Stop background sweepers and wait for them to exit.
    bg_cancel.cancel();
    for h in bg_handles {
        let _ = h.await;
    }
    // 2. Drain in-flight agentic loop tasks (up to 30s).
    if !run_lifecycle
        .drain_background_tasks(std::time::Duration::from_secs(30))
        .await
    {
        tracing::warn!(
            target: "astra_runtime::serve",
            "graceful shutdown: some background tasks did not finish within 30s"
        );
    }
    // 3. Drain Matrix ingestion + tracked session sync tasks.
    if let Some(rt) = matrix_runtime {
        rt.shutdown_ingestion_and_wait().await;
    }
    // 4. Flush OTLP exporter last so the prior shutdown work is observable.
    astra_logging::shutdown_otel();
}

pub async fn serve(addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
    static TRACING: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    TRACING.get_or_init(|| {
        let _ = astra_logging::init_from_env(
            astra_logging::LogInitConfig::new("warn,astra_runtime=info,astra.http.access=info")
                .with_service_name("astra-server"),
        );
    });

    // Apply safety.trust_mode from runtime config before any request can
    // reach the safety guards. Defaults to Strict; only flipped if the
    // operator explicitly sets `[safety] trust_mode = "trusted"` in
    // runtime.toml.
    crate::apply_safety_config_from_runtime_config(
        &astra_config::runtime_config::RuntimeConfig::load(),
    );

    let settings = AppSettings::from_env()?;
    let state = state_builder::build_server_state(settings).await?;

    // Warn about proxy settings that can cause confusing 502s for local clients
    if let Ok(proxy) = std::env::var("http_proxy").or_else(|_| std::env::var("HTTP_PROXY"))
        && !proxy.is_empty()
    {
        tracing::warn!(
            target: "astra_runtime::serve",
            http_proxy = %proxy,
            "HTTP proxy set; local callers should set NO_PROXY=127.0.0.1,localhost or use --noproxy"
        );
    }

    log_memoria_startup_health(&state).await;

    let listener = tokio::net::TcpListener::bind(addr).await?;

    // Cancellation token wired into background sweepers; cancelled after axum
    // serve returns so we can drain them deterministically before tearing down
    // the runtime / pool / OTLP exporter.
    let bg_cancel = tokio_util::sync::CancellationToken::new();
    let mut bg_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    if let Some(ref pool) = state.shared_pool {
        bg_handles.push(spawn_data_cleanup(pool.clone(), bg_cancel.clone()));
        bg_handles.push(spawn_edge_dispatch_cleanup(
            state.execution.edge_dispatch_service.clone(),
            bg_cancel.clone(),
        ));
        bg_handles.push(spawn_edge_dispatch_backlog_metrics_refresh(
            pool.clone(),
            state.multi_agent_metrics.clone(),
            bg_cancel.clone(),
        ));
        bg_handles.push(astra_services::session_reaper::spawn_session_reaper(
            pool.clone(),
            bg_cancel.clone(),
        ));
        // Spawn background cleanup-debt retry task
        {
            let cleanup_store: std::sync::Arc<dyn astra_services::WorkspaceCleanupDebtStore> =
                std::sync::Arc::new(astra_services::DatabaseWorkspaceRecordStore::new(
                    pool.clone(),
                ));
            bg_handles.push(crate::server::cleanup_retry::spawn_cleanup_retry(
                cleanup_store,
                bg_cancel.clone(),
            ));
        }
        bg_handles.extend(crate::server::sweeper_lease::spawn_runtime_sweepers(
            pool.clone(),
            bg_cancel.clone(),
        ));
    }

    // Clone the matrix runtime handle before moving `state` into `build_app`
    // so we can drain ingestion + sync sidecars after axum returns.
    let matrix_runtime = state.matrix_cloud_runtime.clone();
    let run_lifecycle = state.execution.run_lifecycle_service.clone();
    let edge_pool = state.edge_connection_pool.clone();

    axum::serve(listener, build_app(state))
        .with_graceful_shutdown(http_shutdown_signal())
        .await?;

    finish_server_shutdown(
        bg_cancel,
        bg_handles,
        run_lifecycle,
        matrix_runtime,
        edge_pool,
    )
    .await;
    Ok(())
}

async fn log_memoria_startup_health(state: &AppState) {
    let Some(master_key) = state
        .memoria_master_key
        .as_ref()
        .filter(|key| !key.is_empty())
        .cloned()
    else {
        return;
    };

    let client = crate::turn::cloud::memoria_compact::HttpMemoriaClient::new(
        state.memoria_base_url.clone(),
        master_key,
    );
    match client.health_check().await {
        Ok(()) => tracing::info!(
            target: "astra_runtime::serve",
            memoria_base_url = %state.memoria_base_url,
            "Memoria startup health check passed"
        ),
        Err(error) => tracing::warn!(
            target: "astra_runtime::serve",
            memoria_base_url = %state.memoria_base_url,
            error = %error,
            "Memoria startup health check failed; memory features may be degraded"
        ),
    }
}

/// Completes on SIGTERM (Unix) or Ctrl+C so `axum::serve` can exit cleanly and OTLP can flush.
async fn http_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    target: "astra_runtime::serve",
                    error = %e,
                    "failed to register SIGTERM; graceful stop uses Ctrl+C only"
                );
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = sigterm.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Spawn a background task that periodically cleans up expired data.
fn spawn_data_cleanup(
    pool: astra_core::SharedPool,
    cancel: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    use astra_services::RetentionPolicy;
    use std::time::Duration;

    let cleanup_interval = Duration::from_secs(6 * 3600); // 6 hours

    tokio::spawn(async move {
        let policy = RetentionPolicy::default();
        let mut interval = tokio::time::interval(cleanup_interval);
        interval.tick().await; // skip immediate first tick
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!(
                        target: "astra_runtime::cleanup",
                        "data cleanup received cancellation; exiting"
                    );
                    break;
                }
                _ = interval.tick() => {}
            }
            let results = match astra_services::cleanup_expired_data(pool.get(), &policy).await {
                Ok(results) => results,
                Err(error) => {
                    tracing::warn!(
                        target: "astra_runtime::cleanup",
                        error = %error,
                        "expired data cleanup failed"
                    );
                    continue;
                }
            };
            let total: u64 = results.iter().map(|r| r.rows_deleted).sum();
            if total > 0 {
                let detail = results
                    .iter()
                    .filter(|r| r.rows_deleted > 0)
                    .map(|r| format!("{}={}", r.table, r.rows_deleted))
                    .collect::<Vec<_>>()
                    .join(", ");
                tracing::info!(
                    target: "astra_runtime::cleanup",
                    rows_purged = total,
                    tables = %detail,
                    "expired data cleanup"
                );
            }
        }
    })
}

const EDGE_DISPATCH_CLEANUP_INTERVAL_SECS: u64 = 15 * 60;
const EDGE_DISPATCH_STALE_AFTER_SECS: u64 = 60 * 60;
const EDGE_DISPATCH_BACKLOG_METRICS_REFRESH_INTERVAL_SECS: u64 = 15;
const EDGE_DISPATCH_BACKLOG_METRICS_REFRESH_TIMEOUT_SECS: u64 = 5;

/// Spawn a background task that expires orphaned edge dispatch rows.
///
/// Normal tool waits call `fail_dispatch` on timeout/cancel. This sweeper covers
/// the unhappy path where the waiting pod crashes after inserting a dispatch
/// but before it can mark the request terminal.
fn spawn_edge_dispatch_cleanup(
    dispatch: Arc<dyn astra_services::multi_agent::EdgeDispatchService>,
    cancel: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    spawn_edge_dispatch_cleanup_with_config(
        dispatch,
        cancel,
        std::time::Duration::from_secs(EDGE_DISPATCH_CLEANUP_INTERVAL_SECS),
        std::time::Duration::from_secs(EDGE_DISPATCH_STALE_AFTER_SECS),
    )
}

fn spawn_edge_dispatch_cleanup_with_config(
    dispatch: Arc<dyn astra_services::multi_agent::EdgeDispatchService>,
    cancel: tokio_util::sync::CancellationToken,
    cleanup_interval: std::time::Duration,
    stale_after: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(cleanup_interval);
        interval.tick().await; // skip immediate first tick
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!(
                        target: "astra_runtime::cleanup",
                        "edge dispatch cleanup received cancellation; exiting"
                    );
                    break;
                }
                _ = interval.tick() => {}
            }

            match dispatch.cleanup_stale(stale_after).await {
                Ok(rows) if rows > 0 => {
                    tracing::info!(
                        target: "astra_runtime::cleanup",
                        rows,
                        stale_after_secs = stale_after.as_secs(),
                        "edge dispatch cleanup expired or deleted stale rows"
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(
                        target: "astra_runtime::cleanup",
                        error = %error,
                        "edge dispatch cleanup failed"
                    );
                }
            }
        }
    })
}

fn spawn_edge_dispatch_backlog_metrics_refresh(
    shared_pool: astra_core::SharedPool,
    metrics: astra_services::multi_agent::SharedMultiAgentMetrics,
    cancel: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let refresh_interval =
            std::time::Duration::from_secs(EDGE_DISPATCH_BACKLOG_METRICS_REFRESH_INTERVAL_SECS);
        let refresh_timeout =
            std::time::Duration::from_secs(EDGE_DISPATCH_BACKLOG_METRICS_REFRESH_TIMEOUT_SECS);

        loop {
            let refresh = astra_services::multi_agent::refresh_edge_dispatch_backlog_metrics(
                &shared_pool,
                &metrics,
            );
            tokio::select! {
                _ = cancel.cancelled() => {
                    break;
                }
                outcome = tokio::time::timeout(refresh_timeout, refresh) => {
                    match outcome {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            metrics
                                .dispatch_backlog_scrape_errors_total
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            tracing::warn!(
                                target: "astra_runtime::metrics",
                                error = %error,
                                "failed to refresh edge dispatch backlog metrics"
                            );
                        }
                        Err(_) => {
                            metrics
                                .dispatch_backlog_scrape_errors_total
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            tracing::warn!(
                                target: "astra_runtime::metrics",
                                timeout_ms = refresh_timeout.as_millis(),
                                "timed out refreshing edge dispatch backlog metrics"
                            );
                        }
                    }
                }
            }

            tokio::select! {
                _ = cancel.cancelled() => {
                    break;
                }
                _ = tokio::time::sleep(refresh_interval) => {}
            }
        }

        tracing::info!(
            target: "astra_runtime::metrics",
            "edge dispatch backlog metrics refresh received cancellation; exiting"
        );
    })
}

pub use astra_server_types::edge_connection_pool;

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    };

    use astra_core::{ErrorResponse, error_response};
    use astra_services::{
        CancelRunRecord, ChatRequestData, ChatRunRecord, ChatStreamRecord, RunLifecycleService,
        RunListRecord, RunStatusRecord,
        multi_agent::{EdgeDispatchRow, EdgeDispatchService},
    };
    use async_trait::async_trait;
    use axum::{Json, http::StatusCode};

    #[derive(Default)]
    struct RecordingEdgeDispatchService {
        cleanup_calls: AtomicUsize,
        stale_after_secs: AtomicU64,
    }

    #[async_trait]
    impl EdgeDispatchService for RecordingEdgeDispatchService {
        async fn insert_dispatch(
            &self,
            _user_id: &str,
            _edge_agent_id: &str,
            _request_id: &str,
            _payload_json: &str,
        ) -> Result<(), String> {
            unreachable!("insert_dispatch is not used in cleanup tests")
        }

        async fn poll_pending(
            &self,
            _user_id: &str,
            _edge_agent_id: &str,
        ) -> Result<Vec<EdgeDispatchRow>, String> {
            unreachable!("poll_pending is not used in cleanup tests")
        }

        async fn deliver_result(
            &self,
            _user_id: &str,
            _request_id: &str,
            _edge_agent_id: &str,
            _result_json: &str,
        ) -> Result<bool, String> {
            unreachable!("deliver_result is not used in cleanup tests")
        }

        async fn fail_dispatch(
            &self,
            _user_id: &str,
            _request_id: &str,
            _reason: &str,
        ) -> Result<bool, String> {
            unreachable!("fail_dispatch is not used in cleanup tests")
        }

        async fn wait_result(
            &self,
            _user_id: &str,
            _request_id: &str,
            _timeout: std::time::Duration,
        ) -> Result<Option<String>, String> {
            unreachable!("wait_result is not used in cleanup tests")
        }

        async fn cleanup_stale(&self, older_than: std::time::Duration) -> Result<u64, String> {
            self.cleanup_calls.fetch_add(1, Ordering::SeqCst);
            self.stale_after_secs
                .store(older_than.as_secs(), Ordering::SeqCst);
            Ok(1)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn edge_dispatch_cleanup_sweeper_uses_configured_stale_after() {
        let service = Arc::new(RecordingEdgeDispatchService::default());
        let dispatch: Arc<dyn EdgeDispatchService> = service.clone();
        let cancel = tokio_util::sync::CancellationToken::new();
        let handle = super::spawn_edge_dispatch_cleanup_with_config(
            dispatch,
            cancel.clone(),
            std::time::Duration::from_secs(10),
            std::time::Duration::from_secs(30),
        );

        tokio::task::yield_now().await;
        assert_eq!(service.cleanup_calls.load(Ordering::SeqCst), 0);

        tokio::time::advance(std::time::Duration::from_secs(10)).await;
        tokio::task::yield_now().await;

        assert_eq!(service.cleanup_calls.load(Ordering::SeqCst), 1);
        assert_eq!(service.stale_after_secs.load(Ordering::SeqCst), 30);

        cancel.cancel();
        handle.await.expect("cleanup task should stop cleanly");
    }

    #[derive(Default)]
    struct RecordingRunLifecycleService {
        drain_calls: AtomicUsize,
    }

    #[async_trait]
    impl RunLifecycleService for RecordingRunLifecycleService {
        async fn create_run(
            &self,
            _user_id: String,
            _request: ChatRequestData,
        ) -> Result<ChatRunRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!("create_run is not used in shutdown tests")
        }

        async fn stream_chat(
            &self,
            _user_id: String,
            _request: ChatRequestData,
        ) -> Result<ChatStreamRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!("stream_chat is not used in shutdown tests")
        }

        async fn get_run_status(
            &self,
            _run_id: String,
            _user_id: String,
        ) -> Result<RunStatusRecord, (StatusCode, Json<ErrorResponse>)> {
            Err(error_response(StatusCode::NOT_FOUND, "not used"))
        }

        async fn stream_run(
            &self,
            _run_id: String,
            _user_id: String,
            _last_index: u32,
        ) -> Result<Vec<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
            unreachable!("stream_run is not used in shutdown tests")
        }

        async fn cancel_run(
            &self,
            _run_id: String,
            _user_id: String,
        ) -> Result<CancelRunRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!("cancel_run is not used in shutdown tests")
        }

        async fn list_runs_cursor(
            &self,
            _user_id: String,
            _limit: u32,
            _cursor: Option<astra_services::runs::RunListCursor>,
        ) -> Result<RunListRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!("list_runs is not used in shutdown tests")
        }

        async fn drain_background_tasks(&self, _timeout: std::time::Duration) -> bool {
            self.drain_calls.fetch_add(1, Ordering::SeqCst);
            true
        }
    }

    #[tokio::test]
    async fn shutdown_drains_background_tasks() {
        let bg_cancel = tokio_util::sync::CancellationToken::new();
        let cancel_probe = bg_cancel.clone();
        let completed = Arc::new(AtomicUsize::new(0));
        let completed_probe = completed.clone();
        let bg_handle = tokio::spawn(async move {
            cancel_probe.cancelled().await;
            completed_probe.fetch_add(1, Ordering::SeqCst);
        });
        let run_lifecycle = Arc::new(RecordingRunLifecycleService::default());

        super::finish_server_shutdown(
            bg_cancel,
            vec![bg_handle],
            run_lifecycle.clone(),
            None,
            astra_server_types::edge_connection_pool::EdgeConnectionPool::new(),
        )
        .await;

        assert_eq!(completed.load(Ordering::SeqCst), 1);
        assert_eq!(run_lifecycle.drain_calls.load(Ordering::SeqCst), 1);
    }
}
