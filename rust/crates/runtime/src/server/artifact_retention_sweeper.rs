use super::*;
use chrono::NaiveDateTime;
use sqlx::Row;
use uuid::Uuid;

const SWEEP_INTERVAL_SECS: u64 = 3_600;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArtifactRetentionSweepOutcome {
    pub scanned: usize,
    pub marked_expiring: usize,
    pub archived_cold: usize,
    pub extended: usize,
    pub expired: usize,
    pub backlog_overflow_warning: bool,
}

#[derive(Clone, Debug)]
struct ArtifactRetentionRow {
    artifact_id: String,
    session_id: String,
    retention_policy: String,
    retention_until: Option<NaiveDateTime>,
    referenced_by_manifest_count: i64,
    referenced_by_state_items_count: i64,
    referenced_by_citation_count: i64,
}

pub async fn run_artifact_retention_gc_once(
    pool: SharedPool,
    limit: u32,
) -> Result<ArtifactRetentionSweepOutcome, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT artifact_id, session_id, retention_policy, retention_until,
                referenced_by_manifest_count, referenced_by_state_items_count,
                referenced_by_citation_count
         FROM session_artifacts FORCE INDEX (idx_artifacts_retention)
         WHERE status IN ('active', 'expiring')
           AND retention_until IS NOT NULL
           AND retention_until <= DATE_ADD(NOW(6), INTERVAL 7 DAY)
         ORDER BY retention_until ASC
         LIMIT ?",
    )
    .bind(i64::from(limit.max(1)))
    .fetch_all(pool.get())
    .await?;

    let mut outcome = ArtifactRetentionSweepOutcome {
        scanned: rows.len(),
        ..ArtifactRetentionSweepOutcome::default()
    };
    if outcome.scanned >= 1_000 || outcome.scanned >= limit as usize {
        outcome.backlog_overflow_warning = true;
        record_artifact_retention_backlog_warning(&pool, outcome.scanned, limit).await?;
    }
    for row in rows {
        let artifact = ArtifactRetentionRow {
            artifact_id: row.try_get("artifact_id").unwrap_or_default(),
            session_id: row.try_get("session_id").unwrap_or_default(),
            retention_policy: row.try_get("retention_policy").unwrap_or_default(),
            retention_until: row.try_get("retention_until").ok(),
            referenced_by_manifest_count: row
                .try_get::<i64, _>("referenced_by_manifest_count")
                .unwrap_or(0),
            referenced_by_state_items_count: row
                .try_get::<i64, _>("referenced_by_state_items_count")
                .unwrap_or(0),
            referenced_by_citation_count: row
                .try_get::<i64, _>("referenced_by_citation_count")
                .unwrap_or(0),
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
    sqlx::query(
        "INSERT INTO agent_events
         (event_id, session_id, user_id, event_type, content, metadata, created_at)
         VALUES (?, 'system', 'system', 'artifact_retention_backlog_overflow', ?, ?, NOW(6))",
    )
    .bind(Uuid::new_v4().to_string())
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
    .execute(pool.get())
    .await?;
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
        sqlx::query(
            "UPDATE session_artifacts
             SET status = 'active',
                 retention_until = DATE_ADD(NOW(6), INTERVAL 365 DAY),
                 updated_at = NOW(6)
             WHERE artifact_id = ?",
        )
        .bind(&artifact.artifact_id)
        .execute(pool.get())
        .await?;
        return Ok(ArtifactRetentionAction::Extended);
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
        sqlx::query(
            "UPDATE session_artifacts
             SET status = 'active',
                 cold_storage_ref = COALESCE(cold_storage_ref, ?),
                 retention_until = DATE_ADD(NOW(6), INTERVAL 365 DAY),
                 updated_at = NOW(6)
             WHERE artifact_id = ?",
        )
        .bind(cold_ref)
        .bind(&artifact.artifact_id)
        .execute(pool.get())
        .await?;
        return Ok(ArtifactRetentionAction::ArchivedCold);
    }

    if artifact
        .retention_until
        .is_some_and(|value| value <= chrono::Utc::now().naive_utc())
    {
        sqlx::query(
            "UPDATE session_artifacts
             SET status = 'expired', updated_at = NOW(6)
             WHERE artifact_id = ?",
        )
        .bind(&artifact.artifact_id)
        .execute(pool.get())
        .await?;
        return Ok(ArtifactRetentionAction::Expired);
    }

    sqlx::query(
        "UPDATE session_artifacts
         SET status = 'expiring', updated_at = NOW(6)
         WHERE artifact_id = ?",
    )
    .bind(&artifact.artifact_id)
    .execute(pool.get())
    .await?;
    Ok(ArtifactRetentionAction::MarkedExpiring)
}

pub(crate) fn spawn_artifact_retention_sweeper(
    pool: SharedPool,
    lease: Arc<crate::server::sweeper_lease::SweeperLease>,
) {
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(SWEEP_INTERVAL_SECS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            match lease.check_leader().await {
                crate::server::sweeper_lease::LeaderStatus::Leader => {}
                crate::server::sweeper_lease::LeaderStatus::NotLeader => continue,
                crate::server::sweeper_lease::LeaderStatus::Unavailable(e) => {
                    tracing::warn!(
                        target: "astra_runtime::artifact_retention_sweeper",
                        error = %e,
                        "sweeper lease check unavailable, skipping sweep"
                    );
                    continue;
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
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn sweeper_interval_is_one_hour() {
        assert_eq!(super::SWEEP_INTERVAL_SECS, 3_600);
    }
}
