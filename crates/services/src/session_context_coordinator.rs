//! Canonical database-backed session coordination.
//!
//! The mutable record is intentionally small: one branch head, one writer
//! lease, and one turn reservation. Conversation payloads and manifest nodes
//! are immutable and durable before a head can reference them.

use std::{collections::HashSet, time::Duration};

use crate::cancellation_safe_db::CancellationSafePoolConnection;
use astra_core::{SharedPool, matrixone_statement_with_null_shape};
use astra_turn_types::{
    ActorContextV1, AuthorityEpochsV1, CANONICAL_TURN_DELTA_SCHEMA_VERSION, CanonicalDeltaModeV1,
    CanonicalTurnDeltaV1, ContextManifestNodeV1, ConversationSegmentV1, ConversationWriterLeaseV1,
    CoordinatorConflictOptionV1, CoordinatorMutationV1, HandoffRiskEvidenceV1,
    MANIFEST_DELTA_SCHEMA_VERSION, ManifestDeltaV1, SESSION_COORDINATION_SCHEMA_VERSION,
    SessionAttachmentModeV1, SessionAttachmentV1, SessionContextHeadV1,
    SessionCoordinationValidationError, SessionCursorV1, SessionForkManifestV1, SessionForkStateV1,
    SessionHandoffModeV1, SessionKeyV1, SharedManifestPrefixV1, TurnReservationV1,
    canonical_conversation_root, canonical_conversation_serialized_len,
};
use async_trait::async_trait;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{MySql, QueryBuilder, Row, Transaction};
use thiserror::Error;
use uuid::Uuid;

const MAX_IDEMPOTENCY_KEY_BYTES: usize = 512;

/// Evidence that a Session still has execution work which must settle before
/// cancellation or provider handoff can be considered complete. Session
/// history is never a blocker, and another Session using the same physical
/// workspace is not a blocker either.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionExecutionBlocker {
    ActiveRun,
    WriterOrReservation,
    BindingNotReady,
    UnresolvedTool,
    OwnerUnavailable,
}

impl SessionExecutionBlocker {
    pub fn explanation(self) -> &'static str {
        match self {
            Self::ActiveRun => "a run or child task has not stopped",
            Self::WriterOrReservation => {
                "a conversation write or turn admission is still in progress"
            }
            Self::BindingNotReady => {
                "the execution binding needs attention or is switching providers"
            }
            Self::UnresolvedTool => {
                "a tool invocation has no confirmed outcome; external effects may still be running"
            }
            Self::OwnerUnavailable => "the Session state cannot be verified",
        }
    }

    pub fn recovery_action(self) -> &'static str {
        match self {
            Self::ActiveRun => "wait_or_cancel_session",
            Self::WriterOrReservation => "retry_session",
            Self::BindingNotReady | Self::UnresolvedTool | Self::OwnerUnavailable => {
                "inspect_session"
            }
        }
    }

    pub fn user_message(self, owner: &str) -> String {
        let action = match self.recovery_action() {
            "wait_or_cancel_session" => format!(
                "Wait for it to finish, or stop it with `astra session cancel {owner}` and retry"
            ),
            "retry_session" => "Retry after the current admission settles".to_string(),
            _ => format!(
                "Inspect it with `astra session show {owner}` and resolve the outstanding execution state before retrying"
            ),
        };
        format!(
            "Session {owner} still has execution state to settle: {}. {action}. Session history is preserved and another session may use the same workspace.",
            self.explanation()
        )
    }
}
const COORDINATOR_STATE_SCHEMA_VERSION: u32 = 1;
const MAX_SEGMENT_BATCH: usize = 256;
const MAX_CONTEXT_SEGMENT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_CONTEXT_CONTENT_BYTES: u64 = 32 * 1024 * 1024;
const MAX_LEASE_TTL: Duration = Duration::from_secs(15 * 60);
const MAX_RESERVATION_TTL: Duration = Duration::from_secs(15 * 60);
// Execution-binding generations are strictly positive in durable rows. Zero
// is reserved for the internal admission expectation that no native binding
// may exist; it is never persisted into a binding row or run snapshot.
const NO_EXECUTION_BINDING_EXPECTATION: u64 = 0;
const RECEIPT_HASH_DOMAIN: &[u8] = b"astra.session-coordinator-receipt.v1\0";
const TURN_DELTA_HASH_DOMAIN: &[u8] = b"astra.canonical-turn-delta.v1\0";

#[derive(Debug, Error)]
pub enum SessionContextCoordinatorError {
    #[error("session actor is not authorized for this owner")]
    Unauthorized,
    #[error("invalid coordinator request: {0}")]
    Invalid(String),
    #[error("canonical Context resource limit: {0}")]
    ResourceLimit(&'static str),
    #[error("writer authority was fenced by a newer epoch")]
    Fenced,
    #[error("session is being deleted or is no longer writable")]
    SessionLifecycleFenced,
    #[error("session execution is occupied: {0:?}")]
    ExecutionOccupied(SessionExecutionBlocker),
    #[error("writer lease or turn reservation expired")]
    Expired,
    #[error("idempotency key was reused for a different request")]
    IdempotencyMismatch,
    #[error("coordinator state requires repair: {0}")]
    NeedsRepair(String),
    #[error("observed manifest is not an ancestor of the current branch head")]
    DivergentManifest,
    #[error("one or more requested conversation segments do not exist for this owner")]
    SegmentNotFound,
    #[error("coordinator clock is outside the supported range")]
    Clock,
    #[error(
        "session execution binding is fenced: expected generation {expected}, current generation {current:?}"
    )]
    ExecutionBindingFenced { expected: u64, current: Option<u64> },
    #[error("session already has a native execution binding at generation {generation}")]
    ExecutionBindingPresent { generation: u64 },
    #[error("session execution binding is busy with an active Run or unresolved invocation")]
    ExecutionBindingBusy,
    #[error("session execution binding is not ready: {0:?}")]
    ExecutionBindingNotReady(SessionExecutionBindingStateV1),
    #[error("coordinator database operation {operation} failed: {source}")]
    Database {
        operation: &'static str,
        #[source]
        source: sqlx::Error,
    },
    #[error("coordinator database JSON for {entity} failed: {source}")]
    DatabaseJson {
        entity: &'static str,
        #[source]
        source: serde_json::Error,
    },
}

pub const SESSION_EXECUTION_BINDING_SCHEMA_VERSION: u16 = 1;
pub const SESSION_EXECUTION_SWITCH_SCHEMA_VERSION: u16 = 1;

/// Durable state for a provider switch. The receipt is owned by the Session
/// coordinator so a retry can recover the same authority transition after a
/// request timeout or process restart without replaying any workspace write.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionExecutionSwitchStateV1 {
    Switching,
    Succeeded,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BeginSessionExecutionSwitchV1 {
    pub request_id: String,
    pub operation_id: String,
    pub controller_attachment_id: String,
    pub expected_generation: u64,
    pub target: SessionExecutionBindingV1,
    /// Read-only source proof captured before the durable binding fence. The
    /// receipt keeps this value immutable so a retry cannot bless a different
    /// checkout revision after the original request has failed.
    pub source_evidence: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SessionExecutionSwitchReceiptV1 {
    pub schema_version: u16,
    pub operation_id: String,
    pub request_id: String,
    pub controller_attachment_id: String,
    pub request_hash: String,
    pub key: SessionKeyV1,
    pub expected_generation: u64,
    /// Generation immediately before the currently active attempt. Unlike
    /// `expected_generation` (the caller's original CAS expectation), this
    /// advances after each retry and fences delayed completions.
    pub attempt_expected_generation: u64,
    pub switching_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_generation: Option<u64>,
    pub state: SessionExecutionSwitchStateV1,
    pub source: SessionExecutionBindingV1,
    pub target: SessionExecutionBindingV1,
    pub source_evidence: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_code: Option<String>,
    pub attempt: u32,
}

/// The durable provider selection for one canonical Session branch. The
/// generation is independent from an Edge connection generation and is
/// checked again when a Run reserves the Session and when a tool crosses the
/// provider boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionExecutionBindingV1 {
    pub schema_version: u16,
    pub generation: u64,
    pub state: SessionExecutionBindingStateV1,
    pub logical_workspace_id: String,
    /// Stable provider-side identity for the physical materialization. Native
    /// Edge bindings receive a bounded digest of the authenticated canonical
    /// checkout root; connection labels and registry row ids are never a
    /// substitute for this identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub physical_workspace_id: Option<String>,
    pub workspace: crate::runs::WorkspaceBindingRequest,
    pub executor: crate::runs::ExecutorBindingRequest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionExecutionBindingStateV1 {
    Ready,
    Switching,
    NeedsAttention,
}

impl SessionExecutionBindingV1 {
    /// Derive the bounded physical identity used by executor-local workspace
    /// coordination. The persisted materialization identity separates
    /// independent devices that happen to use the same path, while the
    /// canonical root keeps reconnects tied to the same physical materialization.
    pub fn edge_materialization_physical_identity(
        materialization_id: &str,
        canonical_root: &str,
    ) -> String {
        let mut identity = Vec::with_capacity(materialization_id.len() + canonical_root.len() + 1);
        identity.extend_from_slice(materialization_id.as_bytes());
        identity.push(0);
        identity.extend_from_slice(canonical_root.as_bytes());
        format!("edge-materialization-v1:{:x}", Sha256::digest(identity))
    }

    pub fn server_work_default(logical_workspace_id: impl Into<String>) -> Self {
        Self {
            schema_version: SESSION_EXECUTION_BINDING_SCHEMA_VERSION,
            generation: 1,
            state: SessionExecutionBindingStateV1::Ready,
            logical_workspace_id: logical_workspace_id.into(),
            physical_workspace_id: None,
            workspace: Self::server_work_workspace_request(),
            executor: Self::server_work_executor_request(),
        }
    }

    pub fn server_work_workspace_request() -> crate::runs::WorkspaceBindingRequest {
        crate::runs::WorkspaceBindingRequest {
            kind: crate::runs::WorkspaceBindingRequestKind::ServerSandbox,
            display_name: Some("Work workspace".to_string()),
            root: None,
            source: None,
            authority: Some(crate::runs::WorkspaceAuthorityRequest::ReadWrite),
        }
    }

    pub fn server_work_executor_request() -> crate::runs::ExecutorBindingRequest {
        crate::runs::ExecutorBindingRequest {
            kind: crate::runs::ExecutorBindingRequestKind::ServerLocal,
            executor_id: None,
            display_name: None,
            transport: None,
            status: None,
        }
    }

    pub fn validate(&self) -> Result<(), SessionContextCoordinatorError> {
        if self.schema_version != SESSION_EXECUTION_BINDING_SCHEMA_VERSION {
            return Err(SessionContextCoordinatorError::Invalid(
                "unsupported Session execution-binding schema version".into(),
            ));
        }
        if self.generation == 0 {
            return Err(SessionContextCoordinatorError::Invalid(
                "Session execution-binding generation must be positive".into(),
            ));
        }
        if self.generation > i64::MAX as u64 {
            return Err(SessionContextCoordinatorError::Invalid(
                "Session execution-binding generation exceeds the durable integer range".into(),
            ));
        }
        if self.logical_workspace_id.trim().is_empty() || self.logical_workspace_id.len() > 256 {
            return Err(SessionContextCoordinatorError::Invalid(
                "logical workspace identity must be non-empty and at most 256 bytes".into(),
            ));
        }
        if self
            .physical_workspace_id
            .as_deref()
            .is_some_and(|identity| identity.trim().is_empty() || identity.len() > 512)
        {
            return Err(SessionContextCoordinatorError::Invalid(
                "physical workspace identity must be non-empty and at most 512 bytes".into(),
            ));
        }
        use crate::runs::{ExecutorBindingRequestKind, WorkspaceBindingRequestKind};
        let valid_server_binding = matches!(
            (self.workspace.kind, self.executor.kind),
            (
                WorkspaceBindingRequestKind::ServerSandbox,
                ExecutorBindingRequestKind::ServerLocal
            )
        ) && self.workspace.root.is_none()
            && self.workspace.source.is_none()
            && self.workspace.authority == Some(crate::runs::WorkspaceAuthorityRequest::ReadWrite)
            && self.executor.executor_id.is_none()
            && self.executor.transport.is_none()
            && self.executor.status.is_none();
        let edge_workspace_root = self.workspace.root.as_deref().map(str::trim);
        let edge_source_matches_root = matches!(
            self.workspace.source.as_ref(),
            Some(crate::runs::WorkspaceSourceRequest::EdgePath { path })
                if edge_workspace_root == Some(path.trim())
        );
        let edge_transport_supported = matches!(
            self.executor.transport,
            Some(crate::runs::ToolTransportKindRequest::EdgeWs)
                | Some(crate::runs::ToolTransportKindRequest::EdgeWsAuthorized)
                | Some(crate::runs::ToolTransportKindRequest::EdgeLedger)
        );
        let edge_physical_identity_present =
            self.physical_workspace_id
                .as_deref()
                .is_some_and(|identity| {
                    let trimmed = identity.trim();
                    !trimmed.is_empty() && trimmed.len() <= 512 && !trimmed.contains('\0')
                });
        let valid_edge_binding = matches!(
            (self.workspace.kind, self.executor.kind),
            (
                WorkspaceBindingRequestKind::EdgeWorkspace,
                ExecutorBindingRequestKind::EdgeAgent
            )
        ) && self.workspace.root.as_deref().is_some_and(|value| {
            let trimmed = value.trim();
            !trimmed.is_empty() && trimmed.len() <= 4096 && !trimmed.contains('\0')
        }) && edge_source_matches_root
            && self.executor.executor_id.as_deref().is_some_and(|value| {
                let trimmed = value.trim();
                !trimmed.is_empty() && trimmed.len() <= 255 && !trimmed.contains('\0')
            })
            && edge_transport_supported
            && edge_physical_identity_present
            && self.workspace.authority != Some(crate::runs::WorkspaceAuthorityRequest::None);
        if !valid_server_binding && !valid_edge_binding {
            return Err(SessionContextCoordinatorError::Invalid(
                "Session execution binding does not identify a complete supported workspace/executor pair".into(),
            ));
        }
        if valid_server_binding && self.physical_workspace_id.is_some() {
            return Err(SessionContextCoordinatorError::Invalid(
                "Server execution bindings cannot carry a physical workspace identity".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcquireWriterOutcome {
    Acquired(ConversationWriterLeaseV1),
    AlreadyAcquired(ConversationWriterLeaseV1),
    Conflict {
        current_head: Option<SessionContextHeadV1>,
        active_lease_expires_at_unix_ms: Option<i64>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReserveTurnOutcome {
    Reserved(TurnReservationV1),
    AlreadyReserved(TurnReservationV1),
    Conflict {
        current_head: Option<SessionContextHeadV1>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcquireWriterAndReserveTurnOutcome {
    Ready {
        lease: ConversationWriterLeaseV1,
        reservation: TurnReservationV1,
    },
    WriterConflict {
        current_head: Option<SessionContextHeadV1>,
        active_lease_expires_at_unix_ms: Option<i64>,
    },
    ReservationConflict {
        lease: ConversationWriterLeaseV1,
        current_head: Option<SessionContextHeadV1>,
    },
}

/// One atomically renewed writer/reservation pair.
///
/// A canonical turn is writable only while both authorities are live. Renewing
/// them in separate transactions creates a state in which the writer has been
/// extended but its turn reservation has not, so callers must not compose the
/// two narrower renewal operations when they own an active turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenewedTurnAuthority {
    pub writer_lease: ConversationWriterLeaseV1,
    pub turn_reservation: TurnReservationV1,
}

/// Exact parked execution; the caller supplies custody, not a new turn grant.
pub struct ResumedExecutionTurnRequest<'a> {
    pub user_id: &'a str,
    pub session_id: &'a str,
    pub run_id: &'a str,
    pub expected_owner_generation: u64,
    pub checkpoint_id: &'a str,
    pub source: &'a RunContextAuthority,
    pub opening_hash: &'a str,
    pub owner_pod_id: &'a str,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ResumedExecutionTurnReceipt {
    pub source: RunContextAuthority,
    pub authority: RunContextAuthority,
    pub run_id: String,
    pub run_generation: u64,
    pub producer_generation: u64,
    pub checkpoint_id: String,
}

/// Constructed only after Run ownership, slot and checkpoint custody commit together.
/// A deserialized receipt alone cannot authorize restoration or execution.
#[derive(Debug)]
pub struct ResumedExecutionTurn {
    run: crate::runs::DurableRunRecord,
    checkpoint: crate::runs::DurableRunCheckpointRecord,
    receipt: ResumedExecutionTurnReceipt,
}

impl ResumedExecutionTurn {
    pub fn run(&self) -> &crate::runs::DurableRunRecord {
        &self.run
    }

    pub fn checkpoint(&self) -> &crate::runs::DurableRunCheckpointRecord {
        &self.checkpoint
    }

    pub fn receipt(&self) -> &ResumedExecutionTurnReceipt {
        &self.receipt
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum WriterTransferConflictV1 {
    CursorChanged,
    SourceWriterChanged,
    ActiveTurn,
}

#[derive(Debug, Clone)]
pub struct WriterTransferRequestV1 {
    pub handoff_id: String,
    pub idempotency_key: String,
    pub key: SessionKeyV1,
    pub mode: SessionHandoffModeV1,
    pub source_lease: Option<ConversationWriterLeaseV1>,
    /// Optional command-level CAS fence for writer-only authority changes.
    pub expected_writer_epoch: Option<u64>,
    pub expected_cursor: Option<SessionCursorV1>,
    pub target_actor: ActorContextV1,
    pub risk: HandoffRiskEvidenceV1,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum TransferWriterOutcome {
    Transferred(ConversationWriterLeaseV1),
    AlreadyTransferred(ConversationWriterLeaseV1),
    Conflict {
        reason: WriterTransferConflictV1,
        current_head: Option<SessionContextHeadV1>,
        active_lease_expires_at_unix_ms: Option<i64>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct MaterializedConversationV1 {
    pub head: SessionContextHeadV1,
    pub messages: Vec<Value>,
    pub logical_segment_count: u64,
    pub canonical_segment_bytes: u64,
}

/// Read-only facts used to prepare a canonical turn. Mutating admission still
/// revalidates the writer, cursor, epochs, and reservation while holding the
/// database row lock; this snapshot only removes duplicate preflight reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionAdmissionSnapshotV1 {
    pub head: Option<SessionContextHeadV1>,
    pub active_writer: Option<ConversationWriterLeaseV1>,
    pub authority_epochs: AuthorityEpochsV1,
}

#[async_trait]
pub trait SessionContextCoordinator: Send + Sync {
    async fn resume_execution_turn(
        &self,
        request: ResumedExecutionTurnRequest<'_>,
    ) -> Result<ResumedExecutionTurn, SessionContextCoordinatorError>;

    async fn load_head(
        &self,
        key: &SessionKeyV1,
    ) -> Result<Option<SessionContextHeadV1>, SessionContextCoordinatorError>;

    async fn load_admission_snapshot(
        &self,
        key: &SessionKeyV1,
    ) -> Result<SessionAdmissionSnapshotV1, SessionContextCoordinatorError>;

    async fn materialize(
        &self,
        head: &SessionContextHeadV1,
    ) -> Result<MaterializedConversationV1, SessionContextCoordinatorError>;

    /// Load only the manifest nodes after a verified ancestor. Segment
    /// payloads are fetched separately, so warm attach/handoff is
    /// O(changed manifests) rather than O(history bytes).
    async fn load_manifest_delta(
        &self,
        key: &SessionKeyV1,
        after_manifest_root: Option<&str>,
    ) -> Result<ManifestDeltaV1, SessionContextCoordinatorError>;

    /// Read a complete node's payloads through its authenticated manifest
    /// locator. A retained fork prefix may supply an inherited node.
    async fn load_manifest_content(
        &self,
        key: &SessionKeyV1,
        manifest_root: &str,
    ) -> Result<Vec<ConversationSegmentV1>, SessionContextCoordinatorError>;

    async fn load_authority_epochs(
        &self,
        key: &SessionKeyV1,
    ) -> Result<Option<AuthorityEpochsV1>, SessionContextCoordinatorError>;

    /// Return the currently valid controller lease without mutating it.
    async fn load_active_writer(
        &self,
        key: &SessionKeyV1,
    ) -> Result<Option<ConversationWriterLeaseV1>, SessionContextCoordinatorError>;

    async fn load_fork_prefix(
        &self,
        key: &SessionKeyV1,
    ) -> Result<Option<SharedManifestPrefixV1>, SessionContextCoordinatorError>;

    /// Atomically activate a prepared copy-on-write child head and its durable
    /// fork record. No writer/run/tool authority is inherited.
    async fn activate_fork(
        &self,
        manifest: &SessionForkManifestV1,
    ) -> Result<SessionContextHeadV1, SessionContextCoordinatorError>;

    async fn acquire_writer(
        &self,
        key: &SessionKeyV1,
        expected_cursor: Option<&SessionCursorV1>,
        actor: &ActorContextV1,
        ttl: Duration,
        idempotency_key: &str,
    ) -> Result<AcquireWriterOutcome, SessionContextCoordinatorError>;

    async fn release_writer(
        &self,
        lease: &ConversationWriterLeaseV1,
    ) -> Result<(), SessionContextCoordinatorError>;

    /// Atomically fence the previous controller and install the target
    /// controller. Graceful transfer requires the live source lease and a
    /// drained turn slot. Forced transfer requires a server-verified
    /// authorization identity and preserves explicit unresolved-risk facts.
    async fn transfer_writer(
        &self,
        request: &WriterTransferRequestV1,
        ttl: Duration,
    ) -> Result<TransferWriterOutcome, SessionContextCoordinatorError>;

    async fn reserve_turn(
        &self,
        lease: &ConversationWriterLeaseV1,
        expected_cursor: Option<&SessionCursorV1>,
        ttl: Duration,
        idempotency_key: &str,
        expected_execution_binding_generation: Option<u64>,
    ) -> Result<ReserveTurnOutcome, SessionContextCoordinatorError>;

    /// Acquire the branch writer and reserve its next turn as one atomic
    /// admission.
    #[allow(clippy::too_many_arguments)]
    async fn acquire_writer_and_reserve_turn(
        &self,
        key: &SessionKeyV1,
        expected_cursor: Option<&SessionCursorV1>,
        actor: &ActorContextV1,
        ttl: Duration,
        writer_idempotency_key: &str,
        reservation_idempotency_key: &str,
        expected_execution_binding_generation: Option<u64>,
    ) -> Result<AcquireWriterAndReserveTurnOutcome, SessionContextCoordinatorError>;

    /// Atomically renew the complete authority required to commit one turn.
    async fn renew_turn_authority(
        &self,
        lease: &ConversationWriterLeaseV1,
        reservation: &TurnReservationV1,
        ttl: Duration,
    ) -> Result<RenewedTurnAuthority, SessionContextCoordinatorError>;

    async fn commit_turn(
        &self,
        reservation: &TurnReservationV1,
        delta: CanonicalTurnDeltaV1,
        idempotency_key: &str,
    ) -> Result<CoordinatorMutationV1, SessionContextCoordinatorError>;

    /// Commit the turn and release only the writer that owns its reservation
    /// in the same transaction. Exact replay must not release a newer writer.
    async fn commit_turn_and_release_writer(
        &self,
        reservation: &TurnReservationV1,
        delta: CanonicalTurnDeltaV1,
        idempotency_key: &str,
    ) -> Result<CoordinatorMutationV1, SessionContextCoordinatorError>;

    async fn advance_authority_epochs(
        &self,
        key: &SessionKeyV1,
        epochs: AuthorityEpochsV1,
    ) -> Result<(), SessionContextCoordinatorError>;
}

#[derive(Clone)]
pub struct DatabaseSessionContextCoordinator {
    pool: SharedPool,
}

/// Frozen opening facts, not an execution grant. Context mutation additionally
/// requires the store's exact Run/generation terminal admission in the same Tx.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunContextAuthority {
    pub key: SessionKeyV1,
    pub run_id: String,
    pub generation: u64,
    pub row_exists: bool,
    pub base_head: Option<SessionContextHeadV1>,
    pub turn: u32,
    pub writer_epoch: u64,
    pub authority_epochs: AuthorityEpochsV1,
    pub binding: RunContextBinding,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunContextBinding {
    Absent,
    Present { generation: u64 },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "owner",
    content = "facts",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ContextCommitAuthority {
    Run(Box<RunContextAuthority>),
    Controller(Box<TurnReservationV1>),
}

impl RunContextAuthority {
    fn validate(&self) -> Result<(), SessionContextCoordinatorError> {
        let invalid = |detail: &str| SessionContextCoordinatorError::Invalid(detail.into());
        self.key
            .validate()
            .map_err(|error| invalid(&error.to_string()))?;
        if self.key.isolation_domain != "server"
            || self.key.branch_id != astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID
            || self.run_id.is_empty()
            || self.generation > i64::MAX as u64
            || self.base_head.is_some() && !self.row_exists
        {
            return Err(invalid("invalid Server Run context identity"));
        }
        if let Some(head) = &self.base_head {
            validate_head(head)?;
            if head.key != self.key || head.writer_epoch > self.writer_epoch {
                return Err(invalid("Run context base belongs to another authority"));
            }
        } else if !self.row_exists
            && (self.writer_epoch != 0 || self.authority_epochs != AuthorityEpochsV1::default())
        {
            return Err(invalid("absent context row cannot carry authority epochs"));
        }
        let expected = self
            .base_head
            .as_ref()
            .map_or(Some(1), |head| head.cursor.completed_turn.checked_add(1));
        if expected != Some(self.turn) {
            return Err(invalid(
                "Run context turn does not advance its admitted base",
            ));
        }
        for epoch in [
            self.writer_epoch,
            self.authority_epochs.authorization_epoch,
            self.authority_epochs.device_trust_epoch,
            self.authority_epochs.permission_epoch,
        ] {
            i64_from_u64("Run context epoch", epoch)?;
        }
        if let RunContextBinding::Present { generation } = self.binding {
            if generation == 0 {
                return Err(invalid("binding generation must be positive"));
            }
            i64_from_u64("Run context binding generation", generation)?;
        }
        Ok(())
    }
    /// Stable admitted base identity; current generation is separately fenced
    /// by the store-created Run mutation proof on every write.
    pub fn admission_fingerprint(&self) -> String {
        let mut facts = serde_json::to_value(self).expect("serializable Run Context facts");
        facts
            .as_object_mut()
            .expect("Run Context facts are an object")
            .remove("generation");
        let mut digest = Sha256::new();
        digest.update(b"astra.run-context-admission.v1\0");
        digest.update(astra_core::canonical_json_string(&facts).as_bytes());
        format!("{:x}", digest.finalize())
    }
}

impl ContextCommitAuthority {
    pub fn key(&self) -> &SessionKeyV1 {
        match self {
            Self::Run(authority) => &authority.key,
            Self::Controller(reservation) => &reservation.key,
        }
    }
    pub fn base_cursor(&self) -> Option<&SessionCursorV1> {
        match self {
            Self::Run(authority) => authority.base_head.as_ref().map(|head| &head.cursor),
            Self::Controller(reservation) => reservation.expected_cursor.as_ref(),
        }
    }

    pub fn turn(&self) -> u32 {
        match self {
            Self::Run(authority) => authority.turn,
            Self::Controller(reservation) => reservation.reserved_turn,
        }
    }
}

/// Immutable, validated input to one canonical Context commit. Preparing it
/// performs no I/O and grants no authority. Its cursor and request identity can
/// be bound to the Run terminal receipt before either is written.
#[derive(Debug)]
pub struct PreparedSessionContextCommit {
    authority: ContextCommitAuthority,
    opening_hash: Option<String>,
    delta: CanonicalTurnDeltaV1,
    idempotency_key: String,
    release_writer: bool,
    request_hash: String,
    segments: Vec<ConversationSegmentV1>,
    node: ContextManifestNodeV1,
    content_json: String,
}

impl PreparedSessionContextCommit {
    pub fn new(
        reservation: &TurnReservationV1,
        delta: CanonicalTurnDeltaV1,
        idempotency_key: &str,
        release_writer: bool,
    ) -> Result<Self, SessionContextCoordinatorError> {
        Self::prepare(
            ContextCommitAuthority::Controller(Box::new(reservation.clone())),
            delta,
            idempotency_key,
            release_writer,
            None,
        )
    }

    pub fn for_run(
        authority: &RunContextAuthority,
        opening_hash: &str,
        delta: CanonicalTurnDeltaV1,
    ) -> Result<Self, SessionContextCoordinatorError> {
        authority.validate()?;
        if opening_hash.len() != 64 || !opening_hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(SessionContextCoordinatorError::Invalid(
                "Run opening hash is invalid".into(),
            ));
        }
        validate_delta_advance(authority.base_head.as_ref(), authority.turn, &delta)?;
        let key = format!("run:{}:{}:terminal", authority.run_id, authority.generation);
        Self::prepare(
            ContextCommitAuthority::Run(Box::new(authority.clone())),
            delta,
            &key,
            false,
            Some(opening_hash),
        )
    }

    fn prepare(
        authority: ContextCommitAuthority,
        delta: CanonicalTurnDeltaV1,
        idempotency_key: &str,
        release_writer: bool,
        opening_hash: Option<&str>,
    ) -> Result<Self, SessionContextCoordinatorError> {
        delta
            .validate()
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        validate_idempotency_key(idempotency_key)?;
        let content_json = encode_manifest_content(&delta.logical_segments)?;
        let key = authority.key();
        let segments = delta
            .logical_segments
            .iter()
            .cloned()
            .map(|messages| ConversationSegmentV1::new(key, messages))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        let node = manifest_node_for_delta(
            key,
            authority
                .base_cursor()
                .map(|cursor| cursor.canonical_root_hash.clone()),
            &delta,
            &segments,
        )
        .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        let request_hash = match &authority {
            ContextCommitAuthority::Controller(reservation) => {
                commit_request_hash(reservation, &delta)
            }
            ContextCommitAuthority::Run(_) => {
                let value = serde_json::to_value((&authority, &delta, opening_hash))
                    .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
                format!(
                    "{:x}",
                    Sha256::digest(astra_core::canonical_json_string(&value).as_bytes())
                )
            }
        };
        Ok(Self {
            authority,
            opening_hash: opening_hash.map(str::to_owned),
            delta,
            idempotency_key: idempotency_key.to_owned(),
            release_writer,
            request_hash,
            segments,
            node,
            content_json,
        })
    }

    pub fn cursor(&self) -> SessionCursorV1 {
        self.node.cursor()
    }

    pub fn run_context_authority(&self) -> Option<&RunContextAuthority> {
        match &self.authority {
            ContextCommitAuthority::Run(authority) => Some(authority),
            _ => None,
        }
    }

    pub fn opening_hash(&self) -> Option<&str> {
        self.opening_hash.as_deref()
    }

    pub fn request_hash(&self) -> &str {
        &self.request_hash
    }

    pub fn terminal_binding(&self) -> Value {
        serde_json::json!({
            "opening_hash": self.opening_hash,
            "request_hash": self.request_hash,
            "idempotency_key": self.idempotency_key,
            "release_writer": self.release_writer,
            "cursor": self.cursor(),
        })
    }
}

pub struct WriterTurnReservationRequest<'a> {
    pub key: &'a SessionKeyV1,
    pub expected_cursor: Option<&'a SessionCursorV1>,
    pub actor: &'a ActorContextV1,
    pub ttl: Duration,
    pub writer_idempotency_key: &'a str,
    pub reservation_idempotency_key: &'a str,
    pub expected_execution_binding_generation: Option<u64>,
}

// A standalone rejected authority operation commits its rejection audit. In a
// terminal transaction the same rejection must roll back the entire settlement.
enum StagedCoordinatorMutation<T> {
    Mutation(T),
    Rejected(SessionContextCoordinatorError),
}

impl<T> StagedCoordinatorMutation<T> {
    fn into_result(self) -> Result<T, SessionContextCoordinatorError> {
        match self {
            Self::Mutation(mutation) => Ok(mutation),
            Self::Rejected(error) => Err(error),
        }
    }
}

impl DatabaseSessionContextCoordinator {
    async fn commit_turn_inner(
        &self,
        reservation: &TurnReservationV1,
        delta: CanonicalTurnDeltaV1,
        idempotency_key: &str,
        release_writer: bool,
    ) -> Result<CoordinatorMutationV1, SessionContextCoordinatorError> {
        let prepared =
            PreparedSessionContextCommit::new(reservation, delta, idempotency_key, release_writer)?;
        self.commit_prepared_turn(&prepared).await
    }

    pub async fn commit_prepared_turn(
        &self,
        prepared: &PreparedSessionContextCommit,
    ) -> Result<CoordinatorMutationV1, SessionContextCoordinatorError> {
        let mut connection = CancellationSafePoolConnection::acquire(self.pool.get())
            .await
            .map_err(|source| database_error("acquire_commit_turn", source))?;
        let mut tx = connection
            .begin()
            .await
            .map_err(|source| database_error("begin_commit_turn", source))?;
        if matches!(&prepared.authority, ContextCommitAuthority::Run(_)) {
            return Err(SessionContextCoordinatorError::Invalid(
                "Run Context commit requires its terminal transaction proof".into(),
            ));
        }
        admit_controller_context_write(&mut tx, prepared.authority.key()).await?;
        let outcome = self.stage_turn_commit(&mut tx, prepared).await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit_turn", source))?;
        connection.release();
        outcome.into_result()
    }

    /// Stage canonical Context under the same session/execution/run locks as
    /// the terminal. The caller owns COMMIT; any rejection aborts that terminal.
    pub async fn commit_turn_in_terminal_transaction(
        &self,
        tx: &mut Transaction<'_, MySql>,
        prepared: &PreparedSessionContextCommit,
        admission: &crate::runs::RunMutationAdmission,
    ) -> Result<SessionCursorV1, SessionContextCoordinatorError> {
        if !admission.owns_session(
            &prepared.authority.key().owner_user_id,
            &prepared.authority.key().session_id,
        ) {
            return Err(SessionContextCoordinatorError::Invalid(
                "terminal admission does not own the canonical context".into(),
            ));
        }
        if let ContextCommitAuthority::Run(authority) = &prepared.authority {
            return self
                .stage_run_context_commit(tx, prepared, authority, admission)
                .await;
        }
        match self.stage_turn_commit(tx, prepared).await?.into_result()? {
            CoordinatorMutationV1::Applied { cursor }
            | CoordinatorMutationV1::AlreadyApplied { cursor } => Ok(cursor),
            CoordinatorMutationV1::Conflict { .. } => Err(SessionContextCoordinatorError::Invalid(
                "canonical session head changed before terminal commit".into(),
            )),
            CoordinatorMutationV1::NeedsRepair { reason, .. } => {
                Err(SessionContextCoordinatorError::NeedsRepair(reason))
            }
        }
    }

    /// A legitimate empty delta commits no cursor. Validate the exact admitted
    /// base through the same snapshot owner, under the terminal's Session fence.
    pub async fn validate_unchanged_run_context_in_terminal_transaction(
        &self,
        tx: &mut Transaction<'_, MySql>,
        authority: &RunContextAuthority,
        opening_hash: &str,
        admission: &crate::runs::RunMutationAdmission,
    ) -> Result<(), SessionContextCoordinatorError> {
        authority.validate()?;
        if !admission.authorizes_context_terminal(
            &authority.key.owner_user_id,
            &authority.key.session_id,
            &authority.run_id,
            authority.generation,
            &authority.admission_fingerprint(),
            opening_hash,
        ) {
            return Err(SessionContextCoordinatorError::Unauthorized);
        }
        let expected_binding = match authority.binding {
            RunContextBinding::Absent => 0,
            RunContextBinding::Present { generation } => generation,
        };
        let current = self
            .capture_run_context_snapshot_tx(
                tx,
                &authority.key,
                &authority.run_id,
                authority.generation,
                Some(expected_binding),
            )
            .await?;
        if current.admission_fingerprint() != authority.admission_fingerprint() {
            return Err(SessionContextCoordinatorError::Invalid(
                "Context base changed before empty terminal".into(),
            ));
        }
        Ok(())
    }

    /// Capture the Context base under the Run store's opening fence. This
    /// creates no Controller lease, reservation, audit receipt, or empty head.
    /// The store must include the result in the same immutable Run INSERT.
    pub async fn capture_run_opening_in_transaction(
        &self,
        tx: &mut Transaction<'_, MySql>,
        admission: &crate::runs::RunMutationAdmission,
        key: &SessionKeyV1,
        run_id: &str,
        generation: u64,
        expected_binding_generation: Option<u64>,
    ) -> Result<RunContextAuthority, SessionContextCoordinatorError> {
        if !admission.authorizes_context_opening(
            &key.owner_user_id,
            &key.session_id,
            run_id,
            generation,
        ) {
            return Err(SessionContextCoordinatorError::Unauthorized);
        }
        self.capture_run_context_snapshot_tx(
            tx,
            key,
            run_id,
            generation,
            expected_binding_generation,
        )
        .await
    }

    // Only callers that already hold the canonical Session/Run admission may
    // use this snapshot. It is data, not an execution or mutation grant.
    async fn capture_run_context_snapshot_tx(
        &self,
        tx: &mut Transaction<'_, MySql>,
        key: &SessionKeyV1,
        run_id: &str,
        generation: u64,
        expected_binding_generation: Option<u64>,
    ) -> Result<RunContextAuthority, SessionContextCoordinatorError> {
        // All competing writers take the lifecycle fence first. The caller
        // already owns it, including for absent rows.
        let row = sqlx::query(
            "SELECT h.owner_user_id AS head_owner, h.head_json, h.writer_epoch,
                    h.authorization_epoch, h.device_trust_epoch, h.permission_epoch,
                    h.active_writer_expires_at_ms, h.active_reservation_expires_at_ms,
                    CAST(h.active_writer_json IS NOT NULL OR h.active_reservation_json IS NOT NULL AS SIGNED) AS controller_authority_present,
                    b.generation, b.binding_json,
                    CAST(UNIX_TIMESTAMP(NOW(6)) * 1000 AS SIGNED) AS database_now_unix_ms
             FROM (SELECT 1 AS seed) seed
             LEFT JOIN session_context_heads h ON h.isolation_domain = ? AND h.owner_user_id = ?
                  AND h.session_id = ? AND h.branch_id = ?
             LEFT JOIN session_execution_bindings b ON b.isolation_domain = ? AND b.owner_user_id = ?
                  AND b.session_id = ? AND b.branch_id = ?",
        ).bind(&key.isolation_domain).bind(&key.owner_user_id).bind(&key.session_id).bind(&key.branch_id)
         .bind(&key.isolation_domain).bind(&key.owner_user_id).bind(&key.session_id).bind(&key.branch_id)
         .fetch_one(&mut **tx).await.map_err(|source| database_error("capture_Run_context", source))?;
        let optional = |column: &'static str| {
            row.try_get::<Option<i64>, _>(column)
                .map_err(|source| database_error("decode_Run_context", source))
        };
        let now: i64 = row
            .try_get("database_now_unix_ms")
            .map_err(|source| database_error("decode_Run_context_time", source))?;
        if optional("active_writer_expires_at_ms")?.is_some_and(|expiry| expiry > now)
            || optional("active_reservation_expires_at_ms")?.is_some_and(|expiry| expiry > now)
        {
            return Err(SessionContextCoordinatorError::ExecutionOccupied(
                SessionExecutionBlocker::WriterOrReservation,
            ));
        }
        if optional("controller_authority_present")? == Some(1) {
            // Only expired Controller residue reaches this cold branch. Reuse
            // its canonical cleanup owner before admitting a Run whose later
            // head CAS requires absent grants. Ordinary Run opening stays read-only.
            let (mut state, locked_now) = lock_database_state_at_now(tx, key).await?;
            if state
                .active_writer
                .as_ref()
                .is_some_and(|lease| lease.expires_at_unix_ms > locked_now)
                || state
                    .active_reservation
                    .as_ref()
                    .is_some_and(|reservation| reservation.expires_at_unix_ms > locked_now)
            {
                return Err(SessionContextCoordinatorError::ExecutionOccupied(
                    SessionExecutionBlocker::WriterOrReservation,
                ));
            }
            clear_writer_authority_in_tx(tx, &mut state).await?;
        }
        let row_exists = row
            .try_get::<Option<String>, _>("head_owner")
            .map_err(|source| database_error("decode_Run_context_owner", source))?
            .is_some();
        let base_head: Option<SessionContextHeadV1> = row
            .try_get::<Option<String>, _>("head_json")
            .map_err(|source| database_error("decode_Run_context_head", source))?
            .as_deref()
            .map(|json| database_json("Run_context_base", json))
            .transpose()?;
        let epoch = |column| -> Result<u64, SessionContextCoordinatorError> {
            match optional(column)? {
                Some(value) => u64::try_from(value).map_err(|_| {
                    SessionContextCoordinatorError::NeedsRepair("negative Context epoch".into())
                }),
                None if !row_exists => Ok(0),
                None => Err(SessionContextCoordinatorError::NeedsRepair(
                    "missing Context epoch".into(),
                )),
            }
        };
        let binding = match optional("generation")? {
            Some(_) => {
                let current = decode_execution_binding_row(&row)?;
                if expected_binding_generation == Some(0) {
                    return Err(SessionContextCoordinatorError::ExecutionBindingPresent {
                        generation: current.generation,
                    });
                }
                if expected_binding_generation
                    .is_some_and(|expected| expected != current.generation)
                {
                    return Err(SessionContextCoordinatorError::ExecutionBindingFenced {
                        expected: expected_binding_generation.unwrap(),
                        current: Some(current.generation),
                    });
                }
                if current.state != SessionExecutionBindingStateV1::Ready {
                    return Err(SessionContextCoordinatorError::ExecutionBindingNotReady(
                        current.state,
                    ));
                }
                RunContextBinding::Present {
                    generation: current.generation,
                }
            }
            None if expected_binding_generation.is_some_and(|expected| expected > 0) => {
                return Err(SessionContextCoordinatorError::ExecutionBindingFenced {
                    expected: expected_binding_generation.unwrap(),
                    current: None,
                });
            }
            None => RunContextBinding::Absent,
        };
        let authority = RunContextAuthority {
            key: key.clone(),
            run_id: run_id.to_owned(),
            generation,
            row_exists,
            turn: base_head
                .as_ref()
                .map_or(Some(1), |head| head.cursor.completed_turn.checked_add(1))
                .ok_or_else(|| {
                    SessionContextCoordinatorError::NeedsRepair("completed turn overflow".into())
                })?,
            base_head,
            writer_epoch: epoch("writer_epoch")?,
            authority_epochs: AuthorityEpochsV1 {
                authorization_epoch: epoch("authorization_epoch")?,
                device_trust_epoch: epoch("device_trust_epoch")?,
                permission_epoch: epoch("permission_epoch")?,
            },
            binding,
        };
        authority.validate()?;
        Ok(authority)
    }

    async fn stage_run_context_commit(
        &self,
        tx: &mut Transaction<'_, MySql>,
        prepared: &PreparedSessionContextCommit,
        authority: &RunContextAuthority,
        admission: &crate::runs::RunMutationAdmission,
    ) -> Result<SessionCursorV1, SessionContextCoordinatorError> {
        authority.validate()?;
        let key = &authority.key;
        if !admission.authorizes_context_terminal(
            &key.owner_user_id,
            &key.session_id,
            &authority.run_id,
            authority.generation,
            &authority.admission_fingerprint(),
            prepared.opening_hash().ok_or_else(|| {
                SessionContextCoordinatorError::Invalid(
                    "Run Context has no opening identity".into(),
                )
            })?,
        ) {
            return Err(SessionContextCoordinatorError::Invalid(
                "Context requires the exact Run terminal claim".into(),
            ));
        }
        validate_delta_advance(
            authority.base_head.as_ref(),
            authority.turn,
            &prepared.delta,
        )?;
        let (bytes, messages) = next_head_totals(
            authority.base_head.as_ref(),
            &prepared.segments,
            prepared.delta.mode,
        )?;
        self.persist_database_immutables_in_tx(
            tx,
            key,
            &prepared.node,
            &prepared.content_json,
            bytes,
            messages,
        )
        .await?;
        let head = SessionContextHeadV1 {
            schema_version: SESSION_COORDINATION_SCHEMA_VERSION,
            key: key.clone(),
            cursor: prepared.node.cursor(),
            latest_manifest_root: prepared.node.manifest_root.clone(),
            total_canonical_bytes: bytes,
            total_message_count: messages,
            writer_epoch: authority.writer_epoch,
        };
        let head_json = database_to_json("Run context head", &head)?;
        let mut write = if authority.row_exists {
            let mut write =
                QueryBuilder::<MySql>::new("UPDATE session_context_heads SET head_json = ");
            write
                .push_bind(&head_json)
                .push(", canonical_root_hash = ")
                .push_bind(&head.cursor.canonical_root_hash)
                .push(", latest_manifest_root = ")
                .push_bind(&head.latest_manifest_root)
                .push(", total_canonical_bytes = ")
                .push_bind(i64_from_u64("canonical bytes", bytes)?)
                .push(", total_message_count = ")
                .push_bind(i64_from_u64("message count", messages)?)
                .push(", completed_turn = ")
                .push_bind(i64::from(head.cursor.completed_turn))
                .push(", journal_event_seq = ")
                .push_bind(i64_from_u64(
                    "journal sequence",
                    head.cursor.journal_event_seq,
                )?)
                .push(", conversation_seq = ")
                .push_bind(i64_from_u64(
                    "conversation sequence",
                    head.cursor.conversation_seq,
                )?)
                .push(", projection_schema = ")
                .push_bind(i64::from(head.cursor.projection_schema))
                .push(", compaction_generation = ")
                .push_bind(i64_from_u64(
                    "compaction generation",
                    head.cursor.compaction_generation,
                )?)
                .push(", updated_at = NOW(6) WHERE isolation_domain = ")
                .push_bind(&key.isolation_domain)
                .push(" AND owner_user_id = ")
                .push_bind(&key.owner_user_id)
                .push(" AND session_id = ")
                .push_bind(&key.session_id)
                .push(" AND branch_id = ")
                .push_bind(&key.branch_id)
                .push(" AND writer_epoch = ")
                .push_bind(i64_from_u64("writer epoch", authority.writer_epoch)?)
                .push(" AND authorization_epoch = ")
                .push_bind(i64_from_u64(
                    "authorization epoch",
                    authority.authority_epochs.authorization_epoch,
                )?)
                .push(" AND device_trust_epoch = ")
                .push_bind(i64_from_u64(
                    "device epoch",
                    authority.authority_epochs.device_trust_epoch,
                )?)
                .push(" AND permission_epoch = ")
                .push_bind(i64_from_u64(
                    "permission epoch",
                    authority.authority_epochs.permission_epoch,
                )?)
                .push(" AND active_writer_json IS NULL AND active_reservation_json IS NULL");
            if let Some(base) = &authority.base_head {
                write
                    .push(" AND head_json IS NOT NULL AND canonical_root_hash = ")
                    .push_bind(&base.cursor.canonical_root_hash)
                    .push(" AND latest_manifest_root = ")
                    .push_bind(&base.latest_manifest_root)
                    .push(" AND completed_turn = ")
                    .push_bind(i64::from(base.cursor.completed_turn))
                    .push(" AND journal_event_seq = ")
                    .push_bind(i64_from_u64(
                        "base journal sequence",
                        base.cursor.journal_event_seq,
                    )?)
                    .push(" AND conversation_seq = ")
                    .push_bind(i64_from_u64(
                        "base conversation sequence",
                        base.cursor.conversation_seq,
                    )?)
                    .push(" AND compaction_generation = ")
                    .push_bind(i64_from_u64(
                        "base compaction generation",
                        base.cursor.compaction_generation,
                    )?)
                    .push(" AND projection_schema = ")
                    .push_bind(i64::from(base.cursor.projection_schema))
                    .push(" AND total_canonical_bytes = ")
                    .push_bind(i64_from_u64("base bytes", base.total_canonical_bytes)?)
                    .push(" AND total_message_count = ")
                    .push_bind(i64_from_u64("base messages", base.total_message_count)?);
            } else {
                write.push(
                    " AND head_json IS NULL AND canonical_root_hash IS NULL
                    AND completed_turn = 0 AND journal_event_seq = 0 AND conversation_seq = 0
                    AND total_canonical_bytes = 0 AND total_message_count = 0",
                );
            }
            write
        } else {
            let mut write = QueryBuilder::<MySql>::new("INSERT INTO session_context_heads
                (isolation_domain,owner_user_id,session_id,branch_id,head_json,canonical_root_hash,latest_manifest_root,
                 total_canonical_bytes,total_message_count,completed_turn,journal_event_seq,conversation_seq,
                 projection_schema,compaction_generation,writer_epoch,authorization_epoch,device_trust_epoch,permission_epoch)
                 SELECT ");
            let mut fields = write.separated(", ");
            fields
                .push_bind(&key.isolation_domain)
                .push_bind(&key.owner_user_id)
                .push_bind(&key.session_id)
                .push_bind(&key.branch_id)
                .push_bind(&head_json)
                .push_bind(&head.cursor.canonical_root_hash)
                .push_bind(&head.latest_manifest_root)
                .push_bind(i64_from_u64("canonical bytes", bytes)?)
                .push_bind(i64_from_u64("message count", messages)?)
                .push_bind(i64::from(head.cursor.completed_turn))
                .push_bind(i64_from_u64(
                    "journal sequence",
                    head.cursor.journal_event_seq,
                )?)
                .push_bind(i64_from_u64(
                    "conversation sequence",
                    head.cursor.conversation_seq,
                )?)
                .push_bind(i64::from(head.cursor.projection_schema))
                .push_bind(i64_from_u64(
                    "compaction generation",
                    head.cursor.compaction_generation,
                )?)
                .push_bind(i64_from_u64("writer epoch", authority.writer_epoch)?)
                .push_bind(i64_from_u64(
                    "authorization epoch",
                    authority.authority_epochs.authorization_epoch,
                )?)
                .push_bind(i64_from_u64(
                    "device epoch",
                    authority.authority_epochs.device_trust_epoch,
                )?)
                .push_bind(i64_from_u64(
                    "permission epoch",
                    authority.authority_epochs.permission_epoch,
                )?);
            write.push(" WHERE NOT EXISTS (SELECT 1 FROM session_context_heads existing WHERE isolation_domain = ")
                .push_bind(&key.isolation_domain).push(" AND owner_user_id = ").push_bind(&key.owner_user_id)
                .push(" AND session_id = ").push_bind(&key.session_id).push(" AND branch_id = ").push_bind(&key.branch_id).push(")");
            write
        };
        let present = matches!(authority.binding, RunContextBinding::Present { .. });
        write.push(if present {" AND EXISTS (SELECT 1 FROM session_execution_bindings binding WHERE isolation_domain = "}
            else {" AND NOT EXISTS (SELECT 1 FROM session_execution_bindings binding WHERE isolation_domain = "})
            .push_bind(&key.isolation_domain).push(" AND owner_user_id = ").push_bind(&key.owner_user_id)
            .push(" AND session_id = ").push_bind(&key.session_id).push(" AND branch_id = ").push_bind(&key.branch_id);
        if let RunContextBinding::Present { generation } = authority.binding {
            write
                .push(" AND generation = ")
                .push_bind(i64_from_u64("binding generation", generation)?)
                .push(" AND JSON_UNQUOTE(JSON_EXTRACT(binding_json, '$.state')) = 'ready'");
        }
        write.push(")");
        let affected = write
            .build()
            .execute(&mut **tx)
            .await
            .map_err(|source| database_error("advance_Run_context_head", source))?
            .rows_affected();
        if affected != 1 {
            return Err(SessionContextCoordinatorError::Fenced);
        }
        Ok(head.cursor)
    }

    async fn stage_turn_commit(
        &self,
        tx: &mut Transaction<'_, MySql>,
        prepared: &PreparedSessionContextCommit,
    ) -> Result<StagedCoordinatorMutation<CoordinatorMutationV1>, SessionContextCoordinatorError>
    {
        let ContextCommitAuthority::Controller(reservation) = &prepared.authority else {
            return Err(SessionContextCoordinatorError::Invalid(
                "controller commit requires its real reservation".into(),
            ));
        };
        let delta = &prepared.delta;
        let idempotency_key = prepared.idempotency_key.as_str();
        let request_hash = &prepared.request_hash;
        let segments = &prepared.segments;
        let release_writer = prepared.release_writer;
        let (mut state, now) = lock_database_state_at_now(tx, &reservation.key).await?;
        if let Some(last) = state.last_commit.clone()
            && last.idempotency_key == idempotency_key
        {
            validate_commit_request(&last, reservation, delta)?;
            record_database_authority_event(
                tx,
                &state,
                AuthorityAuditFact {
                    operation: "commit_turn",
                    outcome: "idempotent_replay",
                    actor: None,
                    lease_id: Some(&reservation.lease_id),
                    reservation_id: Some(&reservation.reservation_id),
                    expected_cursor: reservation.expected_cursor.as_ref(),
                },
            )
            .await?;
            if release_writer {
                release_committed_writer_in_tx(tx, &mut state, reservation).await?;
            }

            return Ok(StagedCoordinatorMutation::Mutation(
                CoordinatorMutationV1::AlreadyApplied {
                    cursor: last.cursor.clone(),
                },
            ));
        }
        if let Err(error) = validate_active_reservation(&state, reservation, now) {
            // Archived receipts are only relevant when this is not the current
            // active reservation. The successful hot path already proves that
            // the commit has not been applied, so avoid an extra database read.
            if let Some(receipt) = load_database_receipt::<CommitReceiptV1>(
                tx,
                &reservation.key,
                "commit",
                idempotency_key,
                request_hash,
            )
            .await?
            {
                record_database_authority_event(
                    tx,
                    &state,
                    AuthorityAuditFact {
                        operation: "commit_turn",
                        outcome: "idempotent_replay",
                        actor: None,
                        lease_id: Some(&reservation.lease_id),
                        reservation_id: Some(&reservation.reservation_id),
                        expected_cursor: reservation.expected_cursor.as_ref(),
                    },
                )
                .await?;
                if release_writer {
                    release_committed_writer_in_tx(tx, &mut state, reservation).await?;
                }

                return Ok(StagedCoordinatorMutation::Mutation(
                    CoordinatorMutationV1::AlreadyApplied {
                        cursor: receipt.cursor,
                    },
                ));
            }
            record_database_authority_event(
                tx,
                &state,
                AuthorityAuditFact {
                    operation: "commit_turn",
                    outcome: authority_error_outcome(&error),
                    actor: None,
                    lease_id: Some(&reservation.lease_id),
                    reservation_id: Some(&reservation.reservation_id),
                    expected_cursor: reservation.expected_cursor.as_ref(),
                },
            )
            .await?;

            return Ok(StagedCoordinatorMutation::Rejected(error));
        }
        if state.head.as_ref().map(|head| &head.cursor) != reservation.expected_cursor.as_ref() {
            let current_cursor = state.head.as_ref().map(|head| head.cursor.clone());
            record_database_authority_event(
                tx,
                &state,
                AuthorityAuditFact {
                    operation: "commit_turn",
                    outcome: "cursor_conflict",
                    actor: None,
                    lease_id: Some(&reservation.lease_id),
                    reservation_id: Some(&reservation.reservation_id),
                    expected_cursor: reservation.expected_cursor.as_ref(),
                },
            )
            .await?;

            return Ok(StagedCoordinatorMutation::Mutation(
                CoordinatorMutationV1::Conflict {
                    current_cursor,
                    safe_options: vec![
                        CoordinatorConflictOptionV1::Refresh,
                        CoordinatorConflictOptionV1::Fork,
                    ],
                },
            ));
        }
        validate_delta_advance(state.head.as_ref(), reservation.reserved_turn, delta)?;
        let node = &prepared.node;
        let (total_canonical_bytes, total_message_count) =
            next_head_totals(state.head.as_ref(), segments, delta.mode)?;
        self.persist_database_immutables_in_tx(
            tx,
            &reservation.key,
            node,
            &prepared.content_json,
            total_canonical_bytes,
            total_message_count,
        )
        .await?;
        if let Some(previous) = &state.last_commit {
            archive_database_commit(tx, previous).await?;
        }
        let cursor = node.cursor();
        state.head = Some(SessionContextHeadV1 {
            schema_version: SESSION_COORDINATION_SCHEMA_VERSION,
            key: reservation.key.clone(),
            cursor: cursor.clone(),
            latest_manifest_root: node.manifest_root.clone(),
            total_canonical_bytes,
            total_message_count,
            writer_epoch: reservation.writer_epoch,
        });
        state.last_commit = Some(CommitReceiptV1 {
            idempotency_key: idempotency_key.to_owned(),
            reservation_id: reservation.reservation_id.clone(),
            reservation: reservation.as_ref().clone(),
            delta_hash: turn_delta_hash(delta),
            cursor: cursor.clone(),
        });
        state.active_reservation = None;
        // Capture the commit audit before clearing its writer: the audit
        // owner and commit-before-release order must remain unchanged.
        record_database_authority_event(
            tx,
            &state,
            AuthorityAuditFact {
                operation: "commit_turn",
                outcome: "committed",
                actor: None,
                lease_id: Some(&reservation.lease_id),
                reservation_id: Some(&reservation.reservation_id),
                expected_cursor: reservation.expected_cursor.as_ref(),
            },
        )
        .await?;
        if !release_writer || !release_committed_writer_in_tx(tx, &mut state, reservation).await? {
            update_database_state(tx, &state).await?;
        }

        Ok(StagedCoordinatorMutation::Mutation(
            CoordinatorMutationV1::Applied { cursor },
        ))
    }

    pub fn new(pool: SharedPool) -> Self {
        Self { pool }
    }

    /// Fenced proof that this Session has no remaining execution debt. A
    /// caller must also retain any process-local executor evidence it owns.
    pub async fn execution_reuse_blocker(
        &self,
        key: &SessionKeyV1,
    ) -> Result<Option<SessionExecutionBlocker>, SessionContextCoordinatorError> {
        key.validate()
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        let mut connection = CancellationSafePoolConnection::acquire(self.pool.get())
            .await
            .map_err(|source| database_error("acquire_execution_idle_proof", source))?;
        let mut tx = connection
            .begin()
            .await
            .map_err(|source| database_error("begin_execution_idle_proof", source))?;
        let blocker = locked_execution_reuse_blocker(&mut tx, key).await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit_execution_idle_proof", source))?;
        connection.release();
        Ok(blocker)
    }

    /// Load the provider selection for one Work Session, creating the supplied
    /// server-owned initial binding exactly once when upgrading an existing
    /// Work branch. Session-head locking serializes concurrent first reads
    /// with Run admission and later binding changes.
    pub async fn load_or_initialize_execution_binding(
        &self,
        key: &SessionKeyV1,
        initial: &SessionExecutionBindingV1,
    ) -> Result<SessionExecutionBindingV1, SessionContextCoordinatorError> {
        key.validate()
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        initial.validate()?;
        if initial.generation != 1 || initial.state != SessionExecutionBindingStateV1::Ready {
            return Err(SessionContextCoordinatorError::Invalid(
                "initial Session execution binding must be ready at generation 1".into(),
            ));
        }
        let mut connection = CancellationSafePoolConnection::acquire(self.pool.get())
            .await
            .map_err(|source| database_error("acquire_execution_binding_initialize", source))?;
        let mut tx = connection
            .begin()
            .await
            .map_err(|source| database_error("begin_execution_binding_initialize", source))?;
        admit_session_lifecycle_write(&mut tx, key).await?;
        if let Some(binding) = load_execution_binding_in_tx(&mut tx, key, true).await? {
            if binding.logical_workspace_id != initial.logical_workspace_id {
                return Err(SessionContextCoordinatorError::NeedsRepair(
                    "Session execution binding belongs to another logical workspace".into(),
                ));
            }
            tx.commit()
                .await
                .map_err(|source| database_error("commit_execution_binding_existing", source))?;
            connection.release();
            return Ok(binding);
        }

        admit_controller_context_write(&mut tx, key).await?;
        ensure_database_state_admitted(&mut tx, key, AuthorityEpochsV1::default()).await?;
        let (state, now) = lock_database_state_at_now(&mut tx, key).await?;
        if state
            .active_writer
            .as_ref()
            .is_some_and(|lease| lease.expires_at_unix_ms > now)
            || state
                .active_reservation
                .as_ref()
                .is_some_and(|reservation| reservation.expires_at_unix_ms > now)
        {
            return Err(SessionContextCoordinatorError::ExecutionBindingBusy);
        }

        let binding_json = database_to_json("session_execution_binding", initial)?;
        sqlx::query(
            "INSERT IGNORE INTO session_execution_bindings \
             (isolation_domain, owner_user_id, session_id, branch_id, generation, binding_json, \
              created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, NOW(6), NOW(6))",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .bind(i64_from_u64(
            "execution binding generation",
            initial.generation,
        )?)
        .bind(binding_json)
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("insert_execution_binding", source))?;

        let binding = load_execution_binding_in_tx(&mut tx, key, true)
            .await?
            .ok_or_else(|| {
                SessionContextCoordinatorError::NeedsRepair(
                    "execution binding disappeared during initialization".into(),
                )
            })?;
        if binding.logical_workspace_id != initial.logical_workspace_id {
            return Err(SessionContextCoordinatorError::NeedsRepair(
                "Session execution binding belongs to another logical workspace".into(),
            ));
        }
        tx.commit()
            .await
            .map_err(|source| database_error("commit_execution_binding_initialize", source))?;
        connection.release();
        Ok(binding)
    }

    pub async fn load_execution_binding(
        &self,
        key: &SessionKeyV1,
    ) -> Result<Option<SessionExecutionBindingV1>, SessionContextCoordinatorError> {
        key.validate()
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        let row = sqlx::query(
            "SELECT generation, binding_json FROM session_execution_bindings \
             WHERE isolation_domain = ? AND owner_user_id = ? \
               AND session_id = ? AND branch_id = ?",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| database_error("load_execution_binding", source))?;
        row.map(|row| decode_execution_binding_row(&row))
            .transpose()
    }

    /// Begin one durable Work execution-provider switch. The receipt and the
    /// `Ready -> Switching` binding fence are committed together while the
    /// canonical Session head is locked. No network or workspace operation is
    /// performed in this transaction.
    pub async fn begin_execution_switch(
        &self,
        key: &SessionKeyV1,
        request: &BeginSessionExecutionSwitchV1,
    ) -> Result<SessionExecutionSwitchReceiptV1, SessionContextCoordinatorError> {
        key.validate()
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        validate_idempotency_key(&request.request_id)?;
        validate_idempotency_key(&request.operation_id)?;
        validate_idempotency_key(&request.controller_attachment_id)?;
        if request.expected_generation == 0 {
            return Err(SessionContextCoordinatorError::Invalid(
                "expected execution binding generation must be positive".into(),
            ));
        }
        request.target.validate()?;
        validate_execution_attestation_evidence(&request.source_evidence)?;
        if request.target.generation != request.expected_generation.saturating_add(1)
            || request.target.state != SessionExecutionBindingStateV1::Switching
        {
            return Err(SessionContextCoordinatorError::Invalid(
                "switch target must be the next generation in Switching state".into(),
            ));
        }

        let request_hash = execution_switch_request_hash(key, request)?;
        let mut connection = CancellationSafePoolConnection::acquire(self.pool.get())
            .await
            .map_err(|source| database_error("acquire_execution_switch", source))?;
        let mut tx = connection
            .begin()
            .await
            .map_err(|source| database_error("begin_execution_switch", source))?;
        ensure_database_state(&mut tx, key, AuthorityEpochsV1::default()).await?;
        let (state, now) = lock_database_state_at_now(&mut tx, key).await?;

        // Exact idempotency lookup is done under the Session lock. This makes
        // duplicate requests cheap and prevents two writers from both fencing
        // the same generation.
        if let Some(existing) =
            load_execution_switch_by_request_in_tx(&mut tx, key, &request.request_id, true).await?
        {
            if existing.request_hash != request_hash {
                return Err(SessionContextCoordinatorError::IdempotencyMismatch);
            }
            tx.commit()
                .await
                .map_err(|source| database_error("commit_execution_switch_idempotent", source))?;
            connection.release();
            return Ok(existing);
        }

        if state
            .active_writer
            .as_ref()
            .is_some_and(|lease| lease.expires_at_unix_ms > now)
            || state
                .active_reservation
                .as_ref()
                .is_some_and(|reservation| reservation.expires_at_unix_ms > now)
            || !crate::storage::session_execution_activity(
                &mut tx,
                &key.owner_user_id,
                &key.session_id,
                None,
            )
            .await
            .map_err(|source| database_error("check_execution_switch_activity", source))?
            .is_idle()
        {
            return Err(SessionContextCoordinatorError::ExecutionBindingBusy);
        }
        require_controller_attachment_in_tx(&mut tx, key, &request.controller_attachment_id, now)
            .await?;
        let current = load_execution_binding_in_tx(&mut tx, key, true)
            .await?
            .ok_or(SessionContextCoordinatorError::ExecutionBindingFenced {
                expected: request.expected_generation,
                current: None,
            })?;
        if current.generation != request.expected_generation {
            return Err(SessionContextCoordinatorError::ExecutionBindingFenced {
                expected: request.expected_generation,
                current: Some(current.generation),
            });
        }
        if current.logical_workspace_id != request.target.logical_workspace_id {
            return Err(SessionContextCoordinatorError::Invalid(
                "a Session execution binding cannot change logical workspace".into(),
            ));
        }
        if !matches!(
            (current.state, request.target.state),
            (
                SessionExecutionBindingStateV1::Ready,
                SessionExecutionBindingStateV1::Switching
            ) | (
                SessionExecutionBindingStateV1::NeedsAttention,
                SessionExecutionBindingStateV1::Switching
            )
        ) {
            return Err(SessionContextCoordinatorError::ExecutionBindingNotReady(
                current.state,
            ));
        }

        update_execution_binding_in_tx(&mut tx, key, request.expected_generation, &request.target)
            .await?;
        let receipt = SessionExecutionSwitchReceiptV1 {
            schema_version: SESSION_EXECUTION_SWITCH_SCHEMA_VERSION,
            operation_id: request.operation_id.clone(),
            request_id: request.request_id.clone(),
            controller_attachment_id: request.controller_attachment_id.clone(),
            request_hash,
            key: key.clone(),
            expected_generation: request.expected_generation,
            attempt_expected_generation: request.expected_generation,
            switching_generation: request.target.generation,
            completed_generation: None,
            state: SessionExecutionSwitchStateV1::Switching,
            source: current,
            target: request.target.clone(),
            source_evidence: request.source_evidence.clone(),
            evidence: None,
            failure_code: None,
            attempt: 1,
        };
        insert_execution_switch_in_tx(&mut tx, &receipt).await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit_execution_switch_begin", source))?;
        connection.release();
        Ok(receipt)
    }

    /// Complete a switch after the caller has performed its bounded,
    /// read-only provider attestation. The final binding CAS and receipt are
    /// one transaction; a failed attestation leaves the Session explicitly in
    /// `NeedsAttention` and never falls back to another provider.
    #[allow(clippy::too_many_arguments)]
    pub async fn complete_execution_switch(
        &self,
        key: &SessionKeyV1,
        operation_id: &str,
        controller_attachment_id: Option<&str>,
        expected_attempt: u32,
        expected_switching_generation: u64,
        success: bool,
        evidence: Option<Value>,
        failure_code: Option<String>,
    ) -> Result<SessionExecutionSwitchReceiptV1, SessionContextCoordinatorError> {
        key.validate()
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        validate_idempotency_key(operation_id)?;
        if let Some(code) = &failure_code
            && (code.is_empty() || code.len() > 128 || code.chars().any(char::is_control))
        {
            return Err(SessionContextCoordinatorError::Invalid(
                "execution switch failure code must be at most 128 bytes".into(),
            ));
        }
        let mut connection = CancellationSafePoolConnection::acquire(self.pool.get())
            .await
            .map_err(|source| database_error("acquire_execution_switch_complete", source))?;
        let mut tx = connection
            .begin()
            .await
            .map_err(|source| database_error("begin_execution_switch_complete", source))?;
        ensure_database_state(&mut tx, key, AuthorityEpochsV1::default()).await?;
        let (_state, _now) = lock_database_state_at_now(&mut tx, key).await?;
        let mut receipt = load_execution_switch_in_tx(&mut tx, key, operation_id, true)
            .await?
            .ok_or(SessionContextCoordinatorError::NeedsRepair(
                "execution switch receipt is missing".into(),
            ))?;
        if receipt.state != SessionExecutionSwitchStateV1::Switching {
            tx.commit().await.map_err(|source| {
                database_error("commit_execution_switch_terminal_retry", source)
            })?;
            connection.release();
            return Ok(receipt);
        }
        if receipt.attempt != expected_attempt
            || receipt.switching_generation != expected_switching_generation
        {
            return Err(SessionContextCoordinatorError::ExecutionBindingFenced {
                expected: expected_switching_generation,
                current: Some(receipt.switching_generation),
            });
        }
        if let Some(attachment_id) = controller_attachment_id {
            require_controller_attachment_in_tx(&mut tx, key, attachment_id, _now).await?;
        } else {
            require_active_controller_attachment_in_tx(&mut tx, key, _now).await?;
        }
        let current = load_execution_binding_in_tx(&mut tx, key, true)
            .await?
            .ok_or(SessionContextCoordinatorError::NeedsRepair(
                "execution binding disappeared while completing switch".into(),
            ))?;
        if current.generation != receipt.switching_generation
            || current.state != SessionExecutionBindingStateV1::Switching
        {
            return Err(SessionContextCoordinatorError::ExecutionBindingFenced {
                expected: receipt.switching_generation,
                current: Some(current.generation),
            });
        }
        let next_generation = receipt.switching_generation.checked_add(1).ok_or_else(|| {
            SessionContextCoordinatorError::Invalid("execution binding generation overflow".into())
        })?;
        let mut next = receipt.target.clone();
        next.generation = next_generation;
        next.state = if success {
            SessionExecutionBindingStateV1::Ready
        } else {
            SessionExecutionBindingStateV1::NeedsAttention
        };
        next.validate()?;
        update_execution_binding_in_tx(&mut tx, key, receipt.switching_generation, &next).await?;
        receipt.completed_generation = Some(next_generation);
        receipt.state = if success {
            SessionExecutionSwitchStateV1::Succeeded
        } else {
            SessionExecutionSwitchStateV1::Failed
        };
        receipt.evidence = evidence;
        receipt.failure_code = failure_code;
        update_execution_switch_in_tx(&mut tx, &receipt).await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit_execution_switch_complete", source))?;
        connection.release();
        Ok(receipt)
    }

    /// Retry a failed switch after a process restart or a transient Edge
    /// outage. Retrying only repeats read-only checks in the caller; it never
    /// starts a Run or mutates the workspace.
    pub async fn retry_execution_switch(
        &self,
        key: &SessionKeyV1,
        operation_id: &str,
        controller_attachment_id: &str,
        expected_generation: u64,
    ) -> Result<SessionExecutionSwitchReceiptV1, SessionContextCoordinatorError> {
        key.validate()
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        validate_idempotency_key(operation_id)?;
        let mut connection = CancellationSafePoolConnection::acquire(self.pool.get())
            .await
            .map_err(|source| database_error("acquire_execution_switch_retry", source))?;
        let mut tx = connection
            .begin()
            .await
            .map_err(|source| database_error("begin_execution_switch_retry", source))?;
        ensure_database_state(&mut tx, key, AuthorityEpochsV1::default()).await?;
        let (state, now) = lock_database_state_at_now(&mut tx, key).await?;
        validate_idempotency_key(controller_attachment_id)?;
        let mut receipt = load_execution_switch_in_tx(&mut tx, key, operation_id, true)
            .await?
            .ok_or(SessionContextCoordinatorError::NeedsRepair(
                "execution switch receipt is missing".into(),
            ))?;
        require_controller_attachment_in_tx(&mut tx, key, controller_attachment_id, now).await?;
        if receipt.state == SessionExecutionSwitchStateV1::Succeeded
            || receipt.state == SessionExecutionSwitchStateV1::Switching
        {
            tx.commit().await.map_err(|source| {
                database_error("commit_execution_switch_retry_idempotent", source)
            })?;
            connection.release();
            return Ok(receipt);
        }
        if receipt.completed_generation != Some(expected_generation) {
            return Err(SessionContextCoordinatorError::ExecutionBindingFenced {
                expected: expected_generation,
                current: receipt.completed_generation,
            });
        }
        if state
            .active_writer
            .as_ref()
            .is_some_and(|lease| lease.expires_at_unix_ms > now)
            || state
                .active_reservation
                .as_ref()
                .is_some_and(|reservation| reservation.expires_at_unix_ms > now)
            || !crate::storage::session_execution_activity(
                &mut tx,
                &key.owner_user_id,
                &key.session_id,
                None,
            )
            .await
            .map_err(|source| database_error("check_execution_switch_activity", source))?
            .is_idle()
        {
            return Err(SessionContextCoordinatorError::ExecutionBindingBusy);
        }
        let current = load_execution_binding_in_tx(&mut tx, key, true)
            .await?
            .ok_or(SessionContextCoordinatorError::ExecutionBindingFenced {
                expected: expected_generation,
                current: None,
            })?;
        if current.generation != expected_generation
            || current.state != SessionExecutionBindingStateV1::NeedsAttention
        {
            return Err(SessionContextCoordinatorError::ExecutionBindingFenced {
                expected: expected_generation,
                current: Some(current.generation),
            });
        }
        let next_generation = expected_generation.checked_add(1).ok_or_else(|| {
            SessionContextCoordinatorError::Invalid("execution binding generation overflow".into())
        })?;
        let mut target = receipt.target.clone();
        target.generation = next_generation;
        target.state = SessionExecutionBindingStateV1::Switching;
        target.validate()?;
        update_execution_binding_in_tx(&mut tx, key, expected_generation, &target).await?;
        receipt.switching_generation = next_generation;
        // `expected_generation` identifies the binding immediately before the
        // current attempt. It must advance with the attempt; retaining the
        // original generation makes a valid retry receipt fail its own
        // invariant checks and breaks crash recovery after a second attempt.
        receipt.attempt_expected_generation = expected_generation;
        receipt.completed_generation = None;
        receipt.state = SessionExecutionSwitchStateV1::Switching;
        receipt.target = target;
        receipt.evidence = None;
        receipt.failure_code = None;
        receipt.attempt = receipt.attempt.saturating_add(1);
        update_execution_switch_in_tx(&mut tx, &receipt).await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit_execution_switch_retry", source))?;
        connection.release();
        Ok(receipt)
    }

    /// Authorize a retry caller without changing the binding. This is used
    /// before read-only preflight so a request with a valid owner token but no
    /// controller attachment cannot probe or advance a failed switch.
    pub async fn authorize_execution_switch_retry(
        &self,
        key: &SessionKeyV1,
        operation_id: &str,
        controller_attachment_id: &str,
    ) -> Result<SessionExecutionSwitchReceiptV1, SessionContextCoordinatorError> {
        key.validate()
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        validate_idempotency_key(operation_id)?;
        validate_idempotency_key(controller_attachment_id)?;
        let mut connection = CancellationSafePoolConnection::acquire(self.pool.get())
            .await
            .map_err(|source| database_error("acquire_authorize_execution_switch_retry", source))?;
        let mut tx = connection
            .begin()
            .await
            .map_err(|source| database_error("begin_authorize_execution_switch_retry", source))?;
        ensure_database_state(&mut tx, key, AuthorityEpochsV1::default()).await?;
        let (_state, now) = lock_database_state_at_now(&mut tx, key).await?;
        let receipt = load_execution_switch_in_tx(&mut tx, key, operation_id, true)
            .await?
            .ok_or(SessionContextCoordinatorError::NeedsRepair(
                "execution switch receipt is missing".into(),
            ))?;
        require_controller_attachment_in_tx(&mut tx, key, controller_attachment_id, now).await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit_authorize_execution_switch_retry", source))?;
        connection.release();
        Ok(receipt)
    }

    pub async fn load_execution_switch(
        &self,
        key: &SessionKeyV1,
        operation_id: &str,
    ) -> Result<Option<SessionExecutionSwitchReceiptV1>, SessionContextCoordinatorError> {
        key.validate()
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        validate_idempotency_key(operation_id)?;
        let row = sqlx::query(
            "SELECT record_json FROM session_execution_switches
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ? AND operation_id = ?",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .bind(operation_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| database_error("load_execution_switch", source))?;
        row.map(|row| {
            let record = row
                .try_get::<String, _>("record_json")
                .map_err(|source| database_error("decode_execution_switch_record", source))?;
            let receipt: SessionExecutionSwitchReceiptV1 =
                database_json("session_execution_switch", &record)?;
            validate_execution_switch_receipt(&receipt, key)?;
            Ok(receipt)
        })
        .transpose()
    }

    /// Look up a switch by the caller supplied request id. This is intentionally
    /// indexed and owner scoped so an exact retry can return the durable result
    /// before contacting an Edge registry that may currently be unavailable.
    pub async fn load_execution_switch_by_request(
        &self,
        key: &SessionKeyV1,
        request_id: &str,
    ) -> Result<Option<SessionExecutionSwitchReceiptV1>, SessionContextCoordinatorError> {
        key.validate()
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        validate_idempotency_key(request_id)?;
        let row = sqlx::query(
            "SELECT record_json FROM session_execution_switches
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ? AND request_id = ?",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .bind(request_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| database_error("load_execution_switch_by_request", source))?;
        row.map(|row| {
            let record = row
                .try_get::<String, _>("record_json")
                .map_err(|source| database_error("decode_execution_switch_record", source))?;
            let receipt: SessionExecutionSwitchReceiptV1 =
                database_json("session_execution_switch", &record)?;
            validate_execution_switch_receipt(&receipt, key)?;
            Ok(receipt)
        })
        .transpose()
    }

    /// Load the most recently updated switch receipt for a branch. This is an
    /// indexed projection used only to explain `NeedsAttention`/`Switching`
    /// in a surface; it never infers the current provider from history.
    pub async fn load_latest_execution_switch(
        &self,
        key: &SessionKeyV1,
    ) -> Result<Option<SessionExecutionSwitchReceiptV1>, SessionContextCoordinatorError> {
        key.validate()
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        let row = sqlx::query(
            "SELECT record_json FROM session_execution_switches
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ?
             ORDER BY updated_at DESC, operation_id DESC LIMIT 1",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| database_error("load_latest_execution_switch", source))?;
        row.map(|row| {
            let record = row
                .try_get::<String, _>("record_json")
                .map_err(|source| database_error("decode_latest_execution_switch", source))?;
            let receipt: SessionExecutionSwitchReceiptV1 =
                database_json("session_execution_switch", &record)?;
            validate_execution_switch_receipt(&receipt, key)?;
            Ok(receipt)
        })
        .transpose()
    }
}

#[async_trait]
impl SessionContextCoordinator for DatabaseSessionContextCoordinator {
    async fn resume_execution_turn(
        &self,
        request: ResumedExecutionTurnRequest<'_>,
    ) -> Result<ResumedExecutionTurn, SessionContextCoordinatorError> {
        let key = &request.source.key;
        request.source.validate()?;
        validate_idempotency_key(request.owner_pod_id)?;
        validate_idempotency_key(request.checkpoint_id)?;
        if key.owner_user_id != request.user_id
            || key.session_id != request.session_id
            || request.source.run_id != request.run_id
        {
            return Err(SessionContextCoordinatorError::Unauthorized);
        }
        let mut connection = CancellationSafePoolConnection::acquire(self.pool.get())
            .await
            .map_err(|source| database_error("acquire_execution_resume", source))?;
        let mut tx = connection
            .begin()
            .await
            .map_err(|source| database_error("begin_execution_resume", source))?;
        let staged = async {
            let (mut run, checkpoint) = crate::runs::lock_execution_resume_tx(&mut tx, &request)
                .await
                .map_err(execution_resume_storage_error)?;
            #[derive(Deserialize)]
            struct OriginalContext {
                context_authority: RunContextAuthority,
            }
            let crate::runs::DurableExecutionHandoff::V1 {
                producer_run_id,
                producer_owner_generation,
                heavy,
            } = database_json::<crate::runs::DurableExecutionHandoff<OriginalContext>>(
                "execution_handoff",
                &checkpoint.checkpoint_json,
            )?;
            let source = heavy.context_authority;
            let admitted = run
                .opening
                .as_ref()
                .and_then(crate::runs::DurableRunOpening::context_authority)
                .ok_or(SessionContextCoordinatorError::Fenced)?;
            if source != *request.source
                || source.generation != producer_owner_generation
                || producer_run_id != request.run_id
                || checkpoint.checkpoint_id != request.checkpoint_id
                || checkpoint.idempotency_key
                    != format!(
                        "checkpoint:{}:execution_handoff:{}",
                        request.run_id, producer_owner_generation
                    )
                || run.opening_hash.as_deref() != Some(request.opening_hash)
                || run.context_admission_hash.as_deref()
                    != Some(source.admission_fingerprint().as_str())
                || admitted.admission_fingerprint() != source.admission_fingerprint()
            {
                return Err(SessionContextCoordinatorError::Fenced);
            }
            if run.run_generation == request.expected_owner_generation {
                // The existing custody operation performs the only ownership
                // CAS and immutable checkpoint association.
                crate::runs::activate_execution_resume_tx(
                    &mut tx,
                    &mut run,
                    &checkpoint,
                    request.owner_pod_id,
                )
                .await
                .map_err(execution_resume_storage_error)?;
            } else {
                // lock_execution_resume_tx has proven the exact tail custody
                // claim. Also require its current live owner;
                // a metadata-only checkpoint receipt never authorizes replay.
                let live: i64 = sqlx::query_scalar(
                    "SELECT CAST(COALESCE(status = 'running' AND owner_pod_id = ?
                       AND owner_lease_expires_at >= NOW(6), FALSE) AS SIGNED)
                     FROM agent_runs WHERE user_id = ? AND session_id = ? AND run_id = ?",
                )
                .bind(request.owner_pod_id)
                .bind(request.user_id)
                .bind(request.session_id)
                .bind(request.run_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(|source| database_error("validate_resumed_run_owner", source))?;
                if live != 1 {
                    return Err(SessionContextCoordinatorError::Fenced);
                }
            }
            let binding = match source.binding {
                RunContextBinding::Absent => 0,
                RunContextBinding::Present { generation } => generation,
            };
            let authority = self
                .capture_run_context_snapshot_tx(
                    &mut tx,
                    key,
                    request.run_id,
                    run.run_generation,
                    Some(binding),
                )
                .await?;
            if authority.admission_fingerprint() != source.admission_fingerprint() {
                return Err(SessionContextCoordinatorError::Fenced);
            }
            let receipt = ResumedExecutionTurnReceipt {
                source,
                authority,
                run_id: run.run_id.clone(),
                run_generation: run.run_generation,
                producer_generation: producer_owner_generation,
                checkpoint_id: checkpoint.checkpoint_id.clone(),
            };
            run.checkpoint_json = None;
            Ok(ResumedExecutionTurn {
                run,
                checkpoint,
                receipt,
            })
        }
        .await;
        let proof = match staged {
            Ok(proof) => proof,
            Err(error) => {
                tx.rollback()
                    .await
                    .map_err(|source| database_error("rollback_execution_resume", source))?;
                connection.release();
                return Err(error);
            }
        };
        // The exact existing Run custody claim resolves an ambiguous ACK.
        // No Controller grant or second receipt protocol is issued here.
        tx.commit()
            .await
            .map_err(|source| database_error("commit_execution_resume", source))?;
        connection.release();
        Ok(proof)
    }

    async fn load_head(
        &self,
        key: &SessionKeyV1,
    ) -> Result<Option<SessionContextHeadV1>, SessionContextCoordinatorError> {
        key.validate()
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        let row = sqlx::query(
            "SELECT head_json FROM session_context_heads
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ?",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| database_error("load_head", source))?;
        let Some(row) = row else {
            return Ok(None);
        };
        let head_json = row
            .try_get::<Option<String>, _>("head_json")
            .map_err(|source| database_error("decode_head", source))?;
        let head = head_json
            .as_deref()
            .map(|json| database_json("head", json))
            .transpose()?;
        if let Some(head) = &head {
            validate_head(head)?;
            if head.key != *key {
                return Err(SessionContextCoordinatorError::NeedsRepair(
                    "database head key mismatch".into(),
                ));
            }
        }
        Ok(head)
    }

    async fn load_admission_snapshot(
        &self,
        key: &SessionKeyV1,
    ) -> Result<SessionAdmissionSnapshotV1, SessionContextCoordinatorError> {
        key.validate()
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        let row = sqlx::query(
            "SELECT head_json, active_writer_json, authorization_epoch,
                    device_trust_epoch, permission_epoch,
                    CAST(UNIX_TIMESTAMP(NOW(6)) * 1000 AS SIGNED) AS database_now_unix_ms
             FROM session_context_heads
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ?",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| database_error("load_admission_snapshot", source))?;
        let Some(row) = row else {
            return Ok(SessionAdmissionSnapshotV1 {
                head: None,
                active_writer: None,
                authority_epochs: AuthorityEpochsV1::default(),
            });
        };
        let head = row
            .try_get::<Option<String>, _>("head_json")
            .map_err(|source| database_error("decode_admission_head", source))?
            .as_deref()
            .map(|json| database_json("head", json))
            .transpose()?;
        if let Some(head) = &head {
            validate_head(head)?;
            if head.key != *key {
                return Err(SessionContextCoordinatorError::NeedsRepair(
                    "database admission head key mismatch".into(),
                ));
            }
        }
        let now = row
            .try_get::<i64, _>("database_now_unix_ms")
            .map_err(|source| database_error("decode_admission_database_time", source))?;
        let active_writer = row
            .try_get::<Option<String>, _>("active_writer_json")
            .map_err(|source| database_error("decode_admission_writer", source))?
            .as_deref()
            .map(|json| database_json::<ConversationWriterLeaseV1>("active_writer", json))
            .transpose()?
            .filter(|lease| lease.expires_at_unix_ms > now);
        if active_writer
            .as_ref()
            .is_some_and(|lease| lease.key != *key)
        {
            return Err(SessionContextCoordinatorError::NeedsRepair(
                "admission writer owner-scoped key mismatch".into(),
            ));
        }
        Ok(SessionAdmissionSnapshotV1 {
            head,
            active_writer,
            authority_epochs: AuthorityEpochsV1 {
                authorization_epoch: database_u64(&row, "authorization_epoch")?,
                device_trust_epoch: database_u64(&row, "device_trust_epoch")?,
                permission_epoch: database_u64(&row, "permission_epoch")?,
            },
        })
    }

    async fn materialize(
        &self,
        head: &SessionContextHeadV1,
    ) -> Result<MaterializedConversationV1, SessionContextCoordinatorError> {
        validate_head(head)?;
        let mut nodes = std::collections::HashMap::new();
        self.load_manifest_contents_through(
            &head.key,
            head.cursor.compaction_generation,
            head.cursor.conversation_seq,
            &mut nodes,
        )
        .await?;
        let mut scope = head.key.clone();
        let mut through = head.cursor.conversation_seq;
        let mut visited = HashSet::from([scope.clone()]);
        while let Some(prefix) = self.load_fork_prefix(&scope).await? {
            if prefix.parent_cursor.compaction_generation != head.cursor.compaction_generation {
                break;
            }
            if !visited.insert(prefix.parent_key.clone()) {
                return Err(SessionContextCoordinatorError::NeedsRepair(
                    "cyclic fork lineage".into(),
                ));
            }
            through = through.min(prefix.parent_cursor.conversation_seq);
            self.load_manifest_contents_through(
                &prefix.parent_key,
                prefix.parent_cursor.compaction_generation,
                through,
                &mut nodes,
            )
            .await?;
            scope = prefix.parent_key;
        }
        materialize_nodes(head, order_manifest_chain(head, nodes)?)
    }

    async fn load_manifest_delta(
        &self,
        key: &SessionKeyV1,
        after_manifest_root: Option<&str>,
    ) -> Result<ManifestDeltaV1, SessionContextCoordinatorError> {
        key.validate()
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        validate_optional_manifest_root(after_manifest_root)?;
        let head = self.load_head(key).await?;
        let Some(head) = head else {
            if after_manifest_root.is_some() {
                return Err(SessionContextCoordinatorError::DivergentManifest);
            }
            return Ok(ManifestDeltaV1 {
                schema_version: MANIFEST_DELTA_SCHEMA_VERSION,
                key: key.clone(),
                after_manifest_root: None,
                head: None,
                shared_prefix: None,
                missing_nodes: Vec::new(),
                missing_canonical_bytes: 0,
                missing_message_count: 0,
            });
        };
        if after_manifest_root == Some(head.latest_manifest_root.as_str()) {
            return manifest_delta(
                key.clone(),
                after_manifest_root.map(str::to_owned),
                Some(head),
                None,
                Vec::new(),
            );
        }

        let fork_base = self.load_fork_prefix(key).await?;
        let (after_sequence, lower_bound, chain_boundary, shared_prefix) = match after_manifest_root
        {
            Some(root) => {
                let sequence = if let Some(prefix) = &fork_base
                    && root == prefix.parent_manifest_root
                {
                    prefix.parent_cursor.conversation_seq
                } else {
                    let row = sqlx::query(
                        "SELECT conversation_seq FROM conversation_manifest_nodes
                         FORCE INDEX (uq_context_manifest_root)
                         WHERE isolation_domain = ? AND owner_user_id = ?
                           AND session_id = ? AND branch_id = ?
                           AND manifest_root = ? AND reachable = 1",
                    )
                    .bind(&key.isolation_domain)
                    .bind(&key.owner_user_id)
                    .bind(&key.session_id)
                    .bind(&key.branch_id)
                    .bind(root)
                    .fetch_optional(self.pool.get())
                    .await
                    .map_err(|source| database_error("load_manifest_delta_base", source))?;
                    let Some(row) = row else {
                        return Err(SessionContextCoordinatorError::DivergentManifest);
                    };
                    database_u64(&row, "conversation_seq")?
                };
                if sequence >= head.cursor.conversation_seq {
                    return Err(SessionContextCoordinatorError::DivergentManifest);
                }
                (Some(sequence), sequence, Some(root.to_owned()), None)
            }
            None => match &fork_base {
                Some(prefix)
                    if prefix.parent_cursor.compaction_generation
                        == head.cursor.compaction_generation =>
                {
                    (
                        Some(prefix.parent_cursor.conversation_seq),
                        prefix.parent_cursor.conversation_seq,
                        Some(prefix.parent_manifest_root.clone()),
                        Some(prefix.clone()),
                    )
                }
                Some(_) => (None, 0, None, None),
                None => (None, 0, None, None),
            },
        };
        if chain_boundary.as_deref() == Some(head.latest_manifest_root.as_str()) {
            return manifest_delta(
                key.clone(),
                after_manifest_root.map(str::to_owned),
                Some(head),
                shared_prefix,
                Vec::new(),
            );
        }
        let rows = sqlx::query(
            "SELECT manifest_json FROM conversation_manifest_nodes
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ?
               AND compaction_generation = ? AND reachable = 1
               AND conversation_seq > ? AND conversation_seq <= ?
             ORDER BY conversation_seq ASC",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .bind(i64_from_u64(
            "manifest delta compaction generation",
            head.cursor.compaction_generation,
        )?)
        .bind(i64_from_u64("manifest delta lower bound", lower_bound)?)
        .bind(i64_from_u64(
            "manifest delta head sequence",
            head.cursor.conversation_seq,
        )?)
        .fetch_all(self.pool.get())
        .await
        .map_err(|source| database_error("load_manifest_delta_suffix", source))?;
        let mut nodes = std::collections::HashMap::with_capacity(rows.len());
        for row in rows {
            let json = row
                .try_get::<String, _>("manifest_json")
                .map_err(|source| database_error("decode_manifest_delta", source))?;
            let node: ContextManifestNodeV1 = database_json("manifest_delta", &json)?;
            node.validate()
                .map_err(|error| SessionContextCoordinatorError::NeedsRepair(error.to_string()))?;
            if node.key != *key {
                return Err(SessionContextCoordinatorError::NeedsRepair(
                    "manifest delta owner or branch mismatch".into(),
                ));
            }
            nodes.insert(node.manifest_root.clone(), node);
        }
        let missing =
            order_manifest_suffix(&head, chain_boundary.as_deref(), after_sequence, nodes)?;
        manifest_delta(
            key.clone(),
            after_manifest_root.map(str::to_owned),
            Some(head),
            shared_prefix,
            missing,
        )
    }

    async fn load_manifest_content(
        &self,
        key: &SessionKeyV1,
        manifest_root: &str,
    ) -> Result<Vec<ConversationSegmentV1>, SessionContextCoordinatorError> {
        key.validate()
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        validate_optional_manifest_root(Some(manifest_root))?;
        if let Some(content) = self
            .load_manifest_content_at(key, manifest_root, None)
            .await?
        {
            return Ok(content.segments);
        }
        // A fork retains its captured parent prefix even after the parent
        // Session is deleted. The prefix, not a caller-supplied owner/key,
        // scopes this inherited read.
        let mut scope = key.clone();
        let mut bounds: Option<(u64, u64)> = None;
        let mut visited = HashSet::from([scope.clone()]);
        while let Some(prefix) = self.load_fork_prefix(&scope).await? {
            let generation = prefix.parent_cursor.compaction_generation;
            let mut through = prefix.parent_cursor.conversation_seq;
            if let Some((prior_generation, prior_through)) = bounds {
                if generation != prior_generation {
                    break;
                }
                through = through.min(prior_through);
            }
            if !visited.insert(prefix.parent_key.clone()) {
                return Err(SessionContextCoordinatorError::NeedsRepair(
                    "cyclic fork lineage".into(),
                ));
            }
            bounds = Some((generation, through));
            if let Some(content) = self
                .load_manifest_content_at(&prefix.parent_key, manifest_root, bounds)
                .await?
            {
                return Ok(content.segments);
            }
            scope = prefix.parent_key;
        }
        Err(SessionContextCoordinatorError::SegmentNotFound)
    }

    async fn load_authority_epochs(
        &self,
        key: &SessionKeyV1,
    ) -> Result<Option<AuthorityEpochsV1>, SessionContextCoordinatorError> {
        key.validate()
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        let row = sqlx::query(
            "SELECT authorization_epoch, device_trust_epoch, permission_epoch
             FROM session_context_heads
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ?",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| database_error("load_authority_epochs", source))?;
        row.map(|row| {
            Ok(AuthorityEpochsV1 {
                authorization_epoch: database_u64(&row, "authorization_epoch")?,
                device_trust_epoch: database_u64(&row, "device_trust_epoch")?,
                permission_epoch: database_u64(&row, "permission_epoch")?,
            })
        })
        .transpose()
    }

    async fn load_active_writer(
        &self,
        key: &SessionKeyV1,
    ) -> Result<Option<ConversationWriterLeaseV1>, SessionContextCoordinatorError> {
        key.validate()
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        let mut connection = CancellationSafePoolConnection::acquire(self.pool.get())
            .await
            .map_err(|source| database_error("acquire_load_active_writer", source))?;
        let mut tx = connection
            .begin()
            .await
            .map_err(|source| database_error("begin_load_active_writer", source))?;
        let now = database_now_ms(&mut tx).await?;
        let row = sqlx::query(
            "SELECT active_writer_json
             FROM session_context_heads
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ?",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| database_error("load_active_writer", source))?;
        let Some(row) = row else {
            tx.commit()
                .await
                .map_err(|source| database_error("commit_load_active_writer_empty", source))?;
            connection.release();
            return Ok(None);
        };
        let lease = row
            .try_get::<Option<String>, _>("active_writer_json")
            .map_err(|source| database_error("decode_active_writer", source))?
            .as_deref()
            .map(|json| database_json::<ConversationWriterLeaseV1>("active_writer", json))
            .transpose()?
            .filter(|lease| lease.expires_at_unix_ms > now);
        if lease.as_ref().is_some_and(|lease| lease.key != *key) {
            return Err(SessionContextCoordinatorError::NeedsRepair(
                "active writer owner-scoped key mismatch".into(),
            ));
        }
        tx.commit()
            .await
            .map_err(|source| database_error("commit_load_active_writer", source))?;
        connection.release();
        Ok(lease)
    }

    async fn load_fork_prefix(
        &self,
        key: &SessionKeyV1,
    ) -> Result<Option<SharedManifestPrefixV1>, SessionContextCoordinatorError> {
        key.validate()
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        let row = sqlx::query(
            "SELECT manifest_json FROM session_forks
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND child_session_id = ? AND child_branch_id = ? AND state = 'active'",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| database_error("load_fork_prefix", source))?;
        // Fork records and their ancestor pins survive Session deletion while
        // an active descendant still uses them. Heads do not, so lineage reads
        // use the existing fork owner and its unique child key.
        row.map(|row| {
            let json: String = row
                .try_get("manifest_json")
                .map_err(|source| database_error("decode_fork_prefix", source))?;
            let manifest: SessionForkManifestV1 = database_json("fork_prefix", &json)?;
            manifest
                .validate()
                .map_err(|error| SessionContextCoordinatorError::NeedsRepair(error.to_string()))?;
            if manifest.child_key != *key || manifest.state != SessionForkStateV1::Active {
                return Err(SessionContextCoordinatorError::NeedsRepair(
                    "fork lineage identity or state mismatch".into(),
                ));
            }
            Ok(manifest.shared_prefix())
        })
        .transpose()
    }

    async fn activate_fork(
        &self,
        manifest: &SessionForkManifestV1,
    ) -> Result<SessionContextHeadV1, SessionContextCoordinatorError> {
        validate_prepared_fork(manifest)?;
        let mut connection = CancellationSafePoolConnection::acquire(self.pool.get())
            .await
            .map_err(|source| database_error("acquire_activate_fork", source))?;
        let mut tx = connection
            .begin()
            .await
            .map_err(|source| database_error("begin_activate_fork", source))?;
        admit_session_lifecycle_writes(&mut tx, [&manifest.parent_key, &manifest.child_key])
            .await?;
        let row = sqlx::query(
            "SELECT manifest_json, state FROM session_forks
             WHERE isolation_domain = ? AND owner_user_id = ? AND fork_id = ?
             FOR UPDATE",
        )
        .bind(&manifest.child_key.isolation_domain)
        .bind(&manifest.child_key.owner_user_id)
        .bind(&manifest.fork_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| database_error("lock_prepared_fork", source))?
        .ok_or_else(|| {
            SessionContextCoordinatorError::Invalid("prepared fork record does not exist".into())
        })?;
        let stored_json = row
            .try_get::<String, _>("manifest_json")
            .map_err(|source| database_error("decode_prepared_fork", source))?;
        let stored: SessionForkManifestV1 = database_json("prepared_fork", &stored_json)?;
        let stored_state = row
            .try_get::<String, _>("state")
            .map_err(|source| database_error("decode_prepared_fork_state", source))?;
        if stored_state == "active" {
            stored
                .validate()
                .map_err(|error| SessionContextCoordinatorError::NeedsRepair(error.to_string()))?;
            let mut replay_basis = stored.clone();
            replay_basis.state = SessionForkStateV1::Prepared;
            replay_basis.activated_at_unix_ms = None;
            if replay_basis != *manifest {
                return Err(SessionContextCoordinatorError::Fenced);
            }
            let active_head = sqlx::query(
                "SELECT head_json FROM session_context_heads
                 WHERE isolation_domain = ? AND owner_user_id = ?
                   AND session_id = ? AND branch_id = ?",
            )
            .bind(&stored.child_key.isolation_domain)
            .bind(&stored.child_key.owner_user_id)
            .bind(&stored.child_key.session_id)
            .bind(&stored.child_key.branch_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|source| database_error("load_active_fork_head", source))?
            .try_get::<Option<String>, _>("head_json")
            .map_err(|source| database_error("decode_active_fork_head", source))?
            .ok_or_else(|| {
                SessionContextCoordinatorError::NeedsRepair(
                    "active fork has no canonical child head".into(),
                )
            })?;
            let head: SessionContextHeadV1 = database_json("active_fork_head", &active_head)?;
            validate_head(&head)?;
            if head.key != stored.child_key {
                return Err(SessionContextCoordinatorError::NeedsRepair(
                    "active fork child head escaped its owner scope".into(),
                ));
            }
            tx.commit()
                .await
                .map_err(|source| database_error("commit_active_fork_replay", source))?;
            connection.release();
            return Ok(head);
        }
        if stored_state != "prepared" || stored != *manifest {
            return Err(SessionContextCoordinatorError::Fenced);
        }

        let parent_row = sqlx::query(
            "SELECT manifest_json FROM conversation_manifest_nodes
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ?
               AND compaction_generation = ? AND conversation_seq = ?
               AND manifest_root = ? AND reachable = 1",
        )
        .bind(&manifest.parent_key.isolation_domain)
        .bind(&manifest.parent_key.owner_user_id)
        .bind(&manifest.parent_key.session_id)
        .bind(&manifest.parent_key.branch_id)
        .bind(i64_from_u64(
            "fork parent compaction generation",
            manifest.parent_head.cursor.compaction_generation,
        )?)
        .bind(i64_from_u64(
            "fork parent conversation sequence",
            manifest.parent_head.cursor.conversation_seq,
        )?)
        .bind(&manifest.parent_head.latest_manifest_root)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| database_error("verify_fork_parent_manifest", source))?;
        let parent_matches = if let Some(parent_row) = parent_row {
            let parent_json = parent_row
                .try_get::<String, _>("manifest_json")
                .map_err(|source| database_error("decode_fork_parent_manifest", source))?;
            let parent: ContextManifestNodeV1 =
                database_json("fork_parent_manifest", &parent_json)?;
            parent.key == manifest.parent_key && parent.cursor() == manifest.parent_head.cursor
        } else {
            let row = sqlx::query(
                "SELECT fork_base_json FROM session_context_heads
                 WHERE isolation_domain = ? AND owner_user_id = ?
                   AND session_id = ? AND branch_id = ? FOR UPDATE",
            )
            .bind(&manifest.parent_key.isolation_domain)
            .bind(&manifest.parent_key.owner_user_id)
            .bind(&manifest.parent_key.session_id)
            .bind(&manifest.parent_key.branch_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|source| database_error("load_nested_fork_parent_prefix", source))?;
            let prefix_json = row
                .map(|row| {
                    row.try_get::<Option<String>, _>("fork_base_json")
                        .map_err(|source| database_error("decode_nested_fork_prefix", source))
                })
                .transpose()?
                .flatten();
            let prefix = prefix_json
                .map(|json| database_json::<SharedManifestPrefixV1>("nested_fork_prefix", &json))
                .transpose()?;
            prefix.is_some_and(|prefix| {
                prefix.parent_manifest_root == manifest.parent_head.latest_manifest_root
                    && cursor_projection_matches_head(
                        &prefix.parent_cursor,
                        &manifest.parent_head.cursor,
                    )
                    && prefix.total_canonical_bytes == manifest.parent_head.total_canonical_bytes
                    && prefix.total_message_count == manifest.parent_head.total_message_count
            })
        };
        if !parent_matches {
            return Err(SessionContextCoordinatorError::Invalid(
                "fork parent manifest is missing or does not match the prepared cursor".into(),
            ));
        }

        ensure_database_state(&mut tx, &manifest.child_key, AuthorityEpochsV1::default()).await?;
        let mut child_state = lock_database_state(&mut tx, &manifest.child_key).await?;
        if child_state.head.is_some()
            || child_state.active_writer.is_some()
            || child_state.active_reservation.is_some()
            || child_state.fork_base.is_some()
        {
            return Err(SessionContextCoordinatorError::Fenced);
        }
        let now = database_now_ms(&mut tx).await?;
        let mut active = manifest.clone();
        active.state = SessionForkStateV1::Active;
        active.activated_at_unix_ms = Some(now);
        active
            .validate()
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        let child_head = fork_child_head(&active, child_state.writer_epoch);
        child_state.fork_base = Some(active.shared_prefix());
        child_state.head = Some(child_head.clone());
        update_database_state(&mut tx, &child_state).await?;
        sqlx::query(
            "UPDATE session_forks
             SET state = 'active', manifest_json = ?, activated_at_ms = ?, updated_at = NOW(6)
             WHERE isolation_domain = ? AND owner_user_id = ? AND fork_id = ?
               AND state = 'prepared'",
        )
        .bind(database_to_json("active_fork", &active)?)
        .bind(now)
        .bind(&manifest.child_key.isolation_domain)
        .bind(&manifest.child_key.owner_user_id)
        .bind(&manifest.fork_id)
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("activate_fork_record", source))?;
        sqlx::query(
            "UPDATE conversation_manifest_pins
             SET pin_state = 'active', updated_at = NOW(6)
             WHERE isolation_domain = ? AND owner_user_id = ? AND pin_id = ?",
        )
        .bind(&manifest.parent_key.isolation_domain)
        .bind(&manifest.parent_key.owner_user_id)
        .bind(&manifest.fork_id)
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("activate_fork_pin", source))?;
        insert_fork_event(&mut tx, &active, 1, "prepared", "active").await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit_activate_fork", source))?;
        connection.release();
        Ok(child_head)
    }

    async fn acquire_writer(
        &self,
        key: &SessionKeyV1,
        expected_cursor: Option<&SessionCursorV1>,
        actor: &ActorContextV1,
        ttl: Duration,
        idempotency_key: &str,
    ) -> Result<AcquireWriterOutcome, SessionContextCoordinatorError> {
        validate_ttl(ttl, MAX_LEASE_TTL)?;
        validate_idempotency_key(idempotency_key)?;
        actor
            .validate_for(key)
            .map_err(|_| SessionContextCoordinatorError::Unauthorized)?;
        validate_optional_cursor(key, expected_cursor)?;
        let mut connection = CancellationSafePoolConnection::acquire(self.pool.get())
            .await
            .map_err(|source| database_error("acquire_writer", source))?;
        let mut tx = connection
            .begin()
            .await
            .map_err(|source| database_error("begin_acquire_writer", source))?;
        admit_controller_context_write(&mut tx, key).await?;
        ensure_database_state_admitted(&mut tx, key, actor.authority_epochs).await?;
        let (mut state, now) = lock_database_state_at_now(&mut tx, key).await?;
        let expires_at = checked_expiry(now, ttl)?;
        let request_hash = lease_request_hash(key, expected_cursor, actor);
        if let Some(receipt) = load_database_receipt::<LeaseReceiptV1>(
            &mut tx,
            key,
            "acquire",
            idempotency_key,
            &request_hash,
        )
        .await?
        {
            record_database_authority_event(
                &mut tx,
                &state,
                AuthorityAuditFact {
                    operation: "acquire_writer",
                    outcome: "idempotent_replay",
                    actor: Some(actor),
                    lease_id: Some(&receipt.lease.lease_id),
                    reservation_id: None,
                    expected_cursor,
                },
            )
            .await?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_acquire_replay", source))?;
            connection.release();
            return Ok(AcquireWriterOutcome::AlreadyAcquired(receipt.lease));
        }
        if let Some(active) = state.active_writer.clone()
            && active.idempotency_key == idempotency_key
        {
            validate_lease_request(&active, key, &expected_cursor.cloned(), actor)?;
            record_database_authority_event(
                &mut tx,
                &state,
                AuthorityAuditFact {
                    operation: "acquire_writer",
                    outcome: "idempotent_replay",
                    actor: Some(actor),
                    lease_id: Some(&active.lease_id),
                    reservation_id: None,
                    expected_cursor,
                },
            )
            .await?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_acquire_retry", source))?;
            connection.release();
            return Ok(AcquireWriterOutcome::AlreadyAcquired(active.clone()));
        }
        if state.head.as_ref().map(|head| &head.cursor) != expected_cursor {
            let outcome = AcquireWriterOutcome::Conflict {
                current_head: state.head.clone(),
                active_lease_expires_at_unix_ms: state
                    .active_writer
                    .as_ref()
                    .filter(|lease| lease.expires_at_unix_ms > now)
                    .map(|lease| lease.expires_at_unix_ms),
            };
            record_database_authority_event(
                &mut tx,
                &state,
                AuthorityAuditFact {
                    operation: "acquire_writer",
                    outcome: "cursor_conflict",
                    actor: Some(actor),
                    lease_id: None,
                    reservation_id: None,
                    expected_cursor,
                },
            )
            .await?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_acquire_cursor_conflict", source))?;
            connection.release();
            return Ok(outcome);
        }
        if state
            .active_writer
            .as_ref()
            .is_some_and(|lease| lease.expires_at_unix_ms > now)
        {
            let outcome = AcquireWriterOutcome::Conflict {
                current_head: state.head.clone(),
                active_lease_expires_at_unix_ms: state
                    .active_writer
                    .as_ref()
                    .map(|lease| lease.expires_at_unix_ms),
            };
            record_database_authority_event(
                &mut tx,
                &state,
                AuthorityAuditFact {
                    operation: "acquire_writer",
                    outcome: "writer_conflict",
                    actor: Some(actor),
                    lease_id: None,
                    reservation_id: None,
                    expected_cursor,
                },
            )
            .await?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_acquire_lease_conflict", source))?;
            connection.release();
            return Ok(outcome);
        }
        if actor.authority_epochs != state.authority_epochs {
            record_database_authority_event(
                &mut tx,
                &state,
                AuthorityAuditFact {
                    operation: "acquire_writer",
                    outcome: "stale_fenced",
                    actor: Some(actor),
                    lease_id: None,
                    reservation_id: None,
                    expected_cursor,
                },
            )
            .await?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_acquire_fenced_audit", source))?;
            connection.release();
            return Err(SessionContextCoordinatorError::Fenced);
        }
        archive_database_state_receipts(&mut tx, &state).await?;
        state.active_reservation = None;
        state.writer_epoch = state.writer_epoch.checked_add(1).ok_or_else(|| {
            SessionContextCoordinatorError::NeedsRepair("writer epoch overflow".into())
        })?;
        let lease = ConversationWriterLeaseV1 {
            schema_version: SESSION_COORDINATION_SCHEMA_VERSION,
            key: key.clone(),
            lease_id: Uuid::new_v4().to_string(),
            writer_epoch: state.writer_epoch,
            actor: actor.clone(),
            expected_cursor: expected_cursor.cloned(),
            acquired_at_unix_ms: now,
            expires_at_unix_ms: expires_at,
            idempotency_key: idempotency_key.to_owned(),
        };
        state.active_writer = Some(lease.clone());
        update_database_state(&mut tx, &state).await?;
        record_database_authority_event(
            &mut tx,
            &state,
            AuthorityAuditFact {
                operation: "acquire_writer",
                outcome: "acquired",
                actor: Some(actor),
                lease_id: Some(&lease.lease_id),
                reservation_id: None,
                expected_cursor,
            },
        )
        .await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit_acquire_writer", source))?;
        connection.release();
        Ok(AcquireWriterOutcome::Acquired(lease))
    }

    async fn renew_turn_authority(
        &self,
        lease: &ConversationWriterLeaseV1,
        reservation: &TurnReservationV1,
        ttl: Duration,
    ) -> Result<RenewedTurnAuthority, SessionContextCoordinatorError> {
        validate_ttl(ttl, MAX_LEASE_TTL)?;
        validate_ttl(ttl, MAX_RESERVATION_TTL)?;
        validate_reservation_request(reservation, lease, &lease.expected_cursor)?;
        let mut connection = CancellationSafePoolConnection::acquire(self.pool.get())
            .await
            .map_err(|source| database_error("acquire_renew_turn_authority", source))?;
        let mut tx = connection
            .begin()
            .await
            .map_err(|source| database_error("begin_renew_turn_authority", source))?;
        admit_controller_context_write(&mut tx, &lease.key).await?;
        let now = database_now_ms(&mut tx).await?;
        let mut state = lock_database_state(&mut tx, &lease.key).await?;
        let validation = validate_active_lease(&state, lease, now)
            .and_then(|()| validate_active_reservation(&state, reservation, now));
        if let Err(error) = validation {
            record_database_authority_event(
                &mut tx,
                &state,
                AuthorityAuditFact {
                    operation: "renew_turn_authority",
                    outcome: authority_error_outcome(&error),
                    actor: Some(&lease.actor),
                    lease_id: Some(&lease.lease_id),
                    reservation_id: Some(&reservation.reservation_id),
                    expected_cursor: reservation.expected_cursor.as_ref(),
                },
            )
            .await?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_renew_turn_authority_audit", source))?;
            connection.release();
            return Err(error);
        }
        let expires_at_unix_ms = checked_expiry(now, ttl)?;
        let writer_lease = state.active_writer.as_mut().expect("validated lease");
        writer_lease.expires_at_unix_ms = expires_at_unix_ms;
        let writer_lease = writer_lease.clone();
        let turn_reservation = state
            .active_reservation
            .as_mut()
            .expect("validated reservation");
        turn_reservation.expires_at_unix_ms = expires_at_unix_ms;
        let turn_reservation = turn_reservation.clone();
        update_database_state(&mut tx, &state).await?;
        record_database_authority_event(
            &mut tx,
            &state,
            AuthorityAuditFact {
                operation: "renew_turn_authority",
                outcome: "renewed",
                actor: Some(&lease.actor),
                lease_id: Some(&lease.lease_id),
                reservation_id: Some(&reservation.reservation_id),
                expected_cursor: reservation.expected_cursor.as_ref(),
            },
        )
        .await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit_renew_turn_authority", source))?;
        connection.release();
        Ok(RenewedTurnAuthority {
            writer_lease,
            turn_reservation,
        })
    }

    async fn release_writer(
        &self,
        lease: &ConversationWriterLeaseV1,
    ) -> Result<(), SessionContextCoordinatorError> {
        let mut connection = CancellationSafePoolConnection::acquire(self.pool.get())
            .await
            .map_err(|source| database_error("acquire_release_writer", source))?;
        let mut tx = connection
            .begin()
            .await
            .map_err(|source| database_error("begin_release_writer", source))?;
        admit_controller_context_write(&mut tx, &lease.key).await?;
        let mut state = lock_database_state(&mut tx, &lease.key).await?;
        let outcome = if state.active_writer.as_ref().is_some_and(|active| {
            active.lease_id == lease.lease_id && active.writer_epoch == lease.writer_epoch
        }) {
            clear_writer_authority_in_tx(&mut tx, &mut state).await?;
            "released"
        } else if state.writer_epoch > lease.writer_epoch {
            record_database_authority_event(
                &mut tx,
                &state,
                AuthorityAuditFact {
                    operation: "release_writer",
                    outcome: "stale_fenced",
                    actor: Some(&lease.actor),
                    lease_id: Some(&lease.lease_id),
                    reservation_id: None,
                    expected_cursor: lease.expected_cursor.as_ref(),
                },
            )
            .await?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_release_fenced_audit", source))?;
            connection.release();
            return Err(SessionContextCoordinatorError::Fenced);
        } else {
            "already_released"
        };
        record_database_authority_event(
            &mut tx,
            &state,
            AuthorityAuditFact {
                operation: "release_writer",
                outcome,
                actor: Some(&lease.actor),
                lease_id: Some(&lease.lease_id),
                reservation_id: None,
                expected_cursor: lease.expected_cursor.as_ref(),
            },
        )
        .await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit_release_writer", source))?;
        connection.release();
        Ok(())
    }

    async fn transfer_writer(
        &self,
        request: &WriterTransferRequestV1,
        ttl: Duration,
    ) -> Result<TransferWriterOutcome, SessionContextCoordinatorError> {
        validate_writer_transfer_request(request)?;
        validate_ttl(ttl, MAX_LEASE_TTL)?;
        let request_hash = writer_transfer_request_hash(request);
        let mut connection = CancellationSafePoolConnection::acquire(self.pool.get())
            .await
            .map_err(|source| database_error("acquire_transfer_writer", source))?;
        let mut tx = connection
            .begin()
            .await
            .map_err(|source| database_error("begin_transfer_writer", source))?;
        if request.mode == SessionHandoffModeV1::Forced {
            // Authorization was validated above. Revoking an old writer must
            // precede effect sealing; it does not admit new execution or
            // claim that the previous execution/unknown effects have stopped.
            crate::storage::admit_context_execution_write_with_facts(
                &mut tx,
                &request.key.session_id,
                &request.key.owner_user_id,
            )
            .await
            .map_err(|source| match source {
                sqlx::Error::RowNotFound => SessionContextCoordinatorError::SessionLifecycleFenced,
                source => database_error("admit_forced_writer_fence", source),
            })?;
        } else {
            admit_controller_context_write(&mut tx, &request.key).await?;
        }
        let now = database_now_ms(&mut tx).await?;
        let expires_at = checked_expiry(now, ttl)?;
        let mut state = lock_database_state(&mut tx, &request.key).await?;
        if let Some(receipt) = load_database_receipt::<WriterTransferReceiptV1>(
            &mut tx,
            &request.key,
            "transfer",
            &request.idempotency_key,
            &request_hash,
        )
        .await?
        {
            validate_writer_transfer_receipt(&receipt, &request_hash)?;
            record_database_authority_event(
                &mut tx,
                &state,
                AuthorityAuditFact {
                    operation: "transfer_writer",
                    outcome: "idempotent_replay",
                    actor: Some(&request.target_actor),
                    lease_id: Some(&receipt.lease.lease_id),
                    reservation_id: None,
                    expected_cursor: request.expected_cursor.as_ref(),
                },
            )
            .await?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_transfer_replay", source))?;
            connection.release();
            return Ok(TransferWriterOutcome::AlreadyTransferred(receipt.lease));
        }
        if state.head.as_ref().map(|head| &head.cursor) != request.expected_cursor.as_ref() {
            let outcome =
                writer_transfer_conflict(&state, WriterTransferConflictV1::CursorChanged, now);
            record_database_authority_event(
                &mut tx,
                &state,
                AuthorityAuditFact {
                    operation: "transfer_writer",
                    outcome: "cursor_conflict",
                    actor: Some(&request.target_actor),
                    lease_id: None,
                    reservation_id: None,
                    expected_cursor: request.expected_cursor.as_ref(),
                },
            )
            .await?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_transfer_cursor_conflict", source))?;
            connection.release();
            return Ok(outcome);
        }
        if request
            .expected_writer_epoch
            .is_some_and(|expected| expected != state.writer_epoch)
        {
            let outcome = writer_transfer_conflict(
                &state,
                WriterTransferConflictV1::SourceWriterChanged,
                now,
            );
            record_database_authority_event(
                &mut tx,
                &state,
                AuthorityAuditFact {
                    operation: "transfer_writer",
                    outcome: "writer_epoch_conflict",
                    actor: Some(&request.target_actor),
                    lease_id: None,
                    reservation_id: None,
                    expected_cursor: request.expected_cursor.as_ref(),
                },
            )
            .await?;
            tx.commit().await.map_err(|source| {
                database_error("commit_transfer_writer_epoch_conflict", source)
            })?;
            connection.release();
            return Ok(outcome);
        }
        if request.target_actor.authority_epochs != state.authority_epochs {
            record_database_authority_event(
                &mut tx,
                &state,
                AuthorityAuditFact {
                    operation: "transfer_writer",
                    outcome: "stale_fenced",
                    actor: Some(&request.target_actor),
                    lease_id: None,
                    reservation_id: None,
                    expected_cursor: request.expected_cursor.as_ref(),
                },
            )
            .await?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_transfer_fenced", source))?;
            connection.release();
            return Err(SessionContextCoordinatorError::Fenced);
        }
        if request.mode == SessionHandoffModeV1::Graceful {
            let source = request
                .source_lease
                .as_ref()
                .expect("validated graceful source lease");
            if validate_active_lease(&state, source, now).is_err() {
                let outcome = writer_transfer_conflict(
                    &state,
                    WriterTransferConflictV1::SourceWriterChanged,
                    now,
                );
                record_database_authority_event(
                    &mut tx,
                    &state,
                    AuthorityAuditFact {
                        operation: "transfer_writer",
                        outcome: "source_writer_conflict",
                        actor: Some(&request.target_actor),
                        lease_id: Some(&source.lease_id),
                        reservation_id: None,
                        expected_cursor: request.expected_cursor.as_ref(),
                    },
                )
                .await?;
                tx.commit()
                    .await
                    .map_err(|source| database_error("commit_transfer_source_conflict", source))?;
                connection.release();
                return Ok(outcome);
            }
            if state
                .active_reservation
                .as_ref()
                .is_some_and(|reservation| reservation.expires_at_unix_ms > now)
            {
                let outcome =
                    writer_transfer_conflict(&state, WriterTransferConflictV1::ActiveTurn, now);
                record_database_authority_event(
                    &mut tx,
                    &state,
                    AuthorityAuditFact {
                        operation: "transfer_writer",
                        outcome: "active_turn",
                        actor: Some(&request.target_actor),
                        lease_id: Some(&source.lease_id),
                        reservation_id: state
                            .active_reservation
                            .as_ref()
                            .map(|reservation| reservation.reservation_id.as_str()),
                        expected_cursor: request.expected_cursor.as_ref(),
                    },
                )
                .await?;
                tx.commit()
                    .await
                    .map_err(|source| database_error("commit_transfer_active_turn", source))?;
                connection.release();
                return Ok(outcome);
            }
        }

        archive_database_state_receipts(&mut tx, &state).await?;
        state.active_reservation = None;
        state.writer_epoch = state.writer_epoch.checked_add(1).ok_or_else(|| {
            SessionContextCoordinatorError::NeedsRepair("writer epoch overflow".into())
        })?;
        let lease = ConversationWriterLeaseV1 {
            schema_version: SESSION_COORDINATION_SCHEMA_VERSION,
            key: request.key.clone(),
            lease_id: Uuid::new_v4().to_string(),
            writer_epoch: state.writer_epoch,
            actor: request.target_actor.clone(),
            expected_cursor: request.expected_cursor.clone(),
            acquired_at_unix_ms: now,
            expires_at_unix_ms: expires_at,
            idempotency_key: request.idempotency_key.clone(),
        };
        let receipt = WriterTransferReceiptV1 {
            idempotency_key: request.idempotency_key.clone(),
            request_hash: request_hash.clone(),
            handoff_id: request.handoff_id.clone(),
            mode: request.mode,
            risk: request.risk.clone(),
            lease: lease.clone(),
        };
        state.active_writer = Some(lease.clone());
        state.last_transfer = Some(receipt.clone());
        update_database_state(&mut tx, &state).await?;
        store_database_receipt(
            &mut tx,
            &request.key,
            "transfer",
            &request.idempotency_key,
            &request_hash,
            &receipt,
        )
        .await?;
        record_database_authority_event(
            &mut tx,
            &state,
            AuthorityAuditFact {
                operation: "transfer_writer",
                outcome: match request.mode {
                    SessionHandoffModeV1::Graceful => "graceful_transferred",
                    SessionHandoffModeV1::Forced => "forced_transferred",
                },
                actor: Some(&request.target_actor),
                lease_id: Some(&lease.lease_id),
                reservation_id: None,
                expected_cursor: request.expected_cursor.as_ref(),
            },
        )
        .await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit_transfer_writer", source))?;
        connection.release();
        Ok(TransferWriterOutcome::Transferred(lease))
    }

    async fn reserve_turn(
        &self,
        lease: &ConversationWriterLeaseV1,
        expected_cursor: Option<&SessionCursorV1>,
        ttl: Duration,
        idempotency_key: &str,
        expected_execution_binding_generation: Option<u64>,
    ) -> Result<ReserveTurnOutcome, SessionContextCoordinatorError> {
        validate_turn_reservation_inputs(lease, expected_cursor, ttl, idempotency_key)?;
        let mut connection = CancellationSafePoolConnection::acquire(self.pool.get())
            .await
            .map_err(|source| database_error("acquire_reserve_turn", source))?;
        let mut tx = connection
            .begin()
            .await
            .map_err(|source| database_error("begin_reserve_turn", source))?;
        admit_controller_context_write(&mut tx, &lease.key).await?;
        let staged = self
            .stage_reserve_turn(
                &mut tx,
                lease,
                expected_cursor,
                ttl,
                idempotency_key,
                expected_execution_binding_generation,
            )
            .await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit_reserve_turn", source))?;
        connection.release();
        staged.into_result()
    }

    async fn acquire_writer_and_reserve_turn(
        &self,
        key: &SessionKeyV1,
        expected_cursor: Option<&SessionCursorV1>,
        actor: &ActorContextV1,
        ttl: Duration,
        writer_idempotency_key: &str,
        reservation_idempotency_key: &str,
        expected_execution_binding_generation: Option<u64>,
    ) -> Result<AcquireWriterAndReserveTurnOutcome, SessionContextCoordinatorError> {
        validate_writer_turn_inputs(
            key,
            expected_cursor,
            actor,
            ttl,
            writer_idempotency_key,
            reservation_idempotency_key,
        )?;
        let mut connection = CancellationSafePoolConnection::acquire(self.pool.get())
            .await
            .map_err(|source| database_error("acquire_acquire_writer_and_reserve_turn", source))?;
        let mut tx = connection
            .begin()
            .await
            .map_err(|source| database_error("begin_acquire_writer_and_reserve_turn", source))?;
        admit_controller_context_write(&mut tx, key).await?;
        let staged = self
            .stage_acquire_writer_and_reserve_turn(
                &mut tx,
                WriterTurnReservationRequest {
                    key,
                    expected_cursor,
                    actor,
                    ttl,
                    writer_idempotency_key,
                    reservation_idempotency_key,
                    expected_execution_binding_generation,
                },
            )
            .await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit_acquire_writer_and_reserve_turn", source))?;
        connection.release();
        staged.into_result()
    }

    async fn commit_turn(
        &self,
        reservation: &TurnReservationV1,
        delta: CanonicalTurnDeltaV1,
        idempotency_key: &str,
    ) -> Result<CoordinatorMutationV1, SessionContextCoordinatorError> {
        self.commit_turn_inner(reservation, delta, idempotency_key, false)
            .await
    }

    async fn commit_turn_and_release_writer(
        &self,
        reservation: &TurnReservationV1,
        delta: CanonicalTurnDeltaV1,
        idempotency_key: &str,
    ) -> Result<CoordinatorMutationV1, SessionContextCoordinatorError> {
        self.commit_turn_inner(reservation, delta, idempotency_key, true)
            .await
    }

    async fn advance_authority_epochs(
        &self,
        key: &SessionKeyV1,
        epochs: AuthorityEpochsV1,
    ) -> Result<(), SessionContextCoordinatorError> {
        key.validate()
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        let mut connection = CancellationSafePoolConnection::acquire(self.pool.get())
            .await
            .map_err(|source| database_error("acquire_advance_authority", source))?;
        let mut tx = connection
            .begin()
            .await
            .map_err(|source| database_error("begin_advance_authority", source))?;
        ensure_database_state(&mut tx, key, epochs).await?;
        let mut state = lock_database_state(&mut tx, key).await?;
        if epochs.authorization_epoch < state.authority_epochs.authorization_epoch
            || epochs.device_trust_epoch < state.authority_epochs.device_trust_epoch
            || epochs.permission_epoch < state.authority_epochs.permission_epoch
        {
            let error =
                SessionContextCoordinatorError::Invalid("authority epochs cannot decrease".into());
            record_database_authority_event(
                &mut tx,
                &state,
                AuthorityAuditFact {
                    operation: "advance_epochs",
                    outcome: "rejected",
                    actor: None,
                    lease_id: None,
                    reservation_id: None,
                    expected_cursor: state.head.as_ref().map(|head| &head.cursor),
                },
            )
            .await?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_advance_epochs_audit", source))?;
            connection.release();
            return Err(error);
        }
        let outcome = if epochs != state.authority_epochs {
            archive_database_state_receipts(&mut tx, &state).await?;
            state.authority_epochs = epochs;
            state.active_writer = None;
            state.active_reservation = None;
            state.writer_epoch = state.writer_epoch.checked_add(1).ok_or_else(|| {
                SessionContextCoordinatorError::NeedsRepair("writer epoch overflow".into())
            })?;
            update_database_state(&mut tx, &state).await?;
            "advanced"
        } else {
            "unchanged"
        };
        record_database_authority_event(
            &mut tx,
            &state,
            AuthorityAuditFact {
                operation: "advance_epochs",
                outcome,
                actor: None,
                lease_id: None,
                reservation_id: None,
                expected_cursor: state.head.as_ref().map(|head| &head.cursor),
            },
        )
        .await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit_advance_authority", source))?;
        connection.release();
        Ok(())
    }
}

impl DatabaseSessionContextCoordinator {
    /// Verify immutable evidence of an exact Run commit after an unknown ACK.
    /// Historical Context remains valid after a newer head or compaction;
    /// this method neither consults mutable authority nor repairs storage.
    pub async fn verify_prepared_run_context_commit(
        &self,
        prepared: &PreparedSessionContextCommit,
        cursor: &SessionCursorV1,
        commit_hash: &str,
    ) -> Result<(), SessionContextCoordinatorError> {
        let authority = prepared.run_context_authority().ok_or_else(|| {
            SessionContextCoordinatorError::Invalid(
                "exact Run verification requires Run Context".into(),
            )
        })?;
        if prepared.cursor() != *cursor || prepared.request_hash() != commit_hash {
            return Err(SessionContextCoordinatorError::NeedsRepair(
                "terminal Context identity does not match its prepared commit".into(),
            ));
        }
        let key = &authority.key;
        let node = &prepared.node;
        let (bytes, messages) = next_head_totals(
            authority.base_head.as_ref(),
            &prepared.segments,
            prepared.delta.mode,
        )?;
        let canonical_segment_bytes =
            node.appended_segments
                .iter()
                .try_fold(0_u64, |sum, segment| {
                    sum.checked_add(segment.canonical_bytes).ok_or_else(|| {
                        SessionContextCoordinatorError::Invalid(
                            "manifest segment byte count overflow".into(),
                        )
                    })
                })?;
        let stored = sqlx::query(
            "SELECT parent_manifest_root, completed_turn, conversation_seq,
                    compaction_generation, canonical_segment_bytes, total_canonical_bytes,
                    total_message_count, manifest_root, manifest_json, content_json, reachable
             FROM conversation_manifest_nodes
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ?
               AND compaction_generation = ? AND conversation_seq = ? AND manifest_root = ?",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .bind(i64_from_u64(
            "manifest compaction generation",
            node.compaction_generation,
        )?)
        .bind(i64_from_u64(
            "manifest conversation sequence",
            node.conversation_seq,
        )?)
        .bind(&node.manifest_root)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| database_error("verify_committed_Run_manifest", source))?
        .ok_or_else(|| {
            SessionContextCoordinatorError::NeedsRepair(
                "committed Context manifest is missing".into(),
            )
        })?;
        validate_stored_manifest(&stored, node, canonical_segment_bytes, bytes, messages)?;
        let content = decode_stored_manifest_content(&stored, key)?;
        if content.segments != prepared.segments {
            return Err(SessionContextCoordinatorError::NeedsRepair(
                "committed Context differs from its prepared content".into(),
            ));
        }
        Ok(())
    }

    async fn load_manifest_content_at(
        &self,
        key: &SessionKeyV1,
        manifest_root: &str,
        inherited_bounds: Option<(u64, u64)>,
    ) -> Result<Option<StoredManifestContent>, SessionContextCoordinatorError> {
        let mut sql = format!(
            "SELECT {MANIFEST_CONTENT_COLUMNS} FROM conversation_manifest_nodes FORCE INDEX (uq_context_manifest_root)
             WHERE isolation_domain = ? AND owner_user_id = ? AND session_id = ?
               AND branch_id = ? AND manifest_root = ? AND reachable = 1"
        );
        if inherited_bounds.is_some() {
            sql.push_str(" AND compaction_generation = ? AND conversation_seq <= ?");
        }
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&key.isolation_domain)
            .bind(&key.owner_user_id)
            .bind(&key.session_id)
            .bind(&key.branch_id)
            .bind(manifest_root);
        if let Some((generation, through)) = inherited_bounds {
            query = query
                .bind(i64_from_u64("inherited compaction generation", generation)?)
                .bind(i64_from_u64("inherited conversation sequence", through)?);
        }
        let row = query
            .fetch_optional(self.pool.get())
            .await
            .map_err(|source| database_error("load_manifest_content", source))?;
        row.map(|row| decode_stored_manifest_content(&row, key))
            .transpose()
    }

    async fn load_manifest_contents_through(
        &self,
        key: &SessionKeyV1,
        generation: u64,
        through: u64,
        nodes: &mut std::collections::HashMap<String, StoredManifestContent>,
    ) -> Result<(), SessionContextCoordinatorError> {
        let sql = format!(
            "SELECT {MANIFEST_CONTENT_COLUMNS} FROM conversation_manifest_nodes
             WHERE isolation_domain = ? AND owner_user_id = ? AND session_id = ?
               AND branch_id = ? AND compaction_generation = ? AND reachable = 1
               AND conversation_seq <= ?"
        );
        let query = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&key.isolation_domain)
            .bind(&key.owner_user_id)
            .bind(&key.session_id)
            .bind(&key.branch_id)
            .bind(i64_from_u64("head compaction generation", generation)?)
            .bind(i64_from_u64("head conversation sequence", through)?);
        // Consume each bounded body once rather than retaining every SQL row
        // and then allocating a second global segment-hash projection.
        let mut rows = query.fetch(self.pool.get());
        while let Some(row) = rows
            .try_next()
            .await
            .map_err(|source| database_error("load_manifest_contents", source))?
        {
            let content = decode_stored_manifest_content(&row, key)?;
            if nodes
                .insert(content.node.manifest_root.clone(), content)
                .is_some()
            {
                return Err(SessionContextCoordinatorError::NeedsRepair(
                    "duplicate manifest identity".into(),
                ));
            }
        }
        Ok(())
    }

    async fn persist_database_immutables_in_tx(
        &self,
        tx: &mut Transaction<'_, MySql>,
        key: &SessionKeyV1,
        node: &ContextManifestNodeV1,
        content_json: &str,
        total_canonical_bytes: u64,
        total_message_count: u64,
    ) -> Result<(), SessionContextCoordinatorError> {
        let canonical_segment_bytes = node
            .appended_segments
            .iter()
            .try_fold(0_u64, |sum, segment| {
                sum.checked_add(segment.canonical_bytes)
            })
            .ok_or_else(|| {
                SessionContextCoordinatorError::Invalid("manifest byte count overflow".into())
            })?;
        let sql = matrixone_statement_with_null_shape(
            "INSERT INTO conversation_manifest_nodes
             (isolation_domain, owner_user_id, session_id, branch_id, manifest_root,
              parent_manifest_root, completed_turn, conversation_seq, compaction_generation,
              canonical_segment_bytes, total_canonical_bytes, total_message_count,
              manifest_json, content_json, reachable)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1)",
            [node.parent_manifest_root.is_some()],
        );
        let result = sqlx::query(sql)
            .bind(&key.isolation_domain)
            .bind(&key.owner_user_id)
            .bind(&key.session_id)
            .bind(&key.branch_id)
            .bind(&node.manifest_root)
            .bind(&node.parent_manifest_root)
            .bind(i64::from(node.completed_turn))
            .bind(i64_from_u64(
                "conversation sequence",
                node.conversation_seq,
            )?)
            .bind(i64_from_u64(
                "manifest compaction generation",
                node.compaction_generation,
            )?)
            .bind(i64_from_u64(
                "manifest segment bytes",
                canonical_segment_bytes,
            )?)
            .bind(i64_from_u64("manifest total bytes", total_canonical_bytes)?)
            .bind(i64_from_u64(
                "manifest total messages",
                total_message_count,
            )?)
            .bind(database_to_json("manifest", node)?)
            .bind(content_json)
            .execute(&mut **tx)
            .await;
        match result {
            Ok(_) => Ok(()),
            Err(source) if astra_core::is_duplicate_key_error(&source) => {
                let sql = format!(
                    "SELECT {MANIFEST_CONTENT_COLUMNS} FROM conversation_manifest_nodes
                     WHERE isolation_domain = ? AND owner_user_id = ? AND session_id = ?
                       AND branch_id = ? AND compaction_generation = ? AND conversation_seq = ? FOR UPDATE"
                );
                let row = sqlx::query(sqlx::AssertSqlSafe(sql))
                    .bind(&key.isolation_domain)
                    .bind(&key.owner_user_id)
                    .bind(&key.session_id)
                    .bind(&key.branch_id)
                    .bind(i64_from_u64(
                        "manifest compaction generation",
                        node.compaction_generation,
                    )?)
                    .bind(i64_from_u64(
                        "manifest conversation sequence",
                        node.conversation_seq,
                    )?)
                    .fetch_optional(&mut **tx)
                    .await
                    .map_err(|source| database_error("verify_existing_manifest", source))?
                    .ok_or_else(|| {
                        SessionContextCoordinatorError::NeedsRepair(
                            "immutable manifest identity conflicts with another coordinate".into(),
                        )
                    })?;
                validate_stored_manifest(
                    &row,
                    node,
                    canonical_segment_bytes,
                    total_canonical_bytes,
                    total_message_count,
                )?;
                decode_stored_manifest_content(&row, key)?;
                Ok(())
            }
            Err(source) => Err(database_error("persist_manifest", source)),
        }
    }
}

fn order_manifest_chain(
    head: &SessionContextHeadV1,
    mut nodes: std::collections::HashMap<String, StoredManifestContent>,
) -> Result<Vec<StoredManifestContent>, SessionContextCoordinatorError> {
    let mut root = Some(head.latest_manifest_root.clone());
    let mut seen = HashSet::new();
    let mut reverse = Vec::new();
    while let Some(current) = root {
        if !seen.insert(current.clone()) {
            return Err(SessionContextCoordinatorError::NeedsRepair(
                "manifest cycle detected".into(),
            ));
        }
        let node = nodes.remove(&current).ok_or_else(|| {
            SessionContextCoordinatorError::NeedsRepair(format!("missing manifest {current}"))
        })?;
        root = if node.node.replaces_history {
            None
        } else {
            node.node.parent_manifest_root.clone()
        };
        reverse.push(node);
    }
    reverse.reverse();
    if reverse
        .last()
        .is_none_or(|node| !cursor_projection_matches_head(&node.node.cursor(), &head.cursor))
    {
        return Err(SessionContextCoordinatorError::NeedsRepair(
            "head cursor does not match database manifest".into(),
        ));
    }
    Ok(reverse)
}

fn cursor_projection_matches_head(node: &SessionCursorV1, head: &SessionCursorV1) -> bool {
    node.schema_version == head.schema_version
        && node.completed_turn == head.completed_turn
        && node.journal_event_seq == head.journal_event_seq
        && node.conversation_seq == head.conversation_seq
        && node.canonical_root_hash == head.canonical_root_hash
        && node.projection_schema == head.projection_schema
        && node.compaction_generation == head.compaction_generation
        && node.config_version_id == head.config_version_id
}

fn order_manifest_suffix(
    head: &SessionContextHeadV1,
    after_manifest_root: Option<&str>,
    after_sequence: Option<u64>,
    mut nodes: std::collections::HashMap<String, ContextManifestNodeV1>,
) -> Result<Vec<ContextManifestNodeV1>, SessionContextCoordinatorError> {
    let mut root = Some(head.latest_manifest_root.clone());
    let mut seen = HashSet::new();
    let mut reverse = Vec::new();
    while root.as_deref() != after_manifest_root {
        let current = root.ok_or(SessionContextCoordinatorError::DivergentManifest)?;
        if !seen.insert(current.clone()) {
            return Err(SessionContextCoordinatorError::NeedsRepair(
                "manifest cycle detected while loading delta".into(),
            ));
        }
        let node = nodes.remove(&current).ok_or_else(|| {
            if after_manifest_root.is_some() {
                SessionContextCoordinatorError::DivergentManifest
            } else {
                SessionContextCoordinatorError::NeedsRepair(format!("missing manifest {current}"))
            }
        })?;
        if after_sequence.is_some_and(|sequence| node.conversation_seq <= sequence) {
            return Err(SessionContextCoordinatorError::DivergentManifest);
        }
        if node.replaces_history
            && after_manifest_root.is_some()
            && node.parent_manifest_root.as_deref() != after_manifest_root
        {
            return Err(SessionContextCoordinatorError::DivergentManifest);
        }
        root = if node.replaces_history && after_manifest_root.is_none() {
            None
        } else {
            node.parent_manifest_root.clone()
        };
        reverse.push(node);
    }
    reverse.reverse();
    if reverse
        .last()
        .is_none_or(|node| !cursor_projection_matches_head(&node.cursor(), &head.cursor))
    {
        return Err(SessionContextCoordinatorError::NeedsRepair(
            "head cursor does not match manifest delta".into(),
        ));
    }
    Ok(reverse)
}

fn manifest_delta(
    key: SessionKeyV1,
    after_manifest_root: Option<String>,
    head: Option<SessionContextHeadV1>,
    shared_prefix: Option<SharedManifestPrefixV1>,
    missing_nodes: Vec<ContextManifestNodeV1>,
) -> Result<ManifestDeltaV1, SessionContextCoordinatorError> {
    let (missing_canonical_bytes, missing_message_count) = missing_nodes
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
        .ok_or_else(|| {
            SessionContextCoordinatorError::NeedsRepair("manifest delta totals overflow".into())
        })?;
    let delta = ManifestDeltaV1 {
        schema_version: MANIFEST_DELTA_SCHEMA_VERSION,
        key,
        after_manifest_root,
        head,
        shared_prefix,
        missing_nodes,
        missing_canonical_bytes,
        missing_message_count,
    };
    delta
        .validate()
        .map_err(|error| SessionContextCoordinatorError::NeedsRepair(error.to_string()))?;
    Ok(delta)
}

fn materialize_nodes(
    head: &SessionContextHeadV1,
    nodes: Vec<StoredManifestContent>,
) -> Result<MaterializedConversationV1, SessionContextCoordinatorError> {
    let mut messages = Vec::new();
    let mut logical_segment_count = 0_u64;
    let mut canonical_segment_bytes = 0_u64;
    let mut prior_cursor = None;
    for content in nodes {
        validate_manifest_advance(prior_cursor.as_ref(), &content.node)?;
        for segment in content.segments {
            canonical_segment_bytes = canonical_segment_bytes
                .checked_add(segment.canonical_bytes)
                .ok_or_else(|| {
                    SessionContextCoordinatorError::NeedsRepair(
                        "materialized byte count overflow".into(),
                    )
                })?;
            logical_segment_count = logical_segment_count.checked_add(1).ok_or_else(|| {
                SessionContextCoordinatorError::NeedsRepair(
                    "materialized segment count overflow".into(),
                )
            })?;
            messages.extend(segment.messages);
        }
        if content.total_canonical_bytes != canonical_segment_bytes
            || content.total_message_count != u64::try_from(messages.len()).unwrap_or(u64::MAX)
        {
            return Err(SessionContextCoordinatorError::NeedsRepair(
                "manifest totals do not match its content".into(),
            ));
        }
        prior_cursor = Some(content.node.cursor());
    }
    if canonical_segment_bytes != head.total_canonical_bytes
        || u64::try_from(messages.len()).ok() != Some(head.total_message_count)
    {
        return Err(SessionContextCoordinatorError::NeedsRepair(
            "materialized totals do not match the canonical head".into(),
        ));
    }
    Ok(MaterializedConversationV1 {
        head: head.clone(),
        messages,
        logical_segment_count,
        canonical_segment_bytes,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CoordinatorStateV1 {
    schema_version: u32,
    key: SessionKeyV1,
    writer_epoch: u64,
    authority_epochs: AuthorityEpochsV1,
    head: Option<SessionContextHeadV1>,
    active_writer: Option<ConversationWriterLeaseV1>,
    active_reservation: Option<TurnReservationV1>,
    last_commit: Option<CommitReceiptV1>,
    #[serde(default)]
    last_transfer: Option<WriterTransferReceiptV1>,
    #[serde(default)]
    fork_base: Option<SharedManifestPrefixV1>,
    #[serde(default)]
    fork_manifest: Option<SessionForkManifestV1>,
}

impl CoordinatorStateV1 {
    fn new(key: SessionKeyV1) -> Self {
        Self {
            schema_version: COORDINATOR_STATE_SCHEMA_VERSION,
            key,
            writer_epoch: 0,
            authority_epochs: AuthorityEpochsV1::default(),
            head: None,
            active_writer: None,
            active_reservation: None,
            last_commit: None,
            last_transfer: None,
            fork_base: None,
            fork_manifest: None,
        }
    }

    fn validate_for(&self, key: &SessionKeyV1) -> Result<(), SessionContextCoordinatorError> {
        if self.schema_version != COORDINATOR_STATE_SCHEMA_VERSION || &self.key != key {
            return Err(SessionContextCoordinatorError::NeedsRepair(
                "state schema or owner-scoped key mismatch".into(),
            ));
        }
        if self
            .head
            .as_ref()
            .is_some_and(|head| head.key != *key || head.writer_epoch > self.writer_epoch)
            || self
                .last_transfer
                .as_ref()
                .is_some_and(|receipt| receipt.lease.key != *key)
            || self
                .fork_base
                .as_ref()
                .is_some_and(|prefix| prefix.validate_for_child(key).is_err())
            || self.fork_manifest.as_ref().is_some_and(|manifest| {
                manifest.child_key != *key
                    || manifest.state != SessionForkStateV1::Active
                    || manifest.validate().is_err()
            })
        {
            return Err(SessionContextCoordinatorError::NeedsRepair(
                "head key or writer epoch is invalid".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LeaseReceiptV1 {
    lease: ConversationWriterLeaseV1,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReservationReceiptV1 {
    reservation: TurnReservationV1,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CommitReceiptV1 {
    idempotency_key: String,
    reservation_id: String,
    reservation: TurnReservationV1,
    delta_hash: String,
    cursor: SessionCursorV1,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WriterTransferReceiptV1 {
    idempotency_key: String,
    request_hash: String,
    handoff_id: String,
    mode: SessionHandoffModeV1,
    risk: HandoffRiskEvidenceV1,
    lease: ConversationWriterLeaseV1,
}

fn validate_turn_reservation_inputs(
    lease: &ConversationWriterLeaseV1,
    expected_cursor: Option<&SessionCursorV1>,
    ttl: Duration,
    idempotency_key: &str,
) -> Result<(), SessionContextCoordinatorError> {
    validate_ttl(ttl, MAX_RESERVATION_TTL)?;
    validate_idempotency_key(idempotency_key)?;
    validate_optional_cursor(&lease.key, expected_cursor)
}

fn validate_writer_turn_inputs(
    key: &SessionKeyV1,
    expected_cursor: Option<&SessionCursorV1>,
    actor: &ActorContextV1,
    ttl: Duration,
    writer_key: &str,
    reservation_key: &str,
) -> Result<(), SessionContextCoordinatorError> {
    validate_ttl(ttl, MAX_LEASE_TTL.min(MAX_RESERVATION_TTL))?;
    validate_idempotency_key(writer_key)?;
    validate_idempotency_key(reservation_key)?;
    actor
        .validate_for(key)
        .map_err(|_| SessionContextCoordinatorError::Unauthorized)?;
    validate_optional_cursor(key, expected_cursor)
}

impl DatabaseSessionContextCoordinator {
    async fn stage_reserve_turn(
        &self,
        tx: &mut Transaction<'_, MySql>,
        lease: &ConversationWriterLeaseV1,
        expected_cursor: Option<&SessionCursorV1>,
        ttl: Duration,
        idempotency_key: &str,
        expected_execution_binding_generation: Option<u64>,
    ) -> Result<StagedCoordinatorMutation<ReserveTurnOutcome>, SessionContextCoordinatorError> {
        let (mut state, now) = lock_database_state_at_now(tx, &lease.key).await?;
        validate_execution_binding_generation_in_tx(
            tx,
            &lease.key,
            expected_execution_binding_generation,
        )
        .await?;
        let request_hash = reservation_request_hash(lease, expected_cursor);
        if let Some(receipt) = load_database_receipt::<ReservationReceiptV1>(
            tx,
            &lease.key,
            "reserve",
            idempotency_key,
            &request_hash,
        )
        .await?
        {
            record_database_authority_event(
                tx,
                &state,
                AuthorityAuditFact {
                    operation: "reserve_turn",
                    outcome: "idempotent_replay",
                    actor: Some(&lease.actor),
                    lease_id: Some(&lease.lease_id),
                    reservation_id: Some(&receipt.reservation.reservation_id),
                    expected_cursor,
                },
            )
            .await?;
            return Ok(StagedCoordinatorMutation::Mutation(
                ReserveTurnOutcome::AlreadyReserved(receipt.reservation),
            ));
        }
        if let Some(active) = state.active_reservation.clone()
            && active.idempotency_key == idempotency_key
        {
            validate_reservation_request(&active, lease, &expected_cursor.cloned())?;
            validate_active_lease(&state, lease, now)?;
            if fence_expired_reservation_authority(&mut state, lease, now) {
                // The preceding acquire_writer replay can have refreshed the
                // writer. Fence that refresh in this transaction when its
                // paired reservation has already expired.
                update_database_state(tx, &state).await?;
                record_database_authority_event(
                    tx,
                    &state,
                    AuthorityAuditFact {
                        operation: "reserve_turn",
                        outcome: "expired_authority_fenced",
                        actor: Some(&lease.actor),
                        lease_id: Some(&lease.lease_id),
                        reservation_id: Some(&active.reservation_id),
                        expected_cursor,
                    },
                )
                .await?;
                return Ok(StagedCoordinatorMutation::Rejected(
                    SessionContextCoordinatorError::Expired,
                ));
            }
            let expires_at = refreshed_live_expiry(
                now,
                ttl,
                active.expires_at_unix_ms,
                Some(lease.expires_at_unix_ms),
            )?;
            let refreshed = state
                .active_reservation
                .as_mut()
                .expect("matched active reservation");
            refreshed.expires_at_unix_ms = expires_at;
            let refreshed = refreshed.clone();
            update_database_state(tx, &state).await?;
            record_database_authority_event(
                tx,
                &state,
                AuthorityAuditFact {
                    operation: "reserve_turn",
                    outcome: "idempotent_refreshed",
                    actor: Some(&lease.actor),
                    lease_id: Some(&lease.lease_id),
                    reservation_id: Some(&refreshed.reservation_id),
                    expected_cursor,
                },
            )
            .await?;
            return Ok(StagedCoordinatorMutation::Mutation(
                ReserveTurnOutcome::AlreadyReserved(refreshed),
            ));
        }
        if let Err(error) = validate_active_lease(&state, lease, now) {
            record_database_authority_event(
                tx,
                &state,
                AuthorityAuditFact {
                    operation: "reserve_turn",
                    outcome: authority_error_outcome(&error),
                    actor: Some(&lease.actor),
                    lease_id: Some(&lease.lease_id),
                    reservation_id: None,
                    expected_cursor,
                },
            )
            .await?;
            return Ok(StagedCoordinatorMutation::Rejected(error));
        }
        if state.head.as_ref().map(|head| &head.cursor) != expected_cursor {
            let outcome = ReserveTurnOutcome::Conflict {
                current_head: state.head.clone(),
            };
            record_database_authority_event(
                tx,
                &state,
                AuthorityAuditFact {
                    operation: "reserve_turn",
                    outcome: "cursor_conflict",
                    actor: Some(&lease.actor),
                    lease_id: Some(&lease.lease_id),
                    reservation_id: None,
                    expected_cursor,
                },
            )
            .await?;
            return Ok(StagedCoordinatorMutation::Mutation(outcome));
        }
        if state
            .active_reservation
            .as_ref()
            .is_some_and(|reservation| reservation.expires_at_unix_ms > now)
        {
            let outcome = ReserveTurnOutcome::Conflict {
                current_head: state.head.clone(),
            };
            record_database_authority_event(
                tx,
                &state,
                AuthorityAuditFact {
                    operation: "reserve_turn",
                    outcome: "reservation_conflict",
                    actor: Some(&lease.actor),
                    lease_id: Some(&lease.lease_id),
                    reservation_id: None,
                    expected_cursor,
                },
            )
            .await?;
            return Ok(StagedCoordinatorMutation::Mutation(outcome));
        }
        if let Some(previous) = &state.active_reservation {
            archive_database_reservation(tx, previous).await?;
        }
        let reservation = TurnReservationV1 {
            schema_version: SESSION_COORDINATION_SCHEMA_VERSION,
            reservation_id: Uuid::new_v4().to_string(),
            key: lease.key.clone(),
            lease_id: lease.lease_id.clone(),
            writer_epoch: lease.writer_epoch,
            expected_cursor: expected_cursor.cloned(),
            reserved_turn: expected_cursor
                .map_or(1, |cursor| cursor.completed_turn.saturating_add(1)),
            created_at_unix_ms: now,
            expires_at_unix_ms: checked_expiry(now, ttl)?.min(lease.expires_at_unix_ms),
            idempotency_key: idempotency_key.to_owned(),
        };
        state.active_reservation = Some(reservation.clone());
        update_database_state(tx, &state).await?;
        record_database_authority_event(
            tx,
            &state,
            AuthorityAuditFact {
                operation: "reserve_turn",
                outcome: "reserved",
                actor: Some(&lease.actor),
                lease_id: Some(&lease.lease_id),
                reservation_id: Some(&reservation.reservation_id),
                expected_cursor,
            },
        )
        .await?;
        Ok(StagedCoordinatorMutation::Mutation(
            ReserveTurnOutcome::Reserved(reservation),
        ))
    }

    async fn stage_acquire_writer_and_reserve_turn(
        &self,
        tx: &mut Transaction<'_, MySql>,
        request: WriterTurnReservationRequest<'_>,
    ) -> Result<
        StagedCoordinatorMutation<AcquireWriterAndReserveTurnOutcome>,
        SessionContextCoordinatorError,
    > {
        let WriterTurnReservationRequest {
            key,
            expected_cursor,
            actor,
            ttl,
            writer_idempotency_key,
            reservation_idempotency_key,
            expected_execution_binding_generation,
        } = request;
        ensure_database_state_admitted(tx, key, actor.authority_epochs).await?;
        let (mut state, now) = lock_database_state_at_now(tx, key).await?;
        validate_execution_binding_generation_in_tx(tx, key, expected_execution_binding_generation)
            .await?;
        let expected_cursor_owned = expected_cursor.cloned();

        let (lease, acquire_outcome) = if let Some(active) = state.active_writer.clone()
            && active.idempotency_key == writer_idempotency_key
        {
            validate_lease_request(&active, key, &expected_cursor_owned, actor)?;
            let expires_at = refreshed_live_expiry(now, ttl, active.expires_at_unix_ms, None)?;
            let refreshed = state
                .active_writer
                .as_mut()
                .expect("matched active writer lease");
            refreshed.expires_at_unix_ms = expires_at;
            (refreshed.clone(), "idempotent_refreshed")
        } else {
            if state.head.as_ref().map(|head| &head.cursor) != expected_cursor {
                let current_head = state.head.clone();
                let active_lease_expires_at_unix_ms = state
                    .active_writer
                    .as_ref()
                    .filter(|lease| lease.expires_at_unix_ms > now)
                    .map(|lease| lease.expires_at_unix_ms);
                record_database_authority_event(
                    tx,
                    &state,
                    AuthorityAuditFact {
                        operation: "acquire_writer",
                        outcome: "cursor_conflict",
                        actor: Some(actor),
                        lease_id: None,
                        reservation_id: None,
                        expected_cursor,
                    },
                )
                .await?;
                return Ok(StagedCoordinatorMutation::Mutation(
                    AcquireWriterAndReserveTurnOutcome::WriterConflict {
                        current_head,
                        active_lease_expires_at_unix_ms,
                    },
                ));
            }
            if state
                .active_writer
                .as_ref()
                .is_some_and(|lease| lease.expires_at_unix_ms > now)
            {
                let current_head = state.head.clone();
                let active_lease_expires_at_unix_ms = state
                    .active_writer
                    .as_ref()
                    .map(|lease| lease.expires_at_unix_ms);
                record_database_authority_event(
                    tx,
                    &state,
                    AuthorityAuditFact {
                        operation: "acquire_writer",
                        outcome: "writer_conflict",
                        actor: Some(actor),
                        lease_id: None,
                        reservation_id: None,
                        expected_cursor,
                    },
                )
                .await?;
                return Ok(StagedCoordinatorMutation::Mutation(
                    AcquireWriterAndReserveTurnOutcome::WriterConflict {
                        current_head,
                        active_lease_expires_at_unix_ms,
                    },
                ));
            }
            if actor.authority_epochs != state.authority_epochs {
                record_database_authority_event(
                    tx,
                    &state,
                    AuthorityAuditFact {
                        operation: "acquire_writer",
                        outcome: "stale_fenced",
                        actor: Some(actor),
                        lease_id: None,
                        reservation_id: None,
                        expected_cursor,
                    },
                )
                .await?;
                return Ok(StagedCoordinatorMutation::Rejected(
                    SessionContextCoordinatorError::Fenced,
                ));
            }
            archive_database_state_receipts(tx, &state).await?;
            state.active_reservation = None;
            state.writer_epoch = state.writer_epoch.checked_add(1).ok_or_else(|| {
                SessionContextCoordinatorError::NeedsRepair("writer epoch overflow".into())
            })?;
            let lease = ConversationWriterLeaseV1 {
                schema_version: SESSION_COORDINATION_SCHEMA_VERSION,
                key: key.clone(),
                lease_id: Uuid::new_v4().to_string(),
                writer_epoch: state.writer_epoch,
                actor: actor.clone(),
                expected_cursor: expected_cursor_owned.clone(),
                acquired_at_unix_ms: now,
                expires_at_unix_ms: checked_expiry(now, ttl)?,
                idempotency_key: writer_idempotency_key.to_owned(),
            };
            state.active_writer = Some(lease.clone());
            (lease, "acquired")
        };

        let (reservation, reserve_outcome) = if let Some(active) = state.active_reservation.clone()
            && active.idempotency_key == reservation_idempotency_key
        {
            validate_reservation_request(&active, &lease, &expected_cursor_owned)?;
            validate_active_lease(&state, &lease, now)?;
            if fence_expired_reservation_authority(&mut state, &lease, now) {
                update_database_state(tx, &state).await?;
                record_database_authority_event(
                    tx,
                    &state,
                    AuthorityAuditFact {
                        operation: "reserve_turn",
                        outcome: "expired_authority_fenced",
                        actor: Some(&lease.actor),
                        lease_id: Some(&lease.lease_id),
                        reservation_id: Some(&active.reservation_id),
                        expected_cursor,
                    },
                )
                .await?;
                return Ok(StagedCoordinatorMutation::Rejected(
                    SessionContextCoordinatorError::Expired,
                ));
            }
            let expires_at = refreshed_live_expiry(
                now,
                ttl,
                active.expires_at_unix_ms,
                Some(lease.expires_at_unix_ms),
            )?;
            let refreshed = state
                .active_reservation
                .as_mut()
                .expect("matched active turn reservation");
            refreshed.expires_at_unix_ms = expires_at;
            (refreshed.clone(), "idempotent_refreshed")
        } else {
            validate_active_lease(&state, &lease, now)?;
            if state.head.as_ref().map(|head| &head.cursor) != expected_cursor
                || state
                    .active_reservation
                    .as_ref()
                    .is_some_and(|reservation| reservation.expires_at_unix_ms > now)
            {
                let current_head = state.head.clone();
                record_database_authority_event(
                    tx,
                    &state,
                    AuthorityAuditFact {
                        operation: "reserve_turn",
                        outcome: "reservation_conflict",
                        actor: Some(&lease.actor),
                        lease_id: Some(&lease.lease_id),
                        reservation_id: None,
                        expected_cursor,
                    },
                )
                .await?;
                return Ok(StagedCoordinatorMutation::Mutation(
                    AcquireWriterAndReserveTurnOutcome::ReservationConflict {
                        lease,
                        current_head,
                    },
                ));
            }
            if let Some(previous) = &state.active_reservation {
                archive_database_reservation(tx, previous).await?;
            }
            let reservation = TurnReservationV1 {
                schema_version: SESSION_COORDINATION_SCHEMA_VERSION,
                reservation_id: Uuid::new_v4().to_string(),
                key: lease.key.clone(),
                lease_id: lease.lease_id.clone(),
                writer_epoch: lease.writer_epoch,
                expected_cursor: expected_cursor_owned,
                reserved_turn: expected_cursor
                    .map_or(1, |cursor| cursor.completed_turn.saturating_add(1)),
                created_at_unix_ms: now,
                expires_at_unix_ms: checked_expiry(now, ttl)?.min(lease.expires_at_unix_ms),
                idempotency_key: reservation_idempotency_key.to_owned(),
            };
            state.active_reservation = Some(reservation.clone());
            (reservation, "reserved")
        };

        update_database_state(tx, &state).await?;
        record_database_authority_events(
            tx,
            &state,
            &[
                AuthorityAuditFact {
                    operation: "acquire_writer",
                    outcome: acquire_outcome,
                    actor: Some(actor),
                    lease_id: Some(&lease.lease_id),
                    reservation_id: None,
                    expected_cursor,
                },
                AuthorityAuditFact {
                    operation: "reserve_turn",
                    outcome: reserve_outcome,
                    actor: Some(&lease.actor),
                    lease_id: Some(&lease.lease_id),
                    reservation_id: Some(&reservation.reservation_id),
                    expected_cursor,
                },
            ],
        )
        .await?;
        Ok(StagedCoordinatorMutation::Mutation(
            AcquireWriterAndReserveTurnOutcome::Ready { lease, reservation },
        ))
    }
}

async fn ensure_database_state(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
    epochs: AuthorityEpochsV1,
) -> Result<(), SessionContextCoordinatorError> {
    admit_session_lifecycle_write(tx, key).await?;
    ensure_database_state_admitted(tx, key, epochs).await
}

async fn ensure_database_state_admitted(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
    epochs: AuthorityEpochsV1,
) -> Result<(), SessionContextCoordinatorError> {
    sqlx::query(
        "INSERT IGNORE INTO session_context_heads
         (isolation_domain, owner_user_id, session_id, branch_id,
          authorization_epoch, device_trust_epoch, permission_epoch)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .bind(i64_from_u64(
        "authorization epoch",
        epochs.authorization_epoch,
    )?)
    .bind(i64_from_u64(
        "device trust epoch",
        epochs.device_trust_epoch,
    )?)
    .bind(i64_from_u64("permission epoch", epochs.permission_epoch)?)
    .execute(&mut **tx)
    .await
    .map_err(|source| database_error("ensure_context_head", source))?;
    Ok(())
}

/// Admit a coordinator transaction before it locks or mutates any
/// session-owned child row. Session deletion takes this same lifecycle fence
/// before deleting children, so an already-admitted writer commits before the
/// delete or observes the durable deletion state and rolls back as one unit.
async fn admit_session_lifecycle_write(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
) -> Result<(), SessionContextCoordinatorError> {
    match crate::storage::lock_agent_session_write_fence_state(
        tx,
        &key.session_id,
        &key.owner_user_id,
    )
    .await
    .map_err(|source| database_error("lock_session_lifecycle_fence", source))?
    {
        crate::storage::AgentSessionWriteFenceState::Writable => Ok(()),
        crate::storage::AgentSessionWriteFenceState::PendingDelete
        | crate::storage::AgentSessionWriteFenceState::CompletedDelete => {
            Err(SessionContextCoordinatorError::SessionLifecycleFenced)
        }
        crate::storage::AgentSessionWriteFenceState::Missing => Err(database_error(
            "lock_session_lifecycle_fence",
            sqlx::Error::Protocol(
                "session lifecycle fence disappeared before it could be locked".into(),
            ),
        )),
    }
}

/// Standalone Context writers share the Run admission lock order. A vacant
/// Controller lease does not establish that the Session has stopped executing.
async fn admit_controller_context_write(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
) -> Result<(), SessionContextCoordinatorError> {
    let facts = crate::storage::admit_context_execution_write_with_facts(
        tx,
        &key.session_id,
        &key.owner_user_id,
    )
    .await
    .map_err(|source| match source {
        sqlx::Error::RowNotFound => SessionContextCoordinatorError::SessionLifecycleFenced,
        source => database_error("admit_controller_context_write", source),
    })?;
    if let Some(blocker) = lock_session_run_execution_blocker(tx, key, &facts).await? {
        return Err(SessionContextCoordinatorError::ExecutionOccupied(blocker));
    }
    Ok(())
}

/// Fork activation touches two sessions. Acquire their shared lifecycle
/// fences in a deterministic owner/session order before locking fork or
/// manifest rows, otherwise two cross-session activations could deadlock.
pub(crate) async fn admit_session_lifecycle_writes(
    tx: &mut Transaction<'_, MySql>,
    mut keys: [&SessionKeyV1; 2],
) -> Result<(), SessionContextCoordinatorError> {
    keys.sort_unstable_by(|left, right| {
        (
            &left.owner_user_id,
            &left.session_id,
            &left.isolation_domain,
        )
            .cmp(&(
                &right.owner_user_id,
                &right.session_id,
                &right.isolation_domain,
            ))
    });
    for key in keys {
        admit_controller_context_write(tx, key).await?;
    }
    Ok(())
}

async fn lock_database_state(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
) -> Result<CoordinatorStateV1, SessionContextCoordinatorError> {
    lock_database_state_at_now(tx, key)
        .await
        .map(|(state, _)| state)
}

/// The commit receipt authorizes replay of the turn, not release of a later
/// controller. Only the writer named by this exact reservation may be cleared.
async fn release_committed_writer_in_tx(
    tx: &mut Transaction<'_, MySql>,
    state: &mut CoordinatorStateV1,
    reservation: &TurnReservationV1,
) -> Result<bool, SessionContextCoordinatorError> {
    // A retained writer may already own a later turn when an older commit is
    // replayed. Its lease alone is not authority to discard that reservation.
    if state.active_reservation.is_some() {
        return Ok(false);
    }
    let Some(lease) = state
        .active_writer
        .as_ref()
        .filter(|lease| {
            lease.lease_id == reservation.lease_id && lease.writer_epoch == reservation.writer_epoch
        })
        .cloned()
    else {
        return Ok(false);
    };
    clear_writer_authority_in_tx(tx, state).await?;
    record_database_authority_event(
        tx,
        state,
        AuthorityAuditFact {
            operation: "release_writer",
            outcome: "released",
            actor: Some(&lease.actor),
            lease_id: Some(&lease.lease_id),
            reservation_id: None,
            expected_cursor: lease.expected_cursor.as_ref(),
        },
    )
    .await?;
    Ok(true)
}

async fn clear_writer_authority_in_tx(
    tx: &mut Transaction<'_, MySql>,
    state: &mut CoordinatorStateV1,
) -> Result<(), SessionContextCoordinatorError> {
    archive_database_state_receipts(tx, state).await?;
    state.active_writer = None;
    state.active_reservation = None;
    update_database_state(tx, state).await
}

/// Inspect actual execution under lifecycle/Session admission. Writer absence
/// is not idle proof: include children, terminal executors still unwinding and
/// uncertain external effects regardless of Run status.
pub(crate) async fn lock_session_run_execution_blocker<T>(
    tx: &mut T,
    key: &SessionKeyV1,
    facts: &crate::storage::SessionExecutionAdmissionFacts,
) -> Result<Option<SessionExecutionBlocker>, SessionContextCoordinatorError>
where
    T: crate::cancellation_safe_db::TransactionConnection,
{
    if !facts.belongs_to(&key.owner_user_id, &key.session_id) {
        return Err(SessionContextCoordinatorError::Unauthorized);
    }
    let activity =
        crate::storage::session_execution_activity(tx, &key.owner_user_id, &key.session_id, None)
            .await
            .map_err(|source| database_error("read_session_execution_activity", source))?;
    Ok(if activity.unresolved_invocations {
        Some(SessionExecutionBlocker::UnresolvedTool)
    } else if activity.active_run {
        Some(SessionExecutionBlocker::ActiveRun)
    } else {
        None
    })
}

/// One owner-scoped current-read proof for cancellation and provider handoff.
/// A physical workspace is intentionally absent from this proof: independent
/// Sessions may share it, while each Session's own execution state remains
/// fenced by its Session head, Run ownership, invocation custody and binding generation.
async fn locked_execution_reuse_blocker(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
) -> Result<Option<SessionExecutionBlocker>, SessionContextCoordinatorError> {
    let facts = match crate::storage::admit_session_execution_write_with_facts(
        tx,
        &key.session_id,
        &key.owner_user_id,
    )
    .await
    {
        Ok(facts) => facts,
        Err(sqlx::Error::RowNotFound) => {
            return Ok(Some(SessionExecutionBlocker::OwnerUnavailable));
        }
        Err(source) => return Err(database_error("fence_idle_workspace_session", source)),
    };
    if let Some(blocker) = lock_session_run_execution_blocker(tx, key, &facts).await? {
        return Ok(Some(blocker));
    }
    let head_exists = sqlx::query(
        "SELECT 1 FROM session_context_heads WHERE isolation_domain = ? AND owner_user_id = ? AND session_id = ? AND branch_id = ? FOR UPDATE",
    ).bind(&key.isolation_domain).bind(&key.owner_user_id).bind(&key.session_id).bind(&key.branch_id)
        .fetch_optional(&mut **tx).await.map_err(|source| database_error("lock_idle_workspace_head", source))?.is_some();
    if head_exists {
        let (state, now) = lock_database_state_at_now(tx, key).await?;
        if state
            .active_writer
            .as_ref()
            .is_some_and(|lease| lease.expires_at_unix_ms > now)
            || state
                .active_reservation
                .as_ref()
                .is_some_and(|lease| lease.expires_at_unix_ms > now)
        {
            return Ok(Some(SessionExecutionBlocker::WriterOrReservation));
        }
    }
    if let Some(binding) = load_execution_binding_in_tx(tx, key, true).await?
        && binding.state != SessionExecutionBindingStateV1::Ready
    {
        return Ok(Some(SessionExecutionBlocker::BindingNotReady));
    }
    let unresolved = sqlx::query(
        "SELECT 1 FROM tool_invocation_ledger WHERE user_id = ? AND session_id = ?
         AND state IN ('prepared', 'dispatched', 'outcome_unknown') LIMIT 1 FOR UPDATE",
    )
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|source| database_error("lock_idle_workspace_invocations", source))?;
    if unresolved.is_some() {
        return Ok(Some(SessionExecutionBlocker::UnresolvedTool));
    }
    Ok(None)
}

async fn lock_database_state_at_now(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
) -> Result<(CoordinatorStateV1, i64), SessionContextCoordinatorError> {
    let row = sqlx::query(
        "SELECT head_json, writer_epoch, authorization_epoch, device_trust_epoch,
                permission_epoch, active_writer_json, active_reservation_json,
                last_commit_json, fork_base_json,
                CAST(UNIX_TIMESTAMP(NOW(6)) * 1000 AS SIGNED) AS database_now_unix_ms
         FROM session_context_heads
         WHERE isolation_domain = ? AND owner_user_id = ?
           AND session_id = ? AND branch_id = ?
         FOR UPDATE",
    )
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|source| database_error("lock_context_head", source))?
    .ok_or_else(|| {
        SessionContextCoordinatorError::NeedsRepair(
            "context head state is missing during mutation".into(),
        )
    })?;
    let optional_json = |column: &'static str| -> Result<Option<String>, _> {
        row.try_get::<Option<String>, _>(column)
    };
    let mut state = CoordinatorStateV1::new(key.clone());
    state.writer_epoch = database_u64(&row, "writer_epoch")?;
    state.authority_epochs = AuthorityEpochsV1 {
        authorization_epoch: database_u64(&row, "authorization_epoch")?,
        device_trust_epoch: database_u64(&row, "device_trust_epoch")?,
        permission_epoch: database_u64(&row, "permission_epoch")?,
    };
    state.head = optional_json("head_json")
        .map_err(|source| database_error("decode_head_json", source))?
        .as_deref()
        .map(|json| database_json("head", json))
        .transpose()?;
    state.active_writer = optional_json("active_writer_json")
        .map_err(|source| database_error("decode_writer_json", source))?
        .as_deref()
        .map(|json| database_json("writer_lease", json))
        .transpose()?;
    state.active_reservation = optional_json("active_reservation_json")
        .map_err(|source| database_error("decode_reservation_json", source))?
        .as_deref()
        .map(|json| database_json("turn_reservation", json))
        .transpose()?;
    state.last_commit = optional_json("last_commit_json")
        .map_err(|source| database_error("decode_commit_json", source))?
        .as_deref()
        .map(|json| database_json("commit_receipt", json))
        .transpose()?;
    state.fork_base = optional_json("fork_base_json")
        .map_err(|source| database_error("decode_fork_base_json", source))?
        .as_deref()
        .map(|json| database_json("fork_base", json))
        .transpose()?;
    state.validate_for(key)?;
    let now = row
        .try_get::<i64, _>("database_now_unix_ms")
        .map_err(|source| database_error("decode_locked_database_time", source))?;
    Ok((state, now))
}

/// Facts needed by a Work recovery capture while the canonical Session row is
/// locked.  Recovery publication must use this seam instead of calling the
/// ordinary read-only `load_head`: the latter opens its own pool connection
/// and would leave a race between verification and the recovery-row update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecoveryContextFactsV1 {
    pub(crate) head: SessionContextHeadV1,
    pub(crate) execution_binding: Option<SessionExecutionBindingV1>,
    pub(crate) execution_blocker: Option<SessionExecutionBlocker>,
    pub(crate) has_active_reservation: bool,
    pub(crate) has_unresolved_invocations: bool,
}

pub(crate) async fn lock_recovery_context_in_transaction(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
    admission: &crate::storage::SessionExecutionAdmissionFacts,
) -> Result<RecoveryContextFactsV1, SessionContextCoordinatorError> {
    key.validate()
        .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
    let execution_blocker = lock_session_run_execution_blocker(tx, key, admission).await?;
    let (state, now) = lock_database_state_at_now(tx, key).await?;
    let head = state.head.ok_or_else(|| {
        SessionContextCoordinatorError::NeedsRepair(
            "context head is missing for recovery capture".into(),
        )
    })?;
    let has_active_reservation = state
        .active_reservation
        .as_ref()
        .is_some_and(|reservation| reservation.expires_at_unix_ms > now);
    let has_unresolved_invocations = unresolved_session_invocation_exists(tx, key).await?;
    let execution_binding = load_execution_binding_in_tx(tx, key, true).await?;
    Ok(RecoveryContextFactsV1 {
        head,
        execution_blocker,
        execution_binding,
        has_active_reservation,
        has_unresolved_invocations,
    })
}

async fn load_execution_binding_in_tx(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
    for_update: bool,
) -> Result<Option<SessionExecutionBindingV1>, SessionContextCoordinatorError> {
    let row = if for_update {
        sqlx::query(
            "SELECT generation, binding_json FROM session_execution_bindings \
             WHERE isolation_domain = ? AND owner_user_id = ? \
               AND session_id = ? AND branch_id = ? FOR UPDATE",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .fetch_optional(&mut **tx)
        .await
    } else {
        sqlx::query(
            "SELECT generation, binding_json FROM session_execution_bindings \
             WHERE isolation_domain = ? AND owner_user_id = ? \
               AND session_id = ? AND branch_id = ?",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .fetch_optional(&mut **tx)
        .await
    }
    .map_err(|source| database_error("load_execution_binding_in_tx", source))?;
    row.as_ref().map(decode_execution_binding_row).transpose()
}

/// Verify the exact attachment requested by a new switch while the canonical
/// Session head is locked. The read-only handler check is useful for a fast
/// error, but this check is the authority boundary and closes the race where
/// an attachment is detached or expires between the HTTP read and the CAS.
async fn require_controller_attachment_in_tx(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
    attachment_id: &str,
    now_unix_ms: i64,
) -> Result<SessionAttachmentV1, SessionContextCoordinatorError> {
    validate_idempotency_key(attachment_id)?;
    let row = sqlx::query(
        "SELECT attachment_json FROM session_attachments
         WHERE isolation_domain = ? AND owner_user_id = ?
           AND session_id = ? AND branch_id = ? AND attachment_id = ?
         FOR UPDATE",
    )
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .bind(attachment_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|source| database_error("lock_execution_switch_controller", source))?
    .ok_or(SessionContextCoordinatorError::Unauthorized)?;
    let attachment: SessionAttachmentV1 = database_json(
        "session_attachment",
        &row.try_get::<String, _>("attachment_json")
            .map_err(|source| database_error("decode_execution_switch_controller", source))?,
    )?;
    attachment
        .validate()
        .map_err(|error| SessionContextCoordinatorError::NeedsRepair(error.to_string()))?;
    if attachment.key != *key {
        return Err(SessionContextCoordinatorError::NeedsRepair(
            "controller attachment SessionKey mismatch".into(),
        ));
    }
    if attachment.mode != SessionAttachmentModeV1::Controller
        || attachment.expires_at_unix_ms <= now_unix_ms
    {
        return Err(SessionContextCoordinatorError::Unauthorized);
    }
    Ok(attachment)
}

/// A retry or completion may be performed by a newly acquired controller
/// after the original device disappeared. It therefore checks for any single
/// active controller rather than trusting the attachment id from the first
/// attempt.
async fn require_active_controller_attachment_in_tx(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
    now_unix_ms: i64,
) -> Result<SessionAttachmentV1, SessionContextCoordinatorError> {
    let rows = sqlx::query(
        "SELECT attachment_json FROM session_attachments
         WHERE isolation_domain = ? AND owner_user_id = ?
           AND session_id = ? AND branch_id = ?
           AND mode = 'controller' AND expires_at_ms > ?
         ORDER BY attachment_epoch DESC
         LIMIT 2
         FOR UPDATE",
    )
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .bind(now_unix_ms)
    .fetch_all(&mut **tx)
    .await
    .map_err(|source| database_error("lock_execution_switch_active_controller", source))?;
    if rows.len() > 1 {
        return Err(SessionContextCoordinatorError::NeedsRepair(
            "multiple active controller attachments exist".into(),
        ));
    }
    let row = rows
        .into_iter()
        .next()
        .ok_or(SessionContextCoordinatorError::Unauthorized)?;
    let attachment: SessionAttachmentV1 = database_json(
        "session_attachment",
        &row.try_get::<String, _>("attachment_json")
            .map_err(|source| {
                database_error("decode_execution_switch_active_controller", source)
            })?,
    )?;
    attachment
        .validate()
        .map_err(|error| SessionContextCoordinatorError::NeedsRepair(error.to_string()))?;
    if attachment.key != *key || attachment.mode != SessionAttachmentModeV1::Controller {
        return Err(SessionContextCoordinatorError::NeedsRepair(
            "stored controller attachment is inconsistent".into(),
        ));
    }
    Ok(attachment)
}

fn execution_switch_request_hash(
    key: &SessionKeyV1,
    request: &BeginSessionExecutionSwitchV1,
) -> Result<String, SessionContextCoordinatorError> {
    let canonical = database_to_json(
        "execution_switch_request",
        &(
            key,
            &request.request_id,
            &request.controller_attachment_id,
            request.expected_generation,
            &request.target,
            &request.source_evidence,
        ),
    )?;
    Ok(format!("{:x}", Sha256::digest(canonical.as_bytes())))
}

fn validate_execution_switch_receipt(
    receipt: &SessionExecutionSwitchReceiptV1,
    key: &SessionKeyV1,
) -> Result<(), SessionContextCoordinatorError> {
    if receipt.schema_version != SESSION_EXECUTION_SWITCH_SCHEMA_VERSION {
        return Err(SessionContextCoordinatorError::NeedsRepair(
            "unsupported Session execution-switch receipt schema".into(),
        ));
    }
    if receipt.key != *key {
        return Err(SessionContextCoordinatorError::NeedsRepair(
            "execution-switch receipt belongs to another Session".into(),
        ));
    }
    validate_idempotency_key(&receipt.operation_id)?;
    validate_idempotency_key(&receipt.request_id)?;
    validate_idempotency_key(&receipt.controller_attachment_id)?;
    if receipt.expected_generation == 0
        || receipt.attempt_expected_generation == 0
        || receipt.switching_generation != receipt.attempt_expected_generation.saturating_add(1)
        || receipt.attempt == 0
    {
        return Err(SessionContextCoordinatorError::NeedsRepair(
            "execution-switch receipt generations are invalid".into(),
        ));
    }
    receipt.source.validate()?;
    receipt.target.validate()?;
    validate_execution_attestation_evidence(&receipt.source_evidence)?;
    if receipt.source.logical_workspace_id != receipt.target.logical_workspace_id {
        return Err(SessionContextCoordinatorError::NeedsRepair(
            "execution-switch receipt changes logical workspace".into(),
        ));
    }
    if receipt.state == SessionExecutionSwitchStateV1::Switching
        && receipt.completed_generation.is_some()
    {
        return Err(SessionContextCoordinatorError::NeedsRepair(
            "switching receipt cannot have a completed generation".into(),
        ));
    }
    if receipt.state != SessionExecutionSwitchStateV1::Switching
        && receipt.completed_generation.is_none()
    {
        return Err(SessionContextCoordinatorError::NeedsRepair(
            "terminal execution-switch receipt is missing completed generation".into(),
        ));
    }
    Ok(())
}

fn validate_execution_attestation_evidence(
    evidence: &Value,
) -> Result<(), SessionContextCoordinatorError> {
    let object = evidence.as_object().ok_or_else(|| {
        SessionContextCoordinatorError::Invalid(
            "execution attestation evidence must be a JSON object".into(),
        )
    })?;
    let schema_version = object
        .get("schema_version")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            SessionContextCoordinatorError::Invalid(
                "execution attestation evidence schema_version is missing".into(),
            )
        })?;
    if schema_version != 1 {
        return Err(SessionContextCoordinatorError::Invalid(
            "unsupported execution attestation evidence schema".into(),
        ));
    }
    for field in [
        "root",
        "head",
        "tree",
        "object_format",
        "reference",
        "repository",
    ] {
        if object
            .get(field)
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            return Err(SessionContextCoordinatorError::Invalid(format!(
                "execution attestation evidence {field} is missing"
            )));
        }
    }
    if object.get("clean").and_then(Value::as_bool) != Some(true) {
        return Err(SessionContextCoordinatorError::Invalid(
            "execution attestation evidence must prove a clean workspace".into(),
        ));
    }
    Ok(())
}

async fn load_execution_switch_in_tx(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
    operation_id: &str,
    for_update: bool,
) -> Result<Option<SessionExecutionSwitchReceiptV1>, SessionContextCoordinatorError> {
    let base = "SELECT record_json FROM session_execution_switches
                WHERE isolation_domain = ? AND owner_user_id = ?
                  AND session_id = ? AND branch_id = ? AND operation_id = ?";
    let sql = if for_update {
        format!("{base} FOR UPDATE")
    } else {
        base.to_string()
    };
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .bind(operation_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|source| database_error("load_execution_switch_in_tx", source))?;
    row.map(|row| {
        let record = row
            .try_get::<String, _>("record_json")
            .map_err(|source| database_error("decode_execution_switch_record", source))?;
        let receipt: SessionExecutionSwitchReceiptV1 =
            database_json("session_execution_switch", &record)?;
        validate_execution_switch_receipt(&receipt, key)?;
        Ok(receipt)
    })
    .transpose()
}

async fn load_execution_switch_by_request_in_tx(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
    request_id: &str,
    for_update: bool,
) -> Result<Option<SessionExecutionSwitchReceiptV1>, SessionContextCoordinatorError> {
    let base = "SELECT record_json FROM session_execution_switches
                WHERE isolation_domain = ? AND owner_user_id = ?
                  AND session_id = ? AND branch_id = ? AND request_id = ?";
    let sql = if for_update {
        format!("{base} FOR UPDATE")
    } else {
        base.to_string()
    };
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .bind(request_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|source| database_error("load_execution_switch_by_request", source))?;
    row.map(|row| {
        let record = row
            .try_get::<String, _>("record_json")
            .map_err(|source| database_error("decode_execution_switch_record", source))?;
        let receipt: SessionExecutionSwitchReceiptV1 =
            database_json("session_execution_switch", &record)?;
        validate_execution_switch_receipt(&receipt, key)?;
        Ok(receipt)
    })
    .transpose()
}

async fn insert_execution_switch_in_tx(
    tx: &mut Transaction<'_, MySql>,
    receipt: &SessionExecutionSwitchReceiptV1,
) -> Result<(), SessionContextCoordinatorError> {
    let record = database_to_json("session_execution_switch", receipt)?;
    sqlx::query(
        "INSERT INTO session_execution_switches
         (isolation_domain, owner_user_id, session_id, branch_id, operation_id,
          request_id, request_hash, state, expected_generation, switching_generation,
          completed_generation, record_json, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6), NOW(6))",
    )
    .bind(&receipt.key.isolation_domain)
    .bind(&receipt.key.owner_user_id)
    .bind(&receipt.key.session_id)
    .bind(&receipt.key.branch_id)
    .bind(&receipt.operation_id)
    .bind(&receipt.request_id)
    .bind(&receipt.request_hash)
    .bind(match receipt.state {
        SessionExecutionSwitchStateV1::Switching => "switching",
        SessionExecutionSwitchStateV1::Succeeded => "succeeded",
        SessionExecutionSwitchStateV1::Failed => "failed",
    })
    .bind(i64_from_u64(
        "execution switch expected generation",
        receipt.expected_generation,
    )?)
    .bind(i64_from_u64(
        "execution switch switching generation",
        receipt.switching_generation,
    )?)
    .bind(
        receipt
            .completed_generation
            .map(|generation| i64_from_u64("execution switch completed generation", generation))
            .transpose()?,
    )
    .bind(record)
    .execute(&mut **tx)
    .await
    .map_err(|source| database_error("insert_execution_switch", source))?;
    Ok(())
}

async fn update_execution_switch_in_tx(
    tx: &mut Transaction<'_, MySql>,
    receipt: &SessionExecutionSwitchReceiptV1,
) -> Result<(), SessionContextCoordinatorError> {
    let record = database_to_json("session_execution_switch", receipt)?;
    let updated = sqlx::query(
        "UPDATE session_execution_switches SET state = ?, completed_generation = ?,
                record_json = ?, updated_at = NOW(6)
         WHERE isolation_domain = ? AND owner_user_id = ? AND session_id = ?
           AND branch_id = ? AND operation_id = ?",
    )
    .bind(match receipt.state {
        SessionExecutionSwitchStateV1::Switching => "switching",
        SessionExecutionSwitchStateV1::Succeeded => "succeeded",
        SessionExecutionSwitchStateV1::Failed => "failed",
    })
    .bind(
        receipt
            .completed_generation
            .map(|generation| i64_from_u64("execution switch completed generation", generation))
            .transpose()?,
    )
    .bind(record)
    .bind(&receipt.key.isolation_domain)
    .bind(&receipt.key.owner_user_id)
    .bind(&receipt.key.session_id)
    .bind(&receipt.key.branch_id)
    .bind(&receipt.operation_id)
    .execute(&mut **tx)
    .await
    .map_err(|source| database_error("update_execution_switch", source))?
    .rows_affected();
    if updated != 1 {
        return Err(SessionContextCoordinatorError::NeedsRepair(
            "execution switch receipt disappeared during update".into(),
        ));
    }
    Ok(())
}

async fn update_execution_binding_in_tx(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
    expected_generation: u64,
    next: &SessionExecutionBindingV1,
) -> Result<(), SessionContextCoordinatorError> {
    let binding_json = database_to_json("session_execution_binding", next)?;
    let updated = sqlx::query(
        "UPDATE session_execution_bindings
         SET generation = ?, binding_json = ?, updated_at = NOW(6)
         WHERE isolation_domain = ? AND owner_user_id = ?
           AND session_id = ? AND branch_id = ? AND generation = ?",
    )
    .bind(i64_from_u64(
        "execution binding generation",
        next.generation,
    )?)
    .bind(binding_json)
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .bind(i64_from_u64(
        "expected execution binding generation",
        expected_generation,
    )?)
    .execute(&mut **tx)
    .await
    .map_err(|source| database_error("update_execution_binding_in_tx", source))?
    .rows_affected();
    if updated != 1 {
        return Err(SessionContextCoordinatorError::ExecutionBindingFenced {
            expected: expected_generation,
            current: Some(next.generation),
        });
    }
    Ok(())
}

/// Run creation and non-root child admission must still respect the current
/// Session binding state. The physical directory is not part of this check:
/// executor-owned workspace leases coordinate operations after admission.
pub(crate) async fn ensure_run_execution_binding_ready_in_tx(
    tx: &mut Transaction<'_, MySql>,
    user_id: &str,
    session_id: &str,
) -> Result<(), SessionContextCoordinatorError> {
    let key = SessionKeyV1::owner_session(
        "server",
        user_id,
        session_id,
        astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID,
    );
    if let Some(binding) = load_execution_binding_in_tx(tx, &key, true).await?
        && binding.state != SessionExecutionBindingStateV1::Ready
    {
        return Err(SessionContextCoordinatorError::ExecutionBindingNotReady(
            binding.state,
        ));
    }
    Ok(())
}

fn decode_execution_binding_row(
    row: &sqlx::mysql::MySqlRow,
) -> Result<SessionExecutionBindingV1, SessionContextCoordinatorError> {
    let generation = row
        .try_get::<i64, _>("generation")
        .map_err(|source| database_error("decode_execution_binding_generation", source))?;
    let generation = u64::try_from(generation).map_err(|_| {
        SessionContextCoordinatorError::NeedsRepair(
            "stored Session execution-binding generation is not positive".into(),
        )
    })?;
    let binding_json = row
        .try_get::<String, _>("binding_json")
        .map_err(|source| database_error("decode_execution_binding_json", source))?;
    let binding: SessionExecutionBindingV1 =
        database_json("session_execution_binding", &binding_json)?;
    binding.validate()?;
    if generation != binding.generation {
        return Err(SessionContextCoordinatorError::NeedsRepair(
            "Session execution-binding generation disagrees with its payload".into(),
        ));
    }
    Ok(binding)
}

async fn validate_execution_binding_generation_in_tx(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
    expected_generation: Option<u64>,
) -> Result<(), SessionContextCoordinatorError> {
    let Some(expected_generation) = expected_generation else {
        return Ok(());
    };
    if expected_generation == NO_EXECUTION_BINDING_EXPECTATION {
        return validate_no_execution_binding_in_tx(tx, key).await;
    }
    let current = load_execution_binding_in_tx(tx, key, true).await?;
    let Some(current) = current else {
        return Err(SessionContextCoordinatorError::ExecutionBindingFenced {
            expected: expected_generation,
            current: None,
        });
    };
    if current.generation != expected_generation {
        return Err(SessionContextCoordinatorError::ExecutionBindingFenced {
            expected: expected_generation,
            current: Some(current.generation),
        });
    }
    if current.state != SessionExecutionBindingStateV1::Ready {
        return Err(SessionContextCoordinatorError::ExecutionBindingNotReady(
            current.state,
        ));
    }
    Ok(())
}

/// Atomically assert that no native Session execution binding exists. The
/// caller already holds the canonical Session-head lock, so this read and the
/// writer/reservation installation share one transaction and close the race
/// with native binding initialization.
async fn validate_no_execution_binding_in_tx(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
) -> Result<(), SessionContextCoordinatorError> {
    if let Some(current) = load_execution_binding_in_tx(tx, key, true).await? {
        return Err(SessionContextCoordinatorError::ExecutionBindingPresent {
            generation: current.generation,
        });
    }
    Ok(())
}

async fn unresolved_session_invocation_exists(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
) -> Result<bool, SessionContextCoordinatorError> {
    let row = sqlx::query(
        "SELECT 1 FROM tool_invocation_ledger \
         WHERE user_id = ? AND session_id = ? \
           AND state IN ('prepared', 'dispatched', 'outcome_unknown') \
         LIMIT 1",
    )
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|source| database_error("check_unresolved_session_invocations", source))?;
    Ok(row.is_some())
}

async fn update_database_state(
    tx: &mut Transaction<'_, MySql>,
    state: &CoordinatorStateV1,
) -> Result<(), SessionContextCoordinatorError> {
    let head_json = state
        .head
        .as_ref()
        .map(|head| database_to_json("head", head))
        .transpose()?;
    let active_writer_json = state
        .active_writer
        .as_ref()
        .map(|lease| database_to_json("writer_lease", lease))
        .transpose()?;
    let active_reservation_json = state
        .active_reservation
        .as_ref()
        .map(|reservation| database_to_json("turn_reservation", reservation))
        .transpose()?;
    let last_commit_json = state
        .last_commit
        .as_ref()
        .map(|receipt| database_to_json("commit_receipt", receipt))
        .transpose()?;
    let fork_base_json = state
        .fork_base
        .as_ref()
        .map(|prefix| database_to_json("fork_base", prefix))
        .transpose()?;
    let (canonical_root, manifest_root, completed_turn, journal_event_seq, conversation_seq) =
        if let Some(head) = &state.head {
            (
                Some(head.cursor.canonical_root_hash.as_str()),
                Some(head.latest_manifest_root.as_str()),
                i64::from(head.cursor.completed_turn),
                i64_from_u64("journal event sequence", head.cursor.journal_event_seq)?,
                i64_from_u64("conversation sequence", head.cursor.conversation_seq)?,
            )
        } else {
            (None, None, 0_i64, 0_i64, 0_i64)
        };
    let projection_schema = state
        .head
        .as_ref()
        .map_or(0_i64, |head| i64::from(head.cursor.projection_schema));
    let compaction_generation = state.head.as_ref().map_or(Ok(0_i64), |head| {
        i64_from_u64("compaction generation", head.cursor.compaction_generation)
    })?;
    let update_sql = matrixone_statement_with_null_shape(
        "UPDATE session_context_heads
         SET head_json = ?, canonical_root_hash = ?, latest_manifest_root = ?,
             total_canonical_bytes = ?, total_message_count = ?,
             completed_turn = ?, journal_event_seq = ?, conversation_seq = ?,
             projection_schema = ?, compaction_generation = ?, writer_epoch = ?,
             authorization_epoch = ?, device_trust_epoch = ?, permission_epoch = ?,
             active_writer_json = ?, active_writer_expires_at_ms = ?,
             active_reservation_json = ?, active_reservation_expires_at_ms = ?,
             last_commit_json = ?, fork_base_json = ?, updated_at = NOW(6)
         WHERE isolation_domain = ? AND owner_user_id = ?
           AND session_id = ? AND branch_id = ?",
        [
            head_json.is_some(),
            canonical_root.is_some(),
            manifest_root.is_some(),
            active_writer_json.is_some(),
            state.active_writer.is_some(),
            active_reservation_json.is_some(),
            state.active_reservation.is_some(),
            last_commit_json.is_some(),
            fork_base_json.is_some(),
        ],
    );
    let result = sqlx::query(update_sql)
        .bind(head_json)
        .bind(canonical_root)
        .bind(manifest_root)
        .bind(state.head.as_ref().map_or(Ok(0_i64), |head| {
            i64_from_u64("total canonical bytes", head.total_canonical_bytes)
        })?)
        .bind(state.head.as_ref().map_or(Ok(0_i64), |head| {
            i64_from_u64("total message count", head.total_message_count)
        })?)
        .bind(completed_turn)
        .bind(journal_event_seq)
        .bind(conversation_seq)
        .bind(projection_schema)
        .bind(compaction_generation)
        .bind(i64_from_u64("writer epoch", state.writer_epoch)?)
        .bind(i64_from_u64(
            "authorization epoch",
            state.authority_epochs.authorization_epoch,
        )?)
        .bind(i64_from_u64(
            "device trust epoch",
            state.authority_epochs.device_trust_epoch,
        )?)
        .bind(i64_from_u64(
            "permission epoch",
            state.authority_epochs.permission_epoch,
        )?)
        .bind(active_writer_json)
        .bind(
            state
                .active_writer
                .as_ref()
                .map(|lease| lease.expires_at_unix_ms),
        )
        .bind(active_reservation_json)
        .bind(
            state
                .active_reservation
                .as_ref()
                .map(|reservation| reservation.expires_at_unix_ms),
        )
        .bind(last_commit_json)
        .bind(fork_base_json)
        .bind(&state.key.isolation_domain)
        .bind(&state.key.owner_user_id)
        .bind(&state.key.session_id)
        .bind(&state.key.branch_id)
        .execute(&mut **tx)
        .await
        .map_err(|source| database_error("update_context_head", source))?;
    if result.rows_affected() != 1 {
        return Err(SessionContextCoordinatorError::NeedsRepair(
            "context head CAS row disappeared".into(),
        ));
    }
    Ok(())
}

async fn database_now_ms(
    tx: &mut Transaction<'_, MySql>,
) -> Result<i64, SessionContextCoordinatorError> {
    crate::db_row::database_now_unix_ms(tx)
        .await
        .map_err(|source| database_error("load_database_time", source))
}

#[derive(Clone, Copy)]
struct AuthorityAuditFact<'a> {
    operation: &'static str,
    outcome: &'static str,
    actor: Option<&'a ActorContextV1>,
    lease_id: Option<&'a str>,
    reservation_id: Option<&'a str>,
    expected_cursor: Option<&'a SessionCursorV1>,
}

fn authority_error_outcome(error: &SessionContextCoordinatorError) -> &'static str {
    match error {
        SessionContextCoordinatorError::Fenced => "stale_fenced",
        SessionContextCoordinatorError::SessionLifecycleFenced => "session_lifecycle_fenced",
        SessionContextCoordinatorError::Expired => "expired",
        SessionContextCoordinatorError::IdempotencyMismatch => "idempotency_mismatch",
        SessionContextCoordinatorError::Unauthorized => "unauthorized",
        SessionContextCoordinatorError::NeedsRepair(_) => "needs_repair",
        SessionContextCoordinatorError::ExecutionBindingPresent { .. } => {
            "execution_binding_present"
        }
        _ => "rejected",
    }
}

async fn record_database_authority_event(
    tx: &mut Transaction<'_, MySql>,
    state: &CoordinatorStateV1,
    fact: AuthorityAuditFact<'_>,
) -> Result<(), SessionContextCoordinatorError> {
    record_database_authority_events(tx, state, &[fact]).await
}

async fn record_database_authority_events(
    tx: &mut Transaction<'_, MySql>,
    state: &CoordinatorStateV1,
    facts: &[AuthorityAuditFact<'_>],
) -> Result<(), SessionContextCoordinatorError> {
    if facts.is_empty() {
        return Ok(());
    }
    let writer_epoch = i64_from_u64("audit writer epoch", state.writer_epoch)?;
    let authorization_epoch = i64_from_u64(
        "audit authorization epoch",
        state.authority_epochs.authorization_epoch,
    )?;
    let device_trust_epoch = i64_from_u64(
        "audit device trust epoch",
        state.authority_epochs.device_trust_epoch,
    )?;
    let permission_epoch = i64_from_u64(
        "audit permission epoch",
        state.authority_epochs.permission_epoch,
    )?;
    let observed_root = state
        .head
        .as_ref()
        .map(|head| head.cursor.canonical_root_hash.as_str());
    let mut insert = QueryBuilder::<MySql>::new(
        "INSERT INTO session_context_authority_events
         (isolation_domain, owner_user_id, event_id, session_id, branch_id,
          operation_kind, outcome, writer_epoch, actor_id, device_id, lease_id,
          reservation_id, expected_root, observed_root, authorization_epoch,
          device_trust_epoch, permission_epoch)
         ",
    );
    insert.push_values(facts, |mut row, fact| {
        let actor = fact
            .actor
            .or_else(|| state.active_writer.as_ref().map(|lease| &lease.actor));
        row.push_bind(&state.key.isolation_domain)
            .push_bind(&state.key.owner_user_id)
            .push_bind(Uuid::new_v4().to_string())
            .push_bind(&state.key.session_id)
            .push_bind(&state.key.branch_id)
            .push_bind(fact.operation)
            .push_bind(fact.outcome)
            .push_bind(writer_epoch)
            .push_bind(actor.map(|actor| actor.actor_id.as_str()))
            .push_bind(actor.and_then(|actor| actor.device_id.as_deref()))
            .push_bind(fact.lease_id)
            .push_bind(fact.reservation_id)
            .push_bind(
                fact.expected_cursor
                    .map(|cursor| cursor.canonical_root_hash.as_str()),
            )
            .push_bind(observed_root)
            .push_bind(authorization_epoch)
            .push_bind(device_trust_epoch)
            .push_bind(permission_epoch);
    });
    insert
        .build()
        .execute(&mut **tx)
        .await
        .map_err(|source| database_error("record_authority_event", source))?;
    Ok(())
}

async fn load_database_receipt<T: DeserializeOwned>(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
    operation: &'static str,
    idempotency_key: &str,
    request_hash: &str,
) -> Result<Option<T>, SessionContextCoordinatorError> {
    let row = sqlx::query(
        "SELECT request_hash, receipt_json FROM session_context_operation_receipts
         WHERE isolation_domain = ? AND owner_user_id = ?
           AND session_id = ? AND branch_id = ?
           AND operation_kind = ? AND idempotency_hash = ?",
    )
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .bind(operation)
    .bind(hash_receipt(operation, idempotency_key))
    .fetch_optional(&mut **tx)
    .await
    .map_err(|source| database_error("load_operation_receipt", source))?;
    decode_database_receipt(row, request_hash)
}

fn decode_database_receipt<T: DeserializeOwned>(
    row: Option<sqlx::mysql::MySqlRow>,
    request_hash: &str,
) -> Result<Option<T>, SessionContextCoordinatorError> {
    let Some(row) = row else {
        return Ok(None);
    };
    let stored_request_hash = row
        .try_get::<String, _>("request_hash")
        .map_err(|source| database_error("decode_receipt_hash", source))?;
    if stored_request_hash != request_hash {
        return Err(SessionContextCoordinatorError::IdempotencyMismatch);
    }
    let receipt_json = row
        .try_get::<String, _>("receipt_json")
        .map_err(|source| database_error("decode_receipt_json", source))?;
    database_json("operation_receipt", &receipt_json).map(Some)
}

async fn store_database_receipt<T: Serialize>(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
    operation: &'static str,
    idempotency_key: &str,
    request_hash: &str,
    receipt: &T,
) -> Result<(), SessionContextCoordinatorError> {
    sqlx::query(
        "INSERT IGNORE INTO session_context_operation_receipts
         (isolation_domain, owner_user_id, session_id, branch_id, operation_kind,
          idempotency_hash, request_hash, receipt_json)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .bind(operation)
    .bind(hash_receipt(operation, idempotency_key))
    .bind(request_hash)
    .bind(database_to_json("operation_receipt", receipt)?)
    .execute(&mut **tx)
    .await
    .map_err(|source| database_error("store_operation_receipt", source))?;
    Ok(())
}

async fn archive_database_state_receipts(
    tx: &mut Transaction<'_, MySql>,
    state: &CoordinatorStateV1,
) -> Result<(), SessionContextCoordinatorError> {
    if let Some(lease) = &state.active_writer {
        store_database_receipt(
            tx,
            &state.key,
            "acquire",
            &lease.idempotency_key,
            &lease_request_hash(&state.key, lease.expected_cursor.as_ref(), &lease.actor),
            &LeaseReceiptV1 {
                lease: lease.clone(),
            },
        )
        .await?;
    }
    if let Some(reservation) = &state.active_reservation {
        archive_database_reservation(tx, reservation).await?;
    }
    Ok(())
}

async fn archive_database_reservation(
    tx: &mut Transaction<'_, MySql>,
    reservation: &TurnReservationV1,
) -> Result<(), SessionContextCoordinatorError> {
    store_database_receipt(
        tx,
        &reservation.key,
        "reserve",
        &reservation.idempotency_key,
        &reservation_identity_hash(
            &reservation.key,
            &reservation.lease_id,
            reservation.writer_epoch,
            reservation.expected_cursor.as_ref(),
        ),
        &ReservationReceiptV1 {
            reservation: reservation.clone(),
        },
    )
    .await
}

async fn archive_database_commit(
    tx: &mut Transaction<'_, MySql>,
    receipt: &CommitReceiptV1,
) -> Result<(), SessionContextCoordinatorError> {
    store_database_receipt(
        tx,
        &receipt.reservation.key,
        "commit",
        &receipt.idempotency_key,
        &commit_receipt_request_hash(receipt),
        receipt,
    )
    .await
}

fn database_error(operation: &'static str, source: sqlx::Error) -> SessionContextCoordinatorError {
    SessionContextCoordinatorError::Database { operation, source }
}

fn validate_prepared_fork(
    manifest: &SessionForkManifestV1,
) -> Result<(), SessionContextCoordinatorError> {
    manifest
        .validate()
        .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
    if manifest.state != SessionForkStateV1::Prepared {
        return Err(SessionContextCoordinatorError::Invalid(
            "fork activation requires a prepared manifest".into(),
        ));
    }
    Ok(())
}

fn fork_child_head(manifest: &SessionForkManifestV1, writer_epoch: u64) -> SessionContextHeadV1 {
    let mut cursor = manifest.parent_head.cursor.clone();
    cursor.owner_id = manifest.child_key.owner_user_id.clone();
    cursor.session_id = manifest.child_key.session_id.clone();
    cursor.branch_id = manifest.child_key.branch_id.clone();
    SessionContextHeadV1 {
        schema_version: SESSION_COORDINATION_SCHEMA_VERSION,
        key: manifest.child_key.clone(),
        cursor,
        latest_manifest_root: manifest.parent_head.latest_manifest_root.clone(),
        total_canonical_bytes: manifest.parent_head.total_canonical_bytes,
        total_message_count: manifest.parent_head.total_message_count,
        writer_epoch,
    }
}

async fn insert_fork_event(
    tx: &mut Transaction<'_, MySql>,
    manifest: &SessionForkManifestV1,
    transition_seq: u64,
    from_state: &str,
    to_state: &str,
) -> Result<(), SessionContextCoordinatorError> {
    sqlx::query(
        "INSERT IGNORE INTO session_fork_events
         (isolation_domain, owner_user_id, fork_id, transition_seq,
          parent_session_id, child_session_id, from_state, to_state, event_json)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&manifest.child_key.isolation_domain)
    .bind(&manifest.child_key.owner_user_id)
    .bind(&manifest.fork_id)
    .bind(i64_from_u64("fork transition sequence", transition_seq)?)
    .bind(&manifest.parent_key.session_id)
    .bind(&manifest.child_key.session_id)
    .bind(from_state)
    .bind(to_state)
    .bind(database_to_json("fork_event", manifest)?)
    .execute(&mut **tx)
    .await
    .map_err(|source| database_error("insert_fork_event", source))?;
    Ok(())
}

fn database_json<T: DeserializeOwned>(
    entity: &'static str,
    json: &str,
) -> Result<T, SessionContextCoordinatorError> {
    serde_json::from_str(json)
        .map_err(|source| SessionContextCoordinatorError::DatabaseJson { entity, source })
}

fn database_to_json<T: Serialize>(
    entity: &'static str,
    value: &T,
) -> Result<String, SessionContextCoordinatorError> {
    serde_json::to_string(value)
        .map_err(|source| SessionContextCoordinatorError::DatabaseJson { entity, source })
}

fn database_u64(
    row: &sqlx::mysql::MySqlRow,
    column: &'static str,
) -> Result<u64, SessionContextCoordinatorError> {
    let value = row
        .try_get::<i64, _>(column)
        .map_err(|source| database_error("decode_context_head_integer", source))?;
    u64::try_from(value)
        .map_err(|_| SessionContextCoordinatorError::NeedsRepair(format!("{column} is negative")))
}

fn i64_from_u64(field: &'static str, value: u64) -> Result<i64, SessionContextCoordinatorError> {
    i64::try_from(value)
        .map_err(|_| SessionContextCoordinatorError::Invalid(format!("{field} exceeds BIGINT")))
}

fn manifest_node_for_delta(
    key: &SessionKeyV1,
    parent: Option<String>,
    delta: &CanonicalTurnDeltaV1,
    segments: &[ConversationSegmentV1],
) -> Result<ContextManifestNodeV1, SessionCoordinationValidationError> {
    let references = segments
        .iter()
        .map(ConversationSegmentV1::reference)
        .collect();
    match delta.mode {
        CanonicalDeltaModeV1::Append => ContextManifestNodeV1::new(
            key.clone(),
            parent,
            delta.completed_turn,
            delta.journal_event_seq,
            delta.conversation_seq,
            delta.compaction_generation,
            delta.config_version_id.clone(),
            references,
        ),
        CanonicalDeltaModeV1::Replace => ContextManifestNodeV1::new_replacement(
            key.clone(),
            parent,
            delta.completed_turn,
            delta.journal_event_seq,
            delta.conversation_seq,
            delta.compaction_generation,
            delta.config_version_id.clone(),
            references,
        ),
    }
}

fn next_head_totals(
    head: Option<&SessionContextHeadV1>,
    segments: &[ConversationSegmentV1],
    mode: CanonicalDeltaModeV1,
) -> Result<(u64, u64), SessionContextCoordinatorError> {
    let appended_bytes = segments.iter().try_fold(0_u64, |total, segment| {
        total.checked_add(segment.canonical_bytes).ok_or_else(|| {
            SessionContextCoordinatorError::NeedsRepair("head canonical byte count overflow".into())
        })
    })?;
    let appended_messages = segments.iter().try_fold(0_u64, |total, segment| {
        total
            .checked_add(u64::from(segment.message_count))
            .ok_or_else(|| {
                SessionContextCoordinatorError::NeedsRepair("head message count overflow".into())
            })
    })?;
    let base_bytes = if mode == CanonicalDeltaModeV1::Replace {
        0
    } else {
        head.map_or(0, |head| head.total_canonical_bytes)
    };
    let base_messages = if mode == CanonicalDeltaModeV1::Replace {
        0
    } else {
        head.map_or(0, |head| head.total_message_count)
    };
    Ok((
        base_bytes.checked_add(appended_bytes).ok_or_else(|| {
            SessionContextCoordinatorError::NeedsRepair("head canonical byte overflow".into())
        })?,
        base_messages
            .checked_add(appended_messages)
            .ok_or_else(|| {
                SessionContextCoordinatorError::NeedsRepair("head message count overflow".into())
            })?,
    ))
}

fn validate_head(head: &SessionContextHeadV1) -> Result<(), SessionContextCoordinatorError> {
    head.key
        .validate()
        .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
    if head.schema_version != SESSION_COORDINATION_SCHEMA_VERSION
        || !head.key.validates_cursor(&head.cursor)
        || head.cursor.canonical_root_hash != head.latest_manifest_root
        || head.total_message_count == 0
    {
        return Err(SessionContextCoordinatorError::NeedsRepair(
            "invalid context head".into(),
        ));
    }
    Ok(())
}

fn validate_optional_cursor(
    key: &SessionKeyV1,
    cursor: Option<&SessionCursorV1>,
) -> Result<(), SessionContextCoordinatorError> {
    if cursor.is_some_and(|cursor| !key.validates_cursor(cursor)) {
        return Err(SessionContextCoordinatorError::Invalid(
            "cursor identity does not match session key".into(),
        ));
    }
    Ok(())
}

fn execution_resume_storage_error(
    error: crate::runs::ExecutionHandoffReferenceError,
) -> SessionContextCoordinatorError {
    match error {
        crate::runs::ExecutionHandoffReferenceError::Rejected(detail) => {
            tracing::warn!(reason = detail, "execution resume custody rejected");
            SessionContextCoordinatorError::Fenced
        }
        crate::runs::ExecutionHandoffReferenceError::Unavailable(detail) => {
            database_error("execution_resume_storage", sqlx::Error::Protocol(detail))
        }
    }
}

fn validate_stored_manifest(
    stored: &sqlx::mysql::MySqlRow,
    node: &ContextManifestNodeV1,
    canonical_segment_bytes: u64,
    total_canonical_bytes: u64,
    total_message_count: u64,
) -> Result<(), SessionContextCoordinatorError> {
    let stored_manifest = stored
        .try_get::<String, _>("manifest_json")
        .map_err(|source| database_error("decode_existing_manifest", source))?;
    let stored_manifest: ContextManifestNodeV1 =
        database_json("existing_manifest", &stored_manifest)?;
    let stored_parent = stored
        .try_get::<Option<String>, _>("parent_manifest_root")
        .map_err(|source| database_error("decode_existing_manifest_parent", source))?;
    if stored_manifest != *node
        || stored_parent != node.parent_manifest_root
        || database_u64(stored, "completed_turn")? != u64::from(node.completed_turn)
        || database_u64(stored, "conversation_seq")? != node.conversation_seq
        || database_u64(stored, "compaction_generation")? != node.compaction_generation
        || database_u64(stored, "canonical_segment_bytes")? != canonical_segment_bytes
        || database_u64(stored, "total_canonical_bytes")? != total_canonical_bytes
        || database_u64(stored, "total_message_count")? != total_message_count
    {
        return Err(SessionContextCoordinatorError::NeedsRepair(
            "existing immutable manifest does not match its content-addressed key".into(),
        ));
    }
    Ok(())
}

const MANIFEST_CONTENT_COLUMNS: &str = "manifest_root, parent_manifest_root, completed_turn,
    conversation_seq, compaction_generation, canonical_segment_bytes,
    total_canonical_bytes, total_message_count, manifest_json, content_json, reachable";

struct StoredManifestContent {
    node: ContextManifestNodeV1,
    segments: Vec<ConversationSegmentV1>,
    total_canonical_bytes: u64,
    total_message_count: u64,
}

fn encode_manifest_content(
    content: &[Vec<Value>],
) -> Result<String, SessionContextCoordinatorError> {
    if content.len() > MAX_SEGMENT_BATCH {
        return Err(SessionContextCoordinatorError::ResourceLimit(
            "too many Context segments",
        ));
    }
    // Bound the whole frame before any canonical traversal, sorting or hashing.
    // In particular, a batch of individually valid segments must stop counting
    // at 32 MiB instead of canonicalizing up to 256 * 8 MiB first.
    if !astra_turn_types::json_serialized_fits(content, MAX_CONTEXT_CONTENT_BYTES)
        .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?
    {
        return Err(SessionContextCoordinatorError::ResourceLimit(
            "encoded Context content exceeds byte budget",
        ));
    }
    for messages in content {
        if !astra_turn_types::json_serialized_fits(messages, MAX_CONTEXT_SEGMENT_BYTES)
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?
            || canonical_conversation_serialized_len(messages) > MAX_CONTEXT_SEGMENT_BYTES
        {
            return Err(SessionContextCoordinatorError::ResourceLimit(
                "Context segment exceeds byte budget",
            ));
        }
    }
    // Counting stops at the bound before allocating. This buffer is encoded
    // once and reused by the existing transaction, including commit retries.
    database_to_json("manifest content", &content)
}

fn decode_manifest_content(
    node: &ContextManifestNodeV1,
    json: &str,
) -> Result<Vec<ConversationSegmentV1>, SessionContextCoordinatorError> {
    let invalid = |detail: &str| SessionContextCoordinatorError::NeedsRepair(detail.into());
    node.validate()
        .map_err(|error| invalid(&error.to_string()))?;
    if json.len() > MAX_CONTEXT_CONTENT_BYTES as usize
        || node.appended_segments.len() > MAX_SEGMENT_BATCH
    {
        return Err(invalid("stored Context exceeds byte budget"));
    }
    let content: Vec<Vec<Value>> = database_json("manifest content", json)?;
    if content.len() != node.appended_segments.len() {
        return Err(invalid("manifest content segment count mismatch"));
    }
    content
        .into_iter()
        .zip(&node.appended_segments)
        .map(|(messages, expected)| {
            if !astra_turn_types::json_serialized_fits(&messages, MAX_CONTEXT_SEGMENT_BYTES)
                .map_err(|error| invalid(&error.to_string()))?
            {
                return Err(invalid(
                    "stored Context segment exceeds encoded byte budget",
                ));
            }
            let segment = ConversationSegmentV1::new(&node.key, messages)
                .map_err(|error| invalid(&error.to_string()))?;
            if segment.canonical_bytes > MAX_CONTEXT_SEGMENT_BYTES
                || segment.reference() != *expected
            {
                return Err(invalid(
                    "manifest content does not match its ordered reference",
                ));
            }
            Ok(segment)
        })
        .collect()
}

fn decode_stored_manifest_content(
    row: &sqlx::mysql::MySqlRow,
    key: &SessionKeyV1,
) -> Result<StoredManifestContent, SessionContextCoordinatorError> {
    let json: String = row
        .try_get("manifest_json")
        .map_err(|source| database_error("decode_manifest", source))?;
    let node: ContextManifestNodeV1 = database_json("manifest", &json)?;
    let stored_root: String = row
        .try_get("manifest_root")
        .map_err(|source| database_error("decode_manifest_root", source))?;
    if node.key != *key || node.manifest_root != stored_root || database_u64(row, "reachable")? != 1
    {
        return Err(SessionContextCoordinatorError::NeedsRepair(
            "manifest scope or reachability mismatch".into(),
        ));
    }
    let content_json: String = row
        .try_get("content_json")
        .map_err(|source| database_error("decode_manifest_content", source))?;
    let segments = decode_manifest_content(&node, &content_json)?;
    let total_canonical_bytes = database_u64(row, "total_canonical_bytes")?;
    let total_message_count = database_u64(row, "total_message_count")?;
    let canonical_bytes = segments
        .iter()
        .try_fold(0_u64, |sum, segment| {
            sum.checked_add(segment.canonical_bytes)
        })
        .ok_or_else(|| {
            SessionContextCoordinatorError::NeedsRepair(
                "manifest content byte count overflow".into(),
            )
        })?;
    validate_stored_manifest(
        row,
        &node,
        canonical_bytes,
        total_canonical_bytes,
        total_message_count,
    )?;
    Ok(StoredManifestContent {
        node,
        segments,
        total_canonical_bytes,
        total_message_count,
    })
}

fn validate_optional_manifest_root(
    root: Option<&str>,
) -> Result<(), SessionContextCoordinatorError> {
    if root.is_some_and(|root| {
        root.len() != 64
            || !root
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }) {
        return Err(SessionContextCoordinatorError::Invalid(
            "manifest root must be a lowercase SHA-256 digest".into(),
        ));
    }
    Ok(())
}

fn validate_ttl(ttl: Duration, maximum: Duration) -> Result<(), SessionContextCoordinatorError> {
    if ttl.is_zero() || ttl > maximum {
        return Err(SessionContextCoordinatorError::Invalid(format!(
            "TTL must be between 1 ms and {} seconds",
            maximum.as_secs()
        )));
    }
    Ok(())
}

fn validate_idempotency_key(value: &str) -> Result<(), SessionContextCoordinatorError> {
    if value.is_empty()
        || value.len() > MAX_IDEMPOTENCY_KEY_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(SessionContextCoordinatorError::Invalid(
            "idempotency key must be non-empty and at most 512 bytes".into(),
        ));
    }
    Ok(())
}

fn validate_writer_transfer_request(
    request: &WriterTransferRequestV1,
) -> Result<(), SessionContextCoordinatorError> {
    request
        .key
        .validate()
        .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
    request
        .target_actor
        .validate_for(&request.key)
        .map_err(|_| SessionContextCoordinatorError::Unauthorized)?;
    validate_optional_cursor(&request.key, request.expected_cursor.as_ref())?;
    validate_idempotency_key(&request.idempotency_key)?;
    if request.handoff_id.is_empty()
        || request.handoff_id.len() > 128
        || request.handoff_id.chars().any(char::is_control)
    {
        return Err(SessionContextCoordinatorError::Invalid(
            "handoff identity must be non-empty and at most 128 bytes".into(),
        ));
    }
    request
        .risk
        .validate()
        .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
    match request.mode {
        SessionHandoffModeV1::Graceful => {
            let source = request.source_lease.as_ref().ok_or_else(|| {
                SessionContextCoordinatorError::Invalid(
                    "graceful transfer requires the source writer lease".into(),
                )
            })?;
            if source.key != request.key || request.risk != HandoffRiskEvidenceV1::default() {
                return Err(SessionContextCoordinatorError::Invalid(
                    "graceful transfer source or risk evidence is invalid".into(),
                ));
            }
        }
        SessionHandoffModeV1::Forced => {
            if request.source_lease.is_some() || !request.risk.permits_forced_fence() {
                return Err(SessionContextCoordinatorError::Invalid(
                    "forced transfer requires verified authorization and no source lease".into(),
                ));
            }
        }
    }
    Ok(())
}

fn validate_writer_transfer_receipt(
    receipt: &WriterTransferReceiptV1,
    request_hash: &str,
) -> Result<(), SessionContextCoordinatorError> {
    if receipt.request_hash != request_hash {
        return Err(SessionContextCoordinatorError::IdempotencyMismatch);
    }
    Ok(())
}

fn writer_transfer_conflict(
    state: &CoordinatorStateV1,
    reason: WriterTransferConflictV1,
    now: i64,
) -> TransferWriterOutcome {
    TransferWriterOutcome::Conflict {
        reason,
        current_head: state.head.clone(),
        active_lease_expires_at_unix_ms: state
            .active_writer
            .as_ref()
            .filter(|lease| lease.expires_at_unix_ms > now)
            .map(|lease| lease.expires_at_unix_ms),
    }
}

fn checked_expiry(now_unix_ms: i64, ttl: Duration) -> Result<i64, SessionContextCoordinatorError> {
    let ttl_ms =
        i64::try_from(ttl.as_millis()).map_err(|_| SessionContextCoordinatorError::Clock)?;
    now_unix_ms
        .checked_add(ttl_ms)
        .ok_or(SessionContextCoordinatorError::Clock)
}

/// Extends a live authority window without reviving an expired holder.
fn refreshed_live_expiry(
    now_unix_ms: i64,
    ttl: Duration,
    active_expires_at_unix_ms: i64,
    ceiling_expires_at_unix_ms: Option<i64>,
) -> Result<i64, SessionContextCoordinatorError> {
    if active_expires_at_unix_ms <= now_unix_ms
        || ceiling_expires_at_unix_ms.is_some_and(|ceiling| ceiling <= now_unix_ms)
    {
        return Err(SessionContextCoordinatorError::Expired);
    }
    let refreshed = checked_expiry(now_unix_ms, ttl)?;
    Ok(ceiling_expires_at_unix_ms.map_or(refreshed, |ceiling| refreshed.min(ceiling)))
}

/// Atomically removes a writer whose matching in-flight reservation expired.
fn fence_expired_reservation_authority(
    state: &mut CoordinatorStateV1,
    lease: &ConversationWriterLeaseV1,
    now_unix_ms: i64,
) -> bool {
    let reservation_matches_and_expired =
        state
            .active_reservation
            .as_ref()
            .is_some_and(|reservation| {
                reservation.lease_id == lease.lease_id
                    && reservation.writer_epoch == lease.writer_epoch
                    && reservation.expires_at_unix_ms <= now_unix_ms
            });
    let writer_matches = state.active_writer.as_ref().is_some_and(|active| {
        active.lease_id == lease.lease_id && active.writer_epoch == lease.writer_epoch
    });
    if !reservation_matches_and_expired || !writer_matches {
        return false;
    }
    state.active_reservation = None;
    state.active_writer = None;
    true
}

fn validate_lease_request(
    lease: &ConversationWriterLeaseV1,
    key: &SessionKeyV1,
    expected_cursor: &Option<SessionCursorV1>,
    actor: &ActorContextV1,
) -> Result<(), SessionContextCoordinatorError> {
    if &lease.key != key || &lease.expected_cursor != expected_cursor || &lease.actor != actor {
        return Err(SessionContextCoordinatorError::IdempotencyMismatch);
    }
    Ok(())
}

fn validate_reservation_request(
    reservation: &TurnReservationV1,
    lease: &ConversationWriterLeaseV1,
    expected_cursor: &Option<SessionCursorV1>,
) -> Result<(), SessionContextCoordinatorError> {
    if reservation.key != lease.key
        || reservation.lease_id != lease.lease_id
        || reservation.writer_epoch != lease.writer_epoch
        || &reservation.expected_cursor != expected_cursor
    {
        return Err(SessionContextCoordinatorError::IdempotencyMismatch);
    }
    Ok(())
}

fn validate_active_lease(
    state: &CoordinatorStateV1,
    lease: &ConversationWriterLeaseV1,
    now: i64,
) -> Result<(), SessionContextCoordinatorError> {
    if state.writer_epoch != lease.writer_epoch
        || state
            .active_writer
            .as_ref()
            .is_none_or(|active| active.lease_id != lease.lease_id)
        || lease.actor.authority_epochs != state.authority_epochs
    {
        return Err(SessionContextCoordinatorError::Fenced);
    }
    if state
        .active_writer
        .as_ref()
        .is_some_and(|active| active.expires_at_unix_ms <= now)
    {
        return Err(SessionContextCoordinatorError::Expired);
    }
    Ok(())
}

fn validate_active_reservation(
    state: &CoordinatorStateV1,
    reservation: &TurnReservationV1,
    now: i64,
) -> Result<(), SessionContextCoordinatorError> {
    let lease = state
        .active_writer
        .as_ref()
        .ok_or(SessionContextCoordinatorError::Fenced)?;
    if state.writer_epoch != reservation.writer_epoch
        || lease.lease_id != reservation.lease_id
        || state
            .active_reservation
            .as_ref()
            .is_none_or(|active| active.reservation_id != reservation.reservation_id)
    {
        return Err(SessionContextCoordinatorError::Fenced);
    }
    if lease.expires_at_unix_ms <= now
        || state
            .active_reservation
            .as_ref()
            .is_none_or(|active| active.expires_at_unix_ms <= now)
    {
        return Err(SessionContextCoordinatorError::Expired);
    }
    if lease.actor.authority_epochs != state.authority_epochs {
        return Err(SessionContextCoordinatorError::Fenced);
    }
    Ok(())
}

fn validate_delta_advance(
    head: Option<&SessionContextHeadV1>,
    reserved_turn: u32,
    delta: &CanonicalTurnDeltaV1,
) -> Result<(), SessionContextCoordinatorError> {
    if delta.schema_version != CANONICAL_TURN_DELTA_SCHEMA_VERSION
        || delta.completed_turn != reserved_turn
    {
        return Err(SessionContextCoordinatorError::Invalid(
            "turn delta does not match its reservation".into(),
        ));
    }
    let (base_journal_seq, base_conversation_seq, base_compaction_generation) =
        head.map_or((0, 0, 0), |head| {
            (
                head.cursor.journal_event_seq,
                head.cursor.conversation_seq,
                head.cursor.compaction_generation,
            )
        });
    let expected_compaction_generation = match delta.mode {
        CanonicalDeltaModeV1::Append => base_compaction_generation,
        CanonicalDeltaModeV1::Replace => base_compaction_generation.saturating_add(1),
    };
    if delta.journal_event_seq <= base_journal_seq
        || delta.conversation_seq != base_conversation_seq.saturating_add(1)
        || delta.compaction_generation != expected_compaction_generation
    {
        return Err(SessionContextCoordinatorError::Invalid(
            "turn delta must advance the reserved base monotonically".into(),
        ));
    }
    Ok(())
}

fn validate_manifest_advance(
    prior: Option<&SessionCursorV1>,
    node: &ContextManifestNodeV1,
) -> Result<(), SessionContextCoordinatorError> {
    let cursor = node.cursor();
    if let Some(prior) = prior {
        if cursor.completed_turn != prior.completed_turn.saturating_add(1)
            || cursor.conversation_seq != prior.conversation_seq.saturating_add(1)
            || cursor.journal_event_seq <= prior.journal_event_seq
            || if node.replaces_history {
                cursor.compaction_generation != prior.compaction_generation.saturating_add(1)
            } else {
                cursor.compaction_generation != prior.compaction_generation
            }
        {
            return Err(SessionContextCoordinatorError::NeedsRepair(
                "manifest cursor sequence is non-monotonic".into(),
            ));
        }
    } else if node.replaces_history {
        if cursor.completed_turn == 0
            || cursor.conversation_seq == 0
            || cursor.compaction_generation == 0
        {
            return Err(SessionContextCoordinatorError::NeedsRepair(
                "replacement manifest cursor is invalid".into(),
            ));
        }
    } else if cursor.completed_turn != 1
        || cursor.conversation_seq != 1
        || cursor.compaction_generation != 0
    {
        return Err(SessionContextCoordinatorError::NeedsRepair(
            "manifest genesis cursor is invalid".into(),
        ));
    }
    Ok(())
}

fn validate_commit_request(
    receipt: &CommitReceiptV1,
    reservation: &TurnReservationV1,
    delta: &CanonicalTurnDeltaV1,
) -> Result<(), SessionContextCoordinatorError> {
    if receipt.reservation_id != reservation.reservation_id
        || receipt.delta_hash != turn_delta_hash(delta)
    {
        return Err(SessionContextCoordinatorError::IdempotencyMismatch);
    }
    Ok(())
}

fn turn_delta_hash(delta: &CanonicalTurnDeltaV1) -> String {
    let mut digest = Sha256::new();
    digest.update(TURN_DELTA_HASH_DOMAIN);
    digest.update(delta.schema_version.to_be_bytes());
    digest.update(delta.completed_turn.to_be_bytes());
    digest.update(delta.journal_event_seq.to_be_bytes());
    digest.update(delta.conversation_seq.to_be_bytes());
    digest.update(delta.compaction_generation.to_be_bytes());
    if delta.mode == CanonicalDeltaModeV1::Replace {
        digest.update(b"replace\0");
    }
    hash_field(
        &mut digest,
        delta.config_version_id.as_deref().unwrap_or_default(),
    );
    // Keep the v1 optional-field presence byte stable for ordinary deltas.
    // It is part of persisted commit-receipt identity, even though the retired
    // field is no longer representable in the current delta type.
    digest.update([0]);
    digest.update((delta.logical_segments.len() as u64).to_be_bytes());
    for messages in &delta.logical_segments {
        hash_field(&mut digest, &canonical_conversation_root(messages));
        digest.update(canonical_conversation_serialized_len(messages).to_be_bytes());
        digest.update((messages.len() as u64).to_be_bytes());
    }
    format!("{:x}", digest.finalize())
}

#[cfg(test)]
mod turn_delta_hash_tests {
    use super::*;

    #[test]
    fn ordinary_v1_commit_receipt_hash_keeps_its_canonical_encoding() {
        let delta = CanonicalTurnDeltaV1 {
            schema_version: CANONICAL_TURN_DELTA_SCHEMA_VERSION,
            completed_turn: 1,
            journal_event_seq: 2,
            conversation_seq: 3,
            compaction_generation: 4,
            config_version_id: None,
            mode: CanonicalDeltaModeV1::Append,
            logical_segments: vec![vec![serde_json::json!({
                "role": "user",
                "content": "x"
            })]],
        };

        assert_eq!(
            turn_delta_hash(&delta),
            "003614cf21807aa7033085d3b54e222e6ed7e7fb65ecb45eb7b5a154e7375407"
        );
    }
}

fn lease_request_hash(
    key: &SessionKeyV1,
    expected_cursor: Option<&SessionCursorV1>,
    actor: &ActorContextV1,
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"astra.acquire-writer-request.v1\0");
    hash_field(&mut digest, &key.isolation_domain);
    hash_field(&mut digest, &key.owner_user_id);
    hash_field(&mut digest, &key.session_id);
    hash_field(&mut digest, &key.branch_id);
    hash_optional_cursor(&mut digest, expected_cursor);
    hash_field(&mut digest, &actor.actor_user_id);
    hash_field(&mut digest, &actor.actor_id);
    hash_field(&mut digest, actor.device_id.as_deref().unwrap_or_default());
    digest.update([actor.actor_kind as u8, actor.surface as u8]);
    digest.update(actor.authority_epochs.authorization_epoch.to_be_bytes());
    digest.update(actor.authority_epochs.device_trust_epoch.to_be_bytes());
    digest.update(actor.authority_epochs.permission_epoch.to_be_bytes());
    format!("{:x}", digest.finalize())
}

fn writer_transfer_request_hash(request: &WriterTransferRequestV1) -> String {
    let mut digest = Sha256::new();
    digest.update(b"astra.transfer-writer-request.v1\0");
    hash_field(&mut digest, &request.handoff_id);
    hash_field(&mut digest, &request.key.isolation_domain);
    hash_field(&mut digest, &request.key.owner_user_id);
    hash_field(&mut digest, &request.key.session_id);
    hash_field(&mut digest, &request.key.branch_id);
    digest.update([match request.mode {
        SessionHandoffModeV1::Graceful => 0,
        SessionHandoffModeV1::Forced => 1,
    }]);
    if let Some(source) = &request.source_lease {
        digest.update([1]);
        hash_field(&mut digest, &source.lease_id);
        digest.update(source.writer_epoch.to_be_bytes());
    } else {
        digest.update([0]);
    }
    match request.expected_writer_epoch {
        Some(epoch) => {
            digest.update([1]);
            digest.update(epoch.to_be_bytes());
        }
        None => digest.update([0]),
    }
    hash_optional_cursor(&mut digest, request.expected_cursor.as_ref());
    hash_field(&mut digest, &request.target_actor.actor_user_id);
    hash_field(&mut digest, &request.target_actor.actor_id);
    hash_field(
        &mut digest,
        request
            .target_actor
            .device_id
            .as_deref()
            .unwrap_or_default(),
    );
    digest.update([
        request.target_actor.actor_kind as u8,
        request.target_actor.surface as u8,
    ]);
    digest.update(
        request
            .target_actor
            .authority_epochs
            .authorization_epoch
            .to_be_bytes(),
    );
    digest.update(
        request
            .target_actor
            .authority_epochs
            .device_trust_epoch
            .to_be_bytes(),
    );
    digest.update(
        request
            .target_actor
            .authority_epochs
            .permission_epoch
            .to_be_bytes(),
    );
    hash_field(
        &mut digest,
        request
            .risk
            .unsynced_suffix_root
            .as_deref()
            .unwrap_or_default(),
    );
    let mut unknown_effects = request.risk.unknown_effect_invocation_ids.clone();
    unknown_effects.sort_unstable();
    for identity in unknown_effects {
        hash_field(&mut digest, &identity);
    }
    hash_field(
        &mut digest,
        request
            .risk
            .forced_authorization_id
            .as_deref()
            .unwrap_or_default(),
    );
    format!("{:x}", digest.finalize())
}

fn reservation_request_hash(
    lease: &ConversationWriterLeaseV1,
    expected_cursor: Option<&SessionCursorV1>,
) -> String {
    reservation_identity_hash(
        &lease.key,
        &lease.lease_id,
        lease.writer_epoch,
        expected_cursor,
    )
}

fn reservation_identity_hash(
    key: &SessionKeyV1,
    lease_id: &str,
    writer_epoch: u64,
    expected_cursor: Option<&SessionCursorV1>,
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"astra.reserve-turn-request.v1\0");
    hash_field(&mut digest, &key.isolation_domain);
    hash_field(&mut digest, &key.owner_user_id);
    hash_field(&mut digest, &key.session_id);
    hash_field(&mut digest, &key.branch_id);
    hash_field(&mut digest, lease_id);
    digest.update(writer_epoch.to_be_bytes());
    hash_optional_cursor(&mut digest, expected_cursor);
    format!("{:x}", digest.finalize())
}

fn commit_request_hash(reservation: &TurnReservationV1, delta: &CanonicalTurnDeltaV1) -> String {
    let mut digest = Sha256::new();
    digest.update(b"astra.commit-turn-request.v1\0");
    hash_field(&mut digest, &reservation.key.isolation_domain);
    hash_field(&mut digest, &reservation.key.owner_user_id);
    hash_field(&mut digest, &reservation.key.session_id);
    hash_field(&mut digest, &reservation.key.branch_id);
    hash_field(&mut digest, &reservation.reservation_id);
    hash_field(&mut digest, &turn_delta_hash(delta));
    format!("{:x}", digest.finalize())
}

fn commit_receipt_request_hash(receipt: &CommitReceiptV1) -> String {
    let mut digest = Sha256::new();
    digest.update(b"astra.commit-turn-request.v1\0");
    hash_field(&mut digest, &receipt.reservation.key.isolation_domain);
    hash_field(&mut digest, &receipt.reservation.key.owner_user_id);
    hash_field(&mut digest, &receipt.reservation.key.session_id);
    hash_field(&mut digest, &receipt.reservation.key.branch_id);
    hash_field(&mut digest, &receipt.reservation_id);
    hash_field(&mut digest, &receipt.delta_hash);
    format!("{:x}", digest.finalize())
}

fn hash_optional_cursor(digest: &mut Sha256, cursor: Option<&SessionCursorV1>) {
    let Some(cursor) = cursor else {
        digest.update([0]);
        return;
    };
    digest.update([1]);
    hash_field(digest, &cursor.owner_id);
    hash_field(digest, &cursor.session_id);
    hash_field(digest, &cursor.branch_id);
    digest.update(cursor.completed_turn.to_be_bytes());
    digest.update(cursor.journal_event_seq.to_be_bytes());
    digest.update(cursor.conversation_seq.to_be_bytes());
    hash_field(digest, &cursor.canonical_root_hash);
    digest.update(cursor.projection_schema.to_be_bytes());
    digest.update(cursor.compaction_generation.to_be_bytes());
    hash_field(
        digest,
        cursor.config_version_id.as_deref().unwrap_or_default(),
    );
}

fn hash_receipt(operation: &str, idempotency_key: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(RECEIPT_HASH_DOMAIN);
    hash_field(&mut digest, operation);
    hash_field(&mut digest, idempotency_key);
    format!("{:x}", digest.finalize())
}

fn hash_field(digest: &mut Sha256, value: &str) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value.as_bytes());
}

#[cfg(test)]
mod adoption_tests {
    use super::*;

    #[test]
    fn node_content_preserves_duplicate_references_and_rejects_changed_order() {
        let key = SessionKeyV1::owner_session("server", "owner", "session", "main");
        let first = vec![serde_json::json!({"role":"user","content":"中文🙂\n\t\"\\"})];
        let second = vec![serde_json::json!({"role":"assistant","content":"answer"})];
        for content in [
            vec![first.clone(), first.clone()],
            vec![first.clone(), second.clone()],
        ] {
            let segments: Vec<_> = content
                .iter()
                .map(|messages| ConversationSegmentV1::new(&key, messages.clone()).unwrap())
                .collect();
            let node = ContextManifestNodeV1::new(
                key.clone(),
                None,
                1,
                1,
                1,
                0,
                None,
                segments
                    .iter()
                    .map(ConversationSegmentV1::reference)
                    .collect(),
            )
            .unwrap();
            let encoded = encode_manifest_content(&content).unwrap();
            assert_eq!(decode_manifest_content(&node, &encoded).unwrap(), segments);
            for malformed in ["null", "[]", "[[]]"] {
                assert!(decode_manifest_content(&node, malformed).is_err());
            }
            let mut changed = content.clone();
            changed[0][0]["content"] = "changed".into();
            assert!(
                decode_manifest_content(&node, &serde_json::to_string(&changed).unwrap()).is_err()
            );
        }
        let segments = [
            ConversationSegmentV1::new(&key, first.clone()).unwrap(),
            ConversationSegmentV1::new(&key, second.clone()).unwrap(),
        ];
        let node = ContextManifestNodeV1::new(
            key,
            None,
            1,
            1,
            1,
            0,
            None,
            segments
                .iter()
                .map(ConversationSegmentV1::reference)
                .collect(),
        )
        .unwrap();
        let reversed = encode_manifest_content(&[second, first]).unwrap();
        assert!(decode_manifest_content(&node, &reversed).is_err());
    }

    #[test]
    fn ordinary_context_preparation_enforces_segment_and_encoded_frame_limits() {
        let authority = RunContextAuthority {
            key: SessionKeyV1::owner_session("server", "owner", "session", "main"),
            run_id: "run".into(),
            generation: 0,
            row_exists: false,
            base_head: None,
            turn: 1,
            writer_epoch: 0,
            authority_epochs: AuthorityEpochsV1::default(),
            binding: RunContextBinding::Absent,
        };
        let delta = |logical_segments| CanonicalTurnDeltaV1 {
            schema_version: CANONICAL_TURN_DELTA_SCHEMA_VERSION,
            completed_turn: 1,
            journal_event_seq: 1,
            conversation_seq: 1,
            compaction_generation: 0,
            config_version_id: None,
            mode: CanonicalDeltaModeV1::Append,
            logical_segments,
        };
        let input = vec![serde_json::json!({"role":"user","content":""})];
        let overhead = serde_json::to_vec(&input).unwrap().len();
        let at_limit = vec![
            serde_json::json!({"role":"user","content":"x".repeat(MAX_CONTEXT_SEGMENT_BYTES as usize - overhead)}),
        ];
        assert!(
            PreparedSessionContextCommit::for_run(
                &authority,
                &"a".repeat(64),
                delta(vec![at_limit.clone()])
            )
            .is_ok()
        );
        for content in [
            vec![input; MAX_SEGMENT_BATCH + 1],
            vec![vec![
                serde_json::json!({"role":"user","content":"x".repeat(MAX_CONTEXT_SEGMENT_BYTES as usize)}),
            ]],
            vec![at_limit; 4], // Four individually legal segments exceed the whole-frame budget including brackets/commas.
        ] {
            assert!(matches!(
                PreparedSessionContextCommit::for_run(&authority, &"a".repeat(64), delta(content)),
                Err(SessionContextCoordinatorError::ResourceLimit(_))
            ));
        }
    }

    #[test]
    fn run_context_preparation_accepts_initial_run_generation_and_fences_binding_generation() {
        let mut authority = RunContextAuthority {
            key: SessionKeyV1::owner_session(
                "server",
                "owner",
                "session",
                astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID,
            ),
            run_id: "run".into(),
            generation: 0,
            row_exists: false,
            base_head: None,
            turn: 1,
            writer_epoch: 0,
            authority_epochs: AuthorityEpochsV1::default(),
            binding: RunContextBinding::Absent,
        };
        let delta = CanonicalTurnDeltaV1 {
            schema_version: CANONICAL_TURN_DELTA_SCHEMA_VERSION,
            completed_turn: 1,
            journal_event_seq: 1,
            conversation_seq: 1,
            compaction_generation: 0,
            config_version_id: None,
            mode: CanonicalDeltaModeV1::Append,
            logical_segments: vec![vec![serde_json::json!({"role":"user","content":"hi"})]],
        };
        let prepared =
            PreparedSessionContextCommit::for_run(&authority, &"a".repeat(64), delta.clone())
                .unwrap();
        assert_eq!(prepared.cursor().completed_turn, 1);
        let admitted = authority.admission_fingerprint();
        authority.generation = 1;
        assert_eq!(authority.admission_fingerprint(), admitted);
        authority.authority_epochs.permission_epoch = 1;
        assert_ne!(authority.admission_fingerprint(), admitted);
        authority.authority_epochs.permission_epoch = 0;
        authority.binding = RunContextBinding::Present { generation: 0 };
        assert!(
            PreparedSessionContextCommit::for_run(&authority, &"a".repeat(64), delta.clone())
                .is_err()
        );
        authority.binding = RunContextBinding::Absent;
        authority.generation = i64::MAX as u64 + 1;
        assert!(PreparedSessionContextCommit::for_run(&authority, &"a".repeat(64), delta).is_err());
    }

    #[test]
    fn session_execution_blockers_preserve_evidence_and_do_not_require_history_deletion() {
        for (blocker, wire, recovery) in [
            (
                SessionExecutionBlocker::ActiveRun,
                "active_run",
                "wait_or_cancel_session",
            ),
            (
                SessionExecutionBlocker::WriterOrReservation,
                "writer_or_reservation",
                "retry_session",
            ),
            (
                SessionExecutionBlocker::BindingNotReady,
                "binding_not_ready",
                "inspect_session",
            ),
            (
                SessionExecutionBlocker::UnresolvedTool,
                "unresolved_tool",
                "inspect_session",
            ),
            (
                SessionExecutionBlocker::OwnerUnavailable,
                "owner_unavailable",
                "inspect_session",
            ),
        ] {
            assert_eq!(serde_json::to_value(blocker).unwrap(), wire);
            assert_eq!(blocker.recovery_action(), recovery);
            let message = blocker.user_message("owner-session");
            assert!(message.contains("owner-session"));
            assert!(message.contains(blocker.explanation()));
            assert!(message.contains("history is preserved"));
            assert!(!message.contains("resume it"));
            assert!(!message.contains("session delete"));
        }
    }

    #[test]
    fn server_work_execution_binding_is_valid_and_has_no_caller_path() {
        let binding = SessionExecutionBindingV1::server_work_default("work:w1:branch:b1");
        binding
            .validate()
            .expect("canonical Server binding is valid");
        assert_eq!(binding.generation, 1);
        assert_eq!(binding.state, SessionExecutionBindingStateV1::Ready);
        assert_eq!(binding.workspace.root, None);
        assert_eq!(binding.workspace.source, None);
        assert_eq!(binding.executor.executor_id, None);
    }

    #[test]
    fn execution_binding_rejects_unsafe_or_incomplete_provider_pairs() {
        let mut rooted_server = SessionExecutionBindingV1::server_work_default("work:w1:b1");
        rooted_server.workspace.root = Some("/".to_string());
        assert!(rooted_server.validate().is_err());

        let mut incomplete_edge = SessionExecutionBindingV1::server_work_default("work:w1:b1");
        incomplete_edge.workspace.kind = crate::runs::WorkspaceBindingRequestKind::EdgeWorkspace;
        incomplete_edge.executor.kind = crate::runs::ExecutorBindingRequestKind::EdgeAgent;
        assert!(incomplete_edge.validate().is_err());

        let mut edge_without_physical_identity = incomplete_edge.clone();
        edge_without_physical_identity.workspace.root = Some("/workspace/a".into());
        edge_without_physical_identity.workspace.source =
            Some(crate::runs::WorkspaceSourceRequest::EdgePath {
                path: "/workspace/a".into(),
            });
        edge_without_physical_identity.workspace.authority =
            Some(crate::runs::WorkspaceAuthorityRequest::ReadWrite);
        edge_without_physical_identity.executor.executor_id = Some("edge-a".into());
        edge_without_physical_identity.executor.transport =
            Some(crate::runs::ToolTransportKindRequest::EdgeLedger);
        edge_without_physical_identity.executor.status =
            Some(crate::runs::ExecutorStatusRequest::Online);
        assert!(
            edge_without_physical_identity.validate().is_err(),
            "a durable Edge binding without authenticated physical identity must be rejected"
        );

        let mut overflow = SessionExecutionBindingV1::server_work_default("work:w1:b1");
        overflow.generation = i64::MAX as u64 + 1;
        assert!(overflow.validate().is_err());
    }

    #[test]
    fn edge_materialization_identity_survives_reconnect_and_separates_devices() {
        let first = SessionExecutionBindingV1::edge_materialization_physical_identity(
            "materialization-a",
            "/workspace/a",
        );
        let reconnect = SessionExecutionBindingV1::edge_materialization_physical_identity(
            "materialization-a",
            "/workspace/a",
        );
        let other_device = SessionExecutionBindingV1::edge_materialization_physical_identity(
            "materialization-b",
            "/workspace/a",
        );
        let other_root = SessionExecutionBindingV1::edge_materialization_physical_identity(
            "materialization-a",
            "/workspace/b",
        );
        assert_eq!(first, reconnect);
        assert_ne!(first, other_device);
        assert_ne!(first, other_root);
    }

    #[test]
    fn execution_switch_receipt_keeps_original_and_attempt_generations_distinct() {
        let key = SessionKeyV1::owner_session("server", "owner", "session", "main");
        let source = SessionExecutionBindingV1::server_work_default("work:w1:branch:b1");
        let mut target = source.clone();
        target.generation = 2;
        target.state = SessionExecutionBindingStateV1::Switching;
        let evidence = serde_json::json!({
            "schema_version": 1,
            "root": "/workspace",
            "head": "a",
            "tree": "b",
            "object_format": "sha1",
            "reference": "main",
            "repository": "repo",
            "clean": true
        });
        let mut receipt = SessionExecutionSwitchReceiptV1 {
            schema_version: SESSION_EXECUTION_SWITCH_SCHEMA_VERSION,
            operation_id: "operation".into(),
            request_id: "request".into(),
            controller_attachment_id: "attachment".into(),
            request_hash: "hash".into(),
            key: key.clone(),
            expected_generation: 1,
            attempt_expected_generation: 1,
            switching_generation: 2,
            completed_generation: None,
            state: SessionExecutionSwitchStateV1::Switching,
            source,
            target,
            source_evidence: evidence.clone(),
            evidence: None,
            failure_code: None,
            attempt: 1,
        };
        validate_execution_switch_receipt(&receipt, &key).expect("initial receipt is valid");

        receipt.expected_generation = 1;
        receipt.attempt_expected_generation = 2;
        receipt.switching_generation = 3;
        receipt.attempt = 2;
        receipt.source_evidence = evidence;
        validate_execution_switch_receipt(&receipt, &key)
            .expect("retry receipt advances only the active attempt generation");
    }
}
