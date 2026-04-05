//! Journeys that do not belong in the monolithic product matrix: session cancel/delete, `/chat/stream`.
use axum::http::StatusCode;
use serde_json::json;
use sqlx::Row;

use super::harness::{
    bootstrap, delete_no_content, get_json, post_empty, post_json_collect_body_text,
    sse_first_data_json_with_type,
};

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
    assert_eq!(
        st,
        StatusCode::OK,
        "chat/stream status, body prefix: {}",
        &text[..text.len().min(500)]
    );
    let info = sse_first_data_json_with_type(&text, "session_info").unwrap_or_else(|| {
        panic!(
            "expected session_info SSE event in: {}",
            &text[..text.len().min(2000)]
        )
    });
    assert_eq!(info["session_id"].as_str(), Some(session_id.as_str()));
    let run_id = info["run_id"].as_str().expect("run_id in session_info");
    assert!(!run_id.is_empty(), "non-empty run_id");

    ctx.pool.close().await;
}
