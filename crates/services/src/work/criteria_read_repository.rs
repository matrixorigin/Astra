use super::repository::{DatabaseWorkRepository, WorkRepositoryError};
use super::{
    CriterionCommand, CriterionDefinition, CriterionId, CriterionRevision, CriterionSetRevision,
    CriterionStatement, WorkContentHash, WorkCriteriaBasis, WorkCriteriaPage, WorkCriteriaQuery,
    WorkCriterionView, WorkRevision,
};
use serde::{Deserialize, Serialize};
use sqlx::{MySql, QueryBuilder, Row, query};
use std::collections::BTreeMap;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CriterionDefinitionEnvelopeWire {
    schema_version: u32,
    definition: CriterionDefinitionWire,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum CriterionDefinitionWire {
    CommandCheck { statement: String, command: String },
    TestCheck { statement: String, command: String },
    HumanReview { statement: String },
}

#[derive(Serialize)]
struct CriterionDefinitionEnvelope<'a> {
    schema_version: u32,
    definition: &'a CriterionDefinition,
}

fn corrupt_definition(
    source: impl std::error::Error + Send + Sync + 'static,
) -> WorkRepositoryError {
    WorkRepositoryError::corrupt("criterion definition", source)
}

fn decode_definition(
    definition_json: &str,
    persisted_kind: &str,
    persisted_hash: &str,
) -> Result<(CriterionDefinition, WorkContentHash), WorkRepositoryError> {
    let wire: CriterionDefinitionEnvelopeWire =
        serde_json::from_str(definition_json).map_err(corrupt_definition)?;
    if wire.schema_version != super::repository::CRITERION_DEFINITION_SCHEMA_VERSION {
        return Err(corrupt_definition(std::io::Error::other(
            "unsupported criterion definition schema version",
        )));
    }
    let definition = match wire.definition {
        CriterionDefinitionWire::CommandCheck { statement, command } => {
            CriterionDefinition::CommandCheck {
                statement: CriterionStatement::parse(statement).map_err(corrupt_definition)?,
                command: CriterionCommand::parse(command).map_err(corrupt_definition)?,
            }
        }
        CriterionDefinitionWire::TestCheck { statement, command } => {
            CriterionDefinition::TestCheck {
                statement: CriterionStatement::parse(statement).map_err(corrupt_definition)?,
                command: CriterionCommand::parse(command).map_err(corrupt_definition)?,
            }
        }
        CriterionDefinitionWire::HumanReview { statement } => CriterionDefinition::HumanReview {
            statement: CriterionStatement::parse(statement).map_err(corrupt_definition)?,
        },
    };
    if definition.kind().as_str() != persisted_kind {
        return Err(corrupt_definition(std::io::Error::other(
            "criterion kind disagrees with its canonical definition",
        )));
    }
    let canonical = super::repository::canonical_json(
        "criterion definition",
        &CriterionDefinitionEnvelope {
            schema_version: super::repository::CRITERION_DEFINITION_SCHEMA_VERSION,
            definition: &definition,
        },
    )?;
    if canonical != definition_json
        || super::repository::content_hash(definition_json) != persisted_hash
    {
        return Err(corrupt_definition(std::io::Error::other(
            "criterion definition encoding or hash is not canonical",
        )));
    }
    let hash = WorkContentHash::parse(persisted_hash.to_string())
        .map_err(|message| corrupt_definition(std::io::Error::other(message)))?;
    Ok((definition, hash))
}

pub(super) async fn load_criteria_page(
    repository: &DatabaseWorkRepository,
    request: WorkCriteriaQuery,
) -> Result<WorkCriteriaPage, WorkRepositoryError> {
    let mut transaction = repository.pool.get().begin().await.map_err(|source| {
        WorkRepositoryError::persistence("begin Work criteria snapshot", source)
    })?;
    let row = query(
        "SELECT w.work_revision, w.current_criteria_set_revision,
                cs.member_manifest_json, cs.member_manifest_hash, cs.member_count
         FROM works w
         JOIN work_criterion_sets cs
           ON cs.owner_id = w.owner_id AND cs.work_id = w.work_id
          AND cs.revision = w.current_criteria_set_revision
         WHERE w.owner_id = ? AND w.work_id = ?
         LIMIT 1",
    )
    .bind(request.owner_id.as_str())
    .bind(request.work_id.as_str())
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|source| WorkRepositoryError::persistence("load Work criteria basis", source))?
    .ok_or(WorkRepositoryError::NotFound)?;
    let work_revision = WorkRevision::new(
        row.try_get::<i64, _>("work_revision")
            .map_err(|source| WorkRepositoryError::corrupt("Work criteria basis", source))?,
    )
    .map_err(|source| WorkRepositoryError::corrupt("Work criteria basis", source))?;
    let criteria_set_revision = CriterionSetRevision::new(
        row.try_get::<i64, _>("current_criteria_set_revision")
            .map_err(|source| WorkRepositoryError::corrupt("Work criteria basis", source))?,
    )
    .map_err(|source| WorkRepositoryError::corrupt("Work criteria basis", source))?;
    if let Some(expected) = request.expected_criteria_set_revision
        && expected != criteria_set_revision
    {
        return Err(WorkRepositoryError::StaleCriteriaPageRevision {
            expected_criteria_set_revision: expected,
            actual_criteria_set_revision: criteria_set_revision,
        });
    }
    let member_count_i64 = row
        .try_get::<i64, _>("member_count")
        .map_err(|source| WorkRepositoryError::corrupt("Work criteria basis", source))?;
    let member_count = u16::try_from(member_count_i64)
        .ok()
        .filter(|count| usize::from(*count) <= super::criteria::CRITERION_SET_MAX_MEMBERS)
        .ok_or_else(|| {
            WorkRepositoryError::corrupt(
                "Work criteria basis",
                std::io::Error::other("criterion member count exceeds the domain bound"),
            )
        })?;
    if request.offset > member_count {
        return Err(WorkRepositoryError::CriteriaPageCursorAhead {
            offset: request.offset,
            member_count,
        });
    }
    let manifest_json = row
        .try_get::<String, _>("member_manifest_json")
        .map_err(|source| WorkRepositoryError::corrupt("Work criteria basis", source))?;
    let manifest_hash_text = row
        .try_get::<String, _>("member_manifest_hash")
        .map_err(|source| WorkRepositoryError::corrupt("Work criteria basis", source))?;
    if super::repository::content_hash(&manifest_json) != manifest_hash_text {
        return Err(WorkRepositoryError::corrupt(
            "Work criteria basis",
            std::io::Error::other("criterion-set manifest hash mismatch"),
        ));
    }
    let members =
        super::repository::decode_criterion_set_manifest(&manifest_json, member_count_i64)?;
    let offset = usize::from(request.offset);
    let end = offset
        .saturating_add(usize::from(request.limit))
        .min(members.len());
    let page_members = &members[offset..end];
    let mut entries = BTreeMap::new();
    if !page_members.is_empty() {
        let mut builder = QueryBuilder::<MySql>::new(
            "SELECT criterion_id, revision, criterion_kind, definition_json, definition_hash
             FROM work_criterion_revisions
             WHERE owner_id = ",
        );
        builder
            .push_bind(request.owner_id.as_str())
            .push(" AND work_id = ")
            .push_bind(request.work_id.as_str())
            .push(" AND (");
        for (index, member) in page_members.iter().enumerate() {
            if index > 0 {
                builder.push(" OR ");
            }
            builder
                .push("(criterion_id = ")
                .push_bind(member.criterion_id.as_str())
                .push(" AND revision = ")
                .push_bind(member.revision.get())
                .push(")");
        }
        builder.push(")");
        for row in builder
            .build()
            .fetch_all(&mut *transaction)
            .await
            .map_err(|source| WorkRepositoryError::persistence("load Work criteria page", source))?
        {
            let criterion_id = CriterionId::parse(
                row.try_get::<String, _>("criterion_id")
                    .map_err(corrupt_definition)?,
            )
            .map_err(corrupt_definition)?;
            let revision = CriterionRevision::new(
                row.try_get::<i64, _>("revision")
                    .map_err(corrupt_definition)?,
            )
            .map_err(corrupt_definition)?;
            let definition_json = row
                .try_get::<String, _>("definition_json")
                .map_err(corrupt_definition)?;
            let definition_hash_text = row
                .try_get::<String, _>("definition_hash")
                .map_err(corrupt_definition)?;
            let (definition, definition_hash) = decode_definition(
                &definition_json,
                &row.try_get::<String, _>("criterion_kind")
                    .map_err(corrupt_definition)?,
                &definition_hash_text,
            )?;
            let reference = super::CriterionRevisionRef {
                criterion_id: criterion_id.clone(),
                revision,
            };
            if entries
                .insert(
                    reference,
                    WorkCriterionView {
                        criterion_id,
                        revision,
                        definition,
                        definition_hash,
                    },
                )
                .is_some()
            {
                return Err(corrupt_definition(std::io::Error::other(
                    "duplicate immutable criterion revision",
                )));
            }
        }
    }
    let ordered = page_members
        .iter()
        .map(|member| {
            entries.remove(member).ok_or_else(|| {
                corrupt_definition(std::io::Error::other(
                    "criterion-set member revision is missing",
                ))
            })
        })
        .collect::<Result<Vec<_>, WorkRepositoryError>>()?;
    if !entries.is_empty() {
        return Err(corrupt_definition(std::io::Error::other(
            "criteria query returned a revision outside the page",
        )));
    }
    let manifest_hash = WorkContentHash::parse(manifest_hash_text).map_err(|message| {
        WorkRepositoryError::corrupt("Work criteria basis", std::io::Error::other(message))
    })?;
    let page = WorkCriteriaPage::from_parts(
        WorkCriteriaBasis {
            work_id: request.work_id.clone(),
            work_revision,
            criteria_set_revision,
            manifest_hash,
            member_count,
        },
        &request,
        ordered,
    )
    .map_err(super::repository::invalid_mutation)?;
    transaction.commit().await.map_err(|source| {
        WorkRepositoryError::persistence("commit Work criteria snapshot", source)
    })?;
    Ok(page)
}
