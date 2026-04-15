//! Trusted MOI E2E: external user JWT is accepted and propagated to Astra-owned data surfaces.
use axum::http::StatusCode;
use serde_json::json;
use sqlx::Row;

use super::harness::{bootstrap_trusted_moi, cleanup_session_data, get_json, post_json};

pub async fn run_trusted_moi_user_system_integration() {
    let b = bootstrap_trusted_moi().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = b.auth_header.as_str();
    let pool = &ctx.pool;

    let (st_me, me) = get_json(app, "/auth/me", Some(auth), &[]).await;
    assert_eq!(st_me, StatusCode::OK, "trusted_moi /auth/me: {me}");
    assert_eq!(me["user_id"].as_str(), Some(ctx.user_id.as_str()));
    assert_eq!(me["username"].as_str(), Some(ctx.username.as_str()));

    let (st_register, j_register) = post_json(
        app,
        "/auth/register",
        None,
        json!({
            "username": "should-not-work",
            "email": "should-not-work@e2e.test",
            "password": "ignored"
        }),
    )
    .await;
    assert_eq!(
        st_register,
        StatusCode::FORBIDDEN,
        "trusted_moi register should be disabled: {j_register}"
    );
    assert_eq!(
        j_register["detail"].as_str(),
        Some("Local auth endpoints are disabled in trusted_moi mode")
    );

    let (st_login, j_login) = post_json(
        app,
        "/auth/login",
        None,
        json!({ "username": "x", "password": "y" }),
    )
    .await;
    assert_eq!(
        st_login,
        StatusCode::FORBIDDEN,
        "trusted_moi login should be disabled: {j_login}"
    );

    let (st_refresh, j_refresh) = post_json(
        app,
        "/auth/refresh",
        None,
        json!({ "refresh_token": "not-used" }),
    )
    .await;
    assert_eq!(
        st_refresh,
        StatusCode::FORBIDDEN,
        "trusted_moi refresh should be disabled: {j_refresh}"
    );

    let (st_sess, sess) = post_json(
        app,
        "/sessions",
        Some(auth),
        json!({ "title": "trusted moi user-system e2e", "metadata": { "suite": "trusted_moi" } }),
    )
    .await;
    assert_eq!(
        st_sess,
        StatusCode::CREATED,
        "trusted_moi create session: {sess}"
    );
    let extra_session_id = sess["session_id"]
        .as_str()
        .expect("trusted_moi session_id")
        .to_string();

    let row = sqlx::query("SELECT user_id FROM agent_sessions WHERE session_id = ?")
        .bind(&extra_session_id)
        .fetch_optional(pool)
        .await
        .expect("select trusted_moi session owner");
    let row = row.expect("trusted_moi created session row");
    assert_eq!(
        row.try_get::<String, _>("user_id").ok().as_deref(),
        Some(ctx.user_id.as_str()),
        "session owner should be external user id from trusted_moi token"
    );

    let prior_calls = ctx.memoria.calls.lock().await.len();
    let (st_mem, j_mem) = post_json(
        app,
        "/memory/store",
        Some(auth),
        json!({
            "content": "trusted moi isolation probe",
            "memory_type": "semantic",
            "user_id": "spoofed-victim",
            "session_id": "spoofed-session"
        }),
    )
    .await;
    assert_eq!(st_mem, StatusCode::OK, "trusted_moi memory store: {j_mem}");
    let calls = ctx.memoria.calls.lock().await;
    assert!(
        calls.len() > prior_calls,
        "trusted_moi memory proxy should forward at least one call"
    );
    let (_, body) = calls.last().expect("trusted_moi last memoria call");
    assert_eq!(
        body["user_id"].as_str(),
        Some(ctx.user_id.as_str()),
        "spoofed user_id must be replaced by trusted_moi principal"
    );
    assert_eq!(
        body["session_id"].as_str(),
        Some(ctx.user_id.as_str()),
        "spoofed session_id must be replaced by trusted_moi principal"
    );

    cleanup_session_data(pool, &ctx.session_id).await;
    cleanup_session_data(pool, &extra_session_id).await;
    ctx.pool.close().await;
}
