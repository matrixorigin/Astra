//! Live MatrixOne coverage for durable tool invocation compare-and-set.
//!
//! Run with:
//! ASTRA_TEST_DB_IT=1 ASTRA_AUTO_CREATE_DATABASE=1 cargo test -p astra-services \
//!   --test tool_invocation_ledger_db_it -- --ignored --test-threads=1

mod common;

use astra_services::tool_invocation_ledger::{
    DatabaseToolInvocationLedger, ToolInvocationDispatchAdmission, ToolInvocationLedgerStoreError,
    ToolInvocationPreparationProbe,
};
use astra_services::{
    BeginSessionExecutionSwitchV1, DatabaseSessionContextCoordinator,
    SessionExecutionBindingStateV1, SessionExecutionBindingV1,
};
use astra_turn_types::{
    DispatchCertainty, DurableToolReference, SessionKeyV1, ToolInvocationCompletionSource,
    ToolInvocationDecision, ToolInvocationFingerprint, ToolInvocationIdentity,
    ToolInvocationPrepareOutcome, ToolInvocationResultPayload, ToolInvocationState,
    ToolInvocationTerminalOutcome,
};
use serde_json::json;
use std::collections::BTreeMap;
use std::time::Duration;
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

fn dispatch_admission() -> ToolInvocationDispatchAdmission {
    ToolInvocationDispatchAdmission {
        expected_control_epoch: -1,
        expected_owner_generation: 0,
        expected_owner_pod_id: "tool-invocation-ledger-test-owner".to_string(),
        expected_execution_binding_generation: None,
    }
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
    let _ = sqlx::query(
        "DELETE FROM session_execution_bindings WHERE isolation_domain = 'server' \
         AND owner_user_id = ? AND session_id = ? AND branch_id = 'main'",
    )
    .bind(&identity.user_id)
    .bind(&identity.session_id)
    .execute(pool)
    .await;
    let _ = sqlx::query("DELETE FROM agent_run_events WHERE user_id = ? AND run_id = ?")
        .bind(&identity.user_id)
        .bind(&identity.run_id)
        .execute(pool)
        .await;
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

async fn append_user_intent(
    pool: &sqlx::Pool<sqlx::MySql>,
    identity: &ToolInvocationIdentity,
    intent_id: &str,
) {
    let mut tx = pool.begin().await.unwrap();
    let previous_event_index: i64 = sqlx::query_scalar(
        "SELECT last_event_idx FROM agent_runs
         WHERE user_id = ? AND run_id = ? FOR UPDATE",
    )
    .bind(&identity.user_id)
    .bind(&identity.run_id)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    let event_index = previous_event_index + 1;
    let payload = json!({
        "event_type": "user_intent",
        "idempotency_key": format!("user_intent:{intent_id}"),
        "data": {
            "intent_id": intent_id,
            "delivery": "guide_current_run",
            "input": {"content": "replace stale work"},
        },
    });
    let updated = sqlx::query(
        "UPDATE agent_runs SET last_event_idx = ?
         WHERE user_id = ? AND run_id = ? AND last_event_idx = ?",
    )
    .bind(event_index)
    .bind(&identity.user_id)
    .bind(&identity.run_id)
    .bind(previous_event_index)
    .execute(&mut *tx)
    .await
    .unwrap();
    assert_eq!(updated.rows_affected(), 1);
    sqlx::query(
        "INSERT INTO agent_run_events
         (id, run_id, event_idx, user_id, session_id, event_type, event_id,
          idempotency_key, event_hash, producer_pod_id, payload_json, created_at)
         VALUES (?, ?, ?, ?, ?, 'user_intent', ?, ?, ?,
                 'tool-invocation-ledger-test-owner', ?, NOW(6))",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(&identity.run_id)
    .bind(event_index)
    .bind(&identity.user_id)
    .bind(&identity.session_id)
    .bind(intent_id)
    .bind(format!("user_intent:{intent_id}"))
    .bind(format!("{:064x}", 1))
    .bind(payload.to_string())
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn large_terminal_outcome_commits_and_roundtrips_without_a_varchar_projection_limit() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    let prefix = Uuid::new_v4().simple().to_string();
    let invocation = identity(&prefix, "large-result");
    cleanup(&pool, &invocation).await;
    insert_active_run(&pool, &invocation).await;

    let ledger = DatabaseToolInvocationLedger::new(shared.clone());
    let invocation_fingerprint = fingerprint("large-result");
    let invocation_decision = decision();
    ledger
        .prepare(&invocation, &invocation_fingerprint, &invocation_decision)
        .await
        .unwrap();
    ledger
        .claim_dispatch(
            &invocation,
            "large-result-worker",
            90_000,
            dispatch_admission(),
        )
        .await
        .unwrap();

    // MatrixOne's CAST(JSON AS CHAR) is bounded to VARCHAR(65535). Real tool
    // outcomes can exceed that once a truncated command result and its typed
    // metadata share one envelope; committing the terminal state must not
    // roll back merely because the same transaction reads that JSON back.
    let expected = success(&"x".repeat(70_000));
    let completed = ledger
        .compare_and_complete(
            &invocation,
            ToolInvocationState::Dispatched,
            Some("large-result-worker"),
            &expected,
        )
        .await
        .unwrap();

    assert_eq!(completed.state, ToolInvocationState::Succeeded);
    assert_eq!(completed.outcome.as_ref(), Some(&expected));
    assert_eq!(ledger.get(&invocation).await.unwrap(), Some(completed));

    let cached = identity(&prefix, "large-cache-result");
    ledger
        .prepare(&cached, &invocation_fingerprint, &invocation_decision)
        .await
        .unwrap();
    let source = ToolInvocationCompletionSource::semantic_read_cache(
        format!("sha256:{}", "a".repeat(64)),
        format!("sha256:{}", "b".repeat(64)),
    )
    .unwrap();
    let cached_record = ledger
        .complete_from_semantic_read_cache(&cached, expected.result(), &source)
        .await
        .unwrap();
    assert_eq!(cached_record.outcome.as_ref(), Some(&expected));
    assert_eq!(ledger.get(&cached).await.unwrap(), Some(cached_record));

    cleanup(&pool, &invocation).await;
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
            .claim_dispatch(identity, "archive-worker", 90_000, dispatch_admission())
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
    let result_artifact_id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO session_artifacts
         (artifact_id, session_id, user_id, artifact_kind, content_json,
          retention_until, status, created_at)
         VALUES (?, ?, ?, 'tool_result_evidence_v1', '{}',
                 TIMESTAMPADD(DAY, 1, CURRENT_TIMESTAMP(6)), 'active', CURRENT_TIMESTAMP(6))",
    )
    .bind(&result_artifact_id)
    .bind(&identities[0].session_id)
    .bind(&identities[0].user_id)
    .execute(&pool)
    .await
    .unwrap();
    let result_retention_before: String = sqlx::query_scalar(
        "SELECT CAST(retention_until AS CHAR) FROM session_artifacts
         WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
    )
    .bind(&identities[0].user_id)
    .bind(&identities[0].session_id)
    .bind(&result_artifact_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO session_artifact_references
         (user_id, session_id, artifact_id, reference_kind, reference_id)
         VALUES (?, ?, ?, 'invocation_ledger', ?)",
    )
    .bind(&identities[0].user_id)
    .bind(&identities[0].session_id)
    .bind(&result_artifact_id)
    .bind(identities[0].storage_key())
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
        Err(ToolInvocationLedgerStoreError::RunNotTerminal { .. })
    ));
    let stranded = identity(&prefix, "prepared-at-run-closure");
    ledger
        .prepare(&stranded, &invocation_fingerprint, &invocation_decision)
        .await
        .unwrap();
    let expired_dispatch = identity(&prefix, "expired-dispatch-at-run-closure");
    ledger
        .prepare(
            &expired_dispatch,
            &invocation_fingerprint,
            &invocation_decision,
        )
        .await
        .unwrap();
    ledger
        .claim_dispatch(
            &expired_dispatch,
            "expired-worker",
            90_000,
            dispatch_admission(),
        )
        .await
        .unwrap();
    sqlx::query(
        "UPDATE tool_invocation_ledger
         SET dispatch_lease_expires_at = TIMESTAMPADD(SECOND, -1, CURRENT_TIMESTAMP(6))
         WHERE user_id = ? AND session_id = ? AND run_id = ?
           AND turn_chain_id = ? AND invocation_id = ?",
    )
    .bind(&expired_dispatch.user_id)
    .bind(&expired_dispatch.session_id)
    .bind(&expired_dispatch.run_id)
    .bind(&expired_dispatch.turn_chain_id)
    .bind(&expired_dispatch.invocation_id)
    .execute(&pool)
    .await
    .unwrap();
    let active_dispatch = identity(&prefix, "active-dispatch-at-run-closure");
    ledger
        .prepare(
            &active_dispatch,
            &invocation_fingerprint,
            &invocation_decision,
        )
        .await
        .unwrap();
    ledger
        .claim_dispatch(
            &active_dispatch,
            "active-worker",
            90_000,
            dispatch_admission(),
        )
        .await
        .unwrap();
    sqlx::query("UPDATE agent_runs SET status = 'completed' WHERE user_id = ? AND run_id = ?")
        .bind(&identities[0].user_id)
        .bind(&identities[0].run_id)
        .execute(&pool)
        .await
        .unwrap();

    let first_reconciliation = ledger
        .reconcile_terminal_run(
            &identities[0].user_id,
            &identities[0].session_id,
            &identities[0].run_id,
        )
        .await
        .unwrap();
    assert_eq!(first_reconciliation.prepared_rejected, 1);
    assert_eq!(first_reconciliation.inconsistent_prepared_unknown, 0);
    assert_eq!(first_reconciliation.expired_dispatches_unknown, 1);
    assert_eq!(first_reconciliation.active_dispatches_remaining, 1);
    let rejected = ledger.get(&stranded).await.unwrap().unwrap();
    assert_eq!(rejected.state, ToolInvocationState::Rejected);
    assert_eq!(
        rejected.dispatch_certainty,
        DispatchCertainty::NotDispatched
    );
    assert_eq!(rejected.attempt_count, 0);
    assert!(matches!(
        rejected.completion_source,
        Some(ToolInvocationCompletionSource::RunClosure { .. })
    ));
    assert!(matches!(
        ledger
            .prepare(&stranded, &invocation_fingerprint, &invocation_decision)
            .await
            .unwrap(),
        ToolInvocationPrepareOutcome::Existing(record)
            if record.state == ToolInvocationState::Rejected
                && record.dispatch_certainty == DispatchCertainty::NotDispatched
    ));
    assert_eq!(
        ledger.get(&expired_dispatch).await.unwrap().unwrap().state,
        ToolInvocationState::OutcomeUnknown
    );

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
        "UPDATE tool_invocation_ledger
         SET dispatch_lease_expires_at = TIMESTAMPADD(SECOND, -1, CURRENT_TIMESTAMP(6))
         WHERE user_id = ? AND session_id = ? AND run_id = ?
           AND turn_chain_id = ? AND invocation_id = ?",
    )
    .bind(&active_dispatch.user_id)
    .bind(&active_dispatch.session_id)
    .bind(&active_dispatch.run_id)
    .bind(&active_dispatch.turn_chain_id)
    .bind(&active_dispatch.invocation_id)
    .execute(&pool)
    .await
    .unwrap();
    let second_reconciliation = ledger
        .reconcile_terminal_run(
            &identities[0].user_id,
            &identities[0].session_id,
            &identities[0].run_id,
        )
        .await
        .unwrap();
    assert_eq!(second_reconciliation.prepared_rejected, 0);
    assert_eq!(second_reconciliation.expired_dispatches_unknown, 1);
    assert_eq!(second_reconciliation.active_dispatches_remaining, 0);
    let reconciliation_events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_events
         WHERE user_id = ? AND session_id = ? AND run_id = ?
           AND event_type = 'tool_invocation_run_reconciled'",
    )
    .bind(&identities[0].user_id)
    .bind(&identities[0].session_id)
    .bind(&identities[0].run_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(reconciliation_events, 2);
    let deferred_events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_events
         WHERE user_id = ? AND session_id = ? AND run_id = ?
           AND event_type = 'tool_invocation_compaction_deferred'",
    )
    .bind(&identities[0].user_id)
    .bind(&identities[0].session_id)
    .bind(&identities[0].run_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(deferred_events, 1, "active lease deferral is recorded once");
    let diagnostics = ledger
        .lifecycle_diagnostics(
            &identities[0].user_id,
            &identities[0].session_id,
            Some(&identities[0].run_id),
        )
        .await
        .unwrap();
    assert_eq!(diagnostics.hot_total, 6);
    assert_eq!(diagnostics.succeeded, 3);
    assert_eq!(diagnostics.rejected, 1);
    assert_eq!(diagnostics.rejected_without_dispatch, 1);
    assert_eq!(diagnostics.outcome_unknown, 2);
    assert_eq!(diagnostics.durable_artifact_references, 1);
    assert_eq!(diagnostics.reconciliation_events, 2);
    assert_eq!(diagnostics.compaction_deferred_events, 1);

    let compacted = ledger
        .compact_terminal_run_batch(
            &identities[0].user_id,
            &identities[0].session_id,
            &identities[0].run_id,
        )
        .await
        .unwrap();
    // Outcome-unknown effects stay in the hot ledger. They are unresolved
    // provider-boundary facts and must remain visible to Session recovery
    // admission after terminal records are compacted.
    assert_eq!(compacted.archived_records, 4);
    assert_eq!(compacted.remaining_records, 2);
    assert!(compacted.artifact_id.is_some());
    let archived_diagnostics = ledger
        .lifecycle_diagnostics(
            &identities[0].user_id,
            &identities[0].session_id,
            Some(&identities[0].run_id),
        )
        .await
        .unwrap();
    assert_eq!(archived_diagnostics.hot_total, 2);
    assert_eq!(archived_diagnostics.archive_chunks, 1);
    assert_eq!(archived_diagnostics.durable_artifact_references, 2);
    let result_reference: (String, String) = sqlx::query_as(
        "SELECT refs.reference_id,
                CAST(artifacts.retention_until AS CHAR)
         FROM session_artifact_references refs
         JOIN session_artifacts artifacts
           ON artifacts.user_id = refs.user_id
          AND artifacts.session_id = refs.session_id
          AND artifacts.artifact_id = refs.artifact_id
         WHERE refs.user_id = ? AND refs.session_id = ? AND refs.artifact_id = ?
           AND refs.reference_kind = 'invocation_ledger'",
    )
    .bind(&identities[0].user_id)
    .bind(&identities[0].session_id)
    .bind(&result_artifact_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(result_reference.0, identities[0].run_id);
    assert_eq!(
        result_reference.1, result_retention_before,
        "reachability transfer must not rewrite the artifact retention policy"
    );
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
    assert_eq!(hot_rows, 2);
    for (identity, expected) in identities.iter().zip(&expected) {
        assert_eq!(ledger.get(identity).await.unwrap().as_ref(), Some(expected));
        let ToolInvocationPreparationProbe::Missing(miss) = ledger
            .probe_for_prepare(identity, &invocation_fingerprint)
            .await
            .unwrap()
        else {
            panic!("compacted invocation remained hot");
        };
        assert!(matches!(
            ledger
                .finish_prepare_after_miss(miss, &invocation_decision)
                .await
                .unwrap(),
            ToolInvocationPrepareOutcome::Existing(record) if record == *expected
        ));
        assert!(matches!(
            ledger
                .prepare(identity, &invocation_fingerprint, &invocation_decision)
                .await
                .unwrap(),
            ToolInvocationPrepareOutcome::Existing(record) if record == *expected
        ));
    }
    assert!(matches!(
        ledger
            .prepare(
                &identities[0],
                &fingerprint("conflicting archived invocation"),
                &invocation_decision,
            )
            .await,
        Err(ToolInvocationLedgerStoreError::IdentityConflict { .. })
    ));
    let ToolInvocationPreparationProbe::Missing(conflicting_miss) = ledger
        .probe_for_prepare(
            &identities[0],
            &fingerprint("conflicting archived invocation"),
        )
        .await
        .unwrap()
    else {
        panic!("compacted invocation remained hot");
    };
    assert!(matches!(
        ledger
            .finish_prepare_after_miss(conflicting_miss, &invocation_decision)
            .await,
        Err(ToolInvocationLedgerStoreError::IdentityConflict { .. })
    ));
    let replay = ledger
        .compact_terminal_run_batch(
            &identities[0].user_id,
            &identities[0].session_id,
            &identities[0].run_id,
        )
        .await
        .unwrap();
    assert_eq!(replay.archived_records, 0);
    assert_eq!(replay.remaining_records, 2);
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

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn dispatch_fails_closed_when_the_work_provider_generation_is_stale() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    let prefix = Uuid::new_v4().simple().to_string();
    let invocation = identity(&prefix, "stale-provider-generation");
    cleanup(&pool, &invocation).await;
    let mut binding =
        SessionExecutionBindingV1::server_work_default(format!("work:{prefix}:branch:main"));
    binding.physical_workspace_id = Some(
        SessionExecutionBindingV1::edge_materialization_physical_identity(
            "source",
            "/workspace/source",
        ),
    );
    binding.workspace = astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::EdgeWorkspace,
        display_name: None,
        root: Some("/workspace/source".into()),
        source: Some(astra_services::runs::WorkspaceSourceRequest::EdgePath {
            path: "/workspace/source".into(),
        }),
        authority: Some(astra_services::runs::WorkspaceAuthorityRequest::ReadWrite),
    };
    binding.executor = astra_services::runs::ExecutorBindingRequest {
        kind: astra_services::runs::ExecutorBindingRequestKind::EdgeAgent,
        executor_id: Some("source-edge".into()),
        display_name: None,
        transport: Some(astra_services::runs::ToolTransportKindRequest::EdgeLedger),
        status: Some(astra_services::runs::ExecutorStatusRequest::Online),
    };
    sqlx::query(
        "INSERT INTO session_execution_bindings \
         (isolation_domain, owner_user_id, session_id, branch_id, generation, binding_json) \
         VALUES ('server', ?, ?, 'main', ?, ?)",
    )
    .bind(&invocation.user_id)
    .bind(&invocation.session_id)
    .bind(i64::try_from(binding.generation).unwrap())
    .bind(serde_json::to_string(&binding).unwrap())
    .execute(&pool)
    .await
    .unwrap();
    insert_active_run(&pool, &invocation).await;

    let ledger = DatabaseToolInvocationLedger::new(shared.clone());
    let invocation_fingerprint = fingerprint("stale provider selection");
    ledger
        .prepare(&invocation, &invocation_fingerprint, &decision())
        .await
        .unwrap();
    let coordinator = DatabaseSessionContextCoordinator::new(shared.clone());
    let key = SessionKeyV1::owner_session(
        "server",
        &invocation.user_id,
        &invocation.session_id,
        "main",
    );
    let actor = astra_turn_types::ActorContextV1::owner_user(
        &invocation.user_id,
        "dispatch-generation-db-it",
        astra_turn_types::ActorKindV1::Server,
        astra_turn_types::SessionSurfaceV1::Server,
        None,
        astra_turn_types::AuthorityEpochsV1::default(),
    );
    let attachment = common::controller_attachment(&shared, &key, actor).await;
    let mut switching = binding.clone();
    switching.generation = 2;
    switching.state = SessionExecutionBindingStateV1::Switching;
    switching.executor.executor_id = Some("target-edge".into());
    switching.physical_workspace_id = Some(
        SessionExecutionBindingV1::edge_materialization_physical_identity(
            "target",
            "/workspace/target",
        ),
    );
    switching.workspace.root = Some("/workspace/target".into());
    switching.workspace.source = Some(astra_services::runs::WorkspaceSourceRequest::EdgePath {
        path: "/workspace/target".into(),
    });
    assert!(matches!(
        coordinator
            .begin_execution_switch(
                &key,
                &BeginSessionExecutionSwitchV1 {
                    request_id: "blocked-provider-switch".into(),
                    operation_id: "blocked-provider-operation".into(),
                    controller_attachment_id: attachment.attachment_id,
                    expected_generation: 1,
                    target: switching,
                    source_evidence: json!({
                        "schema_version": 1, "root": "/workspace/source", "head": "head",
                        "tree": "tree", "object_format": "sha1", "reference": "main",
                        "repository": "repo", "clean": true,
                    }),
                }
            )
            .await,
        Err(astra_services::SessionContextCoordinatorError::ExecutionBindingBusy)
    ));
    let mut stale_admission = dispatch_admission();
    stale_admission.expected_execution_binding_generation = Some(2);
    assert!(matches!(
        ledger
            .claim_dispatch(&invocation, "stale-worker", 90_000, stale_admission)
            .await,
        Err(ToolInvocationLedgerStoreError::ExecutionBindingFenced {
            expected: 2,
            current: Some(1),
        })
    ));

    let mut current_admission = dispatch_admission();
    current_admission.expected_execution_binding_generation = Some(1);
    let dispatched = ledger
        .claim_dispatch(&invocation, "current-worker", 90_000, current_admission)
        .await
        .expect("the exact ready provider generation may dispatch");
    assert_eq!(dispatched.state, ToolInvocationState::Dispatched);
    sqlx::query("DELETE FROM session_attachments WHERE isolation_domain = 'server' AND owner_user_id = ? AND session_id = ? AND branch_id = 'main'")
        .bind(&invocation.user_id).bind(&invocation.session_id).execute(&pool).await.unwrap();
    cleanup(&pool, &invocation).await;
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
         (run_id, user_id, session_id, root_run_id, ancestor_path, status,
          owner_pod_id, owner_lease_expires_at, run_generation)
         VALUES (?, ?, ?, ?, ?, 'running', 'tool-invocation-ledger-test-owner',
                 TIMESTAMPADD(MINUTE, 10, NOW(6)), 0)",
    )
    .bind(&identity.run_id)
    .bind(&identity.user_id)
    .bind(&identity.session_id)
    .bind(&identity.run_id)
    // Root runs use their bare run id as the canonical ancestry path. A
    // leading separator describes no valid root→child chain and production
    // execution authority correctly rejects it.
    .bind(&identity.run_id)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn dispatch_claim_atomically_orders_guidance_owner_and_invocation_state() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    let prefix = Uuid::new_v4().simple().to_string();
    let first = identity(&prefix, "intent-first");
    cleanup(&pool, &first).await;
    insert_active_run(&pool, &first).await;
    let ledger = DatabaseToolInvocationLedger::new(shared);
    let invocation_fingerprint = fingerprint("atomic dispatch");
    let invocation_decision = decision();

    ledger
        .prepare(&first, &invocation_fingerprint, &invocation_decision)
        .await
        .unwrap();
    append_user_intent(&pool, &first, "intent-first").await;
    for _ in 0..2 {
        assert!(matches!(
            ledger
                .claim_dispatch(&first, "worker", 90_000, dispatch_admission())
                .await,
            Err(ToolInvocationLedgerStoreError::ActionSuperseded {
                user_intent_event_index: 0,
                ..
            })
        ));
    }
    let rejected = ledger.get(&first).await.unwrap().unwrap();
    assert_eq!(rejected.state, ToolInvocationState::Rejected);
    assert_eq!(
        rejected.dispatch_certainty,
        DispatchCertainty::NotDispatched
    );
    assert_eq!(rejected.attempt_count, 0);
    assert!(matches!(
        rejected.completion_source,
        Some(ToolInvocationCompletionSource::SupersededByGuidance {
            user_intent_event_index: 0,
            ..
        })
    ));
    let grant_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_run_events
         WHERE user_id = ? AND run_id = ? AND event_type = 'action_admission_granted'",
    )
    .bind(&first.user_id)
    .bind(&first.run_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(grant_count, 0);

    let wrong_owner = identity(&prefix, "wrong-owner");
    ledger
        .prepare(&wrong_owner, &invocation_fingerprint, &invocation_decision)
        .await
        .unwrap();
    let mut wrong_owner_admission = dispatch_admission();
    wrong_owner_admission.expected_control_epoch = 0;
    wrong_owner_admission.expected_owner_pod_id = "stale-owner".to_string();
    assert!(matches!(
        ledger
            .claim_dispatch(&wrong_owner, "worker", 90_000, wrong_owner_admission)
            .await,
        Err(ToolInvocationLedgerStoreError::RunOwnerMismatch { .. })
    ));
    assert_eq!(
        ledger.get(&wrong_owner).await.unwrap().unwrap().state,
        ToolInvocationState::Prepared
    );

    let stale_generation = identity(&prefix, "stale-generation");
    ledger
        .prepare(
            &stale_generation,
            &invocation_fingerprint,
            &invocation_decision,
        )
        .await
        .unwrap();
    let mut stale_generation_admission = dispatch_admission();
    stale_generation_admission.expected_control_epoch = 0;
    stale_generation_admission.expected_owner_generation = 1;
    assert!(matches!(
        ledger
            .claim_dispatch(
                &stale_generation,
                "worker",
                90_000,
                stale_generation_admission,
            )
            .await,
        Err(ToolInvocationLedgerStoreError::RunOwnerGenerationMismatch { .. })
    ));

    let missing = identity(&prefix, "missing-prepared-row");
    let mut observed_admission = dispatch_admission();
    observed_admission.expected_control_epoch = 0;
    assert!(matches!(
        ledger
            .claim_dispatch(&missing, "worker", 90_000, observed_admission.clone())
            .await,
        Err(ToolInvocationLedgerStoreError::NotFound { .. })
    ));
    let grants_after_rollback: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_run_events
         WHERE user_id = ? AND run_id = ? AND event_type = 'action_admission_granted'",
    )
    .bind(&first.user_id)
    .bind(&first.run_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        grants_after_rollback, 0,
        "failed claim must roll back its grant"
    );

    let admitted = identity(&prefix, "claim-first");
    ledger
        .prepare(&admitted, &invocation_fingerprint, &invocation_decision)
        .await
        .unwrap();
    let dispatched = ledger
        .claim_dispatch(&admitted, "worker", 90_000, observed_admission.clone())
        .await
        .unwrap();
    assert_eq!(dispatched.state, ToolInvocationState::Dispatched);
    assert_eq!(dispatched.attempt_count, 1);
    assert!(matches!(
        ledger
            .claim_dispatch(&admitted, "worker-2", 90_000, observed_admission)
            .await,
        Err(ToolInvocationLedgerStoreError::ActionAlreadyStarted { .. })
    ));
    assert_eq!(
        ledger.get(&admitted).await.unwrap().unwrap().attempt_count,
        1,
        "an already-dispatched invocation must never be replayed"
    );

    // Per-call linearization is the batch contract: A may claim before new
    // guidance, while B, which has not claimed yet, must close as superseded
    // without receiving a dispatch grant.
    append_user_intent(&pool, &first, "between-concurrent-claims").await;
    let pending_batch_member = identity(&prefix, "batch-member-after-guidance");
    ledger
        .prepare(
            &pending_batch_member,
            &invocation_fingerprint,
            &invocation_decision,
        )
        .await
        .unwrap();
    let mut between_claims_admission = dispatch_admission();
    between_claims_admission.expected_control_epoch = 1;
    assert!(matches!(
        ledger
            .claim_dispatch(
                &pending_batch_member,
                "worker-b",
                90_000,
                between_claims_admission,
            )
            .await,
        Err(ToolInvocationLedgerStoreError::ActionSuperseded {
            user_intent_event_index: 2,
            ..
        })
    ));
    let pending_batch_member = ledger.get(&pending_batch_member).await.unwrap().unwrap();
    assert_eq!(pending_batch_member.state, ToolInvocationState::Rejected);
    assert_eq!(
        pending_batch_member.dispatch_certainty,
        DispatchCertainty::NotDispatched
    );
    assert_eq!(pending_batch_member.attempt_count, 0);

    let expired_lease = identity(&prefix, "expired-lease");
    ledger
        .prepare(
            &expired_lease,
            &invocation_fingerprint,
            &invocation_decision,
        )
        .await
        .unwrap();
    sqlx::query(
        "UPDATE agent_runs SET owner_lease_expires_at = TIMESTAMPADD(SECOND, -1, NOW(6))
         WHERE user_id = ? AND run_id = ?",
    )
    .bind(&first.user_id)
    .bind(&first.run_id)
    .execute(&pool)
    .await
    .unwrap();
    let mut expired_admission = dispatch_admission();
    expired_admission.expected_control_epoch = 0;
    assert!(matches!(
        ledger
            .claim_dispatch(&expired_lease, "worker", 90_000, expired_admission)
            .await,
        Err(ToolInvocationLedgerStoreError::ActionAdmissionFailed { .. })
    ));
    assert_eq!(
        ledger.get(&expired_lease).await.unwrap().unwrap().state,
        ToolInvocationState::Prepared
    );
    let final_grant_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_run_events
         WHERE user_id = ? AND run_id = ? AND event_type = 'action_admission_granted'",
    )
    .bind(&first.user_id)
    .bind(&first.run_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        final_grant_count, 1,
        "only A's successful claim may leave an admission grant"
    );

    cleanup(&pool, &first).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn split_prepare_reuses_one_initial_probe_and_keeps_transactional_winner() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    let first = identity(&Uuid::new_v4().simple().to_string(), "split-call");
    cleanup(&pool, &first).await;
    insert_active_run(&pool, &first).await;
    let ledger = DatabaseToolInvocationLedger::new(shared);
    let original = fingerprint("inspect");
    let original_decision = decision();
    let competing_decision = named_decision("competing");

    let ToolInvocationPreparationProbe::Missing(first_miss) =
        ledger.probe_for_prepare(&first, &original).await.unwrap()
    else {
        panic!("fresh invocation already existed");
    };
    let ToolInvocationPreparationProbe::Missing(racing_miss) =
        ledger.probe_for_prepare(&first, &original).await.unwrap()
    else {
        panic!("uncommitted invocation became visible");
    };
    assert!(matches!(
        ledger
            .finish_prepare_after_miss(first_miss, &original_decision)
            .await
            .unwrap(),
        ToolInvocationPrepareOutcome::Prepared(_)
    ));
    assert!(matches!(
        ledger
            .finish_prepare_after_miss(racing_miss, &competing_decision)
            .await
            .unwrap(),
        ToolInvocationPrepareOutcome::Existing(record)
            if record.decision == original_decision
    ));
    assert!(matches!(
        ledger.probe_for_prepare(&first, &original).await.unwrap(),
        ToolInvocationPreparationProbe::Existing(record)
            if record.decision == original_decision
                && record.fingerprint.policy_decision_id == original_decision.decision_id
    ));
    assert!(matches!(
        ledger
            .probe_for_prepare(&first, &fingerprint("other"))
            .await,
        Err(ToolInvocationLedgerStoreError::IdentityConflict { .. })
    ));

    let parallel = identity(
        first.user_id.strip_prefix("invocation-user-").unwrap(),
        "parallel-call",
    );
    let ToolInvocationPreparationProbe::Missing(left) = ledger
        .probe_for_prepare(&parallel, &original)
        .await
        .unwrap()
    else {
        panic!("parallel invocation already existed");
    };
    let ToolInvocationPreparationProbe::Missing(right) = ledger
        .probe_for_prepare(&parallel, &original)
        .await
        .unwrap()
    else {
        panic!("parallel invocation already existed");
    };
    let (left_result, right_result) = tokio::join!(
        ledger.finish_prepare_after_miss(left, &original_decision),
        ledger.finish_prepare_after_miss(right, &competing_decision)
    );
    let results = [left_result.unwrap(), right_result.unwrap()];
    assert_eq!(
        results
            .iter()
            .filter(|outcome| matches!(outcome, ToolInvocationPrepareOutcome::Prepared(_)))
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|outcome| matches!(outcome, ToolInvocationPrepareOutcome::Existing(_)))
            .count(),
        1
    );
    let authoritative = ledger.get(&parallel).await.unwrap().unwrap();
    assert!(results.iter().all(|outcome| match outcome {
        ToolInvocationPrepareOutcome::Prepared(record)
        | ToolInvocationPrepareOutcome::Existing(record) => record == &authoritative,
    }));

    let closed = identity(
        first.user_id.strip_prefix("invocation-user-").unwrap(),
        "closed-after-probe",
    );
    let ToolInvocationPreparationProbe::Missing(closed_miss) =
        ledger.probe_for_prepare(&closed, &original).await.unwrap()
    else {
        panic!("closing invocation already existed");
    };
    sqlx::query("UPDATE agent_runs SET status = 'completed' WHERE user_id = ? AND run_id = ?")
        .bind(&first.user_id)
        .bind(&first.run_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        ledger
            .finish_prepare_after_miss(closed_miss, &original_decision)
            .await,
        Err(ToolInvocationLedgerStoreError::RunNotExecutable { .. })
    ));
    let ToolInvocationPreparationProbe::Missing(closed_retry) =
        ledger.probe_for_prepare(&closed, &original).await.unwrap()
    else {
        panic!("closed run unexpectedly acquired a hot invocation");
    };
    assert!(matches!(
        ledger
            .finish_prepare_after_miss(closed_retry, &original_decision)
            .await,
        Err(ToolInvocationLedgerStoreError::RunNotExecutable { .. })
    ));
    cleanup(&pool, &first).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn completion_waits_for_dispatch_commit_before_evaluating_state_cas() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    let id = identity(&Uuid::new_v4().simple().to_string(), "call-completing");
    insert_active_run(&pool, &id).await;
    let ledger = DatabaseToolInvocationLedger::new(shared);
    ledger
        .prepare(&id, &fingerprint("read"), &decision())
        .await
        .unwrap();

    // Hold the dispatch write open so the other connection sees Prepared.
    // A composite-PK UPDATE filtered by state alone can miss both the row and
    // its lock on MatrixOne (matrixorigin/matrixone#31685). Completion must
    // serialize on the invocation identity before evaluating state/owner.
    let mut dispatch = pool.begin().await.unwrap();
    assert_eq!(
        sqlx::query(
            "UPDATE tool_invocation_ledger
             SET state = 'dispatched', dispatch_certainty = 'dispatched',
                 attempt_count = 1, dispatch_owner = 'worker-completing',
                 dispatch_lease_expires_at = TIMESTAMPADD(SECOND, 90, CURRENT_TIMESTAMP(6))
             WHERE user_id = ? AND session_id = ? AND run_id = ?
               AND turn_chain_id = ? AND invocation_id = ? AND state = 'prepared'",
        )
        .bind(&id.user_id)
        .bind(&id.session_id)
        .bind(&id.run_id)
        .bind(&id.turn_chain_id)
        .bind(&id.invocation_id)
        .execute(&mut *dispatch)
        .await
        .unwrap()
        .rows_affected(),
        1
    );

    let outcome = success("acknowledged result");
    let completion = ledger.compare_and_complete(
        &id,
        ToolInvocationState::Dispatched,
        Some("worker-completing"),
        &outcome,
    );
    tokio::pin!(completion);
    // Poll the actual service entrypoint while the dispatch lock is held.
    // Keep the future alive on timeout, then release the writer before any
    // assertion so neither success nor a regression leaves an open fixture txn.
    let early = tokio::time::timeout(Duration::from_secs(1), &mut completion).await;
    dispatch.commit().await.unwrap();
    assert!(
        early.is_err(),
        "completion bypassed dispatch lock: {early:?}"
    );
    let completed = tokio::time::timeout(Duration::from_secs(10), &mut completion)
        .await
        .expect("completion must finish after dispatch commits")
        .unwrap();
    assert_eq!(completed.state, ToolInvocationState::Succeeded);
    assert_eq!(completed.attempt_count, 1);
    assert_eq!(completed.outcome, Some(outcome.clone()));
    let persisted = ledger.get(&id).await.unwrap().unwrap();
    assert_eq!(persisted.state, ToolInvocationState::Succeeded);
    assert_eq!(persisted.outcome.as_ref(), Some(&outcome));
    assert_eq!(
        completed, persisted,
        "a waited completion must return the entire committed receipt"
    );
    cleanup(&pool, &id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn concurrent_completions_preserve_one_terminal_outcome() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    let id = identity(&Uuid::new_v4().simple().to_string(), "call-racing");
    let ledger = DatabaseToolInvocationLedger::new(shared);
    let first = success("first result");
    let second = failure("second result");
    assert!(matches!(
        ledger
            .compare_and_complete(&id, ToolInvocationState::Dispatched, Some("worker"), &first)
            .await,
        Err(ToolInvocationLedgerStoreError::NotFound { .. })
    ));
    insert_active_run(&pool, &id).await;
    ledger
        .prepare(&id, &fingerprint("read"), &decision())
        .await
        .unwrap();
    ledger
        .claim_dispatch(&id, "worker", 90_000, dispatch_admission())
        .await
        .unwrap();

    let (a, b) = tokio::join!(
        ledger.compare_and_complete(&id, ToolInvocationState::Dispatched, Some("worker"), &first),
        ledger.compare_and_complete(
            &id,
            ToolInvocationState::Dispatched,
            Some("worker"),
            &second
        ),
    );
    let (winner, loser) = match (a, b) {
        (Ok(winner), Err(loser)) | (Err(loser), Ok(winner)) => (winner, loser),
        other => panic!("exactly one terminal writer must win: {other:?}"),
    };
    assert!(matches!(
        loser,
        ToolInvocationLedgerStoreError::StateMismatch {
            expected: ToolInvocationState::Dispatched,
            actual,
            ..
        } if actual == winner.state
    ));
    let persisted = ledger.get(&id).await.unwrap().unwrap();
    assert_eq!(persisted.state, winner.state);
    assert_eq!(persisted.outcome, winner.outcome);
    assert_eq!(persisted.attempt_count, 1);
    cleanup(&pool, &id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn invocation_identity_conflict_and_state_cas_hold_on_live_matrixone() {
    use tracing::instrument::WithSubscriber;
    use tracing_subscriber::prelude::*;

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
    let cache_queries = common::SqlQueries::default();
    let cache_subscriber = tracing_subscriber::registry().with(cache_queries.clone());
    let cached_record = ledger
        .complete_from_semantic_read_cache(
            &cached,
            &ToolInvocationResultPayload {
                output: "cached observation".to_string(),
                metadata: BTreeMap::from([("edge_values".into(), json!({"unicode":"中文\n\"escaped\"", "number":0.1, "max":u64::MAX, "nested":[null, true, [1,2]]}))]),
                exit_semantics: None,
            },
            &ToolInvocationCompletionSource::semantic_read_cache(
                format!("sha256:{}", "a".repeat(64)),
                format!("sha256:{}", "b".repeat(64)),
            )
            .unwrap(),
        )
        .with_subscriber(cache_subscriber)
        .await
        .unwrap();
    assert_eq!(
        cache_queries.0.lock().unwrap().len(),
        3,
        "cache completion needs only BEGIN, UPDATE RETURNING and COMMIT"
    );
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
        ledger
            .claim_dispatch(&cached, "worker-cache", 90_000, dispatch_admission())
            .await,
        Err(ToolInvocationLedgerStoreError::StateMismatch {
            actual: ToolInvocationState::Succeeded,
            ..
        })
    ));

    let dispatch = |ledger: DatabaseToolInvocationLedger,
                    identity: ToolInvocationIdentity,
                    owner: &'static str| async move {
        ledger
            .claim_dispatch(&identity, owner, 90_000, dispatch_admission())
            .await
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
                Err(ToolInvocationLedgerStoreError::ActionAlreadyStarted { .. })
            ))
            .count(),
        1,
        "the losing worker must observe the authoritative started-action fact: {race_results:?}"
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
        .claim_dispatch(&abandoned, "worker-abandoned", 90_000, dispatch_admission())
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
            .claim_dispatch(
                &abandoned,
                "replacement-worker",
                90_000,
                dispatch_admission(),
            )
            .await,
        Err(ToolInvocationLedgerStoreError::ActionAlreadyStarted { .. })
    ));

    let dispatched = ledger
        .claim_dispatch(&first, "worker-first", 90_000, dispatch_admission())
        .await
        .unwrap();
    assert_eq!(dispatched.attempt_count, 1);
    assert!(matches!(
        ledger
            .claim_dispatch(&first, "worker-other", 90_000, dispatch_admission())
            .await,
        Err(ToolInvocationLedgerStoreError::ActionAlreadyStarted { .. })
    ));

    ledger
        .mark_outcome_unknown(&first, "worker-first")
        .await
        .unwrap();
    assert!(matches!(
        ledger
            .claim_dispatch(&first, "worker-other", 90_000, dispatch_admission())
            .await,
        Err(ToolInvocationLedgerStoreError::ActionAlreadyStarted { .. })
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

    let prepared_before_closure = identity(&prefix, "prepared-before-run-closure");
    assert!(matches!(
        ledger
            .prepare(&prepared_before_closure, &original, &original_decision)
            .await
            .unwrap(),
        ToolInvocationPrepareOutcome::Prepared(_)
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
    assert!(matches!(
        ledger
            .prepare(&cached, &original, &original_decision)
            .await
            .unwrap(),
        ToolInvocationPrepareOutcome::Existing(record)
            if record.state == ToolInvocationState::Succeeded
                && record.outcome.as_ref().unwrap().result().output == "cached observation"
    ));
    assert!(matches!(
        ledger
            .prepare(&second, &original, &original_decision)
            .await
            .unwrap(),
        ToolInvocationPrepareOutcome::Existing(record)
            if record.state == ToolInvocationState::Failed
                && record.outcome.as_ref().unwrap().result().output == "provider failed"
    ));
    assert!(matches!(
        ledger
            .claim_dispatch(
                &prepared_before_closure,
                "worker-after-closure",
                90_000,
                dispatch_admission(),
            )
            .await,
        Err(ToolInvocationLedgerStoreError::RunNotExecutable { status, .. })
            if status == "completed"
    ));
    assert!(matches!(
        ledger.get(&prepared_before_closure).await.unwrap(),
        Some(record)
            if record.state == ToolInvocationState::Prepared
                && record.attempt_count == 0
                && record.dispatch_certainty == DispatchCertainty::NotDispatched
    ));
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

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn run_closure_serializes_with_new_admission_and_dispatch_claim() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    let prefix = Uuid::new_v4().simple().to_string();
    let prepare_identity = identity(&format!("{prefix}-prepare"), "concurrent-prepare");
    cleanup(&pool, &prepare_identity).await;
    insert_active_run(&pool, &prepare_identity).await;
    let ledger = DatabaseToolInvocationLedger::new(shared.clone());
    let invocation_fingerprint = fingerprint("concurrent admission");
    let invocation_decision = decision();

    let mut closure_tx = pool.begin().await.unwrap();
    sqlx::query(
        "UPDATE agent_runs SET status = 'completed'
         WHERE user_id = ? AND run_id = ?",
    )
    .bind(&prepare_identity.user_id)
    .bind(&prepare_identity.run_id)
    .execute(&mut *closure_tx)
    .await
    .unwrap();
    let prepare_task = {
        let ledger = ledger.clone();
        let identity = prepare_identity.clone();
        let fingerprint = invocation_fingerprint.clone();
        let decision = invocation_decision.clone();
        tokio::spawn(async move { ledger.prepare(&identity, &fingerprint, &decision).await })
    };
    tokio::pin!(prepare_task);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut prepare_task)
            .await
            .is_err(),
        "new identity admission must wait for the run closure transaction"
    );
    closure_tx.commit().await.unwrap();
    assert!(matches!(
        prepare_task.await.unwrap(),
        Err(ToolInvocationLedgerStoreError::RunNotExecutable { status, .. })
            if status == "completed"
    ));
    assert!(matches!(
        ledger.get(&prepare_identity).await,
        Err(ToolInvocationLedgerStoreError::TerminalRunRecordUnavailable { .. })
    ));

    let dispatch_identity = identity(&format!("{prefix}-dispatch"), "concurrent-dispatch");
    cleanup(&pool, &dispatch_identity).await;
    insert_active_run(&pool, &dispatch_identity).await;
    ledger
        .prepare(
            &dispatch_identity,
            &invocation_fingerprint,
            &invocation_decision,
        )
        .await
        .unwrap();
    let mut closure_tx = pool.begin().await.unwrap();
    sqlx::query(
        "UPDATE agent_runs SET status = 'completed'
         WHERE user_id = ? AND run_id = ?",
    )
    .bind(&dispatch_identity.user_id)
    .bind(&dispatch_identity.run_id)
    .execute(&mut *closure_tx)
    .await
    .unwrap();
    let dispatch_task = {
        let ledger = ledger.clone();
        let identity = dispatch_identity.clone();
        tokio::spawn(async move {
            ledger
                .claim_dispatch(&identity, "concurrent-worker", 90_000, dispatch_admission())
                .await
        })
    };
    tokio::pin!(dispatch_task);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut dispatch_task)
            .await
            .is_err(),
        "dispatch claim must wait for the run closure transaction"
    );
    closure_tx.commit().await.unwrap();
    assert!(matches!(
        dispatch_task.await.unwrap(),
        Err(ToolInvocationLedgerStoreError::RunNotExecutable { status, .. })
            if status == "completed"
    ));
    assert!(matches!(
        ledger.get(&dispatch_identity).await.unwrap(),
        Some(record)
            if record.state == ToolInvocationState::Prepared
                && record.attempt_count == 0
                && record.dispatch_certainty == DispatchCertainty::NotDispatched
    ));

    cleanup(&pool, &prepare_identity).await;
    cleanup(&pool, &dispatch_identity).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne"]
async fn late_unknown_completion_preserves_archive_replay() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    let prefix = Uuid::new_v4().simple().to_string();
    let mut ids = (0..3)
        .map(|i| identity(&prefix, &format!("overlap-{i}")))
        .collect::<Vec<_>>();
    ids.sort_by_key(ToolInvocationIdentity::storage_key);
    insert_active_run(&pool, &ids[0]).await;
    let ledger = DatabaseToolInvocationLedger::new(shared);
    for (i, id) in ids.iter().enumerate() {
        ledger
            .prepare(id, &fingerprint("overlap"), &decision())
            .await
            .unwrap();
        ledger
            .claim_dispatch(id, "worker", 90_000, dispatch_admission())
            .await
            .unwrap();
        if i == 1 {
            ledger.mark_outcome_unknown(id, "worker").await.unwrap();
        } else {
            ledger
                .compare_and_complete(
                    id,
                    ToolInvocationState::Dispatched,
                    Some("worker"),
                    &success("done"),
                )
                .await
                .unwrap();
        }
    }
    sqlx::query("UPDATE agent_runs SET status='completed' WHERE user_id=? AND run_id=?")
        .bind(&ids[0].user_id)
        .bind(&ids[0].run_id)
        .execute(&pool)
        .await
        .unwrap();
    let first = ledger
        .compact_terminal_run_batch(&ids[0].user_id, &ids[0].session_id, &ids[0].run_id)
        .await
        .unwrap();
    assert_eq!((first.archived_records, first.remaining_records), (2, 1));
    ledger
        .compare_and_complete(
            &ids[1],
            ToolInvocationState::OutcomeUnknown,
            None,
            &success("late done"),
        )
        .await
        .unwrap();
    let second = ledger
        .compact_terminal_run_batch(&ids[0].user_id, &ids[0].session_id, &ids[0].run_id)
        .await
        .unwrap();
    assert_eq!((second.archived_records, second.remaining_records), (1, 0));
    let restored = ledger
        .get(&ids[1])
        .await
        .expect("late settled identity must remain replayable through overlapping candidate ranges")
        .unwrap();
    assert_eq!(
        restored.outcome.as_ref().unwrap().result().output,
        "late done"
    );
    for id in &ids {
        assert_eq!(ledger.get(id).await.unwrap().unwrap().identity, *id);
    }
    // A duplicate on a later candidate page must still fail closed.
    let duplicate_artifact = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO session_artifacts (artifact_id,session_id,user_id,artifact_kind,content_json,retention_until) SELECT ?,session_id,user_id,artifact_kind,content_json,retention_until FROM session_artifacts WHERE artifact_id=?")
        .bind(&duplicate_artifact).bind(second.artifact_id.as_ref().unwrap()).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO tool_invocation_archive_chunks (user_id,session_id,run_id,chunk_index,artifact_id,first_identity_key,last_identity_key,record_count,encoded_bytes) SELECT user_id,session_id,run_id,3,?,first_identity_key,last_identity_key,record_count,encoded_bytes FROM tool_invocation_archive_chunks WHERE user_id=? AND session_id=? AND run_id=? AND chunk_index=2")
        .bind(&duplicate_artifact).bind(&ids[0].user_id).bind(&ids[0].session_id).bind(&ids[0].run_id).execute(&pool).await.unwrap();
    assert!(matches!(
        ledger.get(&ids[1]).await,
        Err(ToolInvocationLedgerStoreError::DuplicateArchiveIdentity { .. })
    ));
    sqlx::query("DELETE FROM tool_invocation_archive_chunks WHERE user_id=? AND session_id=? AND run_id=? AND chunk_index=3")
        .bind(&ids[0].user_id).bind(&ids[0].session_id).bind(&ids[0].run_id).execute(&pool).await.unwrap();
    let first_artifact = first.artifact_id.unwrap();
    let body: String =
        sqlx::query_scalar("SELECT content_json FROM session_artifacts WHERE artifact_id = ?")
            .bind(&first_artifact)
            .fetch_one(&pool)
            .await
            .unwrap();
    sqlx::query("UPDATE session_artifacts SET status = 'expired' WHERE artifact_id = ?")
        .bind(&first_artifact)
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        ledger.get(&ids[1]).await,
        Err(ToolInvocationLedgerStoreError::ArchiveUnavailable { .. })
    ));
    sqlx::query("UPDATE session_artifacts SET status = 'active', content_json = 'invalid' WHERE artifact_id = ?")
        .bind(&first_artifact).execute(&pool).await.unwrap();
    assert!(matches!(
        ledger.get(&ids[1]).await,
        Err(ToolInvocationLedgerStoreError::InvalidArchive { .. })
    ));
    let mut chunk: serde_json::Value = serde_json::from_str(&body).unwrap();
    chunk["records"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::to_value(&restored).unwrap());
    sqlx::query("UPDATE session_artifacts SET content_json = ? WHERE artifact_id = ?")
        .bind(chunk.to_string())
        .bind(&first_artifact)
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        ledger.get(&ids[1]).await,
        Err(ToolInvocationLedgerStoreError::DuplicateArchiveIdentity { .. })
    ));
    cleanup(&pool, &ids[0]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn heartbeat_renews_without_loading_result_json() {
    use tracing::instrument::WithSubscriber;
    use tracing_subscriber::prelude::*;

    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    let id = identity(&Uuid::new_v4().simple().to_string(), "heartbeat");
    insert_active_run(&pool, &id).await;
    let ledger = DatabaseToolInvocationLedger::new(shared);
    ledger
        .prepare(&id, &fingerprint("receipt"), &decision())
        .await
        .unwrap();
    let claimed = ledger
        .claim_dispatch(&id, "worker", 90_000, dispatch_admission())
        .await
        .unwrap();
    let queries = common::SqlQueries::default();
    let subscriber = tracing_subscriber::registry().with(queries.clone());
    ledger
        .renew_dispatch(&id, "worker", 180_000)
        .with_subscriber(subscriber)
        .await
        .unwrap();
    let renewed = ledger.get(&id).await.unwrap().unwrap();
    assert_eq!(renewed.state, ToolInvocationState::Dispatched);
    assert_eq!(renewed.attempt_count, claimed.attempt_count);
    assert_eq!(renewed.fingerprint, claimed.fingerprint);
    assert!(
        renewed.dispatch_lease.as_ref().unwrap().expires_at_epoch_ms
            > claimed.dispatch_lease.unwrap().expires_at_epoch_ms
    );
    let statements = queries.0.lock().unwrap().clone();
    assert_eq!(
        statements.len(),
        3,
        "heartbeat needs only BEGIN, UPDATE and COMMIT"
    );
    assert!(
        !statements
            .iter()
            .any(|sql| sql.trim_start().starts_with("SELECT"))
    );
    for count in [0_u64, u64::from(u32::MAX) + 1] {
        sqlx::query("UPDATE tool_invocation_ledger SET attempt_count=? WHERE user_id=? AND session_id=? AND run_id=? AND turn_chain_id=? AND invocation_id=?")
            .bind(count).bind(&id.user_id).bind(&id.session_id).bind(&id.run_id).bind(&id.turn_chain_id).bind(&id.invocation_id).execute(&pool).await.unwrap();
        let before: String = sqlx::query_scalar("SELECT CAST(dispatch_lease_expires_at AS CHAR) FROM tool_invocation_ledger WHERE user_id=? AND session_id=? AND run_id=? AND turn_chain_id=? AND invocation_id=?")
            .bind(&id.user_id).bind(&id.session_id).bind(&id.run_id).bind(&id.turn_chain_id).bind(&id.invocation_id).fetch_one(&pool).await.unwrap();
        assert!(matches!(ledger.renew_dispatch(&id, "worker", 90_000).await,
            Err(ToolInvocationLedgerStoreError::InvalidAttemptCount(actual)) if actual == count));
        let after: String = sqlx::query_scalar("SELECT CAST(dispatch_lease_expires_at AS CHAR) FROM tool_invocation_ledger WHERE user_id=? AND session_id=? AND run_id=? AND turn_chain_id=? AND invocation_id=?")
            .bind(&id.user_id).bind(&id.session_id).bind(&id.run_id).bind(&id.turn_chain_id).bind(&id.invocation_id).fetch_one(&pool).await.unwrap();
        assert_eq!(
            before, after,
            "an invalid heartbeat must not extend custody"
        );
    }
    sqlx::query("UPDATE tool_invocation_ledger SET attempt_count=1 WHERE user_id=? AND session_id=? AND run_id=? AND turn_chain_id=? AND invocation_id=?")
        .bind(&id.user_id).bind(&id.session_id).bind(&id.run_id).bind(&id.turn_chain_id).bind(&id.invocation_id).execute(&pool).await.unwrap();
    let deadline_before: String = sqlx::query_scalar("SELECT CAST(dispatch_lease_expires_at AS CHAR) FROM tool_invocation_ledger WHERE user_id=? AND session_id=? AND run_id=? AND turn_chain_id=? AND invocation_id=?")
        .bind(&id.user_id).bind(&id.session_id).bind(&id.run_id).bind(&id.turn_chain_id).bind(&id.invocation_id).fetch_one(&pool).await.unwrap();
    assert!(
        ledger
            .renew_dispatch(&id, "worker", 300_000_000_000_000)
            .await
            .is_err(),
        "out-of-range duration must not commit a null dispatch deadline"
    );
    let deadline_after: String = sqlx::query_scalar("SELECT CAST(dispatch_lease_expires_at AS CHAR) FROM tool_invocation_ledger WHERE user_id=? AND session_id=? AND run_id=? AND turn_chain_id=? AND invocation_id=?")
        .bind(&id.user_id).bind(&id.session_id).bind(&id.run_id).bind(&id.turn_chain_id).bind(&id.invocation_id).fetch_one(&pool).await.unwrap();
    assert_eq!(deadline_before, deadline_after);
    let completed = ledger
        .compare_and_complete(
            &id,
            ToolInvocationState::Dispatched,
            Some("worker"),
            &success("durable result"),
        )
        .await
        .unwrap();
    assert_eq!(completed, ledger.get(&id).await.unwrap().unwrap());
    assert!(matches!(
        ledger.renew_dispatch(&id, "worker", 90_000).await,
        Err(ToolInvocationLedgerStoreError::StateMismatch { .. })
    ));
    assert!(matches!(
        ledger
            .renew_dispatch(
                &identity(&Uuid::new_v4().simple().to_string(), "absent"),
                "worker",
                90_000
            )
            .await,
        Err(ToolInvocationLedgerStoreError::NotFound { .. })
    ));
    cleanup(&pool, &id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn compaction_pages_shared_evidence_and_preserves_unknown_custody() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    let prefix = Uuid::new_v4().simple().to_string();
    let mut ids = (0..33)
        .map(|i| identity(&prefix, &format!("batch-{i}")))
        .collect::<Vec<_>>();
    ids.sort_by_key(ToolInvocationIdentity::storage_key);
    let unknown = identity(&prefix, "unsettled");
    insert_active_run(&pool, &ids[0]).await;
    let ledger = DatabaseToolInvocationLedger::new(shared);
    for id in ids.iter().chain(std::iter::once(&unknown)) {
        ledger
            .prepare(id, &fingerprint("shared evidence"), &decision())
            .await
            .unwrap();
        ledger
            .claim_dispatch(id, "worker", 90_000, dispatch_admission())
            .await
            .unwrap();
        if id == &unknown {
            ledger.mark_outcome_unknown(id, "worker").await.unwrap();
        } else {
            ledger
                .compare_and_complete(
                    id,
                    ToolInvocationState::Dispatched,
                    Some("worker"),
                    &success(&id.invocation_id),
                )
                .await
                .unwrap();
        }
    }
    let artifacts = (0..130)
        .map(|_| Uuid::new_v4().to_string())
        .collect::<Vec<_>>();
    let mut insert = sqlx::QueryBuilder::<sqlx::MySql>::new(
        "INSERT INTO session_artifacts (artifact_id,session_id,user_id,artifact_kind,content_json,retention_until) ",
    );
    insert.push_values(&artifacts, |mut row, artifact| {
        row.push_bind(artifact)
            .push_bind(&ids[0].session_id)
            .push_bind(&ids[0].user_id)
            .push_bind("tool_result_evidence_v1")
            .push_bind("{}")
            .push("TIMESTAMPADD(DAY, 1, CURRENT_TIMESTAMP(6))");
    });
    insert.build().execute(&pool).await.unwrap();
    let mut insert = sqlx::QueryBuilder::<sqlx::MySql>::new(
        "INSERT INTO session_artifact_references (user_id,session_id,artifact_id,reference_kind,reference_id) ",
    );
    insert.push_values(
        artifacts
            .iter()
            .flat_map(|artifact| [&ids[0], &ids[1], &unknown].map(|id| (artifact, id))),
        |mut row, (artifact, id)| {
            row.push_bind(&id.user_id)
                .push_bind(&id.session_id)
                .push_bind(artifact)
                .push_bind("invocation_ledger")
                .push_bind(id.storage_key());
        },
    );
    insert.build().execute(&pool).await.unwrap();
    sqlx::query("UPDATE agent_runs SET status='completed' WHERE user_id=? AND run_id=?")
        .bind(&ids[0].user_id)
        .bind(&ids[0].run_id)
        .execute(&pool)
        .await
        .unwrap();
    let first = ledger
        .compact_terminal_run_batch(&ids[0].user_id, &ids[0].session_id, &ids[0].run_id)
        .await
        .unwrap();
    assert_eq!((first.archived_records, first.remaining_records), (32, 2));
    let second = ledger
        .compact_terminal_run_batch(&ids[0].user_id, &ids[0].session_id, &ids[0].run_id)
        .await
        .unwrap();
    assert_eq!((second.archived_records, second.remaining_records), (1, 1));
    let empty = ledger
        .compact_terminal_run_batch(&ids[0].user_id, &ids[0].session_id, &ids[0].run_id)
        .await
        .unwrap();
    assert_eq!((empty.archived_records, empty.remaining_records), (0, 1));
    for id in &ids {
        let restored = ledger.get(id).await.unwrap().unwrap();
        assert_eq!(restored.outcome.unwrap().result().output, id.invocation_id);
    }
    assert_eq!(
        ledger.get(&unknown).await.unwrap().unwrap().state,
        ToolInvocationState::OutcomeUnknown
    );
    let references: Vec<(String,String)> = sqlx::query_as("SELECT artifact_id,reference_id FROM session_artifact_references WHERE user_id=? AND session_id=?")
        .bind(&ids[0].user_id).bind(&ids[0].session_id).fetch_all(&pool).await.unwrap();
    for artifact in &artifacts {
        let owners = references
            .iter()
            .filter(|(id, _)| id == artifact)
            .map(|(_, owner)| owner.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        let unknown_key = unknown.storage_key();
        assert_eq!(
            owners,
            std::collections::BTreeSet::from([ids[0].run_id.as_str(), unknown_key.as_str()])
        );
    }
    cleanup(&pool, &ids[0]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn dispatch_receipts_preserve_database_epoch_in_non_utc_sessions() {
    let bootstrap = common::setup_pool().await;
    let mut settings = common::require_db_it_env();
    settings.db_pool_max_connections = 1;
    settings.db_pool_min_connections = 1;
    let shared = astra_core::SharedPool::new(&settings).await.unwrap();
    let pool = shared.get().clone();
    // A one-connection pool makes the timezone a property of this fixture's
    // actual service queries without changing global database configuration.
    let mut connection = pool.acquire().await.unwrap();
    sqlx::Executor::execute(&mut *connection, "SET time_zone = '+08:00'")
        .await
        .unwrap();
    drop(connection);
    let id = identity(&Uuid::new_v4().simple().to_string(), "non-utc");
    insert_active_run(&pool, &id).await;
    let ledger = DatabaseToolInvocationLedger::new(shared.clone());
    ledger
        .prepare(&id, &fingerprint("clock"), &decision())
        .await
        .unwrap();
    let claimed = ledger
        .claim_dispatch(&id, "worker", 90_000, dispatch_admission())
        .await
        .unwrap();
    let expected: u64 = sqlx::query_scalar("SELECT CAST(UNIX_TIMESTAMP(dispatch_lease_expires_at)*1000 AS UNSIGNED) FROM tool_invocation_ledger WHERE user_id=? AND session_id=? AND run_id=? AND turn_chain_id=? AND invocation_id=?")
        .bind(&id.user_id).bind(&id.session_id).bind(&id.run_id).bind(&id.turn_chain_id).bind(&id.invocation_id).fetch_one(&pool).await.unwrap();
    assert_eq!(
        claimed.dispatch_lease.unwrap().expires_at_epoch_ms,
        expected
    );
    ledger.renew_dispatch(&id, "worker", 180_000).await.unwrap();
    ledger.mark_outcome_unknown(&id, "worker").await.unwrap();
    let completed = ledger
        .compare_and_complete(
            &id,
            ToolInvocationState::OutcomeUnknown,
            None,
            &success("late acknowledged"),
        )
        .await
        .unwrap();
    assert_eq!(completed, ledger.get(&id).await.unwrap().unwrap());
    cleanup(&pool, &id).await;
    shared.close().await;
    bootstrap.close().await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn cancelled_cache_completion_rolls_back_and_releases_its_checkout() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    let id = identity(&Uuid::new_v4().simple().to_string(), "cancel-cache");
    insert_active_run(&pool, &id).await;
    let ledger = DatabaseToolInvocationLedger::new(shared);
    ledger
        .prepare(&id, &fingerprint("cache cancellation"), &decision())
        .await
        .unwrap();
    let mut blocker = pool.begin().await.unwrap();
    sqlx::query("SELECT state FROM tool_invocation_ledger WHERE user_id=? AND session_id=? AND run_id=? AND turn_chain_id=? AND invocation_id=? FOR UPDATE")
        .bind(&id.user_id).bind(&id.session_id).bind(&id.run_id).bind(&id.turn_chain_id).bind(&id.invocation_id).fetch_all(&mut *blocker).await.unwrap();
    let result = ToolInvocationResultPayload {
        output: "cached after cancellation".into(),
        metadata: BTreeMap::new(),
        exit_semantics: None,
    };
    let source = ToolInvocationCompletionSource::semantic_read_cache(
        format!("sha256:{}", "a".repeat(64)),
        format!("sha256:{}", "b".repeat(64)),
    )
    .unwrap();
    let cancelled = tokio::time::timeout(
        Duration::from_millis(100),
        ledger.complete_from_semantic_read_cache(&id, &result, &source),
    )
    .await;
    // Release the competing writer before asserting so failures cannot strand
    // its lock. The cancelled operation never reached its COMMIT boundary.
    blocker.commit().await.unwrap();
    assert!(
        cancelled.is_err(),
        "cache completion bypassed a held identity lock"
    );
    let completed = ledger
        .complete_from_semantic_read_cache(&id, &result, &source)
        .await
        .unwrap();
    assert_eq!(completed, ledger.get(&id).await.unwrap().unwrap());
    assert_eq!(completed.attempt_count, 0);
    assert_eq!(completed.outcome.unwrap().result().output, result.output);
    assert!(matches!(
        ledger
            .complete_from_semantic_read_cache(&id, &result, &source)
            .await,
        Err(ToolInvocationLedgerStoreError::StateMismatch {
            actual: ToolInvocationState::Succeeded,
            ..
        })
    ));
    let mut other = id.clone();
    other.invocation_id = "provider-won".into();
    ledger
        .prepare(&other, &fingerprint("cache cancellation"), &decision())
        .await
        .unwrap();
    let dispatched = ledger
        .claim_dispatch(&other, "worker", 90_000, dispatch_admission())
        .await
        .unwrap();
    assert!(matches!(
        ledger
            .complete_from_semantic_read_cache(&other, &result, &source)
            .await,
        Err(ToolInvocationLedgerStoreError::StateMismatch {
            actual: ToolInvocationState::Dispatched,
            ..
        })
    ));
    assert_eq!(ledger.get(&other).await.unwrap().unwrap(), dispatched);
    other.invocation_id = "missing".into();
    assert!(matches!(
        ledger
            .complete_from_semantic_read_cache(&other, &result, &source)
            .await,
        Err(ToolInvocationLedgerStoreError::NotFound { .. })
    ));
    other.invocation_id = "invalid-decision".into();
    ledger
        .prepare(&other, &fingerprint("cache cancellation"), &decision())
        .await
        .unwrap();
    sqlx::query("UPDATE tool_invocation_ledger SET decision_json='{}' WHERE user_id=? AND session_id=? AND run_id=? AND turn_chain_id=? AND invocation_id=?")
        .bind(&other.user_id).bind(&other.session_id).bind(&other.run_id).bind(&other.turn_chain_id).bind(&other.invocation_id).execute(&pool).await.unwrap();
    assert!(matches!(
        ledger
            .complete_from_semantic_read_cache(&other, &result, &source)
            .await,
        Err(ToolInvocationLedgerStoreError::InvalidStoredJson {
            field: "decision_json",
            ..
        })
    ));
    let mut check = pool.begin().await.unwrap();
    let (state,outcome): (String,Option<String>) = sqlx::query_as("SELECT state,JSON_UNQUOTE(outcome_json) FROM tool_invocation_ledger WHERE user_id=? AND session_id=? AND run_id=? AND turn_chain_id=? AND invocation_id=? FOR UPDATE")
        .bind(&other.user_id).bind(&other.session_id).bind(&other.run_id).bind(&other.turn_chain_id).bind(&other.invocation_id).fetch_one(&mut *check).await.unwrap();
    check.commit().await.unwrap();
    assert_eq!(state, "prepared");
    assert!(
        outcome.is_none(),
        "a rejected receipt must roll back the matched update"
    );
    sqlx::query("UPDATE tool_invocation_ledger SET decision_json=? WHERE user_id=? AND session_id=? AND run_id=? AND turn_chain_id=? AND invocation_id=?")
        .bind(serde_json::to_string(&decision()).unwrap()).bind(&other.user_id).bind(&other.session_id).bind(&other.run_id).bind(&other.turn_chain_id).bind(&other.invocation_id).execute(&pool).await.unwrap();
    let retried = ledger
        .complete_from_semantic_read_cache(&other, &result, &source)
        .await
        .unwrap();
    assert_eq!(ledger.get(&other).await.unwrap().unwrap(), retried);
    cleanup(&pool, &id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn interrupted_reference_transfer_rolls_back_and_session_deletion_serializes() {
    use astra_services::{DatabaseSessionService, SessionService};
    use common::mysql_response_proxy::{Boundary, ResponseProxy, proxy_pool};

    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    let id = identity(
        &Uuid::new_v4().simple().to_string(),
        "transfer-cancellation",
    );
    insert_active_run(&pool, &id).await;
    let ledger = DatabaseToolInvocationLedger::new(shared.clone());
    ledger
        .prepare(&id, &fingerprint("transfer"), &decision())
        .await
        .unwrap();
    ledger
        .claim_dispatch(&id, "worker", 90_000, dispatch_admission())
        .await
        .unwrap();
    ledger
        .compare_and_complete(
            &id,
            ToolInvocationState::Dispatched,
            Some("worker"),
            &success("retained evidence"),
        )
        .await
        .unwrap();
    let artifact = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO session_artifacts (artifact_id,session_id,user_id,artifact_kind,content_json,retention_until) VALUES (?,?,?,'tool_result_evidence_v1','{}',TIMESTAMPADD(DAY,1,CURRENT_TIMESTAMP(6)))")
        .bind(&artifact).bind(&id.session_id).bind(&id.user_id).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO session_artifact_references (user_id,session_id,artifact_id,reference_kind,reference_id) VALUES (?,?,?,'invocation_ledger',?)")
        .bind(&id.user_id).bind(&id.session_id).bind(&artifact).bind(id.storage_key()).execute(&pool).await.unwrap();
    sqlx::query("UPDATE agent_runs SET status='completed' WHERE user_id=? AND run_id=?")
        .bind(&id.user_id)
        .bind(&id.run_id)
        .execute(&pool)
        .await
        .unwrap();
    let settings = shared.settings().clone();
    let sessions = DatabaseSessionService::new(settings.clone()).with_pool(shared);
    for cancel in [true, false] {
        let proxy = ResponseProxy::start(
            settings.host.clone(),
            settings.port,
            Boundary::Write(b"DELETE FROM session_artifact_references"),
        )
        .await;
        let proxied_pool = proxy_pool(&settings, proxy.port).await;
        let mut pending = tokio::spawn({
            let writer = DatabaseToolInvocationLedger::new(proxied_pool.clone());
            let id = id.clone();
            async move {
                writer
                    .compact_terminal_run_batch(&id.user_id, &id.session_id, &id.run_id)
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(10), async {
            tokio::select! {
                _ = proxy.gate.received.notified() => {},
                result = &mut pending => panic!("compaction did not reach the transfer barrier: {result:?}"),
            }
        }).await.expect("database must execute the reference transfer");
        if cancel {
            pending.abort();
            assert!(pending.await.unwrap_err().is_cancelled());
            proxy.gate.release.notify_one();
            // A current read waits for server rollback, so the assertions
            // cannot mistake a stale pre-transaction snapshot for cleanup.
            let mut check = pool.begin().await.unwrap();
            sqlx::query("SELECT status FROM agent_runs WHERE user_id=? AND run_id=? FOR UPDATE")
                .bind(&id.user_id)
                .bind(&id.run_id)
                .fetch_all(&mut *check)
                .await
                .unwrap();
            check.commit().await.unwrap();
            let record = ledger.get(&id).await.unwrap().unwrap();
            assert_eq!(record.outcome.unwrap().result().output, "retained evidence");
            let owners: Vec<String> = sqlx::query_scalar("SELECT reference_id FROM session_artifact_references WHERE user_id=? AND session_id=? AND artifact_id=?")
                .bind(&id.user_id).bind(&id.session_id).bind(&artifact).fetch_all(&pool).await.unwrap();
            assert_eq!(owners, vec![id.storage_key()]);
            let archives:i64=sqlx::query_scalar("SELECT COUNT(*) FROM session_artifacts WHERE user_id=? AND session_id=? AND artifact_kind='tool_invocation_archive_v1'")
                .bind(&id.user_id).bind(&id.session_id).fetch_one(&pool).await.unwrap();
            assert_eq!(
                archives, 0,
                "cancellation must not leave an archive artifact or transferred pins"
            );
        } else {
            let deletion = sessions.delete_session(id.session_id.clone(), id.user_id.clone());
            tokio::pin!(deletion);
            let early = tokio::time::timeout(Duration::from_millis(100), &mut deletion).await;
            proxy.gate.release.notify_one();
            assert!(
                early.is_err(),
                "Session deletion must serialize behind the compactor's Run lock"
            );
            let (archived, deleted) = tokio::join!(pending, &mut deletion);
            assert_eq!(archived.unwrap().unwrap().archived_records, 1);
            deleted.unwrap();
        }
        proxied_pool.close().await;
        drop(proxy);
    }
    for table in [
        "tool_invocation_ledger",
        "tool_invocation_archive_chunks",
        "session_artifacts",
        "session_artifact_references",
    ] {
        let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*) FROM {table} WHERE user_id=? AND session_id=?"
        )))
        .bind(&id.user_id)
        .bind(&id.session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 0, "Session deletion left orphaned {table} rows");
    }
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn cache_response_cancellation_and_lost_commit_ack_remain_replayable() {
    use common::mysql_response_proxy::{Boundary, ResponseProxy, proxy_pool};
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    let settings = shared.settings().clone();
    let id = identity(&Uuid::new_v4().simple().to_string(), "cache-protocol");
    insert_active_run(&pool, &id).await;
    let ledger = DatabaseToolInvocationLedger::new(shared);
    let result = ToolInvocationResultPayload {
        output: "one cached result".into(),
        metadata: BTreeMap::new(),
        exit_semantics: None,
    };
    let source = ToolInvocationCompletionSource::semantic_read_cache(
        format!("sha256:{}", "a".repeat(64)),
        format!("sha256:{}", "b".repeat(64)),
    )
    .unwrap();
    for (boundary, committed) in [
        (
            Boundary::RowTerminator(b"UPDATE tool_invocation_ledger"),
            false,
        ),
        (Boundary::Commit, true),
    ] {
        let mut call = id.clone();
        call.invocation_id = format!("cache-committed-{committed}");
        ledger
            .prepare(&call, &fingerprint("cache protocol"), &decision())
            .await
            .unwrap();
        let proxy = ResponseProxy::start(settings.host.clone(), settings.port, boundary).await;
        let proxied_pool = proxy_pool(&settings, proxy.port).await;
        let mut pending = tokio::spawn({
            let writer = DatabaseToolInvocationLedger::new(proxied_pool.clone());
            let call = call.clone();
            let result = result.clone();
            let source = source.clone();
            async move {
                writer
                    .complete_from_semantic_read_cache(&call, &result, &source)
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(10),async {
            tokio::select! {
                _ = proxy.gate.received.notified() => {},
                result = &mut pending => panic!("cache completion bypassed protocol barrier: {result:?}"),
            }
        }).await.expect("cache response must reach the protocol barrier");
        pending.abort();
        assert!(pending.await.unwrap_err().is_cancelled());
        proxy.gate.release.notify_one();
        let mut check = pool.begin().await.unwrap();
        let state:String=sqlx::query_scalar("SELECT state FROM tool_invocation_ledger WHERE user_id=? AND session_id=? AND run_id=? AND turn_chain_id=? AND invocation_id=? FOR UPDATE")
            .bind(&call.user_id).bind(&call.session_id).bind(&call.run_id).bind(&call.turn_chain_id).bind(&call.invocation_id).fetch_one(&mut *check).await.unwrap();
        check.commit().await.unwrap();
        assert_eq!(state, if committed { "succeeded" } else { "prepared" });
        if !committed {
            assert!(ledger.get(&call).await.unwrap().unwrap().outcome.is_none());
            ledger
                .complete_from_semantic_read_cache(&call, &result, &source)
                .await
                .unwrap();
        }
        let replay = ledger
            .prepare(&call, &fingerprint("cache protocol"), &decision())
            .await
            .unwrap();
        let ToolInvocationPrepareOutcome::Existing(record) = replay else {
            panic!("cache retry admitted a new invocation")
        };
        assert_eq!(record.attempt_count, 0);
        assert_eq!(record.outcome.unwrap().result().output, result.output);
        assert!(record.completion_source.is_some());
        proxied_pool.close().await;
        drop(proxy);
    }
    cleanup(&pool, &id).await;
}
