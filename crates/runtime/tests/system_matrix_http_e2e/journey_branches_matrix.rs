//! `POST /branches/cost-estimate` — JWT auth + numeric response shape (no DDL branch creation).
use axum::http::StatusCode;
use serde_json::json;

use super::harness::{bootstrap, post_json};

pub async fn run_branches_cost_estimate_http() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let auth = &b.auth_header;
    let app = &ctx.app;

    let (st_no_auth, no_auth_j) = post_json(
        app,
        "/branches/cost-estimate",
        None,
        json!({
            "operation": "merge",
            "model": "mock-estimate",
        }),
    )
    .await;
    assert_eq!(
        st_no_auth,
        StatusCode::UNAUTHORIZED,
        "cost-estimate withoutAuthorization: {no_auth_j}"
    );

    let (st_ok, body) = post_json(
        app,
        "/branches/cost-estimate",
        Some(auth.as_str()),
        json!({
            "operation": "merge",
            "model": format!("mock-{}", ctx.suffix),
            "session_count": 2,
            "budget_remaining": 100.0
        }),
    )
    .await;
    assert_eq!(st_ok, StatusCode::OK, "cost-estimate: {body}");
    assert_eq!(body["operation"].as_str(), Some("merge"));
    let want_model = format!("mock-{}", ctx.suffix);
    assert_eq!(body["model"].as_str(), Some(want_model.as_str()));
    let est_tokens = body["estimated_tokens"].as_i64().expect("estimated_tokens");
    assert_eq!(est_tokens, 2000, "session_count 2 × 1000: {body}");
    let est_cost = body["estimated_cost"].as_f64().expect("estimated_cost");
    assert!(
        (est_cost - 0.02).abs() < 1e-9,
        "expected cost 0.02, got {est_cost}: {body}"
    );
    assert_eq!(
        body["exceeds_budget"].as_bool(),
        Some(false),
        "within budget: {body}"
    );

    let (st_over, over_j) = post_json(
        app,
        "/branches/cost-estimate",
        Some(auth.as_str()),
        json!({
            "operation": "diff",
            "model": "mock-tiny-budget",
            "session_count": 10,
            "budget_remaining": 0.0001
        }),
    )
    .await;
    assert_eq!(st_over, StatusCode::OK, "cost-estimate exceeds: {over_j}");
    assert_eq!(
        over_j["exceeds_budget"].as_bool(),
        Some(true),
        "should exceed tiny budget: {over_j}"
    );

    ctx.pool.close().await;
}
