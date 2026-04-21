//! `POST /chat/route` response shape + authenticated `GET /models` list.

use axum::http::StatusCode;
use serde_json::json;

use super::harness::{bootstrap, get_json, post_json};

pub async fn run_chat_route_and_models_smoke() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let auth = &b.auth_header;

    let (st_route, route_j) = post_json(
        &ctx.app,
        "/chat/route",
        Some(auth),
        json!({ "query": "run cargo test and fix compile errors" }),
    )
    .await;
    assert_eq!(st_route, StatusCode::OK, "chat/route: {route_j}");
    assert!(
        route_j.get("tool_filter").is_some() && route_j.get("task_type").is_some(),
        "chat/route shape: {route_j}"
    );

    let (st_models, models_j) = get_json(&ctx.app, "/models", Some(auth), &[]).await;
    assert_eq!(st_models, StatusCode::OK, "models: {models_j}");
    assert!(
        models_j.as_array().is_some(),
        "GET /models array: {models_j}"
    );

    b.ctx.pool.close().await;
}
