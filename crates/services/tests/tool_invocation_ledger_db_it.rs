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
    DispatchCertainty, DurableToolReference, ToolInvocationCompletionSource,
    ToolInvocationDecision, ToolInvocationFingerprint, ToolInvocationIdentity,
    ToolInvocationPrepareOutcome, ToolInvocationResultPayload, ToolInvocationState,
    ToolInvocationTerminalOutcome,
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
    let _ = sqlx::query(
        "DELETE FROM tool_invocation_archive_chunks WHERE user_id = ? AND session_id = ?",
    )
    .bind(&identity.user_id)
    .bind(&identity.session_id)
    .execute(pool)
    .await;
    let _ =
        sqlx::query("DELETE FROM session_artifact_references WHERE user_id = ? AND session_id = ?")
            .bind(&identity.user_id)
            .bind(&identity.session_id)
            .execute(pool)
            .await;
    let _ = sqlx::query("DELETE FROM session_artifacts WHERE user_id = ? AND session_id = ?")
        .bind(&identity.user_id)
        .bind(&identity.session_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM agent_runs WHERE user_id = ? AND run_id = ?")
        .bind(&identity.user_id)
        .bind(&identity.run_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM agent_sessions WHERE user_id = ? AND session_id = ?")
        .bind(&identity.user_id)
        .bind(&identity.session_id)
        .execute(pool)
        .await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn terminal_run_compaction_atomically_preserves_replay_and_blocks_new_dispatch() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    let prefix = Uuid::new_v4().simple().to_string();
    let identities = (0..3)
        .map(|index| identity(&prefix, &format!("archive-call-{index}")))
        .collect::<Vec<_>>();
    cleanup(&pool, &identities[0]).await;
    insert_active_run(&pool, &identities[0]).await;
    let ledger = DatabaseToolInvocationLedger::new(shared);
    let invocation_fingerprint = fingerprint("archive-result");
    let invocation_decision = decision();
    let mut expected = Vec::new();
    for (index, identity) in identities.iter().enumerate() {
        ledger
            .prepare(identity, &invocation_fingerprint, &invocation_decision)
            .await
            .unwrap();
        ledger
            .claim_dispatch(identity, "archive-worker", 90_000)
            .await
            .unwrap();
        expected.push(
            ledger
                .compare_and_complete(
                    identity,
                    ToolInvocationState::Dispatched,
                    Some("archive-worker"),
                    &success(&format!("archived output {index}")),
                )
                .await
                .unwrap(),
        );
    }
    assert!(matches!(
        ledger
            .compact_terminal_run_batch(
                &identities[0].user_id,
                &identities[0].session_id,
                &identities[0].run_id,
            )
            .await,
        Err(ToolInvocationLedgerStoreError::RunNotTerminal { .. })
    ));
    let stranded = identity(&prefix, "prepared-at-run-closure");
    ledger
        .prepare(&stranded, &invocation_fingerprint, &invocation_decision)
        .await
        .unwrap();
    sqlx::query("UPDATE agent_runs SET status = 'completed' WHERE user_id = ? AND run_id = ?")
        .bind(&identities[0].user_id)
        .bind(&identities[0].run_id)
        .execute(&pool)
        .await
        .unwrap();

    assert!(matches!(
        ledger
            .compact_terminal_run_batch(
                &identities[0].user_id,
                &identities[0].session_id,
                &identities[0].run_id,
            )
            .await,
        Err(ToolInvocationLedgerStoreError::RunNotQuiescent {
            non_terminal_count: 1,
            ..
        })
    ));
    let artifact_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM session_artifacts
         WHERE user_id = ? AND session_id = ? AND artifact_kind = 'tool_invocation_archive_v1'",
    )
    .bind(&identities[0].user_id)
    .bind(&identities[0].session_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        artifact_count, 0,
        "a non-quiescent run must not partially compact"
    );
    sqlx::query(
        "DELETE FROM tool_invocation_ledger
         WHERE user_id = ? AND session_id = ? AND run_id = ?
           AND turn_chain_id = ? AND invocation_id = ?",
    )
    .bind(&stranded.user_id)
    .bind(&stranded.session_id)
    .bind(&stranded.run_id)
    .bind(&stranded.turn_chain_id)
    .bind(&stranded.invocation_id)
    .execute(&pool)
    .await
    .unwrap();

    let compacted = ledger
        .compact_terminal_run_batch(
            &identities[0].user_id,
            &identities[0].session_id,
            &identities[0].run_id,
        )
        .await
        .unwrap();
    assert_eq!(compacted.archived_records, 3);
    assert_eq!(compacted.remaining_records, 0);
    assert!(compacted.artifact_id.is_some());
    let hot_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM tool_invocation_ledger
         WHERE user_id = ? AND session_id = ? AND run_id = ?",
    )
    .bind(&identities[0].user_id)
    .bind(&identities[0].session_id)
    .bind(&identities[0].run_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(hot_rows, 0);
    for (identity, expected) in identities.iter().zip(expected) {
        assert_eq!(ledger.get(identity).await.unwrap(), Some(expected));
    }
    let replay = ledger
        .compact_terminal_run_batch(
            &identities[0].user_id,
            &identities[0].session_id,
            &identities[0].run_id,
        )
        .await
        .unwrap();
    assert_eq!(replay.archived_records, 0);
    assert_eq!(replay.artifact_id, None);
    let stale = identity(&prefix, "stale-after-compaction");
    assert!(matches!(
        ledger
            .prepare(&stale, &invocation_fingerprint, &invocation_decision)
            .await,
        Err(ToolInvocationLedgerStoreError::RunNotExecutable { .. })
    ));
    sqlx::query(
        "UPDATE session_artifacts SET status = 'expired'
         WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
    )
    .bind(&identities[0].user_id)
    .bind(&identities[0].session_id)
    .bind(compacted.artifact_id.as_deref().unwrap())
    .execute(&pool)
    .await
    .unwrap();
    assert!(matches!(
        ledger.get(&identities[0]).await,
        Err(ToolInvocationLedgerStoreError::ArchiveUnavailable { .. })
    ));

    cleanup(&pool, &identities[0]).await;
}

async fn insert_active_run(pool: &sqlx::Pool<sqlx::MySql>, identity: &ToolInvocationIdentity) {
    sqlx::query(
        "INSERT INTO agent_sessions
         (session_id, user_id, title, status, event_count)
         VALUES (?, ?, 'tool invocation ledger test', 'active', 0)",
    )
    .bind(&identity.session_id)
    .bind(&identity.user_id)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agent_runs
         (run_id, user_id, session_id, root_run_id, ancestor_path, status)
         VALUES (?, ?, ?, ?, ?, 'running')",
    )
    .bind(&identity.run_id)
    .bind(&identity.user_id)
    .bind(&identity.session_id)
    .bind(&identity.run_id)
    .bind(format!("/{}", identity.run_id))
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn invocation_identity_conflict_and_state_cas_hold_on_live_matrixone() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    let prefix = Uuid::new_v4().simple().to_string();
    let first = identity(&prefix, "call-1");
    let second = identity(&prefix, "call-2");
    let abandoned = identity(&prefix, "call-abandoned");
    let cached = identity(&prefix, "call-cached");
    cleanup(&pool, &first).await;
    insert_active_run(&pool, &first).await;
    let ledger = DatabaseToolInvocationLedger::new(shared);
    let original = fingerprint("deploy");
    let original_decision = decision();

    let wrong_session = ToolInvocationIdentity::new(
        &first.user_id,
        "another-session",
        &first.run_id,
        &first.turn_chain_id,
        "call-wrong-session",
    )
    .unwrap();
    assert!(matches!(
        ledger
            .prepare(&wrong_session, &original, &original_decision)
            .await,
        Err(ToolInvocationLedgerStoreError::RunSessionMismatch { .. })
    ));

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

    ledger
        .prepare(&cached, &original, &decision())
        .await
        .unwrap();
    let cached_record = ledger
        .complete_from_semantic_read_cache(
            &cached,
            &ToolInvocationResultPayload {
                output: "cached observation".to_string(),
                metadata: BTreeMap::new(),
                exit_semantics: None,
            },
            &ToolInvocationCompletionSource::semantic_read_cache(
                format!("sha256:{}", "a".repeat(64)),
                format!("sha256:{}", "b".repeat(64)),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cached_record.state, ToolInvocationState::Succeeded);
    assert_eq!(
        cached_record.dispatch_certainty,
        DispatchCertainty::NotDispatched
    );
    assert_eq!(cached_record.attempt_count, 0);
    assert!(cached_record.dispatch_lease.is_none());
    assert!(cached_record.completion_source.is_some());
    assert_eq!(ledger.get(&cached).await.unwrap().unwrap(), cached_record);
    assert!(matches!(
        ledger.claim_dispatch(&cached, "worker-cache", 90_000).await,
        Err(ToolInvocationLedgerStoreError::StateMismatch {
            actual: ToolInvocationState::Succeeded,
            ..
        })
    ));

    let dispatch = |ledger: DatabaseToolInvocationLedger,
                    identity: ToolInvocationIdentity,
                    owner: &'static str| async move {
        ledger.claim_dispatch(&identity, owner, 90_000).await
    };
    let (race_a, race_b) = tokio::join!(
        dispatch(ledger.clone(), second.clone(), "worker-a"),
        dispatch(ledger.clone(), second.clone(), "worker-b")
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
    let winning_owner = race_results
        .iter()
        .find_map(|result| {
            result
                .as_ref()
                .ok()
                .and_then(|record| record.dispatch_lease.as_ref())
                .map(|lease| lease.owner_id.clone())
        })
        .expect("winning dispatch owner");
    assert!(matches!(
        ledger
            .compare_and_complete(
                &second,
                ToolInvocationState::Dispatched,
                Some("not-the-owner"),
                &failure("wrong owner"),
            )
            .await,
        Err(ToolInvocationLedgerStoreError::DispatchOwnerMismatch { .. })
    ));
    let failed = ledger
        .compare_and_complete(
            &second,
            ToolInvocationState::Dispatched,
            Some(&winning_owner),
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

    ledger
        .prepare(&abandoned, &original, &decision())
        .await
        .unwrap();
    ledger
        .claim_dispatch(&abandoned, "worker-abandoned", 90_000)
        .await
        .unwrap();
    assert!(matches!(
        ledger.reconcile_expired_dispatch(&abandoned).await.unwrap(),
        record if record.state == ToolInvocationState::Dispatched
    ));
    let wrong_owner_renewal = ledger
        .renew_dispatch(&abandoned, "wrong-worker", 90_000)
        .await;
    assert!(
        matches!(
            wrong_owner_renewal,
            Err(ToolInvocationLedgerStoreError::DispatchOwnerMismatch { .. })
        ),
        "wrong owner must not renew another worker's dispatch: {wrong_owner_renewal:?}"
    );
    sqlx::query(
        "UPDATE tool_invocation_ledger
         SET dispatch_lease_expires_at = TIMESTAMPADD(SECOND, -1, CURRENT_TIMESTAMP(6))
         WHERE user_id = ? AND session_id = ? AND run_id = ?
           AND turn_chain_id = ? AND invocation_id = ?",
    )
    .bind(&abandoned.user_id)
    .bind(&abandoned.session_id)
    .bind(&abandoned.run_id)
    .bind(&abandoned.turn_chain_id)
    .bind(&abandoned.invocation_id)
    .execute(&pool)
    .await
    .unwrap();
    let expired = ledger.reconcile_expired_dispatch(&abandoned).await.unwrap();
    assert_eq!(expired.state, ToolInvocationState::OutcomeUnknown);
    assert!(matches!(
        ledger
            .claim_dispatch(&abandoned, "replacement-worker", 90_000)
            .await,
        Err(ToolInvocationLedgerStoreError::StateMismatch {
            actual: ToolInvocationState::OutcomeUnknown,
            ..
        })
    ));

    let dispatched = ledger
        .claim_dispatch(&first, "worker-first", 90_000)
        .await
        .unwrap();
    assert_eq!(dispatched.attempt_count, 1);
    assert!(matches!(
        ledger.claim_dispatch(&first, "worker-other", 90_000).await,
        Err(ToolInvocationLedgerStoreError::StateMismatch {
            actual: ToolInvocationState::Dispatched,
            ..
        })
    ));

    ledger
        .mark_outcome_unknown(&first, "worker-first")
        .await
        .unwrap();
    assert!(matches!(
        ledger.claim_dispatch(&first, "worker-other", 90_000).await,
        Err(ToolInvocationLedgerStoreError::StateMismatch { .. })
    ));
    let reconciled = ledger
        .compare_and_complete(
            &first,
            ToolInvocationState::OutcomeUnknown,
            None,
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

    sqlx::query(
        "UPDATE agent_runs SET status = 'completed'
         WHERE user_id = ? AND run_id = ?",
    )
    .bind(&first.user_id)
    .bind(&first.run_id)
    .execute(&pool)
    .await
    .unwrap();
    let after_closure = identity(&prefix, "call-after-run-closure");
    assert!(matches!(
        ledger
            .prepare(&after_closure, &original, &original_decision)
            .await,
        Err(ToolInvocationLedgerStoreError::RunNotExecutable { status, .. })
            if status == "completed"
    ));

    cleanup(&pool, &first).await;
}
