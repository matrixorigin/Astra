//! Live MatrixOne coverage for the durable inference execution boundary.
//!
//! Run with:
//! ASTRA_TEST_DB_IT=1 cargo test -p astra-services \
//!   --test inference_execution_db_it -- --ignored --test-threads=1

mod common;

use astra_services::runtime_maintenance::{RuntimeMaintenancePolicy, maintain_runtime_storage};
use astra_services::{
    InferenceInvocationAdmissionResolution, InferenceInvocationInput, InferenceInvocationPlan,
    InferenceInvocationTerminal, InferenceProviderAttemptPlan, InferenceProviderDeliveryState,
    InferenceProviderWireIdentity, InferenceRunAdmissionAuthority, InferenceTerminalStatus,
    InferenceUsage, InferenceUsageStatus, ModelAccessKind, ModelExecutionPlacement,
    ServiceErrorKind, admit_inference_invocation,
    admit_inference_invocation_with_first_provider_attempt, begin_inference_provider_attempt,
    declare_inference_attempt_settlement, declare_inference_settlement,
    finish_inference_invocation, finish_inference_provider_attempt,
    load_inference_canonical_transitions_for_session, next_inference_logical_attempt_pair_base,
    plan_inference_invocation, plan_inference_provider_attempt, reconcile_inference_settlements,
    renew_inference_invocation_owner, retire_inference_canonical_transitions_through_turn,
    settle_uncertain_inference_admission,
};
use astra_turn_types::{InferenceInvocationScope, InferencePurpose};
use serial_test::serial;
use sha2::Digest;
use sqlx::Row;
use uuid::Uuid;

const TEST_INFERENCE_OWNER_POD_ID: &str = "inference-db-it-owner";

fn run_authority() -> Option<InferenceRunAdmissionAuthority> {
    Some(InferenceRunAdmissionAuthority {
        expected_owner_generation: 0,
        expected_owner_pod_id: TEST_INFERENCE_OWNER_POD_ID.to_string(),
        expected_control_epoch: -1,
    })
}

fn run_input(
    user_id: &str,
    session_id: &str,
    run_id: &str,
    round: u32,
    operation_id: &str,
) -> InferenceInvocationInput {
    InferenceInvocationInput {
        user_id: user_id.to_string(),
        scope: InferenceInvocationScope::Run {
            session_id: session_id.to_string(),
            run_id: run_id.to_string(),
            turn: 1,
            round,
            operation_id: operation_id.to_string(),
            logical_attempt: 0,
        },
        offering_id: "owner-lease-offering".to_string(),
        resolved_model_name: "owner-lease-model".to_string(),
        upstream_model_name: "owner-lease-model".to_string(),
        provider: "openai".to_string(),
        purpose: InferencePurpose::PrimaryAgent,
        execution_placement: ModelExecutionPlacement::Server,
        access_kind: ModelAccessKind::SelfHosted,
        run_authority: run_authority(),
    }
}

fn provider_attempt(
    plan: &InferenceInvocationPlan,
    attempt_index: u32,
) -> InferenceProviderAttemptPlan {
    plan_inference_provider_attempt(
        plan,
        attempt_index,
        InferenceProviderWireIdentity::new(
            "openai_compatible",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            2,
        )
        .expect("test provider wire identity"),
    )
}

fn canonical_authority(label: &str) -> serde_json::Value {
    let content = astra_turn_types::render_append_only_runtime_authority_frame(
        "test_authority",
        astra_turn_types::RuntimeAuthorityLifetime::NextAssistantDecision,
        label,
    )
    .expect("render canonical authority frame");
    let mut message = serde_json::json!({"role": "user", "content": content});
    astra_turn_types::mark_append_only_required_context(
        &mut message,
        "test_authority",
        astra_turn_types::RuntimeAuthorityLifetime::NextAssistantDecision,
    );
    message
}

async fn commit_canonical_transition(
    shared_pool: &astra_core::SharedPool,
    user_id: &str,
    session_id: &str,
    run_id: &str,
    round: u32,
    operation_id: &str,
    transition: &astra_turn_types::ProviderCanonicalTransitionV2,
) {
    let plan =
        plan_inference_invocation(run_input(user_id, session_id, run_id, round, operation_id))
            .expect("plan canonical transition invocation");
    admit_inference_invocation(shared_pool, &plan)
        .await
        .expect("admit canonical transition invocation");
    let attempt = provider_attempt(&plan, 0)
        .with_canonical_transitions(std::slice::from_ref(transition))
        .expect("bind canonical WAL entry");
    begin_inference_provider_attempt(shared_pool, &attempt)
        .await
        .expect("commit canonical WAL entry with provider admission");
    let terminal = InferenceInvocationTerminal {
        status: InferenceTerminalStatus::Cancelled,
        usage: InferenceUsage::default(),
        usage_status: InferenceUsageStatus::Unavailable,
        provider_response_id: None,
        error_kind: Some("test_complete".to_string()),
        error_message: Some("close canonical WAL test attempt".to_string()),
    };
    finish_inference_provider_attempt(shared_pool, &attempt, &terminal)
        .await
        .expect("finish canonical WAL provider attempt");
    finish_inference_invocation(shared_pool, &plan, &terminal)
        .await
        .expect("finish canonical WAL invocation");
}

async fn seed_run(pool: &sqlx::Pool<sqlx::MySql>, user_id: &str, session_id: &str, run_id: &str) {
    sqlx::query(
        "INSERT INTO agent_sessions
         (session_id, user_id, status, event_count, project_retention_policy,
          created_at, updated_at, last_active_at)
         VALUES (?, ?, 'active', 0, 'session', NOW(6), NOW(6), NOW(6))",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(pool)
    .await
    .expect("seed inference session");
    sqlx::query(
        "INSERT INTO agent_runs
         (run_id, user_id, session_id, root_run_id, ancestor_path, depth, retry_scope,
          status, execution_mode, owner_pod_id, owner_lease_expires_at,
          run_generation, last_event_idx, retry_count,
          total_prompt_tokens, total_completion_tokens, total_tool_calls,
          created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, 0, 'node', 'running', 'web_agent', ?,
                 TIMESTAMPADD(MINUTE, 5, NOW(6)), 0, -1, 0,
                 0, 0, 0, NOW(6), NOW(6))",
    )
    .bind(run_id)
    .bind(user_id)
    .bind(session_id)
    .bind(run_id)
    .bind(run_id)
    .bind(TEST_INFERENCE_OWNER_POD_ID)
    .execute(pool)
    .await
    .expect("seed inference run");
}

async fn append_run_control_event(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    session_id: &str,
    run_id: &str,
    event_index: i64,
    event_type: &str,
) {
    let mut tx = pool.begin().await.expect("begin inference control event");
    let updated = sqlx::query(
        "UPDATE agent_runs SET last_event_idx = ?
         WHERE user_id = ? AND run_id = ? AND last_event_idx = ?",
    )
    .bind(event_index)
    .bind(user_id)
    .bind(run_id)
    .bind(event_index - 1)
    .execute(&mut *tx)
    .await
    .expect("advance inference run event index");
    assert_eq!(updated.rows_affected(), 1);
    let event_id = format!("{event_type}-{event_index}-{run_id}");
    let payload = serde_json::json!({
        "event_type": event_type,
        "idempotency_key": event_id,
        "data": {},
    });
    sqlx::query(
        "INSERT INTO agent_run_events
         (id, run_id, event_idx, user_id, session_id, event_type, event_id,
          idempotency_key, event_hash, producer_pod_id, payload_json, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6))",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(run_id)
    .bind(event_index)
    .bind(user_id)
    .bind(session_id)
    .bind(event_type)
    .bind(&event_id)
    .bind(&event_id)
    .bind(format!("{:064x}", event_index + 1))
    .bind(TEST_INFERENCE_OWNER_POD_ID)
    .bind(payload.to_string())
    .execute(&mut *tx)
    .await
    .expect("insert inference control event");
    tx.commit().await.expect("commit inference control event");
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn uncertain_admission_recovery_is_scope_fenced_and_atomic() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("admission-recovery-user-{suffix}");
    let session_id = format!("admission-recovery-session-{suffix}");
    let run_id = format!("admission-recovery-run-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;
    let input = InferenceInvocationInput {
        user_id: user_id.clone(),
        scope: InferenceInvocationScope::Run {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            turn: 1,
            round: 1,
            operation_id: "uncertain_logical_admission".to_string(),
            logical_attempt: 0,
        },
        offering_id: "admission-recovery-offering".to_string(),
        resolved_model_name: "admission-recovery-model".to_string(),
        upstream_model_name: "admission-recovery-model".to_string(),
        provider: "openai".to_string(),
        purpose: InferencePurpose::PrimaryAgent,
        execution_placement: ModelExecutionPlacement::Server,
        access_kind: ModelAccessKind::SelfHosted,
        run_authority: run_authority(),
    };
    let plan = plan_inference_invocation(input.clone()).expect("plan uncertain admission");
    let terminal = InferenceInvocationTerminal {
        status: InferenceTerminalStatus::Cancelled,
        usage: InferenceUsage::default(),
        usage_status: InferenceUsageStatus::Unavailable,
        provider_response_id: None,
        error_kind: Some("cancelled".to_string()),
        error_message: Some("provider delivery was never authorized".to_string()),
    };

    let mut scope_owner = pool.begin().await.expect("begin scope lock");
    sqlx::query(
        "SELECT 1 FROM agent_sessions
         WHERE user_id = ? AND session_id = ? LIMIT 1 FOR UPDATE",
    )
    .bind(&user_id)
    .bind(&session_id)
    .fetch_one(&mut *scope_owner)
    .await
    .expect("hold the same first scope lock as admission");
    let recovery_pool = shared_pool.clone();
    let recovery_plan = plan.clone();
    let recovery_terminal = terminal.clone();
    let recovery = tokio::spawn(async move {
        settle_uncertain_inference_admission(&recovery_pool, &recovery_plan, &recovery_terminal)
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        !recovery.is_finished(),
        "recovery must wait behind the canonical scope transaction lock"
    );
    scope_owner.rollback().await.expect("release scope lock");
    assert_eq!(
        recovery
            .await
            .expect("join recovery")
            .expect("recover rollback"),
        InferenceInvocationAdmissionResolution::Settled
    );

    let atomic = sqlx::query(
        "SELECT invocation.status,
                (SELECT COUNT(*) FROM inference_invocation_settlement_debts AS debt
                 WHERE debt.user_id = invocation.user_id
                   AND debt.invocation_id = invocation.invocation_id
                   AND debt.terminal_status = 'cancelled') AS debt_count
         FROM inference_invocations AS invocation
         WHERE invocation.user_id = ? AND invocation.invocation_id = ?",
    )
    .bind(&user_id)
    .bind(plan.invocation_id())
    .fetch_one(pool)
    .await
    .expect("load atomic recovery pair");
    assert_eq!(atomic.get::<String, _>("status"), "admitted");
    assert_eq!(atomic.get::<i64, _>("debt_count"), 1);

    let initial_delivery_state = sqlx::query_scalar::<_, String>(
        "SELECT provider_delivery_state
         FROM inference_invocation_settlement_debts
         WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(plan.invocation_id())
    .fetch_one(pool)
    .await
    .expect("load atomic recovery delivery authority");
    assert_eq!(initial_delivery_state, "pre_delivery");
    declare_inference_settlement(&shared_pool, &plan, &terminal)
        .await
        .expect("generic terminal replay must preserve stronger delivery authority");
    let replayed_delivery_state = sqlx::query_scalar::<_, String>(
        "SELECT provider_delivery_state
         FROM inference_invocation_settlement_debts
         WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(plan.invocation_id())
    .fetch_one(pool)
    .await
    .expect("load replayed recovery delivery authority");
    assert_eq!(replayed_delivery_state, "pre_delivery");

    let mut unknown_input = input.clone();
    unknown_input.scope = InferenceInvocationScope::Run {
        session_id: session_id.clone(),
        run_id: run_id.clone(),
        turn: 1,
        round: 2,
        operation_id: "unknown_then_pre_delivery".to_string(),
        logical_attempt: 0,
    };
    let unknown_plan =
        plan_inference_invocation(unknown_input).expect("plan unknown delivery settlement");
    admit_inference_invocation(&shared_pool, &unknown_plan)
        .await
        .expect("admit unknown delivery settlement");
    declare_inference_settlement(&shared_pool, &unknown_plan, &terminal)
        .await
        .expect("declare generic logical settlement");
    let unknown_delivery_state = sqlx::query_scalar::<_, String>(
        "SELECT provider_delivery_state
         FROM inference_invocation_settlement_debts
         WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(unknown_plan.invocation_id())
    .fetch_one(pool)
    .await
    .expect("load generic delivery authority");
    assert_eq!(unknown_delivery_state, "unknown");
    assert_eq!(
        settle_uncertain_inference_admission(&shared_pool, &unknown_plan, &terminal)
            .await
            .expect("strengthen generic debt to pre-delivery"),
        InferenceInvocationAdmissionResolution::Settled
    );
    let strengthened_delivery_state = sqlx::query_scalar::<_, String>(
        "SELECT provider_delivery_state
         FROM inference_invocation_settlement_debts
         WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(unknown_plan.invocation_id())
    .fetch_one(pool)
    .await
    .expect("load strengthened delivery authority");
    assert_eq!(strengthened_delivery_state, "pre_delivery");

    let conflicting = plan_inference_invocation(input).expect("plan competing token");
    assert_eq!(conflicting.invocation_id(), plan.invocation_id());
    assert_eq!(
        settle_uncertain_inference_admission(&shared_pool, &conflicting, &terminal)
            .await
            .expect("classify competing admission owner"),
        InferenceInvocationAdmissionResolution::ConflictingIdentity
    );
    let debt_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM inference_invocation_settlement_debts
         WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(plan.invocation_id())
    .fetch_one(pool)
    .await
    .expect("conflict must not duplicate or replace debt");
    assert_eq!(debt_count, 1);

    reconcile_inference_settlements(&shared_pool, 8)
        .await
        .expect("restart-style sweeper converges the atomic debt");
    let status = sqlx::query_scalar::<_, String>(
        "SELECT status FROM inference_invocations WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(plan.invocation_id())
    .fetch_one(pool)
    .await
    .expect("load recovered terminal");
    assert_eq!(status, "cancelled");
    let strengthened_status = sqlx::query_scalar::<_, String>(
        "SELECT status FROM inference_invocations WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(unknown_plan.invocation_id())
    .fetch_one(pool)
    .await
    .expect("load strengthened terminal");
    assert_eq!(strengthened_status, "cancelled");
    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn large_canonical_payload_does_not_change_provider_terminal_identity() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("canonical-large-user-{suffix}");
    let session_id = format!("canonical-large-session-{suffix}");
    let run_id = format!("canonical-large-run-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;

    let durable_base = astra_turn_types::CanonicalPrefixIdentityV1::from_messages(&[])
        .expect("empty durable base");
    let history = vec![serde_json::json!({
        "role": "user",
        "content": "x".repeat(128 * 1024),
    })];
    let transition = astra_turn_types::ProviderCanonicalTransitionV2::new_from_durable_base(
        None,
        durable_base,
        &history,
        Vec::new(),
    )
    .expect("construct a large canonical transition");
    let plan = plan_inference_invocation(run_input(
        &user_id,
        &session_id,
        &run_id,
        0,
        "large_canonical_terminal",
    ))
    .expect("plan large canonical invocation");
    admit_inference_invocation(&shared_pool, &plan)
        .await
        .expect("admit large canonical invocation");
    let attempt = provider_attempt(&plan, 0)
        .with_canonical_transitions(std::slice::from_ref(&transition))
        .expect("bind one large canonical transition");
    begin_inference_provider_attempt(&shared_pool, &attempt)
        .await
        .expect("admit large canonical provider attempt");
    let stored_bytes: i64 = sqlx::query_scalar(
        "SELECT OCTET_LENGTH(payload_json)
         FROM inference_canonical_transition_wal
         WHERE user_id = ? AND attempt_id = ?",
    )
    .bind(&user_id)
    .bind(attempt.attempt_id())
    .fetch_one(pool)
    .await
    .expect("measure persisted canonical payload");
    assert!(
        stored_bytes > 65_535,
        "the regression must cross MatrixOne's CAST AS CHAR truncation boundary"
    );
    let stored_payload: Vec<u8> = sqlx::query_scalar(
        "SELECT payload_json
         FROM inference_canonical_transition_wal
         WHERE user_id = ? AND attempt_id = ?",
    )
    .bind(&user_id)
    .bind(attempt.attempt_id())
    .fetch_one(pool)
    .await
    .expect("read complete persisted canonical payload bytes");
    assert_eq!(
        i64::try_from(stored_payload.len()).expect("stored payload length"),
        stored_bytes
    );
    let locally_rehashed_payload = format!("{:x}", sha2::Sha256::digest(&stored_payload));
    assert_eq!(
        Some(locally_rehashed_payload.as_str()),
        attempt.canonical_transition_hash()
    );
    let terminal = InferenceInvocationTerminal {
        status: InferenceTerminalStatus::Cancelled,
        usage: InferenceUsage::default(),
        usage_status: InferenceUsageStatus::Unavailable,
        provider_response_id: None,
        error_kind: Some("large_payload_complete".to_string()),
        error_message: Some("close large canonical payload attempt".to_string()),
    };
    finish_inference_provider_attempt(&shared_pool, &attempt, &terminal)
        .await
        .expect("terminal identity must not depend on reloading mutable WAL payload bytes");
    finish_inference_invocation(&shared_pool, &plan, &terminal)
        .await
        .expect("finish large canonical invocation");
    let receipts =
        load_inference_canonical_transitions_for_session(&shared_pool, &user_id, &session_id, 1)
            .await
            .expect("recover the complete large canonical payload");
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].transitions.len(), 1);
    let mut recovered = Vec::new();
    receipts[0].transitions[0]
        .apply_to(&mut recovered)
        .expect("materialize the large canonical transition");
    assert_eq!(recovered, history);
    let mut corrupted_payload = stored_payload;
    corrupted_payload.push(b' ');
    sqlx::query(
        "UPDATE inference_canonical_transition_wal
         SET payload_json = ?
         WHERE user_id = ? AND attempt_id = ?",
    )
    .bind(corrupted_payload)
    .bind(&user_id)
    .bind(attempt.attempt_id())
    .execute(pool)
    .await
    .expect("corrupt the head payload without changing immutable metadata");
    assert_eq!(
        load_inference_canonical_transitions_for_session(&shared_pool, &user_id, &session_id, 1,)
            .await
            .expect_err("recovery must fail closed on payload/hash drift")
            .kind,
        ServiceErrorKind::Conflict
    );
    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn superseded_payload_owner_can_terminalize_after_its_child_becomes_head() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("canonical-late-terminal-user-{suffix}");
    let session_id = format!("canonical-late-terminal-session-{suffix}");
    let run_id = format!("canonical-late-terminal-run-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;

    let durable_base = astra_turn_types::CanonicalPrefixIdentityV1::from_messages(&[])
        .expect("empty durable base");
    let parent_history = vec![serde_json::json!({"role": "user", "content": "parent"})];
    let parent_transition = astra_turn_types::ProviderCanonicalTransitionV2::new_from_durable_base(
        None,
        durable_base.clone(),
        &parent_history,
        Vec::new(),
    )
    .expect("construct parent transition");
    let parent_plan = plan_inference_invocation(run_input(
        &user_id,
        &session_id,
        &run_id,
        0,
        "late_terminal_parent",
    ))
    .expect("plan parent invocation");
    admit_inference_invocation(&shared_pool, &parent_plan)
        .await
        .expect("admit parent invocation");
    let parent_attempt = provider_attempt(&parent_plan, 0)
        .with_canonical_transitions(std::slice::from_ref(&parent_transition))
        .expect("bind parent transition");
    begin_inference_provider_attempt(&shared_pool, &parent_attempt)
        .await
        .expect("admit parent provider attempt");

    let child_message = serde_json::json!({"role": "assistant", "content": "child"});
    let child_authority = canonical_authority("continue after child");
    let child_transition = astra_turn_types::ProviderCanonicalTransitionV2::new_from_durable_base(
        Some(parent_transition.transition_id.clone()),
        durable_base,
        &parent_history,
        vec![child_message, child_authority],
    )
    .expect("construct child transition");
    let child_plan = plan_inference_invocation(run_input(
        &user_id,
        &session_id,
        &run_id,
        1,
        "late_terminal_child",
    ))
    .expect("plan child invocation");
    admit_inference_invocation(&shared_pool, &child_plan)
        .await
        .expect("admit child invocation");
    let child_attempt = provider_attempt(&child_plan, 0)
        .with_canonical_transitions(std::slice::from_ref(&child_transition))
        .expect("bind child transition");
    begin_inference_provider_attempt(&shared_pool, &child_attempt)
        .await
        .expect("atomically make child the canonical head");

    let terminal = InferenceInvocationTerminal {
        status: InferenceTerminalStatus::Cancelled,
        usage: InferenceUsage::default(),
        usage_status: InferenceUsageStatus::Unavailable,
        provider_response_id: None,
        error_kind: Some("late_terminal_complete".to_string()),
        error_message: Some("terminalize after successor admission".to_string()),
    };
    finish_inference_provider_attempt(&shared_pool, &parent_attempt, &terminal)
        .await
        .expect("late parent terminal must depend only on immutable attempt identity");
    let payload_owners: Vec<String> = sqlx::query_scalar(
        "SELECT attempt_id FROM inference_canonical_transition_wal
         WHERE user_id = ? AND session_id = ?
         ORDER BY created_at ASC, transition_id ASC",
    )
    .bind(&user_id)
    .bind(&session_id)
    .fetch_all(pool)
    .await
    .expect("load append-only canonical WAL owners");
    assert_eq!(
        payload_owners,
        vec![
            parent_attempt.attempt_id().to_string(),
            child_attempt.attempt_id().to_string()
        ]
    );

    finish_inference_provider_attempt(&shared_pool, &child_attempt, &terminal)
        .await
        .expect("finish child provider attempt");
    finish_inference_invocation(&shared_pool, &parent_plan, &terminal)
        .await
        .expect("finish parent invocation");
    finish_inference_invocation(&shared_pool, &child_plan, &terminal)
        .await
        .expect("finish child invocation");
    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn canonical_transition_wal_is_linear_and_recoverable_across_many_rounds() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("canonical-head-user-{suffix}");
    let session_id = format!("canonical-head-session-{suffix}");
    let run_id = format!("canonical-head-run-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;

    let durable_base = astra_turn_types::CanonicalPrefixIdentityV1::from_messages(&[])
        .expect("empty durable base");
    const ROUNDS: u32 = 300;
    let mut history = Vec::new();
    let mut parent_transition_id = None;
    let mut parent_result = None;
    let mut first_retry_attempt_id = None;
    for round in 0..ROUNDS {
        if round > 0 {
            history.push(serde_json::json!({
                "role": "assistant",
                "content": format!("provider response {round}")
            }));
        }
        let appended = canonical_authority(&format!("durable request {round}"));
        let transition = match (parent_transition_id.clone(), parent_result.clone()) {
            (None, None) => astra_turn_types::ProviderCanonicalTransitionV2::new_from_durable_base(
                None,
                durable_base.clone(),
                &history,
                vec![appended.clone()],
            ),
            (Some(parent_transition_id), Some(parent_result)) => {
                astra_turn_types::ProviderCanonicalTransitionV2::new_linked_from_durable_base(
                    parent_transition_id,
                    parent_result,
                    durable_base.clone(),
                    &history,
                    vec![appended.clone()],
                )
            }
            _ => unreachable!("parent transition identity is atomic"),
        }
        .expect("construct an explicitly linked canonical transition delta");
        let plan = plan_inference_invocation(run_input(
            &user_id,
            &session_id,
            &run_id,
            round,
            &format!("canonical_head_{round}"),
        ))
        .expect("plan canonical head invocation");
        let attempt = provider_attempt(&plan, 0)
            .with_canonical_transitions(std::slice::from_ref(&transition))
            .expect("bind one canonical transition");
        admit_inference_invocation_with_first_provider_attempt(&shared_pool, &plan, &attempt)
            .await
            .expect("atomically admit invocation, attempt, and canonical WAL entry");
        let terminal = InferenceInvocationTerminal {
            status: InferenceTerminalStatus::Cancelled,
            usage: InferenceUsage::default(),
            usage_status: InferenceUsageStatus::Unavailable,
            provider_response_id: None,
            error_kind: Some("test_round_complete".to_string()),
            error_message: Some("close scale-test provider attempt".to_string()),
        };
        finish_inference_provider_attempt(&shared_pool, &attempt, &terminal)
            .await
            .expect("finish canonical head attempt");
        if round == 0 {
            let retry = provider_attempt(&plan, 1)
                .with_canonical_transitions(std::slice::from_ref(&transition))
                .expect("bind the same transition to its physical retry");
            begin_inference_provider_attempt(&shared_pool, &retry)
                .await
                .expect("same-id physical retry moves the unique payload owner");
            first_retry_attempt_id = Some(retry.attempt_id().to_string());
            finish_inference_provider_attempt(&shared_pool, &retry, &terminal)
                .await
                .expect("finish same-id physical retry");
        }
        finish_inference_invocation(&shared_pool, &plan, &terminal)
            .await
            .expect("finish canonical head invocation");
        parent_transition_id = Some(transition.transition_id);
        parent_result = Some(transition.result);
        history.push(appended);
    }

    let stale = astra_turn_types::ProviderCanonicalTransitionV2::new_from_durable_base(
        Some("0".repeat(64)),
        durable_base.clone(),
        &history,
        Vec::new(),
    )
    .expect("construct a structurally valid but causally stale transition");
    let stale_plan = plan_inference_invocation(run_input(
        &user_id,
        &session_id,
        &run_id,
        ROUNDS,
        "canonical_head_stale_parent",
    ))
    .expect("plan stale-parent invocation");
    admit_inference_invocation(&shared_pool, &stale_plan)
        .await
        .expect("admit stale-parent logical invocation");
    let stale_attempt = provider_attempt(&stale_plan, 0)
        .with_canonical_transitions(std::slice::from_ref(&stale))
        .expect("bind stale-parent transition");
    assert_eq!(
        begin_inference_provider_attempt(&shared_pool, &stale_attempt)
            .await
            .expect_err("a stale parent must fail before provider delivery")
            .kind,
        ServiceErrorKind::Conflict
    );
    let stale_attempts: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM inference_provider_attempts
         WHERE user_id = ? AND attempt_id = ?",
    )
    .bind(&user_id)
    .bind(stale_attempt.attempt_id())
    .fetch_one(pool)
    .await
    .expect("count rolled-back stale attempt");
    assert_eq!(stale_attempts, 0);
    let stale_terminal = InferenceInvocationTerminal {
        status: InferenceTerminalStatus::Cancelled,
        usage: InferenceUsage::default(),
        usage_status: InferenceUsageStatus::Unavailable,
        provider_response_id: None,
        error_kind: Some("stale_parent_rejected".to_string()),
        error_message: Some("canonical head CAS rejected stale parent".to_string()),
    };
    finish_inference_invocation(&shared_pool, &stale_plan, &stale_terminal)
        .await
        .expect("close pre-delivery stale-parent invocation");

    let wal_payload_bytes: Vec<i64> = sqlx::query_scalar(
        "SELECT payload_bytes
         FROM inference_canonical_transition_wal
         WHERE user_id = ? AND session_id = ? AND turn_index = 1
         ORDER BY created_at ASC, transition_id ASC",
    )
    .bind(&user_id)
    .bind(&session_id)
    .fetch_all(pool)
    .await
    .expect("measure bounded canonical WAL");
    assert_eq!(wal_payload_bytes.len(), ROUNDS as usize);
    let minimum = wal_payload_bytes.iter().skip(1).copied().min().unwrap();
    let maximum = wal_payload_bytes.iter().skip(1).copied().max().unwrap();
    let total = wal_payload_bytes.iter().copied().sum::<i64>();
    assert!(
        maximum <= minimum + 128,
        "each linked entry must stay proportional to its own append"
    );
    assert!(
        total <= i64::from(ROUNDS) * 2_048,
        "total WAL bytes must grow linearly with the number of fixed-size appends"
    );
    let attempts: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM inference_provider_attempts
         WHERE user_id = ? AND session_id = ? AND canonical_transition_id IS NOT NULL",
    )
    .bind(&user_id)
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("count canonical audit rows");
    assert_eq!(attempts, i64::from(ROUNDS) + 1);
    let root_owner = sqlx::query(
        "SELECT attempt_id, physical_attempt
         FROM inference_canonical_transition_wal
         WHERE user_id = ? AND session_id = ? AND turn_index = 1
           AND parent_transition_id IS NULL",
    )
    .bind(&user_id)
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("load the exact retry-owned WAL anchor");
    assert_eq!(
        root_owner.get::<String, _>("attempt_id"),
        first_retry_attempt_id.expect("first round has an exact retry")
    );
    assert_eq!(root_owner.get::<i64, _>("physical_attempt"), 1);

    let receipts =
        load_inference_canonical_transitions_for_session(&shared_pool, &user_id, &session_id, 1)
            .await
            .expect("load the canonical WAL chain");
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].transitions.len(), ROUNDS as usize);
    assert_eq!(
        receipts[0]
            .transitions
            .last()
            .expect("non-empty chain")
            .transition_id,
        parent_transition_id.expect("final transition id")
    );
    let mut recovered = Vec::new();
    for transition in &receipts[0].transitions {
        transition
            .apply_to(&mut recovered)
            .expect("materialize the linked WAL chain");
    }
    assert_eq!(recovered, history);

    retire_inference_canonical_transitions_through_turn(&shared_pool, &user_id, &session_id, 1)
        .await
        .expect("retire canonical head and payload together");
    let heads: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM inference_canonical_transition_heads
         WHERE user_id = ? AND session_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("count retired canonical heads");
    assert_eq!(heads, 0);
    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn canonical_transition_wal_is_isolated_by_owner_and_session() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_a = format!("canonical-owner-a-{suffix}");
    let user_b = format!("canonical-owner-b-{suffix}");
    let shared_session = format!("canonical-shared-session-{suffix}");
    let session_a2 = format!("canonical-session-a2-{suffix}");
    let run_a = format!("canonical-run-a-{suffix}");
    let run_b = format!("canonical-run-b-{suffix}");
    let run_a2 = format!("canonical-run-a2-{suffix}");
    seed_run(pool, &user_a, &shared_session, &run_a).await;
    seed_run(pool, &user_b, &shared_session, &run_b).await;
    seed_run(pool, &user_a, &session_a2, &run_a2).await;

    let durable_base = astra_turn_types::CanonicalPrefixIdentityV1::from_messages(&[])
        .expect("empty durable base");
    for (scope, user_id, session_id, run_id) in [
        ("owner-a-shared", &user_a, &shared_session, &run_a),
        ("owner-b-shared", &user_b, &shared_session, &run_b),
        ("owner-a-second", &user_a, &session_a2, &run_a2),
    ] {
        let mut history = vec![serde_json::json!({"role": "user", "content": scope})];
        let first = astra_turn_types::ProviderCanonicalTransitionV2::new_from_durable_base(
            None,
            durable_base.clone(),
            &history,
            vec![canonical_authority(&format!("{scope}-first"))],
        )
        .expect("construct owner-scoped WAL anchor");
        commit_canonical_transition(
            &shared_pool,
            user_id,
            session_id,
            run_id,
            0,
            &format!("{scope}-first"),
            &first,
        )
        .await;
        history.extend(first.appended_messages.iter().cloned());
        let second = astra_turn_types::ProviderCanonicalTransitionV2::new_from_durable_base(
            Some(first.transition_id.clone()),
            durable_base.clone(),
            &history,
            vec![canonical_authority(&format!("{scope}-second"))],
        )
        .expect("construct owner-scoped WAL successor");
        commit_canonical_transition(
            &shared_pool,
            user_id,
            session_id,
            run_id,
            1,
            &format!("{scope}-second"),
            &second,
        )
        .await;
    }

    for (scope, user_id, session_id) in [
        ("owner-a-shared", &user_a, &shared_session),
        ("owner-b-shared", &user_b, &shared_session),
        ("owner-a-second", &user_a, &session_a2),
    ] {
        let receipts =
            load_inference_canonical_transitions_for_session(&shared_pool, user_id, session_id, 1)
                .await
                .expect("load only the requested owner/session chain");
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].transitions.len(), 2);
        let mut recovered = Vec::new();
        for transition in &receipts[0].transitions {
            transition
                .apply_to(&mut recovered)
                .expect("replay isolated owner/session chain");
        }
        assert_eq!(recovered[0]["content"], scope);
    }

    assert_eq!(
        retire_inference_canonical_transitions_through_turn(
            &shared_pool,
            &user_a,
            &shared_session,
            1,
        )
        .await
        .expect("retire exactly one owner/session chain"),
        2
    );
    assert!(
        load_inference_canonical_transitions_for_session(
            &shared_pool,
            &user_a,
            &shared_session,
            1,
        )
        .await
        .expect("retired owner/session is empty")
        .is_empty()
    );
    for (user_id, session_id) in [(&user_b, &shared_session), (&user_a, &session_a2)] {
        assert_eq!(
            load_inference_canonical_transitions_for_session(&shared_pool, user_id, session_id, 1,)
                .await
                .expect("retiring a neighbor cannot cross an isolation boundary")[0]
                .transitions
                .len(),
            2
        );
    }

    cleanup(pool, &user_a, &shared_session, &run_a).await;
    cleanup(pool, &user_b, &shared_session, &run_b).await;
    cleanup(pool, &user_a, &session_a2, &run_a2).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn competing_canonical_children_commit_exactly_one_branch_without_orphans() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("canonical-fork-user-{suffix}");
    let session_id = format!("canonical-fork-session-{suffix}");
    let run_id = format!("canonical-fork-run-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;

    let durable_base = astra_turn_types::CanonicalPrefixIdentityV1::from_messages(&[])
        .expect("empty durable base");
    let mut history = vec![serde_json::json!({"role": "user", "content": "root"})];
    let root = astra_turn_types::ProviderCanonicalTransitionV2::new_from_durable_base(
        None,
        durable_base.clone(),
        &history,
        vec![canonical_authority("root")],
    )
    .expect("construct canonical fork root");
    commit_canonical_transition(
        &shared_pool,
        &user_id,
        &session_id,
        &run_id,
        0,
        "fork-root",
        &root,
    )
    .await;
    history.extend(root.appended_messages.iter().cloned());

    let left = astra_turn_types::ProviderCanonicalTransitionV2::new_linked_from_durable_base(
        root.transition_id.clone(),
        root.result.clone(),
        durable_base.clone(),
        &history,
        vec![canonical_authority("left")],
    )
    .expect("construct left fork");
    let right = astra_turn_types::ProviderCanonicalTransitionV2::new_linked_from_durable_base(
        root.transition_id.clone(),
        root.result.clone(),
        durable_base,
        &history,
        vec![canonical_authority("right")],
    )
    .expect("construct right fork");
    let left_plan =
        plan_inference_invocation(run_input(&user_id, &session_id, &run_id, 1, "fork-left"))
            .expect("plan left fork");
    let right_plan =
        plan_inference_invocation(run_input(&user_id, &session_id, &run_id, 2, "fork-right"))
            .expect("plan right fork");
    admit_inference_invocation(&shared_pool, &left_plan)
        .await
        .expect("admit left fork invocation");
    admit_inference_invocation(&shared_pool, &right_plan)
        .await
        .expect("admit right fork invocation");
    let left_attempt = provider_attempt(&left_plan, 0)
        .with_canonical_transitions(std::slice::from_ref(&left))
        .expect("bind left fork");
    let right_attempt = provider_attempt(&right_plan, 0)
        .with_canonical_transitions(std::slice::from_ref(&right))
        .expect("bind right fork");

    let (left_outcome, right_outcome) = tokio::join!(
        begin_inference_provider_attempt(&shared_pool, &left_attempt),
        begin_inference_provider_attempt(&shared_pool, &right_attempt),
    );
    let outcomes = [&left_outcome, &right_outcome];
    assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| outcome
                .as_ref()
                .is_err_and(|error| error.kind == ServiceErrorKind::Conflict))
            .count(),
        1
    );
    let expected_head = if left_outcome.is_ok() { &left } else { &right };
    let rejected_attempt_id = if left_outcome.is_ok() {
        right_attempt.attempt_id()
    } else {
        left_attempt.attempt_id()
    };
    let head: String = sqlx::query_scalar(
        "SELECT head_transition_id FROM inference_canonical_transition_heads
         WHERE user_id = ? AND session_id = ? AND turn_index = 1",
    )
    .bind(&user_id)
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("load winning canonical fork");
    assert_eq!(head, expected_head.transition_id);
    let wal_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM inference_canonical_transition_wal
         WHERE user_id = ? AND session_id = ? AND turn_index = 1",
    )
    .bind(&user_id)
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("count fork WAL entries");
    assert_eq!(wal_rows, 2, "the losing branch must roll back its WAL row");
    let rejected_attempts: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM inference_provider_attempts
         WHERE user_id = ? AND attempt_id = ?",
    )
    .bind(&user_id)
    .bind(rejected_attempt_id)
    .fetch_one(pool)
    .await
    .expect("count rejected fork attempts");
    assert_eq!(
        rejected_attempts, 0,
        "the losing branch must not authorize provider delivery"
    );
    let terminal = InferenceInvocationTerminal {
        status: InferenceTerminalStatus::Cancelled,
        usage: InferenceUsage::default(),
        usage_status: InferenceUsageStatus::Unavailable,
        provider_response_id: None,
        error_kind: Some("canonical_fork_resolved".to_string()),
        error_message: Some("close both sides of the canonical fork race".to_string()),
    };
    if left_outcome.is_ok() {
        finish_inference_provider_attempt(&shared_pool, &left_attempt, &terminal)
            .await
            .expect("finish winning left provider attempt");
    } else {
        finish_inference_provider_attempt(&shared_pool, &right_attempt, &terminal)
            .await
            .expect("finish winning right provider attempt");
    }
    finish_inference_invocation(&shared_pool, &left_plan, &terminal)
        .await
        .expect("finish left fork invocation");
    finish_inference_invocation(&shared_pool, &right_plan, &terminal)
        .await
        .expect("finish right fork invocation");
    let receipts =
        load_inference_canonical_transitions_for_session(&shared_pool, &user_id, &session_id, 1)
            .await
            .expect("load the winning canonical branch");
    assert_eq!(receipts[0].transitions, vec![root, expected_head.clone()]);

    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn canonical_wal_capacity_rejects_append_atomically_and_accepts_checkpoint() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("canonical-capacity-user-{suffix}");
    let session_id = format!("canonical-capacity-session-{suffix}");
    let run_id = format!("canonical-capacity-run-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;

    let durable_base = astra_turn_types::CanonicalPrefixIdentityV1::from_messages(&[])
        .expect("empty durable base");
    let mut history = vec![serde_json::json!({"role": "user", "content": "root"})];
    let root = astra_turn_types::ProviderCanonicalTransitionV2::new_from_durable_base(
        None,
        durable_base.clone(),
        &history,
        vec![canonical_authority("root")],
    )
    .expect("construct capacity root");
    commit_canonical_transition(
        &shared_pool,
        &user_id,
        &session_id,
        &run_id,
        0,
        "capacity-root",
        &root,
    )
    .await;
    history.extend(root.appended_messages.iter().cloned());
    sqlx::query(
        "UPDATE inference_canonical_transition_heads
         SET chain_length = ?, chain_payload_bytes = ?
         WHERE user_id = ? AND session_id = ? AND turn_index = 1",
    )
    .bind(i64::from(
        astra_turn_types::MAX_PROVIDER_CANONICAL_WAL_ENTRIES,
    ))
    .bind(
        i64::try_from(astra_turn_types::MAX_PROVIDER_CANONICAL_WAL_BYTES)
            .expect("WAL byte limit fits i64"),
    )
    .bind(&user_id)
    .bind(&session_id)
    .execute(pool)
    .await
    .expect("simulate a WAL head exactly at capacity");

    let append = astra_turn_types::ProviderCanonicalTransitionV2::new_linked_from_durable_base(
        root.transition_id.clone(),
        root.result.clone(),
        durable_base.clone(),
        &history,
        vec![canonical_authority("over-capacity")],
    )
    .expect("construct over-capacity append");
    let append_plan = plan_inference_invocation(run_input(
        &user_id,
        &session_id,
        &run_id,
        1,
        "capacity-append",
    ))
    .expect("plan over-capacity append");
    admit_inference_invocation(&shared_pool, &append_plan)
        .await
        .expect("admit over-capacity logical invocation");
    let append_attempt = provider_attempt(&append_plan, 0)
        .with_canonical_transitions(std::slice::from_ref(&append))
        .expect("bind over-capacity append");
    assert_eq!(
        begin_inference_provider_attempt(&shared_pool, &append_attempt)
            .await
            .expect_err("an append at the WAL limit must require compaction")
            .kind,
        ServiceErrorKind::Conflict
    );
    let rejected_attempts: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM inference_provider_attempts
         WHERE user_id = ? AND attempt_id = ?",
    )
    .bind(&user_id)
    .bind(append_attempt.attempt_id())
    .fetch_one(pool)
    .await
    .expect("count capacity-rejected attempts");
    assert_eq!(rejected_attempts, 0);
    let wal_before_checkpoint: Vec<String> = sqlx::query_scalar(
        "SELECT transition_id FROM inference_canonical_transition_wal
         WHERE user_id = ? AND session_id = ? AND turn_index = 1",
    )
    .bind(&user_id)
    .bind(&session_id)
    .fetch_all(pool)
    .await
    .expect("load WAL after rejected append");
    assert_eq!(wal_before_checkpoint, vec![root.transition_id.clone()]);
    let rejected_terminal = InferenceInvocationTerminal {
        status: InferenceTerminalStatus::Cancelled,
        usage: InferenceUsage::default(),
        usage_status: InferenceUsageStatus::Unavailable,
        provider_response_id: None,
        error_kind: Some("canonical_wal_capacity".to_string()),
        error_message: Some("close the append rejected at the WAL capacity boundary".to_string()),
    };
    finish_inference_invocation(&shared_pool, &append_plan, &rejected_terminal)
        .await
        .expect("finish capacity-rejected logical invocation");

    let checkpoint_history = vec![
        serde_json::json!({"role": "system", "content": "bounded checkpoint"}),
        serde_json::json!({"role": "user", "content": "continue"}),
    ];
    let checkpoint =
        astra_turn_types::ProviderCanonicalTransitionV2::new_replacement_from_durable_base(
            Some(root.transition_id),
            durable_base,
            1,
            &checkpoint_history,
            vec![canonical_authority("checkpoint")],
        )
        .expect("construct bounded replacement checkpoint");
    commit_canonical_transition(
        &shared_pool,
        &user_id,
        &session_id,
        &run_id,
        2,
        "capacity-checkpoint",
        &checkpoint,
    )
    .await;
    let head = sqlx::query(
        "SELECT head_transition_id, chain_length, chain_payload_bytes
         FROM inference_canonical_transition_heads
         WHERE user_id = ? AND session_id = ? AND turn_index = 1",
    )
    .bind(&user_id)
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("load compacted capacity head");
    assert_eq!(
        head.get::<String, _>("head_transition_id"),
        checkpoint.transition_id
    );
    assert_eq!(head.get::<i64, _>("chain_length"), 1);
    assert!(
        head.get::<i64, _>("chain_payload_bytes")
            < i64::try_from(astra_turn_types::MAX_PROVIDER_CANONICAL_WAL_BYTES)
                .expect("WAL byte limit fits i64")
    );
    let receipts =
        load_inference_canonical_transitions_for_session(&shared_pool, &user_id, &session_id, 1)
            .await
            .expect("load checkpoint after capacity recovery");
    assert_eq!(receipts[0].transitions, vec![checkpoint]);

    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn canonical_transition_wal_fails_closed_and_replacement_is_an_atomic_checkpoint() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("canonical-corruption-user-{suffix}");
    let session_id = format!("canonical-corruption-session-{suffix}");
    let run_id = format!("canonical-corruption-run-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;

    let durable_base = astra_turn_types::CanonicalPrefixIdentityV1::from_messages(&[])
        .expect("empty durable base");
    let mut history = vec![serde_json::json!({"role": "user", "content": "original"})];
    let first = astra_turn_types::ProviderCanonicalTransitionV2::new_from_durable_base(
        None,
        durable_base.clone(),
        &history,
        vec![canonical_authority("original-first")],
    )
    .expect("construct original WAL anchor");
    commit_canonical_transition(
        &shared_pool,
        &user_id,
        &session_id,
        &run_id,
        0,
        "corruption-first",
        &first,
    )
    .await;
    history.extend(first.appended_messages.iter().cloned());
    let second = astra_turn_types::ProviderCanonicalTransitionV2::new_from_durable_base(
        Some(first.transition_id.clone()),
        durable_base.clone(),
        &history,
        vec![canonical_authority("original-second")],
    )
    .expect("construct original WAL successor");
    commit_canonical_transition(
        &shared_pool,
        &user_id,
        &session_id,
        &run_id,
        1,
        "corruption-second",
        &second,
    )
    .await;

    let first_attempt_id: String = sqlx::query_scalar(
        "SELECT attempt_id FROM inference_canonical_transition_wal
         WHERE user_id = ? AND session_id = ? AND turn_index = 1 AND transition_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&first.transition_id)
    .fetch_one(pool)
    .await
    .expect("load original WAL attempt owner");
    let orphan_attempt_id = format!("orphan-{suffix}");
    sqlx::query(
        "UPDATE inference_canonical_transition_wal
         SET attempt_id = ?
         WHERE user_id = ? AND session_id = ? AND turn_index = 1 AND transition_id = ?",
    )
    .bind(&orphan_attempt_id)
    .bind(&user_id)
    .bind(&session_id)
    .bind(&first.transition_id)
    .execute(pool)
    .await
    .expect("inject an orphan WAL attempt owner");
    assert_eq!(
        load_inference_canonical_transitions_for_session(&shared_pool, &user_id, &session_id, 1,)
            .await
            .expect_err("WAL without its exact provider-attempt audit row must fail closed")
            .kind,
        ServiceErrorKind::Conflict
    );
    sqlx::query(
        "UPDATE inference_canonical_transition_wal
         SET attempt_id = ?
         WHERE user_id = ? AND session_id = ? AND turn_index = 1 AND transition_id = ?",
    )
    .bind(&first_attempt_id)
    .bind(&user_id)
    .bind(&session_id)
    .bind(&first.transition_id)
    .execute(pool)
    .await
    .expect("restore the exact WAL attempt owner");

    sqlx::query(
        "DELETE FROM inference_canonical_transition_wal
         WHERE user_id = ? AND session_id = ? AND turn_index = 1 AND transition_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&first.transition_id)
    .execute(pool)
    .await
    .expect("inject a missing WAL ancestor");
    assert_eq!(
        load_inference_canonical_transitions_for_session(&shared_pool, &user_id, &session_id, 1,)
            .await
            .expect_err("a partial chain must never be replayed")
            .kind,
        ServiceErrorKind::Conflict
    );

    let replacement_history = vec![
        serde_json::json!({"role": "system", "content": "compacted checkpoint"}),
        serde_json::json!({"role": "user", "content": "continue from checkpoint"}),
    ];
    let replacement =
        astra_turn_types::ProviderCanonicalTransitionV2::new_replacement_from_durable_base(
            Some(second.transition_id.clone()),
            durable_base,
            1,
            &replacement_history,
            vec![canonical_authority("replacement")],
        )
        .expect("construct explicit replacement checkpoint");
    commit_canonical_transition(
        &shared_pool,
        &user_id,
        &session_id,
        &run_id,
        2,
        "corruption-replacement",
        &replacement,
    )
    .await;

    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT transition_id FROM inference_canonical_transition_wal
         WHERE user_id = ? AND session_id = ? AND turn_index = 1",
    )
    .bind(&user_id)
    .bind(&session_id)
    .fetch_all(pool)
    .await
    .expect("load checkpointed WAL identities");
    assert_eq!(rows, vec![replacement.transition_id.clone()]);
    let receipts =
        load_inference_canonical_transitions_for_session(&shared_pool, &user_id, &session_id, 1)
            .await
            .expect("replacement checkpoint restores a complete bounded chain");
    assert_eq!(receipts[0].transitions, vec![replacement.clone()]);
    let mut restored = Vec::new();
    replacement
        .apply_to(&mut restored)
        .expect("replay replacement from the durable base");
    let mut expected = replacement_history;
    expected.extend(replacement.appended_messages.iter().cloned());
    assert_eq!(restored, expected);

    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn concurrent_physical_attempts_authorize_exactly_one_provider_delivery() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("attempt-race-user-{suffix}");
    let session_id = format!("attempt-race-session-{suffix}");
    let run_id = format!("attempt-race-run-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;
    let plan = plan_inference_invocation(InferenceInvocationInput {
        user_id: user_id.clone(),
        scope: InferenceInvocationScope::Run {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            turn: 1,
            round: 0,
            operation_id: "concurrent_physical_attempts".to_string(),
            logical_attempt: 0,
        },
        offering_id: "attempt-race-offering".to_string(),
        resolved_model_name: "attempt-race-model".to_string(),
        upstream_model_name: "attempt-race-model".to_string(),
        provider: "openai".to_string(),
        purpose: InferencePurpose::PrimaryAgent,
        execution_placement: ModelExecutionPlacement::Server,
        access_kind: ModelAccessKind::SelfHosted,
        run_authority: run_authority(),
    })
    .expect("plan attempt race");
    admit_inference_invocation(&shared_pool, &plan)
        .await
        .expect("admit attempt race");
    let first = provider_attempt(&plan, 0);
    let second = provider_attempt(&plan, 1);
    let (first_outcome, second_outcome) = tokio::join!(
        begin_inference_provider_attempt(&shared_pool, &first),
        begin_inference_provider_attempt(&shared_pool, &second),
    );
    let outcomes = [first_outcome, second_outcome];
    assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| outcome
                .as_ref()
                .is_err_and(|error| error.kind == ServiceErrorKind::Conflict))
            .count(),
        1
    );
    let started: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM inference_provider_attempts
         WHERE user_id = ? AND invocation_id = ? AND status = 'started'",
    )
    .bind(&user_id)
    .bind(plan.invocation_id())
    .fetch_one(pool)
    .await
    .expect("count open provider attempts");
    assert_eq!(started, 1);

    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn combined_first_attempt_admission_is_atomic_and_replay_safe() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("combined-admission-user-{suffix}");
    let session_id = format!("combined-admission-session-{suffix}");
    let run_id = format!("combined-admission-run-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;

    let plan = plan_inference_invocation(InferenceInvocationInput {
        user_id: user_id.clone(),
        scope: InferenceInvocationScope::Run {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            turn: 1,
            round: 0,
            operation_id: "combined_admission".to_string(),
            logical_attempt: 0,
        },
        offering_id: "combined-admission-offering".to_string(),
        resolved_model_name: "combined-admission-model".to_string(),
        upstream_model_name: "combined-admission-model".to_string(),
        provider: "openai".to_string(),
        purpose: InferencePurpose::PrimaryAgent,
        execution_placement: ModelExecutionPlacement::Server,
        access_kind: ModelAccessKind::SelfHosted,
        run_authority: run_authority(),
    })
    .expect("plan combined admission");
    let attempt = provider_attempt(&plan, 0);

    admit_inference_invocation_with_first_provider_attempt(&shared_pool, &plan, &attempt)
        .await
        .expect("atomically admit invocation and first provider attempt");

    let counts = sqlx::query(
        "SELECT
            (SELECT COUNT(*) FROM inference_routes
             WHERE user_id = ? AND route_id = ?) AS route_count,
            (SELECT COUNT(*) FROM inference_invocations
             WHERE user_id = ? AND invocation_id = ?) AS invocation_count,
            (SELECT COUNT(*) FROM inference_provider_attempts
             WHERE user_id = ? AND attempt_id = ?) AS attempt_count,
            (SELECT COUNT(*) FROM model_request_context_events
             WHERE user_id = ? AND attempt_id = ? AND event_stage = 'accepted') AS context_count",
    )
    .bind(&user_id)
    .bind(plan.route_id())
    .bind(&user_id)
    .bind(plan.invocation_id())
    .bind(&user_id)
    .bind(attempt.attempt_id())
    .bind(&user_id)
    .bind(attempt.attempt_id())
    .fetch_one(pool)
    .await
    .expect("load combined admission facts");
    assert_eq!(counts.get::<i64, _>("route_count"), 1);
    assert_eq!(counts.get::<i64, _>("invocation_count"), 1);
    assert_eq!(counts.get::<i64, _>("attempt_count"), 1);
    assert_eq!(counts.get::<i64, _>("context_count"), 1);

    assert_eq!(
        admit_inference_invocation_with_first_provider_attempt(&shared_pool, &plan, &attempt)
            .await
            .expect_err("combined admission replay must not authorize provider redelivery")
            .kind,
        ServiceErrorKind::Conflict
    );

    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn bounded_settlement_recovery_processes_only_one_batch() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("targeted-recovery-user-{suffix}");
    let session_id = format!("targeted-recovery-session-{suffix}");
    let run_id = format!("targeted-recovery-run-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;

    let mut plans = Vec::new();
    for round in 0..2 {
        let plan = plan_inference_invocation(InferenceInvocationInput {
            user_id: user_id.clone(),
            scope: InferenceInvocationScope::Run {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                turn: 1,
                round,
                operation_id: "targeted_settlement_recovery".to_string(),
                logical_attempt: 0,
            },
            offering_id: "targeted-recovery-offering".to_string(),
            resolved_model_name: "targeted-recovery-model".to_string(),
            upstream_model_name: "targeted-recovery-model".to_string(),
            provider: "openai".to_string(),
            purpose: InferencePurpose::PrimaryAgent,
            execution_placement: ModelExecutionPlacement::Server,
            access_kind: ModelAccessKind::SelfHosted,
            run_authority: run_authority(),
        })
        .expect("plan targeted recovery invocation");
        admit_inference_invocation(&shared_pool, &plan)
            .await
            .expect("admit targeted recovery invocation");
        let attempt = provider_attempt(&plan, 0);
        begin_inference_provider_attempt(&shared_pool, &attempt)
            .await
            .expect("begin targeted recovery attempt");
        finish_inference_provider_attempt(
            &shared_pool,
            &attempt,
            &InferenceInvocationTerminal::succeeded(
                InferenceUsage {
                    input: astra_turn_types::NormalizedPromptCacheUsage::new(3, 0, 0),
                    output_tokens: 2,
                },
                Some(format!("targeted-response-{round}")),
            ),
        )
        .await
        .expect("persist targeted recovery attempt");
        plans.push(plan);
    }
    reconcile_inference_settlements(&shared_pool, 1)
        .await
        .expect("reconcile one bounded invocation batch");

    let rows = sqlx::query(
        "SELECT invocation_id, status,
                (SELECT COUNT(*) FROM inference_invocation_settlement_debts AS debt
                 WHERE debt.user_id = invocation.user_id
                   AND debt.invocation_id = invocation.invocation_id) AS debt_count
         FROM inference_invocations AS invocation
         WHERE user_id = ? AND invocation_id IN (?, ?)",
    )
    .bind(&user_id)
    .bind(plans[0].invocation_id())
    .bind(plans[1].invocation_id())
    .fetch_all(pool)
    .await
    .expect("load targeted recovery outcomes");
    assert_eq!(rows.len(), 2);
    let mut completed = 0;
    let mut pending = 0;
    for row in rows {
        let status = row.get::<String, _>("status");
        let debt_count = row.get::<i64, _>("debt_count");
        match (status.as_str(), debt_count) {
            ("succeeded", 0) => completed += 1,
            ("admitted", 1) => pending += 1,
            unexpected => panic!("unexpected bounded recovery state: {unexpected:?}"),
        }
    }
    assert_eq!((completed, pending), (1, 1));

    reconcile_inference_settlements(&shared_pool, 1)
        .await
        .expect("drain the second invocation before cleanup");
    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn deferred_settlement_does_not_consume_another_users_bounded_batch_slot() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("deferred-recovery-user-{suffix}");
    let session_id = format!("deferred-recovery-session-{suffix}");
    let run_id = format!("deferred-recovery-run-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;
    let cancelled = InferenceInvocationTerminal {
        status: InferenceTerminalStatus::Cancelled,
        usage: InferenceUsage::default(),
        usage_status: InferenceUsageStatus::Unavailable,
        provider_response_id: None,
        error_kind: Some("cancelled".to_string()),
        error_message: Some("provider delivery was never authorized".to_string()),
    };
    let mut plans = Vec::new();
    for round in 0..2 {
        let plan = plan_inference_invocation(InferenceInvocationInput {
            user_id: user_id.clone(),
            scope: InferenceInvocationScope::Run {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                turn: 2,
                round,
                operation_id: "deferred_settlement_fairness".to_string(),
                logical_attempt: 0,
            },
            offering_id: "deferred-recovery-offering".to_string(),
            resolved_model_name: "deferred-recovery-model".to_string(),
            upstream_model_name: "deferred-recovery-model".to_string(),
            provider: "openai".to_string(),
            purpose: InferencePurpose::PrimaryAgent,
            execution_placement: ModelExecutionPlacement::Server,
            access_kind: ModelAccessKind::SelfHosted,
            run_authority: run_authority(),
        })
        .expect("plan deferred recovery invocation");
        admit_inference_invocation(&shared_pool, &plan)
            .await
            .expect("admit deferred recovery invocation");
        declare_inference_attempt_settlement(
            &shared_pool,
            &plan,
            &provider_attempt(&plan, 0),
            &cancelled,
            InferenceProviderDeliveryState::PreDelivery,
        )
        .await
        .expect("record deferred recovery settlement");
        plans.push(plan);
    }
    sqlx::query(
        "UPDATE inference_invocation_settlement_debts
         SET next_retry_at = DATE_ADD(NOW(6), INTERVAL 1 HOUR)
         WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(plans[0].invocation_id())
    .execute(pool)
    .await
    .expect("defer one owner's retry eligibility");

    assert_eq!(
        reconcile_inference_settlements(&shared_pool, 1)
            .await
            .expect("eligible settlement behind a deferred row remains recoverable"),
        1
    );
    let statuses = sqlx::query(
        "SELECT invocation_id, status FROM inference_invocations
         WHERE user_id = ? AND invocation_id IN (?, ?)",
    )
    .bind(&user_id)
    .bind(plans[0].invocation_id())
    .bind(plans[1].invocation_id())
    .fetch_all(pool)
    .await
    .expect("load deferred fairness outcomes");
    for row in statuses {
        let invocation_id = row.get::<String, _>("invocation_id");
        let expected = if invocation_id == plans[0].invocation_id() {
            "admitted"
        } else {
            "cancelled"
        };
        assert_eq!(row.get::<String, _>("status"), expected);
    }

    sqlx::query(
        "UPDATE inference_invocation_settlement_debts SET next_retry_at = NOW(6)
         WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(plans[0].invocation_id())
    .execute(pool)
    .await
    .expect("make deferred row eligible for cleanup");
    reconcile_inference_settlements(&shared_pool, 1)
        .await
        .expect("reconcile deferred row after its retry window");
    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn recovery_discards_unproven_success_without_blocking_later_settlement() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("recovery-isolation-user-{suffix}");
    let session_id = format!("recovery-isolation-session-{suffix}");
    let run_id = format!("recovery-isolation-run-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;

    let mut plans = Vec::new();
    for round in 0..2 {
        let plan = plan_inference_invocation(InferenceInvocationInput {
            user_id: user_id.clone(),
            scope: InferenceInvocationScope::Run {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                turn: 1,
                round,
                operation_id: "recovery_failure_isolation".to_string(),
                logical_attempt: 0,
            },
            offering_id: "recovery-isolation-offering".to_string(),
            resolved_model_name: "recovery-isolation-model".to_string(),
            upstream_model_name: "recovery-isolation-model".to_string(),
            provider: "openai".to_string(),
            purpose: InferencePurpose::PrimaryAgent,
            execution_placement: ModelExecutionPlacement::Server,
            access_kind: ModelAccessKind::SelfHosted,
            run_authority: run_authority(),
        })
        .expect("plan recovery isolation invocation");
        admit_inference_invocation(&shared_pool, &plan)
            .await
            .expect("admit recovery isolation invocation");
        plans.push(plan);
    }

    for (plan, status, fingerprint, error_kind) in [
        (&plans[0], "succeeded", "a", None),
        (&plans[1], "failed", "b", Some("provider_unavailable")),
    ] {
        sqlx::query(
            "INSERT INTO inference_invocation_settlement_debts
             (user_id, invocation_id, session_id, harness_run_id,
              terminal_status, terminal_fingerprint, error_kind)
             VALUES (?, ?, ?, NULL, ?, REPEAT(?, 64), ?)",
        )
        .bind(&user_id)
        .bind(plan.invocation_id())
        .bind(&session_id)
        .bind(status)
        .bind(fingerprint)
        .bind(error_kind)
        .execute(pool)
        .await
        .expect("seed explicit recovery evidence");
    }

    reconcile_until_invocation_status(
        &shared_pool,
        pool,
        &user_id,
        plans[1].invocation_id(),
        "failed",
    )
    .await;

    let rows = sqlx::query(
        "SELECT invocation_id, status,
                (SELECT COUNT(*) FROM inference_invocation_settlement_debts AS debt
                 WHERE debt.user_id = invocation.user_id
                   AND debt.invocation_id = invocation.invocation_id) AS debt_count
         FROM inference_invocations AS invocation
         WHERE user_id = ? AND invocation_id IN (?, ?)",
    )
    .bind(&user_id)
    .bind(plans[0].invocation_id())
    .bind(plans[1].invocation_id())
    .fetch_all(pool)
    .await
    .expect("load isolated recovery outcomes");
    assert_eq!(rows.len(), 2);
    for row in rows {
        let invocation_id = row.get::<String, _>("invocation_id");
        let expected_status = if invocation_id == plans[0].invocation_id() {
            "admitted"
        } else {
            assert_eq!(invocation_id, plans[1].invocation_id());
            "failed"
        };
        assert_eq!(row.get::<String, _>("status"), expected_status);
        assert_eq!(row.get::<i64, _>("debt_count"), 0);
    }

    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn authoritative_settlement_debt_converges_an_orphaned_open_attempt() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("orphan-attempt-user-{suffix}");
    let session_id = format!("orphan-attempt-session-{suffix}");
    let run_id = format!("orphan-attempt-run-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;

    let plan = plan_inference_invocation(InferenceInvocationInput {
        user_id: user_id.clone(),
        scope: InferenceInvocationScope::Run {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            turn: 1,
            round: 0,
            operation_id: "orphan_attempt_recovery".to_string(),
            logical_attempt: 0,
        },
        offering_id: "orphan-attempt-offering".to_string(),
        resolved_model_name: "orphan-attempt-model".to_string(),
        upstream_model_name: "orphan-attempt-model".to_string(),
        provider: "openai".to_string(),
        purpose: InferencePurpose::PrimaryAgent,
        execution_placement: ModelExecutionPlacement::Server,
        access_kind: ModelAccessKind::SelfHosted,
        run_authority: run_authority(),
    })
    .expect("plan orphan-attempt invocation");
    admit_inference_invocation(&shared_pool, &plan)
        .await
        .expect("admit orphan-attempt invocation");
    let attempt = provider_attempt(&plan, 0);
    begin_inference_provider_attempt(&shared_pool, &attempt)
        .await
        .expect("begin orphaned physical attempt");

    declare_inference_settlement(
        &shared_pool,
        &plan,
        &InferenceInvocationTerminal {
            status: InferenceTerminalStatus::Failed,
            usage: InferenceUsage::default(),
            usage_status: InferenceUsageStatus::Unavailable,
            provider_response_id: None,
            error_kind: Some("provider_unavailable".to_string()),
            error_message: Some("logical retry policy exhausted".to_string()),
        },
    )
    .await
    .expect("seed authoritative settlement decision");
    assert_eq!(
        begin_inference_provider_attempt(&shared_pool, &provider_attempt(&plan, 1))
            .await
            .expect_err("a durable settlement decision must fence provider redelivery")
            .kind,
        ServiceErrorKind::Conflict
    );

    assert_eq!(
        reconcile_inference_settlements(&shared_pool, 256)
            .await
            .expect("recovery must converge the orphaned provider attempt"),
        1
    );

    let attempt_status: String = sqlx::query_scalar(
        "SELECT status FROM inference_provider_attempts
         WHERE user_id = ? AND attempt_id = ?",
    )
    .bind(&user_id)
    .bind(attempt.attempt_id())
    .fetch_one(pool)
    .await
    .expect("load recovered provider attempt");
    assert_eq!(attempt_status, "delivery_unknown");
    let invocation_status: String = sqlx::query_scalar(
        "SELECT status FROM inference_invocations
         WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(plan.invocation_id())
    .fetch_one(pool)
    .await
    .expect("load recovered logical invocation");
    assert_eq!(invocation_status, "failed");
    let debt_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM inference_invocation_settlement_debts
         WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(plan.invocation_id())
    .fetch_one(pool)
    .await
    .expect("load recovered settlement debt");
    assert_eq!(debt_count, 0);

    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn exact_attempt_debt_recovers_success_without_degrading_provider_facts() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("exact-attempt-debt-user-{suffix}");
    let session_id = format!("exact-attempt-debt-session-{suffix}");
    let run_id = format!("exact-attempt-debt-run-{suffix}");
    let provider = format!("exact-attempt-provider-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;

    let plan = plan_inference_invocation(InferenceInvocationInput {
        user_id: user_id.clone(),
        scope: InferenceInvocationScope::Run {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            turn: 1,
            round: 0,
            operation_id: "exact_attempt_debt".to_string(),
            logical_attempt: 0,
        },
        offering_id: "exact-attempt-offering".to_string(),
        resolved_model_name: "exact-attempt-model".to_string(),
        upstream_model_name: "exact-attempt-model".to_string(),
        provider: provider.clone(),
        purpose: InferencePurpose::PrimaryAgent,
        execution_placement: ModelExecutionPlacement::Server,
        access_kind: ModelAccessKind::SelfHosted,
        run_authority: run_authority(),
    })
    .expect("plan exact-attempt invocation");
    admit_inference_invocation(&shared_pool, &plan)
        .await
        .expect("admit exact-attempt invocation");
    let attempt = provider_attempt(&plan, 0);
    begin_inference_provider_attempt(&shared_pool, &attempt)
        .await
        .expect("begin exact-attempt provider request");
    let terminal = InferenceInvocationTerminal::succeeded(
        InferenceUsage {
            input: astra_turn_types::NormalizedPromptCacheUsage::new(11, 7, 3),
            output_tokens: 5,
        },
        Some("exact-provider-response".to_string()),
    );

    // A successful provider terminal and its exact settlement debt commit in
    // one transaction. Recovery consumes that debt if the runtime disappears
    // before it mirrors the terminal onto the logical invocation.
    finish_inference_provider_attempt(&shared_pool, &attempt, &terminal)
        .await
        .expect("record exact physical terminal and settlement debt");
    reconcile_inference_settlements(&shared_pool, 256)
        .await
        .expect("sweeper applies exact physical and logical terminal");

    let attempt_row = sqlx::query(
        "SELECT status, terminal_fingerprint, input_tokens, output_tokens,
                cache_read_tokens, cache_creation_tokens, provider_response_id
         FROM inference_provider_attempts WHERE user_id = ? AND attempt_id = ?",
    )
    .bind(&user_id)
    .bind(attempt.attempt_id())
    .fetch_one(pool)
    .await
    .expect("load exact recovered provider attempt");
    assert_eq!(attempt_row.get::<String, _>("status"), "succeeded");
    assert!(
        attempt_row
            .get::<Option<String>, _>("terminal_fingerprint")
            .is_some()
    );
    assert_eq!(attempt_row.get::<i64, _>("input_tokens"), 11);
    assert_eq!(attempt_row.get::<i64, _>("output_tokens"), 5);
    assert_eq!(attempt_row.get::<i64, _>("cache_read_tokens"), 7);
    assert_eq!(attempt_row.get::<i64, _>("cache_creation_tokens"), 3);
    assert_eq!(
        attempt_row.get::<Option<String>, _>("provider_response_id"),
        Some("exact-provider-response".to_string())
    );
    let invocation_status: String = sqlx::query_scalar(
        "SELECT status FROM inference_invocations
         WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(plan.invocation_id())
    .fetch_one(pool)
    .await
    .expect("load exact recovered logical invocation");
    assert_eq!(invocation_status, "succeeded");
    let context_rows = sqlx::query(
        "SELECT event_stage, terminal_status, input_tokens, output_tokens,
                cache_read_tokens, cache_creation_tokens
         FROM model_request_context_events
         WHERE user_id = ? AND attempt_id = ? ORDER BY event_stage",
    )
    .bind(&user_id)
    .bind(attempt.attempt_id())
    .fetch_all(pool)
    .await
    .expect("load recovered request-context evidence");
    assert_eq!(context_rows.len(), 2);
    let terminal_context = context_rows
        .iter()
        .find(|row| row.get::<String, _>("event_stage") == "terminal")
        .expect("recovery emits terminal request-context evidence");
    assert_eq!(
        terminal_context.get::<Option<String>, _>("terminal_status"),
        Some("succeeded".to_string())
    );
    assert_eq!(
        terminal_context.get::<Option<i64>, _>("input_tokens"),
        Some(21)
    );
    assert_eq!(
        terminal_context.get::<Option<i64>, _>("output_tokens"),
        Some(5)
    );
    assert_eq!(
        terminal_context.get::<Option<i64>, _>("cache_read_tokens"),
        Some(7)
    );
    assert_eq!(
        terminal_context.get::<Option<i64>, _>("cache_creation_tokens"),
        Some(3)
    );
    let metric = astra_services::aggregate_model_request_metrics(&shared_pool)
        .await
        .expect("load recovered exact-attempt metrics")
        .into_iter()
        .find(|row| row.provider == provider && row.terminal_status == "succeeded")
        .expect("exact-attempt recovery updates its metric shard once");
    assert_eq!(metric.requests, 1);
    assert_eq!(metric.input_tokens, 21);
    assert_eq!(metric.output_tokens, 5);
    assert_eq!(metric.cache_read_tokens, 7);
    assert_eq!(metric.cache_creation_tokens, 3);
    let debt_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM inference_invocation_settlement_debts
         WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(plan.invocation_id())
    .fetch_one(pool)
    .await
    .expect("load exact attempt debt after recovery");
    assert_eq!(debt_count, 0);

    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn conflicting_exact_attempt_settlement_is_rejected_without_promoting_logical_success() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("exact-conflict-user-{suffix}");
    let session_id = format!("exact-conflict-session-{suffix}");
    let run_id = format!("exact-conflict-run-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;

    let plan = plan_inference_invocation(InferenceInvocationInput {
        user_id: user_id.clone(),
        scope: InferenceInvocationScope::Run {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            turn: 1,
            round: 0,
            operation_id: "exact_attempt_conflict".to_string(),
            logical_attempt: 0,
        },
        offering_id: "exact-conflict-offering".to_string(),
        resolved_model_name: "exact-conflict-model".to_string(),
        upstream_model_name: "exact-conflict-model".to_string(),
        provider: "openai".to_string(),
        purpose: InferencePurpose::PrimaryAgent,
        execution_placement: ModelExecutionPlacement::Server,
        access_kind: ModelAccessKind::SelfHosted,
        run_authority: run_authority(),
    })
    .expect("plan exact-conflict invocation");
    admit_inference_invocation(&shared_pool, &plan)
        .await
        .expect("admit exact-conflict invocation");
    let attempt = provider_attempt(&plan, 0);
    begin_inference_provider_attempt(&shared_pool, &attempt)
        .await
        .expect("begin exact-conflict provider request");
    let physical_failure = InferenceInvocationTerminal {
        status: InferenceTerminalStatus::Failed,
        usage: InferenceUsage::default(),
        usage_status: InferenceUsageStatus::Unavailable,
        provider_response_id: Some("conflicting-provider-response".to_string()),
        error_kind: Some("server_error".to_string()),
        error_message: Some("provider rejected the request".to_string()),
    };
    finish_inference_provider_attempt(&shared_pool, &attempt, &physical_failure)
        .await
        .expect("persist the physical failure");

    let conflicting_success = InferenceInvocationTerminal::succeeded(
        InferenceUsage::default(),
        Some("impossible-success".to_string()),
    );
    assert_eq!(
        declare_inference_attempt_settlement(
            &shared_pool,
            &plan,
            &attempt,
            &conflicting_success,
            InferenceProviderDeliveryState::DeliveryAuthorized,
        )
        .await
        .expect_err("a failed physical attempt cannot authorize logical success")
        .kind,
        ServiceErrorKind::Conflict
    );

    let invocation_status: String = sqlx::query_scalar(
        "SELECT status FROM inference_invocations
         WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(plan.invocation_id())
    .fetch_one(pool)
    .await
    .expect("load conflicting logical invocation");
    assert_eq!(invocation_status, "admitted");
    let physical_status: String = sqlx::query_scalar(
        "SELECT status FROM inference_provider_attempts
         WHERE user_id = ? AND attempt_id = ?",
    )
    .bind(&user_id)
    .bind(attempt.attempt_id())
    .fetch_one(pool)
    .await
    .expect("load conflicting physical attempt");
    assert_eq!(physical_status, "failed");
    let debt_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM inference_invocation_settlement_debts
         WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(plan.invocation_id())
    .fetch_one(pool)
    .await
    .expect("count rejected exact-attempt debt");
    assert_eq!(debt_count, 0);

    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn pre_delivery_missing_attempt_cancels_without_fabricating_physical_accounting() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("pre-delivery-user-{suffix}");
    let session_id = format!("pre-delivery-session-{suffix}");
    let run_id = format!("pre-delivery-run-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;
    let plan = plan_inference_invocation(InferenceInvocationInput {
        user_id: user_id.clone(),
        scope: InferenceInvocationScope::Run {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            turn: 1,
            round: 0,
            operation_id: "pre_delivery_missing_attempt".to_string(),
            logical_attempt: 0,
        },
        offering_id: "pre-delivery-offering".to_string(),
        resolved_model_name: "pre-delivery-model".to_string(),
        upstream_model_name: "pre-delivery-model".to_string(),
        provider: "openai".to_string(),
        purpose: InferencePurpose::PrimaryAgent,
        execution_placement: ModelExecutionPlacement::Server,
        access_kind: ModelAccessKind::SelfHosted,
        run_authority: run_authority(),
    })
    .expect("plan pre-delivery invocation");
    admit_inference_invocation(&shared_pool, &plan)
        .await
        .expect("admit pre-delivery invocation");
    let planned_attempt = provider_attempt(&plan, 0);
    let cancelled = InferenceInvocationTerminal {
        status: InferenceTerminalStatus::Cancelled,
        usage: InferenceUsage::default(),
        usage_status: InferenceUsageStatus::Unavailable,
        provider_response_id: None,
        error_kind: Some("cancelled".to_string()),
        error_message: Some("provider delivery was never authorized".to_string()),
    };
    declare_inference_attempt_settlement(
        &shared_pool,
        &plan,
        &planned_attempt,
        &cancelled,
        InferenceProviderDeliveryState::PreDelivery,
    )
    .await
    .expect("record pre-delivery cancellation");
    reconcile_until_invocation_status(
        &shared_pool,
        pool,
        &user_id,
        plan.invocation_id(),
        "cancelled",
    )
    .await;

    let status: String = sqlx::query_scalar(
        "SELECT status FROM inference_invocations WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(plan.invocation_id())
    .fetch_one(pool)
    .await
    .expect("load pre-delivery logical terminal");
    assert_eq!(status, "cancelled");
    let physical_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM inference_provider_attempts
         WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(plan.invocation_id())
    .fetch_one(pool)
    .await
    .expect("count physical attempts");
    assert_eq!(physical_count, 0);
    let context_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM model_request_context_events
         WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(plan.invocation_id())
    .fetch_one(pool)
    .await
    .expect("count request accounting evidence");
    assert_eq!(
        context_count, 0,
        "a request that was never admitted or delivered must not fabricate usage evidence"
    );

    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn quarantined_missing_authorized_attempt_does_not_starve_pending_users() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let bad_user = format!("000-authorized-missing-{suffix}");
    let bad_session = format!("authorized-missing-session-{suffix}");
    let bad_run = format!("authorized-missing-run-{suffix}");
    seed_run(pool, &bad_user, &bad_session, &bad_run).await;
    let bad_plan = plan_inference_invocation(InferenceInvocationInput {
        user_id: bad_user.clone(),
        scope: InferenceInvocationScope::Run {
            session_id: bad_session.clone(),
            run_id: bad_run.clone(),
            turn: 1,
            round: 0,
            operation_id: "authorized_missing_attempt".to_string(),
            logical_attempt: 0,
        },
        offering_id: "authorized-missing-offering".to_string(),
        resolved_model_name: "authorized-missing-model".to_string(),
        upstream_model_name: "authorized-missing-model".to_string(),
        provider: "openai".to_string(),
        purpose: InferencePurpose::PrimaryAgent,
        execution_placement: ModelExecutionPlacement::Server,
        access_kind: ModelAccessKind::SelfHosted,
        run_authority: run_authority(),
    })
    .expect("plan missing authorized invocation");
    admit_inference_invocation(&shared_pool, &bad_plan)
        .await
        .expect("admit missing authorized invocation");
    let missing_attempt = provider_attempt(&bad_plan, 0);
    let delivery_unknown = InferenceInvocationTerminal {
        status: InferenceTerminalStatus::DeliveryUnknown,
        usage: InferenceUsage::default(),
        usage_status: InferenceUsageStatus::Unavailable,
        provider_response_id: None,
        error_kind: Some("stream_transport".to_string()),
        error_message: Some(
            "delivery was authorized but admission evidence is missing".to_string(),
        ),
    };
    declare_inference_attempt_settlement(
        &shared_pool,
        &bad_plan,
        &missing_attempt,
        &delivery_unknown,
        InferenceProviderDeliveryState::DeliveryAuthorized,
    )
    .await
    .expect("record missing authorized exact debt");
    reconcile_until_debt_status(
        &shared_pool,
        pool,
        &bad_user,
        bad_plan.invocation_id(),
        "quarantined",
    )
    .await;
    let quarantine_status: String = sqlx::query_scalar(
        "SELECT reconciliation_status FROM inference_invocation_settlement_debts
         WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&bad_user)
    .bind(bad_plan.invocation_id())
    .fetch_one(pool)
    .await
    .expect("load authorized-missing quarantine");
    assert_eq!(quarantine_status, "quarantined");

    let good_user = format!("zzz-pre-delivery-{suffix}");
    let good_session = format!("good-pre-delivery-session-{suffix}");
    let good_run = format!("good-pre-delivery-run-{suffix}");
    seed_run(pool, &good_user, &good_session, &good_run).await;
    let good_plan = plan_inference_invocation(InferenceInvocationInput {
        user_id: good_user.clone(),
        scope: InferenceInvocationScope::Run {
            session_id: good_session.clone(),
            run_id: good_run.clone(),
            turn: 1,
            round: 0,
            operation_id: "good_pre_delivery".to_string(),
            logical_attempt: 0,
        },
        offering_id: "good-pre-delivery-offering".to_string(),
        resolved_model_name: "good-pre-delivery-model".to_string(),
        upstream_model_name: "good-pre-delivery-model".to_string(),
        provider: "openai".to_string(),
        purpose: InferencePurpose::PrimaryAgent,
        execution_placement: ModelExecutionPlacement::Server,
        access_kind: ModelAccessKind::SelfHosted,
        run_authority: run_authority(),
    })
    .expect("plan later recoverable invocation");
    admit_inference_invocation(&shared_pool, &good_plan)
        .await
        .expect("admit later recoverable invocation");
    let good_attempt = provider_attempt(&good_plan, 0);
    let cancelled = InferenceInvocationTerminal {
        status: InferenceTerminalStatus::Cancelled,
        usage: InferenceUsage::default(),
        usage_status: InferenceUsageStatus::Unavailable,
        provider_response_id: None,
        error_kind: Some("cancelled".to_string()),
        error_message: Some("provider delivery was never authorized".to_string()),
    };
    declare_inference_attempt_settlement(
        &shared_pool,
        &good_plan,
        &good_attempt,
        &cancelled,
        InferenceProviderDeliveryState::PreDelivery,
    )
    .await
    .expect("record later recoverable settlement");
    reconcile_until_invocation_status(
        &shared_pool,
        pool,
        &good_user,
        good_plan.invocation_id(),
        "cancelled",
    )
    .await;
    let good_status: String = sqlx::query_scalar(
        "SELECT status FROM inference_invocations WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&good_user)
    .bind(good_plan.invocation_id())
    .fetch_one(pool)
    .await
    .expect("load later recoverable logical invocation");
    assert_eq!(good_status, "cancelled");

    cleanup(pool, &bad_user, &bad_session, &bad_run).await;
    cleanup(pool, &good_user, &good_session, &good_run).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn orphaned_settlement_debt_is_quarantined_out_of_the_active_batch() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("orphaned-settlement-user-{suffix}");
    let session_id = format!("orphaned-settlement-session-{suffix}");
    let run_id = format!("orphaned-settlement-run-{suffix}");
    let invocation_id = format!("orphaned-settlement-invocation-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;

    sqlx::query(
        "INSERT INTO inference_invocation_settlement_debts
         (user_id, invocation_id, session_id, harness_run_id,
          terminal_status, terminal_fingerprint, usage_status,
          provider_delivery_state)
         VALUES (?, ?, ?, NULL, 'failed', REPEAT('e', 64), 'unavailable', 'unknown')",
    )
    .bind(&user_id)
    .bind(&invocation_id)
    .bind(&session_id)
    .execute(pool)
    .await
    .expect("seed an orphaned durable settlement incident");

    reconcile_until_debt_status(&shared_pool, pool, &user_id, &invocation_id, "quarantined").await;
    let row = sqlx::query(
        "SELECT reconciliation_status, quarantine_reason
         FROM inference_invocation_settlement_debts
         WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(&invocation_id)
    .fetch_one(pool)
    .await
    .expect("load quarantined orphaned settlement");
    assert_eq!(row.get::<String, _>("reconciliation_status"), "quarantined");
    assert!(
        row.get::<Option<String>, _>("quarantine_reason")
            .is_some_and(|reason| reason.contains("logical invocation"))
    );

    cleanup(pool, &user_id, &session_id, &run_id).await;
}

async fn cleanup(pool: &sqlx::Pool<sqlx::MySql>, user_id: &str, session_id: &str, run_id: &str) {
    for (statement, identity) in [
        (
            "DELETE FROM model_request_context_events WHERE user_id = ? AND session_id = ?",
            session_id,
        ),
        (
            "DELETE FROM inference_invocation_settlement_debts WHERE user_id = ? AND session_id = ?",
            session_id,
        ),
        (
            "DELETE FROM inference_canonical_transition_wal WHERE user_id = ? AND session_id = ?",
            session_id,
        ),
        (
            "DELETE FROM inference_canonical_transition_heads WHERE user_id = ? AND session_id = ?",
            session_id,
        ),
        (
            "DELETE FROM inference_provider_attempts WHERE user_id = ? AND session_id = ?",
            session_id,
        ),
        (
            "DELETE FROM inference_invocations WHERE user_id = ? AND session_id = ?",
            session_id,
        ),
        (
            "DELETE FROM inference_routes WHERE user_id = ? AND session_id = ?",
            session_id,
        ),
        (
            "DELETE FROM agent_runs WHERE user_id = ? AND run_id = ?",
            run_id,
        ),
        (
            "DELETE FROM agent_sessions WHERE user_id = ? AND session_id = ?",
            session_id,
        ),
    ] {
        sqlx::query(statement)
            .bind(user_id)
            .bind(identity)
            .execute(pool)
            .await
            .unwrap_or_else(|error| panic!("cleanup `{statement}`: {error}"));
    }
}

async fn reconcile_until_invocation_status(
    shared_pool: &astra_core::SharedPool,
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    invocation_id: &str,
    expected_status: &str,
) {
    for _ in 0..32 {
        let status = sqlx::query_scalar::<_, String>(
            "SELECT status FROM inference_invocations WHERE user_id = ? AND invocation_id = ?",
        )
        .bind(user_id)
        .bind(invocation_id)
        .fetch_optional(pool)
        .await
        .expect("load invocation while reconciling shared backlog");
        if status.as_deref() == Some(expected_status) {
            return;
        }
        reconcile_inference_settlements(shared_pool, 256)
            .await
            .expect("reconcile shared inference backlog");
    }
    panic!(
        "invocation {user_id}/{invocation_id} did not reach {expected_status} after bounded reconciliation"
    );
}

async fn reconcile_until_debt_status(
    shared_pool: &astra_core::SharedPool,
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    invocation_id: &str,
    expected_status: &str,
) {
    for _ in 0..32 {
        let status = sqlx::query_scalar::<_, String>(
            "SELECT reconciliation_status FROM inference_invocation_settlement_debts
             WHERE user_id = ? AND invocation_id = ?",
        )
        .bind(user_id)
        .bind(invocation_id)
        .fetch_optional(pool)
        .await
        .expect("load settlement debt while reconciling shared backlog");
        if status.as_deref() == Some(expected_status) {
            return;
        }
        reconcile_inference_settlements(shared_pool, 256)
            .await
            .expect("reconcile shared inference backlog");
    }
    panic!(
        "settlement debt {user_id}/{invocation_id} did not reach {expected_status} after bounded reconciliation"
    );
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn context_expiry_and_delayed_terminal_never_split_an_attempt() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("context-expiry-user-{suffix}");
    let session_id = format!("context-expiry-session-{suffix}");
    let run_id = format!("context-expiry-run-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;

    let plan = plan_inference_invocation(InferenceInvocationInput {
        user_id: user_id.clone(),
        scope: InferenceInvocationScope::Run {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            turn: 1,
            round: 0,
            operation_id: "context_expiry_terminal_race".to_string(),
            logical_attempt: 0,
        },
        offering_id: "context-expiry-offering".to_string(),
        resolved_model_name: "context-expiry-model".to_string(),
        upstream_model_name: "context-expiry-model".to_string(),
        provider: "openai".to_string(),
        purpose: InferencePurpose::PrimaryAgent,
        execution_placement: ModelExecutionPlacement::Server,
        access_kind: ModelAccessKind::SelfHosted,
        run_authority: run_authority(),
    })
    .expect("plan context expiry invocation");
    admit_inference_invocation(&shared_pool, &plan)
        .await
        .expect("admit context expiry invocation");
    let attempt = provider_attempt(&plan, 0);
    begin_inference_provider_attempt(&shared_pool, &attempt)
        .await
        .expect("persist accepted context event");
    sqlx::query(
        "UPDATE model_request_context_events
         SET created_at = DATE_SUB(NOW(6), INTERVAL 31 DAY)
         WHERE user_id = ? AND attempt_id = ?",
    )
    .bind(&user_id)
    .bind(attempt.attempt_id())
    .execute(pool)
    .await
    .expect("age accepted context event past retention");

    let terminal = InferenceInvocationTerminal {
        status: InferenceTerminalStatus::Failed,
        usage: InferenceUsage::default(),
        usage_status: InferenceUsageStatus::Unavailable,
        provider_response_id: None,
        error_kind: Some("provider_unavailable".to_string()),
        error_message: Some("delayed terminal after diagnostic retention".to_string()),
    };
    let maintenance_policy = RuntimeMaintenancePolicy {
        batch_limit: 1_000,
        ..RuntimeMaintenancePolicy::default()
    };
    let (maintenance, terminal) = tokio::join!(
        maintain_runtime_storage(&shared_pool, None, &maintenance_policy),
        finish_inference_provider_attempt(&shared_pool, &attempt, &terminal),
    );
    assert!(
        maintenance.cleanup_errors.is_empty(),
        "runtime maintenance errors: {:?}",
        maintenance.cleanup_errors
    );
    terminal.expect("terminal persistence must converge with context expiry");

    let stages = sqlx::query(
        "SELECT event_stage FROM model_request_context_events
         WHERE user_id = ? AND attempt_id = ? ORDER BY event_stage",
    )
    .bind(&user_id)
    .bind(attempt.attempt_id())
    .fetch_all(pool)
    .await
    .expect("load retained model request context stages")
    .into_iter()
    .map(|row| row.get::<String, _>("event_stage"))
    .collect::<Vec<_>>();
    assert!(
        stages.is_empty() || stages == ["accepted", "terminal"],
        "expiry and terminal must retain an atomic pair or no diagnostics, never {stages:?}"
    );
    if stages.is_empty() {
        let expired: Option<chrono::NaiveDateTime> = sqlx::query_scalar(
            "SELECT context_expired_at FROM inference_provider_attempts
             WHERE user_id = ? AND attempt_id = ?",
        )
        .bind(&user_id)
        .bind(attempt.attempt_id())
        .fetch_one(pool)
        .await
        .expect("read durable context expiry marker");
        assert!(
            expired.is_some(),
            "a delayed terminal may skip only diagnostics durably expired under its attempt lock"
        );
    }

    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn context_expiry_skips_an_old_attempt_with_a_fresh_terminal() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("context-expiry-hol-user-{suffix}");
    let session_id = format!("context-expiry-hol-session-{suffix}");
    let run_id = format!("context-expiry-hol-run-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;

    let failure = InferenceInvocationTerminal {
        status: InferenceTerminalStatus::Failed,
        usage: InferenceUsage::default(),
        usage_status: InferenceUsageStatus::Unavailable,
        provider_response_id: None,
        error_kind: Some("provider_unavailable".to_string()),
        error_message: Some("retention candidate fixture".to_string()),
    };
    let protected_plan = plan_inference_invocation(InferenceInvocationInput {
        user_id: user_id.clone(),
        scope: InferenceInvocationScope::Run {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            turn: 1,
            round: 0,
            operation_id: "context_expiry_head_of_line".to_string(),
            logical_attempt: 0,
        },
        offering_id: "context-expiry-hol-offering".to_string(),
        resolved_model_name: "context-expiry-hol-model".to_string(),
        upstream_model_name: "context-expiry-hol-model".to_string(),
        provider: "openai".to_string(),
        purpose: InferencePurpose::PrimaryAgent,
        execution_placement: ModelExecutionPlacement::Server,
        access_kind: ModelAccessKind::SelfHosted,
        run_authority: run_authority(),
    })
    .expect("plan protected context attempt");
    admit_inference_invocation(&shared_pool, &protected_plan)
        .await
        .expect("admit protected context attempt");
    let protected_attempt = provider_attempt(&protected_plan, 0);
    begin_inference_provider_attempt(&shared_pool, &protected_attempt)
        .await
        .expect("begin protected context attempt");
    finish_inference_provider_attempt(&shared_pool, &protected_attempt, &failure)
        .await
        .expect("finish protected context attempt");
    sqlx::query(
        "UPDATE model_request_context_events
         SET created_at = DATE_SUB(NOW(6), INTERVAL 32 DAY)
         WHERE user_id = ? AND attempt_id = ? AND event_stage = 'accepted'",
    )
    .bind(&user_id)
    .bind(protected_attempt.attempt_id())
    .execute(pool)
    .await
    .expect("age only the protected accepted event");

    let eligible_plan = plan_inference_invocation(InferenceInvocationInput {
        user_id: user_id.clone(),
        scope: InferenceInvocationScope::Run {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            turn: 1,
            round: 1,
            operation_id: "context_expiry_head_of_line".to_string(),
            logical_attempt: 0,
        },
        offering_id: "context-expiry-hol-offering".to_string(),
        resolved_model_name: "context-expiry-hol-model".to_string(),
        upstream_model_name: "context-expiry-hol-model".to_string(),
        provider: "openai".to_string(),
        purpose: InferencePurpose::PrimaryAgent,
        execution_placement: ModelExecutionPlacement::Server,
        access_kind: ModelAccessKind::SelfHosted,
        run_authority: run_authority(),
    })
    .expect("plan eligible context attempt");
    admit_inference_invocation(&shared_pool, &eligible_plan)
        .await
        .expect("admit eligible context attempt");
    let eligible_attempt = provider_attempt(&eligible_plan, 0);
    begin_inference_provider_attempt(&shared_pool, &eligible_attempt)
        .await
        .expect("begin eligible context attempt");
    finish_inference_provider_attempt(&shared_pool, &eligible_attempt, &failure)
        .await
        .expect("finish eligible context attempt");
    sqlx::query(
        "UPDATE model_request_context_events
         SET created_at = DATE_SUB(NOW(6), INTERVAL 31 DAY)
         WHERE user_id = ? AND attempt_id = ?",
    )
    .bind(&user_id)
    .bind(eligible_attempt.attempt_id())
    .execute(pool)
    .await
    .expect("age the eligible context attempt");

    let maintenance = maintain_runtime_storage(
        &shared_pool,
        None,
        &RuntimeMaintenancePolicy {
            batch_limit: 1,
            ..RuntimeMaintenancePolicy::default()
        },
    )
    .await;
    assert!(
        maintenance.cleanup_errors.is_empty(),
        "runtime maintenance errors: {:?}",
        maintenance.cleanup_errors
    );
    let protected_stages = sqlx::query_scalar::<_, String>(
        "SELECT event_stage FROM model_request_context_events
         WHERE user_id = ? AND attempt_id = ? ORDER BY event_stage",
    )
    .bind(&user_id)
    .bind(protected_attempt.attempt_id())
    .fetch_all(pool)
    .await
    .expect("load protected context stages");
    assert_eq!(protected_stages, ["accepted", "terminal"]);
    let eligible_event_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM model_request_context_events WHERE user_id = ? AND attempt_id = ?",
    )
    .bind(&user_id)
    .bind(eligible_attempt.attempt_id())
    .fetch_one(pool)
    .await
    .expect("count expired context events");
    assert_eq!(eligible_event_count, 0);

    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn provider_attempt_terminal_fails_closed_on_durable_wire_identity_drift() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("wire-drift-user-{suffix}");
    let session_id = format!("wire-drift-session-{suffix}");
    let run_id = format!("wire-drift-run-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;

    let plan = plan_inference_invocation(InferenceInvocationInput {
        user_id: user_id.clone(),
        scope: InferenceInvocationScope::Run {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            turn: 1,
            round: 0,
            operation_id: "provider_wire_drift".to_string(),
            logical_attempt: 0,
        },
        offering_id: "wire-drift-offering".to_string(),
        resolved_model_name: "wire-drift-model".to_string(),
        upstream_model_name: "wire-drift-model".to_string(),
        provider: "openai".to_string(),
        purpose: InferencePurpose::PrimaryAgent,
        execution_placement: ModelExecutionPlacement::Server,
        access_kind: ModelAccessKind::SelfHosted,
        run_authority: run_authority(),
    })
    .expect("plan wire-drift invocation");
    admit_inference_invocation(&shared_pool, &plan)
        .await
        .expect("admit wire-drift invocation");
    let attempt = provider_attempt(&plan, 0);
    begin_inference_provider_attempt(&shared_pool, &attempt)
        .await
        .expect("begin exact physical attempt");

    sqlx::query(
        "UPDATE inference_provider_attempts
         SET provider_wire_hash = REPEAT('b', 64)
         WHERE user_id = ? AND attempt_id = ?",
    )
    .bind(&user_id)
    .bind(attempt.attempt_id())
    .execute(pool)
    .await
    .expect("simulate durable wire identity drift");

    let error = finish_inference_provider_attempt(
        &shared_pool,
        &attempt,
        &InferenceInvocationTerminal {
            status: InferenceTerminalStatus::DeliveryUnknown,
            usage: InferenceUsage::default(),
            usage_status: InferenceUsageStatus::Unavailable,
            provider_response_id: None,
            error_kind: Some("stream_transport".to_string()),
            error_message: Some("partial delivery".to_string()),
        },
    )
    .await
    .expect_err("a terminal writer must not accept a row for different wire bytes");
    assert_eq!(error.kind, ServiceErrorKind::Conflict);
    assert!(
        error.message.contains("provider_wire_hash"),
        "conflict must identify the immutable field: {error}"
    );

    let status: String = sqlx::query_scalar(
        "SELECT status FROM inference_provider_attempts
         WHERE user_id = ? AND attempt_id = ?",
    )
    .bind(&user_id)
    .bind(attempt.attempt_id())
    .fetch_one(pool)
    .await
    .expect("load drifted attempt");
    assert_eq!(status, "started", "conflicting facts must not terminalize");

    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn deleting_session_fences_new_run_and_session_inference_admission() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("deleting-admission-user-{suffix}");
    let session_id = format!("deleting-admission-session-{suffix}");
    let run_id = format!("deleting-admission-run-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;
    sqlx::query(
        "UPDATE agent_sessions SET status = 'deleting' WHERE user_id = ? AND session_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .execute(pool)
    .await
    .expect("mark inference owner session deleting");

    let scopes = [
        InferenceInvocationScope::Run {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            turn: 1,
            round: 0,
            operation_id: "deleting_run_admission".to_string(),
            logical_attempt: 0,
        },
        InferenceInvocationScope::Session {
            session_id: session_id.clone(),
            turn: 1,
            round: 1,
            operation_id: "deleting_session_admission".to_string(),
            logical_attempt: 0,
        },
    ];
    for scope in scopes {
        let authority = matches!(&scope, InferenceInvocationScope::Run { .. })
            .then(run_authority)
            .flatten();
        let plan = plan_inference_invocation(InferenceInvocationInput {
            user_id: user_id.clone(),
            scope,
            run_authority: authority,
            offering_id: "deleting-admission-offering".to_string(),
            resolved_model_name: "deleting-admission-model".to_string(),
            upstream_model_name: "deleting-admission-model".to_string(),
            provider: "openai".to_string(),
            purpose: InferencePurpose::PrimaryAgent,
            execution_placement: ModelExecutionPlacement::Server,
            access_kind: ModelAccessKind::SelfHosted,
        })
        .expect("plan inference against deleting session");
        assert_eq!(
            admit_inference_invocation(&shared_pool, &plan)
                .await
                .expect_err("deleting session must fence new provider admission")
                .kind,
            ServiceErrorKind::NotFound
        );
    }

    let durable_rows: i64 = sqlx::query_scalar(
        "SELECT
             (SELECT COUNT(*) FROM inference_routes WHERE user_id = ? AND session_id = ?)
           + (SELECT COUNT(*) FROM inference_invocations WHERE user_id = ? AND session_id = ?)",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&user_id)
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("count inference rows rejected during deletion");
    assert_eq!(
        durable_rows, 0,
        "rejected admission must leave no route or invocation"
    );

    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn run_inference_admission_fences_generation_owner_lease_guidance_and_cancel() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("inference-authority-user-{suffix}");
    let session_id = format!("inference-authority-session-{suffix}");
    let run_id = format!("inference-authority-run-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;
    let base_input = InferenceInvocationInput {
        user_id: user_id.clone(),
        scope: InferenceInvocationScope::Run {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            turn: 1,
            round: 0,
            operation_id: "authority_base".to_string(),
            logical_attempt: 0,
        },
        run_authority: run_authority(),
        offering_id: "inference-authority-offering".to_string(),
        resolved_model_name: "inference-authority-model".to_string(),
        upstream_model_name: "inference-authority-model".to_string(),
        provider: "openai".to_string(),
        purpose: InferencePurpose::PrimaryAgent,
        execution_placement: ModelExecutionPlacement::Server,
        access_kind: ModelAccessKind::SelfHosted,
    };
    let plan_for = |operation_id: &str, authority: InferenceRunAdmissionAuthority| {
        let mut input = base_input.clone();
        if let InferenceInvocationScope::Run {
            operation_id: operation,
            ..
        } = &mut input.scope
        {
            *operation = operation_id.to_string();
        }
        input.run_authority = Some(authority);
        plan_inference_invocation(input).expect("plan fenced invocation")
    };
    let exact = run_authority().expect("run authority fixture");

    let mut wrong_generation = exact.clone();
    wrong_generation.expected_owner_generation = 1;
    let error = admit_inference_invocation(
        &shared_pool,
        &plan_for("wrong_generation", wrong_generation),
    )
    .await
    .expect_err("stale generation cannot admit provider work");
    assert_eq!(error.kind, ServiceErrorKind::NotFound);

    let mut wrong_owner = exact.clone();
    wrong_owner.expected_owner_pod_id = "other-inference-owner".to_string();
    let error = admit_inference_invocation(&shared_pool, &plan_for("wrong_owner", wrong_owner))
        .await
        .expect_err("stale owner cannot admit provider work");
    assert_eq!(error.kind, ServiceErrorKind::NotFound);

    sqlx::query("UPDATE agent_runs SET status = 'completed' WHERE user_id = ? AND run_id = ?")
        .bind(&user_id)
        .bind(&run_id)
        .execute(pool)
        .await
        .expect("terminalize inference owner run");
    let error = admit_inference_invocation(&shared_pool, &plan_for("terminal_run", exact.clone()))
        .await
        .expect_err("terminal run cannot admit provider work");
    assert_eq!(error.kind, ServiceErrorKind::NotFound);
    sqlx::query("UPDATE agent_runs SET status = 'running' WHERE user_id = ? AND run_id = ?")
        .bind(&user_id)
        .bind(&run_id)
        .execute(pool)
        .await
        .expect("restore running inference owner fixture");

    sqlx::query(
        "UPDATE agent_runs SET owner_lease_expires_at = TIMESTAMPADD(SECOND, -1, NOW(6))
         WHERE user_id = ? AND run_id = ?",
    )
    .bind(&user_id)
    .bind(&run_id)
    .execute(pool)
    .await
    .expect("expire inference owner lease");
    let error = admit_inference_invocation(&shared_pool, &plan_for("expired_lease", exact.clone()))
        .await
        .expect_err("expired lease cannot admit provider work");
    assert_eq!(error.kind, ServiceErrorKind::NotFound);
    sqlx::query(
        "UPDATE agent_runs SET owner_lease_expires_at = TIMESTAMPADD(MINUTE, 5, NOW(6))
         WHERE user_id = ? AND run_id = ?",
    )
    .bind(&user_id)
    .bind(&run_id)
    .execute(pool)
    .await
    .expect("restore inference owner lease");

    append_run_control_event(pool, &user_id, &session_id, &run_id, 0, "user_intent").await;
    let error = admit_inference_invocation(
        &shared_pool,
        &plan_for("unobserved_guidance", exact.clone()),
    )
    .await
    .expect_err("newer guidance cannot race provider admission");
    assert_eq!(error.kind, ServiceErrorKind::NotFound);

    let mut observed_guidance = exact.clone();
    observed_guidance.expected_control_epoch = 0;
    admit_inference_invocation(
        &shared_pool,
        &plan_for("observed_guidance", observed_guidance.clone()),
    )
    .await
    .expect("applied guidance may admit under the same exact owner");

    sqlx::query(
        "UPDATE agent_runs SET cancellation_requested_at = NOW(6)
         WHERE user_id = ? AND session_id = ? AND run_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&run_id)
    .execute(pool)
    .await
    .expect("record run-level user cancellation marker");
    let error =
        admit_inference_invocation(&shared_pool, &plan_for("cancelled_run", observed_guidance))
            .await
            .expect_err("cancellation fences provider admission even when observed");
    assert_eq!(error.kind, ServiceErrorKind::NotFound);

    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn uncertain_admission_rejects_a_different_terminal_fingerprint_without_provider_attempt() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("terminal-fingerprint-user-{suffix}");
    let session_id = format!("terminal-fingerprint-session-{suffix}");
    let run_id = format!("terminal-fingerprint-run-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;
    let plan = plan_inference_invocation(InferenceInvocationInput {
        user_id: user_id.clone(),
        scope: InferenceInvocationScope::Run {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            turn: 1,
            round: 0,
            operation_id: "terminal_fingerprint_conflict".to_string(),
            logical_attempt: 0,
        },
        run_authority: run_authority(),
        offering_id: "terminal-fingerprint-offering".to_string(),
        resolved_model_name: "terminal-fingerprint-model".to_string(),
        upstream_model_name: "terminal-fingerprint-model".to_string(),
        provider: "openai".to_string(),
        purpose: InferencePurpose::PrimaryAgent,
        execution_placement: ModelExecutionPlacement::Server,
        access_kind: ModelAccessKind::SelfHosted,
    })
    .expect("plan terminal fingerprint conflict");
    admit_inference_invocation(&shared_pool, &plan)
        .await
        .expect("admit terminal fingerprint conflict");
    sqlx::query(
        "UPDATE inference_invocations
         SET status = 'cancelled', terminal_fingerprint = REPEAT('b', 64), terminal_at = NOW(6)
         WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(plan.invocation_id())
    .execute(pool)
    .await
    .expect("seed a different authoritative terminal");
    let recovery_terminal = InferenceInvocationTerminal {
        status: InferenceTerminalStatus::Cancelled,
        usage: InferenceUsage::default(),
        usage_status: InferenceUsageStatus::Unavailable,
        provider_response_id: None,
        error_kind: Some("cancelled".to_string()),
        error_message: Some("provider delivery was never authorized".to_string()),
    };

    assert_eq!(
        settle_uncertain_inference_admission(&shared_pool, &plan, &recovery_terminal)
            .await
            .expect("classify different terminal fingerprint"),
        InferenceInvocationAdmissionResolution::ConflictingIdentity
    );
    let attempt_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM inference_provider_attempts
         WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(plan.invocation_id())
    .fetch_one(pool)
    .await
    .expect("count forbidden provider attempts");
    assert_eq!(attempt_count, 0);
    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn provider_attempt_revalidates_scope_authority_after_logical_admission() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("attempt-authority-user-{suffix}");
    let session_id = format!("attempt-authority-session-{suffix}");
    let run_id = format!("attempt-authority-run-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;
    let exact = run_authority().expect("run authority fixture");
    let plan_for = |operation_id: &str, authority: InferenceRunAdmissionAuthority| {
        plan_inference_invocation(InferenceInvocationInput {
            user_id: user_id.clone(),
            scope: InferenceInvocationScope::Run {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                turn: 1,
                round: 0,
                operation_id: operation_id.to_string(),
                logical_attempt: 0,
            },
            run_authority: Some(authority),
            offering_id: "attempt-authority-offering".to_string(),
            resolved_model_name: "attempt-authority-model".to_string(),
            upstream_model_name: "attempt-authority-model".to_string(),
            provider: "openai".to_string(),
            purpose: InferencePurpose::PrimaryAgent,
            execution_placement: ModelExecutionPlacement::Server,
            access_kind: ModelAccessKind::SelfHosted,
        })
        .expect("plan provider attempt authority fixture")
    };

    let expired = plan_for("attempt_after_lease_expiry", exact.clone());
    admit_inference_invocation(&shared_pool, &expired)
        .await
        .expect("admit before lease expires");
    sqlx::query(
        "UPDATE agent_runs SET owner_lease_expires_at = TIMESTAMPADD(SECOND, -1, NOW(6))
         WHERE user_id = ? AND run_id = ?",
    )
    .bind(&user_id)
    .bind(&run_id)
    .execute(pool)
    .await
    .expect("expire owner lease after logical admission");
    assert_eq!(
        begin_inference_provider_attempt(&shared_pool, &provider_attempt(&expired, 0))
            .await
            .expect_err("expired authority cannot authorize physical delivery")
            .kind,
        ServiceErrorKind::NotFound
    );

    sqlx::query(
        "UPDATE agent_runs SET owner_lease_expires_at = TIMESTAMPADD(MINUTE, 5, NOW(6))
         WHERE user_id = ? AND run_id = ?",
    )
    .bind(&user_id)
    .bind(&run_id)
    .execute(pool)
    .await
    .expect("restore lease");
    let transferred = plan_for("attempt_after_owner_transfer", exact.clone());
    admit_inference_invocation(&shared_pool, &transferred)
        .await
        .expect("admit before owner transfer");
    sqlx::query(
        "UPDATE agent_runs SET owner_pod_id = 'new-owner', run_generation = run_generation + 1
         WHERE user_id = ? AND run_id = ?",
    )
    .bind(&user_id)
    .bind(&run_id)
    .execute(pool)
    .await
    .expect("transfer owner after logical admission");
    assert_eq!(
        begin_inference_provider_attempt(&shared_pool, &provider_attempt(&transferred, 0))
            .await
            .expect_err("stale generation and owner cannot authorize physical delivery")
            .kind,
        ServiceErrorKind::NotFound
    );

    sqlx::query(
        "UPDATE agent_runs SET owner_pod_id = ?, run_generation = 0
         WHERE user_id = ? AND run_id = ?",
    )
    .bind(TEST_INFERENCE_OWNER_POD_ID)
    .bind(&user_id)
    .bind(&run_id)
    .execute(pool)
    .await
    .expect("restore exact owner");
    let guided = plan_for("attempt_after_guidance", exact.clone());
    admit_inference_invocation(&shared_pool, &guided)
        .await
        .expect("admit before newer guidance");
    append_run_control_event(pool, &user_id, &session_id, &run_id, 0, "user_intent").await;
    assert_eq!(
        begin_inference_provider_attempt(&shared_pool, &provider_attempt(&guided, 0))
            .await
            .expect_err("new guidance cannot race physical delivery")
            .kind,
        ServiceErrorKind::NotFound
    );

    let mut observed = exact;
    observed.expected_control_epoch = 0;
    let cancelled = plan_for("attempt_after_cancellation", observed);
    admit_inference_invocation(&shared_pool, &cancelled)
        .await
        .expect("admit after applying guidance");
    sqlx::query(
        "UPDATE agent_runs SET cancellation_requested_at = NOW(6)
         WHERE user_id = ? AND session_id = ? AND run_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&run_id)
    .execute(pool)
    .await
    .expect("record run-level user cancellation marker");
    assert_eq!(
        begin_inference_provider_attempt(&shared_pool, &provider_attempt(&cancelled, 0))
            .await
            .expect_err("cancellation cannot race physical delivery")
            .kind,
        ServiceErrorKind::NotFound
    );

    let attempt_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM inference_provider_attempts WHERE user_id = ? AND run_id = ?",
    )
    .bind(&user_id)
    .bind(&run_id)
    .fetch_one(pool)
    .await
    .expect("count physical attempts rejected after authority loss");
    assert_eq!(
        attempt_count, 0,
        "provider HTTP has no durable authorization"
    );
    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn durable_logical_attempt_cursor_skips_complete_pairs_and_fails_closed_on_overflow() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("logical-cursor-user-{suffix}");
    let session_id = format!("logical-cursor-session-{suffix}");
    let run_id = format!("logical-cursor-run-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;
    let base_input = InferenceInvocationInput {
        user_id: user_id.clone(),
        scope: InferenceInvocationScope::Run {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            turn: 1,
            round: 1,
            operation_id: "durable_summary_cursor".to_string(),
            logical_attempt: 0,
        },
        offering_id: "logical-cursor-offering".to_string(),
        resolved_model_name: "logical-cursor-model".to_string(),
        upstream_model_name: "logical-cursor-model".to_string(),
        provider: "openai".to_string(),
        purpose: InferencePurpose::Introspection,
        execution_placement: ModelExecutionPlacement::Server,
        access_kind: ModelAccessKind::SelfHosted,
        run_authority: run_authority(),
    };
    let terminal = InferenceInvocationTerminal {
        status: InferenceTerminalStatus::Cancelled,
        usage: InferenceUsage::default(),
        usage_status: InferenceUsageStatus::Unavailable,
        provider_response_id: None,
        error_kind: Some("cancelled".to_string()),
        error_message: Some("cursor test terminal".to_string()),
    };
    assert_eq!(
        next_inference_logical_attempt_pair_base(&shared_pool, &base_input)
            .await
            .expect("empty cursor"),
        0
    );

    for (attempt, expected_next_pair) in [(0, 2), (1, 2), (2, 4)] {
        let input = InferenceInvocationInput {
            scope: base_input.scope.with_logical_attempt(attempt),
            ..base_input.clone()
        };
        let plan = plan_inference_invocation(input).expect("plan cursor invocation");
        admit_inference_invocation(&shared_pool, &plan)
            .await
            .expect("admit cursor invocation");
        finish_inference_invocation(&shared_pool, &plan, &terminal)
            .await
            .expect("finish cursor invocation");
        assert_eq!(
            next_inference_logical_attempt_pair_base(&shared_pool, &base_input)
                .await
                .expect("advance durable cursor"),
            expected_next_pair
        );
    }
    let provider_attempts: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM inference_provider_attempts WHERE user_id = ?")
            .bind(&user_id)
            .fetch_one(pool)
            .await
            .expect("count cursor provider attempts");
    assert_eq!(provider_attempts, 0);

    let overflow_input = InferenceInvocationInput {
        scope: InferenceInvocationScope::Run {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            turn: 1,
            round: 1,
            operation_id: "durable_summary_cursor_overflow".to_string(),
            logical_attempt: u32::MAX,
        },
        ..base_input.clone()
    };
    let overflow_plan = plan_inference_invocation(overflow_input.clone()).expect("plan overflow");
    admit_inference_invocation(&shared_pool, &overflow_plan)
        .await
        .expect("admit overflow identity");
    let overflow = next_inference_logical_attempt_pair_base(&shared_pool, &overflow_input)
        .await
        .expect_err("cursor overflow must fail closed");
    assert_eq!(overflow.kind, ServiceErrorKind::Conflict);

    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn concurrent_logical_cursor_readers_have_one_admission_winner_and_zero_provider_attempts() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("logical-cursor-race-user-{suffix}");
    let session_id = format!("logical-cursor-race-session-{suffix}");
    let run_id = format!("logical-cursor-race-run-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;
    let input = InferenceInvocationInput {
        user_id: user_id.clone(),
        scope: InferenceInvocationScope::Run {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            turn: 1,
            round: 1,
            operation_id: "durable_summary_cursor_race".to_string(),
            logical_attempt: 0,
        },
        offering_id: "logical-cursor-race-offering".to_string(),
        resolved_model_name: "logical-cursor-race-model".to_string(),
        upstream_model_name: "logical-cursor-race-model".to_string(),
        provider: "openai".to_string(),
        purpose: InferencePurpose::Introspection,
        execution_placement: ModelExecutionPlacement::Server,
        access_kind: ModelAccessKind::SelfHosted,
        run_authority: run_authority(),
    };
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let mut readers = Vec::new();
    for _ in 0..2 {
        let shared_pool = shared_pool.clone();
        let input = input.clone();
        let barrier = barrier.clone();
        readers.push(tokio::spawn(async move {
            let pair_base = next_inference_logical_attempt_pair_base(&shared_pool, &input)
                .await
                .expect("read empty durable cursor");
            barrier.wait().await;
            let plan = plan_inference_invocation(InferenceInvocationInput {
                scope: input.scope.with_logical_attempt(pair_base),
                ..input
            })
            .expect("plan cursor race invocation");
            let admission = admit_inference_invocation(&shared_pool, &plan).await;
            (pair_base, admission)
        }));
    }
    let mut successes = 0;
    let mut conflicts = 0;
    for reader in readers {
        let (pair_base, admission) = reader.await.expect("join cursor reader");
        assert_eq!(pair_base, 0);
        match admission {
            Ok(()) => successes += 1,
            Err(error) if error.kind == ServiceErrorKind::Conflict => conflicts += 1,
            Err(error) => panic!("unexpected cursor race error: {error}"),
        }
    }
    assert_eq!((successes, conflicts), (1, 1));
    assert_eq!(
        next_inference_logical_attempt_pair_base(&shared_pool, &input)
            .await
            .expect("loser re-reads durable cursor"),
        2
    );
    let provider_attempts: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM inference_provider_attempts WHERE user_id = ?")
            .bind(&user_id)
            .fetch_one(pool)
            .await
            .expect("count cursor-race provider attempts");
    assert_eq!(provider_attempts, 0);

    cleanup(pool, &user_id, &session_id, &run_id).await;
}

async fn seed_harness_run(pool: &sqlx::Pool<sqlx::MySql>, user_id: &str, harness_run_id: &str) {
    sqlx::query(
        "INSERT INTO harness_runs
         (harness_run_id, harness_id, version_id, user_id, session_id, status,
          input_json, output_json, created_at, updated_at)
         VALUES (?, 'skillify', 'skillify.v1', ?, NULL, 'running', '{}', '{}', NOW(6), NOW(6))",
    )
    .bind(harness_run_id)
    .bind(user_id)
    .execute(pool)
    .await
    .expect("seed harness inference owner");
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn inference_admission_attempts_and_terminal_state_form_one_durable_contract() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("inf-user-{suffix}");
    let session_id = format!("inf-session-{suffix}");
    let run_id = format!("inf-run-{suffix}");
    let provider = format!("test-provider-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;

    let input = InferenceInvocationInput {
        user_id: user_id.clone(),
        scope: InferenceInvocationScope::Run {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            turn: 4,
            round: 2,
            operation_id: "agent_turn".to_string(),
            logical_attempt: 0,
        },
        offering_id: "offer-online".to_string(),
        resolved_model_name: "wire-model".to_string(),
        upstream_model_name: "provider-wire-model".to_string(),
        provider: provider.clone(),
        purpose: InferencePurpose::PrimaryAgent,
        execution_placement: ModelExecutionPlacement::Server,
        access_kind: ModelAccessKind::SelfHosted,
        run_authority: run_authority(),
    };
    let plan = plan_inference_invocation(input.clone()).expect("plan invocation");

    let mut wrong_owner_input = input.clone();
    wrong_owner_input.user_id = format!("other-{suffix}");
    let wrong_owner = plan_inference_invocation(wrong_owner_input).expect("wrong-owner plan");
    assert_eq!(
        admit_inference_invocation(&shared_pool, &wrong_owner)
            .await
            .expect_err("owner mismatch must reject")
            .kind,
        ServiceErrorKind::NotFound
    );

    let (first_admission, concurrent_admission) = tokio::join!(
        admit_inference_invocation(&shared_pool, &plan),
        admit_inference_invocation(&shared_pool, &plan)
    );
    let mut admitted = 0;
    let mut duplicate_conflicts = 0;
    for result in [first_admission, concurrent_admission] {
        match result {
            Ok(()) => admitted += 1,
            Err(error) if error.kind == ServiceErrorKind::Conflict => {
                duplicate_conflicts += 1;
            }
            Err(error) => panic!("unexpected concurrent admission result: {error}"),
        }
    }
    assert_eq!(admitted, 1);
    assert_eq!(duplicate_conflicts, 1);
    let route = sqlx::query(
        "SELECT offering_id, upstream_model_name, execution_placement, access_kind, purpose
         FROM inference_routes WHERE user_id = ? AND route_id = ?",
    )
    .bind(&user_id)
    .bind(plan.route_id())
    .fetch_one(pool)
    .await
    .expect("durable route exists");
    assert_eq!(route.get::<String, _>("offering_id"), "offer-online");
    assert_eq!(
        route.get::<String, _>("upstream_model_name"),
        "provider-wire-model"
    );
    assert_eq!(route.get::<String, _>("execution_placement"), "server");
    assert_eq!(route.get::<String, _>("access_kind"), "self_hosted");
    assert_eq!(route.get::<String, _>("purpose"), "primary_agent");

    assert_eq!(
        admit_inference_invocation(&shared_pool, &plan)
            .await
            .expect_err("logical invocation replay must not redeliver")
            .kind,
        ServiceErrorKind::Conflict
    );

    let first_attempt = provider_attempt(&plan, 0);
    begin_inference_provider_attempt(&shared_pool, &first_attempt)
        .await
        .expect("begin first physical request");
    let first_failure = InferenceInvocationTerminal {
        status: InferenceTerminalStatus::Failed,
        usage: InferenceUsage::default(),
        usage_status: InferenceUsageStatus::Unavailable,
        provider_response_id: Some("provider-429".to_string()),
        error_kind: Some("rate_limit".to_string()),
        error_message: Some("rate limited".to_string()),
    };
    assert_eq!(
        finish_inference_invocation(&shared_pool, &plan, &first_failure)
            .await
            .expect_err("a logical terminal cannot overtake an open provider attempt")
            .kind,
        ServiceErrorKind::Conflict
    );
    let premature_debts: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM inference_invocation_settlement_debts
         WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(plan.invocation_id())
    .fetch_one(pool)
    .await
    .expect("load premature settlement debt");
    assert_eq!(
        premature_debts, 0,
        "rejected finalization must not block the active attempt with a debt"
    );
    let (first_terminal, concurrent_terminal) = tokio::join!(
        finish_inference_provider_attempt(&shared_pool, &first_attempt, &first_failure),
        finish_inference_provider_attempt(&shared_pool, &first_attempt, &first_failure)
    );
    first_terminal.expect("finish first attempt");
    concurrent_terminal.expect("concurrent exact attempt terminal is idempotent");

    let premature_success = InferenceInvocationTerminal::succeeded(
        InferenceUsage::default(),
        Some("provider-not-recorded".to_string()),
    );
    assert_eq!(
        finish_inference_invocation(&shared_pool, &plan, &premature_success)
            .await
            .expect_err("logical success requires a durable successful physical attempt")
            .kind,
        ServiceErrorKind::Conflict
    );

    let second_attempt = provider_attempt(&plan, 1);
    begin_inference_provider_attempt(&shared_pool, &second_attempt)
        .await
        .expect("begin retry as a distinct physical request");
    let success = InferenceInvocationTerminal::succeeded(
        InferenceUsage {
            input: astra_turn_types::NormalizedPromptCacheUsage::new(120, 80, 10),
            output_tokens: 24,
        },
        Some("provider-ok".to_string()),
    );
    finish_inference_provider_attempt(&shared_pool, &second_attempt, &success)
        .await
        .expect("finish retry");
    let successful_settlement_status: String = sqlx::query_scalar(
        "SELECT terminal_status FROM inference_invocation_settlement_debts
         WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(plan.invocation_id())
    .fetch_one(pool)
    .await
    .expect("successful provider terminal must publish its durable settlement handoff");
    assert_eq!(successful_settlement_status, "succeeded");
    let mismatched_success = InferenceInvocationTerminal::succeeded(
        InferenceUsage {
            output_tokens: 25,
            ..success.usage.clone()
        },
        success.provider_response_id.clone(),
    );
    assert_eq!(
        finish_inference_invocation(&shared_pool, &plan, &mismatched_success)
            .await
            .expect_err("logical success must match its physical provider result")
            .kind,
        ServiceErrorKind::Conflict
    );
    let settlement_after_mismatch: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM inference_invocation_settlement_debts
         WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(plan.invocation_id())
    .fetch_one(pool)
    .await
    .expect("mismatched logical terminal must preserve the provider settlement handoff");
    assert_eq!(settlement_after_mismatch, 1);
    let (first_terminal, concurrent_terminal) = tokio::join!(
        finish_inference_invocation(&shared_pool, &plan, &success),
        finish_inference_invocation(&shared_pool, &plan, &success)
    );
    first_terminal.expect("finish logical invocation");
    concurrent_terminal.expect("concurrent exact invocation terminal is idempotent");
    let settlement_after_success: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM inference_invocation_settlement_debts
         WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(plan.invocation_id())
    .fetch_one(pool)
    .await
    .expect("successful logical terminal must consume the provider settlement handoff");
    assert_eq!(settlement_after_success, 0);
    finish_inference_provider_attempt(&shared_pool, &second_attempt, &success)
        .await
        .expect("an exact provider terminal replay remains idempotent after logical settlement");

    let attempts = sqlx::query(
        "SELECT attempt_id, attempt_index, admission_token, provider_protocol, provider_wire_hash,
                provider_wire_bytes, status, input_tokens, output_tokens,
                cache_read_tokens, cache_creation_tokens
         FROM inference_provider_attempts
         WHERE user_id = ? AND invocation_id = ? ORDER BY attempt_index",
    )
    .bind(&user_id)
    .bind(plan.invocation_id())
    .fetch_all(pool)
    .await
    .expect("load physical provider attempts");
    assert_eq!(attempts.len(), 2);
    assert_eq!(
        attempts[0].get::<String, _>("attempt_id"),
        first_attempt.request_id()
    );
    assert_eq!(
        attempts[1].get::<String, _>("attempt_id"),
        second_attempt.request_id()
    );
    assert_ne!(
        attempts[0].get::<String, _>("attempt_id"),
        attempts[1].get::<String, _>("attempt_id"),
        "each physical provider request needs a distinct durable identity"
    );
    assert_ne!(
        attempts[0].get::<String, _>("admission_token"),
        attempts[1].get::<String, _>("admission_token"),
        "each admission owner needs an independent fencing token"
    );
    for attempt in &attempts {
        assert_eq!(attempt.get::<String, _>("admission_token").len(), 32);
        assert_eq!(
            attempt.get::<String, _>("provider_protocol"),
            "openai_compatible"
        );
        assert_eq!(
            attempt.get::<String, _>("provider_wire_hash"),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(attempt.get::<i64, _>("provider_wire_bytes"), 2);
    }
    assert_eq!(attempts[0].get::<i64, _>("attempt_index"), 0);
    assert_eq!(attempts[0].get::<String, _>("status"), "failed");
    assert_eq!(attempts[1].get::<i64, _>("attempt_index"), 1);
    assert_eq!(attempts[1].get::<String, _>("status"), "succeeded");
    assert_eq!(attempts[1].get::<i64, _>("input_tokens"), 120);
    assert_eq!(attempts[1].get::<i64, _>("output_tokens"), 24);
    assert_eq!(attempts[1].get::<i64, _>("cache_read_tokens"), 80);
    assert_eq!(attempts[1].get::<i64, _>("cache_creation_tokens"), 10);

    let context_events =
        astra_services::list_model_request_context_events(&shared_pool, &user_id, &session_id, 10)
            .await
            .expect("list model request context events");
    assert_eq!(
        context_events.len(),
        4,
        "two accepted physical requests must each have accepted and terminal facts"
    );
    let accepted = context_events
        .iter()
        .filter(|event| event.stage == astra_services::ModelRequestEventStage::Accepted)
        .count();
    let terminal = context_events
        .iter()
        .filter(|event| event.stage == astra_services::ModelRequestEventStage::Terminal)
        .count();
    assert_eq!((accepted, terminal), (2, 2));
    let successful_context = context_events
        .iter()
        .find(|event| event.terminal_status.as_deref() == Some("succeeded"))
        .expect("successful request context");
    assert_eq!(
        successful_context
            .event
            .usage
            .as_ref()
            .expect("terminal usage")
            .input
            .fresh_input_tokens,
        120
    );
    assert_eq!(
        astra_services::model_request_trace_coverage(&shared_pool, &user_id, &session_id,)
            .await
            .expect("request trace coverage"),
        astra_services::ModelRequestTraceCoverage {
            accepted_requests: 2,
            terminal_requests: 2,
            open_requests: 0,
        }
    );
    let metric_rows = astra_services::aggregate_model_request_metrics(&shared_pool)
        .await
        .expect("aggregate durable model request metrics");
    let metric_totals = metric_rows
        .iter()
        .filter(|row| {
            row.topology == "server_only"
                && row.provider == provider
                && row.purpose == "primary_agent"
        })
        .fold([0_u64; 5], |mut totals, row| {
            totals[0] += row.requests;
            totals[1] += row.input_tokens;
            totals[2] += row.output_tokens;
            totals[3] += row.cache_read_tokens;
            totals[4] += row.cache_creation_tokens;
            totals
        });
    assert_eq!(
        metric_totals,
        [2, 210, 24, 80, 10],
        "aggregate metrics must decode MatrixOne count/sum values and reconcile all outcomes: {metric_rows:?}"
    );

    let invocation = sqlx::query(
        "SELECT admission_token, status, input_tokens, output_tokens, cache_read_tokens,
                cache_creation_tokens, provider_response_id
         FROM inference_invocations WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(plan.invocation_id())
    .fetch_one(pool)
    .await
    .expect("load terminal invocation");
    assert_eq!(invocation.get::<String, _>("admission_token").len(), 32);
    assert_eq!(invocation.get::<String, _>("status"), "succeeded");
    assert_eq!(invocation.get::<i64, _>("input_tokens"), 120);
    assert_eq!(invocation.get::<i64, _>("output_tokens"), 24);
    assert_eq!(invocation.get::<i64, _>("cache_read_tokens"), 80);
    assert_eq!(invocation.get::<i64, _>("cache_creation_tokens"), 10);
    assert_eq!(
        invocation.get::<String, _>("provider_response_id"),
        "provider-ok"
    );

    let conflicting = InferenceInvocationTerminal::succeeded(
        InferenceUsage {
            output_tokens: 25,
            ..success.usage.clone()
        },
        success.provider_response_id.clone(),
    );
    assert_eq!(
        finish_inference_invocation(&shared_pool, &plan, &conflicting)
            .await
            .expect_err("different terminal payload must conflict")
            .kind,
        ServiceErrorKind::Conflict
    );

    // The adjacent agentic round is a different logical invocation even when
    // every other causal coordinate is unchanged. This is the online contract
    // that prevents a successful tool round from blocking the model call that
    // consumes its result.
    let mut next_round_input = input;
    let InferenceInvocationScope::Run { round, .. } = &mut next_round_input.scope else {
        unreachable!("test input uses run scope");
    };
    *round = round.saturating_add(1);
    let next_round = plan_inference_invocation(next_round_input).expect("plan adjacent round");
    assert_ne!(plan.invocation_id(), next_round.invocation_id());
    admit_inference_invocation(&shared_pool, &next_round)
        .await
        .expect("adjacent round must admit independently");

    let durable_rounds = sqlx::query(
        "SELECT round_index FROM inference_invocations
         WHERE user_id = ? AND session_id = ? AND turn_index = 4
         ORDER BY round_index",
    )
    .bind(&user_id)
    .bind(&session_id)
    .fetch_all(pool)
    .await
    .expect("load adjacent durable rounds")
    .into_iter()
    .map(|row| row.get::<i64, _>("round_index"))
    .collect::<Vec<_>>();
    assert_eq!(durable_rounds, vec![2, 3]);

    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn inference_settlement_and_retry_are_serialized_by_logical_invocation() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("inference-race-user-{suffix}");
    let session_id = format!("inference-race-session-{suffix}");
    let run_id = format!("inference-race-run-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;
    let plan = plan_inference_invocation(InferenceInvocationInput {
        user_id: user_id.clone(),
        scope: InferenceInvocationScope::Run {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            turn: 9,
            round: 1,
            operation_id: "settlement_retry_race".to_string(),
            logical_attempt: 0,
        },
        offering_id: "offer-race".to_string(),
        resolved_model_name: "race-model".to_string(),
        upstream_model_name: "race-model".to_string(),
        provider: "openai".to_string(),
        purpose: InferencePurpose::PrimaryAgent,
        execution_placement: ModelExecutionPlacement::Server,
        access_kind: ModelAccessKind::SelfHosted,
        run_authority: run_authority(),
    })
    .expect("plan race invocation");
    admit_inference_invocation(&shared_pool, &plan)
        .await
        .expect("admit race invocation");
    let first_attempt = provider_attempt(&plan, 0);
    begin_inference_provider_attempt(&shared_pool, &first_attempt)
        .await
        .expect("begin first attempt");
    let failure = InferenceInvocationTerminal {
        status: InferenceTerminalStatus::Failed,
        usage: InferenceUsage::default(),
        usage_status: InferenceUsageStatus::Unavailable,
        provider_response_id: None,
        error_kind: Some("retryable".to_string()),
        error_message: Some("provider retry decision racing final settlement".to_string()),
    };
    finish_inference_provider_attempt(&shared_pool, &first_attempt, &failure)
        .await
        .expect("finish first physical attempt");

    let retry_attempt = provider_attempt(&plan, 1);
    let (settlement, retry) = tokio::join!(
        finish_inference_invocation(&shared_pool, &plan, &failure),
        begin_inference_provider_attempt(&shared_pool, &retry_attempt)
    );
    match (settlement, retry) {
        (Ok(()), Err(error)) if error.kind == ServiceErrorKind::Conflict => {
            let status: String = sqlx::query_scalar(
                "SELECT status FROM inference_invocations WHERE user_id = ? AND invocation_id = ?",
            )
            .bind(&user_id)
            .bind(plan.invocation_id())
            .fetch_one(pool)
            .await
            .expect("load settled race invocation");
            assert_eq!(status, "failed");
        }
        (Err(error), Ok(())) if error.kind == ServiceErrorKind::Conflict => {
            let status: String = sqlx::query_scalar(
                "SELECT status FROM inference_invocations WHERE user_id = ? AND invocation_id = ?",
            )
            .bind(&user_id)
            .bind(plan.invocation_id())
            .fetch_one(pool)
            .await
            .expect("load retried race invocation");
            assert_eq!(status, "admitted");
        }
        (settlement, retry) => panic!(
            "settlement and retry must have one authoritative winner: settlement={settlement:?}, retry={retry:?}"
        ),
    }

    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn bounded_recovery_recovers_success_without_closing_retryable_attempts() {
    let (shared_pool, settings) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("reconcile-user-{suffix}");
    let session_id = format!("reconcile-session-{suffix}");
    let run_id = format!("reconcile-run-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;

    let plan = plan_inference_invocation(InferenceInvocationInput {
        user_id: user_id.clone(),
        scope: InferenceInvocationScope::Run {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            turn: 1,
            round: 0,
            operation_id: "reconcile_after_terminal_commit".to_string(),
            logical_attempt: 0,
        },
        offering_id: "offer-reconcile".to_string(),
        resolved_model_name: "reconcile-model".to_string(),
        upstream_model_name: "reconcile-model".to_string(),
        provider: "openai".to_string(),
        purpose: InferencePurpose::PrimaryAgent,
        execution_placement: ModelExecutionPlacement::Server,
        access_kind: ModelAccessKind::SelfHosted,
        run_authority: run_authority(),
    })
    .expect("plan reconciliation invocation");
    admit_inference_invocation(&shared_pool, &plan)
        .await
        .expect("admit reconciliation invocation");
    let attempt = provider_attempt(&plan, 0);
    begin_inference_provider_attempt(&shared_pool, &attempt)
        .await
        .expect("begin successful provider attempt");
    let succeeded = InferenceInvocationTerminal::succeeded(
        InferenceUsage {
            input: astra_turn_types::NormalizedPromptCacheUsage::new(9, 2, 1),
            output_tokens: 4,
        },
        Some("provider-reconciled".to_string()),
    );
    finish_inference_provider_attempt(&shared_pool, &attempt, &succeeded)
        .await
        .expect("persist successful provider attempt");
    let pending_success_debt = sqlx::query(
        "SELECT session_id, harness_run_id FROM inference_invocation_settlement_debts
         WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(plan.invocation_id())
    .fetch_one(pool)
    .await
    .expect("load successful provider recovery debt");
    assert_eq!(
        pending_success_debt
            .get::<Option<String>, _>("session_id")
            .as_deref(),
        Some(session_id.as_str()),
        "recovery evidence must carry the canonical session owner"
    );
    assert_eq!(
        pending_success_debt.get::<Option<String>, _>("harness_run_id"),
        None,
        "session recovery evidence must not fabricate a harness owner"
    );
    assert_eq!(
        begin_inference_provider_attempt(&shared_pool, &provider_attempt(&plan, 1))
            .await
            .expect_err("a successful delivery must fence duplicate provider requests")
            .kind,
        ServiceErrorKind::Conflict
    );

    let failed_plan = plan_inference_invocation(InferenceInvocationInput {
        user_id: user_id.clone(),
        scope: InferenceInvocationScope::Run {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            turn: 1,
            round: 1,
            operation_id: "reconcile_after_terminal_commit".to_string(),
            logical_attempt: 0,
        },
        offering_id: "offer-reconcile".to_string(),
        resolved_model_name: "reconcile-model".to_string(),
        upstream_model_name: "reconcile-model".to_string(),
        provider: "openai".to_string(),
        purpose: InferencePurpose::PrimaryAgent,
        execution_placement: ModelExecutionPlacement::Server,
        access_kind: ModelAccessKind::SelfHosted,
        run_authority: run_authority(),
    })
    .expect("plan failed reconciliation invocation");
    admit_inference_invocation(&shared_pool, &failed_plan)
        .await
        .expect("admit failed reconciliation invocation");
    let failed_attempt = provider_attempt(&failed_plan, 0);
    begin_inference_provider_attempt(&shared_pool, &failed_attempt)
        .await
        .expect("begin failed provider attempt");
    let failed = InferenceInvocationTerminal {
        status: InferenceTerminalStatus::Failed,
        usage: InferenceUsage::default(),
        usage_status: InferenceUsageStatus::Unavailable,
        provider_response_id: None,
        error_kind: Some("provider_unavailable".to_string()),
        error_message: Some("provider failed after delivery".to_string()),
    };
    finish_inference_provider_attempt(&shared_pool, &failed_attempt, &failed)
        .await
        .expect("persist failed provider attempt");

    reconcile_inference_settlements(&shared_pool, 256)
        .await
        .expect("bounded worker must reconcile durable inference");

    let invocation = sqlx::query(
        "SELECT status, input_tokens, output_tokens, cache_read_tokens,
                cache_creation_tokens, provider_response_id
         FROM inference_invocations WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(plan.invocation_id())
    .fetch_one(pool)
    .await
    .expect("load reconciled invocation");
    assert_eq!(invocation.get::<String, _>("status"), "succeeded");
    assert_eq!(invocation.get::<i64, _>("input_tokens"), 9);
    assert_eq!(invocation.get::<i64, _>("output_tokens"), 4);
    assert_eq!(invocation.get::<i64, _>("cache_read_tokens"), 2);
    assert_eq!(invocation.get::<i64, _>("cache_creation_tokens"), 1);
    assert_eq!(
        invocation.get::<Option<String>, _>("provider_response_id"),
        Some("provider-reconciled".to_string())
    );
    let success_debts: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM inference_invocation_settlement_debts
         WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(plan.invocation_id())
    .fetch_one(pool)
    .await
    .expect("load settled success debt");
    assert_eq!(success_debts, 0, "recovery must drain the settled debt");

    let failed_invocation = sqlx::query(
        "SELECT status
         FROM inference_invocations WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(failed_plan.invocation_id())
    .fetch_one(pool)
    .await
    .expect("load retryable invocation after recovery");
    assert_eq!(
        failed_invocation.get::<String, _>("status"),
        "admitted",
        "a failed physical attempt is not the logical retry decision"
    );
    let failed_debts: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM inference_invocation_settlement_debts
         WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(failed_plan.invocation_id())
    .fetch_one(pool)
    .await
    .expect("load retryable invocation debts");
    assert_eq!(
        failed_debts, 0,
        "a retryable attempt must not enqueue settlement"
    );

    let retry_attempt = provider_attempt(&failed_plan, 1);
    begin_inference_provider_attempt(&shared_pool, &retry_attempt)
        .await
        .expect("background recovery must not prevent the caller's retry");

    let obsolete_indexes: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
         FROM information_schema.STATISTICS
         WHERE TABLE_SCHEMA = ?
           AND ((TABLE_NAME = 'inference_invocations'
                 AND INDEX_NAME = 'idx_inference_invocations_status_created')
             OR (TABLE_NAME = 'inference_provider_attempts'
                 AND INDEX_NAME = 'idx_inference_attempts_status_started'))",
    )
    .bind(&settings.database)
    .fetch_one(pool)
    .await
    .expect("inspect inference lifecycle indexes");
    assert_eq!(
        obsolete_indexes, 0,
        "lifecycle updates must not depend on mutable-status-leading indexes"
    );

    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn session_scoped_auxiliary_inference_is_attributable_without_a_fake_run() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("aux-user-{suffix}");
    let session_id = format!("aux-session-{suffix}");
    let run_id = format!("aux-run-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;

    let plan = plan_inference_invocation(InferenceInvocationInput {
        user_id: user_id.clone(),
        scope: InferenceInvocationScope::Session {
            session_id: session_id.clone(),
            turn: 5,
            round: 0,
            operation_id: "memory_extraction".to_string(),
            logical_attempt: 0,
        },
        offering_id: "offer-memory".to_string(),
        resolved_model_name: "memory-model".to_string(),
        upstream_model_name: "memory-model".to_string(),
        provider: "openai".to_string(),
        purpose: InferencePurpose::MemoryExtraction,
        execution_placement: ModelExecutionPlacement::Server,
        access_kind: ModelAccessKind::SelfHosted,
        run_authority: None,
    })
    .expect("plan session-scoped inference");
    admit_inference_invocation(&shared_pool, &plan)
        .await
        .expect("admit session-scoped inference");

    let route = sqlx::query(
        "SELECT scope_kind, run_id FROM inference_routes
         WHERE user_id = ? AND route_id = ?",
    )
    .bind(&user_id)
    .bind(plan.route_id())
    .fetch_one(pool)
    .await
    .expect("load session-scoped route");
    assert_eq!(route.get::<String, _>("scope_kind"), "session");
    assert_eq!(route.get::<Option<String>, _>("run_id"), None);

    let attempt = provider_attempt(&plan, 0);
    begin_inference_provider_attempt(&shared_pool, &attempt)
        .await
        .expect("begin provider attempt");
    let terminal = InferenceInvocationTerminal::succeeded(
        InferenceUsage {
            input: astra_turn_types::NormalizedPromptCacheUsage::new(20, 0, 0),
            output_tokens: 4,
        },
        Some("provider-memory".to_string()),
    );
    finish_inference_provider_attempt(&shared_pool, &attempt, &terminal)
        .await
        .expect("finish provider attempt");
    finish_inference_invocation(&shared_pool, &plan, &terminal)
        .await
        .expect("finish logical invocation");

    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn concurrent_run_and_session_admission_preserve_variant_owner_shapes() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("mixed-scope-user-{suffix}");
    let session_id = format!("mixed-scope-session-{suffix}");
    let run_id = format!("mixed-scope-run-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;

    let run_plan = plan_inference_invocation(run_input(
        &user_id,
        &session_id,
        &run_id,
        0,
        "primary_agent",
    ))
    .expect("plan run-scoped inference");
    let session_plan = plan_inference_invocation(InferenceInvocationInput {
        user_id: user_id.clone(),
        scope: InferenceInvocationScope::Session {
            session_id: session_id.clone(),
            turn: 1,
            round: 0,
            operation_id: "turn_intent_judge".to_string(),
            logical_attempt: 0,
        },
        offering_id: "offer-judge".to_string(),
        resolved_model_name: "judge-model".to_string(),
        upstream_model_name: "judge-model".to_string(),
        provider: "openai".to_string(),
        purpose: InferencePurpose::VerificationJudge,
        execution_placement: ModelExecutionPlacement::Server,
        access_kind: ModelAccessKind::SelfHosted,
        run_authority: None,
    })
    .expect("plan session-scoped judge inference");

    let (run_result, session_result) = tokio::join!(
        admit_inference_invocation(&shared_pool, &run_plan),
        admit_inference_invocation(&shared_pool, &session_plan),
    );
    run_result.expect("concurrent run-scoped admission");
    session_result.expect("concurrent session-scoped admission");

    let rows = sqlx::query(
        "SELECT scope_kind, session_id, run_id, harness_run_id
         FROM inference_routes
         WHERE user_id = ? AND route_id IN (?, ?)
         ORDER BY scope_kind",
    )
    .bind(&user_id)
    .bind(run_plan.route_id())
    .bind(session_plan.route_id())
    .fetch_all(pool)
    .await
    .expect("load concurrent mixed-scope routes");
    assert_eq!(rows.len(), 2);
    for row in rows {
        let scope_kind = row.get::<String, _>("scope_kind");
        assert_eq!(
            row.get::<Option<String>, _>("session_id").as_deref(),
            Some(session_id.as_str())
        );
        assert_eq!(row.get::<Option<String>, _>("harness_run_id"), None);
        match scope_kind.as_str() {
            "run" => assert_eq!(
                row.get::<Option<String>, _>("run_id").as_deref(),
                Some(run_id.as_str())
            ),
            "session" => assert_eq!(row.get::<Option<String>, _>("run_id"), None),
            other => panic!("unexpected mixed-scope route {other}"),
        }
    }

    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn harness_inference_is_owned_without_fabricated_session_coordinates() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("harness-inf-user-{suffix}");
    let harness_run_id = format!("harness-run-{suffix}");
    seed_harness_run(pool, &user_id, &harness_run_id).await;

    let input = InferenceInvocationInput {
        user_id: user_id.clone(),
        scope: InferenceInvocationScope::HarnessRun {
            harness_run_id: harness_run_id.clone(),
            operation_id: "skillify_extract".to_string(),
            logical_attempt: 0,
        },
        offering_id: "offer-skillify".to_string(),
        resolved_model_name: "skillify-model".to_string(),
        upstream_model_name: "skillify-model".to_string(),
        provider: "openai".to_string(),
        purpose: InferencePurpose::SkillSynthesis,
        execution_placement: ModelExecutionPlacement::Server,
        access_kind: ModelAccessKind::SelfHosted,
        run_authority: None,
    };

    let mut wrong_owner_input = input.clone();
    wrong_owner_input.user_id = format!("other-{suffix}");
    let wrong_owner = plan_inference_invocation(wrong_owner_input).expect("wrong owner plan");
    assert_eq!(
        admit_inference_invocation(&shared_pool, &wrong_owner)
            .await
            .expect_err("cross-user harness ownership must reject")
            .kind,
        ServiceErrorKind::NotFound
    );

    let plan = plan_inference_invocation(input).expect("harness inference plan");
    admit_inference_invocation(&shared_pool, &plan)
        .await
        .expect("admit harness inference");
    let route = sqlx::query(
        "SELECT scope_kind, session_id, run_id, harness_run_id
         FROM inference_routes WHERE user_id = ? AND route_id = ?",
    )
    .bind(&user_id)
    .bind(plan.route_id())
    .fetch_one(pool)
    .await
    .expect("load harness route");
    assert_eq!(route.get::<String, _>("scope_kind"), "harness_run");
    assert_eq!(route.get::<Option<String>, _>("session_id"), None);
    assert_eq!(route.get::<Option<String>, _>("run_id"), None);
    assert_eq!(
        route.get::<Option<String>, _>("harness_run_id").as_deref(),
        Some(harness_run_id.as_str())
    );

    for table in [
        "model_request_context_events",
        "inference_invocation_settlement_debts",
        "inference_provider_attempts",
        "inference_invocations",
        "inference_routes",
    ] {
        let statement = format!("DELETE FROM {table} WHERE user_id = ? AND harness_run_id = ?");
        sqlx::query(&statement)
            .bind(&user_id)
            .bind(&harness_run_id)
            .execute(pool)
            .await
            .unwrap_or_else(|error| panic!("cleanup `{statement}`: {error}"));
    }
    sqlx::query("DELETE FROM harness_runs WHERE user_id = ? AND harness_run_id = ?")
        .bind(&user_id)
        .bind(&harness_run_id)
        .execute(pool)
        .await
        .expect("cleanup harness owner");
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn harness_inference_requires_running_authority_at_admission_and_provider_boundary() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("harness-state-user-{suffix}");
    let input_for = |harness_run_id: &str, logical_attempt: u32| InferenceInvocationInput {
        user_id: user_id.clone(),
        scope: InferenceInvocationScope::HarnessRun {
            harness_run_id: harness_run_id.to_string(),
            operation_id: "skillify_state_fence".to_string(),
            logical_attempt,
        },
        offering_id: "offer-skillify-state".to_string(),
        resolved_model_name: "skillify-model".to_string(),
        upstream_model_name: "skillify-model".to_string(),
        provider: "openai".to_string(),
        purpose: InferencePurpose::SkillSynthesis,
        execution_placement: ModelExecutionPlacement::Server,
        access_kind: ModelAccessKind::SelfHosted,
        run_authority: None,
    };

    let mut harness_run_ids = Vec::new();
    for (logical_attempt, status) in ["completed", "waiting_for_review", "reviewed", "failed"]
        .into_iter()
        .enumerate()
    {
        let harness_run_id = format!("harness-closed-{logical_attempt}-{suffix}");
        seed_harness_run(pool, &user_id, &harness_run_id).await;
        sqlx::query(
            "UPDATE harness_runs SET status = ?, updated_at = NOW(6)
             WHERE user_id = ? AND harness_run_id = ?",
        )
        .bind(status)
        .bind(&user_id)
        .bind(&harness_run_id)
        .execute(pool)
        .await
        .expect("close harness before inference admission");
        let plan = plan_inference_invocation(input_for(
            &harness_run_id,
            u32::try_from(logical_attempt).expect("bounded logical attempt"),
        ))
        .expect("plan closed harness inference");
        assert_eq!(
            admit_inference_invocation(&shared_pool, &plan)
                .await
                .expect_err("non-running harness must not admit inference")
                .kind,
            ServiceErrorKind::NotFound
        );
        harness_run_ids.push(harness_run_id);
    }

    let boundary_run_id = format!("harness-boundary-{suffix}");
    seed_harness_run(pool, &user_id, &boundary_run_id).await;
    let boundary_plan =
        plan_inference_invocation(input_for(&boundary_run_id, 10)).expect("boundary plan");
    admit_inference_invocation(&shared_pool, &boundary_plan)
        .await
        .expect("running harness admits logical inference");
    sqlx::query(
        "UPDATE harness_runs SET status = 'completed', updated_at = NOW(6)
         WHERE user_id = ? AND harness_run_id = ?",
    )
    .bind(&user_id)
    .bind(&boundary_run_id)
    .execute(pool)
    .await
    .expect("close harness before provider boundary");
    assert_eq!(
        begin_inference_provider_attempt(&shared_pool, &provider_attempt(&boundary_plan, 0))
            .await
            .expect_err("terminal harness must fence provider I/O")
            .kind,
        ServiceErrorKind::NotFound
    );
    harness_run_ids.push(boundary_run_id);

    for table in [
        "model_request_context_events",
        "inference_invocation_settlement_debts",
        "inference_provider_attempts",
        "inference_invocations",
        "inference_routes",
    ] {
        let statement = format!("DELETE FROM {table} WHERE user_id = ?");
        sqlx::query(&statement)
            .bind(&user_id)
            .execute(pool)
            .await
            .unwrap_or_else(|error| panic!("cleanup `{statement}`: {error}"));
    }
    for harness_run_id in harness_run_ids {
        sqlx::query("DELETE FROM harness_runs WHERE user_id = ? AND harness_run_id = ?")
            .bind(&user_id)
            .bind(&harness_run_id)
            .execute(pool)
            .await
            .expect("cleanup harness state owner");
    }
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn expired_inference_owner_recovers_every_sigkill_shape_without_old_owner_reentry() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("owner-kill-user-{suffix}");
    let session_id = format!("owner-kill-session-{suffix}");
    let run_id = format!("owner-kill-run-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;

    let pre_delivery = plan_inference_invocation(run_input(
        &user_id,
        &session_id,
        &run_id,
        10,
        "kill_after_admission",
    ))
    .expect("plan pre-delivery orphan");
    admit_inference_invocation(&shared_pool, &pre_delivery)
        .await
        .expect("admit pre-delivery orphan");
    sqlx::query(
        "UPDATE inference_invocations
         SET owner_lease_expires_at = TIMESTAMPADD(DAY, -2, NOW(6))
         WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(pre_delivery.invocation_id())
    .execute(pool)
    .await
    .expect("expire pre-delivery owner");
    reconcile_until_invocation_status(
        &shared_pool,
        pool,
        &user_id,
        pre_delivery.invocation_id(),
        "cancelled",
    )
    .await;
    let pre_delivery_fact = sqlx::query(
        "SELECT status, usage_status, provider_delivery_state, owner_generation
         FROM inference_invocations WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(pre_delivery.invocation_id())
    .fetch_one(pool)
    .await
    .expect("load pre-delivery recovery");
    assert_eq!(pre_delivery_fact.get::<String, _>("status"), "cancelled");
    assert_eq!(
        pre_delivery_fact.get::<String, _>("usage_status"),
        "unavailable"
    );
    assert_eq!(
        pre_delivery_fact.get::<String, _>("provider_delivery_state"),
        "pre_delivery"
    );
    assert_eq!(pre_delivery_fact.get::<i64, _>("owner_generation"), 2);
    let pre_delivery_attempts: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM inference_provider_attempts
         WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(pre_delivery.invocation_id())
    .fetch_one(pool)
    .await
    .expect("count pre-delivery attempts");
    assert_eq!(pre_delivery_attempts, 0, "pre-dispatch failure owns no WAL");
    assert_eq!(
        renew_inference_invocation_owner(&shared_pool, &pre_delivery)
            .await
            .expect_err("old pre-delivery owner cannot renew after recovery")
            .kind,
        ServiceErrorKind::Conflict
    );

    let delivery_unknown = plan_inference_invocation(run_input(
        &user_id,
        &session_id,
        &run_id,
        11,
        "kill_after_provider_send",
    ))
    .expect("plan delivered orphan");
    admit_inference_invocation(&shared_pool, &delivery_unknown)
        .await
        .expect("admit delivered orphan");
    let durable_base = astra_turn_types::CanonicalPrefixIdentityV1::from_messages(&[])
        .expect("empty durable base");
    let old_user = serde_json::json!({"role": "user", "content": "old request"});
    let frame_content = astra_turn_types::render_append_only_runtime_authority_frame(
        "test_authority",
        astra_turn_types::RuntimeAuthorityLifetime::NextAssistantDecision,
        "preserve this request boundary",
    )
    .expect("authority frame");
    let mut authority = serde_json::json!({"role": "user", "content": frame_content});
    astra_turn_types::mark_append_only_required_context(
        &mut authority,
        "test_authority",
        astra_turn_types::RuntimeAuthorityLifetime::NextAssistantDecision,
    );
    let transition = astra_turn_types::ProviderCanonicalTransitionV2::new_from_durable_base(
        None,
        durable_base,
        std::slice::from_ref(&old_user),
        vec![authority.clone()],
    )
    .expect("self-contained attempt transition");
    let open_attempt = provider_attempt(&delivery_unknown, 0)
        .with_canonical_transitions(&[transition])
        .expect("root attempt may own canonical WAL");
    begin_inference_provider_attempt(&shared_pool, &open_attempt)
        .await
        .expect("authorize provider delivery");
    sqlx::query(
        "UPDATE inference_invocations
         SET owner_lease_expires_at = TIMESTAMPADD(DAY, -2, NOW(6))
         WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(delivery_unknown.invocation_id())
    .execute(pool)
    .await
    .expect("expire delivered owner");
    reconcile_until_invocation_status(
        &shared_pool,
        pool,
        &user_id,
        delivery_unknown.invocation_id(),
        "delivery_unknown",
    )
    .await;
    let delivered_fact = sqlx::query(
        "SELECT invocation.status, invocation.provider_delivery_state,
                attempt.status AS attempt_status,
                (SELECT COUNT(*) FROM model_request_context_events AS context
                 WHERE context.user_id = invocation.user_id
                   AND context.invocation_id = invocation.invocation_id
                   AND context.event_stage = 'terminal') AS terminal_contexts
         FROM inference_invocations AS invocation
         JOIN inference_provider_attempts AS attempt
           ON attempt.user_id = invocation.user_id
          AND attempt.invocation_id = invocation.invocation_id
         WHERE invocation.user_id = ? AND invocation.invocation_id = ?",
    )
    .bind(&user_id)
    .bind(delivery_unknown.invocation_id())
    .fetch_one(pool)
    .await
    .expect("load delivery-unknown recovery");
    assert_eq!(
        delivered_fact.get::<String, _>("status"),
        "delivery_unknown"
    );
    assert_eq!(
        delivered_fact.get::<String, _>("provider_delivery_state"),
        "delivery_authorized"
    );
    assert_eq!(
        delivered_fact.get::<String, _>("attempt_status"),
        "delivery_unknown"
    );
    assert_eq!(delivered_fact.get::<i64, _>("terminal_contexts"), 1);
    let receipts =
        load_inference_canonical_transitions_for_session(&shared_pool, &user_id, &session_id, 1)
            .await
            .expect("delivery-unknown terminal does not brick canonical recovery");
    assert_eq!(receipts.len(), 1);
    let fresh_user = serde_json::json!({"role": "user", "content": "hi"});
    let mut restored = Vec::new();
    receipts[0].transitions[0]
        .apply_to(&mut restored)
        .expect("recover old request on its durable base");
    restored.push(fresh_user.clone());
    assert_eq!(restored, vec![old_user, authority, fresh_user]);
    assert_eq!(
        retire_inference_canonical_transitions_through_turn(
            &shared_pool,
            &user_id,
            &session_id,
            1,
        )
        .await
        .expect("retire canonically absorbed WAL"),
        1
    );
    let remaining_wal: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM inference_canonical_transition_wal
         WHERE user_id = ? AND attempt_id = ?",
    )
    .bind(&user_id)
    .bind(open_attempt.attempt_id())
    .fetch_one(pool)
    .await
    .expect("count retired WAL rows");
    assert_eq!(remaining_wal, 0);
    let audit_hash: Option<String> = sqlx::query_scalar(
        "SELECT canonical_transition_hash FROM inference_provider_attempts
         WHERE user_id = ? AND attempt_id = ?",
    )
    .bind(&user_id)
    .bind(open_attempt.attempt_id())
    .fetch_one(pool)
    .await
    .expect("load immutable WAL audit hash");
    assert!(audit_hash.is_some());

    let successor = plan_inference_invocation(run_input(
        &user_id,
        &session_id,
        &run_id,
        11,
        "after_delivery_unknown",
    ))
    .expect("plan successor invocation");
    admit_inference_invocation(&shared_pool, &successor)
        .await
        .expect("new invocation identity remains admissible");
    let successor_attempt = provider_attempt(&successor, 0);
    begin_inference_provider_attempt(&shared_pool, &successor_attempt)
        .await
        .expect("delivery-unknown old identity cannot block new provider delivery");
    let successor_terminal = InferenceInvocationTerminal {
        status: InferenceTerminalStatus::Cancelled,
        usage: InferenceUsage::default(),
        usage_status: InferenceUsageStatus::Unavailable,
        provider_response_id: None,
        error_kind: Some("test_cleanup".to_string()),
        error_message: Some("close successor attempt".to_string()),
    };
    finish_inference_provider_attempt(&shared_pool, &successor_attempt, &successor_terminal)
        .await
        .expect("finish successor attempt");
    finish_inference_invocation(&shared_pool, &successor, &successor_terminal)
        .await
        .expect("finish successor invocation");
    assert_eq!(
        begin_inference_provider_attempt(&shared_pool, &open_attempt)
            .await
            .expect_err("old attempt identity can never be delivered twice")
            .kind,
        ServiceErrorKind::Conflict
    );
    let late_terminal = InferenceInvocationTerminal {
        status: InferenceTerminalStatus::Failed,
        usage: InferenceUsage::default(),
        usage_status: InferenceUsageStatus::Unavailable,
        provider_response_id: None,
        error_kind: Some("late_old_owner".to_string()),
        error_message: Some("late terminal must lose".to_string()),
    };
    assert_eq!(
        finish_inference_provider_attempt(&shared_pool, &open_attempt, &late_terminal)
            .await
            .expect_err("old owner terminal must lose to recovered generation")
            .kind,
        ServiceErrorKind::Conflict
    );

    let exact_terminal = plan_inference_invocation(run_input(
        &user_id,
        &session_id,
        &run_id,
        12,
        "kill_after_physical_terminal",
    ))
    .expect("plan exact-terminal orphan");
    admit_inference_invocation(&shared_pool, &exact_terminal)
        .await
        .expect("admit exact-terminal orphan");
    let exact_attempt = provider_attempt(&exact_terminal, 0);
    begin_inference_provider_attempt(&shared_pool, &exact_attempt)
        .await
        .expect("begin exact-terminal attempt");
    let physical_terminal = InferenceInvocationTerminal {
        status: InferenceTerminalStatus::Failed,
        usage: InferenceUsage {
            input: astra_turn_types::NormalizedPromptCacheUsage::new(7, 3, 2),
            output_tokens: 5,
        },
        usage_status: InferenceUsageStatus::ProviderPartial,
        provider_response_id: Some("response-before-kill".to_string()),
        error_kind: Some("provider_error".to_string()),
        error_message: Some("physical terminal committed before kill".to_string()),
    };
    finish_inference_provider_attempt(&shared_pool, &exact_attempt, &physical_terminal)
        .await
        .expect("commit physical terminal before kill");
    sqlx::query(
        "UPDATE inference_invocations
         SET owner_lease_expires_at = TIMESTAMPADD(DAY, -2, NOW(6))
         WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(exact_terminal.invocation_id())
    .execute(pool)
    .await
    .expect("expire owner after physical terminal");
    reconcile_until_invocation_status(
        &shared_pool,
        pool,
        &user_id,
        exact_terminal.invocation_id(),
        "failed",
    )
    .await;
    let exact_fact = sqlx::query(
        "SELECT invocation.status, invocation.terminal_fingerprint,
                invocation.usage_status, invocation.input_tokens, invocation.output_tokens,
                invocation.cache_read_tokens, invocation.cache_creation_tokens,
                attempt.terminal_fingerprint AS attempt_fingerprint
         FROM inference_invocations AS invocation
         JOIN inference_provider_attempts AS attempt
           ON attempt.user_id = invocation.user_id
          AND attempt.invocation_id = invocation.invocation_id
         WHERE invocation.user_id = ? AND invocation.invocation_id = ?",
    )
    .bind(&user_id)
    .bind(exact_terminal.invocation_id())
    .fetch_one(pool)
    .await
    .expect("load mirrored physical terminal");
    assert_eq!(exact_fact.get::<String, _>("status"), "failed");
    assert_eq!(
        exact_fact.get::<String, _>("usage_status"),
        "provider_partial"
    );
    assert_eq!(exact_fact.get::<i64, _>("input_tokens"), 7);
    assert_eq!(exact_fact.get::<i64, _>("output_tokens"), 5);
    assert_eq!(exact_fact.get::<i64, _>("cache_read_tokens"), 3);
    assert_eq!(exact_fact.get::<i64, _>("cache_creation_tokens"), 2);
    assert_eq!(
        exact_fact.get::<String, _>("terminal_fingerprint"),
        exact_fact.get::<String, _>("attempt_fingerprint")
    );

    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn heartbeat_and_expiry_finish_race_have_one_durable_owner() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("owner-race-user-{suffix}");
    let session_id = format!("owner-race-session-{suffix}");
    let run_id = format!("owner-race-run-{suffix}");
    seed_run(pool, &user_id, &session_id, &run_id).await;

    let live = plan_inference_invocation(run_input(
        &user_id,
        &session_id,
        &run_id,
        20,
        "heartbeat_survives_sweeps",
    ))
    .expect("plan heartbeat invocation");
    admit_inference_invocation(&shared_pool, &live)
        .await
        .expect("admit heartbeat invocation");
    for _ in 0..3 {
        renew_inference_invocation_owner(&shared_pool, &live)
            .await
            .expect("renew live owner");
        reconcile_inference_settlements(&shared_pool, 8)
            .await
            .expect("sweep around live heartbeat");
    }
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT status FROM inference_invocations WHERE user_id = ? AND invocation_id = ?",
        )
        .bind(&user_id)
        .bind(live.invocation_id())
        .fetch_one(pool)
        .await
        .expect("load live heartbeat status"),
        "admitted"
    );

    let raced = plan_inference_invocation(run_input(
        &user_id,
        &session_id,
        &run_id,
        21,
        "expiry_vs_late_finish",
    ))
    .expect("plan expiry race");
    admit_inference_invocation(&shared_pool, &raced)
        .await
        .expect("admit expiry race");
    let attempt = provider_attempt(&raced, 0);
    begin_inference_provider_attempt(&shared_pool, &attempt)
        .await
        .expect("begin raced attempt");
    sqlx::query(
        "UPDATE inference_invocations
         SET owner_lease_expires_at = TIMESTAMPADD(SECOND, -1, NOW(6))
         WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(raced.invocation_id())
    .execute(pool)
    .await
    .expect("expire raced owner");
    let late = InferenceInvocationTerminal {
        status: InferenceTerminalStatus::Failed,
        usage: InferenceUsage::default(),
        usage_status: InferenceUsageStatus::Unavailable,
        provider_response_id: None,
        error_kind: Some("late_finish".to_string()),
        error_message: Some("old pod woke after lease expiry".to_string()),
    };
    let (sweep, late_finish) = tokio::join!(
        reconcile_inference_settlements(&shared_pool, 8),
        finish_inference_provider_attempt(&shared_pool, &attempt, &late),
    );
    sweep.expect("expiry recovery wins race");
    assert_eq!(
        late_finish
            .expect_err("expired owner cannot win a late terminal race")
            .kind,
        ServiceErrorKind::Conflict
    );
    let raced_fact = sqlx::query(
        "SELECT status, owner_generation, provider_delivery_state
         FROM inference_invocations WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(raced.invocation_id())
    .fetch_one(pool)
    .await
    .expect("load expiry race winner");
    assert_eq!(raced_fact.get::<String, _>("status"), "delivery_unknown");
    assert_eq!(raced_fact.get::<i64, _>("owner_generation"), 2);
    assert_eq!(
        raced_fact.get::<String, _>("provider_delivery_state"),
        "delivery_authorized"
    );

    cleanup(pool, &user_id, &session_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn expired_owner_batch_is_bounded_and_fair_across_300_plus_invocations() {
    let started = std::time::Instant::now();
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    // A killed prior invocation of this exact live-DB test may leave its
    // intentionally orphaned fixture behind. Reclaim only this test's
    // namespace so the first bounded batch measures the 320 rows seeded below.
    for table in [
        "model_request_context_events",
        "inference_invocation_settlement_debts",
        "inference_provider_attempts",
        "inference_invocations",
        "inference_routes",
    ] {
        let statement = format!("DELETE FROM {table} WHERE user_id LIKE 'lease-fair-%'");
        sqlx::query(&statement)
            .execute(pool)
            .await
            .unwrap_or_else(|error| panic!("cleanup stale fair fixture `{statement}`: {error}"));
    }
    let suffix = Uuid::new_v4().simple().to_string();
    let user_prefix = format!("lease-fair-{suffix}");
    let noisy_user = format!("{user_prefix}-noisy");
    let mut tx = pool.begin().await.expect("begin fair orphan seed");
    for index in 0..320_u32 {
        let user_id = if index < 256 {
            noisy_user.clone()
        } else {
            format!("{user_prefix}-quiet-{index}")
        };
        let session_id = format!("fair-session-{index}-{suffix}");
        let route_id = format!("fair-route-{index}-{suffix}");
        let invocation_id = format!("fair-inv-{index}-{suffix}");
        sqlx::query(
            "INSERT INTO inference_routes
             (route_id, user_id, session_id, scope_kind, run_id, harness_run_id,
              offering_id, resolved_model_name, upstream_model_name, provider,
              execution_placement, access_kind, purpose, created_at)
             VALUES (?, ?, ?, 'session', NULL, NULL, 'fair-offering', 'fair-model',
                     'fair-model', 'openai', 'server', 'self_hosted',
                     'primary_agent', NOW(6))",
        )
        .bind(&route_id)
        .bind(&user_id)
        .bind(&session_id)
        .execute(&mut *tx)
        .await
        .expect("seed fair route");
        sqlx::query(
            "INSERT INTO inference_invocations
             (invocation_id, route_id, user_id, session_id, scope_kind, run_id,
              harness_run_id, admission_token, owner_token, owner_generation,
              owner_lease_expires_at, turn_index, round_index, operation_id,
              logical_attempt, purpose, status, terminal_fingerprint, usage_status,
              provider_delivery_state, created_at, terminal_at)
             VALUES (?, ?, ?, ?, 'session', NULL, NULL, ?, ?, 1,
                     TIMESTAMP('1970-01-01 00:00:00.000001'), 1, ?, 'fair_recovery', 0,
                     'primary_agent', 'admitted', NULL, 'unavailable', 'unknown',
                     NOW(6), NULL)",
        )
        .bind(&invocation_id)
        .bind(&route_id)
        .bind(&user_id)
        .bind(&session_id)
        .bind(Uuid::new_v4().simple().to_string())
        .bind(Uuid::new_v4().simple().to_string())
        .bind(i64::from(index))
        .execute(&mut *tx)
        .await
        .expect("seed fair expired invocation");
    }
    tx.commit().await.expect("commit fair orphan seed");

    let first_sweep = reconcile_inference_settlements(&shared_pool, 256)
        .await
        .expect("recover first bounded fair batch");
    assert!(
        first_sweep <= 256,
        "one global settlement sweep must respect its requested bound"
    );
    let quiet_recovered = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM inference_invocations
         WHERE user_id LIKE ? AND user_id <> ? AND status = 'cancelled'",
    )
    .bind(format!("{user_prefix}-quiet-%"))
    .bind(&noisy_user)
    .fetch_one(pool)
    .await
    .expect("count quiet owners in first recovery batch");
    assert!(
        quiet_recovered >= 64,
        "one noisy user must not starve any quiet owner in the bounded batch"
    );
    let total_recovered = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM inference_invocations
         WHERE user_id LIKE ? AND status = 'cancelled'",
    )
    .bind(format!("{user_prefix}%"))
    .fetch_one(pool)
    .await
    .expect("count first bounded recovery batch");
    assert!(
        (128..=320).contains(&total_recovered),
        "at least one fair orphan batch must complete; concurrent global sweepers may complete more"
    );

    let mut converged = total_recovered;
    let mut productive_sweeps = 1_u32;
    for _ in 0..4 {
        if converged == 320 {
            break;
        }
        let recovered = reconcile_inference_settlements(&shared_pool, 256)
            .await
            .expect("converge remaining fair recovery backlog");
        let next_converged = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM inference_invocations
             WHERE user_id LIKE ? AND status = 'cancelled'",
        )
        .bind(format!("{user_prefix}%"))
        .fetch_one(pool)
        .await
        .expect("count fair recovery progress");
        if next_converged > converged || recovered > 0 {
            productive_sweeps = productive_sweeps.saturating_add(1);
        }
        converged = next_converged;
    }
    assert_eq!(
        converged, 320,
        "the bounded fair backlog must fully converge"
    );
    let terminal_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM inference_invocations
         WHERE user_id LIKE ? AND status = 'cancelled'",
    )
    .bind(format!("{user_prefix}%"))
    .fetch_one(pool)
    .await
    .expect("count fully converged fair backlog");
    assert_eq!(terminal_count, 320);
    let non_terminal = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM inference_invocations
         WHERE user_id LIKE ?
           AND (status = 'admitted' OR terminal_fingerprint IS NULL)",
    )
    .bind(format!("{user_prefix}%"))
    .fetch_one(pool)
    .await
    .expect("count residual logical orphans");
    assert_eq!(non_terminal, 0);
    let started_attempts = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM inference_provider_attempts
         WHERE user_id LIKE ? AND status = 'started'",
    )
    .bind(format!("{user_prefix}%"))
    .fetch_one(pool)
    .await
    .expect("count residual physical orphans");
    assert_eq!(started_attempts, 0);
    eprintln!(
        "fair owner recovery converged 320 invocations in {productive_sweeps} productive sweeps over {:?}",
        started.elapsed()
    );

    for table in [
        "model_request_context_events",
        "inference_invocation_settlement_debts",
        "inference_provider_attempts",
        "inference_invocations",
        "inference_routes",
    ] {
        let statement = format!("DELETE FROM {table} WHERE user_id LIKE ?");
        sqlx::query(&statement)
            .bind(format!("{user_prefix}%"))
            .execute(pool)
            .await
            .unwrap_or_else(|error| panic!("cleanup `{statement}`: {error}"));
    }
}
