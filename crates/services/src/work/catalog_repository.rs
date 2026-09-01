use super::catalog::{
    WorkBranchActivity, WorkCatalogAttention, WorkCatalogCursor, WorkCatalogEntry, WorkCatalogPage,
    WorkCatalogQuery,
};
use super::proposal::WORK_PROPOSAL_MAX_PENDING_PER_BRANCH;
use super::repository::{DatabaseWorkRepository, WorkRepositoryError};
use super::{
    GraphRevision, WorkBranchId, WorkBranchRevision, WorkEventSeq, WorkGoal, WorkId, WorkRevision,
};
use crate::runs::{DurableRunStatusKind, durable_run_status_kind};
use sqlx::{QueryBuilder, Row};

pub(super) async fn list_catalog(
    repository: &DatabaseWorkRepository,
    query_value: WorkCatalogQuery,
) -> Result<WorkCatalogPage, WorkRepositoryError> {
    let mut builder = QueryBuilder::new(
        "SELECT w.work_id, w.work_revision, w.delivery_branch_id, g.goal_text,
                b.branch_revision, b.current_graph_revision,
                b.session_id AS branch_session_id,
                gr.item_count AS graph_item_count,
                es.last_event_seq, r.seen_through_event_seq,
                slot.run_id AS active_run_id, active.status AS active_run_status,
                active.waiting_for AS active_run_waiting_for,
                active.session_id AS active_run_session_id,
                active.work_id AS active_run_work_id,
                active.work_branch_id AS active_run_branch_id,
                active.parent_run_id AS active_run_parent_id,
                (SELECT COUNT(*) FROM work_proposals p
                  WHERE p.owner_id = w.owner_id AND p.work_id = w.work_id
                    AND p.branch_id = w.delivery_branch_id
                    AND p.proposal_kind = 'criteria_set'
                    AND p.status = 'pending' AND p.expires_at > NOW(6))
                  AS pending_decision_count,
                DATE_FORMAT(w.created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS created_at,
                DATE_FORMAT(
                  CASE WHEN active.updated_at IS NOT NULL AND active.updated_at > es.updated_at
                       THEN active.updated_at ELSE es.updated_at END,
                  '%Y-%m-%dT%H:%i:%s.%fZ') AS last_activity_at
         FROM works w
         LEFT JOIN work_goal_revisions g
           ON g.owner_id = w.owner_id AND g.work_id = w.work_id
          AND g.revision = w.current_goal_revision
         LEFT JOIN work_branches b
           ON b.owner_id = w.owner_id AND b.work_id = w.work_id
          AND b.branch_id = w.delivery_branch_id AND b.archived_at IS NULL
         LEFT JOIN work_graph_revisions gr
           ON gr.owner_id = b.owner_id AND gr.work_id = b.work_id
          AND gr.revision = b.current_graph_revision
         LEFT JOIN work_event_sequences es
           ON es.owner_id = w.owner_id AND es.work_id = w.work_id
         LEFT JOIN work_attention_receipts r
           ON r.owner_id = w.owner_id AND r.work_id = w.work_id
         LEFT JOIN agent_session_execution_slots slot
           ON slot.user_id = w.owner_id AND slot.session_id = b.session_id
         LEFT JOIN agent_runs active
           ON active.user_id = slot.user_id AND active.run_id = slot.run_id
         WHERE w.owner_id = ",
    );
    builder.push_bind(query_value.owner_id.as_str());
    builder.push(" AND w.archived_at IS NULL");
    if let Some(cursor) = &query_value.before {
        builder
            .push(" AND (w.created_at < ")
            .push_bind(cursor.created_at.naive_utc())
            .push(" OR (w.created_at = ")
            .push_bind(cursor.created_at.naive_utc())
            .push(" AND w.work_id < ")
            .push_bind(cursor.work_id.as_str())
            .push("))");
    }
    builder
        .push(" ORDER BY w.created_at DESC, w.work_id DESC LIMIT ")
        .push_bind(i64::from(query_value.limit.get()) + 1);

    let rows = builder
        .build()
        .fetch_all(repository.pool.get())
        .await
        .map_err(|source| WorkRepositoryError::persistence("list Work catalog", source))?;
    let has_more = rows.len() > usize::from(query_value.limit.get());
    let mut entries = Vec::with_capacity(rows.len().min(usize::from(query_value.limit.get())));
    for row in rows.into_iter().take(usize::from(query_value.limit.get())) {
        entries.push(decode_entry(row)?);
    }
    let next_cursor = if has_more {
        entries.last().map(|entry| WorkCatalogCursor {
            created_at: entry.created_at,
            work_id: entry.work_id.clone(),
        })
    } else {
        None
    };
    Ok(WorkCatalogPage {
        entries,
        next_cursor,
    })
}

fn decode_entry(row: sqlx::mysql::MySqlRow) -> Result<WorkCatalogEntry, WorkRepositoryError> {
    let integer = |field: &'static str| {
        row.try_get::<i64, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("Work catalog entry", source))
    };
    let text = |field: &'static str| {
        row.try_get::<String, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("Work catalog entry", source))
    };
    let work_id = WorkId::parse(text("work_id")?)
        .map_err(|source| WorkRepositoryError::corrupt("Work catalog entry", source))?;
    let event_head = WorkEventSeq::new(integer("last_event_seq")?)
        .map_err(|source| WorkRepositoryError::corrupt("Work catalog entry", source))?;
    let seen_value = integer("seen_through_event_seq")?;
    let seen_through_event_seq = if seen_value == 0 {
        None
    } else {
        Some(
            WorkEventSeq::new(seen_value)
                .map_err(|source| WorkRepositoryError::corrupt("Work catalog entry", source))?,
        )
    };
    if seen_value > event_head.get() {
        return Err(WorkRepositoryError::corrupt(
            "Work catalog entry",
            std::io::Error::other("seen cursor is ahead of the Work event head"),
        ));
    }
    let pending_value = integer("pending_decision_count")?;
    let pending_decision_count = u16::try_from(pending_value)
        .map_err(|source| WorkRepositoryError::corrupt("Work catalog entry", source))?;
    if i64::from(pending_decision_count) > WORK_PROPOSAL_MAX_PENDING_PER_BRANCH {
        return Err(WorkRepositoryError::corrupt(
            "Work catalog entry",
            std::io::Error::other("pending decision count exceeds proposal admission capacity"),
        ));
    }
    let unseen_event_count = u64::try_from(event_head.get() - seen_value)
        .map_err(|source| WorkRepositoryError::corrupt("Work catalog entry", source))?;
    let attention = if pending_decision_count > 0 {
        WorkCatalogAttention::NeedsReview
    } else if unseen_event_count > 0 {
        WorkCatalogAttention::Updated
    } else {
        WorkCatalogAttention::None
    };
    let active_run_id = row
        .try_get::<Option<String>, _>("active_run_id")
        .map_err(|source| WorkRepositoryError::corrupt("Work catalog entry", source))?;
    let delivery_branch_activity = match active_run_id {
        None => WorkBranchActivity::Idle,
        Some(_) => decode_active_run(&row, &work_id)?,
    };
    Ok(WorkCatalogEntry {
        work_id,
        goal: WorkGoal::parse(text("goal_text")?)
            .map_err(|source| WorkRepositoryError::corrupt("Work catalog entry", source))?,
        work_revision: WorkRevision::new(integer("work_revision")?)
            .map_err(|source| WorkRepositoryError::corrupt("Work catalog entry", source))?,
        delivery_branch_id: WorkBranchId::parse(text("delivery_branch_id")?)
            .map_err(|source| WorkRepositoryError::corrupt("Work catalog entry", source))?,
        delivery_branch_revision: WorkBranchRevision::new(integer("branch_revision")?)
            .map_err(|source| WorkRepositoryError::corrupt("Work catalog entry", source))?,
        graph_revision: GraphRevision::new(integer("current_graph_revision")?)
            .map_err(|source| WorkRepositoryError::corrupt("Work catalog entry", source))?,
        graph_item_count: u16::try_from(integer("graph_item_count")?)
            .map_err(|source| WorkRepositoryError::corrupt("Work catalog entry", source))?,
        pending_decision_count,
        event_head,
        seen_through_event_seq,
        unseen_event_count,
        attention,
        delivery_branch_activity,
        created_at: super::repository::decode_timestamp(
            "Work catalog entry",
            "created_at",
            text("created_at")?,
        )?,
        last_activity_at: super::repository::decode_timestamp(
            "Work catalog entry",
            "last_activity_at",
            text("last_activity_at")?,
        )?,
    })
}

fn decode_active_run(
    row: &sqlx::mysql::MySqlRow,
    work_id: &WorkId,
) -> Result<WorkBranchActivity, WorkRepositoryError> {
    let required = |field: &'static str| {
        row.try_get::<Option<String>, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("Work catalog active run", source))?
            .ok_or_else(|| {
                WorkRepositoryError::corrupt(
                    "Work catalog active run",
                    std::io::Error::other(format!("execution slot has no {field}")),
                )
            })
    };
    if required("active_run_work_id")? != work_id.as_str()
        || required("active_run_branch_id")? != required("delivery_branch_id")?
        || required("active_run_session_id")? != required("branch_session_id")?
        || row
            .try_get::<Option<String>, _>("active_run_parent_id")
            .map_err(|source| WorkRepositoryError::corrupt("Work catalog active run", source))?
            .is_some()
    {
        return Err(WorkRepositoryError::corrupt(
            "Work catalog active run",
            std::io::Error::other("execution slot owner is not the Work delivery root run"),
        ));
    }
    let status = required("active_run_status")?;
    let waiting_for = row
        .try_get::<Option<String>, _>("active_run_waiting_for")
        .map_err(|source| WorkRepositoryError::corrupt("Work catalog active run", source))?;
    match (durable_run_status_kind(&status), waiting_for.is_some()) {
        (DurableRunStatusKind::Running, _) => Ok(WorkBranchActivity::Working),
        (DurableRunStatusKind::Waiting, _) => Ok(WorkBranchActivity::Waiting),
        (DurableRunStatusKind::Paused, true) => Ok(WorkBranchActivity::Paused),
        _ => Err(WorkRepositoryError::corrupt(
            "Work catalog active run",
            std::io::Error::other("execution slot and durable run status disagree"),
        )),
    }
}
