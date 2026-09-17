//! User-visible, content-addressed recovery point contracts.
//!
//! A recovery point is a small set of references to already canonical Work,
//! Session, Run, workspace, and Artifact facts.  It is deliberately not a
//! second lifecycle store and it does not grant permission to replay an
//! unfinished effect.  Consumers must call [`RecoveryPointManifestV1::validate`]
//! before treating a decoded value as a restorable boundary.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    CONVERSATION_PROJECTION_SCHEMA_VERSION, SEGMENTED_CONVERSATION_PROJECTION_SCHEMA_VERSION,
    SESSION_COORDINATION_SCHEMA_VERSION, SESSION_CURSOR_SCHEMA_VERSION, SessionContextHeadV1,
    SessionCursorV1, SessionKeyV1,
};

pub const RECOVERY_POINT_MANIFEST_SCHEMA_VERSION: u32 = 1;
const RECOVERY_POINT_HASH_DOMAIN: &[u8] = b"astra.recovery-point-manifest.v1\0";
const MAX_ID_BYTES: usize = 512;
const MAX_DIGEST_BYTES: usize = 71;
const MAX_CAPABILITIES: usize = 128;
const MAX_ARTIFACTS: usize = 256;

/// Why a recovery point was captured.  The reason is explanatory metadata;
/// it does not change the authority needed to restore the point.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryPointReasonV1 {
    UserRequested,
    BeforeEnvironmentChange,
    RunSettled,
    SafeBoundary,
}

/// The last known state of the Run referenced by a recovery point.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryPointRunStateV1 {
    Settled,
    Paused,
    NeedsAttention,
}

/// A Run frontier is evidence for inspection and a future, explicit resume;
/// it is never sufficient proof that an uncertain external operation is safe
/// to replay.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecoveryPointRunFrontierV1 {
    pub run_id: String,
    pub run_generation: u64,
    pub state: RecoveryPointRunStateV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event_idx: Option<i64>,
    /// Opaque, content-addressed invocation/effect frontier.  The referenced
    /// ledger owns the detailed outcomes and idempotency rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect_frontier: Option<String>,
    pub has_unresolved_effects: bool,
}

/// The execution binding at capture time.  Materialization identity is
/// separate from the logical workspace so a new Edge cannot impersonate the
/// old local directory.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryPointExecutorKindV1 {
    Server,
    Edge,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryPointBindingStateV1 {
    Ready,
    Switching,
    NeedsAttention,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecoveryPointExecutionBindingV1 {
    pub binding_generation: u64,
    pub binding_state: RecoveryPointBindingStateV1,
    pub logical_workspace_id: String,
    pub executor_kind: RecoveryPointExecutorKindV1,
    pub executor_id: String,
    /// Digest of the complete canonical Session execution binding. The
    /// recovery projection is intentionally smaller than that binding, so a
    /// publisher must retain this digest and compare it with the canonical
    /// row instead of treating the projection as an authority.
    pub canonical_binding_hash: String,
    /// Hash of the canonical Session/Work execution binding represented by
    /// the fields above.  It proves identity of the captured binding; a
    /// publication verifier must still compare it with the current binding
    /// generation before granting write capability.
    pub binding_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub physical_workspace_id: Option<String>,
}

/// Reference to an independently captured workspace manifest and its content
/// package. A published recovery point must reference a complete snapshot;
/// this stage intentionally leaves the field absent until a snapshot store
/// exists.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecoveryPointWorkspaceReferenceV1 {
    pub snapshot_id: String,
    pub manifest_hash: String,
    pub content_root: String,
    pub byte_size: u64,
    pub complete: bool,
}

/// Typed reference to a durable Artifact.  `location_ref` is an opaque
/// server-owned locator; it is never a local filesystem path or a public URL.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecoveryPointArtifactReferenceV1 {
    pub artifact_id: String,
    pub artifact_type: String,
    pub digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location_ref: Option<String>,
}

/// Environment facts needed to explain what a target must provide.  The
/// target must re-check these requirements at migration time.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecoveryPointEnvironmentRequirementsV1 {
    #[serde(default)]
    pub required_capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_fingerprint: Option<String>,
    #[serde(default)]
    pub required_external_services: Vec<String>,
}

/// One immutable logical recovery boundary shared by Server, TUI, Web, and
/// Edge.  The fields are references to canonical facts; this type owns no
/// execution lease and no filesystem access.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecoveryPointManifestV1 {
    pub schema_version: u32,
    pub recovery_point_id: String,
    pub owner_id: String,
    pub work_id: String,
    pub branch_id: String,
    pub work_revision: u64,
    pub branch_revision: u64,
    pub graph_revision: u64,
    pub goal_revision: u64,
    pub criteria_set_revision: u64,
    pub session_key: SessionKeyV1,
    pub session_cursor: SessionCursorV1,
    /// Context-head state used to build the next prompt.  The cursor alone
    /// identifies a conversation boundary, while this head also preserves
    /// the manifest root, compaction accounting, and writer epoch selected
    /// at that boundary.
    pub context_head: SessionContextHeadV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RecoveryPointRunFrontierV1>,
    pub execution: RecoveryPointExecutionBindingV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<RecoveryPointWorkspaceReferenceV1>,
    #[serde(default)]
    pub artifacts: Vec<RecoveryPointArtifactReferenceV1>,
    pub environment: RecoveryPointEnvironmentRequirementsV1,
    pub reason: RecoveryPointReasonV1,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecoveryPointCapabilityAssessmentV1 {
    pub can_restore_conversation: bool,
    pub can_continue_in_original_environment: bool,
    pub has_portable_workspace: bool,
    pub requires_target_environment_check: bool,
    pub requires_effect_review: bool,
}

/// A fact-level status for one recovery-point dimension. These values are
/// deliberately descriptive: `published` never implies that every dimension
/// is portable or that an automatic restore action exists.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryPointCoverageStatusV1 {
    Verified,
    NotApplicable,
    NotCaptured,
    Changed,
    RequiresReview,
    Unavailable,
    Corrupt,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecoveryPointCoverageV1 {
    pub work: RecoveryPointCoverageStatusV1,
    pub conversation: RecoveryPointCoverageStatusV1,
    pub run: RecoveryPointCoverageStatusV1,
    pub workspace: RecoveryPointCoverageStatusV1,
    pub artifacts: RecoveryPointCoverageStatusV1,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecoveryPointBlockerV1 {
    pub code: String,
    pub user_message: String,
    pub recovery_action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<String>,
}

/// Server-derived explanation of what a published point actually proves.
/// It is a read model, not input to publication and not a restore grant.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecoveryPointAssessmentV1 {
    pub schema_version: u16,
    pub conversation_state_verified: bool,
    pub restore_action_available: bool,
    pub coverage: RecoveryPointCoverageV1,
    pub blockers: Vec<RecoveryPointBlockerV1>,
}

impl RecoveryPointAssessmentV1 {
    /// Assessment exposed when a persisted point can be identified but its
    /// manifest or hash no longer passes the contract.  The record remains
    /// visible so operators can remove or repair the exact point, while no
    /// untrusted manifest data is returned and no recovery action is implied.
    pub fn corrupt(reason: &'static str) -> Self {
        Self {
            schema_version: 1,
            conversation_state_verified: false,
            restore_action_available: false,
            coverage: RecoveryPointCoverageV1 {
                work: RecoveryPointCoverageStatusV1::Corrupt,
                conversation: RecoveryPointCoverageStatusV1::Corrupt,
                run: RecoveryPointCoverageStatusV1::Corrupt,
                workspace: RecoveryPointCoverageStatusV1::Corrupt,
                artifacts: RecoveryPointCoverageStatusV1::Corrupt,
            },
            blockers: vec![RecoveryPointBlockerV1 {
                code: "recovery_point_corrupt".into(),
                user_message: "This recovery point is damaged and cannot be used for continuation."
                    .into(),
                recovery_action:
                    "Remove the damaged point and create a new recovery point at a safe boundary."
                        .into(),
                diagnostics: Some(reason.into()),
            }],
        }
    }

    /// Assessment used while a capture is still only a caller declaration.
    /// Keeping every dimension non-positive prevents a shaped JSON manifest
    /// from becoming an implied restore grant.
    pub fn unverified() -> Self {
        Self {
            schema_version: 1,
            conversation_state_verified: false,
            restore_action_available: false,
            coverage: RecoveryPointCoverageV1 {
                work: RecoveryPointCoverageStatusV1::Unavailable,
                conversation: RecoveryPointCoverageStatusV1::Unavailable,
                run: RecoveryPointCoverageStatusV1::NotApplicable,
                workspace: RecoveryPointCoverageStatusV1::NotCaptured,
                artifacts: RecoveryPointCoverageStatusV1::Unavailable,
            },
            blockers: vec![RecoveryPointBlockerV1 {
                code: "capture_not_verified".into(),
                user_message: "This saved request has not been verified by the server yet.".into(),
                recovery_action: "Retry the save at a safe boundary.".into(),
                diagnostics: None,
            }],
        }
    }

    /// Assessment for the first publisher stage. Work and conversation are
    /// proven by the canonical publisher; workspace portability remains
    /// intentionally unavailable until a snapshot/content store exists.
    pub fn published_without_workspace(manifest: &RecoveryPointManifestV1) -> Self {
        let mut blockers = Vec::new();
        let workspace_status = if manifest.workspace.is_some() {
            blockers.push(RecoveryPointBlockerV1 {
                code: "workspace_not_verified".into(),
                user_message: "A workspace reference is present, but its content has not been verified by the publisher.".into(),
                recovery_action: "Verify the workspace snapshot before using it on another environment.".into(),
                diagnostics: None,
            });
            RecoveryPointCoverageStatusV1::RequiresReview
        } else {
            blockers.push(RecoveryPointBlockerV1 {
                code: "workspace_not_captured".into(),
                user_message: "Code files are still on the original execution environment."
                    .into(),
                recovery_action: "Continue on the original environment or capture a workspace snapshot before moving.".into(),
                diagnostics: None,
            });
            RecoveryPointCoverageStatusV1::NotCaptured
        };
        let run_status = match manifest.run.as_ref() {
            None => {
                blockers.push(RecoveryPointBlockerV1 {
                    code: "run_not_captured".into(),
                    user_message: "No Run checkpoint was captured; this point describes the Session boundary only.".into(),
                    recovery_action: "Inspect the Run history before choosing a continuation action.".into(),
                    diagnostics: None,
                });
                RecoveryPointCoverageStatusV1::NotCaptured
            }
            Some(run) => {
                blockers.push(RecoveryPointBlockerV1 {
                    code: "run_not_verified".into(),
                    user_message: "The saved Run details are available for inspection, but have not been verified for resume.".into(),
                    recovery_action: "Inspect the Run checkpoint before choosing a continuation action.".into(),
                    diagnostics: None,
                });
                if run.has_unresolved_effects {
                    blockers.push(RecoveryPointBlockerV1 {
                        code: "effect_review_required".into(),
                        user_message:
                            "One or more external operation results still need confirmation."
                                .into(),
                        recovery_action:
                            "Review the operation result before continuing or replaying it."
                                .into(),
                        diagnostics: run.effect_frontier.clone(),
                    });
                }
                RecoveryPointCoverageStatusV1::RequiresReview
            }
        };
        let artifacts_status = if manifest.artifacts.is_empty() {
            blockers.push(RecoveryPointBlockerV1 {
                code: "artifacts_not_captured".into(),
                user_message: "No artifact references were captured in this recovery point.".into(),
                recovery_action:
                    "Open the canonical Work artifacts separately before continuing elsewhere."
                        .into(),
                diagnostics: None,
            });
            RecoveryPointCoverageStatusV1::NotCaptured
        } else {
            blockers.push(RecoveryPointBlockerV1 {
                code: "artifacts_not_verified".into(),
                user_message: "Referenced artifacts are listed, but retention and content have not been verified by the publisher.".into(),
                recovery_action: "Verify each artifact before relying on it during recovery.".into(),
                diagnostics: None,
            });
            RecoveryPointCoverageStatusV1::RequiresReview
        };
        Self {
            schema_version: 1,
            conversation_state_verified: true,
            restore_action_available: false,
            coverage: RecoveryPointCoverageV1 {
                work: RecoveryPointCoverageStatusV1::Verified,
                conversation: RecoveryPointCoverageStatusV1::Verified,
                run: run_status,
                workspace: workspace_status,
                artifacts: artifacts_status,
            },
            blockers,
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RecoveryPointValidationError {
    #[error("unsupported recovery point schema version {actual}")]
    UnsupportedSchema { actual: u32 },
    #[error("{field} must be a non-empty safe identifier")]
    InvalidIdentity { field: &'static str },
    #[error("{field} exceeds the {maximum} byte limit")]
    Oversized { field: &'static str, maximum: usize },
    #[error("{field} must be a positive revision")]
    InvalidRevision { field: &'static str },
    #[error("session key and cursor do not describe the same owner/session")]
    SessionIdentityMismatch,
    #[error("session context head does not match the captured cursor")]
    ContextHeadMismatch,
    #[error("workspace reference is incomplete")]
    IncompleteWorkspace,
    #[error("{field} must be a sha256:<64 lowercase hex> digest")]
    InvalidDigest { field: &'static str },
    #[error("recovery point contains duplicate artifact identity {artifact_id}")]
    DuplicateArtifact { artifact_id: String },
    #[error("execution binding hash does not match its canonical fields")]
    InvalidBindingHash,
    #[error("canonical execution binding hash is invalid")]
    InvalidCanonicalBindingHash,
    #[error("too many artifacts (maximum {maximum})")]
    TooManyArtifacts { maximum: usize },
    #[error("too many environment requirements (maximum {maximum})")]
    TooManyRequirements { maximum: usize },
    #[error("recovery point serialization failed: {0}")]
    Serialization(String),
}

impl RecoveryPointManifestV1 {
    /// Validate the wire contract and all identity relationships.
    pub fn validate(&self) -> Result<(), RecoveryPointValidationError> {
        if self.schema_version != RECOVERY_POINT_MANIFEST_SCHEMA_VERSION {
            return Err(RecoveryPointValidationError::UnsupportedSchema {
                actual: self.schema_version,
            });
        }
        for (field, value) in [
            ("recovery_point_id", self.recovery_point_id.as_str()),
            ("owner_id", self.owner_id.as_str()),
            ("work_id", self.work_id.as_str()),
            ("branch_id", self.branch_id.as_str()),
            ("executor_id", self.execution.executor_id.as_str()),
            (
                "execution.logical_workspace_id",
                self.execution.logical_workspace_id.as_str(),
            ),
            ("created_at", self.created_at.as_str()),
        ] {
            validate_identity(field, value)?;
        }
        for (field, value) in [
            ("work_revision", self.work_revision),
            ("branch_revision", self.branch_revision),
            ("graph_revision", self.graph_revision),
            ("goal_revision", self.goal_revision),
            ("criteria_set_revision", self.criteria_set_revision),
        ] {
            if value == 0 {
                return Err(RecoveryPointValidationError::InvalidRevision { field });
            }
        }
        self.session_key
            .validate()
            .map_err(|_| RecoveryPointValidationError::SessionIdentityMismatch)?;
        if self.session_key.owner_user_id != self.owner_id
            || !self.session_key.validates_cursor(&self.session_cursor)
            || self.context_head.key != self.session_key
            || self.context_head.cursor != self.session_cursor
        {
            return Err(RecoveryPointValidationError::SessionIdentityMismatch);
        }
        if self.context_head.schema_version != SESSION_COORDINATION_SCHEMA_VERSION {
            return Err(RecoveryPointValidationError::InvalidIdentity {
                field: "context_head.schema_version",
            });
        }
        if self.context_head.latest_manifest_root != self.session_cursor.canonical_root_hash {
            return Err(RecoveryPointValidationError::ContextHeadMismatch);
        }
        if self.context_head.total_message_count == 0 {
            return Err(RecoveryPointValidationError::InvalidIdentity {
                field: "context_head.total_message_count",
            });
        }
        if !valid_conversation_root(&self.context_head.latest_manifest_root) {
            return Err(RecoveryPointValidationError::InvalidDigest {
                field: "context_head.latest_manifest_root",
            });
        }
        if self.session_cursor.schema_version != SESSION_CURSOR_SCHEMA_VERSION {
            return Err(RecoveryPointValidationError::InvalidIdentity {
                field: "session_cursor.schema_version",
            });
        }
        if self.session_cursor.projection_schema != CONVERSATION_PROJECTION_SCHEMA_VERSION
            && self.session_cursor.projection_schema
                != SEGMENTED_CONVERSATION_PROJECTION_SCHEMA_VERSION
        {
            return Err(RecoveryPointValidationError::InvalidIdentity {
                field: "session_cursor.projection_schema",
            });
        }
        if !valid_conversation_root(&self.session_cursor.canonical_root_hash) {
            return Err(RecoveryPointValidationError::InvalidDigest {
                field: "session_cursor.canonical_root_hash",
            });
        }
        if self.execution.binding_generation == 0 {
            return Err(RecoveryPointValidationError::InvalidRevision {
                field: "execution.binding_generation",
            });
        }
        validate_digest("execution.binding_hash", &self.execution.binding_hash)?;
        validate_digest(
            "execution.canonical_binding_hash",
            &self.execution.canonical_binding_hash,
        )?;
        if self.execution.binding_hash != self.execution.content_hash() {
            return Err(RecoveryPointValidationError::InvalidBindingHash);
        }
        if self.execution.binding_state == RecoveryPointBindingStateV1::Ready
            && matches!(
                self.execution.executor_kind,
                RecoveryPointExecutorKindV1::Edge
            )
            && self.execution.physical_workspace_id.is_none()
        {
            return Err(RecoveryPointValidationError::InvalidIdentity {
                field: "execution.physical_workspace_id",
            });
        }
        if self.execution.executor_kind == RecoveryPointExecutorKindV1::Server
            && self.execution.physical_workspace_id.is_some()
        {
            return Err(RecoveryPointValidationError::InvalidIdentity {
                field: "execution.physical_workspace_id",
            });
        }
        if let Some(physical_workspace_id) = &self.execution.physical_workspace_id {
            validate_identity("execution.physical_workspace_id", physical_workspace_id)?;
        }
        if let Some(run) = &self.run {
            validate_identity("run.run_id", &run.run_id)?;
            if run.run_generation == 0 {
                return Err(RecoveryPointValidationError::InvalidRevision {
                    field: "run.run_generation",
                });
            }
            if let Some(checkpoint_id) = &run.checkpoint_id {
                validate_identity("run.checkpoint_id", checkpoint_id)?;
            }
            if let Some(effect_frontier) = &run.effect_frontier {
                validate_identity("run.effect_frontier", effect_frontier)?;
            }
            if run.has_unresolved_effects && run.effect_frontier.is_none() {
                return Err(RecoveryPointValidationError::InvalidIdentity {
                    field: "run.effect_frontier",
                });
            }
        }
        if let Some(workspace) = &self.workspace {
            validate_identity("workspace.snapshot_id", &workspace.snapshot_id)?;
            validate_digest("workspace.manifest_hash", &workspace.manifest_hash)?;
            validate_digest("workspace.content_root", &workspace.content_root)?;
            if !workspace.complete {
                return Err(RecoveryPointValidationError::IncompleteWorkspace);
            }
        }
        if self.artifacts.len() > MAX_ARTIFACTS {
            return Err(RecoveryPointValidationError::TooManyArtifacts {
                maximum: MAX_ARTIFACTS,
            });
        }
        let mut artifact_ids = std::collections::BTreeSet::new();
        for artifact in &self.artifacts {
            validate_identity("artifact.artifact_id", &artifact.artifact_id)?;
            validate_identity("artifact.artifact_type", &artifact.artifact_type)?;
            validate_digest("artifact.digest", &artifact.digest)?;
            if let Some(location_ref) = &artifact.location_ref {
                validate_identity("artifact.location_ref", location_ref)?;
            }
            if !artifact_ids.insert(&artifact.artifact_id) {
                return Err(RecoveryPointValidationError::DuplicateArtifact {
                    artifact_id: artifact.artifact_id.clone(),
                });
            }
        }
        for (field, values) in [
            (
                "environment.required_capabilities",
                &self.environment.required_capabilities,
            ),
            (
                "environment.required_external_services",
                &self.environment.required_external_services,
            ),
        ] {
            if values.len() > MAX_CAPABILITIES {
                return Err(RecoveryPointValidationError::TooManyRequirements {
                    maximum: MAX_CAPABILITIES,
                });
            }
            for value in values {
                validate_identity(field, value)?;
            }
        }
        if let Some(platform) = &self.environment.platform {
            validate_identity("environment.platform", platform)?;
        }
        if let Some(fingerprint) = &self.environment.runtime_fingerprint {
            validate_identity("environment.runtime_fingerprint", fingerprint)?;
        }
        Ok(())
    }

    /// Return the canonical bytes used for identity and idempotency.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, RecoveryPointValidationError> {
        self.validate()?;
        serde_json::to_vec(self)
            .map_err(|error| RecoveryPointValidationError::Serialization(error.to_string()))
    }

    /// Content hash of this manifest.  The hash is over the validated manifest
    /// itself and is therefore stable for the fixed schema field order.
    pub fn content_hash(&self) -> Result<String, RecoveryPointValidationError> {
        let bytes = self.canonical_bytes()?;
        let mut digest = Sha256::new();
        digest.update(RECOVERY_POINT_HASH_DOMAIN);
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
        Ok(format!("sha256:{:x}", digest.finalize()))
    }

    /// Derive the conservative assessment that is safe before canonical
    /// verification.  A raw manifest is caller input: its cursor may be
    /// well-shaped while the referenced Session, workspace blobs, binding, or
    /// effect ledger no longer exists.  Therefore no positive restore or
    /// continuation capability is exposed here.
    pub fn capability_assessment(&self) -> RecoveryPointCapabilityAssessmentV1 {
        RecoveryPointCapabilityAssessmentV1 {
            can_restore_conversation: false,
            can_continue_in_original_environment: false,
            has_portable_workspace: false,
            requires_target_environment_check: self.environment.platform.is_some()
                || self.environment.runtime_fingerprint.is_some()
                || !self.environment.required_capabilities.is_empty()
                || !self.environment.required_external_services.is_empty(),
            requires_effect_review: self
                .run
                .as_ref()
                .is_some_and(|run| run.has_unresolved_effects),
        }
    }
}

impl RecoveryPointExecutionBindingV1 {
    /// Hash the canonical binding identity without including `binding_hash`
    /// itself.  Keeping this helper on the shared contract makes it possible
    /// for an Edge and the Server verifier to produce the same identity.
    pub fn content_hash(&self) -> String {
        #[derive(Serialize)]
        struct BindingIdentity<'a> {
            binding_generation: u64,
            binding_state: RecoveryPointBindingStateV1,
            logical_workspace_id: &'a str,
            executor_kind: RecoveryPointExecutorKindV1,
            executor_id: &'a str,
            canonical_binding_hash: &'a str,
            physical_workspace_id: Option<&'a str>,
        }

        let identity = BindingIdentity {
            binding_generation: self.binding_generation,
            binding_state: self.binding_state,
            logical_workspace_id: &self.logical_workspace_id,
            executor_kind: self.executor_kind,
            executor_id: &self.executor_id,
            canonical_binding_hash: &self.canonical_binding_hash,
            physical_workspace_id: self.physical_workspace_id.as_deref(),
        };
        let bytes = serde_json::to_vec(&identity).expect("binding identity is serializable");
        let mut digest = Sha256::new();
        digest.update(b"astra.execution-binding.v1\0");
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
        format!("sha256:{:x}", digest.finalize())
    }
}

fn validate_identity(field: &'static str, value: &str) -> Result<(), RecoveryPointValidationError> {
    if value.is_empty()
        || value.len() > MAX_ID_BYTES
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        if value.len() > MAX_ID_BYTES {
            return Err(RecoveryPointValidationError::Oversized {
                field,
                maximum: MAX_ID_BYTES,
            });
        }
        return Err(RecoveryPointValidationError::InvalidIdentity { field });
    }
    Ok(())
}

fn validate_digest(field: &'static str, value: &str) -> Result<(), RecoveryPointValidationError> {
    if value.len() != MAX_DIGEST_BYTES
        || !value.starts_with("sha256:")
        || !value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(RecoveryPointValidationError::InvalidDigest { field });
    }
    Ok(())
}

fn valid_conversation_root(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn digest(byte: char) -> String {
        format!("sha256:{}", byte.to_string().repeat(64))
    }

    fn sample_manifest() -> RecoveryPointManifestV1 {
        let key = SessionKeyV1::owner_session("tenant-a", "user-a", "session-a", "main");
        RecoveryPointManifestV1 {
            schema_version: RECOVERY_POINT_MANIFEST_SCHEMA_VERSION,
            recovery_point_id: "rp-a".into(),
            owner_id: "user-a".into(),
            work_id: "work-a".into(),
            branch_id: "branch-a".into(),
            work_revision: 1,
            branch_revision: 1,
            graph_revision: 1,
            goal_revision: 1,
            criteria_set_revision: 1,
            session_cursor: SessionCursorV1 {
                schema_version: 1,
                owner_id: "user-a".into(),
                session_id: "session-a".into(),
                branch_id: "main".into(),
                completed_turn: 2,
                journal_event_seq: 3,
                conversation_seq: 2,
                canonical_root_hash: "a".repeat(64),
                projection_schema: 1,
                compaction_generation: 0,
                config_version_id: None,
            },
            session_key: key.clone(),
            context_head: SessionContextHeadV1 {
                schema_version: 1,
                key,
                cursor: SessionCursorV1 {
                    schema_version: 1,
                    owner_id: "user-a".into(),
                    session_id: "session-a".into(),
                    branch_id: "main".into(),
                    completed_turn: 2,
                    journal_event_seq: 3,
                    conversation_seq: 2,
                    canonical_root_hash: "a".repeat(64),
                    projection_schema: 1,
                    compaction_generation: 0,
                    config_version_id: None,
                },
                latest_manifest_root: "a".repeat(64),
                total_canonical_bytes: 128,
                total_message_count: 4,
                writer_epoch: 1,
            },
            run: Some(RecoveryPointRunFrontierV1 {
                run_id: "run-a".into(),
                run_generation: 1,
                state: RecoveryPointRunStateV1::Settled,
                checkpoint_id: Some("checkpoint-a".into()),
                last_event_idx: Some(4),
                effect_frontier: None,
                has_unresolved_effects: false,
            }),
            execution: RecoveryPointExecutionBindingV1 {
                binding_generation: 1,
                binding_state: RecoveryPointBindingStateV1::Ready,
                logical_workspace_id: "workspace-a".into(),
                executor_kind: RecoveryPointExecutorKindV1::Edge,
                executor_id: "edge-a".into(),
                binding_hash: digest('e'),
                canonical_binding_hash: digest('f'),
                physical_workspace_id: Some("materialization-a".into()),
            },
            workspace: Some(RecoveryPointWorkspaceReferenceV1 {
                snapshot_id: "snapshot-a".into(),
                manifest_hash: digest('b'),
                content_root: digest('c'),
                byte_size: 12,
                complete: true,
            }),
            artifacts: vec![RecoveryPointArtifactReferenceV1 {
                artifact_id: "artifact-a".into(),
                artifact_type: "markdown".into(),
                digest: digest('d'),
                location_ref: Some("location-a".into()),
            }],
            environment: RecoveryPointEnvironmentRequirementsV1 {
                required_capabilities: vec!["git".into()],
                platform: Some("linux/amd64".into()),
                runtime_fingerprint: None,
                required_external_services: vec![],
            },
            reason: RecoveryPointReasonV1::UserRequested,
            created_at: "2026-09-16T00:00:00Z".into(),
        }
    }

    #[test]
    fn validates_identity_links_and_derives_capabilities() {
        let mut manifest = sample_manifest();
        manifest.execution.binding_hash = manifest.execution.content_hash();
        manifest.validate().unwrap();
        let capabilities = manifest.capability_assessment();
        assert!(!capabilities.can_restore_conversation);
        assert!(!capabilities.can_continue_in_original_environment);
        assert!(!capabilities.has_portable_workspace);
        assert!(capabilities.requires_target_environment_check);
        assert!(!capabilities.requires_effect_review);
        assert!(manifest.content_hash().unwrap().starts_with("sha256:"));
    }

    #[test]
    fn published_assessment_explains_missing_workspace_and_effect_review() {
        let mut manifest = sample_manifest();
        manifest.execution.binding_hash = manifest.execution.content_hash();
        manifest.workspace = None;
        manifest.run.as_mut().unwrap().has_unresolved_effects = true;
        manifest.run.as_mut().unwrap().effect_frontier = Some("frontier-1".into());
        let assessment = RecoveryPointAssessmentV1::published_without_workspace(&manifest);
        assert!(assessment.conversation_state_verified);
        assert!(!assessment.restore_action_available);
        assert_eq!(
            assessment.coverage.workspace,
            RecoveryPointCoverageStatusV1::NotCaptured
        );
        assert_eq!(
            assessment.coverage.run,
            RecoveryPointCoverageStatusV1::RequiresReview
        );
        assert!(
            assessment
                .blockers
                .iter()
                .any(|blocker| blocker.code == "effect_review_required")
        );
    }

    #[test]
    fn corrupt_assessment_is_explicit_and_contains_no_recovery_capability() {
        let assessment = RecoveryPointAssessmentV1::corrupt("manifest_hash_mismatch");
        assert!(!assessment.conversation_state_verified);
        assert!(!assessment.restore_action_available);
        assert_eq!(
            assessment.coverage.conversation,
            RecoveryPointCoverageStatusV1::Corrupt
        );
        assert_eq!(assessment.blockers[0].code, "recovery_point_corrupt");
        assert_eq!(
            assessment.blockers[0].diagnostics.as_deref(),
            Some("manifest_hash_mismatch")
        );
    }

    #[test]
    fn rejects_tampered_owner_and_incomplete_workspace() {
        let mut manifest = sample_manifest();
        manifest.execution.binding_hash = manifest.execution.content_hash();
        manifest.session_cursor.owner_id = "other-user".into();
        assert_eq!(
            manifest.validate(),
            Err(RecoveryPointValidationError::SessionIdentityMismatch)
        );

        let mut manifest = sample_manifest();
        manifest.execution.binding_hash = manifest.execution.content_hash();
        manifest.workspace.as_mut().unwrap().complete = false;
        assert_eq!(
            manifest.validate(),
            Err(RecoveryPointValidationError::IncompleteWorkspace)
        );
    }

    #[test]
    fn rejects_context_head_that_does_not_describe_the_cursor() {
        let mut manifest = sample_manifest();
        manifest.execution.binding_hash = manifest.execution.content_hash();
        manifest.context_head.latest_manifest_root = "b".repeat(64);
        assert_eq!(
            manifest.validate(),
            Err(RecoveryPointValidationError::ContextHeadMismatch)
        );

        let mut manifest = sample_manifest();
        manifest.execution.binding_hash = manifest.execution.content_hash();
        manifest.context_head.total_message_count = 0;
        assert_eq!(
            manifest.validate(),
            Err(RecoveryPointValidationError::InvalidIdentity {
                field: "context_head.total_message_count"
            })
        );
    }

    #[test]
    fn rejects_unsupported_executor_workspace_combinations() {
        let mut manifest = sample_manifest();
        manifest.execution.binding_hash = manifest.execution.content_hash();
        manifest.execution.executor_kind = RecoveryPointExecutorKindV1::Edge;
        manifest.execution.physical_workspace_id = None;
        manifest.execution.binding_hash = manifest.execution.content_hash();
        assert_eq!(
            manifest.validate(),
            Err(RecoveryPointValidationError::InvalidIdentity {
                field: "execution.physical_workspace_id"
            })
        );

        let mut manifest = sample_manifest();
        manifest.execution.binding_hash = manifest.execution.content_hash();
        manifest.execution.executor_kind = RecoveryPointExecutorKindV1::Server;
        manifest.execution.physical_workspace_id = Some("server-directory".into());
        manifest.execution.binding_hash = manifest.execution.content_hash();
        assert_eq!(
            manifest.validate(),
            Err(RecoveryPointValidationError::InvalidIdentity {
                field: "execution.physical_workspace_id"
            })
        );
    }

    #[test]
    fn rejects_unknown_wire_fields() {
        let mut value = serde_json::to_value(sample_manifest()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("future_field".into(), json!(true));
        assert!(serde_json::from_value::<RecoveryPointManifestV1>(value).is_err());
    }

    #[test]
    fn unresolved_effect_requires_a_frontier() {
        let mut manifest = sample_manifest();
        manifest.execution.binding_hash = manifest.execution.content_hash();
        let run = manifest.run.as_mut().unwrap();
        run.has_unresolved_effects = true;
        assert!(matches!(
            manifest.validate(),
            Err(RecoveryPointValidationError::InvalidIdentity {
                field: "run.effect_frontier"
            })
        ));
    }
}
