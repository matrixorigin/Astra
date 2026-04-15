//! Full product matrix journey: sessions through logout (see `main.rs` module docs).
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use futures_util::StreamExt;
use serde_json::json;
use sqlx::Row;
use tower::util::ServiceExt;

use super::harness::{
    MatrixE2eCtx, cleanup_edge_registry, cleanup_session_data, delete_json, delete_no_content,
    get_json, post_empty, post_json, post_json_with_headers, put_json, row_get_opt_i64,
    row_get_opt_str, row_get_str, wait_for_agent_event_types,
};

async fn run_tool_backed_chat_turn(
    app: &axum::Router,
    auth_header: &str,
    session_id: &str,
    agent_id: &str,
    test_secret: &str,
) -> String {
    let read_file_tool = json!({
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
    });
    let payload = json!({
        "agent_id": agent_id,
        "session_id": session_id,
        "messages": [{ "role": "user", "content": "read README through a tool" }],
        "edge_tools": [read_file_tool],
        "test_llm_rounds": [
            {
                "tool_calls": [{
                    "id": "ctx-trace-tool-1",
                    "type": "function",
                    "function": {
                        "name": "read_file",
                        "arguments": "{\"path\":\"README.md\"}"
                    }
                }]
            },
            {
                "full_text": "tool-backed calibration reply",
                "reasoning": "",
                "usage": { "prompt": 7, "completion": 9, "total": 16 }
            }
        ]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/chat/turn")
        .header("authorization", auth_header)
        .header("content-type", "application/json")
        .header("x-mo-bridge-test-secret", test_secret)
        .body(Body::from(payload.to_string()))
        .expect("tool-backed chat request");
    let response = app
        .clone()
        .oneshot(req)
        .await
        .expect("tool-backed chat oneshot");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "tool-backed chat/turn should return 200"
    );

    let mut stream = response.into_body().into_data_stream();
    let mut acc = Vec::new();
    let mut posted_result = false;
    let mut saw_turn_complete = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("tool-backed sse chunk");
        acc.extend_from_slice(&chunk);
        let s = String::from_utf8_lossy(&acc);
        if !posted_result
            && s.contains("\"type\":\"tool_request\"")
            && s.contains("ctx-trace-tool-1")
        {
            let (st_result, result_body) = post_json(
                app,
                "/tools/result",
                Some(auth_header),
                json!({
                    "request_id": "ctx-trace-tool-1",
                    "status": "ok",
                    "output": "# README\nfrom tool-backed matrix e2e\n",
                }),
            )
            .await;
            assert_eq!(
                st_result,
                StatusCode::OK,
                "POST /tools/result for ctx-trace-tool-1: {result_body}"
            );
            posted_result = true;
        }
        if s.contains("turn_complete") {
            saw_turn_complete = true;
            break;
        }
    }

    assert!(posted_result, "tool-backed chat never emitted tool_request");
    assert!(
        saw_turn_complete,
        "tool-backed chat never reached turn_complete"
    );
    String::from_utf8_lossy(&acc).into_owned()
}

pub async fn run_product_matrix_full_journey(
    ctx: &MatrixE2eCtx,
    auth_header: &mut String,
    refresh_token: &mut String,
) {
    let session_id = ctx.session_id.clone();
    let user_id = ctx.user_id.clone();
    let edge_agent_id = ctx.edge_agent_id.clone();
    let suffix = ctx.suffix.clone();
    let pool = &ctx.pool;
    let app = &ctx.app;
    let memoria = &ctx.memoria;

    let (st_h, health) = get_json(app, "/health", None, &[]).await;
    assert_eq!(st_h, StatusCode::OK, "health: {health}");
    let (st_root, root) = get_json(app, "/", None, &[]).await;
    assert_eq!(st_root, StatusCode::OK, "root: {root}");

    let (st_list_s, list_s) = get_json(app, "/sessions", Some(auth_header.as_str()), &[]).await;
    assert_eq!(st_list_s, StatusCode::OK, "list sessions: {list_s}");
    assert!(
        list_s["sessions"].as_array().is_some_and(|a| a
            .iter()
            .any(|s| s["session_id"].as_str() == Some(session_id.as_str()))),
        "session not listed: {list_s}"
    );

    let (st_get_s, got_s) = get_json(
        app,
        &format!("/sessions/{session_id}"),
        Some(auth_header.as_str()),
        &[],
    )
    .await;
    assert_eq!(st_get_s, StatusCode::OK, "get session: {got_s}");

    let (st_put_s, put_s) = put_json(
        app,
        &format!("/sessions/{session_id}"),
        Some(auth_header.as_str()),
        json!({ "title": "product matrix session (updated)" }),
    )
    .await;
    assert_eq!(st_put_s, StatusCode::OK, "put session: {put_s}");
    assert_eq!(
        put_s["title"].as_str(),
        Some("product matrix session (updated)")
    );

    let (st_close, closed) = post_empty(
        app,
        &format!("/sessions/{session_id}/close"),
        Some(auth_header.as_str()),
    )
    .await;
    assert_eq!(st_close, StatusCode::OK, "close session: {closed}");
    assert_eq!(
        closed["status"].as_str(),
        Some("closed"),
        "close response: {closed}"
    );

    let sess_status = sqlx::query("SELECT status FROM agent_sessions WHERE session_id = ?")
        .bind(&session_id)
        .fetch_one(pool)
        .await
        .expect("session status after close");
    assert_eq!(
        sess_status.try_get::<String, _>("status").ok().as_deref(),
        Some("closed"),
        "agent_sessions.status after POST .../close"
    );

    let (st_res, resm) = post_empty(
        app,
        &format!("/sessions/{session_id}/resume"),
        Some(auth_header.as_str()),
    )
    .await;
    assert_eq!(st_res, StatusCode::OK, "resume session: {resm}");
    assert_eq!(
        resm["status"].as_str(),
        Some("active"),
        "resume response: {resm}"
    );

    let sess_active = sqlx::query("SELECT status FROM agent_sessions WHERE session_id = ?")
        .bind(&session_id)
        .fetch_one(pool)
        .await
        .expect("session status after resume");
    assert_eq!(
        sess_active.try_get::<String, _>("status").ok().as_deref(),
        Some("active"),
        "agent_sessions.status after POST .../resume"
    );

    let (st_act, act) = get_json(
        app,
        &format!("/sessions/{session_id}/activity"),
        Some(auth_header.as_str()),
        &[],
    )
    .await;
    assert_eq!(st_act, StatusCode::OK, "session activity: {act}");

    let (st_plat, plat) =
        get_json(app, "/platform/snapshot", Some(auth_header.as_str()), &[]).await;
    assert_eq!(st_plat, StatusCode::OK, "platform snapshot: {plat}");
    assert!(
        plat["health"]["status"].is_string(),
        "snapshot.health.status: {plat}"
    );
    assert!(plat["timestamp"].is_string(), "snapshot.timestamp: {plat}");

    let (st_au_sum, au_sum) = get_json(
        app,
        &format!("/sessions/{session_id}/audit/summary"),
        Some(auth_header.as_str()),
        &[],
    )
    .await;
    assert_eq!(st_au_sum, StatusCode::OK, "audit summary: {au_sum}");

    let (st_au_stats, au_stats) =
        get_json(app, "/audit/stats", Some(auth_header.as_str()), &[]).await;
    assert_eq!(st_au_stats, StatusCode::OK, "audit stats: {au_stats}");

    let (st_au_sess, au_sess) = get_json(
        app,
        "/audit/sessions?page=1&per_page=10",
        Some(auth_header.as_str()),
        &[],
    )
    .await;
    assert_eq!(st_au_sess, StatusCode::OK, "audit sessions: {au_sess}");

    let (st_au_turns, au_turns) = get_json(
        app,
        &format!("/sessions/{session_id}/audit/turns?page=1&per_page=20"),
        Some(auth_header.as_str()),
        &[],
    )
    .await;
    assert_eq!(st_au_turns, StatusCode::OK, "audit turns: {au_turns}");

    let (st_au_sess_tools, au_sess_tools) = get_json(
        app,
        &format!("/sessions/{session_id}/audit/tools"),
        Some(auth_header.as_str()),
        &[],
    )
    .await;
    assert_eq!(
        st_au_sess_tools,
        StatusCode::OK,
        "session audit tools: {au_sess_tools}"
    );

    let (st_au_errs, au_errs) = get_json(
        app,
        &format!("/sessions/{session_id}/audit/errors"),
        Some(auth_header.as_str()),
        &[],
    )
    .await;
    assert_eq!(
        st_au_errs,
        StatusCode::OK,
        "session audit errors: {au_errs}"
    );

    let (st_au_tools, au_tools) =
        get_json(app, "/audit/tools", Some(auth_header.as_str()), &[]).await;
    assert_eq!(
        st_au_tools,
        StatusCode::OK,
        "cross-session audit tools: {au_tools}"
    );

    let (st_mkt, mkt_j) = get_json(
        app,
        "/marketplace/installed?limit=20&offset=0",
        Some(auth_header.as_str()),
        &[],
    )
    .await;
    assert_eq!(st_mkt, StatusCode::OK, "marketplace installed: {mkt_j}");

    // Per-run skill name so parallel E2E processes do not contend on global marketplace stats rows.
    let mkt_probe_skill = format!("e2e_matrix_mkt_{suffix}");
    let (st_qr, qr_j) = post_json(
        app,
        "/marketplace/quality-report",
        Some(auth_header.as_str()),
        json!({
            "skill_name": mkt_probe_skill.as_str(),
            "skill_version": "1.0.0",
            "runtime_version": "matrix-e2e",
            "success_rate": 0.95,
            "avg_tokens": 120.0,
            "invocation_count": 3
        }),
    )
    .await;
    assert_eq!(
        st_qr,
        StatusCode::NO_CONTENT,
        "marketplace quality report: {qr_j}"
    );

    let (st_mst, mst_j) = get_json(
        app,
        &format!("/marketplace/stats/{mkt_probe_skill}"),
        None,
        &[],
    )
    .await;
    assert_eq!(st_mst, StatusCode::OK, "marketplace skill stats: {mst_j}");
    assert_eq!(mst_j["skill_name"].as_str(), Some(mkt_probe_skill.as_str()));

    let (st_msearch, ms_j) =
        get_json(app, "/marketplace/search?limit=10&offset=0", None, &[]).await;
    assert_eq!(st_msearch, StatusCode::OK, "marketplace search: {ms_j}");
    assert!(ms_j["results"].is_array(), "search results: {ms_j}");

    let xuid = &[("x-user-id", user_id.as_str())];
    let (st_gates, gates_j) = get_json(app, "/evaluation/gates?limit=10", None, xuid).await;
    assert_eq!(st_gates, StatusCode::OK, "evaluation gates: {gates_j}");

    let (st_cal, cal_j) = get_json(app, "/evaluation/calibration?days=7", None, xuid).await;
    assert_eq!(st_cal, StatusCode::OK, "evaluation calibration: {cal_j}");

    let (st_scores, scores_j) = get_json(
        app,
        "/evaluation/sessions/scores?limit=10&min_score=0",
        None,
        xuid,
    )
    .await;
    assert_eq!(
        st_scores,
        StatusCode::OK,
        "evaluation session scores: {scores_j}"
    );
    assert!(
        scores_j["sessions"].is_array(),
        "session scores payload: {scores_j}"
    );

    let (st_qt, qt_j) = get_json(app, "/evaluation/quality/trend?days=7", None, xuid).await;
    assert_eq!(st_qt, StatusCode::OK, "evaluation quality trend: {qt_j}");

    let (st_slo, slo_j) =
        get_json(app, "/evaluation/slo/dashboard?period_days=7", None, xuid).await;
    assert_eq!(st_slo, StatusCode::OK, "evaluation slo dashboard: {slo_j}");

    let (st_mh, mh_j) = get_json(app, "/evaluation/memory-health", None, xuid).await;
    assert_eq!(st_mh, StatusCode::OK, "evaluation memory-health: {mh_j}");

    let (st_mm, mm_j) = get_json(app, "/evaluation/memory-metrics", None, xuid).await;
    assert_eq!(st_mm, StatusCode::OK, "evaluation memory-metrics: {mm_j}");

    let (st_agent, agent_j) = post_json(
        app,
        "/agents",
        Some(auth_header.as_str()),
        json!({
            "name": "matrix-crud-agent",
            "agent_config": { "suite": "matrix" },
            "data_source": { "type": "matrixone", "database": ctx.matrixone_database.clone() }
        }),
    )
    .await;
    assert_eq!(st_agent, StatusCode::CREATED, "create agent: {agent_j}");
    let agent_id = agent_j["agent_id"].as_str().expect("agent_id").to_string();

    let agent_db =
        sqlx::query("SELECT agent_name, owner_user_id FROM agent_agents WHERE agent_id = ?")
            .bind(&agent_id)
            .fetch_optional(pool)
            .await
            .expect("agent_agents select");
    let agent_db = agent_db.expect("agent row");
    assert_eq!(
        agent_db.try_get::<String, _>("agent_name").ok().as_deref(),
        Some("matrix-crud-agent")
    );
    assert_eq!(
        agent_db
            .try_get::<String, _>("owner_user_id")
            .ok()
            .as_deref(),
        Some(user_id.as_str())
    );

    let (st_get_ag, got_ag) = get_json(
        app,
        &format!("/agents/{agent_id}"),
        Some(auth_header.as_str()),
        &[],
    )
    .await;
    assert_eq!(st_get_ag, StatusCode::OK, "get agent: {got_ag}");
    assert_eq!(got_ag["name"].as_str(), Some("matrix-crud-agent"));

    let (st_put_ag, put_ag) = put_json(
        app,
        &format!("/agents/{agent_id}"),
        Some(auth_header.as_str()),
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
        .fetch_one(pool)
        .await
        .expect("agent_agents after rename");
    assert_eq!(
        agent_renamed
            .try_get::<String, _>("agent_name")
            .ok()
            .as_deref(),
        Some("matrix-crud-agent-renamed")
    );

    let trust_path = format!("/evaluation/trust-report?agent_id={agent_id}&days=7");
    let (st_trust, trust_j) = get_json(app, &trust_path, None, xuid).await;
    assert_eq!(
        st_trust,
        StatusCode::OK,
        "evaluation trust-report: {trust_j}"
    );

    let slo_hist = format!("/evaluation/slo/{agent_id}/history?days=7");
    let (st_slo_hist, slo_hist_j) = get_json(app, &slo_hist, None, xuid).await;
    assert_eq!(
        st_slo_hist,
        StatusCode::OK,
        "evaluation slo history: {slo_hist_j}"
    );

    let obs_path = format!("/evaluation/observability/metrics?agent_id={agent_id}&days=7");
    let (st_obs, obs_j) = get_json(app, &obs_path, None, xuid).await;
    assert_eq!(
        st_obs,
        StatusCode::OK,
        "evaluation observability metrics: {obs_j}"
    );

    let (st_ev, ev_j) = post_json(
        app,
        "/events",
        Some(auth_header.as_str()),
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
        app,
        &format!("/events/{manual_event_id}"),
        Some(auth_header.as_str()),
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
        app,
        &format!("/events/causal-chain/{causal_chain_id}"),
        Some(auth_header.as_str()),
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
    let (st_list_ev, list_ev) = get_json(app, &list_ev_path, Some(auth_header.as_str()), &[]).await;
    assert_eq!(st_list_ev, StatusCode::OK, "list events (query): {list_ev}");
    assert!(
        list_ev["events"].as_array().is_some_and(|arr| {
            arr.iter()
                .any(|e| e["event_id"].as_str() == Some(manual_event_id.as_str()))
        }),
        "manual event missing from GET /events list: {list_ev}"
    );

    let (st_ev_sess, ev_sess) = get_json(
        app,
        &format!("/events/session/{session_id}?limit=50&offset=0"),
        Some(auth_header.as_str()),
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
        app,
        &format!("/data-versioning/lineage/{manual_event_id}/chain"),
        Some(auth_header.as_str()),
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
        app,
        &format!("/data-versioning/lineage/{manual_event_id}/upstream"),
        Some(auth_header.as_str()),
        &[],
    )
    .await;
    assert_eq!(
        st_dv_up,
        StatusCode::OK,
        "data-versioning upstream lineage: {dv_up}"
    );

    let (st_ctx, ctx_j) = post_json(
        app,
        "/context",
        Some(auth_header.as_str()),
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

    let snap_row =
        sqlx::query("SELECT session_id, event_id FROM ctx_snapshots WHERE context_capture_id = ?")
            .bind(&context_capture_id)
            .fetch_optional(pool)
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
        app,
        &format!("/context/{context_capture_id}"),
        Some(auth_header.as_str()),
        &[],
    )
    .await;
    assert_eq!(st_get_ctx, StatusCode::OK, "get snapshot: {got_ctx}");

    let (st_dec, dec_j) = post_json(
        app,
        "/decisions",
        Some(auth_header.as_str()),
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
    let decision_id = dec_j["decision_id"]
        .as_str()
        .expect("decision_id")
        .to_string();

    let dec_row = sqlx::query(
        "SELECT session_id, decision_type FROM ctx_decision_audits WHERE decision_id = ?",
    )
    .bind(&decision_id)
    .fetch_optional(pool)
    .await
    .expect("ctx_decision_audits");
    let dec_row = dec_row.expect("decision row");
    assert_eq!(
        dec_row.try_get::<String, _>("session_id").ok().as_deref(),
        Some(session_id.as_str())
    );
    assert_eq!(
        dec_row
            .try_get::<String, _>("decision_type")
            .ok()
            .as_deref(),
        Some("e2e_matrix_decision")
    );

    let (st_get_dec, got_dec) = get_json(
        app,
        &format!("/decisions/{decision_id}"),
        Some(auth_header.as_str()),
        &[],
    )
    .await;
    assert_eq!(st_get_dec, StatusCode::OK, "get decision: {got_dec}");

    let (st_audit, audit) = get_json(
        app,
        &format!("/decisions/{decision_id}/audit"),
        Some(auth_header.as_str()),
        &[],
    )
    .await;
    assert_eq!(st_audit, StatusCode::OK, "decision audit: {audit}");

    let list_dec_path = format!("/decisions?session_id={session_id}&limit=20&offset=0");
    let (st_list_d, list_d) = get_json(app, &list_dec_path, Some(auth_header.as_str()), &[]).await;
    assert_eq!(st_list_d, StatusCode::OK, "list decisions: {list_d}");
    assert!(
        list_d["decisions"].as_array().is_some_and(|arr| {
            arr.iter()
                .any(|d| d["decision_id"].as_str() == Some(decision_id.as_str()))
        }),
        "decision not in list: {list_d}"
    );

    let (st_mem_s, mem_s) = post_json(
        app,
        "/memory/store",
        Some(auth_header.as_str()),
        json!({ "content": "matrix e2e memory", "memory_type": "semantic" }),
    )
    .await;
    assert_eq!(st_mem_s, StatusCode::OK, "memory store: {mem_s}");

    let (st_mem_r, mem_r) = post_json(
        app,
        "/memory/retrieve",
        Some(auth_header.as_str()),
        json!({ "query": "matrix" }),
    )
    .await;
    assert_eq!(st_mem_r, StatusCode::OK, "memory retrieve: {mem_r}");

    let (st_mem_q, mem_q) = post_json(
        app,
        "/memory/search",
        Some(auth_header.as_str()),
        json!({ "query": "matrix", "top_k": 3 }),
    )
    .await;
    assert_eq!(st_mem_q, StatusCode::OK, "memory search: {mem_q}");

    let (st_mem_p, mem_p) = post_json(
        app,
        "/memory/purge",
        Some(auth_header.as_str()),
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
        .header("authorization", auth_header.as_str())
        .header("content-type", "application/json")
        .header("x-astra-edge-id", "matrix-e2e-edge")
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
    .fetch_optional(pool)
    .await
    .expect("edge registry select");
    let edge_db = edge_db.expect("edge_agent_registry row");
    assert_eq!(
        edge_db.try_get::<String, _>("edge_id").ok().as_deref(),
        Some("matrix-e2e-edge")
    );

    let (st_hb, hb) = post_json_with_headers(
        app,
        "/agents/edge/heartbeat",
        Some(auth_header.as_str()),
        &[("x-astra-edge-id", "matrix-e2e-edge")],
        json!({ "edge_agent_id": edge_agent_id }),
    )
    .await;
    assert_eq!(st_hb, StatusCode::OK, "edge heartbeat: {hb}");

    let (st_tool, tool_j) = post_json(
        app,
        "/tools/result",
        Some(auth_header.as_str()),
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
        app,
        "/approval/respond",
        Some(auth_header.as_str()),
        json!({
            "request_id": "matrix-appr-1",
            "decision": "allow",
            "reason": "e2e"
        }),
    )
    .await;
    assert_eq!(st_appr, StatusCode::OK, "approval/respond: {appr_j}");

    let (st_runs, runs) = get_json(app, "/runs", Some(auth_header.as_str()), &[]).await;
    assert_eq!(st_runs, StatusCode::OK, "list runs: {runs}");

    let (st_wf, wf_j) = get_json(app, "/workflows", Some(auth_header.as_str()), &[]).await;
    assert_eq!(st_wf, StatusCode::OK, "list workflows: {wf_j}");
    assert!(wf_j.is_array(), "workflows JSON should be an array: {wf_j}");

    let (st_cpl, cpl_j) = get_json(
        app,
        "/data-versioning/checkpoints",
        Some(auth_header.as_str()),
        &[],
    )
    .await;
    assert_eq!(
        st_cpl,
        StatusCode::OK,
        "list checkpoints (read-only): {cpl_j}"
    );
    assert!(
        cpl_j.is_array(),
        "checkpoints list should be a JSON array: {cpl_j}"
    );

    let (st_job, job_j) = post_json(
        app,
        "/jobs",
        Some(auth_header.as_str()),
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
        app,
        &format!("/jobs/{job_id}"),
        Some(auth_header.as_str()),
        &[],
    )
    .await;
    assert_eq!(st_gj, StatusCode::OK, "get job: {gj}");
    assert_eq!(gj["status"].as_str(), Some("pending"));

    let (st_wh, wh_j) = post_json(
        app,
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
        app,
        &format!("/jobs/{job_id}"),
        Some(auth_header.as_str()),
        &[],
    )
    .await;
    assert_eq!(st_gj2, StatusCode::OK, "get job after webhook: {gj2}");
    assert_eq!(gj2["status"].as_str(), Some("completed"));

    let sb_name = format!("sb_{suffix}");
    let (st_sb, sb_j) = post_json(
        app,
        "/sandbox",
        Some(auth_header.as_str()),
        json!({ "name": sb_name, "description": "matrix e2e sandbox" }),
    )
    .await;
    assert_eq!(st_sb, StatusCode::CREATED, "create sandbox: {sb_j}");

    let sb_row =
        sqlx::query("SELECT user_id, status FROM infra_sandbox_metadata WHERE sandbox_name = ?")
            .bind(&sb_name)
            .fetch_optional(pool)
            .await
            .expect("sandbox select");
    let sb_row = sb_row.expect("infra_sandbox_metadata row");
    assert_eq!(
        sb_row.try_get::<String, _>("user_id").ok().as_deref(),
        Some(user_id.as_str())
    );

    let (st_sbl, sbl) = get_json(app, "/sandbox", Some(auth_header.as_str()), &[]).await;
    assert_eq!(st_sbl, StatusCode::OK, "list sandboxes: {sbl}");
    assert!(
        sbl["sandboxes"].as_array().is_some_and(|a| {
            a.iter()
                .any(|s| s["sandbox_name"].as_str() == Some(sb_name.as_str()))
        }),
        "sandbox not listed: {sbl}"
    );

    let (st_sbg, sbg) = get_json(
        app,
        &format!("/sandbox/{sb_name}"),
        Some(auth_header.as_str()),
        &[],
    )
    .await;
    assert_eq!(st_sbg, StatusCode::OK, "get sandbox: {sbg}");

    let st_sbd = delete_no_content(
        app,
        &format!("/sandbox/{sb_name}"),
        Some(auth_header.as_str()),
    )
    .await;
    assert_eq!(st_sbd, StatusCode::NO_CONTENT, "delete sandbox");

    let sb_gone = sqlx::query("SELECT 1 FROM infra_sandbox_metadata WHERE sandbox_name = ?")
        .bind(&sb_name)
        .fetch_optional(pool)
        .await
        .expect("sandbox gone");
    assert!(
        sb_gone.is_none(),
        "sandbox row should be removed after DELETE"
    );

    let (st_tr, tr_j) = post_json(
        app,
        "/triggers",
        Some(auth_header.as_str()),
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

    let (st_tr_l, tr_l) = get_json(app, "/triggers", Some(auth_header.as_str()), &[]).await;
    assert_eq!(st_tr_l, StatusCode::OK, "list triggers: {tr_l}");
    assert!(
        tr_l.as_array().is_some_and(|a| {
            a.iter()
                .any(|t| t["trigger_id"].as_str() == Some(trigger_id.as_str()))
        }),
        "trigger not listed: {tr_l}"
    );

    let (st_fire, fire_j) = post_json(
        app,
        &format!("/triggers/{trigger_id}/fire"),
        None,
        json!({ "secret": wh_secret, "payload": { "hello": "matrix" } }),
    )
    .await;
    assert_eq!(st_fire, StatusCode::OK, "fire webhook: {fire_j}");
    assert_eq!(fire_j["fired"], true);

    let (st_tr_d, tr_d) = delete_json(
        app,
        &format!("/triggers/{trigger_id}"),
        Some(auth_header.as_str()),
    )
    .await;
    assert_eq!(st_tr_d, StatusCode::OK, "delete trigger: {tr_d}");

    let trig_gone = sqlx::query("SELECT 1 FROM wf_triggers WHERE trigger_id = ?")
        .bind(&trigger_id)
        .fetch_optional(pool)
        .await
        .expect("trigger gone");
    assert!(
        trig_gone.is_none(),
        "wf_triggers row should be deleted: {trigger_id}"
    );

    let (st_sks, sks_j) = get_json(app, "/skills", Some(auth_header.as_str()), &[]).await;
    assert_eq!(st_sks, StatusCode::OK, "list skills: {sks_j}");
    assert!(sks_j["skills"].is_array(), "skills list record: {sks_j}");

    let (st_sst, sst_j) = get_json(
        app,
        "/skills/status?per_group=50",
        Some(auth_header.as_str()),
        &[],
    )
    .await;
    assert_eq!(st_sst, StatusCode::OK, "skills status: {sst_j}");

    let (st_intro, intro_j) = get_json(
        app,
        "/introspection/skills",
        Some(auth_header.as_str()),
        &[],
    )
    .await;
    assert_eq!(st_intro, StatusCode::OK, "introspection skills: {intro_j}");

    let intro_mem = format!("/introspection/memory?session_id={session_id}");
    let (st_imem, imem_j) = get_json(app, &intro_mem, Some(auth_header.as_str()), &[]).await;
    assert_eq!(st_imem, StatusCode::OK, "introspection memory: {imem_j}");

    let intro_ct = format!(
        "/introspection/context/trend?session_id={session_id}&turns=8&context_window=128000"
    );
    let (st_ict, ict_j) = get_json(app, &intro_ct, Some(auth_header.as_str()), &[]).await;
    assert_eq!(
        st_ict,
        StatusCode::OK,
        "introspection context trend: {ict_j}"
    );

    let intro_cs = format!("/introspection/context/snapshot?session_id={session_id}&detail=false");
    let (st_ics, ics_j) = get_json(app, &intro_cs, Some(auth_header.as_str()), &[]).await;
    assert_eq!(
        st_ics,
        StatusCode::OK,
        "introspection context snapshot: {ics_j}"
    );

    let intro_rq =
        format!("/introspection/context/retrieval_quality?session_id={session_id}&turns=5");
    let (st_irq, irq_j) = get_json(app, &intro_rq, Some(auth_header.as_str()), &[]).await;
    assert_eq!(
        st_irq,
        StatusCode::OK,
        "introspection retrieval quality: {irq_j}"
    );

    let intro_recall =
        format!("/introspection/memory/recall?session_id={session_id}&query=matrix&limit=5");
    let (st_irc, irc_j) = get_json(app, &intro_recall, Some(auth_header.as_str()), &[]).await;
    assert_eq!(
        st_irc,
        StatusCode::OK,
        "introspection memory recall: {irc_j}"
    );

    let (st_route, route_j) = post_json(
        app,
        "/chat/route",
        Some(auth_header.as_str()),
        json!({ "query": "run tests and fix failures" }),
    )
    .await;
    assert_eq!(st_route, StatusCode::OK, "chat/route: {route_j}");
    assert!(
        route_j.get("tool_filter").is_some() && route_j.get("task_type").is_some(),
        "chat/route shape: {route_j}"
    );

    let (st_models, models_j) = get_json(app, "/models", Some(auth_header.as_str()), &[]).await;
    assert_eq!(st_models, StatusCode::OK, "list models: {models_j}");
    assert!(
        models_j.as_array().is_some(),
        "GET /models should return array: {models_j}"
    );

    let (st_sig, sig) = get_json(
        app,
        "/api/v1/learning/signals",
        Some(auth_header.as_str()),
        &[],
    )
    .await;
    assert_eq!(st_sig, StatusCode::OK, "learning signals: {sig}");

    let (st_lrn_stats, lrn_stats) = get_json(
        app,
        "/api/v1/learning/stats",
        Some(auth_header.as_str()),
        &[],
    )
    .await;
    assert_eq!(st_lrn_stats, StatusCode::OK, "learning stats: {lrn_stats}");

    let (st_drift, drift) = get_json(
        app,
        "/evaluation/drift",
        None,
        &[("x-user-id", user_id.as_str())],
    )
    .await;
    assert_eq!(st_drift, StatusCode::OK, "evaluation drift: {drift}");

    let reflect_path = format!("/chat/session/{session_id}/reflect");
    let (st_refl, refl) = get_json(app, &reflect_path, Some(auth_header.as_str()), &[]).await;
    assert_eq!(st_refl, StatusCode::OK, "reflect: {refl}");

    let trace_path = format!("/chat/session/{session_id}/decision-trace");
    let (st_trace, trace) = get_json(app, &trace_path, Some(auth_header.as_str()), &[]).await;
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

    let test_secret = std::env::var("ASTRA_BRIDGE_TEST_SECRET").expect("bridge test secret");
    let chat_req = Request::builder()
        .method("POST")
        .uri("/chat/turn")
        .header("authorization", auth_header.as_str())
        .header("content-type", "application/json")
        .header("x-mo-bridge-test-secret", &test_secret)
        .body(Body::from(chat_body.to_string()))
        .expect("chat request");

    let response = app.clone().oneshot(chat_req).await.expect("chat oneshot");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "chat/turn should return 200"
    );

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

    wait_for_agent_event_types(
        pool,
        &session_id,
        &["user_query", "llm_response"],
        std::time::Duration::from_secs(30),
    )
    .await;

    let recs = sqlx::query(
        "SELECT event_id, session_id, user_id, event_type, content, parent_event_id, \
         causal_chain_id, token_input, token_output, token_total, llm_model_used, reasoning_content \
         FROM agent_events WHERE session_id = ? ORDER BY created_at ASC",
    )
    .bind(&session_id)
    .fetch_all(pool)
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
    assert!(
        !cc.is_empty(),
        "causal_chain_id should be set on user_query"
    );

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

    let (st_fb, fb_j) = post_json(
        app,
        "/api/v1/learning/feedback",
        Some(auth_header.as_str()),
        json!({
            "event_id": uq_event_id,
            "satisfaction_score": 2
        }),
    )
    .await;
    assert_eq!(st_fb, StatusCode::OK, "learning feedback: {fb_j}");
    assert_eq!(fb_j["status"], "success");

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

    let turn_cnt_row = sqlx::query(
        "SELECT COUNT(*) AS c FROM agent_events \
         WHERE session_id = ? AND user_id = ? AND event_type = 'user_query'",
    )
    .bind(&session_id)
    .bind(&user_id)
    .fetch_one(pool)
    .await
    .expect("turn events count");
    let n_turns: i64 = turn_cnt_row.try_get("c").unwrap_or(0);
    assert!(
        n_turns >= 1,
        "expected >=1 session turn events after chat/turn for audit detail, got {n_turns}"
    );
    let last_turn_n = n_turns as u32;
    let turn_detail_path = format!("/sessions/{session_id}/audit/turns/{last_turn_n}");
    let (st_td, td_j) = get_json(app, &turn_detail_path, Some(auth_header.as_str()), &[]).await;
    assert_eq!(st_td, StatusCode::OK, "audit turn detail: {td_j}");
    assert_eq!(
        td_j["turn"].as_u64(),
        Some(u64::from(last_turn_n)),
        "turn detail index: {td_j}"
    );
    let ui = td_j["user_input"].as_str().unwrap_or("");
    assert!(
        ui.contains("matrix journey ping"),
        "turn detail user_input should include user prompt: {td_j}"
    );

    let tool_turn_sse = run_tool_backed_chat_turn(
        app,
        auth_header.as_str(),
        &session_id,
        &agent_id,
        &test_secret,
    )
    .await;
    assert!(
        tool_turn_sse.contains("\"type\":\"tool_request\""),
        "tool-backed chat should emit tool_request: {tool_turn_sse}"
    );
    wait_for_agent_event_types(
        pool,
        &session_id,
        &["context_trace_signal"],
        std::time::Duration::from_secs(30),
    )
    .await;

    let trace_row = {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if let Some(row) = sqlx::query(
                "SELECT \
                     JSON_UNQUOTE(JSON_EXTRACT(metadata, '$.turn_id')) AS turn_id, \
                     JSON_UNQUOTE(JSON_EXTRACT(metadata, '$.tool_selection.selected_tools[0]')) AS selected_tool, \
                     JSON_UNQUOTE(JSON_EXTRACT(metadata, '$.tool_selection.strategy')) AS strategy, \
                     CAST(JSON_UNQUOTE(JSON_EXTRACT(metadata, '$.tool_selection.confidence')) AS DOUBLE) AS selection_confidence \
                 FROM agent_events \
                 WHERE session_id = ? \
                   AND event_type = 'context_trace_signal' \
                   AND JSON_UNQUOTE(JSON_EXTRACT(metadata, '$.tool_selection.selected_tools[0]')) IS NOT NULL \
                 ORDER BY created_at DESC \
                 LIMIT 1",
            )
            .bind(&session_id)
            .fetch_optional(pool)
            .await
            .expect("latest tool-backed context_trace_signal event")
            {
                break row;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!(
                    "timeout waiting for tool-backed context_trace_signal for session_id={session_id}"
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    };
    assert!(
        trace_row
            .try_get::<Option<String>, _>("turn_id")
            .ok()
            .flatten()
            .is_some_and(|turn_id| turn_id.starts_with("turn-")),
        "context trace event should carry turn_id"
    );
    assert_eq!(
        trace_row
            .try_get::<Option<String>, _>("selected_tool")
            .ok()
            .flatten()
            .as_deref(),
        Some("read_file"),
        "context trace event should persist selected tool"
    );
    assert!(
        trace_row
            .try_get::<Option<String>, _>("strategy")
            .ok()
            .flatten()
            .is_some_and(|strategy| !strategy.is_empty()),
        "context trace event should persist tool selection strategy"
    );
    assert!(
        trace_row
            .try_get::<Option<f64>, _>("selection_confidence")
            .ok()
            .flatten()
            .is_some_and(|confidence| confidence >= 0.0),
        "context trace event should persist selection confidence"
    );

    let assessment_row = sqlx::query(
        "SELECT score, step_count \
         FROM eval_quality_assessments \
         WHERE user_id = ? AND target_id = ? AND level = 'session' \
         ORDER BY updated_at DESC \
         LIMIT 1",
    )
    .bind(&user_id)
    .bind(&session_id)
    .fetch_optional(pool)
    .await
    .expect("session quality assessment row");
    let assessment_row = assessment_row.expect("session quality assessment after tool-backed turn");
    assert!(
        assessment_row
            .try_get::<Option<i32>, _>("step_count")
            .ok()
            .flatten()
            .is_some_and(|step_count| step_count >= 1),
        "session quality assessment should record tool-backed step_count"
    );

    let (st_cal_after, cal_after_j) =
        get_json(app, "/evaluation/calibration?days=7", None, xuid).await;
    assert_eq!(
        st_cal_after,
        StatusCode::OK,
        "evaluation calibration after tool-backed turn: {cal_after_j}"
    );
    assert!(
        cal_after_j["sample_count"].as_u64().unwrap_or(0) >= 1,
        "calibration should include at least one sample after tool-backed turn: {cal_after_j}"
    );

    let replay_cmp_path = format!("/sessions/{session_id}/replay/compare");
    let (st_rcmp, rcmp_j) = get_json(app, &replay_cmp_path, Some(auth_header.as_str()), &[]).await;
    assert_eq!(st_rcmp, StatusCode::OK, "replay compare: {rcmp_j}");
    assert!(
        rcmp_j["original_event_count"].as_i64().unwrap_or(0) > 0,
        "replay compare should count non-replay events: {rcmp_j}"
    );

    cleanup_session_data(pool, &session_id).await;
    cleanup_edge_registry(pool, &user_id, &edge_agent_id).await;

    let del_agent = delete_no_content(
        app,
        &format!("/agents/{agent_id}"),
        Some(auth_header.as_str()),
    )
    .await;
    assert_eq!(
        del_agent,
        StatusCode::NO_CONTENT,
        "delete agent should succeed"
    );

    let (st_out, out_j) = post_json(
        app,
        "/auth/logout",
        Some(auth_header.as_str()),
        json!({ "refresh_token": refresh_token.as_str() }),
    )
    .await;
    assert_eq!(st_out, StatusCode::OK, "logout: {out_j}");
}
