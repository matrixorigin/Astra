use super::acceptance::{
    AcceptanceGapReason, NewWorkAcceptanceDecision, RecordedWorkAcceptanceDecision,
};
use super::repository::{
    DatabaseWorkRepository, WorkAcceptanceBasisResource, WorkConflictResource, WorkRepositoryError,
    invalid_mutation,
};
use super::{CheckRunId, CriterionRevision, CriterionRevisionRef, WorkContentHash};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{MySql, QueryBuilder, Row, query};
use std::collections::{BTreeMap, BTreeSet};

const ACCEPTANCE_PAYLOAD_SCHEMA_VERSION: u16 = 1;

#[derive(Serialize)]
struct AcceptancePayloadV1<'a> {
    schema_version: u16,
    decision: &'a NewWorkAcceptanceDecision,
}

fn payload_hash(
    decision: &NewWorkAcceptanceDecision,
) -> Result<WorkContentHash, WorkRepositoryError> {
    let payload = super::repository::canonical_json(
        "acceptance-decision payload",
        &AcceptancePayloadV1 {
            schema_version: ACCEPTANCE_PAYLOAD_SCHEMA_VERSION,
            decision,
        },
    )?;
    WorkContentHash::parse(super::repository::content_hash(&payload)).map_err(|message| {
        WorkRepositoryError::corrupt(
            "acceptance-decision payload hash",
            std::io::Error::other(message),
        )
    })
}

async fn find_existing(
    repository: &DatabaseWorkRepository,
    decision: &NewWorkAcceptanceDecision,
    expected_payload_hash: &WorkContentHash,
) -> Result<Option<RecordedWorkAcceptanceDecision>, WorkRepositoryError> {
    let row = query(
        "SELECT payload_hash,
                DATE_FORMAT(decided_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS decided_at
         FROM work_acceptance_decisions
         WHERE owner_id = ? AND work_id = ? AND decision_id = ?
         LIMIT 1",
    )
    .bind(decision.owner_id.as_str())
    .bind(decision.work_id.as_str())
    .bind(decision.decision_id.as_str())
    .fetch_optional(repository.pool.get())
    .await
    .map_err(|source| {
        WorkRepositoryError::persistence("load existing acceptance decision", source)
    })?;
    let Some(row) = row else {
        return Ok(None);
    };
    let stored_hash = row
        .try_get::<String, _>("payload_hash")
        .map_err(|source| WorkRepositoryError::corrupt("acceptance decision", source))?;
    let stored_hash = WorkContentHash::parse(stored_hash).map_err(|message| {
        WorkRepositoryError::corrupt("acceptance decision", std::io::Error::other(message))
    })?;
    if &stored_hash != expected_payload_hash {
        return Err(WorkRepositoryError::Conflict {
            resource: WorkConflictResource::AcceptanceDecisionIdentity,
        });
    }
    let decided_at = super::repository::decode_timestamp(
        "acceptance decision",
        "decided_at",
        row.try_get("decided_at")
            .map_err(|source| WorkRepositoryError::corrupt("acceptance decision", source))?,
    )?;
    Ok(Some(RecordedWorkAcceptanceDecision {
        decision: decision.clone(),
        payload_hash: stored_hash,
        decided_at,
    }))
}

async fn validate_basis(
    repository: &DatabaseWorkRepository,
    decision: &NewWorkAcceptanceDecision,
) -> Result<Vec<CriterionRevisionRef>, WorkRepositoryError> {
    let row = query(
        "SELECT
            w.work_revision,
            w.current_goal_revision,
            w.current_criteria_set_revision,
            CASE WHEN w.archived_at IS NULL THEN 0 ELSE 1 END AS work_archived,
            b.branch_revision,
            b.current_graph_revision,
            b.criteria_set_revision_ref AS branch_criteria_set_revision,
            CASE WHEN b.archived_at IS NULL THEN 0 ELSE 1 END AS branch_archived,
            s.graph_revision AS subject_graph_revision,
            s.subject_ref AS current_subject_ref,
            s.subject_revision AS current_subject_revision,
            cs.member_manifest_json,
            cs.member_count
         FROM works w
         LEFT JOIN work_branches b
           ON b.owner_id = w.owner_id
          AND b.work_id = w.work_id
          AND b.branch_id = ?
         LEFT JOIN work_criterion_sets cs
           ON cs.owner_id = w.owner_id
          AND cs.work_id = w.work_id
          AND cs.revision = ?
         LEFT JOIN work_branch_subjects s
           ON s.owner_id = b.owner_id
          AND s.work_id = b.work_id
          AND s.branch_id = b.branch_id
         WHERE w.owner_id = ? AND w.work_id = ?
         LIMIT 1",
    )
    .bind(decision.branch_id.as_str())
    .bind(decision.criterion_set_revision.get())
    .bind(decision.owner_id.as_str())
    .bind(decision.work_id.as_str())
    .fetch_optional(repository.pool.get())
    .await
    .map_err(|source| {
        WorkRepositoryError::persistence("validate acceptance-decision basis", source)
    })?
    .ok_or(WorkRepositoryError::NotFound)?;
    if row
        .try_get::<i64, _>("work_archived")
        .map_err(|source| WorkRepositoryError::corrupt("Work", source))?
        != 0
    {
        return Err(WorkRepositoryError::Archived);
    }
    let integer = |field: &'static str| {
        row.try_get::<i64, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("acceptance basis", source))
    };
    let optional_integer = |field: &'static str| {
        row.try_get::<Option<i64>, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("acceptance basis", source))
    };
    if optional_integer("branch_revision")?.is_none() {
        return Err(WorkRepositoryError::NotFound);
    }
    if optional_integer("branch_archived")?.unwrap_or(1) != 0 {
        return Err(WorkRepositoryError::Archived);
    }
    for (matches, resource) in [
        (
            integer("work_revision")? == decision.work_revision.get(),
            WorkAcceptanceBasisResource::WorkRevision,
        ),
        (
            integer("current_goal_revision")? == decision.goal_revision.get(),
            WorkAcceptanceBasisResource::GoalRevision,
        ),
        (
            optional_integer("branch_revision")? == Some(decision.branch_revision.get()),
            WorkAcceptanceBasisResource::BranchRevision,
        ),
        (
            optional_integer("current_graph_revision")? == Some(decision.graph_revision.get()),
            WorkAcceptanceBasisResource::GraphRevision,
        ),
        (
            integer("current_criteria_set_revision")? == decision.criterion_set_revision.get(),
            WorkAcceptanceBasisResource::CriterionSetRevision,
        ),
        (
            optional_integer("branch_criteria_set_revision")?
                == Some(decision.criterion_set_revision.get()),
            WorkAcceptanceBasisResource::CriterionSetRevision,
        ),
    ] {
        if !matches {
            return Err(WorkRepositoryError::InvalidAcceptanceBasis { resource });
        }
    }
    let optional_string = |field: &'static str| {
        row.try_get::<Option<String>, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("acceptance basis", source))
    };
    let current_subject_matches = optional_integer("subject_graph_revision")?
        == Some(decision.graph_revision.get())
        && optional_string("current_subject_ref")?.as_deref()
            == Some(decision.subject_ref.as_str())
        && optional_string("current_subject_revision")?.as_deref()
            == Some(decision.subject_revision.as_str());
    if !current_subject_matches {
        return Err(WorkRepositoryError::InvalidAcceptanceBasis {
            resource: WorkAcceptanceBasisResource::Subject,
        });
    }
    let manifest_json = row
        .try_get::<Option<String>, _>("member_manifest_json")
        .map_err(|source| WorkRepositoryError::corrupt("acceptance basis", source))?
        .ok_or(WorkRepositoryError::InvalidAcceptanceBasis {
            resource: WorkAcceptanceBasisResource::CriterionSetRevision,
        })?;
    let member_count =
        optional_integer("member_count")?.ok_or(WorkRepositoryError::InvalidAcceptanceBasis {
            resource: WorkAcceptanceBasisResource::CriterionSetRevision,
        })?;
    let members = super::repository::decode_criterion_set_manifest(&manifest_json, member_count)?;
    if decision
        .accepted_gaps
        .iter()
        .any(|gap| members.binary_search(&gap.criterion).is_err())
    {
        return Err(WorkRepositoryError::InvalidAcceptanceBasis {
            resource: WorkAcceptanceBasisResource::CriterionMembership,
        });
    }
    Ok(members)
}

async fn validate_check_refs(
    repository: &DatabaseWorkRepository,
    decision: &NewWorkAcceptanceDecision,
) -> Result<BTreeMap<CheckRunId, WorkContentHash>, WorkRepositoryError> {
    let expected = decision
        .accepted_gaps
        .iter()
        .flat_map(|gap| {
            gap.check_run_refs
                .iter()
                .cloned()
                .map(|check_id| (check_id, (gap.criterion.clone(), gap.reason)))
        })
        .collect::<BTreeMap<_, _>>();
    if expected.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut builder = QueryBuilder::<MySql>::new(
        "SELECT check_run_id, criterion_id, criterion_revision, graph_revision,
                criterion_set_revision, subject_ref, subject_revision, coverage_state,
                payload_hash
         FROM work_check_runs WHERE owner_id = ",
    );
    builder
        .push_bind(decision.owner_id.as_str())
        .push(" AND work_id = ")
        .push_bind(decision.work_id.as_str())
        .push(" AND branch_id = ")
        .push_bind(decision.branch_id.as_str())
        .push(" AND check_run_id IN (");
    let mut separated = builder.separated(", ");
    for check_id in expected.keys() {
        separated.push_bind(check_id.as_str());
    }
    separated.push_unseparated(")");
    let rows = builder
        .build()
        .fetch_all(repository.pool.get())
        .await
        .map_err(|source| {
            WorkRepositoryError::persistence("validate acceptance evidence refs", source)
        })?;
    let mut found = BTreeSet::new();
    let mut resolved = BTreeMap::new();
    for row in rows {
        let check_id = CheckRunId::parse(
            row.try_get::<String, _>("check_run_id")
                .map_err(|source| WorkRepositoryError::corrupt("check run", source))?,
        )
        .map_err(|source| WorkRepositoryError::corrupt("check run", source))?;
        let criterion = CriterionRevisionRef {
            criterion_id: super::CriterionId::parse(
                row.try_get::<String, _>("criterion_id")
                    .map_err(|source| WorkRepositoryError::corrupt("check run", source))?,
            )
            .map_err(|source| WorkRepositoryError::corrupt("check run", source))?,
            revision: CriterionRevision::new(
                row.try_get::<i64, _>("criterion_revision")
                    .map_err(|source| WorkRepositoryError::corrupt("check run", source))?,
            )
            .map_err(|source| WorkRepositoryError::corrupt("check run", source))?,
        };
        let Some((expected_criterion, reason)) = expected.get(&check_id) else {
            return Err(WorkRepositoryError::corrupt(
                "check run",
                std::io::Error::other("query returned an unexpected check identity"),
            ));
        };
        if expected_criterion != &criterion {
            return Err(WorkRepositoryError::InvalidAcceptanceBasis {
                resource: WorkAcceptanceBasisResource::CheckRunCriterion,
            });
        }
        let graph_revision = row
            .try_get::<i64, _>("graph_revision")
            .map_err(|source| WorkRepositoryError::corrupt("check run", source))?;
        let criterion_set_revision = row
            .try_get::<i64, _>("criterion_set_revision")
            .map_err(|source| WorkRepositoryError::corrupt("check run", source))?;
        let subject_revision = row
            .try_get::<String, _>("subject_revision")
            .map_err(|source| WorkRepositoryError::corrupt("check run", source))?;
        let subject_ref = row
            .try_get::<String, _>("subject_ref")
            .map_err(|source| WorkRepositoryError::corrupt("check run", source))?;
        let coverage = row
            .try_get::<String, _>("coverage_state")
            .map_err(|source| WorkRepositoryError::corrupt("check run", source))?;
        let same_basis = graph_revision == decision.graph_revision.get()
            && criterion_set_revision == decision.criterion_set_revision.get()
            && subject_ref == decision.subject_ref.as_str()
            && subject_revision == decision.subject_revision.as_str();
        let incomplete_coverage = match coverage.as_str() {
            "complete" => false,
            "partial" | "unavailable" => true,
            _ => {
                return Err(WorkRepositoryError::corrupt(
                    "check run",
                    std::io::Error::other("unknown persisted coverage state"),
                ));
            }
        };
        let applicable = match reason {
            AcceptanceGapReason::PartialCoverage => same_basis && incomplete_coverage,
            AcceptanceGapReason::StaleEvidence => !same_basis,
            _ => false,
        };
        if !applicable {
            return Err(WorkRepositoryError::InvalidAcceptanceBasis {
                resource: WorkAcceptanceBasisResource::CheckRunApplicability,
            });
        }
        let check_payload_hash = WorkContentHash::parse(
            row.try_get::<String, _>("payload_hash")
                .map_err(|source| WorkRepositoryError::corrupt("check run", source))?,
        )
        .map_err(|message| {
            WorkRepositoryError::corrupt("check run", std::io::Error::other(message))
        })?;
        resolved.insert(check_id.clone(), check_payload_hash);
        found.insert(check_id);
    }
    let missing = expected
        .keys()
        .filter(|check_id| !found.contains(*check_id))
        .cloned()
        .collect::<Vec<_>>();
    if missing.is_empty() {
        Ok(resolved)
    } else {
        Err(WorkRepositoryError::MissingAcceptanceCheckRuns { missing })
    }
}

#[derive(Serialize)]
struct ResolvedAcceptanceCheckRef<'a> {
    check_run_id: &'a CheckRunId,
    payload_hash: &'a WorkContentHash,
}

#[derive(Serialize)]
struct ResolvedAcceptanceFactV1<'a> {
    schema_version: u16,
    decision: &'a NewWorkAcceptanceDecision,
    resolved_check_refs: Vec<ResolvedAcceptanceCheckRef<'a>>,
}

fn acceptance_fact_hash(
    decision: &NewWorkAcceptanceDecision,
    check_payload_hashes: &BTreeMap<CheckRunId, WorkContentHash>,
) -> Result<WorkContentHash, WorkRepositoryError> {
    let resolved_check_refs = check_payload_hashes
        .iter()
        .map(|(check_run_id, payload_hash)| ResolvedAcceptanceCheckRef {
            check_run_id,
            payload_hash,
        })
        .collect();
    let canonical = super::repository::canonical_json(
        "resolved acceptance fact",
        &ResolvedAcceptanceFactV1 {
            schema_version: 1,
            decision,
            resolved_check_refs,
        },
    )?;
    WorkContentHash::parse(super::repository::content_hash(&canonical)).map_err(|message| {
        WorkRepositoryError::corrupt(
            "resolved acceptance fact hash",
            std::io::Error::other(message),
        )
    })
}

struct CurrentGapProjection<'a> {
    gap: &'a super::AcceptedCriterionGap,
    resolved_check_refs_json: String,
}

async fn upsert_current_gap_acceptances(
    transaction: &mut sqlx::Transaction<'_, MySql>,
    decision: &NewWorkAcceptanceDecision,
    current_criteria: &[CriterionRevisionRef],
    decision_event_seq: super::WorkEventSeq,
    acceptance_fact_hash: &WorkContentHash,
    decided_at: DateTime<Utc>,
    check_payload_hashes: &BTreeMap<CheckRunId, WorkContentHash>,
) -> Result<(), WorkRepositoryError> {
    let mut prune =
        QueryBuilder::<MySql>::new("DELETE FROM work_current_gap_acceptances WHERE owner_id = ");
    prune
        .push_bind(decision.owner_id.as_str())
        .push(" AND work_id = ")
        .push_bind(decision.work_id.as_str())
        .push(" AND branch_id = ")
        .push_bind(decision.branch_id.as_str())
        .push(" AND NOT (");
    for (index, criterion) in current_criteria.iter().enumerate() {
        if index > 0 {
            prune.push(" OR ");
        }
        prune
            .push("(criterion_id = ")
            .push_bind(criterion.criterion_id.as_str())
            .push(" AND criterion_revision = ")
            .push_bind(criterion.revision.get())
            .push(")");
    }
    prune.push(")");
    prune
        .build()
        .execute(&mut **transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::persistence("prune obsolete current accepted gaps", source)
        })?;

    let rows = decision
        .accepted_gaps
        .iter()
        .map(|gap| {
            let resolved = gap
                .check_run_refs
                .iter()
                .map(|check_run_id| {
                    let payload_hash = check_payload_hashes.get(check_run_id).ok_or_else(|| {
                        WorkRepositoryError::corrupt(
                            "acceptance check resolution",
                            std::io::Error::other("validated check hash is missing"),
                        )
                    })?;
                    Ok(ResolvedAcceptanceCheckRef {
                        check_run_id,
                        payload_hash,
                    })
                })
                .collect::<Result<Vec<_>, WorkRepositoryError>>()?;
            Ok(CurrentGapProjection {
                gap,
                resolved_check_refs_json: super::repository::canonical_json(
                    "resolved acceptance check references",
                    &resolved,
                )?,
            })
        })
        .collect::<Result<Vec<_>, WorkRepositoryError>>()?;
    let mut builder = QueryBuilder::<MySql>::new(
        "INSERT INTO work_current_gap_acceptances
         (owner_id, work_id, branch_id, criterion_id, criterion_revision,
          decision_id, decision_event_seq, work_revision, goal_revision, branch_revision,
          graph_revision, criterion_set_revision, subject_ref, subject_revision, gap_reason,
          resolved_check_refs_json, resolved_check_ref_count, decision_payload_hash,
          source_cursor, decided_by_id, decided_at, updated_at) ",
    );
    builder.push_values(rows.iter(), |mut values, row| {
        values
            .push_bind(decision.owner_id.as_str())
            .push_bind(decision.work_id.as_str())
            .push_bind(decision.branch_id.as_str())
            .push_bind(row.gap.criterion.criterion_id.as_str())
            .push_bind(row.gap.criterion.revision.get())
            .push_bind(decision.decision_id.as_str())
            .push_bind(decision_event_seq.get())
            .push_bind(decision.work_revision.get())
            .push_bind(decision.goal_revision.get())
            .push_bind(decision.branch_revision.get())
            .push_bind(decision.graph_revision.get())
            .push_bind(decision.criterion_set_revision.get())
            .push_bind(decision.subject_ref.as_str())
            .push_bind(decision.subject_revision.as_str())
            .push_bind(row.gap.reason.as_str())
            .push_bind(&row.resolved_check_refs_json)
            .push_bind(
                i32::try_from(row.gap.check_run_refs.len()).expect("bounded check reference count"),
            )
            .push_bind(acceptance_fact_hash.as_str())
            .push_bind(decision.source_cursor.as_str())
            .push_bind(decision.owner_id.as_str())
            .push_bind(decided_at.naive_utc())
            .push_bind(decided_at.naive_utc());
    });
    builder.push(
        " ON DUPLICATE KEY UPDATE
          criterion_revision = VALUES(criterion_revision),
          decision_id = VALUES(decision_id),
          decision_event_seq = VALUES(decision_event_seq),
          work_revision = VALUES(work_revision),
          goal_revision = VALUES(goal_revision),
          branch_revision = VALUES(branch_revision),
          graph_revision = VALUES(graph_revision),
          criterion_set_revision = VALUES(criterion_set_revision),
          subject_ref = VALUES(subject_ref),
          subject_revision = VALUES(subject_revision),
          gap_reason = VALUES(gap_reason),
          resolved_check_refs_json = VALUES(resolved_check_refs_json),
          resolved_check_ref_count = VALUES(resolved_check_ref_count),
          decision_payload_hash = VALUES(decision_payload_hash),
          source_cursor = VALUES(source_cursor),
          decided_by_id = VALUES(decided_by_id),
          decided_at = VALUES(decided_at),
          updated_at = VALUES(updated_at)",
    );
    builder
        .build()
        .execute(&mut **transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::persistence("upsert current accepted criterion gaps", source)
        })?;
    Ok(())
}

pub(super) async fn accept_gaps(
    repository: &DatabaseWorkRepository,
    decision: NewWorkAcceptanceDecision,
) -> Result<RecordedWorkAcceptanceDecision, WorkRepositoryError> {
    let decision = decision.canonicalized().map_err(invalid_mutation)?;
    let payload_hash = payload_hash(&decision)?;
    if let Some(existing) = find_existing(repository, &decision, &payload_hash).await? {
        return Ok(existing);
    }
    let current_criteria = validate_basis(repository, &decision).await?;
    let check_payload_hashes = validate_check_refs(repository, &decision).await?;
    let resolved_acceptance_fact_hash = acceptance_fact_hash(&decision, &check_payload_hashes)?;
    let accepted_gaps_json =
        super::repository::canonical_json("accepted criterion gaps", &decision.accepted_gaps)?;
    let check_ref_count = decision
        .accepted_gaps
        .iter()
        .map(|gap| gap.check_run_refs.len())
        .sum::<usize>();
    let decided_at = DateTime::<Utc>::from_timestamp_micros(Utc::now().timestamp_micros())
        .expect("current timestamp");
    let event_source =
        super::WorkChangeRef::parse(decision.decision_id.as_str()).map_err(invalid_mutation)?;
    let mut transaction = repository.pool.get().begin().await.map_err(|source| {
        WorkRepositoryError::persistence("begin acceptance event transaction", source)
    })?;

    let result = query(
        "INSERT INTO work_acceptance_decisions
         (owner_id, work_id, decision_id, branch_id, work_revision, goal_revision,
          branch_revision, graph_revision, criterion_set_revision, subject_ref,
          subject_revision, accepted_gaps_json, accepted_gap_count, check_ref_count,
          source_cursor, decided_by_id, decided_at, payload_hash)
         SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?
         FROM works w
         JOIN work_branches b
           ON b.owner_id = w.owner_id
          AND b.work_id = w.work_id
          AND b.branch_id = ?
         JOIN work_branch_subjects s
           ON s.owner_id = b.owner_id
          AND s.work_id = b.work_id
          AND s.branch_id = b.branch_id
         WHERE w.owner_id = ? AND w.work_id = ?
           AND w.archived_at IS NULL AND b.archived_at IS NULL
           AND w.work_revision = ? AND w.current_goal_revision = ?
           AND w.current_criteria_set_revision = ?
           AND b.branch_revision = ? AND b.current_graph_revision = ?
           AND s.graph_revision = ? AND s.subject_ref = ? AND s.subject_revision = ?",
    )
    .bind(decision.owner_id.as_str())
    .bind(decision.work_id.as_str())
    .bind(decision.decision_id.as_str())
    .bind(decision.branch_id.as_str())
    .bind(decision.work_revision.get())
    .bind(decision.goal_revision.get())
    .bind(decision.branch_revision.get())
    .bind(decision.graph_revision.get())
    .bind(decision.criterion_set_revision.get())
    .bind(decision.subject_ref.as_str())
    .bind(decision.subject_revision.as_str())
    .bind(accepted_gaps_json)
    .bind(i32::try_from(decision.accepted_gaps.len()).expect("bounded gap count"))
    .bind(i32::try_from(check_ref_count).expect("bounded check ref count"))
    .bind(decision.source_cursor.as_str())
    .bind(decision.owner_id.as_str())
    .bind(decided_at.naive_utc())
    .bind(payload_hash.as_str())
    .bind(decision.branch_id.as_str())
    .bind(decision.owner_id.as_str())
    .bind(decision.work_id.as_str())
    .bind(decision.work_revision.get())
    .bind(decision.goal_revision.get())
    .bind(decision.criterion_set_revision.get())
    .bind(decision.branch_revision.get())
    .bind(decision.graph_revision.get())
    .bind(decision.graph_revision.get())
    .bind(decision.subject_ref.as_str())
    .bind(decision.subject_revision.as_str())
    .execute(&mut *transaction)
    .await;

    match result {
        Ok(result) if result.rows_affected() == 1 => {
            let event_result = super::events_repository::append_event(
                &mut transaction,
                &super::events::NewWorkEvent {
                    owner_id: decision.owner_id.clone(),
                    work_id: decision.work_id.clone(),
                    branch_id: Some(decision.branch_id.clone()),
                    kind: super::WorkEventKind::GapsAccepted,
                    work_revision: Some(decision.work_revision),
                    goal_revision: Some(decision.goal_revision),
                    criterion_set_revision: Some(decision.criterion_set_revision),
                    branch_revision: Some(decision.branch_revision),
                    graph_revision: Some(decision.graph_revision),
                    source_ref: event_source,
                },
            )
            .await;
            let decision_event_seq = match event_result {
                Ok(event_seq) => event_seq,
                Err(error) => {
                    return Err(super::repository::rollback_transaction(
                        transaction,
                        "rollback acceptance event transaction",
                        error,
                    )
                    .await);
                }
            };
            if let Err(error) = upsert_current_gap_acceptances(
                &mut transaction,
                &decision,
                &current_criteria,
                decision_event_seq,
                &resolved_acceptance_fact_hash,
                decided_at,
                &check_payload_hashes,
            )
            .await
            {
                return Err(super::repository::rollback_transaction(
                    transaction,
                    "rollback current gap acceptance transaction",
                    error,
                )
                .await);
            }
            transaction.commit().await.map_err(|source| {
                WorkRepositoryError::persistence("commit acceptance event transaction", source)
            })?;
            Ok(RecordedWorkAcceptanceDecision {
                decision,
                payload_hash,
                decided_at,
            })
        }
        Ok(_) => {
            transaction.rollback().await.map_err(|source| {
                WorkRepositoryError::persistence("rollback stale acceptance transaction", source)
            })?;
            if let Some(existing) = find_existing(repository, &decision, &payload_hash).await? {
                return Ok(existing);
            }
            validate_basis(repository, &decision).await?;
            Err(WorkRepositoryError::InvalidAcceptanceBasis {
                resource: WorkAcceptanceBasisResource::WorkRevision,
            })
        }
        Err(source)
            if source
                .as_database_error()
                .is_some_and(|error| error.is_unique_violation()) =>
        {
            transaction.rollback().await.map_err(|rollback_error| {
                WorkRepositoryError::persistence(
                    "rollback duplicate acceptance transaction",
                    rollback_error,
                )
            })?;
            find_existing(repository, &decision, &payload_hash)
                .await?
                .ok_or(WorkRepositoryError::Conflict {
                    resource: WorkConflictResource::AcceptanceDecisionIdentity,
                })
        }
        Err(source) => {
            let error = WorkRepositoryError::persistence("insert acceptance decision", source);
            Err(super::repository::rollback_transaction(
                transaction,
                "rollback failed acceptance transaction",
                error,
            )
            .await)
        }
    }
}
