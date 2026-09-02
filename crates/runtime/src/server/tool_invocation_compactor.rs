use std::sync::Arc;

use astra_core::SharedPool;
use chrono::NaiveDateTime;

const COMPACTION_INTERVAL_SECS: u64 = 300;
const COMPACTION_CURSOR_NAME: &str = "tool_invocation_compaction_v1";
const COMPACTION_CURSOR_EPOCH: &str = "1970-01-01 00:00:00.000000";

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ToolInvocationCompactionSweepOutcome {
    pub runs_scanned: usize,
    pub records_archived: usize,
    pub runs_remaining: usize,
    pub runs_not_quiescent: usize,
    pub prepared_rejected: usize,
    pub inconsistent_prepared_unknown: usize,
    pub expired_dispatches_unknown: usize,
    pub cursor_wrapped: bool,
    pub expired_archive_references_released: usize,
    pub expired_archive_indexes_purged: usize,
    pub failures: Vec<ToolInvocationCompactionFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolInvocationCompactionFailure {
    pub user_id: String,
    pub session_id: String,
    pub run_id: String,
    pub phase: &'static str,
    pub error: String,
}

pub async fn run_tool_invocation_compaction_once(
    pool: SharedPool,
    limit: usize,
) -> Result<ToolInvocationCompactionSweepOutcome, sqlx::Error> {
    let effective_limit = limit.clamp(1, 1_000) as i64;
    let expired_archive_references_released =
        release_expired_archive_references(&pool, effective_limit).await?;
    let expired_archive_indexes_purged =
        purge_expired_archive_indexes(&pool, effective_limit).await?;
    let (candidates, cursor_wrapped) =
        claim_compaction_candidates(&pool, COMPACTION_CURSOR_NAME, effective_limit).await?;
    let ledger = astra_services::tool_invocation_ledger::DatabaseToolInvocationLedger::new(pool);
    let mut outcome = ToolInvocationCompactionSweepOutcome {
        runs_scanned: candidates.len(),
        cursor_wrapped,
        expired_archive_references_released,
        expired_archive_indexes_purged,
        ..Default::default()
    };
    for (user_id, session_id, run_id) in candidates {
        match ledger
            .reconcile_terminal_run(&user_id, &session_id, &run_id)
            .await
        {
            Ok(reconciled) => {
                outcome.prepared_rejected = outcome.prepared_rejected.saturating_add(
                    usize::try_from(reconciled.prepared_rejected).unwrap_or(usize::MAX),
                );
                outcome.inconsistent_prepared_unknown =
                    outcome.inconsistent_prepared_unknown.saturating_add(
                        usize::try_from(reconciled.inconsistent_prepared_unknown)
                            .unwrap_or(usize::MAX),
                    );
                outcome.expired_dispatches_unknown =
                    outcome.expired_dispatches_unknown.saturating_add(
                        usize::try_from(reconciled.expired_dispatches_unknown)
                            .unwrap_or(usize::MAX),
                    );
                if reconciled.active_dispatches_remaining > 0 {
                    outcome.runs_not_quiescent += 1;
                    continue;
                }
            }
            Err(error) => {
                outcome.failures.push(ToolInvocationCompactionFailure {
                    user_id,
                    session_id,
                    run_id,
                    phase: "reconcile",
                    error: error.to_string(),
                });
                continue;
            }
        }
        match ledger
            .compact_terminal_run_batch(&user_id, &session_id, &run_id)
            .await
        {
            Ok(compacted) => {
                outcome.records_archived += compacted.archived_records;
                outcome.runs_remaining += usize::from(compacted.remaining_records > 0);
            }
            Err(
                astra_services::tool_invocation_ledger::ToolInvocationLedgerStoreError::RunNotQuiescent {
                    ..
                },
            ) => {
                outcome.runs_not_quiescent += 1;
            }
            Err(error) => outcome.failures.push(ToolInvocationCompactionFailure {
                user_id,
                session_id,
                run_id,
                phase: "compact",
                error: error.to_string(),
            }),
        }
    }
    Ok(outcome)
}

async fn claim_compaction_candidates(
    pool: &SharedPool,
    cursor_name: &str,
    limit: i64,
) -> Result<(Vec<(String, String, String)>, bool), sqlx::Error> {
    let mut tx = pool.get().begin().await?;
    sqlx::query(
        "INSERT IGNORE INTO maintenance_sweep_cursors
         (sweep_name, cursor_updated_at, cursor_user_id, cursor_run_id)
         VALUES (?, ?, '', '')",
    )
    .bind(cursor_name)
    .bind(COMPACTION_CURSOR_EPOCH)
    .execute(&mut *tx)
    .await?;
    let (mut cursor_updated_at, mut cursor_user_id, mut cursor_run_id): (
        NaiveDateTime,
        String,
        String,
    ) = sqlx::query_as(
        "SELECT cursor_updated_at, cursor_user_id, cursor_run_id
         FROM maintenance_sweep_cursors WHERE sweep_name = ? FOR UPDATE",
    )
    .bind(cursor_name)
    .fetch_one(&mut *tx)
    .await?;

    let mut candidates = load_compaction_candidates_after(
        &mut tx,
        cursor_updated_at,
        &cursor_user_id,
        &cursor_run_id,
        limit,
    )
    .await?;
    let cursor_wrapped = candidates.is_empty()
        && (!cursor_user_id.is_empty()
            || !cursor_run_id.is_empty()
            || cursor_updated_at
                != NaiveDateTime::parse_from_str(COMPACTION_CURSOR_EPOCH, "%Y-%m-%d %H:%M:%S%.f")
                    .expect("static compaction cursor epoch is valid"));
    if cursor_wrapped {
        cursor_updated_at =
            NaiveDateTime::parse_from_str(COMPACTION_CURSOR_EPOCH, "%Y-%m-%d %H:%M:%S%.f")
                .expect("static compaction cursor epoch is valid");
        cursor_user_id.clear();
        cursor_run_id.clear();
        candidates = load_compaction_candidates_after(
            &mut tx,
            cursor_updated_at,
            &cursor_user_id,
            &cursor_run_id,
            limit,
        )
        .await?;
    }

    if let Some((user_id, _session_id, run_id, updated_at)) = candidates.last() {
        cursor_updated_at = *updated_at;
        cursor_user_id.clone_from(user_id);
        cursor_run_id.clone_from(run_id);
    }
    sqlx::query(
        "UPDATE maintenance_sweep_cursors
         SET cursor_updated_at = ?, cursor_user_id = ?, cursor_run_id = ?,
             scan_generation = scan_generation + ?, updated_at = CURRENT_TIMESTAMP(6)
         WHERE sweep_name = ?",
    )
    .bind(cursor_updated_at)
    .bind(&cursor_user_id)
    .bind(&cursor_run_id)
    .bind(u8::from(cursor_wrapped))
    .bind(cursor_name)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((
        candidates
            .into_iter()
            .map(|(user_id, session_id, run_id, _)| (user_id, session_id, run_id))
            .collect(),
        cursor_wrapped,
    ))
}

async fn load_compaction_candidates_after(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    cursor_updated_at: NaiveDateTime,
    cursor_user_id: &str,
    cursor_run_id: &str,
    limit: i64,
) -> Result<Vec<(String, String, String, NaiveDateTime)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT ar.user_id, ar.session_id, ar.run_id, ar.updated_at
         FROM agent_runs ar
         WHERE ar.status IN ('completed', 'delegated', 'failed', 'cancelled')
           AND EXISTS (
               SELECT 1 FROM tool_invocation_ledger ledger
               WHERE ledger.user_id = ar.user_id
                 AND ledger.session_id = ar.session_id
                 AND ledger.run_id = ar.run_id
           )
           AND (
               ar.updated_at > ?
               OR (ar.updated_at = ? AND ar.user_id > ?)
               OR (ar.updated_at = ? AND ar.user_id = ? AND ar.run_id > ?)
           )
         ORDER BY ar.updated_at, ar.user_id, ar.run_id
         LIMIT ?",
    )
    .bind(cursor_updated_at)
    .bind(cursor_updated_at)
    .bind(cursor_user_id)
    .bind(cursor_updated_at)
    .bind(cursor_user_id)
    .bind(cursor_run_id)
    .bind(limit)
    .fetch_all(&mut **tx)
    .await
}

async fn purge_expired_archive_indexes(
    pool: &SharedPool,
    limit: i64,
) -> Result<usize, sqlx::Error> {
    let expired = sqlx::query_as::<_, (String, String, String, u64)>(
        "SELECT chunks.user_id, chunks.session_id, chunks.run_id, chunks.chunk_index
         FROM tool_invocation_archive_chunks chunks
         LEFT JOIN session_artifacts artifacts
           ON artifacts.user_id = chunks.user_id
          AND artifacts.session_id = chunks.session_id
          AND artifacts.artifact_id = chunks.artifact_id
         WHERE artifacts.artifact_id IS NULL OR artifacts.status = 'expired'
         ORDER BY chunks.user_id, chunks.session_id, chunks.run_id, chunks.chunk_index
         LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool.get())
    .await?;
    let mut purged = 0;
    for (user_id, session_id, run_id, chunk_index) in expired {
        purged += usize::from(
            sqlx::query(
                "DELETE FROM tool_invocation_archive_chunks
                 WHERE user_id = ? AND session_id = ? AND run_id = ? AND chunk_index = ?
                   AND NOT EXISTS (
                       SELECT 1 FROM session_artifacts artifacts
                       WHERE artifacts.user_id = tool_invocation_archive_chunks.user_id
                         AND artifacts.session_id = tool_invocation_archive_chunks.session_id
                         AND artifacts.artifact_id = tool_invocation_archive_chunks.artifact_id
                         AND artifacts.status <> 'expired'
                   )",
            )
            .bind(user_id)
            .bind(session_id)
            .bind(run_id)
            .bind(chunk_index)
            .execute(pool.get())
            .await?
            .rows_affected()
                > 0,
        );
    }
    Ok(purged)
}

async fn release_expired_archive_references(
    pool: &SharedPool,
    limit: i64,
) -> Result<usize, sqlx::Error> {
    let due = sqlx::query_as::<_, (String, String, String)>(
        "SELECT chunks.user_id, chunks.session_id, chunks.run_id
         FROM tool_invocation_archive_chunks chunks
         JOIN session_artifacts artifacts
           ON artifacts.user_id = chunks.user_id
          AND artifacts.session_id = chunks.session_id
          AND artifacts.artifact_id = chunks.artifact_id
         WHERE EXISTS (
             SELECT 1 FROM session_artifact_references refs
             WHERE refs.user_id = chunks.user_id
               AND refs.session_id = chunks.session_id
               AND refs.reference_kind = 'invocation_ledger'
               AND refs.reference_id = chunks.run_id
         )
         GROUP BY chunks.user_id, chunks.session_id, chunks.run_id
         HAVING COUNT(artifacts.retention_until) = COUNT(*)
            AND MAX(artifacts.retention_until) <= CURRENT_TIMESTAMP(6)
         ORDER BY MAX(artifacts.retention_until), chunks.user_id, chunks.run_id
         LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool.get())
    .await?;
    let mut released: usize = 0;
    for (user_id, session_id, run_id) in due {
        let rows = sqlx::query(
            "DELETE FROM session_artifact_references
             WHERE user_id = ? AND session_id = ?
               AND reference_kind = 'invocation_ledger' AND reference_id = ?
               AND EXISTS (
                   SELECT 1
                   FROM tool_invocation_archive_chunks chunks
                   JOIN session_artifacts artifacts
                     ON artifacts.user_id = chunks.user_id
                    AND artifacts.session_id = chunks.session_id
                    AND artifacts.artifact_id = chunks.artifact_id
                   WHERE chunks.user_id = ? AND chunks.session_id = ?
                     AND chunks.run_id = ?
                   GROUP BY chunks.user_id, chunks.session_id, chunks.run_id
                   HAVING COUNT(artifacts.retention_until) = COUNT(*)
                      AND MAX(artifacts.retention_until) <= CURRENT_TIMESTAMP(6)
               )",
        )
        .bind(&user_id)
        .bind(&session_id)
        .bind(&run_id)
        .bind(&user_id)
        .bind(&session_id)
        .bind(&run_id)
        .execute(pool.get())
        .await?
        .rows_affected();
        released = released.saturating_add(usize::try_from(rows).unwrap_or(usize::MAX));
    }
    Ok(released)
}

pub(crate) fn spawn_tool_invocation_compactor(
    pool: SharedPool,
    lease: Arc<crate::server::sweeper_lease::SweeperLease>,
    cancel: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(COMPACTION_INTERVAL_SECS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {
                    match lease.check_leader().await {
                        crate::server::sweeper_lease::LeaderStatus::Leader => {}
                        crate::server::sweeper_lease::LeaderStatus::NotLeader => continue,
                        crate::server::sweeper_lease::LeaderStatus::Unavailable(error) => {
                            tracing::warn!(%error, "tool invocation compactor lease unavailable");
                            continue;
                        }
                    }
                    match run_tool_invocation_compaction_once(pool.clone(), 100).await {
                        Ok(outcome) => {
                            for failure in outcome.failures {
                                tracing::error!(
                                    user_id = %failure.user_id,
                                    session_id = %failure.session_id,
                                    run_id = %failure.run_id,
                                    phase = failure.phase,
                                    error = %failure.error,
                                    "tool invocation compaction failed for terminal run"
                                );
                            }
                            if outcome.runs_not_quiescent > 0 {
                                tracing::warn!(
                                    runs_not_quiescent = outcome.runs_not_quiescent,
                                    "terminal runs still contain non-terminal tool invocations"
                                );
                            }
                        }
                        Err(error) => tracing::error!(%error, "tool invocation compaction scan failed"),
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_turn_types::{
        DurableToolReference, ToolInvocationDecision, ToolInvocationFingerprint,
        ToolInvocationIdentity, ToolInvocationResultPayload, ToolInvocationTerminalOutcome,
    };
    use serde_json::json;
    use std::collections::BTreeMap;

    static ONLINE_POOL: tokio::sync::OnceCell<SharedPool> = tokio::sync::OnceCell::const_new();

    async fn online_pool() -> SharedPool {
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1"
        );
        ONLINE_POOL
            .get_or_init(|| async {
                let mut settings = astra_core::MatrixOneSettings::from_env();
                settings.db_pool_max_connections = settings.db_pool_max_connections.clamp(1, 8);
                settings.db_pool_min_connections = settings
                    .db_pool_min_connections
                    .min(settings.db_pool_max_connections);
                let catalog = std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG")
                    .unwrap_or_else(|_| "mysql".to_string());
                astra_services::ensure_core_schema(&settings, &catalog)
                    .await
                    .unwrap();
                SharedPool::new(&settings).await.unwrap()
            })
            .await
            .clone()
    }

    #[test]
    fn compaction_interval_and_batch_are_bounded() {
        assert_eq!(COMPACTION_INTERVAL_SECS, 300);
        assert_eq!(1_usize.clamp(1, 1_000), 1);
        assert_eq!(usize::MAX.clamp(1, 1_000), 1_000);
    }

    #[tokio::test]
    #[ignore = "requires ASTRA_TEST_DB_IT=1"]
    async fn expired_archive_release_is_owner_scoped_and_idempotent_on_matrixone() {
        let pool = online_pool().await;
        let suffix = uuid::Uuid::new_v4();
        let user_id = format!("archive-release-user-{suffix}");
        let session_id = format!("archive-release-session-{}", suffix.simple());
        let run_id = format!("archive-release-run-{}", suffix.simple());
        let artifact_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO session_artifacts
             (artifact_id, session_id, user_id, artifact_kind, content_json,
              retention_until, status, created_at)
             VALUES (?, ?, ?, 'tool_invocation_archive_v1', '{}',
                     TIMESTAMPADD(DAY, -1, CURRENT_TIMESTAMP(6)), 'active', CURRENT_TIMESTAMP(6))",
        )
        .bind(&artifact_id)
        .bind(&session_id)
        .bind(&user_id)
        .execute(pool.get())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO tool_invocation_archive_chunks
             (user_id, session_id, run_id, chunk_index, artifact_id,
              first_identity_key, last_identity_key, record_count, encoded_bytes)
             VALUES (?, ?, ?, 1, ?, ?, ?, 1, 2)",
        )
        .bind(&user_id)
        .bind(&session_id)
        .bind(&run_id)
        .bind(&artifact_id)
        .bind(format!("sha256:{}", "0".repeat(64)))
        .bind(format!("sha256:{}", "f".repeat(64)))
        .execute(pool.get())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO session_artifact_references
             (user_id, session_id, artifact_id, reference_kind, reference_id)
             VALUES (?, ?, ?, 'invocation_ledger', ?)",
        )
        .bind(&user_id)
        .bind(&session_id)
        .bind(&artifact_id)
        .bind(&run_id)
        .execute(pool.get())
        .await
        .unwrap();
        let result_artifact_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO session_artifacts
             (artifact_id, session_id, user_id, artifact_kind, content_json,
              retention_until, status, created_at)
             VALUES (?, ?, ?, 'tool_result_evidence_v1', '{}',
                     TIMESTAMPADD(DAY, -1, CURRENT_TIMESTAMP(6)), 'active', CURRENT_TIMESTAMP(6))",
        )
        .bind(&result_artifact_id)
        .bind(&session_id)
        .bind(&user_id)
        .execute(pool.get())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO session_artifact_references
             (user_id, session_id, artifact_id, reference_kind, reference_id)
             VALUES (?, ?, ?, 'invocation_ledger', ?)",
        )
        .bind(&user_id)
        .bind(&session_id)
        .bind(&result_artifact_id)
        .bind(&run_id)
        .execute(pool.get())
        .await
        .unwrap();

        let deadline_before: String = sqlx::query_scalar(
            "SELECT CAST(retention_until AS CHAR) FROM session_artifacts
             WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
        )
        .bind(&user_id)
        .bind(&session_id)
        .bind(&artifact_id)
        .fetch_one(pool.get())
        .await
        .unwrap();
        crate::server::artifact_retention_sweeper::run_artifact_retention_gc_once(
            pool.clone(),
            100,
        )
        .await
        .unwrap();
        let deadline_after: String = sqlx::query_scalar(
            "SELECT CAST(retention_until AS CHAR) FROM session_artifacts
             WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
        )
        .bind(&user_id)
        .bind(&session_id)
        .bind(&artifact_id)
        .fetch_one(pool.get())
        .await
        .unwrap();
        assert_eq!(
            deadline_after, deadline_before,
            "the generic retention sweep must not postpone archive-owner release"
        );
        let first = release_expired_archive_references(&pool, 10).await.unwrap();
        assert!(first >= 2);
        let _ = release_expired_archive_references(&pool, 10).await.unwrap();
        let references: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM session_artifact_references
             WHERE user_id = ? AND session_id = ?
               AND reference_kind = 'invocation_ledger' AND reference_id = ?",
        )
        .bind(&user_id)
        .bind(&session_id)
        .bind(&run_id)
        .fetch_one(pool.get())
        .await
        .unwrap();
        assert_eq!(references, 0);
        sqlx::query(
            "UPDATE session_artifacts SET status = 'expired'
             WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
        )
        .bind(&user_id)
        .bind(&session_id)
        .bind(&artifact_id)
        .execute(pool.get())
        .await
        .unwrap();
        assert!(purge_expired_archive_indexes(&pool, 10).await.unwrap() >= 1);
        let _ = purge_expired_archive_indexes(&pool, 10).await.unwrap();
        let chunks: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM tool_invocation_archive_chunks
             WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
        )
        .bind(&user_id)
        .bind(&session_id)
        .bind(&artifact_id)
        .fetch_one(pool.get())
        .await
        .unwrap();
        assert_eq!(chunks, 0);

        sqlx::query(
            "DELETE FROM tool_invocation_archive_chunks WHERE user_id = ? AND session_id = ?",
        )
        .bind(&user_id)
        .bind(&session_id)
        .execute(pool.get())
        .await
        .unwrap();
        sqlx::query("DELETE FROM session_artifacts WHERE user_id = ? AND session_id = ?")
            .bind(&user_id)
            .bind(&session_id)
            .execute(pool.get())
            .await
            .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires ASTRA_TEST_DB_IT=1"]
    async fn durable_keyset_cursor_does_not_let_stranded_runs_starve_later_runs() {
        let pool = online_pool().await;
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let user_id = format!("compaction-fair-user-{suffix}");
        let session_id = format!("compaction-fair-session-{suffix}");
        let cursor_name = format!("compaction-fair-cursor-{suffix}");
        let run_ids = [
            format!("fair-a-{suffix}"),
            format!("fair-b-{suffix}"),
            format!("fair-c-{suffix}"),
        ];
        sqlx::query(
            "INSERT INTO agent_sessions
             (session_id, user_id, agent_id, title, status, metadata, created_at, updated_at)
             VALUES (?, ?, 'compaction-test', 'compaction fairness', 'active', '{}', NOW(6), NOW(6))",
        )
        .bind(&session_id)
        .bind(&user_id)
        .execute(pool.get())
        .await
        .unwrap();
        let ledger =
            astra_services::tool_invocation_ledger::DatabaseToolInvocationLedger::new(pool.clone());
        let decision = ToolInvocationDecision::new(&json!({"route": "server_local"})).unwrap();
        let fingerprint = ToolInvocationFingerprint::new(
            DurableToolReference::built_in("bash", "registry-v1").unwrap(),
            &json!({"command": "fairness"}),
            &decision.decision_id,
        )
        .unwrap();
        let mut identities = Vec::new();
        for run_id in &run_ids {
            sqlx::query(
                "INSERT INTO agent_runs
                 (run_id, user_id, session_id, root_run_id, ancestor_path, status,
                  owner_pod_id, owner_lease_expires_at, run_generation)
                 VALUES (?, ?, ?, ?, ?, 'running', 'compactor-test-owner',
                         TIMESTAMPADD(MINUTE, 10, NOW(6)), 0)",
            )
            .bind(run_id)
            .bind(&user_id)
            .bind(&session_id)
            .bind(run_id)
            .bind(run_id)
            .execute(pool.get())
            .await
            .unwrap();
            let identity = ToolInvocationIdentity::new(
                &user_id,
                &session_id,
                run_id,
                format!("turn-{run_id}"),
                format!("call-{run_id}"),
            )
            .unwrap();
            ledger
                .prepare(&identity, &fingerprint, &decision)
                .await
                .unwrap();
            identities.push(identity);
        }
        ledger
            .claim_dispatch(
                &identities[2],
                "fair-worker",
                90_000,
                astra_services::tool_invocation_ledger::ToolInvocationDispatchAdmission {
                    expected_control_epoch: -1,
                    expected_owner_generation: 0,
                    expected_owner_pod_id: "compactor-test-owner".to_string(),
                },
            )
            .await
            .unwrap();
        ledger
            .compare_and_complete(
                &identities[2],
                astra_turn_types::ToolInvocationState::Dispatched,
                Some("fair-worker"),
                &ToolInvocationTerminalOutcome::Succeeded {
                    result: ToolInvocationResultPayload::new(
                        "done".to_string(),
                        BTreeMap::new(),
                        None,
                    )
                    .unwrap(),
                },
            )
            .await
            .unwrap();
        for (index, run_id) in run_ids.iter().enumerate() {
            sqlx::query(
                "UPDATE agent_runs
                 SET status = 'completed', updated_at = TIMESTAMPADD(SECOND, ?, '2090-01-01 00:00:00')
                 WHERE user_id = ? AND run_id = ?",
            )
            .bind(i64::try_from(index + 1).unwrap())
            .bind(&user_id)
            .bind(run_id)
            .execute(pool.get())
            .await
            .unwrap();
        }
        sqlx::query("DELETE FROM maintenance_sweep_cursors WHERE sweep_name = ?")
            .bind(&cursor_name)
            .execute(pool.get())
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO maintenance_sweep_cursors
             (sweep_name, cursor_updated_at, cursor_user_id, cursor_run_id)
             VALUES (?, '2089-12-31 23:59:59.000000', '', '')",
        )
        .bind(&cursor_name)
        .execute(pool.get())
        .await
        .unwrap();

        let (first, first_wrapped) = claim_compaction_candidates(&pool, &cursor_name, 2)
            .await
            .unwrap();
        assert!(!first_wrapped);
        assert_eq!(
            first
                .iter()
                .map(|(_, _, run_id)| run_id.as_str())
                .collect::<Vec<_>>(),
            vec![run_ids[0].as_str(), run_ids[1].as_str()]
        );
        let (second, second_wrapped) = claim_compaction_candidates(&pool, &cursor_name, 2)
            .await
            .unwrap();
        assert!(!second_wrapped);
        assert_eq!(
            second
                .iter()
                .map(|(_, _, run_id)| run_id.as_str())
                .collect::<Vec<_>>(),
            vec![run_ids[2].as_str()],
            "advancing past a full page of stranded runs must expose later terminal work"
        );
        let (_wrapped_page, wrapped) = claim_compaction_candidates(&pool, &cursor_name, 2)
            .await
            .unwrap();
        assert!(
            wrapped,
            "the durable cursor must wrap after reaching the end of the keyset"
        );

        sqlx::query("DELETE FROM maintenance_sweep_cursors WHERE sweep_name = ?")
            .bind(&cursor_name)
            .execute(pool.get())
            .await
            .unwrap();
        sqlx::query("DELETE FROM tool_invocation_ledger WHERE user_id = ? AND session_id = ?")
            .bind(&user_id)
            .bind(&session_id)
            .execute(pool.get())
            .await
            .unwrap();
        sqlx::query("DELETE FROM agent_runs WHERE user_id = ? AND session_id = ?")
            .bind(&user_id)
            .bind(&session_id)
            .execute(pool.get())
            .await
            .unwrap();
        sqlx::query("DELETE FROM agent_sessions WHERE user_id = ? AND session_id = ?")
            .bind(&user_id)
            .bind(&session_id)
            .execute(pool.get())
            .await
            .unwrap();
    }
}
