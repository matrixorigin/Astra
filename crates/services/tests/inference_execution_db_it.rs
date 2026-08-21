//! Live MatrixOne coverage for the durable inference execution boundary.
//!
//! Run with:
//! ASTRA_TEST_DB_IT=1 cargo test -p astra-services \
//!   --test inference_execution_db_it -- --ignored --test-threads=1

mod common;

use astra_services::runtime_maintenance::{RuntimeMaintenancePolicy, maintain_runtime_storage};
use astra_services::{
    InferenceInvocationInput, InferenceInvocationPlan, InferenceInvocationTerminal,
    InferenceProviderAttemptPlan, InferenceProviderWireIdentity, InferenceTerminalStatus,
    InferenceUsage, ModelAccessKind, ModelExecutionPlacement, ServiceErrorKind,
    admit_inference_invocation, begin_inference_provider_attempt, declare_inference_settlement,
    finish_inference_invocation, finish_inference_provider_attempt, plan_inference_invocation,
    plan_inference_provider_attempt, reconcile_inference_settlements,
};
use astra_turn_types::{InferenceInvocationScope, InferencePurpose};
use serial_test::serial;
use sqlx::Row;
use uuid::Uuid;

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
          status, execution_mode, run_generation, last_event_idx, retry_count,
          total_prompt_tokens, total_completion_tokens, total_tool_calls,
          created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, 0, 'node', 'running', 'web_agent', 0, -1, 0,
                 0, 0, 0, NOW(6), NOW(6))",
    )
    .bind(run_id)
    .bind(user_id)
    .bind(session_id)
    .bind(run_id)
    .bind(run_id)
    .execute(pool)
    .await
    .expect("seed inference run");
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
                    input_tokens: 3,
                    output_tokens: 2,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                },
                Some(format!("targeted-response-{round}")),
            ),
        )
        .await
        .expect("persist targeted recovery attempt");
        plans.push(plan);
    }
    plans.sort_by(|left, right| left.invocation_id().cmp(right.invocation_id()));

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
    for row in rows {
        let invocation_id = row.get::<String, _>("invocation_id");
        let status = row.get::<String, _>("status");
        let debt_count = row.get::<i64, _>("debt_count");
        if invocation_id == plans[0].invocation_id() {
            assert_eq!(status, "succeeded");
            assert_eq!(debt_count, 0);
        } else {
            assert_eq!(invocation_id, plans[1].invocation_id());
            assert_eq!(status, "admitted");
            assert_eq!(debt_count, 1);
        }
    }

    reconcile_inference_settlements(&shared_pool, 1)
        .await
        .expect("drain the second invocation before cleanup");
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

    let reconciled = reconcile_inference_settlements(&shared_pool, 256)
        .await
        .expect("one invalid debt must not block a later valid settlement");
    assert_eq!(reconciled, 1);

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
        let plan = plan_inference_invocation(InferenceInvocationInput {
            user_id: user_id.clone(),
            scope,
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
            input_tokens: 120,
            output_tokens: 24,
            cache_read_tokens: 80,
            cache_creation_tokens: 10,
        },
        Some("provider-ok".to_string()),
    );
    finish_inference_provider_attempt(&shared_pool, &second_attempt, &success)
        .await
        .expect("finish retry");
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
    let (first_terminal, concurrent_terminal) = tokio::join!(
        finish_inference_invocation(&shared_pool, &plan, &success),
        finish_inference_invocation(&shared_pool, &plan, &success)
    );
    first_terminal.expect("finish logical invocation");
    concurrent_terminal.expect("concurrent exact invocation terminal is idempotent");
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
            .fresh_input_tokens,
        30
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
        [2, 120, 24, 80, 10],
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
            input_tokens: 9,
            output_tokens: 4,
            cache_read_tokens: 2,
            cache_creation_tokens: 1,
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
            input_tokens: 20,
            output_tokens: 4,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
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
