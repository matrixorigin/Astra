//! Extra HTTP journeys: platform snapshot, session cancel/delete, task list + lease read/renew,
//! evaluation POST contract (501 until implemented), `POST /chat/stream` SSE smoke.
use axum::http::StatusCode;
use serde_json::json;
use sqlx::Row;
use tower::util::ServiceExt;

use super::harness::{
    bootstrap, cleanup_task_rows, delete_no_content, get_json, post_empty, post_empty_with_headers,
    post_json, post_json_collect_body_text, post_json_with_headers, sse_first_data_json_with_type,
};

pub async fn run_platform_snapshot_smoke() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;

    let (st, snap) = get_json(app, "/platform/snapshot", Some(auth.as_str()), &[]).await;
    assert_eq!(st, StatusCode::OK, "platform snapshot: {snap}");
    assert!(
        snap["health"]["status"].is_string(),
        "snapshot.health.status: {snap}"
    );
    assert!(snap["agents"].is_object(), "snapshot.agents: {snap}");
    assert!(snap["sessions"].is_object(), "snapshot.sessions: {snap}");
    assert!(snap["events"].is_object(), "snapshot.events: {snap}");
    assert!(snap["timestamp"].is_string(), "snapshot.timestamp: {snap}");

    ctx.pool.close().await;
}

pub async fn run_session_cancel_then_delete() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let session_id = ctx.session_id.clone();

    let (st_can, can_j) = post_empty(
        app,
        &format!("/sessions/{session_id}/cancel"),
        Some(auth.as_str()),
    )
    .await;
    assert_eq!(st_can, StatusCode::OK, "cancel session: {can_j}");
    assert_eq!(
        can_j["status"].as_str(),
        Some("cancelled"),
        "cancel response: {can_j}"
    );

    let row = sqlx::query("SELECT status FROM agent_sessions WHERE session_id = ?")
        .bind(&session_id)
        .fetch_optional(pool)
        .await
        .expect("select session after cancel");
    let row = row.expect("session row after cancel");
    assert_eq!(
        row.try_get::<String, _>("status").ok().as_deref(),
        Some("cancelled")
    );

    let st_del = delete_no_content(
        app,
        &format!("/sessions/{session_id}"),
        Some(auth.as_str()),
    )
    .await;
    assert_eq!(st_del, StatusCode::NO_CONTENT, "delete session");

    let (st_get, _) = get_json(
        app,
        &format!("/sessions/{session_id}"),
        Some(auth.as_str()),
        &[],
    )
    .await;
    assert_eq!(st_get, StatusCode::NOT_FOUND, "get after delete");

    ctx.pool.close().await;
}

pub async fn run_tasks_list_get_lease_read_renew() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let auth = &b.auth_header;
    let app = &ctx.app;
    let pool = &ctx.pool;
    let user_id = ctx.user_id.clone();
    let session_id = ctx.session_id.clone();
    let edge_agent_id = ctx.edge_agent_id.clone();

    let (st_task, task_j) = post_json(
        app,
        "/tasks",
        Some(auth.as_str()),
        json!({
            "title": "matrix e2e list+renew",
            "description": "extended lease",
            "session_id": session_id,
        }),
    )
    .await;
    assert_eq!(st_task, StatusCode::CREATED, "create task: {task_j}");
    let task_id = task_j["task_id"].as_str().expect("task_id").to_string();

    let (st_list, list_j) = get_json(app, "/tasks", Some(auth.as_str()), &[]).await;
    assert_eq!(st_list, StatusCode::OK, "list tasks: {list_j}");
    let tasks = list_j["tasks"].as_array().expect("tasks array");
    assert!(
        tasks.iter().any(|t| t["task_id"].as_str() == Some(task_id.as_str())),
        "list should include {task_id}: {list_j}"
    );

    let (st_get, get_j) = get_json(
        app,
        &format!("/tasks/{task_id}"),
        Some(auth.as_str()),
        &[],
    )
    .await;
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
    assert!(prog_j["progress_events"].is_array(), "progress_events: {prog_j}");

    let edge_reg = axum::http::Request::builder()
        .method("POST")
        .uri("/agents/edge")
        .header("authorization", auth.as_str())
        .header("content-type", "application/json")
        .header("x-mo-edge-id", "matrix-e2e-edge-renew")
        .body(axum::body::Body::from(
            json!({
                "edge_agent_id": edge_agent_id,
                "hostname": "matrix-e2e-host-renew",
                "capabilities": { "tools": ["read_file"] }
            })
            .to_string(),
        ))
        .expect("edge register body");
    let edge_resp = app.clone().oneshot(edge_reg).await.expect("edge reg");
    assert_eq!(edge_resp.status(), StatusCode::OK, "edge register");

    let (st_claim, _) = post_json_with_headers(
        app,
        &format!("/tasks/{task_id}/lease/claim"),
        Some(auth.as_str()),
        &[("x-mo-edge-id", "matrix-e2e-edge-renew")],
        json!({ "edge_agent_id": edge_agent_id, "ttl_sec": 300 }),
    )
    .await;
    assert_eq!(st_claim, StatusCode::OK, "lease claim");

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
        &[("x-mo-edge-id", "matrix-e2e-edge-renew")],
        json!({ "edge_agent_id": edge_agent_id, "ttl_sec": 600 }),
    )
    .await;
    assert_eq!(st_renew, StatusCode::OK, "lease renew: {renew_j}");

    let (st_rel, rel_j) = post_json_with_headers(
        app,
        &format!("/tasks/{task_id}/lease/release"),
        Some(auth.as_str()),
        &[("x-mo-edge-id", "matrix-e2e-edge-renew")],
        json!({ "edge_agent_id": edge_agent_id }),
    )
    .await;
    assert_eq!(st_rel, StatusCode::OK, "lease release: {rel_j}");

    cleanup_task_rows(pool, &user_id, &task_id).await;
    let _ = sqlx::query("DELETE FROM edge_agent_registry WHERE user_id = ? AND edge_agent_id = ?")
        .bind(&user_id)
        .bind(&edge_agent_id)
        .execute(pool)
        .await;

    ctx.pool.close().await;
}

/// `DatabaseEvaluationService` still returns 501 for gate validate, drift pipeline, and closed loop.
/// This test locks the contract so implementations can flip assertions to 200 when ready.
pub async fn run_evaluation_post_writes_not_implemented_yet() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let user_id = ctx.user_id.as_str();
    let xuid = &[("x-user-id", user_id)];

    let (st_g, g_j) = post_json_with_headers(
        app,
        "/evaluation/gate/validate",
        None,
        xuid,
        json!({
            "change_type": "prompt",
            "change_id": "matrix-e2e-gate",
            "change_content": { "note": "e2e" }
        }),
    )
    .await;
    assert_eq!(st_g, StatusCode::NOT_IMPLEMENTED, "gate validate: {g_j}");

    let (st_d, d_j) = post_empty_with_headers(app, "/evaluation/drift/run", None, xuid).await;
    assert_eq!(st_d, StatusCode::NOT_IMPLEMENTED, "drift run: {d_j}");

    let (st_l, l_j) = post_empty_with_headers(
        app,
        "/evaluation/loop?days=7&dry_run=true",
        None,
        xuid,
    )
    .await;
    assert_eq!(st_l, StatusCode::NOT_IMPLEMENTED, "closed loop: {l_j}");

    ctx.pool.close().await;
}

pub async fn run_chat_stream_session_info_smoke() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let session_id = ctx.session_id.clone();

    let body = json!({
        "message": "matrix e2e stream smoke",
        "session_id": session_id,
        "max_candidates": 1
    });
    let (st, text) = post_json_collect_body_text(
        app,
        "/chat/stream",
        Some(auth.as_str()),
        &body,
        512 * 1024,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "chat/stream status, body prefix: {}", &text[..text.len().min(500)]);
    let info = sse_first_data_json_with_type(&text, "session_info")
        .unwrap_or_else(|| panic!("expected session_info SSE event in: {}", &text[..text.len().min(2000)]));
    assert_eq!(info["session_id"].as_str(), Some(session_id.as_str()));
    let run_id = info["run_id"].as_str().expect("run_id in session_info");
    assert!(!run_id.is_empty(), "non-empty run_id");

    ctx.pool.close().await;
}
