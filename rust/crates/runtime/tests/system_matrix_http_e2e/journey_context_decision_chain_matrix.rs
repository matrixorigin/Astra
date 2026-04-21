//! Event → context snapshot → decision: HTTP + `ctx_snapshots` / `ctx_decision_audits` rows.

use axum::http::StatusCode;
use serde_json::json;
use sqlx::Row;

use super::harness::{bootstrap, get_json, post_json};

pub async fn run_context_decision_chain_db() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let auth = &b.auth_header;
    let session_id = ctx.session_id.as_str();

    let (st_ev, ev_j) = post_json(
        &ctx.app,
        "/events",
        Some(auth),
        json!({
            "session_id": session_id,
            "event_type": "e2e_ctx_chain",
            "content": "anchor for context/decision",
            "metadata": { "suite": "matrix_ctx_chain" }
        }),
    )
    .await;
    assert_eq!(st_ev, StatusCode::CREATED, "create event: {ev_j}");
    let event_id = ev_j["event_id"].as_str().expect("event_id");

    let (st_ctx, ctx_j) = post_json(
        &ctx.app,
        "/context",
        Some(auth),
        json!({
            "session_id": session_id,
            "event_id": event_id,
            "context_data": { "probe": "matrix", "n": ctx.suffix.as_str() }
        }),
    )
    .await;
    assert_eq!(st_ctx, StatusCode::CREATED, "context: {ctx_j}");
    let capture_id = ctx_j["context_capture_id"]
        .as_str()
        .expect("context_capture_id");

    let snap =
        sqlx::query("SELECT session_id, event_id FROM ctx_snapshots WHERE context_capture_id = ?")
            .bind(capture_id)
            .fetch_optional(&ctx.pool)
            .await
            .expect("ctx_snapshots")
            .expect("ctx_snapshots row");
    assert_eq!(snap.get::<String, _>("session_id"), session_id);
    assert_eq!(snap.get::<String, _>("event_id"), event_id);

    let (st_dec, dec_j) = post_json(
        &ctx.app,
        "/decisions",
        Some(auth),
        json!({
            "session_id": session_id,
            "event_id": event_id,
            "context_capture_id": capture_id,
            "decision_type": "e2e_matrix_ctx_chain_decision",
            "decision_output": { "picked": "a" },
            "model_params": { "temperature": 0.2 }
        }),
    )
    .await;
    assert_eq!(st_dec, StatusCode::CREATED, "decision: {dec_j}");
    let decision_id = dec_j["decision_id"].as_str().expect("decision_id");

    let dec_row = sqlx::query(
        "SELECT session_id, decision_type, context_capture_id FROM ctx_decision_audits WHERE decision_id = ?",
    )
    .bind(decision_id)
    .fetch_optional(&ctx.pool)
    .await
    .expect("ctx_decision_audits")
    .expect("decision row");
    assert_eq!(dec_row.get::<String, _>("session_id"), session_id);
    assert_eq!(
        dec_row.get::<String, _>("decision_type"),
        "e2e_matrix_ctx_chain_decision"
    );
    assert_eq!(dec_row.get::<String, _>("context_capture_id"), capture_id);

    let (st_gctx, got_ctx) =
        get_json(&ctx.app, &format!("/context/{capture_id}"), Some(auth), &[]).await;
    assert_eq!(st_gctx, StatusCode::OK, "GET context: {got_ctx}");

    let (st_gdec, got_dec) = get_json(
        &ctx.app,
        &format!("/decisions/{decision_id}"),
        Some(auth),
        &[],
    )
    .await;
    assert_eq!(st_gdec, StatusCode::OK, "GET decision: {got_dec}");
    assert_eq!(
        got_dec["decision_type"].as_str(),
        Some("e2e_matrix_ctx_chain_decision")
    );

    b.ctx.pool.close().await;
}
