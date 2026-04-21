//! Team CRUD Matrix E2E: `POST/GET/DELETE /teams`, `GET .../executions`, `team_definitions` rows.
//!
//! Runs against real [`astra_services::team_persistence::MatrixOneTeamStore`] via `build_server_state`.

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
                "role": "coder",
                "skills": [],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            },
            {
                "role": "reviewer",
                "skills": [],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            }
        ],
        "context": { "suite": "matrix_team_crud" },
        "worktree_mode": "shared",
        "max_parallel": 1
    })
}

pub async fn run_team_crud_db() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let auth = &b.auth_header;

    let team_name = format!("e2e_mx_team_{}", ctx.suffix);

    let payload_v1 = minimal_team_payload(&team_name, "matrix e2e team crud v1");
    let (st, body) = post_json(&ctx.app, "/teams", Some(auth), payload_v1.clone()).await;
    assert_eq!(st, StatusCode::OK, "POST /teams create: {body}");
    let team_id = body["team_id"].as_str().expect("team_id").to_string();
    assert_eq!(body["user_id"].as_str(), Some(ctx.user_id.as_str()));
    assert_eq!(body["name"].as_str(), Some(team_name.as_str()));

    let (st_list, list_j) = get_json(&ctx.app, "/teams", Some(auth), &[]).await;
    assert_eq!(st_list, StatusCode::OK, "GET /teams: {list_j}");
    let teams = list_j["teams"].as_array().expect("teams array");
    assert!(
        teams.iter().any(|t| t["name"].as_str() == Some(team_name.as_str())),
        "list should include our team: {list_j}"
    );

    let path_detail = format!("/teams/{team_name}");
    let (st_get, get_j) = get_json(&ctx.app, &path_detail, Some(auth), &[]).await;
    assert_eq!(st_get, StatusCode::OK, "GET team: {get_j}");
    assert_eq!(get_j["team_id"].as_str(), Some(team_id.as_str()));

    let path_exec = format!("/teams/{team_name}/executions");
    let (st_ex, ex_j) = get_json(&ctx.app, &path_exec, Some(auth), &[]).await;
    assert_eq!(st_ex, StatusCode::OK, "GET executions: {ex_j}");
    assert_eq!(
        ex_j["executions"].as_array().map(|a| a.len()),
        Some(0),
        "no runs yet: {ex_j}"
    );

    let row = sqlx::query(
        "SELECT team_id, user_id, name FROM team_definitions WHERE user_id = ? AND name = ?",
    )
    .bind(&ctx.user_id)
    .bind(&team_name)
    .fetch_optional(&ctx.pool)
    .await
    .expect("team_definitions SELECT");
    let row = row.expect("team_definitions row after POST");
    assert_eq!(row.get::<String, _>("team_id"), team_id);
    assert_eq!(row.get::<String, _>("user_id"), ctx.user_id);

    let payload_v2 = minimal_team_payload(&team_name, "matrix e2e team crud v2 upsert");
    let (st2, body2) = post_json(&ctx.app, "/teams", Some(auth), payload_v2).await;
    assert_eq!(st2, StatusCode::OK, "POST /teams upsert: {body2}");
    assert_eq!(
        body2["team_id"].as_str(),
        Some(team_id.as_str()),
        "upsert keeps logical team_id from first create"
    );
    assert_eq!(
        body2["description"].as_str(),
        Some("matrix e2e team crud v2 upsert")
    );

    let desc_db: String = sqlx::query_scalar(
        "SELECT description FROM team_definitions WHERE user_id = ? AND name = ?",
    )
    .bind(&ctx.user_id)
    .bind(&team_name)
    .fetch_one(&ctx.pool)
    .await
    .expect("description after upsert");
    assert_eq!(desc_db, "matrix e2e team crud v2 upsert");

    let (st_del, del_j) = delete_json(&ctx.app, &path_detail, Some(auth)).await;
    assert_eq!(st_del, StatusCode::OK, "DELETE team: {del_j}");
    assert_eq!(del_j["deleted"].as_bool(), Some(true));

    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM team_definitions WHERE user_id = ? AND name = ?",
    )
    .bind(&ctx.user_id)
    .bind(&team_name)
    .fetch_one(&ctx.pool)
    .await
    .expect("count after delete");
    assert_eq!(n, 0, "team_definitions row removed");

    b.ctx.pool.close().await;
}
