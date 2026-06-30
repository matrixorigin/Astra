use super::*;
use astra_services::db_row::{RowDecoder, RowExt};
use sqlx::Row;
use uuid::Uuid;

const SWEEP_INTERVAL_SECS: u64 = 3_600;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArtifactRetentionSweepOutcome {
    pub scanned: usize,
    pub corrupt_rows_skipped: usize,
    pub marked_expiring: usize,
    pub archived_cold: usize,
    pub extended: usize,
    pub expired: usize,
    pub backlog_overflow_warning: bool,
}

#[derive(Clone, Debug)]
struct ArtifactRetentionRow {
    artifact_id: String,
    user_id: String,
    session_id: String,
    retention_policy: String,
    referenced_by_manifest_count: i64,
    referenced_by_state_items_count: i64,
    referenced_by_citation_count: i64,
}

fn decode_artifact_retention_row(row: &impl RowExt) -> Result<ArtifactRetentionRow, String> {
    let dec = RowDecoder::new(row, "artifact retention row");
    let policy = {
        let value = dec.string("retention_policy")?;
        match value.as_str() {
            "default" | "permanent" | "project_long_term" => value,
            other => {
                return Err(dec.err_msg("retention_policy", format!("unknown policy `{other}`")));
            }
        }
    };
    Ok(ArtifactRetentionRow {
        artifact_id: dec.string("artifact_id")?,
        user_id: dec.string("user_id")?,
        session_id: dec.string("session_id")?,
        retention_policy: policy,
        referenced_by_manifest_count: dec.non_negative_i64("referenced_by_manifest_count")?,
        referenced_by_state_items_count: dec.non_negative_i64("referenced_by_state_items_count")?,
        referenced_by_citation_count: dec.non_negative_i64("referenced_by_citation_count")?,
    })
}

pub async fn run_artifact_retention_gc_once(
    pool: SharedPool,
    limit: u32,
) -> Result<ArtifactRetentionSweepOutcome, sqlx::Error> {
    let effective_limit = limit.max(1);
    let rows = sqlx::query(
        "SELECT artifact_id, user_id, session_id, retention_policy,
                referenced_by_manifest_count, referenced_by_state_items_count,
                referenced_by_citation_count
         FROM session_artifacts FORCE INDEX (idx_artifacts_retention)
         WHERE retention_until IS NOT NULL
           AND retention_until <= DATE_ADD(NOW(6), INTERVAL 7 DAY)
           AND status IN ('active', 'expiring')
           AND retention_policy <> 'permanent'
         ORDER BY retention_until ASC
         LIMIT ?",
    )
    .bind(i64::from(effective_limit))
    .fetch_all(pool.get())
    .await?;

    let mut outcome = ArtifactRetentionSweepOutcome {
        scanned: rows.len(),
        ..ArtifactRetentionSweepOutcome::default()
    };
    if outcome.scanned >= 1_000 || outcome.scanned >= effective_limit as usize {
        outcome.backlog_overflow_warning = true;
        if let Err(error) =
            record_artifact_retention_backlog_warning(&pool, outcome.scanned, effective_limit).await
        {
            tracing::warn!(
                target: "astra_runtime::artifact_retention_sweeper",
                scanned = outcome.scanned,
                limit = effective_limit,
                error = %error,
                "artifact retention backlog warning failed; continuing sweep"
            );
        }
    }
    for row in rows {
        let artifact = match decode_artifact_retention_row(&row) {
            Ok(artifact) => artifact,
            Err(error) => {
                outcome.corrupt_rows_skipped += 1;
                tracing::warn!(
                    target: "astra_runtime::artifact_retention_sweeper",
                    error = %error,
                    "skipping corrupt artifact retention row"
                );
                continue;
            }
        };
        match apply_artifact_retention_policy(&pool, &artifact).await? {
            ArtifactRetentionAction::MarkedExpiring => outcome.marked_expiring += 1,
            ArtifactRetentionAction::ArchivedCold => outcome.archived_cold += 1,
            ArtifactRetentionAction::Extended => outcome.extended += 1,
            ArtifactRetentionAction::Expired => outcome.expired += 1,
            ArtifactRetentionAction::Noop => {}
        }
    }
    Ok(outcome)
}

async fn record_artifact_retention_backlog_warning(
    pool: &SharedPool,
    scanned: usize,
    limit: u32,
) -> Result<(), sqlx::Error> {
    let event_id = Uuid::new_v4().to_string();
    let mut tx = pool.get().begin().await?;
    let insert_result = sqlx::query(
        "INSERT INTO agent_events
         (event_id, session_id, user_id, event_type, content, metadata, created_at)
         VALUES (?, 'system', 'system', 'artifact_retention_backlog_overflow', ?, ?, NOW(6))",
    )
    .bind(&event_id)
    .bind(format!(
        "artifact retention sweep reached scan limit: scanned={scanned}, limit={limit}"
    ))
    .bind(
        serde_json::json!({
            "scanned": scanned,
            "limit": limit,
            "action": "reschedule_and_alert",
        })
        .to_string(),
    )
    .execute(&mut *tx)
    .await?;
    let inserted_events = match i64::try_from(insert_result.rows_affected()) {
        Ok(count) if count > 0 => count,
        Ok(_) => return Err(sqlx::Error::RowNotFound),
        Err(_) => {
            return Err(sqlx::Error::Protocol(
                "artifact retention backlog warning row count overflow".into(),
            ));
        }
    };
    astra_services::storage::add_agent_session_event_count_or_create(
        &mut *tx,
        "system",
        "system",
        inserted_events,
        Some(&event_id),
    )
    .await?;
    tx.commit().await?;
    tracing::warn!(
        target: "astra_runtime::artifact_retention_sweeper",
        scanned,
        limit,
        "artifact retention sweep reached scan limit"
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ArtifactRetentionAction {
    Noop,
    MarkedExpiring,
    ArchivedCold,
    Extended,
    Expired,
}

async fn apply_artifact_retention_policy(
    pool: &SharedPool,
    artifact: &ArtifactRetentionRow,
) -> Result<ArtifactRetentionAction, sqlx::Error> {
    if artifact.retention_policy == "permanent" {
        return Ok(ArtifactRetentionAction::Noop);
    }
    if artifact.retention_policy == "project_long_term" {
        let rows_affected = sqlx::query(
            "UPDATE session_artifacts
             SET status = 'active',
                 retention_until = DATE_ADD(NOW(6), INTERVAL 365 DAY),
                 updated_at = NOW(6)
             WHERE user_id = ? AND session_id = ? AND artifact_id = ?
               AND status IN ('active', 'expiring')",
        )
        .bind(&artifact.user_id)
        .bind(&artifact.session_id)
        .bind(&artifact.artifact_id)
        .execute(pool.get())
        .await?
        .rows_affected();
        return Ok(if rows_affected > 0 {
            ArtifactRetentionAction::Extended
        } else {
            ArtifactRetentionAction::Noop
        });
    }

    let refs = artifact
        .referenced_by_manifest_count
        .saturating_add(artifact.referenced_by_state_items_count)
        .saturating_add(artifact.referenced_by_citation_count);
    if refs > 0 {
        let cold_ref = format!(
            "cold_storage://session/{}/artifacts/{}",
            artifact.session_id, artifact.artifact_id
        );
        let rows_affected = sqlx::query(
            "UPDATE session_artifacts
             SET status = 'active',
                 cold_storage_ref = COALESCE(cold_storage_ref, ?),
                 retention_until = DATE_ADD(NOW(6), INTERVAL 365 DAY),
                 updated_at = NOW(6)
             WHERE user_id = ? AND session_id = ? AND artifact_id = ?
               AND status IN ('active', 'expiring')",
        )
        .bind(cold_ref)
        .bind(&artifact.user_id)
        .bind(&artifact.session_id)
        .bind(&artifact.artifact_id)
        .execute(pool.get())
        .await?
        .rows_affected();
        return Ok(if rows_affected > 0 {
            ArtifactRetentionAction::ArchivedCold
        } else {
            ArtifactRetentionAction::Noop
        });
    }

    let expired_rows = sqlx::query(
        "UPDATE session_artifacts
         SET status = 'expired', updated_at = NOW(6)
         WHERE user_id = ? AND session_id = ? AND artifact_id = ?
           AND status IN ('active', 'expiring')
           AND retention_until IS NOT NULL
           AND retention_until <= NOW(6)",
    )
    .bind(&artifact.user_id)
    .bind(&artifact.session_id)
    .bind(&artifact.artifact_id)
    .execute(pool.get())
    .await?
    .rows_affected();
    if expired_rows > 0 {
        return Ok(ArtifactRetentionAction::Expired);
    }

    let marked_rows = sqlx::query(
        "UPDATE session_artifacts
         SET status = 'expiring', updated_at = NOW(6)
         WHERE user_id = ? AND session_id = ? AND artifact_id = ?
           AND status IN ('active', 'expiring')
           AND retention_until IS NOT NULL
           AND retention_until > NOW(6)",
    )
    .bind(&artifact.user_id)
    .bind(&artifact.session_id)
    .bind(&artifact.artifact_id)
    .execute(pool.get())
    .await?
    .rows_affected();
    Ok(if marked_rows > 0 {
        ArtifactRetentionAction::MarkedExpiring
    } else {
        ArtifactRetentionAction::Noop
    })
}

pub(crate) fn spawn_artifact_retention_sweeper(
    pool: SharedPool,
    lease: Arc<crate::server::sweeper_lease::SweeperLease>,
    cancel: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(SWEEP_INTERVAL_SECS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {
                    'tick: {
                        match lease.check_leader().await {
                            crate::server::sweeper_lease::LeaderStatus::Leader => {}
                            crate::server::sweeper_lease::LeaderStatus::NotLeader => break 'tick,
                            crate::server::sweeper_lease::LeaderStatus::Unavailable(e) => {
                                tracing::warn!(
                                    target: "astra_runtime::artifact_retention_sweeper",
                                    error = %e,
                                    "sweeper lease check unavailable, skipping sweep"
                                );
                                break 'tick;
                            }
                        }
                        if let Err(error) = run_artifact_retention_gc_once(pool.clone(), 1_000).await {
                            tracing::warn!(
                                target: "astra_runtime::artifact_retention_sweeper",
                                error = %error,
                                "artifact retention sweeper failed"
                            );
                        }
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup_live_pool() -> SharedPool {
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 for ignored integration tests"
        );
        let settings = astra_core::MatrixOneSettings::from_env();
        let catalog =
            std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
        astra_services::ensure_core_schema(&settings, &catalog)
            .await
            .expect("ensure_core_schema");
        SharedPool::new(&settings).await.expect("SharedPool::new")
    }

    struct FakeArtifactRetentionRow {
        failed_column: Option<&'static str>,
        retention_policy: &'static str,
        referenced_by_manifest_count: i64,
        referenced_by_state_items_count: i64,
        referenced_by_citation_count: i64,
    }

    impl Default for FakeArtifactRetentionRow {
        fn default() -> Self {
            Self {
                failed_column: None,
                retention_policy: "default",
                referenced_by_manifest_count: 1,
                referenced_by_state_items_count: 2,
                referenced_by_citation_count: 3,
            }
        }
    }

    impl FakeArtifactRetentionRow {
        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
                ..Self::default()
            }
        }

        fn with_retention_policy(retention_policy: &'static str) -> Self {
            Self {
                retention_policy,
                ..Self::default()
            }
        }

        fn with_count(column: &'static str, value: i64) -> Self {
            let mut row = Self::default();
            match column {
                "referenced_by_manifest_count" => row.referenced_by_manifest_count = value,
                "referenced_by_state_items_count" => row.referenced_by_state_items_count = value,
                "referenced_by_citation_count" => row.referenced_by_citation_count = value,
                _ => unreachable!("unexpected count column: {column}"),
            }
            row
        }

        fn maybe_fail(&self, column: &str) -> Result<(), sqlx::Error> {
            if self.failed_column == Some(column) {
                Err(sqlx::Error::ColumnNotFound(column.to_string()))
            } else {
                Ok(())
            }
        }
    }

    impl RowExt for FakeArtifactRetentionRow {
        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            self.maybe_fail(column)?;
            let value = match column {
                "artifact_id" => "artifact-1",
                "user_id" => "user-1",
                "session_id" => "session-1",
                "retention_policy" => self.retention_policy,
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            };
            Ok(value.to_string())
        }

        fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
            self.maybe_fail(column)?;
            match column {
                "referenced_by_manifest_count" => Ok(self.referenced_by_manifest_count),
                "referenced_by_state_items_count" => Ok(self.referenced_by_state_items_count),
                "referenced_by_citation_count" => Ok(self.referenced_by_citation_count),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }
    }

    #[test]
    fn artifact_retention_row_decode_preserves_database_values() {
        let row = decode_artifact_retention_row(&FakeArtifactRetentionRow::default()).unwrap();

        assert_eq!(row.artifact_id, "artifact-1");
        assert_eq!(row.user_id, "user-1");
        assert_eq!(row.session_id, "session-1");
        assert_eq!(row.retention_policy, "default");
        assert_eq!(row.referenced_by_manifest_count, 1);
        assert_eq!(row.referenced_by_state_items_count, 2);
        assert_eq!(row.referenced_by_citation_count, 3);
    }

    #[test]
    fn artifact_retention_row_decode_fails_loudly_on_any_selected_column_error() {
        for column in [
            "artifact_id",
            "user_id",
            "session_id",
            "retention_policy",
            "referenced_by_manifest_count",
            "referenced_by_state_items_count",
            "referenced_by_citation_count",
        ] {
            let error = decode_artifact_retention_row(&FakeArtifactRetentionRow::fail_on(column))
                .unwrap_err();
            assert!(
                error.contains("artifact retention row decode") && error.contains(column),
                "decode error should identify selected column `{column}`: {error}"
            );
        }
    }

    #[test]
    fn artifact_retention_row_decode_rejects_unknown_policy_and_negative_counts() {
        let policy = decode_artifact_retention_row(
            &FakeArtifactRetentionRow::with_retention_policy("delete_everything"),
        )
        .unwrap_err();
        assert!(
            policy.contains("retention_policy") && policy.contains("unknown policy"),
            "unknown retention policy should fail loudly: {policy}"
        );

        for column in [
            "referenced_by_manifest_count",
            "referenced_by_state_items_count",
            "referenced_by_citation_count",
        ] {
            let error =
                decode_artifact_retention_row(&FakeArtifactRetentionRow::with_count(column, -1))
                    .unwrap_err();
            assert!(
                error.contains(column) && error.contains("non-negative integer"),
                "negative count should fail loudly for `{column}`: {error}"
            );
        }
    }

    #[test]
    fn sweeper_interval_is_one_hour() {
        assert_eq!(super::SWEEP_INTERVAL_SECS, 3_600);
    }

    #[test]
    fn sweeper_query_filters_status_without_casting_indexed_column() {
        let source = include_str!("artifact_retention_sweeper.rs");
        let body = source
            .split("pub async fn run_artifact_retention_gc_once")
            .nth(1)
            .and_then(|rest| {
                rest.split("async fn record_artifact_retention_backlog_warning")
                    .next()
            })
            .expect("artifact retention query body");
        assert!(body.contains("status IN ('active', 'expiring')"));
        assert!(
            !body.contains("CAST(status AS CHAR)"),
            "retention sweeper must not cast status in WHERE because that blocks index filtering"
        );
    }

    #[test]
    fn sweeper_query_excludes_permanent_artifacts_from_hot_scan() {
        let source = include_str!("artifact_retention_sweeper.rs");
        let body = source
            .split("pub async fn run_artifact_retention_gc_once")
            .nth(1)
            .and_then(|rest| {
                rest.split("async fn record_artifact_retention_backlog_warning")
                    .next()
            })
            .expect("artifact retention query body");
        assert!(
            body.contains("retention_policy <> 'permanent'"),
            "permanent artifacts with stale retention_until must not be scanned every sweep"
        );
    }

    #[test]
    fn sweeper_expiration_uses_database_clock() {
        let source = include_str!("artifact_retention_sweeper.rs");
        let body = source
            .split("async fn apply_artifact_retention_policy")
            .nth(1)
            .and_then(|rest| {
                rest.split("pub(crate) fn spawn_artifact_retention_sweeper")
                    .next()
            })
            .expect("artifact retention policy body");
        assert!(
            body.contains("retention_until <= NOW(6)") && body.contains("retention_until > NOW(6)"),
            "expiration boundary must use the database clock for DB rows"
        );
        assert!(
            !body.contains("chrono::Utc::now"),
            "application clock drift must not decide DB artifact expiration"
        );
    }

    #[test]
    fn sweeper_skips_corrupt_rows_instead_of_failing_whole_sweep() {
        let source = include_str!("artifact_retention_sweeper.rs");
        let body = source
            .split("pub async fn run_artifact_retention_gc_once")
            .nth(1)
            .and_then(|rest| {
                rest.split("async fn record_artifact_retention_backlog_warning")
                    .next()
            })
            .expect("artifact retention query body");
        assert!(body.contains("corrupt_rows_skipped += 1"));
        assert!(
            body.contains("skipping corrupt artifact retention row") && body.contains("continue;"),
            "a single corrupt retention row must not abort the whole sweep"
        );
        assert!(
            !body.contains("decode_artifact_retention_row(&row).map_err(sqlx::Error::Protocol)?"),
            "decode errors should be isolated at the sweeper row boundary"
        );
    }

    #[test]
    fn sweeper_backlog_warning_is_best_effort() {
        let source = include_str!("artifact_retention_sweeper.rs");
        let body = source
            .split("pub async fn run_artifact_retention_gc_once")
            .nth(1)
            .and_then(|rest| {
                rest.split("async fn record_artifact_retention_backlog_warning")
                    .next()
            })
            .expect("artifact retention query body");
        assert!(
            body.contains("let effective_limit = limit.max(1);"),
            "limit must be normalized before query binding and backlog comparison"
        );
        assert!(
            body.contains("if let Err(error) =")
                && body.contains("artifact retention backlog warning failed; continuing sweep"),
            "backlog observability failure must not abort artifact retention actions"
        );
        assert!(
            !body.contains(
                "record_artifact_retention_backlog_warning(&pool, outcome.scanned, limit).await?"
            ),
            "best-effort warning writes must not use ? from the sweep hot path"
        );
    }

    #[tokio::test]
    #[ignore = "requires ASTRA_TEST_DB_IT=1"]
    #[serial_test::serial(artifact_retention_system_event_count)]
    async fn backlog_warning_updates_system_session_event_count_by_insert_delta() {
        let pool = setup_live_pool().await;
        let before = sqlx::query(
            "SELECT event_count, last_event_id FROM agent_sessions \
             WHERE session_id = 'system' AND user_id = 'system'",
        )
        .fetch_optional(pool.get())
        .await
        .expect("load system session before warning")
        .map(|row| {
            (
                row.try_get::<i64, _>("event_count")
                    .expect("decode before event_count"),
                row.try_get::<Option<String>, _>("last_event_id")
                    .expect("decode before last_event_id"),
            )
        });
        let before_count = before.as_ref().map(|(count, _)| *count).unwrap_or(0);

        record_artifact_retention_backlog_warning(&pool, 7, 7)
            .await
            .expect("record backlog warning");

        let after = sqlx::query(
            "SELECT event_count, last_event_id FROM agent_sessions \
             WHERE session_id = 'system' AND user_id = 'system'",
        )
        .fetch_one(pool.get())
        .await
        .expect("load system session after warning");
        let after_count = after
            .try_get::<i64, _>("event_count")
            .expect("decode after event_count");
        let event_id = after
            .try_get::<Option<String>, _>("last_event_id")
            .expect("decode after last_event_id")
            .expect("warning should update last_event_id");
        assert_eq!(
            after_count,
            before_count + 1,
            "system session summary must advance by inserted warning rows"
        );

        let warning_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_events \
             WHERE event_id = ? AND session_id = 'system' AND user_id = 'system' \
               AND event_type = 'artifact_retention_backlog_overflow'",
        )
        .bind(&event_id)
        .fetch_one(pool.get())
        .await
        .expect("count inserted warning event");
        assert_eq!(warning_rows, 1);

        sqlx::query("DELETE FROM agent_events WHERE event_id = ? AND user_id = 'system'")
            .bind(&event_id)
            .execute(pool.get())
            .await
            .expect("cleanup artifact retention backlog warning event");
        match before {
            Some((event_count, last_event_id)) => {
                sqlx::query(
                    "UPDATE agent_sessions SET event_count = ?, last_event_id = ? \
                     WHERE session_id = 'system' AND user_id = 'system'",
                )
                .bind(event_count)
                .bind(last_event_id)
                .execute(pool.get())
                .await
                .expect("restore artifact retention system session summary");
            }
            None => {
                sqlx::query(
                    "DELETE FROM agent_sessions WHERE session_id = 'system' AND user_id = 'system'",
                )
                .execute(pool.get())
                .await
                .expect("cleanup artifact retention system session summary");
            }
        }
    }
}
