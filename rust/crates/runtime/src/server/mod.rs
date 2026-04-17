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
pub mod agent_mailbox;
pub mod agent_mcp;
mod audit_handlers;
mod auth_handlers;
mod bridge_prep;
mod chat_handlers;
pub mod conflict_resolver;
pub mod delegation_engine;
mod delegation_handlers;
mod edge_callback_handlers;
pub mod edge_connection_pool;
mod edge_status_handler;
mod edge_ws_handler;
pub mod edge_ws_protocol;
pub(crate) mod header_utils;
mod http_helpers;
mod http_types;
mod learning_handlers;
mod meta_handlers;
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
pub mod worktree_isolation;
pub mod ws_approval_gate;
mod ws_handler;
pub mod ws_progress_callback;

use self::{
    bridge_prep::prepare_chat_turn_bridge_body,
    chat_route::{ChatRouteResponse, classify_chat_route},
    http_helpers::*,
    http_types::*,
};

mod chat_route;
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
        .layer(axum::middleware::from_fn(
            request_trace::request_trace_middleware,
        ))
        .layer(cors)
}

pub async fn serve(addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
    static TRACING: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    TRACING.get_or_init(|| {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                    tracing_subscriber::EnvFilter::new("warn,astra_runtime=info")
                }),
            )
            .try_init();
    });

    let listener = tokio::net::TcpListener::bind(addr).await?;
    let settings = AppSettings::from_env()?;
    let state = state_builder::build_server_state(settings).await?;

    // Warn about proxy settings that can cause confusing 502s for local clients
    if let Ok(proxy) = std::env::var("http_proxy").or_else(|_| std::env::var("HTTP_PROXY"))
        && !proxy.is_empty()
    {
        eprintln!(
            "[warn] HTTP proxy detected: {proxy}. \
             Local callers should set NO_PROXY=127.0.0.1,localhost or use --noproxy."
        );
    }

    // Spawn periodic expired data cleanup (runs every 6 hours)
    if let Some(ref pool) = state.shared_pool {
        spawn_data_cleanup(pool.clone());
        astra_services::session_reaper::spawn_session_reaper(pool.clone());
    }

    axum::serve(listener, build_app(state)).await?;
    Ok(())
}

/// Spawn a background task that periodically cleans up expired data.
fn spawn_data_cleanup(pool: astra_core::SharedPool) {
    use astra_services::RetentionPolicy;
    use std::time::Duration;

    let cleanup_interval = Duration::from_secs(
        std::env::var("MO_CLEANUP_INTERVAL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(6 * 3600), // default: 6 hours
    );

    tokio::spawn(async move {
        let policy = RetentionPolicy::default();
        let mut interval = tokio::time::interval(cleanup_interval);
        interval.tick().await; // skip immediate first tick
        loop {
            interval.tick().await;
            let results = astra_services::cleanup_expired_data(pool.get(), &policy).await;
            let total: u64 = results.iter().map(|r| r.rows_deleted).sum();
            if total > 0 {
                eprintln!(
                    "[cleanup] Purged {total} expired rows: {}",
                    results
                        .iter()
                        .filter(|r| r.rows_deleted > 0)
                        .map(|r| format!("{}={}", r.table, r.rows_deleted))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
    });
}
