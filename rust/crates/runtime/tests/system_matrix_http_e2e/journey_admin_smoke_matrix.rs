//! `GET /admin/tokens` — 403 without `astra_admin`, then 200 JSON array after role grant.
use axum::http::StatusCode;

use super::harness::{
    bootstrap, get_json, grant_astra_admin_role, revoke_astra_admin_role,
};

pub async fn run_admin_tokens_smoke() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let auth = &b.auth_header;
    let app = &ctx.app;
    let pool = &ctx.pool;
    let user_id = ctx.user_id.as_str();

    revoke_astra_admin_role(pool, user_id).await;

    let (st_denied, denied_j) =
        get_json(app, "/admin/tokens", Some(auth.as_str()), &[]).await;
    assert_eq!(
        st_denied,
        StatusCode::FORBIDDEN,
        "admin tokens without role: {denied_j}"
    );

    grant_astra_admin_role(pool, user_id).await;

    let (st_ok, body) = get_json(app, "/admin/tokens", Some(auth.as_str()), &[]).await;
    assert_eq!(st_ok, StatusCode::OK, "admin tokens: {body}");
    assert!(
        body.as_array().is_some(),
        "GET /admin/tokens should return a JSON array: {body}"
    );

    ctx.pool.close().await;
}
