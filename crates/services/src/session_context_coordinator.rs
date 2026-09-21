//! Canonical database-backed session coordination.
//!
//! The mutable record is intentionally small: one branch head, one writer
//! lease, and one turn reservation. Conversation payloads and manifest nodes
//! are immutable and durable before a head can reference them.

use std::{collections::HashSet, time::Duration};

use crate::cancellation_safe_db::CancellationSafePoolConnection;
use astra_core::{
    SharedPool, matrixone_statement_with_null_shape, push_matrixone_bound_string_set,
};
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
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{MySql, QueryBuilder, Row, Transaction};
use thiserror::Error;
use uuid::Uuid;

const MAX_IDEMPOTENCY_KEY_BYTES: usize = 512;

/// Evidence preventing fenced checkout reuse. Session history is never a blocker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceReuseBlocker {
    ExecutionSlot,
    ActiveRun,
    SettlementPending,
    WriterOrReservation,
    BindingNotReady,
    UnresolvedTool,
    OwnerUnavailable,
    ClaimChanged,
}

impl WorkspaceReuseBlocker {
    pub fn explanation(self) -> &'static str {
        match self {
            Self::ExecutionSlot => "an execution still holds the checkout",
            Self::ActiveRun => "a run or child task has not stopped",
            Self::SettlementPending => "execution stopped but its durable settlement is incomplete",
            Self::WriterOrReservation => {
                "a conversation write or turn admission is still in progress"
            }
            Self::BindingNotReady => {
                "the execution binding needs attention or is switching providers"
            }
            Self::UnresolvedTool => {
                "a tool invocation has no confirmed outcome; external effects may still be running"
            }
            Self::OwnerUnavailable => "the previous owner's state cannot be verified",
            Self::ClaimChanged => "checkout ownership changed during admission",
        }
    }

    pub fn recovery_action(self) -> &'static str {
        match self {
            Self::ExecutionSlot | Self::ActiveRun => "wait_or_cancel_session",
            Self::WriterOrReservation | Self::ClaimChanged => "retry_session",
            Self::BindingNotReady
            | Self::UnresolvedTool
            | Self::OwnerUnavailable
            | Self::SettlementPending => "inspect_session",
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
            "Checkout temporarily unavailable: Session {owner}: {}. {action}. Session history can be kept; an idle checkout is reused automatically. Use a separate worktree for concurrent work.",
            self.explanation()
        )
    }
}
const COORDINATOR_STATE_SCHEMA_VERSION: u32 = 1;
const MAX_SEGMENT_BATCH: usize = 256;
const MAX_STAGED_SEGMENT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_STAGED_BATCH_BYTES: u64 = 32 * 1024 * 1024;
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
    #[error("writer authority was fenced by a newer epoch")]
    Fenced,
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
    #[error(
        "execution workspace is already claimed by session {owner_session_id} on branch {owner_branch_id}: {blocker:?}"
    )]
    ExecutionWorkspaceClaimed {
        owner_session_id: String,
        owner_branch_id: String,
        blocker: WorkspaceReuseBlocker,
    },
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
    /// Derive the bounded physical identity used by Edge workspace claims.
    /// The persisted materialization identity separates independent devices
    /// that happen to use the same path, while the canonical root prevents two
    /// Edge labels from claiming one checkout after a reconnect or relabel.
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

    /// Fetch an explicitly requested, bounded set of immutable payloads.
    /// Request order is preserved so clients can checkpoint resumable
    /// hydration without loading unrelated history.
    async fn load_segments(
        &self,
        key: &SessionKeyV1,
        segment_hashes: &[String],
    ) -> Result<Vec<ConversationSegmentV1>, SessionContextCoordinatorError>;

    /// Idempotently stage a bounded batch of owner-scoped immutable payloads.
    ///
    /// Staging never changes a branch head or writer authority. A separate
    /// canonical journal import must prove the ordered lineage before any
    /// staged payload becomes reachable from a head.
    async fn store_segments(
        &self,
        key: &SessionKeyV1,
        segments: &[ConversationSegmentV1],
    ) -> Result<(), SessionContextCoordinatorError>;

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

pub struct AdoptExecutionTurnRequest<'a> {
    pub claim: &'a crate::runs::RecoveryClaim,
    pub owner_pod_id: &'a str,
    pub checkpoint_id: &'a str,
    pub source: &'a TurnReservationV1,
    pub actor: &'a ActorContextV1,
    pub ttl: Duration,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionTurnAdoptionReceipt {
    pub run_id: String,
    pub run_generation: u64,
    pub producer_generation: u64,
    pub checkpoint_id: String,
    pub source: TurnReservationV1,
    pub writer_lease: ConversationWriterLeaseV1,
    pub turn_reservation: TurnReservationV1,
}

/// Returned only after atomic adoption commits. The checkpoint is the exact
/// locked record whose custody was validated, not a later independent read.
/// This result is not deserializable: a saved receipt alone is not live authority.
#[derive(Debug)]
pub struct AdoptedExecutionHandoff {
    receipt: ExecutionTurnAdoptionReceipt,
    checkpoint: crate::runs::DurableRunCheckpointRecord,
}

impl AdoptedExecutionHandoff {
    pub fn receipt(&self) -> &ExecutionTurnAdoptionReceipt {
        &self.receipt
    }

    pub fn checkpoint(&self) -> &crate::runs::DurableRunCheckpointRecord {
        &self.checkpoint
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionAuthorityEventV1 {
    pub event_id: String,
    pub operation_kind: String,
    pub outcome: String,
    pub writer_epoch: u64,
    pub actor_id: Option<String>,
    pub device_id: Option<String>,
    pub lease_id: Option<String>,
    pub reservation_id: Option<String>,
    pub expected_root: Option<String>,
    pub observed_root: Option<String>,
    pub authorization_epoch: u64,
    pub device_trust_epoch: u64,
    pub permission_epoch: u64,
    pub created_at: chrono::NaiveDateTime,
}

impl DatabaseSessionContextCoordinator {
    #[cfg(test)]
    pub(crate) async fn expire_turn_authority_for_test(&self, key: &SessionKeyV1) {
        let mut tx = self.pool.get().begin().await.unwrap();
        let (mut state, now) = lock_database_state_at_now(&mut tx, key).await.unwrap();
        if let Some(writer) = &mut state.active_writer {
            writer.expires_at_unix_ms = now;
        }
        if let Some(reservation) = &mut state.active_reservation {
            reservation.expires_at_unix_ms = now;
        }
        update_database_state(&mut tx, &state).await.unwrap();
        tx.commit().await.unwrap();
    }

    pub async fn adopt_claimed_execution_turn(
        &self,
        request: AdoptExecutionTurnRequest<'_>,
    ) -> Result<AdoptedExecutionHandoff, SessionContextCoordinatorError> {
        let AdoptExecutionTurnRequest {
            claim,
            owner_pod_id,
            checkpoint_id,
            source,
            actor,
            ttl,
        } = request;
        if source.key.owner_user_id != claim.run.user_id
            || source.key.session_id != claim.run.session_id
        {
            return Err(SessionContextCoordinatorError::Unauthorized);
        }
        source
            .key
            .validate()
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        actor
            .validate_for(&source.key)
            .map_err(|_| SessionContextCoordinatorError::Unauthorized)?;
        let mut identity = Sha256::new();
        identity.update(b"astra.execution-turn-adoption.v1\0");
        hash_field(&mut identity, &claim.run.user_id);
        hash_field(&mut identity, &claim.run.run_id);
        identity.update(claim.run.run_generation.to_be_bytes());
        hash_field(&mut identity, checkpoint_id);
        hash_field(&mut identity, &source.reservation_id);
        let idempotency_key = format!("adopt:{:x}", identity.finalize());
        validate_idempotency_key(&idempotency_key)?;
        let mut connection = CancellationSafePoolConnection::acquire(self.pool.get())
            .await
            .map_err(|source| database_error("acquire_execution_turn_adoption", source))?;
        let mut tx = connection
            .begin()
            .await
            .map_err(|source| database_error("begin_execution_turn_adoption", source))?;
        let locked = crate::runs::lock_claimed_execution_handoff_tx(
            &mut tx,
            claim,
            owner_pod_id,
            checkpoint_id,
        )
        .await
        .map_err(SessionContextCoordinatorError::NeedsRepair)?
        .ok_or(SessionContextCoordinatorError::Fenced)?;
        #[derive(Deserialize)]
        struct SavedReservation {
            reservation: TurnReservationV1,
        }
        let crate::runs::DurableExecutionHandoff::V1 { heavy, .. } =
            database_json::<crate::runs::DurableExecutionHandoff<SavedReservation>>(
                "execution_handoff",
                &locked.checkpoint.checkpoint_json,
            )?;
        if heavy.reservation != *source {
            return Err(SessionContextCoordinatorError::Fenced);
        }
        let (mut state, now) = lock_database_state_at_now(&mut tx, &source.key).await?;
        let source_hash = reservation_identity_hash(
            &source.key,
            &source.lease_id,
            source.writer_epoch,
            source.expected_cursor.as_ref(),
        );
        let stored_source = match state.active_reservation.as_ref() {
            Some(active) if active.reservation_id == source.reservation_id => Some(active.clone()),
            _ => load_database_receipt::<ReservationReceiptV1>(
                &mut tx,
                &source.key,
                "reserve",
                &source.idempotency_key,
                &source_hash,
            )
            .await?
            .map(|receipt| receipt.reservation),
        }
        .ok_or(SessionContextCoordinatorError::Fenced)?;
        if stored_source.reservation_id != source.reservation_id
            || stored_source.reserved_turn != source.reserved_turn
            || reservation_identity_hash(
                &stored_source.key,
                &stored_source.lease_id,
                stored_source.writer_epoch,
                stored_source.expected_cursor.as_ref(),
            ) != source_hash
        {
            return Err(SessionContextCoordinatorError::Fenced);
        }
        let request_hash = database_to_json(
            "adoption_request",
            &(
                &claim.run.user_id,
                &claim.run.run_id,
                claim.run.run_generation,
                owner_pod_id,
                checkpoint_id,
                source,
                actor,
            ),
        )?;
        let request_hash = format!("{:x}", Sha256::digest(request_hash.as_bytes()));
        if let Some(prior) = &locked.prior_adoption {
            if prior.receipt_idempotency_key != idempotency_key
                || prior.key != source.key
                || prior.source_reservation_id != source.reservation_id
            {
                return Err(SessionContextCoordinatorError::Fenced);
            }
            let mut receipt = load_database_receipt::<ExecutionTurnAdoptionReceipt>(
                &mut tx,
                &source.key,
                "adopt_execution_turn",
                &idempotency_key,
                &request_hash,
            )
            .await?
            .ok_or_else(|| {
                SessionContextCoordinatorError::NeedsRepair("adoption receipt missing".into())
            })?;
            if receipt.run_id != claim.run.run_id
                || receipt.run_generation != claim.run.run_generation
                || receipt.producer_generation != locked.association.producer_generation
                || receipt.checkpoint_id != checkpoint_id
                || receipt.source != *source
                || receipt.turn_reservation.reservation_id != prior.adopted_reservation_id
            {
                return Err(SessionContextCoordinatorError::Fenced);
            }
            validate_active_lease(&state, &receipt.writer_lease, now)?;
            validate_active_reservation(&state, &receipt.turn_reservation, now)?;
            // The immutable receipt proves identity. Renewal may have extended
            // the same authority; return its current deadlines, never stale ones.
            receipt.writer_lease = state
                .active_writer
                .as_ref()
                .expect("validated active writer")
                .clone();
            receipt.turn_reservation = state
                .active_reservation
                .as_ref()
                .expect("validated active reservation")
                .clone();
            tx.commit()
                .await
                .map_err(|source| database_error("commit_execution_adoption_retry", source))?;
            connection.release();
            return Ok(AdoptedExecutionHandoff {
                receipt,
                checkpoint: locked.checkpoint,
            });
        }
        let (lease, reservation) =
            prepare_adopted_turn_authority(&state, source, actor, now, ttl, &idempotency_key)?;
        let receipt = ExecutionTurnAdoptionReceipt {
            run_id: claim.run.run_id.clone(),
            run_generation: claim.run.run_generation,
            producer_generation: locked.association.producer_generation,
            checkpoint_id: checkpoint_id.to_owned(),
            source: source.clone(),
            writer_lease: lease.clone(),
            turn_reservation: reservation.clone(),
        };
        archive_database_state_receipts(&mut tx, &state).await?;
        state.writer_epoch = lease.writer_epoch;
        state.active_writer = Some(lease.clone());
        state.active_reservation = Some(reservation.clone());
        update_database_state(&mut tx, &state).await?;
        store_database_receipt(
            &mut tx,
            &source.key,
            "adopt_execution_turn",
            &idempotency_key,
            &request_hash,
            &receipt,
        )
        .await?;
        crate::runs::append_execution_handoff_adoption_tx(
            &mut tx,
            &locked,
            &crate::runs::ExecutionHandoffAdoption {
                checkpoint_id: checkpoint_id.to_owned(),
                producer_generation: locked.association.producer_generation,
                run_generation: claim.run.run_generation,
                key: source.key.clone(),
                receipt_idempotency_key: idempotency_key,
                source_reservation_id: source.reservation_id.clone(),
                adopted_reservation_id: reservation.reservation_id.clone(),
            },
        )
        .await
        .map_err(SessionContextCoordinatorError::NeedsRepair)?;
        record_database_authority_event(
            &mut tx,
            &state,
            AuthorityAuditFact {
                operation: "adopt_execution_turn",
                outcome: "adopted",
                actor: Some(actor),
                lease_id: Some(&lease.lease_id),
                reservation_id: Some(&reservation.reservation_id),
                expected_cursor: source.expected_cursor.as_ref(),
            },
        )
        .await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit_execution_turn_adoption", source))?;
        connection.release();
        Ok(AdoptedExecutionHandoff {
            receipt,
            checkpoint: locked.checkpoint,
        })
    }

    pub fn new(pool: SharedPool) -> Self {
        Self { pool }
    }

    /// The same fenced idle proof used by physical checkout reuse. A caller
    /// must also retain any process-local executor evidence it owns.
    pub async fn execution_reuse_blocker(
        &self,
        key: &SessionKeyV1,
    ) -> Result<Option<WorkspaceReuseBlocker>, SessionContextCoordinatorError> {
        key.validate()
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        let mut connection = CancellationSafePoolConnection::acquire(self.pool.get())
            .await
            .map_err(|source| database_error("acquire_execution_idle_proof", source))?;
        let mut tx = connection
            .begin()
            .await
            .map_err(|source| database_error("begin_execution_idle_proof", source))?;
        let blocker = locked_execution_reuse_blocker(&mut tx, key, None).await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit_execution_idle_proof", source))?;
        connection.release();
        Ok(blocker)
    }

    /// Retire only an internally acquired turn writer whose exact Run
    /// generation is terminal and no longer leased. Run recovery, new
    /// admission, and writer transfer cannot interleave with this proof.
    pub async fn release_terminal_execution_writer(
        &self,
        lease: &ConversationWriterLeaseV1,
        run_id: &str,
        expected_run_generation: u64,
    ) -> Result<bool, SessionContextCoordinatorError> {
        let key = &lease.key;
        key.validate()
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        if lease.actor.actor_kind != astra_turn_types::ActorKindV1::Server
            || lease.actor.actor_id != format!("server-run:{run_id}")
            || lease.idempotency_key != format!("server-run:{run_id}:writer")
        {
            return Ok(false);
        }
        let mut connection = CancellationSafePoolConnection::acquire(self.pool.get())
            .await
            .map_err(|source| database_error("acquire_terminal_writer_release", source))?;
        let mut tx = connection
            .begin()
            .await
            .map_err(|source| database_error("begin_terminal_writer_release", source))?;
        crate::storage::admit_session_execution_write(&mut tx, &key.session_id, &key.owner_user_id)
            .await
            .map_err(|source| database_error("fence_terminal_writer_release", source))?;
        let run = sqlx::query(
            "SELECT status, run_generation,
                    CAST(owner_pod_id IS NOT NULL AND owner_lease_expires_at >= NOW(6) AS SIGNED) AS owner_live
             FROM agent_runs WHERE user_id = ? AND session_id = ? AND run_id = ? FOR UPDATE",
        ).bind(&key.owner_user_id).bind(&key.session_id).bind(run_id)
            .fetch_optional(&mut *tx).await
            .map_err(|source| database_error("lock_terminal_writer_run", source))?;
        let Some(run) = run else {
            return Ok(false);
        };
        let status: String = run
            .try_get("status")
            .map_err(|source| database_error("decode_terminal_writer_status", source))?;
        let generation: i64 = run
            .try_get("run_generation")
            .map_err(|source| database_error("decode_terminal_writer_generation", source))?;
        let owner_live: Option<i64> = run
            .try_get("owner_live")
            .map_err(|source| database_error("decode_terminal_writer_owner", source))?;
        if !crate::runs::durable_run_status_is_terminal(&status)
            || u64::try_from(generation).ok() != Some(expected_run_generation)
            || owner_live == Some(1)
        {
            return Ok(false);
        }
        if crate::runs::run_has_open_settlement_in_tx(
            &mut tx,
            &key.owner_user_id,
            run_id,
            expected_run_generation,
        )
        .await
        .map_err(|source| database_error("lock_terminal_writer_settlement", source))?
        {
            return Ok(false);
        }
        let mut state = lock_database_state(&mut tx, key).await?;
        if !state
            .active_writer
            .as_ref()
            .is_some_and(|active| active == lease)
        {
            return Ok(false);
        }
        if state
            .active_reservation
            .as_ref()
            .is_some_and(|reservation| {
                reservation.key != *key
                    || reservation.lease_id != lease.lease_id
                    || reservation.writer_epoch != lease.writer_epoch
                    || reservation.idempotency_key != format!("server-run:{run_id}:turn")
            })
        {
            return Ok(false);
        }
        clear_writer_authority_in_tx(&mut tx, &mut state).await?;
        record_database_authority_event(
            &mut tx,
            &state,
            AuthorityAuditFact {
                operation: "release_terminal_run_writer",
                outcome: "released",
                actor: Some(&lease.actor),
                lease_id: Some(&lease.lease_id),
                reservation_id: None,
                expected_cursor: lease.expected_cursor.as_ref(),
            },
        )
        .await?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit_terminal_writer_release", source))?;
        connection.release();
        Ok(true)
    }

    /// Retire only an idle physical claim, in a transaction belonging to its
    /// existing owner. Never lock a second Session head inside admission.
    async fn release_idle_execution_workspace_claim(
        &self,
        claimant: &SessionKeyV1,
        target: &SessionExecutionBindingV1,
    ) -> Result<(), SessionContextCoordinatorError> {
        let Some(identity) = execution_workspace_identity(target) else {
            return Ok(());
        };
        let identity_hash = execution_workspace_identity_hash(&identity);
        let owner = sqlx::query(
            "SELECT workspace_identity, session_id, branch_id
             FROM session_execution_workspace_claims
             WHERE isolation_domain = ? AND owner_user_id = ? AND workspace_identity_hash = ?",
        )
        .bind(&claimant.isolation_domain)
        .bind(&claimant.owner_user_id)
        .bind(&identity_hash)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| database_error("discover_idle_workspace_claim", source))?;
        let Some(owner) = owner else {
            return Ok(());
        };
        let existing_identity: String = owner
            .try_get("workspace_identity")
            .map_err(|source| database_error("decode_idle_workspace_identity", source))?;
        let session_id: String = owner
            .try_get("session_id")
            .map_err(|source| database_error("decode_idle_workspace_session", source))?;
        let branch_id: String = owner
            .try_get("branch_id")
            .map_err(|source| database_error("decode_idle_workspace_branch", source))?;
        if existing_identity != identity
            || (session_id == claimant.session_id && branch_id == claimant.branch_id)
        {
            return Ok(());
        }
        let key = SessionKeyV1::owner_session(
            &claimant.isolation_domain,
            &claimant.owner_user_id,
            &session_id,
            &branch_id,
        );
        let blocked = |blocker| SessionContextCoordinatorError::ExecutionWorkspaceClaimed {
            owner_session_id: session_id.clone(),
            owner_branch_id: branch_id.clone(),
            blocker,
        };
        let mut connection = CancellationSafePoolConnection::acquire(self.pool.get())
            .await
            .map_err(|source| database_error("acquire_idle_workspace_release", source))?;
        let mut tx = connection
            .begin()
            .await
            .map_err(|source| database_error("begin_idle_workspace_release", source))?;
        if let Some(blocker) =
            locked_execution_reuse_blocker(&mut tx, &key, Some(&identity)).await?
        {
            // Discovery precedes the owner's fence. It may since have moved
            // to another checkout; never attribute its new execution to this
            // checkout or suggest cancelling it. This is the last lock on a
            // blocked path, so it cannot invert head-before-claim ordering.
            if !workspace_claim_still_owned_in_tx(&mut tx, &key, &identity).await? {
                return Ok(());
            }
            return Err(blocked(blocker));
        }
        // This conditional delete is a current write, not the discovery
        // snapshot. If another owner won meanwhile it cannot delete its claim.
        sqlx::query(
            "DELETE FROM session_execution_workspace_claims
             WHERE isolation_domain = ? AND owner_user_id = ? AND workspace_identity_hash = ?
             AND workspace_identity = ? AND session_id = ? AND branch_id = ?",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&identity_hash)
        .bind(&identity)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("release_idle_workspace_claim", source))?;
        tx.commit()
            .await
            .map_err(|source| database_error("commit_idle_workspace_release", source))?;
        connection.release();
        Ok(())
    }

    async fn release_idle_claim_for_current_binding(
        &self,
        key: &SessionKeyV1,
        expected_generation: Option<u64>,
    ) -> Result<(), SessionContextCoordinatorError> {
        let Some(expected) = expected_generation.filter(|generation| *generation != 0) else {
            return Ok(());
        };
        if let Some(binding) = self.load_execution_binding(key).await?
            && binding.generation == expected
            && binding.state == SessionExecutionBindingStateV1::Ready
        {
            self.release_idle_execution_workspace_claim(key, &binding)
                .await?;
        }
        Ok(())
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
        let target = self
            .load_execution_binding(key)
            .await?
            .unwrap_or_else(|| initial.clone());
        self.release_idle_execution_workspace_claim(key, &target)
            .await?;
        let mut connection = CancellationSafePoolConnection::acquire(self.pool.get())
            .await
            .map_err(|source| database_error("acquire_execution_binding_initialize", source))?;
        let mut tx = connection
            .begin()
            .await
            .map_err(|source| database_error("begin_execution_binding_initialize", source))?;
        ensure_database_state(&mut tx, key, AuthorityEpochsV1::default()).await?;
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

        // Existing Edge bindings may predate the workspace-claim table. Bring
        // their physical claim into the same transaction before returning so
        // initialization is also an admission boundary for multi-Session
        // callers.
        if let Some(binding) = load_execution_binding_in_tx(&mut tx, key, true).await? {
            if binding.logical_workspace_id != initial.logical_workspace_id {
                return Err(SessionContextCoordinatorError::NeedsRepair(
                    "Session execution binding belongs to another logical workspace".into(),
                ));
            }
            ensure_execution_workspace_claim_in_tx(&mut tx, key, &binding).await?;
            tx.commit()
                .await
                .map_err(|source| database_error("commit_execution_binding_existing", source))?;
            connection.release();
            return Ok(binding);
        }

        ensure_execution_workspace_claim_in_tx(&mut tx, key, initial).await?;
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

    /// Compare-and-swap one provider selection while holding the canonical
    /// Session head. A switch cannot cross an active turn, Run, or unresolved
    /// tool invocation, and callers cannot change the logical workspace.
    pub async fn compare_and_swap_execution_binding(
        &self,
        key: &SessionKeyV1,
        expected_generation: u64,
        next: &SessionExecutionBindingV1,
    ) -> Result<SessionExecutionBindingV1, SessionContextCoordinatorError> {
        key.validate()
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        next.validate()?;
        let next_generation = expected_generation.checked_add(1).ok_or_else(|| {
            SessionContextCoordinatorError::Invalid("execution binding generation overflow".into())
        })?;
        if next.generation != next_generation {
            return Err(SessionContextCoordinatorError::Invalid(
                "next execution binding must advance exactly one generation".into(),
            ));
        }

        self.release_idle_execution_workspace_claim(key, next)
            .await?;
        let mut connection = CancellationSafePoolConnection::acquire(self.pool.get())
            .await
            .map_err(|source| database_error("acquire_execution_binding_cas", source))?;
        let mut tx = connection
            .begin()
            .await
            .map_err(|source| database_error("begin_execution_binding_cas", source))?;
        ensure_database_state(&mut tx, key, AuthorityEpochsV1::default()).await?;
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
        let current = load_execution_binding_in_tx(&mut tx, key, true).await?;
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
        if current.logical_workspace_id != next.logical_workspace_id {
            return Err(SessionContextCoordinatorError::Invalid(
                "a Session execution binding cannot change logical workspace".into(),
            ));
        }
        let valid_state_transition = matches!(
            (current.state, next.state),
            (
                SessionExecutionBindingStateV1::Ready,
                SessionExecutionBindingStateV1::Switching
            ) | (
                SessionExecutionBindingStateV1::Switching,
                SessionExecutionBindingStateV1::Ready
            ) | (
                SessionExecutionBindingStateV1::Switching,
                SessionExecutionBindingStateV1::NeedsAttention
            ) | (
                SessionExecutionBindingStateV1::NeedsAttention,
                SessionExecutionBindingStateV1::Switching
            ) | (
                SessionExecutionBindingStateV1::NeedsAttention,
                SessionExecutionBindingStateV1::Ready
            )
        );
        if !valid_state_transition {
            return Err(SessionContextCoordinatorError::Invalid(
                "Session execution binding state transition is not allowed".into(),
            ));
        }
        // Selection changes serialize with Run admission on the canonical
        // Session head and lock this Session's binding row. Tool dispatch uses
        // an exact, non-locking binding read so parallel tool calls in one Run
        // do not serialize on the selection row. Its Run must retain an active
        // Session slot or unresolved invocation record until dispatch can no
        // longer start; the indexed evidence checks below enforce that fence.
        if session_execution_slot_exists(&mut tx, key).await?
            || unresolved_session_invocation_exists(&mut tx, key).await?
        {
            return Err(SessionContextCoordinatorError::ExecutionBindingBusy);
        }

        ensure_execution_workspace_claim_in_tx(&mut tx, key, next).await?;
        let binding_json = database_to_json("session_execution_binding", next)?;
        let updated = sqlx::query(
            "UPDATE session_execution_bindings \
             SET generation = ?, binding_json = ?, updated_at = NOW(6) \
             WHERE isolation_domain = ? AND owner_user_id = ? \
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
        .execute(&mut *tx)
        .await
        .map_err(|source| database_error("compare_and_swap_execution_binding", source))?
        .rows_affected();
        if updated != 1 {
            return Err(SessionContextCoordinatorError::ExecutionBindingFenced {
                expected: expected_generation,
                current: Some(current.generation),
            });
        }
        tx.commit()
            .await
            .map_err(|source| database_error("commit_execution_binding_cas", source))?;
        connection.release();
        Ok(next.clone())
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
        self.release_idle_execution_workspace_claim(key, &request.target)
            .await?;
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
            || session_execution_slot_exists(&mut tx, key).await?
            || unresolved_session_invocation_exists(&mut tx, key).await?
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
            || session_execution_slot_exists(&mut tx, key).await?
            || unresolved_session_invocation_exists(&mut tx, key).await?
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

    pub async fn list_authority_events(
        &self,
        key: &SessionKeyV1,
        limit: u32,
    ) -> Result<Vec<SessionAuthorityEventV1>, SessionContextCoordinatorError> {
        key.validate()
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        let rows = sqlx::query(
            "SELECT event_id, operation_kind, outcome, writer_epoch, actor_id, device_id,
                    lease_id, reservation_id, expected_root, observed_root,
                    authorization_epoch, device_trust_epoch, permission_epoch, created_at
             FROM session_context_authority_events
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ?
             ORDER BY created_at DESC, event_id DESC
             LIMIT ?",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .bind(i64::from(limit.clamp(1, 500)))
        .fetch_all(self.pool.get())
        .await
        .map_err(|source| database_error("list_authority_events", source))?;
        rows.into_iter()
            .map(|row| {
                let nonnegative = |column: &'static str| {
                    let value: i64 = row
                        .try_get(column)
                        .map_err(|source| database_error("decode_authority_event", source))?;
                    u64::try_from(value).map_err(|_| {
                        SessionContextCoordinatorError::NeedsRepair(format!(
                            "authority event column {column} is negative"
                        ))
                    })
                };
                Ok(SessionAuthorityEventV1 {
                    event_id: row
                        .try_get("event_id")
                        .map_err(|source| database_error("decode_authority_event", source))?,
                    operation_kind: row
                        .try_get("operation_kind")
                        .map_err(|source| database_error("decode_authority_event", source))?,
                    outcome: row
                        .try_get("outcome")
                        .map_err(|source| database_error("decode_authority_event", source))?,
                    writer_epoch: nonnegative("writer_epoch")?,
                    actor_id: row
                        .try_get("actor_id")
                        .map_err(|source| database_error("decode_authority_event", source))?,
                    device_id: row
                        .try_get("device_id")
                        .map_err(|source| database_error("decode_authority_event", source))?,
                    lease_id: row
                        .try_get("lease_id")
                        .map_err(|source| database_error("decode_authority_event", source))?,
                    reservation_id: row
                        .try_get("reservation_id")
                        .map_err(|source| database_error("decode_authority_event", source))?,
                    expected_root: row
                        .try_get("expected_root")
                        .map_err(|source| database_error("decode_authority_event", source))?,
                    observed_root: row
                        .try_get("observed_root")
                        .map_err(|source| database_error("decode_authority_event", source))?,
                    authorization_epoch: nonnegative("authorization_epoch")?,
                    device_trust_epoch: nonnegative("device_trust_epoch")?,
                    permission_epoch: nonnegative("permission_epoch")?,
                    created_at: row
                        .try_get("created_at")
                        .map_err(|source| database_error("decode_authority_event", source))?,
                })
            })
            .collect()
    }
}

#[async_trait]
impl SessionContextCoordinator for DatabaseSessionContextCoordinator {
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
        let fork_base = self.load_fork_prefix(&head.key).await?;
        let mut rows = sqlx::query(
            "SELECT manifest_json FROM conversation_manifest_nodes
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ?
               AND compaction_generation = ? AND reachable = 1",
        )
        .bind(&head.key.isolation_domain)
        .bind(&head.key.owner_user_id)
        .bind(&head.key.session_id)
        .bind(&head.key.branch_id)
        .bind(i64_from_u64(
            "head compaction generation",
            head.cursor.compaction_generation,
        )?)
        .fetch_all(self.pool.get())
        .await
        .map_err(|source| database_error("load_manifests", source))?;
        if let Some(prefix) = &fork_base
            && prefix.parent_cursor.compaction_generation == head.cursor.compaction_generation
        {
            rows.extend(
                sqlx::query(
                    "SELECT manifest_json FROM conversation_manifest_nodes
                     WHERE isolation_domain = ? AND owner_user_id = ?
                       AND session_id = ? AND branch_id = ?
                       AND compaction_generation = ? AND reachable = 1
                       AND conversation_seq <= ?",
                )
                .bind(&prefix.parent_key.isolation_domain)
                .bind(&prefix.parent_key.owner_user_id)
                .bind(&prefix.parent_key.session_id)
                .bind(&prefix.parent_key.branch_id)
                .bind(i64_from_u64(
                    "fork parent compaction generation",
                    prefix.parent_cursor.compaction_generation,
                )?)
                .bind(i64_from_u64(
                    "fork parent conversation sequence",
                    prefix.parent_cursor.conversation_seq,
                )?)
                .fetch_all(self.pool.get())
                .await
                .map_err(|source| database_error("load_fork_parent_manifests", source))?,
            );
        }
        let mut nodes = std::collections::HashMap::with_capacity(rows.len());
        for row in rows {
            let json = row
                .try_get::<String, _>("manifest_json")
                .map_err(|source| database_error("decode_manifest_row", source))?;
            let node: ContextManifestNodeV1 = database_json("manifest", &json)?;
            node.validate()
                .map_err(|error| SessionContextCoordinatorError::NeedsRepair(error.to_string()))?;
            if node.key != head.key
                && fork_base
                    .as_ref()
                    .is_none_or(|prefix| node.key != prefix.parent_key)
            {
                return Err(SessionContextCoordinatorError::NeedsRepair(
                    "database manifest owner or branch mismatch".into(),
                ));
            }
            nodes.insert(node.manifest_root.clone(), node);
        }

        let ordered_nodes = order_manifest_chain(head, nodes)?;
        let mut unique_hashes = HashSet::new();
        for node in &ordered_nodes {
            unique_hashes.extend(
                node.appended_segments
                    .iter()
                    .map(|segment| segment.segment_hash.clone()),
            );
        }
        let mut segment_map = self
            .load_database_segments(&head.key, unique_hashes.into_iter().collect())
            .await?;
        materialize_nodes(head, ordered_nodes, &mut segment_map)
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

    async fn load_segments(
        &self,
        key: &SessionKeyV1,
        segment_hashes: &[String],
    ) -> Result<Vec<ConversationSegmentV1>, SessionContextCoordinatorError> {
        validate_segment_batch(key, segment_hashes)?;
        let mut segments = self
            .load_database_segments(key, segment_hashes.to_vec())
            .await?;
        segment_hashes
            .iter()
            .map(|hash| {
                segments
                    .remove(hash)
                    .ok_or(SessionContextCoordinatorError::SegmentNotFound)
            })
            .collect()
    }

    async fn store_segments(
        &self,
        key: &SessionKeyV1,
        segments: &[ConversationSegmentV1],
    ) -> Result<(), SessionContextCoordinatorError> {
        validate_segment_upload(key, segments)?;
        self.persist_database_segments(key, segments).await
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
            "SELECT fork_base_json FROM session_context_heads
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ?",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| database_error("load_fork_prefix", source))?;
        let prefix = row
            .map(|row| {
                row.try_get::<Option<String>, _>("fork_base_json")
                    .map_err(|source| database_error("decode_fork_prefix", source))?
                    .as_deref()
                    .map(|json| database_json::<SharedManifestPrefixV1>("fork_prefix", json))
                    .transpose()
            })
            .transpose()?
            .flatten();
        if let Some(prefix) = &prefix {
            prefix
                .validate_for_child(key)
                .map_err(|error| SessionContextCoordinatorError::NeedsRepair(error.to_string()))?;
        }
        Ok(prefix)
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
               AND manifest_root = ? AND reachable = 1",
        )
        .bind(&manifest.parent_key.isolation_domain)
        .bind(&manifest.parent_key.owner_user_id)
        .bind(&manifest.parent_key.session_id)
        .bind(&manifest.parent_key.branch_id)
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
        ensure_database_state(&mut tx, key, actor.authority_epochs).await?;
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
        validate_ttl(ttl, MAX_RESERVATION_TTL)?;
        validate_idempotency_key(idempotency_key)?;
        validate_optional_cursor(&lease.key, expected_cursor)?;
        self.release_idle_claim_for_current_binding(
            &lease.key,
            expected_execution_binding_generation,
        )
        .await?;
        let mut connection = CancellationSafePoolConnection::acquire(self.pool.get())
            .await
            .map_err(|source| database_error("acquire_reserve_turn", source))?;
        let mut tx = connection
            .begin()
            .await
            .map_err(|source| database_error("begin_reserve_turn", source))?;
        let (mut state, now) = lock_database_state_at_now(&mut tx, &lease.key).await?;
        validate_execution_binding_generation_in_tx(
            &mut tx,
            &lease.key,
            expected_execution_binding_generation,
        )
        .await?;
        let request_hash = reservation_request_hash(lease, expected_cursor);
        if let Some(receipt) = load_database_receipt::<ReservationReceiptV1>(
            &mut tx,
            &lease.key,
            "reserve",
            idempotency_key,
            &request_hash,
        )
        .await?
        {
            record_database_authority_event(
                &mut tx,
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
            tx.commit()
                .await
                .map_err(|source| database_error("commit_reservation_replay", source))?;
            connection.release();
            return Ok(ReserveTurnOutcome::AlreadyReserved(receipt.reservation));
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
                update_database_state(&mut tx, &state).await?;
                record_database_authority_event(
                    &mut tx,
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
                tx.commit()
                    .await
                    .map_err(|source| database_error("commit_reservation_expiry_fence", source))?;
                connection.release();
                return Err(SessionContextCoordinatorError::Expired);
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
            update_database_state(&mut tx, &state).await?;
            record_database_authority_event(
                &mut tx,
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
            tx.commit()
                .await
                .map_err(|source| database_error("commit_reservation_retry", source))?;
            connection.release();
            return Ok(ReserveTurnOutcome::AlreadyReserved(refreshed));
        }
        if let Err(error) = validate_active_lease(&state, lease, now) {
            record_database_authority_event(
                &mut tx,
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
            tx.commit()
                .await
                .map_err(|source| database_error("commit_reserve_turn_audit", source))?;
            connection.release();
            return Err(error);
        }
        if state.head.as_ref().map(|head| &head.cursor) != expected_cursor {
            let outcome = ReserveTurnOutcome::Conflict {
                current_head: state.head.clone(),
            };
            record_database_authority_event(
                &mut tx,
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
            tx.commit()
                .await
                .map_err(|source| database_error("commit_reservation_conflict", source))?;
            connection.release();
            return Ok(outcome);
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
                &mut tx,
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
            tx.commit()
                .await
                .map_err(|source| database_error("commit_active_reservation_conflict", source))?;
            connection.release();
            return Ok(outcome);
        }
        if let Some(previous) = &state.active_reservation {
            archive_database_reservation(&mut tx, previous).await?;
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
        update_database_state(&mut tx, &state).await?;
        record_database_authority_event(
            &mut tx,
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
        tx.commit()
            .await
            .map_err(|source| database_error("commit_reserve_turn", source))?;
        connection.release();
        Ok(ReserveTurnOutcome::Reserved(reservation))
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
        validate_ttl(ttl, MAX_LEASE_TTL.min(MAX_RESERVATION_TTL))?;
        validate_idempotency_key(writer_idempotency_key)?;
        validate_idempotency_key(reservation_idempotency_key)?;
        actor
            .validate_for(key)
            .map_err(|_| SessionContextCoordinatorError::Unauthorized)?;
        validate_optional_cursor(key, expected_cursor)?;

        self.release_idle_claim_for_current_binding(key, expected_execution_binding_generation)
            .await?;
        let mut connection = CancellationSafePoolConnection::acquire(self.pool.get())
            .await
            .map_err(|source| database_error("acquire_and_reserve_turn", source))?;
        let mut tx = connection
            .begin()
            .await
            .map_err(|source| database_error("begin_acquire_and_reserve_turn", source))?;
        ensure_database_state(&mut tx, key, actor.authority_epochs).await?;
        let (mut state, now) = lock_database_state_at_now(&mut tx, key).await?;
        validate_execution_binding_generation_in_tx(
            &mut tx,
            key,
            expected_execution_binding_generation,
        )
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
                tx.commit().await.map_err(|source| {
                    database_error("commit_acquire_and_reserve_conflict", source)
                })?;
                connection.release();
                return Ok(AcquireWriterAndReserveTurnOutcome::WriterConflict {
                    current_head,
                    active_lease_expires_at_unix_ms,
                });
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
                tx.commit().await.map_err(|source| {
                    database_error("commit_acquire_and_reserve_conflict", source)
                })?;
                connection.release();
                return Ok(AcquireWriterAndReserveTurnOutcome::WriterConflict {
                    current_head,
                    active_lease_expires_at_unix_ms,
                });
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
                tx.commit().await.map_err(|source| {
                    database_error("commit_acquire_and_reserve_fenced", source)
                })?;
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
                update_database_state(&mut tx, &state).await?;
                record_database_authority_event(
                    &mut tx,
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
                tx.commit().await.map_err(|source| {
                    database_error("commit_acquire_and_reserve_expiry_fence", source)
                })?;
                connection.release();
                return Err(SessionContextCoordinatorError::Expired);
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
                    &mut tx,
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
                tx.commit().await.map_err(|source| {
                    database_error("commit_acquire_and_reserve_conflict", source)
                })?;
                connection.release();
                return Ok(AcquireWriterAndReserveTurnOutcome::ReservationConflict {
                    lease,
                    current_head,
                });
            }
            if let Some(previous) = &state.active_reservation {
                archive_database_reservation(&mut tx, previous).await?;
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

        update_database_state(&mut tx, &state).await?;
        record_database_authority_events(
            &mut tx,
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
        tx.commit()
            .await
            .map_err(|source| database_error("commit_acquire_and_reserve_turn", source))?;
        connection.release();
        Ok(AcquireWriterAndReserveTurnOutcome::Ready { lease, reservation })
    }

    async fn commit_turn(
        &self,
        reservation: &TurnReservationV1,
        delta: CanonicalTurnDeltaV1,
        idempotency_key: &str,
    ) -> Result<CoordinatorMutationV1, SessionContextCoordinatorError> {
        delta
            .validate()
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        validate_idempotency_key(idempotency_key)?;
        let request_hash = commit_request_hash(reservation, &delta);
        let mut segments = Vec::with_capacity(delta.logical_segments.len());
        for messages in delta.logical_segments.iter().cloned() {
            segments.push(
                ConversationSegmentV1::new(&reservation.key, messages)
                    .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?,
            );
        }
        let mut connection = CancellationSafePoolConnection::acquire(self.pool.get())
            .await
            .map_err(|source| database_error("acquire_commit_turn", source))?;
        let mut tx = connection
            .begin()
            .await
            .map_err(|source| database_error("begin_commit_turn", source))?;
        let (mut state, now) = lock_database_state_at_now(&mut tx, &reservation.key).await?;
        if let Some(last) = state.last_commit.clone()
            && last.idempotency_key == idempotency_key
        {
            validate_commit_request(&last, reservation, &delta)?;
            record_database_authority_event(
                &mut tx,
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
            tx.commit()
                .await
                .map_err(|source| database_error("commit_turn_retry", source))?;
            connection.release();
            return Ok(CoordinatorMutationV1::AlreadyApplied {
                cursor: last.cursor.clone(),
            });
        }
        if let Err(error) = validate_active_reservation(&state, reservation, now) {
            // Archived receipts are only relevant when this is not the current
            // active reservation. The successful hot path already proves that
            // the commit has not been applied, so avoid an extra database read.
            if let Some(receipt) = load_database_receipt::<CommitReceiptV1>(
                &mut tx,
                &reservation.key,
                "commit",
                idempotency_key,
                &request_hash,
            )
            .await?
            {
                record_database_authority_event(
                    &mut tx,
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
                tx.commit()
                    .await
                    .map_err(|source| database_error("commit_turn_replay", source))?;
                connection.release();
                return Ok(CoordinatorMutationV1::AlreadyApplied {
                    cursor: receipt.cursor,
                });
            }
            record_database_authority_event(
                &mut tx,
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
            tx.commit()
                .await
                .map_err(|source| database_error("commit_turn_rejection_audit", source))?;
            connection.release();
            return Err(error);
        }
        if state.head.as_ref().map(|head| &head.cursor) != reservation.expected_cursor.as_ref() {
            let current_cursor = state.head.as_ref().map(|head| head.cursor.clone());
            record_database_authority_event(
                &mut tx,
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
            tx.commit()
                .await
                .map_err(|source| database_error("commit_turn_conflict", source))?;
            connection.release();
            return Ok(CoordinatorMutationV1::Conflict {
                current_cursor,
                safe_options: vec![
                    CoordinatorConflictOptionV1::Refresh,
                    CoordinatorConflictOptionV1::Fork,
                ],
            });
        }
        validate_delta_advance(state.head.as_ref(), reservation, &delta)?;
        // Build and persist immutable canonical objects from the head held by
        // this transaction. This removes the duplicate pre-BEGIN head read
        // while retaining the same row-lock fence.
        let node =
            manifest_node_for_delta(&reservation.key, state.head.as_ref(), &delta, &segments)
                .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        let (total_canonical_bytes, total_message_count) =
            next_head_totals(state.head.as_ref(), &segments, delta.mode)?;
        self.persist_database_immutables_in_tx(
            &mut tx,
            &reservation.key,
            &segments,
            &node,
            total_canonical_bytes,
            total_message_count,
        )
        .await?;
        if let Some(previous) = &state.last_commit {
            archive_database_commit(&mut tx, previous).await?;
        }
        let cursor = node.cursor();
        state.head = Some(SessionContextHeadV1 {
            schema_version: SESSION_COORDINATION_SCHEMA_VERSION,
            key: reservation.key.clone(),
            cursor: cursor.clone(),
            latest_manifest_root: node.manifest_root,
            total_canonical_bytes,
            total_message_count,
            writer_epoch: reservation.writer_epoch,
        });
        state.last_commit = Some(CommitReceiptV1 {
            idempotency_key: idempotency_key.to_owned(),
            reservation_id: reservation.reservation_id.clone(),
            reservation: reservation.clone(),
            delta_hash: turn_delta_hash(&delta),
            cursor: cursor.clone(),
        });
        state.active_reservation = None;
        update_database_state(&mut tx, &state).await?;
        record_database_authority_event(
            &mut tx,
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
        tx.commit()
            .await
            .map_err(|source| database_error("commit_turn", source))?;
        connection.release();
        Ok(CoordinatorMutationV1::Applied { cursor })
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
    async fn load_database_segments(
        &self,
        key: &SessionKeyV1,
        hashes: Vec<String>,
    ) -> Result<
        std::collections::HashMap<String, ConversationSegmentV1>,
        SessionContextCoordinatorError,
    > {
        let mut segments = std::collections::HashMap::with_capacity(hashes.len());
        for chunk in hashes.chunks(256) {
            let mut query = QueryBuilder::<MySql>::new(
                "SELECT segment_hash, segment_json FROM conversation_segments \
                 INNER JOIN ",
            );
            push_matrixone_bound_string_set(&mut query, chunk.iter().map(String::as_str));
            query
                .push(" AS requested_segment ON requested_segment.value = segment_hash")
                .push(" WHERE isolation_domain = ")
                .push_bind(&key.isolation_domain)
                .push(" AND owner_user_id = ")
                .push_bind(&key.owner_user_id);
            let rows = query
                .build()
                .fetch_all(self.pool.get())
                .await
                .map_err(|source| database_error("load_segments", source))?;
            for row in rows {
                let stored_hash = row
                    .try_get::<String, _>("segment_hash")
                    .map_err(|source| database_error("decode_segment_hash", source))?;
                let json = row
                    .try_get::<String, _>("segment_json")
                    .map_err(|source| database_error("decode_segment_json", source))?;
                let segment: ConversationSegmentV1 = database_json("segment", &json)?;
                segment.validate_for(key).map_err(|error| {
                    SessionContextCoordinatorError::NeedsRepair(error.to_string())
                })?;
                if segment.segment_hash != stored_hash {
                    return Err(SessionContextCoordinatorError::NeedsRepair(
                        "database segment key does not match content".into(),
                    ));
                }
                segments.insert(stored_hash, segment);
            }
        }
        Ok(segments)
    }

    async fn persist_database_immutables_in_tx(
        &self,
        tx: &mut Transaction<'_, MySql>,
        key: &SessionKeyV1,
        segments: &[ConversationSegmentV1],
        node: &ContextManifestNodeV1,
        total_canonical_bytes: u64,
        total_message_count: u64,
    ) -> Result<(), SessionContextCoordinatorError> {
        self.persist_database_segments_in_tx(tx, key, segments)
            .await?;
        let canonical_segment_bytes =
            node.appended_segments
                .iter()
                .try_fold(0_u64, |total, segment| {
                    total.checked_add(segment.canonical_bytes).ok_or_else(|| {
                        SessionContextCoordinatorError::Invalid(
                            "manifest segment byte count overflow".into(),
                        )
                    })
                })?;
        let manifest_insert_sql = matrixone_statement_with_null_shape(
            "INSERT INTO conversation_manifest_nodes
             (isolation_domain, owner_user_id, session_id, branch_id, manifest_root,
              parent_manifest_root, completed_turn, conversation_seq,
              compaction_generation, canonical_segment_bytes, total_canonical_bytes,
              total_message_count, manifest_json, reachable)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1)",
            [node.parent_manifest_root.is_some()],
        );
        let manifest_already_exists = match sqlx::query(&manifest_insert_sql)
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
            .bind(i64_from_u64(
                "manifest total canonical bytes",
                total_canonical_bytes,
            )?)
            .bind(i64_from_u64(
                "manifest total message count",
                total_message_count,
            )?)
            .bind(database_to_json("manifest", node)?)
            .execute(&mut **tx)
            .await
        {
            Ok(_) => false,
            Err(source) if astra_core::is_duplicate_key_error(&source) => true,
            Err(source) => return Err(database_error("persist_manifest", source)),
        };
        let existing_manifest_reachable = if manifest_already_exists {
            let stored = sqlx::query(
                "SELECT parent_manifest_root, completed_turn, conversation_seq,
                        compaction_generation, canonical_segment_bytes,
                        total_canonical_bytes, total_message_count, manifest_json, reachable
                 FROM conversation_manifest_nodes
                 WHERE isolation_domain = ? AND owner_user_id = ?
                   AND session_id = ? AND branch_id = ? AND manifest_root = ?
                 FOR UPDATE",
            )
            .bind(&key.isolation_domain)
            .bind(&key.owner_user_id)
            .bind(&key.session_id)
            .bind(&key.branch_id)
            .bind(&node.manifest_root)
            .fetch_one(&mut **tx)
            .await
            .map_err(|source| database_error("verify_existing_manifest", source))?;
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
                || database_u64(&stored, "completed_turn")? != u64::from(node.completed_turn)
                || database_u64(&stored, "conversation_seq")? != node.conversation_seq
                || database_u64(&stored, "compaction_generation")? != node.compaction_generation
                || database_u64(&stored, "canonical_segment_bytes")? != canonical_segment_bytes
                || database_u64(&stored, "total_canonical_bytes")? != total_canonical_bytes
                || database_u64(&stored, "total_message_count")? != total_message_count
            {
                return Err(SessionContextCoordinatorError::NeedsRepair(
                    "existing immutable manifest does not match its content-addressed key".into(),
                ));
            }
            Some(database_u64(&stored, "reachable")?)
        } else {
            None
        };

        let mut insert_references = QueryBuilder::<MySql>::new(
            "INSERT IGNORE INTO conversation_manifest_segments
             (isolation_domain, owner_user_id, session_id, branch_id,
              manifest_root, segment_position, segment_hash) ",
        );
        insert_references.push_values(
            node.appended_segments.iter().enumerate(),
            |mut values, (position, segment)| {
                values
                    .push_bind(&key.isolation_domain)
                    .push_bind(&key.owner_user_id)
                    .push_bind(&key.session_id)
                    .push_bind(&key.branch_id)
                    .push_bind(&node.manifest_root)
                    .push_bind(i64::try_from(position).unwrap_or(i64::MAX))
                    .push_bind(&segment.segment_hash);
            },
        );
        let inserted_references = insert_references
            .build()
            .execute(&mut **tx)
            .await
            .map_err(|source| database_error("persist_manifest_segment_references", source))?;
        if manifest_already_exists
            || inserted_references.rows_affected()
                != u64::try_from(node.appended_segments.len()).unwrap_or(u64::MAX)
        {
            let stored_references = sqlx::query(
                "SELECT segment_position, segment_hash
                 FROM conversation_manifest_segments
                 WHERE isolation_domain = ? AND owner_user_id = ?
                   AND session_id = ? AND branch_id = ? AND manifest_root = ?
                 ORDER BY segment_position ASC FOR UPDATE",
            )
            .bind(&key.isolation_domain)
            .bind(&key.owner_user_id)
            .bind(&key.session_id)
            .bind(&key.branch_id)
            .bind(&node.manifest_root)
            .fetch_all(&mut **tx)
            .await
            .map_err(|source| database_error("verify_manifest_segment_references", source))?;
            if stored_references.len() != node.appended_segments.len() {
                return Err(SessionContextCoordinatorError::NeedsRepair(
                    "immutable manifest segment reference count is inconsistent".into(),
                ));
            }
            for (position, row) in stored_references.iter().enumerate() {
                let stored_position = database_u64(row, "segment_position")?;
                let stored_hash = row.try_get::<String, _>("segment_hash").map_err(|source| {
                    database_error("decode_manifest_segment_reference", source)
                })?;
                if stored_position != u64::try_from(position).unwrap_or(u64::MAX)
                    || stored_hash != node.appended_segments[position].segment_hash
                {
                    return Err(SessionContextCoordinatorError::NeedsRepair(
                        "immutable manifest segment reference does not match the manifest".into(),
                    ));
                }
            }
        }
        if let Some(reachable) = existing_manifest_reachable {
            match reachable {
                0 => {
                    let activated = sqlx::query(
                        "UPDATE conversation_manifest_nodes SET reachable = 1
                         WHERE isolation_domain = ? AND owner_user_id = ?
                           AND session_id = ? AND branch_id = ? AND manifest_root = ?
                           AND reachable = 0 AND compaction_generation = ?
                           AND canonical_segment_bytes = ? AND total_canonical_bytes = ?
                           AND total_message_count = ?",
                    )
                    .bind(&key.isolation_domain)
                    .bind(&key.owner_user_id)
                    .bind(&key.session_id)
                    .bind(&key.branch_id)
                    .bind(&node.manifest_root)
                    .bind(i64_from_u64(
                        "manifest compaction generation",
                        node.compaction_generation,
                    )?)
                    .bind(i64_from_u64(
                        "manifest segment bytes",
                        canonical_segment_bytes,
                    )?)
                    .bind(i64_from_u64(
                        "manifest total canonical bytes",
                        total_canonical_bytes,
                    )?)
                    .bind(i64_from_u64(
                        "manifest total message count",
                        total_message_count,
                    )?)
                    .execute(&mut **tx)
                    .await
                    .map_err(|source| database_error("activate_existing_manifest", source))?;
                    if activated.rows_affected() != 1 {
                        return Err(SessionContextCoordinatorError::NeedsRepair(
                            "verified staged manifest could not be activated".into(),
                        ));
                    }
                }
                1 => {}
                _ => {
                    return Err(SessionContextCoordinatorError::NeedsRepair(
                        "existing immutable manifest has an invalid reachability state".into(),
                    ));
                }
            }
        }
        Ok(())
    }

    async fn persist_database_segments_in_tx(
        &self,
        tx: &mut Transaction<'_, MySql>,
        key: &SessionKeyV1,
        segments: &[ConversationSegmentV1],
    ) -> Result<(), SessionContextCoordinatorError> {
        for segment in segments {
            let json = database_to_json("segment", segment)?;
            let result = sqlx::query(
                "INSERT IGNORE INTO conversation_segments
                 (isolation_domain, owner_user_id, segment_hash, canonical_root_hash,
                  canonical_bytes, message_count, segment_json)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&key.isolation_domain)
            .bind(&key.owner_user_id)
            .bind(&segment.segment_hash)
            .bind(&segment.canonical_root_hash)
            .bind(i64_from_u64("segment bytes", segment.canonical_bytes)?)
            .bind(i64::from(segment.message_count))
            .bind(json)
            .execute(&mut **tx)
            .await
            .map_err(|source| database_error("persist_segment", source))?;
            if result.rows_affected() == 0 {
                let stored = sqlx::query(
                    "SELECT segment_json FROM conversation_segments
                     WHERE isolation_domain = ? AND owner_user_id = ? AND segment_hash = ?
                     FOR UPDATE",
                )
                .bind(&key.isolation_domain)
                .bind(&key.owner_user_id)
                .bind(&segment.segment_hash)
                .fetch_one(&mut **tx)
                .await
                .map_err(|source| database_error("verify_existing_segment", source))?
                .try_get::<String, _>("segment_json")
                .map_err(|source| database_error("decode_existing_segment", source))?;
                let stored: ConversationSegmentV1 = database_json("existing_segment", &stored)?;
                if stored != *segment {
                    return Err(SessionContextCoordinatorError::NeedsRepair(
                        "existing immutable segment does not match its content-addressed key"
                            .into(),
                    ));
                }
            }
        }
        Ok(())
    }

    async fn persist_database_segments(
        &self,
        key: &SessionKeyV1,
        segments: &[ConversationSegmentV1],
    ) -> Result<(), SessionContextCoordinatorError> {
        for segment in segments {
            let json = database_to_json("segment", segment)?;
            let result = sqlx::query(
                "INSERT IGNORE INTO conversation_segments
                 (isolation_domain, owner_user_id, segment_hash, canonical_root_hash,
                  canonical_bytes, message_count, segment_json)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&key.isolation_domain)
            .bind(&key.owner_user_id)
            .bind(&segment.segment_hash)
            .bind(&segment.canonical_root_hash)
            .bind(i64_from_u64("segment bytes", segment.canonical_bytes)?)
            .bind(i64::from(segment.message_count))
            .bind(json)
            .execute(self.pool.get())
            .await
            .map_err(|source| database_error("persist_segment", source))?;
            if result.rows_affected() == 0 {
                let stored = sqlx::query(
                    "SELECT segment_json FROM conversation_segments
                     WHERE isolation_domain = ? AND owner_user_id = ? AND segment_hash = ?",
                )
                .bind(&key.isolation_domain)
                .bind(&key.owner_user_id)
                .bind(&segment.segment_hash)
                .fetch_one(self.pool.get())
                .await
                .map_err(|source| database_error("verify_existing_segment", source))?
                .try_get::<String, _>("segment_json")
                .map_err(|source| database_error("decode_existing_segment", source))?;
                let stored: ConversationSegmentV1 = database_json("existing_segment", &stored)?;
                if stored != *segment {
                    return Err(SessionContextCoordinatorError::NeedsRepair(
                        "existing immutable segment does not match its content-addressed key"
                            .into(),
                    ));
                }
            }
        }
        Ok(())
    }
}

fn order_manifest_chain(
    head: &SessionContextHeadV1,
    mut nodes: std::collections::HashMap<String, ContextManifestNodeV1>,
) -> Result<Vec<ContextManifestNodeV1>, SessionContextCoordinatorError> {
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
        root = if node.replaces_history {
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
    nodes: Vec<ContextManifestNodeV1>,
    segments: &mut std::collections::HashMap<String, ConversationSegmentV1>,
) -> Result<MaterializedConversationV1, SessionContextCoordinatorError> {
    let mut use_counts = std::collections::HashMap::<String, usize>::new();
    for node in &nodes {
        for segment in &node.appended_segments {
            *use_counts.entry(segment.segment_hash.clone()).or_default() += 1;
        }
    }
    let mut messages = Vec::new();
    let mut logical_segment_count = 0_u64;
    let mut canonical_segment_bytes = 0_u64;
    let mut prior_cursor = None;
    for node in nodes {
        validate_manifest_advance(prior_cursor.as_ref(), &node)?;
        let cursor = node.cursor();
        for segment_ref in node.appended_segments {
            let remaining = use_counts
                .get_mut(&segment_ref.segment_hash)
                .ok_or_else(|| {
                    SessionContextCoordinatorError::NeedsRepair(
                        "manifest segment use count is missing".into(),
                    )
                })?;
            *remaining -= 1;
            if *remaining == 0 {
                let segment = segments.remove(&segment_ref.segment_hash).ok_or_else(|| {
                    SessionContextCoordinatorError::NeedsRepair(format!(
                        "missing segment {}",
                        segment_ref.segment_hash
                    ))
                })?;
                if segment.reference() != segment_ref {
                    return Err(SessionContextCoordinatorError::NeedsRepair(
                        "segment metadata does not match manifest reference".into(),
                    ));
                }
                canonical_segment_bytes = canonical_segment_bytes
                    .checked_add(segment.canonical_bytes)
                    .ok_or_else(|| {
                        SessionContextCoordinatorError::NeedsRepair(
                            "materialized byte count overflow".into(),
                        )
                    })?;
                messages.extend(segment.messages);
            } else {
                let segment = segments.get(&segment_ref.segment_hash).ok_or_else(|| {
                    SessionContextCoordinatorError::NeedsRepair(format!(
                        "missing segment {}",
                        segment_ref.segment_hash
                    ))
                })?;
                if segment.reference() != segment_ref {
                    return Err(SessionContextCoordinatorError::NeedsRepair(
                        "segment metadata does not match manifest reference".into(),
                    ));
                }
                canonical_segment_bytes = canonical_segment_bytes
                    .checked_add(segment.canonical_bytes)
                    .ok_or_else(|| {
                        SessionContextCoordinatorError::NeedsRepair(
                            "materialized byte count overflow".into(),
                        )
                    })?;
                messages.extend(segment.messages.iter().cloned());
            }
            logical_segment_count = logical_segment_count.checked_add(1).ok_or_else(|| {
                SessionContextCoordinatorError::NeedsRepair(
                    "materialized segment count overflow".into(),
                )
            })?;
        }
        prior_cursor = Some(cursor);
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

async fn ensure_database_state(
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

async fn lock_database_state(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
) -> Result<CoordinatorStateV1, SessionContextCoordinatorError> {
    lock_database_state_at_now(tx, key)
        .await
        .map(|(state, _)| state)
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

async fn workspace_claim_still_owned_in_tx(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
    identity: &str,
) -> Result<bool, SessionContextCoordinatorError> {
    let current = sqlx::query(
        "SELECT 1 FROM session_execution_workspace_claims
         WHERE isolation_domain = ? AND owner_user_id = ? AND workspace_identity_hash = ?
           AND workspace_identity = ? AND session_id = ? AND branch_id = ? FOR UPDATE",
    )
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(execution_workspace_identity_hash(identity))
    .bind(identity)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|source| database_error("recheck_workspace_blocker_owner", source))?;
    Ok(current.is_some())
}

/// One owner-scoped current-read proof for cancellation and checkout reuse.
/// Never hold a claimant's Session head while establishing this proof.
async fn locked_execution_reuse_blocker(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
    expected_workspace_identity: Option<&str>,
) -> Result<Option<WorkspaceReuseBlocker>, SessionContextCoordinatorError> {
    match crate::storage::admit_session_execution_write(tx, &key.session_id, &key.owner_user_id)
        .await
    {
        Ok(()) => {}
        Err(sqlx::Error::RowNotFound) => return Ok(Some(WorkspaceReuseBlocker::OwnerUnavailable)),
        Err(source) => return Err(database_error("fence_idle_workspace_session", source)),
    }
    let slot = sqlx::query("SELECT 1 FROM agent_session_execution_slots WHERE user_id = ? AND session_id = ? LIMIT 1 FOR UPDATE")
        .bind(&key.owner_user_id).bind(&key.session_id).fetch_optional(&mut **tx).await
        .map_err(|source| database_error("lock_idle_workspace_slot", source))?;
    if slot.is_some() {
        return Ok(Some(WorkspaceReuseBlocker::ExecutionSlot));
    }
    // Terminal executors may still be unwinding. Their owner lease remains
    // relevant even though active-status discovery no longer includes them.
    let running = sqlx::query(
        "SELECT 1 FROM agent_runs WHERE user_id = ? AND session_id = ?
         AND (status IN ('running', 'waiting') OR (status = 'paused' AND waiting_for IS NOT NULL)
              OR (owner_pod_id IS NOT NULL AND owner_lease_expires_at >= NOW(6)))
         LIMIT 1 FOR UPDATE",
    )
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|source| database_error("lock_idle_workspace_runs", source))?;
    if running.is_some() {
        return Ok(Some(WorkspaceReuseBlocker::ActiveRun));
    }
    // An executor may release its lease after failed closure. The durable
    // generation-scoped fence survives pod loss and is still execution debt.
    let settling = sqlx::query(
        "SELECT run_id, run_generation FROM agent_runs r WHERE user_id = ? AND session_id = ?
         AND EXISTS (SELECT 1 FROM agent_run_events e WHERE e.user_id = r.user_id AND e.run_id = r.run_id
                     AND e.idempotency_key = CONCAT('run-settlement-started:', r.run_generation))
         AND NOT EXISTS (SELECT 1 FROM agent_run_events e WHERE e.user_id = r.user_id AND e.run_id = r.run_id
                         AND e.idempotency_key IN (CONCAT('run-settlement-finished:', r.run_generation), CONCAT('run-accounting-finalized:', r.run_generation))) FOR UPDATE",
    ).bind(&key.owner_user_id).bind(&key.session_id).fetch_all(&mut **tx).await
        .map_err(|source| database_error("lock_idle_workspace_settlement_runs", source))?;
    for run in settling {
        let run_id: String = run
            .try_get("run_id")
            .map_err(|source| database_error("decode_idle_settlement_run", source))?;
        let generation = database_u64(&run, "run_generation")?;
        if crate::runs::run_has_open_settlement_in_tx(tx, &key.owner_user_id, &run_id, generation)
            .await
            .map_err(|source| database_error("lock_idle_workspace_settlement", source))?
        {
            return Ok(Some(WorkspaceReuseBlocker::SettlementPending));
        }
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
            return Ok(Some(WorkspaceReuseBlocker::WriterOrReservation));
        }
    } else if expected_workspace_identity.is_some() {
        return Ok(Some(WorkspaceReuseBlocker::OwnerUnavailable));
    }
    let binding_identity = match load_execution_binding_in_tx(tx, key, true).await? {
        Some(binding) => {
            if !head_exists {
                return Ok(Some(WorkspaceReuseBlocker::OwnerUnavailable));
            }
            if binding.state != SessionExecutionBindingStateV1::Ready
                || expected_workspace_identity.is_some_and(|identity| {
                    execution_workspace_identity(&binding).as_deref() != Some(identity)
                })
            {
                return Ok(Some(WorkspaceReuseBlocker::BindingNotReady));
            }
            execution_workspace_identity(&binding)
        }
        None => {
            let claim = sqlx::query("SELECT 1 FROM session_execution_workspace_claims WHERE isolation_domain = ? AND owner_user_id = ? AND session_id = ? AND branch_id = ? LIMIT 1")
                .bind(&key.isolation_domain).bind(&key.owner_user_id).bind(&key.session_id).bind(&key.branch_id)
                .fetch_optional(&mut **tx).await.map_err(|source| database_error("read_idle_workspace_binding_claim", source))?;
            if expected_workspace_identity.is_some() || claim.is_some() {
                return Ok(Some(WorkspaceReuseBlocker::OwnerUnavailable));
            }
            None
        }
    };
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
        return Ok(Some(WorkspaceReuseBlocker::UnresolvedTool));
    }
    if expected_workspace_identity.is_none() {
        // Cancellation has no claimant-supplied physical identity. Check any
        // retained claim against the same binding before saying it is reusable.
        // This is the final lock; do not acquire another Session/head after it.
        let claim: Option<String> = sqlx::query_scalar(
            "SELECT workspace_identity FROM session_execution_workspace_claims WHERE isolation_domain = ? AND owner_user_id = ? AND session_id = ? AND branch_id = ? LIMIT 1 FOR UPDATE",
        ).bind(&key.isolation_domain).bind(&key.owner_user_id).bind(&key.session_id).bind(&key.branch_id)
            .fetch_optional(&mut **tx).await.map_err(|source| database_error("lock_idle_workspace_binding_claim", source))?;
        if claim
            .as_ref()
            .is_some_and(|identity| binding_identity.as_ref() != Some(identity))
        {
            return Ok(Some(WorkspaceReuseBlocker::BindingNotReady));
        }
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
    pub(crate) has_active_reservation: bool,
    pub(crate) has_unresolved_invocations: bool,
}

pub(crate) async fn lock_recovery_context_in_transaction(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
) -> Result<RecoveryContextFactsV1, SessionContextCoordinatorError> {
    key.validate()
        .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
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
    let row = sqlx::query(&sql)
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
    let row = sqlx::query(&sql)
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

fn execution_workspace_root(binding: &SessionExecutionBindingV1) -> Option<String> {
    if binding.workspace.kind == crate::runs::WorkspaceBindingRequestKind::EdgeWorkspace {
        binding
            .workspace
            .root
            .as_deref()
            .map(str::trim)
            .filter(|root| !root.is_empty())
            .map(ToOwned::to_owned)
    } else {
        None
    }
}

fn execution_workspace_identity(binding: &SessionExecutionBindingV1) -> Option<String> {
    let root = execution_workspace_root(binding)?;
    let provider_scope = binding
        .physical_workspace_id
        .as_deref()
        .map(str::trim)
        .filter(|scope| !scope.is_empty())?;
    Some(format!("{provider_scope}\0{root}"))
}

fn execution_workspace_identity_hash(identity: &str) -> String {
    format!("{:x}", Sha256::digest(identity.as_bytes()))
}

fn ordered_execution_claim_hashes(previous_hash: Option<&str>, next_hash: &str) -> Vec<String> {
    let mut hashes = Vec::with_capacity(2);
    if let Some(previous_hash) = previous_hash {
        hashes.push(previous_hash.to_owned());
    }
    hashes.push(next_hash.to_owned());
    hashes.sort_unstable();
    hashes.dedup();
    hashes
}

/// Reserve an Edge checkout for one executing Session at a time. Idle claims
/// may be retired under their owner's canonical execution fence. The claim is
/// keyed by a hash so long paths remain indexable; the full identity is
/// retained and compared after the lock to make a hash collision fail closed.
/// Claims move with the binding in the same transaction, so two Sessions
/// racing for one directory cannot both commit a Ready/Switching selection,
/// even when they use different Edge connection IDs.
pub(crate) async fn ensure_execution_workspace_claim_in_tx(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
    next: &SessionExecutionBindingV1,
) -> Result<(), SessionContextCoordinatorError> {
    let is_edge_binding = matches!(
        (next.workspace.kind, next.executor.kind),
        (
            crate::runs::WorkspaceBindingRequestKind::EdgeWorkspace,
            crate::runs::ExecutorBindingRequestKind::EdgeAgent
        )
    );
    if is_edge_binding && next.physical_workspace_id.is_none() {
        return Err(SessionContextCoordinatorError::NeedsRepair(
            "Edge execution binding has no authenticated physical workspace identity".into(),
        ));
    }
    let Some(identity) = execution_workspace_identity(next) else {
        sqlx::query(
            "DELETE FROM session_execution_workspace_claims
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ?",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .bind(&key.branch_id)
        .execute(&mut **tx)
        .await
        .map_err(|source| database_error("release_execution_workspace_claim", source))?;
        return Ok(());
    };
    let identity_hash = execution_workspace_identity_hash(&identity);
    // A Session may move from one Edge materialization to another. Read its
    // current claim before changing it, then lock the old and new claim keys in
    // canonical order. Two Sessions swapping checkouts therefore wait in the
    // same order instead of deadlocking on opposite unique-key locks.
    let previous_hash = sqlx::query_scalar::<_, String>(
        "SELECT workspace_identity_hash
         FROM session_execution_workspace_claims
         WHERE isolation_domain = ? AND owner_user_id = ?
           AND session_id = ? AND branch_id = ?
         LIMIT 1",
    )
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|source| database_error("read_execution_workspace_claim", source))?;
    for claim_hash in ordered_execution_claim_hashes(previous_hash.as_deref(), &identity_hash) {
        sqlx::query(
            "SELECT workspace_identity_hash
             FROM session_execution_workspace_claims
             WHERE isolation_domain = ? AND owner_user_id = ?
               AND workspace_identity_hash = ?
             FOR UPDATE",
        )
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(claim_hash)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|source| database_error("lock_execution_workspace_claim_ordered", source))?;
    }
    // The per-Session unique key would otherwise make `INSERT IGNORE` hide the
    // old row and the subsequent lookup could never see the new workspace.
    sqlx::query(
        "DELETE FROM session_execution_workspace_claims
         WHERE isolation_domain = ? AND owner_user_id = ?
           AND session_id = ? AND branch_id = ?
           AND workspace_identity_hash <> ?",
    )
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .bind(&identity_hash)
    .execute(&mut **tx)
    .await
    .map_err(|source| database_error("release_stale_execution_workspace_claim", source))?;
    sqlx::query(
        "INSERT IGNORE INTO session_execution_workspace_claims
         (isolation_domain, owner_user_id, workspace_identity_hash, workspace_identity,
          session_id, branch_id, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, NOW(6))",
    )
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&identity_hash)
    .bind(&identity)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .execute(&mut **tx)
    .await
    .map_err(|source| database_error("claim_execution_workspace", source))?;
    let existing = sqlx::query(
        "SELECT workspace_identity, session_id, branch_id
         FROM session_execution_workspace_claims
         WHERE isolation_domain = ? AND owner_user_id = ?
           AND workspace_identity_hash = ?
         FOR UPDATE",
    )
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&identity_hash)
    .fetch_one(&mut **tx)
    .await
    .map_err(|source| database_error("lock_execution_workspace_claim", source))?;
    let existing_identity = existing
        .try_get::<String, _>("workspace_identity")
        .map_err(|source| database_error("decode_execution_workspace_identity", source))?;
    let existing_session = existing
        .try_get::<String, _>("session_id")
        .map_err(|source| database_error("decode_execution_workspace_session", source))?;
    let existing_branch = existing
        .try_get::<String, _>("branch_id")
        .map_err(|source| database_error("decode_execution_workspace_branch", source))?;
    if existing_identity != identity
        || existing_session != key.session_id
        || existing_branch != key.branch_id
    {
        return Err(SessionContextCoordinatorError::ExecutionWorkspaceClaimed {
            owner_session_id: existing_session,
            owner_branch_id: existing_branch,
            blocker: WorkspaceReuseBlocker::ClaimChanged,
        });
    }
    Ok(())
}

/// Run creation/resume holds the canonical Session execution fence. Recheck
/// its selection using current reads so delayed admission cannot execute after
/// an idle Session has relinquished the checkout. This is not a tool hot path.
pub(crate) async fn ensure_run_execution_workspace_claim_in_tx(
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
    if let Some(binding) = load_execution_binding_in_tx(tx, &key, true).await? {
        if binding.state != SessionExecutionBindingStateV1::Ready {
            return Err(SessionContextCoordinatorError::ExecutionBindingNotReady(
                binding.state,
            ));
        }
        ensure_execution_workspace_claim_in_tx(tx, &key, &binding).await?;
    }
    Ok(())
}

/// Verify the claim that admission already established without taking a row
/// lock or issuing a write. Tool dispatch runs once per invocation and may
/// fan out many independent calls; it must only observe the immutable
/// binding/claim pair while the Session execution slot fences handoff.
pub(crate) async fn verify_execution_workspace_claim_in_tx(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
    binding: &SessionExecutionBindingV1,
) -> Result<(), SessionContextCoordinatorError> {
    let Some(identity) = execution_workspace_identity(binding) else {
        return Ok(());
    };
    let identity_hash = execution_workspace_identity_hash(&identity);
    let row = sqlx::query(
        "SELECT workspace_identity, session_id, branch_id
         FROM session_execution_workspace_claims
         WHERE isolation_domain = ? AND owner_user_id = ?
           AND workspace_identity_hash = ?
         LIMIT 1",
    )
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&identity_hash)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|source| database_error("verify_execution_workspace_claim", source))?;
    let Some(row) = row else {
        return Err(SessionContextCoordinatorError::ExecutionBindingBusy);
    };
    let existing_identity = row
        .try_get::<String, _>("workspace_identity")
        .map_err(|source| database_error("decode_execution_workspace_identity", source))?;
    let existing_session = row
        .try_get::<String, _>("session_id")
        .map_err(|source| database_error("decode_execution_workspace_session", source))?;
    let existing_branch = row
        .try_get::<String, _>("branch_id")
        .map_err(|source| database_error("decode_execution_workspace_branch", source))?;
    if existing_identity != identity
        || existing_session != key.session_id
        || existing_branch != key.branch_id
    {
        return Err(SessionContextCoordinatorError::ExecutionWorkspaceClaimed {
            owner_session_id: existing_session,
            owner_branch_id: existing_branch,
            blocker: WorkspaceReuseBlocker::ClaimChanged,
        });
    }
    Ok(())
}

async fn update_execution_binding_in_tx(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
    expected_generation: u64,
    next: &SessionExecutionBindingV1,
) -> Result<(), SessionContextCoordinatorError> {
    ensure_execution_workspace_claim_in_tx(tx, key, next).await?;
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
    ensure_execution_workspace_claim_in_tx(tx, key, &current).await?;
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

async fn session_execution_slot_exists(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
) -> Result<bool, SessionContextCoordinatorError> {
    let row = sqlx::query(
        "SELECT 1 FROM agent_session_execution_slots \
         WHERE user_id = ? AND session_id = ? LIMIT 1",
    )
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|source| database_error("check_session_execution_slot", source))?;
    Ok(row.is_some())
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
    let result = sqlx::query(&update_sql)
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

/// Exact immutable receipt lookup for the recovery cold path. No canonical
/// head or historical adoption chain is scanned while holding run locks.
pub(crate) async fn execution_adoption_receipt_matches_tx(
    tx: &mut Transaction<'_, MySql>,
    run: &crate::runs::DurableRunRecord,
    adoption: &crate::runs::ExecutionHandoffAdoption,
) -> Result<bool, SessionContextCoordinatorError> {
    if adoption.key.owner_user_id != run.user_id
        || adoption.key.session_id != run.session_id
        || adoption.run_generation != run.run_generation
    {
        return Ok(false);
    }
    let payload: Option<String> = sqlx::query_scalar(
        "SELECT receipt_json FROM session_context_operation_receipts
         WHERE isolation_domain = ? AND owner_user_id = ? AND session_id = ? AND branch_id = ?
         AND operation_kind = 'adopt_execution_turn' AND idempotency_hash = ? FOR UPDATE",
    )
    .bind(&adoption.key.isolation_domain)
    .bind(&adoption.key.owner_user_id)
    .bind(&adoption.key.session_id)
    .bind(&adoption.key.branch_id)
    .bind(hash_receipt(
        "adopt_execution_turn",
        &adoption.receipt_idempotency_key,
    ))
    .fetch_optional(&mut **tx)
    .await
    .map_err(|source| database_error("load_execution_adoption_provenance", source))?;
    let Some(payload) = payload else {
        return Ok(false);
    };
    let receipt: ExecutionTurnAdoptionReceipt = match serde_json::from_str(&payload) {
        Ok(receipt) => receipt,
        Err(_) => {
            tracing::warn!(run_id = %run.run_id, "invalid adoption receipt cannot establish checkpoint custody");
            return Ok(false);
        }
    };
    Ok(receipt.run_id == run.run_id
        && receipt.run_generation == run.run_generation
        && receipt.producer_generation == adoption.producer_generation
        && receipt.checkpoint_id == adoption.checkpoint_id
        && receipt.source.key == adoption.key
        && receipt.writer_lease.key == adoption.key
        && receipt.turn_reservation.key == adoption.key
        && receipt.source.reservation_id == adoption.source_reservation_id
        && receipt.turn_reservation.reservation_id == adoption.adopted_reservation_id
        && receipt.turn_reservation.reserved_turn == receipt.source.reserved_turn
        && receipt.turn_reservation.expected_cursor == receipt.source.expected_cursor
        && receipt.turn_reservation.lease_id == receipt.writer_lease.lease_id
        && receipt.turn_reservation.writer_epoch == receipt.writer_lease.writer_epoch)
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
    head: Option<&SessionContextHeadV1>,
    delta: &CanonicalTurnDeltaV1,
    segments: &[ConversationSegmentV1],
) -> Result<ContextManifestNodeV1, SessionCoordinationValidationError> {
    let parent = head.map(|head| head.latest_manifest_root.clone());
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

fn validate_segment_batch(
    key: &SessionKeyV1,
    segment_hashes: &[String],
) -> Result<(), SessionContextCoordinatorError> {
    key.validate()
        .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
    if segment_hashes.is_empty() || segment_hashes.len() > MAX_SEGMENT_BATCH {
        return Err(SessionContextCoordinatorError::Invalid(format!(
            "segment batch must contain between 1 and {MAX_SEGMENT_BATCH} identities"
        )));
    }
    let mut unique = HashSet::with_capacity(segment_hashes.len());
    for hash in segment_hashes {
        validate_optional_manifest_root(Some(hash))?;
        if !unique.insert(hash) {
            return Err(SessionContextCoordinatorError::Invalid(
                "segment batch contains duplicate identities".into(),
            ));
        }
    }
    Ok(())
}

fn validate_segment_upload(
    key: &SessionKeyV1,
    segments: &[ConversationSegmentV1],
) -> Result<(), SessionContextCoordinatorError> {
    let hashes = segments
        .iter()
        .map(|segment| segment.segment_hash.clone())
        .collect::<Vec<_>>();
    validate_segment_batch(key, &hashes)?;
    let mut total_bytes = 0_u64;
    for segment in segments {
        segment
            .validate_for(key)
            .map_err(|error| SessionContextCoordinatorError::Invalid(error.to_string()))?;
        if segment.canonical_bytes > MAX_STAGED_SEGMENT_BYTES {
            return Err(SessionContextCoordinatorError::Invalid(format!(
                "conversation segment exceeds {MAX_STAGED_SEGMENT_BYTES} canonical bytes"
            )));
        }
        total_bytes = total_bytes
            .checked_add(segment.canonical_bytes)
            .ok_or_else(|| {
                SessionContextCoordinatorError::Invalid(
                    "conversation segment batch byte count overflow".into(),
                )
            })?;
    }
    if total_bytes > MAX_STAGED_BATCH_BYTES {
        return Err(SessionContextCoordinatorError::Invalid(format!(
            "conversation segment batch exceeds {MAX_STAGED_BATCH_BYTES} canonical bytes"
        )));
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

/// Construct replacement authority after the enclosing transaction has proved
/// run/checkpoint custody. This does not mutate state or grant authority until
/// the caller atomically commits the canonical receipt and run adoption event.
fn prepare_adopted_turn_authority(
    state: &CoordinatorStateV1,
    source: &TurnReservationV1,
    actor: &ActorContextV1,
    now: i64,
    ttl: Duration,
    idempotency_key: &str,
) -> Result<(ConversationWriterLeaseV1, TurnReservationV1), SessionContextCoordinatorError> {
    validate_ttl(ttl, MAX_RESERVATION_TTL)?;
    validate_idempotency_key(idempotency_key)?;
    actor
        .validate_for(&state.key)
        .map_err(|_| SessionContextCoordinatorError::Unauthorized)?;
    if source.key != state.key || actor.authority_epochs != state.authority_epochs {
        return Err(SessionContextCoordinatorError::Fenced);
    }
    let next_turn = state
        .head
        .as_ref()
        .map_or(Some(1), |head| head.cursor.completed_turn.checked_add(1));
    if state.head.as_ref().map(|head| &head.cursor) != source.expected_cursor.as_ref()
        || next_turn != Some(source.reserved_turn)
        || state
            .active_writer
            .as_ref()
            .is_some_and(|lease| lease.expires_at_unix_ms > now)
        || state
            .active_reservation
            .as_ref()
            .is_some_and(|reservation| reservation.expires_at_unix_ms > now)
    {
        return Err(SessionContextCoordinatorError::Fenced);
    }
    let writer_epoch = state.writer_epoch.checked_add(1).ok_or_else(|| {
        SessionContextCoordinatorError::NeedsRepair("writer epoch overflow".into())
    })?;
    let expires_at_unix_ms = checked_expiry(now, ttl)?;
    let lease = ConversationWriterLeaseV1 {
        schema_version: SESSION_COORDINATION_SCHEMA_VERSION,
        key: state.key.clone(),
        lease_id: Uuid::new_v4().to_string(),
        writer_epoch,
        actor: actor.clone(),
        expected_cursor: source.expected_cursor.clone(),
        acquired_at_unix_ms: now,
        expires_at_unix_ms,
        idempotency_key: idempotency_key.into(),
    };
    let reservation = TurnReservationV1 {
        schema_version: SESSION_COORDINATION_SCHEMA_VERSION,
        reservation_id: Uuid::new_v4().to_string(),
        key: state.key.clone(),
        lease_id: lease.lease_id.clone(),
        writer_epoch,
        expected_cursor: source.expected_cursor.clone(),
        reserved_turn: source.reserved_turn,
        created_at_unix_ms: now,
        expires_at_unix_ms,
        idempotency_key: idempotency_key.into(),
    };
    Ok((lease, reservation))
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
    reservation: &TurnReservationV1,
    delta: &CanonicalTurnDeltaV1,
) -> Result<(), SessionContextCoordinatorError> {
    if delta.schema_version != CANONICAL_TURN_DELTA_SCHEMA_VERSION
        || delta.completed_turn != reservation.reserved_turn
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
    digest.update((delta.logical_segments.len() as u64).to_be_bytes());
    for messages in &delta.logical_segments {
        hash_field(&mut digest, &canonical_conversation_root(messages));
        digest.update(canonical_conversation_serialized_len(messages).to_be_bytes());
        digest.update((messages.len() as u64).to_be_bytes());
    }
    format!("{:x}", digest.finalize())
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

    #[tokio::test]
    #[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
    async fn workspace_blocker_rechecks_owner_after_discovery_race() {
        let _ = dotenvy::dotenv();
        assert_eq!(std::env::var("ASTRA_TEST_DB_IT").as_deref(), Ok("1"));
        let settings = astra_core::MatrixOneSettings::from_env();
        crate::storage::ensure_core_schema(&settings, "mysql")
            .await
            .unwrap();
        let pool = SharedPool::new(&settings).await.unwrap();
        let user = format!("blocker-race-{}", Uuid::new_v4());
        let discovered = SessionKeyV1::owner_session("server", &user, "old-owner", "main");
        let identity = format!("checkout-{}", Uuid::new_v4());
        let hash = execution_workspace_identity_hash(&identity);
        sqlx::query("INSERT INTO session_execution_workspace_claims
            (isolation_domain, owner_user_id, workspace_identity_hash, workspace_identity, session_id, branch_id)
            VALUES ('server', ?, ?, ?, 'old-owner', 'main')")
            .bind(&user).bind(&hash).bind(&identity).execute(pool.get()).await.unwrap();

        let mut tx = pool.get().begin().await.unwrap();
        assert!(
            workspace_claim_still_owned_in_tx(&mut tx, &discovered, &identity)
                .await
                .unwrap()
        );
        tx.commit().await.unwrap();
        // Deterministically model ownership changing after discovery but
        // before taking the old owner's execution fence. Its execution is no
        // longer a blocker for this checkout, regardless of its run state.
        sqlx::query(
            "UPDATE session_execution_workspace_claims SET session_id = 'new-owner'
            WHERE owner_user_id = ? AND workspace_identity_hash = ?",
        )
        .bind(&user)
        .bind(&hash)
        .execute(pool.get())
        .await
        .unwrap();
        let mut tx = pool.get().begin().await.unwrap();
        assert!(
            !workspace_claim_still_owned_in_tx(&mut tx, &discovered, &identity)
                .await
                .unwrap()
        );
        let current = SessionKeyV1::owner_session("server", &user, "new-owner", "main");
        assert!(
            workspace_claim_still_owned_in_tx(&mut tx, &current, &identity)
                .await
                .unwrap()
        );
        tx.commit().await.unwrap();
        sqlx::query("DELETE FROM session_execution_workspace_claims WHERE owner_user_id = ?")
            .bind(&user)
            .execute(pool.get())
            .await
            .unwrap();
        let mut tx = pool.get().begin().await.unwrap();
        assert!(
            !workspace_claim_still_owned_in_tx(&mut tx, &discovered, &identity)
                .await
                .unwrap()
        );
        tx.commit().await.unwrap();
    }

    #[test]
    fn workspace_reuse_blockers_preserve_evidence_and_do_not_require_history_deletion() {
        for (blocker, wire, recovery) in [
            (
                WorkspaceReuseBlocker::ExecutionSlot,
                "execution_slot",
                "wait_or_cancel_session",
            ),
            (
                WorkspaceReuseBlocker::ActiveRun,
                "active_run",
                "wait_or_cancel_session",
            ),
            (
                WorkspaceReuseBlocker::SettlementPending,
                "settlement_pending",
                "inspect_session",
            ),
            (
                WorkspaceReuseBlocker::WriterOrReservation,
                "writer_or_reservation",
                "retry_session",
            ),
            (
                WorkspaceReuseBlocker::BindingNotReady,
                "binding_not_ready",
                "inspect_session",
            ),
            (
                WorkspaceReuseBlocker::UnresolvedTool,
                "unresolved_tool",
                "inspect_session",
            ),
            (
                WorkspaceReuseBlocker::OwnerUnavailable,
                "owner_unavailable",
                "inspect_session",
            ),
            (
                WorkspaceReuseBlocker::ClaimChanged,
                "claim_changed",
                "retry_session",
            ),
        ] {
            assert_eq!(serde_json::to_value(blocker).unwrap(), wire);
            assert_eq!(blocker.recovery_action(), recovery);
            let message = blocker.user_message("owner-session");
            assert!(message.contains("owner-session"));
            assert!(message.contains(blocker.explanation()));
            assert!(message.contains("history can be kept"));
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
    fn execution_workspace_claim_lock_order_is_symmetric_for_swaps() {
        let left_to_right = ordered_execution_claim_hashes(Some("claim-a"), "claim-b");
        let right_to_left = ordered_execution_claim_hashes(Some("claim-b"), "claim-a");
        assert_eq!(left_to_right, vec!["claim-a", "claim-b"]);
        assert_eq!(right_to_left, left_to_right);
        assert_eq!(
            ordered_execution_claim_hashes(Some("claim-a"), "claim-a"),
            vec!["claim-a"]
        );
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

    #[test]
    fn turn_adoption_preserves_logical_turn_without_reviving_authority() {
        let key = SessionKeyV1::owner_session("server", "owner", "session", "main");
        let actor = ActorContextV1::owner_user(
            "owner",
            "recovery",
            astra_turn_types::ActorKindV1::Cli,
            astra_turn_types::SessionSurfaceV1::Cli,
            None,
            AuthorityEpochsV1::default(),
        );
        let mut state = CoordinatorStateV1::new(key.clone());
        let source = TurnReservationV1 {
            schema_version: SESSION_COORDINATION_SCHEMA_VERSION,
            reservation_id: "original".into(),
            key,
            lease_id: "old-writer".into(),
            writer_epoch: 0,
            expected_cursor: None,
            reserved_turn: 1,
            created_at_unix_ms: 0,
            expires_at_unix_ms: 1,
            idempotency_key: "source".into(),
        };
        let (lease, reservation) = prepare_adopted_turn_authority(
            &state,
            &source,
            &actor,
            1000,
            Duration::from_secs(10),
            "adopt-first",
        )
        .unwrap();
        assert_eq!(reservation.reserved_turn, source.reserved_turn);
        assert_eq!(reservation.expected_cursor, source.expected_cursor);
        assert_ne!(reservation.reservation_id, source.reservation_id);
        assert_ne!(lease.lease_id, source.lease_id);
        assert_eq!(lease.writer_epoch, 1);
        state.writer_epoch = lease.writer_epoch;
        state.active_writer = Some(lease.clone());
        state.active_reservation = Some(reservation.clone());
        assert!(matches!(
            prepare_adopted_turn_authority(
                &state,
                &source,
                &actor,
                1001,
                Duration::from_secs(10),
                "competing",
            ),
            Err(SessionContextCoordinatorError::Fenced)
        ));
        let (next_lease, next_reservation) = prepare_adopted_turn_authority(
            &state,
            &source,
            &actor,
            11001,
            Duration::from_secs(10),
            "adopt-second",
        )
        .unwrap();
        assert_eq!(next_lease.writer_epoch, 2);
        assert_eq!(next_reservation.reserved_turn, 1);
        state.writer_epoch = next_lease.writer_epoch;
        state.active_writer = Some(next_lease);
        state.active_reservation = Some(next_reservation);
        assert!(validate_active_lease(&state, &lease, 11001).is_err());
        assert!(validate_active_reservation(&state, &source, 11001).is_err());
        assert!(validate_active_reservation(&state, &reservation, 11001).is_err());
        let mut wrong_turn = source.clone();
        wrong_turn.reserved_turn = 2;
        assert!(
            prepare_adopted_turn_authority(
                &state,
                &wrong_turn,
                &actor,
                22002,
                Duration::from_secs(10),
                "wrong-turn"
            )
            .is_err()
        );
    }
}
