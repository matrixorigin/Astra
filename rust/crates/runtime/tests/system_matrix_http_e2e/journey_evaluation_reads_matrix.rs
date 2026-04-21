//! Evaluation **read** routes that use `x-user-id` (evaluation service) plus a few JWT-only probes.
//!
//! Seeds a minimal `/agents` row because `trust-report` and observability routes require `agent_id`.

use axum::http::StatusCode;
use serde_json::json;

use super::harness::{bootstrap, get_json, post_json};

pub async fn run_evaluation_read_http_smoke() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let auth = &b.auth_header;
    let uid = ctx.user_id.as_str();
    let xuid = &[("x-user-id", uid)];

    let (st_ag, ag_j) = post_json(
        &ctx.app,
        "/agents",
        Some(auth),
        json!({
            "name": format!("eval_smoke_agent_{}", ctx.suffix),
            "agent_config": { "suite": "matrix_eval_smoke" },
            "data_source": {
                "type": "matrixone",
                "database": ctx.matrixone_database.clone()
            }
        }),
    )
    .await;
    assert_eq!(st_ag, StatusCode::CREATED, "seed agent for eval reads: {ag_j}");
    let agent_id = ag_j["agent_id"].as_str().expect("agent_id");

    let endpoints: &[&str] = &[
        "/evaluation/gates?limit=10",
        "/evaluation/calibration?days=7",
        "/evaluation/sessions/scores?limit=10&min_score=0",
        "/evaluation/quality/trend?days=7",
        "/evaluation/slo/dashboard?period_days=7",
        "/evaluation/memory-health",
        "/evaluation/memory-metrics",
        "/evaluation/drift",
    ];

    for path in endpoints {
        let (st, j) = get_json(&ctx.app, path, None, xuid).await;
        assert_eq!(st, StatusCode::OK, "{path}: {j}");
        assert!(
            j.as_object().is_some(),
            "{path} should return JSON object: {j}"
        );
    }

    let trust_path =
        format!("/evaluation/trust-report?agent_id={agent_id}&days=7");
    let (st_trust, trust_j) = get_json(&ctx.app, &trust_path, None, xuid).await;
    assert_eq!(st_trust, StatusCode::OK, "trust-report: {trust_j}");

    let slo_hist = format!("/evaluation/slo/{agent_id}/history?days=7");
    let (st_slo_h, slo_h_j) = get_json(&ctx.app, &slo_hist, None, xuid).await;
    assert_eq!(st_slo_h, StatusCode::OK, "slo history: {slo_h_j}");

    let obs_path =
        format!("/evaluation/observability/metrics?agent_id={agent_id}&days=7");
    let (st_obs, obs_j) = get_json(&ctx.app, &obs_path, None, xuid).await;
    assert_eq!(st_obs, StatusCode::OK, "observability: {obs_j}");

    let (st_learn_h, learn_h) = get_json(&ctx.app, "/api/v1/learning/health", None, &[]).await;
    assert_eq!(st_learn_h, StatusCode::OK, "learning health: {learn_h}");

    let (st_sig, sig) = get_json(&ctx.app, "/api/v1/learning/signals", Some(auth), &[]).await;
    assert_eq!(st_sig, StatusCode::OK, "learning signals: {sig}");

    b.ctx.pool.close().await;
}
