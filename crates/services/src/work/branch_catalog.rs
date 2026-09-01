use super::{
    CriterionSetRevision, ForkCursorRef, GoalRevision, GraphRevision, WORK_ACTIVE_BRANCH_MAX,
    WorkBranchId, WorkBranchRevision, WorkId, WorkOwnerId, WorkRevision,
};
use astra_core::SharedPool;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::Row;
use thiserror::Error;

pub const WORK_BRANCH_CATALOG_SCHEMA_VERSION: u16 = 1;
pub const WORK_ARCHIVED_BRANCH_PAGE_MAX_ITEMS: u16 = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkBranchDimension {
    Conversation,
    Goal,
    Criteria,
    TaskGraph,
    Checkpoint,
    Workspace,
    Artifacts,
    TransientAuthority,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkBranchDimensionDisposition {
    Shared,
    Copied,
    Rebased,
    Excluded,
    Gap,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkBranchDimensionSummary {
    pub dimension: WorkBranchDimension,
    pub disposition: WorkBranchDimensionDisposition,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkBranchCatalogEntry {
    pub branch_id: WorkBranchId,
    pub branch_revision: WorkBranchRevision,
    pub is_delivery: bool,
    pub origin_branch_id: Option<WorkBranchId>,
    pub fork_cursor: Option<ForkCursorRef>,
    pub goal_revision_ref: GoalRevision,
    pub criteria_set_revision_ref: CriterionSetRevision,
    pub basis_graph_revision: GraphRevision,
    pub current_graph_revision: GraphRevision,
    pub materialization: Option<Vec<WorkBranchDimensionSummary>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkBranchCatalog {
    pub schema_version: u16,
    pub work_id: WorkId,
    pub work_revision: WorkRevision,
    pub delivery_branch_id: WorkBranchId,
    pub branches: Vec<WorkBranchCatalogEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkArchivedBranchCursor {
    pub archived_at: DateTime<Utc>,
    pub branch_id: WorkBranchId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkArchivedBranchEntry {
    pub branch_id: WorkBranchId,
    pub branch_revision: WorkBranchRevision,
    pub origin_branch_id: Option<WorkBranchId>,
    pub archived_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkArchivedBranchPage {
    pub schema_version: u16,
    pub work_id: WorkId,
    pub work_revision: WorkRevision,
    pub branches: Vec<WorkArchivedBranchEntry>,
    pub next_cursor: Option<WorkArchivedBranchCursor>,
}

#[derive(Debug, Error)]
pub enum WorkBranchCatalogError {
    #[error("Work was not found")]
    NotFound,
    #[error("Work branch catalog requires repair: {0}")]
    NeedsRepair(String),
    #[error("Work branch catalog database read failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("archived branch cursor or page limit is invalid")]
    InvalidQuery,
}

#[derive(Clone)]
pub struct DatabaseWorkBranchCatalogService {
    pool: SharedPool,
}

impl DatabaseWorkBranchCatalogService {
    pub fn new(pool: SharedPool) -> Self {
        Self { pool }
    }

    /// Load the complete active branch set. Admission caps this set at 32, so
    /// the query and response stay bounded without cursor state or history
    /// scans. Archived branches belong to the later retention surface.
    pub async fn load_active(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
    ) -> Result<WorkBranchCatalog, WorkBranchCatalogError> {
        let rows = sqlx::query(
            "SELECT w.work_revision, w.delivery_branch_id,
                    b.branch_id, b.branch_revision, b.origin_branch_id,
                    b.fork_cursor, b.goal_revision_ref,
                    b.criteria_set_revision_ref, b.basis_graph_revision,
                    b.current_graph_revision, b.created_at
             FROM works w
             LEFT JOIN work_branches b
               ON b.owner_id = w.owner_id AND b.work_id = w.work_id
              AND b.archived_at IS NULL
             WHERE w.owner_id = ? AND w.work_id = ?
             ORDER BY b.created_at ASC, b.branch_id ASC
             LIMIT ?",
        )
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(WORK_ACTIVE_BRANCH_MAX + 1)
        .fetch_all(self.pool.get())
        .await?;
        let first = rows.first().ok_or(WorkBranchCatalogError::NotFound)?;
        let active_branch_max = usize::try_from(WORK_ACTIVE_BRANCH_MAX)
            .expect("active Work branch bound must fit usize");
        if rows.len() > active_branch_max {
            return Err(WorkBranchCatalogError::NeedsRepair(
                "active branch count exceeds its admission bound".into(),
            ));
        }
        let work_revision = WorkRevision::new(integer(first, "work_revision")?).map_err(repair)?;
        let delivery_branch_id =
            WorkBranchId::parse(text(first, "delivery_branch_id")?).map_err(repair)?;
        let mut branches = Vec::with_capacity(rows.len());
        for row in &rows {
            if WorkRevision::new(integer(row, "work_revision")?).map_err(repair)? != work_revision
                || WorkBranchId::parse(text(row, "delivery_branch_id")?).map_err(repair)?
                    != delivery_branch_id
            {
                return Err(WorkBranchCatalogError::NeedsRepair(
                    "one statement returned contradictory Work identity".into(),
                ));
            }
            let branch_id = WorkBranchId::parse(text(row, "branch_id")?).map_err(repair)?;
            let origin_branch_id = optional_text(row, "origin_branch_id")?
                .map(WorkBranchId::parse)
                .transpose()
                .map_err(repair)?;
            let fork_cursor = optional_text(row, "fork_cursor")?
                .map(ForkCursorRef::parse)
                .transpose()
                .map_err(repair)?;
            if origin_branch_id.is_some() != fork_cursor.is_some() {
                return Err(WorkBranchCatalogError::NeedsRepair(
                    "branch has incomplete fork lineage".into(),
                ));
            }
            let is_fork = origin_branch_id.is_some();
            let basis_graph_revision =
                GraphRevision::new(integer(row, "basis_graph_revision")?).map_err(repair)?;
            let current_graph_revision =
                GraphRevision::new(integer(row, "current_graph_revision")?).map_err(repair)?;
            if current_graph_revision < basis_graph_revision {
                return Err(WorkBranchCatalogError::NeedsRepair(
                    "branch graph revision precedes its fork basis".into(),
                ));
            }
            branches.push(WorkBranchCatalogEntry {
                is_delivery: branch_id == delivery_branch_id,
                branch_id,
                branch_revision: WorkBranchRevision::new(integer(row, "branch_revision")?)
                    .map_err(repair)?,
                origin_branch_id,
                fork_cursor,
                goal_revision_ref: GoalRevision::new(integer(row, "goal_revision_ref")?)
                    .map_err(repair)?,
                criteria_set_revision_ref: CriterionSetRevision::new(integer(
                    row,
                    "criteria_set_revision_ref",
                )?)
                .map_err(repair)?,
                basis_graph_revision,
                current_graph_revision,
                materialization: is_fork.then(fork_materialization_summary),
                created_at: row.try_get("created_at").map_err(repair)?,
            });
        }
        if branches.is_empty() || branches.iter().filter(|branch| branch.is_delivery).count() != 1 {
            return Err(WorkBranchCatalogError::NeedsRepair(
                "active delivery branch is missing or duplicated".into(),
            ));
        }
        Ok(WorkBranchCatalog {
            schema_version: WORK_BRANCH_CATALOG_SCHEMA_VERSION,
            work_id: work_id.clone(),
            work_revision,
            delivery_branch_id,
            branches,
        })
    }

    pub async fn load_archived(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        cursor: Option<&WorkArchivedBranchCursor>,
        limit: u16,
    ) -> Result<WorkArchivedBranchPage, WorkBranchCatalogError> {
        if limit == 0 || limit > WORK_ARCHIVED_BRANCH_PAGE_MAX_ITEMS {
            return Err(WorkBranchCatalogError::InvalidQuery);
        }
        let fetch_limit = i64::from(limit) + 1;
        let cursor_time = cursor.map(|cursor| cursor.archived_at.naive_utc());
        let cursor_branch = cursor.map(|cursor| cursor.branch_id.as_str());
        let mut rows = sqlx::query(
            "SELECT w.work_revision, b.branch_id, b.branch_revision, b.origin_branch_id,
                    b.archived_at, b.created_at
             FROM works w
             JOIN work_branches b
               ON b.owner_id = w.owner_id AND b.work_id = w.work_id
              AND b.archived_at IS NOT NULL
             WHERE w.owner_id = ? AND w.work_id = ? AND w.archived_at IS NULL
               AND (? IS NULL OR b.archived_at < ?
                    OR (b.archived_at = ? AND b.branch_id < ?))
             ORDER BY b.archived_at DESC, b.branch_id DESC
             LIMIT ?",
        )
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(cursor_time)
        .bind(cursor_time)
        .bind(cursor_time)
        .bind(cursor_branch)
        .bind(fetch_limit)
        .fetch_all(self.pool.get())
        .await?;
        if rows.is_empty() {
            let exists: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM works
                 WHERE owner_id = ? AND work_id = ? AND archived_at IS NULL",
            )
            .bind(owner_id.as_str())
            .bind(work_id.as_str())
            .fetch_one(self.pool.get())
            .await?;
            if exists == 0 {
                return Err(WorkBranchCatalogError::NotFound);
            }
        }
        let has_more = rows.len() > usize::from(limit);
        if has_more {
            rows.pop();
        }
        let work_revision = match rows.first() {
            Some(row) => WorkRevision::new(integer(row, "work_revision")?).map_err(repair)?,
            None => {
                let revision: i64 = sqlx::query_scalar(
                    "SELECT work_revision FROM works
                     WHERE owner_id = ? AND work_id = ? AND archived_at IS NULL",
                )
                .bind(owner_id.as_str())
                .bind(work_id.as_str())
                .fetch_one(self.pool.get())
                .await?;
                WorkRevision::new(revision).map_err(repair)?
            }
        };
        let branches = rows
            .iter()
            .map(|row| {
                if WorkRevision::new(integer(row, "work_revision")?).map_err(repair)?
                    != work_revision
                {
                    return Err(WorkBranchCatalogError::NeedsRepair(
                        "one statement returned contradictory Work revision".into(),
                    ));
                }
                Ok(WorkArchivedBranchEntry {
                    branch_id: WorkBranchId::parse(text(row, "branch_id")?).map_err(repair)?,
                    branch_revision: WorkBranchRevision::new(integer(row, "branch_revision")?)
                        .map_err(repair)?,
                    origin_branch_id: optional_text(row, "origin_branch_id")?
                        .map(WorkBranchId::parse)
                        .transpose()
                        .map_err(repair)?,
                    archived_at: DateTime::from_naive_utc_and_offset(
                        row.try_get("archived_at").map_err(repair)?,
                        Utc,
                    ),
                    created_at: row.try_get("created_at").map_err(repair)?,
                })
            })
            .collect::<Result<Vec<_>, WorkBranchCatalogError>>()?;
        let next_cursor = has_more.then(|| {
            let last = branches.last().expect("a page with more rows is non-empty");
            WorkArchivedBranchCursor {
                archived_at: last.archived_at,
                branch_id: last.branch_id.clone(),
            }
        });
        Ok(WorkArchivedBranchPage {
            schema_version: WORK_BRANCH_CATALOG_SCHEMA_VERSION,
            work_id: work_id.clone(),
            work_revision,
            branches,
            next_cursor,
        })
    }
}

fn fork_materialization_summary() -> Vec<WorkBranchDimensionSummary> {
    use WorkBranchDimension as Dimension;
    use WorkBranchDimensionDisposition as Disposition;

    [
        (Dimension::Conversation, Disposition::Shared),
        (Dimension::Goal, Disposition::Shared),
        (Dimension::Criteria, Disposition::Shared),
        (Dimension::TaskGraph, Disposition::Shared),
        (Dimension::Checkpoint, Disposition::Gap),
        (Dimension::Workspace, Disposition::Gap),
        (Dimension::Artifacts, Disposition::Gap),
        (Dimension::TransientAuthority, Disposition::Excluded),
    ]
    .into_iter()
    .map(|(dimension, disposition)| WorkBranchDimensionSummary {
        dimension,
        disposition,
    })
    .collect()
}

fn text(
    row: &sqlx::mysql::MySqlRow,
    field: &'static str,
) -> Result<String, WorkBranchCatalogError> {
    row.try_get(field).map_err(repair)
}

fn optional_text(
    row: &sqlx::mysql::MySqlRow,
    field: &'static str,
) -> Result<Option<String>, WorkBranchCatalogError> {
    row.try_get(field).map_err(repair)
}

fn integer(
    row: &sqlx::mysql::MySqlRow,
    field: &'static str,
) -> Result<i64, WorkBranchCatalogError> {
    row.try_get(field).map_err(repair)
}

fn repair(error: impl std::fmt::Display) -> WorkBranchCatalogError {
    WorkBranchCatalogError::NeedsRepair(error.to_string())
}
