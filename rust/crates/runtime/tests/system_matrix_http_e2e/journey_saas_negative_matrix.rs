//! SaaS negative-path and boundary journeys: auth failures, resource governance denials,
//! concurrent-cap recovery, and task-lease cross-user isolation.
//!
//! Maps to `docs/testing/saas-test-plan.md` §5.1–§5.3, §4.2 (P/N/B coverage).

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::StreamExt;
use serde_json::{Value, json};
use sqlx::Row;
use tower::util::ServiceExt;
use uuid::Uuid;

use super::harness::{
    E2E_PASSWORD, bootstrap, build_e2e_access_token, cleanup_task_rows, get_json,
    grant_astra_admin_role, post_empty, post_json, post_json_with_headers, put_json,
    revoke_astra_admin_role, seeded_model_name, selected_model,
};
use super::journey_saas_platform_matrix::{
    cleanup_resource_limits, cleanup_seeded_run, limits_payload, seed_capacity_holding_run,
};
use astra_services::session_journal::{JournalEventType, read_journal};

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

    ctx.pool.close().await;
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
    let mock_model = seeded_model_name(ctx);

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
            "selected_model": selected_model(mock_model.clone()),
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
            "selected_model": selected_model(mock_model.clone()),
            "execution_budget": { "initial_turns": 1, "hard_turn_limit": 1 }
        }),
    )
    .await;
    assert_eq!(st_chat2, StatusCode::OK, "unlimited chat: {chat2}");
    assert!(chat2.get("run_id").is_some(), "run_id: {chat2}");

    cleanup_resource_limits(pool, user_id).await;
    ctx.pool.close().await;
}

/// Concurrent session cap: deny then admin recovery allows new chat.
pub async fn run_saas_resource_concurrent_cap_recovery() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let user_id = ctx.user_id.as_str();
    let mock_model = seeded_model_name(ctx);

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
            "selected_model": selected_model(mock_model.clone()),
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
            "selected_model": selected_model(mock_model.clone()),
            "execution_budget": { "initial_turns": 1, "hard_turn_limit": 1 }
        }),
    )
    .await;
    assert_eq!(st_ok, StatusCode::OK, "after cap raise: {ok_j}");
    assert!(ok_j.get("run_id").is_some());

    cleanup_seeded_run(pool, &holding_run).await;
    cleanup_resource_limits(pool, user_id).await;
    ctx.pool.close().await;
}

/// Task lease cross-user and auth negative paths.
pub async fn run_saas_task_lease_negative_paths() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth_a = &b.auth_header;
    let pool = &ctx.pool;
    let user_a = ctx.user_id.clone();
    let session_id = ctx.session_id.clone();
    let edge_agent_id = ctx.edge_agent_id.clone();

    let (st_task, task_j) = post_json(
        app,
        "/tasks",
        Some(auth_a),
        json!({
            "title": "saas lease iso task",
            "description": "owner A",
            "session_id": session_id,
        }),
    )
    .await;
    assert_eq!(st_task, StatusCode::CREATED, "create task: {task_j}");
    let task_id = task_j["task_id"].as_str().expect("task_id").to_string();

    let (auth_b, _user_b) = register_login_user(app, "lease_iso_b").await;

    // N: unauthenticated lease claim → 401
    let (st_unauth, unauth_j) = post_json_with_headers(
        app,
        &format!("/tasks/{task_id}/lease/claim"),
        None,
        &[],
        json!({ "edge_agent_id": edge_agent_id, "ttl_sec": 300 }),
    )
    .await;
    assert_eq!(
        st_unauth,
        StatusCode::UNAUTHORIZED,
        "claim without auth: {unauth_j}"
    );

    // N: empty edge_agent_id → 400
    let (st_empty, empty_j) = post_json(
        app,
        &format!("/tasks/{task_id}/lease/claim"),
        Some(auth_a),
        json!({ "edge_agent_id": "  ", "ttl_sec": 300 }),
    )
    .await;
    assert_eq!(
        st_empty,
        StatusCode::BAD_REQUEST,
        "empty edge_agent_id: {empty_j}"
    );

    // N: user B cannot access A's task (404, not 403 leak)
    let (st_get_b, get_b) = get_json(app, &format!("/tasks/{task_id}"), Some(&auth_b), &[]).await;
    assert_eq!(st_get_b, StatusCode::NOT_FOUND, "B get task: {get_b}");

    let (st_lease_b, lease_b) =
        get_json(app, &format!("/tasks/{task_id}/lease"), Some(&auth_b), &[]).await;
    assert_eq!(st_lease_b, StatusCode::NOT_FOUND, "B get lease: {lease_b}");

    let (st_claim_b, claim_b) = post_json_with_headers(
        app,
        &format!("/tasks/{task_id}/lease/claim"),
        Some(&auth_b),
        &[("x-astra-edge-id", "foreign-edge")],
        json!({ "edge_agent_id": "foreign-agent", "ttl_sec": 300 }),
    )
    .await;
    assert_eq!(
        st_claim_b,
        StatusCode::NOT_FOUND,
        "B claim A task: {claim_b}"
    );

    // P: owner A can claim
    let edge_reg = axum::http::Request::builder()
        .method("POST")
        .uri("/agents/edge")
        .header("authorization", auth_a.as_str())
        .header("content-type", "application/json")
        .header("x-astra-edge-id", "saas-neg-edge")
        .body(axum::body::Body::from(
            json!({
                "edge_agent_id": edge_agent_id,
                "hostname": "saas-neg-host",
                "capabilities": { "tools": ["read_file"] }
            })
            .to_string(),
        ))
        .expect("edge register");
    let edge_resp = app.clone().oneshot(edge_reg).await.expect("edge reg");
    assert_eq!(edge_resp.status(), StatusCode::OK);

    let (st_claim_a, claim_a) = post_json_with_headers(
        app,
        &format!("/tasks/{task_id}/lease/claim"),
        Some(auth_a),
        &[("x-astra-edge-id", "saas-neg-edge")],
        json!({ "edge_agent_id": edge_agent_id, "ttl_sec": 300 }),
    )
    .await;
    assert_eq!(st_claim_a, StatusCode::OK, "A claim own task: {claim_a}");

    cleanup_task_rows(pool, &user_a, &task_id).await;
    let _ = sqlx::query("DELETE FROM edge_agent_registry WHERE user_id = ? AND edge_agent_id = ?")
        .bind(&user_a)
        .bind(&edge_agent_id)
        .execute(pool)
        .await;

    ctx.pool.close().await;
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

    ctx.pool.close().await;
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
    ctx.pool.close().await;
}

/// Task lease: contested when held; reclaim after forced expiry.
pub async fn run_saas_task_lease_contested_and_expired_reclaim() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let user_id = ctx.user_id.clone();
    let session_id = ctx.session_id.clone();
    let agent_a = ctx.edge_agent_id.clone();
    let agent_b = format!("agent-b-{}", ctx.suffix);

    let (st_task, task_j) = post_json(
        app,
        "/tasks",
        Some(auth),
        json!({
            "title": "lease contested task",
            "description": "contested + reclaim",
            "session_id": session_id,
        }),
    )
    .await;
    assert_eq!(st_task, StatusCode::CREATED, "create task: {task_j}");
    let task_id = task_j["task_id"].as_str().expect("task_id").to_string();

    for (edge_hdr, agent_id, hostname) in [
        ("edge-a", agent_a.as_str(), "host-a"),
        ("edge-b", agent_b.as_str(), "host-b"),
    ] {
        let reg = Request::builder()
            .method("POST")
            .uri("/agents/edge")
            .header("authorization", auth.as_str())
            .header("content-type", "application/json")
            .header("x-astra-edge-id", edge_hdr)
            .body(Body::from(
                json!({
                    "edge_agent_id": agent_id,
                    "hostname": hostname,
                    "capabilities": { "tools": ["read_file"] }
                })
                .to_string(),
            ))
            .expect("edge register body");
        let resp = app.clone().oneshot(reg).await.expect("edge reg");
        assert_eq!(resp.status(), StatusCode::OK, "register {agent_id}");
    }

    // P: agent A claims
    let (st_a, claim_a) = post_json_with_headers(
        app,
        &format!("/tasks/{task_id}/lease/claim"),
        Some(auth),
        &[("x-astra-edge-id", "edge-a")],
        json!({ "edge_agent_id": agent_a, "ttl_sec": 300 }),
    )
    .await;
    assert_eq!(st_a, StatusCode::OK, "A claim: {claim_a}");
    assert_eq!(claim_a["status"].as_str(), Some("granted"));

    // N: agent B contested while A holds active lease
    let (st_b, claim_b) = post_json_with_headers(
        app,
        &format!("/tasks/{task_id}/lease/claim"),
        Some(auth),
        &[("x-astra-edge-id", "edge-b")],
        json!({ "edge_agent_id": agent_b, "ttl_sec": 300 }),
    )
    .await;
    assert_eq!(st_b, StatusCode::OK, "B contested claim: {claim_b}");
    assert_eq!(claim_b["status"].as_str(), Some("contested"));
    assert_eq!(
        claim_b["holder_agent_id"].as_str(),
        Some(agent_a.as_str()),
        "contested holder: {claim_b}"
    );

    // Force-expire lease in DB (simulate TTL elapsed)
    sqlx::query(
        "UPDATE task_leases SET expires_at = DATE_SUB(NOW(6), INTERVAL 1 MINUTE) \
         WHERE task_id = ? AND user_id = ?",
    )
    .bind(&task_id)
    .bind(&user_id)
    .execute(pool)
    .await
    .expect("expire lease row");

    // P: agent B reclaims after expiry
    let (st_reclaim, reclaim_j) = post_json_with_headers(
        app,
        &format!("/tasks/{task_id}/lease/claim"),
        Some(auth),
        &[("x-astra-edge-id", "edge-b")],
        json!({ "edge_agent_id": agent_b, "ttl_sec": 300 }),
    )
    .await;
    assert_eq!(st_reclaim, StatusCode::OK, "B reclaim: {reclaim_j}");
    assert_eq!(reclaim_j["status"].as_str(), Some("granted"));

    cleanup_task_rows(pool, &user_id, &task_id).await;
    for agent_id in [&agent_a, &agent_b] {
        let _ =
            sqlx::query("DELETE FROM edge_agent_registry WHERE user_id = ? AND edge_agent_id = ?")
                .bind(&user_id)
                .bind(agent_id)
                .execute(pool)
                .await;
    }

    ctx.pool.close().await;
}

/// Valid `POST /tools/result` during live `/chat/turn` handoff (positive callback path).
pub async fn run_saas_edge_tool_result_success_path() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let test_secret = std::env::var("ASTRA_TEST_BRIDGE_SECRET").expect("bridge test secret");
    let tool_output = "saas edge tool result ok";

    let payload = json!({
        "agent_id": "saas-edge-callback-agent",
        "session_id": ctx.session_id,
        "selected_model": selected_model(seeded_model_name(ctx)),
        "messages": [{ "role": "user", "content": "read saas probe file" }],
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
    });

    let req = Request::builder()
        .method("POST")
        .uri("/chat/turn")
        .header("authorization", auth.as_str())
        .header("content-type", "application/json")
        .header("x-mo-bridge-test-secret", &test_secret)
        .body(Body::from(payload.to_string()))
        .expect("chat/turn request");

    let response = app.clone().oneshot(req).await.expect("chat/turn");
    assert_eq!(response.status(), StatusCode::OK, "chat/turn status");

    let mut stream = response.into_body().into_data_stream();
    let mut acc = Vec::new();
    let mut posted_result = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("sse chunk");
        acc.extend_from_slice(&chunk);
        let s = String::from_utf8_lossy(&acc);
        if !posted_result && s.contains("tc-saas-tool-ok") {
            let (st_tool, tool_j) = post_json(
                app,
                "/tools/result",
                Some(auth.as_str()),
                json!({
                    "request_id": "tc-saas-tool-ok",
                    "status": "completed",
                    "output": tool_output,
                    "result_hash": astra_thin_client::ToolResultRequest::compute_result_hash(
                        "tc-saas-tool-ok",
                        tool_output,
                    ),
                }),
            )
            .await;
            assert_eq!(st_tool, StatusCode::OK, "valid /tools/result: {tool_j}");
            posted_result = true;
        }
        if s.contains("\"type\":\"turn_complete\"") {
            break;
        }
    }
    assert!(posted_result, "never posted /tools/result for tool_request");

    let full = String::from_utf8_lossy(&acc).into_owned();
    let events = parse_sse_events(&full);
    let completes: Vec<_> = events
        .iter()
        .filter(|e| e["type"].as_str() == Some("turn_complete"))
        .collect();
    assert!(!completes.is_empty(), "missing turn_complete: {full}");
    assert_eq!(completes[0]["has_tool_calls"], true);

    ctx.pool.close().await;
}

/// Memoria proxy degradation: memory routes fail; chat main path stays available.
pub async fn run_saas_memoria_proxy_degradation() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let mock_model = seeded_model_name(ctx);

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
            "selected_model": selected_model(mock_model.clone()),
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

    ctx.pool.close().await;
}

/// Run control cross-user isolation and list scoping.
pub async fn run_saas_run_cross_user_isolation() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth_a = &b.auth_header;
    let mock_model = seeded_model_name(ctx);

    let (st_chat, chat_j) = post_json(
        app,
        "/chat",
        Some(auth_a),
        json!({
            "message": "run isolation probe",
            "session_id": ctx.session_id,
            "selected_model": selected_model(mock_model.clone()),
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

    ctx.pool.close().await;
}

/// Double pause on the same run returns conflict (invalid state transition).
pub async fn run_saas_run_double_pause_conflict() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let mock_model = seeded_model_name(ctx);

    let (st_chat, chat_j) = post_json(
        app,
        "/chat",
        Some(auth),
        json!({
            "message": "double pause probe",
            "session_id": ctx.session_id,
            "selected_model": selected_model(mock_model.clone()),
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

    ctx.pool.close().await;
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

    ctx.pool.close().await;
}

/// Valid approval respond records a single journal decision (positive callback path).
pub async fn run_saas_approval_respond_success_path() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let request_id = format!("tc-saas-appr-{}", ctx.suffix);

    let (st_appr, appr_j) = post_json(
        app,
        "/approval/respond",
        Some(auth),
        json!({
            "request_id": request_id,
            "decision": "allow",
            "reason": "saas e2e allow",
            "session_id": ctx.session_id,
            "tool_name": "write_file",
            "approval_kind": "standard"
        }),
    )
    .await;
    assert_eq!(st_appr, StatusCode::OK, "approval/respond allow: {appr_j}");

    let decisions = read_journal(&ctx.session_id)
        .expect("read approval journal")
        .into_iter()
        .filter(|event| event.event_type == JournalEventType::ApprovalDecision)
        .filter(|event| {
            event
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("approval"))
                .and_then(|approval| approval.get("request_id"))
                .and_then(|v| v.as_str())
                == Some(request_id.as_str())
        })
        .count();
    assert_eq!(decisions, 1, "expected one approval decision recorded");

    ctx.pool.close().await;
}
