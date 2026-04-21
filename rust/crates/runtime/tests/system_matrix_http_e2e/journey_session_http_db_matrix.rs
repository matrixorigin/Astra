//! Session `GET`/`PUT` HTTP responses aligned with `agent_sessions` (`title`, `user_id`, `status`).

use axum::http::StatusCode;
use serde_json::json;
use sqlx::Row;

use super::harness::{bootstrap, get_json, put_json};

pub async fn run_session_http_matches_agent_sessions_row() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let auth = &b.auth_header;

    let session_id = &ctx.session_id;
    let path = format!("/sessions/{session_id}");

    let row = sqlx::query("SELECT title, user_id, status FROM agent_sessions WHERE session_id = ?")
        .bind(session_id.as_str())
        .fetch_optional(&ctx.pool)
        .await
        .expect("agent_sessions SELECT bootstrap")
        .expect("session row should exist after bootstrap POST /sessions");

    let db_title = row.try_get::<String, _>("title").ok();
    let db_uid = row.get::<String, _>("user_id");
    let db_status = row.get::<String, _>("status");

    let (st_get, got) = get_json(&ctx.app, &path, Some(auth), &[]).await;
    assert_eq!(st_get, StatusCode::OK);
    assert_eq!(
        got["session_id"].as_str(),
        Some(session_id.as_str()),
        "{got}"
    );
    assert_eq!(
        got["title"].as_str(),
        db_title.as_deref(),
        "HTTP title vs DB"
    );
    assert_eq!(got["status"].as_str(), Some(db_status.as_str()));

    let new_title = format!("matrix_db_title_{}", ctx.suffix);
    let (st_put, put_j) = put_json(
        &ctx.app,
        &path,
        Some(auth),
        json!({ "title": new_title.clone() }),
    )
    .await;
    assert_eq!(st_put, StatusCode::OK);
    assert_eq!(put_j["title"].as_str(), Some(new_title.as_str()));

    let title_sql: String =
        sqlx::query_scalar("SELECT title FROM agent_sessions WHERE session_id = ? AND user_id = ?")
            .bind(session_id.as_str())
            .bind(&ctx.user_id)
            .fetch_one(&ctx.pool)
            .await
            .expect("title after PUT");
    assert_eq!(title_sql, new_title);
    assert_eq!(db_uid, ctx.user_id);

    b.ctx.pool.close().await;
}
