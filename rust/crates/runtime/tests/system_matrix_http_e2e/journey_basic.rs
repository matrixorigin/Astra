//! Basic auth + session lifecycle slice (HTTP + `agent_sessions` rows).
use axum::http::StatusCode;
use sqlx::Row;

use super::harness::{bootstrap, cleanup_session_data, get_json, post_empty, post_json, put_json};

/// Registers (via [`super::harness::bootstrap`]), lists/gets/updates session, close + resume, DB checks.
pub async fn run_basic_auth_session_lifecycle() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let app = &ctx.app;
    let session_id = ctx.session_id.clone();

    let (st_list_s, list_s) = get_json(app, "/sessions", Some(auth.as_str()), &[]).await;
    assert_eq!(st_list_s, StatusCode::OK, "list sessions: {list_s}");
    assert!(
        list_s["sessions"].as_array().is_some_and(|a| {
            a.iter()
                .any(|s| s["session_id"].as_str() == Some(session_id.as_str()))
        }),
        "session not listed: {list_s}"
    );

    let (st_get_s, got_s) = get_json(
        app,
        &format!("/sessions/{session_id}"),
        Some(auth.as_str()),
        &[],
    )
    .await;
    assert_eq!(st_get_s, StatusCode::OK, "get session: {got_s}");

    let (st_put_s, put_s) = put_json(
        app,
        &format!("/sessions/{session_id}"),
        Some(auth.as_str()),
        serde_json::json!({ "title": "product matrix session (updated)" }),
    )
    .await;
    assert_eq!(st_put_s, StatusCode::OK, "put session: {put_s}");
    assert_eq!(
        put_s["title"].as_str(),
        Some("product matrix session (updated)")
    );

    let (st_close, closed) = post_empty(
        app,
        &format!("/sessions/{session_id}/close"),
        Some(auth.as_str()),
    )
    .await;
    assert_eq!(st_close, StatusCode::OK, "close session: {closed}");
    assert_eq!(
        closed["status"].as_str(),
        Some("closed"),
        "close response: {closed}"
    );

    let sess_status = sqlx::query("SELECT status FROM agent_sessions WHERE session_id = ?")
        .bind(&session_id)
        .fetch_one(pool)
        .await
        .expect("session status after close");
    assert_eq!(
        sess_status.try_get::<String, _>("status").ok().as_deref(),
        Some("closed"),
        "agent_sessions.status after POST .../close"
    );

    let (st_res, resm) = post_empty(
        app,
        &format!("/sessions/{session_id}/resume"),
        Some(auth.as_str()),
    )
    .await;
    assert_eq!(st_res, StatusCode::OK, "resume session: {resm}");
    assert_eq!(
        resm["status"].as_str(),
        Some("active"),
        "resume response: {resm}"
    );

    let sess_active = sqlx::query("SELECT status FROM agent_sessions WHERE session_id = ?")
        .bind(&session_id)
        .fetch_one(pool)
        .await
        .expect("session status after resume");
    assert_eq!(
        sess_active.try_get::<String, _>("status").ok().as_deref(),
        Some("active"),
        "agent_sessions.status after POST .../resume"
    );

    let (st_act, act) = get_json(
        app,
        &format!("/sessions/{session_id}/activity"),
        Some(auth.as_str()),
        &[],
    )
    .await;
    assert_eq!(st_act, StatusCode::OK, "session activity: {act}");

    cleanup_session_data(pool, &session_id).await;

    let (st_out, out_j) = post_json(
        app,
        "/auth/logout",
        Some(auth.as_str()),
        serde_json::json!({ "refresh_token": b.refresh_token.as_str() }),
    )
    .await;
    assert_eq!(st_out, StatusCode::OK, "logout: {out_j}");

    ctx.pool.close().await;
}
