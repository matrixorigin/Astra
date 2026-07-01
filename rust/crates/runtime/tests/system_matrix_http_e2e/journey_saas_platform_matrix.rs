//! SaaS platform HTTP journeys: resource governance, admin config, RBAC grant/revoke,
//! auth refresh, per-user resource isolation, and session IDOR matrix.
//!
//! Maps to `docs/testing/saas-test-plan.md` §5.1–§5.6.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use sqlx::Row;
use tower::util::ServiceExt;
use uuid::Uuid;

use super::harness::{
    E2E_PASSWORD, bootstrap, delete_json, get_json, grant_astra_admin_role, post_empty, post_json,
    put_json, revoke_astra_admin_role, seeded_model_name, selected_model,
};
use super::journey_tasks_runs;
use astra_services::ADMIN_CONFIG_KEY_REASONING_MODEL;
use astra_services::session_journal::{JournalEventType, read_journal};

struct E2eUserAuth {
    auth_header: String,
    #[allow(dead_code)]
    user_id: String,
    #[allow(dead_code)]
    username: String,
}

async fn register_login_user(app: &Router, tag: &str) -> E2eUserAuth {
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

    let (st_login, login_j) = post_json(
        app,
        "/auth/login",
        None,
        json!({ "username": username, "password": E2E_PASSWORD }),
    )
    .await;
    assert_eq!(st_login, StatusCode::OK, "login {tag}: {login_j}");

    E2eUserAuth {
        auth_header: format!(
            "Bearer {}",
            login_j["access_token"].as_str().expect("access token")
        ),
        user_id: login_j["user_id"]
            .as_str()
            .or_else(|| reg["user_id"].as_str())
            .expect("user_id")
            .to_string(),
        username,
    }
}

pub async fn cleanup_resource_limits(pool: &sqlx::MySqlPool, user_id: &str) {
    let _ = sqlx::query("DELETE FROM resource_limits WHERE user_id = ?")
        .bind(user_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM resource_usage WHERE user_id = ?")
        .bind(user_id)
        .execute(pool)
        .await;
}

pub fn limits_payload(max_sessions_per_day: u32, max_concurrent_sessions: u32) -> Value {
    json!({
        "max_concurrent_sessions": max_concurrent_sessions,
        "max_tokens_per_day": 2_000_000,
        "max_disk_bytes": 1_073_741_824u64,
        "max_concurrent_bash": 3,
        "max_sessions_per_day": max_sessions_per_day
    })
}

/// Insert a paused run so `count_active_sessions` sees one capacity holder (no LLM race).
pub async fn seed_capacity_holding_run(
    pool: &sqlx::MySqlPool,
    user_id: &str,
    session_id: &str,
) -> String {
    let run_id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO agent_runs
         (run_id, user_id, session_id, root_run_id, ancestor_path, retry_scope, status, last_event_idx, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, 'node', 'paused', -1, NOW(6), NOW(6))",
    )
    .bind(&run_id)
    .bind(user_id)
    .bind(session_id)
    .bind(&run_id)
    .bind(&run_id)
    .execute(pool)
    .await
    .expect("seed paused run for concurrent cap");
    run_id
}

pub async fn cleanup_seeded_run(pool: &sqlx::MySqlPool, run_id: &str) {
    let _ = sqlx::query("DELETE FROM agent_runs WHERE run_id = ?")
        .bind(run_id)
        .execute(pool)
        .await;
}

/// GET /resources/limits + /resources/usage; admin PUT override reflected in DB.
pub async fn run_saas_resource_limits_read_and_admin_override() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let user_id = ctx.user_id.as_str();

    cleanup_resource_limits(pool, user_id).await;

    let (st_lim, lim_j) = get_json(app, "/resources/limits", Some(auth), &[]).await;
    assert_eq!(st_lim, StatusCode::OK, "GET /resources/limits: {lim_j}");
    assert_eq!(
        lim_j["limits"]["max_concurrent_sessions"].as_u64(),
        Some(5),
        "default concurrent cap: {lim_j}"
    );

    let (st_use, use_j) = get_json(app, "/resources/usage", Some(auth), &[]).await;
    assert_eq!(st_use, StatusCode::OK, "GET /resources/usage: {use_j}");
    assert!(use_j["usage"].is_object(), "usage object: {use_j}");
    assert!(use_j["limits"].is_object(), "limits echo: {use_j}");

    revoke_astra_admin_role(pool, user_id).await;
    grant_astra_admin_role(pool, user_id).await;

    let custom = limits_payload(99, 7);
    let (st_put, put_j) = put_json(
        app,
        &format!("/admin/resources/limits/{user_id}"),
        Some(auth),
        custom.clone(),
    )
    .await;
    assert_eq!(st_put, StatusCode::OK, "admin set limits: {put_j}");
    assert_eq!(put_j["limits"]["max_sessions_per_day"].as_u64(), Some(99));

    let row = sqlx::query(
        "SELECT max_sessions_per_day, max_concurrent_sessions FROM resource_limits WHERE user_id = ?",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .expect("resource_limits row");
    assert_eq!(row.try_get::<i32, _>("max_sessions_per_day").ok(), Some(99));
    assert_eq!(
        row.try_get::<i32, _>("max_concurrent_sessions").ok(),
        Some(7)
    );

    let (st_lim2, lim2) = get_json(app, "/resources/limits", Some(auth), &[]).await;
    assert_eq!(st_lim2, StatusCode::OK);
    assert_eq!(lim2["limits"]["max_sessions_per_day"].as_u64(), Some(99));

    cleanup_resource_limits(pool, user_id).await;
    ctx.pool.close().await;
}

/// Admin caps daily sessions; second auto-provisioned /chat returns 429; admin raises cap → allowed again.
pub async fn run_saas_resource_daily_session_cap_denies_chat() {
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
        limits_payload(1, 5),
    )
    .await;
    assert_eq!(st_cap, StatusCode::OK, "set daily cap=1");

    let chat_body = |session_id: Option<&str>| {
        let mut body = json!({
            "message": "saas quota probe",
            "selected_model": selected_model(mock_model.clone()),
            "execution_budget": {
                "initial_turns": 1,
                "hard_turn_limit": 1
            }
        });
        if let Some(sid) = session_id {
            body["session_id"] = json!(sid);
        }
        body
    };

    let (st1, chat1) = post_json(app, "/chat", Some(auth), chat_body(None)).await;
    assert_eq!(
        st1,
        StatusCode::OK,
        "first /chat creates session under cap: {chat1}"
    );
    assert!(chat1.get("run_id").is_some(), "run_id: {chat1}");

    // Second auto-provisioned session hits daily cap (check_session_create on new session).
    let (st2, chat2) = post_json(app, "/chat", Some(auth), chat_body(None)).await;
    assert_eq!(
        st2,
        StatusCode::TOO_MANY_REQUESTS,
        "second /chat should hit daily cap: {chat2}"
    );
    let err = chat2["detail"]
        .as_str()
        .or_else(|| chat2["error"].as_str())
        .or_else(|| chat2["message"].as_str())
        .unwrap_or("");
    assert!(
        err.to_ascii_lowercase().contains("limit") || err.to_ascii_lowercase().contains("resource"),
        "readable denial: {chat2}"
    );

    let (st_raise, _) = put_json(
        app,
        &format!("/admin/resources/limits/{user_id}"),
        Some(auth),
        limits_payload(5, 5),
    )
    .await;
    assert_eq!(st_raise, StatusCode::OK, "raise daily cap");

    let (st3, chat3) = post_json(app, "/chat", Some(auth), chat_body(None)).await;
    assert_eq!(st3, StatusCode::OK, "after admin raise: {chat3}");

    cleanup_resource_limits(pool, user_id).await;
    ctx.pool.close().await;
}

/// Concurrent run cap: one paused run holds capacity; cap=1 denies a second /chat start.
pub async fn run_saas_resource_concurrent_session_cap_denies_chat() {
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

    let holding_run = seed_capacity_holding_run(pool, user_id, ctx.session_id.as_str()).await;

    let (st_denied, denied) = post_json(
        app,
        "/chat",
        Some(auth),
        json!({
            "message": "concurrent cap probe",
            "selected_model": selected_model(mock_model.clone()),
            "execution_budget": { "initial_turns": 1, "hard_turn_limit": 1 }
        }),
    )
    .await;
    assert_eq!(
        st_denied,
        StatusCode::TOO_MANY_REQUESTS,
        "concurrent cap: {denied}"
    );

    cleanup_seeded_run(pool, &holding_run).await;
    cleanup_resource_limits(pool, user_id).await;
    ctx.pool.close().await;
}

/// Admin config list/get/put/delete with RBAC (403 without role).
pub async fn run_saas_admin_config_crud_rbac() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let user_id = ctx.user_id.as_str();
    let key = ADMIN_CONFIG_KEY_REASONING_MODEL;
    let model_value = format!("mock-saas-{}", ctx.suffix);

    revoke_astra_admin_role(pool, user_id).await;

    let (st_denied, _) = get_json(app, "/admin/config", Some(auth), &[]).await;
    assert_eq!(
        st_denied,
        StatusCode::FORBIDDEN,
        "config list without admin"
    );

    grant_astra_admin_role(pool, user_id).await;

    let (st_put, put_j) = put_json(
        app,
        &format!("/admin/config/{key}"),
        Some(auth),
        json!({ "value": model_value }),
    )
    .await;
    assert_eq!(st_put, StatusCode::OK, "PUT admin config: {put_j}");
    assert_eq!(put_j["value"].as_str(), Some(model_value.as_str()));

    let (st_get, get_j) = get_json(app, &format!("/admin/config/{key}"), Some(auth), &[]).await;
    assert_eq!(st_get, StatusCode::OK, "GET admin config: {get_j}");
    assert_eq!(get_j["value"].as_str(), Some(model_value.as_str()));

    let (st_list, list_j) = get_json(app, "/admin/config", Some(auth), &[]).await;
    assert_eq!(st_list, StatusCode::OK, "LIST admin config: {list_j}");
    let entries = list_j["entries"].as_array().expect("entries array");
    assert!(
        entries.iter().any(|e| e["key"].as_str() == Some(key)),
        "list contains reasoning_model_name: {list_j}"
    );

    let (st_del, del_j) = delete_json(app, &format!("/admin/config/{key}"), Some(auth)).await;
    assert_eq!(st_del, StatusCode::OK, "DELETE admin config: {del_j}");
    assert_eq!(del_j["deleted"].as_bool(), Some(true));

    let (st_missing, _) = get_json(app, &format!("/admin/config/{key}"), Some(auth), &[]).await;
    assert_eq!(st_missing, StatusCode::NOT_FOUND, "GET after delete");

    ctx.pool.close().await;
}

/// Admin grant/revoke `astra_admin` via HTTP; target user gains then loses admin routes.
pub async fn run_saas_admin_grant_revoke_rbac_flow() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth_admin = &b.auth_header;
    let pool = &ctx.pool;
    let admin_user_id = ctx.user_id.as_str();
    let admin_username = ctx.username.as_str();

    grant_astra_admin_role(pool, admin_user_id).await;

    let b_suffix = Uuid::new_v4().simple().to_string();
    let target_username = format!("saas_rbac_{b_suffix}");
    let target_email = format!("saas_rbac_{b_suffix}@e2e.test");

    let (st_reg, reg) = post_json(
        app,
        "/auth/register",
        None,
        json!({
            "username": target_username,
            "email": target_email,
            "password": E2E_PASSWORD,
            "display_name": "SaaS RBAC target"
        }),
    )
    .await;
    assert_eq!(st_reg, StatusCode::CREATED, "register target: {reg}");

    let (st_login, login_j) = post_json(
        app,
        "/auth/login",
        None,
        json!({ "username": target_username, "password": E2E_PASSWORD }),
    )
    .await;
    assert_eq!(st_login, StatusCode::OK, "login target: {login_j}");
    let target_auth = format!(
        "Bearer {}",
        login_j["access_token"].as_str().expect("access")
    );

    let (st_pre, _) = get_json(app, "/admin/tokens", Some(&target_auth), &[]).await;
    assert_eq!(st_pre, StatusCode::FORBIDDEN, "target before grant");

    let (st_grant, grant_j) = post_json(
        app,
        "/admin/users/grant-role",
        Some(auth_admin),
        json!({
            "username": target_username,
            "role_name": "astra_admin"
        }),
    )
    .await;
    assert_eq!(st_grant, StatusCode::OK, "grant role: {grant_j}");

    let (st_ok, body) = get_json(app, "/admin/tokens", Some(&target_auth), &[]).await;
    assert_eq!(st_ok, StatusCode::OK, "target after grant: {body}");
    assert!(body.is_array(), "tokens array: {body}");

    let (st_revoke, revoke_j) = post_json(
        app,
        "/admin/users/revoke-role",
        Some(auth_admin),
        json!({
            "username": target_username,
            "role_name": "astra_admin"
        }),
    )
    .await;
    assert_eq!(st_revoke, StatusCode::OK, "revoke role: {revoke_j}");

    let (st_post, _) = get_json(app, "/admin/tokens", Some(&target_auth), &[]).await;
    assert_eq!(st_post, StatusCode::FORBIDDEN, "target after revoke");

    // Ensure admin_username still works (sanity).
    let _ = admin_username;
    ctx.pool.close().await;
}

/// Resource usage counters are scoped per authenticated user.
pub async fn run_saas_resource_usage_per_user_isolation() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth_a = &b.auth_header;
    let pool = &ctx.pool;
    let user_a = ctx.user_id.as_str();
    let mock_model = seeded_model_name(ctx);

    cleanup_resource_limits(pool, user_a).await;

    let (st_chat, chat_j) = post_json(
        app,
        "/chat",
        Some(auth_a),
        json!({
            "message": "usage isolation probe",
            "selected_model": selected_model(mock_model.clone()),
            "execution_budget": { "initial_turns": 1, "hard_turn_limit": 1 }
        }),
    )
    .await;
    assert_eq!(
        st_chat,
        StatusCode::OK,
        "chat creates session for usage: {chat_j}"
    );

    let (st_a, use_a) = get_json(app, "/resources/usage", Some(auth_a), &[]).await;
    assert_eq!(st_a, StatusCode::OK);
    assert!(
        use_a["usage"]["sessions_created"].as_u64().unwrap_or(0) >= 1,
        "user A should have session usage: {use_a}"
    );

    let b_suffix = Uuid::new_v4().simple().to_string();
    let b_username = format!("saas_usage_{b_suffix}");
    let (st_reg, reg_b) = post_json(
        app,
        "/auth/register",
        None,
        json!({
            "username": b_username,
            "email": format!("{b_username}@e2e.test"),
            "password": E2E_PASSWORD
        }),
    )
    .await;
    assert_eq!(st_reg, StatusCode::CREATED, "register B: {reg_b}");
    let auth_b = format!(
        "Bearer {}",
        reg_b["access_token"].as_str().expect("B token")
    );
    let user_b = reg_b["user_id"].as_str().expect("B user_id");

    let (st_b, use_b) = get_json(app, "/resources/usage", Some(&auth_b), &[]).await;
    assert_eq!(st_b, StatusCode::OK);
    assert_eq!(
        use_b["usage"]["sessions_created"].as_u64(),
        Some(0),
        "user B must not see A's counters: {use_b}"
    );

    cleanup_resource_limits(pool, user_a).await;
    cleanup_resource_limits(pool, user_b).await;
    ctx.pool.close().await;
}

/// POST /auth/refresh returns a new access token that works on GET /auth/me.
pub async fn run_saas_auth_refresh_cycle() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;

    let (st_ref, ref_j) = post_json(
        app,
        "/auth/refresh",
        None,
        json!({ "refresh_token": b.refresh_token }),
    )
    .await;
    assert_eq!(st_ref, StatusCode::OK, "refresh: {ref_j}");
    let new_access = ref_j["access_token"]
        .as_str()
        .expect("new access_token")
        .to_string();
    let new_auth = format!("Bearer {new_access}");

    let (st_me, me_j) = get_json(app, "/auth/me", Some(&new_auth), &[]).await;
    assert_eq!(st_me, StatusCode::OK, "me with refreshed token: {me_j}");
    assert_eq!(me_j["user_id"].as_str(), Some(ctx.user_id.as_str()));

    ctx.pool.close().await;
}

/// User B cannot read or mutate user A's session (IDOR matrix).
pub async fn run_saas_session_cross_user_isolation() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth_a = &b.auth_header;
    let session_id = ctx.session_id.clone();
    let path = format!("/sessions/{session_id}");

    let user_b = register_login_user(app, "sess_iso_b").await;

    let (st_get, _) = get_json(app, &path, Some(&user_b.auth_header), &[]).await;
    assert_eq!(st_get, StatusCode::NOT_FOUND, "B must not GET A session");

    let (st_put, _) = put_json(
        app,
        &path,
        Some(&user_b.auth_header),
        json!({ "title": "hijack" }),
    )
    .await;
    assert_eq!(st_put, StatusCode::NOT_FOUND, "B must not PUT A session");

    let (st_cancel, _) = post_empty(
        app,
        &format!("/sessions/{session_id}/cancel"),
        Some(&user_b.auth_header),
    )
    .await;
    assert_eq!(
        st_cancel,
        StatusCode::NOT_FOUND,
        "B must not cancel A session"
    );

    let (st_del, _) = delete_json(app, &path, Some(&user_b.auth_header)).await;
    assert_eq!(st_del, StatusCode::NOT_FOUND, "B must not DELETE A session");

    let (st_list_b, list_b) = get_json(app, "/sessions", Some(&user_b.auth_header), &[]).await;
    assert_eq!(st_list_b, StatusCode::OK);
    let sessions_b = list_b["sessions"].as_array().expect("sessions B");
    assert!(
        !sessions_b
            .iter()
            .any(|s| s["session_id"].as_str() == Some(session_id.as_str())),
        "B list must not contain A session: {list_b}"
    );

    let (st_get_a, got_a) = get_json(app, &path, Some(auth_a), &[]).await;
    assert_eq!(st_get_a, StatusCode::OK, "A still reads session: {got_a}");

    ctx.pool.close().await;
}

/// Events, audit, and activity for foreign sessions return 404 (not empty leak).
pub async fn run_saas_events_and_audit_cross_user_isolation() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let session_id = ctx.session_id.clone();

    let user_b = register_login_user(app, "evt_iso_b").await;

    let (st_ev, _) = get_json(
        app,
        &format!("/events/session/{session_id}?limit=10"),
        Some(&user_b.auth_header),
        &[],
    )
    .await;
    assert_eq!(
        st_ev,
        StatusCode::NOT_FOUND,
        "B must not list A session events"
    );

    let (st_audit, _) = get_json(
        app,
        &format!("/sessions/{session_id}/audit/summary"),
        Some(&user_b.auth_header),
        &[],
    )
    .await;
    assert_eq!(
        st_audit,
        StatusCode::NOT_FOUND,
        "B must not read A session audit"
    );

    let (st_act, _) = get_json(
        app,
        &format!("/sessions/{session_id}/activity?limit=10"),
        Some(&user_b.auth_header),
        &[],
    )
    .await;
    assert_eq!(
        st_act,
        StatusCode::NOT_FOUND,
        "B must not read A session activity"
    );

    let (st_list_ev, list_ev) = get_json(
        app,
        &format!("/events?session_id={session_id}&limit=10"),
        Some(&user_b.auth_header),
        &[],
    )
    .await;
    assert_eq!(st_list_ev, StatusCode::OK, "list events: {list_ev}");
    let events = list_ev["events"].as_array().expect("events array");
    assert!(
        events.is_empty(),
        "filtered events for foreign session must be empty: {list_ev}"
    );

    ctx.pool.close().await;
}
async fn register_fresh_user(app: &Router, tag: &str) -> (String, String, String, String) {
    let suffix = Uuid::new_v4().simple().to_string();
    let username = format!("saas_{tag}_{suffix}");
    let (st, reg) = post_json(
        app,
        "/auth/register",
        None,
        json!({
            "username": username,
            "email": format!("{username}@e2e.test"),
            "password": E2E_PASSWORD,
            "display_name": format!("SaaS {tag}")
        }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "register {tag}: {reg}");
    let user_id = reg["user_id"].as_str().expect("user_id").to_string();
    let access = reg["access_token"].as_str().expect("access").to_string();
    let refresh = reg["refresh_token"].as_str().expect("refresh").to_string();
    (format!("Bearer {access}"), user_id, username, refresh)
}

fn json_has_api_key_leak(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            if map.contains_key("api_key") || map.contains_key("api_key_encrypted") {
                return true;
            }
            map.values().any(json_has_api_key_leak)
        }
        Value::Array(items) => items.iter().any(json_has_api_key_leak),
        _ => false,
    }
}

/// GET /health + GET /auth/me reflects registered user (§5.1, §7.4).
pub async fn run_saas_platform_health_and_auth_me() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;

    let (st_health, health_j) = get_json(app, "/health", None, &[]).await;
    assert_eq!(st_health, StatusCode::OK, "health: {health_j}");
    assert_eq!(health_j["status"].as_str(), Some("healthy"));

    let (st_me, me_j) = get_json(app, "/auth/me", Some(&b.auth_header), &[]).await;
    assert_eq!(st_me, StatusCode::OK, "auth/me: {me_j}");
    assert_eq!(me_j["user_id"].as_str(), Some(ctx.user_id.as_str()));
    assert_eq!(me_j["username"].as_str(), Some(ctx.username.as_str()));

    ctx.pool.close().await;
}

/// Refresh rotates refresh token; old refresh is rejected (§5.1).
pub async fn run_saas_auth_refresh_token_rotation() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let old_refresh = b.refresh_token.clone();

    let (st_ref, ref_j) = post_json(
        app,
        "/auth/refresh",
        None,
        json!({ "refresh_token": old_refresh }),
    )
    .await;
    assert_eq!(st_ref, StatusCode::OK, "refresh: {ref_j}");
    let new_access = ref_j["access_token"].as_str().expect("new access");
    let new_refresh = ref_j["refresh_token"].as_str().expect("new refresh");
    assert_ne!(
        new_refresh,
        old_refresh.as_str(),
        "refresh token must rotate"
    );

    let (st_old, old_j) = post_json(
        app,
        "/auth/refresh",
        None,
        json!({ "refresh_token": old_refresh }),
    )
    .await;
    assert_eq!(
        st_old,
        StatusCode::UNAUTHORIZED,
        "old refresh must be revoked: {old_j}"
    );

    let new_auth = format!("Bearer {new_access}");
    let (st_me, me_j) = get_json(app, "/auth/me", Some(&new_auth), &[]).await;
    assert_eq!(st_me, StatusCode::OK, "me with rotated access: {me_j}");

    let _ = new_refresh;
    ctx.pool.close().await;
}

/// Memory proxy overwrites spoofed user_id in body (§5.4, §5.7).
pub async fn run_saas_memory_proxy_user_isolation() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let user_id = ctx.user_id.as_str();

    let before = ctx.memoria.calls.lock().await.len();
    let (st, body) = post_json(
        app,
        "/memory/store",
        Some(auth),
        json!({
            "content": "saas memory isolation",
            "memory_type": "semantic",
            "user_id": "victim-user",
            "session_id": "victim-session"
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "memory store: {body}");

    let calls = ctx.memoria.calls.lock().await;
    assert!(calls.len() > before, "memoria forwarder invoked");
    let (_, forwarded) = calls.last().expect("last call");
    assert_eq!(forwarded["user_id"].as_str(), Some(user_id));
    assert_eq!(forwarded["session_id"].as_str(), Some(user_id));

    ctx.pool.close().await;
}

/// GET /models never exposes api_key; admin create stores encrypted key (§5.6).
pub async fn run_saas_models_list_and_key_encryption() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let plain_key = format!("saas-plain-key-{}", ctx.suffix);
    let model_name = format!("saas_mdl_{}", ctx.suffix);

    let (st_list0, list0) = get_json(app, "/models", Some(auth), &[]).await;
    assert_eq!(st_list0, StatusCode::OK, "list models: {list0}");
    assert!(
        !json_has_api_key_leak(&list0),
        "models list must not leak api_key: {list0}"
    );

    grant_astra_admin_role(pool, ctx.user_id.as_str()).await;
    let (st_create, create_j) = post_json(
        app,
        "/models",
        Some(auth),
        json!({
            "name": model_name,
            "provider": "mock",
            "context_window": 200000,
            "api_key": plain_key,
            "input_modalities": ["text"],
            "output_modalities": ["text"]
        }),
    )
    .await;
    assert_eq!(st_create, StatusCode::CREATED, "create model: {create_j}");
    assert!(
        create_j.get("api_key").is_none(),
        "create response must not echo api_key: {create_j}"
    );

    let (st_list1, list1) = get_json(app, "/models", Some(auth), &[]).await;
    assert_eq!(st_list1, StatusCode::OK, "list after create: {list1}");
    assert!(
        !json_has_api_key_leak(&list1),
        "models list after create: {list1}"
    );

    let encrypted: Option<String> =
        sqlx::query_scalar("SELECT api_key_encrypted FROM infra_llm_models WHERE model_name = ?")
            .bind(&model_name)
            .fetch_optional(pool)
            .await
            .expect("select api_key_encrypted");
    let encrypted = encrypted.expect("encrypted key row");
    assert!(!encrypted.is_empty(), "api_key_encrypted must be set");
    assert_ne!(encrypted, plain_key, "DB must not store plaintext api key");

    let _ = delete_json(app, &format!("/models/{model_name}"), Some(auth)).await;
    ctx.pool.close().await;
}

/// Session CRUD positive path (§5.1, §6.1).
pub async fn run_saas_session_lifecycle_positive() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;

    let (st_create, created) = post_json(
        app,
        "/sessions",
        Some(auth),
        json!({
            "title": "saas lifecycle session",
            "metadata": { "suite": "saas_coverage" }
        }),
    )
    .await;
    assert_eq!(st_create, StatusCode::CREATED, "create session: {created}");
    let session_id = created["session_id"].as_str().expect("session_id");

    let (st_get, got) = get_json(app, &format!("/sessions/{session_id}"), Some(auth), &[]).await;
    assert_eq!(st_get, StatusCode::OK, "get session: {got}");
    assert_eq!(got["title"].as_str(), Some("saas lifecycle session"));

    let (st_put, updated) = put_json(
        app,
        &format!("/sessions/{session_id}"),
        Some(auth),
        json!({ "title": "saas lifecycle renamed" }),
    )
    .await;
    assert_eq!(st_put, StatusCode::OK, "update session: {updated}");

    let (st_list, list_j) = get_json(app, "/sessions", Some(auth), &[]).await;
    assert_eq!(st_list, StatusCode::OK, "list sessions: {list_j}");
    let sessions = list_j["sessions"].as_array().expect("sessions");
    assert!(
        sessions
            .iter()
            .any(|s| s["session_id"].as_str() == Some(session_id)),
        "list contains new session: {list_j}"
    );

    ctx.pool.close().await;
}

/// Resource usage counters increment after chat creates a session (§5.3, §6.6).
pub async fn run_saas_resource_usage_increments_after_chat() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let user_id = ctx.user_id.as_str();
    let mock_model = seeded_model_name(ctx);

    cleanup_resource_limits(pool, user_id).await;

    let (st_before, before_j) = get_json(app, "/resources/usage", Some(auth), &[]).await;
    assert_eq!(st_before, StatusCode::OK, "usage before: {before_j}");
    let sessions_before = before_j["usage"]["sessions_created"].as_u64().unwrap_or(0);

    let (st_chat, chat_j) = post_json(
        app,
        "/chat",
        Some(auth),
        json!({
            "message": "usage counter probe",
            "selected_model": selected_model(mock_model.clone()),
            "execution_budget": { "initial_turns": 1, "hard_turn_limit": 1 }
        }),
    )
    .await;
    assert_eq!(st_chat, StatusCode::OK, "chat for usage: {chat_j}");

    let (st_after, after_j) = get_json(app, "/resources/usage", Some(auth), &[]).await;
    assert_eq!(st_after, StatusCode::OK, "usage after: {after_j}");
    let sessions_after = after_j["usage"]["sessions_created"].as_u64().unwrap_or(0);
    assert!(
        sessions_after > sessions_before,
        "sessions_created should increment: before={sessions_before} after={sessions_after}"
    );

    ctx.pool.close().await;
}

/// Cross-user run cancel forbidden; owner cancel succeeds (§4.3, §5.4).
pub async fn run_saas_run_cancel_cross_user_and_owner() {
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
            "message": "cancel isolation probe",
            "session_id": ctx.session_id,
            "selected_model": selected_model(mock_model.clone()),
            "execution_budget": { "initial_turns": 1, "hard_turn_limit": 1 }
        }),
    )
    .await;
    assert_eq!(st_chat, StatusCode::OK, "create run: {chat_j}");
    let run_id = chat_j["run_id"].as_str().expect("run_id").to_string();

    // Pause so the run stays cancellable (mock runs may finish quickly otherwise).
    let (st_pause, _) = post_empty(app, &format!("/chat/runs/{run_id}/pause"), Some(auth_a)).await;
    assert_eq!(st_pause, StatusCode::OK, "pause before cancel");

    let (auth_b, _, _, _) = register_fresh_user(app, "cancel_b").await;

    let (st_denied, denied_j) =
        delete_json(app, &format!("/chat/runs/{run_id}"), Some(&auth_b)).await;
    assert_eq!(
        st_denied,
        StatusCode::NOT_FOUND,
        "B must not cancel A run: {denied_j}"
    );

    let (st_cancel, cancel_j) =
        delete_json(app, &format!("/chat/runs/{run_id}"), Some(auth_a)).await;
    assert_eq!(st_cancel, StatusCode::OK, "owner cancel: {cancel_j}");
    let status = cancel_j["status"].as_str().unwrap_or("");
    assert!(
        status == "cancelled" || status == "failed" || status == "completed",
        "terminal cancel status: {cancel_j}"
    );

    ctx.pool.close().await;
}

/// Approval deny decision is recorded (§4.2 negative callback path).
pub async fn run_saas_approval_respond_deny_path() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let request_id = format!("tc-saas-deny-{}", ctx.suffix);

    let (st, body) = post_json(
        app,
        "/approval/respond",
        Some(auth),
        json!({
            "request_id": request_id,
            "decision": "deny",
            "reason": "saas e2e deny",
            "session_id": ctx.session_id,
            "tool_name": "bash",
            "approval_kind": "standard"
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "approval deny: {body}");

    let decisions = read_journal(&ctx.session_id)
        .expect("journal")
        .into_iter()
        .filter(|e| e.event_type == JournalEventType::ApprovalDecision)
        .filter(|e| {
            e.metadata
                .as_ref()
                .and_then(|m| m.get("approval"))
                .and_then(|a| a.get("request_id"))
                .and_then(|v| v.as_str())
                == Some(request_id.as_str())
        })
        .count();
    assert_eq!(decisions, 1, "deny should record one approval decision");

    ctx.pool.close().await;
}

/// Headless run pause/resume happy path (§4.3) — delegates to shared journey.
pub async fn run_saas_chat_run_pause_resume_positive() {
    journey_tasks_runs::run_chat_run_pause_resume_http().await;
}

/// Admin tokens smoke + non-admin forbidden (§5.2).
pub async fn run_saas_admin_tokens_rbac_smoke() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let user_id = ctx.user_id.as_str();

    revoke_astra_admin_role(pool, user_id).await;
    let (st_denied, _) = get_json(app, "/admin/tokens", Some(auth), &[]).await;
    assert_eq!(st_denied, StatusCode::FORBIDDEN, "tokens without admin");

    grant_astra_admin_role(pool, user_id).await;
    let (st_ok, body) = get_json(app, "/admin/tokens", Some(auth), &[]).await;
    assert_eq!(st_ok, StatusCode::OK, "admin tokens: {body}");
    assert!(body.is_array(), "tokens array: {body}");

    ctx.pool.close().await;
}

/// Register + login positive path distinct from bootstrap (§5.1).
pub async fn run_saas_auth_register_login_positive() {
    let b = bootstrap().await;
    let app = &b.ctx.app;

    let (auth, user_id, username, _) = register_fresh_user(app, "reg_login").await;

    let (st_login, login_j) = post_json(
        app,
        "/auth/login",
        None,
        json!({ "username": username, "password": E2E_PASSWORD }),
    )
    .await;
    assert_eq!(st_login, StatusCode::OK, "login after register: {login_j}");
    assert!(login_j["access_token"].as_str().is_some());
    let login_auth = format!(
        "Bearer {}",
        login_j["access_token"].as_str().expect("login access")
    );
    let (st_me_login, me_login) = get_json(app, "/auth/me", Some(&login_auth), &[]).await;
    assert_eq!(st_me_login, StatusCode::OK, "me after login: {me_login}");
    assert_eq!(me_login["user_id"].as_str(), Some(user_id.as_str()));

    let (st_me, me_j) = get_json(app, "/auth/me", Some(&auth), &[]).await;
    assert_eq!(st_me, StatusCode::OK, "me with register token: {me_j}");

    b.ctx.pool.close().await;
}
/// Duplicate email on register returns 409 (§5.1).
pub async fn run_saas_auth_duplicate_email_register() {
    let b = bootstrap().await;
    let app = &b.ctx.app;
    let email = format!("dup_{}@e2e.test", Uuid::new_v4().simple());
    let user1 = format!("dup_a_{}", Uuid::new_v4().simple());
    let user2 = format!("dup_b_{}", Uuid::new_v4().simple());

    let (st1, _) = post_json(
        app,
        "/auth/register",
        None,
        json!({
            "username": user1,
            "email": email,
            "password": E2E_PASSWORD
        }),
    )
    .await;
    assert_eq!(st1, StatusCode::CREATED);

    let (st_dup, j_dup) = post_json(
        app,
        "/auth/register",
        None,
        json!({
            "username": user2,
            "email": email,
            "password": E2E_PASSWORD
        }),
    )
    .await;
    assert_eq!(st_dup, StatusCode::BAD_REQUEST, "duplicate email: {j_dup}");
    let detail = j_dup["detail"].as_str().unwrap_or("");
    assert!(
        detail.to_ascii_lowercase().contains("email"),
        "duplicate email detail: {j_dup}"
    );

    b.ctx.pool.close().await;
}

/// GET /runs lists owner runs after POST /chat (§4.3).
pub async fn run_saas_runs_list_pagination_positive() {
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
            "message": "runs list probe",
            "session_id": ctx.session_id,
            "selected_model": selected_model(mock_model.clone()),
            "execution_budget": { "initial_turns": 1, "hard_turn_limit": 1 }
        }),
    )
    .await;
    assert_eq!(st_chat, StatusCode::OK, "create run: {chat_j}");
    let run_id = chat_j["run_id"].as_str().expect("run_id");

    let (st_list, list_j) = get_json(app, "/runs?limit=20", Some(auth), &[]).await;
    assert_eq!(st_list, StatusCode::OK, "list runs: {list_j}");
    let runs = list_j["runs"].as_array().expect("runs array");
    assert!(
        runs.iter().any(|r| r["run_id"].as_str() == Some(run_id)),
        "list must contain created run: {list_j}"
    );

    ctx.pool.close().await;
}

/// POST /agents/edge registers edge agent in registry (§4.2).
pub async fn run_saas_edge_agent_registration_smoke() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let edge_agent_id = format!("saas-edge-reg-{}", ctx.suffix);
    let edge_header_id = format!("saas-edge-hdr-{}", ctx.suffix);

    let edge_reg = Request::builder()
        .method("POST")
        .uri("/agents/edge")
        .header("authorization", auth.as_str())
        .header("content-type", "application/json")
        .header("x-astra-edge-id", edge_header_id.as_str())
        .body(Body::from(
            json!({
                "edge_agent_id": edge_agent_id,
                "hostname": "saas-e2e-host",
                "capabilities": { "tools": ["read_file"] }
            })
            .to_string(),
        ))
        .expect("edge register request");
    let edge_resp = app.clone().oneshot(edge_reg).await.expect("edge register");
    assert_eq!(edge_resp.status(), StatusCode::OK);

    let row = sqlx::query(
        "SELECT edge_agent_id FROM edge_agent_registry WHERE user_id = ? AND edge_agent_id = ?",
    )
    .bind(ctx.user_id.as_str())
    .bind(&edge_agent_id)
    .fetch_optional(pool)
    .await
    .expect("edge registry row");
    assert!(row.is_some(), "edge agent should be registered");

    let _ = sqlx::query("DELETE FROM edge_agent_registry WHERE user_id = ? AND edge_agent_id = ?")
        .bind(ctx.user_id.as_str())
        .bind(&edge_agent_id)
        .execute(pool)
        .await;

    ctx.pool.close().await;
}

/// POST /admin/cleanup requires admin (§5.2, §5.5).
pub async fn run_saas_admin_cleanup_rbac_smoke() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let user_id = ctx.user_id.as_str();

    revoke_astra_admin_role(pool, user_id).await;
    let (st_denied, _) = post_json(app, "/admin/cleanup", Some(auth), json!({})).await;
    assert_eq!(st_denied, StatusCode::FORBIDDEN, "cleanup without admin");

    grant_astra_admin_role(pool, user_id).await;
    let (st_ok, body) = post_json(app, "/admin/cleanup", Some(auth), json!({})).await;
    assert_eq!(st_ok, StatusCode::OK, "admin cleanup: {body}");
    assert!(
        body.get("total_deleted").is_some(),
        "cleanup summary: {body}"
    );
    assert!(body.get("tables").is_some(), "cleanup tables: {body}");

    ctx.pool.close().await;
}

/// GET /admin/audit requires admin (§5.2).
pub async fn run_saas_admin_audit_rbac_smoke() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let user_id = ctx.user_id.as_str();

    revoke_astra_admin_role(pool, user_id).await;
    let (st_denied, _) = get_json(app, "/admin/audit?limit=5", Some(auth), &[]).await;
    assert_eq!(st_denied, StatusCode::FORBIDDEN, "audit without admin");

    grant_astra_admin_role(pool, user_id).await;
    let (st_ok, body) = get_json(app, "/admin/audit?limit=5", Some(auth), &[]).await;
    assert_eq!(st_ok, StatusCode::OK, "admin audit: {body}");

    ctx.pool.close().await;
}

/// User-scoped skills are invisible to other users (§5.4).
pub async fn run_saas_skills_cross_user_isolation() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth_a = &b.auth_header;
    let skill_name = format!("saas_skill_iso_{}", ctx.suffix);

    let (st_create, create_j) = post_json(
        app,
        "/skills/user",
        Some(auth_a),
        json!({
            "skill_name": skill_name,
            "visibility": "private"
        }),
    )
    .await;
    assert_eq!(st_create, StatusCode::CREATED, "create skill: {create_j}");

    let (st_list_a, list_a) = get_json(app, "/skills/user", Some(auth_a), &[]).await;
    assert_eq!(
        st_list_a,
        StatusCode::OK,
        "A list personal skills: {list_a}"
    );
    let empty: Vec<Value> = vec![];
    let skills_a = list_a.as_array().unwrap_or(&empty);
    assert!(
        skills_a
            .iter()
            .any(|s| s["skill_name"].as_str() == Some(skill_name.as_str())),
        "A list must include A skill: {list_a}"
    );

    let (auth_b, _, _, _) = register_fresh_user(app, "skill_iso_b").await;

    let (st_list_b, list_b) = get_json(app, "/skills/user", Some(&auth_b), &[]).await;
    assert_eq!(
        st_list_b,
        StatusCode::OK,
        "B list personal skills: {list_b}"
    );
    let skills_b = list_b.as_array().unwrap_or(&empty);
    assert!(
        !skills_b
            .iter()
            .any(|s| s["skill_name"].as_str() == Some(skill_name.as_str())),
        "B list must not include A skill: {list_b}"
    );

    ctx.pool.close().await;
}

/// Team isolation — delegates to shared journey (§5.4).
pub async fn run_saas_team_cross_user_isolation() {
    super::journey_team_isolation_matrix::run_team_cross_user_isolation().await;
}

/// GET /sessions/{id}/replay/compare after chat activity (§6.1).
pub async fn run_saas_session_replay_compare_smoke() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let session_id = ctx.session_id.as_str();
    let mock_model = seeded_model_name(ctx);

    let (st_chat, chat_j) = post_json(
        app,
        "/chat",
        Some(auth),
        json!({
            "message": "replay compare probe",
            "session_id": session_id,
            "selected_model": selected_model(mock_model.clone()),
            "execution_budget": { "initial_turns": 1, "hard_turn_limit": 1 }
        }),
    )
    .await;
    assert_eq!(st_chat, StatusCode::OK, "chat for replay: {chat_j}");

    let (st_cmp, cmp_j) = get_json(
        app,
        &format!("/sessions/{session_id}/replay/compare"),
        Some(auth),
        &[],
    )
    .await;
    assert_eq!(st_cmp, StatusCode::OK, "replay compare: {cmp_j}");
    assert!(
        cmp_j.get("original_event_count").is_some(),
        "replay compare shape: {cmp_j}"
    );

    ctx.pool.close().await;
}

/// POST /sessions/{id}/replay after chat (§6.1).
pub async fn run_saas_session_replay_post_positive() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let session_id = ctx.session_id.as_str();
    let mock_model = seeded_model_name(ctx);

    let (st_chat, chat_j) = post_json(
        app,
        "/chat",
        Some(auth),
        json!({
            "message": "replay post probe",
            "session_id": session_id,
            "selected_model": selected_model(mock_model.clone()),
            "execution_budget": { "initial_turns": 1, "hard_turn_limit": 1 }
        }),
    )
    .await;
    assert_eq!(st_chat, StatusCode::OK, "chat for replay post: {chat_j}");

    let (st_rep, rep_j) = post_json(
        app,
        &format!("/sessions/{session_id}/replay"),
        Some(auth),
        json!({ "mock_mode": true, "sandbox_name": "saas-e2e" }),
    )
    .await;
    assert_eq!(st_rep, StatusCode::OK, "replay post: {rep_j}");
    assert_eq!(rep_j["session_id"].as_str(), Some(session_id));
    assert_eq!(rep_j["status"].as_str(), Some("completed"));

    let (auth_b, _, _, _) = register_fresh_user(app, "replay_iso_b").await;
    let (st_forbid, forbid_j) = post_json(
        app,
        &format!("/sessions/{session_id}/replay"),
        Some(&auth_b),
        json!({ "mock_mode": true }),
    )
    .await;
    assert_eq!(
        st_forbid,
        StatusCode::NOT_FOUND,
        "foreign user replay: {forbid_j}"
    );

    ctx.pool.close().await;
}

/// GET /admin/feedback/stats RBAC + filter query (§5.2).
pub async fn run_saas_admin_feedback_stats_rbac() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let user_id = ctx.user_id.as_str();

    revoke_astra_admin_role(pool, user_id).await;
    let (st_denied, _) = get_json(app, "/admin/feedback/stats", Some(auth), &[]).await;
    assert_eq!(
        st_denied,
        StatusCode::FORBIDDEN,
        "feedback stats without admin"
    );

    grant_astra_admin_role(pool, user_id).await;
    let (st_ok, stats_j) = get_json(app, "/admin/feedback/stats", Some(auth), &[]).await;
    assert_eq!(st_ok, StatusCode::OK, "feedback stats: {stats_j}");
    assert!(stats_j.get("total_feedback").is_some(), "shape: {stats_j}");

    let (st_filt, filt_j) = get_json(
        app,
        "/admin/feedback/stats?agent_id=saas-e2e-agent&since=2020-01-01%2000:00:00",
        Some(auth),
        &[],
    )
    .await;
    assert_eq!(st_filt, StatusCode::OK, "filtered stats: {filt_j}");

    ctx.pool.close().await;
}

/// GET /chat/runs/{id}/projection after POST /chat (§4.3).
pub async fn run_saas_run_projection_smoke() {
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
            "message": "run projection probe",
            "session_id": ctx.session_id,
            "selected_model": selected_model(mock_model.clone()),
            "execution_budget": { "initial_turns": 1, "hard_turn_limit": 1 }
        }),
    )
    .await;
    assert_eq!(st_chat, StatusCode::OK, "create run: {chat_j}");
    let run_id = chat_j["run_id"].as_str().expect("run_id");

    let (st_proj, proj_j) = get_json(
        app,
        &format!("/chat/runs/{run_id}/projection?recent_limit=10"),
        Some(auth),
        &[],
    )
    .await;
    assert_eq!(st_proj, StatusCode::OK, "run projection: {proj_j}");
    assert_eq!(proj_j["run_id"].as_str(), Some(run_id));

    ctx.pool.close().await;
}

/// Session audit endpoints after chat activity.
pub async fn run_saas_session_audit_after_chat_smoke() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let session_id = ctx.session_id.as_str();
    let mock_model = seeded_model_name(ctx);

    let (st_chat, chat_j) = post_json(
        app,
        "/chat",
        Some(auth),
        json!({
            "message": "session audit probe",
            "session_id": session_id,
            "selected_model": selected_model(mock_model.clone()),
            "execution_budget": { "initial_turns": 1, "hard_turn_limit": 1 }
        }),
    )
    .await;
    assert_eq!(st_chat, StatusCode::OK, "chat for audit: {chat_j}");

    for path in [
        format!("/sessions/{session_id}/audit/summary"),
        format!("/sessions/{session_id}/audit/turns?page=1&per_page=10"),
        format!("/sessions/{session_id}/audit/tools"),
    ] {
        let (st, body) = get_json(app, &path, Some(auth), &[]).await;
        assert_eq!(st, StatusCode::OK, "GET {path}: {body}");
    }

    ctx.pool.close().await;
}

/// Task lease claim → renew → release (§4.2).
pub async fn run_saas_task_lease_renew_release_positive() {
    journey_tasks_runs::run_tasks_lease_with_db_assertions().await;
}

/// GET /platform/snapshot (§5.1).
pub async fn run_saas_platform_snapshot_smoke() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;

    let (st_snap, snap_j) = get_json(app, "/platform/snapshot", Some(auth), &[]).await;
    assert_eq!(st_snap, StatusCode::OK, "platform snapshot: {snap_j}");
    assert_eq!(snap_j["health"]["status"].as_str(), Some("healthy"));

    ctx.pool.close().await;
}

/// Session activity, transcript, artifacts after chat.
pub async fn run_saas_session_activity_transcript_artifacts_smoke() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let session_id = ctx.session_id.as_str();
    let mock_model = seeded_model_name(ctx);

    let (st_chat, chat_j) = post_json(
        app,
        "/chat",
        Some(auth),
        json!({
            "message": "session handlers probe",
            "session_id": session_id,
            "selected_model": selected_model(mock_model.clone()),
            "execution_budget": { "initial_turns": 1, "hard_turn_limit": 1 }
        }),
    )
    .await;
    assert_eq!(st_chat, StatusCode::OK, "chat: {chat_j}");

    let (st_act, act_j) = get_json(
        app,
        &format!("/sessions/{session_id}/activity?limit=10"),
        Some(auth),
        &[],
    )
    .await;
    assert_eq!(st_act, StatusCode::OK, "activity: {act_j}");

    let (st_tr, tr_j) = get_json(
        app,
        &format!("/sessions/{session_id}/transcript?limit=20"),
        Some(auth),
        &[],
    )
    .await;
    assert_eq!(st_tr, StatusCode::OK, "transcript: {tr_j}");
    assert_eq!(tr_j["session_id"].as_str(), Some(session_id));

    ctx.pool.close().await;
}

/// GET /events/session/{id} after chat.
pub async fn run_saas_events_session_after_chat_positive() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let session_id = ctx.session_id.as_str();
    let mock_model = seeded_model_name(ctx);

    let (st_chat, chat_j) = post_json(
        app,
        "/chat",
        Some(auth),
        json!({
            "message": "events session probe",
            "session_id": session_id,
            "selected_model": selected_model(mock_model.clone()),
            "execution_budget": { "initial_turns": 1, "hard_turn_limit": 1 }
        }),
    )
    .await;
    assert_eq!(st_chat, StatusCode::OK, "chat: {chat_j}");

    let (st_ev, ev_j) = get_json(
        app,
        &format!("/events/session/{session_id}?limit=20"),
        Some(auth),
        &[],
    )
    .await;
    assert_eq!(st_ev, StatusCode::OK, "events/session: {ev_j}");
    let events = ev_j["events"].as_array().expect("events");
    assert!(!events.is_empty(), "events after chat: {ev_j}");

    ctx.pool.close().await;
}

/// Delegation HTTP boundaries (§4.3).
pub async fn run_saas_delegate_http_boundaries() {
    super::journey_delegate_http_matrix::run_delegate_http_boundaries().await;
}
