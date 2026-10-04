//! Team HTTP negative paths on Matrix stack: auth, 404, validation (`validate_team`).

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::harness::{bootstrap, delete_json, get_json, post_json};

pub async fn run_team_http_negative_paths() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let auth = &b.auth_header;

    let (st_noauth, _) = get_json(&ctx.app, "/teams", None, &[]).await;
    assert_eq!(
        st_noauth,
        StatusCode::UNAUTHORIZED,
        "GET /teams without Authorization"
    );

    let ghost = format!("no_such_team_{}", ctx.suffix);
    let (st_404_get, _) = get_json(&ctx.app, &format!("/teams/{ghost}"), Some(auth), &[]).await;
    assert_eq!(st_404_get, StatusCode::NOT_FOUND);

    let (st_404_del, _) = delete_json(&ctx.app, &format!("/teams/{ghost}"), Some(auth)).await;
    assert_eq!(st_404_del, StatusCode::NOT_FOUND);

    let dup_roles: Value = json!({
        "name": format!("bad_dup_roles_{}", ctx.suffix),
        "description": "duplicate roles",
        "members": [
            {
                "role": "twin",
                "skills": [],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            },
            {
                "role": "twin",
                "skills": [],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            }
        ]
    });
    let (st_dup, dup_j) = post_json(&ctx.app, "/teams", Some(auth), dup_roles).await;
    assert_eq!(st_dup, StatusCode::BAD_REQUEST, "duplicate roles: {dup_j}");
    b.ctx.close().await;
}
