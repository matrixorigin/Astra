use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use thiserror::Error;

use crate::{
    ActorContextV1, AuthorityEpochsV1, ContextManifestNodeV1, SessionContextHeadV1,
    SessionCursorV1, SessionKeyV1, SharedManifestPrefixV1,
};

pub const SESSION_HANDOFF_SCHEMA_VERSION: u32 = 1;
pub const SESSION_ATTACHMENT_SCHEMA_VERSION: u32 = 1;
pub const MANIFEST_DELTA_SCHEMA_VERSION: u32 = 1;
pub const MAX_HANDOFF_EFFECT_IDENTITIES: usize = 1_024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionPlacementV1 {
    Server,
    Cli,
    Edge,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionAttachmentModeV1 {
    ReadOnly,
    Controller,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionHandoffModeV1 {
    Graceful,
    Forced,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionHandoffStateV1 {
    Requested,
    Validating,
    Draining,
    Checkpointed,
    Fencing,
    Fenced,
    Hydrating,
    Active,
    Blocked,
    Aborted,
    NeedsReconciliation,
}

impl SessionHandoffStateV1 {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Active | Self::Aborted)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceHandoffEvidenceV1 {
    pub workspace_id: String,
    pub revision: String,
    pub fingerprint_digest: String,
    pub authority: String,
    pub capability_digest: String,
    pub policy_digest: String,
}

impl WorkspaceHandoffEvidenceV1 {
    pub fn validate(&self) -> Result<(), SessionHandoffValidationError> {
        for (field, value) in [
            ("workspace_id", self.workspace_id.as_str()),
            ("revision", self.revision.as_str()),
            ("fingerprint_digest", self.fingerprint_digest.as_str()),
            ("authority", self.authority.as_str()),
            ("capability_digest", self.capability_digest.as_str()),
            ("policy_digest", self.policy_digest.as_str()),
        ] {
            validate_identity(field, value, 512)?;
        }
        validate_hash("workspace fingerprint digest", &self.fingerprint_digest)?;
        validate_hash("capability digest", &self.capability_digest)?;
        validate_hash("policy digest", &self.policy_digest)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestDeltaV1 {
    pub schema_version: u32,
    pub key: SessionKeyV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_manifest_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<SessionContextHeadV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_prefix: Option<SharedManifestPrefixV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_nodes: Vec<ContextManifestNodeV1>,
    pub missing_canonical_bytes: u64,
    pub missing_message_count: u64,
}

impl ManifestDeltaV1 {
    pub fn validate(&self) -> Result<(), SessionHandoffValidationError> {
        if self.schema_version != MANIFEST_DELTA_SCHEMA_VERSION {
            return Err(SessionHandoffValidationError::UnsupportedSchema);
        }
        self.key
            .validate()
            .map_err(|_| SessionHandoffValidationError::InvalidIdentity {
                field: "session_key",
            })?;
        if let Some(root) = &self.after_manifest_root {
            validate_hash("after manifest root", root)?;
        }
        if self.head.as_ref().is_some_and(|head| head.key != self.key)
            || self.missing_nodes.iter().any(|node| node.key != self.key)
        {
            return Err(SessionHandoffValidationError::OwnerMismatch);
        }
        for node in &self.missing_nodes {
            node.validate()
                .map_err(|_| SessionHandoffValidationError::InvalidManifestDelta)?;
        }
        match &self.head {
            None => {
                if self.after_manifest_root.is_some()
                    || self.shared_prefix.is_some()
                    || !self.missing_nodes.is_empty()
                    || self.missing_canonical_bytes != 0
                    || self.missing_message_count != 0
                {
                    return Err(SessionHandoffValidationError::InvalidManifestDelta);
                }
            }
            Some(head) => {
                if let Some(prefix) = &self.shared_prefix {
                    prefix
                        .validate_for_child(&self.key)
                        .map_err(|_| SessionHandoffValidationError::InvalidManifestDelta)?;
                    if self.after_manifest_root.is_some() {
                        return Err(SessionHandoffValidationError::InvalidManifestDelta);
                    }
                }
                let mut expected_parent = self.after_manifest_root.as_deref().or_else(|| {
                    self.shared_prefix
                        .as_ref()
                        .map(|prefix| prefix.parent_manifest_root.as_str())
                });
                for (index, node) in self.missing_nodes.iter().enumerate() {
                    let replacement_from_empty =
                        index == 0 && expected_parent.is_none() && node.replaces_history;
                    if !replacement_from_empty
                        && node.parent_manifest_root.as_deref() != expected_parent
                    {
                        return Err(SessionHandoffValidationError::InvalidManifestDelta);
                    }
                    expected_parent = Some(node.manifest_root.as_str());
                }
                if expected_parent != Some(head.latest_manifest_root.as_str())
                    || head.cursor.canonical_root_hash != head.latest_manifest_root
                {
                    return Err(SessionHandoffValidationError::InvalidManifestDelta);
                }
                let (bytes, messages) = self
                    .missing_nodes
                    .iter()
                    .try_fold((0_u64, 0_u64), |(bytes, messages), node| {
                        node.appended_segments.iter().try_fold(
                            (bytes, messages),
                            |(bytes, messages), segment| {
                                Some((
                                    bytes.checked_add(segment.canonical_bytes)?,
                                    messages.checked_add(u64::from(segment.message_count))?,
                                ))
                            },
                        )
                    })
                    .ok_or(SessionHandoffValidationError::CoordinateOverflow)?;
                if bytes != self.missing_canonical_bytes || messages != self.missing_message_count {
                    return Err(SessionHandoffValidationError::InvalidManifestDelta);
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionAttachmentV1 {
    pub schema_version: u32,
    pub attachment_id: String,
    pub attachment_epoch: u64,
    pub key: SessionKeyV1,
    pub actor: ActorContextV1,
    pub mode: SessionAttachmentModeV1,
    pub placement: SessionPlacementV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_cursor: Option<SessionCursorV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_manifest_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceHandoffEvidenceV1>,
    pub attached_at_unix_ms: i64,
    pub expires_at_unix_ms: i64,
}

impl SessionAttachmentV1 {
    pub fn validate(&self) -> Result<(), SessionHandoffValidationError> {
        if self.schema_version != SESSION_ATTACHMENT_SCHEMA_VERSION {
            return Err(SessionHandoffValidationError::UnsupportedSchema);
        }
        self.actor
            .validate_for(&self.key)
            .map_err(|_| SessionHandoffValidationError::OwnerMismatch)?;
        validate_identity("attachment_id", &self.attachment_id, 128)?;
        if self.attachment_epoch == 0 || self.expires_at_unix_ms <= self.attached_at_unix_ms {
            return Err(SessionHandoffValidationError::InvalidCoordinate);
        }
        if self
            .observed_cursor
            .as_ref()
            .is_some_and(|cursor| !self.key.validates_cursor(cursor))
        {
            return Err(SessionHandoffValidationError::OwnerMismatch);
        }
        if let Some(root) = &self.observed_manifest_root {
            validate_hash("observed manifest root", root)?;
        }
        if let Some(workspace) = &self.workspace {
            workspace.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct HandoffOperationWatermarksV1 {
    pub run_id: Option<String>,
    pub run_generation: Option<u64>,
    pub checkpoint_id: Option<String>,
    pub effect_cursor: Option<String>,
    pub provider_binding_id: Option<String>,
    pub provider_generation: Option<u64>,
    pub delivery_generation: Option<u64>,
    pub pending_invocation_count: u32,
    pub pending_approval_count: u32,
    pub pending_outbox_count: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct HandoffRiskEvidenceV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unsynced_suffix_root: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unknown_effect_invocation_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forced_authorization_id: Option<String>,
    /// Database time at which the Server fenced the old authority, stopped
    /// every durable run, and sealed the complete set of possibly dispatched
    /// effects. Absence means takeover is not yet safe to hydrate or expose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effects_sealed_at_unix_ms: Option<i64>,
}

impl HandoffRiskEvidenceV1 {
    pub fn validate(&self) -> Result<(), SessionHandoffValidationError> {
        if self.unknown_effect_invocation_ids.len() > MAX_HANDOFF_EFFECT_IDENTITIES {
            return Err(SessionHandoffValidationError::TooManyEffects);
        }
        if let Some(root) = &self.unsynced_suffix_root {
            validate_hash("unsynced suffix root", root)?;
        }
        if let Some(id) = &self.forced_authorization_id {
            validate_identity("forced_authorization_id", id, 512)?;
        }
        if self.effects_sealed_at_unix_ms.is_some_and(|time| time <= 0) {
            return Err(SessionHandoffValidationError::InvalidCoordinate);
        }
        for id in &self.unknown_effect_invocation_ids {
            validate_identity("unknown_effect_invocation_id", id, 512)?;
        }
        if self
            .unknown_effect_invocation_ids
            .iter()
            .collect::<HashSet<_>>()
            .len()
            != self.unknown_effect_invocation_ids.len()
        {
            return Err(SessionHandoffValidationError::DuplicateEffect);
        }
        Ok(())
    }

    pub fn permits_forced_fence(&self) -> bool {
        self.forced_authorization_id.is_some()
    }

    pub fn effects_are_sealed(&self) -> bool {
        self.effects_sealed_at_unix_ms.is_some()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionHandoffRecordV1 {
    pub schema_version: u32,
    pub handoff_id: String,
    pub idempotency_key: String,
    pub key: SessionKeyV1,
    pub state: SessionHandoffStateV1,
    pub mode: SessionHandoffModeV1,
    pub from_attachment_id: Option<String>,
    pub to_attachment_id: Option<String>,
    pub from_placement: SessionPlacementV1,
    pub to_placement: SessionPlacementV1,
    pub target_actor: ActorContextV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_cursor: Option<SessionCursorV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_writer_epoch: Option<u64>,
    pub authority_epochs: AuthorityEpochsV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceHandoffEvidenceV1>,
    #[serde(default)]
    pub watermarks: HandoffOperationWatermarksV1,
    #[serde(default)]
    pub risk: HandoffRiskEvidenceV1,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_detail: Option<String>,
    /// Exact operation state interrupted by `Blocked`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_from: Option<SessionHandoffStateV1>,
    pub deadline_unix_ms: i64,
    pub created_at_unix_ms: i64,
    pub updated_at_unix_ms: i64,
    pub transition_seq: u64,
}

impl SessionHandoffRecordV1 {
    pub fn validate(&self) -> Result<(), SessionHandoffValidationError> {
        if self.schema_version != SESSION_HANDOFF_SCHEMA_VERSION {
            return Err(SessionHandoffValidationError::UnsupportedSchema);
        }
        validate_identity("handoff_id", &self.handoff_id, 128)?;
        validate_identity("idempotency_key", &self.idempotency_key, 512)?;
        validate_identity("reason", &self.reason, 1_024)?;
        if let Some(detail) = &self.status_detail {
            validate_identity("status_detail", detail, 2_048)?;
        }
        self.target_actor
            .validate_for(&self.key)
            .map_err(|_| SessionHandoffValidationError::OwnerMismatch)?;
        if self.target_actor.authority_epochs != self.authority_epochs {
            return Err(SessionHandoffValidationError::AuthorityMismatch);
        }
        if self
            .base_cursor
            .as_ref()
            .is_some_and(|cursor| !self.key.validates_cursor(cursor))
        {
            return Err(SessionHandoffValidationError::OwnerMismatch);
        }
        if self.transition_seq == 0
            || self.deadline_unix_ms <= self.created_at_unix_ms
            || self.updated_at_unix_ms < self.created_at_unix_ms
            || (self.state != SessionHandoffStateV1::Blocked && self.blocked_from.is_some())
            || self.blocked_from == Some(SessionHandoffStateV1::Blocked)
        {
            return Err(SessionHandoffValidationError::InvalidCoordinate);
        }
        if let Some(workspace) = &self.workspace {
            workspace.validate()?;
        }
        self.risk.validate()?;
        if self.mode == SessionHandoffModeV1::Forced && !self.risk.permits_forced_fence() {
            return Err(SessionHandoffValidationError::ForcedEvidenceRequired);
        }
        if self.mode == SessionHandoffModeV1::Forced
            && matches!(
                self.state,
                SessionHandoffStateV1::Hydrating | SessionHandoffStateV1::Active
            )
            && !self.risk.effects_are_sealed()
        {
            return Err(SessionHandoffValidationError::ForcedEffectsNotSealed);
        }
        Ok(())
    }

    pub fn transition(
        &mut self,
        expected: SessionHandoffStateV1,
        next: SessionHandoffStateV1,
        now_unix_ms: i64,
    ) -> Result<(), SessionHandoffValidationError> {
        if self.state != expected {
            return Err(SessionHandoffValidationError::StateConflict);
        }
        let resumes_blocked_operation = expected == SessionHandoffStateV1::Blocked
            && (self.blocked_from == Some(next)
                || (self.blocked_from.is_none() && next == SessionHandoffStateV1::Validating));
        if !resumes_blocked_operation && !valid_transition(self.mode, expected, next) {
            return Err(SessionHandoffValidationError::InvalidTransition {
                from: expected,
                to: next,
            });
        }
        if now_unix_ms < self.updated_at_unix_ms {
            return Err(SessionHandoffValidationError::InvalidCoordinate);
        }
        if next == SessionHandoffStateV1::Blocked {
            self.blocked_from = Some(expected);
        } else if expected == SessionHandoffStateV1::Blocked {
            self.blocked_from = None;
        }
        self.state = next;
        self.updated_at_unix_ms = now_unix_ms;
        self.transition_seq = self
            .transition_seq
            .checked_add(1)
            .ok_or(SessionHandoffValidationError::CoordinateOverflow)?;
        Ok(())
    }
}

pub fn valid_transition(
    mode: SessionHandoffModeV1,
    from: SessionHandoffStateV1,
    to: SessionHandoffStateV1,
) -> bool {
    use SessionHandoffStateV1 as State;
    if from == to {
        return false;
    }
    if matches!(to, State::Blocked | State::Aborted)
        && !matches!(from, State::Active | State::Aborted)
    {
        return true;
    }
    matches!(
        (mode, from, to),
        (_, State::Requested, State::Validating)
            | (_, State::Fencing, State::Fenced)
            | (_, State::Fenced, State::Hydrating)
            | (_, State::Hydrating, State::Active)
            | (_, State::NeedsReconciliation, State::Fencing)
            | (
                SessionHandoffModeV1::Graceful,
                State::Validating,
                State::Draining
            )
            | (
                SessionHandoffModeV1::Graceful,
                State::Draining,
                State::Checkpointed
            )
            | (
                SessionHandoffModeV1::Graceful,
                State::Checkpointed,
                State::Fencing
            )
            | (
                SessionHandoffModeV1::Forced,
                State::Validating,
                State::Fencing
            )
            | (
                SessionHandoffModeV1::Forced,
                State::Validating,
                State::NeedsReconciliation
            )
    )
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SessionHandoffValidationError {
    #[error("unsupported handoff schema version")]
    UnsupportedSchema,
    #[error("invalid handoff identity field {field}")]
    InvalidIdentity { field: &'static str },
    #[error("handoff owner or cursor does not match the SessionKey")]
    OwnerMismatch,
    #[error("target actor authority epochs do not match the handoff")]
    AuthorityMismatch,
    #[error("invalid handoff hash: {0}")]
    InvalidHash(&'static str),
    #[error("handoff coordinate or timestamp is invalid")]
    InvalidCoordinate,
    #[error("handoff coordinate overflow")]
    CoordinateOverflow,
    #[error("handoff state changed concurrently")]
    StateConflict,
    #[error("invalid handoff transition {from:?} -> {to:?}")]
    InvalidTransition {
        from: SessionHandoffStateV1,
        to: SessionHandoffStateV1,
    },
    #[error("forced takeover requires a verified authorization identity")]
    ForcedEvidenceRequired,
    #[error("forced takeover effects were not sealed after fencing")]
    ForcedEffectsNotSealed,
    #[error("too many unknown effect identities")]
    TooManyEffects,
    #[error("duplicate unknown effect identity")]
    DuplicateEffect,
    #[error("manifest delta is not a complete, ordered suffix of its head")]
    InvalidManifestDelta,
}

fn validate_identity(
    field: &'static str,
    value: &str,
    maximum_bytes: usize,
) -> Result<(), SessionHandoffValidationError> {
    if value.is_empty() || value.len() > maximum_bytes || value.chars().any(char::is_control) {
        return Err(SessionHandoffValidationError::InvalidIdentity { field });
    }
    Ok(())
}

fn validate_hash(field: &'static str, value: &str) -> Result<(), SessionHandoffValidationError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(SessionHandoffValidationError::InvalidHash(field));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ActorKindV1, ConversationSegmentV1, SESSION_COORDINATION_SCHEMA_VERSION, SessionSurfaceV1,
    };
    use serde_json::json;

    fn record(mode: SessionHandoffModeV1) -> SessionHandoffRecordV1 {
        let key = SessionKeyV1::owner_session("test", "owner", "session", "main");
        SessionHandoffRecordV1 {
            schema_version: SESSION_HANDOFF_SCHEMA_VERSION,
            handoff_id: "handoff-1".into(),
            idempotency_key: "request-1".into(),
            key: key.clone(),
            state: SessionHandoffStateV1::Requested,
            mode,
            from_attachment_id: Some("attachment-a".into()),
            to_attachment_id: None,
            from_placement: SessionPlacementV1::Cli,
            to_placement: SessionPlacementV1::Server,
            target_actor: ActorContextV1::owner_user(
                "owner",
                "actor-b",
                ActorKindV1::Server,
                SessionSurfaceV1::Server,
                None,
                AuthorityEpochsV1::default(),
            ),
            base_cursor: None,
            target_writer_epoch: None,
            authority_epochs: AuthorityEpochsV1::default(),
            workspace: None,
            watermarks: HandoffOperationWatermarksV1::default(),
            risk: if mode == SessionHandoffModeV1::Forced {
                HandoffRiskEvidenceV1 {
                    unsynced_suffix_root: None,
                    unknown_effect_invocation_ids: vec!["invocation-unknown".into()],
                    forced_authorization_id: Some("reauth-proof".into()),
                    effects_sealed_at_unix_ms: None,
                }
            } else {
                HandoffRiskEvidenceV1::default()
            },
            reason: "move".into(),
            status_detail: None,
            blocked_from: None,
            deadline_unix_ms: 10_000,
            created_at_unix_ms: 1_000,
            updated_at_unix_ms: 1_000,
            transition_seq: 1,
        }
    }

    #[test]
    fn graceful_and_forced_paths_are_structurally_distinct() {
        let mut graceful = record(SessionHandoffModeV1::Graceful);
        graceful
            .transition(
                SessionHandoffStateV1::Requested,
                SessionHandoffStateV1::Validating,
                1_001,
            )
            .unwrap();
        assert!(matches!(
            graceful.transition(
                SessionHandoffStateV1::Validating,
                SessionHandoffStateV1::Fencing,
                1_002,
            ),
            Err(SessionHandoffValidationError::InvalidTransition { .. })
        ));

        let mut forced = record(SessionHandoffModeV1::Forced);
        forced
            .transition(
                SessionHandoffStateV1::Requested,
                SessionHandoffStateV1::Validating,
                1_001,
            )
            .unwrap();
        forced
            .transition(
                SessionHandoffStateV1::Validating,
                SessionHandoffStateV1::Fencing,
                1_002,
            )
            .unwrap();
        forced
            .transition(
                SessionHandoffStateV1::Fencing,
                SessionHandoffStateV1::Fenced,
                1_003,
            )
            .unwrap();
        forced
            .transition(
                SessionHandoffStateV1::Fenced,
                SessionHandoffStateV1::Hydrating,
                1_004,
            )
            .unwrap();
        assert_eq!(
            forced.validate(),
            Err(SessionHandoffValidationError::ForcedEffectsNotSealed)
        );
        forced.risk.effects_sealed_at_unix_ms = Some(1_004);
        forced.validate().unwrap();
    }

    #[test]
    fn graceful_handoff_requires_every_durability_boundary() {
        let mut handoff = record(SessionHandoffModeV1::Graceful);
        for (next, now) in [
            (SessionHandoffStateV1::Validating, 1_001),
            (SessionHandoffStateV1::Draining, 1_002),
            (SessionHandoffStateV1::Checkpointed, 1_003),
            (SessionHandoffStateV1::Fencing, 1_004),
            (SessionHandoffStateV1::Fenced, 1_005),
            (SessionHandoffStateV1::Hydrating, 1_006),
            (SessionHandoffStateV1::Active, 1_007),
        ] {
            let prior = handoff.state;
            handoff.transition(prior, next, now).unwrap();
        }
        assert!(handoff.state.is_terminal());
        assert_eq!(handoff.transition_seq, 8);
    }

    #[test]
    fn blocked_retry_resumes_the_exact_interrupted_operation() {
        let mut fencing = record(SessionHandoffModeV1::Forced);
        fencing
            .transition(
                SessionHandoffStateV1::Requested,
                SessionHandoffStateV1::Validating,
                1_001,
            )
            .unwrap();
        fencing
            .transition(
                SessionHandoffStateV1::Validating,
                SessionHandoffStateV1::Fencing,
                1_002,
            )
            .unwrap();
        fencing
            .transition(
                SessionHandoffStateV1::Fencing,
                SessionHandoffStateV1::Blocked,
                1_003,
            )
            .unwrap();
        assert_eq!(fencing.blocked_from, Some(SessionHandoffStateV1::Fencing));
        assert!(matches!(
            fencing.transition(
                SessionHandoffStateV1::Blocked,
                SessionHandoffStateV1::Validating,
                1_004,
            ),
            Err(SessionHandoffValidationError::InvalidTransition { .. })
        ));
        fencing
            .transition(
                SessionHandoffStateV1::Blocked,
                SessionHandoffStateV1::Fencing,
                1_004,
            )
            .unwrap();
        assert_eq!(fencing.blocked_from, None);
    }

    #[test]
    fn manifest_delta_is_a_verified_suffix_not_an_untrusted_node_list() {
        let key = SessionKeyV1::owner_session("test", "owner", "session", "main");
        let segment = ConversationSegmentV1::new(
            &key,
            vec![
                json!({"role": "user", "content": "question"}),
                json!({"role": "assistant", "content": "answer"}),
            ],
        )
        .unwrap();
        let node = ContextManifestNodeV1::new(
            key.clone(),
            None,
            1,
            1,
            1,
            0,
            None,
            vec![segment.reference()],
        )
        .unwrap();
        let head = SessionContextHeadV1 {
            schema_version: SESSION_COORDINATION_SCHEMA_VERSION,
            key: key.clone(),
            cursor: node.cursor(),
            latest_manifest_root: node.manifest_root.clone(),
            total_canonical_bytes: segment.canonical_bytes,
            total_message_count: u64::from(segment.message_count),
            writer_epoch: 1,
        };
        let valid = ManifestDeltaV1 {
            schema_version: MANIFEST_DELTA_SCHEMA_VERSION,
            key,
            after_manifest_root: None,
            head: Some(head),
            shared_prefix: None,
            missing_nodes: vec![node],
            missing_canonical_bytes: segment.canonical_bytes,
            missing_message_count: u64::from(segment.message_count),
        };
        valid.validate().unwrap();

        let mut wrong_total = valid.clone();
        wrong_total.missing_message_count += 1;
        assert_eq!(
            wrong_total.validate(),
            Err(SessionHandoffValidationError::InvalidManifestDelta)
        );

        let mut missing_link = valid;
        missing_link.after_manifest_root = Some("0".repeat(64));
        assert_eq!(
            missing_link.validate(),
            Err(SessionHandoffValidationError::InvalidManifestDelta)
        );
    }
}
