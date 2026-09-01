use super::{
    CriterionSetRevision, WorkChangeReason, WorkChangeRef, WorkContentHash, WorkDomainError,
    WorkId, WorkOwnerId, WorkRevision, validate_resource_identity,
};
use serde::Serialize;

const CRITERION_ID_MAX_CHARS: usize = 64;
const CRITERION_STATEMENT_MAX_BYTES: usize = 16 * 1024;
const CRITERION_COMMAND_MAX_BYTES: usize = 64 * 1024;
pub(crate) const CRITERION_SET_MAX_MEMBERS: usize = 128;
const CRITERION_SET_MAX_DEFINITION_BYTES: usize = 1024 * 1024;
pub const WORK_CRITERIA_PAGE_MAX_ITEMS: u16 = 8;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct CriterionId(String);

impl CriterionId {
    pub fn parse(value: impl Into<String>) -> Result<Self, WorkDomainError> {
        let value = value.into();
        validate_resource_identity("criterion_id", &value, CRITERION_ID_MAX_CHARS)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct CriterionRevision(i64);

impl CriterionRevision {
    pub const INITIAL: Self = Self(1);

    pub fn new(value: i64) -> Result<Self, WorkDomainError> {
        if value < 1 {
            return Err(WorkDomainError::InvalidRevision {
                field: "criterion",
                value,
            });
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> i64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CriterionKind {
    CommandCheck,
    TestCheck,
    ArtifactCheck,
    StateCheck,
    HumanReview,
    ModelAssessment,
}

impl CriterionKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CommandCheck => "command_check",
            Self::TestCheck => "test_check",
            Self::ArtifactCheck => "artifact_check",
            Self::StateCheck => "state_check",
            Self::HumanReview => "human_review",
            Self::ModelAssessment => "model_assessment",
        }
    }

    pub const fn is_deterministic(self) -> bool {
        matches!(
            self,
            Self::CommandCheck | Self::TestCheck | Self::ArtifactCheck | Self::StateCheck
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct CriterionStatement(String);

impl CriterionStatement {
    pub fn parse(value: impl Into<String>) -> Result<Self, WorkDomainError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(WorkDomainError::InvalidCriterionStatement {
                violation: super::CriterionStatementViolation::Empty,
            });
        }
        if value.len() > CRITERION_STATEMENT_MAX_BYTES {
            return Err(WorkDomainError::InvalidCriterionStatement {
                violation: super::CriterionStatementViolation::TooLarge {
                    max_bytes: CRITERION_STATEMENT_MAX_BYTES,
                },
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct CriterionCommand(String);

impl CriterionCommand {
    pub fn parse(value: impl Into<String>) -> Result<Self, WorkDomainError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(WorkDomainError::InvalidCriterionCommand {
                violation: super::CriterionCommandViolation::Empty,
            });
        }
        if value.len() > CRITERION_COMMAND_MAX_BYTES {
            return Err(WorkDomainError::InvalidCriterionCommand {
                violation: super::CriterionCommandViolation::TooLarge {
                    max_bytes: CRITERION_COMMAND_MAX_BYTES,
                },
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Executable or explicitly human-owned criterion definition.
///
/// Artifact, state, and model-assessment kinds are reserved in the persistence
/// vocabulary but intentionally have no constructible definition until their
/// verifier payloads are specified. This prevents accepting a hard criterion
/// that only looks executable because of its label.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CriterionDefinition {
    CommandCheck {
        statement: CriterionStatement,
        command: CriterionCommand,
    },
    TestCheck {
        statement: CriterionStatement,
        command: CriterionCommand,
    },
    HumanReview {
        statement: CriterionStatement,
    },
}

impl CriterionDefinition {
    pub const fn kind(&self) -> CriterionKind {
        match self {
            Self::CommandCheck { .. } => CriterionKind::CommandCheck,
            Self::TestCheck { .. } => CriterionKind::TestCheck,
            Self::HumanReview { .. } => CriterionKind::HumanReview,
        }
    }

    fn payload_bytes(&self) -> usize {
        match self {
            Self::CommandCheck { statement, command } | Self::TestCheck { statement, command } => {
                statement.as_str().len() + command.as_str().len()
            }
            Self::HumanReview { statement } => statement.as_str().len(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct CriterionRevisionRef {
    pub criterion_id: CriterionId,
    pub revision: CriterionRevision,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NewWorkCriterion {
    pub criterion_id: CriterionId,
    pub definition: CriterionDefinition,
}

impl NewWorkCriterion {
    fn validate_aggregate<'a>(
        criteria: impl IntoIterator<Item = &'a Self>,
    ) -> Result<(), WorkDomainError> {
        let payload_bytes = criteria.into_iter().try_fold(0usize, |total, criterion| {
            total.checked_add(
                criterion.criterion_id.as_str().len() + criterion.definition.payload_bytes(),
            )
        });
        if payload_bytes.is_none_or(|bytes| bytes > CRITERION_SET_MAX_DEFINITION_BYTES) {
            return Err(WorkDomainError::CriteriaPayloadTooLarge {
                max_bytes: CRITERION_SET_MAX_DEFINITION_BYTES,
            });
        }
        Ok(())
    }

    /// Validate and order one explicit criterion set before deriving any
    /// idempotency or persistence identity from it.
    pub fn canonicalize_set(mut criteria: Vec<Self>) -> Result<Vec<Self>, WorkDomainError> {
        if criteria.len() > CRITERION_SET_MAX_MEMBERS {
            return Err(WorkDomainError::TooManyCriteria {
                max_members: CRITERION_SET_MAX_MEMBERS,
            });
        }
        criteria.sort_by(|left, right| left.criterion_id.cmp(&right.criterion_id));
        if let Some(duplicate) = criteria
            .windows(2)
            .find(|pair| pair[0].criterion_id == pair[1].criterion_id)
        {
            return Err(WorkDomainError::DuplicateCriterion {
                criterion_id: duplicate[0].criterion_id.as_str().to_string(),
            });
        }
        Self::validate_aggregate(&criteria)?;
        Ok(criteria)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CriterionSetMemberChange {
    Existing(CriterionRevisionRef),
    New(NewWorkCriterion),
}

impl CriterionSetMemberChange {
    pub(crate) fn revision_ref(&self) -> CriterionRevisionRef {
        match self {
            Self::Existing(reference) => reference.clone(),
            Self::New(criterion) => CriterionRevisionRef {
                criterion_id: criterion.criterion_id.clone(),
                revision: CriterionRevision::INITIAL,
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkCriteriaChange {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub expected_work_revision: WorkRevision,
    pub expected_criteria_set_revision: super::CriterionSetRevision,
    pub members: Vec<CriterionSetMemberChange>,
    pub source_ref: WorkChangeRef,
    pub reason: Option<WorkChangeReason>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkCriteriaQuery {
    pub(crate) owner_id: WorkOwnerId,
    pub(crate) work_id: WorkId,
    pub(crate) expected_criteria_set_revision: Option<CriterionSetRevision>,
    pub(crate) offset: u16,
    pub(crate) limit: u16,
}

impl WorkCriteriaQuery {
    pub fn new(
        owner_id: WorkOwnerId,
        work_id: WorkId,
        expected_criteria_set_revision: Option<CriterionSetRevision>,
        offset: u16,
        limit: u16,
    ) -> Result<Self, WorkDomainError> {
        if limit == 0 || limit > WORK_CRITERIA_PAGE_MAX_ITEMS {
            return Err(WorkDomainError::InvalidCriteriaPageLimit {
                max_items: WORK_CRITERIA_PAGE_MAX_ITEMS,
            });
        }
        if usize::from(offset) > CRITERION_SET_MAX_MEMBERS {
            return Err(WorkDomainError::InvalidCriteriaPageOffset {
                max_items: CRITERION_SET_MAX_MEMBERS as u16,
            });
        }
        if offset > 0 && expected_criteria_set_revision.is_none() {
            return Err(WorkDomainError::UnpinnedCriteriaPageCursor);
        }
        Ok(Self {
            owner_id,
            work_id,
            expected_criteria_set_revision,
            offset,
            limit,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkCriteriaBasis {
    pub work_id: WorkId,
    pub work_revision: WorkRevision,
    pub criteria_set_revision: CriterionSetRevision,
    pub manifest_hash: WorkContentHash,
    pub member_count: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkCriterionView {
    pub criterion_id: CriterionId,
    pub revision: CriterionRevision,
    #[serde(flatten)]
    pub definition: CriterionDefinition,
    pub definition_hash: WorkContentHash,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkCriteriaCursor {
    pub criteria_set_revision: CriterionSetRevision,
    pub offset: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkCriteriaSlice {
    pub offset: u16,
    pub limit: u16,
    pub total: u16,
    pub entries: Vec<WorkCriterionView>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkCriteriaPage {
    pub schema_version: u16,
    pub basis: WorkCriteriaBasis,
    pub cursor: WorkCriteriaCursor,
    pub next_cursor: Option<WorkCriteriaCursor>,
    pub criteria: WorkCriteriaSlice,
}

impl WorkCriteriaPage {
    pub(crate) fn from_parts(
        basis: WorkCriteriaBasis,
        query: &WorkCriteriaQuery,
        entries: Vec<WorkCriterionView>,
    ) -> Result<Self, WorkDomainError> {
        let returned = u16::try_from(entries.len()).map_err(|_| {
            WorkDomainError::InvalidCriteriaPageLimit {
                max_items: WORK_CRITERIA_PAGE_MAX_ITEMS,
            }
        })?;
        if usize::from(basis.member_count) > CRITERION_SET_MAX_MEMBERS {
            return Err(WorkDomainError::InvalidCriteriaPageEntries);
        }
        let expected = basis
            .member_count
            .checked_sub(query.offset)
            .map(|remaining| remaining.min(query.limit));
        if expected != Some(returned)
            || entries
                .windows(2)
                .any(|pair| pair[0].criterion_id >= pair[1].criterion_id)
        {
            return Err(WorkDomainError::InvalidCriteriaPageEntries);
        }
        let next_offset = query.offset.checked_add(returned);
        let next_cursor = next_offset
            .filter(|offset| *offset < basis.member_count)
            .map(|offset| WorkCriteriaCursor {
                criteria_set_revision: basis.criteria_set_revision,
                offset,
            });
        Ok(Self {
            schema_version: 1,
            cursor: WorkCriteriaCursor {
                criteria_set_revision: basis.criteria_set_revision,
                offset: query.offset,
            },
            next_cursor,
            criteria: WorkCriteriaSlice {
                offset: query.offset,
                limit: query.limit,
                total: basis.member_count,
                entries,
            },
            basis,
        })
    }
}

pub(crate) fn canonical_member_refs(
    members: &[CriterionSetMemberChange],
) -> Result<Vec<CriterionRevisionRef>, WorkDomainError> {
    if members.len() > CRITERION_SET_MAX_MEMBERS {
        return Err(WorkDomainError::TooManyCriteria {
            max_members: CRITERION_SET_MAX_MEMBERS,
        });
    }
    NewWorkCriterion::validate_aggregate(members.iter().filter_map(|member| match member {
        CriterionSetMemberChange::Existing(_) => None,
        CriterionSetMemberChange::New(criterion) => Some(criterion),
    }))?;
    let mut references = members
        .iter()
        .map(CriterionSetMemberChange::revision_ref)
        .collect::<Vec<_>>();
    references.sort();
    if let Some(duplicate) = references
        .windows(2)
        .find(|pair| pair[0].criterion_id == pair[1].criterion_id)
    {
        return Err(WorkDomainError::DuplicateCriterion {
            criterion_id: duplicate[0].criterion_id.as_str().to_string(),
        });
    }
    Ok(references)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::{CriterionSetRevision, WorkOwnerId};

    fn change(members: Vec<CriterionSetMemberChange>) -> WorkCriteriaChange {
        WorkCriteriaChange {
            owner_id: WorkOwnerId::parse("owner-1").expect("owner"),
            work_id: WorkId::parse("work-1").expect("work"),
            expected_work_revision: WorkRevision::INITIAL,
            expected_criteria_set_revision: CriterionSetRevision::INITIAL,
            members,
            source_ref: WorkChangeRef::parse("event-1").expect("source"),
            reason: None,
        }
    }

    fn new(id: &str, kind: CriterionKind) -> CriterionSetMemberChange {
        let statement = CriterionStatement::parse(format!("Prove {id}")).expect("statement");
        let definition = match kind {
            CriterionKind::CommandCheck => CriterionDefinition::CommandCheck {
                statement,
                command: CriterionCommand::parse("make check").expect("command"),
            },
            CriterionKind::TestCheck => CriterionDefinition::TestCheck {
                statement,
                command: CriterionCommand::parse("cargo test").expect("command"),
            },
            CriterionKind::HumanReview => CriterionDefinition::HumanReview { statement },
            unsupported => panic!("unsupported test criterion kind: {unsupported:?}"),
        };
        CriterionSetMemberChange::New(NewWorkCriterion {
            criterion_id: CriterionId::parse(id).expect("criterion id"),
            definition,
        })
    }

    #[test]
    fn hard_and_soft_criteria_are_typed_not_inferred_from_statements() {
        assert!(CriterionKind::CommandCheck.is_deterministic());
        assert!(CriterionKind::StateCheck.is_deterministic());
        assert!(!CriterionKind::HumanReview.is_deterministic());
        assert!(!CriterionKind::ModelAssessment.is_deterministic());

        let misleading =
            CriterionStatement::parse("human review says all tests passed").expect("statement");
        let criterion = NewWorkCriterion {
            criterion_id: CriterionId::parse("criterion-1").expect("id"),
            definition: CriterionDefinition::HumanReview {
                statement: misleading,
            },
        };
        assert!(!criterion.definition.kind().is_deterministic());
    }

    #[test]
    fn initial_criteria_fail_before_persistence_when_aggregate_payload_is_unbounded() {
        let criteria = (0..16)
            .map(|index| NewWorkCriterion {
                criterion_id: CriterionId::parse(format!("criterion-{index}")).expect("id"),
                definition: CriterionDefinition::CommandCheck {
                    statement: CriterionStatement::parse("Prove the bounded input.")
                        .expect("statement"),
                    command: CriterionCommand::parse("x".repeat(CRITERION_COMMAND_MAX_BYTES))
                        .expect("individually valid command"),
                },
            })
            .collect();
        assert!(matches!(
            NewWorkCriterion::canonicalize_set(criteria),
            Err(WorkDomainError::CriteriaPayloadTooLarge {
                max_bytes: CRITERION_SET_MAX_DEFINITION_BYTES
            })
        ));
    }

    #[test]
    fn criteria_pages_are_bounded_and_continuations_are_revision_pinned() {
        let owner = WorkOwnerId::parse("owner").expect("owner");
        let work = WorkId::parse("work").expect("work");
        assert!(matches!(
            WorkCriteriaQuery::new(owner.clone(), work.clone(), None, 1, 8),
            Err(WorkDomainError::UnpinnedCriteriaPageCursor)
        ));
        assert!(matches!(
            WorkCriteriaQuery::new(owner.clone(), work.clone(), None, 0, 9),
            Err(WorkDomainError::InvalidCriteriaPageLimit { max_items: 8 })
        ));
        assert!(matches!(
            WorkCriteriaQuery::new(owner.clone(), work.clone(), None, 129, 8),
            Err(WorkDomainError::InvalidCriteriaPageOffset { max_items: 128 })
        ));

        let query = WorkCriteriaQuery::new(
            owner,
            work.clone(),
            Some(CriterionSetRevision::INITIAL),
            0,
            1,
        )
        .expect("query");
        let entry = WorkCriterionView {
            criterion_id: CriterionId::parse("review").expect("criterion"),
            revision: CriterionRevision::INITIAL,
            definition: CriterionDefinition::HumanReview {
                statement: CriterionStatement::parse("Review the result.").expect("statement"),
            },
            definition_hash: WorkContentHash::parse(format!("sha256:{}", "a".repeat(64)))
                .expect("hash"),
        };
        let page = WorkCriteriaPage::from_parts(
            WorkCriteriaBasis {
                work_id: work,
                work_revision: WorkRevision::INITIAL,
                criteria_set_revision: CriterionSetRevision::INITIAL,
                manifest_hash: WorkContentHash::parse(format!("sha256:{}", "b".repeat(64)))
                    .expect("hash"),
                member_count: 2,
            },
            &query,
            vec![entry],
        )
        .expect("page");
        assert_eq!(page.cursor.offset, 0);
        assert_eq!(page.next_cursor.expect("continuation").offset, 1);
        assert!(matches!(
            WorkCriteriaPage::from_parts(
                WorkCriteriaBasis {
                    work_id: WorkId::parse("work").expect("work"),
                    work_revision: WorkRevision::INITIAL,
                    criteria_set_revision: CriterionSetRevision::INITIAL,
                    manifest_hash: WorkContentHash::parse(format!("sha256:{}", "c".repeat(64)))
                        .expect("hash"),
                    member_count: 1,
                },
                &query,
                Vec::new(),
            ),
            Err(WorkDomainError::InvalidCriteriaPageEntries)
        ));
    }

    #[test]
    fn criterion_set_members_are_bounded_unique_and_canonical() {
        let members = vec![
            new("criterion-b", CriterionKind::HumanReview),
            new("criterion-a", CriterionKind::TestCheck),
        ];
        let references = canonical_member_refs(&change(members).members).expect("canonical refs");
        assert_eq!(references[0].criterion_id.as_str(), "criterion-a");
        assert_eq!(references[1].criterion_id.as_str(), "criterion-b");

        let duplicate = vec![
            new("criterion-a", CriterionKind::TestCheck),
            new("criterion-a", CriterionKind::HumanReview),
        ];
        assert!(matches!(
            canonical_member_refs(&duplicate),
            Err(WorkDomainError::DuplicateCriterion { criterion_id })
                if criterion_id == "criterion-a"
        ));
    }

    #[test]
    fn empty_criterion_set_is_explicitly_valid_but_has_no_members() {
        assert_eq!(
            canonical_member_refs(&change(Vec::new()).members).expect("empty set"),
            Vec::new()
        );
    }
}
