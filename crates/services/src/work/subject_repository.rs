use super::repository::{
    DatabaseWorkRepository, WorkRepositoryError, invalid_mutation, rollback_transaction,
};
use super::{
    GraphRevision, WorkBranchId, WorkBranchRevision, WorkBranchSubject, WorkBranchSubjectChange,
    WorkBranchSubjectInvalidation, WorkBranchSubjectRevision, WorkContentHash, WorkEventKind,
    WorkId, WorkOwnerId, WorkSubjectRef,
};
use chrono::{DateTime, Utc};
use sqlx::{MySql, Row, Transaction, query};

const SUBJECT_COLUMNS: &str = "
    s.subject_record_revision,
    s.branch_revision AS subject_branch_revision,
    s.graph_revision AS subject_graph_revision,
    s.subject_ref,
    s.subject_revision,
    s.source_ref,
    DATE_FORMAT(s.created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS subject_created_at,
    DATE_FORMAT(s.updated_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS subject_updated_at";

struct SubjectContext {
    branch_revision: WorkBranchRevision,
    graph_revision: GraphRevision,
    archived: bool,
    subject: Option<WorkBranchSubject>,
}

fn decode_subject(
    row: &sqlx::mysql::MySqlRow,
    work_id: &WorkId,
    branch_id: &WorkBranchId,
) -> Result<Option<WorkBranchSubject>, WorkRepositoryError> {
    let Some(subject_record_revision) = row
        .try_get::<Option<i64>, _>("subject_record_revision")
        .map_err(|source| WorkRepositoryError::corrupt("Work branch subject", source))?
    else {
        return Ok(None);
    };
    let required_i64 = |field: &'static str| {
        row.try_get::<Option<i64>, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("Work branch subject", source))?
            .ok_or_else(|| {
                WorkRepositoryError::corrupt(
                    "Work branch subject",
                    std::io::Error::other(format!("missing {field}")),
                )
            })
    };
    let required_string = |field: &'static str| {
        row.try_get::<Option<String>, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("Work branch subject", source))?
            .ok_or_else(|| {
                WorkRepositoryError::corrupt(
                    "Work branch subject",
                    std::io::Error::other(format!("missing {field}")),
                )
            })
    };
    let parse_timestamp = |field: &'static str| {
        super::repository::decode_timestamp("Work branch subject", field, required_string(field)?)
    };
    Ok(Some(WorkBranchSubject {
        work_id: work_id.clone(),
        branch_id: branch_id.clone(),
        subject_record_revision: WorkBranchSubjectRevision::new(subject_record_revision)
            .map_err(|source| WorkRepositoryError::corrupt("Work branch subject", source))?,
        branch_revision: WorkBranchRevision::new(required_i64("subject_branch_revision")?)
            .map_err(|source| WorkRepositoryError::corrupt("Work branch subject", source))?,
        graph_revision: GraphRevision::new(required_i64("subject_graph_revision")?)
            .map_err(|source| WorkRepositoryError::corrupt("Work branch subject", source))?,
        subject_ref: WorkSubjectRef::parse(required_string("subject_ref")?)
            .map_err(|source| WorkRepositoryError::corrupt("Work branch subject", source))?,
        subject_revision: WorkContentHash::parse(required_string("subject_revision")?).map_err(
            |message| {
                WorkRepositoryError::corrupt("Work branch subject", std::io::Error::other(message))
            },
        )?,
        source_ref: super::WorkChangeRef::parse(required_string("source_ref")?)
            .map_err(|source| WorkRepositoryError::corrupt("Work branch subject", source))?,
        created_at: parse_timestamp("subject_created_at")?,
        updated_at: parse_timestamp("subject_updated_at")?,
    }))
}

async fn load_context(
    repository: &DatabaseWorkRepository,
    owner_id: &WorkOwnerId,
    work_id: &WorkId,
    branch_id: &WorkBranchId,
) -> Result<SubjectContext, WorkRepositoryError> {
    let statement = format!(
        "SELECT b.branch_revision, b.current_graph_revision,
                CASE WHEN w.archived_at IS NULL AND b.archived_at IS NULL THEN 0 ELSE 1 END AS archived,
                {SUBJECT_COLUMNS}
         FROM works w
         LEFT JOIN work_branches b
           ON b.owner_id = w.owner_id AND b.work_id = w.work_id AND b.branch_id = ?
         LEFT JOIN work_branch_subjects s
           ON s.owner_id = b.owner_id AND s.work_id = b.work_id AND s.branch_id = b.branch_id
         WHERE w.owner_id = ? AND w.work_id = ?
         LIMIT 1"
    );
    let row = query(&statement)
        .bind(branch_id.as_str())
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .fetch_optional(repository.pool.get())
        .await
        .map_err(|source| WorkRepositoryError::persistence("load Work branch subject", source))?
        .ok_or(WorkRepositoryError::NotFound)?;
    let branch_revision = row
        .try_get::<Option<i64>, _>("branch_revision")
        .map_err(|source| WorkRepositoryError::corrupt("Work branch subject", source))?
        .ok_or(WorkRepositoryError::NotFound)?;
    let graph_revision = row
        .try_get::<Option<i64>, _>("current_graph_revision")
        .map_err(|source| WorkRepositoryError::corrupt("Work branch subject", source))?
        .ok_or(WorkRepositoryError::NotFound)?;
    let branch_revision = WorkBranchRevision::new(branch_revision)
        .map_err(|source| WorkRepositoryError::corrupt("Work branch", source))?;
    let graph_revision = GraphRevision::new(graph_revision)
        .map_err(|source| WorkRepositoryError::corrupt("Work branch", source))?;
    let subject = decode_subject(&row, work_id, branch_id)?;
    if subject.as_ref().is_some_and(|subject| {
        subject.branch_revision > branch_revision
            || subject.graph_revision > graph_revision
            || (subject.branch_revision == branch_revision
                && subject.graph_revision != graph_revision)
    }) {
        return Err(WorkRepositoryError::corrupt(
            "Work branch subject",
            std::io::Error::other("subject basis is ahead of or incoherent with its branch"),
        ));
    }
    Ok(SubjectContext {
        branch_revision,
        graph_revision,
        archived: row
            .try_get::<i64, _>("archived")
            .map_err(|source| WorkRepositoryError::corrupt("Work branch", source))?
            != 0,
        subject,
    })
}

fn classify_context(
    context: &SubjectContext,
    change: &WorkBranchSubjectChange,
) -> Result<Option<WorkBranchSubject>, WorkRepositoryError> {
    if context.archived {
        return Err(WorkRepositoryError::Archived);
    }
    if let Some(subject) = &context.subject {
        if subject.is_exact_replay(change) {
            return Ok(Some(subject.clone()));
        }
        if subject.branch_revision == context.branch_revision
            && context.graph_revision == change.graph_revision
            && subject.represents(change)
        {
            // A second producer observed the same immutable target. This is a
            // semantic no-op: do not invalidate evidence or emit another event.
            return Ok(Some(subject.clone()));
        }
    }
    if context.branch_revision != change.expected_branch_revision
        || context.graph_revision != change.graph_revision
    {
        return Err(WorkRepositoryError::StaleSubjectBasis {
            expected_branch_revision: change.expected_branch_revision,
            actual_branch_revision: context.branch_revision,
            expected_graph_revision: change.graph_revision,
            actual_graph_revision: context.graph_revision,
        });
    }
    Ok(None)
}

pub(super) async fn load_branch_subject(
    repository: &DatabaseWorkRepository,
    owner_id: &WorkOwnerId,
    work_id: &WorkId,
    branch_id: &WorkBranchId,
) -> Result<Option<WorkBranchSubject>, WorkRepositoryError> {
    let context = load_context(repository, owner_id, work_id, branch_id).await?;
    if context.archived {
        return Err(WorkRepositoryError::Archived);
    }
    Ok(context.subject)
}

async fn load_subject_in_transaction(
    transaction: &mut Transaction<'_, MySql>,
    work_id: &WorkId,
    branch_id: &WorkBranchId,
    owner_id: &WorkOwnerId,
) -> Result<WorkBranchSubject, WorkRepositoryError> {
    let statement = format!(
        "SELECT {SUBJECT_COLUMNS}
         FROM work_branch_subjects s
         WHERE s.owner_id = ? AND s.work_id = ? AND s.branch_id = ?
         LIMIT 1"
    );
    let row = query(&statement)
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(branch_id.as_str())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(|source| WorkRepositoryError::persistence("load updated branch subject", source))?
        .ok_or_else(|| {
            WorkRepositoryError::corrupt(
                "Work branch subject",
                std::io::Error::other("subject update did not leave a current row"),
            )
        })?;
    decode_subject(&row, work_id, branch_id)?.ok_or_else(|| {
        WorkRepositoryError::corrupt(
            "Work branch subject",
            std::io::Error::other("subject row decoded as absent"),
        )
    })
}

pub(super) async fn set_branch_subject(
    repository: &DatabaseWorkRepository,
    change: WorkBranchSubjectChange,
) -> Result<WorkBranchSubject, WorkRepositoryError> {
    let context = load_context(
        repository,
        &change.owner_id,
        &change.work_id,
        &change.branch_id,
    )
    .await?;
    if let Some(existing) = classify_context(&context, &change)? {
        return Ok(existing);
    }
    let next_branch_revision = change
        .expected_branch_revision
        .checked_next()
        .map_err(invalid_mutation)?;
    let next_subject_revision = match &context.subject {
        Some(subject) => subject
            .subject_record_revision
            .checked_next()
            .map_err(invalid_mutation)?,
        None => WorkBranchSubjectRevision::INITIAL,
    };
    let updated_at = DateTime::<Utc>::from_timestamp_micros(Utc::now().timestamp_micros())
        .expect("current timestamp");
    let mut transaction = repository.pool.get().begin().await.map_err(|source| {
        WorkRepositoryError::persistence("begin branch subject transaction", source)
    })?;
    let update = query(
        "UPDATE work_branches
         SET branch_revision = ?, updated_at = ?
         WHERE owner_id = ? AND work_id = ? AND branch_id = ?
           AND branch_revision = ? AND current_graph_revision = ? AND archived_at IS NULL",
    )
    .bind(next_branch_revision.get())
    .bind(updated_at.naive_utc())
    .bind(change.owner_id.as_str())
    .bind(change.work_id.as_str())
    .bind(change.branch_id.as_str())
    .bind(change.expected_branch_revision.get())
    .bind(change.graph_revision.get())
    .execute(&mut *transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("advance branch subject CAS", source))?;
    if update.rows_affected() != 1 {
        transaction.rollback().await.map_err(|source| {
            WorkRepositoryError::persistence("rollback stale branch subject transaction", source)
        })?;
        let current = load_context(
            repository,
            &change.owner_id,
            &change.work_id,
            &change.branch_id,
        )
        .await?;
        if let Some(existing) = classify_context(&current, &change)? {
            return Ok(existing);
        }
        return Err(WorkRepositoryError::corrupt(
            "Work branch subject CAS",
            std::io::Error::other("CAS missed without a changed basis"),
        ));
    }

    let upsert = query(
        "INSERT INTO work_branch_subjects
         (owner_id, work_id, branch_id, subject_record_revision, branch_revision,
          graph_revision, subject_ref, subject_revision, source_ref, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON DUPLICATE KEY UPDATE
          subject_record_revision = VALUES(subject_record_revision),
          branch_revision = VALUES(branch_revision),
          graph_revision = VALUES(graph_revision),
          subject_ref = VALUES(subject_ref),
          subject_revision = VALUES(subject_revision),
          source_ref = VALUES(source_ref),
          updated_at = VALUES(updated_at)",
    )
    .bind(change.owner_id.as_str())
    .bind(change.work_id.as_str())
    .bind(change.branch_id.as_str())
    .bind(next_subject_revision.get())
    .bind(next_branch_revision.get())
    .bind(change.graph_revision.get())
    .bind(change.subject_ref.as_str())
    .bind(change.subject_revision.as_str())
    .bind(change.source_ref.as_str())
    .bind(updated_at.naive_utc())
    .bind(updated_at.naive_utc())
    .execute(&mut *transaction)
    .await;
    if let Err(source) = upsert {
        let error = WorkRepositoryError::persistence("store current branch subject", source);
        return Err(rollback_transaction(
            transaction,
            "rollback failed branch subject transaction",
            error,
        )
        .await);
    }

    if let Err(error) = super::events_repository::append_event(
        &mut transaction,
        &super::events::NewWorkEvent {
            owner_id: change.owner_id.clone(),
            work_id: change.work_id.clone(),
            branch_id: Some(change.branch_id.clone()),
            kind: WorkEventKind::SubjectChanged,
            work_revision: None,
            goal_revision: None,
            criterion_set_revision: None,
            branch_revision: Some(next_branch_revision),
            graph_revision: Some(change.graph_revision),
            source_ref: change.source_ref.clone(),
        },
    )
    .await
    {
        return Err(rollback_transaction(
            transaction,
            "rollback branch subject event transaction",
            error,
        )
        .await);
    }
    let updated = load_subject_in_transaction(
        &mut transaction,
        &change.work_id,
        &change.branch_id,
        &change.owner_id,
    )
    .await?;
    transaction.commit().await.map_err(|source| {
        WorkRepositoryError::persistence("commit branch subject transaction", source)
    })?;
    Ok(updated)
}

pub(super) async fn invalidate_branch_subject(
    repository: &DatabaseWorkRepository,
    invalidation: WorkBranchSubjectInvalidation,
) -> Result<WorkBranchRevision, WorkRepositoryError> {
    let mut transaction = repository.pool.get().begin().await.map_err(|source| {
        WorkRepositoryError::persistence("begin branch subject invalidation", source)
    })?;
    let row = query(
        "SELECT b.branch_revision, b.current_graph_revision,
                CASE WHEN w.archived_at IS NULL AND b.archived_at IS NULL
                          AND b.deletion_operation_id IS NULL
                     THEN 0 ELSE 1 END AS unavailable,
                s.subject_record_revision
         FROM works w
         JOIN work_branches b
           ON b.owner_id = w.owner_id AND b.work_id = w.work_id AND b.branch_id = ?
         LEFT JOIN work_branch_subjects s
           ON s.owner_id = b.owner_id AND s.work_id = b.work_id AND s.branch_id = b.branch_id
         WHERE w.owner_id = ? AND w.work_id = ? LIMIT 1 FOR UPDATE",
    )
    .bind(invalidation.branch_id.as_str())
    .bind(invalidation.owner_id.as_str())
    .bind(invalidation.work_id.as_str())
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("lock branch subject invalidation", source))?
    .ok_or(WorkRepositoryError::NotFound)?;
    if row
        .try_get::<i64, _>("unavailable")
        .map_err(|source| WorkRepositoryError::corrupt("Work branch subject", source))?
        != 0
    {
        return Err(WorkRepositoryError::Archived);
    }
    let branch_revision = WorkBranchRevision::new(
        row.try_get::<i64, _>("branch_revision")
            .map_err(|source| WorkRepositoryError::corrupt("Work branch subject", source))?,
    )
    .map_err(|source| WorkRepositoryError::corrupt("Work branch subject", source))?;
    let graph_revision = GraphRevision::new(
        row.try_get::<i64, _>("current_graph_revision")
            .map_err(|source| WorkRepositoryError::corrupt("Work branch subject", source))?,
    )
    .map_err(|source| WorkRepositoryError::corrupt("Work branch subject", source))?;
    let subject_record_revision = row
        .try_get::<Option<i64>, _>("subject_record_revision")
        .map_err(|source| WorkRepositoryError::corrupt("Work branch subject", source))?;
    if subject_record_revision.is_none() {
        transaction.commit().await.map_err(|source| {
            WorkRepositoryError::persistence("commit absent branch subject invalidation", source)
        })?;
        return Ok(branch_revision);
    }
    if branch_revision != invalidation.expected_branch_revision
        || graph_revision != invalidation.graph_revision
    {
        return Err(WorkRepositoryError::StaleSubjectBasis {
            expected_branch_revision: invalidation.expected_branch_revision,
            actual_branch_revision: branch_revision,
            expected_graph_revision: invalidation.graph_revision,
            actual_graph_revision: graph_revision,
        });
    }
    let next_branch_revision = branch_revision.checked_next().map_err(invalid_mutation)?;
    let updated_at = DateTime::<Utc>::from_timestamp_micros(Utc::now().timestamp_micros())
        .expect("current timestamp");
    let branch_update = query(
        "UPDATE work_branches SET branch_revision = ?, updated_at = ?
         WHERE owner_id = ? AND work_id = ? AND branch_id = ?
           AND branch_revision = ? AND current_graph_revision = ?
           AND archived_at IS NULL AND deletion_operation_id IS NULL",
    )
    .bind(next_branch_revision.get())
    .bind(updated_at.naive_utc())
    .bind(invalidation.owner_id.as_str())
    .bind(invalidation.work_id.as_str())
    .bind(invalidation.branch_id.as_str())
    .bind(branch_revision.get())
    .bind(graph_revision.get())
    .execute(&mut *transaction)
    .await
    .map_err(|source| {
        WorkRepositoryError::persistence("advance invalidated branch subject", source)
    })?;
    if branch_update.rows_affected() != 1 {
        return Err(WorkRepositoryError::corrupt(
            "Work branch subject invalidation",
            std::io::Error::other("locked branch failed its subject invalidation CAS"),
        ));
    }
    let deleted = query(
        "DELETE FROM work_branch_subjects
         WHERE owner_id = ? AND work_id = ? AND branch_id = ?
           AND subject_record_revision = ? AND branch_revision = ? AND graph_revision = ?",
    )
    .bind(invalidation.owner_id.as_str())
    .bind(invalidation.work_id.as_str())
    .bind(invalidation.branch_id.as_str())
    .bind(subject_record_revision.expect("checked present"))
    .bind(branch_revision.get())
    .bind(graph_revision.get())
    .execute(&mut *transaction)
    .await
    .map_err(|source| {
        WorkRepositoryError::persistence("remove invalidated branch subject", source)
    })?;
    if deleted.rows_affected() != 1 {
        return Err(WorkRepositoryError::corrupt(
            "Work branch subject invalidation",
            std::io::Error::other("locked subject disappeared before invalidation"),
        ));
    }
    super::events_repository::append_event(
        &mut transaction,
        &super::events::NewWorkEvent {
            owner_id: invalidation.owner_id,
            work_id: invalidation.work_id,
            branch_id: Some(invalidation.branch_id),
            kind: WorkEventKind::SubjectChanged,
            work_revision: None,
            goal_revision: None,
            criterion_set_revision: None,
            branch_revision: Some(next_branch_revision),
            graph_revision: Some(graph_revision),
            source_ref: invalidation.source_ref,
        },
    )
    .await?;
    transaction.commit().await.map_err(|source| {
        WorkRepositoryError::persistence("commit branch subject invalidation", source)
    })?;
    Ok(next_branch_revision)
}
