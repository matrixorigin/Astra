use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    response::Response,
    routing::{delete, get, post},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE};
use chrono::Utc;
use tower_http::cors::{AllowHeaders, AllowOrigin, CorsLayer};
use uuid::Uuid;

use super::*;

mod admin_handlers;
mod audit_handlers;
mod auth_handlers;
mod bridge_prep;
mod chat_handlers;
pub mod conflict_resolver;
pub mod delegation_engine;
mod delegation_handlers;
mod edge_callback_handlers;
mod edge_status_handler;
mod edge_ws_handler;
pub(crate) mod header_utils;
mod http_helpers;
mod learning_handlers;
mod llm_trusted_domains_handlers;
mod meta_handlers;
mod plan_handlers;
mod platform_handlers;
mod reflect_handlers;
mod request_trace;
mod resource_handlers;
mod router_builder;
pub mod run_engine;
mod run_handlers;
pub mod run_lifecycle;
pub mod server_loop_host;
pub mod server_skill_subrun;
pub mod server_tool_executor;
mod session_handlers;
mod state_builder;
mod task_handlers;
mod team_handlers;
pub mod team_orchestrator;
mod ws_handler;

use self::{bridge_prep::prepare_chat_turn_bridge_body, http_helpers::*};
use astra_server_types::*;
use astra_server_types::{ChatRouteResponse, classify_chat_route};
mod completions;

pub use request_trace::RequestTrace;
pub use state_builder::build_server_state;

pub fn build_app(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::any())
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

    let listener = tokio::net::TcpListener::bind(addr).await?;
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

    // Cancellation token wired into background sweepers; cancelled after axum
    // serve returns so we can drain them deterministically before tearing down
    // the runtime / pool / OTLP exporter.
    let bg_cancel = tokio_util::sync::CancellationToken::new();
    let mut bg_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    if let Some(ref pool) = state.shared_pool {
        bg_handles.push(spawn_data_cleanup(pool.clone(), bg_cancel.clone()));
        bg_handles.push(astra_services::session_reaper::spawn_session_reaper(
            pool.clone(),
            bg_cancel.clone(),
        ));
    }

    // Clone the matrix runtime handle before moving `state` into `build_app`
    // so we can drain ingestion + sync sidecars after axum returns.
    let matrix_runtime = state.matrix_cloud_runtime.clone();
    let run_lifecycle = state.run_lifecycle_service.clone();

    axum::serve(listener, build_app(state))
        .with_graceful_shutdown(http_shutdown_signal())
        .await?;

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
    Ok(())
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
            let results = astra_services::cleanup_expired_data(pool.get(), &policy).await;
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

pub use astra_server_types::edge_connection_pool;

#[cfg(test)]
mod tests {
    /// U1: build_app must set a DefaultBodyLimit to prevent OOM from
    /// oversized request bodies. The raw `Bytes` extractor on /chat/turn
    /// has no built-in limit — without an explicit layer, the server
    /// buffers the entire request into memory.
    #[test]
    fn build_app_has_body_size_limit() {
        let source = include_str!("mod.rs");

        let test_start = source.find("#[cfg(test)]").unwrap_or(source.len());
        let prod_code = &source[..test_start];
        assert!(
            prod_code.contains("DefaultBodyLimit"),
            "build_app must apply DefaultBodyLimit layer to prevent OOM"
        );
    }

    /// P0-C: serve() shutdown path must drain background agentic loop tasks.
    #[test]
    fn shutdown_drains_background_tasks() {
        let source = include_str!("mod.rs");

        let test_start = source.find("#[cfg(test)]").unwrap_or(source.len());
        let prod_code = &source[..test_start];
        assert!(
            prod_code.contains("drain_background_tasks"),
            "serve() shutdown must call drain_background_tasks"
        );
    }
}
