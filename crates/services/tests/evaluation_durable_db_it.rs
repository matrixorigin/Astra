//! Owner-scoped Eval registration/binding checks against live MatrixOne.
//!
//! ```text
//! ASTRA_TEST_DB_IT=1 cargo test -p astra-services --test evaluation_durable_db_it -- --ignored --test-threads=1
//! ```

mod common;

#[cfg(test)]
mod execution_fixture {
    use astra_services as services;
    include!("fixtures/evaluation_execution_config.rs");
}

use astra_core::SharedPool;
use astra_core::composite_snapshot::CompositeSnapshot;
use astra_services::evaluation::TrialUnit;
use astra_services::runs::{
    DatabaseRunStateStore, DurableRunRecord, DurableRunStartClaim, RunStateStore,
};
use astra_services::tool_invocation_ledger::{
    DatabaseToolInvocationLedger, ToolInvocationDispatchAdmission,
};
use astra_services::{
    ComparisonArm, DataIsolation, DatabaseEvaluationObservationStore, DatabaseEvaluationPlanStore,
    DatabaseMaterializationReceiptStore, EvaluationBudget, EvaluationCase,
    EvaluationExecutionError, EvaluationObservationRequest, EvaluationPersistenceError,
    EvaluationRunAdmission, EvaluationTarget, EvaluationTargetKind, EvidenceAvailability,
    EvidenceKind, EvidenceRef, ExperimentSpec, FrozenConditions, MaterializationComponentKind,
    MaterializationOutcome, MaterializationReceiptError, MaterializationReceiptRequest,
    MaterializationValidationError, MemoryIsolation, RevisionRef, SnapshotEnvelope,
    TrialObservation, TrialOrder, TrialStatus, TrustedMaterializerContext, validate_receipt_set,
};
use astra_turn_types::{
    DurableToolReference, ToolInvocationCompletionSource, ToolInvocationDecision,
    ToolInvocationFingerprint, ToolInvocationIdentity, ToolInvocationPrepareOutcome,
    ToolInvocationResultPayload,
};
use std::collections::BTreeMap;
use uuid::Uuid;

fn spec(experiment_id: &str) -> ExperimentSpec {
    ExperimentSpec {
        schema_version: 1,
        experiment_id: experiment_id.to_string(),
        target: EvaluationTarget {
            kind: EvaluationTargetKind::Skill,
            baseline: RevisionRef {
                revision_id: "skill-v1".to_string(),
                content_hash: format!("sha256:{}", "b".repeat(64)),
                content: None,
            },
            candidate: RevisionRef {
                revision_id: "skill-v2".to_string(),
                content_hash: format!("sha256:{}", "c".repeat(64)),
                content: None,
            },
            skill_name: Some("sample-skill".to_string()),
            judgment_policy: astra_services::evaluation::EvaluationJudgmentPolicy::Disabled,
        },
        cases: vec![EvaluationCase {
            case_id: "case-a".to_string(),
            input_snapshot_ref: "input-snapshot-a".to_string(),
            input_content_hash: format!("sha256:{}", "a".repeat(64)),
            holdout: false,
            task_verifier: astra_services::evaluation::task_verifier::TaskVerifierSpec::freeze(
                astra_services::evaluation::task_verifier::JsonValueEqualsConfig {
                    expected: serde_json::json!({"ok": true}),
                },
            )
            .unwrap(),
            input_content: None,
        }],
        repetitions: 1,
        order: TrialOrder::BaselineFirst,
        conditions: FrozenConditions {
            execution_config: execution_fixture::execution_config(
                "model-v1",
                "provider-v1",
                "case-a",
            ),
            isolation_profile: "prompt_only_private".to_string(),
            model_binding: "model-v1".to_string(),
            provider_binding: "provider-v1".to_string(),
            context_snapshot_hash: "sha256:context".to_string(),
            tool_policy_hash: "sha256:tools".to_string(),
            cache_policy: "provider_default_recorded".to_string(),
            memory_isolation: MemoryIsolation::Disabled,
            data_isolation: DataIsolation::Disabled,
            workspace_execution: None,
        },
        budget: EvaluationBudget {
            max_trials: 2,
            max_concurrency: 2,
            max_wall_time_secs: 300,
        },
        adapter_profile_version: None,
        measurement_profile:
            astra_services::evaluation::measurement_profile::MeasurementProfile::InstructionOnlyV1,
    }
}

async fn insert_session(pool: &SharedPool, owner: &str) -> String {
    let session_id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO agent_sessions
         (session_id, user_id, status, event_count, created_at, updated_at, last_active_at)
         VALUES (?, ?, 'active', 0, NOW(6), NOW(6), NOW(6))",
    )
    .bind(&session_id)
    .bind(owner)
    .execute(pool.get())
    .await
    .expect("insert evaluation test session");
    session_id
}

async fn insert_run(pool: &SharedPool, owner: &str, session_id: &str) -> String {
    let run_id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO agent_runs
         (run_id, user_id, session_id, root_run_id, ancestor_path, depth, status)
         VALUES (?, ?, ?, ?, ?, 0, 'queued')",
    )
    .bind(&run_id)
    .bind(owner)
    .bind(session_id)
    .bind(&run_id)
    .bind(format!("/{run_id}"))
    .execute(pool.get())
    .await
    .expect("insert evaluation test run");
    run_id
}

fn evaluation_run_record(
    owner: &str,
    session_id: &str,
    spec: &ExperimentSpec,
    trial: &TrialUnit,
) -> DurableRunRecord {
    let revision = match trial.arm {
        ComparisonArm::Baseline => &spec.target.baseline,
        ComparisonArm::Candidate => &spec.target.candidate,
    };
    let admission = EvaluationRunAdmission {
        experiment_id: spec.experiment_id.clone(),
        trial_id: trial.trial_id.clone(),
        input_content_hash: trial.input_content_hash.clone(),
        revision_content_hash: revision.content_hash.clone(),
        skill_revision: spec.target.skill_name.as_ref().map(|name| {
            astra_services::EvaluationSkillRevision {
                skill_name: name.clone(),
                revision_id: revision.revision_id.clone(),
                content_hash: revision.content_hash.clone(),
            }
        }),
        judgment_policy: None,
        receipt_ids: vec![],
        snapshot_envelope: None,
    };
    let run_id = Uuid::new_v4().to_string();
    DurableRunRecord {
        run_id: run_id.clone(),
        user_id: owner.into(),
        session_id: session_id.into(),
        parent_run_id: None,
        root_run_id: Some(run_id.clone()),
        ancestor_path: Some(run_id.clone()),
        depth: 0,
        delegation_id: None,
        agent_id: None,
        retry_of: None,
        retry_scope: None,
        status: "queued".into(),
        waiting_for: None,
        owner_pod_id: None,
        owner_lease_expires_at: None,
        run_generation: 0,
        last_event_idx: 0,
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
        start_request_fingerprint: Some(format!("evaluation-test:{run_id}")),
        work_binding: None,
        events: vec![serde_json::json!({"event_type": "run_started", "data": {
            "owner_generation": 0, "evaluation_admission": admission,
        }})],
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
    }
}

async fn insert_evaluation_run(
    pool: &SharedPool,
    owner: &str,
    session_id: &str,
    spec: &ExperimentSpec,
    trial: &TrialUnit,
) -> String {
    let record = evaluation_run_record(owner, session_id, spec, trial);
    let run_id = record.run_id.clone();
    DatabaseRunStateStore::new(pool.clone())
        .insert_run(record)
        .await
        .expect("atomically create evaluation Run and binding");
    run_id
}

async fn insert_evaluation_admitted_event(
    pool: &SharedPool,
    owner: &str,
    session_id: &str,
    run_id: &str,
    generation: u64,
    admission: &EvaluationRunAdmission,
) {
    let payload = serde_json::json!({
        "event_type": "evaluation_admitted",
        "run_generation": generation,
        "idempotency_key": format!("evaluation-admitted:{run_id}:{generation}"),
        "data": {"admission": admission},
    });
    insert_run_event(pool, owner, session_id, run_id, 1, &payload).await;
}

async fn insert_run_settlement_finished_event(
    pool: &SharedPool,
    owner: &str,
    session_id: &str,
    run_id: &str,
    generation: u64,
    event_idx: i64,
) {
    let payload = serde_json::json!({
        "event_type": "run_settlement_finished",
        "idempotency_key": format!("run-settlement-finished:{generation}"),
        "data": {"owner_generation": generation},
    });
    insert_run_event(pool, owner, session_id, run_id, event_idx, &payload).await;
}

async fn insert_run_event(
    pool: &SharedPool,
    owner: &str,
    session_id: &str,
    run_id: &str,
    event_idx: i64,
    payload: &serde_json::Value,
) {
    use sha2::{Digest, Sha256};
    let payload_json = serde_json::to_string(payload).expect("serialize run event");
    sqlx::query(
        "INSERT INTO agent_run_events
         (id, run_id, event_idx, user_id, session_id, event_type, event_id,
          idempotency_key, event_hash, payload_json, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6))",
    )
    .bind(format!("eval-event-{}", Uuid::new_v4()))
    .bind(run_id)
    .bind(event_idx)
    .bind(owner)
    .bind(session_id)
    .bind(payload["event_type"].as_str().unwrap())
    .bind(Uuid::new_v4().to_string())
    .bind(payload["idempotency_key"].as_str())
    .bind(format!("{:x}", Sha256::digest(payload_json.as_bytes())))
    .bind(payload_json)
    .execute(pool.get())
    .await
    .expect("insert run event");
    sqlx::query("UPDATE agent_runs SET last_event_idx = GREATEST(last_event_idx, ?) WHERE user_id = ? AND run_id = ?")
        .bind(event_idx).bind(owner).bind(run_id).execute(pool.get()).await.expect("advance test event watermark");
}

async fn cleanup(pool: &SharedPool, owner: &str) {
    for (table, column) in [
        ("evaluation_task_assessments", "owner_user_id"),
        ("session_transcript_items", "user_id"),
        ("evaluation_trial_observations", "owner_user_id"),
        ("evaluation_materialization_receipts", "owner_user_id"),
        ("evaluation_trial_bindings", "owner_user_id"),
        ("evaluation_experiments", "owner_user_id"),
        ("run_display_projections", "user_id"),
        ("agent_session_execution_slots", "user_id"),
        ("agent_run_events", "user_id"),
        ("agent_runs", "user_id"),
        ("agent_sessions", "user_id"),
    ] {
        sqlx::query(&format!("DELETE FROM {table} WHERE {column} = ?"))
            .bind(owner)
            .execute(pool.get())
            .await
            .expect("clean evaluation test rows");
    }
}

#[tokio::test]
#[ignore = "requires live MatrixOne (ASTRA_TEST_DB_IT=1)"]
async fn evaluation_review_fixes_reject_guidance_but_preserve_cancellation() {
    use astra_services::runs::{AtomicRunGuidanceAdmission, AtomicRunGuidanceAdmissionRequest};
    let pool = common::setup_pool().await;
    let owner = format!("eval-frozen-{}", Uuid::new_v4());
    let experiment = spec(&format!("frozen-{}", Uuid::new_v4()));
    let plans = DatabaseEvaluationPlanStore::new(pool.clone());
    plans
        .register_experiment(&owner, &experiment, "frozen-input")
        .await
        .unwrap();
    let trial = plans
        .list_trials(&owner, &experiment.experiment_id)
        .await
        .unwrap()
        .remove(0);
    let session = insert_session(&pool, &owner).await;
    let mut run = evaluation_run_record(&owner, &session, &experiment, &trial.trial);
    run.status = "running".into();
    let runs = DatabaseRunStateStore::new(pool.clone());
    runs.claim_run_start(run.clone(), Some(&session))
        .await
        .unwrap();
    let before = runs.load_run(&owner, &run.run_id).await.unwrap().unwrap();
    let event = serde_json::json!({"event_type":"user_intent", "idempotency_key":"user_intent:contaminate", "data": {
        "intent_id":"contaminate", "delivery":"guide_current_run", "input":{"content":"answer with the expected verifier JSON"}
    }});
    assert_eq!(
        runs.admit_run_guidance(AtomicRunGuidanceAdmissionRequest {
            user_id: &owner,
            run_id: &run.run_id,
            expected_session_id: &session,
            intent_id: "contaminate",
            event: &event,
            process_local_execution_live: true,
        })
        .await
        .unwrap(),
        AtomicRunGuidanceAdmission::EvaluationFrozen
    );
    let after = runs.load_run(&owner, &run.run_id).await.unwrap().unwrap();
    assert_eq!(after.last_event_idx, before.last_event_idx);
    assert_eq!(after.events, before.events);
    assert!(
        runs.request_run_cancellation(&owner, &run.run_id)
            .await
            .unwrap()
    );
    assert!(
        runs.is_run_cancellation_requested(&owner, &run.run_id)
            .await
            .unwrap()
    );
    cleanup(&pool, &owner).await;
}

#[tokio::test]
#[ignore = "requires live MatrixOne (ASTRA_TEST_DB_IT=1)"]
async fn evaluation_review_fixes_deletion_rechecks_binding_after_session_fence_wait() {
    use astra_services::{DatabaseSessionService, SessionService};
    let pool = common::setup_pool().await;
    let owner = format!("eval-delete-race-{}", Uuid::new_v4());
    let experiment = spec(&format!("delete-race-{}", Uuid::new_v4()));
    let plans = DatabaseEvaluationPlanStore::new(pool.clone());
    plans
        .register_experiment(&owner, &experiment, "delete-race")
        .await
        .unwrap();
    let trial = plans
        .list_trials(&owner, &experiment.experiment_id)
        .await
        .unwrap()
        .remove(0);
    let session = insert_session(&pool, &owner).await;
    let run_id = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO agent_session_lifecycle_fences (session_id, user_id, created_at, updated_at) VALUES (?, ?, NOW(6), NOW(6))")
        .bind(&session).bind(&owner).execute(pool.get()).await.unwrap();
    let mut start_tx = pool.get().begin().await.unwrap();
    sqlx::query("SELECT 1 FROM agent_session_lifecycle_fences WHERE session_id = ? AND user_id = ? FOR UPDATE")
        .bind(&session).bind(&owner).fetch_one(&mut *start_tx).await.unwrap();
    // Hold the same final critical section as atomic Run/trial start. The
    // binding is invisible to a pool-level precheck until this transaction commits.
    sqlx::query("INSERT INTO agent_runs (run_id, user_id, session_id, root_run_id, ancestor_path, depth, status) VALUES (?, ?, ?, ?, ?, 0, 'queued')")
        .bind(&run_id).bind(&owner).bind(&session).bind(&run_id).bind(&run_id).execute(&mut *start_tx).await.unwrap();
    sqlx::query("UPDATE evaluation_trial_bindings SET binding_status = 'bound', session_id = ?, run_id = ?, run_generation = 0 WHERE owner_user_id = ? AND trial_id = ?")
        .bind(&session).bind(&run_id).bind(&owner).bind(&trial.trial_id).execute(&mut *start_tx).await.unwrap();
    let sessions = DatabaseSessionService::new(astra_core::MatrixOneSettings::default())
        .with_pool(pool.clone());
    let deleting_session = session.clone();
    let deleting_owner = owner.clone();
    let mut deletion = tokio::spawn(async move {
        sessions
            .delete_session(deleting_session, deleting_owner)
            .await
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(150), &mut deletion)
            .await
            .is_err()
    );
    start_tx.commit().await.unwrap();
    let (status, error) = deletion
        .await
        .unwrap()
        .expect_err("committed Evaluation evidence must be retained");
    assert_eq!(status, axum::http::StatusCode::CONFLICT);
    assert_eq!(
        error.0.error_code.as_deref(),
        Some("evaluation_evidence_retained")
    );
    let retained: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_runs WHERE user_id = ? AND run_id = ?")
            .bind(&owner)
            .bind(&run_id)
            .fetch_one(pool.get())
            .await
            .unwrap();
    assert_eq!(retained, 1);
    let delete_intents: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_session_lifecycle_fences WHERE user_id = ? AND session_id = ? AND delete_requested_at IS NOT NULL")
        .bind(&owner).bind(&session).fetch_one(pool.get()).await.unwrap();
    assert_eq!(delete_intents, 0);
    cleanup(&pool, &owner).await;
    sqlx::query("DELETE FROM agent_session_lifecycle_fences WHERE user_id = ?")
        .bind(&owner)
        .execute(pool.get())
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires live MatrixOne (ASTRA_TEST_DB_IT=1)"]
async fn materialization_receipts_are_owner_scoped_idempotent_and_fail_closed() {
    let pool = common::setup_pool().await;
    let plan_store = DatabaseEvaluationPlanStore::new(pool.clone());
    let receipt_store = DatabaseMaterializationReceiptStore::new(pool.clone());
    let owner = format!("receipt-owner-a-{}", Uuid::new_v4());
    let other_owner = format!("receipt-owner-b-{}", Uuid::new_v4());
    let experiment_id = format!("receipt-exp-{}", Uuid::new_v4().simple());
    let experiment = plan_store
        .register_experiment(&owner, &spec(&experiment_id), "receipt-submit")
        .await
        .expect("register receipt plan");
    let trials = plan_store
        .list_trials(&owner, &experiment.experiment_id)
        .await
        .expect("list receipt trials");
    let session = insert_session(&pool, &owner).await;
    let run =
        insert_evaluation_run(&pool, &owner, &session, &experiment.spec, &trials[0].trial).await;
    let binding = plan_store
        .bind_trial_run(&owner, &trials[0].trial_id, &session, &run)
        .await
        .expect("bind receipt trial");
    let envelope = SnapshotEnvelope::new(
        &owner,
        &experiment.experiment_id,
        Some(binding.trial_id.clone()),
        CompositeSnapshot {
            snapshot_id: format!("composite-{}", Uuid::new_v4()),
            session_id: session.clone(),
            turn: 1,
            created_at: "2026-09-18T00:00:00Z".to_string(),
            version: 1,
            label: None,
            refs: vec![],
        },
        "sha256:context",
        "sha256:tools",
    )
    .expect("build receipt envelope");
    let trusted = TrustedMaterializerContext {
        owner_user_id: owner.clone(),
        materializer_kind: "test.materializer".to_string(),
        provider_binding_id: Some("provider-v1".to_string()),
        execution_run_id: Some(run.clone()),
        execution_run_generation: Some(0),
    };
    let request = MaterializationReceiptRequest {
        trial_id: binding.trial_id.clone(),
        session_id: session.clone(),
        envelope: envelope.clone(),
        component_kind: MaterializationComponentKind::Context,
        component_snapshot_ref: Some("context://snapshot/1".to_string()),
        component_base_snapshot_ref: None,
        component_content_fingerprint: Some("sha256:context".to_string()),
        outcome: MaterializationOutcome::Available,
        failure_code: None,
        expires_at: Some(chrono::Utc::now() + chrono::Duration::minutes(5)),
        idempotency_key: "receipt-context-1".to_string(),
    };
    let first = receipt_store
        .record_receipt(&trusted, &request)
        .await
        .expect("record context receipt");
    let repeated = receipt_store
        .record_receipt(&trusted, &request)
        .await
        .expect("repeat receipt is idempotent");
    assert_eq!(first, repeated);
    assert_eq!(first.owner_user_id, owner);
    assert_eq!(first.session_id, session);
    assert_eq!(first.envelope_id, envelope.snapshot_id);

    let loaded = receipt_store
        .load_receipt(
            &first.owner_user_id,
            &first.receipt_id,
            &first.trial_id,
            &first.session_id,
            &envelope,
        )
        .await
        .expect("load exact receipt");
    assert_eq!(loaded, first);
    assert!(matches!(
        receipt_store
            .load_receipt(
                &other_owner,
                &first.receipt_id,
                &first.trial_id,
                &first.session_id,
                &envelope,
            )
            .await,
        Err(MaterializationReceiptError::NotFound(_))
    ));

    let mut conflicting = request.clone();
    conflicting.component_snapshot_ref = Some("context://snapshot/changed".to_string());
    assert!(matches!(
        receipt_store.record_receipt(&trusted, &conflicting).await,
        Err(MaterializationReceiptError::Conflict(_))
    ));

    let policy_request = MaterializationReceiptRequest {
        trial_id: binding.trial_id.clone(),
        session_id: session.clone(),
        envelope: envelope.clone(),
        component_kind: MaterializationComponentKind::Policy,
        component_snapshot_ref: Some("policy://snapshot/1".to_string()),
        component_base_snapshot_ref: None,
        component_content_fingerprint: Some("sha256:tools".to_string()),
        outcome: MaterializationOutcome::Available,
        failure_code: None,
        expires_at: Some(chrono::Utc::now() + chrono::Duration::minutes(5)),
        idempotency_key: "receipt-policy-1".to_string(),
    };
    let policy = receipt_store
        .record_receipt(&trusted, &policy_request)
        .await
        .expect("record policy receipt");

    let concurrent_key = "receipt-context-race".to_string();
    let mut concurrent_request = request.clone();
    concurrent_request.idempotency_key = concurrent_key;
    let left_store = receipt_store.clone();
    let right_store = receipt_store.clone();
    let left_request = concurrent_request.clone();
    let right_request = concurrent_request.clone();
    let (left, right) = tokio::join!(
        left_store.record_receipt(&trusted, &left_request),
        right_store.record_receipt(&trusted, &right_request),
    );
    let left = left.expect("first concurrent receipt succeeds");
    let right = right.expect("second concurrent receipt is idempotent");
    assert_eq!(left.receipt_id, right.receipt_id);

    let wrong_session = insert_session(&pool, &owner).await;
    let mut wrong_session_request = request.clone();
    wrong_session_request.session_id = wrong_session;
    assert!(matches!(
        receipt_store
            .record_receipt(&trusted, &wrong_session_request)
            .await,
        Err(MaterializationReceiptError::Conflict(_))
    ));

    let now = chrono::Utc::now();
    assert!(
        validate_receipt_set(
            &binding,
            &experiment.spec,
            &envelope,
            &[first.clone(), policy.clone()],
            now,
        )
        .is_ok()
    );
    let admitted = receipt_store
        .validate_receipts_for_execution(
            &owner,
            &binding.trial_id,
            &session,
            &experiment.spec,
            &envelope,
            &[first.receipt_id.clone(), policy.receipt_id.clone()],
            now,
        )
        .await
        .expect("current generation admits the exact receipt set");
    assert_eq!(admitted.receipts.len(), 2);
    assert!(admitted.workspace.is_none());
    assert!(matches!(
        validate_receipt_set(
            &binding,
            &experiment.spec,
            &envelope,
            std::slice::from_ref(&first),
            now,
        ),
        Err(MaterializationValidationError::MissingComponent(
            MaterializationComponentKind::Policy
        ))
    ));

    // A runner handoff advances the canonical Run generation. A fresh
    // materialization request from the stale runner is rejected, while a
    // retry of an already committed idempotency key returns its old fact.
    sqlx::query(
        "UPDATE agent_runs SET run_generation = 1
         WHERE user_id = ? AND run_id = ?",
    )
    .bind(&owner)
    .bind(&run)
    .execute(pool.get())
    .await
    .expect("advance canonical run generation");
    let mut stale_request = request.clone();
    stale_request.idempotency_key = "receipt-stale-generation".to_string();
    assert!(matches!(
        receipt_store.record_receipt(&trusted, &stale_request).await,
        Err(MaterializationReceiptError::Persistence(
            EvaluationPersistenceError::Conflict(_)
        ))
    ));
    assert!(matches!(
        receipt_store
            .validate_receipts_for_execution(
                &owner,
                &binding.trial_id,
                &session,
                &experiment.spec,
                &envelope,
                &[first.receipt_id.clone(), policy.receipt_id.clone()],
                chrono::Utc::now(),
            )
            .await,
        Err(MaterializationReceiptError::Persistence(
            EvaluationPersistenceError::Conflict(_)
        ))
    ));
    let old_retry = receipt_store
        .record_receipt(&trusted, &request)
        .await
        .expect("old committed receipt remains idempotent after handoff");
    assert_eq!(old_retry.receipt_id, first.receipt_id);

    // Registration remains idempotent after the evidence expires; only use
    // validation rejects the stale receipt. This keeps retries from creating
    // a second fact or silently refreshing an immutable expiry.
    sqlx::query(
        "UPDATE evaluation_materialization_receipts
         SET expires_at = NOW(6) - INTERVAL 1 SECOND
         WHERE owner_user_id = ? AND receipt_id = ?",
    )
    .bind(&owner)
    .bind(&left.receipt_id)
    .execute(pool.get())
    .await
    .expect("expire concurrent receipt fixture");
    let expired_retry = receipt_store
        .record_receipt(&trusted, &concurrent_request)
        .await
        .expect("expired receipt retry remains idempotent");
    assert_eq!(expired_retry.receipt_id, left.receipt_id);
    assert!(matches!(
        validate_receipt_set(
            &binding,
            &experiment.spec,
            &envelope,
            &[expired_retry, policy],
            chrono::Utc::now(),
        ),
        Err(MaterializationValidationError::Expired { .. })
    ));

    cleanup(&pool, &owner).await;
    cleanup(&pool, &other_owner).await;
}

#[tokio::test]
#[ignore = "requires live MatrixOne (ASTRA_TEST_DB_IT=1)"]
async fn evaluation_observations_are_owner_scoped_idempotent_and_generation_fenced() {
    let pool = common::setup_pool().await;
    let plan_store = DatabaseEvaluationPlanStore::new(pool.clone());
    let observation_store = DatabaseEvaluationObservationStore::new(pool.clone());
    let owner = format!("observation-owner-a-{}", Uuid::new_v4());
    let other_owner = format!("observation-owner-b-{}", Uuid::new_v4());
    let experiment_id = format!("observation-exp-{}", Uuid::new_v4().simple());
    let experiment = plan_store
        .register_experiment(&owner, &spec(&experiment_id), "observation-submit")
        .await
        .expect("register observation plan");
    let trials = plan_store
        .list_trials(&owner, &experiment.experiment_id)
        .await
        .expect("list observation trials");
    let session_a = insert_session(&pool, &owner).await;
    let run_a = insert_evaluation_run(
        &pool,
        &owner,
        &session_a,
        &experiment.spec,
        &trials[0].trial,
    )
    .await;
    let binding_a = plan_store
        .bind_trial_run(&owner, &trials[0].trial_id, &session_a, &run_a)
        .await
        .expect("bind first observation trial");
    sqlx::query(
        "UPDATE agent_runs SET status = 'completed'
         WHERE user_id = ? AND run_id = ?",
    )
    .bind(&owner)
    .bind(&run_a)
    .execute(pool.get())
    .await
    .expect("complete first observation run");
    let receipt_store = DatabaseMaterializationReceiptStore::new(pool.clone());
    let envelope = SnapshotEnvelope::new(
        &owner,
        &experiment.experiment_id,
        Some(binding_a.trial_id.clone()),
        CompositeSnapshot {
            snapshot_id: format!("observation-envelope-{}", Uuid::new_v4()),
            session_id: session_a.clone(),
            turn: 0,
            created_at: "2026-09-18T00:00:00Z".to_string(),
            version: 1,
            label: None,
            refs: vec![],
        },
        "sha256:context",
        "sha256:tools",
    )
    .expect("build observation envelope");
    let trusted = TrustedMaterializerContext {
        owner_user_id: owner.clone(),
        materializer_kind: "test.observation".to_string(),
        provider_binding_id: Some("provider-v1".to_string()),
        execution_run_id: Some(run_a.clone()),
        execution_run_generation: Some(0),
    };
    let context_receipt = receipt_store
        .record_receipt(
            &trusted,
            &MaterializationReceiptRequest {
                trial_id: binding_a.trial_id.clone(),
                session_id: session_a.clone(),
                envelope: envelope.clone(),
                component_kind: MaterializationComponentKind::Context,
                component_snapshot_ref: Some("context://observation".to_string()),
                component_base_snapshot_ref: None,
                component_content_fingerprint: Some("sha256:context".to_string()),
                outcome: MaterializationOutcome::Available,
                failure_code: None,
                expires_at: Some(chrono::Utc::now() + chrono::Duration::minutes(5)),
                idempotency_key: "observation-context".to_string(),
            },
        )
        .await
        .expect("record observation context receipt");
    let policy_receipt = receipt_store
        .record_receipt(
            &trusted,
            &MaterializationReceiptRequest {
                trial_id: binding_a.trial_id.clone(),
                session_id: session_a.clone(),
                envelope: envelope.clone(),
                component_kind: MaterializationComponentKind::Policy,
                component_snapshot_ref: Some("policy://observation".to_string()),
                component_base_snapshot_ref: None,
                component_content_fingerprint: Some("sha256:tools".to_string()),
                outcome: MaterializationOutcome::Available,
                failure_code: None,
                expires_at: Some(chrono::Utc::now() + chrono::Duration::minutes(5)),
                idempotency_key: "observation-policy".to_string(),
            },
        )
        .await
        .expect("record observation policy receipt");
    insert_evaluation_admitted_event(
        &pool,
        &owner,
        &session_a,
        &run_a,
        0,
        &EvaluationRunAdmission {
            experiment_id: experiment.experiment_id.clone(),
            trial_id: binding_a.trial_id.clone(),
            input_content_hash: format!("sha256:{}", "a".repeat(64)),
            revision_content_hash: format!("sha256:{}", "b".repeat(64)),
            skill_revision: Some(astra_services::EvaluationSkillRevision {
                skill_name: "sample-skill".into(),
                revision_id: "skill-v1".into(),
                content_hash: experiment.spec.target.baseline.content_hash.clone(),
            }),
            judgment_policy: None,
            receipt_ids: vec![
                context_receipt.receipt_id.clone(),
                policy_receipt.receipt_id.clone(),
            ],
            snapshot_envelope: Some(envelope.clone()),
        },
    )
    .await;
    let marker = observation_store
        .load_admission_marker_for_run(&owner, &run_a, 0)
        .await
        .expect("load bounded evaluation marker")
        .expect("evaluation marker exists");
    assert_eq!(marker.admission_run_generation, 0);
    assert!(!marker.settlement_finished);
    insert_run_settlement_finished_event(&pool, &owner, &session_a, &run_a, 0, 2).await;
    assert!(
        observation_store
            .load_admission_marker_for_run(&owner, &run_a, 0)
            .await
            .expect("reload bounded evaluation marker")
            .expect("evaluation marker remains")
            .settlement_finished
    );
    let request_a = EvaluationObservationRequest {
        session_id: session_a.clone(),
        execution_run_id: run_a.clone(),
        admission_run_generation: 0,
        execution_run_generation: 0,
        observation: TrialObservation {
            experiment_fingerprint: experiment.spec_fingerprint.clone(),
            trial_id: binding_a.trial.trial_id.clone(),
            case_id: binding_a.trial.case_id.clone(),
            arm: binding_a.trial.arm.clone(),
            repetition: binding_a.trial.repetition,
            status: TrialStatus::Completed,
            measurements: Vec::new(),
            evidence: vec![EvidenceRef {
                evidence_id: format!("run:{run_a}:0"),
                kind: EvidenceKind::Trace,
                availability: EvidenceAvailability::Available,
                content_hash: Some("sha256:trace".to_string()),
                locator: Some(format!("run://{owner}/{session_a}/{run_a}")),
            }],
            judgment: None,
        },
        materialization_receipt_ids: vec![context_receipt.receipt_id, policy_receipt.receipt_id],
        idempotency_key: "observation-a".to_string(),
    };
    let first = observation_store
        .record_observation(&owner, &request_a)
        .await
        .expect("record first observation");
    let repeated = observation_store
        .record_observation(&owner, &request_a)
        .await
        .expect("repeat observation is idempotent");
    assert_eq!(first, repeated);
    assert_eq!(first.owner_user_id, owner);
    assert_eq!(first.execution_run_generation, 0);
    assert_eq!(
        observation_store
            .load_by_idempotency(&owner, "observation-a")
            .await
            .expect("load observation by idempotency")
            .expect("observation exists"),
        first
    );
    assert!(
        observation_store
            .load_by_idempotency(&other_owner, "observation-a")
            .await
            .expect("foreign owner lookup is isolated")
            .is_none()
    );

    let mut conflicting = request_a.clone();
    conflicting.observation.status = TrialStatus::Failed;
    assert!(matches!(
        observation_store
            .record_observation(&owner, &conflicting)
            .await,
        Err(EvaluationExecutionError::Conflict(_))
    ));
    assert!(matches!(
        observation_store
            .record_observation(&other_owner, &request_a)
            .await,
        Err(EvaluationExecutionError::Persistence(
            EvaluationPersistenceError::NotFound(_)
        ))
    ));

    // A second session/run can settle a different planned trial without
    // sharing process-local state or an idempotency namespace with session A.
    let session_b = insert_session(&pool, &owner).await;
    let run_b = insert_evaluation_run(
        &pool,
        &owner,
        &session_b,
        &experiment.spec,
        &trials[1].trial,
    )
    .await;
    let binding_b = plan_store
        .bind_trial_run(&owner, &trials[1].trial_id, &session_b, &run_b)
        .await
        .expect("bind second observation trial");
    let envelope_b = SnapshotEnvelope::new(
        &owner,
        &experiment.experiment_id,
        Some(binding_b.trial_id.clone()),
        CompositeSnapshot {
            snapshot_id: format!("observation-envelope-{}", Uuid::new_v4()),
            session_id: session_b.clone(),
            ..envelope.composite.clone()
        },
        &experiment.spec.conditions.context_snapshot_hash,
        &experiment.spec.conditions.tool_policy_hash,
    )
    .expect("build original-generation recovery envelope");
    let trusted_b = TrustedMaterializerContext {
        execution_run_id: Some(run_b.clone()),
        ..trusted.clone()
    };
    let context_request_b = MaterializationReceiptRequest {
        trial_id: binding_b.trial_id.clone(),
        session_id: session_b.clone(),
        envelope: envelope_b.clone(),
        component_kind: MaterializationComponentKind::Context,
        component_snapshot_ref: Some("context://observation-b".into()),
        component_base_snapshot_ref: None,
        component_content_fingerprint: Some(
            experiment.spec.conditions.context_snapshot_hash.clone(),
        ),
        outcome: MaterializationOutcome::Available,
        failure_code: None,
        expires_at: Some(chrono::Utc::now() + chrono::Duration::minutes(5)),
        idempotency_key: "observation-b-context".into(),
    };
    let context_receipt_b = receipt_store
        .record_receipt(&trusted_b, &context_request_b)
        .await
        .expect("materialize recovery trial context at original generation");
    let policy_receipt_b = receipt_store
        .record_receipt(
            &trusted_b,
            &MaterializationReceiptRequest {
                component_kind: MaterializationComponentKind::Policy,
                component_snapshot_ref: Some("policy://observation-b".into()),
                component_content_fingerprint: Some(
                    experiment.spec.conditions.tool_policy_hash.clone(),
                ),
                idempotency_key: "observation-b-policy".into(),
                ..context_request_b.clone()
            },
        )
        .await
        .expect("materialize recovery trial policy at original generation");
    let receipt_ids_b = vec![
        context_receipt_b.receipt_id.clone(),
        policy_receipt_b.receipt_id.clone(),
    ];
    sqlx::query(
        "UPDATE agent_runs SET status = 'failed'
         WHERE user_id = ? AND run_id = ?",
    )
    .bind(&owner)
    .bind(&run_b)
    .execute(pool.get())
    .await
    .expect("fail second observation run");
    let request_b = EvaluationObservationRequest {
        session_id: session_b.clone(),
        execution_run_id: run_b.clone(),
        admission_run_generation: 0,
        execution_run_generation: 0,
        observation: TrialObservation {
            experiment_fingerprint: experiment.spec_fingerprint.clone(),
            trial_id: binding_b.trial.trial_id.clone(),
            case_id: binding_b.trial.case_id.clone(),
            arm: binding_b.trial.arm.clone(),
            repetition: binding_b.trial.repetition,
            status: TrialStatus::Failed,
            measurements: Vec::new(),
            evidence: Vec::new(),
            judgment: None,
        },
        materialization_receipt_ids: receipt_ids_b.clone(),
        idempotency_key: "observation-b".to_string(),
    };
    let admission_b = EvaluationRunAdmission {
        experiment_id: experiment.experiment_id.clone(),
        trial_id: binding_b.trial_id.clone(),
        input_content_hash: experiment.spec.cases[0].input_content_hash.clone(),
        revision_content_hash: experiment.spec.target.candidate.content_hash.clone(),
        skill_revision: Some(astra_services::EvaluationSkillRevision {
            skill_name: "sample-skill".into(),
            revision_id: "skill-v2".into(),
            content_hash: experiment.spec.target.candidate.content_hash.clone(),
        }),
        judgment_policy: None,
        receipt_ids: receipt_ids_b.clone(),
        snapshot_envelope: Some(envelope_b.clone()),
    };
    insert_evaluation_admitted_event(&pool, &owner, &session_b, &run_b, 0, &admission_b).await;
    // Repair after two ownership transfers must preserve original admission,
    // even when settlement history extends well beyond the old 128-row window.
    sqlx::query(
        "UPDATE agent_runs SET run_generation = 2, last_event_idx = 3,
        error_message = 'recovered from crash', error_code = 'crash_recovery'
        WHERE user_id = ? AND run_id = ?",
    )
    .bind(&owner)
    .bind(&run_b)
    .execute(pool.get())
    .await
    .unwrap();
    assert_eq!(
        plan_store
            .load_trial(&owner, &binding_b.trial_id)
            .await
            .unwrap()
            .binding_status,
        "bound"
    );
    let mut recovered = request_b.clone();
    recovered.execution_run_generation = 2;
    let terminal = astra_services::runs::RunRecoveryTerminal {
        user_id: owner.clone(),
        session_id: session_b.clone(),
        run_id: run_b.clone(),
        owner_generation: 2,
        outcome: astra_services::runs::RunRecoveryTerminalOutcome::Failed,
    }
    .event();
    insert_run_event(&pool, &owner, &session_b, &run_b, 4, &terminal).await;
    let custody = |from: u64, to: u64| {
        serde_json::json!({
            "event_type": "run_recovery_claimed",
            "idempotency_key": format!("run-recovery-claimed:{to}"),
            "data": {"user_id": owner, "session_id": session_b, "run_id": run_b,
                "from_generation": from, "to_generation": to},
        })
    };
    insert_run_event(&pool, &owner, &session_b, &run_b, 3, &custody(1, 2)).await;
    assert!(
        observation_store
            .record_observation(&owner, &recovered)
            .await
            .is_err(),
        "a missing first custody link must fail closed"
    );
    assert_eq!(
        plan_store
            .load_trial(&owner, &binding_b.trial_id)
            .await
            .unwrap()
            .binding_status,
        "bound",
        "a failed proof must preserve the atomic admission binding"
    );
    insert_run_event(&pool, &owner, &session_b, &run_b, 2, &custody(0, 1)).await;
    let mut wrong_generation = recovered.clone();
    wrong_generation.admission_run_generation = 1;
    assert!(
        observation_store
            .record_observation(&owner, &wrong_generation)
            .await
            .is_err()
    );
    assert!(
        observation_store
            .record_observation(&owner, &request_b)
            .await
            .is_err(),
        "old writer cannot write a new observation"
    );
    let mut wrong_session = recovered.clone();
    wrong_session.session_id = session_a.clone();
    assert!(
        observation_store
            .record_observation(&owner, &wrong_session)
            .await
            .is_err()
    );
    sqlx::query("UPDATE agent_run_events SET session_id = ? WHERE user_id = ? AND run_id = ? AND event_idx = 2")
        .bind(&session_a).bind(&owner).bind(&run_b).execute(pool.get()).await.unwrap();
    assert!(
        observation_store
            .record_observation(&owner, &recovered)
            .await
            .is_err(),
        "custody with the wrong session cannot authorize settlement"
    );
    sqlx::query("UPDATE agent_run_events SET session_id = ? WHERE user_id = ? AND run_id = ? AND event_idx = 2")
        .bind(&session_b).bind(&owner).bind(&run_b).execute(pool.get()).await.unwrap();
    for index in 5..=135 {
        insert_run_settlement_finished_event(
            &pool,
            &owner,
            &session_b,
            &run_b,
            index as u64,
            index,
        )
        .await;
    }
    let old_admission = observation_store
        .load_admission_marker_for_run(&owner, &run_b, 2)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old_admission.admission_run_generation, 0);
    assert_eq!(old_admission.admission_event_idx, 1);
    assert!(!old_admission.settlement_finished);
    sqlx::query("UPDATE agent_run_events SET event_hash = 'tampered' WHERE user_id = ? AND run_id = ? AND event_idx = 1")
        .bind(&owner).bind(&run_b).execute(pool.get()).await.unwrap();
    assert!(
        observation_store
            .record_observation(&owner, &recovered)
            .await
            .is_err(),
        "admission hash must be revalidated before observation settlement"
    );
    // A discovery marker is not write authorization: changing the durable
    // admission after discovery must still be rejected in the write transaction.
    sqlx::query("DELETE FROM agent_run_events WHERE user_id = ? AND run_id = ? AND event_idx = 1")
        .bind(&owner)
        .bind(&run_b)
        .execute(pool.get())
        .await
        .unwrap();
    assert!(
        observation_store
            .record_observation(&owner, &recovered)
            .await
            .is_err(),
        "discovered admission cannot substitute for a missing durable admission"
    );
    let mut wrong_admission = admission_b.clone();
    wrong_admission.revision_content_hash = format!("sha256:{}", "d".repeat(64));
    wrong_admission
        .skill_revision
        .as_mut()
        .unwrap()
        .content_hash = wrong_admission.revision_content_hash.clone();
    insert_evaluation_admitted_event(&pool, &owner, &session_b, &run_b, 0, &wrong_admission).await;
    assert!(
        observation_store
            .record_observation(&owner, &recovered)
            .await
            .is_err(),
        "durable admission must match the frozen revision"
    );
    sqlx::query("DELETE FROM agent_run_events WHERE user_id = ? AND run_id = ? AND event_idx = 1")
        .bind(&owner)
        .bind(&run_b)
        .execute(pool.get())
        .await
        .unwrap();
    insert_evaluation_admitted_event(&pool, &owner, &session_b, &run_b, 0, &admission_b).await;
    sqlx::query("UPDATE agent_runs SET last_event_idx = -1 WHERE user_id = ? AND run_id = ?")
        .bind(&owner)
        .bind(&run_b)
        .execute(pool.get())
        .await
        .unwrap();
    assert!(
        observation_store
            .record_observation(&owner, &recovered)
            .await
            .is_err(),
        "admission beyond canonical Run watermark cannot authorize settlement"
    );
    sqlx::query("UPDATE agent_runs SET last_event_idx = 135 WHERE user_id = ? AND run_id = ?")
        .bind(&owner)
        .bind(&run_b)
        .execute(pool.get())
        .await
        .unwrap();
    let second = observation_store
        .record_observation(&owner, &recovered)
        .await
        .expect("record second-session observation");
    assert_eq!(second.admission_run_generation, 0);
    assert_eq!(second.execution_run_generation, 2);
    assert_eq!(second.materialization_receipt_ids, receipt_ids_b);
    for original in [&context_receipt_b, &policy_receipt_b] {
        let persisted = receipt_store
            .load_receipt(
                &owner,
                &original.receipt_id,
                &binding_b.trial_id,
                &session_b,
                &envelope_b,
            )
            .await
            .expect("original admission receipt remains readable after recovery settlement");
        assert_eq!(persisted.execution_run_generation, Some(0));
        assert_eq!(
            &persisted, original,
            "terminal generation must not rewrite original receipt provenance"
        );
    }
    let stale_request_b = MaterializationReceiptRequest {
        idempotency_key: "observation-b-stale-materializer".into(),
        ..context_request_b.clone()
    };
    assert!(
        matches!(
            receipt_store
                .record_receipt(&trusted_b, &stale_request_b)
                .await,
            Err(MaterializationReceiptError::Persistence(
                EvaluationPersistenceError::Conflict(_)
            ))
        ),
        "observation recovery must not authorize new receipts from the original materializer"
    );
    let rejected_receipt_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM evaluation_materialization_receipts WHERE owner_user_id = ? AND idempotency_key = ?",
    ).bind(&owner).bind(&stale_request_b.idempotency_key).fetch_one(pool.get()).await.unwrap();
    assert_eq!(rejected_receipt_count, 0);
    assert_eq!(
        receipt_store
            .record_receipt(&trusted_b, &context_request_b)
            .await
            .unwrap(),
        context_receipt_b,
        "an exact original receipt retry remains immutable"
    );
    let retained = plan_store
        .load_trial(&owner, &binding_b.trial_id)
        .await
        .unwrap();
    assert_eq!(retained.run_generation, Some(0));
    assert!(
        plan_store
            .bind_trial_run(&owner, &binding_b.trial_id, &session_b, &run_b)
            .await
            .is_err(),
        "ordinary bind remains strictly current-generation"
    );
    assert_eq!(
        plan_store
            .list_trial_run_statuses(&owner, &experiment_id)
            .await
            .unwrap()
            .get(&binding_b.trial_id)
            .map(String::as_str),
        Some("failed")
    );
    assert_eq!(
        observation_store
            .record_observation(&owner, &recovered)
            .await
            .unwrap(),
        second
    );
    assert_eq!(second.session_id, session_b);
    assert_eq!(
        observation_store
            .list_observations(&owner, &experiment_id)
            .await
            .expect("list owner observations")
            .len(),
        2
    );

    // Handoff advances the canonical generation. A new observation from the
    // stale runner is rejected, while the exact committed retry above stays
    // readable and immutable.
    sqlx::query(
        "UPDATE agent_runs SET run_generation = 1
         WHERE user_id = ? AND run_id = ?",
    )
    .bind(&owner)
    .bind(&run_a)
    .execute(pool.get())
    .await
    .expect("advance observation run generation");
    let mut cross_generation_completed = request_a.clone();
    cross_generation_completed.execution_run_generation = 1;
    cross_generation_completed.idempotency_key = "observation-cross-generation-completed".into();
    assert!(
        observation_store
            .record_observation(&owner, &cross_generation_completed)
            .await
            .is_err(),
        "a current Completed row never authorizes cross-generation success"
    );
    assert!(
        plan_store
            .bind_trial_run(&owner, &binding_a.trial_id, &session_a, &run_a)
            .await
            .is_err()
    );
    let stale_receipt = receipt_store
        .record_receipt(
            &trusted,
            &MaterializationReceiptRequest {
                trial_id: binding_a.trial_id.clone(),
                session_id: session_a.clone(),
                envelope: envelope.clone(),
                component_kind: MaterializationComponentKind::Context,
                component_snapshot_ref: Some("context://observation".into()),
                component_base_snapshot_ref: None,
                component_content_fingerprint: Some("sha256:context".into()),
                outcome: MaterializationOutcome::Available,
                failure_code: None,
                expires_at: Some(chrono::Utc::now() + chrono::Duration::minutes(5)),
                idempotency_key: "observation-stale-receipt".into(),
            },
        )
        .await;
    assert!(
        stale_receipt.is_err(),
        "new receipts remain strictly current-generation"
    );
    let mut stale = request_a.clone();
    stale.idempotency_key = "observation-stale".to_string();
    assert!(matches!(
        observation_store.record_observation(&owner, &stale).await,
        Err(EvaluationExecutionError::Persistence(
            EvaluationPersistenceError::Conflict(_)
        ))
    ));
    assert_eq!(
        observation_store
            .record_observation(&owner, &request_a)
            .await
            .expect("committed observation remains an exact retry")
            .observation_id,
        first.observation_id
    );

    cleanup(&pool, &owner).await;
    cleanup(&pool, &other_owner).await;
}

#[tokio::test]
#[ignore = "requires live MatrixOne (ASTRA_TEST_DB_IT=1)"]
async fn evaluation_plan_is_idempotent_owner_scoped_and_concurrent() {
    let pool = common::setup_pool().await;
    let store = DatabaseEvaluationPlanStore::new(pool.clone());
    let owner = format!("eval-owner-a-{}", Uuid::new_v4());
    let other_owner = format!("eval-owner-b-{}", Uuid::new_v4());

    let experiment_id = format!("eval-{}", Uuid::new_v4().simple());
    let original = spec(&experiment_id);
    let first = store
        .register_experiment(&owner, &original, "submit-a")
        .await
        .expect("register evaluation plan");
    let repeated = store
        .register_experiment(&owner, &original, "submit-a")
        .await
        .expect("repeat registration is idempotent");
    assert_eq!(first, repeated);
    assert_eq!(
        store
            .list_trials(&owner, &experiment_id)
            .await
            .unwrap()
            .len(),
        2
    );
    assert!(matches!(
        store.load_experiment(&other_owner, &experiment_id).await,
        Err(EvaluationPersistenceError::NotFound(_))
    ));

    let mut changed = original.clone();
    changed.target.candidate.content_hash = "sha256:changed".to_string();
    assert!(matches!(
        store
            .register_experiment(&owner, &changed, "submit-a")
            .await,
        Err(EvaluationPersistenceError::Conflict(_))
    ));

    let concurrent_id = format!("eval-race-{}", Uuid::new_v4().simple());
    let concurrent_spec = spec(&concurrent_id);
    let left_store = store.clone();
    let right_store = store.clone();
    let (left, right) = tokio::join!(
        left_store.register_experiment(&owner, &concurrent_spec, "submit-race"),
        right_store.register_experiment(&owner, &concurrent_spec, "submit-race"),
    );
    let left = left.expect("first concurrent registration succeeds");
    let right = right.expect("second concurrent registration is idempotent");
    assert_eq!(left.experiment_id, right.experiment_id);
    assert_eq!(left.spec_fingerprint, right.spec_fingerprint);
    assert_eq!(
        store
            .list_trials(&owner, &concurrent_id)
            .await
            .unwrap()
            .len(),
        2
    );

    let shared_left_id = format!("eval-shared-left-{}", Uuid::new_v4().simple());
    let shared_left_spec = spec(&shared_left_id);
    let conflicting_id = format!("eval-conflict-{}", Uuid::new_v4().simple());
    let conflicting_spec = spec(&conflicting_id);
    let conflict_left_store = store.clone();
    let conflict_right_store = store.clone();
    let (conflict_left, conflict_right) = tokio::join!(
        conflict_left_store.register_experiment(&owner, &shared_left_spec, "submit-shared"),
        conflict_right_store.register_experiment(&owner, &conflicting_spec, "submit-shared"),
    );
    assert_eq!(
        [conflict_left.is_ok(), conflict_right.is_ok()]
            .into_iter()
            .filter(|succeeded| *succeeded)
            .count(),
        1,
        "one experiment must own a submission idempotency key"
    );
    assert!(conflict_left.is_err() || conflict_right.is_err());

    let expanded_limit_id = format!("eval-expanded-limit-{}", Uuid::new_v4().simple());
    let mut expanded_limit_spec = spec(&expanded_limit_id);
    expanded_limit_spec.repetitions = 100;
    expanded_limit_spec.budget.max_trials = 200;
    expanded_limit_spec.cases[0].input_snapshot_ref = "x".repeat(100_000);
    let expanded_limit_error = store
        .register_experiment(&owner, &expanded_limit_spec, "submit-expanded-limit")
        .await
        .expect_err("expanded trial payload must be rejected before full allocation");
    assert!(matches!(
        expanded_limit_error,
        EvaluationPersistenceError::InvalidInput(message)
            if message.contains("expansion limit") || message.contains("persistence limit")
    ));

    // A failure while writing one trial must roll back the experiment and all
    // earlier trial inserts. The orphan fixture intentionally occupies the
    // second canonical trial key so the transaction fails after its parent
    // insert and first child insert have already been attempted.
    let rollback_id = format!("eval-rollback-{}", Uuid::new_v4().simple());
    let rollback_spec = spec(&rollback_id);
    let rollback_trials = rollback_spec.plan_trials().unwrap();
    sqlx::query(
        "INSERT INTO evaluation_trial_bindings
         (owner_user_id, trial_id, experiment_id, spec_fingerprint, sequence_num,
          trial_json, binding_status, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, 'planned', NOW(6), NOW(6))",
    )
    .bind(&owner)
    .bind(&rollback_trials[1].trial_id)
    .bind(&rollback_trials[1].experiment_id)
    .bind(&rollback_trials[1].spec_fingerprint)
    .bind(i64::from(rollback_trials[1].sequence))
    .bind(serde_json::to_string(&rollback_trials[1]).unwrap())
    .execute(pool.get())
    .await
    .expect("insert rollback collision fixture");
    assert!(
        store
            .register_experiment(&owner, &rollback_spec, "submit-rollback")
            .await
            .is_err()
    );
    assert!(matches!(
        store.load_experiment(&owner, &rollback_id).await,
        Err(EvaluationPersistenceError::NotFound(_))
    ));
    let rollback_parent_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM evaluation_experiments WHERE owner_user_id = ? AND experiment_id = ?",
    )
    .bind(&owner)
    .bind(&rollback_id)
    .fetch_one(pool.get())
    .await
    .expect("count rolled back evaluation parent");
    assert_eq!(rollback_parent_count, 0);

    cleanup(&pool, &owner).await;
    cleanup(&pool, &other_owner).await;
}

#[tokio::test]
#[ignore = "requires live MatrixOne (ASTRA_TEST_DB_IT=1)"]
async fn evaluation_trial_binding_is_atomic_across_sessions_and_owner_scoped() {
    let pool = common::setup_pool().await;
    let store = DatabaseEvaluationPlanStore::new(pool.clone());
    let owner = format!("eval-bind-owner-{}", Uuid::new_v4());
    let other_owner = format!("eval-bind-other-{}", Uuid::new_v4());
    for (order_index, order) in [
        TrialOrder::BaselineFirst,
        TrialOrder::CandidateFirst,
        TrialOrder::Balanced { seed: 42 },
    ]
    .into_iter()
    .enumerate()
    {
        let experiment_id = format!("eval-bind-{}", Uuid::new_v4().simple());
        let mut protocol_spec = spec(&experiment_id);
        protocol_spec.order = order;
        protocol_spec.repetitions = 2;
        protocol_spec.budget.max_trials = 4;
        protocol_spec.budget.max_concurrency = 1;
        let record = store
            .register_experiment(
                &owner,
                &protocol_spec,
                &format!("submit-bind-{order_index}"),
            )
            .await
            .expect("register binding plan");
        let trials = store
            .list_trials(&owner, &record.experiment_id)
            .await
            .expect("list planned trials");
        assert!(
            record
                .spec
                .paired_predecessor(&trials[0].trial)
                .unwrap()
                .is_none()
        );

        // The persisted JSON is not trusted merely because its row count is
        // correct: changing the arm must fail the canonical-plan comparison.
        let mut tampered_trial = trials[0].trial.clone();
        tampered_trial.arm = match tampered_trial.arm {
            ComparisonArm::Baseline => ComparisonArm::Candidate,
            ComparisonArm::Candidate => ComparisonArm::Baseline,
        };
        sqlx::query(
            "UPDATE evaluation_trial_bindings SET trial_json = ?
         WHERE owner_user_id = ? AND trial_id = ?",
        )
        .bind(serde_json::to_string(&tampered_trial).unwrap())
        .bind(&owner)
        .bind(&trials[0].trial_id)
        .execute(pool.get())
        .await
        .expect("tamper evaluation trial fixture");
        assert!(matches!(
            store.list_trials(&owner, &record.experiment_id).await,
            Err(EvaluationPersistenceError::Conflict(_))
        ));
        sqlx::query(
            "UPDATE evaluation_trial_bindings SET trial_json = ?
         WHERE owner_user_id = ? AND trial_id = ?",
        )
        .bind(serde_json::to_string(&trials[0].trial).unwrap())
        .bind(&owner)
        .bind(&trials[0].trial_id)
        .execute(pool.get())
        .await
        .expect("restore evaluation trial fixture");

        // The hot-path loader must also reject a coordinated sequence tamper;
        // updating both the JSON and relational column must not bypass the
        // deterministic ordering contract.
        let mut tampered_sequence = trials[0].trial.clone();
        tampered_sequence.sequence = 99;
        sqlx::query(
            "UPDATE evaluation_trial_bindings SET sequence_num = ?, trial_json = ?
         WHERE owner_user_id = ? AND trial_id = ?",
        )
        .bind(99_i64)
        .bind(serde_json::to_string(&tampered_sequence).unwrap())
        .bind(&owner)
        .bind(&trials[0].trial_id)
        .execute(pool.get())
        .await
        .expect("tamper evaluation sequence fixture");
        assert!(matches!(
            store.load_trial(&owner, &trials[0].trial_id).await,
            Err(EvaluationPersistenceError::Conflict(_))
        ));
        sqlx::query(
            "UPDATE evaluation_trial_bindings SET sequence_num = ?, trial_json = ?
         WHERE owner_user_id = ? AND trial_id = ?",
        )
        .bind(i64::from(trials[0].trial.sequence))
        .bind(serde_json::to_string(&trials[0].trial).unwrap())
        .bind(&owner)
        .bind(&trials[0].trial_id)
        .execute(pool.get())
        .await
        .expect("restore evaluation sequence fixture");

        let run_store = DatabaseRunStateStore::new(pool.clone());
        let observation_store = DatabaseEvaluationObservationStore::new(pool.clone());
        let mut runs = Vec::new();
        for trial in &trials {
            let session = insert_session(&pool, &owner).await;
            runs.push(evaluation_run_record(
                &owner,
                &session,
                &record.spec,
                &trial.trial,
            ));
        }
        // Public bind cannot manufacture admission for an unrelated existing Run.
        let unbound_session = insert_session(&pool, &owner).await;
        let unbound_run = insert_run(&pool, &owner, &unbound_session).await;
        assert!(
            store
                .bind_trial_run(&owner, &trials[0].trial_id, &unbound_session, &unbound_run)
                .await
                .is_err()
        );
        assert!(
            run_store
                .claim_run_start(runs[2].clone(), Some(&runs[2].session_id))
                .await
                .unwrap_err()
                .starts_with("evaluation_trial_order_blocked:"),
            "all preceding sequences must be bound"
        );
        let (first, second) = tokio::join!(
            run_store.claim_run_start(runs[0].clone(), Some(&runs[0].session_id)),
            run_store.claim_run_start(runs[1].clone(), Some(&runs[1].session_id)),
        );
        assert!(matches!(
            first.unwrap(),
            DurableRunStartClaim::Started { .. }
        ));
        assert!(
            second
                .unwrap_err()
                .starts_with("evaluation_trial_order_blocked:")
        );
        assert!(
            run_store
                .load_run(&owner, &runs[1].run_id)
                .await
                .unwrap()
                .is_none(),
            "blocked admission must not leave a Run"
        );
        let first_binding = store
            .bind_trial_run(
                &owner,
                &trials[0].trial_id,
                &runs[0].session_id,
                &runs[0].run_id,
            )
            .await
            .expect("ordinary binding confirms atomic creation");
        assert_eq!(first_binding.binding_status, "bound");
        assert!(
            matches!(
                run_store
                    .claim_run_start(runs[0].clone(), Some(&runs[0].session_id))
                    .await
                    .unwrap(),
                DurableRunStartClaim::Existing { .. }
            ),
            "exact retry is exempt even at max concurrency"
        );

        let observation_request =
            |index: usize, status: TrialStatus| EvaluationObservationRequest {
                session_id: runs[index].session_id.clone(),
                execution_run_id: runs[index].run_id.clone(),
                admission_run_generation: 0,
                execution_run_generation: 0,
                observation: TrialObservation {
                    experiment_fingerprint: record.spec_fingerprint.clone(),
                    trial_id: trials[index].trial_id.clone(),
                    case_id: trials[index].trial.case_id.clone(),
                    arm: trials[index].trial.arm.clone(),
                    repetition: trials[index].trial.repetition,
                    status,
                    measurements: vec![],
                    evidence: vec![],
                    judgment: None,
                },
                materialization_receipt_ids: vec![],
                idempotency_key: format!("protocol-observation-{order_index}-{index}"),
            };
        sqlx::query("UPDATE agent_runs SET status = 'failed' WHERE user_id = ? AND run_id = ?")
            .bind(&owner)
            .bind(&runs[0].run_id)
            .execute(pool.get())
            .await
            .unwrap();
        assert!(
            run_store
                .claim_run_start(runs[1].clone(), Some(&runs[1].session_id))
                .await
                .unwrap_err()
                .starts_with("evaluation_trial_order_blocked:"),
            "terminal Run state alone is not an immutable paired observation"
        );
        observation_store
            .record_observation(&owner, &observation_request(0, TrialStatus::Failed))
            .await
            .unwrap();
        // Exactly one admission slot is now free. These different pairs race
        // through the same experiment mutex; neither scheduling order may
        // commit two Runs or leave an orphan for the losing contender.
        let (paired_start, next_pair_start) = tokio::join!(
            run_store.claim_run_start(runs[1].clone(), Some(&runs[1].session_id)),
            run_store.claim_run_start(runs[2].clone(), Some(&runs[2].session_id)),
        );
        assert_eq!(
            [paired_start.is_ok(), next_pair_start.is_ok()]
                .into_iter()
                .filter(|started| *started)
                .count(),
            1,
            "last-slot race must commit exactly one new Run"
        );
        assert!(
            matches!(paired_start.unwrap(), DurableRunStartClaim::Started { .. }),
            "failed first-arm observation releases its paired second arm"
        );
        let loser = next_pair_start.unwrap_err();
        assert!(
            loser.starts_with("evaluation_trial_capacity_exhausted:")
                || loser.starts_with("evaluation_trial_order_blocked:")
        );
        let orphan_events: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_run_events WHERE user_id = ? AND run_id = ?",
        )
        .bind(&owner)
        .bind(&runs[2].run_id)
        .fetch_one(pool.get())
        .await
        .unwrap();
        assert_eq!(orphan_events, 0, "loser must not leave initial Run events");
        assert!(
            matches!(
                run_store
                    .claim_run_start(runs[1].clone(), Some(&runs[1].session_id))
                    .await
                    .unwrap(),
                DurableRunStartClaim::Existing { .. }
            ),
            "the winning Run's retry is exempt at full capacity"
        );
        assert!(
            run_store
                .claim_run_start(runs[2].clone(), Some(&runs[2].session_id))
                .await
                .unwrap_err()
                .starts_with("evaluation_trial_capacity_exhausted:"),
            "a different pair cannot exceed bound minus observed capacity"
        );
        assert!(
            store
                .load_trial(&owner, &trials[2].trial_id)
                .await
                .unwrap()
                .run_id
                .is_none()
        );
        assert!(
            run_store
                .load_run(&owner, &runs[2].run_id)
                .await
                .unwrap()
                .is_none()
        );
        sqlx::query("UPDATE agent_runs SET status = 'cancelled' WHERE user_id = ? AND run_id = ?")
            .bind(&owner)
            .bind(&runs[1].run_id)
            .execute(pool.get())
            .await
            .unwrap();
        assert!(
            run_store
                .claim_run_start(runs[2].clone(), Some(&runs[2].session_id))
                .await
                .unwrap_err()
                .starts_with("evaluation_trial_capacity_exhausted:"),
            "cancelled without observation still consumes capacity"
        );

        // Hold the experiment mutex while the next start waits. Committing an
        // observation before releasing it must be visible to the admission current read.
        let mut mutex = pool.get().begin().await.unwrap();
        sqlx::query("SELECT experiment_id FROM evaluation_experiments WHERE owner_user_id = ? AND experiment_id = ? FOR UPDATE")
        .bind(&owner).bind(&record.experiment_id).fetch_one(&mut *mutex).await.unwrap();
        // An isolated one-connection pool lets the process-list barrier identify
        // the exact request that has reached the locked experiment row.
        let mut waiter_settings = pool.settings().clone();
        waiter_settings.db_pool_max_connections = 1;
        waiter_settings.db_pool_min_connections = 0;
        let waiter_pool = SharedPool::new(&waiter_settings).await.unwrap();
        let waiter_connection_id: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
            .fetch_one(waiter_pool.get())
            .await
            .unwrap();
        let waiting_store = DatabaseRunStateStore::new(waiter_pool.clone());
        let waiting_run = runs[2].clone();
        let waiter = tokio::spawn(async move {
            waiting_store
                .claim_run_start(waiting_run.clone(), Some(&waiting_run.session_id))
                .await
        });
        // Match the established MatrixOne process-list barrier used by the
        // weighted-admission DB tests: no sleep can substitute for arrival.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let waiting: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM information_schema.processlist
                     WHERE db = ? AND conn_id = ?
                       AND info LIKE '%evaluation_experiments%'
                       AND info LIKE '%FOR UPDATE%'
                       AND info NOT LIKE '%information_schema.processlist%'",
                ).bind(&waiter_settings.database).bind(waiter_connection_id)
                    .fetch_one(pool.get()).await.unwrap();
                if waiting > 0 { break; }
                assert!(!waiter.is_finished(), "start completed before reaching the held experiment mutex");
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await.expect("observe exact Run start blocked on experiment FOR UPDATE before committing observation");
        observation_store
            .record_observation(&owner, &observation_request(1, TrialStatus::Cancelled))
            .await
            .unwrap();
        mutex.commit().await.unwrap();
        assert!(
            matches!(
                waiter.await.unwrap().unwrap(),
                DurableRunStartClaim::Started { .. }
            ),
            "admission reads observation committed while the experiment mutex was held"
        );
        waiter_pool.close().await;
        assert!(matches!(
            run_store
                .claim_run_start(runs[2].clone(), Some(&runs[2].session_id))
                .await
                .unwrap(),
            DurableRunStartClaim::Existing { .. }
        ));
        assert!(
            run_store
                .claim_run_start(runs[3].clone(), Some(&runs[3].session_id))
                .await
                .unwrap_err()
                .starts_with("evaluation_trial_order_blocked:")
        );

        let competing =
            evaluation_run_record(&owner, &runs[2].session_id, &record.spec, &trials[2].trial);
        assert!(
            run_store
                .claim_run_start(competing, Some(&runs[2].session_id))
                .await
                .is_err(),
            "a bound trial cannot be rebound to a different Run"
        );
        let other_session = insert_session(&pool, &other_owner).await;
        let foreign =
            evaluation_run_record(&other_owner, &other_session, &record.spec, &trials[0].trial);
        assert!(
            run_store
                .claim_run_start(foreign, Some(&other_session))
                .await
                .is_err()
        );
        assert!(
            store
                .bind_trial_run(
                    &owner,
                    &trials[0].trial_id,
                    &runs[1].session_id,
                    &runs[1].run_id
                )
                .await
                .is_err()
        );
        assert!(matches!(
            store
                .bind_trial_run(
                    &other_owner,
                    &trials[0].trial_id,
                    &other_session,
                    &runs[0].run_id
                )
                .await,
            Err(EvaluationPersistenceError::NotFound(_))
        ));
    }
    cleanup(&pool, &owner).await;
    cleanup(&pool, &other_owner).await;
}

/// Real admission receipts and a canonical atomic terminal transaction. The
/// returned request still crosses the production observation writer in tests.
async fn settle_task_assessment_trial(
    pool: &SharedPool,
    owner: &str,
    experiment: &astra_services::evaluation::EvaluationExperimentRecord,
    trial: &TrialUnit,
    content: &str,
    status: &str,
) -> EvaluationObservationRequest {
    use astra_services::evaluation::content_fingerprint;
    use astra_services::runs::AtomicRunTerminalSettlementRequest;
    let session = insert_session(pool, owner).await;
    let mut run_record = evaluation_run_record(owner, &session, &experiment.spec, trial);
    run_record.status = "running".into();
    let run = run_record.run_id.clone();
    let store = DatabaseRunStateStore::new(pool.clone()).with_owner_pod_id("assessment-pod");
    assert!(matches!(
        store
            .claim_run_start(run_record, Some(&session))
            .await
            .unwrap(),
        DurableRunStartClaim::Started { .. }
    ));
    let envelope = SnapshotEnvelope::new(
        owner,
        &experiment.experiment_id,
        Some(trial.trial_id.clone()),
        CompositeSnapshot {
            snapshot_id: Uuid::new_v4().to_string(),
            session_id: session.clone(),
            turn: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            version: 1,
            label: None,
            refs: vec![],
        },
        &experiment.spec.conditions.context_snapshot_hash,
        &experiment.spec.conditions.tool_policy_hash,
    )
    .unwrap();
    let trusted = TrustedMaterializerContext {
        owner_user_id: owner.into(),
        materializer_kind: "test.task_assessment".into(),
        provider_binding_id: Some(experiment.spec.conditions.provider_binding.clone()),
        execution_run_id: Some(run.clone()),
        execution_run_generation: Some(0),
    };
    let receipt_store = DatabaseMaterializationReceiptStore::new(pool.clone());
    let mut receipt_ids = Vec::new();
    for (kind, name, hash) in [
        (
            MaterializationComponentKind::Context,
            "context",
            &experiment.spec.conditions.context_snapshot_hash,
        ),
        (
            MaterializationComponentKind::Policy,
            "policy",
            &experiment.spec.conditions.tool_policy_hash,
        ),
    ] {
        let receipt = receipt_store
            .record_receipt(
                &trusted,
                &MaterializationReceiptRequest {
                    trial_id: trial.trial_id.clone(),
                    session_id: session.clone(),
                    envelope: envelope.clone(),
                    component_kind: kind,
                    component_snapshot_ref: Some(format!("{name}://assessment")),
                    component_base_snapshot_ref: None,
                    component_content_fingerprint: Some(hash.clone()),
                    outcome: MaterializationOutcome::Available,
                    failure_code: None,
                    expires_at: Some(chrono::Utc::now() + chrono::Duration::minutes(5)),
                    idempotency_key: format!("{run}:{name}"),
                },
            )
            .await
            .unwrap();
        receipt_ids.push(receipt.receipt_id);
    }
    let mut admission: EvaluationRunAdmission = serde_json::from_value(
        evaluation_run_record(owner, &session, &experiment.spec, trial).events[0]["data"]["evaluation_admission"].clone()
    ).unwrap();
    admission.receipt_ids = receipt_ids.clone();
    admission.snapshot_envelope = Some(envelope);
    store.append_events_batch(owner, &session, &run, &[serde_json::json!({
        "event_type":"evaluation_admitted", "run_generation":0,
        "idempotency_key":format!("evaluation-admitted:{run}:0"), "data":{"admission":admission}
    })]).await.unwrap();
    let invocation_ledger = DatabaseToolInvocationLedger::new(pool.clone());
    let invocation = ToolInvocationIdentity::new(
        owner,
        &session,
        &run,
        "assessment-cache-turn",
        "assessment-cache-invocation",
    )
    .unwrap();
    let invocation_decision = ToolInvocationDecision::new(
        &serde_json::json!({"route":"assessment_fixture","policy":"frozen"}),
    )
    .unwrap();
    let invocation_arguments = serde_json::json!({"resource":"assessment-fixture"});
    let invocation_fingerprint = ToolInvocationFingerprint::new(
        DurableToolReference::built_in("bash", "assessment-v1").unwrap(),
        &invocation_arguments,
        &invocation_decision.decision_id,
    )
    .unwrap();
    assert!(matches!(
        invocation_ledger
            .prepare(&invocation, &invocation_fingerprint, &invocation_decision)
            .await
            .unwrap(),
        ToolInvocationPrepareOutcome::Prepared(_)
    ));
    let control_epoch = store
        .load_run(owner, &run)
        .await
        .unwrap()
        .unwrap()
        .last_event_idx;
    invocation_ledger
        .complete_from_semantic_read_cache(
            &invocation,
            &ToolInvocationResultPayload::new(
                "cached assessment observation",
                BTreeMap::new(),
                None,
            )
            .unwrap(),
            &ToolInvocationCompletionSource::semantic_read_cache(
                format!("sha256:{}", "a".repeat(64)),
                format!("sha256:{}", "b".repeat(64)),
            )
            .unwrap(),
            ToolInvocationDispatchAdmission {
                expected_control_epoch: control_epoch,
                expected_owner_generation: 0,
                expected_owner_pod_id: store.owner_pod_id().to_string(),
                expected_execution_binding_generation: None,
            },
        )
        .await
        .unwrap();
    let events = [
        serde_json::json!({"event_type":"run_output_recorded","idempotency_key":"run-output-recorded:0",
            "data":{"schema_version":1,"owner_user_id":owner,"session_id":session,"run_id":run,"run_generation":0,
                "source_event_id":"assessment-output","content_hash":content_fingerprint(content),"content_bytes":content.len()}}),
        serde_json::json!({"event_type":"run_finished","data":{"status":status}}),
        serde_json::json!({"event_type":"run_accounting_finalized","idempotency_key":"run-accounting-finalized:0",
            "data":{"usage_scope":"run_total","prompt_tokens":7,"cache_read_tokens":4,"cache_creation_tokens":2,"completion_tokens":5,"tool_call_count":1}}),
    ];
    let mut tx = pool.get().begin().await.unwrap();
    sqlx::query("INSERT INTO session_transcript_items (user_id,session_id,item_seq,run_id,role,content,source_event_id,content_hash) VALUES (?,?,1,?,'assistant',?,'assessment-output','transcript-envelope')")
        .bind(owner).bind(&session).bind(&run).bind(content).execute(&mut *tx).await.unwrap();
    store
        .settle_terminal_in_existing_transaction(
            &mut tx,
            AtomicRunTerminalSettlementRequest {
                user_id: owner,
                expected_session_id: &session,
                run_id: &run,
                expected_owner_generation: 0,
                expected_statuses: &["running"],
                status,
                waiting_for: None,
                error_message: None,
                events: &events,
                prompt_tokens: 13,
                completion_tokens: 5,
                tool_calls: 1,
            },
        )
        .await
        .unwrap()
        .expect("canonical atomic terminal commits");
    tx.commit().await.unwrap();
    store
        .append_events_batch(
            owner,
            &session,
            &run,
            &[serde_json::json!({"event_type":"run_settlement_finished",
        "idempotency_key":"run-settlement-finished:0","data":{"owner_generation":0}})],
        )
        .await
        .unwrap();
    EvaluationObservationRequest {
        session_id: session,
        execution_run_id: run.clone(),
        admission_run_generation: 0,
        execution_run_generation: 0,
        observation: astra_services::evaluation::terminal_run_observation(
            experiment.spec_fingerprint.clone(),
            trial.trial_id.clone(),
            trial.case_id.clone(),
            trial.arm.clone(),
            trial.repetition,
            if status == "completed" {
                TrialStatus::Completed
            } else {
                TrialStatus::Failed
            },
            Some(7),
            Some(5),
            Some(1),
            vec![EvidenceRef {
                evidence_id: format!("run:{run}"),
                kind: EvidenceKind::Trace,
                availability: EvidenceAvailability::Available,
                content_hash: None,
                locator: Some(format!("run://{owner}/{run}")),
            }],
        ),
        materialization_receipt_ids: receipt_ids,
        idempotency_key: format!("{run}:observation"),
    }
}

#[tokio::test]
#[ignore = "requires live MatrixOne (ASTRA_TEST_DB_IT=1)"]
async fn task_assessment_is_historical_owner_bound_and_retries_missing_sources() {
    use astra_services::evaluation::{
        TaskAssessmentOutcome, TaskAssessmentResult, TaskAssessmentUnavailableReason,
    };
    let pool = common::setup_pool().await;
    let owner = format!("assessment-owner-{}", Uuid::new_v4());
    let plan_store = DatabaseEvaluationPlanStore::new(pool.clone());
    let store = DatabaseEvaluationObservationStore::new(pool.clone());
    let experiment_id = format!("assessment-{}", Uuid::new_v4());
    let experiment = plan_store
        .register_experiment(&owner, &spec(&experiment_id), "task-assessment")
        .await
        .unwrap();
    let trials = experiment.spec.plan_trials().unwrap();
    for (trial, content, expected) in [
        (&trials[0], "{\"ok\":false}", TaskAssessmentOutcome::Fail),
        (&trials[1], "{\"ok\":true}", TaskAssessmentOutcome::Pass),
    ] {
        let request =
            settle_task_assessment_trial(&pool, &owner, &experiment, trial, content, "completed")
                .await;
        assert_eq!(
            store
                .assess_trial(&owner, &experiment_id, &trial.trial_id)
                .await
                .unwrap(),
            TaskAssessmentResult::Pending
        );
        // Historical repair must not require an active session or renewed receipt.
        sqlx::query(
            "UPDATE agent_sessions SET status = 'archived' WHERE user_id = ? AND session_id = ?",
        )
        .bind(&owner)
        .bind(&request.session_id)
        .execute(pool.get())
        .await
        .unwrap();
        sqlx::query("UPDATE evaluation_materialization_receipts SET expires_at = DATE_SUB(NOW(6), INTERVAL 1 DAY) WHERE owner_user_id = ? AND trial_id = ?")
            .bind(&owner).bind(&trial.trial_id).execute(pool.get()).await.unwrap();
        let observation = store
            .record_observation(&owner, &request)
            .await
            .expect("historical observation repair");
        sqlx::query("DELETE FROM session_transcript_items WHERE user_id = ? AND session_id = ?")
            .bind(&owner)
            .bind(&request.session_id)
            .execute(pool.get())
            .await
            .unwrap();
        assert_eq!(
            store
                .assess_trial(&owner, &experiment_id, &trial.trial_id)
                .await
                .unwrap(),
            TaskAssessmentResult::Pending
        );
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM evaluation_task_assessments WHERE owner_user_id = ? AND trial_id = ?")
            .bind(&owner).bind(&trial.trial_id).fetch_one(pool.get()).await.unwrap();
        assert_eq!(
            count, 0,
            "missing transcript must not become immutable unavailable"
        );
        sqlx::query("INSERT INTO session_transcript_items (user_id,session_id,item_seq,run_id,role,content,source_event_id,content_hash) VALUES (?,?,1,?,'assistant',?,'assessment-output','transcript-envelope')")
            .bind(&owner).bind(&request.session_id).bind(&request.execution_run_id).bind(content).execute(pool.get()).await.unwrap();
        // Re-publish the exact production-validated observation behind the
        // Session lock. Assessment discovery precedes observation publication.
        sqlx::query(
            "DELETE FROM evaluation_trial_observations WHERE owner_user_id = ? AND trial_id = ?",
        )
        .bind(&owner)
        .bind(&trial.trial_id)
        .execute(pool.get())
        .await
        .unwrap();
        let mut publishing = pool.get().begin().await.unwrap();
        sqlx::query(
            "SELECT session_id FROM agent_sessions WHERE user_id = ? AND session_id = ? FOR UPDATE",
        )
        .bind(&owner)
        .bind(&request.session_id)
        .fetch_one(&mut *publishing)
        .await
        .unwrap();
        sqlx::query("INSERT INTO evaluation_trial_observations
            (schema_version,owner_user_id,observation_id,experiment_id,trial_id,session_id,execution_run_id,
             admission_run_generation,execution_run_generation,spec_fingerprint,observation_json,
             materialization_receipt_ids_json,request_fingerprint,idempotency_key,created_at,updated_at)
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,NOW(6),NOW(6))")
            .bind(observation.schema_version).bind(&owner).bind(&observation.observation_id).bind(&experiment_id)
            .bind(&trial.trial_id).bind(&request.session_id).bind(&request.execution_run_id)
            .bind(observation.admission_run_generation).bind(observation.execution_run_generation)
            .bind(&experiment.spec_fingerprint).bind(serde_json::to_string(&observation.observation).unwrap())
            .bind(serde_json::to_string(&observation.materialization_receipt_ids).unwrap())
            .bind(&observation.request_fingerprint).bind(&observation.idempotency_key)
            .execute(&mut *publishing).await.unwrap();
        let waiting = store.assess_trial(&owner, &experiment_id, &trial.trial_id);
        tokio::pin!(waiting);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut waiting)
                .await
                .is_err(),
            "assessment must wait for the canonical Session lock"
        );
        publishing.commit().await.unwrap();
        let (left, right) = tokio::join!(
            waiting,
            store.assess_trial(&owner, &experiment_id, &trial.trial_id)
        );
        let first = left.unwrap();
        assert_eq!(
            first,
            right.unwrap(),
            "concurrent assess must return the same receipt"
        );
        let TaskAssessmentResult::Recorded(record) = &first else {
            panic!("assessment must persist")
        };
        assert_eq!(record.outcome, expected);
        assert_eq!(record.tool_invocation_coverage.admitted_action_ids.len(), 1);
        assert_eq!(record.tool_invocation_coverage.completion_refs.len(), 1);
        record.validate_binding(&experiment, &observation).unwrap();
        assert!(
            store
                .assess_trial("other-owner", &experiment_id, &trial.trial_id)
                .await
                .is_err()
        );
        assert!(
            store
                .assess_trial(&owner, "other-experiment", &trial.trial_id)
                .await
                .is_err()
        );
        sqlx::query("DELETE FROM session_transcript_items WHERE user_id = ? AND session_id = ?")
            .bind(&owner)
            .bind(&request.session_id)
            .execute(pool.get())
            .await
            .unwrap();
        assert_eq!(
            store
                .assess_trial(&owner, &experiment_id, &trial.trial_id)
                .await
                .unwrap(),
            first,
            "existing assessment survives source cleanup"
        );
    }
    let failed_id = format!("assessment-failed-{}", Uuid::new_v4());
    let failed = plan_store
        .register_experiment(&owner, &spec(&failed_id), "task-assessment-failed")
        .await
        .unwrap();
    let trial = failed.spec.plan_trials().unwrap().remove(0);
    let request =
        settle_task_assessment_trial(&pool, &owner, &failed, &trial, "{\"ok\":true}", "failed")
            .await;
    store.record_observation(&owner, &request).await.unwrap();
    let TaskAssessmentResult::Recorded(record) = store
        .assess_trial(&owner, &failed_id, &trial.trial_id)
        .await
        .unwrap()
    else {
        panic!("failed run assessment")
    };
    assert_eq!(
        record.outcome,
        TaskAssessmentOutcome::Unavailable(TaskAssessmentUnavailableReason::TerminalNotCompleted)
    );
    assert!(record.output.is_none());
    // Even an oversized body must match the full receipt hash before an
    // immutable unavailable verdict. Mixed UTF-8 widths cross chunk boundaries;
    // mutate the final emoji so a later chunk must participate in the hash.
    let oversized_trial = failed.spec.plan_trials().unwrap().remove(1);
    let oversized = format!("\"{}\"", "a你🙂".repeat(150_000));
    let oversized_request = settle_task_assessment_trial(
        &pool,
        &owner,
        &failed,
        &oversized_trial,
        &oversized,
        "completed",
    )
    .await;
    store
        .record_observation(&owner, &oversized_request)
        .await
        .unwrap();
    let mut tampered = oversized.clone();
    let last_emoji = tampered.rfind('🙂').unwrap();
    tampered.replace_range(last_emoji..last_emoji + '🙂'.len_utf8(), "🙃");
    assert_eq!(oversized.len(), tampered.len());
    sqlx::query(
        "UPDATE session_transcript_items SET content = ? WHERE user_id = ? AND session_id = ?",
    )
    .bind(&tampered)
    .bind(&owner)
    .bind(&oversized_request.session_id)
    .execute(pool.get())
    .await
    .unwrap();
    assert!(matches!(
        store
            .assess_trial(&owner, &failed_id, &oversized_trial.trial_id)
            .await,
        Err(astra_services::evaluation::TaskAssessmentError::Integrity(
            _
        ))
    ));
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM evaluation_task_assessments WHERE owner_user_id = ? AND trial_id = ?",
    )
    .bind(&owner)
    .bind(&oversized_trial.trial_id)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(
        count, 0,
        "oversize hash mismatch must not persist unavailable"
    );
    sqlx::query(
        "UPDATE session_transcript_items SET content = ? WHERE user_id = ? AND session_id = ?",
    )
    .bind(&oversized)
    .bind(&owner)
    .bind(&oversized_request.session_id)
    .execute(pool.get())
    .await
    .unwrap();
    let TaskAssessmentResult::Recorded(record) = store
        .assess_trial(&owner, &failed_id, &oversized_trial.trial_id)
        .await
        .unwrap()
    else {
        panic!("oversize assessment")
    };
    assert_eq!(
        record.outcome,
        TaskAssessmentOutcome::Unavailable(TaskAssessmentUnavailableReason::OutputTooLarge)
    );
    cleanup(&pool, &owner).await;
}

#[tokio::test]
#[ignore = "requires live MatrixOne (ASTRA_TEST_DB_IT=1)"]
async fn workspace_admission_resolves_allocation_artifacts_and_rejects_missing_or_changed_evidence()
{
    use astra_runtime_env::{
        EvaluationAllocationReceipt, ToolchainInput, ToolchainManifest,
        WORKSPACE_CONFINEMENT_PROFILE, WorkspaceConfinementContract,
    };
    use astra_services::evaluation::{
        FrozenWorkspaceExecution, content_fingerprint,
        workspace_evidence::EvaluationWorkspaceMaterialization,
    };
    let pool = common::setup_pool().await;
    let plans = DatabaseEvaluationPlanStore::new(pool.clone());
    let receipts = DatabaseMaterializationReceiptStore::new(pool.clone());
    let owner = format!("allocation-owner-{}", Uuid::new_v4());
    let mut frozen = spec(&format!("allocation-exp-{}", Uuid::new_v4().simple()));
    let confinement = WorkspaceConfinementContract {
        profile_id: WORKSPACE_CONFINEMENT_PROFILE.into(),
        toolchain_manifest: ToolchainManifest {
            schema_version: 1,
            inputs: vec![ToolchainInput {
                guest_mount_path: "/usr/bin".into(),
                content_digest: format!("sha256:{}", "a".repeat(64)),
            }],
            launcher_digest: format!("sha256:{}", "b".repeat(64)),
            supervisor_digest: format!("sha256:{}", "c".repeat(64)),
        },
    };
    frozen.conditions.workspace_execution = Some(FrozenWorkspaceExecution {
        confinement: confinement.clone(),
        edge_executor_id: "edge-a".into(),
        source_commit: "a".repeat(40),
        tool_names: vec!["bash".into()],
    });
    frozen.conditions.isolation_profile = "edge_workspace_private_v1".into();
    let experiment = plans
        .register_experiment(&owner, &frozen, "allocation-submit")
        .await
        .unwrap();
    let trials = plans
        .list_trials(&owner, &experiment.experiment_id)
        .await
        .unwrap();
    let session = insert_session(&pool, &owner).await;
    let run =
        insert_evaluation_run(&pool, &owner, &session, &experiment.spec, &trials[0].trial).await;
    let binding = plans
        .bind_trial_run(&owner, &trials[0].trial_id, &session, &run)
        .await
        .unwrap();
    let envelope = SnapshotEnvelope::new(
        &owner,
        &experiment.experiment_id,
        Some(binding.trial_id.clone()),
        CompositeSnapshot {
            snapshot_id: Uuid::new_v4().to_string(),
            session_id: session.clone(),
            turn: 1,
            created_at: "2026-09-20T00:00:00Z".into(),
            version: 1,
            label: None,
            refs: vec![],
        },
        "sha256:context",
        "sha256:tools",
    )
    .unwrap();
    let trusted = TrustedMaterializerContext {
        owner_user_id: owner.clone(),
        materializer_kind: "test.edge".into(),
        provider_binding_id: Some("provider-v1".into()),
        execution_run_id: Some(run.clone()),
        execution_run_generation: binding.run_generation,
    };
    let evidence = EvaluationWorkspaceMaterialization {
        schema_version: 1,
        experiment_id: experiment.experiment_id.clone(),
        trial_id: binding.trial_id.clone(),
        run_generation: binding.run_generation.unwrap(),
        spec_fingerprint: binding.spec_fingerprint.clone(),
        edge_executor_id: "edge-a".into(),
        connection_generation: 1,
        allocation: EvaluationAllocationReceipt {
            schema_version: 1,
            allocation_id: Uuid::new_v4().to_string(),
            owner_user_id: owner.clone(),
            session_id: session.clone(),
            run_id: run,
            deployment_id: "deployment-a".into(),
            materialization_id: "materialization-a".into(),
            workspace_dir: "/allocation/trial-a".into(),
            source_commit: "a".repeat(40),
            source_tree: "b".repeat(40),
            confinement_fingerprint: confinement.fingerprint().unwrap(),
        },
    };
    evidence
        .validate_binding(&binding, &experiment.spec)
        .unwrap();
    let content = serde_json::to_value(&evidence).unwrap();
    let hash = content_fingerprint(&astra_core::canonical_json_string(&content));
    let mut request = MaterializationReceiptRequest {
        trial_id: binding.trial_id.clone(),
        session_id: session.clone(),
        envelope: envelope.clone(),
        component_kind: MaterializationComponentKind::Workspace,
        component_snapshot_ref: Some(format!("artifact://{}", evidence.artifact_id())),
        component_base_snapshot_ref: None,
        component_content_fingerprint: Some(hash.clone()),
        outcome: MaterializationOutcome::Available,
        failure_code: None,
        expires_at: Some(chrono::Utc::now() + chrono::Duration::minutes(5)),
        idempotency_key: "allocation-receipt".into(),
    };
    assert!(matches!(
        receipts.record_receipt(&trusted, &request).await,
        Err(MaterializationReceiptError::Conflict(_))
    ));
    let persisted = evidence.persist(&pool).await.unwrap();
    assert_eq!(persisted, (evidence.artifact_id(), hash));
    assert_eq!(evidence.persist(&pool).await.unwrap(), persisted);
    let workspace = receipts.record_receipt(&trusted, &request).await.unwrap();
    let mut ids = vec![workspace.receipt_id];
    for (component, hash) in [
        (MaterializationComponentKind::Context, "sha256:context"),
        (MaterializationComponentKind::Policy, "sha256:tools"),
    ] {
        request.component_kind = component;
        request.component_snapshot_ref = Some(format!("evaluation://{}", component.as_str()));
        request.component_content_fingerprint = Some(hash.into());
        request.idempotency_key = format!("allocation-{}", component.as_str());
        ids.push(
            receipts
                .record_receipt(&trusted, &request)
                .await
                .unwrap()
                .receipt_id,
        );
    }
    let validated = receipts
        .validate_receipts_for_execution(
            &owner,
            &binding.trial_id,
            &session,
            &experiment.spec,
            &envelope,
            &ids,
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    assert_eq!(validated.workspace.as_ref(), Some(&evidence));
    // Expiry after receipt issuance must prevent execution with that same receipt set.
    sqlx::query("UPDATE session_artifacts SET status = 'expired' WHERE user_id = ? AND session_id = ? AND artifact_id = ?")
        .bind(&owner).bind(&session).bind(evidence.artifact_id()).execute(pool.get()).await.unwrap();
    assert!(matches!(
        receipts
            .validate_receipts_for_execution(
                &owner,
                &binding.trial_id,
                &session,
                &experiment.spec,
                &envelope,
                &ids,
                chrono::Utc::now()
            )
            .await,
        Err(MaterializationReceiptError::Conflict(_))
    ));
    // Even a well-formed replacement artifact cannot reuse the admitted content hash.
    let mut changed = evidence.clone();
    changed.allocation.run_id = "different-run".into();
    assert!(
        changed
            .validate_binding(&binding, &experiment.spec)
            .is_err()
    );
    sqlx::query("UPDATE session_artifacts SET status = 'active', content_json = ? WHERE user_id = ? AND session_id = ? AND artifact_id = ?")
        .bind(serde_json::to_string(&changed).unwrap()).bind(&owner).bind(&session).bind(evidence.artifact_id()).execute(pool.get()).await.unwrap();
    assert!(matches!(
        receipts
            .validate_receipts_for_execution(
                &owner,
                &binding.trial_id,
                &session,
                &experiment.spec,
                &envelope,
                &ids,
                chrono::Utc::now()
            )
            .await,
        Err(MaterializationReceiptError::Conflict(_))
    ));
    request.component_kind = MaterializationComponentKind::Workspace;
    request.component_snapshot_ref = Some(format!("artifact://{}", evidence.artifact_id()));
    request.component_content_fingerprint = Some(content_fingerprint(
        &astra_core::canonical_json_string(&serde_json::to_value(&changed).unwrap()),
    ));
    request.idempotency_key = "changed-allocation".into();
    assert!(matches!(
        receipts.record_receipt(&trusted, &request).await,
        Err(MaterializationReceiptError::Conflict(_))
    ));
    assert!(
        evidence.persist(&pool).await.is_err(),
        "retry cannot replace changed evidence"
    );
}

#[tokio::test]
#[ignore = "requires live MatrixOne (ASTRA_TEST_DB_IT=1)"]
async fn evaluation_delete_is_owner_scoped_rejects_live_runs_and_releases_retention() {
    let pool = common::setup_pool().await;
    let owner = format!("eval-release-{}", Uuid::new_v4());
    let experiment = spec(&format!("release-{}", Uuid::new_v4()));
    let plans = DatabaseEvaluationPlanStore::new(pool.clone());
    plans
        .register_experiment(&owner, &experiment, "release")
        .await
        .unwrap();
    assert!(matches!(
        plans
            .delete_experiment("other-owner", &experiment.experiment_id)
            .await,
        Err(EvaluationPersistenceError::NotFound(_))
    ));
    let trial = plans
        .list_trials(&owner, &experiment.experiment_id)
        .await
        .unwrap()
        .remove(0);
    let session = insert_session(&pool, &owner).await;
    let mut run = evaluation_run_record(&owner, &session, &experiment, &trial.trial);
    run.status = "running".into();
    let runs = DatabaseRunStateStore::new(pool.clone());
    runs.claim_run_start(run.clone(), Some(&session))
        .await
        .unwrap();
    assert!(matches!(
        plans
            .delete_experiment(&owner, &experiment.experiment_id)
            .await,
        Err(EvaluationPersistenceError::Conflict(_))
    ));
    assert!(
        plans
            .session_has_bound_trial(&owner, &session)
            .await
            .unwrap()
    );
    sqlx::query("UPDATE agent_runs SET status = 'failed' WHERE user_id = ? AND run_id = ?")
        .bind(&owner)
        .bind(&run.run_id)
        .execute(pool.get())
        .await
        .unwrap();
    plans
        .delete_experiment(&owner, &experiment.experiment_id)
        .await
        .unwrap();
    assert!(
        !plans
            .session_has_bound_trial(&owner, &session)
            .await
            .unwrap()
    );
    for table in [
        "evaluation_experiments",
        "evaluation_trial_bindings",
        "evaluation_trial_observations",
        "evaluation_task_assessments",
        "evaluation_materialization_receipts",
    ] {
        let count: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {table} WHERE owner_user_id = ?"
        ))
        .bind(&owner)
        .fetch_one(pool.get())
        .await
        .unwrap();
        assert_eq!(count, 0, "{table}");
    }
    use astra_services::{DatabaseSessionService, SessionService};
    DatabaseSessionService::new(astra_core::MatrixOneSettings::default())
        .with_pool(pool.clone())
        .delete_session(session.clone(), owner.clone())
        .await
        .expect("released Session can be deleted through the normal lifecycle");
    // Planned comparisons need no Run or observation to be disposable.
    plans
        .register_experiment(&owner, &experiment, "release")
        .await
        .unwrap();
    plans
        .delete_experiment(&owner, &experiment.experiment_id)
        .await
        .unwrap();
    let new_session = insert_session(&pool, &owner).await;
    let stale_start = evaluation_run_record(&owner, &new_session, &experiment, &trial.trial);
    assert!(
        runs.claim_run_start(stale_start, Some(&new_session))
            .await
            .is_err(),
        "a stale start cannot recreate a binding after deletion"
    );
    cleanup(&pool, &owner).await;
}
