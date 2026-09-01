use super::delivery_selection::{
    WorkDeliverySelection, WorkDeliverySelectionBasisResource, WorkDeliverySelectionOutcome,
    WorkDeliverySelectionReceipt,
};
use super::events::NewWorkEvent;
use super::repository::{DatabaseWorkRepository, WorkRepositoryError};
use super::{
    CriterionSetRevision, GoalRevision, GraphRevision, WorkBranchId, WorkBranchRevision,
    WorkContentHash, WorkEventKind, WorkRevision,
};
use sqlx::{MySql, Row, Transaction, query};

fn conflict(resource: WorkDeliverySelectionBasisResource) -> WorkRepositoryError {
    WorkRepositoryError::StaleDeliverySelection { resource }
}

fn payload_hash(selection: &WorkDeliverySelection) -> Result<WorkContentHash, WorkRepositoryError> {
    let payload = serde_json::to_string(selection).map_err(|source| {
        WorkRepositoryError::ManifestEncoding {
            entity: "delivery selection payload",
            source,
        }
    })?;
    WorkContentHash::parse(super::repository::content_hash(&payload)).map_err(|source| {
        WorkRepositoryError::corrupt("delivery selection payload", std::io::Error::other(source))
    })
}

async fn replay(
    transaction: &mut Transaction<'_, MySql>,
    selection: &WorkDeliverySelection,
    expected_payload_hash: &WorkContentHash,
) -> Result<Option<WorkDeliverySelectionReceipt>, WorkRepositoryError> {
    let row = query(
        "SELECT branch_id, work_revision, branch_revision, graph_revision, payload_hash
         FROM work_events
         WHERE owner_id = ? AND work_id = ?
           AND event_kind = 'delivery_branch_selected' AND source_ref = ? LIMIT 1",
    )
    .bind(selection.owner_id.as_str())
    .bind(selection.work_id.as_str())
    .bind(selection.request_id.as_str())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("load delivery selection replay", source))?;
    let Some(row) = row else { return Ok(None) };
    let stored_hash = WorkContentHash::parse(
        row.try_get::<String, _>("payload_hash")
            .map_err(|source| WorkRepositoryError::corrupt("delivery selection event", source))?,
    )
    .map_err(|source| {
        WorkRepositoryError::corrupt("delivery selection event", std::io::Error::other(source))
    })?;
    if stored_hash != *expected_payload_hash {
        return Err(conflict(WorkDeliverySelectionBasisResource::RequestPayload));
    }
    let work_revision = WorkRevision::new(
        row.try_get("work_revision")
            .map_err(|source| WorkRepositoryError::corrupt("delivery selection event", source))?,
    )
    .map_err(|source| WorkRepositoryError::corrupt("delivery selection event", source))?;
    let outcome = if work_revision == selection.expected_work_revision {
        WorkDeliverySelectionOutcome::AlreadySelected
    } else {
        WorkDeliverySelectionOutcome::Selected
    };
    Ok(Some(WorkDeliverySelectionReceipt {
        schema_version: super::WORK_DELIVERY_SELECTION_SCHEMA_VERSION,
        work_id: selection.work_id.clone(),
        request_id: selection.request_id.clone(),
        delivery_branch_id: WorkBranchId::parse(
            row.try_get::<String, _>("branch_id").map_err(|source| {
                WorkRepositoryError::corrupt("delivery selection event", source)
            })?,
        )
        .map_err(|source| WorkRepositoryError::corrupt("delivery selection event", source))?,
        work_revision,
        branch_revision: WorkBranchRevision::new(
            row.try_get("branch_revision").map_err(|source| {
                WorkRepositoryError::corrupt("delivery selection event", source)
            })?,
        )
        .map_err(|source| WorkRepositoryError::corrupt("delivery selection event", source))?,
        graph_revision: GraphRevision::new(
            row.try_get("graph_revision").map_err(|source| {
                WorkRepositoryError::corrupt("delivery selection event", source)
            })?,
        )
        .map_err(|source| WorkRepositoryError::corrupt("delivery selection event", source))?,
        evidence_manifest_hash: selection.expected_evidence_manifest_hash.clone(),
        outcome,
    }))
}

pub(super) async fn select_delivery_branch(
    repository: &DatabaseWorkRepository,
    selection: WorkDeliverySelection,
) -> Result<WorkDeliverySelectionReceipt, WorkRepositoryError> {
    let selection_payload_hash = payload_hash(&selection)?;
    let mut transaction = repository.pool.get().begin().await.map_err(|source| {
        WorkRepositoryError::persistence("begin delivery selection transaction", source)
    })?;
    // Serialize at the Work aggregate before replay. A concurrent identical
    // request must observe the first committed receipt instead of racing into
    // a stale-basis error. This follows the same aggregate-first lock order as
    // other Work mutations; the event-sequence row remains append-owned.
    query(
        "SELECT work_revision FROM works
         WHERE owner_id = ? AND work_id = ? LIMIT 1 FOR UPDATE",
    )
    .bind(selection.owner_id.as_str())
    .bind(selection.work_id.as_str())
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("lock delivery selection Work", source))?
    .ok_or(WorkRepositoryError::NotFound)?;
    if let Some(receipt) = replay(&mut transaction, &selection, &selection_payload_hash).await? {
        transaction.commit().await.map_err(|source| {
            WorkRepositoryError::persistence("commit delivery selection replay", source)
        })?;
        return Ok(receipt);
    }
    let row = query(
        "SELECT w.work_revision, w.current_goal_revision, w.current_criteria_set_revision,
                w.delivery_branch_id, CASE WHEN w.archived_at IS NULL THEN 0 ELSE 1 END AS archived,
                es.last_event_seq, b.branch_revision, b.goal_revision_ref,
                b.criteria_set_revision_ref, b.current_graph_revision,
                CASE WHEN b.archived_at IS NULL THEN 0 ELSE 1 END AS branch_archived,
                CASE WHEN b.deletion_operation_id IS NULL THEN 0 ELSE 1 END AS branch_deleting,
                cs.member_count, cs.member_manifest_json,
                gr.revision AS materialized_graph_revision,
                s.graph_revision AS subject_graph_revision, s.subject_ref, s.subject_revision
         FROM works w
         JOIN work_branches b
           ON b.owner_id = w.owner_id AND b.work_id = w.work_id AND b.branch_id = ?
         LEFT JOIN work_event_sequences es
           ON es.owner_id = w.owner_id AND es.work_id = w.work_id
         LEFT JOIN work_criterion_sets cs
           ON cs.owner_id = b.owner_id AND cs.work_id = b.work_id
          AND cs.revision = b.criteria_set_revision_ref
         LEFT JOIN work_graph_revisions gr
           ON gr.owner_id = b.owner_id AND gr.work_id = b.work_id
          AND gr.revision = b.current_graph_revision
         LEFT JOIN work_branch_subjects s
           ON s.owner_id = b.owner_id AND s.work_id = b.work_id AND s.branch_id = b.branch_id
         WHERE w.owner_id = ? AND w.work_id = ? LIMIT 1 FOR UPDATE",
    )
    .bind(selection.branch_id.as_str())
    .bind(selection.owner_id.as_str())
    .bind(selection.work_id.as_str())
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("lock delivery selection basis", source))?
    .ok_or(WorkRepositoryError::NotFound)?;
    let integer = |field: &'static str| {
        row.try_get::<i64, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("delivery selection basis", source))
    };
    let text = |field: &'static str| {
        row.try_get::<String, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("delivery selection basis", source))
    };
    let optional_text = |field: &'static str| {
        row.try_get::<Option<String>, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("delivery selection basis", source))
    };
    let optional_integer = |field: &'static str| {
        row.try_get::<Option<i64>, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("delivery selection basis", source))
    };
    if integer("archived")? != 0 || integer("branch_archived")? != 0 {
        return Err(WorkRepositoryError::Archived);
    }
    if integer("branch_deleting")? != 0 {
        return Err(WorkRepositoryError::BranchDeleting);
    }
    let work_revision = WorkRevision::new(integer("work_revision")?)
        .map_err(|source| WorkRepositoryError::corrupt("delivery selection basis", source))?;
    let work_goal = GoalRevision::new(integer("current_goal_revision")?)
        .map_err(|source| WorkRepositoryError::corrupt("delivery selection basis", source))?;
    let work_criteria = CriterionSetRevision::new(integer("current_criteria_set_revision")?)
        .map_err(|source| WorkRepositoryError::corrupt("delivery selection basis", source))?;
    let branch_revision = WorkBranchRevision::new(integer("branch_revision")?)
        .map_err(|source| WorkRepositoryError::corrupt("delivery selection basis", source))?;
    let branch_goal = GoalRevision::new(integer("goal_revision_ref")?)
        .map_err(|source| WorkRepositoryError::corrupt("delivery selection basis", source))?;
    let branch_criteria = CriterionSetRevision::new(integer("criteria_set_revision_ref")?)
        .map_err(|source| WorkRepositoryError::corrupt("delivery selection basis", source))?;
    let graph_revision = GraphRevision::new(integer("current_graph_revision")?)
        .map_err(|source| WorkRepositoryError::corrupt("delivery selection basis", source))?;
    if optional_integer("materialized_graph_revision")? != Some(graph_revision.get()) {
        return Err(WorkRepositoryError::corrupt(
            "delivery selection basis",
            std::io::Error::other("selected graph revision is missing"),
        ));
    }
    for (matches, resource) in [
        (
            work_revision == selection.expected_work_revision,
            WorkDeliverySelectionBasisResource::WorkRevision,
        ),
        (
            branch_revision == selection.expected_branch_revision,
            WorkDeliverySelectionBasisResource::BranchRevision,
        ),
        (
            branch_goal == selection.expected_goal_revision,
            WorkDeliverySelectionBasisResource::GoalRevision,
        ),
        (
            branch_criteria == selection.expected_criteria_set_revision,
            WorkDeliverySelectionBasisResource::CriterionSetRevision,
        ),
        (
            graph_revision == selection.expected_graph_revision,
            WorkDeliverySelectionBasisResource::GraphRevision,
        ),
    ] {
        if !matches {
            return Err(conflict(resource));
        }
    }
    if branch_goal != work_goal || branch_criteria != work_criteria {
        return Err(conflict(WorkDeliverySelectionBasisResource::WorkDefinition));
    }
    let current_subject = match (
        optional_integer("subject_graph_revision")?,
        optional_text("subject_ref")?,
        optional_text("subject_revision")?,
    ) {
        (None, None, None) => None,
        (Some(graph), Some(reference), Some(revision)) => {
            Some(super::WorkDeliverySelectionSubject {
                graph_revision: GraphRevision::new(graph).map_err(|source| {
                    WorkRepositoryError::corrupt("delivery selection subject", source)
                })?,
                subject_ref: super::WorkSubjectRef::parse(reference).map_err(|source| {
                    WorkRepositoryError::corrupt("delivery selection subject", source)
                })?,
                subject_revision: WorkContentHash::parse(revision).map_err(|source| {
                    WorkRepositoryError::corrupt(
                        "delivery selection subject",
                        std::io::Error::other(source),
                    )
                })?,
            })
        }
        _ => {
            return Err(WorkRepositoryError::corrupt(
                "delivery selection subject",
                std::io::Error::other("subject identity is incomplete"),
            ));
        }
    };
    if current_subject != selection.expected_subject {
        return Err(conflict(WorkDeliverySelectionBasisResource::Subject));
    }
    let member_count = integer("member_count")?;
    let criteria = super::repository::decode_criterion_set_manifest(
        &text("member_manifest_json")?,
        member_count,
    )?;
    let event_head = super::WorkEventSeq::new(integer("last_event_seq")?)
        .map_err(|source| WorkRepositoryError::corrupt("delivery selection basis", source))?;
    let evidence_basis = current_subject.as_ref().and_then(|subject| {
        (subject.graph_revision == graph_revision).then_some(
            super::observation_repository::DeliveryEvidenceBasis {
                owner_id: &selection.owner_id,
                work_id: &selection.work_id,
                branch_id: &selection.branch_id,
                work_revision,
                goal_revision: branch_goal,
                branch_revision,
                graph_revision,
                criterion_set_revision: branch_criteria,
                event_head,
                subject_ref: &subject.subject_ref,
                subject_revision: &subject.subject_revision,
            },
        )
    });
    let evidence = super::observation_repository::load_delivery_evidence_projection(
        &mut transaction,
        evidence_basis.as_ref(),
        &criteria,
    )
    .await?;
    if evidence.manifest_hash != selection.expected_evidence_manifest_hash {
        return Err(conflict(WorkDeliverySelectionBasisResource::Evidence));
    }
    let current_delivery = WorkBranchId::parse(text("delivery_branch_id")?)
        .map_err(|source| WorkRepositoryError::corrupt("delivery selection basis", source))?;
    let (result_work_revision, outcome) = if current_delivery == selection.branch_id {
        (work_revision, WorkDeliverySelectionOutcome::AlreadySelected)
    } else {
        let next = work_revision
            .checked_next()
            .map_err(super::repository::invalid_mutation)?;
        let updated = query(
            "UPDATE works SET work_revision = ?, delivery_branch_id = ?, updated_at = NOW(6)
             WHERE owner_id = ? AND work_id = ? AND work_revision = ? AND delivery_branch_id = ?
               AND current_goal_revision = ? AND current_criteria_set_revision = ?
               AND archived_at IS NULL",
        )
        .bind(next.get())
        .bind(selection.branch_id.as_str())
        .bind(selection.owner_id.as_str())
        .bind(selection.work_id.as_str())
        .bind(work_revision.get())
        .bind(current_delivery.as_str())
        .bind(work_goal.get())
        .bind(work_criteria.get())
        .execute(&mut *transaction)
        .await
        .map_err(|source| WorkRepositoryError::persistence("select delivery branch CAS", source))?;
        if updated.rows_affected() != 1 {
            return Err(conflict(WorkDeliverySelectionBasisResource::WorkRevision));
        }
        (next, WorkDeliverySelectionOutcome::Selected)
    };
    super::events_repository::append_event_with_payload_hash(
        &mut transaction,
        &NewWorkEvent {
            owner_id: selection.owner_id.clone(),
            work_id: selection.work_id.clone(),
            branch_id: Some(selection.branch_id.clone()),
            kind: WorkEventKind::DeliveryBranchSelected,
            work_revision: Some(result_work_revision),
            goal_revision: Some(branch_goal),
            criterion_set_revision: Some(branch_criteria),
            branch_revision: Some(branch_revision),
            graph_revision: Some(graph_revision),
            source_ref: selection.request_id.clone(),
        },
        Some(&selection_payload_hash),
    )
    .await?;
    transaction
        .commit()
        .await
        .map_err(|source| WorkRepositoryError::persistence("commit delivery selection", source))?;
    Ok(WorkDeliverySelectionReceipt {
        schema_version: super::WORK_DELIVERY_SELECTION_SCHEMA_VERSION,
        work_id: selection.work_id,
        request_id: selection.request_id,
        delivery_branch_id: selection.branch_id,
        work_revision: result_work_revision,
        branch_revision,
        graph_revision,
        evidence_manifest_hash: selection.expected_evidence_manifest_hash,
        outcome,
    })
}
