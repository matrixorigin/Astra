//! Live MatrixOne coverage for durable tool invocation compare-and-set.
//!
//! Run with:
//! ASTRA_TEST_DB_IT=1 ASTRA_AUTO_CREATE_DATABASE=1 cargo test -p astra-services \
//!   --test tool_invocation_ledger_db_it -- --ignored --test-threads=1

mod common;

use astra_services::tool_invocation_ledger::{
    DatabaseToolInvocationLedger, ToolInvocationLedgerStoreError,
};
use astra_turn_types::{
    DispatchCertainty, DurableToolReference, ToolInvocationDecision, ToolInvocationFingerprint,
    ToolInvocationIdentity, ToolInvocationPrepareOutcome, ToolInvocationResultPayload,
    ToolInvocationState, ToolInvocationTerminalOutcome,
};
use serde_json::json;
use std::collections::BTreeMap;
use uuid::Uuid;

fn identity(prefix: &str, invocation_id: &str) -> ToolInvocationIdentity {
    ToolInvocationIdentity::new(
        format!("invocation-user-{prefix}"),
        format!("invocation-session-{prefix}"),
        format!("invocation-run-{prefix}"),
        format!("invocation-turn-{prefix}"),
        invocation_id,
    )
    .unwrap()
}

fn fingerprint(command: &str) -> ToolInvocationFingerprint {
    fingerprint_with_decision(command, &decision())
}

fn fingerprint_with_decision(
    command: &str,
    decision: &ToolInvocationDecision,
) -> ToolInvocationFingerprint {
    ToolInvocationFingerprint::new(
        DurableToolReference::built_in("bash", "registry-v1").unwrap(),
        &json!({"command": command}),
        &decision.decision_id,
    )
    .unwrap()
}

fn decision() -> ToolInvocationDecision {
    ToolInvocationDecision::new(&json!({"route": "server_local"})).unwrap()
}

fn named_decision(policy: &str) -> ToolInvocationDecision {
    ToolInvocationDecision::new(&json!({"route": "server_local", "policy": policy})).unwrap()
}

fn success(output: &str) -> ToolInvocationTerminalOutcome {
    ToolInvocationTerminalOutcome::Succeeded {
        result: ToolInvocationResultPayload {
            output: output.to_string(),
            metadata: BTreeMap::new(),
            exit_semantics: None,
        },
    }
}

fn failure(output: &str) -> ToolInvocationTerminalOutcome {
    ToolInvocationTerminalOutcome::Failed {
        result: ToolInvocationResultPayload {
            output: output.to_string(),
            metadata: BTreeMap::from([("provider_request_id".to_string(), json!("req-9"))]),
            exit_semantics: Some("execution_error".to_string()),
        },
        error_kind: Some("provider_failure".to_string()),
        retryable: false,
    }
}

async fn cleanup(pool: &sqlx::Pool<sqlx::MySql>, identity: &ToolInvocationIdentity) {
    let _ = sqlx::query("DELETE FROM tool_invocation_ledger WHERE user_id = ? AND session_id = ?")
        .bind(&identity.user_id)
        .bind(&identity.session_id)
        .execute(pool)
        .await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn invocation_identity_conflict_and_state_cas_hold_on_live_matrixone() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    let prefix = Uuid::new_v4().simple().to_string();
    let first = identity(&prefix, "call-1");
    let second = identity(&prefix, "call-2");
    cleanup(&pool, &first).await;
    let ledger = DatabaseToolInvocationLedger::new(shared);
    let original = fingerprint("deploy");
    let original_decision = decision();

    assert!(matches!(
        ledger
            .prepare(&first, &original, &original_decision)
            .await
            .unwrap(),
        ToolInvocationPrepareOutcome::Prepared(record)
            if record.state == ToolInvocationState::Prepared
                && record.attempt_count == 0
                && record.decision == original_decision
    ));
    let changed_decision = named_decision("changed-after-prepare");
    let changed_fingerprint = fingerprint_with_decision("deploy", &changed_decision);
    assert!(matches!(
        ledger
            .prepare(&first, &changed_fingerprint, &changed_decision)
            .await
            .unwrap(),
        ToolInvocationPrepareOutcome::Existing(record)
            if record.decision == original_decision
                && record.fingerprint == original
    ));
    assert!(matches!(
        ledger.prepare(&first, &original, &decision()).await.unwrap(),
        ToolInvocationPrepareOutcome::Existing(record)
            if record.state == ToolInvocationState::Prepared
    ));
    assert!(matches!(
        ledger
            .prepare(&first, &fingerprint("destroy"), &decision())
            .await,
        Err(ToolInvocationLedgerStoreError::IdentityConflict { .. })
    ));

    // Equal arguments under a different invocation ID remain distinct intent.
    assert!(matches!(
        ledger
            .prepare(&second, &original, &decision())
            .await
            .unwrap(),
        ToolInvocationPrepareOutcome::Prepared(_)
    ));

    let dispatch = |ledger: DatabaseToolInvocationLedger, identity: ToolInvocationIdentity| async move {
        ledger
            .compare_and_transition(
                &identity,
                ToolInvocationState::Prepared,
                ToolInvocationState::Dispatched,
                DispatchCertainty::Dispatched,
            )
            .await
    };
    let (race_a, race_b) = tokio::join!(
        dispatch(ledger.clone(), second.clone()),
        dispatch(ledger.clone(), second.clone())
    );
    let race_results = [race_a, race_b];
    assert_eq!(
        race_results.iter().filter(|result| result.is_ok()).count(),
        1,
        "exactly one worker must claim Prepared -> Dispatched: {race_results:?}"
    );
    assert_eq!(
        race_results
            .iter()
            .filter(|result| matches!(
                result,
                Err(ToolInvocationLedgerStoreError::StateMismatch {
                    actual: ToolInvocationState::Dispatched,
                    ..
                })
            ))
            .count(),
        1,
        "the losing worker must observe the authoritative dispatched state: {race_results:?}"
    );
    assert!(matches!(
        ledger
            .compare_and_transition(
                &second,
                ToolInvocationState::Dispatched,
                ToolInvocationState::Failed,
                DispatchCertainty::Dispatched,
            )
            .await,
        Err(ToolInvocationLedgerStoreError::TerminalOutcomeRequired {
            state: ToolInvocationState::Failed
        })
    ));
    let failed = ledger
        .compare_and_complete(
            &second,
            ToolInvocationState::Dispatched,
            &failure("provider failed"),
        )
        .await
        .unwrap();
    assert_eq!(failed.state, ToolInvocationState::Failed);
    assert_eq!(failed.outcome.unwrap().result().output, "provider failed");
    assert!(matches!(
        ledger.prepare(&second, &original, &decision()).await.unwrap(),
        ToolInvocationPrepareOutcome::Existing(record)
            if record.state == ToolInvocationState::Failed
                && record.outcome.as_ref().unwrap().result().output == "provider failed"
    ));

    let dispatched = ledger
        .compare_and_transition(
            &first,
            ToolInvocationState::Prepared,
            ToolInvocationState::Dispatched,
            DispatchCertainty::Dispatched,
        )
        .await
        .unwrap();
    assert_eq!(dispatched.attempt_count, 1);
    assert!(matches!(
        ledger
            .compare_and_transition(
                &first,
                ToolInvocationState::Prepared,
                ToolInvocationState::Dispatched,
                DispatchCertainty::Dispatched,
            )
            .await,
        Err(ToolInvocationLedgerStoreError::StateMismatch {
            actual: ToolInvocationState::Dispatched,
            ..
        })
    ));

    ledger
        .compare_and_transition(
            &first,
            ToolInvocationState::Dispatched,
            ToolInvocationState::OutcomeUnknown,
            DispatchCertainty::Unknown,
        )
        .await
        .unwrap();
    assert!(matches!(
        ledger
            .compare_and_transition(
                &first,
                ToolInvocationState::OutcomeUnknown,
                ToolInvocationState::Dispatched,
                DispatchCertainty::Dispatched,
            )
            .await,
        Err(ToolInvocationLedgerStoreError::IllegalTransition { .. })
    ));
    let reconciled = ledger
        .compare_and_complete(
            &first,
            ToolInvocationState::OutcomeUnknown,
            &success("deployed"),
        )
        .await
        .unwrap();
    assert_eq!(reconciled.state, ToolInvocationState::Succeeded);
    assert_eq!(reconciled.attempt_count, 1);
    assert_eq!(reconciled.outcome.unwrap().result().output, "deployed");

    sqlx::query(
        "UPDATE tool_invocation_ledger SET outcome_json = NULL
         WHERE user_id = ? AND session_id = ? AND run_id = ?
           AND turn_chain_id = ? AND invocation_id = ?",
    )
    .bind(&first.user_id)
    .bind(&first.session_id)
    .bind(&first.run_id)
    .bind(&first.turn_chain_id)
    .bind(&first.invocation_id)
    .execute(&pool)
    .await
    .unwrap();
    assert!(matches!(
        ledger.get(&first).await,
        Err(ToolInvocationLedgerStoreError::Contract(_))
    ));

    cleanup(&pool, &first).await;
}
