//! `POST /chat/route` plus authenticated Offering and Model Access projections.

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
    let mut route_keys = route_j
        .as_object()
        .expect("chat/route should return an object")
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    route_keys.sort_unstable();
    assert_eq!(
        route_keys,
        ["intent", "matched_by", "query", "task_type", "tier"],
        "chat/route shape: {route_j}"
    );

    let (st_models, models_j) = get_json(&ctx.app, "/models", Some(auth), &[]).await;
    assert_eq!(st_models, StatusCode::OK, "models: {models_j}");
    assert!(
        models_j.as_array().is_some(),
        "GET /models array: {models_j}"
    );

    let (st_access, access_j) = get_json(&ctx.app, "/model-access", Some(auth), &[]).await;
    assert_eq!(st_access, StatusCode::OK, "model-access: {access_j}");
    let accesses = access_j["accesses"]
        .as_array()
        .expect("model-access accesses array");
    assert_eq!(accesses.len(), 1, "self-hosted server access: {access_j}");
    assert_eq!(accesses[0]["id"], "self-hosted");
    assert_eq!(accesses[0]["kind"], "self_hosted");
    assert_eq!(accesses[0]["execution_placement"], "server");

    let effective_offerings = access_j["offerings"]
        .as_array()
        .expect("model-access offerings array");
    assert!(effective_offerings.iter().all(|offering| {
        offering["is_active"] == true
            && offering["access_id"] == "self-hosted"
            && offering["execution_placement"] == "server"
            && offering["offering_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
    }));
    assert_eq!(
        accesses[0]["available_model_count"].as_u64(),
        Some(effective_offerings.len() as u64),
        "access readiness must derive from effective Offerings"
    );

    b.ctx.pool.close().await;
}
