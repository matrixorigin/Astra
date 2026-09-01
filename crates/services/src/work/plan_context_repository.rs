use super::repository::{DatabaseWorkRepository, WorkRepositoryError};
use super::{
    CriterionSetRevision, GoalRevision, GraphRevision, InternalSessionId, WorkBranchId,
    WorkBranchRevision, WorkBranchRuntimeBinding, WorkCheckFreshness, WorkContentHash, WorkGoal,
    WorkId, WorkItemCheckFact, WorkItemDeclarationState, WorkItemExecution,
    WorkItemExecutionRunRef, WorkItemExecutionStatus, WorkItemId, WorkItemKind, WorkItemRevision,
    WorkItemRevisionRef, WorkItemText, WorkItemVerification, WorkOwnerId, WorkPlanBasis,
    WorkPlanContext, WorkPlanItem, WorkRevision, WorkSessionPlanBinding, WorkTaskGraphPage,
    WorkTaskGraphQuery,
};
use chrono::Utc;
use sqlx::{MySql, QueryBuilder, Row, Transaction, query};
use std::collections::BTreeMap;

pub(super) async fn load_session_plan_binding(
    repository: &DatabaseWorkRepository,
    owner_id: &WorkOwnerId,
    session_id: &InternalSessionId,
) -> Result<WorkSessionPlanBinding, WorkRepositoryError> {
    let row = query(
        "SELECT b.work_id, b.branch_id, b.current_graph_revision
         FROM work_branches b
         JOIN works w
           ON w.owner_id = b.owner_id AND w.work_id = b.work_id
         WHERE b.owner_id = ? AND b.session_id = ?
           AND b.archived_at IS NULL AND b.deletion_operation_id IS NULL
           AND w.archived_at IS NULL
         LIMIT 1",
    )
    .bind(owner_id.as_str())
    .bind(session_id.as_str())
    .fetch_optional(repository.pool.get())
    .await
    .map_err(|source| WorkRepositoryError::persistence("load session Work binding", source))?
    .ok_or(WorkRepositoryError::NotFound)?;
    let string = |field: &'static str| {
        row.try_get::<String, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("Work session binding", source))
    };
    Ok(WorkSessionPlanBinding {
        work_id: WorkId::parse(string("work_id")?)
            .map_err(|source| WorkRepositoryError::corrupt("Work session binding", source))?,
        branch_id: WorkBranchId::parse(string("branch_id")?)
            .map_err(|source| WorkRepositoryError::corrupt("Work session binding", source))?,
        graph_revision: GraphRevision::new(
            row.try_get::<i64, _>("current_graph_revision")
                .map_err(|source| WorkRepositoryError::corrupt("Work session binding", source))?,
        )
        .map_err(|source| WorkRepositoryError::corrupt("Work session binding", source))?,
    })
}

pub(super) async fn load_branch_runtime_binding(
    repository: &DatabaseWorkRepository,
    owner_id: &WorkOwnerId,
    work_id: &WorkId,
    branch_id: &WorkBranchId,
) -> Result<WorkBranchRuntimeBinding, WorkRepositoryError> {
    let row = query(
        "SELECT b.session_id, b.branch_revision, b.current_graph_revision,
                b.deletion_operation_id
         FROM work_branches b
         JOIN works w
           ON w.owner_id = b.owner_id AND w.work_id = b.work_id
         WHERE b.owner_id = ? AND b.work_id = ? AND b.branch_id = ?
           AND b.archived_at IS NULL AND w.archived_at IS NULL
         LIMIT 1",
    )
    .bind(owner_id.as_str())
    .bind(work_id.as_str())
    .bind(branch_id.as_str())
    .fetch_optional(repository.pool.get())
    .await
    .map_err(|source| WorkRepositoryError::persistence("load Work branch runtime binding", source))?
    .ok_or(WorkRepositoryError::NotFound)?;
    if row
        .try_get::<Option<String>, _>("deletion_operation_id")
        .map_err(|source| WorkRepositoryError::corrupt("Work branch runtime binding", source))?
        .is_some()
    {
        return Err(WorkRepositoryError::BranchDeleting);
    }
    let session_id = row
        .try_get::<String, _>("session_id")
        .map_err(|source| WorkRepositoryError::corrupt("Work branch runtime binding", source))?;
    Ok(WorkBranchRuntimeBinding {
        work_id: work_id.clone(),
        branch_id: branch_id.clone(),
        branch_revision: WorkBranchRevision::new(
            row.try_get::<i64, _>("branch_revision").map_err(|source| {
                WorkRepositoryError::corrupt("Work branch runtime binding", source)
            })?,
        )
        .map_err(|source| WorkRepositoryError::corrupt("Work branch runtime binding", source))?,
        session_id: InternalSessionId::parse(session_id).map_err(|source| {
            WorkRepositoryError::corrupt("Work branch runtime binding", source)
        })?,
        graph_revision: GraphRevision::new(
            row.try_get::<i64, _>("current_graph_revision")
                .map_err(|source| {
                    WorkRepositoryError::corrupt("Work branch runtime binding", source)
                })?,
        )
        .map_err(|source| WorkRepositoryError::corrupt("Work branch runtime binding", source))?,
    })
}

pub(super) async fn load_session_item_runtime_binding(
    repository: &DatabaseWorkRepository,
    owner_id: &WorkOwnerId,
    session_id: &InternalSessionId,
    work_id: &WorkId,
    branch_id: &WorkBranchId,
    item: &WorkItemRevisionRef,
) -> Result<WorkSessionPlanBinding, WorkRepositoryError> {
    // Runtime admission is on the delegation path, so keep this to bounded
    // indexed reads. The previous four-table join was logically correct but
    // hit a MatrixOne column-remapping failure in production. More
    // importantly, no join is needed to establish this authority: the
    // session-bound branch chooses the immutable graph revision, and the
    // graph plus item rows are independently primary-key-addressed facts.
    let binding = load_session_plan_binding(repository, owner_id, session_id).await?;
    if binding.work_id != *work_id || binding.branch_id != *branch_id {
        return Err(WorkRepositoryError::NotFound);
    }

    let graph_row = query(
        "SELECT item_revision_manifest_json, item_count,
                edge_manifest_json, edge_count, manifest_hash
         FROM work_graph_revisions
         WHERE owner_id = ? AND work_id = ? AND revision = ?
         LIMIT 1",
    )
    .bind(owner_id.as_str())
    .bind(work_id.as_str())
    .bind(binding.graph_revision.get())
    .fetch_optional(repository.pool.get())
    .await
    .map_err(|source| WorkRepositoryError::persistence("load WorkItem graph binding", source))?
    .ok_or(WorkRepositoryError::NotFound)?;

    let item_row = query(
        "SELECT declaration_state
         FROM work_item_revisions
         WHERE owner_id = ? AND work_id = ? AND item_id = ? AND revision = ?
         LIMIT 1",
    )
    .bind(owner_id.as_str())
    .bind(work_id.as_str())
    .bind(item.item_id.as_str())
    .bind(item.revision.get())
    .fetch_optional(repository.pool.get())
    .await
    .map_err(|source| {
        WorkRepositoryError::persistence("load WorkItem declaration binding", source)
    })?
    .ok_or(WorkRepositoryError::NotFound)?;
    let graph = super::graph_repository::decode_persisted_graph(
        &graph_row
            .try_get::<String, _>("item_revision_manifest_json")
            .map_err(|source| WorkRepositoryError::corrupt("WorkItem run binding", source))?,
        graph_row
            .try_get("item_count")
            .map_err(|source| WorkRepositoryError::corrupt("WorkItem run binding", source))?,
        &graph_row
            .try_get::<String, _>("edge_manifest_json")
            .map_err(|source| WorkRepositoryError::corrupt("WorkItem run binding", source))?,
        graph_row
            .try_get("edge_count")
            .map_err(|source| WorkRepositoryError::corrupt("WorkItem run binding", source))?,
    )?;
    super::graph_repository::validate_persisted_graph_hash(
        &graph,
        &graph_row
            .try_get::<String, _>("manifest_hash")
            .map_err(|source| WorkRepositoryError::corrupt("WorkItem run binding", source))?,
    )?;
    if graph.item_refs.binary_search(item).is_err() {
        return Err(WorkRepositoryError::NotFound);
    }
    let declaration_state = item_row
        .try_get::<String, _>("declaration_state")
        .map_err(|source| WorkRepositoryError::corrupt("WorkItem run binding", source))?;
    if WorkItemDeclarationState::from_persisted(&declaration_state)
        != Some(WorkItemDeclarationState::Active)
    {
        return Err(WorkRepositoryError::NotFound);
    }
    Ok(binding)
}

fn bounded_count(
    entity: &'static str,
    value: i32,
    maximum: usize,
) -> Result<u16, WorkRepositoryError> {
    let value =
        u16::try_from(value).map_err(|source| WorkRepositoryError::corrupt(entity, source))?;
    if usize::from(value) > maximum {
        return Err(WorkRepositoryError::corrupt(
            entity,
            std::io::Error::other(format!("count {value} exceeds bound {maximum}")),
        ));
    }
    Ok(value)
}

fn parsed_hash(
    entity: &'static str,
    value: String,
) -> Result<WorkContentHash, WorkRepositoryError> {
    WorkContentHash::parse(value)
        .map_err(|message| WorkRepositoryError::corrupt(entity, std::io::Error::other(message)))
}

enum WorkPlanLookup<'a> {
    Session(&'a InternalSessionId),
    Branch {
        work_id: &'a WorkId,
        branch_id: &'a WorkBranchId,
    },
}

async fn load_basis_and_graph(
    transaction: &mut Transaction<'_, MySql>,
    owner_id: &WorkOwnerId,
    lookup: WorkPlanLookup<'_>,
) -> Result<(WorkPlanBasis, super::graph_repository::PersistedGraph), WorkRepositoryError> {
    let mut builder = QueryBuilder::<MySql>::new(
        "SELECT w.work_id, w.work_revision,
                w.current_goal_revision, w.current_criteria_set_revision,
                CASE WHEN w.archived_at IS NULL THEN 0 ELSE 1 END AS work_archived,
                g.revision AS goal_revision, g.goal_text,
                cs.revision AS criteria_set_revision,
                cs.member_count AS criteria_member_count,
                cs.member_manifest_hash AS criteria_manifest_hash,
                b.branch_id, b.branch_revision,
                b.goal_revision_ref, b.criteria_set_revision_ref,
                b.basis_graph_revision, b.current_graph_revision,
                CASE WHEN b.archived_at IS NULL THEN 0 ELSE 1 END AS branch_archived,
                gr.revision AS graph_revision,
                gr.item_revision_manifest_json, gr.item_count,
                gr.edge_manifest_json, gr.edge_count, gr.manifest_hash
         FROM work_branches b
         JOIN works w
           ON w.owner_id = b.owner_id AND w.work_id = b.work_id
         LEFT JOIN work_goal_revisions g
           ON g.owner_id = w.owner_id AND g.work_id = w.work_id
          AND g.revision = w.current_goal_revision
         LEFT JOIN work_criterion_sets cs
           ON cs.owner_id = w.owner_id AND cs.work_id = w.work_id
          AND cs.revision = w.current_criteria_set_revision
         LEFT JOIN work_graph_revisions gr
           ON gr.owner_id = b.owner_id AND gr.work_id = b.work_id
          AND gr.revision = b.current_graph_revision
         WHERE b.owner_id = ",
    );
    builder.push_bind(owner_id.as_str()).push(" AND ");
    match lookup {
        WorkPlanLookup::Session(session_id) => {
            builder
                .push("b.session_id = ")
                .push_bind(session_id.as_str());
        }
        WorkPlanLookup::Branch { work_id, branch_id } => {
            builder
                .push("b.work_id = ")
                .push_bind(work_id.as_str())
                .push(" AND b.branch_id = ")
                .push_bind(branch_id.as_str());
        }
    }
    builder.push(" LIMIT 1");
    let row = builder
        .build()
        .fetch_optional(&mut **transaction)
        .await
        .map_err(|source| WorkRepositoryError::persistence("load Work plan basis", source))?
        .ok_or(WorkRepositoryError::NotFound)?;
    let integer = |field: &'static str| {
        row.try_get::<i64, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("Work plan basis", source))
    };
    let string = |field: &'static str| {
        row.try_get::<String, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("Work plan basis", source))
    };
    if integer("work_archived")? != 0 || integer("branch_archived")? != 0 {
        return Err(WorkRepositoryError::Archived);
    }
    let work_revision = WorkRevision::new(integer("work_revision")?)
        .map_err(|source| WorkRepositoryError::corrupt("Work plan basis", source))?;
    let goal_revision = GoalRevision::new(integer("current_goal_revision")?)
        .map_err(|source| WorkRepositoryError::corrupt("Work plan basis", source))?;
    let materialized_goal_revision = GoalRevision::new(integer("goal_revision")?)
        .map_err(|source| WorkRepositoryError::corrupt("Work plan basis", source))?;
    if materialized_goal_revision != goal_revision {
        return Err(WorkRepositoryError::corrupt(
            "Work plan basis",
            std::io::Error::other("current Goal revision is missing or incoherent"),
        ));
    }
    let criteria_set_revision =
        CriterionSetRevision::new(integer("current_criteria_set_revision")?)
            .map_err(|source| WorkRepositoryError::corrupt("Work plan basis", source))?;
    let materialized_criteria_set_revision =
        CriterionSetRevision::new(integer("criteria_set_revision")?)
            .map_err(|source| WorkRepositoryError::corrupt("Work plan basis", source))?;
    if materialized_criteria_set_revision != criteria_set_revision {
        return Err(WorkRepositoryError::corrupt(
            "Work plan basis",
            std::io::Error::other("current criterion-set revision is missing or incoherent"),
        ));
    }
    let branch_goal_revision = GoalRevision::new(integer("goal_revision_ref")?)
        .map_err(|source| WorkRepositoryError::corrupt("Work plan basis", source))?;
    if branch_goal_revision > goal_revision {
        return Err(WorkRepositoryError::corrupt(
            "Work plan basis",
            std::io::Error::other("branch Goal revision is ahead of current Work Goal"),
        ));
    }
    let branch_criteria_set_revision =
        CriterionSetRevision::new(integer("criteria_set_revision_ref")?)
            .map_err(|source| WorkRepositoryError::corrupt("Work plan basis", source))?;
    if branch_criteria_set_revision > criteria_set_revision {
        return Err(WorkRepositoryError::corrupt(
            "Work plan basis",
            std::io::Error::other(
                "branch criterion-set revision is ahead of current Work criteria",
            ),
        ));
    }
    let branch_basis_graph_revision = GraphRevision::new(integer("basis_graph_revision")?)
        .map_err(|source| WorkRepositoryError::corrupt("Work plan basis", source))?;
    let graph_revision = GraphRevision::new(integer("current_graph_revision")?)
        .map_err(|source| WorkRepositoryError::corrupt("Work plan basis", source))?;
    if graph_revision < branch_basis_graph_revision {
        return Err(WorkRepositoryError::corrupt(
            "Work plan basis",
            std::io::Error::other("current graph revision precedes the branch basis"),
        ));
    }
    let materialized_graph_revision = GraphRevision::new(integer("graph_revision")?)
        .map_err(|source| WorkRepositoryError::corrupt("Work plan basis", source))?;
    if materialized_graph_revision != graph_revision {
        return Err(WorkRepositoryError::corrupt(
            "Work plan basis",
            std::io::Error::other("current graph revision is missing or incoherent"),
        ));
    }
    let graph = super::graph_repository::decode_persisted_graph(
        &string("item_revision_manifest_json")?,
        row.try_get("item_count")
            .map_err(|source| WorkRepositoryError::corrupt("Work plan graph", source))?,
        &string("edge_manifest_json")?,
        row.try_get("edge_count")
            .map_err(|source| WorkRepositoryError::corrupt("Work plan graph", source))?,
    )?;
    let graph_manifest_hash = string("manifest_hash")?;
    super::graph_repository::validate_persisted_graph_hash(&graph, &graph_manifest_hash)?;
    let basis = WorkPlanBasis {
        work_id: WorkId::parse(string("work_id")?)
            .map_err(|source| WorkRepositoryError::corrupt("Work plan basis", source))?,
        work_revision,
        goal_revision,
        goal: WorkGoal::parse(string("goal_text")?)
            .map_err(|source| WorkRepositoryError::corrupt("Work plan basis", source))?,
        criteria_set_revision,
        criteria_member_count: bounded_count(
            "Work plan criteria",
            row.try_get("criteria_member_count")
                .map_err(|source| WorkRepositoryError::corrupt("Work plan basis", source))?,
            super::criteria::CRITERION_SET_MAX_MEMBERS,
        )?,
        criteria_manifest_hash: parsed_hash(
            "Work plan criteria",
            string("criteria_manifest_hash")?,
        )?,
        branch_id: WorkBranchId::parse(string("branch_id")?)
            .map_err(|source| WorkRepositoryError::corrupt("Work plan basis", source))?,
        branch_revision: WorkBranchRevision::new(integer("branch_revision")?)
            .map_err(|source| WorkRepositoryError::corrupt("Work plan basis", source))?,
        branch_goal_revision,
        branch_criteria_set_revision,
        branch_basis_graph_revision,
        graph_revision,
        graph_item_count: bounded_count(
            "Work plan graph",
            row.try_get("item_count")
                .map_err(|source| WorkRepositoryError::corrupt("Work plan graph", source))?,
            super::graph::WORK_GRAPH_MAX_ITEMS,
        )?,
        graph_edge_count: bounded_count(
            "Work plan graph",
            row.try_get("edge_count")
                .map_err(|source| WorkRepositoryError::corrupt("Work plan graph", source))?,
            super::graph::WORK_GRAPH_MAX_EDGES,
        )?,
        graph_manifest_hash: parsed_hash("Work plan graph", graph_manifest_hash)?,
    };
    Ok((basis, graph))
}

async fn load_graph_items(
    transaction: &mut Transaction<'_, MySql>,
    owner_id: &WorkOwnerId,
    work_id: &WorkId,
    expected: &[WorkItemRevisionRef],
) -> Result<Vec<WorkPlanItem>, WorkRepositoryError> {
    if expected.is_empty() {
        return Ok(Vec::new());
    }
    let mut builder = QueryBuilder::<MySql>::new(
        "SELECT item_id, revision, item_kind, objective, expected_result, declaration_state
         FROM work_item_revisions WHERE owner_id = ",
    );
    builder
        .push_bind(owner_id.as_str())
        .push(" AND work_id = ")
        .push_bind(work_id.as_str())
        .push(" AND (");
    for (index, reference) in expected.iter().enumerate() {
        if index > 0 {
            builder.push(" OR ");
        }
        builder
            .push("(item_id = ")
            .push_bind(reference.item_id.as_str())
            .push(" AND revision = ")
            .push_bind(reference.revision.get())
            .push(")");
    }
    builder.push(")");
    let rows = builder
        .build()
        .fetch_all(&mut **transaction)
        .await
        .map_err(|source| WorkRepositoryError::persistence("load Work plan items", source))?;
    let mut items = BTreeMap::new();
    for row in rows {
        let item_id = WorkItemId::parse(
            row.try_get::<String, _>("item_id")
                .map_err(|source| WorkRepositoryError::corrupt("Work plan item", source))?,
        )
        .map_err(|source| WorkRepositoryError::corrupt("Work plan item", source))?;
        let revision = WorkItemRevision::new(
            row.try_get::<i64, _>("revision")
                .map_err(|source| WorkRepositoryError::corrupt("Work plan item", source))?,
        )
        .map_err(|source| WorkRepositoryError::corrupt("Work plan item", source))?;
        let persisted = |field: &'static str| {
            row.try_get::<String, _>(field)
                .map_err(|source| WorkRepositoryError::corrupt("Work plan item", source))
        };
        let kind = WorkItemKind::from_persisted(&persisted("item_kind")?).ok_or_else(|| {
            WorkRepositoryError::corrupt(
                "Work plan item",
                std::io::Error::other("unknown WorkItem kind"),
            )
        })?;
        let declaration_state = WorkItemDeclarationState::from_persisted(&persisted(
            "declaration_state",
        )?)
        .ok_or_else(|| {
            WorkRepositoryError::corrupt(
                "Work plan item",
                std::io::Error::other("unknown WorkItem declaration state"),
            )
        })?;
        let text = |field: &'static str| {
            WorkItemText::parse(persisted(field)?)
                .map_err(|source| WorkRepositoryError::corrupt("Work plan item", source))
        };
        let key = (item_id.as_str().to_string(), revision.get());
        if items
            .insert(
                key,
                WorkPlanItem {
                    item_id,
                    revision,
                    kind,
                    objective: text("objective")?,
                    expected_result: text("expected_result")?,
                    declaration_state,
                },
            )
            .is_some()
        {
            return Err(WorkRepositoryError::corrupt(
                "Work plan item",
                std::io::Error::other("duplicate item revision returned by primary-key query"),
            ));
        }
    }
    let missing = expected
        .iter()
        .filter(|reference| {
            !items.contains_key(&(
                reference.item_id.as_str().to_string(),
                reference.revision.get(),
            ))
        })
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(WorkRepositoryError::MissingWorkItemRevisions { missing });
    }
    expected
        .iter()
        .map(|reference| {
            items
                .remove(&(
                    reference.item_id.as_str().to_string(),
                    reference.revision.get(),
                ))
                .ok_or_else(|| {
                    WorkRepositoryError::corrupt(
                        "Work plan item",
                        std::io::Error::other("validated item revision disappeared from snapshot"),
                    )
                })
        })
        .collect()
}

pub(super) async fn load_plan_context_for_session(
    repository: &DatabaseWorkRepository,
    owner_id: &WorkOwnerId,
    session_id: &InternalSessionId,
) -> Result<WorkPlanContext, WorkRepositoryError> {
    let mut transaction = repository.pool.get().begin().await.map_err(|source| {
        WorkRepositoryError::persistence("begin Work plan context snapshot", source)
    })?;
    // The first statement pins exact immutable revisions. Later reads use
    // those identities, so correctness does not depend on transaction-level
    // repeatable-read defaults and a concurrent branch advance cannot tear the
    // returned context.
    let (basis, graph) = load_basis_and_graph(
        &mut transaction,
        owner_id,
        WorkPlanLookup::Session(session_id),
    )
    .await?;
    let items =
        load_graph_items(&mut transaction, owner_id, &basis.work_id, &graph.item_refs).await?;
    let context = WorkPlanContext::from_parts(basis, items, graph.edges)?;
    transaction.commit().await.map_err(|source| {
        WorkRepositoryError::persistence("commit Work plan context snapshot", source)
    })?;
    Ok(context)
}

/// Load one coherent execution cut for the durable Work coordinator.
///
/// This is deliberately separate from the public paged Task Graph endpoint:
/// foreground admission must reason over every declared dependency in one
/// transaction, while UI readers remain bounded to small pages.  The Work
/// graph itself is capped at 256 items, and execution reconciliation uses the
/// same exact-prefix latest-run query as task-board pages rather than scanning
/// a session's run history.
pub(super) async fn load_task_execution_snapshot_for_session(
    repository: &DatabaseWorkRepository,
    owner_id: &WorkOwnerId,
    session_id: &InternalSessionId,
) -> Result<super::WorkTaskExecutionSnapshot, WorkRepositoryError> {
    let mut transaction = repository.pool.get().begin().await.map_err(|source| {
        WorkRepositoryError::persistence("begin Work task execution snapshot", source)
    })?;
    let snapshot =
        load_task_execution_snapshot_in_transaction(&mut transaction, owner_id, session_id).await?;
    transaction.commit().await.map_err(|source| {
        WorkRepositoryError::persistence("commit Work task execution snapshot", source)
    })?;
    Ok(snapshot)
}

/// Load the same coherent coordinator cut inside a caller-owned transaction.
/// Settlement uses this seam after locking the Work branch so advancing to a
/// successor attempt cannot observe a mixed graph/delivery revision.
pub(super) async fn load_task_execution_snapshot_in_transaction(
    transaction: &mut Transaction<'_, MySql>,
    owner_id: &WorkOwnerId,
    session_id: &InternalSessionId,
) -> Result<super::WorkTaskExecutionSnapshot, WorkRepositoryError> {
    let (basis, graph) =
        load_basis_and_graph(transaction, owner_id, WorkPlanLookup::Session(session_id)).await?;
    let items = load_graph_items(transaction, owner_id, &basis.work_id, &graph.item_refs).await?;
    let (executions, deliveries) = load_item_executions(
        transaction,
        owner_id,
        &basis.work_id,
        &basis.branch_id,
        &graph.item_refs,
    )
    .await?;
    let snapshot = super::WorkTaskExecutionSnapshot::from_parts(
        basis,
        items,
        executions,
        deliveries,
        graph.edges,
    )?;
    Ok(snapshot)
}

fn decode_execution_status(status: &str) -> Result<WorkItemExecutionStatus, WorkRepositoryError> {
    match status {
        "running" => Ok(WorkItemExecutionStatus::Running),
        "waiting" => Ok(WorkItemExecutionStatus::Waiting),
        "paused" => Ok(WorkItemExecutionStatus::Paused),
        "completed" => Ok(WorkItemExecutionStatus::Completed),
        "delegated" => Ok(WorkItemExecutionStatus::Delegated),
        "failed" => Ok(WorkItemExecutionStatus::Failed),
        "cancelled" => Ok(WorkItemExecutionStatus::Cancelled),
        _ => Err(WorkRepositoryError::corrupt(
            "WorkItem execution projection",
            std::io::Error::other("unknown WorkItem attempt status"),
        )),
    }
}

async fn load_item_executions(
    transaction: &mut Transaction<'_, MySql>,
    owner_id: &WorkOwnerId,
    work_id: &WorkId,
    branch_id: &WorkBranchId,
    items: &[WorkItemRevisionRef],
) -> Result<
    (
        BTreeMap<WorkItemRevisionRef, WorkItemExecution>,
        BTreeMap<WorkItemRevisionRef, super::WorkItemDelivery>,
    ),
    WorkRepositoryError,
> {
    if items.is_empty() {
        return Ok((BTreeMap::new(), BTreeMap::new()));
    }

    // One bounded page contains at most eight items. A UNION of exact-prefix,
    // LIMIT-1 index seeks avoids both N+1 round trips and a history-sized
    // GROUP BY/window scan for long-lived, multi-attempt WorkItems.
    let mut builder = QueryBuilder::<MySql>::new(
        "SELECT executor_run_id, execution_mode, status, graph_revision, terminal_graph_revision,
                terminal_control_epoch,
                work_item_id, work_item_revision,
                attempt_id, run_generation, last_event_idx, updated_at,
                delivery_outcome, delivery_summary, delivery_blocker_kind,
                delivery_unavailable_capabilities_json
         FROM (",
    );
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            builder.push(" UNION ALL ");
        }
        builder
            .push(
                "(SELECT a.executor_run_id, a.execution_mode, a.status, a.graph_revision,
                           c.graph_revision AS terminal_graph_revision,
                           c.control_epoch AS terminal_control_epoch, a.work_item_id,
                           a.work_item_revision, a.attempt_id, a.run_generation,
                           a.last_event_idx,
                           DATE_FORMAT(a.updated_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS updated_at,
                           a.outcome AS delivery_outcome,
                           a.summary_text AS delivery_summary,
                           a.blocker_kind AS delivery_blocker_kind,
                           a.unavailable_capabilities_json AS delivery_unavailable_capabilities_json
                    FROM work_item_attempts a
                    LEFT JOIN work_terminal_cuts c
                      ON c.owner_id = a.owner_id
                     AND c.work_id = a.work_id
                     AND c.branch_id = a.branch_id
                     AND c.attempt_id = a.attempt_id
                    WHERE a.owner_id = ",
            )
            .push_bind(owner_id.as_str())
            .push(" AND a.work_id = ")
            .push_bind(work_id.as_str())
            .push(" AND a.branch_id = ")
            .push_bind(branch_id.as_str())
            .push(" AND a.work_item_id = ")
            .push_bind(item.item_id.as_str())
            .push(" AND a.work_item_revision = ")
            .push_bind(item.revision.get())
            .push(" ORDER BY a.started_at DESC, a.attempt_id DESC LIMIT 1)");
    }
    builder.push(") AS latest_item_runs");
    let rows = builder
        .build()
        .fetch_all(&mut **transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::persistence("load WorkItem execution projection", source)
        })?;

    let mut executions = BTreeMap::new();
    let mut deliveries = BTreeMap::new();
    for row in rows {
        let text = |field: &'static str| {
            row.try_get::<String, _>(field).map_err(|source| {
                WorkRepositoryError::corrupt("WorkItem execution projection", source)
            })
        };
        let item_ref = WorkItemRevisionRef {
            item_id: WorkItemId::parse(text("work_item_id")?).map_err(|source| {
                WorkRepositoryError::corrupt("WorkItem execution projection", source)
            })?,
            revision: WorkItemRevision::new(row.try_get("work_item_revision").map_err(
                |source| WorkRepositoryError::corrupt("WorkItem execution projection", source),
            )?)
            .map_err(|source| {
                WorkRepositoryError::corrupt("WorkItem execution projection", source)
            })?,
        };
        let run_id = text("executor_run_id")?;
        let attempt_id =
            super::WorkItemAttemptId::parse(text("attempt_id")?).map_err(|source| {
                WorkRepositoryError::corrupt("WorkItem execution projection", source)
            })?;
        let run_generation = row.try_get::<i64, _>("run_generation").map_err(|source| {
            WorkRepositoryError::corrupt("WorkItem execution projection", source)
        })?;
        let run_generation = u64::try_from(run_generation).map_err(|source| {
            WorkRepositoryError::corrupt("WorkItem execution projection", source)
        })?;
        let last_event_idx = row.try_get::<i64, _>("last_event_idx").map_err(|source| {
            WorkRepositoryError::corrupt("WorkItem execution projection", source)
        })?;
        if last_event_idx < -1 {
            return Err(WorkRepositoryError::corrupt(
                "WorkItem execution projection",
                std::io::Error::other("Run event cursor is below its initial value"),
            ));
        }
        let status = decode_execution_status(&text("status")?)?;
        let execution_mode = super::WorkAttemptExecutionMode::from_persisted(&text(
            "execution_mode",
        )?)
        .ok_or_else(|| {
            WorkRepositoryError::corrupt(
                "WorkItem execution projection",
                std::io::Error::other("unknown WorkItem attempt execution mode"),
            )
        })?;
        let terminal_graph_revision = row
            .try_get::<Option<i64>, _>("terminal_graph_revision")
            .map_err(|source| {
                WorkRepositoryError::corrupt("WorkItem execution projection", source)
            })?
            .map(GraphRevision::new)
            .transpose()
            .map_err(|source| {
                WorkRepositoryError::corrupt("WorkItem execution projection", source)
            })?;
        let terminal_control_epoch = row
            .try_get::<Option<i64>, _>("terminal_control_epoch")
            .map_err(|source| {
                WorkRepositoryError::corrupt("WorkItem execution projection", source)
            })?;
        let terminal_cut = match (terminal_graph_revision, terminal_control_epoch) {
            (None, None) => None,
            (Some(graph_revision), Some(control_epoch)) => Some(
                super::WorkAttemptTerminalCut::new(graph_revision, control_epoch).ok_or_else(
                    || {
                        WorkRepositoryError::corrupt(
                            "WorkItem execution projection",
                            std::io::Error::other(
                                "terminal control epoch is below its initial value",
                            ),
                        )
                    },
                )?,
            ),
            _ => {
                return Err(WorkRepositoryError::corrupt(
                    "WorkItem execution projection",
                    std::io::Error::other("incomplete terminal Work cut"),
                ));
            }
        };
        let execution = WorkItemExecution::from_run(
            status,
            WorkItemExecutionRunRef {
                run_id,
                attempt_id,
                graph_revision: GraphRevision::new(row.try_get("graph_revision").map_err(
                    |source| WorkRepositoryError::corrupt("WorkItem execution projection", source),
                )?)
                .map_err(|source| {
                    WorkRepositoryError::corrupt("WorkItem execution projection", source)
                })?,
                terminal_cut,
                execution_mode,
                run_generation,
                last_event_idx,
                updated_at: super::repository::decode_timestamp(
                    "WorkItem execution projection",
                    "updated_at",
                    text("updated_at")?,
                )?,
            },
        );
        if executions.insert(item_ref.clone(), execution).is_some() {
            return Err(WorkRepositoryError::corrupt(
                "WorkItem execution projection",
                std::io::Error::other("multiple latest root Runs returned for one WorkItem"),
            ));
        }
        let optional_text = |field: &'static str| {
            row.try_get::<Option<String>, _>(field).map_err(|source| {
                WorkRepositoryError::corrupt("WorkItem delivery projection", source)
            })
        };
        if let Some(outcome) = optional_text("delivery_outcome")? {
            let status = match outcome.as_str() {
                "delivered" => super::WorkItemDeliveryStatus::Delivered,
                "blocked" => super::WorkItemDeliveryStatus::Blocked,
                "failed" => super::WorkItemDeliveryStatus::Failed,
                _ => {
                    return Err(WorkRepositoryError::corrupt(
                        "WorkItem delivery projection",
                        std::io::Error::other("unknown delivery outcome"),
                    ));
                }
            };
            let blocker_kind = optional_text("delivery_blocker_kind")?
                .map(|value| match value.as_str() {
                    "capability_unavailable" => {
                        Ok(super::WorkAttemptBlockerKind::CapabilityUnavailable)
                    }
                    "dependency_blocked" => Ok(super::WorkAttemptBlockerKind::DependencyBlocked),
                    "policy_blocked" => Ok(super::WorkAttemptBlockerKind::PolicyBlocked),
                    "external_unavailable" => {
                        Ok(super::WorkAttemptBlockerKind::ExternalUnavailable)
                    }
                    _ => Err(WorkRepositoryError::corrupt(
                        "WorkItem delivery projection",
                        std::io::Error::other("unknown delivery blocker kind"),
                    )),
                })
                .transpose()?;
            let unavailable_capabilities = serde_json::from_str::<Vec<String>>(
                &optional_text("delivery_unavailable_capabilities_json")?.ok_or_else(|| {
                    WorkRepositoryError::corrupt(
                        "WorkItem delivery projection",
                        std::io::Error::other("delivery capability facts are missing"),
                    )
                })?,
            )
            .map_err(|source| {
                WorkRepositoryError::corrupt("WorkItem delivery projection", source)
            })?;
            deliveries.insert(
                item_ref.clone(),
                super::WorkItemDelivery {
                    status,
                    summary: optional_text("delivery_summary")?,
                    blocker_kind,
                    unavailable_capabilities,
                },
            );
        }
    }
    Ok((executions, deliveries))
}

async fn load_item_verifications(
    transaction: &mut Transaction<'_, MySql>,
    owner_id: &WorkOwnerId,
    work_id: &WorkId,
    branch_id: &WorkBranchId,
    branch_criteria_set_revision: CriterionSetRevision,
    current_graph_revision: GraphRevision,
    executions: &BTreeMap<WorkItemRevisionRef, WorkItemExecution>,
) -> Result<BTreeMap<WorkItemRevisionRef, WorkItemVerification>, WorkRepositoryError> {
    let attempts = executions
        .iter()
        .filter_map(|(item, execution)| execution.run.as_ref().map(|run| (item, &run.attempt_id)))
        .collect::<Vec<_>>();
    if attempts.is_empty() {
        return Ok(BTreeMap::new());
    }

    // Like execution reconciliation, this is one bounded round trip composed
    // of exact-prefix latest-row seeks. Check history can grow without making
    // a Task Graph page scan or sort all prior attempts.
    let mut builder = QueryBuilder::<MySql>::new(
        "SELECT check_run_id, graph_revision, work_item_id, work_item_revision,
                work_item_attempt_id, criterion_set_revision, criterion_id,
                criterion_revision, subject_ref, subject_revision, verifier_kind,
                outcome, coverage_state, evidence_ref_count, produced_at, expires_at,
                current_subject_graph_revision, current_subject_ref,
                current_subject_revision
         FROM (",
    );
    for (index, (item, attempt_id)) in attempts.iter().enumerate() {
        if index > 0 {
            builder.push(" UNION ALL ");
        }
        builder
            .push(
                "(SELECT c.check_run_id, c.graph_revision, c.work_item_id,
                           c.work_item_revision, c.work_item_attempt_id,
                           c.criterion_set_revision, c.criterion_id, c.criterion_revision,
                           c.subject_ref, c.subject_revision, c.verifier_kind, c.outcome,
                           c.coverage_state, c.evidence_ref_count,
                           DATE_FORMAT(c.produced_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS produced_at,
                           DATE_FORMAT(c.expires_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS expires_at,
                           s.graph_revision AS current_subject_graph_revision,
                           s.subject_ref AS current_subject_ref,
                           s.subject_revision AS current_subject_revision
                    FROM work_check_runs c
                    LEFT JOIN work_branch_subjects s
                      ON s.owner_id = c.owner_id AND s.work_id = c.work_id
                     AND s.branch_id = c.branch_id
                    WHERE c.owner_id = ",
            )
            .push_bind(owner_id.as_str())
            .push(" AND c.work_id = ")
            .push_bind(work_id.as_str())
            .push(" AND c.branch_id = ")
            .push_bind(branch_id.as_str())
            .push(" AND c.work_item_id = ")
            .push_bind(item.item_id.as_str())
            .push(" AND c.work_item_revision = ")
            .push_bind(item.revision.get())
            .push(" AND c.work_item_attempt_id = ")
            .push_bind(attempt_id.as_str())
            .push(" ORDER BY c.produced_at DESC, c.check_run_id DESC LIMIT 1)");
    }
    builder.push(") AS latest_item_checks");
    let rows = builder
        .build()
        .fetch_all(&mut **transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::persistence("load WorkItem verification projection", source)
        })?;

    let now = Utc::now();
    let mut verifications = BTreeMap::new();
    for row in rows {
        let text = |field: &'static str| {
            row.try_get::<String, _>(field).map_err(|source| {
                WorkRepositoryError::corrupt("WorkItem verification projection", source)
            })
        };
        let optional_text = |field: &'static str| {
            row.try_get::<Option<String>, _>(field).map_err(|source| {
                WorkRepositoryError::corrupt("WorkItem verification projection", source)
            })
        };
        let item = WorkItemRevisionRef {
            item_id: WorkItemId::parse(text("work_item_id")?).map_err(|source| {
                WorkRepositoryError::corrupt("WorkItem verification projection", source)
            })?,
            revision: WorkItemRevision::new(row.try_get("work_item_revision").map_err(
                |source| WorkRepositoryError::corrupt("WorkItem verification projection", source),
            )?)
            .map_err(|source| {
                WorkRepositoryError::corrupt("WorkItem verification projection", source)
            })?,
        };
        let attempt_id =
            super::WorkItemAttemptId::parse(text("work_item_attempt_id")?).map_err(|source| {
                WorkRepositoryError::corrupt("WorkItem verification projection", source)
            })?;
        if executions
            .get(&item)
            .and_then(|execution| execution.run.as_ref())
            .is_none_or(|run| run.attempt_id != attempt_id)
        {
            return Err(WorkRepositoryError::corrupt(
                "WorkItem verification projection",
                std::io::Error::other("check does not belong to the projected item attempt"),
            ));
        }
        let check_graph_revision =
            GraphRevision::new(row.try_get("graph_revision").map_err(|source| {
                WorkRepositoryError::corrupt("WorkItem verification projection", source)
            })?)
            .map_err(|source| {
                WorkRepositoryError::corrupt("WorkItem verification projection", source)
            })?;
        let subject_ref = text("subject_ref")?;
        let subject_revision =
            WorkContentHash::parse(text("subject_revision")?).map_err(|message| {
                WorkRepositoryError::corrupt(
                    "WorkItem verification projection",
                    std::io::Error::other(message),
                )
            })?;
        let expires_at = optional_text("expires_at")?
            .map(|value| {
                super::repository::decode_timestamp(
                    "WorkItem verification projection",
                    "expires_at",
                    value,
                )
            })
            .transpose()?;
        let current_subject_graph = row
            .try_get::<Option<i64>, _>("current_subject_graph_revision")
            .map_err(|source| {
                WorkRepositoryError::corrupt("WorkItem verification projection", source)
            })?
            .map(GraphRevision::new)
            .transpose()
            .map_err(|source| {
                WorkRepositoryError::corrupt("WorkItem verification projection", source)
            })?;
        let current_subject_ref = optional_text("current_subject_ref")?;
        let current_subject_revision = optional_text("current_subject_revision")?;
        let check_criteria_set_revision =
            CriterionSetRevision::new(row.try_get("criterion_set_revision").map_err(|source| {
                WorkRepositoryError::corrupt("WorkItem verification projection", source)
            })?)
            .map_err(|source| {
                WorkRepositoryError::corrupt("WorkItem verification projection", source)
            })?;
        let freshness = if check_criteria_set_revision != branch_criteria_set_revision {
            WorkCheckFreshness::CriteriaChanged
        } else if check_graph_revision != current_graph_revision {
            WorkCheckFreshness::GraphChanged
        } else if current_subject_graph.is_none()
            || current_subject_ref.is_none()
            || current_subject_revision.is_none()
        {
            WorkCheckFreshness::SubjectUnavailable
        } else if current_subject_graph != Some(current_graph_revision) {
            WorkCheckFreshness::GraphChanged
        } else if current_subject_ref.as_deref() != Some(subject_ref.as_str())
            || current_subject_revision.as_deref() != Some(subject_revision.as_str())
        {
            WorkCheckFreshness::SubjectChanged
        } else if expires_at.is_some_and(|expires_at| expires_at <= now) {
            WorkCheckFreshness::Expired
        } else {
            WorkCheckFreshness::Current
        };
        let persisted_enum = |field: &'static str, value: String| {
            WorkRepositoryError::corrupt(
                "WorkItem verification projection",
                std::io::Error::other(format!("unknown persisted {field}: {value}")),
            )
        };
        let verifier = text("verifier_kind")?;
        let outcome = text("outcome")?;
        let coverage = text("coverage_state")?;
        let verifier_kind = super::CheckVerifierKind::from_persisted(&verifier)
            .ok_or_else(|| persisted_enum("verifier kind", verifier))?;
        let outcome = super::CheckOutcome::from_persisted(&outcome)
            .ok_or_else(|| persisted_enum("check outcome", outcome))?;
        let coverage = super::CheckCoverage::from_persisted(&coverage)
            .ok_or_else(|| persisted_enum("check coverage", coverage))?;
        let evidence_ref_count = row
            .try_get::<i32, _>("evidence_ref_count")
            .map_err(|source| {
                WorkRepositoryError::corrupt("WorkItem verification projection", source)
            })?;
        let evidence_ref_count = u16::try_from(evidence_ref_count).map_err(|source| {
            WorkRepositoryError::corrupt("WorkItem verification projection", source)
        })?;
        if (outcome == super::CheckOutcome::Passed
            && (coverage != super::CheckCoverage::Complete || evidence_ref_count == 0))
            || (outcome == super::CheckOutcome::Failed && evidence_ref_count == 0)
        {
            return Err(WorkRepositoryError::corrupt(
                "WorkItem verification projection",
                std::io::Error::other("persisted check outcome lacks coherent coverage evidence"),
            ));
        }
        let verification = WorkItemVerification::from_check(WorkItemCheckFact {
            check_run_id: super::CheckRunId::parse(text("check_run_id")?).map_err(|source| {
                WorkRepositoryError::corrupt("WorkItem verification projection", source)
            })?,
            criterion: super::CriterionRevisionRef {
                criterion_id: super::CriterionId::parse(text("criterion_id")?).map_err(
                    |source| {
                        WorkRepositoryError::corrupt("WorkItem verification projection", source)
                    },
                )?,
                revision: super::CriterionRevision::new(
                    row.try_get("criterion_revision").map_err(|source| {
                        WorkRepositoryError::corrupt("WorkItem verification projection", source)
                    })?,
                )
                .map_err(|source| {
                    WorkRepositoryError::corrupt("WorkItem verification projection", source)
                })?,
            },
            criterion_set_revision: check_criteria_set_revision,
            graph_revision: check_graph_revision,
            verifier_kind,
            outcome,
            coverage,
            subject_revision,
            evidence_ref_count,
            produced_at: super::repository::decode_timestamp(
                "WorkItem verification projection",
                "produced_at",
                text("produced_at")?,
            )?,
            expires_at,
            freshness,
        });
        if verifications.insert(item, verification).is_some() {
            return Err(WorkRepositoryError::corrupt(
                "WorkItem verification projection",
                std::io::Error::other("multiple latest checks returned for one WorkItem"),
            ));
        }
    }
    Ok(verifications)
}

pub(super) async fn load_task_graph_page(
    repository: &DatabaseWorkRepository,
    query: WorkTaskGraphQuery,
) -> Result<WorkTaskGraphPage, WorkRepositoryError> {
    let mut transaction = repository.pool.get().begin().await.map_err(|source| {
        WorkRepositoryError::persistence("begin Work Task Graph snapshot", source)
    })?;
    let (basis, graph) = load_basis_and_graph(
        &mut transaction,
        &query.owner_id,
        WorkPlanLookup::Branch {
            work_id: &query.work_id,
            branch_id: &query.branch_id,
        },
    )
    .await?;
    if let Some(expected) = query.expected_graph_revision
        && expected != basis.graph_revision
    {
        return Err(WorkRepositoryError::StaleTaskGraphRevision {
            expected_graph_revision: expected,
            actual_graph_revision: basis.graph_revision,
        });
    }
    let item_offset = usize::from(query.item_offset);
    let dependency_offset = usize::from(query.dependency_offset);
    if item_offset > graph.item_refs.len() || dependency_offset > graph.edges.len() {
        return Err(WorkRepositoryError::TaskGraphCursorAhead {
            item_offset: query.item_offset,
            item_count: basis.graph_item_count,
            dependency_offset: query.dependency_offset,
            dependency_count: basis.graph_edge_count,
        });
    }
    let item_end = item_offset
        .saturating_add(usize::from(query.item_limit))
        .min(graph.item_refs.len());
    let dependency_end = dependency_offset
        .saturating_add(usize::from(query.dependency_limit))
        .min(graph.edges.len());
    let items = load_graph_items(
        &mut transaction,
        &query.owner_id,
        &basis.work_id,
        &graph.item_refs[item_offset..item_end],
    )
    .await?;
    let (executions, deliveries) = load_item_executions(
        &mut transaction,
        &query.owner_id,
        &basis.work_id,
        &basis.branch_id,
        &graph.item_refs[item_offset..item_end],
    )
    .await?;
    let verifications = load_item_verifications(
        &mut transaction,
        &query.owner_id,
        &basis.work_id,
        &basis.branch_id,
        basis.branch_criteria_set_revision,
        basis.graph_revision,
        &executions,
    )
    .await?;
    let page = WorkTaskGraphPage::from_parts(
        basis,
        &query,
        items,
        executions,
        deliveries,
        verifications,
        graph.edges[dependency_offset..dependency_end].to_vec(),
    )?;
    transaction.commit().await.map_err(|source| {
        WorkRepositoryError::persistence("commit Work Task Graph snapshot", source)
    })?;
    Ok(page)
}
