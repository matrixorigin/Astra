//! System HTTP end-to-end: real Axum app + MatrixOne + full `build_server_state` wiring.
//!
//! ## Tests in this binary
//! 1. **`product_matrix_api_journey_hits_multiple_tables`** — **product matrix**: one
//!    realistic client-style journey that exercises many public routes and asserts persistence on
//!    `auth_users`, `agent_sessions`, `agent_agents`, `agent_events`, `ctx_snapshots`,
//!    `ctx_decision_audits`, `edge_agent_registry`, plus in-memory edge callbacks; also hits
//!    `auth/refresh`, `auth/logout`, session audit APIs, `GET /events` + causal-chain + list,
//!    data-versioning lineage (read-only), agent update, memory search/purge, `GET /workflows`,
//!    jobs (in-memory + webhook), sandbox CRUD, webhook triggers (create → fire → delete), and
//!    `GET /introspection/skills`, session **close → resume** (with `agent_sessions` status checks),
//!    session audit **tools/errors**, more evaluation reads (scores, quality trend, SLO, memory health,
//!    trust + observability once `agent_id` exists). Ends with `chat/turn` (SSE) and strict `agent_events`
//!    checks (parent link, per-field tokens, `reasoning_content`, causal chain).
//!
//! External dependencies remain mocked where the product already allows it:
//! - LLM: `test_llm_rounds` + `bridge-e2e-hooks` (no external model server).
//! - Memoria: [`astra_runtime::MemoriaForwarder`] stub (memory proxy routes only).
//!
//! ## How to run
//! ```text
//! # Terminal 1: MatrixOne with schema (same as local dev)
//! MO_AGENT_SYSTEM_MATRIX_E2E=1 \
//! MO_AGENT_BRIDGE_TEST_SECRET=system-matrix-e2e-secret \
//! cargo test -p astra-runtime --test system_matrix_http_e2e --features bridge-e2e-hooks -- \
//!   --ignored --nocapture
//! ```
//!
//! Requires the same env as `astra-server` startup: `MATRIXONE_*`, JWT / Fernet secrets from
//! [`astra_core::AppSettings::from_env`], etc. Load `.env` if you use one for development.

use std::sync::Arc;
use std::sync::OnceLock;

use async_trait::async_trait;
use axum::{
    Router,
    body::{self, Body},
    http::{Request, StatusCode},
};
use astra_core::config::AppSettings;
use astra_runtime::{MemoriaForwarder, build_app, build_server_state};
use futures_util::StreamExt;
use serde_json::{Value, json};
use sqlx::mysql::MySqlRow;
use sqlx::Row;
use tokio::sync::Mutex;
use tower::util::ServiceExt;
use uuid::Uuid;

static E2E_ENV_INIT: OnceLock<()> = OnceLock::new();

fn require_system_e2e_env() {
    assert_eq!(
        std::env::var("MO_AGENT_SYSTEM_MATRIX_E2E").as_deref(),
        Ok("1"),
        "set MO_AGENT_SYSTEM_MATRIX_E2E=1 to run this ignored test"
    );
    E2E_ENV_INIT.get_or_init(|| {
        let secret = std::env::var("MO_AGENT_BRIDGE_TEST_SECRET").unwrap_or_else(|_| {
            "system-matrix-e2e-secret".to_string()
        });
        // SAFETY: set once before parallel test threads (single ignored test in practice).
        unsafe {
            std::env::set_var("MO_AGENT_BRIDGE_TEST_SECRET", &secret);
        }
    });
}

#[derive(Clone, Default)]
struct E2eMemoriaStub {
    calls: Arc<Mutex<Vec<(String, Value)>>>,
}

#[async_trait]
impl MemoriaForwarder for E2eMemoriaStub {
    async fn forward(
        &self,
        endpoint: &str,
        body: Value,
    ) -> Result<Value, String> {
        self.calls
            .lock()
            .await
            .push((endpoint.to_string(), body));
        if endpoint.contains("retrieve") {
            return Ok(json!({ "memories": [] }));
        }
        Ok(json!({ "memory_id": "e2e-stub-memory" }))
    }
}

async fn get_json(
    app: &Router,
    path: &str,
    auth: Option<&str>,
    extra_headers: &[(&str, &str)],
) -> (StatusCode, Value) {
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
    let bytes = body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .expect("body");
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));
    (status, json)
}

async fn put_json(
    app: &Router,
    path: &str,
    auth: Option<&str>,
    payload: Value,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method("PUT")
        .uri(path)
        .header("content-type", "application/json");
    if let Some(t) = auth {
        req = req.header("authorization", t);
    }
    let req = req
        .body(Body::from(payload.to_string()))
        .expect("request");
    let response = app.clone().oneshot(req).await.expect("oneshot");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .expect("body");
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));
    (status, json)
}

async fn delete_no_content(app: &Router, path: &str, auth: Option<&str>) -> StatusCode {
    let mut req = Request::builder().method("DELETE").uri(path);
    if let Some(t) = auth {
        req = req.header("authorization", t);
    }
    let req = req.body(Body::empty()).expect("request");
    let response = app.clone().oneshot(req).await.expect("oneshot");
    response.status()
}

async fn delete_json(app: &Router, path: &str, auth: Option<&str>) -> (StatusCode, Value) {
    let mut req = Request::builder().method("DELETE").uri(path);
    if let Some(t) = auth {
        req = req.header("authorization", t);
    }
    let req = req.body(Body::empty()).expect("request");
    let response = app.clone().oneshot(req).await.expect("oneshot");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .expect("body");
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));
    (status, json)
}

/// POST with empty body (routes that have no `Json` extractor).
async fn post_empty(app: &Router, path: &str, auth: Option<&str>) -> (StatusCode, Value) {
    let mut req = Request::builder().method("POST").uri(path);
    if let Some(t) = auth {
        req = req.header("authorization", t);
    }
    let req = req.body(Body::empty()).expect("request");
    let response = app.clone().oneshot(req).await.expect("oneshot");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .expect("body");
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));
    (status, json)
}

async fn post_json(
    app: &Router,
    path: &str,
    auth: Option<&str>,
    payload: Value,
) -> (StatusCode, Value) {
    post_json_with_headers(app, path, auth, &[], payload).await
}

async fn post_json_with_headers(
    app: &Router,
    path: &str,
    auth: Option<&str>,
    extra_headers: &[(&str, &str)],
    payload: Value,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json");
    if let Some(t) = auth {
        req = req.header("authorization", t);
    }
    for (k, v) in extra_headers {
        req = req.header(*k, *v);
    }
    let req = req
        .body(Body::from(payload.to_string()))
        .expect("request");
    let response = app.clone().oneshot(req).await.expect("oneshot");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .expect("body");
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));
    (status, json)
}

async fn cleanup_session_data(pool: &sqlx::MySqlPool, session_id: &str) {
    let _ = sqlx::query("DELETE FROM ctx_decision_audits WHERE session_id = ?")
        .bind(session_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM ctx_snapshots WHERE session_id = ?")
        .bind(session_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM agent_events WHERE session_id = ?")
        .bind(session_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM agent_sessions WHERE session_id = ?")
        .bind(session_id)
        .execute(pool)
        .await;
}

async fn cleanup_edge_registry(
    pool: &sqlx::MySqlPool,
    user_id: &str,
    edge_agent_id: &str,
) {
    let _ = sqlx::query(
        "DELETE FROM edge_agent_registry WHERE user_id = ? AND edge_agent_id = ?",
    )
    .bind(user_id)
    .bind(edge_agent_id)
    .execute(pool)
    .await;
}

fn row_get_str(r: &MySqlRow, col: &str) -> String {
    r.try_get::<String, _>(col)
        .unwrap_or_else(|_| panic!("missing string column {col}"))
}

fn row_get_opt_str(r: &MySqlRow, col: &str) -> Option<String> {
    r.try_get::<Option<String>, _>(col).ok().flatten()
}

fn row_get_opt_i64(r: &MySqlRow, col: &str) -> Option<i64> {
    r.try_get::<Option<i64>, _>(col).ok().flatten()
}

#[tokio::test]
#[ignore = "live MatrixOne + full secrets; MO_AGENT_SYSTEM_MATRIX_E2E=1 — see module doc"]
async fn product_matrix_api_journey_hits_multiple_tables() {
    require_system_e2e_env();
    dotenvy::dotenv().ok();

    const PASSWORD: &str = "E2e-matrix-pass-9";

    let settings = AppSettings::from_env().expect("AppSettings::from_env (see astra-server env)");
    let url = settings.matrixone.database_url();
    let pool = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect MatrixOne for assertions");

    let memoria = Arc::new(E2eMemoriaStub::default());
    let state = build_server_state(settings.clone())
        .await
        .expect("build_server_state")
        .with_memoria_forwarder(memoria.clone());

    let app = build_app(state);

    let suffix = Uuid::new_v4().simple().to_string();
    let username = format!("prod_matrix_{suffix}");
    let email = format!("prod_matrix_{suffix}@e2e.test");
    let edge_agent_id = format!("edge-{suffix}");

    let (st_h, health) = get_json(&app, "/health", None, &[]).await;
    assert_eq!(st_h, StatusCode::OK, "health: {health}");

    let (st_root, root) = get_json(&app, "/", None, &[]).await;
    assert_eq!(st_root, StatusCode::OK, "root: {root}");

    let (st_reg, reg) = post_json(
        &app,
        "/auth/register",
        None,
        json!({
            "username": username,
            "email": email,
            "password": PASSWORD,
            "display_name": "Product Matrix E2E"
        }),
    )
    .await;
    assert_eq!(st_reg, StatusCode::CREATED, "register: {reg}");
    let access = reg["access_token"].as_str().expect("access_token");
    let user_id = reg["user_id"].as_str().expect("user_id").to_string();
    let mut refresh_token = reg["refresh_token"]
        .as_str()
        .expect("refresh_token")
        .to_string();
    let mut auth_header = format!("Bearer {access}");

    let auth_row = sqlx::query("SELECT username, email FROM auth_users WHERE user_id = ?")
        .bind(&user_id)
        .fetch_optional(&pool)
        .await
        .expect("auth_users lookup");
    let auth_row = auth_row.expect("auth_users row after register");
    assert_eq!(
        auth_row.try_get::<String, _>("username").ok().as_deref(),
        Some(username.as_str())
    );

    let (st_login, login_j) = post_json(
        &app,
        "/auth/login",
        None,
        json!({ "username": username, "password": PASSWORD }),
    )
    .await;
    assert_eq!(st_login, StatusCode::OK, "login: {login_j}");
    assert!(
        login_j["access_token"].as_str().is_some(),
        "login access_token: {login_j}"
    );

    let (st_me, me) = get_json(&app, "/auth/me", Some(&auth_header), &[]).await;
    assert_eq!(st_me, StatusCode::OK, "me: {me}");
    assert_eq!(me["user_id"].as_str(), Some(user_id.as_str()));

    let (st_ref, ref_j) = post_json(
        &app,
        "/auth/refresh",
        None,
        json!({ "refresh_token": refresh_token }),
    )
    .await;
    assert_eq!(st_ref, StatusCode::OK, "refresh: {ref_j}");
    let access2 = ref_j["access_token"].as_str().expect("post-refresh access_token");
    refresh_token = ref_j["refresh_token"]
        .as_str()
        .expect("post-refresh refresh_token")
        .to_string();
    auth_header = format!("Bearer {access2}");

    let (st_me2, me2) = get_json(&app, "/auth/me", Some(&auth_header), &[]).await;
    assert_eq!(st_me2, StatusCode::OK, "me after refresh: {me2}");
    assert_eq!(me2["user_id"].as_str(), Some(user_id.as_str()));

    let (st_learn_h, learn_h) = get_json(&app, "/api/v1/learning/health", None, &[]).await;
    assert_eq!(st_learn_h, StatusCode::OK, "learning health: {learn_h}");

    let (st_sess, sess) = post_json(
        &app,
        "/sessions",
        Some(&auth_header),
        json!({ "title": "product matrix session", "metadata": { "suite": "matrix" } }),
    )
    .await;
    assert_eq!(st_sess, StatusCode::CREATED, "create session: {sess}");
    let session_id = sess["session_id"].as_str().expect("session_id").to_string();

    cleanup_session_data(&pool, &session_id).await;
    cleanup_edge_registry(&pool, &user_id, &edge_agent_id).await;

    let (st_list_s, list_s) = get_json(&app, "/sessions", Some(&auth_header), &[]).await;
    assert_eq!(st_list_s, StatusCode::OK, "list sessions: {list_s}");
    assert!(
        list_s["sessions"]
            .as_array()
            .is_some_and(|a| a.iter().any(|s| s["session_id"].as_str() == Some(&session_id))),
        "session not listed: {list_s}"
    );

    let (st_get_s, got_s) = get_json(
        &app,
        &format!("/sessions/{session_id}"),
        Some(&auth_header),
        &[],
    )
    .await;
    assert_eq!(st_get_s, StatusCode::OK, "get session: {got_s}");

    let (st_put_s, put_s) = put_json(
        &app,
        &format!("/sessions/{session_id}"),
        Some(&auth_header),
        json!({ "title": "product matrix session (updated)" }),
    )
    .await;
    assert_eq!(st_put_s, StatusCode::OK, "put session: {put_s}");
    assert_eq!(
        put_s["title"].as_str(),
        Some("product matrix session (updated)")
    );

    let (st_close, closed) = post_empty(
        &app,
        &format!("/sessions/{session_id}/close"),
        Some(&auth_header),
    )
    .await;
    assert_eq!(st_close, StatusCode::OK, "close session: {closed}");
    assert_eq!(closed["status"].as_str(), Some("closed"), "close response: {closed}");

    let sess_status = sqlx::query("SELECT status FROM agent_sessions WHERE session_id = ?")
        .bind(&session_id)
        .fetch_one(&pool)
        .await
        .expect("session status after close");
    assert_eq!(
        sess_status.try_get::<String, _>("status").ok().as_deref(),
        Some("closed"),
        "agent_sessions.status after POST .../close"
    );

    let (st_res, resm) = post_empty(
        &app,
        &format!("/sessions/{session_id}/resume"),
        Some(&auth_header),
    )
    .await;
    assert_eq!(st_res, StatusCode::OK, "resume session: {resm}");
    assert_eq!(resm["status"].as_str(), Some("active"), "resume response: {resm}");

    let sess_active = sqlx::query("SELECT status FROM agent_sessions WHERE session_id = ?")
        .bind(&session_id)
        .fetch_one(&pool)
        .await
        .expect("session status after resume");
    assert_eq!(
        sess_active.try_get::<String, _>("status").ok().as_deref(),
        Some("active"),
        "agent_sessions.status after POST .../resume"
    );

    let (st_act, act) = get_json(
        &app,
        &format!("/sessions/{session_id}/activity"),
        Some(&auth_header),
        &[],
    )
    .await;
    assert_eq!(st_act, StatusCode::OK, "session activity: {act}");

    let (st_au_sum, au_sum) = get_json(
        &app,
        &format!("/sessions/{session_id}/audit/summary"),
        Some(&auth_header),
        &[],
    )
    .await;
    assert_eq!(st_au_sum, StatusCode::OK, "audit summary: {au_sum}");

    let (st_au_stats, au_stats) = get_json(&app, "/audit/stats", Some(&auth_header), &[]).await;
    assert_eq!(st_au_stats, StatusCode::OK, "audit stats: {au_stats}");

    let (st_au_sess, au_sess) = get_json(
        &app,
        "/audit/sessions?page=1&per_page=10",
        Some(&auth_header),
        &[],
    )
    .await;
    assert_eq!(st_au_sess, StatusCode::OK, "audit sessions: {au_sess}");

    let (st_au_turns, au_turns) = get_json(
        &app,
        &format!("/sessions/{session_id}/audit/turns?page=1&per_page=20"),
        Some(&auth_header),
        &[],
    )
    .await;
    assert_eq!(st_au_turns, StatusCode::OK, "audit turns: {au_turns}");

    let (st_au_sess_tools, au_sess_tools) = get_json(
        &app,
        &format!("/sessions/{session_id}/audit/tools"),
        Some(&auth_header),
        &[],
    )
    .await;
    assert_eq!(
        st_au_sess_tools,
        StatusCode::OK,
        "session audit tools: {au_sess_tools}"
    );

    let (st_au_errs, au_errs) = get_json(
        &app,
        &format!("/sessions/{session_id}/audit/errors"),
        Some(&auth_header),
        &[],
    )
    .await;
    assert_eq!(st_au_errs, StatusCode::OK, "session audit errors: {au_errs}");

    let (st_au_tools, au_tools) = get_json(&app, "/audit/tools", Some(&auth_header), &[]).await;
    assert_eq!(st_au_tools, StatusCode::OK, "cross-session audit tools: {au_tools}");

    let (st_mkt, mkt_j) = get_json(
        &app,
        "/marketplace/installed?limit=20&offset=0",
        Some(&auth_header),
        &[],
    )
    .await;
    assert_eq!(st_mkt, StatusCode::OK, "marketplace installed: {mkt_j}");

    let xuid = &[("x-user-id", user_id.as_str())];
    let (st_gates, gates_j) = get_json(&app, "/evaluation/gates?limit=10", None, xuid).await;
    assert_eq!(st_gates, StatusCode::OK, "evaluation gates: {gates_j}");

    let (st_cal, cal_j) = get_json(
        &app,
        "/evaluation/calibration?days=7",
        None,
        xuid,
    )
    .await;
    assert_eq!(st_cal, StatusCode::OK, "evaluation calibration: {cal_j}");

    let (st_scores, scores_j) = get_json(
        &app,
        "/evaluation/sessions/scores?limit=10&min_score=0",
        None,
        xuid,
    )
    .await;
    assert_eq!(st_scores, StatusCode::OK, "evaluation session scores: {scores_j}");
    assert!(
        scores_j["sessions"].is_array(),
        "session scores payload: {scores_j}"
    );

    let (st_qt, qt_j) = get_json(
        &app,
        "/evaluation/quality/trend?days=7",
        None,
        xuid,
    )
    .await;
    assert_eq!(st_qt, StatusCode::OK, "evaluation quality trend: {qt_j}");

    let (st_slo, slo_j) = get_json(
        &app,
        "/evaluation/slo/dashboard?period_days=7",
        None,
        xuid,
    )
    .await;
    assert_eq!(st_slo, StatusCode::OK, "evaluation slo dashboard: {slo_j}");

    let (st_mh, mh_j) = get_json(&app, "/evaluation/memory-health", None, xuid).await;
    assert_eq!(st_mh, StatusCode::OK, "evaluation memory-health: {mh_j}");

    let (st_mm, mm_j) = get_json(&app, "/evaluation/memory-metrics", None, xuid).await;
    assert_eq!(st_mm, StatusCode::OK, "evaluation memory-metrics: {mm_j}");

    let (st_agent, agent_j) = post_json(
        &app,
        "/agents",
        Some(&auth_header),
        json!({
            "name": "matrix-crud-agent",
            "agent_config": { "suite": "matrix" },
            "data_source": { "type": "matrixone", "database": "astra_runtime" }
        }),
    )
    .await;
    assert_eq!(st_agent, StatusCode::CREATED, "create agent: {agent_j}");
    let agent_id = agent_j["agent_id"].as_str().expect("agent_id").to_string();

    let agent_db = sqlx::query(
        "SELECT agent_name, owner_user_id FROM agent_agents WHERE agent_id = ?",
    )
    .bind(&agent_id)
    .fetch_optional(&pool)
    .await
    .expect("agent_agents select");
    let agent_db = agent_db.expect("agent row");
    assert_eq!(
        agent_db.try_get::<String, _>("agent_name").ok().as_deref(),
        Some("matrix-crud-agent")
    );
    assert_eq!(
        agent_db.try_get::<String, _>("owner_user_id").ok().as_deref(),
        Some(user_id.as_str())
    );

    let (st_get_ag, got_ag) = get_json(
        &app,
        &format!("/agents/{agent_id}"),
        Some(&auth_header),
        &[],
    )
    .await;
    assert_eq!(st_get_ag, StatusCode::OK, "get agent: {got_ag}");
    assert_eq!(got_ag["name"].as_str(), Some("matrix-crud-agent"));

    let (st_put_ag, put_ag) = put_json(
        &app,
        &format!("/agents/{agent_id}"),
        Some(&auth_header),
        json!({ "name": "matrix-crud-agent-renamed" }),
    )
    .await;
    assert_eq!(st_put_ag, StatusCode::OK, "put agent: {put_ag}");
    assert_eq!(
        put_ag["name"].as_str(),
        Some("matrix-crud-agent-renamed"),
        "agent update response: {put_ag}"
    );
    let agent_renamed = sqlx::query("SELECT agent_name FROM agent_agents WHERE agent_id = ?")
        .bind(&agent_id)
        .fetch_one(&pool)
        .await
        .expect("agent_agents after rename");
    assert_eq!(
        agent_renamed.try_get::<String, _>("agent_name").ok().as_deref(),
        Some("matrix-crud-agent-renamed")
    );

    let trust_path = format!("/evaluation/trust-report?agent_id={agent_id}&days=7");
    let (st_trust, trust_j) = get_json(&app, &trust_path, None, xuid).await;
    assert_eq!(st_trust, StatusCode::OK, "evaluation trust-report: {trust_j}");

    let slo_hist = format!("/evaluation/slo/{agent_id}/history?days=7");
    let (st_slo_hist, slo_hist_j) = get_json(&app, &slo_hist, None, xuid).await;
    assert_eq!(
        st_slo_hist,
        StatusCode::OK,
        "evaluation slo history: {slo_hist_j}"
    );

    let obs_path = format!("/evaluation/observability/metrics?agent_id={agent_id}&days=7");
    let (st_obs, obs_j) = get_json(&app, &obs_path, None, xuid).await;
    assert_eq!(
        st_obs,
        StatusCode::OK,
        "evaluation observability metrics: {obs_j}"
    );

    let (st_ev, ev_j) = post_json(
        &app,
        "/events",
        Some(&auth_header),
        json!({
            "session_id": session_id,
            "event_type": "e2e_capability_probe",
            "content": "manual event for matrix",
            "agent_id": agent_id,
            "metadata": { "source": "e2e_matrix" }
        }),
    )
    .await;
    assert_eq!(st_ev, StatusCode::CREATED, "create event: {ev_j}");
    let manual_event_id = ev_j["event_id"].as_str().expect("event_id").to_string();

    let (st_ev_one, ev_one) = get_json(
        &app,
        &format!("/events/{manual_event_id}"),
        Some(&auth_header),
        &[],
    )
    .await;
    assert_eq!(st_ev_one, StatusCode::OK, "get event by id: {ev_one}");
    assert_eq!(ev_one["event_id"].as_str(), Some(manual_event_id.as_str()));
    let causal_chain_id = ev_one["causal_chain_id"]
        .as_str()
        .expect("causal_chain_id on event")
        .to_string();

    let (st_cc, cc_j) = get_json(
        &app,
        &format!("/events/causal-chain/{causal_chain_id}"),
        Some(&auth_header),
        &[],
    )
    .await;
    assert_eq!(st_cc, StatusCode::OK, "causal chain events: {cc_j}");
    assert!(
        cc_j.as_array().is_some_and(|a| {
            a.iter()
                .any(|e| e["event_id"].as_str() == Some(manual_event_id.as_str()))
        }),
        "manual event missing from causal chain: {cc_j}"
    );

    let list_ev_path = format!("/events?session_id={session_id}&limit=20&offset=0");
    let (st_list_ev, list_ev) = get_json(&app, &list_ev_path, Some(&auth_header), &[]).await;
    assert_eq!(st_list_ev, StatusCode::OK, "list events (query): {list_ev}");
    assert!(
        list_ev["events"].as_array().is_some_and(|arr| {
            arr.iter()
                .any(|e| e["event_id"].as_str() == Some(manual_event_id.as_str()))
        }),
        "manual event missing from GET /events list: {list_ev}"
    );

    let (st_ev_sess, ev_sess) = get_json(
        &app,
        &format!("/events/session/{session_id}?limit=50&offset=0"),
        Some(&auth_header),
        &[],
    )
    .await;
    assert_eq!(st_ev_sess, StatusCode::OK, "session events: {ev_sess}");
    assert!(
        ev_sess["events"].as_array().is_some_and(|arr| {
            arr.iter()
                .any(|e| e["event_id"].as_str() == Some(manual_event_id.as_str()))
        }),
        "manual event missing in list: {ev_sess}"
    );

    let (st_dv_chain, dv_chain) = get_json(
        &app,
        &format!("/data-versioning/lineage/{manual_event_id}/chain"),
        Some(&auth_header),
        &[],
    )
    .await;
    assert_eq!(
        st_dv_chain,
        StatusCode::OK,
        "data-versioning lineage chain: {dv_chain}"
    );
    assert!(
        dv_chain.as_array().is_some_and(|a| !a.is_empty()),
        "expected non-empty lineage for manual event: {dv_chain}"
    );

    let (st_dv_up, dv_up) = get_json(
        &app,
        &format!("/data-versioning/lineage/{manual_event_id}/upstream"),
        Some(&auth_header),
        &[],
    )
    .await;
    assert_eq!(
        st_dv_up,
        StatusCode::OK,
        "data-versioning upstream lineage: {dv_up}"
    );

    let (st_ctx, ctx_j) = post_json(
        &app,
        "/context",
        Some(&auth_header),
        json!({
            "session_id": session_id,
            "event_id": manual_event_id,
            "context_data": { "window": "matrix", "tokens": 42 }
        }),
    )
    .await;
    assert_eq!(st_ctx, StatusCode::CREATED, "context snapshot: {ctx_j}");
    let context_capture_id = ctx_j["context_capture_id"]
        .as_str()
        .expect("context_capture_id")
        .to_string();

    let snap_row = sqlx::query(
        "SELECT session_id, event_id FROM ctx_snapshots WHERE context_capture_id = ?",
    )
    .bind(&context_capture_id)
    .fetch_optional(&pool)
    .await
    .expect("ctx_snapshots");
    let snap_row = snap_row.expect("ctx_snapshots row");
    assert_eq!(
        snap_row.try_get::<String, _>("session_id").ok().as_deref(),
        Some(session_id.as_str())
    );
    assert_eq!(
        snap_row.try_get::<String, _>("event_id").ok().as_deref(),
        Some(manual_event_id.as_str())
    );

    let (st_get_ctx, got_ctx) = get_json(
        &app,
        &format!("/context/{context_capture_id}"),
        Some(&auth_header),
        &[],
    )
    .await;
    assert_eq!(st_get_ctx, StatusCode::OK, "get snapshot: {got_ctx}");

    let (st_dec, dec_j) = post_json(
        &app,
        "/decisions",
        Some(&auth_header),
        json!({
            "session_id": session_id,
            "event_id": manual_event_id,
            "context_capture_id": context_capture_id,
            "decision_type": "e2e_matrix_decision",
            "decision_output": { "choice": "path_a" },
            "model_params": { "temperature": 0.1 }
        }),
    )
    .await;
    assert_eq!(st_dec, StatusCode::CREATED, "record decision: {dec_j}");
    let decision_id = dec_j["decision_id"].as_str().expect("decision_id").to_string();

    let dec_row = sqlx::query(
        "SELECT session_id, decision_type FROM ctx_decision_audits WHERE decision_id = ?",
    )
    .bind(&decision_id)
    .fetch_optional(&pool)
    .await
    .expect("ctx_decision_audits");
    let dec_row = dec_row.expect("decision row");
    assert_eq!(
        dec_row.try_get::<String, _>("session_id").ok().as_deref(),
        Some(session_id.as_str())
    );
    assert_eq!(
        dec_row.try_get::<String, _>("decision_type").ok().as_deref(),
        Some("e2e_matrix_decision")
    );

    let (st_get_dec, got_dec) = get_json(
        &app,
        &format!("/decisions/{decision_id}"),
        Some(&auth_header),
        &[],
    )
    .await;
    assert_eq!(st_get_dec, StatusCode::OK, "get decision: {got_dec}");

    let (st_audit, audit) = get_json(
        &app,
        &format!("/decisions/{decision_id}/audit"),
        Some(&auth_header),
        &[],
    )
    .await;
    assert_eq!(st_audit, StatusCode::OK, "decision audit: {audit}");

    let list_dec_path = format!("/decisions?session_id={session_id}&limit=20&offset=0");
    let (st_list_d, list_d) = get_json(&app, &list_dec_path, Some(&auth_header), &[]).await;
    assert_eq!(st_list_d, StatusCode::OK, "list decisions: {list_d}");
    assert!(
        list_d["decisions"].as_array().is_some_and(|arr| {
            arr.iter()
                .any(|d| d["decision_id"].as_str() == Some(decision_id.as_str()))
        }),
        "decision not in list: {list_d}"
    );

    let (st_mem_s, mem_s) = post_json(
        &app,
        "/memory/store",
        Some(&auth_header),
        json!({ "content": "matrix e2e memory", "memory_type": "semantic" }),
    )
    .await;
    assert_eq!(st_mem_s, StatusCode::OK, "memory store: {mem_s}");

    let (st_mem_r, mem_r) = post_json(
        &app,
        "/memory/retrieve",
        Some(&auth_header),
        json!({ "query": "matrix" }),
    )
    .await;
    assert_eq!(st_mem_r, StatusCode::OK, "memory retrieve: {mem_r}");

    let (st_mem_q, mem_q) = post_json(
        &app,
        "/memory/search",
        Some(&auth_header),
        json!({ "query": "matrix", "top_k": 3 }),
    )
    .await;
    assert_eq!(st_mem_q, StatusCode::OK, "memory search: {mem_q}");

    let (st_mem_p, mem_p) = post_json(
        &app,
        "/memory/purge",
        Some(&auth_header),
        json!({ "memory_id": "e2e-purge-dummy" }),
    )
    .await;
    assert_eq!(st_mem_p, StatusCode::OK, "memory purge: {mem_p}");

    assert!(
        !memoria.calls.lock().await.is_empty(),
        "memoria forwarder should see at least one proxy call"
    );

    let edge_reg = Request::builder()
        .method("POST")
        .uri("/agents/edge")
        .header("authorization", &auth_header)
        .header("content-type", "application/json")
        .header("x-mo-edge-id", "matrix-e2e-edge")
        .body(Body::from(
            json!({
                "edge_agent_id": edge_agent_id,
                "hostname": "matrix-e2e-host",
                "capabilities": { "tools": ["read_file"] }
            })
            .to_string(),
        ))
        .expect("edge register body");
    let edge_resp = app.clone().oneshot(edge_reg).await.expect("edge reg");
    assert_eq!(edge_resp.status(), StatusCode::OK, "edge register status");

    let edge_db = sqlx::query(
        "SELECT user_id, edge_id FROM edge_agent_registry WHERE user_id = ? AND edge_agent_id = ?",
    )
    .bind(&user_id)
    .bind(&edge_agent_id)
    .fetch_optional(&pool)
    .await
    .expect("edge registry select");
    let edge_db = edge_db.expect("edge_agent_registry row");
    assert_eq!(
        edge_db.try_get::<String, _>("edge_id").ok().as_deref(),
        Some("matrix-e2e-edge")
    );

    let (st_hb, hb) = post_json_with_headers(
        &app,
        "/agents/edge/heartbeat",
        Some(&auth_header),
        &[("x-mo-edge-id", "matrix-e2e-edge")],
        json!({ "edge_agent_id": edge_agent_id }),
    )
    .await;
    assert_eq!(st_hb, StatusCode::OK, "edge heartbeat: {hb}");

    let (st_tool, tool_j) = post_json(
        &app,
        "/tools/result",
        Some(&auth_header),
        json!({
            "request_id": "matrix-tool-req-1",
            "status": "ok",
            "output": "done",
            "duration_ms": 12
        }),
    )
    .await;
    assert_eq!(st_tool, StatusCode::OK, "tools/result: {tool_j}");
    assert_eq!(tool_j["ok"], true);

    let (st_appr, appr_j) = post_json(
        &app,
        "/approval/respond",
        Some(&auth_header),
        json!({
            "request_id": "matrix-appr-1",
            "decision": "allow",
            "reason": "e2e"
        }),
    )
    .await;
    assert_eq!(st_appr, StatusCode::OK, "approval/respond: {appr_j}");

    let (st_runs, runs) = get_json(&app, "/runs", Some(&auth_header), &[]).await;
    assert_eq!(st_runs, StatusCode::OK, "list runs: {runs}");

    let (st_wf, wf_j) = get_json(&app, "/workflows", Some(&auth_header), &[]).await;
    assert_eq!(st_wf, StatusCode::OK, "list workflows: {wf_j}");
    assert!(wf_j.is_array(), "workflows JSON should be an array: {wf_j}");

    let (st_cpl, cpl_j) = get_json(
        &app,
        "/data-versioning/checkpoints",
        Some(&auth_header),
        &[],
    )
    .await;
    assert_eq!(st_cpl, StatusCode::OK, "list checkpoints (read-only): {cpl_j}");
    assert!(
        cpl_j.is_array(),
        "checkpoints list should be a JSON array: {cpl_j}"
    );

    let (st_job, job_j) = post_json(
        &app,
        "/jobs",
        Some(&auth_header),
        json!({
            "job_type": "matrix_e2e",
            "inputs": { "suite": "matrix" },
            "gpu_required": false,
            "timeout_seconds": 120
        }),
    )
    .await;
    assert_eq!(st_job, StatusCode::OK, "submit job: {job_j}");
    let job_id = job_j["job_id"].as_str().expect("job_id").to_string();

    let (st_gj, gj) = get_json(
        &app,
        &format!("/jobs/{job_id}"),
        Some(&auth_header),
        &[],
    )
    .await;
    assert_eq!(st_gj, StatusCode::OK, "get job: {gj}");
    assert_eq!(gj["status"].as_str(), Some("pending"));

    let (st_wh, wh_j) = post_json(
        &app,
        "/jobs/webhook",
        None,
        json!({
            "job_id": job_id,
            "status": "completed",
            "result": { "ok": true },
            "error": null
        }),
    )
    .await;
    assert_eq!(st_wh, StatusCode::OK, "job webhook: {wh_j}");

    let (st_gj2, gj2) = get_json(
        &app,
        &format!("/jobs/{job_id}"),
        Some(&auth_header),
        &[],
    )
    .await;
    assert_eq!(st_gj2, StatusCode::OK, "get job after webhook: {gj2}");
    assert_eq!(gj2["status"].as_str(), Some("completed"));

    let sb_name = format!("sb_{suffix}");
    let (st_sb, sb_j) = post_json(
        &app,
        "/sandbox",
        Some(&auth_header),
        json!({ "name": sb_name, "description": "matrix e2e sandbox" }),
    )
    .await;
    assert_eq!(st_sb, StatusCode::CREATED, "create sandbox: {sb_j}");

    let sb_row = sqlx::query(
        "SELECT user_id, status FROM infra_sandbox_metadata WHERE sandbox_name = ?",
    )
    .bind(&sb_name)
    .fetch_optional(&pool)
    .await
    .expect("sandbox select");
    let sb_row = sb_row.expect("infra_sandbox_metadata row");
    assert_eq!(
        sb_row.try_get::<String, _>("user_id").ok().as_deref(),
        Some(user_id.as_str())
    );

    let (st_sbl, sbl) = get_json(&app, "/sandbox", Some(&auth_header), &[]).await;
    assert_eq!(st_sbl, StatusCode::OK, "list sandboxes: {sbl}");
    assert!(
        sbl["sandboxes"].as_array().is_some_and(|a| {
            a.iter()
                .any(|s| s["sandbox_name"].as_str() == Some(sb_name.as_str()))
        }),
        "sandbox not listed: {sbl}"
    );

    let (st_sbg, sbg) = get_json(
        &app,
        &format!("/sandbox/{sb_name}"),
        Some(&auth_header),
        &[],
    )
    .await;
    assert_eq!(st_sbg, StatusCode::OK, "get sandbox: {sbg}");

    let st_sbd = delete_no_content(
        &app,
        &format!("/sandbox/{sb_name}"),
        Some(&auth_header),
    )
    .await;
    assert_eq!(st_sbd, StatusCode::NO_CONTENT, "delete sandbox");

    let sb_gone = sqlx::query("SELECT 1 FROM infra_sandbox_metadata WHERE sandbox_name = ?")
        .bind(&sb_name)
        .fetch_optional(&pool)
        .await
        .expect("sandbox gone");
    assert!(
        sb_gone.is_none(),
        "sandbox row should be removed after DELETE"
    );

    let (st_tr, tr_j) = post_json(
        &app,
        "/triggers",
        Some(&auth_header),
        json!({
            "trigger_type": "webhook",
            "name": format!("wh_{suffix}"),
            "agent_id": agent_id,
            "user_input": "matrix e2e webhook trigger",
            "session_id": session_id,
            "context": { "suite": "matrix" }
        }),
    )
    .await;
    assert_eq!(st_tr, StatusCode::OK, "create webhook trigger: {tr_j}");
    let trigger_id = tr_j["trigger_id"].as_str().expect("trigger_id").to_string();
    let wh_secret = tr_j["secret"].as_str().expect("webhook secret");

    let (st_tr_l, tr_l) = get_json(&app, "/triggers", Some(&auth_header), &[]).await;
    assert_eq!(st_tr_l, StatusCode::OK, "list triggers: {tr_l}");
    assert!(
        tr_l.as_array().is_some_and(|a| {
            a.iter()
                .any(|t| t["trigger_id"].as_str() == Some(trigger_id.as_str()))
        }),
        "trigger not listed: {tr_l}"
    );

    let (st_fire, fire_j) = post_json(
        &app,
        &format!("/triggers/{trigger_id}/fire"),
        None,
        json!({ "secret": wh_secret, "payload": { "hello": "matrix" } }),
    )
    .await;
    assert_eq!(st_fire, StatusCode::OK, "fire webhook: {fire_j}");
    assert_eq!(fire_j["fired"], true);

    let (st_tr_d, tr_d) = delete_json(
        &app,
        &format!("/triggers/{trigger_id}"),
        Some(&auth_header),
    )
    .await;
    assert_eq!(st_tr_d, StatusCode::OK, "delete trigger: {tr_d}");

    let trig_gone = sqlx::query("SELECT 1 FROM wf_triggers WHERE trigger_id = ?")
        .bind(&trigger_id)
        .fetch_optional(&pool)
        .await
        .expect("trigger gone");
    assert!(
        trig_gone.is_none(),
        "wf_triggers row should be deleted: {trigger_id}"
    );

    let (st_sks, sks_j) = get_json(&app, "/skills", Some(&auth_header), &[]).await;
    assert_eq!(st_sks, StatusCode::OK, "list skills: {sks_j}");
    assert!(
        sks_j["skills"].is_array(),
        "skills list record: {sks_j}"
    );

    let (st_sst, sst_j) = get_json(
        &app,
        "/skills/status?per_group=50",
        Some(&auth_header),
        &[],
    )
    .await;
    assert_eq!(st_sst, StatusCode::OK, "skills status: {sst_j}");

    let (st_intro, intro_j) = get_json(
        &app,
        "/introspection/skills",
        Some(&auth_header),
        &[],
    )
    .await;
    assert_eq!(st_intro, StatusCode::OK, "introspection skills: {intro_j}");

    let (st_route, route_j) = post_json(
        &app,
        "/chat/route",
        Some(&auth_header),
        json!({ "query": "run tests and fix failures" }),
    )
    .await;
    assert_eq!(st_route, StatusCode::OK, "chat/route: {route_j}");

    let (st_sig, sig) = get_json(
        &app,
        "/api/v1/learning/signals",
        Some(&auth_header),
        &[],
    )
    .await;
    assert_eq!(st_sig, StatusCode::OK, "learning signals: {sig}");

    let (st_lrn_stats, lrn_stats) = get_json(
        &app,
        "/api/v1/learning/stats",
        Some(&auth_header),
        &[],
    )
    .await;
    assert_eq!(st_lrn_stats, StatusCode::OK, "learning stats: {lrn_stats}");

    let (st_drift, drift) = get_json(
        &app,
        "/evaluation/drift",
        None,
        &[("x-user-id", user_id.as_str())],
    )
    .await;
    assert_eq!(st_drift, StatusCode::OK, "evaluation drift: {drift}");

    let reflect_path = format!("/chat/session/{session_id}/reflect");
    let (st_refl, refl) = get_json(&app, &reflect_path, Some(&auth_header), &[]).await;
    assert_eq!(st_refl, StatusCode::OK, "reflect: {refl}");

    let trace_path = format!("/chat/session/{session_id}/decision-trace");
    let (st_trace, trace) = get_json(&app, &trace_path, Some(&auth_header), &[]).await;
    assert_eq!(st_trace, StatusCode::OK, "decision-trace: {trace}");

    const LLM_TEXT: &str = "product-matrix-e2e-reply";
    let chat_body = json!({
        "agent_id": agent_id,
        "session_id": session_id,
        "messages": [{ "role": "user", "content": "matrix journey ping" }],
        "edge_tools": [],
        "test_llm_rounds": [{
            "full_text": LLM_TEXT,
            "reasoning": "",
            "usage": { "prompt": 5, "completion": 15, "total": 20 }
        }]
    });

    let test_secret = std::env::var("MO_AGENT_BRIDGE_TEST_SECRET").expect("bridge test secret");
    let chat_req = Request::builder()
        .method("POST")
        .uri("/chat/turn")
        .header("authorization", &auth_header)
        .header("content-type", "application/json")
        .header("x-mo-bridge-test-secret", &test_secret)
        .body(Body::from(chat_body.to_string()))
        .expect("chat request");

    let response = app.clone().oneshot(chat_req).await.expect("chat oneshot");
    assert_eq!(response.status(), StatusCode::OK, "chat/turn should return 200");

    let mut stream = response.into_body().into_data_stream();
    let mut acc = Vec::new();
    let mut saw_turn_complete = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("sse chunk");
        acc.extend_from_slice(&chunk);
        if String::from_utf8_lossy(&acc).contains("turn_complete") {
            saw_turn_complete = true;
            break;
        }
    }
    assert!(
        saw_turn_complete,
        "expected turn_complete in SSE, got: {}",
        String::from_utf8_lossy(&acc)
    );

    tokio::time::sleep(std::time::Duration::from_millis(900)).await;

    let recs = sqlx::query(
        "SELECT event_id, session_id, user_id, event_type, content, parent_event_id, \
         causal_chain_id, token_input, token_output, token_total, llm_model_used, reasoning_content \
         FROM agent_events WHERE session_id = ? ORDER BY created_at ASC",
    )
    .bind(&session_id)
    .fetch_all(&pool)
    .await
    .expect("select agent_events");

    let user_q = recs
        .iter()
        .find(|r| {
            row_get_str(r, "event_type") == "user_query"
                && row_get_str(r, "content").contains("matrix journey ping")
        })
        .expect("user_query event from chat/turn");
    assert_eq!(row_get_str(user_q, "session_id"), session_id);
    assert_eq!(row_get_str(user_q, "user_id"), user_id);
    assert!(!row_get_str(user_q, "event_id").is_empty());
    let cc = row_get_opt_str(user_q, "causal_chain_id").unwrap_or_default();
    assert!(!cc.is_empty(), "causal_chain_id should be set on user_query");

    let llm = recs
        .iter()
        .find(|r| {
            row_get_str(r, "event_type") == "llm_response"
                && row_get_str(r, "content").contains(LLM_TEXT)
        })
        .expect("llm_response from chat/turn with expected assistant text");
    assert_eq!(row_get_str(llm, "session_id"), session_id);
    assert_eq!(row_get_str(llm, "user_id"), user_id);
    let llm_content = row_get_str(llm, "content");
    assert!(
        llm_content.contains(LLM_TEXT),
        "llm_response content: {llm_content}"
    );
    let uq_event_id = row_get_str(user_q, "event_id");
    assert_eq!(
        row_get_opt_str(llm, "parent_event_id").as_deref(),
        Some(uq_event_id.as_str()),
        "llm_response should parent to user_query"
    );
    assert_eq!(row_get_opt_i64(llm, "token_input"), Some(5));
    assert_eq!(row_get_opt_i64(llm, "token_output"), Some(15));
    assert_eq!(row_get_opt_i64(llm, "token_total"), Some(20));
    assert_eq!(
        row_get_opt_str(llm, "llm_model_used").as_deref(),
        Some("bridge-e2e-mock")
    );
    assert!(
        row_get_opt_str(llm, "reasoning_content")
            .map(|s| s.is_empty())
            .unwrap_or(true),
        "reasoning_content should be empty for mock round with reasoning: \"\""
    );

    cleanup_session_data(&pool, &session_id).await;
    cleanup_edge_registry(&pool, &user_id, &edge_agent_id).await;

    let del_agent = delete_no_content(
        &app,
        &format!("/agents/{agent_id}"),
        Some(&auth_header),
    )
    .await;
    assert_eq!(
        del_agent,
        StatusCode::NO_CONTENT,
        "delete agent should succeed"
    );

    let (st_out, out_j) = post_json(
        &app,
        "/auth/logout",
        Some(&auth_header),
        json!({ "refresh_token": refresh_token }),
    )
    .await;
    assert_eq!(st_out, StatusCode::OK, "logout: {out_j}");

    pool.close().await;
}
