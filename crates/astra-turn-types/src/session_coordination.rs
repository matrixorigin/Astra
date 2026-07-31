use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    SEGMENTED_CONVERSATION_PROJECTION_SCHEMA_VERSION, SessionCursorV1, canonical_conversation_root,
    canonical_conversation_serialized_len,
};

pub const SESSION_COORDINATION_SCHEMA_VERSION: u32 = 1;
pub const CONVERSATION_SEGMENT_SCHEMA_VERSION: u32 = 1;
pub const CONTEXT_MANIFEST_NODE_SCHEMA_VERSION: u32 = 1;
pub const CANONICAL_TURN_DELTA_SCHEMA_VERSION: u32 = 1;
pub const EXECUTION_GRANT_SCHEMA_VERSION: u32 = 1;
pub const CONVERSATION_AUTHORITY_ENVELOPE_SCHEMA_VERSION: u32 = 1;

const SEGMENT_HASH_DOMAIN: &[u8] = b"astra.owner-scoped-conversation-segment.v1\0";
const MANIFEST_HASH_DOMAIN: &[u8] = b"astra.incremental-context-manifest.v1\0";

/// Full isolation key for one canonical conversation branch.
///
/// `isolation_domain` reserves a namespace boundary for deployments and
/// encryption domains. It must never be omitted from persistence/cache keys.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct SessionKeyV1 {
    pub schema_version: u32,
    pub isolation_domain: String,
    pub owner_user_id: String,
    pub session_id: String,
    pub branch_id: String,
}

impl SessionKeyV1 {
    pub fn owner_session(
        isolation_domain: impl Into<String>,
        owner_user_id: impl Into<String>,
        session_id: impl Into<String>,
        branch_id: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: SESSION_COORDINATION_SCHEMA_VERSION,
            isolation_domain: isolation_domain.into(),
            owner_user_id: owner_user_id.into(),
            session_id: session_id.into(),
            branch_id: branch_id.into(),
        }
    }

    pub fn validate(&self) -> Result<(), SessionCoordinationValidationError> {
        if self.schema_version != SESSION_COORDINATION_SCHEMA_VERSION {
            return Err(SessionCoordinationValidationError::UnsupportedSchema {
                entity: "session_key",
                actual: self.schema_version,
            });
        }
        for (field, value, maximum_bytes) in [
            ("isolation_domain", self.isolation_domain.as_str(), 128),
            ("owner_user_id", self.owner_user_id.as_str(), 128),
            ("session_id", self.session_id.as_str(), 128),
            ("branch_id", self.branch_id.as_str(), 128),
        ] {
            validate_identity(field, value, maximum_bytes)?;
        }
        Ok(())
    }

    pub fn validates_cursor(&self, cursor: &SessionCursorV1) -> bool {
        cursor.owner_id == self.owner_user_id
            && cursor.session_id == self.session_id
            && cursor.branch_id == self.branch_id
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActorKindV1 {
    User,
    Server,
    Cli,
    Edge,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionSurfaceV1 {
    Cli,
    Tui,
    Web,
    App,
    Server,
    Edge,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuthorityEpochsV1 {
    pub authorization_epoch: u64,
    pub device_trust_epoch: u64,
    pub permission_epoch: u64,
}

/// Authenticated request identity supplied by the composition boundary.
///
/// A coordinator still checks owner equality and authority epochs. Callers
/// must construct this from verified auth state, never from request-body
/// identity claims.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActorContextV1 {
    pub schema_version: u32,
    pub actor_user_id: String,
    pub actor_id: String,
    pub actor_kind: ActorKindV1,
    pub surface: SessionSurfaceV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    pub authority_epochs: AuthorityEpochsV1,
}

impl ActorContextV1 {
    pub fn owner_user(
        owner_user_id: impl Into<String>,
        actor_id: impl Into<String>,
        actor_kind: ActorKindV1,
        surface: SessionSurfaceV1,
        device_id: Option<String>,
        authority_epochs: AuthorityEpochsV1,
    ) -> Self {
        Self {
            schema_version: SESSION_COORDINATION_SCHEMA_VERSION,
            actor_user_id: owner_user_id.into(),
            actor_id: actor_id.into(),
            actor_kind,
            surface,
            device_id,
            authority_epochs,
        }
    }

    pub fn validate_for(
        &self,
        key: &SessionKeyV1,
    ) -> Result<(), SessionCoordinationValidationError> {
        key.validate()?;
        if self.schema_version != SESSION_COORDINATION_SCHEMA_VERSION {
            return Err(SessionCoordinationValidationError::UnsupportedSchema {
                entity: "actor_context",
                actual: self.schema_version,
            });
        }
        validate_identity("actor_user_id", &self.actor_user_id, 128)?;
        validate_identity("actor_id", &self.actor_id, 512)?;
        if let Some(device_id) = &self.device_id {
            validate_identity("device_id", device_id, 512)?;
        }
        if self.actor_user_id != key.owner_user_id {
            return Err(SessionCoordinationValidationError::OwnerMismatch);
        }
        Ok(())
    }
}

/// Immutable owner-isolated logical segment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConversationSegmentV1 {
    pub schema_version: u32,
    pub isolation_domain: String,
    pub owner_user_id: String,
    pub segment_hash: String,
    pub canonical_root_hash: String,
    pub canonical_bytes: u64,
    pub message_count: u32,
    pub messages: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationSegmentRefV1 {
    pub schema_version: u32,
    pub segment_hash: String,
    pub canonical_root_hash: String,
    pub canonical_bytes: u64,
    pub message_count: u32,
}

impl ConversationSegmentV1 {
    pub fn new(
        key: &SessionKeyV1,
        messages: Vec<Value>,
    ) -> Result<Self, SessionCoordinationValidationError> {
        key.validate()?;
        let (segment_hash, canonical_root_hash, canonical_bytes, message_count) =
            segment_identity(key, &messages)?;
        Ok(Self {
            schema_version: CONVERSATION_SEGMENT_SCHEMA_VERSION,
            isolation_domain: key.isolation_domain.clone(),
            owner_user_id: key.owner_user_id.clone(),
            segment_hash,
            canonical_root_hash,
            canonical_bytes,
            message_count,
            messages,
        })
    }

    pub fn reference(&self) -> ConversationSegmentRefV1 {
        ConversationSegmentRefV1 {
            schema_version: self.schema_version,
            segment_hash: self.segment_hash.clone(),
            canonical_root_hash: self.canonical_root_hash.clone(),
            canonical_bytes: self.canonical_bytes,
            message_count: self.message_count,
        }
    }

    pub fn validate_for(
        &self,
        key: &SessionKeyV1,
    ) -> Result<(), SessionCoordinationValidationError> {
        if self.schema_version != CONVERSATION_SEGMENT_SCHEMA_VERSION {
            return Err(SessionCoordinationValidationError::UnsupportedSchema {
                entity: "conversation_segment",
                actual: self.schema_version,
            });
        }
        if self.isolation_domain != key.isolation_domain || self.owner_user_id != key.owner_user_id
        {
            return Err(SessionCoordinationValidationError::OwnerMismatch);
        }
        let (segment_hash, canonical_root_hash, canonical_bytes, message_count) =
            segment_identity(key, &self.messages)?;
        if self.segment_hash != segment_hash
            || self.canonical_root_hash != canonical_root_hash
            || self.canonical_bytes != canonical_bytes
            || self.message_count != message_count
        {
            return Err(SessionCoordinationValidationError::HashMismatch {
                entity: "conversation_segment",
            });
        }
        Ok(())
    }
}

fn segment_identity(
    key: &SessionKeyV1,
    messages: &[Value],
) -> Result<(String, String, u64, u32), SessionCoordinationValidationError> {
    if messages.is_empty() {
        return Err(SessionCoordinationValidationError::EmptyDelta);
    }
    let message_count = u32::try_from(messages.len())
        .map_err(|_| SessionCoordinationValidationError::CountOverflow)?;
    let canonical_root_hash = canonical_conversation_root(messages);
    let canonical_bytes = canonical_conversation_serialized_len(messages);
    let segment_hash = segment_hash(
        &key.isolation_domain,
        &key.owner_user_id,
        &canonical_root_hash,
        canonical_bytes,
        message_count,
    );
    Ok((
        segment_hash,
        canonical_root_hash,
        canonical_bytes,
        message_count,
    ))
}

/// One immutable linked manifest node. Its root is the canonical root for the
/// segmented projection and commits only the parent plus changed segments.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextManifestNodeV1 {
    pub schema_version: u32,
    pub key: SessionKeyV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_manifest_root: Option<String>,
    pub completed_turn: u32,
    pub journal_event_seq: u64,
    pub conversation_seq: u64,
    pub compaction_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_version_id: Option<String>,
    pub appended_segments: Vec<ConversationSegmentRefV1>,
    pub manifest_root: String,
}

impl ContextManifestNodeV1 {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        key: SessionKeyV1,
        parent_manifest_root: Option<String>,
        completed_turn: u32,
        journal_event_seq: u64,
        conversation_seq: u64,
        compaction_generation: u64,
        config_version_id: Option<String>,
        appended_segments: Vec<ConversationSegmentRefV1>,
    ) -> Result<Self, SessionCoordinationValidationError> {
        key.validate()?;
        if appended_segments.is_empty() {
            return Err(SessionCoordinationValidationError::EmptyDelta);
        }
        for segment in &appended_segments {
            if segment.schema_version != CONVERSATION_SEGMENT_SCHEMA_VERSION {
                return Err(SessionCoordinationValidationError::UnsupportedSchema {
                    entity: "conversation_segment_ref",
                    actual: segment.schema_version,
                });
            }
            validate_hash("segment_hash", &segment.segment_hash)?;
            validate_hash("canonical_root_hash", &segment.canonical_root_hash)?;
        }
        if let Some(parent) = &parent_manifest_root {
            validate_hash("parent_manifest_root", parent)?;
        }
        let manifest_root = manifest_hash(
            &key,
            parent_manifest_root.as_deref(),
            completed_turn,
            journal_event_seq,
            conversation_seq,
            compaction_generation,
            config_version_id.as_deref(),
            &appended_segments,
        );
        Ok(Self {
            schema_version: CONTEXT_MANIFEST_NODE_SCHEMA_VERSION,
            key,
            parent_manifest_root,
            completed_turn,
            journal_event_seq,
            conversation_seq,
            compaction_generation,
            config_version_id,
            appended_segments,
            manifest_root,
        })
    }

    pub fn cursor(&self) -> SessionCursorV1 {
        SessionCursorV1 {
            schema_version: crate::SESSION_CURSOR_SCHEMA_VERSION,
            owner_id: self.key.owner_user_id.clone(),
            session_id: self.key.session_id.clone(),
            branch_id: self.key.branch_id.clone(),
            completed_turn: self.completed_turn,
            journal_event_seq: self.journal_event_seq,
            conversation_seq: self.conversation_seq,
            canonical_root_hash: self.manifest_root.clone(),
            projection_schema: SEGMENTED_CONVERSATION_PROJECTION_SCHEMA_VERSION,
            compaction_generation: self.compaction_generation,
            config_version_id: self.config_version_id.clone(),
        }
    }

    pub fn validate(&self) -> Result<(), SessionCoordinationValidationError> {
        let expected = Self::new(
            self.key.clone(),
            self.parent_manifest_root.clone(),
            self.completed_turn,
            self.journal_event_seq,
            self.conversation_seq,
            self.compaction_generation,
            self.config_version_id.clone(),
            self.appended_segments.clone(),
        )?;
        if expected.manifest_root != self.manifest_root {
            return Err(SessionCoordinationValidationError::HashMismatch {
                entity: "context_manifest",
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionContextHeadV1 {
    pub schema_version: u32,
    pub key: SessionKeyV1,
    pub cursor: SessionCursorV1,
    pub latest_manifest_root: String,
    pub total_canonical_bytes: u64,
    pub total_message_count: u64,
    pub writer_epoch: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationWriterLeaseV1 {
    pub schema_version: u32,
    pub key: SessionKeyV1,
    pub lease_id: String,
    pub writer_epoch: u64,
    pub actor: ActorContextV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_cursor: Option<SessionCursorV1>,
    pub acquired_at_unix_ms: i64,
    pub expires_at_unix_ms: i64,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnReservationV1 {
    pub schema_version: u32,
    pub reservation_id: String,
    pub key: SessionKeyV1,
    pub lease_id: String,
    pub writer_epoch: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_cursor: Option<SessionCursorV1>,
    pub reserved_turn: u32,
    pub created_at_unix_ms: i64,
    pub expires_at_unix_ms: i64,
    pub idempotency_key: String,
}

/// Authority coordinates carried by `/chat/turn`, `/chat/stream`, and Edge
/// execution messages. The token is server-issued and binds every mutable
/// generation; the request body alone is never proof of authority.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionGrantClaimsV1 {
    pub schema_version: u32,
    pub key: SessionKeyV1,
    pub actor_id: String,
    pub lease_id: String,
    pub writer_epoch: u64,
    pub authority_epochs: AuthorityEpochsV1,
    pub run_id: String,
    pub run_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_binding_id: Option<String>,
    pub provider_generation: u64,
    pub issued_at_unix_ms: i64,
    pub expires_at_unix_ms: i64,
    pub nonce: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SignedExecutionGrantV1 {
    pub schema_version: u32,
    pub claims: ExecutionGrantClaimsV1,
    pub signature: String,
}

/// Versioned transport envelope. Optional placement fields are explicit
/// coordinates, not inferred from prompt text or topology-specific state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationAuthorityEnvelopeV1 {
    pub schema_version: u32,
    pub key: SessionKeyV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_cursor: Option<SessionCursorV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_manifest_root: Option<String>,
    pub writer_epoch: u64,
    pub run_id: String,
    pub run_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_binding_id: Option<String>,
    pub provider_generation: u64,
    pub round: u32,
    pub attempt: u32,
    pub idempotency_key: String,
    pub actor_id: String,
    pub trace_id: String,
    pub execution_grant: SignedExecutionGrantV1,
}

impl ConversationAuthorityEnvelopeV1 {
    pub fn validate_shape(&self) -> Result<(), SessionCoordinationValidationError> {
        if self.schema_version != CONVERSATION_AUTHORITY_ENVELOPE_SCHEMA_VERSION {
            return Err(SessionCoordinationValidationError::UnsupportedSchema {
                entity: "conversation_authority_envelope",
                actual: self.schema_version,
            });
        }
        self.key.validate()?;
        if self
            .expected_cursor
            .as_ref()
            .is_some_and(|cursor| !self.key.validates_cursor(cursor))
        {
            return Err(SessionCoordinationValidationError::OwnerMismatch);
        }
        if let Some(root) = &self.prompt_manifest_root {
            validate_hash("prompt_manifest_root", root)?;
        }
        for (field, value) in [
            ("run_id", self.run_id.as_str()),
            ("idempotency_key", self.idempotency_key.as_str()),
            ("actor_id", self.actor_id.as_str()),
            ("trace_id", self.trace_id.as_str()),
        ] {
            validate_identity(field, value, 512)?;
        }
        if let Some(provider_binding_id) = &self.provider_binding_id {
            validate_identity("provider_binding_id", provider_binding_id, 512)?;
        }
        if self.execution_grant.schema_version != EXECUTION_GRANT_SCHEMA_VERSION
            || self.execution_grant.claims.schema_version != EXECUTION_GRANT_SCHEMA_VERSION
        {
            return Err(SessionCoordinationValidationError::UnsupportedSchema {
                entity: "execution_grant",
                actual: self.execution_grant.schema_version,
            });
        }
        Ok(())
    }
}

/// Changed logical groups and deterministic cursor coordinates for one turn.
/// The coordinator derives the new root; callers cannot choose it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CanonicalTurnDeltaV1 {
    pub schema_version: u32,
    pub completed_turn: u32,
    pub journal_event_seq: u64,
    pub conversation_seq: u64,
    pub compaction_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_version_id: Option<String>,
    pub logical_segments: Vec<Vec<Value>>,
}

impl CanonicalTurnDeltaV1 {
    pub fn validate(&self) -> Result<(), SessionCoordinationValidationError> {
        if self.schema_version != CANONICAL_TURN_DELTA_SCHEMA_VERSION {
            return Err(SessionCoordinationValidationError::UnsupportedSchema {
                entity: "canonical_turn_delta",
                actual: self.schema_version,
            });
        }
        if self.logical_segments.is_empty() || self.logical_segments.iter().any(Vec::is_empty) {
            return Err(SessionCoordinationValidationError::EmptyDelta);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum CoordinatorMutationV1 {
    Applied {
        cursor: SessionCursorV1,
    },
    AlreadyApplied {
        cursor: SessionCursorV1,
    },
    Conflict {
        current_cursor: Option<SessionCursorV1>,
        safe_options: Vec<CoordinatorConflictOptionV1>,
    },
    NeedsRepair {
        reason: String,
        safe_cursor: Option<SessionCursorV1>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CoordinatorConflictOptionV1 {
    Refresh,
    RetryAfterLeaseExpiry,
    AttachReadOnly,
    Fork,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SessionCoordinationValidationError {
    #[error("unsupported {entity} schema version {actual}")]
    UnsupportedSchema { entity: &'static str, actual: u32 },
    #[error("{field} must be non-empty and at most {maximum_bytes} bytes")]
    InvalidIdentity {
        field: &'static str,
        maximum_bytes: usize,
    },
    #[error("{field} must be a lowercase SHA-256 hex digest")]
    InvalidHash { field: &'static str },
    #[error("actor or content owner does not match the session owner")]
    OwnerMismatch,
    #[error("{entity} hash does not match its content")]
    HashMismatch { entity: &'static str },
    #[error("canonical turn delta must contain non-empty logical segments")]
    EmptyDelta,
    #[error("message count exceeds the protocol limit")]
    CountOverflow,
}

fn validate_identity(
    field: &'static str,
    value: &str,
    maximum_bytes: usize,
) -> Result<(), SessionCoordinationValidationError> {
    if value.is_empty() || value.len() > maximum_bytes || value.chars().any(char::is_control) {
        return Err(SessionCoordinationValidationError::InvalidIdentity {
            field,
            maximum_bytes,
        });
    }
    Ok(())
}

fn validate_hash(
    field: &'static str,
    value: &str,
) -> Result<(), SessionCoordinationValidationError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(SessionCoordinationValidationError::InvalidHash { field });
    }
    Ok(())
}

fn hash_field(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
}

fn segment_hash(
    isolation_domain: &str,
    owner_user_id: &str,
    canonical_root_hash: &str,
    canonical_bytes: u64,
    message_count: u32,
) -> String {
    let mut digest = Sha256::new();
    digest.update(SEGMENT_HASH_DOMAIN);
    hash_field(&mut digest, isolation_domain.as_bytes());
    hash_field(&mut digest, owner_user_id.as_bytes());
    hash_field(&mut digest, canonical_root_hash.as_bytes());
    digest.update(canonical_bytes.to_be_bytes());
    digest.update(message_count.to_be_bytes());
    format!("{:x}", digest.finalize())
}

#[allow(clippy::too_many_arguments)]
fn manifest_hash(
    key: &SessionKeyV1,
    parent_manifest_root: Option<&str>,
    completed_turn: u32,
    journal_event_seq: u64,
    conversation_seq: u64,
    compaction_generation: u64,
    config_version_id: Option<&str>,
    segments: &[ConversationSegmentRefV1],
) -> String {
    let mut digest = Sha256::new();
    digest.update(MANIFEST_HASH_DOMAIN);
    hash_field(&mut digest, key.isolation_domain.as_bytes());
    hash_field(&mut digest, key.owner_user_id.as_bytes());
    hash_field(&mut digest, key.session_id.as_bytes());
    hash_field(&mut digest, key.branch_id.as_bytes());
    hash_field(
        &mut digest,
        parent_manifest_root.unwrap_or_default().as_bytes(),
    );
    digest.update(completed_turn.to_be_bytes());
    digest.update(journal_event_seq.to_be_bytes());
    digest.update(conversation_seq.to_be_bytes());
    digest.update(compaction_generation.to_be_bytes());
    hash_field(
        &mut digest,
        config_version_id.unwrap_or_default().as_bytes(),
    );
    digest.update((segments.len() as u64).to_be_bytes());
    for segment in segments {
        hash_field(&mut digest, segment.segment_hash.as_bytes());
        hash_field(&mut digest, segment.canonical_root_hash.as_bytes());
        digest.update(segment.canonical_bytes.to_be_bytes());
        digest.update(segment.message_count.to_be_bytes());
    }
    format!("{:x}", digest.finalize())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn key(owner: &str) -> SessionKeyV1 {
        SessionKeyV1::owner_session("prod", owner, "same-session", "main")
    }

    #[test]
    fn equal_content_is_not_deduplicated_across_owners() {
        let messages = vec![json!({"role": "user", "content": "private"})];
        let a = ConversationSegmentV1::new(&key("owner-a"), messages.clone()).unwrap();
        let b = ConversationSegmentV1::new(&key("owner-b"), messages).unwrap();

        assert_ne!(a.segment_hash, b.segment_hash);
        assert_eq!(a.canonical_root_hash, b.canonical_root_hash);
    }

    #[test]
    fn manifest_root_binds_parent_cursor_coordinates_and_ordered_delta() {
        let key = key("owner-a");
        let a = ConversationSegmentV1::new(&key, vec![json!({"role": "user", "content": "a"})])
            .unwrap();
        let b =
            ConversationSegmentV1::new(&key, vec![json!({"role": "assistant", "content": "b"})])
                .unwrap();
        let node = ContextManifestNodeV1::new(
            key.clone(),
            None,
            1,
            1,
            1,
            0,
            None,
            vec![a.reference(), b.reference()],
        )
        .unwrap();
        let reversed = ContextManifestNodeV1::new(
            key.clone(),
            None,
            1,
            1,
            1,
            0,
            None,
            vec![b.reference(), a.reference()],
        )
        .unwrap();
        let child = ContextManifestNodeV1::new(
            key,
            Some(node.manifest_root.clone()),
            2,
            2,
            2,
            0,
            None,
            vec![a.reference()],
        )
        .unwrap();

        assert_ne!(node.manifest_root, reversed.manifest_root);
        assert_ne!(node.manifest_root, child.manifest_root);
        assert_eq!(child.cursor().canonical_root_hash, child.manifest_root);
        assert_eq!(
            child.cursor().projection_schema,
            SEGMENTED_CONVERSATION_PROJECTION_SCHEMA_VERSION
        );
    }
}
