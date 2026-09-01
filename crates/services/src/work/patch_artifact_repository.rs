use super::events::NewWorkEvent;
use super::repository::{DatabaseWorkRepository, WorkRepositoryError};
use super::{
    GraphRevision, InternalSessionId, NewWorkPatchArtifact, WorkBranchId, WorkBranchRevision,
    WorkBranchSubjectRevision, WorkContentHash, WorkEventKind, WorkId, WorkOwnerId,
    WorkPatchArtifact, WorkPatchArtifactBasisResource, WorkPatchArtifactId, WorkPatchFormat,
    WorkProviderInvocationRef, WorkSubjectRef,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{MySql, Row, Transaction, query};

fn conflict(resource: WorkPatchArtifactBasisResource) -> WorkRepositoryError {
    WorkRepositoryError::PatchArtifactConflict { resource }
}

fn repair(error: impl std::fmt::Display) -> WorkRepositoryError {
    WorkRepositoryError::corrupt(
        "Work patch artifact",
        std::io::Error::other(error.to_string()),
    )
}

fn decode_patch(row: &sqlx::mysql::MySqlRow) -> Result<WorkPatchArtifact, WorkRepositoryError> {
    let text = |field: &'static str| {
        row.try_get::<String, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("Work patch artifact", source))
    };
    let integer = |field: &'static str| {
        row.try_get::<i64, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("Work patch artifact", source))
    };
    let payload_bytes = u64::try_from(integer("payload_bytes")?)
        .map_err(|_| repair("payload_bytes is negative"))?;
    Ok(WorkPatchArtifact {
        schema_version: super::WORK_PATCH_ARTIFACT_SCHEMA_VERSION,
        work_id: WorkId::parse(text("work_id")?).map_err(repair)?,
        branch_id: WorkBranchId::parse(text("branch_id")?).map_err(repair)?,
        patch_artifact_id: WorkPatchArtifactId::parse(text("patch_artifact_id")?)
            .map_err(repair)?,
        session_id: InternalSessionId::parse(text("session_id")?).map_err(repair)?,
        payload_artifact_id: text("payload_artifact_id")?,
        source_branch_revision: WorkBranchRevision::new(integer("source_branch_revision")?)
            .map_err(repair)?,
        source_graph_revision: GraphRevision::new(integer("source_graph_revision")?)
            .map_err(repair)?,
        source_subject_record_revision: WorkBranchSubjectRevision::new(integer(
            "source_subject_record_revision",
        )?)
        .map_err(repair)?,
        subject_ref: WorkSubjectRef::parse(text("subject_ref")?).map_err(repair)?,
        base_subject_revision: WorkContentHash::parse(text("base_subject_revision")?)
            .map_err(repair)?,
        result_subject_revision: WorkContentHash::parse(text("result_subject_revision")?)
            .map_err(repair)?,
        payload_hash: WorkContentHash::parse(text("payload_hash")?).map_err(repair)?,
        payload_bytes,
        format: WorkPatchFormat::from_persisted(&text("patch_format")?)
            .ok_or_else(|| repair("unknown patch format"))?,
        provider_invocation_ref: WorkProviderInvocationRef::parse(text("provider_invocation_ref")?)
            .map_err(repair)?,
        source_ref: super::WorkChangeRef::parse(text("source_ref")?).map_err(repair)?,
        created_at: super::repository::decode_timestamp(
            "Work patch artifact",
            "created_at",
            text("patch_created_at")?,
        )?,
    })
}

fn represents(record: &WorkPatchArtifact, request: &NewWorkPatchArtifact) -> bool {
    record.work_id == request.work_id
        && record.branch_id == request.branch_id
        && record.patch_artifact_id == request.patch_artifact_id
        && record.payload_artifact_id == request.payload_artifact_id
        && record.source_branch_revision == request.expected_branch_revision
        && record.source_graph_revision == request.expected_graph_revision
        && record.source_subject_record_revision == request.expected_subject_record_revision
        && record.subject_ref == request.subject_ref
        && record.base_subject_revision == request.base_subject_revision
        && record.result_subject_revision == request.result_subject_revision
        && record.format == request.format
        && record.provider_invocation_ref == request.provider_invocation_ref
        && record.source_ref == request.source_ref
}

async fn load_in_transaction(
    transaction: &mut Transaction<'_, MySql>,
    owner_id: &WorkOwnerId,
    work_id: &WorkId,
    patch_artifact_id: &WorkPatchArtifactId,
) -> Result<Option<WorkPatchArtifact>, WorkRepositoryError> {
    query(
        "SELECT *, DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS patch_created_at
         FROM work_patch_artifacts
         WHERE owner_id = ? AND work_id = ? AND patch_artifact_id = ? LIMIT 1",
    )
    .bind(owner_id.as_str())
    .bind(work_id.as_str())
    .bind(patch_artifact_id.as_str())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("load Work patch artifact", source))?
    .as_ref()
    .map(decode_patch)
    .transpose()
}

struct PayloadContract {
    hash: WorkContentHash,
    bytes: u64,
    data: String,
}

fn string_field<'a>(
    content: &'a Value,
    field: &'static str,
) -> Result<&'a str, WorkRepositoryError> {
    content
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| conflict(WorkPatchArtifactBasisResource::PayloadContract))
}

fn validate_payload_contract(raw: &str) -> Result<PayloadContract, WorkRepositoryError> {
    let mut content: Value = serde_json::from_str(raw).map_err(repair)?;
    if string_field(&content, "kind")? != "patch"
        || string_field(&content, "content_type")? != "text/x-diff"
        || string_field(&content, "encoding")? != "utf-8"
    {
        return Err(conflict(WorkPatchArtifactBasisResource::PayloadContract));
    }
    let data = content
        .as_object_mut()
        .and_then(|object| object.remove("data"))
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .ok_or_else(|| conflict(WorkPatchArtifactBasisResource::PayloadContract))?;
    let declared_bytes = content
        .get("byte_size")
        .and_then(Value::as_u64)
        .ok_or_else(|| conflict(WorkPatchArtifactBasisResource::PayloadContract))?;
    if declared_bytes != data.len() as u64
        || declared_bytes > super::WORK_PATCH_ARTIFACT_MAX_BYTES
        || super::work_patch_line_count(data.as_bytes()) > super::WORK_PATCH_ARTIFACT_MAX_LINES
    {
        return Err(conflict(WorkPatchArtifactBasisResource::PayloadContract));
    }
    let digest = format!("{:x}", Sha256::digest(data.as_bytes()));
    if string_field(&content, "sha256")? != digest {
        return Err(conflict(WorkPatchArtifactBasisResource::PayloadContract));
    }
    Ok(PayloadContract {
        hash: WorkContentHash::parse(format!("sha256:{digest}"))
            .expect("SHA-256 formatter emits a canonical Work hash"),
        bytes: declared_bytes,
        data,
    })
}

pub(super) async fn record_patch_artifact(
    repository: &DatabaseWorkRepository,
    request: NewWorkPatchArtifact,
) -> Result<WorkPatchArtifact, WorkRepositoryError> {
    super::validate_identity("payload_artifact_id", &request.payload_artifact_id, 64)
        .map_err(super::repository::invalid_mutation)?;
    if request.base_subject_revision == request.result_subject_revision {
        return Err(conflict(WorkPatchArtifactBasisResource::PayloadContract));
    }
    let mut transaction = repository.pool.get().begin().await.map_err(|source| {
        WorkRepositoryError::persistence("begin Work patch artifact transaction", source)
    })?;
    query(
        "SELECT work_revision FROM works
         WHERE owner_id = ? AND work_id = ? LIMIT 1 FOR UPDATE",
    )
    .bind(request.owner_id.as_str())
    .bind(request.work_id.as_str())
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("lock Work patch aggregate", source))?
    .ok_or(WorkRepositoryError::NotFound)?;
    if let Some(existing) = load_in_transaction(
        &mut transaction,
        &request.owner_id,
        &request.work_id,
        &request.patch_artifact_id,
    )
    .await?
    {
        if represents(&existing, &request) {
            transaction.commit().await.map_err(|source| {
                WorkRepositoryError::persistence("commit Work patch artifact replay", source)
            })?;
            return Ok(existing);
        }
        return Err(conflict(WorkPatchArtifactBasisResource::PatchIdentity));
    }
    let row = query(
        "SELECT CASE WHEN w.archived_at IS NULL THEN 0 ELSE 1 END AS work_archived,
                b.branch_revision, b.current_graph_revision,
                CASE WHEN b.archived_at IS NULL THEN 0 ELSE 1 END AS branch_archived,
                b.deletion_operation_id, b.session_id,
                s.subject_record_revision, s.branch_revision AS subject_branch_revision,
                s.graph_revision AS subject_graph_revision, s.subject_ref, s.subject_revision,
                a.artifact_kind, a.status AS artifact_status, a.content_json
         FROM works w
         JOIN work_branches b
           ON b.owner_id = w.owner_id AND b.work_id = w.work_id AND b.branch_id = ?
         LEFT JOIN work_branch_subjects s
           ON s.owner_id = b.owner_id AND s.work_id = b.work_id AND s.branch_id = b.branch_id
         LEFT JOIN session_artifacts a
           ON a.user_id = b.owner_id AND a.session_id = b.session_id AND a.artifact_id = ?
         WHERE w.owner_id = ? AND w.work_id = ? LIMIT 1 FOR UPDATE",
    )
    .bind(request.branch_id.as_str())
    .bind(&request.payload_artifact_id)
    .bind(request.owner_id.as_str())
    .bind(request.work_id.as_str())
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("lock Work patch artifact basis", source))?
    .ok_or(WorkRepositoryError::NotFound)?;
    let integer = |field: &'static str| {
        row.try_get::<i64, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("Work patch artifact basis", source))
    };
    let optional_integer = |field: &'static str| {
        row.try_get::<Option<i64>, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("Work patch artifact basis", source))
    };
    let optional_text = |field: &'static str| {
        row.try_get::<Option<String>, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("Work patch artifact basis", source))
    };
    if integer("work_archived")? != 0
        || integer("branch_archived")? != 0
        || optional_text("deletion_operation_id")?.is_some()
    {
        return Err(WorkRepositoryError::Archived);
    }
    let branch_revision = WorkBranchRevision::new(integer("branch_revision")?).map_err(repair)?;
    let graph_revision = GraphRevision::new(integer("current_graph_revision")?).map_err(repair)?;
    if branch_revision != request.expected_branch_revision {
        return Err(conflict(WorkPatchArtifactBasisResource::BranchRevision));
    }
    if graph_revision != request.expected_graph_revision {
        return Err(conflict(WorkPatchArtifactBasisResource::GraphRevision));
    }
    let subject_record_revision = optional_integer("subject_record_revision")?
        .map(WorkBranchSubjectRevision::new)
        .transpose()
        .map_err(repair)?;
    let subject_branch_revision = optional_integer("subject_branch_revision")?
        .map(WorkBranchRevision::new)
        .transpose()
        .map_err(repair)?;
    let subject_graph_revision = optional_integer("subject_graph_revision")?
        .map(GraphRevision::new)
        .transpose()
        .map_err(repair)?;
    if subject_record_revision != Some(request.expected_subject_record_revision)
        || subject_branch_revision != Some(request.expected_branch_revision)
        || subject_graph_revision != Some(request.expected_graph_revision)
        || optional_text("subject_ref")?.as_deref() != Some(request.subject_ref.as_str())
        || optional_text("subject_revision")?.as_deref()
            != Some(request.result_subject_revision.as_str())
    {
        return Err(conflict(WorkPatchArtifactBasisResource::Subject));
    }
    if optional_text("artifact_kind")?.as_deref() != Some("patch")
        || optional_text("artifact_status")?.as_deref() != Some("active")
    {
        return Err(conflict(WorkPatchArtifactBasisResource::PayloadArtifact));
    }
    let payload = validate_payload_contract(
        &optional_text("content_json")?
            .ok_or_else(|| conflict(WorkPatchArtifactBasisResource::PayloadArtifact))?,
    )?;
    let session_id = InternalSessionId::parse(
        row.try_get::<String, _>("session_id")
            .map_err(|source| WorkRepositoryError::corrupt("Work patch artifact basis", source))?,
    )
    .map_err(repair)?;
    query(
        "INSERT INTO work_patch_artifacts
         (owner_id, work_id, branch_id, patch_artifact_id, session_id, payload_artifact_id,
          source_branch_revision, source_graph_revision, source_subject_record_revision,
          subject_ref, base_subject_revision, result_subject_revision, payload_hash,
          payload_bytes, patch_format, provider_invocation_ref, source_ref)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(request.owner_id.as_str())
    .bind(request.work_id.as_str())
    .bind(request.branch_id.as_str())
    .bind(request.patch_artifact_id.as_str())
    .bind(session_id.as_str())
    .bind(&request.payload_artifact_id)
    .bind(branch_revision.get())
    .bind(graph_revision.get())
    .bind(request.expected_subject_record_revision.get())
    .bind(request.subject_ref.as_str())
    .bind(request.base_subject_revision.as_str())
    .bind(request.result_subject_revision.as_str())
    .bind(payload.hash.as_str())
    .bind(i64::try_from(payload.bytes).expect("patch size limit fits i64"))
    .bind(request.format.as_str())
    .bind(request.provider_invocation_ref.as_str())
    .bind(request.source_ref.as_str())
    .execute(&mut *transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("store Work patch artifact", source))?;
    query(
        "INSERT INTO session_artifact_references
         (user_id, session_id, artifact_id, reference_kind, reference_id)
         VALUES (?, ?, ?, 'state_item', ?)
         ON DUPLICATE KEY UPDATE created_at = created_at",
    )
    .bind(request.owner_id.as_str())
    .bind(session_id.as_str())
    .bind(&request.payload_artifact_id)
    .bind(request.patch_artifact_id.as_str())
    .execute(&mut *transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("retain Work patch payload", source))?;
    super::events_repository::append_event(
        &mut transaction,
        &NewWorkEvent {
            owner_id: request.owner_id.clone(),
            work_id: request.work_id.clone(),
            branch_id: Some(request.branch_id.clone()),
            kind: WorkEventKind::PatchArtifactExported,
            work_revision: None,
            goal_revision: None,
            criterion_set_revision: None,
            branch_revision: Some(branch_revision),
            graph_revision: Some(graph_revision),
            source_ref: request.source_ref.clone(),
        },
    )
    .await?;
    let stored = load_in_transaction(
        &mut transaction,
        &request.owner_id,
        &request.work_id,
        &request.patch_artifact_id,
    )
    .await?
    .ok_or_else(|| repair("inserted patch artifact could not be loaded"))?;
    transaction
        .commit()
        .await
        .map_err(|source| WorkRepositoryError::persistence("commit Work patch artifact", source))?;
    Ok(stored)
}

pub(super) async fn load_patch_artifact(
    repository: &DatabaseWorkRepository,
    owner_id: &WorkOwnerId,
    work_id: &WorkId,
    patch_artifact_id: &WorkPatchArtifactId,
) -> Result<Option<WorkPatchArtifact>, WorkRepositoryError> {
    let mut transaction = repository.pool.get().begin().await.map_err(|source| {
        WorkRepositoryError::persistence("begin Work patch artifact read", source)
    })?;
    let record =
        load_in_transaction(&mut transaction, owner_id, work_id, patch_artifact_id).await?;
    transaction.commit().await.map_err(|source| {
        WorkRepositoryError::persistence("commit Work patch artifact read", source)
    })?;
    Ok(record)
}

pub(super) async fn load_patch_artifact_content(
    repository: &DatabaseWorkRepository,
    owner_id: &WorkOwnerId,
    work_id: &WorkId,
    branch_id: &WorkBranchId,
    patch_artifact_id: &WorkPatchArtifactId,
) -> Result<Option<super::WorkPatchArtifactContent>, WorkRepositoryError> {
    let mut transaction = repository.pool.get().begin().await.map_err(|source| {
        WorkRepositoryError::persistence("begin Work patch content read", source)
    })?;
    let Some(artifact) =
        load_in_transaction(&mut transaction, owner_id, work_id, patch_artifact_id).await?
    else {
        transaction.commit().await.map_err(|source| {
            WorkRepositoryError::persistence("commit missing Work patch content read", source)
        })?;
        return Ok(None);
    };
    if artifact.branch_id != *branch_id {
        transaction.commit().await.map_err(|source| {
            WorkRepositoryError::persistence("commit mismatched Work patch content read", source)
        })?;
        return Ok(None);
    }
    let row = query(
        "SELECT artifact_kind, status AS artifact_status, content_json
         FROM session_artifacts
         WHERE user_id = ? AND session_id = ? AND artifact_id = ? LIMIT 1",
    )
    .bind(owner_id.as_str())
    .bind(artifact.session_id.as_str())
    .bind(&artifact.payload_artifact_id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("load Work patch content", source))?;
    let Some(row) = row else {
        return Err(repair("patch payload artifact is missing"));
    };
    let artifact_kind = row
        .try_get::<String, _>("artifact_kind")
        .map_err(|source| WorkRepositoryError::corrupt("Work patch content", source))?;
    let artifact_status = row
        .try_get::<String, _>("artifact_status")
        .map_err(|source| WorkRepositoryError::corrupt("Work patch content", source))?;
    if artifact_kind != "patch" || artifact_status != "active" {
        return Err(repair("patch payload artifact is not active patch content"));
    }
    let raw = row
        .try_get::<String, _>("content_json")
        .map_err(|source| WorkRepositoryError::corrupt("Work patch content", source))?;
    let contract = validate_payload_contract(&raw)?;
    if contract.hash != artifact.payload_hash || contract.bytes != artifact.payload_bytes {
        return Err(repair(
            "patch payload no longer matches immutable Work provenance",
        ));
    }
    transaction.commit().await.map_err(|source| {
        WorkRepositoryError::persistence("commit Work patch content read", source)
    })?;
    Ok(Some(super::WorkPatchArtifactContent {
        artifact,
        data: contract.data,
    }))
}

pub(super) async fn list_patch_artifacts(
    repository: &DatabaseWorkRepository,
    request: super::WorkPatchArtifactQuery,
) -> Result<super::WorkPatchArtifactPage, WorkRepositoryError> {
    let mut transaction = repository.pool.get().begin().await.map_err(|source| {
        WorkRepositoryError::persistence("begin Work patch artifact page read", source)
    })?;
    let branch_exists: i64 = query(
        "SELECT COUNT(*) FROM works w
         JOIN work_branches b
           ON b.owner_id = w.owner_id AND b.work_id = w.work_id AND b.branch_id = ?
         WHERE w.owner_id = ? AND w.work_id = ?
           AND w.archived_at IS NULL AND b.archived_at IS NULL",
    )
    .bind(request.branch_id.as_str())
    .bind(request.owner_id.as_str())
    .bind(request.work_id.as_str())
    .fetch_one(&mut *transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("locate Work patch branch", source))?
    .try_get(0)
    .map_err(|source| WorkRepositoryError::corrupt("Work patch branch", source))?;
    if branch_exists == 0 {
        return Err(WorkRepositoryError::NotFound);
    }
    let cursor_time = request
        .before
        .as_ref()
        .map(|cursor| cursor.created_at.naive_utc());
    let cursor_id = request
        .before
        .as_ref()
        .map(|cursor| cursor.patch_artifact_id.as_str());
    let fetch_limit = i64::from(request.limit.get()) + 1;
    let mut rows = query(
        "SELECT *, DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS patch_created_at
         FROM work_patch_artifacts
         WHERE owner_id = ? AND work_id = ? AND branch_id = ?
           AND (? IS NULL OR created_at < ?
                OR (created_at = ? AND patch_artifact_id < ?))
         ORDER BY created_at DESC, patch_artifact_id DESC
         LIMIT ?",
    )
    .bind(request.owner_id.as_str())
    .bind(request.work_id.as_str())
    .bind(request.branch_id.as_str())
    .bind(cursor_time)
    .bind(cursor_time)
    .bind(cursor_time)
    .bind(cursor_id)
    .bind(fetch_limit)
    .fetch_all(&mut *transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("load Work patch artifact page", source))?;
    let has_more = rows.len() > usize::from(request.limit.get());
    if has_more {
        rows.pop();
    }
    let artifacts = rows
        .iter()
        .map(decode_patch)
        .collect::<Result<Vec<_>, _>>()?;
    let next_cursor = has_more.then(|| {
        let last = artifacts
            .last()
            .expect("a Work patch page with more rows is non-empty");
        super::WorkPatchArtifactCursor {
            created_at: last.created_at,
            patch_artifact_id: last.patch_artifact_id.clone(),
        }
    });
    transaction.commit().await.map_err(|source| {
        WorkRepositoryError::persistence("commit Work patch artifact page read", source)
    })?;
    Ok(super::WorkPatchArtifactPage {
        schema_version: super::WORK_PATCH_ARTIFACT_SCHEMA_VERSION,
        work_id: request.work_id,
        branch_id: request.branch_id,
        artifacts,
        next_cursor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn payload_contract_verifies_the_actual_bytes() {
        let data = "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-old\n+new\n";
        let digest = format!("{:x}", Sha256::digest(data.as_bytes()));
        let raw = json!({
            "kind": "patch",
            "content_type": "text/x-diff",
            "encoding": "utf-8",
            "data": data,
            "byte_size": data.len(),
            "sha256": digest,
        })
        .to_string();
        let contract = validate_payload_contract(&raw).expect("valid patch contract");
        assert_eq!(contract.bytes, data.len() as u64);
        assert_eq!(contract.hash.as_str(), format!("sha256:{digest}"));
    }

    #[test]
    fn payload_contract_rejects_metadata_that_does_not_match_bytes() {
        let raw = json!({
            "kind": "patch",
            "content_type": "text/x-diff",
            "encoding": "utf-8",
            "data": "actual",
            "byte_size": 6,
            "sha256": "0".repeat(64),
        })
        .to_string();
        assert!(matches!(
            validate_payload_contract(&raw),
            Err(WorkRepositoryError::PatchArtifactConflict {
                resource: WorkPatchArtifactBasisResource::PayloadContract
            })
        ));
    }

    #[test]
    fn payload_contract_rejects_pathological_line_cardinality() {
        let data = "+\n".repeat((super::super::WORK_PATCH_ARTIFACT_MAX_LINES + 1) as usize);
        let digest = format!("{:x}", Sha256::digest(data.as_bytes()));
        let raw = json!({
            "kind": "patch",
            "content_type": "text/x-diff",
            "encoding": "utf-8",
            "data": data,
            "byte_size": data.len(),
            "sha256": digest,
        })
        .to_string();
        assert!(matches!(
            validate_payload_contract(&raw),
            Err(WorkRepositoryError::PatchArtifactConflict {
                resource: WorkPatchArtifactBasisResource::PayloadContract
            })
        ));
    }
}
