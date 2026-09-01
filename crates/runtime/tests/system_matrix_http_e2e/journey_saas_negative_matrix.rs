//! SaaS negative-path and boundary journeys: auth failures, resource governance denials,
//! and concurrent-cap recovery.
//!
//! Maps to `docs/testing/saas-test-plan.md` §5.1–§5.3 (P/N/B coverage).

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::StreamExt;
use serde_json::{Value, json};
use sqlx::Row;
use tower::util::ServiceExt;
use uuid::Uuid;

use super::harness::{
    E2E_PASSWORD, bootstrap, build_e2e_access_token, get_json, grant_astra_admin_role,
    load_durable_interaction_event, maybe_tool_result_payload_from_sse, model_selection,
    post_empty, post_json, put_json, revoke_astra_admin_role, seed_pending_approval,
    seeded_model_selection,
};
use super::journey_saas_platform_matrix::{
    cleanup_resource_limits, cleanup_seeded_run, limits_payload, seed_capacity_holding_run,
};

fn parse_sse_events(raw: &str) -> Vec<Value> {
    raw.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str(data).ok())
        .collect()
}

async fn seed_resource_usage_tokens(pool: &sqlx::MySqlPool, user_id: &str, tokens: i64) {
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    sqlx::query(
        "INSERT INTO resource_usage (user_id, usage_date, tokens_consumed) \
         VALUES (?, ?, ?) \
         ON DUPLICATE KEY UPDATE tokens_consumed = VALUES(tokens_consumed)",
    )
    .bind(user_id)
    .bind(&today)
    .bind(tokens)
    .execute(pool)
    .await
    .expect("seed resource_usage tokens");
}

async fn register_login_user(app: &Router, tag: &str) -> (String, String) {
    let suffix = Uuid::new_v4().simple().to_string();
    let username = format!("saas_{tag}_{suffix}");
    let email = format!("{username}@e2e.test");

    let (st_reg, reg) = post_json(
        app,
        "/auth/register",
        None,
        json!({
            "username": username,
            "email": email,
            "password": E2E_PASSWORD,
            "display_name": format!("SaaS {tag}")
        }),
    )
    .await;
    assert_eq!(st_reg, StatusCode::CREATED, "register {tag}: {reg}");

    let auth = format!(
        "Bearer {}",
        reg["access_token"].as_str().expect("access token")
    );
    let user_id = reg["user_id"].as_str().expect("user_id").to_string();
    (auth, user_id)
}

/// Auth negative matrix: unauthenticated access, duplicate register, bad login/refresh.
pub async fn run_saas_auth_negative_paths() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;

    // N: unauthenticated protected routes → 401
    for (label, path) in [
        ("sessions list", "/sessions"),
        ("auth me", "/auth/me"),
        ("resources usage", "/resources/usage"),
        ("resources limits", "/resources/limits"),
    ] {
        let (st, body) = get_json(app, path, None, &[]).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED, "{label} without auth: {body}");
    }

    // N: malformed bearer → 401
    let (st_bad_bearer, body_bad) =
        get_json(app, "/auth/me", Some("Bearer not-a-valid-jwt"), &[]).await;
    assert_eq!(
        st_bad_bearer,
        StatusCode::UNAUTHORIZED,
        "garbage bearer: {body_bad}"
    );

    // N: duplicate username register → 400
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
        "duplicate username: {j_dup}"
    );
    assert_eq!(
        j_dup["detail"].as_str(),
        Some("Username already exists"),
        "duplicate detail: {j_dup}"
    );

    // N: wrong password login → 401
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
        "bad password: {j_bad}"
    );
    assert_eq!(
        j_bad["detail"].as_str(),
        Some("Invalid username or password"),
        "bad login detail: {j_bad}"
    );

    // N: invalid refresh token → 401
    let (st_bad_refresh, j_refresh) = post_json(
        app,
        "/auth/refresh",
        None,
        json!({ "refresh_token": "not.a.valid.refresh.jwt" }),
    )
    .await;
    assert_eq!(
        st_bad_refresh,
        StatusCode::UNAUTHORIZED,
        "bad refresh: {j_refresh}"
    );

    // P: valid login still works after negatives
    let (st_ok, j_ok) = post_json(
        app,
        "/auth/login",
        None,
        json!({ "username": ctx.username, "password": E2E_PASSWORD }),
    )
    .await;
    assert_eq!(st_ok, StatusCode::OK, "login recovery: {j_ok}");
    assert!(j_ok["access_token"].as_str().is_some());

    ctx.close().await;
}

/// Resource governance negatives: non-admin override forbidden; token cap denies run;
/// unlimited (0) allows run despite high usage.
pub async fn run_saas_resource_governance_negative_paths() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let user_id = ctx.user_id.as_str();
    let model_offering_id = ctx.model_offering_id.clone();

    cleanup_resource_limits(pool, user_id).await;
    grant_astra_admin_role(pool, user_id).await;

    // N: non-admin cannot PUT admin resource limits → 403
    revoke_astra_admin_role(pool, user_id).await;
    let (st_forbidden, forbidden_j) = put_json(
        app,
        &format!("/admin/resources/limits/{user_id}"),
        Some(auth),
        limits_payload(10, 5),
    )
    .await;
    assert_eq!(
        st_forbidden,
        StatusCode::FORBIDDEN,
        "non-admin PUT limits: {forbidden_j}"
    );
    grant_astra_admin_role(pool, user_id).await;

    // N: token daily cap exhausted → POST /chat returns 429 with readable error
    let token_cap = 500_i64;
    let (st_cap, _) = put_json(
        app,
        &format!("/admin/resources/limits/{user_id}"),
        Some(auth),
        json!({
            "max_concurrent_sessions": 5,
            "max_tokens_per_day": token_cap,
            "max_disk_bytes": 1_073_741_824u64,
            "max_concurrent_bash": 3,
            "max_sessions_per_day": 50
        }),
    )
    .await;
    assert_eq!(st_cap, StatusCode::OK, "set token cap");
    seed_resource_usage_tokens(pool, user_id, token_cap).await;

    let (st_chat, chat_j) = post_json(
        app,
        "/chat",
        Some(auth),
        json!({
            "message": "token cap probe",
            "session_id": ctx.session_id,
            "model_selection": model_selection(model_offering_id.clone()),
            "execution_budget": { "initial_turns": 1, "hard_turn_limit": 1 }
        }),
    )
    .await;
    assert_eq!(
        st_chat,
        StatusCode::TOO_MANY_REQUESTS,
        "token-exhausted chat: {chat_j}"
    );
    let err = chat_j["detail"]
        .as_str()
        .or_else(|| chat_j["error"].as_str())
        .unwrap_or("");
    assert!(
        err.to_ascii_lowercase().contains("token"),
        "readable token denial: {chat_j}"
    );

    // B/P: max_tokens_per_day=0 (unlimited) → run not failed for token cap
    cleanup_resource_limits(pool, user_id).await;
    grant_astra_admin_role(pool, user_id).await;
    let (st_unlim, _) = put_json(
        app,
        &format!("/admin/resources/limits/{user_id}"),
        Some(auth),
        json!({
            "max_concurrent_sessions": 5,
            "max_tokens_per_day": 0,
            "max_disk_bytes": 1_073_741_824u64,
            "max_concurrent_bash": 3,
            "max_sessions_per_day": 50
        }),
    )
    .await;
    assert_eq!(st_unlim, StatusCode::OK, "set unlimited tokens");
    seed_resource_usage_tokens(pool, user_id, 9_999_999).await;

    let (st_chat2, chat2) = post_json(
        app,
        "/chat",
        Some(auth),
        json!({
            "message": "unlimited token probe",
            "session_id": ctx.session_id,
            "model_selection": model_selection(model_offering_id.clone()),
            "execution_budget": { "initial_turns": 1, "hard_turn_limit": 1 }
        }),
    )
    .await;
    assert_eq!(st_chat2, StatusCode::OK, "unlimited chat: {chat2}");
    assert!(chat2.get("run_id").is_some(), "run_id: {chat2}");

    cleanup_resource_limits(pool, user_id).await;
    ctx.close().await;
}

/// Concurrent session cap: deny then admin recovery allows new chat.
pub async fn run_saas_resource_concurrent_cap_recovery() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let user_id = ctx.user_id.as_str();
    let model_offering_id = ctx.model_offering_id.clone();

    cleanup_resource_limits(pool, user_id).await;
    grant_astra_admin_role(pool, user_id).await;

    let (st_cap, _) = put_json(
        app,
        &format!("/admin/resources/limits/{user_id}"),
        Some(auth),
        limits_payload(50, 1),
    )
    .await;
    assert_eq!(st_cap, StatusCode::OK);

    // N: concurrent cap hit — seed a paused run (mock /chat completes before pause).
    let holding_run = seed_capacity_holding_run(pool, user_id, ctx.session_id.as_str()).await;

    let (st_denied, denied) = post_json(
        app,
        "/chat",
        Some(auth),
        json!({
            "message": "concurrent cap deny",
            "model_selection": model_selection(model_offering_id.clone()),
            "execution_budget": { "initial_turns": 1, "hard_turn_limit": 1 }
        }),
    )
    .await;
    assert_eq!(
        st_denied,
        StatusCode::TOO_MANY_REQUESTS,
        "concurrent cap deny: {denied}"
    );

    // P: admin raises cap → chat allowed
    let (st_raise, _) = put_json(
        app,
        &format!("/admin/resources/limits/{user_id}"),
        Some(auth),
        limits_payload(50, 5),
    )
    .await;
    assert_eq!(st_raise, StatusCode::OK, "raise concurrent cap");

    let (st_ok, ok_j) = post_json(
        app,
        "/chat",
        Some(auth),
        json!({
            "message": "concurrent cap recovery",
            "model_selection": model_selection(model_offering_id.clone()),
            "execution_budget": { "initial_turns": 1, "hard_turn_limit": 1 }
        }),
    )
    .await;
    assert_eq!(st_ok, StatusCode::OK, "after cap raise: {ok_j}");
    assert!(ok_j.get("run_id").is_some());

    cleanup_seeded_run(pool, &holding_run).await;
    cleanup_resource_limits(pool, user_id).await;
    ctx.close().await;
}

/// Logout revokes refresh; expired access JWT is rejected.
pub async fn run_saas_auth_logout_and_expired_token() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;

    // N: expired access token → 401
    let expired = build_e2e_access_token(&ctx.user_id, &ctx.username, 1_000_000_000);
    let expired_auth = format!("Bearer {expired}");
    let (st_exp, exp_j) = get_json(app, "/auth/me", Some(&expired_auth), &[]).await;
    assert_eq!(
        st_exp,
        StatusCode::UNAUTHORIZED,
        "expired access token: {exp_j}"
    );

    // P: logout succeeds
    let (st_logout, logout_j) = post_json(
        app,
        "/auth/logout",
        None,
        json!({ "refresh_token": b.refresh_token }),
    )
    .await;
    assert_eq!(st_logout, StatusCode::OK, "logout: {logout_j}");

    // N: refresh token revoked after logout → 401
    let (st_refresh, refresh_j) = post_json(
        app,
        "/auth/refresh",
        None,
        json!({ "refresh_token": b.refresh_token }),
    )
    .await;
    assert_eq!(
        st_refresh,
        StatusCode::UNAUTHORIZED,
        "refresh after logout: {refresh_j}"
    );

    // P: re-login works
    let (st_login, login_j) = post_json(
        app,
        "/auth/login",
        None,
        json!({ "username": ctx.username, "password": E2E_PASSWORD }),
    )
    .await;
    assert_eq!(st_login, StatusCode::OK, "re-login: {login_j}");
    let new_auth = format!(
        "Bearer {}",
        login_j["access_token"].as_str().expect("access")
    );
    let (st_me, me_j) = get_json(app, "/auth/me", Some(&new_auth), &[]).await;
    assert_eq!(st_me, StatusCode::OK, "me after re-login: {me_j}");

    ctx.close().await;
}

/// Admin override for bash/disk limits is readable via GET /resources/limits (+ DB).
/// Note: HTTP enforcement for bash/disk is Edge-side; this covers SaaS config contract.
pub async fn run_saas_resource_limits_extended_fields() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let user_id = ctx.user_id.as_str();

    cleanup_resource_limits(pool, user_id).await;
    grant_astra_admin_role(pool, user_id).await;

    let custom = json!({
        "max_concurrent_sessions": 4,
        "max_tokens_per_day": 1_000_000,
        "max_disk_bytes": 4096u64,
        "max_concurrent_bash": 1,
        "max_sessions_per_day": 25
    });
    let (st_put, put_j) = put_json(
        app,
        &format!("/admin/resources/limits/{user_id}"),
        Some(auth),
        custom,
    )
    .await;
    assert_eq!(st_put, StatusCode::OK, "PUT extended limits: {put_j}");

    let (st_lim, lim_j) = get_json(app, "/resources/limits", Some(auth), &[]).await;
    assert_eq!(st_lim, StatusCode::OK, "GET limits: {lim_j}");
    assert_eq!(lim_j["limits"]["max_disk_bytes"].as_u64(), Some(4096));
    assert_eq!(lim_j["limits"]["max_concurrent_bash"].as_u64(), Some(1));
    assert_eq!(lim_j["limits"]["max_sessions_per_day"].as_u64(), Some(25));

    let row = sqlx::query(
        "SELECT max_disk_bytes, max_concurrent_bash, max_sessions_per_day \
         FROM resource_limits WHERE user_id = ?",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .expect("resource_limits row");
    assert_eq!(row.try_get::<i64, _>("max_disk_bytes").ok(), Some(4096));
    assert_eq!(row.try_get::<i32, _>("max_concurrent_bash").ok(), Some(1));
    assert_eq!(row.try_get::<i32, _>("max_sessions_per_day").ok(), Some(25));

    cleanup_resource_limits(pool, user_id).await;
    ctx.close().await;
}

/// Valid `POST /tools/result` consumed by the same server-owned `/chat/stream`
/// admission (positive callback path).
pub async fn run_saas_edge_tool_result_success_path() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let tool_output = "saas edge tool result ok";

    let payload = json!({
        "agent_id": "saas-edge-callback-agent",
        "session_id": ctx.session_id,
        "edge_executor_id": ctx.edge_agent_id,
        "workspace_binding": {
            "kind": "edge_workspace",
            "display_name": "system-matrix-edge",
            "root": "/tmp/astra-system-matrix-edge",
            "authority": "read_write"
        },
        "executor_binding": {
            "kind": "edge_agent",
            "executor_id": ctx.edge_agent_id,
            "display_name": "system-matrix-edge",
            "transport": "edge_ledger",
            "status": "online"
        },
        "model_selection": seeded_model_selection(ctx),
        "message": "read saas probe file",
        "context": {
            "edge_profile": {
                "cwd": "/tmp/astra-system-matrix-edge",
                "edge_agent_id": ctx.edge_agent_id,
                "hostname": "system-matrix-edge"
            },
            "edge_tools": [{
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "read a file",
                    "parameters": {
                        "type": "object",
                        "properties": { "path": { "type": "string" } },
                        "required": ["path"]
                    }
                }
            }],
            "test_llm_rounds": [
                {
                    "tool_calls": [{
                        "id": "tc-saas-tool-ok",
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "arguments": "{\"path\":\"saas-probe.txt\"}"
                        }
                    }]
                },
                { "full_text": "Done after tool result." }
            ]
        }
    });

    let req = Request::builder()
        .method("POST")
        .uri("/chat/stream")
        .header("authorization", auth.as_str())
        .header("content-type", "application/json")
        .body(Body::from(payload.to_string()))
        .expect("chat/stream request");

    let response = app.clone().oneshot(req).await.expect("chat/stream");
    assert_eq!(response.status(), StatusCode::OK, "chat/stream status");

    let mut stream = response.into_body().into_data_stream();
    let mut acc = Vec::new();
    let mut posted_result = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("sse chunk");
        acc.extend_from_slice(&chunk);
        let s = String::from_utf8_lossy(&acc);
        if !posted_result
            && let Some(payload) = maybe_tool_result_payload_from_sse(
                s.as_ref(),
                "tc-saas-tool-ok",
                &ctx.edge_agent_id,
                "completed",
                tool_output,
                0,
            )
        {
            let (st_tool, tool_j) =
                post_json(app, "/tools/result", Some(auth.as_str()), payload).await;
            assert_eq!(st_tool, StatusCode::OK, "valid /tools/result: {tool_j}");
            posted_result = true;
        }
    }
    assert!(
        posted_result,
        "server stream never emitted tool_request: {}",
        String::from_utf8_lossy(&acc)
    );

    let full = String::from_utf8_lossy(&acc).into_owned();
    let events = parse_sse_events(&full);
    let completes: Vec<_> = events
        .iter()
        .filter(|e| e["type"].as_str() == Some("turn_complete"))
        .collect();
    assert_eq!(completes.len(), 1, "one server-owned terminal: {full}");
    assert_eq!(
        completes[0]["continuation_owner"], "server",
        "callback must be consumed by the same server-owned stream: {full}"
    );
    assert_eq!(
        completes[0].get("tool_calls_count").and_then(Value::as_u64),
        Some(1),
        "SaaS callback terminal must account for one tool call: {full}"
    );
    assert_eq!(
        completes[0]
            .get("tools_used")
            .and_then(Value::as_array)
            .map(|tools| tools.iter().filter_map(Value::as_str).collect::<Vec<_>>()),
        Some(vec!["read_file"]),
        "SaaS callback terminal must report the normalized tool list: {full}"
    );
    assert_eq!(
        completes[0].get("llm_rounds").and_then(Value::as_u64),
        Some(2),
        "SaaS callback terminal must include the initial tool round and final model round: {full}"
    );
    assert!(
        events.iter().any(|event| {
            event["type"].as_str() == Some("text_delta")
                && event["content"].as_str() == Some("Done after tool result.")
        }),
        "server stream must reach the final model round after callback: {full}"
    );

    ctx.close().await;
}

/// Memoria proxy degradation: memory routes fail; chat main path stays available.
pub async fn run_saas_memoria_proxy_degradation() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let model_offering_id = ctx.model_offering_id.clone();

    ctx.memoria.set_fail_forward(true);

    // N: memory store fails when Memoria is down
    let (st_mem, mem_j) = post_json(
        app,
        "/memory/store",
        Some(auth),
        json!({
            "content": "degradation probe",
            "memory_type": "semantic"
        }),
    )
    .await;
    assert!(
        st_mem.is_server_error(),
        "memory store should fail when memoria down: {st_mem} {mem_j}"
    );

    let (st_ret, ret_j) = post_json(
        app,
        "/memory/retrieve",
        Some(auth),
        json!({ "query": "degradation probe", "limit": 3 }),
    )
    .await;
    assert!(
        st_ret.is_server_error(),
        "memory retrieve should fail when memoria down: {st_ret} {ret_j}"
    );

    // P: chat main path remains available (memory is best-effort / degraded)
    let (st_chat, chat_j) = post_json(
        app,
        "/chat",
        Some(auth),
        json!({
            "message": "chat while memoria down",
            "session_id": ctx.session_id,
            "model_selection": model_selection(model_offering_id.clone()),
            "execution_budget": { "initial_turns": 1, "hard_turn_limit": 1 }
        }),
    )
    .await;
    assert_eq!(
        st_chat,
        StatusCode::OK,
        "chat should work when memoria proxy fails: {chat_j}"
    );

    ctx.memoria.set_fail_forward(false);

    // P: memory recovers after Memoria is back
    let (st_ok, ok_j) = post_json(
        app,
        "/memory/store",
        Some(auth),
        json!({
            "content": "recovery probe",
            "memory_type": "semantic"
        }),
    )
    .await;
    assert_eq!(st_ok, StatusCode::OK, "memory store after recovery: {ok_j}");

    ctx.close().await;
}

/// Run control cross-user isolation and list scoping.
pub async fn run_saas_run_cross_user_isolation() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth_a = &b.auth_header;
    let model_offering_id = ctx.model_offering_id.clone();

    let (st_chat, chat_j) = post_json(
        app,
        "/chat",
        Some(auth_a),
        json!({
            "message": "run isolation probe",
            "session_id": ctx.session_id,
            "model_selection": model_selection(model_offering_id.clone()),
            "execution_budget": { "initial_turns": 1, "hard_turn_limit": 1 }
        }),
    )
    .await;
    assert_eq!(st_chat, StatusCode::OK, "create run: {chat_j}");
    let run_id = chat_j["run_id"].as_str().expect("run_id").to_string();

    let (auth_b, _user_b) = register_login_user(app, "run_iso_b").await;

    let (st_get, get_j) = get_json(app, &format!("/chat/runs/{run_id}"), Some(&auth_b), &[]).await;
    assert_eq!(
        st_get,
        StatusCode::NOT_FOUND,
        "B must not GET A run: {get_j}"
    );

    let (st_pause, pause_j) =
        post_empty(app, &format!("/chat/runs/{run_id}/pause"), Some(&auth_b)).await;
    assert_eq!(
        st_pause,
        StatusCode::NOT_FOUND,
        "B must not pause A run: {pause_j}"
    );

    let (st_list_b, list_b) = get_json(app, "/runs?limit=50", Some(&auth_b), &[]).await;
    assert_eq!(st_list_b, StatusCode::OK, "B list runs: {list_b}");
    let runs_b = list_b["runs"].as_array().expect("runs B");
    assert!(
        !runs_b
            .iter()
            .any(|r| r["run_id"].as_str() == Some(run_id.as_str())),
        "B list must not include A run: {list_b}"
    );

    let (st_list_a, list_a) = get_json(app, "/runs?limit=50", Some(auth_a), &[]).await;
    assert_eq!(st_list_a, StatusCode::OK, "A list runs: {list_a}");
    let runs_a = list_a["runs"].as_array().expect("runs A");
    assert!(
        runs_a
            .iter()
            .any(|r| r["run_id"].as_str() == Some(run_id.as_str())),
        "A list should include own run: {list_a}"
    );

    ctx.close().await;
}

/// Double pause on the same run returns conflict (invalid state transition).
pub async fn run_saas_run_double_pause_conflict() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let model_offering_id = ctx.model_offering_id.clone();

    let (st_chat, chat_j) = post_json(
        app,
        "/chat",
        Some(auth),
        json!({
            "message": "double pause probe",
            "session_id": ctx.session_id,
            "model_selection": model_selection(model_offering_id.clone()),
            "execution_budget": { "initial_turns": 1, "hard_turn_limit": 1 }
        }),
    )
    .await;
    assert_eq!(st_chat, StatusCode::OK, "create run: {chat_j}");
    let run_id = chat_j["run_id"].as_str().expect("run_id").to_string();

    let (st_pause1, _) = post_empty(app, &format!("/chat/runs/{run_id}/pause"), Some(auth)).await;
    assert_eq!(st_pause1, StatusCode::OK, "first pause");

    let (st_pause2, pause2_j) =
        post_empty(app, &format!("/chat/runs/{run_id}/pause"), Some(auth)).await;
    assert_eq!(
        st_pause2,
        StatusCode::CONFLICT,
        "second pause should conflict: {pause2_j}"
    );

    ctx.close().await;
}

/// GET /edges/status auth + response shape (connected edges may be empty without WS).
pub async fn run_saas_edges_status_smoke() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;

    let (st_unauth, unauth_j) = get_json(app, "/edges/status", None, &[]).await;
    assert_eq!(
        st_unauth,
        StatusCode::UNAUTHORIZED,
        "edges/status without auth: {unauth_j}"
    );

    let (st_ok, ok_j) = get_json(app, "/edges/status", Some(auth), &[]).await;
    assert_eq!(st_ok, StatusCode::OK, "edges/status: {ok_j}");
    assert!(
        ok_j["edges"].is_array(),
        "edges field must be array: {ok_j}"
    );

    ctx.close().await;
}

/// GET /service/edges/status: verifies auth gate and response shape.
///
/// Covers:
/// 1. Missing Authorization header (env var set) → 401.
/// 2. Invalid key → 401.
/// 3. Valid key → 200 with `edges` array.
pub async fn run_saas_service_edges_status_smoke() {
    let service_key = std::env::var("ASTRA_BACKEND_SERVICE_KEY")
        .expect("service-edge E2E runner must set ASTRA_BACKEND_SERVICE_KEY");
    assert!(
        !service_key.trim().is_empty(),
        "service-edge E2E runner must set a non-empty ASTRA_BACKEND_SERVICE_KEY"
    );

    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let user_id = &b.ctx.user_id;

    // 1. No Authorization header → 401 (env var set but no key provided).
    let (st_no_key, j_no_key) = get_json(
        app,
        &format!("/service/edges/status?user_id={user_id}"),
        None,
        &[],
    )
    .await;
    assert_eq!(
        st_no_key,
        StatusCode::UNAUTHORIZED,
        "missing key should be 401: {j_no_key}"
    );

    // 2. Wrong key → 401.
    let (st_bad, j_bad) = get_json(
        app,
        &format!("/service/edges/status?user_id={user_id}"),
        Some("Bearer wrong-key"),
        &[],
    )
    .await;
    assert_eq!(
        st_bad,
        StatusCode::UNAUTHORIZED,
        "invalid key should be 401: {j_bad}"
    );

    // 3. Valid key → 200 with edges array (may be empty; no live WS in this test).
    let valid_auth = format!("Bearer {service_key}");
    let (st_ok, ok_j) = get_json(
        app,
        &format!("/service/edges/status?user_id={user_id}"),
        Some(&valid_auth),
        &[],
    )
    .await;
    assert_eq!(st_ok, StatusCode::OK, "valid key should be 200: {ok_j}");
    assert!(
        ok_j["edges"].is_array(),
        "edges field must be array: {ok_j}"
    );

    ctx.close().await;
}

/// Valid approval respond commits one owner-scoped durable interaction decision.
pub async fn run_saas_approval_respond_success_path() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let request_id = format!("tc-saas-appr-{}", ctx.suffix);
    let run_id = format!("run-saas-appr-{}", ctx.suffix);
    seed_pending_approval(ctx, &run_id, &request_id, "write_file", "standard").await;

    let (st_appr, appr_j) = post_json(
        app,
        "/approval/respond",
        Some(auth),
        json!({
            "request_id": request_id,
            "decision": "allow",
            "reason": "saas e2e allow",
            "session_id": ctx.session_id,
            "run_id": run_id,
            "tool_name": "write_file",
            "approval_kind": "standard"
        }),
    )
    .await;
    assert_eq!(st_appr, StatusCode::OK, "approval/respond allow: {appr_j}");

    let decision =
        load_durable_interaction_event(ctx, &run_id, &request_id, "approval_resolved").await;
    assert_eq!(decision.pointer("/data/decision"), Some(&json!("allow")));
    assert_eq!(decision.pointer("/data/outcome"), Some(&json!("approved")));

    ctx.close().await;
}
