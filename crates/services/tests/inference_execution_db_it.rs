//! Live MatrixOne coverage for the durable inference execution boundary.
//!
//! Run with:
//! ASTRA_TEST_DB_IT=1 cargo test -p astra-services \
//!   --test inference_execution_db_it -- --ignored --test-threads=1

mod common;

use astra_services::{
    InferenceInvocationInput, InferenceInvocationTerminal, InferenceTerminalStatus, InferenceUsage,
    ModelAccessKind, ModelExecutionPlacement, ServiceErrorKind, admit_inference_invocation,
    begin_inference_provider_attempt, finish_inference_invocation,
    finish_inference_provider_attempt, plan_inference_invocation, plan_inference_provider_attempt,
};
use astra_turn_types::{InferenceInvocationScope, InferencePurpose};
use serial_test::serial;
use sqlx::Row;
use uuid::Uuid;

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

async fn cleanup(pool: &sqlx::Pool<sqlx::MySql>, user_id: &str, session_id: &str, run_id: &str) {
    for (statement, identity) in [
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
async fn inference_admission_attempts_and_terminal_state_form_one_durable_contract() {
    let (shared_pool, _) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("inf-user-{suffix}");
    let session_id = format!("inf-session-{suffix}");
    let run_id = format!("inf-run-{suffix}");
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
        provider: "openai".to_string(),
        purpose: InferencePurpose::PrimaryAgent,
        execution_placement: ModelExecutionPlacement::Server,
        access_kind: ModelAccessKind::SelfHosted,
    };
    let plan = plan_inference_invocation(input.clone()).expect("plan invocation");

    let mut wrong_owner_input = input;
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

    let first_attempt = plan_inference_provider_attempt(&plan, 0);
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

    let second_attempt = plan_inference_provider_attempt(&plan, 1);
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

    let attempts = sqlx::query(
        "SELECT attempt_index, status, input_tokens, output_tokens,
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
    assert_eq!(attempts[0].get::<i64, _>("attempt_index"), 0);
    assert_eq!(attempts[0].get::<String, _>("status"), "failed");
    assert_eq!(attempts[1].get::<i64, _>("attempt_index"), 1);
    assert_eq!(attempts[1].get::<String, _>("status"), "succeeded");
    assert_eq!(attempts[1].get::<i64, _>("input_tokens"), 120);
    assert_eq!(attempts[1].get::<i64, _>("output_tokens"), 24);
    assert_eq!(attempts[1].get::<i64, _>("cache_read_tokens"), 80);
    assert_eq!(attempts[1].get::<i64, _>("cache_creation_tokens"), 10);

    let invocation = sqlx::query(
        "SELECT status, input_tokens, output_tokens, cache_read_tokens,
                cache_creation_tokens, provider_response_id
         FROM inference_invocations WHERE user_id = ? AND invocation_id = ?",
    )
    .bind(&user_id)
    .bind(plan.invocation_id())
    .fetch_one(pool)
    .await
    .expect("load terminal invocation");
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

    let attempt = plan_inference_provider_attempt(&plan, 0);
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
