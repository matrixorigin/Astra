//! Production store/ledger boundary coverage. Live tests are opt-in:
//! ASTRA_TEST_DB_IT=1 cargo test -p astra-services --test collaborator_stage_db_it -- --ignored --test-threads=1
mod common;
use common::collaborator_fixture::{self, admission, record};

use astra_core::SharedPool;
use astra_services::runs::{
    AtomicRunInteractionBatchRegistration, AtomicRunInteractionBatchRegistrationRequest,
    AtomicRunInteractionWaitRequest, COLLABORATOR_NATIVE_SESSION_METADATA_KEY,
    CollaboratorNativeSession, CollaboratorProvider, CollaboratorStageAdmission,
    CollaboratorStageReceipt, DatabaseRunStateStore, DurableRunInteractionKind,
    DurableRunInteractionResolveOutcome, DurableRunInteractionWaitOutcome, DurableRunRecord,
    InMemoryRunStateStore, RunAdmissionError, RunStateStore, ToolInteractionAdmissionError,
    ToolInvocationInteractionOrigin,
};
use astra_services::tool_invocation_ledger::{
    DatabaseToolInvocationLedger, ToolInvocationDispatchAdmission,
};
use astra_turn_types::{
    DispatchCertainty, DurableToolReference, ToolInvocationDecision, ToolInvocationFingerprint,
    ToolInvocationIdentity, ToolInvocationResultPayload, ToolInvocationState,
    ToolInvocationTerminalOutcome,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and real MatrixOne"]
async fn committed_terminal_guidance_withholding_survives_late_physical_completion() {
    use astra_services::runs::{
        AtomicRunGuidanceAdmissionRequest, AtomicRunTerminalSettlementRequest,
        AtomicRunTerminalSettlementResolution,
    };
    let fixture = Fixture::new().await;
    let (ledger, identity) = fixture.interaction_invocation().await;
    let source = json!({"event_type":"user_intent", "idempotency_key":"user_intent:uncertain-before-terminal",
        "data":{"intent_id":"uncertain-before-terminal", "delivery":"guide_current_run", "input":{"content":"a constraint"}}});
    fixture
        .store
        .admit_run_guidance(AtomicRunGuidanceAdmissionRequest {
            user_id: &fixture.user_id,
            run_id: &identity.run_id,
            expected_session_id: &fixture.session_id,
            intent_id: "uncertain-before-terminal",
            event: &source,
            process_local_execution_live: false,
        })
        .await
        .unwrap();
    let terminal = [
        json!({"event_type":"run_finished", "idempotency_key":"terminal:uncertain-guidance",
        "data":{"status":"completed"}}),
    ];
    let request = AtomicRunTerminalSettlementRequest {
        user_id: &fixture.user_id,
        run_id: &identity.run_id,
        expected_session_id: &fixture.session_id,
        expected_statuses: &["running"],
        expected_owner_generation: 1,
        status: "completed",
        waiting_for: None,
        error_message: None,
        events: &terminal,
        prompt_tokens: 0,
        completion_tokens: 0,
        tool_calls: 0,
    };
    let mut tx = fixture.pool.get().begin().await.unwrap();
    let committed = fixture
        .store
        .settle_terminal_in_existing_transaction(&mut tx, request)
        .await
        .unwrap()
        .unwrap();
    assert!(
        committed
            .committed_events
            .iter()
            .all(|event| event["event_type"] != "user_intent_returned")
    );
    tx.commit().await.unwrap();
    let outcome = ToolInvocationTerminalOutcome::Succeeded {
        result: ToolInvocationResultPayload::new("finished later", BTreeMap::new(), None).unwrap(),
    };
    ledger
        .compare_and_complete(
            &identity,
            ToolInvocationState::Dispatched,
            Some("native-interaction-worker"),
            &outcome,
        )
        .await
        .unwrap();
    for receipts in [None, Some(committed.event_receipts.as_slice())] {
        let resolved = fixture
            .store
            .resolve_atomic_terminal_settlement(request, receipts)
            .await
            .unwrap();
        assert!(
            matches!(resolved, AtomicRunTerminalSettlementResolution::Exact(_)),
            "late completion cannot change the committed terminal prefix: {resolved:?}"
        );
    }
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and real MatrixOne"]
async fn provider_ack_settlement_records_acceptance_without_resuming_paused_or_cancelled_work() {
    use astra_services::runs::{
        AtomicRunGuidanceAdmission, AtomicRunGuidanceAdmissionRequest, AtomicRunUserIntentApply,
        AtomicRunUserIntentApplyRequest, UserIntentApplyPurpose,
    };
    for (status, rejected) in [
        ("paused", false),
        ("cancelled", false),
        ("paused", true),
        ("cancelled", true),
    ] {
        let fixture = Fixture::new().await;
        let (_ledger, identity) = fixture.interaction_invocation().await;
        let source = json!({"event_type":"user_intent", "idempotency_key":"user_intent:accepted-before-control",
            "data":{"intent_id":"accepted-before-control", "delivery":"guide_current_run", "input":{"content":"additional constraint"}}});
        let admitted = fixture
            .store
            .admit_run_guidance(AtomicRunGuidanceAdmissionRequest {
                user_id: &fixture.user_id,
                run_id: &identity.run_id,
                expected_session_id: &fixture.session_id,
                intent_id: "accepted-before-control",
                event: &source,
                process_local_execution_live: false,
            })
            .await
            .unwrap();
        let AtomicRunGuidanceAdmission::Committed { event_index } = admitted else {
            panic!("source admission failed");
        };
        if status == "cancelled" {
            assert!(
                fixture
                    .store
                    .request_run_cancellation(&fixture.user_id, &identity.run_id)
                    .await
                    .unwrap()
            );
        }
        let control_events = if status == "cancelled" {
            vec![
                json!({"event_type":"run_finished", "idempotency_key":"control:cancelled",
                "data":{"status":"cancelled", "cancelled":true, "cancellation_origin":"user"}}),
            ]
        } else {
            Vec::new()
        };
        assert!(
            fixture
                .store
                .update_run_status_with_events_if_current(
                    &fixture.user_id,
                    &fixture.session_id,
                    &identity.run_id,
                    &["running"],
                    Some(1),
                    status,
                    None,
                    None,
                    &control_events,
                )
                .await
                .unwrap()
        );
        let make_request = |purpose| AtomicRunUserIntentApplyRequest {
            purpose,
            invocation_identity: Some(&identity),
            user_id: &fixture.user_id,
            run_id: &identity.run_id,
            expected_session_id: &fixture.session_id,
            expected_owner_generation: 1,
            source_event_indices: std::slice::from_ref(&event_index),
        };
        let ordinary = fixture
            .store
            .apply_run_user_intents(make_request(UserIntentApplyPurpose::ModelBoundary))
            .await;
        assert!(
            matches!(
                ordinary,
                Err(_) | Ok(AtomicRunUserIntentApply::Inactive { .. })
            ),
            "ordinary action must be rejected, never record an applied effect: {ordinary:?}"
        );
        let provider_purpose = |dispatch_owner| {
            if rejected {
                UserIntentApplyPurpose::ProviderRejection {
                    dispatch_owner: Some(dispatch_owner),
                }
            } else {
                UserIntentApplyPurpose::ProviderAcknowledgement { dispatch_owner }
            }
        };
        assert!(
            fixture
                .store
                .apply_run_user_intents(make_request(provider_purpose("foreign-worker")))
                .await
                .is_err()
        );
        let settled = fixture
            .store
            .apply_run_user_intents(make_request(provider_purpose("native-interaction-worker")))
            .await
            .unwrap();
        assert!(
            matches!(
                settled,
                AtomicRunUserIntentApply::Returned { .. } if rejected
            ) || matches!(settled, AtomicRunUserIntentApply::Applied { .. } if !rejected)
        );
        let replay = fixture
            .store
            .apply_run_user_intents(make_request(provider_purpose("native-interaction-worker")))
            .await
            .unwrap();
        assert!(
            matches!(replay, AtomicRunUserIntentApply::Returned { .. } if rejected)
                || matches!(replay, AtomicRunUserIntentApply::AlreadyApplied { .. } if !rejected)
        );
        let observed = fixture
            .store
            .load_run_observation(&fixture.user_id, &identity.run_id, 128)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            observed.run.status, status,
            "recording an ACK must not resume execution"
        );
        assert!(
            observed
                .run
                .events
                .iter()
                .any(|event| event["event_type"] == "user_intent_returned")
                == rejected,
            "only conclusive rejection may return the original source"
        );
        assert_eq!(
            observed
                .run
                .events
                .iter()
                .filter(|event| event["event_type"]
                    == if rejected {
                        "user_intent_returned"
                    } else {
                        "user_intent_applied"
                    })
                .count(),
            1,
            "reconciliation must not duplicate the disposition"
        );
        fixture.cleanup().await;
    }
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and real MatrixOne"]
async fn terminal_presentation_survives_maximum_durable_observation_window() {
    let fixture = Fixture::new().await;
    let full = json!({
        "type": "tool_call_end", "run_id": fixture.parent_id,
        "session_id": fixture.session_id, "call_id": "large-call", "tool": "native_codex",
        "result": "界\\\"\n".repeat(12_000), "status": "completed", "success": true,
        "guidance_disposition": "unconfirmed",
        "native_stage_observation": {
            "native_session_id": "thread", "native_turn_id": "turn", "dispatch_state": "acknowledged",
            "settlement_authoritative": true, "native_terminal": "completed",
            "stage_inclusive_input_tokens": 30, "stage_usage": {"cached_input_tokens": 10, "output_tokens": 5},
            "last_request_input_tokens": null, "model_context_window": null,
            "acknowledged_model": null, "provider_error_code": null, "provider_error_class": null
        }
    });
    let event = astra_services::runs::project_tool_terminal_presentation(full.clone(), 4096 - 128);
    assert!(serde_json::to_vec(&event).unwrap().len() <= 4096 - 128);
    assert_eq!(event["guidance_disposition"], "unconfirmed");
    use sha2::Digest;
    let original_result = serde_json::to_vec(&full["result"]).unwrap();
    assert_eq!(event["result_bytes"], original_result.len());
    assert_eq!(
        event["result_sha256"],
        format!("{:x}", sha2::Sha256::digest(&original_result))
    );
    for status in ["completed", "unknown"] {
        let mut candidate = full.clone();
        candidate["status"] = json!(status);
        candidate["success"] = if status == "unknown" {
            serde_json::Value::Null
        } else {
            json!(true)
        };
        for budget in [1024, 4096 - 128] {
            let projected =
                astra_services::runs::project_tool_terminal_presentation(candidate.clone(), budget);
            assert!(serde_json::to_vec(&projected).unwrap().len() <= budget);
            assert_eq!(projected["guidance_disposition"], "unconfirmed");
            assert_eq!(projected["status"], candidate["status"]);
            assert_eq!(projected["success"], candidate["success"]);
            assert_eq!(
                astra_services::runs::project_tool_terminal_presentation(projected.clone(), budget),
                projected
            );
        }
    }
    let mut oversized_identity = full.clone();
    oversized_identity["call_id"] = json!("界".repeat(4000));
    let gap = astra_services::runs::project_tool_terminal_presentation(oversized_identity, 1024);
    assert_eq!(gap["type"], "stream_gap");
    assert_eq!(gap["guidance_disposition"], "unconfirmed");
    assert_eq!(
        event["native_stage_observation"],
        full["native_stage_observation"]
    );
    let replay =
        astra_services::runs::project_tool_terminal_presentation(event.clone(), 4096 - 128);
    assert_eq!(
        replay, event,
        "replay must preserve original integrity facts"
    );
    fixture
        .store
        .append_event(
            &fixture.user_id,
            &fixture.session_id,
            &fixture.parent_id,
            event.clone(),
        )
        .await
        .unwrap();
    let observation = fixture
        .store
        .load_run_observation(&fixture.user_id, &fixture.parent_id, 256)
        .await
        .unwrap()
        .unwrap();
    // insert_run already records run_created; presentation adds exactly one
    // event, not a second observation ledger or auxiliary fact row.
    assert_eq!(observation.total_event_count, 2);
    assert_eq!(observation.run.events.len(), 2);
    let retained = observation
        .run
        .events
        .iter()
        .find(|event| event["call_id"] == "large-call")
        .unwrap();
    assert_eq!(retained["index"], 1);
    assert_eq!(retained["run_id"], fixture.parent_id);
    assert_eq!(retained["session_id"], fixture.session_id);
    assert_eq!(retained["call_id"], "large-call");
    assert_eq!(
        retained["native_stage_observation"],
        event["native_stage_observation"]
    );
    assert_eq!(retained["result_sha256"], event["result_sha256"]);
    assert_eq!(retained["guidance_disposition"], "unconfirmed");
    assert!(
        fixture
            .store
            .load_run_observation("foreign-owner", &fixture.parent_id, 256)
            .await
            .unwrap()
            .is_none()
    );
    let initial = astra_services::runs::project_tool_terminal_presentation(
        json!({
            "type":"tool_call_end", "call_id":"lifecycle-call", "tool":"agent_fanout",
            "result":{
                "group_id":"group", "target_count":3, "results":[{"result":"x".repeat(10_000)}],
                "fanout":{"group_id":"group", "slots":[{"run_id":"child-a"},{"run_id":"child-b"},{"run_id":"r".repeat(512)}]},
                "work_unit_observation":{"id":"group", "kind":"agent_fanout", "status":"completed",
                    "revision":1, "mode":"current", "wake_policy":"none"}
            }
        }),
        4096 - 128,
    );
    let mut rebound = initial.clone();
    rebound["run_id"] = json!(fixture.parent_id);
    rebound["session_id"] = json!(fixture.session_id);
    rebound["run_generation"] = json!(1);
    rebound["idempotency_key"] = json!("subrun-tool:1:lifecycle-call");
    rebound["workspace"] = json!({"path":""});
    let padding =
        (4096 - 128 + 64usize).saturating_sub(serde_json::to_vec(&rebound).unwrap().len());
    rebound["workspace"]["path"] = json!("x".repeat(padding));
    assert!(serde_json::to_vec(&rebound).unwrap().len() > 4096 - 128);
    let rebound = astra_services::runs::project_tool_terminal_presentation(rebound, 4096 - 128);
    assert_eq!(rebound["result"]["group_id"], "group");
    assert_eq!(
        rebound["result"]["content_sha256"],
        initial["result"]["content_sha256"]
    );
    assert_eq!(
        rebound["result"]["original_bytes"],
        initial["result"]["original_bytes"]
    );
    assert_eq!(
        astra_services::runs::project_tool_terminal_presentation(rebound.clone(), 4096 - 128),
        rebound
    );
    fixture
        .store
        .append_event(
            &fixture.user_id,
            &fixture.session_id,
            &fixture.parent_id,
            rebound.clone(),
        )
        .await
        .unwrap();
    let observation = fixture
        .store
        .load_run_observation(&fixture.user_id, &fixture.parent_id, 256)
        .await
        .unwrap()
        .unwrap();
    let retained = observation
        .run
        .events
        .iter()
        .find(|event| event["call_id"] == "lifecycle-call")
        .unwrap();
    assert_eq!(retained["result"], rebound["result"]);
    assert_eq!(retained["result_sha256"], initial["result_sha256"]);
    assert_eq!(retained["run_generation"], 1);
    fixture.cleanup().await;
}

struct Fixture {
    pool: SharedPool,
    store: DatabaseRunStateStore,
    user_id: String,
    session_id: String,
    parent_id: String,
    owner: String,
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and real MatrixOne"]
async fn cancelled_native_receipt_recovers_session_after_store_restart_without_resuming_work() {
    use astra_services::runs::{
        AtomicExecutionOwnerCancellation, AtomicExecutionOwnerCancellationRequest,
        ExecutionOwnerCancellationOrigin,
    };
    let fixture = Fixture::new().await;
    let (mut request, first) = fixture.first().await;
    let (ledger, identity) = fixture.invocation(&first.run_id).await;
    let native = CollaboratorNativeSession {
        anchor_run_id: first.anchor_run_id.clone(),
        provider: CollaboratorProvider::Codex,
        native_session_id: Uuid::new_v4().to_string(),
    };
    let metadata = BTreeMap::from([
        (
            COLLABORATOR_NATIVE_SESSION_METADATA_KEY.into(),
            serde_json::to_value(&native).unwrap(),
        ),
        ("workspace_effect_settled".into(), json!(true)),
    ]);
    let result =
        ToolInvocationResultPayload::new("partial output before cancellation", metadata, None)
            .unwrap();
    ledger
        .compare_and_complete(
            &identity,
            ToolInvocationState::Dispatched,
            Some("native-session-worker"),
            &ToolInvocationTerminalOutcome::Failed {
                result,
                error_kind: Some("cancelled".into()),
                retryable: false,
            },
        )
        .await
        .unwrap();
    assert!(
        fixture
            .store
            .release_owner_lease(&fixture.user_id, &fixture.session_id, &first.run_id, 1)
            .await
            .unwrap()
    );
    let new_owner = Uuid::new_v4().to_string();
    let restarted = DatabaseRunStateStore::new(fixture.pool.clone()).with_owner_pod_id(&new_owner);
    assert!(
        restarted
            .acquire_owner_lease(
                &fixture.user_id,
                &first.run_id,
                &new_owner,
                Duration::from_secs(300)
            )
            .await
            .unwrap()
    );
    assert_eq!(
        restarted
            .cancel_if_exact_live_owner(AtomicExecutionOwnerCancellationRequest {
                user_id: &fixture.user_id,
                run_id: &first.run_id,
                expected_session_id: &fixture.session_id,
                expected_owner_generation: 2,
                expected_statuses: &["running"],
                origin: ExecutionOwnerCancellationOrigin::Runtime,
                reason: "cancelled after receipt commit",
            })
            .await
            .unwrap(),
        AtomicExecutionOwnerCancellation::Committed
    );
    assert!(
        fixture
            .store
            .load_collaborator_association(
                &fixture.user_id,
                &fixture.session_id,
                &first.anchor_run_id
            )
            .await
            .unwrap()
            .unwrap()
            .native_session
            .is_none()
    );
    assert!(matches!(
        restarted
            .recover_collaborator_native_session(
                &fixture.user_id,
                &fixture.session_id,
                &first.anchor_run_id,
                &first.run_id,
                1
            )
            .await,
        Err(RunAdmissionError::ParentNotCurrent { .. })
    ));
    assert!(matches!(
        restarted
            .recover_collaborator_native_session(
                &fixture.user_id,
                &fixture.session_id,
                &first.anchor_run_id,
                "wrong-predecessor",
                2
            )
            .await,
        Err(RunAdmissionError::PreviousStageChanged { .. })
    ));
    let grant: (i64, String) = sqlx::query_as("SELECT event_idx, event_hash FROM agent_run_events
        WHERE user_id = ? AND session_id = ? AND run_id = ? AND event_type = 'action_admission_granted' LIMIT 1")
        .bind(&fixture.user_id).bind(&fixture.session_id).bind(&first.run_id)
        .fetch_one(fixture.pool.get()).await.unwrap();
    sqlx::query("UPDATE agent_run_events SET event_hash = ? WHERE user_id = ? AND run_id = ? AND event_idx = ?")
        .bind("0".repeat(64)).bind(&fixture.user_id).bind(&first.run_id).bind(grant.0)
        .execute(fixture.pool.get()).await.unwrap();
    assert!(matches!(
        restarted
            .recover_collaborator_native_session(
                &fixture.user_id,
                &fixture.session_id,
                &first.anchor_run_id,
                &first.run_id,
                2
            )
            .await,
        Err(RunAdmissionError::NativeSessionUnproven)
    ));
    sqlx::query("UPDATE agent_run_events SET event_hash = ? WHERE user_id = ? AND run_id = ? AND event_idx = ?")
        .bind(grant.1).bind(&fixture.user_id).bind(&first.run_id).bind(grant.0)
        .execute(fixture.pool.get()).await.unwrap();
    assert_eq!(
        restarted
            .recover_collaborator_native_session(
                &fixture.user_id,
                &fixture.session_id,
                &first.anchor_run_id,
                &first.run_id,
                2
            )
            .await
            .unwrap(),
        Some(native.clone())
    );
    assert_eq!(
        restarted
            .recover_collaborator_native_session(
                &fixture.user_id,
                &fixture.session_id,
                &first.anchor_run_id,
                &first.run_id,
                2
            )
            .await
            .unwrap(),
        Some(native.clone())
    );
    let cancelled = restarted
        .load_run(&fixture.user_id, &first.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cancelled.status, "cancelled");
    assert!(cancelled.owner_pod_id.is_none());
    let origin: String = sqlx::query_scalar("SELECT payload_json FROM agent_run_events
        WHERE user_id = ? AND session_id = ? AND run_id = ? AND event_type = 'collaborator_native_session_bound' LIMIT 1")
        .bind(&fixture.user_id).bind(&fixture.session_id).bind(&first.anchor_run_id)
        .fetch_one(fixture.pool.get()).await.unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&origin).unwrap()["owner_generation"],
        1
    );
    assert!(matches!(
        restarted
            .recover_collaborator_native_session(
                "other-user",
                &fixture.session_id,
                &first.anchor_run_id,
                &first.run_id,
                2
            )
            .await,
        Err(RunAdmissionError::SessionUnavailable)
    ));
    // The existing live parent admits a later stage; recording the old receipt
    // did not restart or renew the cancelled stage or create another root run.
    // Transfer its lease through the actual recovery API too: the new store
    // cannot admit work using the previous node's still-live parent authority.
    assert!(
        fixture
            .store
            .release_owner_lease(&fixture.user_id, &fixture.session_id, &fixture.parent_id, 1)
            .await
            .unwrap()
    );
    assert!(
        restarted
            .acquire_owner_lease(
                &fixture.user_id,
                &fixture.parent_id,
                &new_owner,
                Duration::from_secs(300)
            )
            .await
            .unwrap()
    );
    let next = record(
        &fixture.user_id,
        &fixture.session_id,
        Some(&fixture.parent_id),
    );
    request.expected_previous_stage_run_id = Some(first.run_id.clone());
    request.expected_parent_generation = 2;
    request.source_message_id = Uuid::new_v4().to_string();
    request.request_fingerprint = "later-stage-intent".into();
    let receipt = restarted
        .insert_run_with_collaborator_stage(next, request, None)
        .await
        .unwrap();
    assert_ne!(receipt.run_id, first.run_id);
    let recovered = restarted
        .load_collaborator_association(&fixture.user_id, &fixture.session_id, &first.anchor_run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(recovered.native_session, Some(native));
    fixture.cleanup().await;
}

impl Fixture {
    fn physical_workspace_id(&self) -> String {
        collaborator_fixture::physical_workspace_id()
    }

    async fn cleanup(&self) {
        // This fixture owns a freshly generated user. Never widen cleanup to
        // other users, even when test binaries share the same database.
        for table in [
            "tool_invocation_ledger",
            "tool_invocation_archive_chunks",
            "edge_pending_dispatch",
        ] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DELETE FROM {table} WHERE user_id = ? AND session_id = ?"
            )))
            .bind(&self.user_id)
            .bind(&self.session_id)
            .execute(self.pool.get())
            .await
            .unwrap();
        }
        sqlx::query(
            "DELETE FROM session_execution_bindings WHERE owner_user_id = ? AND session_id = ?",
        )
        .bind(&self.user_id)
        .bind(&self.session_id)
        .execute(self.pool.get())
        .await
        .unwrap();
        common::cleanup_work_owner(&self.pool, &self.user_id).await;
    }
    async fn new() -> Self {
        let (pool, _) = common::setup_pool_and_settings().await;
        let user_id = Uuid::new_v4().to_string();
        let session_id = Uuid::new_v4().to_string();
        let owner = Uuid::new_v4().to_string();
        let store = DatabaseRunStateStore::new(pool.clone())
            .with_owner_pod_id(&owner)
            .with_lease_ttl(Duration::from_secs(300));
        sqlx::query("INSERT INTO agent_sessions (user_id, session_id, status, created_at, updated_at, last_active_at)
                     VALUES (?, ?, 'active', NOW(6), NOW(6), NOW(6))")
            .bind(&user_id).bind(&session_id).execute(pool.get()).await.unwrap();
        collaborator_fixture::seed_execution_binding(&pool, &user_id, &session_id).await;
        let parent = record(&user_id, &session_id, None);
        let parent_id = parent.run_id.clone();
        store.insert_run(parent).await.unwrap();
        Self {
            pool,
            store,
            user_id,
            session_id,
            parent_id,
            owner,
        }
    }

    fn child(&self) -> DurableRunRecord {
        record(&self.user_id, &self.session_id, Some(&self.parent_id))
    }

    async fn first(&self) -> (CollaboratorStageAdmission, CollaboratorStageReceipt) {
        let child = self.child();
        let request = admission(&child.run_id);
        let receipt = self
            .store
            .insert_run_with_collaborator_stage(child, request.clone(), None)
            .await
            .unwrap();
        assert!(!receipt.replayed);
        (request, receipt)
    }

    async fn settle(&self, run_id: &str) {
        self.settle_as(run_id, "completed").await;
    }

    async fn bind_native_session(&self, run_id: &str) {
        let native = CollaboratorNativeSession {
            anchor_run_id: run_id.into(),
            provider: CollaboratorProvider::Codex,
            native_session_id: Uuid::new_v4().to_string(),
        };
        let (ledger, identity) = self.invocation(run_id).await;
        let result = ToolInvocationResultPayload::new(
            "session acknowledged",
            BTreeMap::from([
                ("workspace_effect_settled".into(), json!(true)),
                (
                    COLLABORATOR_NATIVE_SESSION_METADATA_KEY.into(),
                    serde_json::to_value(&native).unwrap(),
                ),
            ]),
            None,
        )
        .unwrap();
        ledger
            .compare_and_complete(
                &identity,
                ToolInvocationState::Dispatched,
                Some("native-session-worker"),
                &ToolInvocationTerminalOutcome::Succeeded { result },
            )
            .await
            .unwrap();
        self.store
            .confirm_collaborator_native_session(&identity, &native, 1)
            .await
            .unwrap();
    }

    async fn settle_as(&self, run_id: &str, status: &str) {
        assert!(
            self.store
                .update_run_status_with_events_if_current(
                    &self.user_id,
                    &self.session_id,
                    run_id,
                    &["running"],
                    Some(1),
                    status,
                    None,
                    None,
                    &[json!({"event_type": "turn_completed"})],
                )
                .await
                .unwrap()
        );
    }

    async fn invocation(
        &self,
        run_id: &str,
    ) -> (DatabaseToolInvocationLedger, ToolInvocationIdentity) {
        let identity = ToolInvocationIdentity::new(
            &self.user_id,
            &self.session_id,
            run_id,
            "native-session-stage",
            Uuid::new_v4().to_string(),
        )
        .unwrap();
        let ledger = DatabaseToolInvocationLedger::new(self.pool.clone());
        let decision = ToolInvocationDecision::new(
            &json!({"route":"user_runner", "runner_id":"selected-runner"}),
        )
        .unwrap();
        let association = self
            .store
            .load_collaborator_association(&self.user_id, &self.session_id, run_id)
            .await
            .unwrap()
            .unwrap();
        let tool = DurableToolReference::Provider {
            descriptor: association.latest_native_execution.descriptor,
        };
        let fingerprint = ToolInvocationFingerprint::new(
            tool,
            &json!({"command":"native-session-create"}),
            &decision.decision_id,
        )
        .unwrap();
        ledger
            .prepare(&identity, &fingerprint, &decision)
            .await
            .unwrap();
        ledger
            .claim_dispatch(
                &identity,
                "native-session-worker",
                90_000,
                ToolInvocationDispatchAdmission {
                    expected_control_epoch: -1,
                    expected_owner_generation: 1,
                    expected_owner_pod_id: self.owner.clone(),
                    expected_execution_binding_generation: None,
                },
            )
            .await
            .unwrap();
        (ledger, identity)
    }

    async fn interaction_invocation(
        &self,
    ) -> (DatabaseToolInvocationLedger, ToolInvocationIdentity) {
        let child = self.child();
        let stage = admission(&child.run_id);
        collaborator_fixture::seed_live_edge_invocation(
            &self.pool,
            &self.store,
            child,
            stage,
            &self.owner,
        )
        .await
    }
}

fn interaction_event(
    origin: &ToolInvocationInteractionOrigin,
    native_id: serde_json::Value,
) -> serde_json::Value {
    json!({"event_type":"provider_interaction_required", "idempotency_key":"server-provider-interaction-required:native-question", "data":{
        "request_id":"native-question", "session_id":origin.identity.session_id, "run_id":origin.identity.run_id,
        "tool_invocation_origin":origin,
        "interaction":{"request_id":"native-question", "payload":{"native_request_id":native_id, "question":"Continue?"}, "timeout_ms":60000},
        "delivery":"durable", "timeout_ms":60000
    }})
}

async fn register_interaction(
    store: &DatabaseRunStateStore,
    origin: &ToolInvocationInteractionOrigin,
    event: &serde_json::Value,
) -> Result<AtomicRunInteractionBatchRegistration, String> {
    eprintln!("native interaction: register start");
    let started = std::time::Instant::now();
    let result = store
        .register_guarded_interaction_batch(AtomicRunInteractionBatchRegistrationRequest {
            user_id: &origin.identity.user_id,
            run_id: &origin.identity.run_id,
            expected_session_id: &origin.identity.session_id,
            expected_control_epoch: origin.control_epoch,
            expected_owner_generation: origin.owner_generation,
            events: std::slice::from_ref(event),
        })
        .await;
    eprintln!(
        "native interaction: register complete {:?}",
        started.elapsed()
    );
    result
}

async fn begin_interaction(
    store: &DatabaseRunStateStore,
    origin: &ToolInvocationInteractionOrigin,
) -> Result<DurableRunInteractionWaitOutcome, String> {
    eprintln!("native interaction: begin wait start");
    let started = std::time::Instant::now();
    let result = store
        .begin_run_interaction_wait(AtomicRunInteractionWaitRequest {
            user_id: &origin.identity.user_id,
            run_id: &origin.identity.run_id,
            expected_session_id: &origin.identity.session_id,
            request_id: "native-question",
            kind: DurableRunInteractionKind::Provider,
            expected_control_epoch: origin.control_epoch,
            expected_owner_generation: origin.owner_generation,
        })
        .await;
    eprintln!(
        "native interaction: begin wait complete {:?}",
        started.elapsed()
    );
    result
}

async fn resolve_interaction(
    store: &DatabaseRunStateStore,
    origin: &ToolInvocationInteractionOrigin,
    response: serde_json::Value,
) -> DurableRunInteractionResolveOutcome {
    eprintln!("native interaction: resolve start");
    let started = std::time::Instant::now();
    let result = store
        .resolve_run_interaction(
            &origin.identity.user_id,
            &origin.identity.session_id,
            &origin.identity.run_id,
            "native-question",
            DurableRunInteractionKind::Provider,
            response,
        )
        .await
        .unwrap();
    eprintln!(
        "native interaction: resolve complete {:?}",
        started.elapsed()
    );
    result
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and real MatrixOne"]
async fn native_interaction_real_producer_register_begin_resolve_and_early_answer() {
    for (early, native_id) in [(false, json!(7)), (true, json!("7"))] {
        let fixture = Fixture::new().await;
        let (ledger, identity) = fixture.interaction_invocation().await;
        let origin = fixture
            .store
            .derive_tool_interaction_origin(
                &identity,
                "selected-runner",
                &fixture.physical_workspace_id(),
            )
            .await
            .unwrap();
        assert_eq!(origin.owner_generation, 1);
        assert_eq!(origin.execution_binding_generation, 1);
        // A different server handles callbacks; it must not acquire or replace
        // the execution owner merely to register/wait/answer an interaction.
        let callback =
            DatabaseRunStateStore::new(fixture.pool.clone()).with_owner_pod_id("callback-node");
        let event = interaction_event(&origin, native_id.clone());
        assert_eq!(
            register_interaction(&callback, &origin, &event)
                .await
                .unwrap(),
            AtomicRunInteractionBatchRegistration::Registered
        );
        assert_eq!(
            register_interaction(&callback, &origin, &event)
                .await
                .unwrap(),
            AtomicRunInteractionBatchRegistration::Registered
        );
        let response = json!({"request_id":"native-question", "outcome":"submitted", "payload":{"answer":"yes"}});
        if early {
            assert!(matches!(
                resolve_interaction(&callback, &origin, response.clone()).await,
                DurableRunInteractionResolveOutcome::Queued(_)
            ));
            assert!(matches!(
                begin_interaction(&callback, &origin).await.unwrap(),
                DurableRunInteractionWaitOutcome::AlreadyResolved(_)
            ));
        } else {
            assert_eq!(
                begin_interaction(&callback, &origin).await.unwrap(),
                DurableRunInteractionWaitOutcome::Waiting
            );
            let waiting = fixture
                .store
                .load_run(&fixture.user_id, &identity.run_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(waiting.status, "waiting");
            assert_eq!(waiting.waiting_for.as_deref(), Some("provider_interaction"));
            assert!(matches!(
                resolve_interaction(&callback, &origin, response.clone()).await,
                DurableRunInteractionResolveOutcome::Resolved(_)
            ));
        }
        let resumed = fixture
            .store
            .load_run(&fixture.user_id, &identity.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resumed.status, "running");
        assert_eq!(
            resumed.owner_pod_id.as_deref(),
            Some(fixture.owner.as_str())
        );
        assert_eq!(resumed.run_generation, 1);
        assert_eq!(
            ledger.get(&identity).await.unwrap().unwrap().state,
            ToolInvocationState::Dispatched
        );
        let required = fixture
            .store
            .load_run_interaction_event(
                &fixture.user_id,
                &identity.run_id,
                "native-question",
                "provider_interaction_required",
            )
            .await
            .unwrap()
            .unwrap();
        assert!(
            required
                .get("index")
                .and_then(serde_json::Value::as_u64)
                .is_some(),
            "native ACK must use the committed required event cursor"
        );
        assert_eq!(
            required["data"]["interaction"]["payload"]["native_request_id"],
            native_id
        );
        assert!(matches!(
            resolve_interaction(&callback, &origin, response).await,
            DurableRunInteractionResolveOutcome::Idempotent(_)
        ));
        assert!(matches!(resolve_interaction(&callback, &origin, json!({"request_id":"native-question", "outcome":"submitted", "payload":{"answer":"no"}})).await, DurableRunInteractionResolveOutcome::Conflict(_)));
        fixture.cleanup().await;
    }
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and real MatrixOne"]
async fn native_interaction_keeps_dispatch_authority_without_consuming_unacknowledged_guidance() {
    let fixture = Fixture::new().await;
    let (_ledger, identity) = fixture.interaction_invocation().await;
    let intent_id = "guidance-before-provider-ack";
    fixture
        .store
        .append_event(
            &fixture.user_id,
            &fixture.session_id,
            &identity.run_id,
            json!({
                "event_type": "user_intent",
                "idempotency_key": format!("user_intent:{intent_id}"),
                "data": {
                    "intent_id": intent_id,
                    "delivery": "guide_current_run",
                    "input": {"content": "continue with the review"}
                }
            }),
        )
        .await
        .unwrap();
    let source = fixture
        .store
        .load_run_event_by_idempotency_key(
            &fixture.user_id,
            &identity.run_id,
            "user_intent",
            &format!("user_intent:{intent_id}"),
        )
        .await
        .unwrap()
        .unwrap();
    let source_index = source["index"].as_i64().unwrap();
    let provisional = fixture
        .store
        .derive_tool_interaction_origin(
            &identity,
            "selected-runner",
            &fixture.physical_workspace_id(),
        )
        .await
        .unwrap();
    assert!(source_index > provisional.control_epoch);
    let event = interaction_event(&provisional, json!(7));

    assert_eq!(
        register_interaction(&fixture.store, &provisional, &event)
            .await
            .unwrap(),
        AtomicRunInteractionBatchRegistration::Registered
    );
    assert_eq!(
        begin_interaction(&fixture.store, &provisional)
            .await
            .unwrap(),
        DurableRunInteractionWaitOutcome::Waiting
    );

    let durable = fixture
        .store
        .load_run(&fixture.user_id, &identity.run_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        durable.events.iter().all(|event| {
            event.get("event_type").and_then(|value| value.as_str()) != Some("user_intent_applied")
        }),
        "registering a pre-ACK provider question must not consume guidance"
    );
    fixture.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and real MatrixOne"]
async fn native_interaction_lookup_work_does_not_grow_with_guidance_history() {
    use sqlx::Row;
    use std::sync::{Arc, Mutex};
    use tracing::{
        field::{Field, Visit},
        instrument::WithSubscriber,
    };
    use tracing_subscriber::{Layer, layer::SubscriberExt};

    #[derive(Default)]
    struct Sample {
        sql: String,
        rows: u64,
        seconds: f64,
    }
    impl Visit for Sample {
        fn record_str(&mut self, field: &Field, value: &str) {
            if field.name() == "db.statement" {
                self.sql = value.to_owned();
            }
        }
        fn record_u64(&mut self, field: &Field, value: u64) {
            if field.name() == "rows_returned" {
                self.rows = value;
            }
        }
        fn record_f64(&mut self, field: &Field, value: f64) {
            if field.name() == "elapsed_secs" {
                self.seconds = value;
            }
        }
        fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
    }
    struct Lookups(Arc<Mutex<Vec<Sample>>>);
    impl<S: tracing::Subscriber> Layer<S> for Lookups {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if event.metadata().target() != "sqlx::query" {
                return;
            }
            let mut sample = Sample::default();
            event.record(&mut sample);
            if sample.sql.contains("invocation_supersession_scope_key = ?")
                && sample.sql.contains("event_idx >")
            {
                self.0.lock().unwrap().push(sample);
            }
        }
    }
    let mut baseline = [None, None];
    for history in [0, 64, 1024] {
        for early in [false, true] {
            let fixture = Fixture::new().await;
            let (_ledger, identity) = fixture.interaction_invocation().await;
            let origin = fixture
                .store
                .derive_tool_interaction_origin(
                    &identity,
                    "selected-runner",
                    &fixture.physical_workspace_id(),
                )
                .await
                .unwrap();
            let events: Vec<_> = (0..history).map(|n| json!({"event_type":"user_intent", "idempotency_key":format!("user_intent:guide-{n}"),
                "data":{"intent_id":format!("guide-{n}"), "delivery":"guide_current_run", "input":{"content":" 界与新增约束 \n"}}})).collect();
            fixture
                .store
                .append_events_batch(
                    &fixture.user_id,
                    &fixture.session_id,
                    &identity.run_id,
                    &events,
                )
                .await
                .unwrap();
            // New store instance has no process-local validation cursor/cache.
            let callback = DatabaseRunStateStore::new(fixture.pool.clone())
                .with_owner_pod_id(&fixture.owner)
                .with_lease_ttl(Duration::from_secs(300));
            let samples = Arc::new(Mutex::new(Vec::new()));
            let subscriber = tracing::Dispatch::new(
                tracing_subscriber::registry().with(Lookups(samples.clone())),
            );
            async {
                let required = interaction_event(&origin, json!(7));
                assert!(matches!(register_interaction(&callback, &origin, &required).await.unwrap(), AtomicRunInteractionBatchRegistration::Registered));
                let response = json!({"request_id":"native-question","outcome":"submitted","payload":{"answer":"yes"}});
                if early {
                    assert!(matches!(resolve_interaction(&callback, &origin, response).await, DurableRunInteractionResolveOutcome::Queued(_)));
                    assert!(matches!(begin_interaction(&callback, &origin).await.unwrap(), DurableRunInteractionWaitOutcome::AlreadyResolved(_)));
                } else {
                    assert_eq!(begin_interaction(&callback, &origin).await.unwrap(), DurableRunInteractionWaitOutcome::Waiting);
                    assert!(matches!(resolve_interaction(&callback, &origin, response).await, DurableRunInteractionResolveOutcome::Resolved(_)));
                }
            }.with_subscriber(subscriber).await;
            let sql = {
                let samples = samples.lock().unwrap();
                assert!(
                    !samples.is_empty(),
                    "capture actual SQLx lookups, not assumed counts"
                );
                assert_eq!(
                    *baseline[usize::from(early)].get_or_insert(samples.len()),
                    samples.len(),
                    "query count grew with {history} guides"
                );
                assert!(
                    samples
                        .iter()
                        .all(|s| s.rows == 0 && !s.sql.contains("payload_json"))
                );
                eprintln!(
                    "guides={history} early={early} queries={} returned_rows=0 lookup_ms={:.3}",
                    samples.len(),
                    samples.iter().map(|s| s.seconds).sum::<f64>() * 1000.0
                );
                samples[0].sql.clone()
            };
            if history == 1024 && !early {
                // Diagnose the producer key; interaction uses prepared SQL.
                let key: String = sqlx::query_scalar("SELECT invocation_supersession_scope_key FROM agent_run_events WHERE user_id=? AND session_id=? AND run_id=? AND event_type='action_admission_granted' LIMIT 1")
                    .bind(&fixture.user_id).bind(&fixture.session_id).bind(&identity.run_id).fetch_one(fixture.pool.get()).await.unwrap();
                assert!(key.len() == 64 && key.bytes().all(|c| c.is_ascii_hexdigit()));
                let mut explain = format!("EXPLAIN ANALYZE {sql}");
                explain = explain.replacen('?', &format!("'{key}'"), 1);
                explain = explain.replacen('?', &origin.control_epoch.to_string(), 1);
                let rows = sqlx::raw_sql(sqlx::AssertSqlSafe(explain))
                    .fetch_all(fixture.pool.get())
                    .await
                    .unwrap();
                let plan = rows
                    .iter()
                    .map(|r| r.try_get::<String, _>(0).unwrap())
                    .collect::<Vec<_>>()
                    .join("\n");
                eprintln!("native intent lookup plan:\n{plan}");
                assert!(
                    plan.contains("Index Table Scan on agent_run_events.idx_agent_run_events_invocation_control"),
                    "must use purpose index: {plan}"
                );
                let lines: Vec<_> = plan.lines().collect();
                for marker in ["Index Table Scan on", "Table Scan on"] {
                    let scan = lines
                        .iter()
                        .position(|line| {
                            line.contains(marker)
                                && (marker.starts_with("Index")
                                    || !line.contains("Index Table Scan"))
                        })
                        .unwrap();
                    let input_rows = lines[scan + 1]
                        .split_whitespace()
                        .find_map(|part| part.strip_prefix("inputRows="))
                        .unwrap()
                        .parse::<u64>()
                        .unwrap();
                    assert_eq!(
                        input_rows, 1,
                        "only the grant, never guidance history: {plan}"
                    );
                }
            }
            // A malformed guide is a superseding fact; a claimed projection
            // inside the payload cannot grant exclusion from this check.
            let invalid = json!({"event_type":"user_intent", "idempotency_key":"user_intent:invalid", "invocation_supersession_scope_key":true,
                "data":{"intent_id":"invalid", "delivery":"guide_current_run", "input":{"content":" \n"}}});
            fixture
                .store
                .append_event(
                    &fixture.user_id,
                    &fixture.session_id,
                    &identity.run_id,
                    invalid,
                )
                .await
                .unwrap();
            let run = fixture
                .store
                .load_run(&fixture.user_id, &identity.run_id)
                .await
                .unwrap()
                .unwrap();
            let positive_samples = Arc::new(Mutex::new(Vec::new()));
            let subscriber = tracing::Dispatch::new(
                tracing_subscriber::registry().with(Lookups(positive_samples.clone())),
            );
            assert_eq!(
                register_interaction(&callback, &origin, &interaction_event(&origin, json!(7)))
                    .with_subscriber(subscriber)
                    .await
                    .unwrap(),
                AtomicRunInteractionBatchRegistration::Superseded {
                    user_intent_event_index: run.last_event_idx
                }
            );
            {
                let positive_samples = positive_samples.lock().unwrap();
                assert_eq!(positive_samples.len(), 1);
                assert_eq!(
                    positive_samples[0].rows, 1,
                    "calibrate the observer with a real superseding row"
                );
            }
            if history == 1024 && !early {
                let first = run.last_event_idx;
                fixture.store.append_event(&fixture.user_id, &fixture.session_id, &identity.run_id,
                    json!({"event_type":"user_intent","idempotency_key":"user_intent:later","data":{"intent_id":"later","delivery":"replace_objective","input":"new request"}})).await.unwrap();
                assert_eq!(
                    register_interaction(&callback, &origin, &interaction_event(&origin, json!(7)))
                        .await
                        .unwrap(),
                    AtomicRunInteractionBatchRegistration::Superseded {
                        user_intent_event_index: first
                    }
                );
                // A stored candidate key is not tenant authority. Inject a
                // corrupt header and verify the real admission fails closed.
                let foreign = Uuid::new_v4().to_string();
                sqlx::query("UPDATE agent_run_events SET user_id=? WHERE user_id=? AND run_id=? AND event_idx=?")
                    .bind(&foreign).bind(&fixture.user_id).bind(&identity.run_id).bind(first).execute(fixture.pool.get()).await.unwrap();
                assert!(
                    register_interaction(&callback, &origin, &interaction_event(&origin, json!(7)))
                        .await
                        .is_err()
                );
                sqlx::query("UPDATE agent_run_events SET user_id=? WHERE user_id=? AND run_id=? AND event_idx=?")
                    .bind(&fixture.user_id).bind(&foreign).bind(&identity.run_id).bind(first).execute(fixture.pool.get()).await.unwrap();
                // Other indexed intents cannot replace the canonical grant.
                sqlx::query("UPDATE agent_run_events SET invocation_supersession_scope_key=NULL WHERE user_id=? AND run_id=? AND event_type='action_admission_granted'")
                    .bind(&fixture.user_id).bind(&identity.run_id).execute(fixture.pool.get()).await.unwrap();
                assert!(matches!(
                    fixture
                        .store
                        .derive_tool_interaction_origin(
                            &identity,
                            "selected-runner",
                            &fixture.physical_workspace_id()
                        )
                        .await,
                    Err(ToolInteractionAdmissionError::Unproven)
                ));
            }
            fixture.cleanup().await;
        }
    }
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and real MatrixOne"]
async fn native_question_remains_answerable_after_current_run_guidance() {
    use astra_services::runs::{
        AtomicRunGuidanceAdmission, AtomicRunGuidanceAdmissionRequest, AtomicRunUserIntentApply,
        AtomicRunUserIntentApplyRequest,
    };
    // Capture the origin once. Guidance can arrive before registration,
    // between registration and waiting, or while the question is visible.
    for (timing, accepted) in [(0, true), (1, true), (2, true), (2, false)] {
        let fixture = Fixture::new().await;
        let (ledger, identity) = fixture.interaction_invocation().await;
        let origin = fixture
            .store
            .derive_tool_interaction_origin(
                &identity,
                "selected-runner",
                &fixture.physical_workspace_id(),
            )
            .await
            .unwrap();
        let required = interaction_event(&origin, json!(7));
        if timing >= 1 {
            register_interaction(&fixture.store, &origin, &required)
                .await
                .unwrap();
        }
        if timing == 2 {
            assert_eq!(
                begin_interaction(&fixture.store, &origin).await.unwrap(),
                DurableRunInteractionWaitOutcome::Waiting
            );
        }
        let source = json!({"event_type":"user_intent", "idempotency_key":"user_intent:live-guidance",
            "data":{"intent_id":"live-guidance", "delivery":"guide_current_run", "input":{"content":"Also consider the new constraint."}}});
        let admitted = fixture
            .store
            .admit_run_guidance(AtomicRunGuidanceAdmissionRequest {
                user_id: &fixture.user_id,
                run_id: &identity.run_id,
                expected_session_id: &fixture.session_id,
                intent_id: "live-guidance",
                event: &source,
                process_local_execution_live: false,
            })
            .await
            .unwrap();
        let AtomicRunGuidanceAdmission::Committed { event_index } = admitted else {
            panic!("guidance must enter canonical custody: {admitted:?}")
        };
        // These are the same canonical application calls made only after a
        // positive Edge ACK. Protocol ACK certainty is covered at its owner.
        if timing == 2 {
            let ordinary = fixture
                .store
                .apply_run_user_intents(AtomicRunUserIntentApplyRequest {
                    purpose: astra_services::runs::UserIntentApplyPurpose::ModelBoundary,
                    invocation_identity: None,
                    user_id: &fixture.user_id,
                    run_id: &identity.run_id,
                    expected_session_id: &fixture.session_id,
                    expected_owner_generation: origin.owner_generation,
                    source_event_indices: &[event_index],
                })
                .await
                .unwrap();
            assert!(
                matches!(ordinary, AtomicRunUserIntentApply::Inactive { .. }),
                "Waiting is not a general execution permission"
            );
            let mut forged = identity.clone();
            forged.invocation_id = Uuid::new_v4().to_string();
            assert!(
                fixture
                    .store
                    .apply_run_user_intents(AtomicRunUserIntentApplyRequest {
                        purpose: astra_services::runs::UserIntentApplyPurpose::ModelBoundary,
                        invocation_identity: Some(&forged),
                        user_id: &fixture.user_id,
                        run_id: &identity.run_id,
                        expected_session_id: &fixture.session_id,
                        expected_owner_generation: origin.owner_generation,
                        source_event_indices: &[event_index],
                    })
                    .await
                    .is_err(),
                "an arbitrary identity is not invocation proof"
            );
        }
        let applied = if accepted {
            Some(
                fixture
                    .store
                    .apply_run_user_intents(AtomicRunUserIntentApplyRequest {
                        purpose: astra_services::runs::UserIntentApplyPurpose::ModelBoundary,
                        invocation_identity: Some(&identity),

                        user_id: &fixture.user_id,
                        run_id: &identity.run_id,
                        expected_session_id: &fixture.session_id,
                        expected_owner_generation: origin.owner_generation,
                        source_event_indices: &[event_index],
                    })
                    .await
                    .unwrap(),
            )
        } else {
            None
        };
        if timing == 0 {
            register_interaction(&fixture.store, &origin, &required)
                .await
                .unwrap();
        }
        if timing < 2 {
            assert_eq!(
                begin_interaction(&fixture.store, &origin).await.unwrap(),
                DurableRunInteractionWaitOutcome::Waiting
            );
        }
        let resolved = resolve_interaction(&fixture.store, &origin,
            json!({"request_id":"native-question", "outcome":"submitted", "payload":{"answer":"yes"}})).await;
        let run = fixture
            .store
            .load_run(&fixture.user_id, &identity.run_id)
            .await
            .unwrap()
            .unwrap();
        let invocation = ledger.get(&identity).await.unwrap().unwrap();
        fixture.cleanup().await;
        if accepted {
            assert!(
                matches!(applied, Some(AtomicRunUserIntentApply::Applied { .. })),
                "accepted guidance while waiting: {applied:?}"
            );
        }
        assert!(
            matches!(resolved, DurableRunInteractionResolveOutcome::Resolved(_)),
            "timing={timing}, accepted={accepted}: {resolved:?}"
        );
        assert_eq!(run.status, "running");
        assert_eq!(run.run_generation, origin.owner_generation);
        assert_eq!(invocation.state, ToolInvocationState::Dispatched);
        assert_eq!(
            run.events
                .iter()
                .filter(|event| event["event_type"] == "user_intent_applied")
                .count(),
            usize::from(accepted)
        );
    }
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and real MatrixOne"]
async fn native_interaction_real_producer_accepts_reconnect_and_rejects_wrong_workspace() {
    let fixture = Fixture::new().await;
    let (_ledger, identity) = fixture.interaction_invocation().await;
    assert!(matches!(
        fixture
            .store
            .derive_tool_interaction_origin(&identity, "other-runner", "other-physical",)
            .await,
        Err(ToolInteractionAdmissionError::Unproven)
    ));
    let origin = fixture
        .store
        .derive_tool_interaction_origin(
            &identity,
            "reconnected-runner",
            &fixture.physical_workspace_id(),
        )
        .await
        .unwrap();
    let before = fixture
        .store
        .load_run(&fixture.user_id, &identity.run_id)
        .await
        .unwrap()
        .unwrap();
    for field in [
        "user",
        "session",
        "run",
        "invocation",
        "binding",
        "epoch",
        "descriptor",
        "physical_workspace",
        "owner",
    ] {
        let mut forged = origin.clone();
        match field {
            "user" => forged.identity.user_id = Uuid::new_v4().to_string(),
            "session" => forged.identity.session_id = Uuid::new_v4().to_string(),
            "run" => forged.identity.run_id = Uuid::new_v4().to_string(),
            "invocation" => forged.identity.invocation_id = Uuid::new_v4().to_string(),
            "binding" => forged.execution_binding_generation += 1,
            "epoch" => forged.control_epoch += 1,
            "descriptor" => {
                forged.descriptor.descriptor_version =
                    astra_turn_types::DescriptorVersion::new("forged-descriptor").unwrap()
            }
            "physical_workspace" => forged.physical_workspace_id = "other-physical".into(),
            "owner" => forged.owner_generation += 1,
            _ => unreachable!(),
        }
        // Keep the authoritative target: only the persisted origin is forged.
        // Changing the target too would merely test lookup of a missing Run.
        assert!(
            register_interaction(
                &fixture.store,
                &origin,
                &interaction_event(&forged, json!(7))
            )
            .await
            .is_err(),
            "{field}"
        );
        assert!(
            fixture
                .store
                .load_run_interaction_event(
                    &fixture.user_id,
                    &identity.run_id,
                    "native-question",
                    "provider_interaction_required"
                )
                .await
                .unwrap()
                .is_none(),
            "{field}"
        );
        let after = fixture
            .store
            .load_run(&fixture.user_id, &identity.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                after.status,
                after.last_event_idx,
                after.run_generation,
                after.owner_pod_id
            ),
            (
                before.status.clone(),
                before.last_event_idx,
                before.run_generation,
                before.owner_pod_id.clone()
            ),
            "{field}"
        );
    }
    // Rejection must not poison admission of the authoritative producer facts.
    assert_eq!(
        register_interaction(
            &fixture.store,
            &origin,
            &interaction_event(&origin, json!(7))
        )
        .await
        .unwrap(),
        AtomicRunInteractionBatchRegistration::Registered
    );
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and real MatrixOne"]
async fn native_interaction_real_producer_unknown_dispatch_fences_answer() {
    let fixture = Fixture::new().await;
    let (ledger, identity) = fixture.interaction_invocation().await;
    let origin = fixture
        .store
        .derive_tool_interaction_origin(
            &identity,
            "selected-runner",
            &fixture.physical_workspace_id(),
        )
        .await
        .unwrap();
    register_interaction(
        &fixture.store,
        &origin,
        &interaction_event(&origin, json!(7)),
    )
    .await
    .unwrap();
    assert_eq!(
        begin_interaction(&fixture.store, &origin).await.unwrap(),
        DurableRunInteractionWaitOutcome::Waiting
    );
    ledger
        .mark_outcome_unknown(&identity, "native-interaction-worker")
        .await
        .unwrap();
    assert!(matches!(
        fixture
            .store
            .derive_tool_interaction_origin(
                &identity,
                "selected-runner",
                &fixture.physical_workspace_id(),
            )
            .await,
        Err(ToolInteractionAdmissionError::Unproven)
    ));
    assert!(matches!(resolve_interaction(&fixture.store, &origin,
        json!({"request_id":"native-question", "outcome":"submitted", "payload":{"answer":"yes"}})).await,
        DurableRunInteractionResolveOutcome::NoLongerWaiting));
    let run = fixture
        .store
        .load_run(&fixture.user_id, &identity.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(run.status, "waiting");
    assert_eq!(run.run_generation, origin.owner_generation);
    assert_eq!(
        ledger.get(&identity).await.unwrap().unwrap().state,
        ToolInvocationState::OutcomeUnknown
    );
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and real MatrixOne"]
async fn native_interaction_real_producer_settlement_lease_and_cancel_fence_answer() {
    for fence in ["settled", "dispatch_lease", "owner_lease", "cancelled"] {
        let fixture = Fixture::new().await;
        let (ledger, identity) = fixture.interaction_invocation().await;
        let origin = fixture
            .store
            .derive_tool_interaction_origin(
                &identity,
                "selected-runner",
                &fixture.physical_workspace_id(),
            )
            .await
            .unwrap();
        register_interaction(
            &fixture.store,
            &origin,
            &interaction_event(&origin, json!(7)),
        )
        .await
        .unwrap();
        assert_eq!(
            begin_interaction(&fixture.store, &origin).await.unwrap(),
            DurableRunInteractionWaitOutcome::Waiting
        );
        match fence {
            "settled" => {
                let outcome = ToolInvocationTerminalOutcome::Succeeded {
                    result: ToolInvocationResultPayload::new("done", BTreeMap::new(), None)
                        .unwrap(),
                };
                ledger
                    .compare_and_complete(
                        &identity,
                        ToolInvocationState::Dispatched,
                        Some("native-interaction-worker"),
                        &outcome,
                    )
                    .await
                    .unwrap();
            }
            "cancelled" => {
                assert!(
                    fixture
                        .store
                        .request_run_cancellation(&fixture.user_id, &identity.run_id)
                        .await
                        .unwrap()
                );
            }
            "dispatch_lease" => {
                // Deterministic clock-expiry fixture, not a fabricated producer fact.
                let result = sqlx::query("UPDATE tool_invocation_ledger SET dispatch_lease_expires_at = TIMESTAMPADD(SECOND, -1, NOW(6)) WHERE user_id = ? AND session_id = ? AND run_id = ? AND turn_chain_id = ? AND invocation_id = ?")
                    .bind(&identity.user_id).bind(&identity.session_id).bind(&identity.run_id)
                    .bind(&identity.turn_chain_id).bind(&identity.invocation_id).execute(fixture.pool.get()).await.unwrap();
                assert_eq!(result.rows_affected(), 1);
            }
            "owner_lease" => {
                let result = sqlx::query("UPDATE agent_runs SET owner_lease_expires_at = TIMESTAMPADD(SECOND, -1, NOW(6)) WHERE user_id = ? AND session_id = ? AND run_id = ?")
                    .bind(&identity.user_id).bind(&identity.session_id).bind(&identity.run_id)
                    .execute(fixture.pool.get()).await.unwrap();
                assert_eq!(result.rows_affected(), 1);
            }
            _ => unreachable!(),
        }
        let before = fixture
            .store
            .load_run(&fixture.user_id, &identity.run_id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(
                fixture
                    .store
                    .derive_tool_interaction_origin(
                        &identity,
                        "selected-runner",
                        &fixture.physical_workspace_id(),
                    )
                    .await,
                Err(ToolInteractionAdmissionError::Unproven)
            ),
            "{fence}"
        );
        assert!(matches!(resolve_interaction(&fixture.store, &origin,
            json!({"request_id":"native-question", "outcome":"submitted", "payload":{"answer":"yes"}})).await,
            DurableRunInteractionResolveOutcome::NoLongerWaiting), "{fence}");
        let after = fixture
            .store
            .load_run(&fixture.user_id, &identity.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                after.status,
                after.last_event_idx,
                after.run_generation,
                after.owner_pod_id
            ),
            (
                before.status,
                before.last_event_idx,
                before.run_generation,
                before.owner_pod_id
            ),
            "{fence}"
        );
        fixture.cleanup().await;
    }
}

#[test]
fn receipt_wire_is_observation_and_admission_requires_supported_native_identity() {
    let request = admission("anchor");
    let encoded = serde_json::to_value(&request).unwrap();
    assert_eq!(
        serde_json::from_value::<CollaboratorStageAdmission>(encoded.clone()).unwrap(),
        request
    );
    for (field, invalid) in [
        ("provider", json!("internal_model")),
        ("boundary", json!({"kind":"server_managed"})),
        (
            "boundary",
            json!({"kind":"cli", "runner_id":"selected-runner"}),
        ),
        ("locator", Value::Null),
        ("missing_locator", Value::Null),
    ] {
        let mut rejected = encoded.clone();
        match field {
            "provider" => rejected["association"]["provider"] = invalid,
            "boundary" => rejected["association"]["execution_boundary"] = invalid,
            "locator" => rejected["native_execution"] = invalid,
            _ => {
                rejected.as_object_mut().unwrap().remove("native_execution");
            }
        }
        assert!(
            serde_json::from_value::<CollaboratorStageAdmission>(rejected).is_err(),
            "{field}"
        );
    }
    let receipt = CollaboratorStageReceipt {
        anchor_run_id: "anchor".into(),
        source_message_id: "source".into(),
        run_id: "original-child".into(),
        owner_generation: 1,
        replayed: true,
    };
    assert!(receipt.replayed);
    assert!(
        serde_json::from_value::<CollaboratorStageReceipt>(serde_json::to_value(&receipt).unwrap())
            .unwrap()
            .replayed
    );
    // A reconstructed durable receipt is not a dispatch grant. The store
    // explicitly marks source replay; runtime must reconcile the original ID.
    assert_eq!(receipt.run_id, "original-child");
}

#[tokio::test]
async fn memory_store_does_not_invent_durable_ledger_or_session_evidence() {
    let store = InMemoryRunStateStore::new();
    let child = record("user", "session", Some("parent"));
    let request = admission(&child.run_id);
    assert!(matches!(
        store
            .insert_run_with_collaborator_stage(child, request, None)
            .await,
        Err(RunAdmissionError::Unsupported)
    ));
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn source_replay_survives_store_recreation_and_does_not_create_candidate() {
    let fixture = Fixture::new().await;
    let (request, first) = fixture.first().await;
    let reopened =
        DatabaseRunStateStore::new(fixture.pool.clone()).with_owner_pod_id(&fixture.owner);
    let candidate = fixture.child();
    let candidate_id = candidate.run_id.clone();
    let replay = reopened
        .insert_run_with_collaborator_stage(candidate, request.clone(), None)
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.run_id, first.run_id);
    assert!(
        reopened
            .load_run(&fixture.user_id, &candidate_id)
            .await
            .unwrap()
            .is_none()
    );
    let mut conflict = request;
    conflict.request_fingerprint = "changed-intent".into();
    assert!(matches!(
        reopened
            .insert_run_with_collaborator_stage(fixture.child(), conflict, None)
            .await,
        Err(RunAdmissionError::SourceMessageConflict { .. })
    ));
    let association = reopened
        .load_collaborator_association(&fixture.user_id, &fixture.session_id, &first.anchor_run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(association.latest_stage.run_id, first.run_id);
    assert!(association.native_session.is_none());
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn concurrent_next_stages_have_one_winner_and_preserve_terminal_anchor() {
    let fixture = Fixture::new().await;
    let (mut request, first) = fixture.first().await;
    request.expected_previous_stage_run_id = Some(first.run_id.clone());
    request.source_message_id = Uuid::new_v4().to_string();
    assert!(matches!(
        fixture
            .store
            .insert_run_with_collaborator_stage(fixture.child(), request.clone(), None)
            .await,
        Err(RunAdmissionError::PreviousStageActive { .. })
    ));
    fixture.bind_native_session(&first.run_id).await;
    fixture.settle(&first.run_id).await;
    let before = fixture
        .store
        .load_run(&fixture.user_id, &first.run_id)
        .await
        .unwrap()
        .unwrap();
    let other_store =
        DatabaseRunStateStore::new(fixture.pool.clone()).with_owner_pod_id(&fixture.owner);
    let mut other_request = request.clone();
    other_request.source_message_id = Uuid::new_v4().to_string();
    let (left, right) = tokio::join!(
        fixture
            .store
            .insert_run_with_collaborator_stage(fixture.child(), request, None),
        other_store.insert_run_with_collaborator_stage(fixture.child(), other_request, None),
    );
    assert!(matches!(
        (&left, &right),
        (Ok(_), Err(RunAdmissionError::PreviousStageChanged { .. }))
            | (Err(RunAdmissionError::PreviousStageChanged { .. }), Ok(_))
    ));
    let after = fixture
        .store
        .load_run(&fixture.user_id, &first.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.status, "completed");
    assert_eq!(after.checkpoint_json, before.checkpoint_json);
    assert_eq!(after.run_generation, before.run_generation);
    assert_eq!(after.last_event_idx, before.last_event_idx + 1);
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn codex_binding_uses_real_ledger_terminal_outcome() {
    let provider = CollaboratorProvider::Codex;
    for task_succeeded in [true, false] {
        let fixture = Fixture::new().await;
        let (mut request, first) = fixture.first().await;
        let native = CollaboratorNativeSession {
            anchor_run_id: first.anchor_run_id.clone(),
            provider: provider.clone(),
            native_session_id: Uuid::new_v4().to_string(),
        };
        let (ledger, identity) = fixture.invocation(&first.run_id).await;
        assert!(matches!(
            fixture
                .store
                .confirm_collaborator_native_session(&identity, &native, 1)
                .await,
            Err(RunAdmissionError::NativeSessionUnproven)
        ));
        let mut metadata = BTreeMap::new();
        metadata.insert("workspace_effect_settled".into(), json!(true));
        metadata.insert(
            COLLABORATOR_NATIVE_SESSION_METADATA_KEY.into(),
            serde_json::to_value(&native).unwrap(),
        );
        let result =
            ToolInvocationResultPayload::new("session acknowledged", metadata, None).unwrap();
        // Native ACK alone is insufficient: descriptor, dispatch and physical
        // settlement are independent requirements. Keep the settlement probes
        // otherwise valid, with their own immutable invocation receipts.
        for (cached, other_descriptor, settled) in [
            (false, false, Some(true)),
            (false, true, Some(true)),
            (true, false, Some(true)),
            (false, false, None),
            (false, false, Some(false)),
        ] {
            let probe = ToolInvocationIdentity::new(
                &fixture.user_id,
                &fixture.session_id,
                &first.run_id,
                "proof-counterexample",
                Uuid::new_v4().to_string(),
            )
            .unwrap();
            let decision = ToolInvocationDecision::new(&json!({"route":"user_runner"})).unwrap();
            let tool = if other_descriptor {
                DurableToolReference::Provider {
                    descriptor: astra_turn_types::ResolvedToolDescriptorRef::new(
                        request.native_execution.descriptor.identity.clone(),
                        "different-descriptor-version",
                    )
                    .unwrap(),
                }
            } else if cached || settled != Some(true) {
                DurableToolReference::Provider {
                    descriptor: request.native_execution.descriptor.clone(),
                }
            } else {
                DurableToolReference::built_in("bash", "registry-v1").unwrap()
            };
            let fingerprint = ToolInvocationFingerprint::new(
                tool,
                &json!({"task":"proof"}),
                &decision.decision_id,
            )
            .unwrap();
            ledger
                .prepare(&probe, &fingerprint, &decision)
                .await
                .unwrap();
            let mut probe_result = result.clone();
            if let Some(settled) = settled {
                probe_result
                    .metadata
                    .insert("workspace_effect_settled".into(), json!(settled));
            } else {
                probe_result.metadata.remove("workspace_effect_settled");
            }
            if cached {
                ledger
                    .complete_from_semantic_read_cache(
                        &probe,
                        &probe_result,
                        &astra_turn_types::ToolInvocationCompletionSource::semantic_read_cache(
                            format!("sha256:{}", "a".repeat(64)),
                            format!("sha256:{}", "b".repeat(64)),
                        )
                        .unwrap(),
                    )
                    .await
                    .unwrap();
            } else {
                ledger
                    .claim_dispatch(
                        &probe,
                        "proof-worker",
                        90_000,
                        ToolInvocationDispatchAdmission {
                            expected_control_epoch: -1,
                            expected_owner_generation: 1,
                            expected_owner_pod_id: fixture.owner.clone(),
                            expected_execution_binding_generation: None,
                        },
                    )
                    .await
                    .unwrap();
                ledger
                    .compare_and_complete(
                        &probe,
                        ToolInvocationState::Dispatched,
                        Some("proof-worker"),
                        &ToolInvocationTerminalOutcome::Succeeded {
                            result: probe_result,
                        },
                    )
                    .await
                    .unwrap();
            }
            assert!(matches!(
                fixture
                    .store
                    .confirm_collaborator_native_session(&probe, &native, 1)
                    .await,
                Err(RunAdmissionError::NativeSessionUnproven)
            ));
            assert!(
                fixture
                    .store
                    .load_collaborator_association(
                        &fixture.user_id,
                        &fixture.session_id,
                        &first.anchor_run_id,
                    )
                    .await
                    .unwrap()
                    .unwrap()
                    .native_session
                    .is_none(),
                "rejected proof must not establish continuation authority"
            );
        }
        let outcome = if task_succeeded {
            ToolInvocationTerminalOutcome::Succeeded { result }
        } else {
            ToolInvocationTerminalOutcome::Failed {
                result,
                error_kind: Some("provider_quota_exceeded".into()),
                retryable: false,
            }
        };
        if !task_succeeded {
            ledger
                .mark_outcome_unknown(&identity, "native-session-worker")
                .await
                .unwrap();
            assert!(matches!(
                fixture
                    .store
                    .confirm_collaborator_native_session(&identity, &native, 1)
                    .await,
                Err(RunAdmissionError::NativeSessionUnproven)
            ));
            assert!(
                fixture
                    .store
                    .load_collaborator_association(
                        &fixture.user_id,
                        &fixture.session_id,
                        &first.anchor_run_id
                    )
                    .await
                    .unwrap()
                    .unwrap()
                    .native_session
                    .is_none(),
                "unknown dispatch cannot establish a native session"
            );
        }
        let completed = ledger
            .compare_and_complete(
                &identity,
                if task_succeeded {
                    ToolInvocationState::Dispatched
                } else {
                    ToolInvocationState::OutcomeUnknown
                },
                task_succeeded.then_some("native-session-worker"),
                &outcome,
            )
            .await
            .unwrap();
        assert_eq!(
            completed.state,
            if task_succeeded {
                ToolInvocationState::Succeeded
            } else {
                ToolInvocationState::Failed
            }
        );
        assert_eq!(completed.dispatch_certainty, DispatchCertainty::Dispatched);
        let mut mismatched = native.clone();
        mismatched.native_session_id = Uuid::new_v4().to_string();
        assert!(matches!(
            fixture
                .store
                .confirm_collaborator_native_session(&identity, &mismatched, 1)
                .await,
            Err(RunAdmissionError::NativeSessionUnproven)
        ));
        assert!(matches!(
            fixture
                .store
                .confirm_collaborator_native_session(&identity, &native, 2)
                .await,
            Err(RunAdmissionError::ParentNotCurrent { .. })
        ));
        fixture
            .store
            .confirm_collaborator_native_session(&identity, &native, 1)
            .await
            .unwrap();
        let mut unrelated = identity.clone();
        unrelated.invocation_id = Uuid::new_v4().to_string();
        assert!(matches!(
            fixture
                .store
                .confirm_collaborator_native_session(&unrelated, &native, 1)
                .await,
            Err(RunAdmissionError::NativeSessionUnproven)
        ));
        fixture
            .store
            .confirm_collaborator_native_session(&identity, &native, 1)
            .await
            .unwrap();
        let reopened =
            DatabaseRunStateStore::new(fixture.pool.clone()).with_owner_pod_id(&fixture.owner);
        let association = reopened
            .load_collaborator_association(
                &fixture.user_id,
                &fixture.session_id,
                &first.anchor_run_id,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(association.native_session, Some(native));
        assert_eq!(
            association.latest_native_execution, request.native_execution,
            "reopening the owner preserves the native model and descriptor, not only their digest"
        );
        fixture
            .settle_as(
                &first.run_id,
                if task_succeeded {
                    "completed"
                } else {
                    "failed"
                },
            )
            .await;
        request.expected_previous_stage_run_id = Some(first.run_id);
        request.source_message_id = Uuid::new_v4().to_string();
        let next = reopened
            .insert_run_with_collaborator_stage(fixture.child(), request, None)
            .await
            .unwrap();
        assert!(!next.replayed);
        assert_ne!(next.run_id, next.anchor_run_id);
        fixture.cleanup().await;
    }
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn outcome_unknown_and_dispatched_previous_stages_fence_admission() {
    let fixture = Fixture::new().await;
    let (mut request, first) = fixture.first().await;
    let (ledger, identity) = fixture.invocation(&first.run_id).await;
    fixture.settle(&first.run_id).await;
    request.expected_previous_stage_run_id = Some(first.run_id);
    request.source_message_id = Uuid::new_v4().to_string();
    for unknown in [false, true] {
        if unknown {
            ledger
                .mark_outcome_unknown(&identity, "native-session-worker")
                .await
                .unwrap();
        }
        assert!(matches!(
            fixture
                .store
                .insert_run_with_collaborator_stage(fixture.child(), request.clone(), None)
                .await,
            Err(RunAdmissionError::UnsettledDispatch { .. })
        ));
    }
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn tenant_generation_cancel_and_failed_insert_do_not_publish_association() {
    let fixture = Fixture::new().await;
    let child = fixture.child();
    let mut request = admission(&child.run_id);
    let mut foreign = child.clone();
    foreign.user_id = Uuid::new_v4().to_string();
    assert!(matches!(
        fixture
            .store
            .insert_run_with_collaborator_stage(foreign, request.clone(), None)
            .await,
        Err(RunAdmissionError::SessionUnavailable)
    ));
    request.expected_parent_generation = 2;
    assert!(matches!(
        fixture
            .store
            .insert_run_with_collaborator_stage(child.clone(), request.clone(), None)
            .await,
        Err(RunAdmissionError::ParentNotCurrent { .. })
    ));
    request.expected_parent_generation = 1;
    let existing_child_id = child.run_id.clone();
    fixture.store.insert_run(child.clone()).await.unwrap();
    assert!(matches!(
        fixture
            .store
            .insert_run_with_collaborator_stage(child, request, None)
            .await,
        Err(RunAdmissionError::AssociationConflict { .. })
    ));
    assert!(
        fixture
            .store
            .load_collaborator_association(
                &fixture.user_id,
                &fixture.session_id,
                &existing_child_id
            )
            .await
            .unwrap()
            .is_none()
    );
    fixture
        .store
        .request_run_cancellation(&fixture.user_id, &fixture.parent_id)
        .await
        .unwrap();
    let child = fixture.child();
    let request = admission(&child.run_id);
    assert!(matches!(
        fixture
            .store
            .insert_run_with_collaborator_stage(child.clone(), request, None)
            .await,
        Err(RunAdmissionError::ParentNotCurrent { .. })
    ));
    assert!(
        fixture
            .store
            .load_run(&fixture.user_id, &child.run_id)
            .await
            .unwrap()
            .is_none()
    );
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn failed_child_insert_rolls_back_already_staged_anchor_receipt() {
    let fixture = Fixture::new().await;
    let (mut request, first) = fixture.first().await;
    fixture.bind_native_session(&first.run_id).await;
    fixture.settle(&first.run_id).await;
    let candidate = fixture.child();
    fixture.store.insert_run(candidate.clone()).await.unwrap();
    request.expected_previous_stage_run_id = Some(first.run_id.clone());
    request.source_message_id = Uuid::new_v4().to_string();
    let before = fixture
        .store
        .load_run(&fixture.user_id, &first.run_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        fixture
            .store
            .insert_run_with_collaborator_stage(candidate, request, None)
            .await
            .is_err()
    );
    let after = fixture
        .store
        .load_run(&fixture.user_id, &first.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.last_event_idx, before.last_event_idx);
    let association = fixture
        .store
        .load_collaborator_association(&fixture.user_id, &fixture.session_id, &first.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(association.latest_stage.run_id, first.run_id);
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn association_snapshots_do_not_wait_for_session_write_admission_locks() {
    let fixture = Fixture::new().await;
    let (_, stage) = fixture.first().await;
    let ordinary = fixture.child();
    fixture.store.insert_run(ordinary.clone()).await.unwrap();
    let association = fixture
        .store
        .load_collaborator_association_for_stage(
            &fixture.user_id,
            &fixture.session_id,
            &stage.run_id,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(association.latest_stage.run_id, stage.run_id);
    let mut writer = fixture.pool.get().begin().await.unwrap();
    sqlx::query(
        "SELECT status FROM agent_sessions WHERE user_id = ? AND session_id = ? FOR UPDATE",
    )
    .bind(&fixture.user_id)
    .bind(&fixture.session_id)
    .fetch_all(&mut *writer)
    .await
    .unwrap();
    for target in [&stage.run_id, &ordinary.run_id, &Uuid::new_v4().to_string()] {
        let snapshot = tokio::time::timeout(
            Duration::from_secs(2),
            fixture.store.load_collaborator_association_for_stage(
                &fixture.user_id,
                &fixture.session_id,
                target,
            ),
        )
        .await
        .expect("routing read must finish while the session write lock is still held")
        .unwrap();
        assert_eq!(snapshot.is_some(), target == &stage.run_id);
    }
    assert!(
        tokio::time::timeout(
            Duration::from_secs(2),
            fixture.store.load_collaborator_association(
                &fixture.user_id,
                &fixture.session_id,
                &stage.run_id
            )
        )
        .await
        .expect("anchor snapshot must not acquire write locks")
        .unwrap()
        .is_some()
    );
    writer.rollback().await.unwrap();
    assert!(matches!(
        fixture
            .store
            .load_collaborator_association_for_stage(
                "other-user",
                &fixture.session_id,
                &stage.run_id
            )
            .await,
        Err(RunAdmissionError::SessionUnavailable)
    ));
    assert_eq!(
        fixture
            .store
            .load_run(&fixture.user_id, &ordinary.run_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "running"
    );
    sqlx::query("UPDATE agent_session_lifecycle_fences SET delete_requested_at = NOW(6) WHERE user_id = ? AND session_id = ?")
        .bind(&fixture.user_id).bind(&fixture.session_id).execute(fixture.pool.get()).await.unwrap();
    for for_stage in [false, true] {
        let result = if for_stage {
            fixture
                .store
                .load_collaborator_association_for_stage(
                    &fixture.user_id,
                    &fixture.session_id,
                    &stage.run_id,
                )
                .await
        } else {
            fixture
                .store
                .load_collaborator_association(&fixture.user_id, &fixture.session_id, &stage.run_id)
                .await
        };
        assert!(matches!(result, Err(RunAdmissionError::SessionUnavailable)));
    }
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn expired_budget_is_typed_and_external_native_binding_is_required() {
    let fixture = Fixture::new().await;
    let child = fixture.child();
    let request = admission(&child.run_id);
    assert!(matches!(
        fixture
            .store
            .insert_run_with_collaborator_stage(
                child.clone(),
                request,
                Some(tokio::time::Instant::now()),
            )
            .await,
        Err(RunAdmissionError::DeadlineExpired)
    ));
    assert!(
        fixture
            .store
            .load_run(&fixture.user_id, &child.run_id)
            .await
            .unwrap()
            .is_none()
    );
    let (mut request, first) = fixture.first().await;
    fixture.settle(&first.run_id).await;
    request.expected_previous_stage_run_id = Some(first.run_id);
    request.source_message_id = Uuid::new_v4().to_string();
    assert!(matches!(
        fixture
            .store
            .insert_run_with_collaborator_stage(fixture.child(), request, None)
            .await,
        Err(RunAdmissionError::NativeSessionUnbound { .. })
    ));
    fixture.cleanup().await;
}
