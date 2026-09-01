use super::WorkContentHash;
use super::evidence::{CheckVerifierKind, NewWorkCheckRun, RecordedWorkCheckRun};
use super::repository::{
    DatabaseWorkRepository, WorkCheckBasisResource, WorkConflictResource, WorkRepositoryError,
    invalid_mutation,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{Row, query};

const CHECK_RUN_PAYLOAD_SCHEMA_VERSION: u16 = 1;

#[derive(Serialize)]
struct CheckRunPayloadV1<'a> {
    schema_version: u16,
    check: &'a NewWorkCheckRun,
    criterion_definition_hash: &'a WorkContentHash,
}

struct CheckBasis {
    criterion_definition_hash: WorkContentHash,
}

async fn validate_check_basis(
    repository: &DatabaseWorkRepository,
    check: &NewWorkCheckRun,
) -> Result<CheckBasis, WorkRepositoryError> {
    let row = query(
        "SELECT
            CASE WHEN w.archived_at IS NULL THEN 0 ELSE 1 END AS work_archived,
            b.branch_id AS basis_branch_id,
            CASE WHEN b.archived_at IS NULL THEN 0 ELSE 1 END AS branch_archived,
            b.current_graph_revision AS branch_current_graph_revision,
            b.criteria_set_revision_ref AS branch_criteria_set_revision,
            s.graph_revision AS subject_graph_revision,
            s.subject_ref AS current_subject_ref,
            s.subject_revision AS current_subject_revision,
            gr.revision AS basis_graph_revision,
            gr.item_revision_manifest_json,
            gr.item_count,
            gr.edge_manifest_json,
            gr.edge_count,
            gr.manifest_hash,
            ir.revision AS basis_item_revision,
            ir.declaration_state AS basis_item_declaration_state,
            r.run_id AS basis_run_id,
            r.work_id AS run_work_id,
            r.work_branch_id AS run_branch_id,
            r.work_graph_revision AS run_graph_revision,
            r.work_item_id AS run_item_id,
            r.work_item_revision AS run_item_revision,
            r.work_item_attempt_id AS run_item_attempt_id,
            cs.revision AS basis_criterion_set_revision,
            cs.member_manifest_json,
            cs.member_count,
            cr.revision AS basis_criterion_revision,
            cr.criterion_kind,
            cr.definition_hash
         FROM works w
         LEFT JOIN work_branches b
           ON b.owner_id = w.owner_id
          AND b.work_id = w.work_id
          AND b.branch_id = ?
         LEFT JOIN work_graph_revisions gr
           ON gr.owner_id = w.owner_id
          AND gr.work_id = w.work_id
          AND gr.revision = ?
         LEFT JOIN work_item_revisions ir
           ON ir.owner_id = w.owner_id
          AND ir.work_id = w.work_id
          AND ir.item_id = ?
          AND ir.revision = ?
         LEFT JOIN agent_runs r
           ON r.user_id = w.owner_id
          AND r.run_id = ?
         LEFT JOIN work_branch_subjects s
           ON s.owner_id = b.owner_id
          AND s.work_id = b.work_id
          AND s.branch_id = b.branch_id
         LEFT JOIN work_criterion_sets cs
           ON cs.owner_id = w.owner_id
          AND cs.work_id = w.work_id
          AND cs.revision = ?
         LEFT JOIN work_criterion_revisions cr
           ON cr.owner_id = w.owner_id
          AND cr.work_id = w.work_id
          AND cr.criterion_id = ?
          AND cr.revision = ?
         WHERE w.owner_id = ? AND w.work_id = ?
         LIMIT 1",
    )
    .bind(check.branch_id.as_str())
    .bind(check.graph_revision.get())
    .bind(check.item.item_id.as_str())
    .bind(check.item.revision.get())
    .bind(check.run_ref.as_str())
    .bind(check.criterion_set_revision.get())
    .bind(check.criterion.criterion_id.as_str())
    .bind(check.criterion.revision.get())
    .bind(check.owner_id.as_str())
    .bind(check.work_id.as_str())
    .fetch_optional(repository.pool.get())
    .await
    .map_err(|source| WorkRepositoryError::persistence("validate check-run basis", source))?
    .ok_or(WorkRepositoryError::NotFound)?;

    if row
        .try_get::<i64, _>("work_archived")
        .map_err(|source| WorkRepositoryError::corrupt("Work", source))?
        != 0
    {
        return Err(WorkRepositoryError::Archived);
    }
    let optional_string = |field: &'static str| {
        row.try_get::<Option<String>, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("check-run basis", source))
    };
    let optional_i64 = |field: &'static str| {
        row.try_get::<Option<i64>, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("check-run basis", source))
    };
    let optional_i32 = |field: &'static str| {
        row.try_get::<Option<i32>, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("check-run basis", source))
    };
    if optional_string("basis_branch_id")?.is_none() {
        return Err(WorkRepositoryError::InvalidCheckBasis {
            resource: WorkCheckBasisResource::Branch,
        });
    }
    if optional_i64("branch_archived")?.unwrap_or(1) != 0 {
        return Err(WorkRepositoryError::Archived);
    }
    let current_graph_revision = optional_i64("branch_current_graph_revision")?
        .ok_or(WorkRepositoryError::InvalidCheckBasis {
            resource: WorkCheckBasisResource::Branch,
        })
        .and_then(|value| {
            super::GraphRevision::new(value)
                .map_err(|source| WorkRepositoryError::corrupt("Work branch", source))
        })?;
    if current_graph_revision != check.graph_revision {
        return Err(WorkRepositoryError::StaleCheckGraphRevision {
            evidence_graph_revision: check.graph_revision,
            current_graph_revision,
        });
    }
    if optional_i64("branch_criteria_set_revision")? != Some(check.criterion_set_revision.get()) {
        return Err(WorkRepositoryError::InvalidCheckBasis {
            resource: WorkCheckBasisResource::CriterionSetRevision,
        });
    }
    let current_subject_matches = optional_i64("subject_graph_revision")?
        == Some(check.graph_revision.get())
        && optional_string("current_subject_ref")?.as_deref() == Some(check.subject_ref.as_str())
        && optional_string("current_subject_revision")?.as_deref()
            == Some(check.subject_revision.as_str());
    if !current_subject_matches {
        return Err(WorkRepositoryError::InvalidCheckBasis {
            resource: WorkCheckBasisResource::Subject,
        });
    }
    if optional_i64("basis_graph_revision")?.is_none() {
        return Err(WorkRepositoryError::InvalidCheckBasis {
            resource: WorkCheckBasisResource::GraphRevision,
        });
    }
    let graph = super::graph_repository::decode_persisted_graph(
        &optional_string("item_revision_manifest_json")?.ok_or(
            WorkRepositoryError::InvalidCheckBasis {
                resource: WorkCheckBasisResource::GraphRevision,
            },
        )?,
        optional_i32("item_count")?.ok_or(WorkRepositoryError::InvalidCheckBasis {
            resource: WorkCheckBasisResource::GraphRevision,
        })?,
        &optional_string("edge_manifest_json")?.ok_or(WorkRepositoryError::InvalidCheckBasis {
            resource: WorkCheckBasisResource::GraphRevision,
        })?,
        optional_i32("edge_count")?.ok_or(WorkRepositoryError::InvalidCheckBasis {
            resource: WorkCheckBasisResource::GraphRevision,
        })?,
    )?;
    super::graph_repository::validate_persisted_graph_hash(
        &graph,
        &optional_string("manifest_hash")?.ok_or(WorkRepositoryError::InvalidCheckBasis {
            resource: WorkCheckBasisResource::GraphRevision,
        })?,
    )?;
    let item_is_current = graph.item_refs.binary_search(&check.item).is_ok()
        && optional_i64("basis_item_revision")? == Some(check.item.revision.get())
        && optional_string("basis_item_declaration_state")?.as_deref() == Some("active");
    if !item_is_current {
        return Err(WorkRepositoryError::InvalidCheckBasis {
            resource: WorkCheckBasisResource::WorkItemRevision,
        });
    }
    let run_is_exact = optional_string("basis_run_id")?.as_deref() == Some(check.run_ref.as_str())
        && optional_string("run_work_id")?.as_deref() == Some(check.work_id.as_str())
        && optional_string("run_branch_id")?.as_deref() == Some(check.branch_id.as_str())
        && optional_i64("run_graph_revision")? == Some(check.graph_revision.get())
        && optional_string("run_item_id")?.as_deref() == Some(check.item.item_id.as_str())
        && optional_i64("run_item_revision")? == Some(check.item.revision.get())
        && optional_string("run_item_attempt_id")?.as_deref() == Some(check.attempt_id.as_str());
    if !run_is_exact {
        return Err(WorkRepositoryError::InvalidCheckBasis {
            resource: WorkCheckBasisResource::RunBinding,
        });
    }
    if optional_i64("basis_criterion_set_revision")?.is_none() {
        return Err(WorkRepositoryError::InvalidCheckBasis {
            resource: WorkCheckBasisResource::CriterionSetRevision,
        });
    }
    if optional_i64("basis_criterion_revision")?.is_none() {
        return Err(WorkRepositoryError::InvalidCheckBasis {
            resource: WorkCheckBasisResource::CriterionRevision,
        });
    }

    let manifest_json = optional_string("member_manifest_json")?.ok_or_else(|| {
        WorkRepositoryError::InvalidCheckBasis {
            resource: WorkCheckBasisResource::CriterionSetRevision,
        }
    })?;
    let member_count =
        optional_i64("member_count")?.ok_or_else(|| WorkRepositoryError::InvalidCheckBasis {
            resource: WorkCheckBasisResource::CriterionSetRevision,
        })?;
    let members = super::repository::decode_criterion_set_manifest(&manifest_json, member_count)?;
    let is_member = members.binary_search(&check.criterion).is_ok();
    if !is_member {
        return Err(WorkRepositoryError::InvalidCheckBasis {
            resource: WorkCheckBasisResource::CriterionSetMembership,
        });
    }

    let criterion_kind = optional_string("criterion_kind")?.ok_or_else(|| {
        WorkRepositoryError::InvalidCheckBasis {
            resource: WorkCheckBasisResource::CriterionRevision,
        }
    })?;
    let expected_verifier = match criterion_kind.as_str() {
        "command_check" => CheckVerifierKind::Command,
        "test_check" => CheckVerifierKind::Test,
        "artifact_check" => {
            return Err(verifier_mismatch("artifact_check", check.verifier_kind));
        }
        "state_check" => return Err(verifier_mismatch("state_check", check.verifier_kind)),
        "human_review" => return Err(verifier_mismatch("human_review", check.verifier_kind)),
        "model_assessment" => {
            return Err(verifier_mismatch("model_assessment", check.verifier_kind));
        }
        _ => {
            return Err(WorkRepositoryError::corrupt(
                "criterion revision",
                std::io::Error::other("unknown persisted criterion kind"),
            ));
        }
    };
    if expected_verifier != check.verifier_kind {
        return Err(verifier_mismatch(
            match expected_verifier {
                CheckVerifierKind::Command => "command_check",
                CheckVerifierKind::Test => "test_check",
            },
            check.verifier_kind,
        ));
    }
    let definition_hash = optional_string("definition_hash")?.ok_or_else(|| {
        WorkRepositoryError::InvalidCheckBasis {
            resource: WorkCheckBasisResource::CriterionRevision,
        }
    })?;
    Ok(CheckBasis {
        criterion_definition_hash: WorkContentHash::parse(definition_hash).map_err(|message| {
            WorkRepositoryError::corrupt("criterion revision", std::io::Error::other(message))
        })?,
    })
}

fn verifier_mismatch(
    criterion_kind: &'static str,
    verifier_kind: CheckVerifierKind,
) -> WorkRepositoryError {
    WorkRepositoryError::CheckVerifierMismatch {
        criterion_kind,
        verifier_kind: verifier_kind.as_str(),
    }
}

fn payload_hash(
    check: &NewWorkCheckRun,
    criterion_definition_hash: &WorkContentHash,
) -> Result<WorkContentHash, WorkRepositoryError> {
    let payload_json = super::repository::canonical_json(
        "check-run payload",
        &CheckRunPayloadV1 {
            schema_version: CHECK_RUN_PAYLOAD_SCHEMA_VERSION,
            check,
            criterion_definition_hash,
        },
    )?;
    WorkContentHash::parse(super::repository::content_hash(&payload_json)).map_err(|message| {
        WorkRepositoryError::corrupt("check-run payload hash", std::io::Error::other(message))
    })
}

async fn find_existing(
    repository: &DatabaseWorkRepository,
    check: &NewWorkCheckRun,
) -> Result<Option<RecordedWorkCheckRun>, WorkRepositoryError> {
    let row = query(
        "SELECT criterion_definition_hash, payload_hash,
                DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS created_at
         FROM work_check_runs
         WHERE owner_id = ? AND work_id = ? AND check_run_id = ?
         LIMIT 1",
    )
    .bind(check.owner_id.as_str())
    .bind(check.work_id.as_str())
    .bind(check.check_run_id.as_str())
    .fetch_optional(repository.pool.get())
    .await
    .map_err(|source| WorkRepositoryError::persistence("load existing check run", source))?;
    let Some(row) = row else {
        return Ok(None);
    };
    let definition_hash = row
        .try_get::<String, _>("criterion_definition_hash")
        .map_err(|source| WorkRepositoryError::corrupt("check run", source))?;
    let definition_hash = WorkContentHash::parse(definition_hash).map_err(|message| {
        WorkRepositoryError::corrupt("check run", std::io::Error::other(message))
    })?;
    let expected_payload_hash = payload_hash(check, &definition_hash)?;
    let payload_hash = row
        .try_get::<String, _>("payload_hash")
        .map_err(|source| WorkRepositoryError::corrupt("check run", source))?;
    let payload_hash = WorkContentHash::parse(payload_hash).map_err(|message| {
        WorkRepositoryError::corrupt("check run", std::io::Error::other(message))
    })?;
    if payload_hash != expected_payload_hash {
        return Err(WorkRepositoryError::Conflict {
            resource: WorkConflictResource::CheckRunIdentity,
        });
    }
    let created_at = super::repository::decode_timestamp(
        "check run",
        "created_at",
        row.try_get("created_at")
            .map_err(|source| WorkRepositoryError::corrupt("check run", source))?,
    )?;
    Ok(Some(RecordedWorkCheckRun {
        check: check.clone(),
        payload_hash,
        created_at,
    }))
}

pub(super) async fn record_check_run(
    repository: &DatabaseWorkRepository,
    check: NewWorkCheckRun,
) -> Result<RecordedWorkCheckRun, WorkRepositoryError> {
    let check = check.canonicalized().map_err(invalid_mutation)?;
    if let Some(existing) = find_existing(repository, &check).await? {
        return Ok(existing);
    }
    let basis = validate_check_basis(repository, &check).await?;
    let payload_hash = payload_hash(&check, &basis.criterion_definition_hash)?;
    let coverage_gaps_json =
        super::repository::canonical_json("check-run coverage gaps", &check.coverage_gaps)?;
    let evidence_refs_json =
        super::repository::canonical_json("check-run evidence references", &check.evidence_refs)?;
    let created_at = DateTime::<Utc>::from_timestamp_micros(Utc::now().timestamp_micros())
        .expect("current timestamp");
    let event_source =
        super::WorkChangeRef::parse(check.check_run_id.as_str()).map_err(invalid_mutation)?;
    let mut transaction = repository.pool.get().begin().await.map_err(|source| {
        WorkRepositoryError::persistence("begin check-run event transaction", source)
    })?;

    let result = query(
        "INSERT INTO work_check_runs
         (owner_id, work_id, check_run_id, branch_id, graph_revision,
          work_item_id, work_item_revision, work_item_attempt_id,
          criterion_set_revision, criterion_id, criterion_revision, criterion_definition_hash,
          subject_ref, subject_revision, artifact_digest, run_ref, invocation_ref,
          verifier_kind, verifier_fingerprint, environment_fingerprint,
          outcome, error_kind, coverage_state, coverage_gaps_json, coverage_gap_count,
          evidence_refs_json, evidence_ref_count, source_cursor, produced_at, expires_at,
          payload_hash, created_at)
         SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?
         FROM work_branches b
         JOIN works w
           ON w.owner_id = b.owner_id AND w.work_id = b.work_id
         JOIN work_branch_subjects s
           ON s.owner_id = b.owner_id AND s.work_id = b.work_id AND s.branch_id = b.branch_id
         WHERE b.owner_id = ? AND b.work_id = ? AND b.branch_id = ?
           AND w.archived_at IS NULL AND b.archived_at IS NULL
           AND b.current_graph_revision = ?
           AND s.graph_revision = ? AND s.subject_ref = ? AND s.subject_revision = ?
           AND EXISTS (
             SELECT 1 FROM agent_runs r
             WHERE r.user_id = ? AND r.run_id = ?
               AND r.work_id = ? AND r.work_branch_id = ? AND r.work_graph_revision = ?
               AND r.work_item_id = ? AND r.work_item_revision = ?
               AND r.work_item_attempt_id = ?
           )",
    )
    .bind(check.owner_id.as_str())
    .bind(check.work_id.as_str())
    .bind(check.check_run_id.as_str())
    .bind(check.branch_id.as_str())
    .bind(check.graph_revision.get())
    .bind(check.item.item_id.as_str())
    .bind(check.item.revision.get())
    .bind(check.attempt_id.as_str())
    .bind(check.criterion_set_revision.get())
    .bind(check.criterion.criterion_id.as_str())
    .bind(check.criterion.revision.get())
    .bind(basis.criterion_definition_hash.as_str())
    .bind(check.subject_ref.as_str())
    .bind(check.subject_revision.as_str())
    .bind(check.artifact_digest.as_ref().map(WorkContentHash::as_str))
    .bind(check.run_ref.as_str())
    .bind(check.invocation_ref.as_str())
    .bind(check.verifier_kind.as_str())
    .bind(check.verifier_fingerprint.as_str())
    .bind(check.environment_fingerprint.as_str())
    .bind(check.outcome.as_str())
    .bind(check.error_kind.map(|kind| kind.as_str()))
    .bind(check.coverage.as_str())
    .bind(coverage_gaps_json)
    .bind(i32::try_from(check.coverage_gaps.len()).expect("bounded gap count"))
    .bind(evidence_refs_json)
    .bind(i32::try_from(check.evidence_refs.len()).expect("bounded evidence count"))
    .bind(check.source_cursor.as_str())
    .bind(check.produced_at.naive_utc())
    .bind(check.expires_at.map(|value| value.naive_utc()))
    .bind(payload_hash.as_str())
    .bind(created_at.naive_utc())
    .bind(check.owner_id.as_str())
    .bind(check.work_id.as_str())
    .bind(check.branch_id.as_str())
    .bind(check.graph_revision.get())
    .bind(check.graph_revision.get())
    .bind(check.subject_ref.as_str())
    .bind(check.subject_revision.as_str())
    .bind(check.owner_id.as_str())
    .bind(check.run_ref.as_str())
    .bind(check.work_id.as_str())
    .bind(check.branch_id.as_str())
    .bind(check.graph_revision.get())
    .bind(check.item.item_id.as_str())
    .bind(check.item.revision.get())
    .bind(check.attempt_id.as_str())
    .execute(&mut *transaction)
    .await;

    match result {
        Ok(result) => {
            if result.rows_affected() != 1 {
                transaction.rollback().await.map_err(|source| {
                    WorkRepositoryError::persistence("rollback stale check-run transaction", source)
                })?;
                if let Some(existing) = find_existing(repository, &check).await? {
                    return Ok(existing);
                }
                // Classify a concurrent graph/subject change with the same
                // typed errors as the initial admission. A still-valid basis
                // after a zero-row INSERT SELECT is storage corruption.
                validate_check_basis(repository, &check).await?;
                return Err(WorkRepositoryError::corrupt(
                    "check-run admission",
                    std::io::Error::other("conditional insert missed an unchanged basis"),
                ));
            }
            let event_result = super::events_repository::append_event(
                &mut transaction,
                &super::events::NewWorkEvent {
                    owner_id: check.owner_id.clone(),
                    work_id: check.work_id.clone(),
                    branch_id: Some(check.branch_id.clone()),
                    kind: super::WorkEventKind::CheckRecorded,
                    work_revision: None,
                    goal_revision: None,
                    criterion_set_revision: Some(check.criterion_set_revision),
                    branch_revision: None,
                    graph_revision: Some(check.graph_revision),
                    source_ref: event_source,
                },
            )
            .await;
            if let Err(error) = event_result {
                return Err(super::repository::rollback_transaction(
                    transaction,
                    "rollback check-run event transaction",
                    error,
                )
                .await);
            }
            transaction.commit().await.map_err(|source| {
                WorkRepositoryError::persistence("commit check-run event transaction", source)
            })?;
            Ok(RecordedWorkCheckRun {
                check,
                payload_hash,
                created_at,
            })
        }
        Err(source)
            if source
                .as_database_error()
                .is_some_and(|error| error.is_unique_violation()) =>
        {
            transaction.rollback().await.map_err(|rollback_error| {
                WorkRepositoryError::persistence(
                    "rollback duplicate check-run event transaction",
                    rollback_error,
                )
            })?;
            find_existing(repository, &check)
                .await?
                .ok_or(WorkRepositoryError::Conflict {
                    resource: WorkConflictResource::CheckRunIdentity,
                })
        }
        Err(source) => {
            let error = WorkRepositoryError::persistence("insert check run", source);
            Err(super::repository::rollback_transaction(
                transaction,
                "rollback failed check-run transaction",
                error,
            )
            .await)
        }
    }
}
