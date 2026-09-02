//! Durable run pause/resume over the real HTTP and MatrixOne wiring.

use astra_services::runs::DurableWorkRunBinding;
use astra_services::work::{GraphRevision, WorkBranchId, WorkId};
use astra_services::{DatabaseRunStateStore, DurableRunRecord, RunStateStore};
use axum::http::StatusCode;
use serde_json::json;

use super::harness::{
    self, bootstrap, delete_json, get_json, post_empty, post_json, seeded_model_selection,
};

async fn seed_orphan_cancel_race_run(
    ctx: &harness::MatrixE2eCtx,
    case: &str,
) -> (String, String, String) {
    let run_id = format!("orphan-http-{case}-{}", ctx.suffix);
    let work_id = format!("ow-{case}-{}", ctx.suffix);
    let branch_id = format!("ob-{case}-{}", ctx.suffix);
    let request_id = format!("approval-{case}-{}", ctx.suffix);
    let attempt_id = format!("oa-{case}-{}", ctx.suffix);
    let item_id = format!("oi-{case}-{}", ctx.suffix);
    let now = chrono::Utc::now().to_rfc3339();
    let fixture_owner = format!("orphan-fixture-{case}");
    let store = DatabaseRunStateStore::new(ctx.shared_pool.clone())
        .with_owner_pod_id(fixture_owner.clone());
    store
        .insert_run(DurableRunRecord {
            run_id: run_id.clone(),
            user_id: ctx.user_id.clone(),
            session_id: ctx.session_id.clone(),
            parent_run_id: None,
            root_run_id: Some(run_id.clone()),
            ancestor_path: Some(run_id.clone()),
            depth: 0,
            delegation_id: None,
            agent_id: Some(fixture_owner.clone()),
            retry_of: None,
            retry_scope: Some("node".to_string()),
            status: "running".to_string(),
            waiting_for: None,
            owner_pod_id: Some(fixture_owner),
            owner_lease_expires_at: Some(
                (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339(),
            ),
            run_generation: 0,
            last_event_idx: -1,
            checkpoint_version: None,
            checkpoint_json: None,
            error_code: None,
            error_message: None,
            retry_count: 0,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_tool_calls: 0,
            agent_binding_id: None,
            agent_binding_name: None,
            agent_binding_schema_version: None,
            model_offering_id: None,
            resolved_model_name: None,
            runtime_profile: None,
            start_request_fingerprint: None,
            work_binding: Some(DurableWorkRunBinding::new(
                WorkId::parse(&work_id).expect("work id"),
                WorkBranchId::parse(&branch_id).expect("branch id"),
                GraphRevision::new(1).expect("graph revision"),
            )),
            events: vec![json!({"event_type": "run_started", "data": {}})],
            created_at: now.clone(),
            updated_at: now,
        })
        .await
        .expect("seed orphan cancellation run");
    store
        .append_events_batch(
            &ctx.user_id,
            &ctx.session_id,
            &run_id,
            &[
                json!({
                    "event_type": "user_intent",
                    "idempotency_key": format!("user_intent:{run_id}"),
                    "data": {
                        "intent_id": format!("intent-{run_id}"),
                        "delivery": "guide_current_run",
                        "input": {"content": "preserve this guidance"}
                    }
                }),
                json!({
                    "event_type": "approval_required",
                    "idempotency_key": format!("approval:{request_id}:required"),
                    "data": {
                        "request_id": request_id,
                        "session_id": ctx.session_id,
                        "tool": "bash",
                        "approval_kind": "standard",
                        "delivery": "durable"
                    }
                }),
            ],
        )
        .await
        .expect("seed pending guidance and interaction");
    sqlx::query(
        "INSERT INTO work_item_attempts
         (owner_id, work_id, branch_id, work_item_id, work_item_revision,
          attempt_id, executor_run_id, execution_mode, status, graph_revision,
          run_generation, last_event_idx, unavailable_capabilities_json)
         VALUES (?, ?, ?, ?, 1, ?, ?, 'primary', 'waiting', 1, 0, -1, '[]')",
    )
    .bind(&ctx.user_id)
    .bind(&work_id)
    .bind(&branch_id)
    .bind(&item_id)
    .bind(&attempt_id)
    .bind(&run_id)
    .execute(&ctx.pool)
    .await
    .expect("seed pending Work carrier");
    sqlx::query(
        "INSERT INTO work_runtime_event_outbox_slots
         (owner_id, work_id, last_enqueued_event_seq, last_projected_event_seq, has_pending)
         VALUES (?, ?, 0, 0, 0)",
    )
    .bind(&ctx.user_id)
    .bind(&work_id)
    .execute(&ctx.pool)
    .await
    .expect("seed Work runtime outbox slot");
    sqlx::query(
        "UPDATE agent_runs
         SET status = 'waiting', waiting_for = 'tool_approval',
             owner_pod_id = NULL, owner_lease_expires_at = NULL,
             updated_at = '1000-01-01 00:00:00.000000'
         WHERE user_id = ? AND run_id = ?",
    )
    .bind(&ctx.user_id)
    .bind(&run_id)
    .execute(&ctx.pool)
    .await
    .expect("orphan exact run generation");
    (run_id, work_id, request_id)
}

async fn cleanup_orphan_cancel_work_fixture(
    ctx: &harness::MatrixE2eCtx,
    run_id: &str,
    work_id: &str,
) {
    for statement in [
        "DELETE FROM work_runtime_event_outbox WHERE owner_id = ? AND work_id = ?",
        "DELETE FROM work_runtime_event_outbox_slots WHERE owner_id = ? AND work_id = ?",
        "DELETE FROM work_item_attempts WHERE owner_id = ? AND executor_run_id = ?",
    ] {
        let mut query = sqlx::query(statement).bind(&ctx.user_id);
        query = if statement.contains("executor_run_id") {
            query.bind(run_id)
        } else {
            query.bind(work_id)
        };
        query
            .execute(&ctx.pool)
            .await
            .expect("cleanup Work fixture");
    }
}

pub async fn run_orphan_cancel_claim_race_http() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let auth = &b.auth_header;

    let (cancel_run_id, cancel_work_id, cancel_request_id) =
        seed_orphan_cancel_race_run(ctx, "cancel-win").await;
    let (cancel_status, cancel_body) =
        delete_json(&ctx.app, &format!("/chat/runs/{cancel_run_id}"), Some(auth)).await;
    assert_eq!(
        cancel_status,
        StatusCode::OK,
        "cancel-win DELETE: {cancel_body}"
    );
    assert_eq!(cancel_body["status"], "cancelled");
    assert_eq!(cancel_body["execution_settled"], true);
    let (duplicate_status, duplicate_body) =
        delete_json(&ctx.app, &format!("/chat/runs/{cancel_run_id}"), Some(auth)).await;
    assert_eq!(
        duplicate_status,
        StatusCode::OK,
        "duplicate cancel-win DELETE: {duplicate_body}"
    );
    assert_eq!(duplicate_body["status"], "cancelled");
    assert_eq!(duplicate_body["execution_settled"], true);

    let store = DatabaseRunStateStore::new(ctx.shared_pool.clone())
        .with_owner_pod_id(format!("race-claimer-{}", ctx.suffix));
    let cancelled = store
        .load_run(&ctx.user_id, &cancel_run_id)
        .await
        .expect("load cancel-win run")
        .expect("cancel-win run");
    let terminal_types = cancelled
        .events
        .iter()
        .rev()
        .take(3)
        .map(|event| event["event_type"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        terminal_types,
        vec!["run_finished", "user_intent_returned", "approval_resolved"]
    );
    assert_eq!(
        cancelled
            .events
            .iter()
            .filter(|event| event["event_type"] == "user_intent_returned")
            .count(),
        1
    );
    assert_eq!(
        cancelled
            .events
            .iter()
            .filter(|event| event["event_type"] == "run_finished")
            .count(),
        1
    );
    assert_eq!(
        cancelled
            .events
            .iter()
            .filter(|event| {
                event["event_type"] == "approval_resolved"
                    && event["data"]["request_id"] == cancel_request_id
            })
            .count(),
        1
    );
    let carrier: String = sqlx::query_scalar(
        "SELECT status FROM work_item_attempts WHERE owner_id = ? AND executor_run_id = ?",
    )
    .bind(&ctx.user_id)
    .bind(&cancel_run_id)
    .fetch_one(&ctx.pool)
    .await
    .expect("load cancelled Work carrier");
    assert_eq!(carrier, "cancelled");
    let outbox_kind: String = sqlx::query_scalar(
        "SELECT event_kind FROM work_runtime_event_outbox WHERE owner_id = ? AND work_id = ?",
    )
    .bind(&ctx.user_id)
    .bind(&cancel_work_id)
    .fetch_one(&ctx.pool)
    .await
    .expect("load cancellation outbox");
    assert_eq!(outbox_kind, "run_cancelled");

    let (claim_run_id, claim_work_id, claim_request_id) =
        seed_orphan_cancel_race_run(ctx, "claim-win").await;
    let claimed = store
        .claim_recoverable_active_runs(1)
        .await
        .expect("production recovery claim before DELETE");
    assert!(
        claimed.iter().all(|run| run.run_id != cancel_run_id),
        "the cancel-win terminal generation must not be claimable"
    );
    let claimed = claimed
        .iter()
        .find(|run| run.run_id == claim_run_id)
        .expect("the oldest orphan fixture must be claimed");
    assert_eq!(claimed.run_generation, 1);
    assert_eq!(claimed.owner_pod_id.as_deref(), Some(store.owner_pod_id()));

    let (claim_status, claim_body) =
        delete_json(&ctx.app, &format!("/chat/runs/{claim_run_id}"), Some(auth)).await;
    assert_eq!(
        claim_status,
        StatusCode::OK,
        "claim-win DELETE: {claim_body}"
    );
    assert_eq!(claim_body["status"], "cancellation_requested");
    assert_eq!(claim_body["execution_settled"], false);
    let claimed_run = store
        .load_run(&ctx.user_id, &claim_run_id)
        .await
        .expect("load claim-win run")
        .expect("claim-win run");
    assert_eq!(claimed_run.status, "waiting");
    assert_eq!(claimed_run.run_generation, 1);
    assert!(claimed_run.events.iter().all(|event| {
        !matches!(
            event["event_type"].as_str(),
            Some("approval_resolved" | "user_intent_returned" | "run_finished")
        )
    }));
    assert!(claimed_run.events.iter().any(|event| {
        event["event_type"] == "approval_required"
            && event["data"]["request_id"] == claim_request_id
    }));
    let claim_carrier: String = sqlx::query_scalar(
        "SELECT status FROM work_item_attempts WHERE owner_id = ? AND executor_run_id = ?",
    )
    .bind(&ctx.user_id)
    .bind(&claim_run_id)
    .fetch_one(&ctx.pool)
    .await
    .expect("load claimed Work carrier");
    assert_eq!(claim_carrier, "waiting");

    cleanup_orphan_cancel_work_fixture(ctx, &cancel_run_id, &cancel_work_id).await;
    cleanup_orphan_cancel_work_fixture(ctx, &claim_run_id, &claim_work_id).await;
    ctx.close().await;
}

pub async fn run_chat_run_pause_resume_http() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let auth = &b.auth_header;
    let app = &ctx.app;
    let session_id = ctx.session_id.clone();

    let (st_chat, chat_j) = post_json(
        app,
        "/chat",
        Some(auth.as_str()),
        json!({
            "message": "matrix e2e background run",
            "session_id": session_id,
            "model_selection": seeded_model_selection(ctx),
            "context": {
                "test_llm_rounds": [
                    {
                        "full_text": "matrix e2e pause/resume completed",
                        "delay_ms": 1500
                    }
                ]
            },
            "execution_budget": {
                "initial_turns": 10,
                "hard_turn_limit": 10
            }
        }),
    )
    .await;
    assert_eq!(st_chat, StatusCode::OK, "POST /chat: {chat_j}");
    let run_id = chat_j["run_id"].as_str().expect("run_id").to_string();
    assert!(!run_id.is_empty(), "run_id from ChatResponse");

    let observed_status = harness::wait_for_run_status(
        app,
        &run_id,
        auth.as_str(),
        "running",
        std::time::Duration::from_secs(5),
    )
    .await;
    assert_eq!(
        observed_status, "running",
        "run should remain running long enough for pause/resume coverage"
    );

    let (st_pause, pause_j) = post_empty(
        app,
        &format!("/chat/runs/{run_id}/pause"),
        Some(auth.as_str()),
    )
    .await;
    assert_eq!(st_pause, StatusCode::OK, "pause run: {pause_j}");

    let (st_get, get_j) = get_json(
        app,
        &format!("/chat/runs/{run_id}"),
        Some(auth.as_str()),
        &[],
    )
    .await;
    assert_eq!(st_get, StatusCode::OK, "get run after pause: {get_j}");
    assert_eq!(get_j["status"].as_str(), Some("paused"));

    let (st_resume, resume_j) = post_empty(
        app,
        &format!("/chat/runs/{run_id}/resume"),
        Some(auth.as_str()),
    )
    .await;
    assert_eq!(st_resume, StatusCode::OK, "resume run: {resume_j}");

    match resume_j["disposition"].as_str() {
        // The local executor is still able to continue the existing run.
        Some("applied") => {
            let status = harness::wait_for_run_status(
                app,
                &run_id,
                auth.as_str(),
                "completed",
                std::time::Duration::from_secs(10),
            )
            .await;
            assert_eq!(
                status, "completed",
                "resumed run should complete: {resume_j}"
            );
        }
        // The executor stopped while paused. The client must start a fresh
        // turn in the same session; that directive must free the prior slot.
        Some("session_continuation_required") => {
            assert_eq!(
                resume_j["continuation"]["strategy"].as_str(),
                Some("session_continuation"),
                "resume must provide the typed continuation strategy: {resume_j}"
            );
            assert_eq!(
                resume_j["continuation"]["session_id"].as_str(),
                Some(session_id.as_str()),
                "continuation must stay in the original session: {resume_j}"
            );
            assert_eq!(
                resume_j["continuation"]["source_run_id"].as_str(),
                Some(run_id.as_str()),
                "continuation must identify the paused source run: {resume_j}"
            );

            let durable_slot_owner: Option<String> = sqlx::query_scalar(
                "SELECT run_id FROM agent_session_execution_slots WHERE user_id = ? AND session_id = ?",
            )
            .bind(&ctx.user_id)
            .bind(&session_id)
            .fetch_optional(&ctx.pool)
            .await
            .expect("read session execution slot after continuation directive");
            assert!(
                durable_slot_owner.is_none(),
                "a session-continuation directive must release the durable execution slot; owner={durable_slot_owner:?}"
            );

            let (st_continuation, continuation_j) = post_json(
                app,
                "/chat",
                Some(auth.as_str()),
                json!({
                    "message": "matrix e2e continuation after paused run",
                    "session_id": session_id,
                    "model_selection": seeded_model_selection(ctx),
                    "context": {
                        "test_llm_rounds": [
                            { "full_text": "matrix e2e session continuation completed" }
                        ]
                    },
                    "execution_budget": {
                        "initial_turns": 10,
                        "hard_turn_limit": 10
                    }
                }),
            )
            .await;
            assert_eq!(
                st_continuation,
                StatusCode::OK,
                "POST /chat for session continuation: {continuation_j}"
            );
            let continuation_run_id = continuation_j["run_id"]
                .as_str()
                .expect("continuation run_id")
                .to_string();
            assert_ne!(
                continuation_run_id, run_id,
                "session continuation must start a distinct run: {continuation_j}"
            );
            assert_eq!(
                continuation_j["session_id"].as_str(),
                Some(session_id.as_str()),
                "continuation response must remain in the original session: {continuation_j}"
            );
            let status = harness::wait_for_run_status(
                app,
                &continuation_run_id,
                auth.as_str(),
                "completed",
                std::time::Duration::from_secs(10),
            )
            .await;
            assert_eq!(
                status, "completed",
                "continuation run should complete: {continuation_j}"
            );
        }
        other => panic!("unexpected resume disposition {other:?}: {resume_j}"),
    }

    ctx.close().await;
}

pub async fn run_paused_accounting_generation_fence_http() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let run_id = format!("run-paused-accounting-{}", ctx.suffix);
    let now = chrono::Utc::now().to_rfc3339();
    let generation = 7;
    let store = DatabaseRunStateStore::new(ctx.shared_pool.clone())
        .with_owner_pod_id("different-from-draining-executor");
    store
        .insert_run(DurableRunRecord {
            run_id: run_id.clone(),
            user_id: ctx.user_id.clone(),
            session_id: ctx.session_id.clone(),
            parent_run_id: None,
            root_run_id: Some(run_id.clone()),
            ancestor_path: Some(run_id.clone()),
            depth: 0,
            delegation_id: None,
            agent_id: Some("paused-accounting-fixture".into()),
            retry_of: None,
            retry_scope: Some("node".into()),
            status: "paused".into(),
            waiting_for: None,
            owner_pod_id: None,
            owner_lease_expires_at: None,
            run_generation: generation,
            last_event_idx: -1,
            checkpoint_version: None,
            checkpoint_json: None,
            error_code: None,
            error_message: None,
            retry_count: 0,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_tool_calls: 0,
            agent_binding_id: None,
            agent_binding_name: None,
            agent_binding_schema_version: None,
            model_offering_id: None,
            resolved_model_name: None,
            runtime_profile: None,
            start_request_fingerprint: None,
            work_binding: None,
            events: Vec::new(),
            created_at: now.clone(),
            updated_at: now,
        })
        .await
        .expect("seed paused accounting run");
    let accounting = json!({
        "event_type": "run_accounting_finalized",
        "idempotency_key": format!("run-accounting-finalized:{generation}"),
        "data": {
            "prompt_tokens": 101,
            "cache_read_tokens": 202,
            "cache_creation_tokens": 303,
            "completion_tokens": 404,
            "tool_call_count": 1,
            "usage_scope": "run_total",
            "last_request_usage": {
                "prompt_tokens": 11,
                "cache_read_tokens": 22,
                "cache_creation_tokens": 33,
                "completion_tokens": 44
            },
            "tool_outcomes": {
                "requested": 1,
                "executed": 1,
                "succeeded": 1,
                "failed": 0,
                "rejected": 0,
                "reused": 0,
                "suppressed": 0,
                "deferred": 0
            }
        }
    });
    assert!(
        store
            .append_events_if_current_generation_and_status(
                &ctx.user_id,
                &ctx.session_id,
                &run_id,
                generation,
                &["paused"],
                std::slice::from_ref(&accounting),
            )
            .await
            .expect("append paused accounting")
    );
    assert!(
        store
            .append_events_if_current_generation_and_status(
                &ctx.user_id,
                &ctx.session_id,
                &run_id,
                generation,
                &["paused"],
                std::slice::from_ref(&accounting),
            )
            .await
            .expect("retry paused accounting")
    );
    let mut conflicting = accounting.clone();
    conflicting["data"]["prompt_tokens"] = json!(999);
    store
        .append_events_if_current_generation_and_status(
            &ctx.user_id,
            &ctx.session_id,
            &run_id,
            generation,
            &["paused"],
            &[conflicting],
        )
        .await
        .expect_err("MatrixOne must reject conflicting immutable accounting");
    assert!(
        !store
            .append_events_if_current_generation_and_status(
                &ctx.user_id,
                &ctx.session_id,
                &run_id,
                generation + 1,
                &["paused"],
                std::slice::from_ref(&accounting),
            )
            .await
            .expect("reject stale accounting generation")
    );

    let (status, body) = get_json(
        &ctx.app,
        &format!("/chat/runs/{run_id}"),
        Some(&b.auth_header),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "GET paused accounting run: {body}");
    assert_eq!(body["status"], "paused");
    assert!(body["waiting_for"].is_null());
    assert_eq!(body["accounting"]["prompt_tokens"], 101);
    assert_eq!(body["accounting"]["cache_read_tokens"], 202);
    assert_eq!(body["accounting"]["cache_creation_tokens"], 303);
    assert_eq!(body["accounting"]["completion_tokens"], 404);
    assert_eq!(
        body["accounting"]["last_request_usage"]["prompt_tokens"],
        11
    );
    assert_eq!(body["accounting"]["tool_outcomes"]["succeeded"], 1);
    let finalized_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_run_events
         WHERE user_id = ? AND run_id = ? AND event_type = 'run_accounting_finalized'",
    )
    .bind(&ctx.user_id)
    .bind(&run_id)
    .fetch_one(&ctx.pool)
    .await
    .expect("count generation-fenced accounting facts");
    assert_eq!(finalized_count, 1);

    ctx.close().await;
}

pub async fn run_live_pause_wins_post_loop_settlement_accounting() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let session_id = ctx.session_id.clone();
    let (status, chat) = post_json(
        &ctx.app,
        "/chat",
        Some(&b.auth_header),
        json!({
            "message": "pause after the provider response but before settlement",
            "session_id": session_id,
            "model_selection": seeded_model_selection(ctx),
            "context": {
                "test_post_loop_settlement_delay_ms": 500,
                "test_llm_rounds": [{
                    "full_text": "provider work completed before pause",
                    "usage": {
                        "prompt_tokens": 606,
                        "completion_tokens": 404,
                        "prompt_tokens_details": {
                            "cached_tokens": 202,
                            "cache_creation_input_tokens": 303
                        }
                    }
                }]
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "start barrier run: {chat}");
    let run_id = chat["run_id"].as_str().expect("barrier run_id").to_string();

    let barrier_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_run_events
             WHERE user_id = ? AND run_id = ?
               AND event_type = 'test_post_loop_settlement_barrier_reached'",
        )
        .bind(&ctx.user_id)
        .bind(&run_id)
        .fetch_one(&ctx.pool)
        .await
        .expect("poll post-loop settlement barrier");
        if count == 1 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < barrier_deadline,
            "provider loop never reached the pre-settlement barrier"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    let (pause_status, pause) = post_empty(
        &ctx.app,
        &format!("/chat/runs/{run_id}/pause"),
        Some(&b.auth_header),
    )
    .await;
    assert_eq!(pause_status, StatusCode::OK, "pause barrier run: {pause}");

    // Resume immediately, before the deliberately delayed settlement can
    // publish its marker. The API must wait for the atomic buffered terminal
    // batch instead of manufacturing a session-continuation directive.
    let (resume_status, resume) = post_empty(
        &ctx.app,
        &format!("/chat/runs/{run_id}/resume"),
        Some(&b.auth_header),
    )
    .await;
    assert_eq!(
        resume_status,
        StatusCode::OK,
        "resume settled source: {resume}"
    );
    assert_eq!(
        resume["disposition"], "applied",
        "resume must promote the already-buffered completion: {resume}"
    );
    assert_eq!(resume["status"], "completed");
    let (source_status, source) = get_json(
        &ctx.app,
        &format!("/chat/runs/{run_id}"),
        Some(&b.auth_header),
        &[],
    )
    .await;
    assert_eq!(source_status, StatusCode::OK, "reload source run: {source}");
    assert_eq!(source["status"], "completed");
    assert!(source["waiting_for"].is_null());
    assert_eq!(source["accounting"]["prompt_tokens"], 101);
    assert_eq!(source["accounting"]["cache_read_tokens"], 202);
    assert_eq!(source["accounting"]["cache_creation_tokens"], 303);
    assert_eq!(source["accounting"]["completion_tokens"], 404);
    assert_eq!(
        source["accounting"]["last_request_usage"]["prompt_tokens"],
        101
    );
    assert_eq!(source["accounting"]["tool_outcomes"]["requested"], 0);
    let finalized_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_run_events
         WHERE user_id = ? AND run_id = ? AND event_type = 'run_accounting_finalized'",
    )
    .bind(&ctx.user_id)
    .bind(&run_id)
    .fetch_one(&ctx.pool)
    .await
    .expect("count live paused accounting facts");
    assert_eq!(finalized_count, 1);

    let (continuation_status, continuation) = post_json(
        &ctx.app,
        "/chat",
        Some(&b.auth_header),
        json!({
            "message": "start a follow-up after promoting the paused completion",
            "session_id": session_id,
            "model_selection": seeded_model_selection(ctx),
            "context": {
                "test_llm_rounds": [{"full_text": "continuation completed"}]
            }
        }),
    )
    .await;
    assert_eq!(
        continuation_status,
        StatusCode::OK,
        "start follow-up after paused accounting: {continuation}"
    );
    let continuation_run_id = continuation["run_id"].as_str().expect("follow-up run_id");
    assert_ne!(continuation_run_id, run_id);
    assert_eq!(
        harness::wait_for_run_status(
            &ctx.app,
            continuation_run_id,
            &b.auth_header,
            "completed",
            std::time::Duration::from_secs(10),
        )
        .await,
        "completed"
    );

    ctx.close().await;
}
