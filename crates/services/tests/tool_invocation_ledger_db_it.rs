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
    DispatchCertainty, DurableToolReference, ToolInvocationFingerprint, ToolInvocationIdentity,
    ToolInvocationPrepareOutcome, ToolInvocationState,
};
use serde_json::json;
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
    ToolInvocationFingerprint::new(
        DurableToolReference::built_in("bash", "registry-v1").unwrap(),
        &json!({"command": command}),
        "policy-v1",
    )
    .unwrap()
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

    assert!(matches!(
        ledger.prepare(&first, &original).await.unwrap(),
        ToolInvocationPrepareOutcome::Prepared(record)
            if record.state == ToolInvocationState::Prepared && record.attempt_count == 0
    ));
    assert!(matches!(
        ledger.prepare(&first, &original).await.unwrap(),
        ToolInvocationPrepareOutcome::Existing(record)
            if record.state == ToolInvocationState::Prepared
    ));
    assert!(matches!(
        ledger.prepare(&first, &fingerprint("destroy")).await,
        Err(ToolInvocationLedgerStoreError::IdentityConflict { .. })
    ));

    // Equal arguments under a different invocation ID remain distinct intent.
    assert!(matches!(
        ledger.prepare(&second, &original).await.unwrap(),
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
        .compare_and_transition(
            &first,
            ToolInvocationState::OutcomeUnknown,
            ToolInvocationState::Succeeded,
            DispatchCertainty::Dispatched,
        )
        .await
        .unwrap();
    assert_eq!(reconciled.state, ToolInvocationState::Succeeded);
    assert_eq!(reconciled.attempt_count, 1);

    cleanup(&pool, &first).await;
}
