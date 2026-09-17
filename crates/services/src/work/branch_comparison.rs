use super::{
    CriterionSetRevision, GoalRevision, GraphRevision, WorkBranchId, WorkBranchRevision,
    WorkContentHash, WorkId, WorkOwnerId, WorkRevision, WorkSubjectRef,
};
use astra_core::SharedPool;
use serde::Serialize;
use sqlx::Row;
use thiserror::Error;

pub const WORK_BRANCH_COMPARISON_SCHEMA_VERSION: u16 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkBranchComparisonBlocker {
    GoalRevisionDiffers,
    CriteriaRevisionDiffers,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkBranchComparisonCoverageGap {
    ChangeDetails,
    FreshChecks,
    Risks,
    TimeCost,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkBranchComparisonRelation {
    Same,
    Different,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkBranchComparisonCriteria {
    pub revision: CriterionSetRevision,
    pub manifest_hash: WorkContentHash,
    pub member_count: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkBranchComparisonGraph {
    pub basis_revision: GraphRevision,
    pub current_revision: GraphRevision,
    pub manifest_hash: WorkContentHash,
    pub item_count: u16,
    pub edge_count: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkBranchComparisonSubject {
    pub subject_ref: WorkSubjectRef,
    pub subject_revision: WorkContentHash,
    pub graph_revision: GraphRevision,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkBranchComparisonSide {
    pub branch_id: WorkBranchId,
    pub branch_revision: WorkBranchRevision,
    pub is_delivery: bool,
    pub goal_revision_ref: GoalRevision,
    pub criteria: WorkBranchComparisonCriteria,
    pub graph: WorkBranchComparisonGraph,
    pub subject: Option<WorkBranchComparisonSubject>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkBranchComparisonEvidence {
    pub manifest_hash: WorkContentHash,
    pub required_count: u16,
    pub fresh_check_count: u16,
    pub accepted_gap_count: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkBranchComparisonReport {
    pub schema_version: u16,
    pub work_id: WorkId,
    pub work_revision: WorkRevision,
    pub directly_comparable: bool,
    pub blockers: Vec<WorkBranchComparisonBlocker>,
    pub graph_relation: WorkBranchComparisonRelation,
    pub subject_relation: WorkBranchComparisonRelation,
    pub evidence_relation: WorkBranchComparisonRelation,
    pub left: WorkBranchComparisonSide,
    pub right: WorkBranchComparisonSide,
    pub left_evidence: WorkBranchComparisonEvidence,
    pub right_evidence: WorkBranchComparisonEvidence,
    pub coverage_gaps: Vec<WorkBranchComparisonCoverageGap>,
}

#[derive(Debug, Error)]
pub enum WorkBranchComparisonError {
    #[error("two distinct Work branches are required")]
    SameBranch,
    #[error("Work branch comparison target was not found")]
    NotFound,
    #[error("Work branch comparison requires repair: {0}")]
    NeedsRepair(String),
    #[error("Work branch comparison database read failed: {0}")]
    Database(#[from] sqlx::Error),
}

#[derive(Clone)]
pub struct DatabaseWorkBranchComparisonService {
    pool: SharedPool,
}

impl DatabaseWorkBranchComparisonService {
    pub fn new(pool: SharedPool) -> Self {
        Self { pool }
    }

    pub async fn compare(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        left_branch_id: &WorkBranchId,
        right_branch_id: &WorkBranchId,
    ) -> Result<WorkBranchComparisonReport, WorkBranchComparisonError> {
        if left_branch_id == right_branch_id {
            return Err(WorkBranchComparisonError::SameBranch);
        }
        let mut transaction = self.pool.get().begin().await?;
        let rows = sqlx::query(
            "SELECT w.work_revision, w.delivery_branch_id, es.last_event_seq,
                    b.branch_id, b.branch_revision, b.goal_revision_ref,
                    b.criteria_set_revision_ref, b.basis_graph_revision,
                    b.current_graph_revision,
                    cs.member_manifest_hash AS criteria_manifest_hash,
                    cs.member_count AS criteria_member_count,
                    cs.member_manifest_json AS criteria_member_manifest_json,
                    gr.manifest_hash AS graph_manifest_hash,
                    gr.item_count AS graph_item_count,
                    gr.edge_count AS graph_edge_count,
                    s.subject_ref, s.subject_revision,
                    s.graph_revision AS subject_graph_revision
             FROM works w
             JOIN work_branches b
               ON b.owner_id = w.owner_id AND b.work_id = w.work_id
              AND b.archived_at IS NULL
             LEFT JOIN work_event_sequences es
               ON es.owner_id = w.owner_id AND es.work_id = w.work_id
             LEFT JOIN work_criterion_sets cs
               ON cs.owner_id = b.owner_id AND cs.work_id = b.work_id
              AND cs.revision = b.criteria_set_revision_ref
             LEFT JOIN work_graph_revisions gr
               ON gr.owner_id = b.owner_id AND gr.work_id = b.work_id
              AND gr.revision = b.current_graph_revision
             LEFT JOIN work_branch_subjects s
               ON s.owner_id = b.owner_id AND s.work_id = b.work_id
              AND s.branch_id = b.branch_id
             WHERE w.owner_id = ? AND w.work_id = ?
               AND w.archived_at IS NULL AND b.branch_id IN (?, ?)",
        )
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(left_branch_id.as_str())
        .bind(right_branch_id.as_str())
        .fetch_all(&mut *transaction)
        .await?;
        if rows.len() != 2 {
            return Err(WorkBranchComparisonError::NotFound);
        }
        let work_revision =
            WorkRevision::new(integer(&rows[0], "work_revision")?).map_err(repair)?;
        let delivery_branch_id =
            WorkBranchId::parse(text(&rows[0], "delivery_branch_id")?).map_err(repair)?;
        let first_branch_id = WorkBranchId::parse(text(&rows[0], "branch_id")?).map_err(repair)?;
        let second_branch_id = WorkBranchId::parse(text(&rows[1], "branch_id")?).map_err(repair)?;
        if first_branch_id == second_branch_id {
            return Err(WorkBranchComparisonError::NeedsRepair(
                "comparison query returned a duplicate branch".into(),
            ));
        }
        let decode = |branch_id: &WorkBranchId| {
            let row = if &first_branch_id == branch_id {
                &rows[0]
            } else if &second_branch_id == branch_id {
                &rows[1]
            } else {
                return Err(WorkBranchComparisonError::NotFound);
            };
            if WorkRevision::new(integer(row, "work_revision")?).map_err(repair)? != work_revision
                || WorkBranchId::parse(text(row, "delivery_branch_id")?).map_err(repair)?
                    != delivery_branch_id
            {
                return Err(WorkBranchComparisonError::NeedsRepair(
                    "one statement returned contradictory Work identity".into(),
                ));
            }
            decode_side(row, branch_id == &delivery_branch_id)
        };
        let left = decode(left_branch_id)?;
        let right = decode(right_branch_id)?;
        let left_row = if first_branch_id == *left_branch_id {
            &rows[0]
        } else {
            &rows[1]
        };
        let right_row = if first_branch_id == *right_branch_id {
            &rows[0]
        } else {
            &rows[1]
        };
        let left_evidence = load_evidence(
            &mut transaction,
            owner_id,
            work_id,
            work_revision,
            left_row,
            &left,
        )
        .await?;
        let right_evidence = load_evidence(
            &mut transaction,
            owner_id,
            work_id,
            work_revision,
            right_row,
            &right,
        )
        .await?;
        let mut blockers = Vec::new();
        if left.goal_revision_ref != right.goal_revision_ref {
            blockers.push(WorkBranchComparisonBlocker::GoalRevisionDiffers);
        }
        if left.criteria.revision != right.criteria.revision
            || left.criteria.manifest_hash != right.criteria.manifest_hash
        {
            blockers.push(WorkBranchComparisonBlocker::CriteriaRevisionDiffers);
        }
        if left.criteria.manifest_hash == right.criteria.manifest_hash
            && left.criteria.member_count != right.criteria.member_count
        {
            return Err(WorkBranchComparisonError::NeedsRepair(
                "one criterion manifest has contradictory member counts".into(),
            ));
        }
        let graph_relation = if left.graph.manifest_hash == right.graph.manifest_hash {
            if left.graph.item_count != right.graph.item_count
                || left.graph.edge_count != right.graph.edge_count
            {
                return Err(WorkBranchComparisonError::NeedsRepair(
                    "one graph manifest has contradictory counts".into(),
                ));
            }
            WorkBranchComparisonRelation::Same
        } else {
            WorkBranchComparisonRelation::Different
        };
        let subject_relation = match (&left.subject, &right.subject) {
            (Some(left_subject), Some(right_subject))
                if left_subject.graph_revision != left.graph.current_revision
                    || right_subject.graph_revision != right.graph.current_revision =>
            {
                WorkBranchComparisonRelation::Unavailable
            }
            (Some(left), Some(right)) if left.subject_ref == right.subject_ref => {
                if left.subject_revision == right.subject_revision {
                    WorkBranchComparisonRelation::Same
                } else {
                    WorkBranchComparisonRelation::Different
                }
            }
            (Some(_), Some(_)) => WorkBranchComparisonRelation::Different,
            _ => WorkBranchComparisonRelation::Unavailable,
        };
        let evidence_relation = if left_evidence.manifest_hash == right_evidence.manifest_hash {
            WorkBranchComparisonRelation::Same
        } else {
            WorkBranchComparisonRelation::Different
        };
        transaction.commit().await?;
        Ok(WorkBranchComparisonReport {
            schema_version: WORK_BRANCH_COMPARISON_SCHEMA_VERSION,
            work_id: work_id.clone(),
            work_revision,
            directly_comparable: blockers.is_empty(),
            blockers,
            graph_relation,
            subject_relation,
            evidence_relation,
            left,
            right,
            left_evidence,
            right_evidence,
            coverage_gaps: vec![
                WorkBranchComparisonCoverageGap::ChangeDetails,
                WorkBranchComparisonCoverageGap::Risks,
                WorkBranchComparisonCoverageGap::TimeCost,
            ],
        })
    }
}

async fn load_evidence(
    transaction: &mut sqlx::Transaction<'_, sqlx::MySql>,
    owner_id: &WorkOwnerId,
    work_id: &WorkId,
    work_revision: WorkRevision,
    row: &sqlx::mysql::MySqlRow,
    side: &WorkBranchComparisonSide,
) -> Result<WorkBranchComparisonEvidence, WorkBranchComparisonError> {
    let criteria = super::repository::decode_criterion_set_manifest(
        &text(row, "criteria_member_manifest_json")?,
        i64::from(side.criteria.member_count),
    )
    .map_err(repair)?;
    let event_head = super::WorkEventSeq::new(integer(row, "last_event_seq")?).map_err(repair)?;
    let basis = side.subject.as_ref().and_then(|subject| {
        (subject.graph_revision == side.graph.current_revision).then_some(
            super::observation_repository::DeliveryEvidenceBasis {
                owner_id,
                work_id,
                branch_id: &side.branch_id,
                work_revision,
                goal_revision: side.goal_revision_ref,
                branch_revision: side.branch_revision,
                graph_revision: side.graph.current_revision,
                criterion_set_revision: side.criteria.revision,
                event_head,
                subject_ref: &subject.subject_ref,
                subject_revision: &subject.subject_revision,
            },
        )
    });
    let evidence = super::observation_repository::load_delivery_evidence_projection(
        transaction,
        basis.as_ref(),
        &criteria,
    )
    .await
    .map_err(repair)?;
    Ok(WorkBranchComparisonEvidence {
        manifest_hash: evidence.manifest_hash,
        required_count: bounded_usize(criteria.len(), 128)?,
        fresh_check_count: bounded_usize(evidence.fresh_checks.len(), 128)?,
        accepted_gap_count: bounded_usize(evidence.accepted_gaps.len(), 128)?,
    })
}

fn decode_side(
    row: &sqlx::mysql::MySqlRow,
    is_delivery: bool,
) -> Result<WorkBranchComparisonSide, WorkBranchComparisonError> {
    let branch_id = WorkBranchId::parse(text(row, "branch_id")?).map_err(repair)?;
    let basis_revision =
        GraphRevision::new(integer(row, "basis_graph_revision")?).map_err(repair)?;
    let current_revision =
        GraphRevision::new(integer(row, "current_graph_revision")?).map_err(repair)?;
    if current_revision < basis_revision {
        return Err(WorkBranchComparisonError::NeedsRepair(
            "branch graph revision precedes its fork basis".into(),
        ));
    }
    let subject_ref = optional_text(row, "subject_ref")?;
    let subject_revision = optional_text(row, "subject_revision")?;
    let subject_graph_revision = optional_integer(row, "subject_graph_revision")?;
    let subject = match (subject_ref, subject_revision, subject_graph_revision) {
        (None, None, None) => None,
        (Some(subject_ref), Some(subject_revision), Some(graph_revision)) => {
            Some(WorkBranchComparisonSubject {
                subject_ref: WorkSubjectRef::parse(subject_ref).map_err(repair)?,
                subject_revision: WorkContentHash::parse(subject_revision).map_err(repair)?,
                graph_revision: GraphRevision::new(graph_revision).map_err(repair)?,
            })
        }
        _ => {
            return Err(WorkBranchComparisonError::NeedsRepair(
                "branch has incomplete subject identity".into(),
            ));
        }
    };
    let item_count = bounded_count(integer(row, "graph_item_count")?, 256)?;
    let edge_count = bounded_count(integer(row, "graph_edge_count")?, 1024)?;
    let maximum_edges = item_count.saturating_mul(item_count.saturating_sub(1)) / 2;
    if edge_count > maximum_edges {
        return Err(WorkBranchComparisonError::NeedsRepair(
            "graph edge count exceeds a simple directed acyclic graph".into(),
        ));
    }
    Ok(WorkBranchComparisonSide {
        branch_id,
        branch_revision: WorkBranchRevision::new(integer(row, "branch_revision")?)
            .map_err(repair)?,
        is_delivery,
        goal_revision_ref: GoalRevision::new(integer(row, "goal_revision_ref")?).map_err(repair)?,
        criteria: WorkBranchComparisonCriteria {
            revision: CriterionSetRevision::new(integer(row, "criteria_set_revision_ref")?)
                .map_err(repair)?,
            manifest_hash: WorkContentHash::parse(text(row, "criteria_manifest_hash")?)
                .map_err(repair)?,
            member_count: bounded_count(integer(row, "criteria_member_count")?, 128)?,
        },
        graph: WorkBranchComparisonGraph {
            basis_revision,
            current_revision,
            manifest_hash: WorkContentHash::parse(text(row, "graph_manifest_hash")?)
                .map_err(repair)?,
            item_count,
            edge_count,
        },
        subject,
    })
}

fn bounded_count(value: i64, maximum: u16) -> Result<u16, WorkBranchComparisonError> {
    let value = u16::try_from(value).map_err(repair)?;
    if value > maximum {
        return Err(WorkBranchComparisonError::NeedsRepair(
            "comparison count exceeds its admission bound".into(),
        ));
    }
    Ok(value)
}

fn bounded_usize(value: usize, maximum: u16) -> Result<u16, WorkBranchComparisonError> {
    bounded_count(i64::try_from(value).map_err(repair)?, maximum)
}

fn text(
    row: &sqlx::mysql::MySqlRow,
    field: &'static str,
) -> Result<String, WorkBranchComparisonError> {
    row.try_get(field).map_err(repair)
}

fn optional_text(
    row: &sqlx::mysql::MySqlRow,
    field: &'static str,
) -> Result<Option<String>, WorkBranchComparisonError> {
    row.try_get(field).map_err(repair)
}

fn integer(
    row: &sqlx::mysql::MySqlRow,
    field: &'static str,
) -> Result<i64, WorkBranchComparisonError> {
    row.try_get(field).map_err(repair)
}

fn optional_integer(
    row: &sqlx::mysql::MySqlRow,
    field: &'static str,
) -> Result<Option<i64>, WorkBranchComparisonError> {
    row.try_get(field).map_err(repair)
}

fn repair(error: impl std::fmt::Display) -> WorkBranchComparisonError {
    WorkBranchComparisonError::NeedsRepair(error.to_string())
}
