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
    let (st_404_get, _) = get_json(
        &ctx.app,
        &format!("/teams/{ghost}"),
        Some(auth),
        &[],
    )
    .await;
    assert_eq!(st_404_get, StatusCode::NOT_FOUND);

    let (st_404_del, _) = delete_json(&ctx.app, &format!("/teams/{ghost}"), Some(auth)).await;
    assert_eq!(st_404_del, StatusCode::NOT_FOUND);

    let bad_empty_members: Value = json!({
        "name": format!("bad_empty_{}", ctx.suffix),
        "description": "should fail validation",
        "coordination": { "type": "pipeline" },
        "members": []
    });
    let (st_bad, bad_j) = post_json(&ctx.app, "/teams", Some(auth), bad_empty_members).await;
    assert_eq!(st_bad, StatusCode::BAD_REQUEST, "empty members: {bad_j}");

    let dup_roles: Value = json!({
        "name": format!("bad_dup_roles_{}", ctx.suffix),
        "description": "duplicate roles",
        "coordination": { "type": "pipeline" },
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

    let budget_all_zero: Value = json!({
        "name": format!("bad_budget_all_zero_{}", ctx.suffix),
        "description": "invalid budget",
        "coordination": { "type": "pipeline" },
        "members": [
            {
                "role": "only",
                "skills": [],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            },
            {
                "role": "second",
                "skills": [],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            }
        ],
        "budget": {
            "max_cost_usd": 0.0,
            "max_tokens": 0,
            "max_duration_secs": 0
        }
    });
    let (st_bz, bz_j) = post_json(&ctx.app, "/teams", Some(auth), budget_all_zero).await;
    assert_eq!(st_bz, StatusCode::BAD_REQUEST, "budget all zero: {bz_j}");

    let budget_negative: Value = json!({
        "name": format!("bad_budget_neg_{}", ctx.suffix),
        "description": "negative usd",
        "coordination": { "type": "pipeline" },
        "members": [
            {
                "role": "x1",
                "skills": [],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            },
            {
                "role": "x2",
                "skills": [],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            }
        ],
        "budget": {
            "max_cost_usd": -1.0,
            "max_tokens": 100,
            "max_duration_secs": 60
        }
    });
    let (st_bn, bn_j) = post_json(&ctx.app, "/teams", Some(auth), budget_negative).await;
    assert_eq!(st_bn, StatusCode::BAD_REQUEST, "negative budget: {bn_j}");

    let adversarial_three_members: Value = json!({
        "name": format!("bad_adv_count_{}", ctx.suffix),
        "description": "adversarial needs exactly 2 members",
        "coordination": { "type": "adversarial", "max_rounds": 3, "threshold": 0.8 },
        "members": [
            {
                "role": "p",
                "skills": [],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            },
            {
                "role": "r",
                "skills": [],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            },
            {
                "role": "extra",
                "skills": [],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            }
        ]
    });
    let (st_adv, adv_j) = post_json(&ctx.app, "/teams", Some(auth), adversarial_three_members).await;
    assert_eq!(
        st_adv,
        StatusCode::BAD_REQUEST,
        "adversarial member count: {adv_j}"
    );

    b.ctx.pool.close().await;
}
