use super::events::{
    WorkEventCoverage, WorkEventKind, WorkEventPage, WorkEventQuery, WorkEventRecord, WorkEventSeq,
};
use super::repository::{DatabaseWorkRepository, WorkRepositoryError};
use super::{
    CriterionSetRevision, GoalRevision, GraphRevision, WorkBranchId, WorkBranchRevision,
    WorkChangeRef, WorkRevision,
};
use sqlx::{Row, query};

pub(super) async fn list_events(
    repository: &DatabaseWorkRepository,
    query_value: WorkEventQuery,
) -> Result<WorkEventPage, WorkRepositoryError> {
    let metadata = query(
        "SELECT s.last_event_seq, s.retained_from_event_seq,
                r.seen_through_event_seq
         FROM work_event_sequences s
         LEFT JOIN work_attention_receipts r
           ON r.owner_id = s.owner_id AND r.work_id = s.work_id
         WHERE s.owner_id = ? AND s.work_id = ? LIMIT 1",
    )
    .bind(query_value.owner_id.as_str())
    .bind(query_value.work_id.as_str())
    .fetch_optional(repository.pool.get())
    .await
    .map_err(|source| WorkRepositoryError::persistence("load Work event page metadata", source))?
    .ok_or(WorkRepositoryError::NotFound)?;
    let integer = |field: &'static str| {
        metadata
            .try_get::<i64, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("Work event page", source))
    };
    let event_head = WorkEventSeq::new(integer("last_event_seq")?)
        .map_err(|source| WorkRepositoryError::corrupt("Work event page", source))?;
    let retained_from_event_seq = WorkEventSeq::new(integer("retained_from_event_seq")?)
        .map_err(|source| WorkRepositoryError::corrupt("Work event page", source))?;
    let seen_value = metadata
        .try_get::<Option<i64>, _>("seen_through_event_seq")
        .map_err(|source| WorkRepositoryError::corrupt("Work attention receipt", source))?
        .ok_or_else(|| {
            WorkRepositoryError::corrupt(
                "Work attention receipt",
                std::io::Error::other("receipt is missing for an existing Work"),
            )
        })?;
    let seen_through_event_seq = decode_optional_event_seq(seen_value, "seen cursor")?;
    if seen_through_event_seq.is_some_and(|cursor| cursor > event_head) {
        return Err(WorkRepositoryError::corrupt(
            "Work attention receipt",
            std::io::Error::other("seen cursor is ahead of the event head"),
        ));
    }
    let requested_after = query_value
        .after_event_seq
        .map(WorkEventSeq::get)
        .unwrap_or(0);
    if requested_after > event_head.get() {
        return Err(WorkRepositoryError::EventCursorAhead {
            through_event_seq: requested_after,
            event_head: event_head.get(),
        });
    }
    let retained_after = retained_from_event_seq.get() - 1;
    let coverage = if requested_after < retained_after {
        WorkEventCoverage::Expired
    } else {
        WorkEventCoverage::Complete
    };
    let effective_after = requested_after.max(retained_after);
    let fetch_limit = i64::from(query_value.limit.get()) + 1;
    let rows = query(
        "SELECT event_seq, branch_id, event_kind, work_revision, goal_revision,
                criterion_set_revision, branch_revision, graph_revision, source_ref,
                DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS created_at
         FROM work_events
         WHERE owner_id = ? AND work_id = ? AND event_seq > ?
         ORDER BY event_seq LIMIT ?",
    )
    .bind(query_value.owner_id.as_str())
    .bind(query_value.work_id.as_str())
    .bind(effective_after)
    .bind(fetch_limit)
    .fetch_all(repository.pool.get())
    .await
    .map_err(|source| WorkRepositoryError::persistence("load bounded Work events", source))?;
    let mut events = Vec::with_capacity(rows.len());
    for (index, row) in rows.into_iter().enumerate() {
        let event = decode_event(row)?;
        let expected_seq = effective_after
            .checked_add(i64::try_from(index + 1).expect("bounded page index"))
            .ok_or_else(|| {
                WorkRepositoryError::corrupt(
                    "Work event page",
                    std::io::Error::other("event sequence overflow"),
                )
            })?;
        if event.event_seq.get() != expected_seq {
            return Err(WorkRepositoryError::corrupt(
                "Work event page",
                std::io::Error::other("retained event sequence contains a gap"),
            ));
        }
        events.push(event);
    }
    let has_more = events.len() > usize::from(query_value.limit.get());
    if has_more {
        events.pop();
    }
    if !has_more
        && events
            .last()
            .map(|event| event.event_seq.get())
            .unwrap_or(effective_after)
            != event_head.get()
    {
        return Err(WorkRepositoryError::corrupt(
            "Work event page",
            std::io::Error::other("retained event tail does not reach the event head"),
        ));
    }
    let next_after_event_seq = events
        .last()
        .map(|event| event.event_seq)
        .or(query_value.after_event_seq);
    Ok(WorkEventPage {
        work_id: query_value.work_id,
        requested_after_event_seq: query_value.after_event_seq,
        next_after_event_seq,
        event_head,
        retained_from_event_seq,
        seen_through_event_seq,
        coverage,
        has_more,
        events,
    })
}

fn decode_event(row: sqlx::mysql::MySqlRow) -> Result<WorkEventRecord, WorkRepositoryError> {
    let optional_revision = |field: &'static str| {
        row.try_get::<Option<i64>, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("Work event", source))
    };
    let branch_id = row
        .try_get::<Option<String>, _>("branch_id")
        .map_err(|source| WorkRepositoryError::corrupt("Work event", source))?
        .map(WorkBranchId::parse)
        .transpose()
        .map_err(|source| WorkRepositoryError::corrupt("Work event", source))?;
    let event_kind = row
        .try_get::<String, _>("event_kind")
        .map_err(|source| WorkRepositoryError::corrupt("Work event", source))?;
    let kind = WorkEventKind::from_persisted(&event_kind).ok_or_else(|| {
        WorkRepositoryError::corrupt(
            "Work event",
            std::io::Error::other("event kind is outside the sealed schema"),
        )
    })?;
    Ok(WorkEventRecord {
        event_seq: WorkEventSeq::new(
            row.try_get::<i64, _>("event_seq")
                .map_err(|source| WorkRepositoryError::corrupt("Work event", source))?,
        )
        .map_err(|source| WorkRepositoryError::corrupt("Work event", source))?,
        branch_id,
        kind,
        work_revision: optional_revision("work_revision")?
            .map(WorkRevision::new)
            .transpose()
            .map_err(|source| WorkRepositoryError::corrupt("Work event", source))?,
        goal_revision: optional_revision("goal_revision")?
            .map(GoalRevision::new)
            .transpose()
            .map_err(|source| WorkRepositoryError::corrupt("Work event", source))?,
        criterion_set_revision: optional_revision("criterion_set_revision")?
            .map(CriterionSetRevision::new)
            .transpose()
            .map_err(|source| WorkRepositoryError::corrupt("Work event", source))?,
        branch_revision: optional_revision("branch_revision")?
            .map(WorkBranchRevision::new)
            .transpose()
            .map_err(|source| WorkRepositoryError::corrupt("Work event", source))?,
        graph_revision: optional_revision("graph_revision")?
            .map(GraphRevision::new)
            .transpose()
            .map_err(|source| WorkRepositoryError::corrupt("Work event", source))?,
        source_ref: WorkChangeRef::parse(
            row.try_get::<String, _>("source_ref")
                .map_err(|source| WorkRepositoryError::corrupt("Work event", source))?,
        )
        .map_err(|source| WorkRepositoryError::corrupt("Work event", source))?,
        created_at: super::repository::decode_timestamp(
            "Work event",
            "created_at",
            row.try_get("created_at")
                .map_err(|source| WorkRepositoryError::corrupt("Work event", source))?,
        )?,
    })
}

fn decode_optional_event_seq(
    value: i64,
    entity: &'static str,
) -> Result<Option<WorkEventSeq>, WorkRepositoryError> {
    if value == 0 {
        Ok(None)
    } else {
        WorkEventSeq::new(value)
            .map(Some)
            .map_err(|source| WorkRepositoryError::corrupt(entity, source))
    }
}
