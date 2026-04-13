//! Cross-session audit HTTP: `GET /audit/stats`, `/audit/mutations`, `/audit/promotions` with DB seeding.

use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use super::harness;

fn enc(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c == ' ' {
                "%20".to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}

pub async fn run_audit_cross_session_analytics_http() {
    let b = harness::bootstrap().await;
    let pool = &b.ctx.pool;
    let app = &b.ctx.app;
    let uid = &b.ctx.user_id;
    let auth = &b.auth_header;

    let since = "2026-10-20 00:00:00.000000";
    let until = "2026-10-20 23:59:59.000000";
    let s_stats_a = Uuid::new_v4().to_string();
    let s_stats_b = Uuid::new_v4().to_string();
    let e_turn_a = Uuid::new_v4().to_string();
    let e_turn_b = Uuid::new_v4().to_string();

    let s_mut = Uuid::new_v4().to_string();
    let decision_id = Uuid::new_v4().to_string();
    let ev_anchor = Uuid::new_v4().to_string();
    let ev_ms = Uuid::new_v4().to_string();

    let s_pr = Uuid::new_v4().to_string();
    let ev_pr = Uuid::new_v4().to_string();

    let event_ids = vec![
        e_turn_a.clone(),
        e_turn_b.clone(),
        ev_anchor.clone(),
        ev_ms.clone(),
        ev_pr.clone(),
    ];
    let session_ids = vec![
        s_stats_a.clone(),
        s_stats_b.clone(),
        s_mut.clone(),
        s_pr.clone(),
    ];

    for sid in &session_ids {
        let _ = sqlx::query("DELETE FROM ctx_decision_audits WHERE session_id = ?")
            .bind(sid)
            .execute(pool)
            .await;
        let _ = sqlx::query(
            "DELETE edge FROM agent_event_edges edge \
             JOIN agent_events ev ON edge.child_event_id = ev.event_id \
             WHERE ev.session_id = ?",
        )
        .bind(sid)
        .execute(pool)
        .await;
        let _ = sqlx::query("DELETE FROM agent_events WHERE session_id = ?")
            .bind(sid)
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM agent_sessions WHERE session_id = ?")
            .bind(sid)
            .execute(pool)
            .await;
    }

    for (sid, ts_sess) in [
        (&s_stats_a, "2026-10-20 08:00:00.000000"),
        (&s_stats_b, "2026-10-20 08:01:00.000000"),
        (&s_mut, "2026-10-20 08:02:00.000000"),
        (&s_pr, "2026-10-20 08:03:00.000000"),
    ] {
        sqlx::query(
            "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count, \
             created_at, updated_at, last_active_at) \
             VALUES (?, ?, 'e2e_audit_xs', 'active', 0, ?, ?, ?)",
        )
        .bind(sid)
        .bind(uid)
        .bind(ts_sess)
        .bind(ts_sess)
        .bind(ts_sess)
        .execute(pool)
        .await
        .expect("insert session");
    }

    for (eid, sid, ts) in [
        (&e_turn_a, &s_stats_a, "2026-10-20 10:00:00.000000"),
        (&e_turn_b, &s_stats_b, "2026-10-20 10:01:00.000000"),
    ] {
        sqlx::query(
            "INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, \
             causal_chain_id, token_input, token_output, token_total, created_at) \
             VALUES (?, ?, ?, 'turn', '{}', '', 1, 1, 2, ?)",
        )
        .bind(eid)
        .bind(sid)
        .bind(uid)
        .bind(ts)
        .execute(pool)
        .await
        .expect("insert turn event");
    }

    sqlx::query(
        "INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, \
         causal_chain_id, created_at) \
         VALUES (?, ?, ?, 'it_anchor', '{}', '', ?)",
    )
    .bind(&ev_anchor)
    .bind(&s_mut)
    .bind(uid)
    .bind("2026-10-20 10:59:00.000000")
    .execute(pool)
    .await
    .expect("insert decision anchor event");

    let decision_output = json!({
        "turn": 1,
        "mutation_objective_score": {
            "quality": {"point": 0.9, "lower": 0.9, "upper": 0.9},
            "reward_hacking_risk": {"point": 0.05, "lower": 0.05, "upper": 0.05},
            "causal_support": {"point": 0.9, "lower": 0.9, "upper": 0.9},
            "was_corrected": false
        },
        "action_profiles": [{
            "tool_call_id": "call-e2e",
            "tool_name": "edit_file",
            "arguments": {"path": "src/x.rs"},
            "profile": {
                "bounded": true,
                "reversible": true,
                "requires_pre_state": false,
                "action_category": "write",
                "compensation_kind": "restore_file",
                "compensation_summary": "restore"
            }
        }]
    });
    sqlx::query(
        "INSERT INTO ctx_decision_audits \
         (decision_id, session_id, event_id, context_capture_id, decision_type, decision_output, model_params, created_at) \
         VALUES (?, ?, ?, 'cc-e2e', 'tool_selection', CAST(? AS JSON), CAST('{}' AS JSON), ?)",
    )
    .bind(&decision_id)
    .bind(&s_mut)
    .bind(&ev_anchor)
    .bind(decision_output.to_string())
    .bind("2026-10-20 11:00:00.000000")
    .execute(pool)
    .await
    .expect("insert decision");

    sqlx::query(
        "INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, \
         causal_chain_id, metadata, created_at) \
         VALUES (?, ?, ?, 'mutation_state', '{}', '', CAST(? AS JSON), ?)",
    )
    .bind(&ev_ms)
    .bind(&s_mut)
    .bind(uid)
    .bind(
        json!({
            "mutation_id": format!("{decision_id}:call-e2e"),
            "state": "applied",
            "note": null,
            "tool_name": "edit_file",
            "turn": 1
        })
        .to_string(),
    )
    .bind("2026-10-20 11:01:00.000000")
    .execute(pool)
    .await
    .expect("insert mutation_state");

    let promo_meta = json!({
        "controller": "evolution",
        "outcome": "promoted",
        "recommendation": "promote",
        "subject_id": "subj-e2e",
        "summary": "e2e-promo",
        "turn": 2,
        "confidence_score": 0.88,
        "support_score": 0.77,
        "safety_score": 0.66,
        "overall_score": 0.75,
        "blockers": [],
        "evidence": [],
        "rollback_hint": null,
        "run_id": "run-e2e"
    });
    sqlx::query(
        "INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, \
         causal_chain_id, metadata, created_at) \
         VALUES (?, ?, ?, ?, '{}', '', CAST(? AS JSON), ?)",
    )
    .bind(&ev_pr)
    .bind(&s_pr)
    .bind(uid)
    .bind(astra_services::session_audit::RUNTIME_PROMOTION_EVENT_TYPE)
    .bind(promo_meta.to_string())
    .bind("2026-10-20 12:00:00.000000")
    .execute(pool)
    .await
    .expect("insert runtime promotion");

    let q = format!("/audit/stats?since={}&until={}", enc(since), enc(until));
    let (st, body): (StatusCode, Value) = harness::get_json(app, &q, Some(auth), &[]).await;
    assert_eq!(st, StatusCode::OK, "stats body: {body}");
    assert_eq!(body["session_count"], json!(4));
    assert_eq!(body["total_turns"], json!(2));
    assert_eq!(body["total_mutations"], json!(1));
    assert_eq!(body["applied_mutations"], json!(1));
    assert_eq!(body["total_runtime_promotions"], json!(1));

    let qm = format!(
        "/audit/mutations?page=1&per_page=50&since={}&until={}",
        enc(since),
        enc(until)
    );
    let (stm, mut_body) = harness::get_json(app, &qm, Some(auth), &[]).await;
    assert_eq!(stm, StatusCode::OK, "mutations: {mut_body}");
    assert_eq!(mut_body["total"], json!(1));
    assert_eq!(
        mut_body["mutations"][0]["mutation_id"],
        json!(format!("{decision_id}:call-e2e"))
    );

    let qp = format!(
        "/audit/promotions?page=1&per_page=20&since={}&until={}",
        enc(since),
        enc(until)
    );
    let (stp, pr_body) = harness::get_json(app, &qp, Some(auth), &[]).await;
    assert_eq!(stp, StatusCode::OK, "promotions: {pr_body}");
    assert_eq!(pr_body["total"], json!(1));
    assert_eq!(pr_body["promotions"][0]["event_id"], json!(ev_pr));
    assert_eq!(pr_body["promotions"][0]["subject_id"], json!("subj-e2e"));

    for eid in &event_ids {
        let _ = sqlx::query("DELETE FROM agent_event_edges WHERE child_event_id = ?")
            .bind(eid)
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM agent_events WHERE event_id = ?")
            .bind(eid)
            .execute(pool)
            .await;
    }
    let _ = sqlx::query("DELETE FROM ctx_decision_audits WHERE decision_id = ?")
        .bind(&decision_id)
        .execute(pool)
        .await;
    for sid in &session_ids {
        let _ = sqlx::query("DELETE FROM agent_sessions WHERE session_id = ?")
            .bind(sid)
            .execute(pool)
            .await;
    }

    b.ctx.pool.close().await;
}
