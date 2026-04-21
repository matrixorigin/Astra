//! Team HTTP ↔ Matrix row fidelity: JSON columns match GET responses; snapshot blob matches definition;
//! list length vs SQL count; executions `limit` query still OK when empty.

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::Row;

use super::harness::{bootstrap, delete_json, get_json, post_json};

fn fidelity_team_payload(name: &str) -> Value {
    json!({
        "name": name,
        "description": "data fidelity probe",
        "coordination": { "type": "pipeline" },
        "members": [
            {
                "role": "coder",
                "system_prompt": "Do coding",
                "skills": ["read"],
                "model_override": null,
                "mcp_servers": [],
                "agent_id": null,
                "can_delegate": false,
                "max_delegation_depth": 0
            },
            {
                "role": "reviewer",
                "system_prompt": null,
                "skills": [],
                "mcp_servers": [],
                "agent_id": "custom-rev",
                "can_delegate": true,
                "max_delegation_depth": 1
            }
        ],
        "context": { "suite": "matrix_team_data_fidelity", "k": "v" },
        "worktree_mode": "isolated",
        "max_parallel": 2,
        "budget": {
            "max_cost_usd": 10.0,
            "max_tokens": 50000,
            "max_duration_secs": 600
        }
    })
}

pub async fn run_team_http_db_fidelity() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let auth = &b.auth_header;

    let team_name = format!("e2e_mx_fidelity_{}", ctx.suffix);

    let payload = fidelity_team_payload(&team_name);
    let (st, detail) = post_json(&ctx.app, "/teams", Some(auth), payload).await;
    assert_eq!(st, StatusCode::OK, "POST team: {detail}");

    let path_detail = format!("/teams/{team_name}");
    let (st_get, get_j) = get_json(&ctx.app, &path_detail, Some(auth), &[]).await;
    assert_eq!(st_get, StatusCode::OK, "GET detail: {get_j}");

    let row = sqlx::query(
        "SELECT description, coordination, members_json, context_json, \
                worktree_mode, budget_json, max_parallel \
         FROM team_definitions WHERE user_id = ? AND name = ?",
    )
    .bind(&ctx.user_id)
    .bind(&team_name)
    .fetch_one(&ctx.pool)
    .await
    .expect("team_definitions fidelity row");

    assert_eq!(
        row.get::<String, _>("description"),
        get_j["description"].as_str().unwrap_or("")
    );

    let coord_db: String = row.get("coordination");
    let coord_http = get_j["coordination"].clone();
    let coord_parsed: Value =
        serde_json::from_str(&coord_db).expect("coordination JSON from DB");
    assert_eq!(
        coord_parsed, coord_http,
        "coordination DB vs GET detail mismatch"
    );

    let members_db: String = row.get("members_json");
    let members_parsed: Value =
        serde_json::from_str(&members_db).expect("members_json from DB");
    assert_eq!(
        members_parsed,
        get_j["members"].clone(),
        "members_json DB vs GET detail mismatch"
    );

    let ctx_db: Option<String> = row.try_get("context_json").ok();
    let ctx_str = ctx_db.unwrap_or_default();
    let ctx_parsed: Value = if ctx_str.is_empty() {
        json!({})
    } else {
        serde_json::from_str(&ctx_str).expect("context_json")
    };
    assert_eq!(
        ctx_parsed,
        get_j["context"].clone(),
        "context_json DB vs GET detail mismatch"
    );

    assert_eq!(
        row.get::<String, _>("worktree_mode").to_ascii_lowercase(),
        get_j["worktree_mode"]
            .as_str()
            .unwrap_or("")
            .to_ascii_lowercase(),
        "worktree_mode DB vs GET"
    );

    let budget_db: Option<String> = row.try_get("budget_json").ok().flatten();
    match (budget_db.as_deref(), get_j.get("budget")) {
        (Some(bs), Some(bv)) if !bs.is_empty() => {
            let bv_db: Value = serde_json::from_str(bs).expect("budget_json");
            assert_eq!(bv_db, *bv, "budget_json DB vs GET");
        }
        _ => panic!("budget roundtrip missing: db={budget_db:?} http={:?}", get_j.get("budget")),
    }

    let mp_db: i64 = row
        .try_get::<i64, _>("max_parallel")
        .or_else(|_| row.try_get::<u32, _>("max_parallel").map(|x| x as i64))
        .unwrap_or(0);
    assert_eq!(
        mp_db,
        get_j["max_parallel"].as_i64().unwrap_or_else(|| {
            get_j["max_parallel"]
                .as_u64()
                .expect("max_parallel") as i64
        }),
        "max_parallel DB vs GET"
    );

    let (st_list, list_j) = get_json(&ctx.app, "/teams", Some(auth), &[]).await;
    assert_eq!(st_list, StatusCode::OK);
    let listed = list_j["teams"].as_array().expect("teams");
    let sql_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM team_definitions WHERE user_id = ?",
    )
    .bind(&ctx.user_id)
    .fetch_one(&ctx.pool)
    .await
    .expect("COUNT team_definitions");
    assert_eq!(
        listed.len() as i64,
        sql_count,
        "GET /teams len should match SQL COUNT(*) for user"
    );

    let path_exec_limited = format!(
        "/teams/{team_name}/executions?limit=3",
    );
    let (st_ex, ex_j) = get_json(&ctx.app, &path_exec_limited, Some(auth), &[]).await;
    assert_eq!(st_ex, StatusCode::OK, "GET executions limited: {ex_j}");
    assert!(
        ex_j["executions"].as_array().map(|a| a.is_empty()).unwrap_or(false),
        "still no executions: {ex_j}"
    );

    let snap_body = json!({
        "label": "fidelity-snap",
        "session_id": ctx.session_id,
        "git_commit": "cafef00d"
    });
    let (st_sn, sn_j) = post_json(
        &ctx.app,
        &format!("/teams/{team_name}/snapshots"),
        Some(auth),
        snap_body,
    )
    .await;
    assert_eq!(st_sn, StatusCode::OK, "snapshot: {sn_j}");
    let snapshot_id = sn_j["snapshot_id"].as_str().expect("snapshot_id");

    let blob: Option<String> = sqlx::query_scalar(
        "SELECT team_definition_json FROM team_snapshots WHERE snapshot_id = ? AND user_id = ?",
    )
    .bind(snapshot_id)
    .bind(&ctx.user_id)
    .fetch_optional(&ctx.pool)
    .await
    .expect("team_definition_json SELECT")
    .flatten();
    let blob_str = blob.expect("team_definition_json present");
    let blob_val: Value = serde_json::from_str(&blob_str).expect("team_definition_json parse");
    assert_eq!(
        blob_val["team_id"].as_str(),
        detail["team_id"].as_str(),
        "snapshot blob team_id matches create response"
    );
    assert_eq!(
        blob_val["name"].as_str(),
        Some(team_name.as_str()),
        "snapshot blob name matches"
    );
    assert_eq!(
        blob_val["user_id"].as_str(),
        Some(ctx.user_id.as_str()),
        "snapshot blob user_id matches JWT subject"
    );

    let path_snaps = format!("/teams/{team_name}/snapshots");
    let (st_sn_list, snaps_j) = get_json(&ctx.app, &path_snaps, Some(auth), &[]).await;
    assert_eq!(st_sn_list, StatusCode::OK);
    let snaps = snaps_j["snapshots"].as_array().expect("snapshots");
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0]["snapshot_id"].as_str(), Some(snapshot_id));
    assert_eq!(snaps[0]["label"].as_str(), Some("fidelity-snap"));
    assert_eq!(
        snaps[0]["session_id"].as_str(),
        Some(ctx.session_id.as_str())
    );
    assert_eq!(snaps[0]["git_commit"].as_str(), Some("cafef00d"));

    let (_, _) =
        delete_json(&ctx.app, &format!("/teams/snapshots/{snapshot_id}"), Some(auth)).await;
    let (_, _) = delete_json(&ctx.app, &path_detail, Some(auth)).await;

    b.ctx.pool.close().await;
}
