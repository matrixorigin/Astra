use super::events::{NewWorkEvent, WorkEventKind};
use super::repository::{WorkConflictResource, WorkRepositoryError};
use super::{GraphRevision, WorkBranchId, WorkChangeRef, WorkId, WorkOwnerId};
use crate::runs::{DurableRunStatusKind, DurableWorkRunBinding, durable_run_status_kind};
use astra_core::SharedPool;
use sqlx::{MySql, Row, Transaction, query};
use std::time::Duration;

const RUNTIME_EVENT_RING_CAPACITY: i64 = 1_024;
const MAX_PROJECTION_BATCH: u16 = 100;
const MAX_STEPS_PER_WORK_PER_SWEEP: u16 = 8;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorkRuntimeEventProjectionResult {
    pub projected: u64,
    /// Explicit Work events emitted when a projector fell behind the retained ring.
    pub coverage_expired: u64,
}

#[derive(Clone, Debug)]
struct PendingRuntimeEvent {
    owner_id: WorkOwnerId,
    work_id: WorkId,
    branch_id: WorkBranchId,
    kind: WorkEventKind,
    graph_revision: GraphRevision,
    source_ref: WorkChangeRef,
}

enum ProjectionStep {
    Projected,
    CoverageExpired,
    Current,
}

pub(super) async fn insert_genesis_slot(
    transaction: &mut Transaction<'_, MySql>,
    owner_id: &WorkOwnerId,
    work_id: &WorkId,
) -> Result<(), WorkRepositoryError> {
    query(
        "INSERT INTO work_runtime_event_outbox_slots
         (owner_id, work_id, last_enqueued_event_seq, last_projected_event_seq, has_pending)
         VALUES (?, ?, 0, 0, 0)",
    )
    .bind(owner_id.as_str())
    .bind(work_id.as_str())
    .execute(&mut **transaction)
    .await
    .map_err(|source| {
        WorkRepositoryError::insert(
            "insert Work runtime event outbox slot",
            WorkConflictResource::WorkEventSequence,
            source,
        )
    })?;
    Ok(())
}

/// Commit a user-relevant root Run terminal fact to a fixed-size per-Work
/// delivery ring in the authoritative Run transaction. Projection can lag or
/// fail without changing the Run outcome. If lag exceeds the ring, the
/// projector emits a typed coverage-expired fact instead of inventing status
/// or silently dropping history.
pub(crate) async fn enqueue_root_run_terminal_event(
    transaction: &mut Transaction<'_, MySql>,
    run_id: &str,
    owner_id: &str,
    parent_run_id: Option<&str>,
    binding: Option<&DurableWorkRunBinding>,
    previous_status: &str,
    next_status: &str,
) -> Result<(), String> {
    if previous_status == next_status || parent_run_id.is_some() {
        return Ok(());
    }
    let kind = match durable_run_status_kind(next_status) {
        DurableRunStatusKind::Completed => WorkEventKind::RunCompleted,
        DurableRunStatusKind::Delegated => WorkEventKind::RunDelegated,
        DurableRunStatusKind::Failed => WorkEventKind::RunFailed,
        DurableRunStatusKind::Cancelled => WorkEventKind::RunCancelled,
        DurableRunStatusKind::Running
        | DurableRunStatusKind::Waiting
        | DurableRunStatusKind::Paused
        | DurableRunStatusKind::Other => return Ok(()),
    };
    let Some(binding) = binding else {
        return Ok(());
    };
    let owner_id = WorkOwnerId::parse(owner_id.to_owned())
        .map_err(|error| format!("invalid Work-bound Run owner: {error}"))?;
    let source_ref = WorkChangeRef::parse(format!("run:{run_id}"))
        .map_err(|error| format!("invalid Work-bound Run identity: {error}"))?;

    let advanced = query(
        "UPDATE work_runtime_event_outbox_slots
         SET last_enqueued_event_seq = last_enqueued_event_seq + 1,
             has_pending = 1, updated_at = NOW(6)
         WHERE owner_id = ? AND work_id = ? AND last_enqueued_event_seq < ?",
    )
    .bind(owner_id.as_str())
    .bind(binding.work_id().as_str())
    .bind(i64::MAX)
    .execute(&mut **transaction)
    .await
    .map_err(|error| format!("allocate Work runtime event sequence: {error}"))?;
    if advanced.rows_affected() != 1 {
        return Err("Work runtime event sequence is missing or exhausted".to_string());
    }
    let runtime_event_seq: i64 = query(
        "SELECT last_enqueued_event_seq FROM work_runtime_event_outbox_slots
         WHERE owner_id = ? AND work_id = ? FOR UPDATE",
    )
    .bind(owner_id.as_str())
    .bind(binding.work_id().as_str())
    .fetch_one(&mut **transaction)
    .await
    .map_err(|error| format!("read Work runtime event sequence: {error}"))?
    .try_get("last_enqueued_event_seq")
    .map_err(|error| format!("decode Work runtime event sequence: {error}"))?;

    query(
        "INSERT INTO work_runtime_event_outbox
         (owner_id, work_id, branch_id, runtime_event_seq, event_kind, graph_revision, source_ref)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(owner_id.as_str())
    .bind(binding.work_id().as_str())
    .bind(binding.branch_id().as_str())
    .bind(runtime_event_seq)
    .bind(kind.as_str())
    .bind(binding.graph_revision().get())
    .bind(source_ref.as_str())
    .execute(&mut **transaction)
    .await
    .map_err(|error| format!("enqueue Work runtime event: {error}"))?;

    let retained_from = retained_from(runtime_event_seq);
    if retained_from > 1 {
        query(
            "DELETE FROM work_runtime_event_outbox
             WHERE owner_id = ? AND work_id = ? AND runtime_event_seq < ?",
        )
        .bind(owner_id.as_str())
        .bind(binding.work_id().as_str())
        .bind(retained_from)
        .execute(&mut **transaction)
        .await
        .map_err(|error| format!("prune Work runtime event ring: {error}"))?;
    }
    Ok(())
}

/// Project a bounded, owner-fair batch. A sweep chooses one dirty Work per
/// owner, then advances those Works round-robin with a per-Work burst cap.
pub async fn project_pending_runtime_events(
    pool: &SharedPool,
    limit: u16,
) -> Result<WorkRuntimeEventProjectionResult, String> {
    if limit == 0 || limit > MAX_PROJECTION_BATCH {
        return Err(format!(
            "Work runtime event projection limit must be between 1 and {MAX_PROJECTION_BATCH}"
        ));
    }
    let mut result = WorkRuntimeEventProjectionResult::default();
    let candidates = query(
        "SELECT owner_id, work_id
         FROM (
           SELECT owner_id, work_id, updated_at,
                  ROW_NUMBER() OVER (
                    PARTITION BY owner_id ORDER BY updated_at, work_id
                  ) AS owner_rank
           FROM work_runtime_event_outbox_slots
           WHERE has_pending = 1
         ) ranked
         WHERE owner_rank = 1
         ORDER BY updated_at, owner_id, work_id
         LIMIT ?",
    )
    .bind(i64::from(limit))
    .fetch_all(pool.get())
    .await
    .map_err(|error| format!("select Work runtime event candidates: {error}"))?;
    let candidates = candidates
        .into_iter()
        .map(|candidate| {
            let owner_id = candidate
                .try_get::<String, _>("owner_id")
                .map_err(|error| format!("decode Work runtime event owner: {error}"))?;
            let work_id = candidate
                .try_get::<String, _>("work_id")
                .map_err(|error| format!("decode Work runtime event Work: {error}"))?;
            Ok((owner_id, work_id))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut active = vec![true; candidates.len()];
    for _ in 0..MAX_STEPS_PER_WORK_PER_SWEEP {
        for (index, (owner_id, work_id)) in candidates.iter().enumerate() {
            if !active[index] {
                continue;
            }
            match project_one(pool, owner_id, work_id).await? {
                ProjectionStep::Projected => result.projected += 1,
                ProjectionStep::CoverageExpired => result.coverage_expired += 1,
                ProjectionStep::Current => active[index] = false,
            }
            if result.total() >= u64::from(limit) {
                break;
            }
        }
        if result.total() >= u64::from(limit) {
            break;
        }
        if active.iter().all(|is_active| !is_active) {
            break;
        }
    }
    Ok(result)
}

impl WorkRuntimeEventProjectionResult {
    fn total(self) -> u64 {
        self.projected + self.coverage_expired
    }
}

async fn project_one(
    pool: &SharedPool,
    owner_id: &str,
    work_id: &str,
) -> Result<ProjectionStep, String> {
    let mut transaction = pool
        .get()
        .begin()
        .await
        .map_err(|error| format!("begin Work runtime event projection: {error}"))?;
    let slot = query(
        "SELECT last_enqueued_event_seq, last_projected_event_seq
         FROM work_runtime_event_outbox_slots
         WHERE owner_id = ? AND work_id = ? FOR UPDATE",
    )
    .bind(owner_id)
    .bind(work_id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|error| format!("lock Work runtime event slot: {error}"))?;
    let Some(slot) = slot else {
        transaction
            .rollback()
            .await
            .map_err(|error| format!("rollback missing Work runtime event slot: {error}"))?;
        return Ok(ProjectionStep::Current);
    };
    let enqueued: i64 = slot
        .try_get("last_enqueued_event_seq")
        .map_err(|error| format!("decode enqueued Work runtime event sequence: {error}"))?;
    let projected: i64 = slot
        .try_get("last_projected_event_seq")
        .map_err(|error| format!("decode projected Work runtime event sequence: {error}"))?;
    if projected >= enqueued {
        transaction
            .rollback()
            .await
            .map_err(|error| format!("rollback raced Work runtime event projection: {error}"))?;
        return Ok(ProjectionStep::Current);
    }

    let retained_from = retained_from(enqueued);
    if projected < retained_from - 1 {
        let owner = WorkOwnerId::parse(owner_id.to_owned()).map_err(|error| error.to_string())?;
        let work = WorkId::parse(work_id.to_owned()).map_err(|error| error.to_string())?;
        super::events_repository::append_event(
            &mut transaction,
            &NewWorkEvent {
                owner_id: owner,
                work_id: work,
                branch_id: None,
                kind: WorkEventKind::RuntimeEventsExpired,
                work_revision: None,
                goal_revision: None,
                criterion_set_revision: None,
                branch_revision: None,
                graph_revision: None,
                source_ref: WorkChangeRef::parse(format!("runtime-gap:{retained_from}"))
                    .map_err(|error| error.to_string())?,
            },
        )
        .await
        .map_err(|error| format!("append Work runtime coverage gap: {error}"))?;
        advance_projected_sequence(
            &mut transaction,
            owner_id,
            work_id,
            projected,
            retained_from - 1,
        )
        .await?;
        transaction
            .commit()
            .await
            .map_err(|error| format!("commit Work runtime coverage gap: {error}"))?;
        return Ok(ProjectionStep::CoverageExpired);
    }

    let next = projected + 1;
    let row = query(
        "SELECT branch_id, event_kind, graph_revision, source_ref
         FROM work_runtime_event_outbox
         WHERE owner_id = ? AND work_id = ? AND runtime_event_seq = ? FOR UPDATE",
    )
    .bind(owner_id)
    .bind(work_id)
    .bind(next)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|error| format!("load next Work runtime event: {error}"))?
    .ok_or_else(|| "retained Work runtime event sequence contains a gap".to_string())?;
    let event = decode_pending_event(owner_id, work_id, &row)?;
    super::events_repository::append_event(
        &mut transaction,
        &NewWorkEvent {
            owner_id: event.owner_id.clone(),
            work_id: event.work_id.clone(),
            branch_id: Some(event.branch_id),
            kind: event.kind,
            work_revision: None,
            goal_revision: None,
            criterion_set_revision: None,
            branch_revision: None,
            graph_revision: Some(event.graph_revision),
            source_ref: event.source_ref,
        },
    )
    .await
    .map_err(|error| format!("append projected Work runtime event: {error}"))?;
    let deleted = query(
        "DELETE FROM work_runtime_event_outbox
         WHERE owner_id = ? AND work_id = ? AND runtime_event_seq = ?",
    )
    .bind(owner_id)
    .bind(work_id)
    .bind(next)
    .execute(&mut *transaction)
    .await
    .map_err(|error| format!("delete projected Work runtime event: {error}"))?;
    if deleted.rows_affected() != 1 {
        return Err("projected Work runtime event disappeared".to_string());
    }
    advance_projected_sequence(&mut transaction, owner_id, work_id, projected, next).await?;
    transaction
        .commit()
        .await
        .map_err(|error| format!("commit Work runtime event projection: {error}"))?;
    Ok(ProjectionStep::Projected)
}

async fn advance_projected_sequence(
    transaction: &mut Transaction<'_, MySql>,
    owner_id: &str,
    work_id: &str,
    expected: i64,
    next: i64,
) -> Result<(), String> {
    let updated = query(
        "UPDATE work_runtime_event_outbox_slots
         SET last_projected_event_seq = ?,
             has_pending = CASE WHEN last_enqueued_event_seq = ? THEN 0 ELSE 1 END,
             updated_at = NOW(6)
         WHERE owner_id = ? AND work_id = ? AND last_projected_event_seq = ?",
    )
    .bind(next)
    .bind(next)
    .bind(owner_id)
    .bind(work_id)
    .bind(expected)
    .execute(&mut **transaction)
    .await
    .map_err(|error| format!("advance projected Work runtime event sequence: {error}"))?;
    if updated.rows_affected() != 1 {
        return Err("Work runtime event projection sequence changed unexpectedly".to_string());
    }
    Ok(())
}

fn decode_pending_event(
    owner_id: &str,
    work_id: &str,
    row: &sqlx::mysql::MySqlRow,
) -> Result<PendingRuntimeEvent, String> {
    let persisted_kind = row
        .try_get::<String, _>("event_kind")
        .map_err(|error| error.to_string())?;
    let kind = WorkEventKind::from_persisted(&persisted_kind)
        .filter(|kind| {
            matches!(
                kind,
                WorkEventKind::RunCompleted
                    | WorkEventKind::RunDelegated
                    | WorkEventKind::RunFailed
                    | WorkEventKind::RunCancelled
            )
        })
        .ok_or_else(|| "invalid Work runtime event kind".to_string())?;
    Ok(PendingRuntimeEvent {
        owner_id: WorkOwnerId::parse(owner_id.to_owned()).map_err(|error| error.to_string())?,
        work_id: WorkId::parse(work_id.to_owned()).map_err(|error| error.to_string())?,
        branch_id: WorkBranchId::parse(
            row.try_get::<String, _>("branch_id")
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?,
        kind,
        graph_revision: GraphRevision::new(
            row.try_get::<i64, _>("graph_revision")
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?,
        source_ref: WorkChangeRef::parse(
            row.try_get::<String, _>("source_ref")
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?,
    })
}

const fn retained_from(head: i64) -> i64 {
    let candidate = head - RUNTIME_EVENT_RING_CAPACITY + 1;
    if candidate < 1 { 1 } else { candidate }
}

pub fn spawn_work_runtime_event_projector(
    pool: SharedPool,
    cancel: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(250));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {}
            }
            match project_pending_runtime_events(&pool, MAX_PROJECTION_BATCH).await {
                Ok(result) if result.total() > 0 => tracing::debug!(
                    target: "astra_services::work_runtime_event_projector",
                    projected = result.projected,
                    coverage_expired = result.coverage_expired,
                    "projected durable Work runtime events"
                ),
                Ok(_) => {}
                Err(error) => tracing::warn!(
                    target: "astra_services::work_runtime_event_projector",
                    error = %error,
                    "Work runtime event projection sweep failed"
                ),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_event_retention_is_fixed_and_overflow_safe() {
        assert_eq!(retained_from(1), 1);
        assert_eq!(retained_from(RUNTIME_EVENT_RING_CAPACITY), 1);
        assert_eq!(retained_from(RUNTIME_EVENT_RING_CAPACITY + 1), 2);
        assert_eq!(
            retained_from(i64::MAX),
            i64::MAX - RUNTIME_EVENT_RING_CAPACITY + 1
        );
    }
}
