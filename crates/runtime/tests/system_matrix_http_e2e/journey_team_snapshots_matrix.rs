//! Team snapshots Matrix E2E: `GET/POST .../snapshots`, `DELETE /teams/snapshots/{id}`, `team_snapshots` SQL.

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::Row;

use super::harness::{bootstrap, delete_json, get_json, post_json};

fn minimal_team_payload(name: &str, description: &str) -> Value {
    json!({
        "name": name,
        "description": description,
        "coordination": { "type": "pipeline" },
        "members": [
            {
                "role": "alpha",
                "skills": [],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            },
            {
                "role": "beta",
                "skills": [],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            }
        ],
        "context": { "suite": "matrix_team_snapshots" },
        "worktree_mode": "shared",
        "max_parallel": 1
    })
}

pub async fn run_team_snapshots_db() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let auth = &b.auth_header;

    let team_name = format!("e2e_mx_snap_team_{}", ctx.suffix);

    let (st_create, _) = post_json(
        &ctx.app,
        "/teams",
        Some(auth),
        minimal_team_payload(&team_name, "snapshot journey"),
    )
    .await;
    assert_eq!(st_create, StatusCode::OK);

    let snap_body = json!({
        "label": "matrix-e2e-label",
        "session_id": ctx.session_id,
        "git_commit": "deadbeef"
    });

    let path_snap_post = format!("/teams/{team_name}/snapshots");
    let (st_sn, sn_j) = post_json(&ctx.app, &path_snap_post, Some(auth), snap_body).await;
    assert_eq!(st_sn, StatusCode::OK, "POST snapshot: {sn_j}");
    let snapshot_id = sn_j["snapshot_id"]
        .as_str()
        .expect("snapshot_id")
        .to_string();

    let path_snap_list = format!("/teams/{team_name}/snapshots");
    let (st_li, li_j) = get_json(&ctx.app, &path_snap_list, Some(auth), &[]).await;
    assert_eq!(st_li, StatusCode::OK, "GET snapshots: {li_j}");
    let snaps = li_j["snapshots"].as_array().expect("snapshots");
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0]["snapshot_id"].as_str(), Some(snapshot_id.as_str()));

    let row = sqlx::query(
        "SELECT snapshot_id, user_id, team_name, label FROM team_snapshots \
         WHERE snapshot_id = ? AND user_id = ?",
    )
    .bind(&snapshot_id)
    .bind(&ctx.user_id)
    .fetch_optional(&ctx.pool)
    .await
    .expect("team_snapshots SELECT");
    let row = row.expect("snapshot row");
    assert_eq!(row.get::<String, _>("team_name"), team_name);
    assert_eq!(row.get::<String, _>("label"), "matrix-e2e-label");

    let path_snap_del = format!("/teams/snapshots/{snapshot_id}");
    let (st_del, del_j) = delete_json(&ctx.app, &path_snap_del, Some(auth)).await;
    assert_eq!(st_del, StatusCode::OK, "DELETE snapshot: {del_j}");
    assert_eq!(del_j["deleted"].as_bool(), Some(true));

    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM team_snapshots WHERE snapshot_id = ? AND user_id = ?",
    )
    .bind(&snapshot_id)
    .bind(&ctx.user_id)
    .fetch_one(&ctx.pool)
    .await
    .expect("count after snapshot delete");
    assert_eq!(n, 0);

    let (st_li2, li2_j) = get_json(&ctx.app, &path_snap_list, Some(auth), &[]).await;
    assert_eq!(st_li2, StatusCode::OK);
    assert_eq!(
        li2_j["snapshots"].as_array().map(|a| a.len()),
        Some(0),
        "list empty after delete: {li2_j}"
    );

    let path_team = format!("/teams/{team_name}");
    let (st_team_del, _) = delete_json(&ctx.app, &path_team, Some(auth)).await;
    assert_eq!(st_team_del, StatusCode::OK);

    b.ctx.pool.close().await;
}
