//! Global admin control-plane routes reject normal users and remain usable
//! after the same principal receives `astra_admin`.
use super::harness::{bootstrap, get_json, grant_astra_admin_role, revoke_astra_admin_role};
use axum::http::StatusCode;

pub async fn run_admin_control_plane_rbac() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let auth = &b.auth_header;
    let app = &ctx.app;
    let pool = &ctx.pool;
    let user_id = ctx.user_id.as_str();

    revoke_astra_admin_role(pool, user_id).await;

    let (st_denied, denied_j) = get_json(app, "/admin/tokens", Some(auth.as_str()), &[]).await;
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
