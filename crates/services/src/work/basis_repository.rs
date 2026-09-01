use super::events::NewWorkEvent;
use super::repository::{DatabaseWorkRepository, WorkRepositoryError, rollback_transaction};
use super::{
    CriterionSetRevision, GoalRevision, WorkBranchBasisChange, WorkBranchRecord, WorkEventKind,
    WorkRevision,
};
use sqlx::{Row, query};

struct CurrentBasis {
    work_revision: WorkRevision,
    current_goal_revision: GoalRevision,
    current_criteria_set_revision: CriterionSetRevision,
    branch: WorkBranchRecord,
    archived: bool,
}

async fn load_current_basis(
    transaction: &mut sqlx::Transaction<'_, sqlx::MySql>,
    change: &WorkBranchBasisChange,
) -> Result<CurrentBasis, WorkRepositoryError> {
    let row = query(
        "SELECT w.work_revision, w.current_goal_revision, w.current_criteria_set_revision,
                CASE WHEN w.archived_at IS NULL THEN 0 ELSE 1 END AS work_archived
         FROM works w
         WHERE w.owner_id = ? AND w.work_id = ? LIMIT 1",
    )
    .bind(change.owner_id.as_str())
    .bind(change.work_id.as_str())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("load Work branch basis", source))?
    .ok_or(WorkRepositoryError::NotFound)?;
    let revision = |field: &'static str| {
        row.try_get::<i64, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("Work branch basis", source))
    };
    let work_revision = WorkRevision::new(revision("work_revision")?)
        .map_err(|source| WorkRepositoryError::corrupt("Work branch basis", source))?;
    let current_goal_revision = GoalRevision::new(revision("current_goal_revision")?)
        .map_err(|source| WorkRepositoryError::corrupt("Work branch basis", source))?;
    let current_criteria_set_revision =
        CriterionSetRevision::new(revision("current_criteria_set_revision")?)
            .map_err(|source| WorkRepositoryError::corrupt("Work branch basis", source))?;
    let branch = super::graph_repository::load_branch_by_identity(
        transaction,
        &change.owner_id,
        &change.work_id,
        &change.branch_id,
    )
    .await?;
    Ok(CurrentBasis {
        work_revision,
        current_goal_revision,
        current_criteria_set_revision,
        archived: revision("work_archived")? != 0 || branch.parts().archived_at.is_some(),
        branch,
    })
}

fn stale(change: &WorkBranchBasisChange, current: &CurrentBasis) -> WorkRepositoryError {
    WorkRepositoryError::StaleBranchBasis {
        expected_work_revision: change.expected_work_revision,
        actual_work_revision: current.work_revision,
        expected_branch_revision: change.expected_branch_revision,
        actual_branch_revision: current.branch.parts().branch_revision,
        expected_goal_revision: change.expected_goal_revision,
        actual_goal_revision: current.branch.parts().goal_revision_ref,
        expected_criteria_set_revision: change.expected_criteria_set_revision,
        actual_criteria_set_revision: current.branch.parts().criteria_set_revision_ref,
    }
}

pub(super) async fn adopt_branch_basis(
    repository: &DatabaseWorkRepository,
    change: WorkBranchBasisChange,
) -> Result<WorkBranchRecord, WorkRepositoryError> {
    let next_branch_revision = change
        .expected_branch_revision
        .checked_next()
        .map_err(super::repository::invalid_mutation)?;
    let mut transaction = repository.pool.get().begin().await.map_err(|source| {
        WorkRepositoryError::persistence("begin Work branch basis transaction", source)
    })?;
    let current = load_current_basis(&mut transaction, &change).await?;
    if current.archived {
        return Err(WorkRepositoryError::Archived);
    }

    let replay = query(
        "SELECT branch_id, goal_revision, criterion_set_revision, branch_revision
         FROM work_events
         WHERE owner_id = ? AND work_id = ? AND event_kind = 'branch_basis_adopted'
           AND source_ref = ? LIMIT 1",
    )
    .bind(change.owner_id.as_str())
    .bind(change.work_id.as_str())
    .bind(change.source_ref.as_str())
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("load branch basis replay", source))?;
    if let Some(replay) = replay {
        let exact = replay
            .try_get::<Option<String>, _>("branch_id")
            .map_err(|source| WorkRepositoryError::corrupt("branch basis event", source))?
            .as_deref()
            == Some(change.branch_id.as_str())
            && replay
                .try_get::<Option<i64>, _>("goal_revision")
                .map_err(|source| WorkRepositoryError::corrupt("branch basis event", source))?
                == Some(change.target_goal_revision.get())
            && replay
                .try_get::<Option<i64>, _>("criterion_set_revision")
                .map_err(|source| WorkRepositoryError::corrupt("branch basis event", source))?
                == Some(change.target_criteria_set_revision.get())
            && replay
                .try_get::<Option<i64>, _>("branch_revision")
                .map_err(|source| WorkRepositoryError::corrupt("branch basis event", source))?
                == Some(next_branch_revision.get());
        if !exact {
            return Err(WorkRepositoryError::Conflict {
                resource: super::WorkConflictResource::WorkEventIdentity,
            });
        }
        if current.branch.parts().branch_revision < next_branch_revision
            || current.branch.parts().goal_revision_ref != change.target_goal_revision
            || current.branch.parts().criteria_set_revision_ref
                != change.target_criteria_set_revision
        {
            return Err(stale(&change, &current));
        }
        transaction.commit().await.map_err(|source| {
            WorkRepositoryError::persistence("commit branch basis replay", source)
        })?;
        return Ok(current.branch);
    }

    if current.current_goal_revision != change.target_goal_revision
        || current.current_criteria_set_revision != change.target_criteria_set_revision
    {
        return Err(WorkRepositoryError::InvalidBranchBasisTarget {
            target_goal_revision: change.target_goal_revision,
            current_goal_revision: current.current_goal_revision,
            target_criteria_set_revision: change.target_criteria_set_revision,
            current_criteria_set_revision: current.current_criteria_set_revision,
        });
    }
    if current.branch.parts().goal_revision_ref == change.target_goal_revision
        && current.branch.parts().criteria_set_revision_ref == change.target_criteria_set_revision
    {
        transaction.commit().await.map_err(|source| {
            WorkRepositoryError::persistence("commit branch basis no-op", source)
        })?;
        return Ok(current.branch);
    }
    if current.work_revision != change.expected_work_revision
        || current.branch.parts().branch_revision != change.expected_branch_revision
        || current.branch.parts().goal_revision_ref != change.expected_goal_revision
        || current.branch.parts().criteria_set_revision_ref != change.expected_criteria_set_revision
    {
        return Err(stale(&change, &current));
    }

    let updated = query(
        "UPDATE work_branches
         SET branch_revision = ?, goal_revision_ref = ?, criteria_set_revision_ref = ?,
             updated_at = NOW(6)
         WHERE owner_id = ? AND work_id = ? AND branch_id = ?
           AND branch_revision = ? AND goal_revision_ref = ?
           AND criteria_set_revision_ref = ? AND archived_at IS NULL",
    )
    .bind(next_branch_revision.get())
    .bind(change.target_goal_revision.get())
    .bind(change.target_criteria_set_revision.get())
    .bind(change.owner_id.as_str())
    .bind(change.work_id.as_str())
    .bind(change.branch_id.as_str())
    .bind(change.expected_branch_revision.get())
    .bind(change.expected_goal_revision.get())
    .bind(change.expected_criteria_set_revision.get())
    .execute(&mut *transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("advance branch basis CAS", source))?;
    if updated.rows_affected() != 1 {
        transaction.rollback().await.map_err(|source| {
            WorkRepositoryError::persistence("rollback stale branch basis CAS", source)
        })?;
        let mut refresh = repository.pool.get().begin().await.map_err(|source| {
            WorkRepositoryError::persistence("begin branch basis conflict read", source)
        })?;
        let refreshed = load_current_basis(&mut refresh, &change).await?;
        refresh.commit().await.map_err(|source| {
            WorkRepositoryError::persistence("commit branch basis conflict read", source)
        })?;
        if !refreshed.archived
            && refreshed.branch.parts().branch_revision >= next_branch_revision
            && refreshed.branch.parts().goal_revision_ref == change.target_goal_revision
            && refreshed.branch.parts().criteria_set_revision_ref
                == change.target_criteria_set_revision
        {
            // Another producer committed the same immutable target. The
            // semantic effect has converged; do not create a duplicate event.
            return Ok(refreshed.branch);
        }
        return Err(stale(&change, &refreshed));
    }
    if let Err(error) = super::events_repository::append_event(
        &mut transaction,
        &NewWorkEvent {
            owner_id: change.owner_id.clone(),
            work_id: change.work_id.clone(),
            branch_id: Some(change.branch_id.clone()),
            kind: WorkEventKind::BranchBasisAdopted,
            work_revision: Some(change.expected_work_revision),
            goal_revision: Some(change.target_goal_revision),
            criterion_set_revision: Some(change.target_criteria_set_revision),
            branch_revision: Some(next_branch_revision),
            graph_revision: Some(current.branch.parts().current_graph_revision),
            source_ref: change.source_ref.clone(),
        },
    )
    .await
    {
        return Err(rollback_transaction(
            transaction,
            "rollback branch basis event transaction",
            error,
        )
        .await);
    }
    let branch = super::graph_repository::load_branch_by_identity(
        &mut transaction,
        &change.owner_id,
        &change.work_id,
        &change.branch_id,
    )
    .await?;
    transaction.commit().await.map_err(|source| {
        WorkRepositoryError::persistence("commit Work branch basis transaction", source)
    })?;
    Ok(branch)
}
