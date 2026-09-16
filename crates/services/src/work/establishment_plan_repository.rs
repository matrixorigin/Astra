//! Derive pending graph application from immutable admission and accepted
//! graph facts inside the same snapshot transaction used by assignment.

use super::{
    GraphRevision, InternalSessionId, WorkAppliedGraphMutation, WorkEstablishmentMutationGroup,
    WorkItemAttemptId, WorkItemDeliveryStatus, WorkItemId, WorkItemRevision, WorkItemRevisionRef,
    WorkOwnerId, WorkRepositoryError, WorkTaskExecutionSnapshot, compile_work_establishment_plan,
    decode_work_establishment_payload,
};
use sqlx::{MySql, QueryBuilder, Row, Transaction};
use std::collections::{BTreeMap, BTreeSet};

pub(super) async fn load_graph_mutation_barrier(
    tx: &mut Transaction<'_, MySql>,
    owner: &WorkOwnerId,
    session: &InternalSessionId,
    snapshot: &WorkTaskExecutionSnapshot,
) -> Result<
    (
        Vec<WorkEstablishmentMutationGroup>,
        bool,
        Vec<WorkAppliedGraphMutation>,
    ),
    WorkRepositoryError,
> {
    // The existing owner/session recovery index selects this one genesis.
    // Completed establishment is still the immutable owner of future changes.
    let rows = sqlx::query(
        "SELECT operation_id, payload_json FROM work_establishment_operations
         WHERE owner_id = ? AND session_id = ? AND work_id = ? AND branch_id = ?
           AND operation_state IN ('pending', 'complete')
         ORDER BY operation_id LIMIT 2",
    )
    .bind(owner.as_str())
    .bind(session.as_str())
    .bind(snapshot.basis().work_id.as_str())
    .bind(snapshot.basis().branch_id.as_str())
    .fetch_all(&mut **tx)
    .await
    .map_err(|e| WorkRepositoryError::persistence("load Work mutation schedules", e))?;
    let corrupt = |message: String| {
        WorkRepositoryError::corrupt("Work mutation schedule", std::io::Error::other(message))
    };
    if rows.len() > 1 {
        return Err(corrupt(
            "too many establishment schedules for one Work branch".into(),
        ));
    }
    let mut pending = Vec::new();
    let mut applied = Vec::new();
    let mut has_unapplied = false;
    for row in rows {
        let operation_id: String = row
            .try_get("operation_id")
            .map_err(|e| WorkRepositoryError::corrupt("Work mutation schedule identity", e))?;
        let payload: String = row
            .try_get("payload_json")
            .map_err(|e| WorkRepositoryError::corrupt("Work mutation schedule payload", e))?;
        let (_, decision) = decode_work_establishment_payload(&payload).map_err(&corrupt)?;
        let Some(decision) = decision else {
            continue;
        };
        if decision.deferred_graph_mutations().is_empty() {
            continue;
        }
        let (_, tasks) = decision
            .initial_work_plan()
            .ok_or_else(|| corrupt("scheduled mutations require an initial Work graph".into()))?;
        let plan = compile_work_establishment_plan(
            &operation_id,
            tasks,
            decision.deferred_graph_mutations(),
        )
        .map_err(&corrupt)?;
        // Initial graph materialization has its own establishment recovery
        // phase; a mutation must never run against a partial genesis.
        let initial_graph_present = plan.initial_items.iter().all(|initial| {
            snapshot
                .items()
                .iter()
                .any(|item| item.item_id.as_str() == initial.item_id)
        });
        let proposal_ids = plan
            .mutation_groups
            .iter()
            .map(|group| {
                group
                    .proposal_id(
                        owner.as_str(),
                        session.as_str(),
                        snapshot.basis().work_id.as_str(),
                        snapshot.basis().branch_id.as_str(),
                    )
                    .map_err(&corrupt)
            })
            .collect::<Result<Vec<_>, _>>()?;
        // A graph revision is the canonical acceptance fact: graph change and
        // proposal resolution commit together, while terminal proposal rows
        // may later be pruned. `patch_ref` is the server-derived proposal ID,
        // so this bounded lookup remains branch-safe without scanning history.
        let mut query = QueryBuilder::<MySql>::new(
            "SELECT g.patch_ref AS proposal_id, g.revision AS result_graph_revision,
                    t.trigger_attempt_id, t.trigger_item_id, t.trigger_item_revision
               FROM work_graph_revisions g
               LEFT JOIN work_proposal_trigger_attempts t
                 ON t.owner_id = g.owner_id
                AND t.work_id = g.work_id
                AND t.branch_id = ",
        );
        query
            .push_bind(snapshot.basis().branch_id.as_str())
            .push(
                " AND t.proposal_id = g.patch_ref
              WHERE g.owner_id = ",
            )
            .push_bind(owner.as_str())
            .push(" AND g.work_id = ")
            .push_bind(snapshot.basis().work_id.as_str())
            .push(" AND g.patch_ref IN (");
        let mut ids = query.separated(", ");
        for id in &proposal_ids {
            ids.push_bind(id.as_str());
        }
        ids.push_unseparated(")");
        let mut accepted = BTreeMap::new();
        for row in query.build().fetch_all(&mut **tx).await.map_err(|e| {
            WorkRepositoryError::persistence("load applied Work mutation markers", e)
        })? {
            let proposal_id: String = row
                .try_get("proposal_id")
                .map_err(|e| WorkRepositoryError::corrupt("Work mutation proposal identity", e))?;
            let result_graph_revision = row
                .try_get::<i64, _>("result_graph_revision")
                .map_err(|e| WorkRepositoryError::corrupt("Work mutation graph revision", e))?;
            let result_graph_revision = GraphRevision::new(result_graph_revision)
                .map_err(|e| corrupt(format!("invalid accepted graph revision: {e}")))?;
            let trigger_attempt_id = row
                .try_get::<Option<String>, _>("trigger_attempt_id")
                .map_err(|e| WorkRepositoryError::corrupt("Work mutation trigger attempt", e))?;
            let trigger_item_id = row
                .try_get::<Option<String>, _>("trigger_item_id")
                .map_err(|e| WorkRepositoryError::corrupt("Work mutation trigger item", e))?;
            let trigger_item_revision = row
                .try_get::<Option<i64>, _>("trigger_item_revision")
                .map_err(|e| WorkRepositoryError::corrupt("Work mutation trigger revision", e))?;
            let trigger = match (trigger_attempt_id, trigger_item_id, trigger_item_revision) {
                (None, None, None) => None,
                (Some(attempt_id), Some(item_id), Some(revision)) => Some((
                    WorkItemAttemptId::parse(attempt_id)
                        .map_err(|e| corrupt(format!("invalid mutation trigger attempt: {e}")))?,
                    WorkItemRevisionRef {
                        item_id: WorkItemId::parse(item_id)
                            .map_err(|e| corrupt(format!("invalid mutation trigger item: {e}")))?,
                        revision: WorkItemRevision::new(revision).map_err(|e| {
                            corrupt(format!("invalid mutation trigger revision: {e}"))
                        })?,
                    },
                )),
                _ => {
                    return Err(corrupt(
                        "mutation trigger association has a partial identity".into(),
                    ));
                }
            };
            if accepted
                .insert(proposal_id, (result_graph_revision, trigger))
                .is_some()
            {
                return Err(corrupt(
                    "multiple graph revisions share one Work mutation proposal identity".into(),
                ));
            }
        }

        // An accepted proposal must have produced a graph revision in the
        // same transaction. Detect a corrupt live row before treating it as a
        // pending mutation; otherwise recovery could attempt a duplicate.
        let mut accepted_rows = QueryBuilder::<MySql>::new(
            "SELECT proposal_id FROM work_proposals
              WHERE owner_id = ",
        );
        accepted_rows
            .push_bind(owner.as_str())
            .push(" AND work_id = ")
            .push_bind(snapshot.basis().work_id.as_str())
            .push(" AND branch_id = ")
            .push_bind(snapshot.basis().branch_id.as_str())
            .push(" AND proposal_kind = 'plan_patch' AND status = 'accepted' AND proposal_id IN (");
        let mut accepted_ids = accepted_rows.separated(", ");
        for id in &proposal_ids {
            accepted_ids.push_bind(id.as_str());
        }
        accepted_ids.push_unseparated(")");
        for row in accepted_rows
            .build()
            .fetch_all(&mut **tx)
            .await
            .map_err(|e| WorkRepositoryError::persistence("verify accepted Work mutations", e))?
        {
            let proposal_id: String = row
                .try_get("proposal_id")
                .map_err(|e| WorkRepositoryError::corrupt("Work mutation proposal identity", e))?;
            if !accepted.contains_key(&proposal_id) {
                return Err(corrupt(
                    "accepted Work mutation has no canonical graph revision".into(),
                ));
            }
        }
        let trigger_items = plan
            .mutation_groups
            .iter()
            .zip(&proposal_ids)
            .filter(|(_, id)| !accepted.contains_key(id.as_str()))
            .flat_map(|(group, _)| group.trigger_items.iter().cloned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        // Trigger delivery belongs to the immutable initial revision. A later
        // retirement must not erase a receipt already satisfying another group.
        let (_, deliveries) = super::plan_context_repository::load_item_executions(
            tx,
            owner,
            &snapshot.basis().work_id,
            &snapshot.basis().branch_id,
            &trigger_items,
        )
        .await?;
        for (group, proposal_id) in plan.mutation_groups.into_iter().zip(proposal_ids) {
            if let Some((result_graph_revision, trigger)) =
                accepted.get(proposal_id.as_str()).cloned()
            {
                if let Some((_, trigger_item)) = trigger.as_ref()
                    && !group
                        .trigger_items
                        .iter()
                        .any(|candidate| candidate == trigger_item)
                {
                    return Err(corrupt(
                        "mutation trigger association is outside its immutable trigger set".into(),
                    ));
                }
                let trigger_association_known = trigger.is_some() || group.trigger_items.is_empty();
                applied.push(WorkAppliedGraphMutation {
                    group,
                    result_graph_revision,
                    trigger_attempt_id: trigger.as_ref().map(|(attempt, _)| attempt.clone()),
                    trigger_item: trigger.map(|(_, item)| item),
                    trigger_association_known,
                });
                continue;
            }
            has_unapplied = true;
            if !initial_graph_present {
                continue;
            }
            let all_delivered = group.trigger_items.iter().all(|trigger| {
                deliveries
                    .get(trigger)
                    .is_some_and(|delivery| delivery.status == WorkItemDeliveryStatus::Delivered)
            });
            if all_delivered {
                pending.push(group);
            }
        }
    }
    Ok((pending, has_unapplied, applied))
}

/// Record the exact primary settlement that made each due admission mutation
/// eligible. The branch lock and immutable settlement identity make one insert
/// authoritative; a conflicting duplicate is surfaced instead of being
/// silently swallowed.
pub(super) async fn record_graph_mutation_trigger_attempts(
    tx: &mut Transaction<'_, MySql>,
    owner: &WorkOwnerId,
    session: &InternalSessionId,
    snapshot: &WorkTaskExecutionSnapshot,
    groups: &[WorkEstablishmentMutationGroup],
    trigger_attempt_id: &WorkItemAttemptId,
    trigger_item: &WorkItemRevisionRef,
) -> Result<(), WorkRepositoryError> {
    let groups = groups
        .iter()
        .filter(|group| !group.trigger_items.is_empty())
        .collect::<Vec<_>>();
    if groups.is_empty() {
        return Ok(());
    }
    if groups
        .iter()
        .any(|group| !group.trigger_items.iter().any(|item| item == trigger_item))
    {
        return Err(WorkRepositoryError::corrupt(
            "Work mutation trigger item",
            std::io::Error::other("settlement item is outside a due mutation trigger set"),
        ));
    }
    let rows = groups
        .iter()
        .map(|group| {
            let proposal_id = group
                .proposal_id(
                    owner.as_str(),
                    session.as_str(),
                    snapshot.basis().work_id.as_str(),
                    snapshot.basis().branch_id.as_str(),
                )
                .map_err(|error| {
                    WorkRepositoryError::corrupt(
                        "Work mutation trigger identity",
                        std::io::Error::other(error),
                    )
                })?;
            Ok::<_, WorkRepositoryError>(proposal_id)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut query = QueryBuilder::<MySql>::new(
        "INSERT INTO work_proposal_trigger_attempts
         (owner_id, work_id, branch_id, proposal_id, trigger_attempt_id,
          trigger_item_id, trigger_item_revision) ",
    );
    query.push_values(rows, |mut values, proposal_id| {
        values
            .push_bind(owner.as_str().to_string())
            .push_bind(snapshot.basis().work_id.as_str().to_string())
            .push_bind(snapshot.basis().branch_id.as_str().to_string())
            .push_bind(proposal_id.as_str().to_string())
            .push_bind(trigger_attempt_id.as_str().to_string())
            .push_bind(trigger_item.item_id.as_str().to_string())
            .push_bind(trigger_item.revision.get());
    });
    query.build().execute(&mut **tx).await.map_err(|error| {
        WorkRepositoryError::persistence("record Work mutation trigger attempts", error)
    })?;
    Ok(())
}
