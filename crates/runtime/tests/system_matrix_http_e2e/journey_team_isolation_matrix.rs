//! Cross-user isolation: user B cannot load or delete team rows owned by user A (404 + list).

use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;

use super::harness::{E2E_PASSWORD, bootstrap, delete_json, get_json, post_json};

pub async fn run_team_cross_user_isolation() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let auth_a = &b.auth_header;

    let team_name = format!("e2e_mx_iso_{}", ctx.suffix);

    let payload = json!({
        "name": team_name,
        "description": "owner A only",
        "coordination": { "type": "pipeline" },
        "members": [
            {
                "role": "a1",
                "skills": [],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            },
            {
                "role": "a2",
                "skills": [],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            }
        ],
        "context": {},
        "worktree_mode": "shared",
        "max_parallel": 1
    });

    let (st_up, _) = post_json(&ctx.app, "/teams", Some(auth_a), payload).await;
    assert_eq!(st_up, StatusCode::OK);

    let b_suffix = Uuid::new_v4().simple().to_string();
    let b_username = format!("prod_matrix_iso_{b_suffix}");
    let b_email = format!("prod_matrix_iso_{b_suffix}@e2e.test");

    let (st_reg, reg_b) = post_json(
        &ctx.app,
        "/auth/register",
        None,
        json!({
            "username": b_username,
            "email": b_email,
            "password": E2E_PASSWORD,
            "display_name": "Team isolation B"
        }),
    )
    .await;
    assert_eq!(st_reg, StatusCode::CREATED, "register B: {reg_b}");

    let (st_login, login_j) = post_json(
        &ctx.app,
        "/auth/login",
        None,
        json!({ "username": b_username, "password": E2E_PASSWORD }),
    )
    .await;
    assert_eq!(st_login, StatusCode::OK, "login B: {login_j}");
    let access_b = login_j["access_token"].as_str().expect("B access_token");
    let auth_b = format!("Bearer {access_b}");

    let path_t = format!("/teams/{team_name}");
    let (st_g, _) = get_json(&ctx.app, &path_t, Some(&auth_b), &[]).await;
    assert_eq!(st_g, StatusCode::NOT_FOUND, "B must not see A team by name");

    let (st_d, _) = delete_json(&ctx.app, &path_t, Some(&auth_b)).await;
    assert_eq!(st_d, StatusCode::NOT_FOUND, "B must not delete A team");

    let (st_list_b, list_b) = get_json(&ctx.app, "/teams", Some(&auth_b), &[]).await;
    assert_eq!(st_list_b, StatusCode::OK);
    let teams_b = list_b["teams"].as_array().expect("teams B");
    assert!(
        !teams_b
            .iter()
            .any(|t| t["name"].as_str() == Some(team_name.as_str())),
        "B list must not contain A team: {list_b}"
    );

    let (st_del_a, _) = delete_json(&ctx.app, &path_t, Some(auth_a)).await;
    assert_eq!(st_del_a, StatusCode::OK);

    b.ctx.pool.close().await;
}
