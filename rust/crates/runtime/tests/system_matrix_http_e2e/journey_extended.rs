//! Journeys that do not belong in the monolithic product matrix: session cancel/delete, `/chat/stream`,
//! auth/session negative paths (replaces stub `auth_contract` / `session_contract` coverage),
//! models admin CRUD with DB checks (replaces `model_crud_contract`).
use axum::http::StatusCode;
use serde_json::json;
use sqlx::Row;

use super::harness::{
    E2E_PASSWORD, bootstrap, delete_no_content, get_json, grant_astra_admin_role, post_empty,
    post_json, post_json_collect_body_text, put_json, sse_first_data_json_with_type,
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

    let st_del =
        delete_no_content(app, &format!("/sessions/{session_id}"), Some(auth.as_str())).await;
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

/// Unauthenticated `/sessions`, duplicate register, and bad password login (real DB + services).
/// Memory proxy must overwrite spoofed `user_id` / `session_id` with the authenticated user (real
/// `MemoriaForwarder` + JWT). Replaces stub `memory_contract` security coverage.
pub async fn run_memory_proxy_user_isolation() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let user_id = ctx.user_id.as_str();

    let (st_unauth, j_unauth) = post_json(
        app,
        "/memory/store",
        None,
        json!({ "content": "x", "memory_type": "semantic" }),
    )
    .await;
    assert_eq!(
        st_unauth,
        StatusCode::UNAUTHORIZED,
        "memory without auth: {j_unauth}"
    );

    let before = ctx.memoria.calls.lock().await.len();
    let (st_spoof, j_spoof) = post_json(
        app,
        "/memory/store",
        Some(auth.as_str()),
        json!({
            "content": "spoof probe",
            "memory_type": "semantic",
            "user_id": "victim-user-id",
            "session_id": "victim-session-id"
        }),
    )
    .await;
    assert_eq!(st_spoof, StatusCode::OK, "memory store: {j_spoof}");

    let calls = ctx.memoria.calls.lock().await;
    assert!(
        calls.len() > before,
        "memoria forwarder should record /memory/store"
    );
    let (_, body) = calls.last().expect("last memoria call");
    assert_eq!(
        body["user_id"].as_str(),
        Some(user_id),
        "spoofed user_id must be replaced: {body}"
    );
    assert_eq!(
        body["session_id"].as_str(),
        Some(user_id),
        "spoofed session_id must be replaced with authenticated user_id: {body}"
    );

    ctx.pool.close().await;
}

pub async fn run_auth_and_session_negative_paths() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;

    let (st_sess, j_sess) = get_json(app, "/sessions", None, &[]).await;
    assert_eq!(
        st_sess,
        StatusCode::UNAUTHORIZED,
        "GET /sessions without auth: {j_sess}"
    );

    let dup_email = format!("dup_{}@e2e.test", ctx.suffix);
    let (st_dup, j_dup) = post_json(
        app,
        "/auth/register",
        None,
        json!({
            "username": ctx.username,
            "email": dup_email,
            "password": "DifferentPass-1",
            "display_name": "duplicate probe"
        }),
    )
    .await;
    assert_eq!(
        st_dup,
        StatusCode::BAD_REQUEST,
        "duplicate username register: {j_dup}"
    );
    assert_eq!(
        j_dup["detail"].as_str(),
        Some("Username already exists"),
        "duplicate username detail: {j_dup}"
    );

    let (st_bad_login, j_bad) = post_json(
        app,
        "/auth/login",
        None,
        json!({ "username": ctx.username, "password": "wrong-password-not-real" }),
    )
    .await;
    assert_eq!(
        st_bad_login,
        StatusCode::UNAUTHORIZED,
        "bad password login: {j_bad}"
    );
    assert_eq!(
        j_bad["detail"].as_str(),
        Some("Invalid username or password"),
        "bad login detail: {j_bad}"
    );

    // Sanity: bearer still works after negative calls
    let (st_ok, j_ok) = post_json(
        app,
        "/auth/login",
        None,
        json!({ "username": ctx.username, "password": E2E_PASSWORD }),
    )
    .await;
    assert_eq!(st_ok, StatusCode::OK, "login still ok: {j_ok}");

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
    let (st, text) =
        post_json_collect_body_text(app, "/chat/stream", Some(auth.as_str()), &body, 512 * 1024)
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

/// Admin model CRUD with `infra_llm_models` assertions. Uses `provider: mock` so connectivity check
/// skips the network (`validate_connectivity` short-circuit).
pub async fn run_models_admin_crud_with_db() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    grant_astra_admin_role(&ctx.pool, &ctx.user_id).await;

    let app = &ctx.app;
    let auth = b.auth_header.as_str();
    let pool = &ctx.pool;
    let model_name = format!("e2e_mtx_mdl_{}", ctx.suffix);

    let (st_c, j_c) = post_json(
        app,
        "/models",
        Some(auth),
        json!({
            "name": model_name,
            "provider": "mock",
            "api_key": "e2e-key-not-used",
            "input_modalities": ["text"],
            "output_modalities": ["text"],
            "supported_parameters": ["temperature"],
            "tags": ["e2e_matrix"]
        }),
    )
    .await;
    assert_eq!(st_c, StatusCode::CREATED, "create model: {j_c}");
    assert_eq!(j_c["name"].as_str(), Some(model_name.as_str()));

    let row = sqlx::query("SELECT model_name, provider FROM infra_llm_models WHERE model_name = ?")
        .bind(&model_name)
        .fetch_optional(pool)
        .await
        .expect("select infra_llm_models");
    let row = row.expect("model row after create");
    assert_eq!(
        row.try_get::<String, _>("model_name").ok().as_deref(),
        Some(model_name.as_str())
    );
    assert_eq!(
        row.try_get::<String, _>("provider").ok().as_deref(),
        Some("mock")
    );

    let (st_u, j_u) = put_json(
        app,
        &format!("/models/{model_name}"),
        Some(auth),
        json!({ "description": "e2e matrix updated", "is_active": true }),
    )
    .await;
    assert_eq!(st_u, StatusCode::OK, "update model: {j_u}");
    assert_eq!(j_u["description"].as_str(), Some("e2e matrix updated"));

    let desc_row = sqlx::query("SELECT description FROM infra_llm_models WHERE model_name = ?")
        .bind(&model_name)
        .fetch_one(pool)
        .await
        .expect("description after update");
    assert_eq!(
        desc_row
            .try_get::<Option<String>, _>("description")
            .ok()
            .flatten()
            .as_deref(),
        Some("e2e matrix updated")
    );

    let st_d = delete_no_content(app, &format!("/models/{model_name}"), Some(auth)).await;
    assert_eq!(st_d, StatusCode::NO_CONTENT, "delete model");

    let gone = sqlx::query("SELECT 1 FROM infra_llm_models WHERE model_name = ?")
        .bind(&model_name)
        .fetch_optional(pool)
        .await
        .expect("select after delete");
    assert!(gone.is_none(), "model row should be deleted");

    let (st_g, _) = get_json(app, &format!("/models/{model_name}"), Some(auth), &[]).await;
    assert_eq!(st_g, StatusCode::NOT_FOUND, "get after delete");

    ctx.pool.close().await;
}
