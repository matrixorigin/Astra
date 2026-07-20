//! Global admin control-plane routes reject normal users and remain usable
//! after the same principal receives `astra_admin`.
use axum::http::StatusCode;
use serde_json::json;

use super::harness::{
    bootstrap, get_json, grant_astra_admin_role, post_json, revoke_astra_admin_role,
};

pub async fn run_admin_control_plane_rbac() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let auth = &b.auth_header;
    let app = &ctx.app;
    let pool = &ctx.pool;
    let user_id = ctx.user_id.as_str();

    revoke_astra_admin_role(pool, user_id).await;

    let gateway_id = format!("gateway-{}", ctx.suffix);
    let gateway_path = format!("/model-gateways/{gateway_id}");
    let gateway_request = json!({
        "id": gateway_id,
        "resolve_url": "https://models.example/v1",
        "model_protocol": "openai_chat_completions",
        "metadata": {"region": "test"}
    });

    let (st_denied, denied_j) = get_json(app, "/admin/tokens", Some(auth.as_str()), &[]).await;
    assert_eq!(
        st_denied,
        StatusCode::FORBIDDEN,
        "admin tokens without role: {denied_j}"
    );
    let (gateway_denied, denied_gateway_body) = post_json(
        app,
        "/model-gateways",
        Some(auth.as_str()),
        gateway_request.clone(),
    )
    .await;
    assert_eq!(
        gateway_denied,
        StatusCode::FORBIDDEN,
        "normal users must not register model execution infrastructure: {denied_gateway_body}"
    );

    grant_astra_admin_role(pool, user_id).await;

    let (st_ok, body) = get_json(app, "/admin/tokens", Some(auth.as_str()), &[]).await;
    assert_eq!(st_ok, StatusCode::OK, "admin tokens: {body}");
    assert!(
        body.as_array().is_some(),
        "GET /admin/tokens should return a JSON array: {body}"
    );

    let (gateway_created, created_body) =
        post_json(app, "/model-gateways", Some(auth.as_str()), gateway_request).await;
    assert_eq!(
        gateway_created,
        StatusCode::OK,
        "create gateway: {created_body}"
    );
    assert_eq!(created_body["id"], gateway_id);
    assert_eq!(created_body["status"], "active");

    let (gateway_loaded, loaded_body) =
        get_json(app, &gateway_path, Some(auth.as_str()), &[]).await;
    assert_eq!(gateway_loaded, StatusCode::OK, "get gateway: {loaded_body}");
    assert_eq!(loaded_body["resolve_url"], "https://models.example/v1");
    assert_eq!(loaded_body["model_protocol"], "openai_chat_completions");

    let (unsupported_status, unsupported_body) = post_json(
        app,
        "/model-gateways",
        Some(auth.as_str()),
        json!({
            "id": format!("unsupported-{}", ctx.suffix),
            "resolve_url": "https://models.example/v1",
            "model_protocol": "unknown_protocol"
        }),
    )
    .await;
    assert_eq!(unsupported_status, StatusCode::BAD_REQUEST);
    assert_eq!(
        unsupported_body["error_code"],
        "model_gateway_protocol_unsupported"
    );

    let (disabled_status, disabled_body) = post_json(
        app,
        &format!("{gateway_path}/disable"),
        Some(auth.as_str()),
        json!({}),
    )
    .await;
    assert_eq!(
        disabled_status,
        StatusCode::OK,
        "disable gateway: {disabled_body}"
    );
    assert_eq!(disabled_body["status"], "disabled");

    ctx.pool.close().await;
}
