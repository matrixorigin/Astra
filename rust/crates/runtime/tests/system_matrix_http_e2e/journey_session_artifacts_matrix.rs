//! Session artifact HTTP E2E: authenticated list/get routes align with `session_artifacts`,
//! including kind filtering, session scoping, and cross-user isolation.

use axum::http::StatusCode;
use axum::{body, body::Body, http::Request};
use futures_util::StreamExt;
use serde_json::json;
use sqlx::Row;
use tower::util::ServiceExt;
use uuid::Uuid;

use astra_services::session_restore::COMPOSITE_SNAPSHOT_INDEX_ARTIFACT_KIND;
use astra_services::session_workspace::WORKSPACE_METADATA_ARTIFACT_KIND;

use super::harness::{
    E2E_PASSWORD, E2eAuthMode, bootstrap, bootstrap_trusted_moi, cleanup_session_data, get_json,
    post_json,
};

async fn collect_full_sse_stream(
    app: &axum::Router,
    req: Request<Body>,
    timeout_secs: u64,
) -> (StatusCode, String) {
    let resp = app.clone().oneshot(req).await.expect("oneshot");
    let status = resp.status();
    let mut stream = resp.into_body().into_data_stream();
    let mut acc = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    while let Ok(Some(chunk)) = tokio::time::timeout_at(deadline, stream.next()).await {
        let chunk = chunk.expect("body chunk");
        acc.extend_from_slice(&chunk);
    }
    (status, String::from_utf8_lossy(&acc).to_string())
}

async fn stream_chat_full(
    app: &axum::Router,
    auth: &str,
    payload: serde_json::Value,
) -> (StatusCode, String) {
    let test_secret = std::env::var("ASTRA_BRIDGE_TEST_SECRET").expect("bridge test secret");
    let req = Request::builder()
        .method("POST")
        .uri("/chat/stream")
        .header("authorization", auth)
        .header("content-type", "application/json")
        .header("x-mo-bridge-test-secret", &test_secret)
        .body(Body::from(payload.to_string()))
        .expect("stream request");
    collect_full_sse_stream(app, req, 30).await
}

async fn get_bytes(
    app: &axum::Router,
    path: &str,
    auth: Option<&str>,
    extra_headers: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut req = Request::builder().method("GET").uri(path);
    if let Some(t) = auth {
        req = req.header("authorization", t);
    }
    for (k, v) in extra_headers {
        req = req.header(*k, *v);
    }
    let req = req.body(Body::empty()).expect("request");
    let response = app.clone().oneshot(req).await.expect("oneshot");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .expect("body");
    (status, headers, bytes.to_vec())
}

async fn wait_for_artifact_count(
    pool: &sqlx::MySqlPool,
    session_id: &str,
    artifact_kind: &str,
    min_count: i64,
    timeout: std::time::Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM session_artifacts WHERE session_id = ? AND artifact_kind = ?",
        )
        .bind(session_id)
        .bind(artifact_kind)
        .fetch_one(pool)
        .await
        .unwrap_or(0);
        if n >= min_count {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "timeout ({timeout:?}) waiting for >= {min_count} artifacts of kind={artifact_kind} for session_id={session_id} (got {n})"
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

pub async fn run_session_artifact_http_matches_session_artifacts_rows() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let auth = &b.auth_header;

    let workspace_artifact_id = Uuid::new_v4().to_string();
    let composite_artifact_id = Uuid::new_v4().to_string();

    sqlx::query(
        "INSERT INTO session_artifacts \
         (artifact_id, session_id, user_id, artifact_kind, source, turn, round, content_json, metadata, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NOW())",
    )
    .bind(&workspace_artifact_id)
    .bind(&ctx.session_id)
    .bind(&ctx.user_id)
    .bind(WORKSPACE_METADATA_ARTIFACT_KIND)
    .bind("workspace_metadata")
    .bind(7_i32)
    .bind(0_i32)
    .bind(
        json!({
            "session_id": ctx.session_id,
            "status": "active",
            "model": "gpt-5.4",
            "turn_count": 7
        })
        .to_string(),
    )
    .bind(json!({ "status": "active", "model": "gpt-5.4" }).to_string())
    .execute(&ctx.pool)
    .await
    .expect("insert workspace artifact");

    sqlx::query(
        "INSERT INTO session_artifacts \
         (artifact_id, session_id, user_id, artifact_kind, source, turn, round, content_json, metadata, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NOW())",
    )
    .bind(&composite_artifact_id)
    .bind(&ctx.session_id)
    .bind(&ctx.user_id)
    .bind(COMPOSITE_SNAPSHOT_INDEX_ARTIFACT_KIND)
    .bind("composite_snapshot_index")
    .bind(7_i32)
    .bind(1_i32)
    .bind(
        json!({
            "snapshots": [{
                "snapshot_id": format!("{}-snapshot", ctx.suffix),
                "session_id": ctx.session_id,
                "turn": 7,
                "created_at": "2026-09-09T10:00:00Z",
                "version": 1,
                "label": "http-e2e",
                "refs": []
            }]
        })
        .to_string(),
    )
    .bind(json!({ "snapshot_count": 1, "latest_version": 1 }).to_string())
    .execute(&ctx.pool)
    .await
    .expect("insert composite artifact");

    let artifact_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM session_artifacts WHERE session_id = ?")
            .bind(&ctx.session_id)
            .fetch_one(&ctx.pool)
            .await
            .expect("artifact count");
    assert_eq!(artifact_count, 2);

    let list_path = format!(
        "/sessions/{}/artifacts?artifact_kind={}&limit=1",
        ctx.session_id, WORKSPACE_METADATA_ARTIFACT_KIND
    );
    let (st_list, list_j) = get_json(&ctx.app, &list_path, Some(auth), &[]).await;
    assert_eq!(st_list, StatusCode::OK, "artifact list: {list_j}");
    assert_eq!(list_j["session_id"].as_str(), Some(ctx.session_id.as_str()));
    assert_eq!(list_j["limit"].as_u64(), Some(1));
    let artifacts = list_j["artifacts"].as_array().expect("artifacts array");
    assert_eq!(
        artifacts.len(),
        1,
        "artifact kind filter should narrow results"
    );
    assert_eq!(
        artifacts[0]["artifact_id"].as_str(),
        Some(workspace_artifact_id.as_str())
    );
    assert_eq!(
        artifacts[0]["artifact_kind"].as_str(),
        Some(WORKSPACE_METADATA_ARTIFACT_KIND)
    );
    assert_eq!(artifacts[0]["turn"].as_u64(), Some(7));
    assert_eq!(artifacts[0]["content"]["model"].as_str(), Some("gpt-5.4"));

    let get_path = format!(
        "/sessions/{}/artifacts/{}",
        ctx.session_id, workspace_artifact_id
    );
    let (st_get, get_j) = get_json(&ctx.app, &get_path, Some(auth), &[]).await;
    assert_eq!(st_get, StatusCode::OK, "artifact get: {get_j}");
    assert_eq!(
        get_j["artifact_id"].as_str(),
        Some(workspace_artifact_id.as_str())
    );
    assert_eq!(get_j["user_id"].as_str(), Some(ctx.user_id.as_str()));
    assert_eq!(get_j["metadata"]["status"].as_str(), Some("active"));

    let (st_create_other_session, other_session_j) = post_json(
        &ctx.app,
        "/sessions",
        Some(auth),
        json!({ "title": "artifact wrong-session probe" }),
    )
    .await;
    assert_eq!(
        st_create_other_session,
        StatusCode::CREATED,
        "create second session: {other_session_j}"
    );
    let other_session_id = other_session_j["session_id"]
        .as_str()
        .expect("other session_id")
        .to_string();
    let wrong_session_path = format!(
        "/sessions/{}/artifacts/{}",
        other_session_id, workspace_artifact_id
    );
    let (st_wrong_session, wrong_session_j) =
        get_json(&ctx.app, &wrong_session_path, Some(auth), &[]).await;
    assert_eq!(
        st_wrong_session,
        StatusCode::NOT_FOUND,
        "artifact id must not be readable through a different session path: {wrong_session_j}"
    );

    let (other_app, other_auth) = match b.auth_mode {
        E2eAuthMode::LocalJwt => {
            let b_suffix = Uuid::new_v4().simple().to_string();
            let short = &b_suffix[..12];
            let b_username = format!("art_iso_{short}");
            let b_email = format!("art_iso_{short}@e2e.test");

            let (st_reg, reg_b) = post_json(
                &ctx.app,
                "/auth/register",
                None,
                json!({
                    "username": b_username,
                    "email": b_email,
                    "password": E2E_PASSWORD,
                    "display_name": "Artifact isolation B"
                }),
            )
            .await;
            assert_eq!(st_reg, StatusCode::CREATED, "register B: {reg_b}");

            let (st_login, login_j) = post_json(
                &ctx.app,
                "/auth/login",
                None,
                json!({ "username": b_username, "password": E2E_PASSWORD }),
            )
            .await;
            assert_eq!(st_login, StatusCode::OK, "login B: {login_j}");
            let access_b = login_j["access_token"].as_str().expect("B access_token");
            (ctx.app.clone(), format!("Bearer {access_b}"))
        }
        E2eAuthMode::TrustedMoi => {
            let other = bootstrap_trusted_moi().await;
            (other.ctx.app.clone(), other.auth_header)
        }
    };

    let (st_foreign_list, foreign_list_j) =
        get_json(&other_app, &list_path, Some(&other_auth), &[]).await;
    assert_eq!(
        st_foreign_list,
        StatusCode::NOT_FOUND,
        "foreign user must not list another user's session artifacts: {foreign_list_j}"
    );
    let (st_foreign_get, foreign_get_j) =
        get_json(&other_app, &get_path, Some(&other_auth), &[]).await;
    assert_eq!(
        st_foreign_get,
        StatusCode::NOT_FOUND,
        "foreign user must not get another user's session artifact: {foreign_get_j}"
    );

    let db_row = sqlx::query(
        "SELECT artifact_kind, source, CAST(metadata AS CHAR) AS metadata_json \
         FROM session_artifacts WHERE artifact_id = ? AND session_id = ?",
    )
    .bind(&workspace_artifact_id)
    .bind(&ctx.session_id)
    .fetch_one(&ctx.pool)
    .await
    .expect("workspace artifact row");
    assert_eq!(
        db_row.try_get::<String, _>("artifact_kind").ok().as_deref(),
        Some(WORKSPACE_METADATA_ARTIFACT_KIND)
    );
    assert_eq!(
        db_row
            .try_get::<Option<String>, _>("source")
            .ok()
            .flatten()
            .as_deref(),
        Some("workspace_metadata")
    );

    ctx.pool.close().await;
}

pub async fn run_published_session_artifact_round_trip() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;

    let (st_sess, sess) = post_json(
        app,
        "/sessions",
        Some(auth.as_str()),
        json!({ "title": "artifact publish roundtrip", "metadata": { "suite": "artifact_roundtrip" } }),
    )
    .await;
    assert_eq!(st_sess, StatusCode::CREATED, "create session: {sess}");
    let session_id = sess["session_id"].as_str().expect("session_id").to_string();

    let before_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM session_artifacts WHERE session_id = ? AND artifact_kind = 'llm_capture'",
    )
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("llm capture count before stream");
    assert_eq!(
        before_count, 0,
        "fresh session should not have llm_capture artifacts"
    );

    let payload = json!({
        "message": "publish llm capture and read it back",
        "session_id": &session_id,
        "context": {
            "test_llm_rounds": [{ "full_text": "Artifact publish verified." }]
        }
    });
    let (status, body) = stream_chat_full(app, auth, payload).await;
    assert_eq!(status, StatusCode::OK, "chat/stream: {body}");
    assert!(
        body.contains("Artifact publish verified."),
        "SSE body should include the model text response: {body}"
    );

    wait_for_artifact_count(
        pool,
        &session_id,
        "llm_capture",
        1,
        std::time::Duration::from_secs(15),
    )
    .await;

    let row = sqlx::query(
        "SELECT artifact_id, source, turn, round, content_json, CAST(metadata AS CHAR) AS metadata_json \
         FROM session_artifacts WHERE session_id = ? AND artifact_kind = 'llm_capture' \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("latest llm_capture row");
    let artifact_id: String = row.try_get("artifact_id").expect("artifact_id");
    let source: Option<String> = row.try_get("source").expect("source");
    let turn: Option<i32> = row.try_get("turn").expect("turn");
    let round: Option<i32> = row.try_get("round").expect("round");
    let content_json: String = row.try_get("content_json").expect("content_json");
    let content: serde_json::Value =
        serde_json::from_str(&content_json).expect("parse llm_capture content");
    assert_eq!(source.as_deref(), Some("server_loop_host"));
    assert!(turn.unwrap_or_default() >= 1);
    assert!(round.unwrap_or_default() >= 0);
    assert_eq!(
        content["response"]["full_text"].as_str(),
        Some("Artifact publish verified.")
    );

    let list_path = format!("/sessions/{session_id}/artifacts?artifact_kind=llm_capture&limit=10");
    let (st_list, list_j) = get_json(app, &list_path, Some(auth), &[]).await;
    assert_eq!(
        st_list,
        StatusCode::OK,
        "artifact list after publish: {list_j}"
    );
    let artifacts = list_j["artifacts"].as_array().expect("artifacts array");
    assert!(
        artifacts
            .iter()
            .any(|artifact| artifact["artifact_id"].as_str() == Some(artifact_id.as_str())),
        "list should contain the published llm_capture artifact: {list_j}"
    );

    let get_path = format!("/sessions/{session_id}/artifacts/{artifact_id}");
    let (st_get, get_j) = get_json(app, &get_path, Some(auth), &[]).await;
    assert_eq!(
        st_get,
        StatusCode::OK,
        "artifact get after publish: {get_j}"
    );
    assert_eq!(get_j["artifact_kind"].as_str(), Some("llm_capture"));
    assert_eq!(get_j["source"].as_str(), Some("server_loop_host"));
    assert_eq!(
        get_j["content"]["response"]["full_text"].as_str(),
        Some("Artifact publish verified.")
    );
    assert_eq!(
        get_j["metadata"]["outcome"].as_str(),
        Some("success"),
        "live artifact read-back should preserve runtime-published metadata"
    );

    let (st_wrong_session, wrong_session_j) = get_json(
        app,
        &format!("/sessions/{}/artifacts/{}", ctx.session_id, artifact_id),
        Some(auth),
        &[],
    )
    .await;
    assert_eq!(
        st_wrong_session,
        StatusCode::NOT_FOUND,
        "published artifact should still be session-scoped over HTTP: {wrong_session_j}"
    );

    let _ = sqlx::query("DELETE FROM session_artifacts WHERE session_id = ?")
        .bind(&session_id)
        .execute(pool)
        .await;
    cleanup_session_data(pool, &session_id).await;
    ctx.pool.close().await;
}

pub async fn run_session_artifact_latest_and_download_routes() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;

    let (st_sess, sess) = post_json(
        app,
        "/sessions",
        Some(auth.as_str()),
        json!({ "title": "artifact latest download", "metadata": { "suite": "artifact_latest_download" } }),
    )
    .await;
    assert_eq!(st_sess, StatusCode::CREATED, "create session: {sess}");
    let session_id = sess["session_id"].as_str().expect("session_id").to_string();

    let payload = json!({
        "message": "publish llm capture for latest and download routes",
        "session_id": &session_id,
        "context": {
            "test_llm_rounds": [{ "full_text": "Artifact download verified." }]
        }
    });
    let (status, body) = stream_chat_full(app, auth, payload).await;
    assert_eq!(status, StatusCode::OK, "chat/stream: {body}");
    assert!(
        body.contains("Artifact download verified."),
        "SSE body should include the model text response: {body}"
    );

    wait_for_artifact_count(
        pool,
        &session_id,
        "llm_capture",
        1,
        std::time::Duration::from_secs(15),
    )
    .await;

    let row = sqlx::query(
        "SELECT artifact_id \
         FROM session_artifacts WHERE session_id = ? AND artifact_kind = 'llm_capture' \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("latest llm_capture row");
    let artifact_id: String = row.try_get("artifact_id").expect("artifact_id");

    let latest_path = format!("/sessions/{session_id}/artifacts/latest/llm_capture");
    let (st_latest, latest_j) = get_json(app, &latest_path, Some(auth), &[]).await;
    assert_eq!(st_latest, StatusCode::OK, "artifact latest: {latest_j}");
    assert_eq!(latest_j["artifact_id"].as_str(), Some(artifact_id.as_str()));
    assert_eq!(latest_j["artifact_kind"].as_str(), Some("llm_capture"));
    assert_eq!(
        latest_j["content"]["response"]["full_text"].as_str(),
        Some("Artifact download verified.")
    );

    let download_path = format!("/sessions/{session_id}/artifacts/{artifact_id}/download");
    let (st_download, download_headers, download_body) =
        get_bytes(app, &download_path, Some(auth), &[]).await;
    assert_eq!(st_download, StatusCode::OK, "artifact download");
    assert_eq!(
        download_headers
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/json")
    );
    let content_disposition = download_headers
        .get("content-disposition")
        .and_then(|value| value.to_str().ok())
        .expect("content-disposition");
    assert!(
        content_disposition.contains("attachment;"),
        "download should be an attachment: {content_disposition}"
    );
    assert!(
        content_disposition.contains(artifact_id.as_str()),
        "download filename should include the artifact id: {content_disposition}"
    );
    let download_j: serde_json::Value =
        serde_json::from_slice(&download_body).expect("download json");
    assert_eq!(
        download_j["artifact_id"].as_str(),
        Some(artifact_id.as_str())
    );
    assert_eq!(download_j["artifact_kind"].as_str(), Some("llm_capture"));
    assert_eq!(
        download_j["content"]["response"]["full_text"].as_str(),
        Some("Artifact download verified.")
    );

    let (st_other_session, other_session_j) = post_json(
        app,
        "/sessions",
        Some(auth.as_str()),
        json!({ "title": "artifact latest wrong session" }),
    )
    .await;
    assert_eq!(
        st_other_session,
        StatusCode::CREATED,
        "create second session: {other_session_j}"
    );
    let other_session_id = other_session_j["session_id"]
        .as_str()
        .expect("other session_id")
        .to_string();

    let (st_wrong_latest, wrong_latest_j) = get_json(
        app,
        &format!("/sessions/{other_session_id}/artifacts/latest/llm_capture"),
        Some(auth),
        &[],
    )
    .await;
    assert_eq!(
        st_wrong_latest,
        StatusCode::NOT_FOUND,
        "latest artifact should stay session-scoped: {wrong_latest_j}"
    );

    let (st_wrong_download, _headers, wrong_download_body) = get_bytes(
        app,
        &format!("/sessions/{other_session_id}/artifacts/{artifact_id}/download"),
        Some(auth),
        &[],
    )
    .await;
    assert_eq!(
        st_wrong_download,
        StatusCode::NOT_FOUND,
        "artifact download should stay session-scoped: {}",
        String::from_utf8_lossy(&wrong_download_body)
    );

    let (other_app, other_auth) = match b.auth_mode {
        E2eAuthMode::LocalJwt => {
            let b_suffix = Uuid::new_v4().simple().to_string();
            let short = &b_suffix[..12];
            let b_username = format!("art_dl_{short}");
            let b_email = format!("art_dl_{short}@e2e.test");

            let (st_reg, reg_b) = post_json(
                &ctx.app,
                "/auth/register",
                None,
                json!({
                    "username": b_username,
                    "email": b_email,
                    "password": E2E_PASSWORD,
                    "display_name": "Artifact download isolation B"
                }),
            )
            .await;
            assert_eq!(st_reg, StatusCode::CREATED, "register B: {reg_b}");

            let (st_login, login_j) = post_json(
                &ctx.app,
                "/auth/login",
                None,
                json!({ "username": b_username, "password": E2E_PASSWORD }),
            )
            .await;
            assert_eq!(st_login, StatusCode::OK, "login B: {login_j}");
            let access_b = login_j["access_token"].as_str().expect("B access_token");
            (ctx.app.clone(), format!("Bearer {access_b}"))
        }
        E2eAuthMode::TrustedMoi => {
            let other = bootstrap_trusted_moi().await;
            (other.ctx.app.clone(), other.auth_header)
        }
    };

    let (st_foreign_latest, foreign_latest_j) =
        get_json(&other_app, &latest_path, Some(&other_auth), &[]).await;
    assert_eq!(
        st_foreign_latest,
        StatusCode::NOT_FOUND,
        "foreign user must not read another user's latest artifact: {foreign_latest_j}"
    );

    let (st_foreign_download, _headers, foreign_download_body) =
        get_bytes(&other_app, &download_path, Some(&other_auth), &[]).await;
    assert_eq!(
        st_foreign_download,
        StatusCode::NOT_FOUND,
        "foreign user must not download another user's artifact: {}",
        String::from_utf8_lossy(&foreign_download_body)
    );

    let _ = sqlx::query("DELETE FROM session_artifacts WHERE session_id IN (?, ?)")
        .bind(&session_id)
        .bind(&other_session_id)
        .execute(pool)
        .await;
    cleanup_session_data(pool, &session_id).await;
    cleanup_session_data(pool, &other_session_id).await;
    ctx.pool.close().await;
}
