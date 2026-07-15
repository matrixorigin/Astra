use std::sync::Arc;

use astra_core::SharedPool;

const COMPACTION_INTERVAL_SECS: u64 = 300;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ToolInvocationCompactionSweepOutcome {
    pub runs_scanned: usize,
    pub records_archived: usize,
    pub runs_remaining: usize,
    pub runs_not_quiescent: usize,
    pub expired_archive_references_released: usize,
    pub expired_archive_indexes_purged: usize,
    pub failures: Vec<ToolInvocationCompactionFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolInvocationCompactionFailure {
    pub user_id: String,
    pub session_id: String,
    pub run_id: String,
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
    let candidates = sqlx::query_as::<_, (String, String, String)>(
        "SELECT runs.user_id, runs.session_id, runs.run_id
         FROM agent_runs runs
         WHERE runs.status IN ('completed', 'delegated', 'failed', 'cancelled')
           AND EXISTS (
               SELECT 1 FROM tool_invocation_ledger ledger
               WHERE ledger.user_id = runs.user_id
                 AND ledger.session_id = runs.session_id
                 AND ledger.run_id = runs.run_id
           )
         ORDER BY runs.updated_at, runs.user_id, runs.run_id
         LIMIT ?",
    )
    .bind(effective_limit)
    .fetch_all(pool.get())
    .await?;
    let ledger = astra_services::tool_invocation_ledger::DatabaseToolInvocationLedger::new(pool);
    let mut outcome = ToolInvocationCompactionSweepOutcome {
        runs_scanned: candidates.len(),
        expired_archive_references_released,
        expired_archive_indexes_purged,
        ..Default::default()
    };
    for (user_id, session_id, run_id) in candidates {
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
                error: error.to_string(),
            }),
        }
    }
    Ok(outcome)
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

    #[test]
    fn compaction_interval_and_batch_are_bounded() {
        assert_eq!(COMPACTION_INTERVAL_SECS, 300);
        assert_eq!(1_usize.clamp(1, 1_000), 1);
        assert_eq!(usize::MAX.clamp(1, 1_000), 1_000);
    }

    #[tokio::test]
    #[ignore = "requires ASTRA_TEST_DB_IT=1"]
    async fn expired_archive_release_is_owner_scoped_and_idempotent_on_matrixone() {
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1"
        );
        let settings = astra_core::MatrixOneSettings::from_env();
        let catalog = std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG")
            .unwrap_or_else(|_| "mysql".to_string());
        astra_services::ensure_core_schema(&settings, &catalog)
            .await
            .unwrap();
        let pool = SharedPool::new(&settings).await.unwrap();
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
}
