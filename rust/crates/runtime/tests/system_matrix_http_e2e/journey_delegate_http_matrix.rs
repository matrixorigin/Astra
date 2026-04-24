//! Delegation HTTP boundaries: list delegations on a chat run + `POST .../delegate` rejected at
//! validation (400) without executing `ServerSubRunExecutor`.
use axum::http::StatusCode;
use serde_json::json;

use super::harness::{bootstrap, get_json, post_json};

pub async fn run_delegate_http_boundaries() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let auth = &b.auth_header;
    let app = &ctx.app;
    let session_id = ctx.session_id.clone();
    let user_id = ctx.user_id.clone();
    let mock_model = format!("mock-{}", ctx.suffix);

    let (st_chat, chat_j) = post_json(
        app,
        "/chat",
        Some(auth.as_str()),
        json!({
            "message": "matrix e2e delegation boundary",
            "session_id": session_id,
            "model": mock_model,
            "execution_budget": {
                "initial_turns": 1,
                "hard_turn_limit": 1
            }
        }),
    )
    .await;
    assert_eq!(st_chat, StatusCode::OK, "POST /chat: {chat_j}");
    let run_id = chat_j["run_id"].as_str().expect("run_id").to_string();

    let (st_list, list_j) = get_json(
        app,
        &format!("/chat/runs/{run_id}/delegations"),
        Some(auth.as_str()),
        &[],
    )
    .await;
    assert_eq!(st_list, StatusCode::OK, "list delegations: {list_j}");
    assert_eq!(
        list_j["parent_run_id"].as_str(),
        Some(run_id.as_str()),
        "{list_j}"
    );
    let subs = list_j["sub_run_ids"].as_array().expect("sub_run_ids array");
    assert!(
        subs.is_empty() || subs.iter().all(|v| v.is_string()),
        "sub_run_ids should be strings: {list_j}"
    );

    // Valid `DelegationRequest` JSON; validation fails before execute (default source agent id
    // `"main"` is not registered in `AgentProfileRegistry`).
    let (st_del, del_j) = post_json(
        app,
        &format!("/chat/runs/{run_id}/delegate"),
        Some(auth.as_str()),
        json!({
            "delegation_id": format!("del-e2e-{}", ctx.suffix),
            "parent_run_id": run_id,
            "task": "noop delegation probe",
            "pattern": {
                "pattern": "fan_out",
                "agent_ids": ["coder"],
                "aggregation": "all_results",
                "timeout_sec": 60
            },
            "user_id": user_id,
            "depth": 0,
            "context": {}
        }),
    )
    .await;
    assert_eq!(
        st_del,
        StatusCode::BAD_REQUEST,
        "delegate should fail validation: {del_j}"
    );

    ctx.pool.close().await;
}
