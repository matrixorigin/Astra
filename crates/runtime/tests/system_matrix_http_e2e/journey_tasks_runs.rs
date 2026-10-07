//! Durable run pause/resume over the real HTTP and MatrixOne wiring.

use astra_services::runs::DurableWorkRunBinding;
use astra_services::work::{GraphRevision, WorkBranchId, WorkId};
use astra_services::{DatabaseRunStateStore, DurableRunRecord, RunStateStore};
use axum::http::StatusCode;
use serde_json::json;

use super::harness::{
    self, ProviderResponse, ProviderScript, bootstrap, delete_json, get_json, post_empty,
    post_json, seeded_model_selection,
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
        claimed
            .iter()
            .all(|claim| claim.run.run_id != cancel_run_id),
        "the cancel-win terminal generation must not be claimable"
    );
    let claimed = claimed
        .iter()
        .find(|claim| claim.run.run_id == claim_run_id)
        .expect("the oldest orphan fixture must be claimed");
    assert_eq!(claimed.run.run_generation, 1);
    assert_eq!(claimed.claimed_from_generation, 0);
    assert_eq!(
        claimed.run.owner_pod_id.as_deref(),
        Some(store.owner_pod_id())
    );

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

    let entered = std::sync::Arc::new(tokio::sync::Notify::new());
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let entered_provider = entered.clone();
    let fixture_model = format!("mock-{}", ctx.suffix);
    ctx.install_native_provider(auth,vec![ProviderScript::new("paused native provider request",move |request| {
        let matched=request.path=="/v1/chat/completions" && request.body["model"]==fixture_model && request.body["stream"]==true;
        if matched { entered_provider.notify_one(); }
        matched
    },vec![ProviderResponse::Stream{content_type:"text/event-stream",chunks:vec![
        format!("data: {}\n\n",json!({"choices":[{"index":0,"delta":{"content":"matrix e2e pause/resume completed"}}]})).into_bytes(),
        format!("data: {}\n\n",json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})).into_bytes(),
        b"data: [DONE]\n\n".to_vec()
    ],release_before_chunk:Some((0,release.clone()))}])]).await;
    let (st_chat, chat_j) = post_json(
        app,
        "/chat",
        Some(auth.as_str()),
        json!({
            "message": "matrix e2e background run",
        "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},
            "session_id": session_id,
            "model_selection": seeded_model_selection(ctx),
            "context": {

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

    tokio::time::timeout(std::time::Duration::from_secs(10), entered.notified())
        .await
        .expect("actual provider request must enter before pause");
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

    release.notify_one();
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

            let fixture_model = format!("mock-{}", ctx.suffix);
            ctx.install_native_provider(auth,vec![ProviderScript::new("typed session continuation",move |request|request.path=="/v1/chat/completions" && request.body["model"]==fixture_model && request.body["stream"]==true,vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"matrix e2e session continuation completed","reasoning_content":"","tool_calls":[]},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;
            let (st_continuation, continuation_j) = post_json(
                app,
                "/chat",
                Some(auth.as_str()),
                json!({
                    "message": "matrix e2e continuation after paused run",
                "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},
                    "session_id": session_id,
                    "model_selection": seeded_model_selection(ctx),
                    "context": {

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

pub async fn run_execution_handoff_resume_reaches_provider_http() {
    use astra_services::runs::{CheckpointWriteAuthority, RunCheckpointWriteRequest};
    use futures_util::FutureExt;
    use serde_json::Value;
    use sha2::{Digest, Sha256};
    use sqlx::Row;
    use std::{panic::AssertUnwindSafe, sync::Arc, task::Poll, time::Duration};
    use tokio::sync::Notify;

    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_test_writer()
        .try_init();

    // Dropping a timed-out journey must also release the loopback HTTP tasks.
    struct Gates([Arc<Notify>; 2]);
    impl Drop for Gates {
        fn drop(&mut self) {
            for gate in &self.0 {
                gate.notify_one();
            }
        }
    }

    // Own the exact random catalog and a separate connection before bootstrap
    // can create anything. Restart may close every pool inside the fixture.
    let database = format!("astra_test_http_handoff_{}", uuid::Uuid::new_v4().simple());
    let mut cleanup_settings = astra_core::config::AppSettings::from_env()
        .expect("handoff catalog settings")
        .matrixone;
    cleanup_settings.database = "mysql".into();
    cleanup_settings.db_pool_max_connections = 1;
    cleanup_settings.db_pool_min_connections = 0;
    let catalog = tokio::time::timeout(
        Duration::from_secs(30),
        astra_core::SharedPool::new(&cleanup_settings),
    )
    .await
    .expect("bounded independent catalog connection")
    .expect("independent handoff catalog connection");
    let mut fixture = None;
    let result = AssertUnwindSafe(async {
        fixture = Some(tokio::time::timeout(
            Duration::from_secs(60),
            harness::bootstrap_isolated_execution_handoff(database.clone()),
        ).await.expect("isolated handoff fixture bootstrap"));
        let b = fixture.as_mut().expect("bootstrapped handoff fixture");
        tokio::time::timeout(Duration::from_secs(120), async {
        let ctx = &mut b.ctx;
        let auth = &b.auth_header;
        let entered = Arc::new(Notify::new());
        let gates = Gates([Arc::new(Notify::new()), Arc::new(Notify::new())]);
        let gated_response = |delta, finish, prompt, completion, gate| ProviderResponse::Stream {
            content_type: "text/event-stream",
            chunks: vec![
                format!("data: {}\n\n", json!({"choices":[{"index":0,"delta":delta}]})).into_bytes(),
                format!("data: {}\n\n", json!({"choices":[{"index":0,"delta":{},"finish_reason":finish}],
                    "usage":{"prompt_tokens":prompt,"completion_tokens":completion,"total_tokens":prompt + completion,
                        "prompt_tokens_details":{"cached_tokens":0,"cache_creation_input_tokens":0}}})).into_bytes(),
                b"data: [DONE]\n\n".to_vec(),
            ],
            release_before_chunk: Some((0, gate)),
        };
        let model = format!("mock-{}", ctx.suffix);
        let provider_entered = entered.clone();
        ctx.install_native_provider(auth, vec![ProviderScript::new(
            "original and restored same-turn primary requests",
            move |request| {
                let matched = request.path == "/v1/chat/completions"
                    && request.body["model"] == model && request.body["stream"] == true;
                if matched { provider_entered.notify_one(); }
                matched
            },
            vec![
                gated_response(json!({"tool_calls":[{"index":0,"id":"handoff-search","type":"function",
                    "function":{"name":"tool_search","arguments":json!({"query":"select:agent"}).to_string()}}]}),
                    "tool_calls", 42, 7, gates.0[0].clone()),
                gated_response(json!({"content":"Restored execution kept the original turn."}),
                    "stop", 52, 11, gates.0[1].clone()),
            ],
        )]).await;
        let message = "\n  Discover the agent tool without invoking it, then report completion.\t\n";
        let (status, admitted) = post_json(&ctx.app, "/chat", Some(auth), json!({
            "message":message, "session_id":ctx.session_id,
            "model_selection":seeded_model_selection(ctx),
            "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},
            "execution_budget":{"initial_turns":4,"hard_turn_limit":4},
            "execution_time_budget":{"remaining_seconds":30}
        })).await;
        assert_eq!(status, StatusCode::OK, "original admission: {admitted}");
        let run_id = admitted["run_id"].as_str().expect("run id").to_string();
        tokio::time::timeout(Duration::from_secs(10), entered.notified()).await
            .expect("original provider request");
        let store = DatabaseRunStateStore::new(ctx.shared_pool.clone());
        let original = store.load_run(&ctx.user_id, &run_id).await.unwrap().unwrap();
        let original_admission = original.original_admission_data().unwrap().clone();
        assert_eq!(original.depth, 0);
        assert!(original.work_binding.is_none());

        // Poll once before releasing the physical response: drain sets the
        // cooperative handoff token before its first suspension. No sleeps or
        // synthetic pause/checkpoint state establish this ordering.
        let checkpoint = {
            let drain = ctx.app_state.drain_background_runs(Duration::from_secs(5));
            tokio::pin!(drain);
            assert!(matches!(futures_util::poll!(&mut drain), Poll::Pending));
            gates.0[0].notify_one();
            let checkpoint = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if let Some(checkpoint) = store.load_latest_checkpoint(
                        &ctx.user_id, &run_id, Some("execution_handoff")
                    ).await.unwrap() {
                        break checkpoint;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            });
            let (drained, checkpoint) = tokio::join!(drain, checkpoint);
            assert!(!drained, "handoff freezes until shutdown aborts the producer");
            checkpoint.expect("settled execution_handoff, not a generic shutdown checkpoint")
        };
        assert_eq!(checkpoint.checkpoint_version, "execution_handoff_v1");
        let handoff: Value = serde_json::from_str(&checkpoint.checkpoint_json).unwrap();
        assert_eq!(handoff["producer_run_id"], run_id);
        assert_eq!(handoff["producer_owner_generation"], original.run_generation);
        let payload = &handoff["heavy"];
        let source = &payload["reservation"];
        let turn = source["reserved_turn"].as_u64().expect("original turn");
        let budget = &payload["heavy"]["run_execution_budget"];
        assert_eq!(budget["charged_iterations"], 1);
        assert_eq!(budget["remaining_iterations"], 3);
        assert_eq!(budget["effective_hard_turn_limit"], 4);
        assert_eq!(payload["original_facts"]["total_prompt"], 42);
        assert_eq!(payload["original_facts"]["total_completion"], 7);
        assert_eq!(payload["original_facts"]["total_tool_calls"], 1);
        assert!(payload["original_facts"]["canonical_turn_started_at"].is_string());
        assert!(payload["execution_deadline"]["deadline_unix_ms"].as_u64().is_some());
        assert!(payload["continuation"].is_object());
        let before_restart = store.load_run(&ctx.user_id, &run_id).await.unwrap().unwrap();
        assert!(!before_restart.events.iter().any(|event| event["event_type"] == "run_finished"));
        assert!(ctx.app_state.stop_background_runs(Duration::from_secs(5)).await);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let released: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM session_context_heads WHERE owner_user_id = ? AND session_id = ?
                     AND active_writer_json IS NULL AND active_reservation_json IS NULL"
                ).bind(&ctx.user_id).bind(&ctx.session_id).fetch_one(&ctx.pool).await.unwrap();
                if released == 1 { break; }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.expect("producer Drop releases canonical writer before pool shutdown");
        drop(store);
        ctx.restart_after_execution_handoff().await;
        let store = DatabaseRunStateStore::new(ctx.shared_pool.clone());
        let recovered = store.load_run(&ctx.user_id, &run_id).await.unwrap().unwrap();
        assert_eq!(recovered.status, "paused");
        assert!(recovered.run_generation > original.run_generation);
        assert_eq!(recovered.checkpoint_json.as_deref(), Some(checkpoint.checkpoint_json.as_str()));
        assert_eq!(ctx.native_provider_requests().await.len(), 1, "recovery cannot dispatch");

        let resume_path = format!("/chat/runs/{run_id}/resume");
        let (status, resumed) = post_empty(&ctx.app, &resume_path, Some(auth)).await;
        assert_eq!(status, StatusCode::OK, "public exact resume: {resumed}");
        assert_eq!(resumed["disposition"], "applied");
        assert_eq!(resumed["run_id"], run_id);
        assert_eq!(resumed["status"], "running");
        tokio::time::timeout(Duration::from_secs(10), entered.notified()).await
            .expect("public resume must reach the actual provider");
        let active = store.load_run(&ctx.user_id, &run_id).await.unwrap().unwrap();
        assert_eq!(active.run_generation, recovered.run_generation + 1);
        assert_eq!(active.original_admission_data().unwrap(), &original_admission);
        let receipts: Vec<String> = sqlx::query_scalar(
            "SELECT receipt_json FROM session_context_operation_receipts
             WHERE owner_user_id = ? AND session_id = ? AND operation_kind = 'resume_execution'"
        ).bind(&ctx.user_id).bind(&ctx.session_id).fetch_all(&ctx.pool).await.unwrap();
        assert_eq!(receipts.len(), 1);
        let receipt: Value = serde_json::from_str(&receipts[0]).unwrap();
        assert_eq!(receipt["run_id"], run_id);
        assert_eq!(receipt["run_generation"], active.run_generation);
        assert_eq!(receipt["checkpoint_id"], checkpoint.checkpoint_id);
        assert_eq!(receipt["producer_generation"], original.run_generation);
        assert_eq!(&receipt["source"], source);
        assert_eq!(receipt["turn_reservation"]["reserved_turn"], turn);
        assert!(receipt["writer_lease"]["writer_epoch"].as_u64().unwrap()
            > source["writer_epoch"].as_u64().unwrap());
        let slot: String = sqlx::query_scalar(
            "SELECT run_id FROM agent_session_execution_slots WHERE user_id = ? AND session_id = ?"
        ).bind(&ctx.user_id).bind(&ctx.session_id).fetch_one(&ctx.pool).await.unwrap();
        assert_eq!(slot, run_id);

        let (duplicate_status, duplicate) = post_empty(&ctx.app, &resume_path, Some(auth)).await;
        assert_eq!(duplicate_status, StatusCode::CONFLICT, "duplicate resume: {duplicate}");
        assert!(store.save_checkpoint(RunCheckpointWriteRequest {
            user_id: &ctx.user_id, expected_session_id: &ctx.session_id, run_id: &run_id,
            checkpoint_json: &checkpoint.checkpoint_json,
            authority: CheckpointWriteAuthority::ExecutionOwner {
                expected_owner_generation: original.run_generation,
            },
        }).await.unwrap().is_none(), "old producer cannot republish its checkpoint");
        assert!(!store.append_events_if_current_generation_and_status(
            &ctx.user_id, &ctx.session_id, &run_id, original.run_generation, &["running"],
            &[json!({"event_type":"run_accounting_finalized","idempotency_key":"stale-handoff-accounting",
                "data":{"prompt_tokens":999}})],
        ).await.unwrap(), "old producer cannot settle resumed usage");
        assert_eq!(store.load_run(&ctx.user_id, &run_id).await.unwrap().unwrap().run_generation,
            active.run_generation);
        assert_eq!(store.load_latest_checkpoint(&ctx.user_id, &run_id, Some("execution_handoff"))
            .await.unwrap().unwrap(), checkpoint);
        let requests = ctx.native_provider_requests().await;
        assert_eq!(requests.len(), 2, "one physical request before and after restart");
        let messages = requests[1].body["messages"].as_array().unwrap();
        assert_eq!(payload["original_user_message"], message);
        assert_eq!(requests[0].body["messages"].as_array().unwrap().iter()
            .filter(|m| m["role"] == "user" && m["content"] == message.trim()).count(), 1);
        assert_eq!(messages.iter().filter(|m| m["role"] == "user" && m["content"] == message.trim()).count(), 1);
        assert_eq!(messages.iter().filter_map(|m| m["tool_calls"].as_array()).flatten()
            .filter(|call| call["id"] == "handoff-search").count(), 1);
        let results: Vec<_> = messages.iter().filter(|m| m["role"] == "tool"
            && m["tool_call_id"] == "handoff-search").collect();
        assert_eq!(results.len(), 1);
        let saved_result = payload["heavy"]["messages"].as_array().unwrap().iter()
            .find(|m| m["role"] == "tool" && m["tool_call_id"] == "handoff-search").unwrap();
        assert_eq!(results[0]["content"], saved_result["content"], "restore exact settled tool output");
        gates.0[1].notify_one();
        assert_eq!(harness::wait_for_run_status(&ctx.app, &run_id, auth, "completed", Duration::from_secs(15)).await,
            "completed");
        let (status, completed) = get_json(&ctx.app, &format!("/chat/runs/{run_id}"), Some(auth), &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(completed["accounting"]["prompt_tokens"], 94);
        assert_eq!(completed["accounting"]["completion_tokens"], 18);
        assert_eq!(completed["accounting"]["cache_read_tokens"], 0);
        assert_eq!(completed["accounting"]["cache_creation_tokens"], 0);
        assert_eq!(completed["accounting"]["tool_outcomes"]["succeeded"], 1);
        assert_eq!(completed["accounting"]["last_request_usage"]["prompt_tokens"], 52);
        assert_eq!(completed["accounting"]["last_request_usage"]["completion_tokens"], 11);
        let terminal = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let terminal = store.load_run(&ctx.user_id, &run_id).await.unwrap().unwrap();
                if terminal.events.iter().any(|event| event["event_type"] == "run_settlement_finished"
                    && event["data"]["owner_generation"] == active.run_generation) {
                    break terminal;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.expect("exact-generation terminal settlement must close");
        assert_eq!((terminal.total_prompt_tokens, terminal.total_completion_tokens), (94, 18));
        assert_eq!(terminal.total_tool_calls, 1, "settled discovery cannot execute again on restore");
        assert_eq!(terminal.events.iter().filter(|e| e["event_type"] == "run_started").count(), 1);
        let finished: Vec<_> = terminal.events.iter().filter(|e| e["event_type"] == "run_finished").collect();
        assert_eq!(finished.len(), 1, "normal completion has one atomic accounting terminal");
        assert_eq!(finished[0]["data"]["owner_generation"], active.run_generation);
        assert_eq!(finished[0]["data"]["status"], "completed");
        assert_eq!(finished[0]["data"]["prompt_tokens"], 94);
        assert_eq!(finished[0]["data"]["completion_tokens"], 18);
        assert_eq!(finished[0]["data"]["last_request_usage"]["prompt_tokens"], 52);
        assert_eq!(terminal.events.iter().filter(|e| e["event_type"] == "run_accounting_finalized").count(), 0,
            "normal atomic completion needs no control-terminal accounting correction");
        assert!(terminal.events.iter().any(|e| e["event_type"] == "text_done"
            && e["data"]["full_text"] == "Restored execution kept the original turn."));
        let roots: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_runs WHERE user_id = ? AND session_id = ?")
            .bind(&ctx.user_id).bind(&ctx.session_id).fetch_one(&ctx.pool).await.unwrap();
        assert_eq!(roots, 1, "resume cannot admit a replacement run or delegate");
        let slots: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_session_execution_slots WHERE user_id = ? AND session_id = ?")
            .bind(&ctx.user_id).bind(&ctx.session_id).fetch_one(&ctx.pool).await.unwrap();
        assert_eq!(slots, 0);
        let completed_turn: i64 = sqlx::query_scalar(
            "SELECT completed_turn FROM session_context_heads WHERE owner_user_id = ? AND session_id = ?"
        ).bind(&ctx.user_id).bind(&ctx.session_id).fetch_one(&ctx.pool).await.unwrap();
        assert_eq!(completed_turn, turn as i64, "same reserved conversation turn commits once");

        // The normal terminal checkpoint exposes the restored loop budget;
        // checking only the parked snapshot would miss a reset on dispatch.
        let final_heavy = astra_pipeline::step_checkpoint::read_latest_heavy_checkpoint(
            &ctx.user_id, &ctx.session_id).unwrap().expect("terminal heavy checkpoint");
        let final_budget = serde_json::to_value(final_heavy.run_execution_budget.unwrap()).unwrap();
        assert_eq!(final_budget["run_id"], run_id);
        assert_eq!(final_budget["producer_owner_generation"], active.run_generation);
        assert_eq!(final_budget["charged_iterations"], 2);
        assert_eq!(final_budget["remaining_iterations"], 2);
        assert_eq!(final_budget["effective_hard_turn_limit"], 4);

        // Correlate each physical call with its canonical inference and route
        // instead of accepting aggregate run totals as provider evidence.
        let attempts = sqlx::query(
            "SELECT a.attempt_id, a.status, a.usage_status, a.input_tokens, a.output_tokens,
                    a.session_id, a.run_id, a.provider_wire_bytes, a.provider_wire_hash,
                    i.invocation_id, i.turn_index, i.round_index,
                    i.session_id AS invocation_session, i.run_id AS invocation_run,
                    r.session_id AS route_session, r.run_id AS route_run,
                    r.offering_id, r.resolved_model_name
             FROM inference_provider_attempts a
             JOIN inference_invocations i ON i.user_id = a.user_id AND i.invocation_id = a.invocation_id
             JOIN inference_routes r ON r.user_id = i.user_id AND r.route_id = i.route_id
             WHERE a.user_id = ? AND a.run_id = ? AND i.purpose = 'primary_agent'
             ORDER BY i.round_index, a.attempt_index"
        ).bind(&ctx.user_id).bind(&run_id).fetch_all(&ctx.pool).await.unwrap();
        assert_eq!(attempts.len(), 2);
        for (index, row) in attempts.iter().enumerate() {
            for column in ["session_id", "invocation_session", "route_session"] {
                assert_eq!(row.get::<String, _>(column), ctx.session_id);
            }
            for column in ["run_id", "invocation_run", "route_run"] {
                assert_eq!(row.get::<String, _>(column), run_id);
            }
            assert_eq!(row.get::<String, _>("offering_id"), ctx.model_offering_id);
            assert_eq!(row.get::<String, _>("resolved_model_name"), format!("mock-{}", ctx.suffix));
            assert_eq!(row.get::<String, _>("status"), "succeeded");
            assert_eq!(row.get::<String, _>("usage_status"), "provider_exact");
            assert_eq!(row.get::<i64, _>("turn_index"), turn as i64);
            assert_eq!((row.get::<i64, _>("input_tokens"), row.get::<i64, _>("output_tokens")), [(42, 7), (52, 11)][index]);
            assert_eq!(row.get::<i64, _>("provider_wire_bytes"), requests[index].raw_body.len() as i64);
            assert_eq!(row.get::<String, _>("provider_wire_hash"), format!("{:x}", Sha256::digest(&requests[index].raw_body)),
                "database attempt identifies the exact captured provider body");
        }
        assert_ne!(attempts[0].get::<String, _>("attempt_id"), attempts[1].get::<String, _>("attempt_id"));
        assert_ne!(attempts[0].get::<String, _>("invocation_id"), attempts[1].get::<String, _>("invocation_id"));
        assert_eq!(attempts[1].get::<i64, _>("round_index"), attempts[0].get::<i64, _>("round_index") + 1);
        assert_eq!(store.load_latest_checkpoint(&ctx.user_id, &run_id, Some("execution_handoff"))
            .await.unwrap().unwrap(), checkpoint);
        }).await.expect("bounded public execution handoff regression");
    }).catch_unwind().await;
    // Release gates (including on assertion failure) before stopping producers
    // and dropping this fixture's catalog. Keep the original assertion panic.
    let cleanup = AssertUnwindSafe(async {
        if let Some(b) = fixture.as_ref() {
            tokio::time::timeout(Duration::from_secs(30), b.ctx.close())
                .await
                .expect("bounded handoff fixture cleanup");
        }
    })
    .catch_unwind()
    .await;
    // Always attempt DROP, including incomplete bootstrap and closed-pool
    // cleanup failures. IF EXISTS covers a bootstrap that never created it.
    let dropped = AssertUnwindSafe(async {
        tokio::time::timeout(
            Duration::from_secs(30),
            sqlx::query(&format!("DROP DATABASE IF EXISTS `{database}`")).execute(catalog.get()),
        )
        .await
        .expect("bounded handoff catalog cleanup")
        .expect("remove exact owned handoff catalog");
    })
    .catch_unwind()
    .await;
    let catalog_closed = tokio::time::timeout(Duration::from_secs(5), catalog.close()).await;
    // Report the original bootstrap/journey error before any cleanup error.
    result.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    cleanup.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    dropped.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    catalog_closed.expect("bounded catalog connection close");
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
    let fixture_model = format!("mock-{}", ctx.suffix);
    ctx.install_native_provider(&b.auth_header,vec![ProviderScript::new("settlement accounting through actual provider",move |request|request.path=="/v1/chat/completions" && request.body["model"]==fixture_model && request.body["stream"]==true && request.body["messages"].as_array().is_some_and(|messages| messages.iter().any(|message|message["role"]=="user" && message["content"]=="pause after the provider response but before settlement")),vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"provider work completed before pause","reasoning_content":"","tool_calls":[]},"finish_reason":"stop"}],"usage":{
                        "prompt_tokens": 606,
                        "completion_tokens": 404,
                        "prompt_tokens_details": {
                            "cached_tokens": 202,
                            "cache_creation_input_tokens": 303
                        }
                    }}))])]).await;
    let (status, chat) = post_json(
        &ctx.app,
        "/chat",
        Some(&b.auth_header),
        json!({
            "message": "pause after the provider response but before settlement",
        "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},
            "session_id": session_id,
            "model_selection": seeded_model_selection(ctx),
            "context": {
                "test_post_loop_settlement_delay_ms": 500}
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

    let fixture_model = format!("mock-{}", ctx.suffix);
    ctx.install_native_provider(&b.auth_header,vec![ProviderScript::new("settlement accounting through actual provider",move |request|request.path=="/v1/chat/completions" && request.body["model"]==fixture_model && request.body["stream"]==true && request.body["messages"].as_array().is_some_and(|messages| messages.iter().any(|message|message["role"]=="user" && message["content"]=="start a follow-up after promoting the paused completion")),vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"continuation completed","reasoning_content":"","tool_calls":[]},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;
    let (continuation_status, continuation) = post_json(
        &ctx.app,
        "/chat",
        Some(&b.auth_header),
        json!({
            "message": "start a follow-up after promoting the paused completion",
        "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},
            "session_id": session_id,
            "model_selection": seeded_model_selection(ctx),
            "context": {

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
