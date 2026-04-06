//! `/tasks` list/get/progress + lease claim/GET/renew/release + `agent_tasks` / `task_leases` SQL;
//! `/chat` run pause/resume (HTTP only; run store is in-memory in `build_server_state`).
use axum::http::StatusCode;
use serde_json::json;
use sqlx::Row;
use tower::util::ServiceExt;

use super::harness::{
    bootstrap, cleanup_task_rows, get_json, post_empty, post_json, post_json_with_headers, put_json,
};

pub async fn run_tasks_lease_with_db_assertions() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let app = &ctx.app;
    let user_id = ctx.user_id.clone();
    let session_id = ctx.session_id.clone();
    let edge_agent_id = ctx.edge_agent_id.clone();

    let (st_task, task_j) = post_json(
        app,
        "/tasks",
        Some(auth.as_str()),
        json!({
            "title": "matrix e2e task",
            "description": "lease probe",
            "session_id": session_id,
        }),
    )
    .await;
    assert_eq!(st_task, StatusCode::CREATED, "create task: {task_j}");
    let task_id = task_j["task_id"].as_str().expect("task_id").to_string();

    let row =
        sqlx::query("SELECT user_id, session_id, title, status FROM agent_tasks WHERE task_id = ?")
            .bind(&task_id)
            .fetch_optional(pool)
            .await
            .expect("agent_tasks select");
    let row = row.expect("agent_tasks row after POST /tasks");
    assert_eq!(
        row.try_get::<String, _>("user_id").ok().as_deref(),
        Some(user_id.as_str())
    );
    assert_eq!(
        row.try_get::<Option<String>, _>("session_id")
            .ok()
            .flatten()
            .as_deref(),
        Some(session_id.as_str())
    );
    assert_eq!(
        row.try_get::<String, _>("title").ok().as_deref(),
        Some("matrix e2e task")
    );
    assert_eq!(
        row.try_get::<String, _>("status").ok().as_deref(),
        Some("pending")
    );

    let (st_list, list_j) = get_json(app, "/tasks", Some(auth.as_str()), &[]).await;
    assert_eq!(st_list, StatusCode::OK, "list tasks: {list_j}");
    let tasks = list_j["tasks"].as_array().expect("tasks array");
    assert!(
        tasks
            .iter()
            .any(|t| t["task_id"].as_str() == Some(task_id.as_str())),
        "list should include {task_id}: {list_j}"
    );

    let (st_get, get_j) =
        get_json(app, &format!("/tasks/{task_id}"), Some(auth.as_str()), &[]).await;
    assert_eq!(st_get, StatusCode::OK, "get task: {get_j}");
    assert_eq!(get_j["task_id"].as_str(), Some(task_id.as_str()));

    let (st_prog, prog_j) = get_json(
        app,
        &format!("/tasks/{task_id}/progress?session_id={session_id}"),
        Some(auth.as_str()),
        &[],
    )
    .await;
    assert_eq!(st_prog, StatusCode::OK, "task progress: {prog_j}");
    assert!(
        prog_j["progress_events"].is_array(),
        "progress_events: {prog_j}"
    );

    let edge_reg = axum::http::Request::builder()
        .method("POST")
        .uri("/agents/edge")
        .header("authorization", auth.as_str())
        .header("content-type", "application/json")
        .header("x-astra-edge-id", "matrix-e2e-edge")
        .body(axum::body::Body::from(
            json!({
                "edge_agent_id": edge_agent_id,
                "hostname": "matrix-e2e-host",
                "capabilities": { "tools": ["read_file"] }
            })
            .to_string(),
        ))
        .expect("edge register body");
    let edge_resp = app.clone().oneshot(edge_reg).await.expect("edge reg");
    assert_eq!(edge_resp.status(), StatusCode::OK, "edge register status");

    let (st_claim, claim_j) = post_json_with_headers(
        app,
        &format!("/tasks/{task_id}/lease/claim"),
        Some(auth.as_str()),
        &[("x-astra-edge-id", "matrix-e2e-edge")],
        json!({ "edge_agent_id": edge_agent_id, "ttl_sec": 300 }),
    )
    .await;
    assert_eq!(st_claim, StatusCode::OK, "lease claim: {claim_j}");

    let lease_row = sqlx::query(
        "SELECT user_id, holder_agent_id, holder_edge_id FROM task_leases WHERE task_id = ?",
    )
    .bind(&task_id)
    .fetch_optional(pool)
    .await
    .expect("task_leases select");
    let lease_row = lease_row.expect("task_leases row after claim");
    assert_eq!(
        lease_row.try_get::<String, _>("user_id").ok().as_deref(),
        Some(user_id.as_str())
    );
    assert_eq!(
        lease_row
            .try_get::<String, _>("holder_agent_id")
            .ok()
            .as_deref(),
        Some(edge_agent_id.as_str())
    );
    assert_eq!(
        lease_row
            .try_get::<Option<String>, _>("holder_edge_id")
            .ok()
            .flatten()
            .as_deref(),
        Some("matrix-e2e-edge")
    );

    let (st_lease, lease_j) = get_json(
        app,
        &format!("/tasks/{task_id}/lease"),
        Some(auth.as_str()),
        &[],
    )
    .await;
    assert_eq!(st_lease, StatusCode::OK, "get lease: {lease_j}");
    assert!(
        !lease_j["lease"].is_null() || lease_j.get("holder_agent_id").is_some(),
        "expected lease payload, got {lease_j}"
    );

    let (st_renew, renew_j) = post_json_with_headers(
        app,
        &format!("/tasks/{task_id}/lease/renew"),
        Some(auth.as_str()),
        &[("x-astra-edge-id", "matrix-e2e-edge")],
        json!({ "edge_agent_id": edge_agent_id, "ttl_sec": 600 }),
    )
    .await;
    assert_eq!(st_renew, StatusCode::OK, "lease renew: {renew_j}");

    let (st_rel, rel_j) = post_json_with_headers(
        app,
        &format!("/tasks/{task_id}/lease/release"),
        Some(auth.as_str()),
        &[("x-astra-edge-id", "matrix-e2e-edge")],
        json!({ "edge_agent_id": edge_agent_id }),
    )
    .await;
    assert_eq!(st_rel, StatusCode::OK, "lease release: {rel_j}");
    assert_eq!(rel_j["released"], true);

    let (st_put, put_j) = put_json(
        app,
        &format!("/tasks/{task_id}/status"),
        Some(auth.as_str()),
        json!({ "status": "in_progress" }),
    )
    .await;
    assert_eq!(st_put, StatusCode::OK, "task status: {put_j}");

    let st_row = sqlx::query("SELECT status FROM agent_tasks WHERE task_id = ?")
        .bind(&task_id)
        .fetch_one(pool)
        .await
        .expect("agent_tasks after status");
    assert_eq!(
        st_row.try_get::<String, _>("status").ok().as_deref(),
        Some("in_progress")
    );

    cleanup_task_rows(pool, &user_id, &task_id).await;
    let _ = sqlx::query("DELETE FROM edge_agent_registry WHERE user_id = ? AND edge_agent_id = ?")
        .bind(&user_id)
        .bind(&edge_agent_id)
        .execute(pool)
        .await;

    ctx.pool.close().await;
}

pub async fn run_chat_run_pause_resume_http() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let auth = &b.auth_header;
    let app = &ctx.app;
    let session_id = ctx.session_id.clone();

    let (st_chat, chat_j) = post_json(
        app,
        "/chat",
        Some(auth.as_str()),
        json!({
            "message": "matrix e2e background run",
            "session_id": session_id,
            "max_candidates": 1
        }),
    )
    .await;
    assert_eq!(st_chat, StatusCode::OK, "POST /chat: {chat_j}");
    let run_id = chat_j["run_id"].as_str().expect("run_id").to_string();
    assert!(!run_id.is_empty(), "run_id from ChatResponse");

    // Pause immediately: `create_run` leaves the run `Running` until the background loop finishes.
    let (st_pause, pause_j) = post_empty(
        app,
        &format!("/chat/runs/{run_id}/pause"),
        Some(auth.as_str()),
    )
    .await;
    assert_eq!(st_pause, StatusCode::OK, "pause run: {pause_j}");
    assert_eq!(pause_j["run_id"].as_str(), Some(run_id.as_str()));

    let (st_get, get_j) = get_json(
        app,
        &format!("/chat/runs/{run_id}"),
        Some(auth.as_str()),
        &[],
    )
    .await;
    assert_eq!(st_get, StatusCode::OK, "get run after pause: {get_j}");
    assert_eq!(
        get_j["status"].as_str(),
        Some("paused"),
        "get run after pause: {get_j}"
    );

    let (st_resume, resume_j) = post_empty(
        app,
        &format!("/chat/runs/{run_id}/resume"),
        Some(auth.as_str()),
    )
    .await;
    assert_eq!(st_resume, StatusCode::OK, "resume run: {resume_j}");

    let (st_get2, get2_j) = get_json(
        app,
        &format!("/chat/runs/{run_id}"),
        Some(auth.as_str()),
        &[],
    )
    .await;
    assert_eq!(st_get2, StatusCode::OK, "get run after resume: {get2_j}");
    let st2 = get2_j["status"].as_str().unwrap_or("");
    assert!(
        st2 == "running" || st2 == "completed",
        "expected running or completed after resume, got {st2}: {get2_j}"
    );

    ctx.pool.close().await;
}
