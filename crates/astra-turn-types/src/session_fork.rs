use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    ConversationWriterLeaseV1, SEGMENTED_CONVERSATION_PROJECTION_SCHEMA_VERSION,
    SESSION_COORDINATION_SCHEMA_VERSION, SESSION_CURSOR_SCHEMA_VERSION, SessionContextHeadV1,
    SessionCursorV1, SessionKeyV1,
};

pub const SESSION_FORK_MANIFEST_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ForkBasisDimensionV1 {
    Conversation,
    TaskBoard,
    Checkpoint,
    Workspace,
    Artifacts,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ForkDimensionDispositionV1 {
    SharedPrefix,
    Copied,
    Rebased,
    Gap,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForkDimensionEvidenceV1 {
    pub dimension: ForkBasisDimensionV1,
    pub disposition: ForkDimensionDispositionV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_cursor: Option<SessionCursorV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ForkExcludedAuthorityV1 {
    Run,
    WriterLease,
    Approval,
    Mailbox,
    Invocation,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionForkStateV1 {
    Prepared,
    Active,
    Aborted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SharedManifestPrefixV1 {
    pub parent_key: SessionKeyV1,
    pub parent_cursor: SessionCursorV1,
    pub parent_manifest_root: String,
    pub total_canonical_bytes: u64,
    pub total_message_count: u64,
}

impl SharedManifestPrefixV1 {
    pub fn validate_for_child(
        &self,
        child_key: &SessionKeyV1,
    ) -> Result<(), SessionForkValidationError> {
        self.parent_key
            .validate()
            .map_err(|_| SessionForkValidationError::InvalidIdentity)?;
        child_key
            .validate()
            .map_err(|_| SessionForkValidationError::InvalidIdentity)?;
        if self.parent_key.isolation_domain != child_key.isolation_domain
            || self.parent_key.owner_user_id != child_key.owner_user_id
            || !self.parent_key.validates_cursor(&self.parent_cursor)
            || self.parent_cursor.schema_version != SESSION_CURSOR_SCHEMA_VERSION
            || self.parent_cursor.projection_schema
                != SEGMENTED_CONVERSATION_PROJECTION_SCHEMA_VERSION
            || self.parent_cursor.canonical_root_hash != self.parent_manifest_root
            || self.parent_key == *child_key
        {
            return Err(SessionForkValidationError::OwnerOrLineageMismatch);
        }
        validate_hash(&self.parent_manifest_root)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionForkManifestV1 {
    pub schema_version: u32,
    pub fork_id: String,
    pub parent_key: SessionKeyV1,
    pub child_key: SessionKeyV1,
    pub parent_head: SessionContextHeadV1,
    pub dimensions: Vec<ForkDimensionEvidenceV1>,
    pub excluded_authority: Vec<ForkExcludedAuthorityV1>,
    pub state: SessionForkStateV1,
    pub created_at_unix_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activated_at_unix_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_detail: Option<String>,
}

impl SessionForkManifestV1 {
    pub fn shared_prefix(&self) -> SharedManifestPrefixV1 {
        SharedManifestPrefixV1 {
            parent_key: self.parent_key.clone(),
            parent_cursor: self.parent_head.cursor.clone(),
            parent_manifest_root: self.parent_head.latest_manifest_root.clone(),
            total_canonical_bytes: self.parent_head.total_canonical_bytes,
            total_message_count: self.parent_head.total_message_count,
        }
    }

    pub fn validate(&self) -> Result<(), SessionForkValidationError> {
        if self.schema_version != SESSION_FORK_MANIFEST_SCHEMA_VERSION {
            return Err(SessionForkValidationError::UnsupportedSchema);
        }
        validate_identity(&self.fork_id)?;
        self.shared_prefix().validate_for_child(&self.child_key)?;
        if self.parent_head.schema_version != SESSION_COORDINATION_SCHEMA_VERSION
            || self.parent_head.key != self.parent_key
            || self.parent_head.cursor.canonical_root_hash != self.parent_head.latest_manifest_root
            || self.parent_head.total_canonical_bytes == 0
            || self.parent_head.total_message_count == 0
            || self.created_at_unix_ms < 0
            || self
                .activated_at_unix_ms
                .is_some_and(|activated| activated < self.created_at_unix_ms)
            || (self.state == SessionForkStateV1::Active && self.activated_at_unix_ms.is_none())
            || (self.state != SessionForkStateV1::Active && self.activated_at_unix_ms.is_some())
        {
            return Err(SessionForkValidationError::InvalidCoordinate);
        }

        let expected_dimensions = HashSet::from([
            ForkBasisDimensionV1::Conversation,
            ForkBasisDimensionV1::TaskBoard,
            ForkBasisDimensionV1::Checkpoint,
            ForkBasisDimensionV1::Workspace,
            ForkBasisDimensionV1::Artifacts,
        ]);
        let dimensions = self
            .dimensions
            .iter()
            .map(|dimension| dimension.dimension)
            .collect::<HashSet<_>>();
        if dimensions != expected_dimensions || dimensions.len() != self.dimensions.len() {
            return Err(SessionForkValidationError::InvalidDimensions);
        }
        for dimension in &self.dimensions {
            if let Some(cursor) = &dimension.source_cursor
                && (!self.parent_key.validates_cursor(cursor)
                    || cursor.schema_version != SESSION_CURSOR_SCHEMA_VERSION
                    || cursor.projection_schema != SEGMENTED_CONVERSATION_PROJECTION_SCHEMA_VERSION)
            {
                return Err(SessionForkValidationError::OwnerOrLineageMismatch);
            }
            if let Some(digest) = &dimension.evidence_digest {
                validate_hash(digest)?;
            }
            if dimension.disposition == ForkDimensionDispositionV1::Gap
                && dimension.detail.as_deref().is_none_or(str::is_empty)
            {
                return Err(SessionForkValidationError::InvalidDimensions);
            }
        }
        let excluded = self
            .excluded_authority
            .iter()
            .copied()
            .collect::<HashSet<_>>();
        if excluded
            != HashSet::from([
                ForkExcludedAuthorityV1::Run,
                ForkExcludedAuthorityV1::WriterLease,
                ForkExcludedAuthorityV1::Approval,
                ForkExcludedAuthorityV1::Mailbox,
                ForkExcludedAuthorityV1::Invocation,
            ])
            || excluded.len() != self.excluded_authority.len()
        {
            return Err(SessionForkValidationError::TransientAuthorityInherited);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionForkActivationV1 {
    pub manifest: SessionForkManifestV1,
    pub child_head: SessionContextHeadV1,
    pub writer_lease: ConversationWriterLeaseV1,
}

impl SessionForkActivationV1 {
    pub fn validate(&self) -> Result<(), SessionForkValidationError> {
        self.manifest.validate()?;
        if self.manifest.state != SessionForkStateV1::Active
            || self.child_head.schema_version != SESSION_COORDINATION_SCHEMA_VERSION
            || self.child_head.key != self.manifest.child_key
            || !self
                .manifest
                .child_key
                .validates_cursor(&self.child_head.cursor)
            || self.child_head.cursor.schema_version != SESSION_CURSOR_SCHEMA_VERSION
            || self.child_head.cursor.projection_schema
                != SEGMENTED_CONVERSATION_PROJECTION_SCHEMA_VERSION
            || self.child_head.latest_manifest_root
                != self.manifest.parent_head.latest_manifest_root
            || self.child_head.cursor.canonical_root_hash
                != self.manifest.parent_head.latest_manifest_root
            || self.child_head.total_canonical_bytes
                != self.manifest.parent_head.total_canonical_bytes
            || self.child_head.total_message_count != self.manifest.parent_head.total_message_count
            || self.writer_lease.schema_version != SESSION_COORDINATION_SCHEMA_VERSION
            || self.writer_lease.key != self.manifest.child_key
            || self.writer_lease.expected_cursor.as_ref() != Some(&self.child_head.cursor)
            || self.writer_lease.writer_epoch <= self.child_head.writer_epoch
            || self
                .writer_lease
                .actor
                .validate_for(&self.manifest.child_key)
                .is_err()
        {
            return Err(SessionForkValidationError::InvalidActivation);
        }
        Ok(())
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SessionForkValidationError {
    #[error("unsupported session fork manifest schema")]
    UnsupportedSchema,
    #[error("fork identity is invalid")]
    InvalidIdentity,
    #[error("fork owner or lineage coordinates do not match")]
    OwnerOrLineageMismatch,
    #[error("fork coordinate is invalid")]
    InvalidCoordinate,
    #[error("fork dimensions are incomplete or duplicated")]
    InvalidDimensions,
    #[error("transient execution authority cannot be inherited by a fork")]
    TransientAuthorityInherited,
    #[error("fork activation does not match the prepared child coordinates")]
    InvalidActivation,
}

fn validate_identity(value: &str) -> Result<(), SessionForkValidationError> {
    if value.is_empty() || value.len() > 128 || value.chars().any(char::is_control) {
        Err(SessionForkValidationError::InvalidIdentity)
    } else {
        Ok(())
    }
}

fn validate_hash(value: &str) -> Result<(), SessionForkValidationError> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(SessionForkValidationError::InvalidCoordinate)
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        SEGMENTED_CONVERSATION_PROJECTION_SCHEMA_VERSION, SESSION_COORDINATION_SCHEMA_VERSION,
    };

    use super::*;

    fn manifest() -> SessionForkManifestV1 {
        let parent_key = SessionKeyV1::owner_session("server", "owner-a", "parent", "main");
        let child_key = SessionKeyV1::owner_session("server", "owner-a", "child", "main");
        let cursor = SessionCursorV1 {
            schema_version: crate::SESSION_CURSOR_SCHEMA_VERSION,
            owner_id: "owner-a".into(),
            session_id: "parent".into(),
            branch_id: "main".into(),
            completed_turn: 3,
            journal_event_seq: 3,
            conversation_seq: 3,
            canonical_root_hash: "a".repeat(64),
            projection_schema: SEGMENTED_CONVERSATION_PROJECTION_SCHEMA_VERSION,
            compaction_generation: 0,
            config_version_id: None,
        };
        SessionForkManifestV1 {
            schema_version: SESSION_FORK_MANIFEST_SCHEMA_VERSION,
            fork_id: "fork-a".into(),
            parent_key: parent_key.clone(),
            child_key: child_key.clone(),
            parent_head: SessionContextHeadV1 {
                schema_version: SESSION_COORDINATION_SCHEMA_VERSION,
                key: parent_key,
                cursor,
                latest_manifest_root: "a".repeat(64),
                total_canonical_bytes: 100,
                total_message_count: 6,
                writer_epoch: 2,
            },
            dimensions: [
                ForkBasisDimensionV1::Conversation,
                ForkBasisDimensionV1::TaskBoard,
                ForkBasisDimensionV1::Checkpoint,
                ForkBasisDimensionV1::Workspace,
                ForkBasisDimensionV1::Artifacts,
            ]
            .into_iter()
            .map(|dimension| ForkDimensionEvidenceV1 {
                dimension,
                disposition: if dimension == ForkBasisDimensionV1::Conversation {
                    ForkDimensionDispositionV1::SharedPrefix
                } else {
                    ForkDimensionDispositionV1::Gap
                },
                source_cursor: None,
                evidence_digest: None,
                detail: (dimension != ForkBasisDimensionV1::Conversation)
                    .then(|| "not available".into()),
            })
            .collect(),
            excluded_authority: vec![
                ForkExcludedAuthorityV1::Run,
                ForkExcludedAuthorityV1::WriterLease,
                ForkExcludedAuthorityV1::Approval,
                ForkExcludedAuthorityV1::Mailbox,
                ForkExcludedAuthorityV1::Invocation,
            ],
            state: SessionForkStateV1::Prepared,
            created_at_unix_ms: 1,
            activated_at_unix_ms: None,
            status_detail: None,
        }
    }

    #[test]
    fn complete_manifest_excludes_all_transient_authority() {
        manifest().validate().unwrap();
        let mut invalid = manifest();
        invalid
            .excluded_authority
            .retain(|authority| *authority != ForkExcludedAuthorityV1::Invocation);
        assert_eq!(
            invalid.validate(),
            Err(SessionForkValidationError::TransientAuthorityInherited)
        );
    }
}
