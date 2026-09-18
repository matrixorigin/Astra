//! Owner-scoped Eval registration/binding checks against live MatrixOne.
//!
//! ```text
//! ASTRA_TEST_DB_IT=1 cargo test -p astra-services --test evaluation_durable_db_it -- --ignored --test-threads=1
//! ```

mod common;

use astra_core::SharedPool;
use astra_core::composite_snapshot::CompositeSnapshot;
use astra_services::{
    ComparisonArm, DataIsolation, DatabaseEvaluationPlanStore, DatabaseMaterializationReceiptStore,
    EvaluationBudget, EvaluationCase, EvaluationPersistenceError, EvaluationTarget,
    EvaluationTargetKind, ExperimentSpec, FrozenConditions, MaterializationComponentKind,
    MaterializationOutcome, MaterializationReceiptError, MaterializationReceiptRequest,
    MaterializationValidationError, MemoryIsolation, RevisionRef, SnapshotEnvelope, TrialOrder,
    TrustedMaterializerContext, validate_receipt_set,
};
use uuid::Uuid;

fn spec(experiment_id: &str) -> ExperimentSpec {
    ExperimentSpec {
        schema_version: 1,
        experiment_id: experiment_id.to_string(),
        target: EvaluationTarget {
            kind: EvaluationTargetKind::Skill,
            baseline: RevisionRef {
                revision_id: "skill-v1".to_string(),
                content_hash: "sha256:baseline".to_string(),
            },
            candidate: RevisionRef {
                revision_id: "skill-v2".to_string(),
                content_hash: "sha256:candidate".to_string(),
            },
        },
        cases: vec![EvaluationCase {
            case_id: "case-a".to_string(),
            input_snapshot_ref: "input-snapshot-a".to_string(),
            input_content_hash: "sha256:input-a".to_string(),
            verifier_id: "verifier-a".to_string(),
            verifier_version: "1".to_string(),
            holdout: false,
        }],
        repetitions: 1,
        order: TrialOrder::BaselineFirst,
        conditions: FrozenConditions {
            isolation_profile: "prompt_only_private".to_string(),
            model_binding: "model-v1".to_string(),
            provider_binding: "provider-v1".to_string(),
            context_snapshot_hash: "sha256:context".to_string(),
            tool_policy_hash: "sha256:tools".to_string(),
            cache_policy: "provider_default_recorded".to_string(),
            memory_isolation: MemoryIsolation::Disabled,
            data_isolation: DataIsolation::Disabled,
        },
        budget: EvaluationBudget {
            max_trials: 2,
            max_concurrency: 2,
            max_wall_time_secs: 300,
        },
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

async fn cleanup(pool: &SharedPool, owner: &str) {
    for (table, column) in [
        ("evaluation_materialization_receipts", "owner_user_id"),
        ("evaluation_trial_bindings", "owner_user_id"),
        ("evaluation_experiments", "owner_user_id"),
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
    let run = insert_run(&pool, &owner, &session).await;
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
    assert_eq!(admitted.len(), 2);
    assert!(matches!(
        validate_receipt_set(&binding, &experiment.spec, &envelope, &[first.clone()], now,),
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
    let experiment_id = format!("eval-bind-{}", Uuid::new_v4().simple());
    let record = store
        .register_experiment(&owner, &spec(&experiment_id), "submit-bind")
        .await
        .expect("register binding plan");
    let trials = store
        .list_trials(&owner, &record.experiment_id)
        .await
        .expect("list planned trials");
    assert!(matches!(trials[0].trial.arm, ComparisonArm::Baseline));

    // The persisted JSON is not trusted merely because its row count is
    // correct: changing the arm must fail the canonical-plan comparison.
    let mut tampered_trial = trials[0].trial.clone();
    tampered_trial.arm = ComparisonArm::Candidate;
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

    let session_a = insert_session(&pool, &owner).await;
    let session_b = insert_session(&pool, &owner).await;
    let run_a = insert_run(&pool, &owner, &session_a).await;
    let run_b = insert_run(&pool, &owner, &session_b).await;
    let other_session = insert_session(&pool, &other_owner).await;
    let other_run = insert_run(&pool, &other_owner, &other_session).await;

    let first_store = store.clone();
    let second_store = store.clone();
    let (first, second) = tokio::join!(
        first_store.bind_trial_run(&owner, &trials[0].trial_id, &session_a, &run_a),
        second_store.bind_trial_run(&owner, &trials[1].trial_id, &session_b, &run_b),
    );
    let first = first.expect("first session binding succeeds");
    let second = second.expect("second session binding succeeds concurrently");
    assert_eq!(first.binding_status, "bound");
    assert_eq!(second.binding_status, "bound");
    assert_eq!(first.session_id.as_deref(), Some(session_a.as_str()));
    assert_eq!(second.session_id.as_deref(), Some(session_b.as_str()));

    assert_eq!(
        store
            .bind_trial_run(&owner, &trials[0].trial_id, &session_a, &run_a)
            .await
            .expect("same binding is idempotent"),
        first
    );
    assert!(matches!(
        store
            .bind_trial_run(&owner, &trials[0].trial_id, &session_b, &run_b)
            .await,
        Err(EvaluationPersistenceError::Conflict(_))
    ));
    assert!(matches!(
        store
            .bind_trial_run(
                &other_owner,
                &trials[0].trial_id,
                &other_session,
                &other_run,
            )
            .await,
        Err(EvaluationPersistenceError::NotFound(_))
    ));

    // A canonical Run is a single execution identity. It cannot be reused by
    // another trial, even across experiments, and concurrent contenders are
    // resolved by the database unique key rather than by process-local state.
    let reuse_experiment_id = format!("eval-bind-reuse-{}", Uuid::new_v4().simple());
    let mut reuse_spec = spec(&reuse_experiment_id);
    reuse_spec.repetitions = 2;
    reuse_spec.budget.max_trials = 4;
    let reuse_record = store
        .register_experiment(&owner, &reuse_spec, "submit-bind-reuse")
        .await
        .expect("register run-reuse plan");
    let reuse_trials = store
        .list_trials(&owner, &reuse_record.experiment_id)
        .await
        .expect("list run-reuse trials");
    assert!(matches!(
        store
            .bind_trial_run(&owner, &reuse_trials[0].trial_id, &session_a, &run_a,)
            .await,
        Err(EvaluationPersistenceError::Conflict(_))
    ));

    let reuse_session = insert_session(&pool, &owner).await;
    let reuse_run = insert_run(&pool, &owner, &reuse_session).await;
    let reuse_left_store = store.clone();
    let reuse_right_store = store.clone();
    let (reuse_left, reuse_right) = tokio::join!(
        reuse_left_store.bind_trial_run(
            &owner,
            &reuse_trials[0].trial_id,
            &reuse_session,
            &reuse_run,
        ),
        reuse_right_store.bind_trial_run(
            &owner,
            &reuse_trials[1].trial_id,
            &reuse_session,
            &reuse_run,
        ),
    );
    assert_eq!(
        [reuse_left.is_ok(), reuse_right.is_ok()]
            .into_iter()
            .filter(|bound| *bound)
            .count(),
        1,
        "exactly one trial may own a run"
    );
    assert!(
        matches!(reuse_left, Err(EvaluationPersistenceError::Conflict(_)))
            || matches!(reuse_right, Err(EvaluationPersistenceError::Conflict(_))),
        "the losing contender must receive a binding conflict"
    );

    cleanup(&pool, &owner).await;
    cleanup(&pool, &other_owner).await;
}
