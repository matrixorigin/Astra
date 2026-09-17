use astra_turn_types::{UserIntentDelivery, UserIntentStatus};
use async_trait::async_trait;
use serde_json::Value;

pub use astra_services::runs::AtomicRunActionAdmission as ActionAdmissionOutcome;

const RUNTIME_NOTIFICATION_INPUT_KIND: &str = "astra_runtime_notification_v1";
pub(crate) const CANCELLATION_ORIGIN_LOOKUP_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(5);
static RUNTIME_NOTIFICATION_NONCE: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| uuid::Uuid::now_v7().simple().to_string());

/// Run status for cross-pod control polling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunControlStatus {
    Cancelled,
    Paused,
}

/// Authority presented when changing current-run guidance admission.
///
/// Keeping local ownership and durable lease ownership as distinct variants
/// prevents a missing durable generation from being silently interpreted as
/// process-local authority by a shared-store provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserIntentAdmissionAuthority {
    ProcessLocal,
    DurableOwnerGeneration(u64),
}

/// Exact decision at one externally metered provider boundary. Only
/// `Authorized` permits calling the provider; durable-store failures and lost
/// ownership remain distinguishable from a terminal/inactive run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderBoundaryAuthorization {
    Authorized,
    /// The durable run was manually paused after turn preparation but before
    /// the externally metered provider request. This is a control hold, not
    /// a cancellation or loss of execution ownership.
    Paused,
    Inactive {
        status: String,
    },
    AuthorityLost {
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct QueuedUserIntent {
    /// Stable identity minted by the accepting client and preserved through
    /// durable delivery, application, replay, and transcript projection.
    pub intent_id: String,
    pub delivery: UserIntentDelivery,
    pub status: UserIntentStatus,
    pub event_index: usize,
    pub input: Value,
}

/// Immutable inputs to the durable action-admission fence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionAdmissionRequest {
    pub action_id: String,
    pub expected_session_id: String,
    /// Greatest durable run-event index through which the caller completed
    /// user-intent application at a control boundary. Seeing a later unrelated
    /// event is insufficient. `-1` denotes no applied event boundary.
    pub expected_control_epoch: i64,
    /// Exact durable execution-owner generation. `RunEngine` rejects `None`;
    /// it is reserved for non-durable/local providers with their own authority
    /// contract.
    pub expected_owner_generation: Option<u64>,
}

/// Normalize the user-facing content carried by a user-intent payload. This is
/// shared by prompt injection and every applied-event surface so CLI and
/// server modes report the same bytes for the same accepted input.
pub fn user_intent_content(input: &Value) -> Option<String> {
    astra_services::runs::normalized_run_user_intent_content(input)
}

/// Build an internal run-control payload for runtime-owned context. It shares
/// the existing delivery queue so active local runs wake at the same safe
/// model boundary as user guidance, but is identified explicitly so it never
/// becomes a synthetic user message or replaces the latest user goal. The
/// process-local nonce prevents a remote user-intent payload from spoofing
/// this internal lane; local notifications are turn-scoped and never need to
/// survive a process restart.
pub fn runtime_notification_input(content: impl Into<String>) -> Value {
    serde_json::json!({
        "_astra_input_kind": RUNTIME_NOTIFICATION_INPUT_KIND,
        "_astra_runtime_nonce": RUNTIME_NOTIFICATION_NONCE.as_str(),
        "content": content.into(),
    })
}

pub fn runtime_notification_content(input: &Value) -> Option<String> {
    (input.get("_astra_input_kind").and_then(Value::as_str)
        == Some(RUNTIME_NOTIFICATION_INPUT_KIND)
        && input.get("_astra_runtime_nonce").and_then(Value::as_str)
            == Some(RUNTIME_NOTIFICATION_NONCE.as_str()))
    .then(|| {
        input
            .get("content")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|content| !content.is_empty())
            .map(ToString::to_string)
    })
    .flatten()
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct UserIntentPoll {
    pub next_cursor: usize,
    /// True when the authoritative snapshot contains another control page.
    /// Callers at an action boundary must continue from `next_cursor` rather
    /// than treating an empty `inputs` page as a drained control lane.
    pub snapshot_has_more: bool,
    /// Raw durable control facts inspected in this page, including returned
    /// dispositions and settled sources that intentionally produce no input.
    pub snapshot_page_fact_count: usize,
    pub inputs: Vec<QueuedUserIntent>,
    /// Malformed durable events skipped during this scan. A corrupt record is
    /// observable evidence, but never a reason to block later valid intents.
    pub issues: Vec<UserIntentPollIssue>,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserIntentPollIssueKind {
    MissingData,
    MissingIntentId,
    MissingDelivery,
    InvalidDelivery,
    MissingInput,
    NoActionableContent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserIntentPollIssue {
    pub event_index: usize,
    pub intent_id: Option<String>,
    pub kind: UserIntentPollIssueKind,
}

/// Durable outcome of acknowledging locally staged user intent application.
/// A terminal run is not equivalent to an applied intent: callers must not
/// publish an `applied` event when the terminal transition won the race.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserIntentApplyAck {
    Applied,
    /// The run terminated before application and durably returned delivery
    /// ownership to the submitting client.
    RunTerminalReturned,
}

/// Polls the database for the authoritative run status, enabling cross-pod
/// cancel/pause control without sticky sessions.
#[async_trait]
pub trait RunStatusProvider: Send + Sync {
    /// Returns `Some(Cancelled)`, `Some(Paused)`, or `None` if the run is
    /// still active (or doesn't exist). Transient lookup failures must be
    /// surfaced so callers do not confuse control-plane unavailability with a
    /// durable cancel/pause signal.
    async fn control_status(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Result<Option<RunControlStatus>, String>;

    /// Resolve the canonical origin for an observed cancellation. Providers
    /// without durable user-request evidence treat status/token cancellation
    /// as runtime-owned; lookup failures must be returned so callers can
    /// project `Unverified` instead of guessing.
    async fn cancellation_origin(
        &self,
        _user_id: &str,
        _run_id: &str,
    ) -> Result<astra_turn_core::orchestration_types::CancellationOrigin, String> {
        Ok(astra_turn_core::orchestration_types::CancellationOrigin::Runtime)
    }
}

/// Polls durable user intents accepted while a run is executing.
#[async_trait]
pub trait UserIntentProvider: Send + Sync {
    /// Cheap local wake hint. Durable providers may keep the default and use
    /// the bounded poll cadence; in-process providers override it so newly
    /// queued guidance/runtime facts reach the very next model boundary.
    fn has_pending_inputs(&self) -> bool {
        false
    }

    /// Revalidate exact execution authority immediately before a provider
    /// request. Process-local providers have authority by construction;
    /// durable providers must override this and prove their generation/owner
    /// lease against shared state.
    async fn authorize_provider_boundary(
        &self,
        _user_id: &str,
        _expected_session_id: &str,
        _run_id: &str,
        authority: UserIntentAdmissionAuthority,
    ) -> Result<ProviderBoundaryAuthorization, String> {
        match authority {
            UserIntentAdmissionAuthority::ProcessLocal => {
                Ok(ProviderBoundaryAuthorization::Authorized)
            }
            UserIntentAdmissionAuthority::DurableOwnerGeneration(_) => Err(
                "durable provider-boundary authorization is not supported by this provider"
                    .to_string(),
            ),
        }
    }

    /// Atomically fence a not-yet-started external action against guidance
    /// accepted after the caller's observed control epoch. Unsupported
    /// providers fail closed instead of silently admitting the action.
    async fn begin_action(
        &self,
        _user_id: &str,
        _run_id: &str,
        _request: ActionAdmissionRequest,
    ) -> Result<ActionAdmissionOutcome, String> {
        Err("atomic action admission is not supported by this run-control provider".to_string())
    }

    /// Durably close admission for new current-run intents before the final
    /// model-boundary poll. Implementations with a remote durable queue must
    /// serialize this fence with intent append operations.
    async fn fence_user_intent_submissions(
        &self,
        _user_id: &str,
        _expected_session_id: &str,
        _run_id: &str,
        authority: UserIntentAdmissionAuthority,
    ) -> Result<(), String> {
        match authority {
            UserIntentAdmissionAuthority::ProcessLocal => Ok(()),
            UserIntentAdmissionAuthority::DurableOwnerGeneration(_) => Err(
                "durable user-intent admission fencing is not supported by this provider"
                    .to_string(),
            ),
        }
    }

    /// Reopen current-run intent admission when settlement discovered guidance
    /// and the same run will continue through another model boundary.
    async fn reopen_user_intent_submissions(
        &self,
        _user_id: &str,
        _expected_session_id: &str,
        _run_id: &str,
        authority: UserIntentAdmissionAuthority,
    ) -> Result<(), String> {
        match authority {
            UserIntentAdmissionAuthority::ProcessLocal => Ok(()),
            UserIntentAdmissionAuthority::DurableOwnerGeneration(_) => Err(
                "durable user-intent admission reopening is not supported by this provider"
                    .to_string(),
            ),
        }
    }

    /// Poll `user_intent` events appended to a durable run after the
    /// provided exclusive cursor.
    async fn poll_user_intents(
        &self,
        user_id: &str,
        run_id: &str,
        after_event_index: usize,
    ) -> UserIntentPoll;

    /// Mark accepted intents as applied to the next model boundary.
    async fn mark_user_intents_applied(
        &self,
        user_id: &str,
        expected_session_id: &str,
        run_id: &str,
        event_indices: &[usize],
        authority: UserIntentAdmissionAuthority,
    ) -> Result<UserIntentApplyAck, String>;
}

/// Full run-control surface required by the agentic loop.
///
/// This is intentionally a composition of the status and input traits instead
/// of a trait with optional no-op methods. Implementors must explicitly provide
/// both halves, so a missing user-intent implementation fails at compile time.
pub trait RunControlProvider: RunStatusProvider + UserIntentProvider {}

impl<T> RunControlProvider for T where T: RunStatusProvider + UserIntentProvider + Send + Sync {}
