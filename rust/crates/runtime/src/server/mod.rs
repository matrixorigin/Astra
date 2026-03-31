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
mod auth_handlers;
mod bridge_prep;
mod chat_handlers;
pub mod delegation_engine;
mod delegation_handlers;
mod edge_callback_handlers;
mod http_helpers;
mod http_types;
mod learning_handlers;
mod meta_handlers;
mod platform_handlers;
mod reflect_handlers;
mod router_builder;
pub mod run_engine;
mod run_handlers;
pub mod run_lifecycle;
pub mod server_loop_host;
mod session_handlers;
mod state_builder;
mod task_handlers;
mod ws_handler;

use self::{
    bridge_prep::prepare_chat_turn_bridge_body,
    chat_route::{ChatRouteResponse, classify_chat_route},
    http_helpers::*,
    http_types::*,
};

mod chat_route;

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

    router_builder::build_router(state).layer(cors)
}

pub async fn serve(addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let settings = AppSettings::from_env()?;
    let state = state_builder::build_server_state(settings).await?;

    axum::serve(listener, build_app(state)).await?;
    Ok(())
}
