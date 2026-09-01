use super::observation::{
    RevisionAlignment, WorkBranchOverview, WorkContentHash, WorkCriteriaSummary,
    WorkDeliverySubjectBasis, WorkDeliverySummary, WorkGoalOverview, WorkGraphSummary,
    WorkObservationReport, WorkObservationSatisfactionEvidenceRef, WorkOverview,
    WorkRetentionState,
};
use super::repository::{DatabaseWorkRepository, WorkRepositoryError};
use super::{
    CriterionRevisionRef, CriterionSetRevision, ForkCursorRef, GoalRevision, GraphRevision,
    OriginalIntentRef, ProjectId, WorkBranchId, WorkBranchRevision, WorkGoal, WorkObservationQuery,
    WorkRevision,
};
use chrono::Utc;
use sqlx::{MySql, QueryBuilder, Row, Transaction, query};
use std::collections::{BTreeMap, BTreeSet};

const SELECT_DECLARED_WORK_OBSERVATION_SQL: &str = "SELECT
    w.work_id,
    w.work_revision,
    w.project_id,
    w.original_intent_ref,
    w.current_goal_revision,
    w.current_criteria_set_revision,
    es.last_event_seq AS event_head,
    DATE_FORMAT(w.created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS work_created_at,
    DATE_FORMAT(w.archived_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS work_archived_at,
    g.revision AS goal_revision,
    g.goal_text,
    cs.revision AS criteria_revision,
    cs.member_count AS criteria_member_count,
    cs.member_manifest_json AS criteria_member_manifest_json,
    cs.member_manifest_hash AS criteria_manifest_hash,
    b.work_id AS branch_work_id,
    b.branch_id,
    b.branch_revision,
    b.origin_branch_id,
    b.fork_cursor,
    b.goal_revision_ref,
    b.criteria_set_revision_ref,
    b.basis_graph_revision,
    b.current_graph_revision,
    DATE_FORMAT(b.created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS branch_created_at,
    DATE_FORMAT(b.archived_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS branch_archived_at,
    s.graph_revision AS subject_graph_revision,
    s.subject_ref,
    s.subject_revision,
    gr.revision AS graph_revision,
    gr.item_count AS graph_item_count,
    gr.edge_count AS graph_edge_count,
    gr.manifest_hash AS graph_manifest_hash
FROM works w
LEFT JOIN work_event_sequences es
  ON es.owner_id = w.owner_id
 AND es.work_id = w.work_id
LEFT JOIN work_goal_revisions g
  ON g.owner_id = w.owner_id
 AND g.work_id = w.work_id
 AND g.revision = w.current_goal_revision
LEFT JOIN work_criterion_sets cs
  ON cs.owner_id = w.owner_id
 AND cs.work_id = w.work_id
 AND cs.revision = w.current_criteria_set_revision
LEFT JOIN work_branches b
  ON b.owner_id = w.owner_id
 AND b.work_id = w.work_id
 AND b.branch_id = w.delivery_branch_id
LEFT JOIN work_graph_revisions gr
  ON gr.owner_id = b.owner_id
 AND gr.work_id = b.work_id
 AND gr.revision = b.current_graph_revision
LEFT JOIN work_branch_subjects s
  ON s.owner_id = b.owner_id
 AND s.work_id = b.work_id
 AND s.branch_id = b.branch_id
WHERE w.owner_id = ? AND w.work_id = ?
LIMIT 1";

fn retention_state(archived: bool) -> WorkRetentionState {
    if archived {
        WorkRetentionState::Archived
    } else {
        WorkRetentionState::Active
    }
}

fn alignment(
    entity: &'static str,
    reference: i64,
    current: i64,
) -> Result<RevisionAlignment, WorkRepositoryError> {
    if reference > current {
        return Err(WorkRepositoryError::corrupt(
            entity,
            std::io::Error::other(format!(
                "branch reference r{reference} is ahead of current Work revision r{current}"
            )),
        ));
    }
    Ok(if reference == current {
        RevisionAlignment::Current
    } else {
        RevisionAlignment::Behind
    })
}

fn bounded_count(
    entity: &'static str,
    value: i32,
    maximum: u16,
) -> Result<u16, WorkRepositoryError> {
    let value =
        u16::try_from(value).map_err(|source| WorkRepositoryError::corrupt(entity, source))?;
    if value > maximum {
        return Err(WorkRepositoryError::corrupt(
            entity,
            std::io::Error::other(format!("count {value} exceeds bound {maximum}")),
        ));
    }
    Ok(value)
}

fn content_hash(
    entity: &'static str,
    value: String,
) -> Result<WorkContentHash, WorkRepositoryError> {
    WorkContentHash::parse(value)
        .map_err(|message| WorkRepositoryError::corrupt(entity, std::io::Error::other(message)))
}

pub(super) struct DeliveryEvidenceBasis<'a> {
    pub(super) owner_id: &'a super::WorkOwnerId,
    pub(super) work_id: &'a super::WorkId,
    pub(super) branch_id: &'a WorkBranchId,
    pub(super) work_revision: WorkRevision,
    pub(super) goal_revision: GoalRevision,
    pub(super) branch_revision: WorkBranchRevision,
    pub(super) graph_revision: GraphRevision,
    pub(super) criterion_set_revision: CriterionSetRevision,
    pub(super) event_head: super::WorkEventSeq,
    pub(super) subject_ref: &'a super::WorkSubjectRef,
    pub(super) subject_revision: &'a WorkContentHash,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub(super) struct FreshCheckEvidence {
    pub(super) check_run_id: super::CheckRunId,
    pub(super) payload_hash: WorkContentHash,
    pub(super) expires_at: Option<chrono::DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub(super) struct AcceptedGapEvidence {
    pub(super) decision_id: super::AcceptanceDecisionId,
    pub(super) payload_hash: WorkContentHash,
}

#[derive(Debug)]
pub(super) struct DeliveryEvidenceProjection {
    pub(super) manifest_hash: WorkContentHash,
    pub(super) fresh_checks: BTreeMap<CriterionRevisionRef, FreshCheckEvidence>,
    pub(super) accepted_gaps: BTreeMap<CriterionRevisionRef, AcceptedGapEvidence>,
}

#[derive(serde::Serialize)]
struct EvidenceManifestV1<'a> {
    schema_version: u16,
    criteria: &'a [CriterionRevisionRef],
    fresh_checks: Vec<(&'a CriterionRevisionRef, &'a FreshCheckEvidence)>,
    accepted_gaps: Vec<(&'a CriterionRevisionRef, &'a AcceptedGapEvidence)>,
}

pub(super) async fn load_fresh_check_criteria(
    transaction: &mut Transaction<'_, MySql>,
    basis: &DeliveryEvidenceBasis<'_>,
    criteria: &[CriterionRevisionRef],
) -> Result<BTreeMap<CriterionRevisionRef, FreshCheckEvidence>, WorkRepositoryError> {
    if criteria.is_empty() {
        return Ok(BTreeMap::new());
    }
    // One bounded round trip over at most 128 exact index-prefix seeks. This
    // remains independent of branch/check history length and avoids N+1.
    let mut builder = QueryBuilder::<MySql>::new(
        "SELECT check_run_id, payload_hash, criterion_id, criterion_revision, criterion_set_revision,
                graph_revision, subject_ref, subject_revision, outcome,
                coverage_state, evidence_ref_count,
                DATE_FORMAT(produced_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS produced_at,
                DATE_FORMAT(expires_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS expires_at
         FROM (",
    );
    for (index, criterion) in criteria.iter().enumerate() {
        if index > 0 {
            builder.push(" UNION ALL ");
        }
        builder
            .push(
                "(SELECT c.check_run_id, c.payload_hash, c.criterion_id, c.criterion_revision, c.criterion_set_revision,
                         c.graph_revision, c.subject_ref, c.subject_revision, c.outcome,
                         c.coverage_state, c.evidence_ref_count, c.produced_at, c.expires_at
                  FROM work_check_runs c
                  JOIN work_events e
                    ON e.owner_id = c.owner_id AND e.work_id = c.work_id
                   AND e.event_kind = 'check_recorded'
                   AND e.source_ref = c.check_run_id
                  WHERE c.owner_id = ",
            )
            .push_bind(basis.owner_id.as_str())
            .push(" AND c.work_id = ")
            .push_bind(basis.work_id.as_str())
            .push(" AND c.branch_id = ")
            .push_bind(basis.branch_id.as_str())
            .push(" AND c.criterion_id = ")
            .push_bind(criterion.criterion_id.as_str())
            .push(" AND c.criterion_revision = ")
            .push_bind(criterion.revision.get())
            .push(" AND e.event_seq <= ")
            .push_bind(basis.event_head.get())
            .push(" ORDER BY c.produced_at DESC, c.check_run_id DESC LIMIT 1)");
    }
    builder.push(") AS latest_criterion_checks");
    let rows = builder
        .build()
        .fetch_all(&mut **transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::persistence("load delivery check projection", source)
        })?;
    let members = criteria.iter().cloned().collect::<BTreeSet<_>>();
    let now = Utc::now();
    let mut fresh = BTreeMap::new();
    for row in rows {
        let text = |field: &'static str| {
            row.try_get::<String, _>(field)
                .map_err(|source| WorkRepositoryError::corrupt("delivery check projection", source))
        };
        let optional_text = |field: &'static str| {
            row.try_get::<Option<String>, _>(field)
                .map_err(|source| WorkRepositoryError::corrupt("delivery check projection", source))
        };
        let integer = |field: &'static str| {
            row.try_get::<i64, _>(field)
                .map_err(|source| WorkRepositoryError::corrupt("delivery check projection", source))
        };
        let criterion = CriterionRevisionRef {
            criterion_id: super::CriterionId::parse(text("criterion_id")?).map_err(|source| {
                WorkRepositoryError::corrupt("delivery check projection", source)
            })?,
            revision: super::CriterionRevision::new(integer("criterion_revision")?).map_err(
                |source| WorkRepositoryError::corrupt("delivery check projection", source),
            )?,
        };
        if !members.contains(&criterion) {
            return Err(WorkRepositoryError::corrupt(
                "delivery check projection",
                std::io::Error::other("latest check does not belong to current criteria"),
            ));
        }
        let outcome = text("outcome")?;
        let outcome = super::CheckOutcome::from_persisted(&outcome).ok_or_else(|| {
            WorkRepositoryError::corrupt(
                "delivery check projection",
                std::io::Error::other("unknown persisted check outcome"),
            )
        })?;
        let coverage = text("coverage_state")?;
        let coverage = super::CheckCoverage::from_persisted(&coverage).ok_or_else(|| {
            WorkRepositoryError::corrupt(
                "delivery check projection",
                std::io::Error::other("unknown persisted check coverage"),
            )
        })?;
        let evidence_ref_count = row
            .try_get::<i32, _>("evidence_ref_count")
            .map_err(|source| WorkRepositoryError::corrupt("delivery check projection", source))?;
        if !(0..=32).contains(&evidence_ref_count) {
            return Err(WorkRepositoryError::corrupt(
                "delivery check projection",
                std::io::Error::other("persisted evidence count exceeds its admission bound"),
            ));
        }
        if (outcome == super::CheckOutcome::Passed
            && (coverage != super::CheckCoverage::Complete || evidence_ref_count == 0))
            || (outcome == super::CheckOutcome::Failed && evidence_ref_count == 0)
        {
            return Err(WorkRepositoryError::corrupt(
                "delivery check projection",
                std::io::Error::other("persisted check has incoherent coverage evidence"),
            ));
        }
        let expires_at = optional_text("expires_at")?
            .map(|value| {
                super::repository::decode_timestamp(
                    "delivery check projection",
                    "expires_at",
                    value,
                )
            })
            .transpose()?;
        // Decode produced_at even though freshness only needs expiry. This
        // fails closed if ordering metadata is corrupt.
        let produced_at = super::repository::decode_timestamp(
            "delivery check projection",
            "produced_at",
            text("produced_at")?,
        )?;
        if expires_at.is_some_and(|expires_at| expires_at <= produced_at) {
            return Err(WorkRepositoryError::corrupt(
                "delivery check projection",
                std::io::Error::other("persisted check expiry precedes production"),
            ));
        }
        let exact_basis = integer("criterion_set_revision")? == basis.criterion_set_revision.get()
            && integer("graph_revision")? == basis.graph_revision.get()
            && text("subject_ref")? == basis.subject_ref.as_str()
            && text("subject_revision")? == basis.subject_revision.as_str()
            && expires_at.is_none_or(|expires_at| expires_at > now);
        if exact_basis
            && outcome == super::CheckOutcome::Passed
            && coverage == super::CheckCoverage::Complete
            && evidence_ref_count > 0
            && fresh
                .insert(
                    criterion,
                    FreshCheckEvidence {
                        check_run_id: super::CheckRunId::parse(text("check_run_id")?).map_err(
                            |source| {
                                WorkRepositoryError::corrupt("delivery check projection", source)
                            },
                        )?,
                        payload_hash: content_hash(
                            "delivery check projection",
                            text("payload_hash")?,
                        )?,
                        expires_at,
                    },
                )
                .is_some()
        {
            return Err(WorkRepositoryError::corrupt(
                "delivery check projection",
                std::io::Error::other("multiple latest checks returned for one criterion"),
            ));
        }
    }
    Ok(fresh)
}

pub(super) async fn load_current_accepted_gaps(
    transaction: &mut Transaction<'_, MySql>,
    basis: &DeliveryEvidenceBasis<'_>,
    criteria: &[CriterionRevisionRef],
) -> Result<BTreeMap<CriterionRevisionRef, AcceptedGapEvidence>, WorkRepositoryError> {
    if criteria.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut builder = QueryBuilder::<MySql>::new(
        "SELECT criterion_id, criterion_revision, decision_id, decision_payload_hash,
                work_revision, goal_revision,
                branch_revision, graph_revision, criterion_set_revision,
                subject_ref, subject_revision, gap_reason
         FROM work_current_gap_acceptances
         WHERE owner_id = ",
    );
    builder
        .push_bind(basis.owner_id.as_str())
        .push(" AND work_id = ")
        .push_bind(basis.work_id.as_str())
        .push(" AND branch_id = ")
        .push_bind(basis.branch_id.as_str())
        .push(" AND criterion_id IN (");
    let mut separated = builder.separated(", ");
    for criterion in criteria {
        separated.push_bind(criterion.criterion_id.as_str());
    }
    separated.push_unseparated(") AND decision_event_seq <= ");
    builder.push_bind(basis.event_head.get());
    let rows = builder
        .build()
        .fetch_all(&mut **transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::persistence("load delivery acceptance projection", source)
        })?;
    let members = criteria.iter().cloned().collect::<BTreeSet<_>>();
    let mut accepted = BTreeMap::new();
    for row in rows {
        let text = |field: &'static str| {
            row.try_get::<String, _>(field).map_err(|source| {
                WorkRepositoryError::corrupt("delivery acceptance projection", source)
            })
        };
        let integer = |field: &'static str| {
            row.try_get::<i64, _>(field).map_err(|source| {
                WorkRepositoryError::corrupt("delivery acceptance projection", source)
            })
        };
        let criterion = CriterionRevisionRef {
            criterion_id: super::CriterionId::parse(text("criterion_id")?).map_err(|source| {
                WorkRepositoryError::corrupt("delivery acceptance projection", source)
            })?,
            revision: super::CriterionRevision::new(integer("criterion_revision")?).map_err(
                |source| WorkRepositoryError::corrupt("delivery acceptance projection", source),
            )?,
        };
        if !members.contains(&criterion) {
            continue;
        }
        let reason = text("gap_reason")?;
        super::AcceptanceGapReason::from_persisted(&reason).ok_or_else(|| {
            WorkRepositoryError::corrupt(
                "delivery acceptance projection",
                std::io::Error::other("unknown persisted acceptance-gap reason"),
            )
        })?;
        let exact_basis = integer("work_revision")? == basis.work_revision.get()
            && integer("goal_revision")? == basis.goal_revision.get()
            && integer("branch_revision")? == basis.branch_revision.get()
            && integer("graph_revision")? == basis.graph_revision.get()
            && integer("criterion_set_revision")? == basis.criterion_set_revision.get()
            && text("subject_ref")? == basis.subject_ref.as_str()
            && text("subject_revision")? == basis.subject_revision.as_str();
        if exact_basis {
            let evidence = AcceptedGapEvidence {
                decision_id: super::AcceptanceDecisionId::parse(text("decision_id")?).map_err(
                    |source| WorkRepositoryError::corrupt("delivery acceptance projection", source),
                )?,
                payload_hash: content_hash(
                    "delivery acceptance projection",
                    text("decision_payload_hash")?,
                )?,
            };
            if accepted.insert(criterion, evidence).is_some() {
                return Err(WorkRepositoryError::corrupt(
                    "delivery acceptance projection",
                    std::io::Error::other(
                        "multiple current acceptances returned for one criterion",
                    ),
                ));
            }
        }
    }
    Ok(accepted)
}

pub(super) async fn load_delivery_evidence_projection(
    transaction: &mut Transaction<'_, MySql>,
    basis: Option<&DeliveryEvidenceBasis<'_>>,
    criteria: &[CriterionRevisionRef],
) -> Result<DeliveryEvidenceProjection, WorkRepositoryError> {
    let (fresh_checks, accepted_gaps) = if let Some(basis) = basis {
        (
            load_fresh_check_criteria(transaction, basis, criteria).await?,
            load_current_accepted_gaps(transaction, basis, criteria).await?,
        )
    } else {
        (BTreeMap::new(), BTreeMap::new())
    };
    let manifest = EvidenceManifestV1 {
        schema_version: 1,
        criteria,
        fresh_checks: fresh_checks.iter().collect(),
        accepted_gaps: accepted_gaps.iter().collect(),
    };
    let manifest_json = serde_json::to_string(&manifest).map_err(|source| {
        WorkRepositoryError::ManifestEncoding {
            entity: "delivery evidence manifest",
            source,
        }
    })?;
    let manifest_hash = WorkContentHash::parse(super::repository::content_hash(&manifest_json))
        .map_err(|source| {
            WorkRepositoryError::corrupt(
                "delivery evidence manifest",
                std::io::Error::other(source),
            )
        })?;
    Ok(DeliveryEvidenceProjection {
        manifest_hash,
        fresh_checks,
        accepted_gaps,
    })
}

pub(super) async fn observe_declared_work(
    repository: &DatabaseWorkRepository,
    observation: WorkObservationQuery,
) -> Result<WorkObservationReport, WorkRepositoryError> {
    let mut transaction = repository.pool.get().begin().await.map_err(|source| {
        WorkRepositoryError::persistence("begin Work observation transaction", source)
    })?;
    let row = query(SELECT_DECLARED_WORK_OBSERVATION_SQL)
        .bind(observation.owner_id.as_str())
        .bind(observation.work_id.as_str())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::persistence("load declared Work observation", source)
        })?
        .ok_or(WorkRepositoryError::NotFound)?;
    let string = |field: &'static str| {
        row.try_get::<String, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("declared Work observation", source))
    };
    let optional_string = |field: &'static str| {
        row.try_get::<Option<String>, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("declared Work observation", source))
    };
    let integer = |field: &'static str| {
        row.try_get::<i64, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("declared Work observation", source))
    };
    let optional_integer = |field: &'static str| {
        row.try_get::<Option<i64>, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("declared Work observation", source))
    };
    let count = |field: &'static str| {
        row.try_get::<i32, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("declared Work observation", source))
    };

    let work_id = super::WorkId::parse(string("work_id")?)
        .map_err(|source| WorkRepositoryError::corrupt("Work", source))?;
    let work_revision = WorkRevision::new(integer("work_revision")?)
        .map_err(|source| WorkRepositoryError::corrupt("Work", source))?;
    let current_goal_revision = GoalRevision::new(integer("current_goal_revision")?)
        .map_err(|source| WorkRepositoryError::corrupt("Work", source))?;
    let current_criteria_revision =
        CriterionSetRevision::new(integer("current_criteria_set_revision")?)
            .map_err(|source| WorkRepositoryError::corrupt("Work", source))?;
    let event_head = super::WorkEventSeq::new(integer("event_head")?)
        .map_err(|source| WorkRepositoryError::corrupt("Work event sequence", source))?;
    let goal_revision = GoalRevision::new(integer("goal_revision")?)
        .map_err(|source| WorkRepositoryError::corrupt("Goal revision", source))?;
    if goal_revision != current_goal_revision {
        return Err(WorkRepositoryError::corrupt(
            "Goal revision",
            std::io::Error::other("joined Goal revision does not match the Work pointer"),
        ));
    }
    let criteria_revision = CriterionSetRevision::new(integer("criteria_revision")?)
        .map_err(|source| WorkRepositoryError::corrupt("criterion set", source))?;
    if criteria_revision != current_criteria_revision {
        return Err(WorkRepositoryError::corrupt(
            "criterion set",
            std::io::Error::other("joined criterion-set revision does not match the Work pointer"),
        ));
    }
    let branch_work_id = super::WorkId::parse(string("branch_work_id")?)
        .map_err(|source| WorkRepositoryError::corrupt("Work branch", source))?;
    if branch_work_id != work_id {
        return Err(WorkRepositoryError::corrupt(
            "Work branch",
            std::io::Error::other("delivery branch belongs to a different Work"),
        ));
    }
    let branch_goal_revision = GoalRevision::new(integer("goal_revision_ref")?)
        .map_err(|source| WorkRepositoryError::corrupt("Work branch", source))?;
    let branch_criteria_revision = CriterionSetRevision::new(integer("criteria_set_revision_ref")?)
        .map_err(|source| WorkRepositoryError::corrupt("Work branch", source))?;
    let current_graph_revision = GraphRevision::new(integer("current_graph_revision")?)
        .map_err(|source| WorkRepositoryError::corrupt("Work branch", source))?;
    let graph_revision = GraphRevision::new(integer("graph_revision")?)
        .map_err(|source| WorkRepositoryError::corrupt("graph revision", source))?;
    if graph_revision != current_graph_revision {
        return Err(WorkRepositoryError::corrupt(
            "graph revision",
            std::io::Error::other("joined graph revision does not match the branch pointer"),
        ));
    }

    let basis_graph_revision = GraphRevision::new(integer("basis_graph_revision")?)
        .map_err(|source| WorkRepositoryError::corrupt("Work branch", source))?;
    if current_graph_revision < basis_graph_revision {
        return Err(WorkRepositoryError::corrupt(
            "Work branch",
            std::io::Error::other("current graph revision precedes the branch basis"),
        ));
    }
    let origin_branch_id = optional_string("origin_branch_id")?
        .map(WorkBranchId::parse)
        .transpose()
        .map_err(|source| WorkRepositoryError::corrupt("Work branch", source))?;
    let fork_cursor = optional_string("fork_cursor")?
        .map(ForkCursorRef::parse)
        .transpose()
        .map_err(|source| WorkRepositoryError::corrupt("Work branch", source))?;
    if origin_branch_id.is_some() != fork_cursor.is_some() {
        return Err(WorkRepositoryError::corrupt(
            "Work branch",
            std::io::Error::other("fork origin and cursor must be present together"),
        ));
    }
    let work_created_at =
        super::repository::decode_timestamp("Work", "created_at", string("work_created_at")?)?;
    let work_archived_at = super::repository::optional_timestamp(
        "Work",
        "archived_at",
        optional_string("work_archived_at")?,
    )?;
    if work_archived_at.is_some_and(|archived_at| archived_at < work_created_at) {
        return Err(WorkRepositoryError::corrupt(
            "Work",
            std::io::Error::other("archive timestamp precedes creation"),
        ));
    }
    let branch_created_at = super::repository::decode_timestamp(
        "Work branch",
        "created_at",
        string("branch_created_at")?,
    )?;
    let branch_archived_at = super::repository::optional_timestamp(
        "Work branch",
        "archived_at",
        optional_string("branch_archived_at")?,
    )?;
    if branch_archived_at.is_some_and(|archived_at| archived_at < branch_created_at) {
        return Err(WorkRepositoryError::corrupt(
            "Work branch",
            std::io::Error::other("archive timestamp precedes creation"),
        ));
    }
    let item_count = bounded_count(
        "graph revision",
        count("graph_item_count")?,
        super::graph::WORK_GRAPH_MAX_ITEMS as u16,
    )?;
    let edge_count = bounded_count(
        "graph revision",
        count("graph_edge_count")?,
        super::graph::WORK_GRAPH_MAX_EDGES as u16,
    )?;
    let maximum_dag_edges = item_count.saturating_mul(item_count.saturating_sub(1)) / 2;
    if edge_count > maximum_dag_edges {
        return Err(WorkRepositoryError::corrupt(
            "graph revision",
            std::io::Error::other("edge count exceeds a simple directed acyclic graph"),
        ));
    }
    let criteria_member_count = bounded_count(
        "criterion set",
        count("criteria_member_count")?,
        super::criteria::CRITERION_SET_MAX_MEMBERS as u16,
    )?;
    let criteria = super::repository::decode_criterion_set_manifest(
        &string("criteria_member_manifest_json")?,
        i64::from(criteria_member_count),
    )?;
    let goal_alignment = alignment(
        "Work branch Goal basis",
        branch_goal_revision.get(),
        current_goal_revision.get(),
    )?;
    let criteria_alignment = alignment(
        "Work branch criterion-set basis",
        branch_criteria_revision.get(),
        current_criteria_revision.get(),
    )?;
    let branch_id = WorkBranchId::parse(string("branch_id")?)
        .map_err(|source| WorkRepositoryError::corrupt("Work branch", source))?;
    let branch_revision = WorkBranchRevision::new(integer("branch_revision")?)
        .map_err(|source| WorkRepositoryError::corrupt("Work branch", source))?;
    let subject_graph_revision = optional_integer("subject_graph_revision")?;
    let subject_ref = optional_string("subject_ref")?;
    let subject_revision = optional_string("subject_revision")?;
    let (subject, current_subject) = match (subject_graph_revision, subject_ref, subject_revision) {
        (None, None, None) => (WorkDeliverySubjectBasis::Unavailable, None),
        (Some(subject_graph_revision), Some(subject_ref), Some(subject_revision)) => {
            let subject_graph_revision = GraphRevision::new(subject_graph_revision)
                .map_err(|source| WorkRepositoryError::corrupt("Work branch subject", source))?;
            if subject_graph_revision > current_graph_revision {
                return Err(WorkRepositoryError::corrupt(
                    "Work branch subject",
                    std::io::Error::other("subject graph revision is ahead of the branch"),
                ));
            }
            let subject_ref = super::WorkSubjectRef::parse(subject_ref)
                .map_err(|source| WorkRepositoryError::corrupt("Work branch subject", source))?;
            let subject_revision = content_hash("Work branch subject", subject_revision)?;
            if subject_graph_revision == current_graph_revision {
                (
                    WorkDeliverySubjectBasis::Current(subject_revision.clone()),
                    Some((subject_ref, subject_revision)),
                )
            } else {
                (WorkDeliverySubjectBasis::OutOfDate, None)
            }
        }
        _ => {
            return Err(WorkRepositoryError::corrupt(
                "Work branch subject",
                std::io::Error::other("subject identity and revisions must be present together"),
            ));
        }
    };
    let branch_basis_current = goal_alignment == RevisionAlignment::Current
        && criteria_alignment == RevisionAlignment::Current;
    let (fresh_check_evidence, accepted_gap_evidence) = match current_subject {
        Some((subject_ref, subject_revision)) if branch_basis_current && !criteria.is_empty() => {
            let basis = DeliveryEvidenceBasis {
                owner_id: &observation.owner_id,
                work_id: &work_id,
                branch_id: &branch_id,
                work_revision,
                goal_revision: current_goal_revision,
                branch_revision,
                graph_revision: current_graph_revision,
                criterion_set_revision: current_criteria_revision,
                event_head,
                subject_ref: &subject_ref,
                subject_revision: &subject_revision,
            };
            let fresh_checks =
                load_fresh_check_criteria(&mut transaction, &basis, &criteria).await?;
            let accepted_gaps =
                load_current_accepted_gaps(&mut transaction, &basis, &criteria).await?;
            (fresh_checks, accepted_gaps)
        }
        _ => (BTreeMap::new(), BTreeMap::new()),
    };
    let fresh_checks = fresh_check_evidence
        .iter()
        .map(|(criterion, evidence)| (criterion.clone(), evidence.expires_at))
        .collect();
    let accepted_gaps = accepted_gap_evidence.keys().cloned().collect();
    let delivery = WorkDeliverySummary::derive(
        &criteria,
        branch_basis_current,
        subject,
        fresh_checks,
        accepted_gaps,
    )
    .map_err(|message| {
        WorkRepositoryError::corrupt("Work delivery projection", std::io::Error::other(message))
    })?;
    let overview = WorkOverview {
        work_id: work_id.clone(),
        work_revision,
        project_id: optional_string("project_id")?
            .map(ProjectId::parse)
            .transpose()
            .map_err(|source| WorkRepositoryError::corrupt("Work", source))?,
        original_intent_ref: OriginalIntentRef::parse(string("original_intent_ref")?)
            .map_err(|source| WorkRepositoryError::corrupt("Work", source))?,
        goal: WorkGoalOverview {
            revision: goal_revision,
            goal: WorkGoal::parse(string("goal_text")?)
                .map_err(|source| WorkRepositoryError::corrupt("Goal revision", source))?,
        },
        criteria: WorkCriteriaSummary {
            revision: criteria_revision,
            member_count: criteria_member_count,
            manifest_hash: content_hash("criterion set", string("criteria_manifest_hash")?)?,
        },
        delivery_branch: WorkBranchOverview {
            work_id: work_id.clone(),
            branch_id,
            branch_revision,
            origin_branch_id,
            fork_cursor,
            goal_revision_ref: branch_goal_revision,
            goal_alignment,
            criteria_set_revision_ref: branch_criteria_revision,
            criteria_alignment,
            basis_graph_revision,
            current_graph_revision,
            retention_state: retention_state(branch_archived_at.is_some()),
            created_at: branch_created_at,
            archived_at: branch_archived_at,
        },
        graph: WorkGraphSummary {
            revision: graph_revision,
            item_count,
            edge_count,
            manifest_hash: content_hash("graph revision", string("graph_manifest_hash")?)?,
        },
        delivery,
        event_head,
        retention_state: retention_state(work_archived_at.is_some()),
        created_at: work_created_at,
        archived_at: work_archived_at,
    };
    let mut satisfaction_evidence_refs = fresh_check_evidence
        .into_iter()
        .map(
            |(criterion, evidence)| WorkObservationSatisfactionEvidenceRef::CheckRun {
                criterion,
                check_run_id: evidence.check_run_id,
                payload_hash: evidence.payload_hash,
            },
        )
        .collect::<Vec<_>>();
    let checked_criteria = satisfaction_evidence_refs
        .iter()
        .filter_map(|evidence| match evidence {
            WorkObservationSatisfactionEvidenceRef::CheckRun { criterion, .. } => {
                Some(criterion.clone())
            }
            WorkObservationSatisfactionEvidenceRef::AcceptanceDecision { .. } => None,
        })
        .collect::<BTreeSet<_>>();
    satisfaction_evidence_refs.extend(
        accepted_gap_evidence
            .into_iter()
            .filter(|(criterion, _)| !checked_criteria.contains(criterion))
            .map(|(criterion, evidence)| {
                WorkObservationSatisfactionEvidenceRef::AcceptanceDecision {
                    criterion,
                    decision_id: evidence.decision_id,
                    payload_hash: evidence.payload_hash,
                }
            }),
    );
    let report = WorkObservationReport::from_overview(overview, satisfaction_evidence_refs)
        .map_err(|source| WorkRepositoryError::corrupt("Work observation report", source))?;
    transaction.commit().await.map_err(|source| {
        WorkRepositoryError::persistence("commit Work observation transaction", source)
    })?;
    Ok(report)
}
