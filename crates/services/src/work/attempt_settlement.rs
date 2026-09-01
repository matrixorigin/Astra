use super::graph_repository::decode_persisted_graph;
use super::{
    GraphRevision, InternalSessionId, WorkBranchId, WorkId, WorkItemAttemptId, WorkItemRevision,
    WorkItemRevisionRef, WorkItemText, WorkOwnerId, WorkTaskExecutionNext,
    validate_resource_identity,
};
use crate::runs::{DurableRunStatusKind, durable_run_status_kind};
use astra_core::{SharedPool, is_duplicate_key_error};
use serde::{Deserialize, Serialize};
use sqlx::{MySql, Row, Transaction};
use thiserror::Error;

const SETTLEMENT_SUMMARY_MAX_BYTES: usize = 8 * 1024;
const SETTLEMENT_MAX_CAPABILITIES: usize = 16;
const CAPABILITY_REF_MAX_CHARS: usize = 128;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkAttemptOutcome {
    Delivered,
    Blocked,
    Failed,
}

impl WorkAttemptOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Delivered => "delivered",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
        }
    }

    fn terminal_status(self) -> &'static str {
        match self {
            Self::Delivered | Self::Blocked => "completed",
            Self::Failed => "failed",
        }
    }

    fn from_persisted(value: &str) -> Option<Self> {
        match value {
            "delivered" => Some(Self::Delivered),
            "blocked" => Some(Self::Blocked),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkAttemptBlockerKind {
    CapabilityUnavailable,
    DependencyBlocked,
    PolicyBlocked,
    ExternalUnavailable,
}

impl WorkAttemptBlockerKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::CapabilityUnavailable => "capability_unavailable",
            Self::DependencyBlocked => "dependency_blocked",
            Self::PolicyBlocked => "policy_blocked",
            Self::ExternalUnavailable => "external_unavailable",
        }
    }

    fn from_persisted(value: &str) -> Option<Self> {
        match value {
            "capability_unavailable" => Some(Self::CapabilityUnavailable),
            "dependency_blocked" => Some(Self::DependencyBlocked),
            "policy_blocked" => Some(Self::PolicyBlocked),
            "external_unavailable" => Some(Self::ExternalUnavailable),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkAttemptExecutionMode {
    Primary,
    Delegated,
}

/// Durable execution-carrier state for an unsettled primary Work attempt.
/// Delivery remains a separate fact and is never inferred from run status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrimaryWorkAttemptCarrierState {
    Running,
    Waiting,
    Paused,
    Failed,
    Cancelled,
}

impl PrimaryWorkAttemptCarrierState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Paused => "paused",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

fn primary_carrier_transition_sources(
    target: PrimaryWorkAttemptCarrierState,
) -> &'static [&'static str] {
    match target {
        PrimaryWorkAttemptCarrierState::Running => &["waiting", "paused"],
        PrimaryWorkAttemptCarrierState::Waiting => &["running"],
        PrimaryWorkAttemptCarrierState::Paused => &["running", "waiting"],
        PrimaryWorkAttemptCarrierState::Failed | PrimaryWorkAttemptCarrierState::Cancelled => {
            &["running", "waiting", "paused"]
        }
    }
}

impl WorkAttemptExecutionMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Delegated => "delegated",
        }
    }

    pub(crate) fn from_persisted(value: &str) -> Option<Self> {
        match value {
            "primary" => Some(Self::Primary),
            "delegated" => Some(Self::Delegated),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewWorkItemAttempt {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub session_id: String,
    pub item: WorkItemRevisionRef,
    pub graph_revision: GraphRevision,
    pub attempt_id: WorkItemAttemptId,
    pub executor_run_id: String,
    pub execution_mode: WorkAttemptExecutionMode,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NewWorkAttemptSettlement {
    pub outcome: WorkAttemptOutcome,
    pub summary: String,
    #[serde(default)]
    pub blocker_kind: Option<WorkAttemptBlockerKind>,
    #[serde(default)]
    pub unavailable_capabilities: Vec<String>,
}

impl NewWorkAttemptSettlement {
    pub fn validate(mut self) -> Result<Self, WorkAttemptSettlementError> {
        if self.summary.trim().is_empty() || self.summary.len() > SETTLEMENT_SUMMARY_MAX_BYTES {
            return Err(WorkAttemptSettlementError::Invalid(
                "summary must be non-empty and at most 8192 bytes".into(),
            ));
        }
        self.unavailable_capabilities.sort();
        self.unavailable_capabilities.dedup();
        if self.unavailable_capabilities.len() > SETTLEMENT_MAX_CAPABILITIES {
            return Err(WorkAttemptSettlementError::Invalid(format!(
                "at most {SETTLEMENT_MAX_CAPABILITIES} unavailable capabilities may be reported"
            )));
        }
        for capability in &self.unavailable_capabilities {
            validate_resource_identity("capability_ref", capability, CAPABILITY_REF_MAX_CHARS)
                .map_err(|error| WorkAttemptSettlementError::Invalid(error.to_string()))?;
        }
        match (self.outcome, self.blocker_kind) {
            (WorkAttemptOutcome::Blocked, Some(WorkAttemptBlockerKind::CapabilityUnavailable))
                if self.unavailable_capabilities.is_empty() =>
            {
                return Err(WorkAttemptSettlementError::Invalid(
                    "capability_unavailable requires at least one unavailable capability".into(),
                ));
            }
            (WorkAttemptOutcome::Blocked, Some(kind))
                if kind != WorkAttemptBlockerKind::CapabilityUnavailable
                    && !self.unavailable_capabilities.is_empty() =>
            {
                return Err(WorkAttemptSettlementError::Invalid(
                    "unavailable_capabilities is valid only for capability_unavailable".into(),
                ));
            }
            (WorkAttemptOutcome::Blocked, None) => {
                return Err(WorkAttemptSettlementError::Invalid(
                    "blocked outcome requires blocker_kind".into(),
                ));
            }
            (WorkAttemptOutcome::Delivered | WorkAttemptOutcome::Failed, _)
                if !self.unavailable_capabilities.is_empty() || self.blocker_kind.is_some() =>
            {
                return Err(WorkAttemptSettlementError::Invalid(
                    "only blocked outcomes may carry blocker facts".into(),
                ));
            }
            _ => {}
        }
        Ok(self)
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct RecordedWorkAttemptSettlement {
    pub run_id: String,
    pub work_id: String,
    pub branch_id: String,
    pub item_id: String,
    pub item_revision: i64,
    pub attempt_id: String,
    pub outcome: WorkAttemptOutcome,
    pub summary: String,
    pub blocker_kind: Option<WorkAttemptBlockerKind>,
    pub unavailable_capabilities: Vec<String>,
}

/// Deterministic successor state returned by an atomic primary settlement.
/// `Assigned` is a durable attempt, not a suggestion inferred from model text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrimaryWorkAttemptAdvance {
    Assigned {
        attempt_id: WorkItemAttemptId,
        item_id: super::WorkItemId,
        item_revision: WorkItemRevision,
        objective: WorkItemText,
        expected_result: WorkItemText,
        resumed: bool,
    },
    NeedsRecovery,
    Blocked,
    Complete,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedPrimaryWorkAttemptAdvance {
    pub settlement: RecordedWorkAttemptSettlement,
    pub advance: PrimaryWorkAttemptAdvance,
}

#[derive(Debug, Error)]
pub enum WorkAttemptSettlementError {
    #[error("invalid attempt or settlement: {0}")]
    Invalid(String),
    #[error("Work branch, graph, or task revision is not current")]
    StaleAssignment,
    #[error("executor is not bound to a canonical Work task attempt")]
    UnboundRun,
    #[error("executor run has not reached a terminal state")]
    RunNotTerminal,
    #[error("another primary task attempt is already active on this branch")]
    ActivePrimaryAttempt,
    #[error("this attempt already has different immutable facts or settlement")]
    Conflict,
    #[error("attempt storage unavailable: {0}")]
    Persistence(String),
}

#[derive(Clone, Debug)]
pub struct DatabaseWorkAttemptSettlementService {
    pool: SharedPool,
}

impl DatabaseWorkAttemptSettlementService {
    pub fn new(pool: SharedPool) -> Self {
        Self { pool }
    }

    /// Converge an unsettled primary attempt with its durable execution run.
    /// The exact owner/attempt/run tuple is the authority; this never promotes
    /// execution completion into a delivery outcome.
    pub async fn transition_primary_carriers_for_run(
        &self,
        owner_id: &str,
        executor_run_id: &str,
        target: PrimaryWorkAttemptCarrierState,
    ) -> Result<bool, WorkAttemptSettlementError> {
        let allowed_sources = primary_carrier_transition_sources(target);
        let mut query =
            sqlx::QueryBuilder::<sqlx::MySql>::new("UPDATE work_item_attempts SET status = ");
        query
            .push_bind(target.as_str())
            .push(", updated_at = CURRENT_TIMESTAMP(6) WHERE owner_id = ")
            .push_bind(owner_id)
            .push(" AND executor_run_id = ")
            .push_bind(executor_run_id)
            .push(" AND execution_mode = 'primary' AND outcome IS NULL AND status IN (");
        let mut separated = query.separated(", ");
        for source in allowed_sources {
            separated.push_bind(*source);
        }
        separated.push_unseparated(")");
        let result = query
            .build()
            .execute(self.pool.get())
            .await
            .map_err(persistence)?;
        Ok(result.rows_affected() > 0)
    }

    /// Transfer a paused primary attempt to a new live run in the same
    /// session. This is the durable session-continuation boundary: a new run
    /// cannot settle an old run's attempt until ownership changes atomically.
    pub async fn take_over_paused_primary_attempt(
        &self,
        owner_id: &str,
        attempt_id: &str,
        new_executor_run_id: &str,
    ) -> Result<bool, WorkAttemptSettlementError> {
        let mut tx = self.pool.get().begin().await.map_err(persistence)?;
        let attempt = sqlx::query(
            "SELECT work_id, branch_id, executor_run_id, status
             FROM work_item_attempts
             WHERE owner_id = ? AND attempt_id = ?
               AND execution_mode = 'primary' AND outcome IS NULL FOR UPDATE",
        )
        .bind(owner_id)
        .bind(attempt_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(persistence)?;
        let Some(attempt) = attempt else {
            tx.commit().await.map_err(persistence)?;
            return Ok(false);
        };
        let status: String = attempt.try_get("status").map_err(persistence)?;
        if status != "paused" {
            tx.commit().await.map_err(persistence)?;
            return Ok(false);
        }
        let old_run_id: String = attempt.try_get("executor_run_id").map_err(persistence)?;
        let session_id: String = sqlx::query_scalar(
            "SELECT session_id FROM work_branches
             WHERE owner_id = ? AND work_id = ? AND branch_id = ?
               AND archived_at IS NULL AND deletion_operation_id IS NULL FOR UPDATE",
        )
        .bind(owner_id)
        .bind(
            attempt
                .try_get::<String, _>("work_id")
                .map_err(persistence)?,
        )
        .bind(
            attempt
                .try_get::<String, _>("branch_id")
                .map_err(persistence)?,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(persistence)?
        .ok_or(WorkAttemptSettlementError::StaleAssignment)?;
        let new_run = sqlx::query(
            "SELECT session_id, status, run_generation, last_event_idx
             FROM agent_runs WHERE user_id = ? AND run_id = ? FOR UPDATE",
        )
        .bind(owner_id)
        .bind(new_executor_run_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(persistence)?
        .ok_or(WorkAttemptSettlementError::UnboundRun)?;
        let new_run_status: String = new_run.try_get("status").map_err(persistence)?;
        if new_run
            .try_get::<String, _>("session_id")
            .map_err(persistence)?
            != session_id
            || !matches!(new_run_status.as_str(), "running" | "waiting")
        {
            return Err(WorkAttemptSettlementError::UnboundRun);
        }
        let old_run_status: Option<String> = sqlx::query_scalar(
            "SELECT status FROM agent_runs WHERE user_id = ? AND run_id = ? FOR UPDATE",
        )
        .bind(owner_id)
        .bind(&old_run_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(persistence)?;
        if !old_run_status
            .as_deref()
            .is_some_and(|status| matches!(status, "paused" | "completed" | "failed" | "cancelled"))
        {
            return Err(WorkAttemptSettlementError::ActivePrimaryAttempt);
        }
        let result = sqlx::query(
            "UPDATE work_item_attempts
             SET executor_run_id = ?, status = 'running', run_generation = ?,
                 last_event_idx = ?, updated_at = CURRENT_TIMESTAMP(6)
             WHERE owner_id = ? AND attempt_id = ? AND executor_run_id = ?
               AND execution_mode = 'primary' AND status = 'paused' AND outcome IS NULL",
        )
        .bind(new_executor_run_id)
        .bind(
            new_run
                .try_get::<i64, _>("run_generation")
                .map_err(persistence)?,
        )
        .bind(
            new_run
                .try_get::<i64, _>("last_event_idx")
                .map_err(persistence)?,
        )
        .bind(owner_id)
        .bind(attempt_id)
        .bind(&old_run_id)
        .execute(&mut *tx)
        .await
        .map_err(persistence)?;
        tx.commit().await.map_err(persistence)?;
        Ok(result.rows_affected() == 1)
    }

    /// Reconcile primary attempts whose execution carrier already reached a
    /// terminal durable state without settlement. This prevents one crashed
    /// or cancelled root loop from pinning the branch as in-flight forever.
    pub async fn reconcile_terminal_primary_attempts(
        &self,
        owner_id: &str,
        work_id: &str,
        branch_id: &str,
    ) -> Result<u64, WorkAttemptSettlementError> {
        let mut tx = self.pool.get().begin().await.map_err(persistence)?;
        let branch_exists: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM work_branches
             WHERE owner_id = ? AND work_id = ? AND branch_id = ?
               AND archived_at IS NULL AND deletion_operation_id IS NULL FOR UPDATE",
        )
        .bind(owner_id)
        .bind(work_id)
        .bind(branch_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(persistence)?;
        if branch_exists.is_none() {
            return Err(WorkAttemptSettlementError::StaleAssignment);
        }
        let rows = sqlx::query(
            "SELECT a.attempt_id, r.status AS executor_status
             FROM work_item_attempts a
             JOIN agent_runs r
               ON r.user_id = a.owner_id AND r.run_id = a.executor_run_id
             WHERE a.owner_id = ? AND a.work_id = ? AND a.branch_id = ?
               AND a.execution_mode = 'primary'
               AND a.status IN ('running', 'waiting', 'paused')",
        )
        .bind(owner_id)
        .bind(work_id)
        .bind(branch_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(persistence)?;
        let mut reconciled = 0_u64;
        for row in rows {
            let executor_status: String = row.try_get("executor_status").map_err(persistence)?;
            let status = match durable_run_status_kind(&executor_status) {
                DurableRunStatusKind::Cancelled => "cancelled",
                DurableRunStatusKind::Completed
                | DurableRunStatusKind::Delegated
                | DurableRunStatusKind::Failed => "failed",
                _ => continue,
            };
            let result = sqlx::query(
                "UPDATE work_item_attempts SET status = ?, updated_at = CURRENT_TIMESTAMP(6)
                 WHERE owner_id = ? AND attempt_id = ? AND outcome IS NULL
                   AND status IN ('running', 'waiting', 'paused')",
            )
            .bind(status)
            .bind(owner_id)
            .bind(
                row.try_get::<String, _>("attempt_id")
                    .map_err(persistence)?,
            )
            .execute(&mut *tx)
            .await
            .map_err(persistence)?;
            reconciled = reconciled.saturating_add(result.rows_affected());
        }
        tx.commit().await.map_err(persistence)?;
        Ok(reconciled)
    }

    /// Begin an exact WorkItem attempt. Branch locking makes the single-active
    /// primary invariant deterministic without a history scan or process-local lock.
    pub async fn begin_attempt(
        &self,
        attempt: NewWorkItemAttempt,
    ) -> Result<(), WorkAttemptSettlementError> {
        validate_resource_identity("session_id", &attempt.session_id, 64)
            .map_err(|error| WorkAttemptSettlementError::Invalid(error.to_string()))?;
        validate_resource_identity("executor_run_id", &attempt.executor_run_id, 64)
            .map_err(|error| WorkAttemptSettlementError::Invalid(error.to_string()))?;
        let mut tx = self.pool.get().begin().await.map_err(persistence)?;

        if let Some(existing) = load_attempt_by_id(
            &mut tx,
            attempt.owner_id.as_str(),
            attempt.attempt_id.as_str(),
        )
        .await?
        {
            if existing.matches_new(&attempt) {
                tx.commit().await.map_err(persistence)?;
                return Ok(());
            }
            return Err(WorkAttemptSettlementError::Conflict);
        }

        let branch = sqlx::query(
            "SELECT session_id, current_graph_revision FROM work_branches
             WHERE owner_id = ? AND work_id = ? AND branch_id = ?
               AND archived_at IS NULL AND deletion_operation_id IS NULL FOR UPDATE",
        )
        .bind(attempt.owner_id.as_str())
        .bind(attempt.work_id.as_str())
        .bind(attempt.branch_id.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(persistence)?
        .ok_or(WorkAttemptSettlementError::StaleAssignment)?;
        let session_id: String = branch.try_get("session_id").map_err(persistence)?;
        let graph_revision: i64 = branch
            .try_get("current_graph_revision")
            .map_err(persistence)?;
        if session_id != attempt.session_id || graph_revision != attempt.graph_revision.get() {
            return Err(WorkAttemptSettlementError::StaleAssignment);
        }
        // The unlocked fast-path above avoids locking a branch for ordinary
        // retries. Recheck under the branch lock so concurrent first delivery
        // of the same exact attempt is also idempotent.
        if let Some(existing) = load_attempt_by_id(
            &mut tx,
            attempt.owner_id.as_str(),
            attempt.attempt_id.as_str(),
        )
        .await?
        {
            if existing.matches_new(&attempt) {
                tx.commit().await.map_err(persistence)?;
                return Ok(());
            }
            return Err(WorkAttemptSettlementError::Conflict);
        }
        let executor = sqlx::query(
            "SELECT session_id, status, run_generation, last_event_idx FROM agent_runs
             WHERE user_id = ? AND run_id = ? FOR UPDATE",
        )
        .bind(attempt.owner_id.as_str())
        .bind(&attempt.executor_run_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(persistence)?
        .ok_or(WorkAttemptSettlementError::UnboundRun)?;
        if executor
            .try_get::<String, _>("session_id")
            .map_err(persistence)?
            != attempt.session_id
            || is_terminal_run(
                &executor
                    .try_get::<String, _>("status")
                    .map_err(persistence)?,
            )
        {
            return Err(WorkAttemptSettlementError::UnboundRun);
        }

        let graph = sqlx::query(
            "SELECT item_revision_manifest_json, item_count, edge_manifest_json, edge_count
             FROM work_graph_revisions WHERE owner_id = ? AND work_id = ? AND revision = ?",
        )
        .bind(attempt.owner_id.as_str())
        .bind(attempt.work_id.as_str())
        .bind(attempt.graph_revision.get())
        .fetch_optional(&mut *tx)
        .await
        .map_err(persistence)?
        .ok_or(WorkAttemptSettlementError::StaleAssignment)?;
        let persisted = decode_persisted_graph(
            &graph
                .try_get::<String, _>("item_revision_manifest_json")
                .map_err(persistence)?,
            graph.try_get("item_count").map_err(persistence)?,
            &graph
                .try_get::<String, _>("edge_manifest_json")
                .map_err(persistence)?,
            graph.try_get("edge_count").map_err(persistence)?,
        )
        .map_err(|error| WorkAttemptSettlementError::Persistence(error.to_string()))?;
        if persisted.item_refs.binary_search(&attempt.item).is_err() {
            return Err(WorkAttemptSettlementError::StaleAssignment);
        }
        let task_is_active: Option<i8> = sqlx::query_scalar(
            "SELECT 1 FROM work_item_revisions
             WHERE owner_id = ? AND work_id = ? AND item_id = ? AND revision = ?
               AND item_kind = 'task' AND declaration_state = 'active'",
        )
        .bind(attempt.owner_id.as_str())
        .bind(attempt.work_id.as_str())
        .bind(attempt.item.item_id.as_str())
        .bind(attempt.item.revision.get())
        .fetch_optional(&mut *tx)
        .await
        .map_err(persistence)?;
        if task_is_active.is_none() {
            return Err(WorkAttemptSettlementError::StaleAssignment);
        }
        if attempt.execution_mode == WorkAttemptExecutionMode::Primary {
            let active: Option<String> = sqlx::query_scalar(
                "SELECT attempt_id FROM work_item_attempts
                 WHERE owner_id = ? AND work_id = ? AND branch_id = ?
                   AND execution_mode = 'primary' AND status IN ('running', 'waiting', 'paused')
                 ORDER BY started_at DESC, attempt_id DESC LIMIT 1",
            )
            .bind(attempt.owner_id.as_str())
            .bind(attempt.work_id.as_str())
            .bind(attempt.branch_id.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(persistence)?;
            if active.is_some() {
                return Err(WorkAttemptSettlementError::ActivePrimaryAttempt);
            }
        }
        insert_attempt(
            &mut tx,
            &attempt,
            "running",
            executor
                .try_get::<i64, _>("run_generation")
                .map_err(persistence)?,
            executor
                .try_get::<i64, _>("last_event_idx")
                .map_err(persistence)?,
        )
        .await?;
        tx.commit().await.map_err(persistence)?;
        Ok(())
    }

    pub async fn record_for_attempt(
        &self,
        owner_id: &str,
        attempt_id: &str,
        executor_run_id: &str,
        settlement: NewWorkAttemptSettlement,
    ) -> Result<RecordedWorkAttemptSettlement, WorkAttemptSettlementError> {
        self.record_exact(owner_id, attempt_id, executor_run_id, settlement, false)
            .await
            .map(|(recorded, _)| recorded)
    }

    pub async fn record_for_run(
        &self,
        owner_id: &str,
        run_id: &str,
        settlement: NewWorkAttemptSettlement,
    ) -> Result<RecordedWorkAttemptSettlement, WorkAttemptSettlementError> {
        let attempt_id = self
            .ensure_delegated_attempt_for_run(owner_id, run_id, false)
            .await?;
        self.record_for_attempt(owner_id, &attempt_id, run_id, settlement)
            .await
    }

    pub async fn record_if_unsettled_for_terminal_run(
        &self,
        owner_id: &str,
        run_id: &str,
        settlement: NewWorkAttemptSettlement,
    ) -> Result<bool, WorkAttemptSettlementError> {
        let attempt_id = self
            .ensure_delegated_attempt_for_run(owner_id, run_id, true)
            .await?;
        self.record_exact(owner_id, &attempt_id, run_id, settlement, true)
            .await
            .map(|(_, inserted)| inserted)
    }

    async fn record_exact(
        &self,
        owner_id: &str,
        attempt_id: &str,
        executor_run_id: &str,
        settlement: NewWorkAttemptSettlement,
        preserve_existing: bool,
    ) -> Result<(RecordedWorkAttemptSettlement, bool), WorkAttemptSettlementError> {
        let settlement = settlement.validate()?;
        let mut tx = self.pool.get().begin().await.map_err(persistence)?;
        let recorded = record_exact_in_transaction(
            &mut tx,
            owner_id,
            attempt_id,
            executor_run_id,
            &settlement,
            preserve_existing,
        )
        .await?;
        tx.commit().await.map_err(persistence)?;
        Ok(recorded)
    }

    /// Settle the active primary attempt and, when its delivery unblocks a
    /// successor, begin that exact successor in the same branch-locked
    /// transaction. The successor attempt id is derived by the runtime from
    /// the settlement tool-call identity, so a lost response and retry returns
    /// the same assignment instead of advancing twice.
    pub async fn record_and_advance_primary(
        &self,
        owner_id: &str,
        attempt_id: &str,
        executor_run_id: &str,
        expected_control_epoch: i64,
        settlement: NewWorkAttemptSettlement,
        successor_attempt_id: WorkItemAttemptId,
    ) -> Result<RecordedPrimaryWorkAttemptAdvance, WorkAttemptSettlementError> {
        if expected_control_epoch < -1 {
            return Err(WorkAttemptSettlementError::Invalid(
                "terminal control epoch is below its initial value".to_string(),
            ));
        }
        let settlement = settlement.validate()?;
        let owner = WorkOwnerId::parse(owner_id.to_string())
            .map_err(|error| WorkAttemptSettlementError::Invalid(error.to_string()))?;
        let mut tx = self.pool.get().begin().await.map_err(persistence)?;

        let current = sqlx::query(
            "SELECT work_id, branch_id FROM work_item_attempts
             WHERE owner_id = ? AND attempt_id = ? AND executor_run_id = ?
               AND execution_mode = 'primary'",
        )
        .bind(owner_id)
        .bind(attempt_id)
        .bind(executor_run_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(persistence)?
        .ok_or(WorkAttemptSettlementError::UnboundRun)?;
        let work_id: String = current.try_get("work_id").map_err(persistence)?;
        let branch_id: String = current.try_get("branch_id").map_err(persistence)?;
        let branch = sqlx::query(
            "SELECT session_id FROM work_branches
             WHERE owner_id = ? AND work_id = ? AND branch_id = ?
               AND archived_at IS NULL AND deletion_operation_id IS NULL FOR UPDATE",
        )
        .bind(owner_id)
        .bind(&work_id)
        .bind(&branch_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(persistence)?
        .ok_or(WorkAttemptSettlementError::StaleAssignment)?;
        let session_id = InternalSessionId::parse(
            branch
                .try_get::<String, _>("session_id")
                .map_err(persistence)?,
        )
        .map_err(|error| WorkAttemptSettlementError::Persistence(error.to_string()))?;

        let (recorded, settlement_inserted) = record_exact_in_transaction(
            &mut tx,
            owner_id,
            attempt_id,
            executor_run_id,
            &settlement,
            false,
        )
        .await?;
        let snapshot = super::plan_context_repository::load_task_execution_snapshot_in_transaction(
            &mut tx,
            &owner,
            &session_id,
        )
        .await
        .map_err(|error| WorkAttemptSettlementError::Persistence(error.to_string()))?;
        let advance = match snapshot.next_foreground_task() {
            WorkTaskExecutionNext::Ready(item) => {
                let executor = sqlx::query(
                    "SELECT session_id, status, run_generation, last_event_idx FROM agent_runs
                     WHERE user_id = ? AND run_id = ? FOR UPDATE",
                )
                .bind(owner_id)
                .bind(executor_run_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(persistence)?
                .ok_or(WorkAttemptSettlementError::UnboundRun)?;
                if executor
                    .try_get::<String, _>("session_id")
                    .map_err(persistence)?
                    != session_id.as_str()
                    || is_terminal_run(
                        &executor
                            .try_get::<String, _>("status")
                            .map_err(persistence)?,
                    )
                {
                    return Err(WorkAttemptSettlementError::UnboundRun);
                }
                let next = NewWorkItemAttempt {
                    owner_id: owner.clone(),
                    work_id: snapshot.basis().work_id.clone(),
                    branch_id: snapshot.basis().branch_id.clone(),
                    session_id: session_id.as_str().to_string(),
                    item: WorkItemRevisionRef {
                        item_id: item.item_id.clone(),
                        revision: item.revision,
                    },
                    graph_revision: snapshot.basis().graph_revision,
                    attempt_id: successor_attempt_id.clone(),
                    executor_run_id: executor_run_id.to_string(),
                    execution_mode: WorkAttemptExecutionMode::Primary,
                };
                if let Some(existing) =
                    load_attempt_by_id(&mut tx, owner_id, successor_attempt_id.as_str()).await?
                {
                    if !existing.matches_new(&next) {
                        return Err(WorkAttemptSettlementError::Conflict);
                    }
                } else {
                    insert_attempt(
                        &mut tx,
                        &next,
                        "running",
                        executor
                            .try_get::<i64, _>("run_generation")
                            .map_err(persistence)?,
                        executor
                            .try_get::<i64, _>("last_event_idx")
                            .map_err(persistence)?,
                    )
                    .await?;
                }
                PrimaryWorkAttemptAdvance::Assigned {
                    attempt_id: successor_attempt_id,
                    item_id: item.item_id,
                    item_revision: item.revision,
                    objective: item.objective,
                    expected_result: item.expected_result,
                    resumed: false,
                }
            }
            WorkTaskExecutionNext::InFlight(item) => {
                let Some(run) = item.execution.run.as_ref() else {
                    return Err(WorkAttemptSettlementError::Persistence(
                        "in-flight WorkItem has no attempt identity".into(),
                    ));
                };
                if run.run_id != executor_run_id || run.attempt_id != successor_attempt_id {
                    return Err(WorkAttemptSettlementError::ActivePrimaryAttempt);
                }
                PrimaryWorkAttemptAdvance::Assigned {
                    attempt_id: successor_attempt_id,
                    item_id: item.item_id,
                    item_revision: item.revision,
                    objective: item.objective,
                    expected_result: item.expected_result,
                    resumed: true,
                }
            }
            WorkTaskExecutionNext::NeedsRecovery(_) => PrimaryWorkAttemptAdvance::NeedsRecovery,
            WorkTaskExecutionNext::Blocked => PrimaryWorkAttemptAdvance::Blocked,
            WorkTaskExecutionNext::Complete => {
                // The terminal graph cut is a separate, single-row authority
                // fact. Its branch primary key and attempt unique key make
                // both competing terminal attempts impossible without relying
                // on MatrixOne CHECK enforcement.
                if recorded.outcome == WorkAttemptOutcome::Delivered {
                    let expected_cut = super::WorkAttemptTerminalCut::new(
                        snapshot.basis().graph_revision,
                        expected_control_epoch,
                    )
                    .ok_or_else(|| {
                        WorkAttemptSettlementError::Invalid(
                            "terminal control epoch is below its initial value".into(),
                        )
                    })?;
                    if settlement_inserted {
                        let insert = sqlx::query(
                            "INSERT INTO work_terminal_cuts
                             (owner_id, work_id, branch_id, graph_revision,
                              attempt_id, control_epoch)
                             VALUES (?, ?, ?, ?, ?, ?)",
                        )
                        .bind(owner_id)
                        .bind(&work_id)
                        .bind(&branch_id)
                        .bind(expected_cut.graph_revision.get())
                        .bind(attempt_id)
                        .bind(expected_cut.control_epoch)
                        .execute(&mut *tx)
                        .await;
                        match insert {
                            Ok(result) if result.rows_affected() == 1 => {}
                            Ok(_) => return Err(WorkAttemptSettlementError::Conflict),
                            Err(error) if is_duplicate_key_error(&error) => {
                                return Err(WorkAttemptSettlementError::Conflict);
                            }
                            Err(error) => return Err(persistence(error)),
                        }
                    } else {
                        let existing = sqlx::query(
                            "SELECT work_id, branch_id, graph_revision, control_epoch
                             FROM work_terminal_cuts
                             WHERE owner_id = ? AND attempt_id = ?",
                        )
                        .bind(owner_id)
                        .bind(attempt_id)
                        .fetch_optional(&mut *tx)
                        .await
                        .map_err(persistence)?
                        .ok_or(WorkAttemptSettlementError::Conflict)?;
                        let existing_cut = super::WorkAttemptTerminalCut::new(
                            GraphRevision::new(
                                existing.try_get("graph_revision").map_err(persistence)?,
                            )
                            .map_err(|_| WorkAttemptSettlementError::Conflict)?,
                            existing.try_get("control_epoch").map_err(persistence)?,
                        )
                        .ok_or(WorkAttemptSettlementError::Conflict)?;
                        if existing
                            .try_get::<String, _>("work_id")
                            .map_err(persistence)?
                            != work_id
                            || existing
                                .try_get::<String, _>("branch_id")
                                .map_err(persistence)?
                                != branch_id
                            || existing_cut != expected_cut
                        {
                            return Err(WorkAttemptSettlementError::Conflict);
                        }
                    }
                }
                PrimaryWorkAttemptAdvance::Complete
            }
        };
        tx.commit().await.map_err(persistence)?;
        Ok(RecordedPrimaryWorkAttemptAdvance {
            settlement: recorded,
            advance,
        })
    }

    // Temporary adapter for explicitly delegated Runs. Runtime creation will
    // move to begin_attempt; this adapter never turns a root session into a worker.
    async fn ensure_delegated_attempt_for_run(
        &self,
        owner_id: &str,
        run_id: &str,
        require_terminal: bool,
    ) -> Result<String, WorkAttemptSettlementError> {
        let mut tx = self.pool.get().begin().await.map_err(persistence)?;
        let run = sqlx::query(
            "SELECT status, work_id, work_branch_id, work_item_id, work_item_revision,
                    work_item_attempt_id, work_graph_revision, run_generation, last_event_idx
             FROM agent_runs WHERE user_id = ? AND run_id = ? FOR UPDATE",
        )
        .bind(owner_id)
        .bind(run_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(persistence)?
        .ok_or(WorkAttemptSettlementError::UnboundRun)?;
        let status: String = run.try_get("status").map_err(persistence)?;
        if require_terminal && !is_terminal_run(&status) {
            return Err(WorkAttemptSettlementError::RunNotTerminal);
        }
        let required = |column: &'static str| -> Result<String, WorkAttemptSettlementError> {
            run.try_get::<Option<String>, _>(column)
                .map_err(persistence)?
                .ok_or(WorkAttemptSettlementError::UnboundRun)
        };
        let work_id = required("work_id")?;
        let branch_id = required("work_branch_id")?;
        let item_id = required("work_item_id")?;
        let attempt_id = required("work_item_attempt_id")?;
        let item_revision = run
            .try_get::<Option<i64>, _>("work_item_revision")
            .map_err(persistence)?
            .filter(|revision| *revision > 0)
            .ok_or(WorkAttemptSettlementError::UnboundRun)?;
        let graph_revision = run
            .try_get::<Option<i64>, _>("work_graph_revision")
            .map_err(persistence)?
            .filter(|revision| *revision > 0)
            .ok_or(WorkAttemptSettlementError::UnboundRun)?;
        if let Some(existing) = load_attempt_by_id(&mut tx, owner_id, &attempt_id).await? {
            if existing.executor_run_id != run_id {
                return Err(WorkAttemptSettlementError::Conflict);
            }
            tx.commit().await.map_err(persistence)?;
            return Ok(attempt_id);
        }
        let unavailable = "[]";
        sqlx::query(
            "INSERT INTO work_item_attempts
             (owner_id, work_id, branch_id, work_item_id, work_item_revision, attempt_id,
              executor_run_id, execution_mode, status, graph_revision, run_generation,
              last_event_idx, unavailable_capabilities_json)
             VALUES (?, ?, ?, ?, ?, ?, ?, 'delegated', ?, ?, ?, ?, ?)",
        )
        .bind(owner_id)
        .bind(work_id)
        .bind(branch_id)
        .bind(item_id)
        .bind(item_revision)
        .bind(&attempt_id)
        .bind(run_id)
        .bind(normalize_run_status(&status))
        .bind(graph_revision)
        .bind(
            run.try_get::<i64, _>("run_generation")
                .map_err(persistence)?,
        )
        .bind(
            run.try_get::<i64, _>("last_event_idx")
                .map_err(persistence)?,
        )
        .bind(unavailable)
        .execute(&mut *tx)
        .await
        .map_err(persistence)?;
        tx.commit().await.map_err(persistence)?;
        Ok(attempt_id)
    }
}

async fn record_exact_in_transaction(
    tx: &mut Transaction<'_, MySql>,
    owner_id: &str,
    attempt_id: &str,
    executor_run_id: &str,
    settlement: &NewWorkAttemptSettlement,
    preserve_existing: bool,
) -> Result<(RecordedWorkAttemptSettlement, bool), WorkAttemptSettlementError> {
    let row = sqlx::query(
        "SELECT work_id, branch_id, work_item_id, work_item_revision, attempt_id,
                executor_run_id, outcome, summary_text, blocker_kind,
                unavailable_capabilities_json
         FROM work_item_attempts WHERE owner_id = ? AND attempt_id = ? FOR UPDATE",
    )
    .bind(owner_id)
    .bind(attempt_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(persistence)?
    .ok_or(WorkAttemptSettlementError::UnboundRun)?;
    if row
        .try_get::<String, _>("executor_run_id")
        .map_err(persistence)?
        != executor_run_id
    {
        return Err(WorkAttemptSettlementError::UnboundRun);
    }
    if row
        .try_get::<Option<String>, _>("outcome")
        .map_err(persistence)?
        .is_some()
    {
        let recorded = decode_recorded(&row)?;
        if !preserve_existing && !same_settlement(&recorded, settlement) {
            return Err(WorkAttemptSettlementError::Conflict);
        }
        return Ok((recorded, false));
    }
    let capabilities = serde_json::to_string(&settlement.unavailable_capabilities)
        .map_err(|error| WorkAttemptSettlementError::Persistence(error.to_string()))?;
    sqlx::query(
        "UPDATE work_item_attempts SET status = ?, outcome = ?, summary_text = ?,
                blocker_kind = ?, unavailable_capabilities_json = ?,
                updated_at = CURRENT_TIMESTAMP(6), settled_at = CURRENT_TIMESTAMP(6)
         WHERE owner_id = ? AND attempt_id = ? AND outcome IS NULL",
    )
    .bind(settlement.outcome.terminal_status())
    .bind(settlement.outcome.as_str())
    .bind(&settlement.summary)
    .bind(settlement.blocker_kind.map(WorkAttemptBlockerKind::as_str))
    .bind(capabilities)
    .bind(owner_id)
    .bind(attempt_id)
    .execute(&mut **tx)
    .await
    .map_err(persistence)?;
    let recorded = load_recorded(tx, owner_id, attempt_id).await?;
    if !same_settlement(&recorded, settlement) {
        return Err(WorkAttemptSettlementError::Conflict);
    }
    Ok((recorded, true))
}

#[derive(Debug)]
struct ExistingAttempt {
    work_id: String,
    branch_id: String,
    item_id: String,
    item_revision: i64,
    attempt_id: String,
    executor_run_id: String,
    execution_mode: String,
    graph_revision: i64,
}

impl ExistingAttempt {
    fn matches_new(&self, new: &NewWorkItemAttempt) -> bool {
        self.work_id == new.work_id.as_str()
            && self.branch_id == new.branch_id.as_str()
            && self.item_id == new.item.item_id.as_str()
            && self.item_revision == new.item.revision.get()
            && self.attempt_id == new.attempt_id.as_str()
            && self.executor_run_id == new.executor_run_id
            && self.execution_mode == new.execution_mode.as_str()
            && self.graph_revision == new.graph_revision.get()
    }
}

async fn load_attempt_by_id(
    tx: &mut Transaction<'_, MySql>,
    owner_id: &str,
    attempt_id: &str,
) -> Result<Option<ExistingAttempt>, WorkAttemptSettlementError> {
    let row = sqlx::query(
        "SELECT work_id, branch_id, work_item_id, work_item_revision, attempt_id,
                executor_run_id, execution_mode, graph_revision
         FROM work_item_attempts WHERE owner_id = ? AND attempt_id = ?",
    )
    .bind(owner_id)
    .bind(attempt_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(persistence)?;
    row.map(|row| {
        Ok(ExistingAttempt {
            work_id: row.try_get("work_id").map_err(persistence)?,
            branch_id: row.try_get("branch_id").map_err(persistence)?,
            item_id: row.try_get("work_item_id").map_err(persistence)?,
            item_revision: row.try_get("work_item_revision").map_err(persistence)?,
            attempt_id: row.try_get("attempt_id").map_err(persistence)?,
            executor_run_id: row.try_get("executor_run_id").map_err(persistence)?,
            execution_mode: row.try_get("execution_mode").map_err(persistence)?,
            graph_revision: row.try_get("graph_revision").map_err(persistence)?,
        })
    })
    .transpose()
}

async fn insert_attempt(
    tx: &mut Transaction<'_, MySql>,
    attempt: &NewWorkItemAttempt,
    status: &str,
    run_generation: i64,
    last_event_idx: i64,
) -> Result<(), WorkAttemptSettlementError> {
    sqlx::query(
        "INSERT INTO work_item_attempts
         (owner_id, work_id, branch_id, work_item_id, work_item_revision, attempt_id,
          executor_run_id, execution_mode, status, graph_revision, run_generation,
          last_event_idx, unavailable_capabilities_json)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, '[]')",
    )
    .bind(attempt.owner_id.as_str())
    .bind(attempt.work_id.as_str())
    .bind(attempt.branch_id.as_str())
    .bind(attempt.item.item_id.as_str())
    .bind(attempt.item.revision.get())
    .bind(attempt.attempt_id.as_str())
    .bind(&attempt.executor_run_id)
    .bind(attempt.execution_mode.as_str())
    .bind(status)
    .bind(attempt.graph_revision.get())
    .bind(run_generation)
    .bind(last_event_idx)
    .execute(&mut **tx)
    .await
    .map_err(persistence)?;
    Ok(())
}

async fn load_recorded(
    tx: &mut Transaction<'_, MySql>,
    owner_id: &str,
    attempt_id: &str,
) -> Result<RecordedWorkAttemptSettlement, WorkAttemptSettlementError> {
    let row = sqlx::query(
        "SELECT work_id, branch_id, work_item_id, work_item_revision, attempt_id,
                executor_run_id, outcome, summary_text, blocker_kind,
                unavailable_capabilities_json
         FROM work_item_attempts WHERE owner_id = ? AND attempt_id = ?",
    )
    .bind(owner_id)
    .bind(attempt_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(persistence)?
    .ok_or_else(|| WorkAttemptSettlementError::Persistence("attempt disappeared".into()))?;
    decode_recorded(&row)
}

fn decode_recorded(
    row: &sqlx::mysql::MySqlRow,
) -> Result<RecordedWorkAttemptSettlement, WorkAttemptSettlementError> {
    let outcome = row
        .try_get::<Option<String>, _>("outcome")
        .map_err(persistence)?
        .and_then(|value| WorkAttemptOutcome::from_persisted(&value))
        .ok_or_else(|| {
            WorkAttemptSettlementError::Persistence("unknown or absent outcome".into())
        })?;
    let blocker_kind = row
        .try_get::<Option<String>, _>("blocker_kind")
        .map_err(persistence)?
        .map(|value| {
            WorkAttemptBlockerKind::from_persisted(&value).ok_or_else(|| {
                WorkAttemptSettlementError::Persistence("unknown blocker kind".into())
            })
        })
        .transpose()?;
    let capabilities: String = row
        .try_get("unavailable_capabilities_json")
        .map_err(persistence)?;
    Ok(RecordedWorkAttemptSettlement {
        run_id: row.try_get("executor_run_id").map_err(persistence)?,
        work_id: row.try_get("work_id").map_err(persistence)?,
        branch_id: row.try_get("branch_id").map_err(persistence)?,
        item_id: row.try_get("work_item_id").map_err(persistence)?,
        item_revision: row.try_get("work_item_revision").map_err(persistence)?,
        attempt_id: row.try_get("attempt_id").map_err(persistence)?,
        outcome,
        summary: row
            .try_get::<Option<String>, _>("summary_text")
            .map_err(persistence)?
            .ok_or_else(|| {
                WorkAttemptSettlementError::Persistence("settled attempt has no summary".into())
            })?,
        blocker_kind,
        unavailable_capabilities: serde_json::from_str(&capabilities)
            .map_err(|error| WorkAttemptSettlementError::Persistence(error.to_string()))?,
    })
}

fn same_settlement(
    recorded: &RecordedWorkAttemptSettlement,
    settlement: &NewWorkAttemptSettlement,
) -> bool {
    recorded.outcome == settlement.outcome
        && recorded.summary == settlement.summary
        && recorded.blocker_kind == settlement.blocker_kind
        && recorded.unavailable_capabilities == settlement.unavailable_capabilities
}

fn is_terminal_run(status: &str) -> bool {
    matches!(
        durable_run_status_kind(status),
        DurableRunStatusKind::Completed
            | DurableRunStatusKind::Delegated
            | DurableRunStatusKind::Failed
            | DurableRunStatusKind::Cancelled
    )
}

fn normalize_run_status(status: &str) -> &'static str {
    match durable_run_status_kind(status) {
        DurableRunStatusKind::Running | DurableRunStatusKind::Other => "running",
        DurableRunStatusKind::Waiting => "waiting",
        DurableRunStatusKind::Paused => "paused",
        DurableRunStatusKind::Completed => "completed",
        DurableRunStatusKind::Delegated => "delegated",
        DurableRunStatusKind::Failed => "failed",
        DurableRunStatusKind::Cancelled => "cancelled",
    }
}

fn persistence(error: sqlx::Error) -> WorkAttemptSettlementError {
    WorkAttemptSettlementError::Persistence(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_carrier_transition_matrix_is_reversible_only_for_live_control() {
        assert_eq!(
            primary_carrier_transition_sources(PrimaryWorkAttemptCarrierState::Paused),
            ["running", "waiting"]
        );
        assert_eq!(
            primary_carrier_transition_sources(PrimaryWorkAttemptCarrierState::Running),
            ["waiting", "paused"]
        );
        for terminal in [
            PrimaryWorkAttemptCarrierState::Failed,
            PrimaryWorkAttemptCarrierState::Cancelled,
        ] {
            assert_eq!(
                primary_carrier_transition_sources(terminal),
                ["running", "waiting", "paused"]
            );
        }
    }

    #[test]
    fn settlement_shape_rejects_ambiguous_blocker_facts() {
        assert!(
            NewWorkAttemptSettlement {
                outcome: WorkAttemptOutcome::Blocked,
                summary: "Network fetch is unavailable".into(),
                blocker_kind: Some(WorkAttemptBlockerKind::CapabilityUnavailable),
                unavailable_capabilities: vec!["web_fetch".into()],
            }
            .validate()
            .is_ok()
        );
        assert!(
            NewWorkAttemptSettlement {
                outcome: WorkAttemptOutcome::Blocked,
                summary: "Cannot continue".into(),
                blocker_kind: Some(WorkAttemptBlockerKind::CapabilityUnavailable),
                unavailable_capabilities: Vec::new(),
            }
            .validate()
            .is_err()
        );
        assert!(
            NewWorkAttemptSettlement {
                outcome: WorkAttemptOutcome::Delivered,
                summary: "Done".into(),
                blocker_kind: Some(WorkAttemptBlockerKind::ExternalUnavailable),
                unavailable_capabilities: Vec::new(),
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn execution_status_is_derived_from_typed_outcome() {
        assert_eq!(WorkAttemptOutcome::Delivered.terminal_status(), "completed");
        assert_eq!(WorkAttemptOutcome::Blocked.terminal_status(), "completed");
        assert_eq!(WorkAttemptOutcome::Failed.terminal_status(), "failed");
    }
}
