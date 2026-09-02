//! Durable run execution engine.
//!
//! `RunEngine` orchestrates agentic run execution with persistence backing via
//! [`RunStateStore`]. Durable storage is the authority for run status, replay,
//! listing, and restart recovery; process-local state is only for live controls.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────┐     ┌──────────────┐
//! │ RunLifecycleService │────▶│  RunEngine   │
//! │   (HTTP handlers)   │     │              │
//! └─────────────────────┘     │  start_run() │
//!                             │  persist()   │
//!                             │  resume()    │
//!                             │  recover()   │
//!                             └──────┬───────┘
//!                                    │
//!                             ┌──────▼───────┐
//!                             │ RunStateStore │
//!                             │  (durable)   │
//!                             └──────────────┘
//! ```
//!
//! # Lifecycle
//!
//! 1. `start_run()` — Creates a durable record, returns run_id
//! 2. `persist_status()` — Syncs status changes to store
//! 3. `persist_checkpoint()` — Saves checkpoint for crash recovery
//! 4. `persist_usage()` — Updates token/tool counts
//! 5. `recover_active_runs()` — On startup, loads runs that were active when process died
//! 6. `load_run()` — Loads a run from store (cache miss path)

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use astra_services::{
    DatabaseStateProjectionStore,
    runs::{
        AtomicOrphanRunCancellationRequest, AtomicRunActionAdmissionRequest,
        AtomicRunGuidanceAdmission, AtomicRunGuidanceAdmissionRequest,
        AtomicRunToolRequestCommitOutcome, AtomicRunToolRequestCommitRequest,
        AtomicRunUserIntentAdmissionTransition, AtomicRunUserIntentAdmissionTransitionRequest,
        AtomicRunUserIntentApply, AtomicRunUserIntentApplyRequest, DurableCancellationOrigin,
        DurableRunCheckpointRecord, DurableRunDisplayProjectionRecord, DurableRunEventDelta,
        DurableRunGuidanceAdmissionRecord, DurableRunInteractionKind,
        DurableRunInteractionResolveOutcome, DurableRunListPage, DurableRunRecord,
        DurableRunStartClaim, DurableRunStatusKind, DurableRunStatusSnapshot,
        DurableRunUserIntentControlDelta, DurableWorkItemRunBinding, DurableWorkRunBinding,
        GuardedRunStatusTransition, GuardedRunStatusTransitionRequest,
        RUN_RECOVERY_CLAIM_BATCH_SIZE, RequestedTurnInteractionMode, ResolvedModelSelection,
        RunExecutionBoundaryAuthorization, RunExecutionBoundaryAuthorizationRequest, RunListCursor,
        RunStateStore, RunStatusCasRequest, RunUsageOwnerUpdateRequest,
        RunUserIntentAdmissionTransition, RuntimeProfileRequest, SkillAutoRouteExecutionPolicy,
        TurnIntentExecutionPolicy, USER_INTENT_CONTROL_DELTA_PAGE_SIZE,
        durable_run_status_is_terminal, durable_run_status_kind,
    },
};
use astra_turn_core::pipeline_metrics::MetricsRegistry;
use astra_turn_types::ModelSelection;

use astra_core::{
    STATUS_CANCELLED, STATUS_COMPLETED, STATUS_DELEGATED, STATUS_FAILED, STATUS_PAUSED,
    STATUS_RUNNING, STATUS_WAITING,
};

const METRIC_RUN_CONTROL_POLL_ATTEMPTS_TOTAL: &str = "astra_run_control_poll_attempts_total";
const METRIC_RUN_CONTROL_POLL_ERRORS_TOTAL: &str = "astra_run_control_poll_errors_total";
const METRIC_RUN_RECOVERY_SCANS_TOTAL: &str = "astra_run_recovery_scans_total";
const METRIC_RUN_RECOVERY_RUNS_TOTAL: &str = "astra_run_recovery_runs_total";
const TERMINAL_TRANSITION_MAX_ATTEMPTS: usize = 3;
const TERMINAL_TRANSITION_RETRY_BASE_DELAY_MS: u64 = 25;
const RUN_RECOVERY_MAX_CONCURRENCY: usize = 8;
const RUN_RECOVERY_SWEEP_INTERVAL: Duration = Duration::from_secs(5);
const OWNER_LEASE_RENEWAL_STATUSES: &[&str] = &[STATUS_RUNNING, STATUS_WAITING, STATUS_PAUSED];
const OWNER_LEASE_ACTIVATION_MAX_WAIT: Duration = Duration::from_secs(5);
// Lease release only shortens the recovery TTL; it is not a correctness
// commit. Never let a stalled database release leave one detached task per
// completed run behind indefinitely.
const OWNER_LEASE_RELEASE_MAX_WAIT: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, PartialEq)]
pub enum TerminalTransitionOutcome {
    /// The terminal CAS committed. The enclosed row is the durable fact after
    /// store-owned terminal projections (including returned user intents)
    /// were appended in the same transaction.
    Committed(Box<DurableRunRecord>),
    /// Another durable transition won the CAS. The enclosed record is the
    /// authority callers must project instead of their stale local outcome.
    Superseded(Box<DurableRunRecord>),
}

#[derive(Clone, Copy)]
struct DelegationOutcomeTransition {
    canonical_status: &'static str,
    expected_statuses: &'static [&'static str],
    terminal: bool,
}

fn delegation_outcome_transition(status: &str) -> Option<DelegationOutcomeTransition> {
    let transition = match durable_run_status_kind(status) {
        DurableRunStatusKind::Running => DelegationOutcomeTransition {
            canonical_status: STATUS_RUNNING,
            expected_statuses: &[STATUS_RUNNING],
            terminal: false,
        },
        DurableRunStatusKind::Waiting => DelegationOutcomeTransition {
            canonical_status: STATUS_WAITING,
            expected_statuses: &[STATUS_RUNNING, STATUS_WAITING],
            terminal: false,
        },
        DurableRunStatusKind::Paused => DelegationOutcomeTransition {
            canonical_status: STATUS_PAUSED,
            expected_statuses: &[STATUS_RUNNING, STATUS_PAUSED],
            terminal: false,
        },
        DurableRunStatusKind::Completed => DelegationOutcomeTransition {
            canonical_status: STATUS_COMPLETED,
            expected_statuses: &[STATUS_RUNNING, STATUS_WAITING],
            terminal: true,
        },
        DurableRunStatusKind::Delegated => DelegationOutcomeTransition {
            canonical_status: STATUS_DELEGATED,
            expected_statuses: &[STATUS_RUNNING, STATUS_WAITING],
            terminal: true,
        },
        DurableRunStatusKind::Failed => DelegationOutcomeTransition {
            canonical_status: STATUS_FAILED,
            expected_statuses: &[STATUS_RUNNING, STATUS_WAITING],
            terminal: true,
        },
        DurableRunStatusKind::Cancelled => DelegationOutcomeTransition {
            canonical_status: STATUS_CANCELLED,
            expected_statuses: &[STATUS_RUNNING, STATUS_WAITING],
            terminal: true,
        },
        // Verification failure is an AgentResult detail, not a separate
        // durable lifecycle state.
        DurableRunStatusKind::Other if status == "verification_failed" => {
            DelegationOutcomeTransition {
                canonical_status: STATUS_FAILED,
                expected_statuses: &[STATUS_RUNNING, STATUS_WAITING],
                terminal: true,
            }
        }
        DurableRunStatusKind::Other => return None,
    };
    Some(transition)
}

fn delegation_terminal_events(
    canonical_status: &str,
    error_message: Option<&str>,
) -> Vec<serde_json::Value> {
    let mut events = Vec::with_capacity(2);
    if let Some(error) = error_message {
        events.push(serde_json::json!({
            "event_type": "run_error",
            "data": {
                "error": error,
                "error_code": "delegation_error",
                "error_kind": "delegation_error",
            }
        }));
    }
    let mut terminal_data = serde_json::json!({
        "status": canonical_status,
        "error": error_message,
    });
    if canonical_status == STATUS_CANCELLED {
        terminal_data["cancelled"] = serde_json::Value::Bool(true);
        terminal_data["cancellation_origin"] = serde_json::Value::String(
            astra_turn_core::orchestration_types::CancellationOrigin::Unverified
                .as_str()
                .to_string(),
        );
    }
    events.push(serde_json::json!({
        "event_type": "run_finished",
        "data": terminal_data,
    }));
    events
}

fn crash_recovery_terminal_events() -> [serde_json::Value; 2] {
    [
        serde_json::json!({
            "event_type": "run_error",
            "data": {
                "error": "recovered from crash",
                "error_code": "crash_recovery",
                "error_kind": "crash_recovery",
                "source": "crash_recovery",
            },
        }),
        serde_json::json!({
            "event_type": "run_finished",
            "data": {
                "status": STATUS_FAILED,
                "error": "recovered from crash",
                "error_code": "crash_recovery",
                "error_kind": "crash_recovery",
                "source": "crash_recovery",
            },
        }),
    ]
}

fn crash_recovery_cancellation_event(
    run_id: &str,
    origin: astra_turn_core::orchestration_types::CancellationOrigin,
) -> serde_json::Value {
    serde_json::json!({
        "event_type": "run_finished",
        "data": {
            "run_id": run_id,
            "status": STATUS_CANCELLED,
            "cancelled": true,
            "reason": "recovered durable cancellation control",
            "source": "crash_recovery",
            "cancellation_origin": origin,
        }
    })
}

fn turn_cancellation_origin(
    origin: DurableCancellationOrigin,
) -> astra_turn_core::orchestration_types::CancellationOrigin {
    match origin {
        DurableCancellationOrigin::User => {
            astra_turn_core::orchestration_types::CancellationOrigin::User
        }
        DurableCancellationOrigin::Runtime => {
            astra_turn_core::orchestration_types::CancellationOrigin::Runtime
        }
        DurableCancellationOrigin::Unverified => {
            astra_turn_core::orchestration_types::CancellationOrigin::Unverified
        }
    }
}

fn restart_session_continuation_event(
    previous_status: &str,
    checkpoint_available: bool,
) -> serde_json::Value {
    serde_json::json!({
        "event_type": "run_interrupted_after_restart",
        "data": {
            "previous_status": previous_status,
            "reason_kind": "execution_process_restarted",
            "checkpoint_available": checkpoint_available,
            "resumable": true,
            "resume_strategy": "session_continuation",
            "releases_session_slot": true,
        }
    })
}

/// Durable run execution engine.
///
/// Wraps a [`RunStateStore`] and provides high-level operations for
/// durable run management. The engine is designed to be composed into
/// `AgenticRunLifecycleService` alongside process-local live control handles:
/// create, status transitions, usage/checkpoint persistence, event logging,
/// session blocking queries, and recovery.
#[derive(Clone)]
pub struct RunEngine {
    store: Arc<dyn RunStateStore>,
    projection_store: Option<Arc<DatabaseStateProjectionStore>>,
    metrics_registry: Option<Arc<MetricsRegistry>>,
}

pub(crate) struct RunOwnerLeaseHeartbeat {
    stop_tx: Option<tokio::sync::oneshot::Sender<()>>,
    _join: tokio::task::JoinHandle<()>,
}

/// Exact execution-owner epoch created with a durable run start. This is
/// process-local authority carried into the executor; it is never reconstructed
/// by reading whichever owner happens to be current later.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunExecutionAuthority {
    pub owner_generation: u64,
}

impl Drop for RunOwnerLeaseHeartbeat {
    fn drop(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        // Dropping a JoinHandle detaches the task. Let it observe `stop_tx`
        // and release the durable lease; aborting here would preserve a stale
        // owner until TTL expiry after every graceful executor exit.
    }
}

/// Run-start interaction context persisted into the durable `run_started`
/// event so replay/status surfaces can explain policy decisions.
///
/// `interaction_mode` is already effective at this boundary. Wire-level
/// omission is normalized before constructing this context; keeping the value
/// non-optional prevents child/recovery paths from reinterpreting an absent
/// mode differently from a durable explicit `headless` mode.
#[derive(Clone, Debug, PartialEq)]
pub struct RunStartContext {
    pub interaction_mode: RequestedTurnInteractionMode,
    pub interactive_client: Option<bool>,
    pub turn_intent_policy: TurnIntentExecutionPolicy,
    pub skill_auto_route_policy: SkillAutoRouteExecutionPolicy,
    pub execution_metadata: Option<serde_json::Map<String, serde_json::Value>>,
    pub agent_binding_ids: Vec<String>,
    pub agent_binding_id: Option<String>,
    pub agent_binding_name: Option<String>,
    pub agent_binding_schema_version: Option<String>,
    pub model_selection: Option<ModelSelection>,
    pub resolved_model_selection: Option<ResolvedModelSelection>,
    pub runtime_profile: Option<RuntimeProfileRequest>,
    pub provider_request_fingerprint: Option<String>,
    pub provider_run_owner: Option<astra_services::runs::ProviderRunOwner>,
    pub start_request_fingerprint: Option<String>,
    /// Explicit canonical Work authority for this exact run.
    ///
    /// `None` means detached even when a parent is Work-bound. Session,
    /// transcript, cancellation, and model lineage are inherited separately;
    /// Work authority is never inherited by omission.
    pub work_binding: Option<DurableWorkRunBinding>,
    /// Set only after the Work repository has verified an exact item revision
    /// in the branch's current graph. This permits a child to narrow its
    /// parent's Work scope to a concrete attempt without permitting arbitrary
    /// cross-Work or cross-branch binding changes.
    pub(crate) validated_work_item_assignment: bool,
}

impl Default for RunStartContext {
    fn default() -> Self {
        Self {
            interaction_mode: RequestedTurnInteractionMode::Headless,
            interactive_client: None,
            turn_intent_policy: TurnIntentExecutionPolicy::default(),
            skill_auto_route_policy: SkillAutoRouteExecutionPolicy::default(),
            execution_metadata: None,
            agent_binding_ids: Vec::new(),
            agent_binding_id: None,
            agent_binding_name: None,
            agent_binding_schema_version: None,
            model_selection: None,
            resolved_model_selection: None,
            runtime_profile: None,
            provider_request_fingerprint: None,
            provider_run_owner: None,
            start_request_fingerprint: None,
            work_binding: None,
            validated_work_item_assignment: false,
        }
    }
}

fn durable_model_identity(
    context: &RunStartContext,
) -> Result<(Option<String>, Option<String>), String> {
    match (
        context.model_selection.as_ref(),
        context.resolved_model_selection.as_ref(),
    ) {
        (None, None) => Ok((None, None)),
        (Some(selection), Some(resolved))
            if selection.offering_id == resolved.offering_id
                && astra_services::validate_model_offering_id(&selection.offering_id).is_ok()
                && !resolved.model_name.is_empty() =>
        {
            Ok((
                Some(selection.offering_id.clone()),
                Some(resolved.model_name.clone()),
            ))
        }
        _ => Err(
            "run model identity must contain one matching admitted Offering and resolved model"
                .to_string(),
        ),
    }
}

fn inherit_parent_run_identity(
    context: &mut RunStartContext,
    parent: &DurableRunRecord,
) -> Result<(), String> {
    match (
        parent.model_offering_id.as_deref(),
        parent.resolved_model_name.as_deref(),
    ) {
        (None, None) => Ok(()),
        (Some(offering_id), Some(model_name)) => {
            match (
                context.model_selection.as_ref(),
                context.resolved_model_selection.as_ref(),
            ) {
                (None, None) => {
                    context.model_selection = Some(ModelSelection {
                        offering_id: offering_id.to_string(),
                    });
                    context.resolved_model_selection = Some(ResolvedModelSelection {
                        offering_id: offering_id.to_string(),
                        model_name: model_name.to_string(),
                    });
                    Ok(())
                }
                (Some(selection), Some(resolved))
                    if selection.offering_id == offering_id
                        && resolved.offering_id == offering_id
                        && resolved.model_name == model_name =>
                {
                    Ok(())
                }
                _ => Err(
                    "child run model identity must inherit the admitted parent Offering"
                        .to_string(),
                ),
            }
        }
        _ => Err("durable parent run contains an incomplete model identity".to_string()),
    }?;

    let parent_provider_run_owner = parent
        .events
        .iter()
        .find(|event| event["event_type"] == "run_started")
        .and_then(|event| event.pointer("/data/provider_run_owner"))
        .cloned()
        .map(serde_json::from_value::<astra_services::runs::ProviderRunOwner>)
        .transpose()
        .map_err(|error| format!("durable parent run has an invalid provider owner: {error}"))?;
    match (
        context.provider_run_owner.as_ref(),
        parent_provider_run_owner,
    ) {
        (None, Some(parent_owner)) => {
            context.provider_run_owner = Some(parent_owner);
            Ok(())
        }
        (Some(child_owner), Some(parent_owner)) if child_owner != &parent_owner => {
            Err("child run provider owner must match its durable parent".to_string())
        }
        _ => Ok(()),
    }
}

fn validate_child_work_binding(
    context: &RunStartContext,
    parent: &DurableRunRecord,
) -> Result<(), String> {
    match (&context.work_binding, &parent.work_binding) {
        // Absence is an explicit detached execution role. Session and causal
        // lineage are inherited independently; canonical Work authority is
        // never ambient authority.
        (None, _) => Ok(()),
        (Some(child_binding), Some(parent_binding)) if child_binding == parent_binding => Ok(()),
        (Some(child_binding), Some(parent_binding))
            if context.validated_work_item_assignment
                && child_binding.work_id() == parent_binding.work_id()
                && child_binding.branch_id() == parent_binding.branch_id()
                && child_binding.graph_revision().get()
                    >= parent_binding.graph_revision().get()
                && child_binding.item().is_some()
                && parent_binding.item().is_none_or(|parent_item| {
                    parent_item.item_id().as_str() == "root"
                        || child_binding.item().is_some_and(|child_item| {
                            child_item.item_id() == parent_item.item_id()
                                && child_item.item_revision() == parent_item.item_revision()
                        })
                }) =>
        {
            Ok(())
        }
        (Some(_), None) => {
            Err("child run cannot acquire a Work binding absent from its parent".to_string())
        }
        (Some(_), Some(_)) => {
            Err("child run must inherit its parent's exact Work graph binding".to_string())
        }
    }
}

fn requested_mode_label(mode: RequestedTurnInteractionMode) -> &'static str {
    match mode {
        RequestedTurnInteractionMode::NonInteractive => "non_interactive",
        RequestedTurnInteractionMode::Prompt => "prompt",
        RequestedTurnInteractionMode::Auto => "auto",
        RequestedTurnInteractionMode::Deny => "deny",
        RequestedTurnInteractionMode::Headless => "headless",
    }
}

fn turn_intent_policy_label(policy: TurnIntentExecutionPolicy) -> &'static str {
    match policy {
        TurnIntentExecutionPolicy::Auto => "auto",
        TurnIntentExecutionPolicy::FixedDefault => "fixed_default",
    }
}

fn skill_auto_route_policy_label(policy: SkillAutoRouteExecutionPolicy) -> &'static str {
    match policy {
        SkillAutoRouteExecutionPolicy::Auto => "auto",
        SkillAutoRouteExecutionPolicy::Disabled => "disabled",
    }
}

pub(crate) fn effective_requested_interaction_mode(
    requested: Option<RequestedTurnInteractionMode>,
    interactive_client: bool,
) -> RequestedTurnInteractionMode {
    requested.unwrap_or({
        if interactive_client {
            RequestedTurnInteractionMode::Prompt
        } else {
            RequestedTurnInteractionMode::Headless
        }
    })
}

/// Read the immutable interaction authority recorded at run start. Records
/// created before the field became mandatory, and malformed records, close to
/// explicit Headless rather than borrowing a newer parent's approval owner.
pub(crate) fn durable_run_effective_interaction_mode(
    run: &DurableRunRecord,
) -> RequestedTurnInteractionMode {
    run.events
        .iter()
        .rev()
        .find(|event| {
            event.get("event_type").and_then(serde_json::Value::as_str) == Some("run_started")
        })
        .and_then(|event| event.pointer("/data/interaction_mode"))
        .and_then(|value| {
            serde_json::from_value::<RequestedTurnInteractionMode>(value.clone()).ok()
        })
        .unwrap_or(RequestedTurnInteractionMode::Headless)
}

fn runtime_profile_label(profile: RuntimeProfileRequest) -> &'static str {
    match profile {
        RuntimeProfileRequest::RequestScopedRuntimeMcp => "request_scoped_runtime_mcp",
        RuntimeProfileRequest::AgentBindingRegistry => "agent_binding_registry",
    }
}

fn register_run_control_poll_metrics(registry: &MetricsRegistry) {
    registry.register_counter(
        METRIC_RUN_CONTROL_POLL_ATTEMPTS_TOTAL,
        "Run control-plane poll attempts by operation and low-cardinality outcome.",
    );
    registry.register_counter(
        METRIC_RUN_CONTROL_POLL_ERRORS_TOTAL,
        "Run control-plane poll errors by operation and low-cardinality class.",
    );
}

fn register_run_recovery_metrics(registry: &MetricsRegistry) {
    registry.register_counter(
        METRIC_RUN_RECOVERY_SCANS_TOTAL,
        "Startup run recovery scans by low-cardinality outcome.",
    );
    registry.register_counter(
        METRIC_RUN_RECOVERY_RUNS_TOTAL,
        "Startup run recovery actions by low-cardinality action and outcome.",
    );
}

fn record_control_poll_attempt(
    registry: Option<&Arc<MetricsRegistry>>,
    operation: &'static str,
    outcome: &'static str,
) {
    let Some(registry) = registry else {
        return;
    };
    register_run_control_poll_metrics(registry);
    registry.increment_counter(
        METRIC_RUN_CONTROL_POLL_ATTEMPTS_TOTAL,
        &[("operation", operation), ("outcome", outcome)],
        1,
    );
}

fn record_control_poll_error(
    registry: Option<&Arc<MetricsRegistry>>,
    operation: &'static str,
    class: &'static str,
) {
    let Some(registry) = registry else {
        return;
    };
    register_run_control_poll_metrics(registry);
    registry.increment_counter(
        METRIC_RUN_CONTROL_POLL_ERRORS_TOTAL,
        &[("operation", operation), ("class", class)],
        1,
    );
}

fn record_recovery_scan(registry: Option<&Arc<MetricsRegistry>>, outcome: &'static str) {
    let Some(registry) = registry else {
        return;
    };
    register_run_recovery_metrics(registry);
    registry.increment_counter(METRIC_RUN_RECOVERY_SCANS_TOTAL, &[("outcome", outcome)], 1);
}

fn record_recovery_run(
    registry: Option<&Arc<MetricsRegistry>>,
    action: &'static str,
    outcome: &'static str,
) {
    let Some(registry) = registry else {
        return;
    };
    register_run_recovery_metrics(registry);
    registry.increment_counter(
        METRIC_RUN_RECOVERY_RUNS_TOTAL,
        &[("action", action), ("outcome", outcome)],
        1,
    );
}

fn terminal_transition_retry_delay(attempt: usize) -> Duration {
    Duration::from_millis(TERMINAL_TRANSITION_RETRY_BASE_DELAY_MS.saturating_mul(attempt as u64))
}

fn json_contains_expected_fields(actual: &serde_json::Value, expected: &serde_json::Value) -> bool {
    match (actual, expected) {
        (serde_json::Value::Object(actual), serde_json::Value::Object(expected)) => {
            expected.iter().all(|(key, expected_value)| {
                key == "index"
                    || actual.get(key).is_some_and(|actual_value| {
                        json_contains_expected_fields(actual_value, expected_value)
                    })
            })
        }
        (serde_json::Value::Array(actual), serde_json::Value::Array(expected)) => {
            actual.len() == expected.len()
                && actual
                    .iter()
                    .zip(expected)
                    .all(|(actual_value, expected_value)| {
                        json_contains_expected_fields(actual_value, expected_value)
                    })
        }
        _ => actual == expected,
    }
}

fn durable_run_contains_event_batch(
    run: &DurableRunRecord,
    expected_events: &[serde_json::Value],
) -> bool {
    expected_events.is_empty()
        || run
            .events
            .windows(expected_events.len())
            .any(|actual_events| {
                actual_events
                    .iter()
                    .zip(expected_events)
                    .all(|(actual, expected)| json_contains_expected_fields(actual, expected))
            })
}

fn run_started_event_data(context: &RunStartContext) -> serde_json::Value {
    let mut data = serde_json::Map::new();
    data.insert(
        "turn_intent_policy".to_string(),
        serde_json::Value::String(turn_intent_policy_label(context.turn_intent_policy).to_string()),
    );
    data.insert(
        "skill_auto_route_policy".to_string(),
        serde_json::Value::String(
            skill_auto_route_policy_label(context.skill_auto_route_policy).to_string(),
        ),
    );
    data.insert(
        "interaction_mode".to_string(),
        serde_json::Value::String(requested_mode_label(context.interaction_mode).to_string()),
    );
    if let Some(interactive_client) = context.interactive_client {
        data.insert(
            "interactive_client".to_string(),
            serde_json::Value::Bool(interactive_client),
        );
    }
    if let Some(metadata) = context.execution_metadata.as_ref() {
        for (key, value) in metadata {
            data.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }
    if let Some(fingerprint) = context.provider_request_fingerprint.as_ref() {
        data.insert(
            "provider_request_fingerprint".to_string(),
            serde_json::Value::String(fingerprint.clone()),
        );
    }
    if let Some(fingerprint) = context.start_request_fingerprint.as_ref() {
        data.insert(
            "start_request_fingerprint".to_string(),
            serde_json::Value::String(fingerprint.clone()),
        );
    }
    if let Some(owner) = context.provider_run_owner.as_ref() {
        data.insert(
            "provider_run_owner".to_string(),
            serde_json::to_value(owner).expect("provider run owner must serialize"),
        );
    }
    if !context.agent_binding_ids.is_empty() {
        data.insert(
            "agent_binding_ids".to_string(),
            serde_json::json!(context.agent_binding_ids),
        );
    }
    if let Some(agent_binding_id) = context.agent_binding_id.as_ref() {
        data.insert(
            "agent_binding_id".to_string(),
            serde_json::Value::String(agent_binding_id.clone()),
        );
    }
    if let Some(agent_binding_name) = context.agent_binding_name.as_ref() {
        data.insert(
            "agent_binding_name".to_string(),
            serde_json::Value::String(agent_binding_name.clone()),
        );
    }
    if let Some(agent_binding_schema_version) = context.agent_binding_schema_version.as_ref() {
        data.insert(
            "agent_binding_schema_version".to_string(),
            serde_json::Value::String(agent_binding_schema_version.clone()),
        );
    }
    if let Some(model_selection) = context.model_selection.as_ref()
        && let Ok(value) = serde_json::to_value(model_selection)
    {
        data.insert("model_selection".to_string(), value);
    }
    if let Some(resolved_model_selection) = context.resolved_model_selection.as_ref()
        && let Ok(value) = serde_json::to_value(resolved_model_selection)
    {
        data.insert("resolved_model_selection".to_string(), value);
    }
    if let Some(runtime_profile) = context.runtime_profile {
        data.insert(
            "runtime_profile".to_string(),
            serde_json::Value::String(runtime_profile_label(runtime_profile).to_string()),
        );
    }
    serde_json::Value::Object(data)
}

impl RunEngine {
    /// Create a new engine backed by the given store.
    pub fn new(store: Arc<dyn RunStateStore>) -> Self {
        Self {
            store,
            projection_store: None,
            metrics_registry: None,
        }
    }

    /// Exact process-local owner capability of the durable store that claimed
    /// runs for this engine. External dispatch ledgers bind this once at
    /// composition time; they must not reconstruct authority from a database
    /// row that may already belong to another executor.
    pub(crate) fn execution_owner_pod_id(&self) -> Option<&str> {
        self.store.execution_owner_pod_id()
    }

    /// Whether invocation dispatch must be admitted inside the database
    /// ledger transaction. Stores without a durable owner capability are
    /// process-local and use the store's in-memory action fence instead.
    pub(crate) fn uses_transactional_invocation_admission(&self) -> bool {
        self.execution_owner_pod_id().is_some()
    }

    pub(crate) fn process_local_action_fence(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Option<Arc<tokio::sync::Mutex<()>>> {
        self.store.process_local_action_fence(user_id, run_id)
    }

    pub(crate) async fn begin_action_while_process_local_fence_held(
        &self,
        user_id: &str,
        run_id: &str,
        request: ActionAdmissionRequest,
    ) -> Result<astra_services::runs::AtomicRunActionAdmission, String> {
        let expected_owner_generation = request.expected_owner_generation.ok_or_else(|| {
            "process-local run action admission requires exact owner generation".to_string()
        })?;
        self.store
            .begin_action_while_process_local_fence_held(AtomicRunActionAdmissionRequest {
                user_id,
                run_id,
                expected_session_id: &request.expected_session_id,
                action_id: &request.action_id,
                expected_control_epoch: request.expected_control_epoch,
                expected_owner_generation,
            })
            .await
    }

    /// Attach the database projection store used by web-agent session state.
    ///
    /// Delegation paths call `RunEngine::start_run_ext` and
    /// `RunEngine::persist_status`; wiring here keeps projection persistence on
    /// the production run lifecycle instead of isolated test helpers.
    pub fn with_projection_store(
        mut self,
        projection_store: Arc<DatabaseStateProjectionStore>,
    ) -> Self {
        self.projection_store = Some(projection_store);
        self
    }

    /// Attach the shared metrics registry used by `/metrics`.
    pub fn with_metrics_registry(mut self, registry: Arc<MetricsRegistry>) -> Self {
        register_run_control_poll_metrics(&registry);
        register_run_recovery_metrics(&registry);
        self.metrics_registry = Some(registry);
        self
    }

    pub(crate) fn metrics_registry(&self) -> Option<&Arc<MetricsRegistry>> {
        self.metrics_registry.as_ref()
    }

    pub(crate) fn owner_lease_duration(&self) -> Option<Duration> {
        self.store.owner_lease_duration()
    }

    /// Start renewing the store owner's active-run lease until the returned
    /// guard is dropped. Stores without shared owner leases return `None`.
    pub(crate) fn start_owner_lease_heartbeat(
        &self,
        user_id: String,
        expected_session_id: String,
        run_id: String,
        expected_owner_generation: u64,
        execution_lease_lost: Arc<AtomicBool>,
        execution_cancel_token: Arc<tokio_util::sync::CancellationToken>,
    ) -> Option<RunOwnerLeaseHeartbeat> {
        let interval = self
            .store
            .owner_lease_renewal_interval()?
            .max(Duration::from_millis(1));
        let lease_duration = self
            .store
            .owner_lease_duration()?
            .max(Duration::from_millis(1));
        // Fence before the database TTL can expire. This leaves one renewal
        // interval as skew/scheduling margin and makes a hung renewal future
        // unable to outlive execution authority.
        let fence_window = lease_duration
            .saturating_sub(interval)
            .max(Duration::from_millis(1));
        let engine = self.clone();
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();
        let join = tokio::spawn(async move {
            let mut fence_deadline = tokio::time::Instant::now() + fence_window;
            let mut next_renewal = tokio::time::Instant::now();
            let mut ownership_lost = false;
            loop {
                tokio::select! {
                    biased;
                    _ = &mut stop_rx => break,
                    _ = tokio::time::sleep_until(fence_deadline) => {
                        ownership_lost = true;
                        break;
                    }
                    _ = tokio::time::sleep_until(next_renewal) => {}
                }

                let renewal_started_at = tokio::time::Instant::now();
                let renewal = engine.store.renew_owner_lease(
                    &user_id,
                    &expected_session_id,
                    &run_id,
                    expected_owner_generation,
                    OWNER_LEASE_RENEWAL_STATUSES,
                );
                let renewed = tokio::select! {
                    biased;
                    _ = &mut stop_rx => break,
                    _ = tokio::time::sleep_until(fence_deadline) => {
                        ownership_lost = true;
                        break;
                    }
                    renewed = renewal => renewed,
                };
                match renewed {
                    Ok(true) => {
                        let now = tokio::time::Instant::now();
                        // The database lease begins when the renewal is
                        // committed, not when its acknowledgement eventually
                        // reaches this task. Anchoring the local fence to the
                        // request start is deliberately conservative and
                        // prevents a delayed response from extending local
                        // execution beyond durable authority.
                        fence_deadline = renewal_started_at + fence_window;
                        next_renewal = now + interval;
                    }
                    Ok(false) => {
                        tracing::warn!(
                            target: "astra_runtime::run_engine",
                            run_id = %run_id,
                            expected_owner_generation,
                            "execution owner lease was superseded; fencing the local producer"
                        );
                        ownership_lost = true;
                        break;
                    }
                    Err(error) => {
                        tracing::warn!(
                            target: "astra_runtime::run_engine",
                            run_id = %run_id,
                            expected_owner_generation,
                            error = %error,
                            "failed to renew active run owner lease"
                        );
                        next_renewal = (tokio::time::Instant::now() + interval).min(fence_deadline);
                    }
                }
            }

            if ownership_lost {
                execution_lease_lost.store(true, Ordering::Release);
                execution_cancel_token.cancel();
                return;
            }

            match tokio::time::timeout(
                OWNER_LEASE_RELEASE_MAX_WAIT,
                engine.store.release_owner_lease(
                    &user_id,
                    &expected_session_id,
                    &run_id,
                    expected_owner_generation,
                ),
            )
            .await
            {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    tracing::warn!(
                        target: "astra_runtime::run_engine",
                        run_id = %run_id,
                        error = %error,
                        "failed to release run owner lease after executor exit"
                    );
                }
                Err(_) => {
                    tracing::warn!(
                        target: "astra_runtime::run_engine",
                        run_id = %run_id,
                        timeout_ms = OWNER_LEASE_RELEASE_MAX_WAIT.as_millis(),
                        "timed out releasing run owner lease; durable TTL will recover it"
                    );
                }
            }
        });

        Some(RunOwnerLeaseHeartbeat {
            stop_tx: Some(stop_tx),
            _join: join,
        })
    }

    /// Prove and refresh execution authority at the activation boundary.
    /// This closes the durable-start-to-executor gap for delegated work: an
    /// exact generation alone is not enough once its lease has expired.
    pub(crate) async fn confirm_execution_authority(
        &self,
        user_id: &str,
        expected_session_id: &str,
        run_id: &str,
        expected_owner_generation: u64,
        cancel_token: &tokio_util::sync::CancellationToken,
    ) -> Result<bool, String> {
        let Some(lease_duration) = self.store.owner_lease_duration() else {
            return Ok(true);
        };
        let renewal_interval = self
            .store
            .owner_lease_renewal_interval()
            .unwrap_or_else(|| lease_duration / 3)
            .max(Duration::from_millis(1));
        let activation_wait = renewal_interval
            .min(lease_duration)
            .min(OWNER_LEASE_ACTIVATION_MAX_WAIT)
            .max(Duration::from_millis(1));
        let renew = self.store.renew_owner_lease(
            user_id,
            expected_session_id,
            run_id,
            expected_owner_generation,
            OWNER_LEASE_RENEWAL_STATUSES,
        );
        tokio::select! {
            biased;
            _ = cancel_token.cancelled() => Err(format!(
                "durable execution authority activation was cancelled for run {run_id}"
            )),
            result = tokio::time::timeout(activation_wait, renew) => match result {
                Ok(result) => result,
                Err(_) => Err(format!(
                    "durable execution authority activation timed out after {}ms for run {run_id}; durable recovery owns the unactivated row",
                    activation_wait.as_millis()
                )),
            },
        }
    }

    /// Create a durable run record in the store.
    ///
    /// Called by `create_run()` in the lifecycle service after the in-memory
    /// RunState is inserted. This ensures the run survives process restarts.
    pub async fn start_run(
        &self,
        run_id: &str,
        user_id: &str,
        session_id: &str,
    ) -> Result<RunExecutionAuthority, String> {
        self.start_run_with_context(run_id, user_id, session_id, RunStartContext::default())
            .await
    }

    /// Create a durable run record and capture request-level interaction context.
    pub async fn start_run_with_context(
        &self,
        run_id: &str,
        user_id: &str,
        session_id: &str,
        context: RunStartContext,
    ) -> Result<RunExecutionAuthority, String> {
        self.start_run_ext_with_context(
            run_id, user_id, session_id, None, None, None, None, context,
        )
        .await
    }

    /// Extended version of `start_run` with delegation metadata.
    pub async fn start_run_ext(
        &self,
        run_id: &str,
        user_id: &str,
        session_id: &str,
        parent_run_id: Option<&str>,
        delegation_id: Option<&str>,
        agent_id: Option<&str>,
        retry_of: Option<&str>,
    ) -> Result<RunExecutionAuthority, String> {
        self.start_run_ext_with_context(
            run_id,
            user_id,
            session_id,
            parent_run_id,
            delegation_id,
            agent_id,
            retry_of,
            RunStartContext::default(),
        )
        .await
    }

    /// Load and validate the durable parent for a delegated run before any
    /// child-side effects are emitted.
    ///
    /// A run tree cannot cross user or session boundaries. User ownership is
    /// enforced by the store lookup; session ownership is checked here so a
    /// caller cannot attach a child to a run from another conversation.
    pub(crate) async fn require_delegation_parent(
        &self,
        user_id: &str,
        session_id: &str,
        parent_run_id: &str,
    ) -> Result<DurableRunRecord, String> {
        let parent = self
            .store
            .load_run(user_id, parent_run_id)
            .await?
            .ok_or_else(|| {
                "delegated run cannot be persisted without its durable parent".to_string()
            })?;
        if parent.session_id != session_id {
            return Err("delegated run and durable parent must belong to one session".to_string());
        }
        Ok(parent)
    }

    /// Extended version of `start_run` with delegation metadata and interaction context.
    pub(crate) async fn start_run_ext_with_context(
        &self,
        run_id: &str,
        user_id: &str,
        session_id: &str,
        parent_run_id: Option<&str>,
        delegation_id: Option<&str>,
        agent_id: Option<&str>,
        retry_of: Option<&str>,
        context: RunStartContext,
    ) -> Result<RunExecutionAuthority, String> {
        let record = self
            .build_run_start_record(
                run_id,
                user_id,
                session_id,
                parent_run_id,
                delegation_id,
                agent_id,
                retry_of,
                context,
            )
            .await?;
        let owner_generation = record.run_generation;
        self.store.insert_run(record).await?;
        self.project_delegation_run_if_needed(user_id, run_id, None)
            .await?;
        Ok(RunExecutionAuthority { owner_generation })
    }

    /// Atomically claim a provider-selected run identity or observe the
    /// immutable session already bound by a concurrent/retried request.
    pub async fn claim_run_with_context(
        &self,
        run_id: &str,
        user_id: &str,
        session_id: &str,
        requested_session_id: Option<&str>,
        context: RunStartContext,
    ) -> Result<DurableRunStartClaim, String> {
        let record = self
            .build_run_start_record(run_id, user_id, session_id, None, None, None, None, context)
            .await?;
        let claim = self
            .store
            .claim_run_start(record, requested_session_id)
            .await?;
        if matches!(claim, DurableRunStartClaim::Started { .. }) {
            self.project_delegation_run_if_needed(user_id, run_id, None)
                .await?;
        }
        Ok(claim)
    }

    async fn build_run_start_record(
        &self,
        run_id: &str,
        user_id: &str,
        session_id: &str,
        parent_run_id: Option<&str>,
        delegation_id: Option<&str>,
        agent_id: Option<&str>,
        retry_of: Option<&str>,
        mut context: RunStartContext,
    ) -> Result<DurableRunRecord, String> {
        let now = chrono::Utc::now().to_rfc3339();
        let (root_run_id, ancestor_path, depth) = if let Some(parent_run_id) = parent_run_id {
            let parent = self
                .require_delegation_parent(user_id, session_id, parent_run_id)
                .await?;
            inherit_parent_run_identity(&mut context, &parent)?;
            validate_child_work_binding(&context, &parent)?;
            let parent_root = parent.root_run_id.unwrap_or(parent.run_id.clone());
            let parent_path = parent.ancestor_path.unwrap_or(parent.run_id);
            (
                Some(parent_root),
                Some(format!("{parent_path}/{run_id}")),
                parent.depth.saturating_add(1),
            )
        } else {
            (Some(run_id.to_string()), Some(run_id.to_string()), 0)
        };
        let (model_offering_id, resolved_model_name) = durable_model_identity(&context)?;
        let runtime_profile = context
            .runtime_profile
            .map(runtime_profile_label)
            .map(str::to_string);
        let run_started_data = run_started_event_data(&context);
        let record = DurableRunRecord {
            run_id: run_id.to_string(),
            user_id: user_id.to_string(),
            session_id: session_id.to_string(),
            parent_run_id: parent_run_id.map(ToString::to_string),
            root_run_id,
            ancestor_path,
            depth,
            delegation_id: delegation_id.map(ToString::to_string),
            agent_id: agent_id.map(ToString::to_string),
            retry_of: retry_of.map(ToString::to_string),
            retry_scope: Some("node".to_string()),
            status: STATUS_RUNNING.to_string(),
            waiting_for: None,
            owner_pod_id: None,
            owner_lease_expires_at: None,
            run_generation: 0,
            last_event_idx: -1,
            checkpoint_version: None,
            checkpoint_json: None,
            error_code: None,
            error_message: None,
            retry_count: 0,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_tool_calls: 0,
            agent_binding_id: context.agent_binding_id,
            agent_binding_name: context.agent_binding_name,
            agent_binding_schema_version: context.agent_binding_schema_version,
            model_offering_id,
            resolved_model_name,
            runtime_profile,
            start_request_fingerprint: context.start_request_fingerprint,
            work_binding: context.work_binding,
            events: vec![serde_json::json!({
                "event_type": "run_started",
                "data": run_started_data
            })],
            created_at: now.clone(),
            updated_at: now,
        };
        Ok(record)
    }

    /// Persist a status change to the durable store.
    pub async fn persist_status(
        &self,
        user_id: &str,
        expected_session_id: &str,
        run_id: &str,
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<bool, String> {
        if durable_run_status_kind(status) == DurableRunStatusKind::Cancelled {
            return Err(format!(
                "persist_status cannot infer cancellation authority for run {run_id}; use the durable User marker flow or cancel_if_exact_live_owner with an explicit Runtime/Unverified origin"
            ));
        }
        let terminal = matches!(
            durable_run_status_kind(status),
            DurableRunStatusKind::Completed
                | DurableRunStatusKind::Delegated
                | DurableRunStatusKind::Failed
        );
        let updated = if terminal {
            let Some(current) = self.store.load_run(user_id, run_id).await? else {
                return Ok(false);
            };
            self.store
                .update_run_status_with_events_if_current(
                    user_id,
                    expected_session_id,
                    run_id,
                    &[current.status.as_str()],
                    None,
                    status,
                    waiting_for,
                    error_message,
                    &[],
                )
                .await?
        } else {
            self.store
                .update_run_status(
                    user_id,
                    expected_session_id,
                    run_id,
                    status,
                    waiting_for,
                    error_message,
                )
                .await?
        };
        if updated {
            let summary = error_message.or(waiting_for);
            if let Err(error) = self
                .project_delegation_run_if_needed(user_id, run_id, summary)
                .await
            {
                tracing::warn!(
                    user_id,
                    run_id,
                    status,
                    error = %error,
                    "run transition committed but delegation projection refresh failed"
                );
            }
        }
        Ok(updated)
    }

    /// Persist a status change only if the durable row is still in one of the
    /// expected states. This prevents stale control-plane observations from
    /// overwriting a newer pause/cancel/terminal status.
    pub async fn persist_status_if_current(
        &self,
        request: RunStatusCasRequest<'_>,
    ) -> Result<bool, String> {
        let RunStatusCasRequest {
            user_id,
            expected_session_id,
            run_id,
            expected_statuses,
            status,
            waiting_for,
            error_message,
        } = request;
        if durable_run_status_kind(status) == DurableRunStatusKind::Cancelled {
            return Err(format!(
                "persist_status_if_current cannot infer cancellation authority for run {run_id}; use the durable User marker flow or cancel_if_exact_live_owner with an explicit Runtime/Unverified origin"
            ));
        }
        let terminal = matches!(
            durable_run_status_kind(status),
            DurableRunStatusKind::Completed
                | DurableRunStatusKind::Delegated
                | DurableRunStatusKind::Failed
        );
        let updated = if terminal {
            self.store
                .update_run_status_with_events_if_current(
                    user_id,
                    expected_session_id,
                    run_id,
                    expected_statuses,
                    None,
                    status,
                    waiting_for,
                    error_message,
                    &[],
                )
                .await?
        } else {
            self.store
                .update_run_status_if_current(RunStatusCasRequest {
                    user_id,
                    expected_session_id,
                    run_id,
                    expected_statuses,
                    status,
                    waiting_for,
                    error_message,
                })
                .await?
        };
        if updated {
            let summary = error_message.or(waiting_for);
            if let Err(error) = self
                .project_delegation_run_if_needed(user_id, run_id, summary)
                .await
            {
                tracing::warn!(
                    user_id,
                    run_id,
                    status,
                    error = %error,
                    "run transition committed but delegation projection refresh failed"
                );
            }
        }
        Ok(updated)
    }

    /// Build an explicit typed cancellation fact for cross-module tests.
    ///
    /// Production callers must use their real User marker/orphan or exact-live
    /// Runtime owner flow. Keeping this test-only prevents the old ambiguous
    /// status-only API from reappearing as a production authority shortcut.
    #[cfg(test)]
    pub(crate) async fn persist_typed_cancellation_fixture(
        &self,
        user_id: &str,
        expected_session_id: &str,
        run_id: &str,
        expected_statuses: &[&str],
        origin: astra_turn_core::orchestration_types::CancellationOrigin,
    ) -> Result<bool, String> {
        if origin == astra_turn_core::orchestration_types::CancellationOrigin::User
            && !self.request_run_cancellation(user_id, run_id).await?
        {
            return Ok(false);
        }
        self.transition_status_with_events_if_current(
            user_id,
            expected_session_id,
            run_id,
            expected_statuses,
            STATUS_CANCELLED,
            None,
            None,
            &[serde_json::json!({
                "event_type": "run_finished",
                "data": {
                    "run_id": run_id,
                    "status": STATUS_CANCELLED,
                    "cancelled": true,
                    "reason": "explicit typed test cancellation",
                    "source": "test_fixture",
                    "cancellation_origin": origin,
                }
            })],
        )
        .await
    }

    /// Persist an executor-produced delegation outcome without allowing a
    /// stale child completion to overwrite a concurrent pause or cancel.
    ///
    /// The durable run state is authoritative. Terminal outcomes are committed
    /// with their replay events in the same CAS; a lost CAS is reported as
    /// `Ok(false)` and the winning durable state remains untouched.
    pub async fn persist_delegation_outcome_status(
        &self,
        user_id: &str,
        expected_session_id: &str,
        run_id: &str,
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<bool, String> {
        let transition = delegation_outcome_transition(status).ok_or_else(|| {
            format!("unsupported delegation outcome status '{status}' for run {run_id}")
        })?;

        if !transition.terminal {
            return self
                .persist_status_if_current(RunStatusCasRequest {
                    user_id,
                    expected_session_id,
                    run_id,
                    expected_statuses: transition.expected_statuses,
                    status: transition.canonical_status,
                    waiting_for,
                    error_message,
                })
                .await;
        }

        let events = delegation_terminal_events(transition.canonical_status, error_message);

        match self
            .commit_terminal_status_with_events_if_current(
                user_id,
                expected_session_id,
                run_id,
                transition.expected_statuses,
                transition.canonical_status,
                waiting_for,
                error_message,
                &events,
            )
            .await?
        {
            TerminalTransitionOutcome::Committed(_) => Ok(true),
            TerminalTransitionOutcome::Superseded(durable) => {
                tracing::info!(
                    target: "astra_runtime::delegation",
                    user_id,
                    run_id,
                    attempted_status = transition.canonical_status,
                    durable_status = %durable.status,
                    "delegation outcome lost its status CAS; preserving durable authority"
                );
                Ok(false)
            }
        }
    }

    /// Owner-fenced counterpart of [`Self::persist_delegation_outcome_status`].
    ///
    /// Scheduler-owned executors do not emit their own durable lifecycle. This
    /// method keeps their terminal status and replay events under the same
    /// generation CAS, rather than allowing a status-only terminal row that a
    /// reconnecting client cannot observe.
    pub async fn persist_delegation_outcome_status_if_current_owner(
        &self,
        user_id: &str,
        expected_session_id: &str,
        run_id: &str,
        expected_owner_generation: u64,
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<bool, String> {
        let transition = delegation_outcome_transition(status).ok_or_else(|| {
            format!("unsupported delegation outcome status '{status}' for run {run_id}")
        })?;

        if !transition.terminal {
            return self
                .transition_status_with_events_if_current_owner(
                    user_id,
                    expected_session_id,
                    run_id,
                    transition.expected_statuses,
                    expected_owner_generation,
                    transition.canonical_status,
                    waiting_for,
                    error_message,
                    &[],
                )
                .await;
        }

        let events = delegation_terminal_events(transition.canonical_status, error_message);
        match self
            .commit_terminal_status_with_events_if_current_owner(
                user_id,
                expected_session_id,
                run_id,
                transition.expected_statuses,
                expected_owner_generation,
                transition.canonical_status,
                waiting_for,
                error_message,
                &events,
            )
            .await?
        {
            TerminalTransitionOutcome::Committed(_) => Ok(true),
            TerminalTransitionOutcome::Superseded(durable) => {
                tracing::info!(
                    target: "astra_runtime::delegation",
                    user_id,
                    run_id,
                    expected_owner_generation,
                    attempted_status = transition.canonical_status,
                    durable_status = %durable.status,
                    "owner-fenced delegation outcome lost its status CAS; preserving durable authority"
                );
                Ok(false)
            }
        }
    }

    /// Atomically persist a status transition and its durable audit event.
    ///
    /// Use this for user/control-plane transitions where status without the
    /// corresponding event would be an inconsistent durable fact.
    pub async fn transition_status_with_event_if_current(
        &self,
        user_id: &str,
        expected_session_id: &str,
        run_id: &str,
        expected_statuses: &[&str],
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
        event: serde_json::Value,
    ) -> Result<bool, String> {
        let terminal = matches!(
            durable_run_status_kind(status),
            DurableRunStatusKind::Cancelled
                | DurableRunStatusKind::Completed
                | DurableRunStatusKind::Delegated
                | DurableRunStatusKind::Failed
        ) || event
            .pointer("/data/releases_session_slot")
            .and_then(serde_json::Value::as_bool)
            == Some(true);
        let updated = if terminal {
            self.store
                .update_run_status_with_events_if_current(
                    user_id,
                    expected_session_id,
                    run_id,
                    expected_statuses,
                    None,
                    status,
                    waiting_for,
                    error_message,
                    std::slice::from_ref(&event),
                )
                .await?
        } else {
            self.store
                .update_run_status_with_event_if_current(
                    user_id,
                    expected_session_id,
                    run_id,
                    expected_statuses,
                    status,
                    waiting_for,
                    error_message,
                    event,
                )
                .await?
        };
        if updated {
            let summary = error_message.or(waiting_for);
            if let Err(error) = self
                .project_delegation_run_if_needed(user_id, run_id, summary)
                .await
            {
                tracing::warn!(
                    user_id,
                    run_id,
                    status,
                    error = %error,
                    "run transition committed but delegation projection refresh failed"
                );
            }
        }
        Ok(updated)
    }

    /// Atomically persist a status transition and durable audit event only
    /// when the session has no other blocking run.
    ///
    /// The current run is excluded from the session guard so a paused run can
    /// resume itself; any sibling active/input/waiting/manual-paused run blocks
    /// the transition.
    ///
    /// Delegation projection refresh after a committed transition is
    /// intentionally best-effort and outside the store transaction. The
    /// authoritative facts are the durable run row, run events, and session
    /// execution slot; projection failures are repairable derived-state lag.
    #[allow(clippy::too_many_arguments)]
    pub async fn transition_status_with_event_if_current_unless_session_blocked(
        &self,
        user_id: &str,
        run_id: &str,
        session_id: &str,
        expected_statuses: &[&str],
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
        forbid_open_settlement_generation: Option<u64>,
        event: serde_json::Value,
    ) -> Result<GuardedRunStatusTransition, String> {
        let outcome = self
            .store
            .update_run_status_with_event_if_current_unless_session_blocked(
                GuardedRunStatusTransitionRequest {
                    user_id,
                    run_id,
                    expected_session_id: session_id,
                    expected_statuses,
                    status,
                    waiting_for,
                    error_message,
                    forbid_open_settlement_generation,
                    event,
                },
            )
            .await?;
        if outcome == GuardedRunStatusTransition::Updated {
            let summary = error_message.or(waiting_for);
            if let Err(error) = self
                .project_delegation_run_if_needed(user_id, run_id, summary)
                .await
            {
                tracing::warn!(
                    user_id,
                    run_id,
                    status,
                    error = %error,
                    "guarded run transition committed but delegation projection refresh failed"
                );
            }
        }
        Ok(outcome)
    }

    /// Atomically persist a status transition and a durable audit event batch.
    ///
    /// Empty `events` is valid and behaves as a CAS status transition.
    pub async fn transition_status_with_events_if_current(
        &self,
        user_id: &str,
        expected_session_id: &str,
        run_id: &str,
        expected_statuses: &[&str],
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
        events: &[serde_json::Value],
    ) -> Result<bool, String> {
        let updated = self
            .store
            .update_run_status_with_events_if_current(
                user_id,
                expected_session_id,
                run_id,
                expected_statuses,
                None,
                status,
                waiting_for,
                error_message,
                events,
            )
            .await?;
        if updated {
            let summary = error_message.or(waiting_for);
            if let Err(error) = self
                .project_delegation_run_if_needed(user_id, run_id, summary)
                .await
            {
                tracing::warn!(
                    user_id,
                    run_id,
                    status,
                    error = %error,
                    "run transition committed but delegation projection refresh failed"
                );
            }
        }
        Ok(updated)
    }

    pub async fn load_run_event_by_idempotency_key(
        &self,
        user_id: &str,
        run_id: &str,
        event_type: &str,
        idempotency_key: &str,
    ) -> Result<Option<serde_json::Value>, String> {
        self.store
            .load_run_event_by_idempotency_key(user_id, run_id, event_type, idempotency_key)
            .await
    }

    pub async fn load_run_guidance_admission(
        &self,
        user_id: &str,
        run_id: &str,
        intent_id: &str,
    ) -> Result<Option<DurableRunGuidanceAdmissionRecord>, String> {
        self.store
            .load_run_guidance_admission(user_id, run_id, intent_id)
            .await
    }

    pub async fn admit_run_guidance(
        &self,
        request: AtomicRunGuidanceAdmissionRequest<'_>,
    ) -> Result<AtomicRunGuidanceAdmission, String> {
        self.store.admit_run_guidance(request).await
    }

    pub async fn load_run_status_snapshot(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Result<Option<DurableRunStatusSnapshot>, String> {
        self.store.load_run_status_snapshot(user_id, run_id).await
    }

    /// The execution-owner variant of the status/event CAS. Unlike a
    /// process-local lease-lost check, this predicate is evaluated atomically
    /// with the durable transition, so a recovered owner generation cannot be
    /// overwritten by a stale executor.
    #[allow(clippy::too_many_arguments)]
    pub async fn transition_status_with_events_if_current_owner(
        &self,
        user_id: &str,
        expected_session_id: &str,
        run_id: &str,
        expected_statuses: &[&str],
        expected_owner_generation: u64,
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
        events: &[serde_json::Value],
    ) -> Result<bool, String> {
        let updated = self
            .store
            .update_run_status_with_events_if_current(
                user_id,
                expected_session_id,
                run_id,
                expected_statuses,
                Some(expected_owner_generation),
                status,
                waiting_for,
                error_message,
                events,
            )
            .await?;
        if updated {
            let summary = error_message.or(waiting_for);
            if let Err(error) = self
                .project_delegation_run_if_needed(user_id, run_id, summary)
                .await
            {
                tracing::warn!(
                    user_id,
                    run_id,
                    status,
                    expected_owner_generation,
                    error = %error,
                    "owner-fenced run transition committed but delegation projection refresh failed"
                );
            }
        }
        Ok(updated)
    }

    /// Append generation-owned facts without mutating the winning lifecycle
    /// status or its waiting/session-slot projection.
    pub async fn append_events_if_current_generation_and_status(
        &self,
        user_id: &str,
        expected_session_id: &str,
        run_id: &str,
        expected_generation: u64,
        expected_statuses: &[&str],
        events: &[serde_json::Value],
    ) -> Result<bool, String> {
        self.store
            .append_events_if_current_generation_and_status(
                user_id,
                expected_session_id,
                run_id,
                expected_generation,
                expected_statuses,
                events,
            )
            .await
    }

    /// Atomically cancel a run for a runtime/unverified cause while this exact
    /// execution owner still holds its live durable capability.
    ///
    /// User cancellation is admitted only through the run-level cancellation
    /// marker. This method never creates a recovery debt half-state.
    pub async fn cancel_if_exact_live_owner(
        &self,
        user_id: &str,
        expected_session_id: &str,
        run_id: &str,
        expected_owner_generation: u64,
        expected_statuses: &[&str],
        origin: astra_turn_core::orchestration_types::CancellationOrigin,
        reason: &str,
    ) -> Result<astra_services::runs::AtomicExecutionOwnerCancellation, String> {
        use astra_services::runs::{
            AtomicExecutionOwnerCancellationRequest, ExecutionOwnerCancellationOrigin,
        };
        let origin = match origin {
            astra_turn_core::orchestration_types::CancellationOrigin::Runtime => {
                ExecutionOwnerCancellationOrigin::Runtime
            }
            astra_turn_core::orchestration_types::CancellationOrigin::Unverified => {
                ExecutionOwnerCancellationOrigin::Unverified
            }
            astra_turn_core::orchestration_types::CancellationOrigin::User => {
                return Err(format!(
                    "user cancellation for run {run_id} requires the durable run-level marker"
                ));
            }
        };
        let request = AtomicExecutionOwnerCancellationRequest {
            user_id,
            run_id,
            expected_session_id,
            expected_owner_generation,
            expected_statuses,
            origin,
            reason,
        };
        let mut last_error = None;
        for attempt in 1..=TERMINAL_TRANSITION_MAX_ATTEMPTS {
            match self.store.cancel_if_exact_live_owner(request).await {
                Ok(outcome) => return Ok(outcome),
                Err(error) => {
                    last_error = Some(error.clone());
                    if attempt == TERMINAL_TRANSITION_MAX_ATTEMPTS {
                        break;
                    }
                    tracing::warn!(
                        user_id,
                        run_id,
                        expected_owner_generation,
                        attempt,
                        max_attempts = TERMINAL_TRANSITION_MAX_ATTEMPTS,
                        error = %error,
                        "atomic execution-owner cancellation failed; retrying"
                    );
                    tokio::time::sleep(terminal_transition_retry_delay(attempt)).await;
                }
            }
        }
        Err(last_error
            .unwrap_or_else(|| "atomic execution-owner cancellation retry exhausted".to_string()))
    }

    /// Atomically persist a terminal status transition and durable terminal events,
    /// retrying short-lived store errors without weakening the underlying CAS.
    ///
    /// If the store commits but the connection drops before the caller observes
    /// success, a retry can return `Ok(false)` because the status is already
    /// terminal. In that case we reconcile by loading the durable row and only
    /// treat it as committed when the stored status and terminal event batch are
    /// both present. If the status committed but the expected events are missing,
    /// repair the event batch before reporting success.
    async fn try_transition_terminal_status_with_events_if_current(
        &self,
        user_id: &str,
        expected_session_id: &str,
        run_id: &str,
        expected_statuses: &[&str],
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
        events: &[serde_json::Value],
        expected_owner_generation: Option<u64>,
    ) -> Result<bool, String> {
        let mut saw_store_error = false;
        let mut last_error: Option<String> = None;
        for attempt in 1..=TERMINAL_TRANSITION_MAX_ATTEMPTS {
            let transition = match expected_owner_generation {
                Some(generation) => {
                    self.transition_status_with_events_if_current_owner(
                        user_id,
                        expected_session_id,
                        run_id,
                        expected_statuses,
                        generation,
                        status,
                        waiting_for,
                        error_message,
                        events,
                    )
                    .await
                }
                None => {
                    self.transition_status_with_events_if_current(
                        user_id,
                        expected_session_id,
                        run_id,
                        expected_statuses,
                        status,
                        waiting_for,
                        error_message,
                        events,
                    )
                    .await
                }
            };
            match transition {
                Ok(true) => return Ok(true),
                Ok(false) if saw_store_error => {
                    return self
                        .reconcile_terminal_transition_after_store_error(
                            user_id,
                            expected_session_id,
                            run_id,
                            status,
                            waiting_for,
                            error_message,
                            events,
                            expected_owner_generation,
                            last_error.as_deref(),
                        )
                        .await;
                }
                Ok(false) => return Ok(false),
                Err(error) => {
                    saw_store_error = true;
                    last_error = Some(error.clone());
                    if attempt >= TERMINAL_TRANSITION_MAX_ATTEMPTS {
                        break;
                    }
                    tracing::warn!(
                        user_id,
                        run_id,
                        status,
                        attempt,
                        max_attempts = TERMINAL_TRANSITION_MAX_ATTEMPTS,
                        error = %error,
                        "terminal run transition failed; retrying"
                    );
                    tokio::time::sleep(terminal_transition_retry_delay(attempt)).await;
                }
            }
        }
        Err(last_error.unwrap_or_else(|| "terminal transition retry exhausted".to_string()))
    }

    /// Commit a terminal transition and return the authoritative durable fact.
    /// A lost CAS is not an ambiguous `false`: callers receive the winning row
    /// and must not publish their stale local terminal result.
    pub async fn commit_terminal_status_with_events_if_current(
        &self,
        user_id: &str,
        expected_session_id: &str,
        run_id: &str,
        expected_statuses: &[&str],
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
        events: &[serde_json::Value],
    ) -> Result<TerminalTransitionOutcome, String> {
        if self
            .try_transition_terminal_status_with_events_if_current(
                user_id,
                expected_session_id,
                run_id,
                expected_statuses,
                status,
                waiting_for,
                error_message,
                events,
                None,
            )
            .await?
        {
            let durable = self
                .load_run(user_id, run_id)
                .await?
                .ok_or_else(|| format!("run {run_id} disappeared after terminal commit"))?;
            return Ok(TerminalTransitionOutcome::Committed(Box::new(durable)));
        }

        let durable = self
            .load_run(user_id, run_id)
            .await?
            .ok_or_else(|| format!("run {run_id} disappeared after terminal transition CAS"))?;
        Ok(TerminalTransitionOutcome::Superseded(Box::new(durable)))
    }

    /// Commit a terminal result only while this exact execution generation
    /// still owns the unexpired durable lease.
    #[allow(clippy::too_many_arguments)]
    pub async fn commit_terminal_status_with_events_if_current_owner(
        &self,
        user_id: &str,
        expected_session_id: &str,
        run_id: &str,
        expected_statuses: &[&str],
        expected_owner_generation: u64,
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
        events: &[serde_json::Value],
    ) -> Result<TerminalTransitionOutcome, String> {
        if self
            .try_transition_terminal_status_with_events_if_current(
                user_id,
                expected_session_id,
                run_id,
                expected_statuses,
                status,
                waiting_for,
                error_message,
                events,
                Some(expected_owner_generation),
            )
            .await?
        {
            let durable = self
                .load_run(user_id, run_id)
                .await?
                .ok_or_else(|| format!("run {run_id} disappeared after terminal commit"))?;
            if durable.run_generation != expected_owner_generation {
                return Ok(TerminalTransitionOutcome::Superseded(Box::new(durable)));
            }
            return Ok(TerminalTransitionOutcome::Committed(Box::new(durable)));
        }

        let durable = self
            .load_run(user_id, run_id)
            .await?
            .ok_or_else(|| format!("run {run_id} disappeared after terminal transition CAS"))?;
        let control = self.store.load_run_control(user_id, run_id).await?;
        if durable.run_generation == expected_owner_generation
            && matches!(
                durable.status.as_str(),
                STATUS_RUNNING | STATUS_WAITING | STATUS_PAUSED
            )
            && control.is_some_and(|control| control.cancellation_requested)
        {
            let event = serde_json::json!({"event_type":"run_finished","data":{"run_id":run_id,"status":STATUS_CANCELLED,"cancelled":true,"reason":"durable cancellation request won terminal race","source":"terminal_transition_reconciliation","cancellation_origin":astra_turn_core::orchestration_types::CancellationOrigin::User}});
            if self
                .try_transition_terminal_status_with_events_if_current(
                    user_id,
                    expected_session_id,
                    run_id,
                    &[STATUS_RUNNING, STATUS_WAITING, STATUS_PAUSED],
                    STATUS_CANCELLED,
                    None,
                    None,
                    &[event],
                    Some(expected_owner_generation),
                )
                .await?
            {
                let cancelled = self.load_run(user_id, run_id).await?.ok_or_else(|| {
                    format!("run {run_id} disappeared after cancellation settlement")
                })?;
                // The requested completed/failed terminal did not commit. Let
                // callers project the authoritative cancelled row rather than
                // treating their stale terminal events as committed.
                return Ok(TerminalTransitionOutcome::Superseded(Box::new(cancelled)));
            }
        }
        Ok(TerminalTransitionOutcome::Superseded(Box::new(durable)))
    }

    async fn reconcile_terminal_transition_after_store_error(
        &self,
        user_id: &str,
        expected_session_id: &str,
        run_id: &str,
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
        events: &[serde_json::Value],
        expected_owner_generation: Option<u64>,
        last_error: Option<&str>,
    ) -> Result<bool, String> {
        let Some(run) = self
            .store
            .load_run(user_id, run_id)
            .await
            .map_err(|error| {
                if let Some(last_error) = last_error {
                    format!(
                        "{last_error}; failed to reconcile terminal transition after retry: {error}"
                    )
                } else {
                    error
                }
            })?
        else {
            return Ok(false);
        };
        if run.status != status
            || expected_owner_generation.is_some_and(|generation| run.run_generation != generation)
        {
            return Ok(false);
        }
        if !durable_run_contains_event_batch(&run, events) {
            self.store
                .append_events_batch(user_id, expected_session_id, run_id, events)
                .await
                .map_err(|error| {
                    if let Some(last_error) = last_error {
                        format!(
                            "{last_error}; terminal transition reached status {status} but event repair failed: {error}"
                        )
                    } else {
                        format!(
                            "terminal transition reached status {status} but event repair failed: {error}"
                        )
                    }
                })?;
            tracing::warn!(
                user_id,
                run_id,
                status,
                repaired_event_count = events.len(),
                "terminal run transition reconciled status but had to repair missing events"
            );
        }
        let summary = error_message.or(waiting_for);
        if let Err(error) = self
            .project_delegation_run_if_needed(user_id, run_id, summary)
            .await
        {
            tracing::warn!(
                user_id,
                run_id,
                status,
                error = %error,
                "terminal run transition reconciled but delegation projection refresh failed"
            );
        }
        Ok(true)
    }

    async fn project_delegation_run_if_needed(
        &self,
        user_id: &str,
        run_id: &str,
        last_summary_text: Option<&str>,
    ) -> Result<(), String> {
        let Some(projection_store) = self.projection_store.as_ref() else {
            return Ok(());
        };
        let Some(run) = self
            .store
            .load_run_delegation_projection_target(user_id, run_id)
            .await?
        else {
            return Ok(());
        };
        if run.parent_run_id.is_none() || run.delegation_id.is_none() {
            return Ok(());
        }
        projection_store
            .upsert_delegation_projection_for_run(
                &run.user_id,
                run_id,
                run.agent_id.as_deref(),
                last_summary_text,
            )
            .await
            .map_err(|error| {
                format!("state projection update failed for delegated run {run_id}: {error}")
            })
    }

    /// Persist token/tool usage counters.
    pub async fn persist_usage(
        &self,
        user_id: &str,
        expected_session_id: &str,
        run_id: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
        tool_calls: u32,
    ) -> Result<bool, String> {
        self.store
            .update_run_usage(
                user_id,
                expected_session_id,
                run_id,
                prompt_tokens,
                completion_tokens,
                tool_calls,
            )
            .await
    }

    /// Persist the semantic run aggregate only for the exact execution
    /// generation that produced it. Provider-attempt accounting remains
    /// independent and append-only; this protects the user-visible run total
    /// from a stale executor's absolute overwrite.
    pub async fn persist_usage_if_current_owner(
        &self,
        user_id: &str,
        expected_session_id: &str,
        run_id: &str,
        expected_owner_generation: u64,
        prompt_tokens: u64,
        completion_tokens: u64,
        tool_calls: u32,
    ) -> Result<bool, String> {
        self.store
            .update_run_usage_if_current_owner(RunUsageOwnerUpdateRequest {
                user_id,
                expected_session_id,
                run_id,
                expected_owner_generation,
                prompt_tokens,
                completion_tokens,
                tool_calls,
            })
            .await
    }

    /// Save a checkpoint for crash recovery.
    pub async fn persist_checkpoint(
        &self,
        user_id: &str,
        expected_session_id: &str,
        run_id: &str,
        checkpoint_json: &str,
    ) -> Result<bool, String> {
        self.store
            .save_checkpoint(user_id, expected_session_id, run_id, checkpoint_json)
            .await
    }

    /// Load the newest typed checkpoint for a run.
    pub async fn load_latest_checkpoint(
        &self,
        user_id: &str,
        run_id: &str,
        checkpoint_kind: Option<&str>,
    ) -> Result<Option<DurableRunCheckpointRecord>, String> {
        self.store
            .load_latest_checkpoint(user_id, run_id, checkpoint_kind)
            .await
    }

    /// Load the current durable display projection for a run.
    pub async fn load_run_projection(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Result<Option<DurableRunDisplayProjectionRecord>, String> {
        self.store.load_run_projection(user_id, run_id).await
    }

    /// Rebuild the durable display projection from authoritative run facts.
    pub async fn rebuild_run_projection(
        &self,
        user_id: &str,
        expected_session_id: &str,
        run_id: &str,
    ) -> Result<Option<DurableRunDisplayProjectionRecord>, String> {
        self.store
            .rebuild_run_projection(user_id, expected_session_id, run_id)
            .await
    }

    /// Append an event to the durable event log.
    pub async fn append_event(
        &self,
        user_id: &str,
        expected_session_id: &str,
        run_id: &str,
        event: serde_json::Value,
    ) -> Result<(), String> {
        self.store
            .append_event(user_id, expected_session_id, run_id, event)
            .await
    }

    /// Append multiple events in a single batch.
    pub async fn append_events_batch(
        &self,
        user_id: &str,
        expected_session_id: &str,
        run_id: &str,
        events: &[serde_json::Value],
    ) -> Result<(), String> {
        self.store
            .append_events_batch(user_id, expected_session_id, run_id, events)
            .await
    }

    /// Atomically commit one Edge/browser action grant together with its
    /// exact replayable tool-request outbox fact. The store's immutable local
    /// owner identity is the execution capability; callers cannot supply or
    /// reconstruct it from mutable run state.
    pub async fn commit_guarded_tool_request(
        &self,
        user_id: &str,
        run_id: &str,
        session_id: &str,
        action_id: &str,
        expected_control_epoch: i64,
        expected_owner_generation: u64,
        tool_request_event: &serde_json::Value,
    ) -> Result<AtomicRunToolRequestCommitOutcome, String> {
        self.store
            .commit_guarded_tool_request(AtomicRunToolRequestCommitRequest {
                action: AtomicRunActionAdmissionRequest {
                    user_id,
                    run_id,
                    expected_session_id: session_id,
                    action_id,
                    expected_control_epoch,
                    expected_owner_generation,
                },
                tool_request_event,
            })
            .await
    }

    /// Load a run from the durable store (cache miss or recovery path).
    pub async fn load_run(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Result<Option<DurableRunRecord>, String> {
        self.store.load_run(user_id, run_id).await
    }

    /// Read only the durable user-intent control plane. Approval and action
    /// fences must not hydrate an entire long-running event history merely to
    /// answer this boolean question.
    pub async fn has_unsettled_user_intent(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Result<Option<bool>, String> {
        self.store.has_unsettled_user_intent(user_id, run_id).await
    }

    /// Persist a stop request on the independent control plane.  This does
    /// not acquire the session execution fence, so it can interrupt a turn
    /// that is currently blocked inside that fence.
    pub async fn request_run_cancellation(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Result<bool, String> {
        self.store.request_run_cancellation(user_id, run_id).await
    }

    pub async fn terminalize_orphaned_run_cancellation(
        &self,
        request: AtomicOrphanRunCancellationRequest<'_>,
    ) -> Result<bool, String> {
        let updated = self
            .store
            .terminalize_orphaned_run_cancellation(request)
            .await?;
        if updated
            && let Err(error) = self
                .project_delegation_run_if_needed(request.user_id, request.run_id, None)
                .await
        {
            tracing::warn!(
                user_id = request.user_id,
                run_id = request.run_id,
                error = %error,
                "orphan cancellation committed but delegation projection refresh failed"
            );
        }
        Ok(updated)
    }

    pub async fn load_run_control(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Result<Option<astra_services::runs::DurableRunControlRecord>, String> {
        self.store.load_run_control(user_id, run_id).await
    }

    pub async fn load_run_event_delta(
        &self,
        user_id: &str,
        run_id: &str,
        after_event_idx: i64,
    ) -> Result<Option<DurableRunEventDelta>, String> {
        self.store
            .load_run_event_delta(user_id, run_id, after_event_idx)
            .await
    }

    /// Lightweight shared-state check used by durable interaction waiters.
    /// It avoids hydrating the event log on every poll while still observing
    /// terminal transitions and resolutions committed by another pod.
    pub async fn run_is_waiting_for(
        &self,
        user_id: &str,
        run_id: &str,
        waiting_for: &str,
    ) -> Result<bool, String> {
        Ok(self
            .store
            .load_run_control(user_id, run_id)
            .await?
            .is_some_and(|run| {
                run.status == STATUS_WAITING && run.waiting_for.as_deref() == Some(waiting_for)
            }))
    }

    /// Check whether this run or any of its durable ancestors has been
    /// cancelled or paused externally (for example by another pod).
    ///
    /// An ancestor cancellation wins over every pause so a child cannot keep
    /// waiting merely because it observed an older local pause first. This
    /// makes parent control safe without copying a mutable pause flag into
    /// every descendant row.
    pub async fn check_control_status(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Result<Option<RunControlStatus>, String> {
        let record = match self.store.load_run_control(user_id, run_id).await {
            Ok(record) => {
                record_control_poll_attempt(self.metrics_registry.as_ref(), "status", "ok");
                record
            }
            Err(error) => {
                record_control_poll_attempt(self.metrics_registry.as_ref(), "status", "error");
                record_control_poll_error(self.metrics_registry.as_ref(), "status", "store");
                return Err(error);
            }
        };
        let Some(record) = record else {
            return Ok(None);
        };
        let own_status = durable_run_status_kind(&record.status);
        if matches!(
            own_status,
            DurableRunStatusKind::Completed | DurableRunStatusKind::Failed
        ) {
            return Ok(None);
        }
        if matches!(own_status, DurableRunStatusKind::Cancelled) {
            return Ok(Some(RunControlStatus::Cancelled));
        }
        if record.cancellation_requested {
            return Ok(Some(RunControlStatus::Cancelled));
        }

        let ancestor_ids = match record.parent_run_id.as_deref() {
            None => Vec::new(),
            Some(parent_run_id) => {
                let Some(path) = record.ancestor_path.as_deref() else {
                    record_control_poll_error(
                        self.metrics_registry.as_ref(),
                        "status",
                        "invalid_lineage",
                    );
                    return Err(format!(
                        "durable child run {run_id} is missing ancestor_path"
                    ));
                };
                let segments = path
                    .split('/')
                    .filter(|segment| !segment.is_empty())
                    .collect::<Vec<_>>();
                if segments.last().copied() != Some(run_id)
                    || segments.len() < 2
                    || segments[segments.len() - 2] != parent_run_id
                {
                    record_control_poll_error(
                        self.metrics_registry.as_ref(),
                        "status",
                        "invalid_lineage",
                    );
                    return Err(format!(
                        "durable child run {run_id} has inconsistent ancestor_path"
                    ));
                }
                let mut seen = HashSet::with_capacity(segments.len());
                let ancestors = segments[..segments.len() - 1]
                    .iter()
                    .map(|segment| (*segment).to_string())
                    .collect::<Vec<_>>();
                if ancestors.iter().any(|ancestor| !seen.insert(ancestor)) {
                    record_control_poll_error(
                        self.metrics_registry.as_ref(),
                        "status",
                        "lineage_cycle",
                    );
                    return Err(format!(
                        "durable run lineage cycle while checking control for {run_id}"
                    ));
                }
                ancestors
            }
        };

        let ancestors = self.store.load_run_controls(user_id, &ancestor_ids).await?;
        if ancestors.len() != ancestor_ids.len() {
            record_control_poll_error(self.metrics_registry.as_ref(), "status", "missing_ancestor");
            return Err(format!(
                "durable run ancestor missing while checking control for {run_id}"
            ));
        }
        let mut inherited_pause = false;
        for ancestor in ancestors {
            if ancestor.cancellation_requested {
                return Ok(Some(RunControlStatus::Cancelled));
            }
            match durable_run_status_kind(&ancestor.status) {
                DurableRunStatusKind::Cancelled => {
                    if self
                        .latest_terminal_cancellation_origin(user_id, &ancestor.run_id)
                        .await?
                        == Some(astra_turn_core::orchestration_types::CancellationOrigin::User)
                    {
                        return Ok(Some(RunControlStatus::Cancelled));
                    }
                }
                DurableRunStatusKind::Paused if ancestor.waiting_for.is_some() => {
                    inherited_pause = true;
                }
                _ => {}
            }
        }

        Ok(match own_status {
            DurableRunStatusKind::Cancelled => Some(RunControlStatus::Cancelled),
            DurableRunStatusKind::Paused => Some(RunControlStatus::Paused),
            _ if inherited_pause => Some(RunControlStatus::Paused),
            _ => None,
        })
    }

    /// Return whether this exact run or one of its validated durable
    /// ancestors carries a user cancellation request.
    ///
    /// Terminal `cancelled` status is deliberately not evidence of origin:
    /// Runtime deadlines, shutdown, and other system controls use the same
    /// lifecycle terminal. Once a target or ancestor is terminal, its exact
    /// typed `run_finished` origin is canonical; cancelled status alone is
    /// never evidence. Before terminal settlement, only the canonical
    /// cancellation-request marker derives user origin. Missing or malformed
    /// lineage and terminal facts remain unverified rather than guessed.
    pub(crate) async fn cancellation_origin_in_lineage(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Result<astra_turn_core::orchestration_types::CancellationOrigin, String> {
        tokio::time::timeout(
            crate::turn::run_control::CANCELLATION_ORIGIN_LOOKUP_TIMEOUT,
            self.cancellation_origin_in_lineage_unbounded(user_id, run_id),
        )
        .await
        .map_err(|_| {
            format!(
                "cancellation origin lookup timed out after {}ms for run {run_id}",
                crate::turn::run_control::CANCELLATION_ORIGIN_LOOKUP_TIMEOUT.as_millis()
            )
        })?
    }

    /// Bounded indexed lookup for the canonical typed terminal origin.
    ///
    /// Lifecycle callers use this instead of hydrating the complete event
    /// history of a long run.
    pub(crate) async fn latest_terminal_cancellation_origin(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Result<Option<astra_turn_core::orchestration_types::CancellationOrigin>, String> {
        tokio::time::timeout(
            crate::turn::run_control::CANCELLATION_ORIGIN_LOOKUP_TIMEOUT,
            self.store
                .load_latest_terminal_cancellation_origin(user_id, run_id),
        )
        .await
        .map_err(|_| {
            format!(
                "terminal cancellation origin lookup timed out after {}ms for run {run_id}",
                crate::turn::run_control::CANCELLATION_ORIGIN_LOOKUP_TIMEOUT.as_millis()
            )
        })?
        .map(|origin| origin.map(turn_cancellation_origin))
    }

    async fn cancellation_origin_in_lineage_unbounded(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Result<astra_turn_core::orchestration_types::CancellationOrigin, String> {
        let target = self
            .store
            .load_run_control(user_id, run_id)
            .await?
            .ok_or_else(|| {
                format!("durable run {run_id} is missing while checking cancellation origin")
            })?;
        let path = target.ancestor_path.as_deref().ok_or_else(|| {
            format!(
                "durable run {run_id} is missing ancestor_path while checking cancellation origin"
            )
        })?;
        let segments = path.split('/').collect::<Vec<_>>();
        if segments.is_empty()
            || segments.iter().any(|segment| segment.is_empty())
            || segments.last().copied() != Some(run_id)
        {
            return Err(format!(
                "durable run {run_id} has malformed ancestor_path while checking cancellation origin"
            ));
        }
        let expected_parent = (segments.len() > 1).then(|| segments[segments.len() - 2]);
        if target.parent_run_id.as_deref() != expected_parent {
            return Err(format!(
                "durable run {run_id} has inconsistent parent lineage while checking cancellation origin"
            ));
        }
        let mut seen = HashSet::with_capacity(segments.len());
        if segments.iter().any(|segment| !seen.insert(*segment)) {
            return Err(format!(
                "durable run lineage cycle while checking cancellation origin for {run_id}"
            ));
        }

        let ancestor_ids = segments[..segments.len() - 1]
            .iter()
            .map(|segment| (*segment).to_string())
            .collect::<Vec<_>>();
        let ancestors = self.store.load_run_controls(user_id, &ancestor_ids).await?;
        if ancestors.len() != ancestor_ids.len() {
            return Err(format!(
                "durable run ancestor missing while checking cancellation origin for {run_id}"
            ));
        }
        let mut ancestors_by_id = HashMap::with_capacity(ancestors.len());
        for ancestor in ancestors {
            let ancestor_id = ancestor.run_id.clone();
            if ancestors_by_id
                .insert(ancestor_id.clone(), ancestor)
                .is_some()
            {
                return Err(format!(
                    "durable run ancestor {ancestor_id} is duplicated while checking cancellation origin for {run_id}"
                ));
            }
        }

        let mut validated_ancestors = Vec::with_capacity(ancestor_ids.len());
        for (index, ancestor_id) in ancestor_ids.iter().enumerate() {
            let ancestor = ancestors_by_id.remove(ancestor_id).ok_or_else(|| {
                format!(
                    "durable run ancestor {ancestor_id} is missing while checking cancellation origin for {run_id}"
                )
            })?;
            let expected_parent = index.checked_sub(1).map(|parent| segments[parent]);
            let expected_path = segments[..=index].join("/");
            if ancestor.session_id != target.session_id
                || ancestor.parent_run_id.as_deref() != expected_parent
                || ancestor.ancestor_path.as_deref() != Some(expected_path.as_str())
            {
                return Err(format!(
                    "durable run ancestor {ancestor_id} has inconsistent lineage while checking cancellation origin for {run_id}"
                ));
            }
            validated_ancestors.push(ancestor);
        }
        if !ancestors_by_id.is_empty() {
            return Err(format!(
                "durable run lineage contains unexpected ancestors while checking cancellation origin for {run_id}"
            ));
        }
        // User control is run-tree authority. Inspect the complete validated
        // lineage before considering execution-local Runtime/Unverified
        // terminals: an intermediate runtime terminal must not hide a later
        // or higher durable User marker from an active descendant.
        if target.cancellation_requested
            || validated_ancestors
                .iter()
                .any(|ancestor| ancestor.cancellation_requested)
        {
            return Ok(astra_turn_core::orchestration_types::CancellationOrigin::User);
        }

        let target_terminal_origin = if target.status == STATUS_CANCELLED {
            Some(
                self.latest_terminal_cancellation_origin(user_id, run_id)
                    .await?
                    .unwrap_or(
                        astra_turn_core::orchestration_types::CancellationOrigin::Unverified,
                    ),
            )
        } else {
            None
        };
        if target_terminal_origin
            == Some(astra_turn_core::orchestration_types::CancellationOrigin::User)
        {
            return Ok(astra_turn_core::orchestration_types::CancellationOrigin::User);
        }
        for ancestor in &validated_ancestors {
            if ancestor.status == STATUS_CANCELLED
                && self
                    .latest_terminal_cancellation_origin(user_id, &ancestor.run_id)
                    .await?
                    == Some(astra_turn_core::orchestration_types::CancellationOrigin::User)
            {
                return Ok(astra_turn_core::orchestration_types::CancellationOrigin::User);
            }
        }

        // Runtime and Unverified are execution-scoped. Only the target's own
        // exact typed terminal may supply either origin; neither crosses an
        // ancestor boundary. An active target with no durable User evidence
        // retains the local Runtime default.
        if let Some(origin) = target_terminal_origin {
            return Ok(origin);
        }
        Ok(astra_turn_core::orchestration_types::CancellationOrigin::Runtime)
    }

    /// Find all runs in WAITING status (for the resume engine to re-evaluate).
    pub async fn find_waiting_runs(&self) -> Result<Vec<DurableRunRecord>, String> {
        self.store.find_waiting_runs().await
    }

    /// Find the newest run that blocks another run in the same user/session.
    pub async fn find_blocking_session_run(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Result<Option<DurableRunRecord>, String> {
        self.store
            .find_blocking_session_run(user_id, session_id)
            .await
    }

    /// Find all sub-runs belonging to a delegation.
    pub async fn find_sub_runs(
        &self,
        user_id: &str,
        delegation_id: &str,
    ) -> Result<Vec<DurableRunRecord>, String> {
        self.store.find_sub_runs(user_id, delegation_id).await
    }

    /// Persist the verification-gate retry count for a run.
    pub async fn persist_retry_count(
        &self,
        user_id: &str,
        expected_session_id: &str,
        run_id: &str,
        retry_count: u32,
    ) -> Result<bool, String> {
        self.store
            .update_retry_count(user_id, expected_session_id, run_id, retry_count)
            .await
    }

    pub async fn load_run_interaction_event(
        &self,
        user_id: &str,
        run_id: &str,
        request_id: &str,
        event_type: &str,
    ) -> Result<Option<serde_json::Value>, String> {
        self.store
            .load_run_interaction_event(user_id, run_id, request_id, event_type)
            .await
    }

    pub async fn resolve_run_interaction(
        &self,
        user_id: &str,
        expected_session_id: &str,
        run_id: &str,
        request_id: &str,
        kind: DurableRunInteractionKind,
        response_data: serde_json::Value,
    ) -> Result<DurableRunInteractionResolveOutcome, String> {
        self.store
            .resolve_run_interaction(
                user_id,
                expected_session_id,
                run_id,
                request_id,
                kind,
                response_data,
            )
            .await
    }

    pub async fn begin_run_interaction_wait(
        &self,
        request: astra_services::runs::AtomicRunInteractionWaitRequest<'_>,
    ) -> Result<astra_services::runs::DurableRunInteractionWaitOutcome, String> {
        self.store.begin_run_interaction_wait(request).await
    }

    pub async fn register_guarded_interaction_batch(
        &self,
        request: astra_services::runs::AtomicRunInteractionBatchRegistrationRequest<'_>,
    ) -> Result<astra_services::runs::AtomicRunInteractionBatchRegistration, String> {
        self.store.register_guarded_interaction_batch(request).await
    }

    /// Recover active runs after a crash/restart.
    ///
    /// A durable status is not proof that an execution task survived. The
    /// current shutdown checkpoint records detection metadata, not enough
    /// state to reconstruct an agent loop safely across processes.
    ///
    /// - a durable user cancellation marker always wins before generic crash
    ///   classification; runtime-owned cancellation has no recovery half-state
    ///   because its terminal transition is one atomic owner-fenced commit;
    /// - orphaned `waiting` / blocking `paused` runs are moved to non-blocking
    ///   `paused` and direct the caller to continue the session with a new run;
    /// - `running` runs with a graceful checkpoint use the same honest
    ///   session-continuation fallback;
    /// - other `running` runs are marked failed because their
    ///   in-flight effects cannot be proven replay-safe.
    pub async fn recover_active_runs(&self) -> Result<Vec<DurableRunRecord>, String> {
        self.recover_active_runs_with_claim_policy(false).await
    }

    /// Periodic recovery is not process startup. It may only take rows whose
    /// durable owner lease is absent or expired; sharing a pod id with a live
    /// executor is not a crash signal.
    async fn recover_expired_active_runs(&self) -> Result<Vec<DurableRunRecord>, String> {
        self.recover_active_runs_with_claim_policy(true).await
    }

    async fn recover_active_runs_with_claim_policy(
        &self,
        expired_only: bool,
    ) -> Result<Vec<DurableRunRecord>, String> {
        use futures_util::{StreamExt, stream};

        let mut recovered = Vec::new();
        let mut claimed_run_ids = HashSet::new();
        loop {
            let claim = if expired_only {
                self.store
                    .claim_expired_recoverable_active_runs(RUN_RECOVERY_CLAIM_BATCH_SIZE)
            } else {
                self.store
                    .claim_recoverable_active_runs(RUN_RECOVERY_CLAIM_BATCH_SIZE)
            };
            let claimed = match claim.await {
                Ok(claimed) => claimed,
                Err(error) => {
                    record_recovery_scan(self.metrics_registry.as_ref(), "error");
                    if recovered.is_empty() {
                        return Err(error);
                    }
                    tracing::warn!(
                        target: "astra_runtime::run_engine",
                        recovered_total = recovered.len(),
                        error = %error,
                        "recovery scan stopped after committing a partial set"
                    );
                    return Ok(recovered);
                }
            };
            if claimed.is_empty() {
                record_recovery_scan(self.metrics_registry.as_ref(), "ok");
                break;
            }
            let fresh_claims = claimed
                .into_iter()
                .filter(|run| claimed_run_ids.insert((run.user_id.clone(), run.run_id.clone())))
                .collect::<Vec<_>>();
            if fresh_claims.is_empty() {
                record_recovery_scan(self.metrics_registry.as_ref(), "ok");
                break;
            }
            let batch = stream::iter(fresh_claims.into_iter().map(|run| {
                let engine = self.clone();
                async move { engine.recover_active_run(run).await }
            }))
            .buffer_unordered(RUN_RECOVERY_MAX_CONCURRENCY)
            .filter_map(|run| async move { run })
            .collect::<Vec<_>>()
            .await;
            recovered.extend(batch);
            tokio::task::yield_now().await;
        }
        Ok(recovered)
    }

    async fn recover_active_run(&self, mut run: DurableRunRecord) -> Option<DurableRunRecord> {
        let expected_status = run.status.clone();
        let direct_cancellation_requested = match self
            .store
            .is_run_cancellation_requested(&run.user_id, &run.run_id)
            .await
        {
            Ok(requested) => requested,
            Err(error) => {
                record_recovery_run(
                    self.metrics_registry.as_ref(),
                    "cancellation_request_lookup",
                    "error",
                );
                tracing::warn!(
                    target: "astra_runtime::run_engine",
                    run_id = %run.run_id,
                    %error,
                    "could not determine whether an orphaned run has a durable cancellation request; leaving it active for retry"
                );
                return None;
            }
        };
        // Recovery must apply the same transitive cancellation authority as a
        // live executor. Otherwise an orphaned child can be recovered as
        // paused/failed after its parent was cancelled on another pod.
        let cancellation_controls_this_run = match self
            .check_control_status(&run.user_id, &run.run_id)
            .await
        {
            Ok(Some(RunControlStatus::Cancelled)) => true,
            Ok(_) => false,
            Err(error) => {
                record_recovery_run(
                    self.metrics_registry.as_ref(),
                    "ancestor_control_lookup",
                    "error",
                );
                tracing::warn!(
                    target: "astra_runtime::run_engine",
                    run_id = %run.run_id,
                    %error,
                    "could not determine ancestor cancellation during crash recovery; leaving run active for retry"
                );
                return None;
            }
        };
        if direct_cancellation_requested || cancellation_controls_this_run {
            let cancellation_origin = match self
                .cancellation_origin_in_lineage(&run.user_id, &run.run_id)
                .await
            {
                Ok(origin) => origin,
                Err(error) => {
                    record_recovery_run(
                        self.metrics_registry.as_ref(),
                        "cancellation_origin_lookup",
                        "error",
                    );
                    tracing::warn!(
                        target: "astra_runtime::run_engine",
                        run_id = %run.run_id,
                        %error,
                        "could not prove crash-recovery cancellation origin; leaving run active for retry"
                    );
                    return None;
                }
            };
            let event = crash_recovery_cancellation_event(&run.run_id, cancellation_origin);
            return match self
                .store
                .update_run_status_with_events_if_current(
                    &run.user_id,
                    &run.session_id,
                    &run.run_id,
                    &[expected_status.as_str()],
                    None,
                    STATUS_CANCELLED,
                    None,
                    None,
                    std::slice::from_ref(&event),
                )
                .await
            {
                Ok(true) => {
                    record_recovery_run(
                        self.metrics_registry.as_ref(),
                        "user_cancellation",
                        "committed",
                    );
                    run.status = STATUS_CANCELLED.to_string();
                    run.waiting_for = None;
                    run.last_event_idx += 1;
                    run.events.push(event);
                    Some(run)
                }
                Ok(false) => self.recovery_conflict_current_run(&run).await,
                Err(error) => {
                    record_recovery_run(
                        self.metrics_registry.as_ref(),
                        "user_cancellation",
                        "error",
                    );
                    tracing::warn!(
                        target: "astra_runtime::run_engine",
                        run_id = %run.run_id,
                        %error,
                        "failed to converge durable child cancellation during crash recovery"
                    );
                    None
                }
            };
        }
        let checkpoint_available = has_graceful_resume_checkpoint(self, &run).await;
        let continue_via_session =
            matches!(run.status.as_str(), STATUS_WAITING | STATUS_PAUSED) || checkpoint_available;
        if continue_via_session {
            let event = restart_session_continuation_event(&expected_status, checkpoint_available);
            match self
                .store
                .update_run_status_with_events_if_current(
                    &run.user_id,
                    &run.session_id,
                    &run.run_id,
                    &[expected_status.as_str()],
                    None,
                    STATUS_PAUSED,
                    None,
                    None,
                    std::slice::from_ref(&event),
                )
                .await
            {
                Ok(true) => {
                    record_recovery_run(
                        self.metrics_registry.as_ref(),
                        "session_continuation",
                        "committed",
                    );
                    run.status = STATUS_PAUSED.to_string();
                    run.waiting_for = None;
                    run.last_event_idx += 1;
                    run.events.push(event);
                    Some(run)
                }
                Ok(false) => self.recovery_conflict_current_run(&run).await,
                Err(e) => {
                    record_recovery_run(
                        self.metrics_registry.as_ref(),
                        "session_continuation",
                        "error",
                    );
                    tracing::warn!(
                        target: "astra_runtime::run_engine",
                        run_id = %run.run_id,
                        error = %e,
                        "failed to atomically release orphaned run for session continuation"
                    );
                    None
                }
            }
        } else {
            let events = crash_recovery_terminal_events();
            match self
                .store
                .update_run_status_with_events_if_current(
                    &run.user_id,
                    &run.session_id,
                    &run.run_id,
                    &[expected_status.as_str()],
                    None,
                    STATUS_FAILED,
                    None,
                    Some("recovered from crash"),
                    &events,
                )
                .await
            {
                Ok(true) => {
                    record_recovery_run(
                        self.metrics_registry.as_ref(),
                        "fail_crashed",
                        "committed",
                    );
                    run.status = STATUS_FAILED.to_string();
                    run.waiting_for = None;
                    run.error_message = Some("recovered from crash".to_string());
                    run.error_code = Some("crash_recovery".to_string());
                    run.last_event_idx += events.len() as i64;
                    run.events.extend(events);
                    Some(run)
                }
                Ok(false) => self.recovery_conflict_current_run(&run).await,
                Err(e) => {
                    record_recovery_run(self.metrics_registry.as_ref(), "fail_crashed", "error");
                    tracing::warn!(
                        target: "astra_runtime::run_engine",
                        run_id = %run.run_id,
                        error = %e,
                        "failed to atomically mark crashed run as failed during recovery"
                    );
                    None
                }
            }
        }
    }

    async fn recovery_conflict_current_run(
        &self,
        stale_run: &DurableRunRecord,
    ) -> Option<DurableRunRecord> {
        let current = match self.load_run(&stale_run.user_id, &stale_run.run_id).await {
            Ok(current) => current,
            Err(error) => {
                record_recovery_run(self.metrics_registry.as_ref(), "conflict_reload", "error");
                tracing::warn!(
                    target: "astra_runtime::run_engine",
                    run_id = %stale_run.run_id,
                    error = %error,
                    "recovery transition CAS missed and current run reload failed"
                );
                return None;
            }
        };
        let Some(current) = current else {
            record_recovery_run(self.metrics_registry.as_ref(), "conflict_reload", "missing");
            tracing::warn!(
                target: "astra_runtime::run_engine",
                run_id = %stale_run.run_id,
                "recovery transition CAS missed and current run is gone"
            );
            return None;
        };
        if current.status == STATUS_WAITING || current.status == STATUS_FAILED {
            record_recovery_run(
                self.metrics_registry.as_ref(),
                "conflict_current",
                "accepted",
            );
            Some(current)
        } else {
            record_recovery_run(
                self.metrics_registry.as_ref(),
                "conflict_current",
                "skipped",
            );
            tracing::warn!(
                target: "astra_runtime::run_engine",
                run_id = %stale_run.run_id,
                stale_status = %stale_run.status,
                current_status = %current.status,
                "recovery transition skipped because durable status changed"
            );
            None
        }
    }

    /// List runs for a user using seek pagination.
    pub async fn list_user_runs_cursor(
        &self,
        user_id: &str,
        limit: u32,
        cursor: Option<RunListCursor>,
    ) -> Result<DurableRunListPage, String> {
        self.store
            .list_user_runs_cursor(user_id, limit, cursor)
            .await
    }

    /// Return the authoritative bounded run working set for one session.
    pub async fn list_session_runs(
        &self,
        user_id: &str,
        session_id: &str,
        limit: u32,
    ) -> Result<astra_services::runs::DurableSessionRunPage, String> {
        self.store
            .list_session_runs(user_id, session_id, limit)
            .await
    }

    pub async fn list_active_session_runs_cursor(
        &self,
        user_id: &str,
        session_id: &str,
        limit: u32,
        cursor: Option<RunListCursor>,
    ) -> Result<astra_services::runs::DurableRunListPage, String> {
        self.store
            .list_active_session_runs_cursor(user_id, session_id, limit, cursor)
            .await
    }

    pub async fn load_session_agent_recovery(
        &self,
        user_id: &str,
        session_id: &str,
        limit: u32,
    ) -> Result<astra_services::runs::DurableSessionRunPage, String> {
        self.store
            .load_session_agent_recovery(user_id, session_id, limit)
            .await
    }

    pub async fn load_session_agent_recovery_after(
        &self,
        user_id: &str,
        session_id: &str,
        limit: u32,
        after_run_id: Option<&str>,
    ) -> Result<astra_services::runs::DurableSessionRunPage, String> {
        self.store
            .load_session_agent_recovery_after(user_id, session_id, limit, after_run_id)
            .await
    }

    /// Access the underlying store (for advanced queries).
    pub fn store(&self) -> &Arc<dyn RunStateStore> {
        &self.store
    }
}

use crate::turn::run_control::{
    ActionAdmissionRequest, ProviderBoundaryAuthorization, QueuedUserIntent, RunControlStatus,
    RunStatusProvider, UserIntentAdmissionAuthority, UserIntentApplyAck, UserIntentPoll,
    UserIntentPollIssue, UserIntentPollIssueKind, UserIntentProvider,
};

fn durable_event_index(event: &serde_json::Value, fallback: usize) -> usize {
    event
        .get("index")
        .and_then(serde_json::Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
        .unwrap_or(fallback)
}

fn user_intent_issue(
    event_index: usize,
    intent_id: Option<&str>,
    kind: UserIntentPollIssueKind,
) -> UserIntentPollIssue {
    UserIntentPollIssue {
        event_index,
        intent_id: intent_id.map(str::to_string),
        kind,
    }
}

fn parse_queued_user_intent(
    event_index: usize,
    event: &serde_json::Value,
) -> Result<QueuedUserIntent, UserIntentPollIssue> {
    let Some(data) = event.get("data") else {
        return Err(user_intent_issue(
            event_index,
            None,
            UserIntentPollIssueKind::MissingData,
        ));
    };
    let intent_id = data
        .get("intent_id")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            user_intent_issue(event_index, None, UserIntentPollIssueKind::MissingIntentId)
        })?;
    let delivery = data.get("delivery").cloned().ok_or_else(|| {
        user_intent_issue(
            event_index,
            Some(intent_id),
            UserIntentPollIssueKind::MissingDelivery,
        )
    })?;
    let delivery = serde_json::from_value(delivery).map_err(|_| {
        user_intent_issue(
            event_index,
            Some(intent_id),
            UserIntentPollIssueKind::InvalidDelivery,
        )
    })?;
    let input = data.get("input").cloned().ok_or_else(|| {
        user_intent_issue(
            event_index,
            Some(intent_id),
            UserIntentPollIssueKind::MissingInput,
        )
    })?;
    if crate::turn::run_control::user_intent_content(&input).is_none() {
        return Err(user_intent_issue(
            event_index,
            Some(intent_id),
            UserIntentPollIssueKind::NoActionableContent,
        ));
    }
    Ok(QueuedUserIntent {
        intent_id: intent_id.to_string(),
        delivery,
        status: astra_turn_types::UserIntentStatus::AcceptedRemote,
        event_index,
        input,
    })
}

fn parse_applied_user_intent(
    event: &serde_json::Value,
) -> Result<QueuedUserIntent, UserIntentPollIssue> {
    let durable_index = event
        .get("index")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(0);
    let Some(data) = event.get("data") else {
        return Err(user_intent_issue(
            durable_index,
            None,
            UserIntentPollIssueKind::MissingData,
        ));
    };
    let source_index = data
        .get("event_index")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| {
            user_intent_issue(durable_index, None, UserIntentPollIssueKind::MissingData)
        })?;
    let intent_id = data
        .get("intent_id")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            user_intent_issue(source_index, None, UserIntentPollIssueKind::MissingIntentId)
        })?;
    let delivery = data.get("delivery").cloned().ok_or_else(|| {
        user_intent_issue(
            source_index,
            Some(intent_id),
            UserIntentPollIssueKind::MissingDelivery,
        )
    })?;
    let delivery = serde_json::from_value(delivery).map_err(|_| {
        user_intent_issue(
            source_index,
            Some(intent_id),
            UserIntentPollIssueKind::InvalidDelivery,
        )
    })?;
    let input = data
        .get("input")
        .cloned()
        .or_else(|| {
            data.get("content")
                .cloned()
                .map(|content| serde_json::json!({"content": content}))
        })
        .ok_or_else(|| {
            user_intent_issue(
                source_index,
                Some(intent_id),
                UserIntentPollIssueKind::MissingInput,
            )
        })?;
    Ok(QueuedUserIntent {
        intent_id: intent_id.to_string(),
        delivery,
        status: astra_turn_types::UserIntentStatus::Applied,
        event_index: source_index,
        input,
    })
}

fn user_intent_indices_with_disposition(
    events: &[serde_json::Value],
    event_type: &str,
) -> std::collections::HashSet<usize> {
    events
        .iter()
        .filter(|event| {
            event.get("event_type").and_then(serde_json::Value::as_str) == Some(event_type)
        })
        .filter_map(|event| {
            event
                .get("data")
                .and_then(|data| data.get("event_index"))
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
        })
        .collect()
}

impl RunEngine {
    async fn transition_user_intent_admission(
        &self,
        user_id: &str,
        expected_session_id: &str,
        run_id: &str,
        expected_owner_generation: u64,
        transition: RunUserIntentAdmissionTransition,
    ) -> Result<(), String> {
        match self
            .store
            .transition_user_intent_admission(AtomicRunUserIntentAdmissionTransitionRequest {
                user_id,
                run_id,
                expected_session_id,
                expected_owner_generation,
                transition,
            })
            .await?
        {
            AtomicRunUserIntentAdmissionTransition::Changed { .. } => Ok(()),
            AtomicRunUserIntentAdmissionTransition::DurableFactRecovered { .. }
                if transition == RunUserIntentAdmissionTransition::Fence =>
            {
                Ok(())
            }
            AtomicRunUserIntentAdmissionTransition::LiveReopenAuthorized { .. }
                if transition == RunUserIntentAdmissionTransition::Reopen =>
            {
                Ok(())
            }
            AtomicRunUserIntentAdmissionTransition::AlreadyInState { .. }
                if transition == RunUserIntentAdmissionTransition::Fence =>
            {
                Ok(())
            }
            AtomicRunUserIntentAdmissionTransition::DurableFactRecovered { .. }
            | AtomicRunUserIntentAdmissionTransition::LiveReopenAuthorized { .. }
            | AtomicRunUserIntentAdmissionTransition::AlreadyInState { .. } => Err(format!(
                "user-intent admission transition returned authority for the wrong operation on run {run_id}"
            )),
            AtomicRunUserIntentAdmissionTransition::Inactive { status } => Err(format!(
                "run became {status} before user-intent admission transition: {run_id}"
            )),
            AtomicRunUserIntentAdmissionTransition::OwnerGenerationMismatch {
                actual_owner_generation,
            } => Err(format!(
                "stale execution generation {expected_owner_generation} cannot change user-intent admission for run {run_id}; current generation is {actual_owner_generation}"
            )),
            AtomicRunUserIntentAdmissionTransition::OwnerMismatch {
                actual_owner_pod_id,
            } => Err(format!(
                "this executor no longer owns user-intent admission for run {run_id}; current owner is {}",
                actual_owner_pod_id.as_deref().unwrap_or("none")
            )),
            AtomicRunUserIntentAdmissionTransition::OwnerLeaseExpired => Err(format!(
                "execution owner lease expired before user-intent admission transition for run {run_id}"
            )),
            AtomicRunUserIntentAdmissionTransition::IdentityConflict => Err(format!(
                "user-intent admission transition identity conflict for run {run_id}"
            )),
            AtomicRunUserIntentAdmissionTransition::Missing => Err(format!(
                "run not found while changing user-intent admission: {run_id}"
            )),
        }
    }
}

#[async_trait::async_trait]
impl RunStatusProvider for RunEngine {
    #[allow(clippy::blocks_in_conditions)]
    async fn control_status(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Result<Option<RunControlStatus>, String> {
        self.check_control_status(user_id, run_id).await
    }

    async fn cancellation_origin(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Result<astra_turn_core::orchestration_types::CancellationOrigin, String> {
        self.cancellation_origin_in_lineage(user_id, run_id).await
    }
}

#[async_trait::async_trait]
impl UserIntentProvider for RunEngine {
    async fn begin_action(
        &self,
        user_id: &str,
        run_id: &str,
        request: ActionAdmissionRequest,
    ) -> Result<astra_services::runs::AtomicRunActionAdmission, String> {
        if self.uses_transactional_invocation_admission() {
            return Err(
                "database run actions must be admitted inside the invocation dispatch transaction"
                    .to_string(),
            );
        }
        let expected_owner_generation = request.expected_owner_generation.ok_or_else(|| {
            "process-local run action admission requires exact owner generation".to_string()
        })?;
        self.store
            .begin_action(AtomicRunActionAdmissionRequest {
                user_id,
                run_id,
                expected_session_id: &request.expected_session_id,
                action_id: &request.action_id,
                expected_control_epoch: request.expected_control_epoch,
                expected_owner_generation,
            })
            .await
    }

    async fn authorize_provider_boundary(
        &self,
        user_id: &str,
        expected_session_id: &str,
        run_id: &str,
        authority: UserIntentAdmissionAuthority,
    ) -> Result<ProviderBoundaryAuthorization, String> {
        let UserIntentAdmissionAuthority::DurableOwnerGeneration(expected_owner_generation) =
            authority
        else {
            return Err(
                "durable provider-boundary authorization requires exact owner generation"
                    .to_string(),
            );
        };
        let max_wait = self
            .store
            .owner_lease_duration()
            .unwrap_or(OWNER_LEASE_ACTIVATION_MAX_WAIT)
            .min(OWNER_LEASE_ACTIVATION_MAX_WAIT)
            .max(Duration::from_millis(1));
        let outcome = tokio::time::timeout(
            max_wait,
            self.store
                .authorize_execution_boundary(RunExecutionBoundaryAuthorizationRequest {
                    user_id,
                    run_id,
                    expected_session_id,
                    expected_owner_generation,
                }),
        )
        .await
        .map_err(|_| {
            format!(
                "provider-boundary execution authorization timed out after {}ms for run {run_id}",
                max_wait.as_millis()
            )
        })??;
        Ok(match outcome {
            RunExecutionBoundaryAuthorization::Authorized => {
                ProviderBoundaryAuthorization::Authorized
            }
            RunExecutionBoundaryAuthorization::Inactive { status } if status == STATUS_PAUSED => {
                ProviderBoundaryAuthorization::Paused
            }
            RunExecutionBoundaryAuthorization::Inactive { status } => {
                ProviderBoundaryAuthorization::Inactive { status }
            }
            RunExecutionBoundaryAuthorization::OwnerGenerationMismatch {
                actual_owner_generation,
            } => ProviderBoundaryAuthorization::AuthorityLost {
                reason: format!(
                    "owner generation changed from {expected_owner_generation} to {actual_owner_generation}"
                ),
            },
            RunExecutionBoundaryAuthorization::OwnerMismatch {
                actual_owner_pod_id,
            } => ProviderBoundaryAuthorization::AuthorityLost {
                reason: format!(
                    "owner pod changed to {}",
                    actual_owner_pod_id.as_deref().unwrap_or("none")
                ),
            },
            RunExecutionBoundaryAuthorization::OwnerLeaseExpired => {
                ProviderBoundaryAuthorization::AuthorityLost {
                    reason: "owner lease expired".to_string(),
                }
            }
            RunExecutionBoundaryAuthorization::Missing => {
                ProviderBoundaryAuthorization::AuthorityLost {
                    reason: "run no longer exists".to_string(),
                }
            }
        })
    }

    async fn fence_user_intent_submissions(
        &self,
        user_id: &str,
        expected_session_id: &str,
        run_id: &str,
        authority: UserIntentAdmissionAuthority,
    ) -> Result<(), String> {
        let UserIntentAdmissionAuthority::DurableOwnerGeneration(expected_owner_generation) =
            authority
        else {
            return Err(
                "durable user-intent admission fencing requires exact owner generation".to_string(),
            );
        };
        self.transition_user_intent_admission(
            user_id,
            expected_session_id,
            run_id,
            expected_owner_generation,
            RunUserIntentAdmissionTransition::Fence,
        )
        .await
    }

    async fn reopen_user_intent_submissions(
        &self,
        user_id: &str,
        expected_session_id: &str,
        run_id: &str,
        authority: UserIntentAdmissionAuthority,
    ) -> Result<(), String> {
        let UserIntentAdmissionAuthority::DurableOwnerGeneration(expected_owner_generation) =
            authority
        else {
            return Err(
                "durable user-intent admission reopening requires exact owner generation"
                    .to_string(),
            );
        };
        self.transition_user_intent_admission(
            user_id,
            expected_session_id,
            run_id,
            expected_owner_generation,
            RunUserIntentAdmissionTransition::Reopen,
        )
        .await
    }

    async fn poll_user_intents(
        &self,
        user_id: &str,
        run_id: &str,
        after_event_index: usize,
    ) -> UserIntentPoll {
        let after_event_idx = i64::try_from(after_event_index).unwrap_or(i64::MAX);
        let delta = match self
            .store
            .load_user_intent_control_delta(
                user_id,
                run_id,
                after_event_idx,
                USER_INTENT_CONTROL_DELTA_PAGE_SIZE,
            )
            .await
        {
            Ok(Some(delta)) => delta,
            Ok(None) => {
                record_control_poll_attempt(
                    self.metrics_registry.as_ref(),
                    "user_intent_poll",
                    "missing",
                );
                record_control_poll_error(
                    self.metrics_registry.as_ref(),
                    "user_intent_poll",
                    "missing",
                );
                let error = format!("run not found while polling user intents: {run_id}");
                return UserIntentPoll {
                    next_cursor: after_event_index,
                    snapshot_has_more: false,
                    snapshot_page_fact_count: 0,
                    inputs: Vec::new(),
                    issues: Vec::new(),
                    error: Some(error),
                };
            }
            Err(error) => {
                tracing::warn!(
                    run_id,
                    error = %error,
                    "failed to poll accepted user intents from run store"
                );
                record_control_poll_attempt(
                    self.metrics_registry.as_ref(),
                    "user_intent_poll",
                    "error",
                );
                record_control_poll_error(
                    self.metrics_registry.as_ref(),
                    "user_intent_poll",
                    "store",
                );
                return UserIntentPoll {
                    next_cursor: after_event_index,
                    snapshot_has_more: false,
                    snapshot_page_fact_count: 0,
                    inputs: Vec::new(),
                    issues: Vec::new(),
                    error: Some(error),
                };
            }
        };
        let snapshot_has_more = delta.has_more;
        let snapshot_page_fact_count = delta.events.len();
        let mut settled_indices = delta
            .settled_source_indices
            .iter()
            .filter_map(|index| usize::try_from(*index).ok())
            .collect::<std::collections::HashSet<_>>();
        settled_indices.extend(user_intent_indices_with_disposition(
            &delta.events,
            "user_intent_applied",
        ));
        settled_indices.extend(user_intent_indices_with_disposition(
            &delta.events,
            "user_intent_returned",
        ));
        let mut inputs = Vec::new();
        let mut issues = Vec::new();
        let page_cursor = usize::try_from(delta.page_cursor).unwrap_or(after_event_index);
        let authoritative_tail =
            usize::try_from(delta.authoritative_last_event_idx).unwrap_or(after_event_index);
        let next_cursor = if delta.has_more {
            after_event_index.max(page_cursor)
        } else {
            after_event_index.max(authoritative_tail)
        };
        for (position, event) in delta.events.iter().enumerate() {
            let fallback = after_event_index.saturating_add(position).saturating_add(1);
            let event_index = durable_event_index(event, fallback);
            let parsed = match event.get("event_type").and_then(serde_json::Value::as_str) {
                Some("user_intent") if !settled_indices.contains(&event_index) => {
                    Some(parse_queued_user_intent(event_index, event))
                }
                Some("user_intent_applied") => Some(parse_applied_user_intent(event)),
                _ => None,
            };
            let Some(parsed) = parsed else { continue };
            match parsed {
                Ok(input) => inputs.push(input),
                Err(issue) => issues.push(issue),
            }
        }

        let outcome = if issues.is_empty() { "ok" } else { "partial" };
        record_control_poll_attempt(self.metrics_registry.as_ref(), "user_intent_poll", outcome);
        if !issues.is_empty() {
            record_control_poll_error(
                self.metrics_registry.as_ref(),
                "user_intent_poll",
                "invalid_event",
            );
        }

        UserIntentPoll {
            next_cursor,
            snapshot_has_more,
            snapshot_page_fact_count,
            inputs,
            issues,
            error: None,
        }
    }

    async fn mark_user_intents_applied(
        &self,
        user_id: &str,
        expected_session_id: &str,
        run_id: &str,
        event_indices: &[usize],
        authority: UserIntentAdmissionAuthority,
    ) -> Result<UserIntentApplyAck, String> {
        let UserIntentAdmissionAuthority::DurableOwnerGeneration(expected_owner_generation) =
            authority
        else {
            return Err("durable user-intent apply requires exact owner generation".to_string());
        };
        if event_indices.is_empty() {
            return Ok(UserIntentApplyAck::Applied);
        }
        let source_event_indices = event_indices
            .iter()
            .map(|event_index| {
                i64::try_from(*event_index).map_err(|_| {
                    format!("user intent event index {event_index} exceeds durable range")
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let outcome = self
            .store
            .apply_run_user_intents(AtomicRunUserIntentApplyRequest {
                user_id,
                run_id,
                expected_session_id,
                expected_owner_generation,
                source_event_indices: &source_event_indices,
            })
            .await?;
        match outcome {
            AtomicRunUserIntentApply::Applied { .. }
            | AtomicRunUserIntentApply::AckRecovered { .. } => {
                record_control_poll_attempt(
                    self.metrics_registry.as_ref(),
                    "user_intent_apply",
                    "applied",
                );
                Ok(UserIntentApplyAck::Applied)
            }
            AtomicRunUserIntentApply::AlreadyApplied { .. } => {
                record_control_poll_attempt(
                    self.metrics_registry.as_ref(),
                    "user_intent_apply",
                    "already_applied",
                );
                Ok(UserIntentApplyAck::Applied)
            }
            AtomicRunUserIntentApply::RunTerminalReturned => {
                record_control_poll_attempt(
                    self.metrics_registry.as_ref(),
                    "user_intent_apply",
                    "run_terminal",
                );
                Ok(UserIntentApplyAck::RunTerminalReturned)
            }
            AtomicRunUserIntentApply::SettlementFenced => Err(format!(
                "user-intent apply was fenced by settlement for run {run_id}"
            )),
            AtomicRunUserIntentApply::Inactive { status } => Err(format!(
                "cannot apply user intents while run {run_id} is {status}"
            )),
            AtomicRunUserIntentApply::OwnerGenerationMismatch {
                actual_owner_generation,
            } => Err(format!(
                "stale execution generation {expected_owner_generation} cannot apply user intents for run {run_id}; current generation is {actual_owner_generation}"
            )),
            AtomicRunUserIntentApply::OwnerMismatch {
                actual_owner_pod_id,
            } => Err(format!(
                "this executor no longer owns user-intent apply for run {run_id}; current owner is {}",
                actual_owner_pod_id.as_deref().unwrap_or("none")
            )),
            AtomicRunUserIntentApply::OwnerLeaseExpired => Err(format!(
                "execution owner lease expired before user-intent apply for run {run_id}"
            )),
            AtomicRunUserIntentApply::SourceMissing { event_index } => Err(format!(
                "cannot apply user intent for unknown event index {event_index}"
            )),
            AtomicRunUserIntentApply::IdentityConflict => Err(format!(
                "user-intent apply identity conflict for run {run_id}"
            )),
            AtomicRunUserIntentApply::Missing => Err(format!(
                "run not found while applying user intents: {run_id}"
            )),
        }
    }
}

async fn has_graceful_resume_checkpoint(engine: &RunEngine, run: &DurableRunRecord) -> bool {
    // `save_checkpoint` updates the agent_runs snapshot in the same transaction,
    // so the recovery scan already carries the common-case checkpoint. Avoid
    // one extra query per active run; fall back to the typed history table only
    // for legacy/incomplete rows.
    if checkpoint_is_graceful_resume(
        run.checkpoint_version.as_deref().unwrap_or_default(),
        run.checkpoint_json.as_deref().unwrap_or_default(),
    ) {
        return true;
    }
    if let Ok(Some(checkpoint)) = engine
        .load_latest_checkpoint(&run.user_id, &run.run_id, Some("resume"))
        .await
    {
        return checkpoint_is_graceful_resume(
            &checkpoint.checkpoint_version,
            &checkpoint.checkpoint_json,
        );
    }
    false
}

fn checkpoint_is_graceful_resume(checkpoint_version: &str, checkpoint_json: &str) -> bool {
    if checkpoint_version != "checkpoint_v1" || checkpoint_json.is_empty() {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(checkpoint_json)
        .ok()
        .and_then(|value| value.get("graceful").and_then(serde_json::Value::as_bool))
        .unwrap_or(false)
}

// ─── Tests ──────────────────────────────────────────────────────────────────

// ─── Trait Implementation ─────────────────────────────────────────────────────────

#[async_trait::async_trait]
impl astra_server_types::team_orchestrator_traits::RunPersistence for RunEngine {
    async fn start_run_ext(
        &self,
        run_id: &str,
        user_id: &str,
        session_id: &str,
        parent_run_id: Option<&str>,
        delegation_id: Option<&str>,
        agent_id: Option<&str>,
        retry_of: Option<&str>,
    ) -> Result<(), String> {
        RunEngine::start_run_ext(
            self,
            run_id,
            user_id,
            session_id,
            parent_run_id,
            delegation_id,
            agent_id,
            retry_of,
        )
        .await
        .map(|_| ())
    }

    async fn persist_status_if_current(
        &self,
        request: RunStatusCasRequest<'_>,
    ) -> Result<bool, String> {
        RunEngine::persist_status_if_current(self, request).await
    }

    async fn persist_usage(
        &self,
        user_id: &str,
        expected_session_id: &str,
        run_id: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
        tool_calls: u32,
    ) -> Result<bool, String> {
        RunEngine::persist_usage(
            self,
            user_id,
            expected_session_id,
            run_id,
            prompt_tokens,
            completion_tokens,
            tool_calls,
        )
        .await
    }

    async fn persist_checkpoint(
        &self,
        user_id: &str,
        expected_session_id: &str,
        run_id: &str,
        checkpoint_json: &str,
    ) -> Result<bool, String> {
        RunEngine::persist_checkpoint(self, user_id, expected_session_id, run_id, checkpoint_json)
            .await
    }

    async fn append_event(
        &self,
        user_id: &str,
        expected_session_id: &str,
        run_id: &str,
        event: serde_json::Value,
    ) -> Result<(), String> {
        RunEngine::append_event(self, user_id, expected_session_id, run_id, event).await
    }
}

/// Continuously owns orphan classification after the startup pass. A
/// transient cancellation-intent lookup failure deliberately leaves the run
/// active; this leased sweeper is the corresponding retry owner, so
/// fail-closed never degrades into a permanently abandoned row.
pub(crate) fn spawn_active_run_recovery_sweeper(
    pool: astra_core::SharedPool,
    lease: Arc<crate::server::sweeper_lease::SweeperLease>,
    cancel: tokio_util::sync::CancellationToken,
    owner_pod_id: Option<String>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let store = astra_services::runs::DatabaseRunStateStore::new(pool);
        let store = match owner_pod_id.as_deref() {
            Some(owner) => store.with_owner_pod_id(owner),
            None => store,
        };
        let engine = RunEngine::new(Arc::new(store));
        let mut interval = tokio::time::interval(RUN_RECOVERY_SWEEP_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {
                    match lease.check_leader().await {
                        crate::server::sweeper_lease::LeaderStatus::Leader => {}
                        crate::server::sweeper_lease::LeaderStatus::NotLeader => continue,
                        crate::server::sweeper_lease::LeaderStatus::Unavailable(error) => {
                            tracing::warn!(
                                target: "astra_runtime::run_recovery_sweeper",
                                %error,
                                "run recovery leadership unavailable; retrying later"
                            );
                            continue;
                        }
                    }
                    match engine.recover_expired_active_runs().await {
                        Ok(recovered) if recovered.is_empty() => {}
                        Ok(recovered) => tracing::info!(
                            target: "astra_runtime::run_recovery_sweeper",
                            recovered = recovered.len(),
                            "reconciled orphaned durable runs"
                        ),
                        Err(error) => tracing::warn!(
                            target: "astra_runtime::run_recovery_sweeper",
                            %error,
                            "run recovery sweep failed; retrying later"
                        ),
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::runs::{DurableRunRecord, InMemoryRunStateStore, RunStateStore};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn test_engine() -> RunEngine {
        RunEngine::new(Arc::new(InMemoryRunStateStore::new()))
    }

    async fn transition_typed_cancellation(
        engine: &RunEngine,
        user_id: &str,
        session_id: &str,
        run_id: &str,
        expected_statuses: &[&str],
        origin: astra_turn_core::orchestration_types::CancellationOrigin,
    ) -> bool {
        if origin == astra_turn_core::orchestration_types::CancellationOrigin::User {
            assert!(
                engine
                    .request_run_cancellation(user_id, run_id)
                    .await
                    .expect("persist explicit User cancellation marker")
            );
        }
        engine
            .transition_status_with_events_if_current(
                user_id,
                session_id,
                run_id,
                expected_statuses,
                STATUS_CANCELLED,
                None,
                None,
                &[crash_recovery_cancellation_event(run_id, origin)],
            )
            .await
            .expect("persist typed cancellation terminal")
    }

    fn work_binding(work_id: &str, branch_id: &str, graph_revision: i64) -> DurableWorkRunBinding {
        DurableWorkRunBinding::new(
            astra_services::work::WorkId::parse(work_id).expect("work"),
            astra_services::work::WorkBranchId::parse(branch_id).expect("branch"),
            astra_services::work::GraphRevision::new(graph_revision).expect("graph revision"),
        )
    }

    #[tokio::test]
    async fn process_local_run_control_admits_action_on_its_shared_store() {
        let engine = test_engine();
        engine
            .start_run("action-admission-wrapper", "user-1", "session-1")
            .await
            .unwrap();
        let events_before = engine
            .load_run("user-1", "action-admission-wrapper")
            .await
            .unwrap()
            .unwrap()
            .events
            .len();
        let request = crate::turn::run_control::ActionAdmissionRequest {
            action_id: "tool-batch-1".to_string(),
            expected_session_id: "session-1".to_string(),
            expected_control_epoch: -1,
            expected_owner_generation: Some(0),
        };

        let outcome = UserIntentProvider::begin_action(
            &engine,
            "user-1",
            "action-admission-wrapper",
            request,
        )
        .await
        .expect("process-local actions use the lifecycle's in-memory store fence");
        assert!(matches!(
            outcome,
            astra_services::runs::AtomicRunActionAdmission::Started { .. }
        ));
        let run = engine
            .load_run("user-1", "action-admission-wrapper")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.events.len(), events_before + 1);
        assert!(run.events.iter().any(|event| {
            event.get("event_type").and_then(serde_json::Value::as_str)
                == Some("action_admission_granted")
        }));
    }

    #[tokio::test]
    async fn delegation_completion_cannot_overwrite_pause_or_cancel() {
        for (run_id, winning_status) in [
            ("delegation-pause-wins", STATUS_PAUSED),
            ("delegation-cancel-wins", STATUS_CANCELLED),
        ] {
            let engine = test_engine();
            engine
                .start_run(run_id, "user-1", "session-1")
                .await
                .unwrap();
            if winning_status == STATUS_CANCELLED {
                assert!(
                    transition_typed_cancellation(
                        &engine,
                        "user-1",
                        "session-1",
                        run_id,
                        &[STATUS_RUNNING],
                        astra_turn_core::orchestration_types::CancellationOrigin::Runtime,
                    )
                    .await
                );
            } else {
                engine
                    .persist_status(
                        "user-1",
                        "session-1",
                        run_id,
                        winning_status,
                        Some("control"),
                        None,
                    )
                    .await
                    .unwrap();
            }

            let committed = engine
                .persist_delegation_outcome_status(
                    "user-1",
                    "session-1",
                    run_id,
                    STATUS_COMPLETED,
                    None,
                    None,
                )
                .await
                .unwrap();

            assert!(!committed, "the stale child result must lose its CAS");
            let durable = engine.load_run("user-1", run_id).await.unwrap().unwrap();
            assert_eq!(durable.status, winning_status);
            assert!(!durable.events.iter().any(|event| {
                event.get("event_type").and_then(serde_json::Value::as_str) == Some("run_finished")
                    && event["data"]["status"] == STATUS_COMPLETED
            }));
        }
    }

    #[tokio::test]
    async fn delegation_verification_failure_uses_canonical_failed_status() {
        let engine = test_engine();
        engine
            .start_run("delegation-verification", "user-1", "session-1")
            .await
            .unwrap();

        assert!(
            engine
                .persist_delegation_outcome_status(
                    "user-1",
                    "session-1",
                    "delegation-verification",
                    "verification_failed",
                    None,
                    Some("evidence did not satisfy the gate"),
                )
                .await
                .unwrap()
        );
        let durable = engine
            .load_run("user-1", "delegation-verification")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(durable.status, STATUS_FAILED);
        assert!(durable.events.iter().any(|event| {
            event.get("event_type").and_then(serde_json::Value::as_str) == Some("run_finished")
                && event["data"]["status"] == STATUS_FAILED
        }));
    }

    #[tokio::test]
    async fn repeated_delegation_terminal_outcome_does_not_append_duplicate_events() {
        let engine = test_engine();
        let run_id = "delegation-terminal-replay";
        engine
            .start_run(run_id, "user-1", "session-1")
            .await
            .unwrap();

        assert!(
            engine
                .persist_delegation_outcome_status(
                    "user-1",
                    "session-1",
                    run_id,
                    STATUS_FAILED,
                    None,
                    Some("worker failed"),
                )
                .await
                .unwrap()
        );
        let first = engine.load_run("user-1", run_id).await.unwrap().unwrap();

        assert!(
            !engine
                .persist_delegation_outcome_status(
                    "user-1",
                    "session-1",
                    run_id,
                    STATUS_FAILED,
                    None,
                    Some("worker failed"),
                )
                .await
                .unwrap(),
            "a replay must observe the existing terminal authority, not recommit it"
        );
        let replayed = engine.load_run("user-1", run_id).await.unwrap().unwrap();

        assert_eq!(replayed.events, first.events);
        assert_eq!(
            replayed
                .events
                .iter()
                .filter(|event| event["event_type"] == "run_finished")
                .count(),
            1
        );
        assert_eq!(
            replayed
                .events
                .iter()
                .filter(|event| event["event_type"] == "run_error")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn owner_lease_heartbeat_is_disabled_when_store_has_no_interval() {
        let lease_lost = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(tokio_util::sync::CancellationToken::new());
        assert!(
            test_engine()
                .start_owner_lease_heartbeat(
                    "user-1".to_string(),
                    "session-1".to_string(),
                    "run-1".to_string(),
                    0,
                    lease_lost,
                    cancel,
                )
                .is_none(),
            "stores without shared lease state should not spawn a heartbeat task"
        );
    }

    #[tokio::test]
    async fn owner_lease_heartbeat_renews_and_releases_when_guard_drops() {
        let store = Arc::new(
            FlakyBatchTransitionStore::new(0, BatchTransitionFailureMode::FailBeforeStoreWrite)
                .with_owner_lease_heartbeat(Duration::from_millis(5)),
        );
        let engine = RunEngine::new(store.clone());
        let lease_lost = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(tokio_util::sync::CancellationToken::new());
        let guard = engine
            .start_owner_lease_heartbeat(
                "user-1".to_string(),
                "session-1".to_string(),
                "run-1".to_string(),
                0,
                lease_lost.clone(),
                cancel.clone(),
            )
            .expect("heartbeat-enabled store should start a guard");

        let deadline = tokio::time::Instant::now() + Duration::from_millis(100);
        while store.lease_renewals() < 2 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            store.lease_renewals() >= 2,
            "heartbeat should renew the active run lease while the guard is alive"
        );

        drop(guard);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(100);
        while store.lease_releases() == 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            store.lease_releases(),
            1,
            "dropping the heartbeat guard must release durable ownership"
        );
        let renewals_after_release = store.lease_renewals();
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(
            store.lease_renewals(),
            renewals_after_release,
            "released ownership must not continue renewing"
        );
        assert!(!lease_lost.load(Ordering::Acquire));
        assert!(!cancel.is_cancelled());
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_owner_lease_release_is_bounded_and_dropped() {
        let store = Arc::new(
            FlakyBatchTransitionStore::new(0, BatchTransitionFailureMode::FailBeforeStoreWrite)
                .with_owner_lease_heartbeat(Duration::from_secs(30))
                .with_lease_release_behavior(LeaseReleaseBehavior::Pending),
        );
        let lease_lost = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(tokio_util::sync::CancellationToken::new());
        let guard = RunEngine::new(store.clone())
            .start_owner_lease_heartbeat(
                "user-1".to_string(),
                "session-1".to_string(),
                "run-release-pending".to_string(),
                0,
                lease_lost.clone(),
                cancel.clone(),
            )
            .expect("heartbeat-enabled store");

        drop(guard);
        for _ in 0..10 {
            tokio::task::yield_now().await;
            if store.active_lease_releases() == 1 {
                break;
            }
        }
        assert_eq!(store.lease_releases(), 1);
        assert_eq!(store.active_lease_releases(), 1);

        tokio::time::advance(OWNER_LEASE_RELEASE_MAX_WAIT + Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            store.active_lease_releases(),
            0,
            "the timeout must drop a stalled database release future"
        );
        assert!(!lease_lost.load(Ordering::Acquire));
        assert!(!cancel.is_cancelled());
    }

    #[tokio::test]
    async fn refused_owner_lease_renewal_fences_local_execution_without_releasing_winner() {
        let store = Arc::new(
            FlakyBatchTransitionStore::new(0, BatchTransitionFailureMode::FailBeforeStoreWrite)
                .with_owner_lease_heartbeat(Duration::from_millis(5))
                .with_lease_renewal_behavior(LeaseRenewalBehavior::Refuse),
        );
        let lease_lost = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(tokio_util::sync::CancellationToken::new());
        let guard = RunEngine::new(store.clone())
            .start_owner_lease_heartbeat(
                "user-1".to_string(),
                "session-1".to_string(),
                "run-1".to_string(),
                7,
                lease_lost.clone(),
                cancel.clone(),
            )
            .expect("heartbeat-enabled store");

        tokio::time::timeout(Duration::from_millis(100), cancel.cancelled())
            .await
            .expect("lease refusal must wake provider I/O promptly");
        assert!(lease_lost.load(Ordering::Acquire));
        assert_eq!(store.lease_releases(), 0);
        drop(guard);
    }

    #[tokio::test(start_paused = true)]
    async fn hung_owner_lease_renewal_is_fenced_before_durable_ttl() {
        let store = Arc::new(
            FlakyBatchTransitionStore::new(0, BatchTransitionFailureMode::FailBeforeStoreWrite)
                .with_owner_lease_heartbeat(Duration::from_millis(10))
                .with_lease_renewal_behavior(LeaseRenewalBehavior::Pending),
        );
        let lease_lost = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(tokio_util::sync::CancellationToken::new());
        let guard = RunEngine::new(store.clone())
            .start_owner_lease_heartbeat(
                "user-1".to_string(),
                "session-1".to_string(),
                "run-1".to_string(),
                3,
                lease_lost.clone(),
                cancel.clone(),
            )
            .expect("heartbeat-enabled store");

        tokio::task::yield_now().await;
        assert_eq!(store.lease_renewals(), 1);
        tokio::time::advance(Duration::from_millis(21)).await;
        tokio::task::yield_now().await;
        assert!(lease_lost.load(Ordering::Acquire));
        assert!(cancel.is_cancelled());
        assert_eq!(store.lease_releases(), 0);
        drop(guard);
    }

    #[tokio::test(start_paused = true)]
    async fn hung_execution_activation_is_bounded_and_left_for_durable_recovery() {
        let store = Arc::new(
            FlakyBatchTransitionStore::new(0, BatchTransitionFailureMode::FailBeforeStoreWrite)
                .with_owner_lease_heartbeat(Duration::from_millis(10))
                .with_lease_renewal_behavior(LeaseRenewalBehavior::Pending),
        );
        let engine = RunEngine::new(store.clone());
        let authority = engine
            .start_run("activation-pending", "user-1", "session-1")
            .await
            .expect("durable admission");
        let cancel = tokio_util::sync::CancellationToken::new();
        let task = tokio::spawn({
            let engine = engine.clone();
            let cancel = cancel.clone();
            async move {
                engine
                    .confirm_execution_authority(
                        "user-1",
                        "session-1",
                        "activation-pending",
                        authority.owner_generation,
                        &cancel,
                    )
                    .await
            }
        });

        tokio::task::yield_now().await;
        assert_eq!(store.lease_renewals(), 1);
        tokio::time::advance(Duration::from_millis(10)).await;
        let error = task
            .await
            .expect("activation task")
            .expect_err("pending renewal must time out");
        assert!(error.contains("activation timed out"));
        let durable = engine
            .load_run("user-1", "activation-pending")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(durable.status, STATUS_RUNNING);
        assert_eq!(durable.run_generation, authority.owner_generation);
    }

    #[tokio::test]
    async fn execution_activation_observes_process_local_cancellation() {
        let store = Arc::new(
            FlakyBatchTransitionStore::new(0, BatchTransitionFailureMode::FailBeforeStoreWrite)
                .with_owner_lease_heartbeat(Duration::from_secs(30))
                .with_lease_renewal_behavior(LeaseRenewalBehavior::Pending),
        );
        let engine = RunEngine::new(store.clone());
        let authority = engine
            .start_run("activation-cancelled", "user-1", "session-1")
            .await
            .expect("durable admission");
        let cancel = tokio_util::sync::CancellationToken::new();
        let task = tokio::spawn({
            let engine = engine.clone();
            let cancel = cancel.clone();
            async move {
                engine
                    .confirm_execution_authority(
                        "user-1",
                        "session-1",
                        "activation-cancelled",
                        authority.owner_generation,
                        &cancel,
                    )
                    .await
            }
        });
        let deadline = tokio::time::Instant::now() + Duration::from_millis(100);
        while store.lease_renewals() == 0 && tokio::time::Instant::now() < deadline {
            tokio::task::yield_now().await;
        }
        cancel.cancel();

        let error = tokio::time::timeout(Duration::from_millis(100), task)
            .await
            .expect("cancellation must bound activation")
            .expect("activation task")
            .expect_err("cancelled activation must not authorize execution");
        assert!(error.contains("activation was cancelled"));
        let durable = engine
            .load_run("user-1", "activation-cancelled")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(durable.status, STATUS_RUNNING);
    }

    #[tokio::test(start_paused = true)]
    async fn delayed_renewal_ack_does_not_extend_the_local_execution_fence() {
        let store = Arc::new(
            FlakyBatchTransitionStore::new(0, BatchTransitionFailureMode::FailBeforeStoreWrite)
                .with_owner_lease_heartbeat(Duration::from_millis(10))
                .with_lease_renewal_behavior(LeaseRenewalBehavior::Delayed(Duration::from_millis(
                    19,
                ))),
        );
        let lease_lost = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(tokio_util::sync::CancellationToken::new());
        let guard = RunEngine::new(store.clone())
            .start_owner_lease_heartbeat(
                "user-1".to_string(),
                "session-1".to_string(),
                "run-1".to_string(),
                3,
                lease_lost.clone(),
                cancel.clone(),
            )
            .expect("heartbeat-enabled store");

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(19)).await;
        tokio::task::yield_now().await;
        assert_eq!(store.lease_renewals(), 1);
        assert!(!cancel.is_cancelled());

        tokio::time::advance(Duration::from_millis(2)).await;
        tokio::task::yield_now().await;
        assert!(lease_lost.load(Ordering::Acquire));
        assert!(cancel.is_cancelled());
        assert_eq!(store.lease_releases(), 0);
        drop(guard);
    }

    #[tokio::test]
    async fn superseded_execution_generation_cannot_commit_terminal_state() {
        let store = Arc::new(InMemoryRunStateStore::new());
        let engine = RunEngine::new(store.clone());
        let authority = engine
            .start_run("run-generation-fence", "user-1", "session-1")
            .await
            .expect("start durable run");
        assert_eq!(authority.owner_generation, 0);
        assert!(
            astra_turn_core::tool_ledger_receipt::ToolLedgerReceipt::empty(
                "run-generation-fence",
                authority.owner_generation,
            )
            .validate()
            .is_ok(),
            "the first exact durable owner is generation zero receipt authority"
        );

        let claimed = store
            .claim_recoverable_active_runs(1)
            .await
            .expect("claim execution owner");
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].run_generation, 1);

        let stale = engine
            .commit_terminal_status_with_events_if_current_owner(
                "user-1",
                "session-1",
                "run-generation-fence",
                &[STATUS_RUNNING],
                authority.owner_generation,
                STATUS_COMPLETED,
                None,
                None,
                &[serde_json::json!({"event_type":"run_finished","data":{}})],
            )
            .await
            .expect("stale terminal CAS is a resolved ownership race");
        assert!(matches!(stale, TerminalTransitionOutcome::Superseded(_)));
        let durable = engine
            .load_run("user-1", "run-generation-fence")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(durable.status, STATUS_RUNNING);
        assert_eq!(durable.run_generation, 1);
        assert!(durable.events.iter().all(|event| {
            event.get("event_type").and_then(serde_json::Value::as_str) != Some("run_finished")
        }));
    }

    #[tokio::test]
    async fn expired_generation_cannot_commit_execution_owner_cancellation_after_reclaim() {
        let store = Arc::new(
            InMemoryRunStateStore::new().with_execution_owner("recovery-owner", Duration::ZERO),
        );
        let engine = RunEngine::new(store.clone());
        let authority = engine
            .start_run("run-stale-owner-cancel", "user-1", "session-1")
            .await
            .expect("start durable run");
        let claimed = store
            .claim_recoverable_active_runs(1)
            .await
            .expect("claim next execution owner");
        assert_eq!(claimed[0].run_generation, authority.owner_generation + 1);

        let outcome = engine
            .cancel_if_exact_live_owner(
                "user-1",
                "session-1",
                "run-stale-owner-cancel",
                authority.owner_generation,
                &[STATUS_RUNNING],
                astra_turn_core::orchestration_types::CancellationOrigin::Runtime,
                "stale executor stopped",
            )
            .await
            .expect("stale cancellation is a resolved ownership race");

        assert!(matches!(
            outcome,
            astra_services::runs::AtomicExecutionOwnerCancellation::NotOwnedActive {
                owner_generation: 1,
                ..
            }
        ));
        let durable = engine
            .load_run("user-1", "run-stale-owner-cancel")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(durable.run_generation, authority.owner_generation + 1);
        assert!(
            durable
                .events
                .iter()
                .all(|event| { astra_services::runs::extract_event_type(event) != "run_finished" })
        );
    }

    #[tokio::test]
    async fn user_marker_first_prevents_runtime_from_overwriting_cancellation_origin() {
        let store = Arc::new(
            InMemoryRunStateStore::new()
                .with_execution_owner("runtime-owner", Duration::from_secs(60)),
        );
        let engine = RunEngine::new(store);
        let authority = engine
            .start_run("run-user-first", "user-1", "session-1")
            .await
            .unwrap();
        assert!(
            engine
                .request_run_cancellation("user-1", "run-user-first")
                .await
                .unwrap()
        );

        assert!(matches!(
            engine
                .cancel_if_exact_live_owner(
                    "user-1",
                    "session-1",
                    "run-user-first",
                    authority.owner_generation,
                    &[STATUS_RUNNING],
                    astra_turn_core::orchestration_types::CancellationOrigin::Runtime,
                    "runtime observed shutdown",
                )
                .await
                .unwrap(),
            astra_services::runs::AtomicExecutionOwnerCancellation::NotOwnedActive { .. }
        ));
        let user_terminal = serde_json::json!({
            "event_type": "run_finished",
            "data": {
                "run_id": "run-user-first",
                "status": STATUS_CANCELLED,
                "cancelled": true,
                "reason": "user requested cancellation",
                "source": "run_cancellation_request",
                "cancellation_origin": "user",
            }
        });
        assert!(matches!(
            engine
                .commit_terminal_status_with_events_if_current(
                    "user-1",
                    "session-1",
                    "run-user-first",
                    &[STATUS_RUNNING],
                    STATUS_CANCELLED,
                    None,
                    None,
                    &[user_terminal],
                )
                .await
                .unwrap(),
            TerminalTransitionOutcome::Committed(_)
        ));

        let durable = engine
            .load_run("user-1", "run-user-first")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(durable.status, STATUS_CANCELLED);
        let terminals = durable
            .events
            .iter()
            .filter(|event| astra_services::runs::extract_event_type(event) == "run_finished")
            .collect::<Vec<_>>();
        assert_eq!(terminals.len(), 1);
        assert_eq!(terminals[0]["data"]["cancellation_origin"], "user");
    }

    #[tokio::test]
    async fn runtime_terminal_first_prevents_late_user_marker_from_rewriting_origin() {
        let store = Arc::new(
            InMemoryRunStateStore::new()
                .with_execution_owner("runtime-owner", Duration::from_secs(60)),
        );
        let engine = RunEngine::new(store);
        let authority = engine
            .start_run("run-runtime-first", "user-1", "session-1")
            .await
            .unwrap();

        assert_eq!(
            engine
                .cancel_if_exact_live_owner(
                    "user-1",
                    "session-1",
                    "run-runtime-first",
                    authority.owner_generation,
                    &[STATUS_RUNNING],
                    astra_turn_core::orchestration_types::CancellationOrigin::Runtime,
                    "runtime stopped execution",
                )
                .await
                .unwrap(),
            astra_services::runs::AtomicExecutionOwnerCancellation::Committed
        );
        assert!(
            !engine
                .request_run_cancellation("user-1", "run-runtime-first")
                .await
                .unwrap()
        );

        let durable = engine
            .load_run("user-1", "run-runtime-first")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(durable.status, STATUS_CANCELLED);
        assert!(durable.owner_pod_id.is_none());
        assert!(durable.owner_lease_expires_at.is_none());
        let terminals = durable
            .events
            .iter()
            .filter(|event| astra_services::runs::extract_event_type(event) == "run_finished")
            .collect::<Vec<_>>();
        assert_eq!(terminals.len(), 1);
        assert_eq!(terminals[0]["data"]["cancellation_origin"], "runtime");
    }

    #[tokio::test]
    async fn owner_terminal_reconciliation_records_user_origin_from_request_marker() {
        let engine = test_engine();
        let authority = engine
            .start_run("run-terminal-user-cancel", "user-1", "session-1")
            .await
            .expect("start durable run");
        assert!(
            engine
                .request_run_cancellation("user-1", "run-terminal-user-cancel")
                .await
                .expect("record user cancellation")
        );

        let outcome = engine
            .commit_terminal_status_with_events_if_current_owner(
                "user-1",
                "session-1",
                "run-terminal-user-cancel",
                &[STATUS_RUNNING],
                authority.owner_generation,
                STATUS_COMPLETED,
                None,
                None,
                &[serde_json::json!({
                    "event_type": "run_finished",
                    "data": {"status": STATUS_COMPLETED},
                })],
            )
            .await
            .expect("cancellation request must reconcile terminal race");

        let TerminalTransitionOutcome::Superseded(run) = outcome else {
            panic!("user cancellation must supersede completed terminal");
        };
        assert_eq!(run.status, STATUS_CANCELLED);
        assert_eq!(
            run.events
                .last()
                .and_then(|event| event.pointer("/data/cancellation_origin"))
                .and_then(serde_json::Value::as_str),
            Some("user")
        );
    }

    #[tokio::test]
    async fn superseded_execution_generation_cannot_overwrite_run_usage() {
        let store = Arc::new(InMemoryRunStateStore::new());
        let engine = RunEngine::new(store.clone());
        let authority = engine
            .start_run("run-usage-generation-fence", "user-1", "session-1")
            .await
            .expect("start durable run");
        let claimed = store
            .claim_recoverable_active_runs(1)
            .await
            .expect("claim execution owner");
        assert_eq!(claimed[0].run_generation, 1);

        assert!(
            !engine
                .persist_usage_if_current_owner(
                    "user-1",
                    "session-1",
                    "run-usage-generation-fence",
                    authority.owner_generation,
                    900,
                    90,
                    9,
                )
                .await
                .expect("stale usage CAS is a resolved ownership race")
        );
        assert!(
            engine
                .persist_usage_if_current_owner(
                    "user-1",
                    "session-1",
                    "run-usage-generation-fence",
                    1,
                    100,
                    10,
                    1,
                )
                .await
                .expect("current owner writes semantic aggregate")
        );
        let durable = engine
            .load_run("user-1", "run-usage-generation-fence")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(durable.total_prompt_tokens, 100);
        assert_eq!(durable.total_completion_tokens, 10);
        assert_eq!(durable.total_tool_calls, 1);
    }

    #[derive(Clone, Copy)]
    enum BatchTransitionFailureMode {
        FailBeforeStoreWrite,
        FailAfterStoreWrite,
        FailAfterStatusWrite,
        ConcurrentCancelWins,
    }

    #[derive(Clone, Copy)]
    enum LeaseRenewalBehavior {
        Renew,
        Refuse,
        Pending,
        Delayed(Duration),
    }

    #[derive(Clone, Copy)]
    enum LeaseReleaseBehavior {
        Succeed,
        Pending,
    }

    struct ActiveLeaseReleaseGuard<'a>(&'a AtomicUsize);

    impl Drop for ActiveLeaseReleaseGuard<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    struct FlakyBatchTransitionStore {
        inner: InMemoryRunStateStore,
        fail_remaining: AtomicUsize,
        cancellation_lookup_failures: AtomicUsize,
        attempts: AtomicUsize,
        waiting_queries: AtomicUsize,
        recovery_claims: AtomicUsize,
        lease_renewal_interval: Option<Duration>,
        lease_duration: Option<Duration>,
        lease_renewal_behavior: LeaseRenewalBehavior,
        lease_release_behavior: LeaseReleaseBehavior,
        lease_renewals: AtomicUsize,
        lease_releases: AtomicUsize,
        active_lease_releases: AtomicUsize,
        mode: BatchTransitionFailureMode,
    }

    impl FlakyBatchTransitionStore {
        fn new(failures: usize, mode: BatchTransitionFailureMode) -> Self {
            Self {
                inner: InMemoryRunStateStore::new(),
                fail_remaining: AtomicUsize::new(failures),
                cancellation_lookup_failures: AtomicUsize::new(0),
                attempts: AtomicUsize::new(0),
                waiting_queries: AtomicUsize::new(0),
                recovery_claims: AtomicUsize::new(0),
                lease_renewal_interval: None,
                lease_duration: None,
                lease_renewal_behavior: LeaseRenewalBehavior::Renew,
                lease_release_behavior: LeaseReleaseBehavior::Succeed,
                lease_renewals: AtomicUsize::new(0),
                lease_releases: AtomicUsize::new(0),
                active_lease_releases: AtomicUsize::new(0),
                mode,
            }
        }

        fn with_cancellation_lookup_failures(self, failures: usize) -> Self {
            self.cancellation_lookup_failures
                .store(failures, Ordering::SeqCst);
            self
        }

        fn with_owner_lease_heartbeat(mut self, interval: Duration) -> Self {
            self.lease_renewal_interval = Some(interval);
            self.lease_duration = Some(interval.saturating_mul(3));
            self
        }

        fn with_lease_renewal_behavior(mut self, behavior: LeaseRenewalBehavior) -> Self {
            self.lease_renewal_behavior = behavior;
            self
        }

        fn with_lease_release_behavior(mut self, behavior: LeaseReleaseBehavior) -> Self {
            self.lease_release_behavior = behavior;
            self
        }

        fn attempts(&self) -> usize {
            self.attempts.load(Ordering::SeqCst)
        }

        fn waiting_queries(&self) -> usize {
            self.waiting_queries.load(Ordering::SeqCst)
        }

        fn recovery_claims(&self) -> usize {
            self.recovery_claims.load(Ordering::SeqCst)
        }

        fn lease_renewals(&self) -> usize {
            self.lease_renewals.load(Ordering::SeqCst)
        }

        fn lease_releases(&self) -> usize {
            self.lease_releases.load(Ordering::SeqCst)
        }

        fn active_lease_releases(&self) -> usize {
            self.active_lease_releases.load(Ordering::SeqCst)
        }

        fn should_fail_this_attempt(&self) -> bool {
            self.fail_remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
        }
    }

    #[async_trait::async_trait]
    impl RunStateStore for FlakyBatchTransitionStore {
        async fn insert_run(&self, record: DurableRunRecord) -> Result<(), String> {
            self.inner.insert_run(record).await
        }

        async fn claim_run_start(
            &self,
            record: DurableRunRecord,
            requested_session_id: Option<&str>,
        ) -> Result<DurableRunStartClaim, String> {
            self.inner
                .claim_run_start(record, requested_session_id)
                .await
        }

        async fn load_run(
            &self,
            user_id: &str,
            run_id: &str,
        ) -> Result<Option<DurableRunRecord>, String> {
            self.inner.load_run(user_id, run_id).await
        }

        async fn apply_run_user_intents(
            &self,
            request: AtomicRunUserIntentApplyRequest<'_>,
        ) -> Result<AtomicRunUserIntentApply, String> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            if self.should_fail_this_attempt() {
                match self.mode {
                    BatchTransitionFailureMode::FailBeforeStoreWrite => {
                        return Err("transient EOF before atomic intent apply".to_string());
                    }
                    BatchTransitionFailureMode::FailAfterStoreWrite
                    | BatchTransitionFailureMode::FailAfterStatusWrite => {
                        return Ok(match self.inner.apply_run_user_intents(request).await? {
                            AtomicRunUserIntentApply::Applied { event_indices } => {
                                AtomicRunUserIntentApply::AckRecovered { event_indices }
                            }
                            outcome => outcome,
                        });
                    }
                    BatchTransitionFailureMode::ConcurrentCancelWins => {
                        self.inner
                            .update_run_status_with_events_if_current(
                                request.user_id,
                                request.expected_session_id,
                                request.run_id,
                                &[STATUS_RUNNING],
                                Some(request.expected_owner_generation),
                                STATUS_CANCELLED,
                                None,
                                Some("cancelled elsewhere"),
                                &[serde_json::json!({
                                    "event_type": "run_finished",
                                    "data": {
                                        "status": STATUS_CANCELLED,
                                        "cancelled": true,
                                        "cancellation_origin": "unverified",
                                        "reason": "concurrent cancellation won",
                                        "source": "test_store",
                                    }
                                })],
                            )
                            .await?;
                    }
                }
            }
            self.inner.apply_run_user_intents(request).await
        }

        async fn is_run_cancellation_requested(
            &self,
            user_id: &str,
            run_id: &str,
        ) -> Result<bool, String> {
            if self
                .cancellation_lookup_failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                return Err("transient cancellation-intent lookup failure".to_string());
            }
            self.inner
                .is_run_cancellation_requested(user_id, run_id)
                .await
        }

        async fn load_run_event_delta(
            &self,
            user_id: &str,
            run_id: &str,
            after_event_idx: i64,
        ) -> Result<Option<DurableRunEventDelta>, String> {
            self.inner
                .load_run_event_delta(user_id, run_id, after_event_idx)
                .await
        }

        async fn load_user_intent_control_delta(
            &self,
            user_id: &str,
            run_id: &str,
            after_event_idx: i64,
            limit: usize,
        ) -> Result<Option<DurableRunUserIntentControlDelta>, String> {
            self.inner
                .load_user_intent_control_delta(user_id, run_id, after_event_idx, limit)
                .await
        }

        async fn update_run_status(
            &self,
            user_id: &str,
            expected_session_id: &str,
            run_id: &str,
            status: &str,
            waiting_for: Option<&str>,
            error_message: Option<&str>,
        ) -> Result<bool, String> {
            self.inner
                .update_run_status(
                    user_id,
                    expected_session_id,
                    run_id,
                    status,
                    waiting_for,
                    error_message,
                )
                .await
        }

        async fn update_run_status_if_current(
            &self,
            request: RunStatusCasRequest<'_>,
        ) -> Result<bool, String> {
            self.inner.update_run_status_if_current(request).await
        }

        async fn update_run_status_with_event_if_current(
            &self,
            user_id: &str,
            expected_session_id: &str,
            run_id: &str,
            expected_statuses: &[&str],
            status: &str,
            waiting_for: Option<&str>,
            error_message: Option<&str>,
            event: serde_json::Value,
        ) -> Result<bool, String> {
            self.inner
                .update_run_status_with_event_if_current(
                    user_id,
                    expected_session_id,
                    run_id,
                    expected_statuses,
                    status,
                    waiting_for,
                    error_message,
                    event,
                )
                .await
        }

        async fn update_run_status_with_events_if_current(
            &self,
            user_id: &str,
            expected_session_id: &str,
            run_id: &str,
            expected_statuses: &[&str],
            expected_owner_generation: Option<u64>,
            status: &str,
            waiting_for: Option<&str>,
            error_message: Option<&str>,
            events: &[serde_json::Value],
        ) -> Result<bool, String> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            if self.should_fail_this_attempt() {
                match self.mode {
                    BatchTransitionFailureMode::FailBeforeStoreWrite => {
                        return Err("transient EOF before commit".to_string());
                    }
                    BatchTransitionFailureMode::FailAfterStoreWrite => {
                        self.inner
                            .update_run_status_with_events_if_current(
                                user_id,
                                expected_session_id,
                                run_id,
                                expected_statuses,
                                expected_owner_generation,
                                status,
                                waiting_for,
                                error_message,
                                events,
                            )
                            .await?;
                        return Err("transient EOF after commit".to_string());
                    }
                    BatchTransitionFailureMode::FailAfterStatusWrite => {
                        self.inner
                            .update_run_status_if_current(RunStatusCasRequest {
                                user_id,
                                expected_session_id,
                                run_id,
                                expected_statuses,
                                status,
                                waiting_for,
                                error_message,
                            })
                            .await?;
                        return Err("transient EOF after status-only commit".to_string());
                    }
                    BatchTransitionFailureMode::ConcurrentCancelWins => {
                        self.inner
                            .update_run_status_with_events_if_current(
                                user_id,
                                expected_session_id,
                                run_id,
                                expected_statuses,
                                expected_owner_generation,
                                STATUS_CANCELLED,
                                None,
                                Some("cancelled elsewhere"),
                                &[serde_json::json!({
                                    "event_type": "run_finished",
                                    "data": {
                                        "status": STATUS_CANCELLED,
                                        "cancelled": true,
                                        "cancellation_origin": "unverified",
                                        "reason": "concurrent cancellation won",
                                        "source": "test_store",
                                    }
                                })],
                            )
                            .await?;
                        return Err("transient EOF after concurrent cancel".to_string());
                    }
                }
            }
            self.inner
                .update_run_status_with_events_if_current(
                    user_id,
                    expected_session_id,
                    run_id,
                    expected_statuses,
                    expected_owner_generation,
                    status,
                    waiting_for,
                    error_message,
                    events,
                )
                .await
        }

        async fn update_run_usage(
            &self,
            user_id: &str,
            expected_session_id: &str,
            run_id: &str,
            prompt_tokens: u64,
            completion_tokens: u64,
            tool_calls: u32,
        ) -> Result<bool, String> {
            self.inner
                .update_run_usage(
                    user_id,
                    expected_session_id,
                    run_id,
                    prompt_tokens,
                    completion_tokens,
                    tool_calls,
                )
                .await
        }

        async fn update_run_usage_if_current_owner(
            &self,
            request: RunUsageOwnerUpdateRequest<'_>,
        ) -> Result<bool, String> {
            self.inner.update_run_usage_if_current_owner(request).await
        }

        async fn save_checkpoint(
            &self,
            user_id: &str,
            expected_session_id: &str,
            run_id: &str,
            checkpoint_json: &str,
        ) -> Result<bool, String> {
            self.inner
                .save_checkpoint(user_id, expected_session_id, run_id, checkpoint_json)
                .await
        }

        async fn load_latest_checkpoint(
            &self,
            user_id: &str,
            run_id: &str,
            checkpoint_kind: Option<&str>,
        ) -> Result<Option<DurableRunCheckpointRecord>, String> {
            self.inner
                .load_latest_checkpoint(user_id, run_id, checkpoint_kind)
                .await
        }

        async fn load_run_projection(
            &self,
            user_id: &str,
            run_id: &str,
        ) -> Result<Option<DurableRunDisplayProjectionRecord>, String> {
            self.inner.load_run_projection(user_id, run_id).await
        }

        async fn rebuild_run_projection(
            &self,
            user_id: &str,
            expected_session_id: &str,
            run_id: &str,
        ) -> Result<Option<DurableRunDisplayProjectionRecord>, String> {
            self.inner
                .rebuild_run_projection(user_id, expected_session_id, run_id)
                .await
        }

        async fn append_events_batch(
            &self,
            user_id: &str,
            expected_session_id: &str,
            run_id: &str,
            events: &[serde_json::Value],
        ) -> Result<(), String> {
            self.inner
                .append_events_batch(user_id, expected_session_id, run_id, events)
                .await
        }

        async fn list_user_runs_cursor(
            &self,
            user_id: &str,
            limit: u32,
            cursor: Option<astra_services::runs::RunListCursor>,
        ) -> Result<astra_services::runs::DurableRunListPage, String> {
            self.inner
                .list_user_runs_cursor(user_id, limit, cursor)
                .await
        }

        async fn find_waiting_runs(&self) -> Result<Vec<DurableRunRecord>, String> {
            self.waiting_queries.fetch_add(1, Ordering::SeqCst);
            self.inner.find_waiting_runs().await
        }

        async fn find_running_runs(&self) -> Result<Vec<DurableRunRecord>, String> {
            self.inner.find_running_runs().await
        }

        async fn claim_recoverable_active_runs(
            &self,
            limit: u32,
        ) -> Result<Vec<DurableRunRecord>, String> {
            self.recovery_claims.fetch_add(1, Ordering::SeqCst);
            self.inner.claim_recoverable_active_runs(limit).await
        }

        fn owner_lease_renewal_interval(&self) -> Option<Duration> {
            self.lease_renewal_interval
        }

        fn owner_lease_duration(&self) -> Option<Duration> {
            self.lease_duration
        }

        async fn renew_owner_lease(
            &self,
            _user_id: &str,
            _expected_session_id: &str,
            _run_id: &str,
            _expected_owner_generation: u64,
            _expected_statuses: &[&str],
        ) -> Result<bool, String> {
            self.lease_renewals.fetch_add(1, Ordering::SeqCst);
            match self.lease_renewal_behavior {
                LeaseRenewalBehavior::Renew => Ok(true),
                LeaseRenewalBehavior::Refuse => Ok(false),
                LeaseRenewalBehavior::Pending => {
                    std::future::pending::<Result<bool, String>>().await
                }
                LeaseRenewalBehavior::Delayed(delay) => {
                    tokio::time::sleep(delay).await;
                    Ok(true)
                }
            }
        }

        async fn release_owner_lease(
            &self,
            _user_id: &str,
            _expected_session_id: &str,
            _run_id: &str,
            _expected_owner_generation: u64,
        ) -> Result<bool, String> {
            self.lease_releases.fetch_add(1, Ordering::SeqCst);
            self.active_lease_releases.fetch_add(1, Ordering::SeqCst);
            let _active = ActiveLeaseReleaseGuard(&self.active_lease_releases);
            match self.lease_release_behavior {
                LeaseReleaseBehavior::Succeed => Ok(true),
                LeaseReleaseBehavior::Pending => {
                    std::future::pending::<Result<bool, String>>().await
                }
            }
        }

        async fn find_blocking_session_run(
            &self,
            user_id: &str,
            session_id: &str,
        ) -> Result<Option<DurableRunRecord>, String> {
            self.inner
                .find_blocking_session_run(user_id, session_id)
                .await
        }

        async fn find_sub_runs(
            &self,
            user_id: &str,
            delegation_id: &str,
        ) -> Result<Vec<DurableRunRecord>, String> {
            self.inner.find_sub_runs(user_id, delegation_id).await
        }

        async fn update_retry_count(
            &self,
            user_id: &str,
            expected_session_id: &str,
            run_id: &str,
            retry_count: u32,
        ) -> Result<bool, String> {
            self.inner
                .update_retry_count(user_id, expected_session_id, run_id, retry_count)
                .await
        }
    }

    struct FailingLoadRunStore;

    #[async_trait::async_trait]
    impl RunStateStore for FailingLoadRunStore {
        async fn insert_run(&self, _record: DurableRunRecord) -> Result<(), String> {
            Err("store unavailable".into())
        }

        async fn claim_run_start(
            &self,
            _record: DurableRunRecord,
            _requested_session_id: Option<&str>,
        ) -> Result<DurableRunStartClaim, String> {
            Err("store unavailable".into())
        }

        async fn load_run(
            &self,
            _user_id: &str,
            _run_id: &str,
        ) -> Result<Option<DurableRunRecord>, String> {
            Err("load failed".into())
        }

        async fn load_run_event_delta(
            &self,
            _user_id: &str,
            _run_id: &str,
            _after_event_idx: i64,
        ) -> Result<Option<DurableRunEventDelta>, String> {
            Err("load failed".into())
        }

        async fn load_user_intent_control_delta(
            &self,
            _user_id: &str,
            _run_id: &str,
            _after_event_idx: i64,
            _limit: usize,
        ) -> Result<Option<DurableRunUserIntentControlDelta>, String> {
            Err("load failed".into())
        }

        async fn update_run_status(
            &self,
            _user_id: &str,
            _expected_session_id: &str,
            _run_id: &str,
            _status: &str,
            _waiting_for: Option<&str>,
            _error_message: Option<&str>,
        ) -> Result<bool, String> {
            Err("store unavailable".into())
        }

        async fn update_run_status_if_current(
            &self,
            _request: RunStatusCasRequest<'_>,
        ) -> Result<bool, String> {
            Err("store unavailable".into())
        }

        async fn update_run_status_with_event_if_current(
            &self,
            _user_id: &str,
            _expected_session_id: &str,
            _run_id: &str,
            _expected_statuses: &[&str],
            _status: &str,
            _waiting_for: Option<&str>,
            _error_message: Option<&str>,
            _event: serde_json::Value,
        ) -> Result<bool, String> {
            Err("store unavailable".into())
        }

        async fn update_run_status_with_events_if_current(
            &self,
            _user_id: &str,
            _expected_session_id: &str,
            _run_id: &str,
            _expected_statuses: &[&str],
            _expected_owner_generation: Option<u64>,
            _status: &str,
            _waiting_for: Option<&str>,
            _error_message: Option<&str>,
            _events: &[serde_json::Value],
        ) -> Result<bool, String> {
            Err("store unavailable".into())
        }

        async fn update_run_usage(
            &self,
            _user_id: &str,
            _expected_session_id: &str,
            _run_id: &str,
            _prompt_tokens: u64,
            _completion_tokens: u64,
            _tool_calls: u32,
        ) -> Result<bool, String> {
            Err("store unavailable".into())
        }

        async fn update_run_usage_if_current_owner(
            &self,
            _request: RunUsageOwnerUpdateRequest<'_>,
        ) -> Result<bool, String> {
            Err("store unavailable".into())
        }

        async fn save_checkpoint(
            &self,
            _user_id: &str,
            _expected_session_id: &str,
            _run_id: &str,
            _checkpoint_json: &str,
        ) -> Result<bool, String> {
            Err("store unavailable".into())
        }

        async fn load_latest_checkpoint(
            &self,
            _user_id: &str,
            _run_id: &str,
            _checkpoint_kind: Option<&str>,
        ) -> Result<Option<DurableRunCheckpointRecord>, String> {
            Err("store unavailable".into())
        }

        async fn load_run_projection(
            &self,
            _user_id: &str,
            _run_id: &str,
        ) -> Result<Option<DurableRunDisplayProjectionRecord>, String> {
            Err("store unavailable".into())
        }

        async fn rebuild_run_projection(
            &self,
            _user_id: &str,
            _expected_session_id: &str,
            _run_id: &str,
        ) -> Result<Option<DurableRunDisplayProjectionRecord>, String> {
            Err("store unavailable".into())
        }

        async fn append_events_batch(
            &self,
            _user_id: &str,
            _expected_session_id: &str,
            _run_id: &str,
            _events: &[serde_json::Value],
        ) -> Result<(), String> {
            Err("store unavailable".into())
        }

        async fn list_user_runs_cursor(
            &self,
            _user_id: &str,
            _limit: u32,
            _cursor: Option<RunListCursor>,
        ) -> Result<DurableRunListPage, String> {
            Err("store unavailable".into())
        }

        async fn find_waiting_runs(&self) -> Result<Vec<DurableRunRecord>, String> {
            Err("store unavailable".into())
        }

        async fn find_running_runs(&self) -> Result<Vec<DurableRunRecord>, String> {
            Err("store unavailable".into())
        }

        async fn find_blocking_session_run(
            &self,
            _user_id: &str,
            _session_id: &str,
        ) -> Result<Option<DurableRunRecord>, String> {
            Err("store unavailable".into())
        }

        async fn find_sub_runs(
            &self,
            _user_id: &str,
            _delegation_id: &str,
        ) -> Result<Vec<DurableRunRecord>, String> {
            Err("store unavailable".into())
        }

        async fn update_retry_count(
            &self,
            _user_id: &str,
            _expected_session_id: &str,
            _run_id: &str,
            _retry_count: u32,
        ) -> Result<bool, String> {
            Err("store unavailable".into())
        }
    }

    #[tokio::test]
    async fn start_and_load_run() {
        let engine = test_engine();
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();
        let run = engine.load_run("user-1", "run-1").await.unwrap().unwrap();
        assert_eq!(run.run_id, "run-1");
        assert_eq!(run.user_id, "user-1");
        assert_eq!(run.session_id, "sess-1");
        assert_eq!(run.status, "running");
        assert_eq!(run.events.len(), 1);
        assert_eq!(run.events[0]["data"]["interaction_mode"], "headless");
    }

    #[tokio::test]
    async fn legacy_or_malformed_durable_mode_fails_closed_to_headless() {
        let engine = test_engine();
        engine
            .start_run("legacy", "user-1", "sess-1")
            .await
            .unwrap();
        let mut run = engine.load_run("user-1", "legacy").await.unwrap().unwrap();

        run.events[0]["data"]
            .as_object_mut()
            .unwrap()
            .remove("interaction_mode");
        assert_eq!(
            durable_run_effective_interaction_mode(&run),
            RequestedTurnInteractionMode::Headless
        );

        run.events[0]["data"]["interaction_mode"] = serde_json::json!("unknown");
        assert_eq!(
            durable_run_effective_interaction_mode(&run),
            RequestedTurnInteractionMode::Headless
        );
    }

    #[tokio::test]
    async fn start_run_with_context_persists_interaction_metadata() {
        let engine = test_engine();
        engine
            .start_run_with_context(
                "run-ctx",
                "user-1",
                "sess-1",
                RunStartContext {
                    interaction_mode: RequestedTurnInteractionMode::Auto,
                    interactive_client: Some(true),
                    turn_intent_policy: TurnIntentExecutionPolicy::FixedDefault,
                    skill_auto_route_policy: SkillAutoRouteExecutionPolicy::Disabled,
                    execution_metadata: None,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let run = engine.load_run("user-1", "run-ctx").await.unwrap().unwrap();
        assert_eq!(run.events[0]["event_type"], "run_started");
        assert_eq!(run.events[0]["data"]["interaction_mode"], "auto");
        assert_eq!(run.events[0]["data"]["interactive_client"], true);
        assert_eq!(run.events[0]["data"]["turn_intent_policy"], "fixed_default");
        assert_eq!(run.events[0]["data"]["skill_auto_route_policy"], "disabled");
    }

    #[test]
    fn run_started_event_records_complete_ordered_binding_set() {
        let event = run_started_event_data(&RunStartContext {
            agent_binding_ids: vec![
                "binding-foundation".to_string(),
                "binding-extension".to_string(),
            ],
            agent_binding_id: Some("binding-extension".to_string()),
            ..Default::default()
        });

        assert_eq!(
            event["agent_binding_ids"],
            serde_json::json!(["binding-foundation", "binding-extension"])
        );
        assert_eq!(event["agent_binding_id"], "binding-extension");
    }

    #[tokio::test]
    async fn start_run_with_context_persists_admitted_offering_identity_without_route_fields() {
        let engine = test_engine();
        engine
            .start_run_with_context(
                "run-model-identity",
                "user-1",
                "sess-1",
                RunStartContext {
                    model_selection: Some(ModelSelection {
                        offering_id: "offer-primary".to_string(),
                    }),
                    resolved_model_selection: Some(ResolvedModelSelection {
                        offering_id: "offer-primary".to_string(),
                        model_name: "provider-model-v2".to_string(),
                    }),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let run = engine
            .load_run("user-1", "run-model-identity")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.model_offering_id.as_deref(), Some("offer-primary"));
        assert_eq!(
            run.resolved_model_name.as_deref(),
            Some("provider-model-v2")
        );
    }

    #[tokio::test]
    async fn start_run_with_context_rejects_inconsistent_model_identity_before_persistence() {
        let engine = test_engine();
        let result = engine
            .start_run_with_context(
                "run-model-mismatch",
                "user-1",
                "sess-1",
                RunStartContext {
                    model_selection: Some(ModelSelection {
                        offering_id: "offer-a".to_string(),
                    }),
                    resolved_model_selection: Some(ResolvedModelSelection {
                        offering_id: "offer-b".to_string(),
                        model_name: "provider-model-v2".to_string(),
                    }),
                    ..Default::default()
                },
            )
            .await;

        assert!(result.is_err());
        assert!(
            engine
                .load_run("user-1", "run-model-mismatch")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn delegated_run_inherits_parent_run_identity() {
        let engine = test_engine();
        engine
            .start_run_with_context(
                "run-model-parent",
                "user-1",
                "sess-1",
                RunStartContext {
                    model_selection: Some(ModelSelection {
                        offering_id: "offer-primary".to_string(),
                    }),
                    resolved_model_selection: Some(ResolvedModelSelection {
                        offering_id: "offer-primary".to_string(),
                        model_name: "provider-model-v2".to_string(),
                    }),
                    provider_run_owner: Some(astra_services::runs::ProviderRunOwner {
                        provider_id: "moi".to_string(),
                        provider_scope_id: "workspace-a".to_string(),
                    }),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        engine
            .start_run_ext(
                "run-model-child",
                "user-1",
                "sess-1",
                Some("run-model-parent"),
                Some("delegation-1"),
                Some("reviewer"),
                None,
            )
            .await
            .unwrap();

        let child = engine
            .load_run("user-1", "run-model-child")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(child.model_offering_id.as_deref(), Some("offer-primary"));
        assert_eq!(
            child.resolved_model_name.as_deref(),
            Some("provider-model-v2")
        );
        assert_eq!(
            child.events[0]["data"]["provider_run_owner"],
            serde_json::json!({
                "provider_id": "moi",
                "provider_scope_id": "workspace-a"
            })
        );
    }

    #[tokio::test]
    async fn delegated_run_without_explicit_assignment_is_detached_from_work() {
        let engine = test_engine();
        let binding =
            work_binding("work-1", "branch-1", 3).with_item(DurableWorkItemRunBinding::new(
                astra_services::work::WorkItemId::root(),
                astra_services::work::WorkItemRevision::INITIAL,
                astra_services::work::WorkItemAttemptId::parse("attempt-1").expect("attempt"),
            ));
        engine
            .start_run_with_context(
                "run-work-parent",
                "user-1",
                "sess-1",
                RunStartContext {
                    work_binding: Some(binding.clone()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        engine
            .start_run_ext(
                "run-work-child",
                "user-1",
                "sess-1",
                Some("run-work-parent"),
                Some("delegation-1"),
                Some("reviewer"),
                None,
            )
            .await
            .unwrap();

        let child = engine
            .load_run("user-1", "run-work-child")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(child.work_binding, None);
    }

    #[tokio::test]
    async fn delegated_run_accepts_explicit_exact_work_graph_cut() {
        let engine = test_engine();
        let binding = work_binding("work-1", "branch-1", 3);
        engine
            .start_run_with_context(
                "run-work-parent",
                "user-1",
                "sess-1",
                RunStartContext {
                    work_binding: Some(binding.clone()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        engine
            .start_run_ext_with_context(
                "run-work-child",
                "user-1",
                "sess-1",
                Some("run-work-parent"),
                Some("delegation-1"),
                Some("worker"),
                None,
                RunStartContext {
                    work_binding: Some(binding.clone()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let child = engine
            .load_run("user-1", "run-work-child")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(child.work_binding, Some(binding));
    }

    #[tokio::test]
    async fn delegated_run_cannot_jump_to_another_work_graph_cut() {
        let engine = test_engine();
        engine
            .start_run_with_context(
                "run-work-parent",
                "user-1",
                "sess-1",
                RunStartContext {
                    work_binding: Some(work_binding("work-1", "branch-1", 3)),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let result = engine
            .start_run_ext_with_context(
                "run-work-child",
                "user-1",
                "sess-1",
                Some("run-work-parent"),
                Some("delegation-1"),
                Some("reviewer"),
                None,
                RunStartContext {
                    work_binding: Some(work_binding("work-2", "branch-2", 1)),
                    ..Default::default()
                },
            )
            .await;

        assert!(result.is_err());
        assert!(
            engine
                .load_run("user-1", "run-work-child")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn repository_validated_child_can_narrow_work_scope_to_current_item_attempt() {
        let engine = test_engine();
        let parent_binding =
            work_binding("work-1", "branch-1", 1).with_item(DurableWorkItemRunBinding::new(
                astra_services::work::WorkItemId::root(),
                astra_services::work::WorkItemRevision::INITIAL,
                astra_services::work::WorkItemAttemptId::parse("parent-attempt").expect("attempt"),
            ));
        engine
            .start_run_with_context(
                "run-work-parent",
                "user-1",
                "sess-1",
                RunStartContext {
                    work_binding: Some(parent_binding),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let child_binding =
            work_binding("work-1", "branch-1", 2).with_item(DurableWorkItemRunBinding::new(
                astra_services::work::WorkItemId::parse("task-1").expect("item"),
                astra_services::work::WorkItemRevision::INITIAL,
                astra_services::work::WorkItemAttemptId::parse("child-attempt").expect("attempt"),
            ));
        engine
            .start_run_ext_with_context(
                "run-work-child",
                "user-1",
                "sess-1",
                Some("run-work-parent"),
                Some("delegation-1"),
                Some("worker"),
                None,
                RunStartContext {
                    work_binding: Some(child_binding.clone()),
                    validated_work_item_assignment: true,
                    ..Default::default()
                },
            )
            .await
            .expect("repository-validated assignment may advance to the current graph");
        assert_eq!(
            engine
                .load_run("user-1", "run-work-child")
                .await
                .unwrap()
                .unwrap()
                .work_binding,
            Some(child_binding)
        );

        let unvalidated = engine
            .start_run_ext_with_context(
                "run-work-unvalidated-child",
                "user-1",
                "sess-1",
                Some("run-work-parent"),
                Some("delegation-2"),
                Some("worker"),
                None,
                RunStartContext {
                    work_binding: Some(
                        work_binding("work-1", "branch-1", 2).with_item(
                            DurableWorkItemRunBinding::new(
                                astra_services::work::WorkItemId::parse("task-1").expect("item"),
                                astra_services::work::WorkItemRevision::INITIAL,
                                astra_services::work::WorkItemAttemptId::parse("other-attempt")
                                    .expect("attempt"),
                            ),
                        ),
                    ),
                    ..Default::default()
                },
            )
            .await;
        assert!(unvalidated.is_err());
        assert!(
            engine
                .load_run("user-1", "run-work-unvalidated-child")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn delegated_run_cannot_replace_parent_offering_without_admission() {
        let engine = test_engine();
        engine
            .start_run_with_context(
                "run-model-parent",
                "user-1",
                "sess-1",
                RunStartContext {
                    model_selection: Some(ModelSelection {
                        offering_id: "offer-primary".to_string(),
                    }),
                    resolved_model_selection: Some(ResolvedModelSelection {
                        offering_id: "offer-primary".to_string(),
                        model_name: "provider-model-v2".to_string(),
                    }),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let result = engine
            .start_run_ext_with_context(
                "run-model-child",
                "user-1",
                "sess-1",
                Some("run-model-parent"),
                Some("delegation-1"),
                Some("reviewer"),
                None,
                RunStartContext {
                    model_selection: Some(ModelSelection {
                        offering_id: "offer-unadmitted".to_string(),
                    }),
                    resolved_model_selection: Some(ResolvedModelSelection {
                        offering_id: "offer-unadmitted".to_string(),
                        model_name: "other-provider-model".to_string(),
                    }),
                    ..Default::default()
                },
            )
            .await;

        assert!(result.is_err());
        assert!(
            engine
                .load_run("user-1", "run-model-child")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn delegated_run_requires_a_durable_parent() {
        let engine = test_engine();
        let result = engine
            .start_run_ext(
                "orphan-child",
                "user-1",
                "sess-1",
                Some("missing-parent"),
                Some("delegation-1"),
                Some("reviewer"),
                None,
            )
            .await;

        assert!(result.is_err());
        assert!(
            engine
                .load_run("user-1", "orphan-child")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn delegated_run_cannot_cross_its_parent_session_boundary() {
        let engine = test_engine();
        engine
            .start_run("parent-run", "user-1", "session-a")
            .await
            .unwrap();

        let result = engine
            .start_run_ext(
                "cross-session-child",
                "user-1",
                "session-b",
                Some("parent-run"),
                Some("delegation-1"),
                Some("reviewer"),
                None,
            )
            .await;

        assert!(result.is_err());
        assert!(
            engine
                .load_run("user-1", "cross-session-child")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn start_run_with_context_persists_effective_agent_binding_runtime_profile_when_omitted()
    {
        let engine = test_engine();
        let request = astra_services::runs::ChatRequestData {
            message: "hello".to_string(),
            conversation_authority: None,
            user_intent: None,
            parts: Vec::new(),
            attachments: Vec::new(),
            stable_runtime_system_prompt: None,
            runtime_system_prompt: None,
            session_id: None,
            work_binding: None,
            run_start_idempotency: None,
            full_llm_capture: false,
            agent_id: None,
            model: None,
            model_selection_mode: astra_services::runs::ModelSelectionMode::ExplicitOffering,
            model_selection: None,
            resolved_model_selection: None,
            admitted_model_execution: None,
            capability_descriptors: None,
            provider_runtime_authorized: false,
            agent_bindings: Vec::new(),
            agent_binding: Some(astra_services::runs::AgentBindingRuntimeRequest {
                id: "ab_018f05f5-c7dd-7f43-83e6-93d56d9d7391".to_string(),
            }),
            runtime_auth: None,
            runtime_skill_binding: None,
            runtime_profile: None,
            skill_search: None,
            allow_skills: None,
            allow_skill_sources: None,
            allow_tools: None,
            enabled_tools: None,
            workspace_binding: None,
            executor_binding: None,
            runtime_mcp_bindings: Vec::new(),
            context: None,
            edge_executor_id: None,
            capabilities: Vec::new(),
            forward_headers: std::collections::HashMap::new(),
            execution_budget: None,
            execution_time_budget: None,
            execution_policy: Default::default(),
            explain: false,
            interaction_mode: None,
            interactive_client: false,
            provider_run_owner: None,
            provider_workspace_id: None,
            agent_binding_owner_scope: None,
        };
        let context = crate::server::run::binding_resolution::run_start_context_from_request(
            &request, None, None,
        );

        engine
            .start_run_with_context("run-agent-binding", "user-1", "sess-1", context)
            .await
            .unwrap();

        let run = engine
            .load_run("user-1", "run-agent-binding")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            run.runtime_profile.as_deref(),
            Some("agent_binding_registry")
        );
        assert_eq!(
            run.events[0]["data"]["runtime_profile"],
            "agent_binding_registry"
        );
    }

    #[tokio::test]
    async fn run_start_context_persists_pre_normalized_prompt_mode() {
        let engine = test_engine();
        engine
            .start_run_with_context(
                "run-prompt",
                "user-1",
                "sess-1",
                RunStartContext {
                    interaction_mode: RequestedTurnInteractionMode::Prompt,
                    interactive_client: Some(true),
                    execution_metadata: None,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let run = engine
            .load_run("user-1", "run-prompt")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.events[0]["data"]["interaction_mode"], "prompt");
        assert_eq!(run.events[0]["data"]["interactive_client"], true);
    }

    #[tokio::test]
    async fn start_run_with_context_uses_snake_case_non_interactive_label() {
        let engine = test_engine();
        engine
            .start_run_with_context(
                "run-non-interactive",
                "user-1",
                "sess-1",
                RunStartContext {
                    interaction_mode: RequestedTurnInteractionMode::NonInteractive,
                    interactive_client: Some(false),
                    execution_metadata: None,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let run = engine
            .load_run("user-1", "run-non-interactive")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.events[0]["data"]["interaction_mode"], "non_interactive");
    }

    #[tokio::test]
    async fn start_run_ext_persists_retry_linkage() {
        let engine = test_engine();
        engine
            .start_run("parent-1", "user-1", "sess-1")
            .await
            .unwrap();
        engine
            .start_run_ext(
                "run-retry",
                "user-1",
                "sess-1",
                Some("parent-1"),
                Some("del-1"),
                Some("coder"),
                Some("run-original"),
            )
            .await
            .unwrap();

        let run = engine
            .load_run("user-1", "run-retry")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.parent_run_id.as_deref(), Some("parent-1"));
        assert_eq!(run.delegation_id.as_deref(), Some("del-1"));
        assert_eq!(run.agent_id.as_deref(), Some("coder"));
        assert_eq!(run.retry_of.as_deref(), Some("run-original"));
    }

    #[tokio::test]
    async fn load_nonexistent_returns_none() {
        let engine = test_engine();
        assert!(engine.load_run("user-1", "nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn persist_status_updates() {
        let engine = test_engine();
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();
        let ok = engine
            .persist_status(
                "user-1",
                "sess-1",
                "run-1",
                "paused",
                Some("user_resume"),
                None,
            )
            .await
            .unwrap();
        assert!(ok);
        let run = engine.load_run("user-1", "run-1").await.unwrap().unwrap();
        assert_eq!(run.status, "paused");
        assert_eq!(run.waiting_for.as_deref(), Some("user_resume"));
    }

    #[tokio::test]
    async fn persist_status_nonexistent_returns_false() {
        let engine = test_engine();
        let ok = engine
            .persist_status("user-1", "sess-1", "nope", "failed", None, Some("crash"))
            .await
            .unwrap();
        assert!(!ok);
    }

    #[tokio::test]
    async fn wrong_session_mutations_fail_closed_without_touching_the_owner_run() {
        let engine = test_engine();
        engine
            .start_run("session-fenced-run", "user-1", "session-owner")
            .await
            .unwrap();

        assert!(
            !engine
                .persist_status(
                    "user-1",
                    "session-other",
                    "session-fenced-run",
                    STATUS_PAUSED,
                    Some("user_resume"),
                    None,
                )
                .await
                .unwrap(),
            "a same-user caller from another session must lose the status mutation fence"
        );
        let append_error = engine
            .append_event(
                "user-1",
                "session-other",
                "session-fenced-run",
                serde_json::json!({"event_type": "run_paused"}),
            )
            .await
            .expect_err("a same-user caller from another session must not append facts");
        assert!(append_error.contains("run not found"));

        let durable = engine
            .load_run("user-1", "session-fenced-run")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(durable.status, STATUS_RUNNING);
        assert_eq!(durable.events.len(), 1);
    }

    #[tokio::test]
    async fn terminal_transition_retries_transient_error_before_commit() {
        let store = Arc::new(FlakyBatchTransitionStore::new(
            1,
            BatchTransitionFailureMode::FailBeforeStoreWrite,
        ));
        let engine = RunEngine::new(store.clone());
        engine
            .start_run("run-terminal-retry", "user-1", "sess-1")
            .await
            .unwrap();
        let terminal_events = vec![serde_json::json!({
            "event_type": "run_finished",
            "data": {"status": STATUS_COMPLETED}
        })];

        let outcome = engine
            .commit_terminal_status_with_events_if_current(
                "user-1",
                "sess-1",
                "run-terminal-retry",
                &[STATUS_RUNNING],
                STATUS_COMPLETED,
                None,
                None,
                &terminal_events,
            )
            .await
            .unwrap();

        assert!(matches!(outcome, TerminalTransitionOutcome::Committed(_)));
        assert_eq!(store.attempts(), 2);
        let run = engine
            .load_run("user-1", "run-terminal-retry")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.status, STATUS_COMPLETED);
        assert_eq!(
            run.events
                .iter()
                .filter(
                    |event| event.get("event_type").and_then(serde_json::Value::as_str)
                        == Some("run_finished")
                )
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn terminal_transition_returns_the_store_owned_intent_disposition() {
        let engine = test_engine();
        engine
            .start_run("run-terminal-intent-projection", "user-1", "sess-1")
            .await
            .unwrap();
        engine
            .append_events_batch(
                "user-1",
                "sess-1",
                "run-terminal-intent-projection",
                &[serde_json::json!({
                    "event_type": "user_intent",
                    "idempotency_key": "user_intent:intent-late",
                    "data": {
                        "intent_id": "intent-late",
                        "delivery": "guide_current_run",
                        "input": {"content": "preserve me"},
                    },
                })],
            )
            .await
            .unwrap();

        let outcome = engine
            .commit_terminal_status_with_events_if_current(
                "user-1",
                "sess-1",
                "run-terminal-intent-projection",
                &[STATUS_RUNNING],
                STATUS_COMPLETED,
                None,
                None,
                &[serde_json::json!({
                    "event_type": "run_finished",
                    "data": {"status": STATUS_COMPLETED},
                })],
            )
            .await
            .unwrap();

        let TerminalTransitionOutcome::Committed(run) = outcome else {
            panic!("terminal transition was unexpectedly superseded");
        };
        let terminal_types = run
            .events
            .iter()
            .rev()
            .take(2)
            .map(|event| event.get("event_type").and_then(serde_json::Value::as_str))
            .collect::<Vec<_>>();
        assert_eq!(
            terminal_types,
            vec![Some("run_finished"), Some("user_intent_returned")]
        );
        assert_eq!(
            run.events[run.events.len() - 2].pointer("/data/content"),
            Some(&serde_json::json!("preserve me"))
        );
    }

    #[tokio::test]
    async fn terminal_transition_reconciles_commit_after_unknown_error() {
        let store = Arc::new(FlakyBatchTransitionStore::new(
            1,
            BatchTransitionFailureMode::FailAfterStoreWrite,
        ));
        let engine = RunEngine::new(store.clone());
        engine
            .start_run("run-terminal-reconcile", "user-1", "sess-1")
            .await
            .unwrap();
        let terminal_events = vec![serde_json::json!({
            "event_type": "run_finished",
            "data": {"status": STATUS_COMPLETED}
        })];

        let outcome = engine
            .commit_terminal_status_with_events_if_current(
                "user-1",
                "sess-1",
                "run-terminal-reconcile",
                &[STATUS_RUNNING],
                STATUS_COMPLETED,
                None,
                None,
                &terminal_events,
            )
            .await
            .unwrap();

        assert!(matches!(outcome, TerminalTransitionOutcome::Committed(_)));
        assert_eq!(store.attempts(), 2);
        let run = engine
            .load_run("user-1", "run-terminal-reconcile")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.status, STATUS_COMPLETED);
        assert_eq!(
            run.events
                .iter()
                .filter(
                    |event| event.get("event_type").and_then(serde_json::Value::as_str)
                        == Some("run_finished")
                )
                .count(),
            1,
            "commit-after-EOF reconcile must not append duplicate terminal events"
        );
    }

    #[tokio::test]
    async fn terminal_transition_repairs_missing_events_after_status_only_unknown_error() {
        let store = Arc::new(FlakyBatchTransitionStore::new(
            1,
            BatchTransitionFailureMode::FailAfterStatusWrite,
        ));
        let engine = RunEngine::new(store.clone());
        engine
            .start_run("run-terminal-repair", "user-1", "sess-1")
            .await
            .unwrap();
        let terminal_events = vec![
            serde_json::json!({
                "event_type": "run_error",
                "data": {
                    "error": "tool failed",
                    "error_code": "tool_error",
                    "error_kind": "tool_error"
                }
            }),
            serde_json::json!({
                "event_type": "run_finished",
                "data": {
                    "status": STATUS_FAILED,
                    "error": "tool failed",
                    "error_code": "tool_error",
                    "error_kind": "tool_error"
                }
            }),
        ];

        let outcome = engine
            .commit_terminal_status_with_events_if_current(
                "user-1",
                "sess-1",
                "run-terminal-repair",
                &[STATUS_RUNNING],
                STATUS_FAILED,
                None,
                Some("tool failed"),
                &terminal_events,
            )
            .await
            .unwrap();

        assert!(matches!(outcome, TerminalTransitionOutcome::Committed(_)));
        assert_eq!(store.attempts(), 2);
        let run = engine
            .load_run("user-1", "run-terminal-repair")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.status, STATUS_FAILED);
        assert_eq!(
            run.events
                .iter()
                .filter(
                    |event| event.get("event_type").and_then(serde_json::Value::as_str)
                        == Some("run_error")
                )
                .count(),
            1
        );
        assert_eq!(
            run.events
                .iter()
                .filter(
                    |event| event.get("event_type").and_then(serde_json::Value::as_str)
                        == Some("run_finished")
                )
                .count(),
            1
        );
        assert!(durable_run_contains_event_batch(&run, &terminal_events));
    }

    #[tokio::test]
    async fn terminal_transition_retry_does_not_override_concurrent_cancel() {
        let store = Arc::new(FlakyBatchTransitionStore::new(
            1,
            BatchTransitionFailureMode::ConcurrentCancelWins,
        ));
        let engine = RunEngine::new(store.clone());
        engine
            .start_run("run-terminal-cancel-race", "user-1", "sess-1")
            .await
            .unwrap();
        let terminal_events = vec![serde_json::json!({
            "event_type": "run_finished",
            "data": {"status": STATUS_COMPLETED}
        })];

        let outcome = engine
            .commit_terminal_status_with_events_if_current(
                "user-1",
                "sess-1",
                "run-terminal-cancel-race",
                &[STATUS_RUNNING],
                STATUS_COMPLETED,
                None,
                None,
                &terminal_events,
            )
            .await
            .unwrap();

        assert!(matches!(
            outcome,
            TerminalTransitionOutcome::Superseded(ref run) if run.status == STATUS_CANCELLED
        ));
        assert_eq!(store.attempts(), 2);
        let run = engine
            .load_run("user-1", "run-terminal-cancel-race")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.status, STATUS_CANCELLED);
        assert_eq!(
            run.events.last().and_then(|event| {
                event
                    .pointer("/data/status")
                    .and_then(serde_json::Value::as_str)
            }),
            Some(STATUS_CANCELLED)
        );
    }

    #[tokio::test]
    async fn persist_usage_updates() {
        let engine = test_engine();
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();
        engine
            .persist_usage("user-1", "sess-1", "run-1", 1000, 500, 7)
            .await
            .unwrap();
        let run = engine.load_run("user-1", "run-1").await.unwrap().unwrap();
        assert_eq!(run.total_prompt_tokens, 1000);
        assert_eq!(run.total_completion_tokens, 500);
        assert_eq!(run.total_tool_calls, 7);
    }

    #[tokio::test]
    async fn persist_checkpoint_saves() {
        let engine = test_engine();
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();
        let ck = r#"{"version":"checkpoint_v1","graceful":true,"messages":[],"turn":3}"#;
        engine
            .persist_checkpoint("user-1", "sess-1", "run-1", ck)
            .await
            .unwrap();
        let checkpoint = engine
            .load_latest_checkpoint("user-1", "run-1", Some("resume"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(checkpoint.checkpoint_kind, "resume");
        assert_eq!(checkpoint.checkpoint_json, ck);
    }

    #[tokio::test]
    async fn run_projection_tracks_latest_event_usage_and_checkpoint() {
        let engine = test_engine();
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();
        engine
            .append_event(
                "user-1",
                "sess-1",
                "run-1",
                serde_json::json!({"event_type": "tool_call_start", "data": {"tool": "bash"}}),
            )
            .await
            .unwrap();
        engine
            .persist_usage("user-1", "sess-1", "run-1", 11, 7, 3)
            .await
            .unwrap();
        engine
            .persist_checkpoint(
                "user-1",
                "sess-1",
                "run-1",
                r#"{"version":"checkpoint_v2","graceful":true,"last_batch_id":"batch-1"}"#,
            )
            .await
            .unwrap();
        engine
            .persist_status(
                "user-1",
                "sess-1",
                "run-1",
                "waiting",
                Some("user_input"),
                None,
            )
            .await
            .unwrap();

        let projection = engine
            .load_run_projection("user-1", "run-1")
            .await
            .unwrap()
            .expect("projection should exist");
        assert_eq!(projection.status, "waiting");
        assert_eq!(projection.waiting_for.as_deref(), Some("user_input"));
        assert_eq!(projection.projection_event_idx, 1);
        assert_eq!(
            projection.latest_event_type.as_deref(),
            Some("tool_call_start")
        );
        assert_eq!(projection.latest_checkpoint_kind.as_deref(), Some("resume"));
        assert_eq!(
            projection.latest_checkpoint_version.as_deref(),
            Some("checkpoint_v2")
        );
        assert_eq!(projection.total_prompt_tokens, 11);
        assert_eq!(projection.total_completion_tokens, 7);
        assert_eq!(projection.total_tool_calls, 3);
        assert!(
            !projection.projection_hash.is_empty(),
            "projection hash should be stable and non-empty"
        );
    }

    #[tokio::test]
    async fn load_run_projection_missing_run_returns_none() {
        let engine = test_engine();
        let projection = engine.load_run_projection("user-1", "nope").await.unwrap();
        assert!(projection.is_none());
    }

    #[tokio::test]
    async fn append_event_accumulates() {
        let engine = test_engine();
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();
        engine
            .append_event(
                "user-1",
                "sess-1",
                "run-1",
                serde_json::json!({"event_type": "tool_call_start"}),
            )
            .await
            .unwrap();
        engine
            .append_event(
                "user-1",
                "sess-1",
                "run-1",
                serde_json::json!({"event_type": "tool_result"}),
            )
            .await
            .unwrap();
        let run = engine.load_run("user-1", "run-1").await.unwrap().unwrap();
        assert_eq!(run.events.len(), 3); // run_started + 2 appended
    }

    #[tokio::test]
    async fn find_waiting_runs_filters_correctly() {
        let engine = test_engine();
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();
        engine.start_run("run-2", "user-1", "sess-2").await.unwrap();
        engine
            .persist_status(
                "user-1",
                "sess-2",
                "run-2",
                "waiting",
                Some("tool_approval"),
                None,
            )
            .await
            .unwrap();
        let waiting = engine.find_waiting_runs().await.unwrap();
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].run_id, "run-2");
    }

    #[tokio::test]
    async fn find_blocking_session_run_matches_only_blocking_states() {
        let engine = test_engine();
        engine
            .start_run("running", "user-1", "sess-blocked")
            .await
            .unwrap();
        engine
            .start_run("paused-free", "user-1", "sess-free")
            .await
            .unwrap();
        engine
            .persist_status("user-1", "sess-free", "paused-free", "paused", None, None)
            .await
            .unwrap();
        engine
            .start_run("completed", "user-1", "sess-done")
            .await
            .unwrap();
        engine
            .persist_status("user-1", "sess-done", "completed", "completed", None, None)
            .await
            .unwrap();

        let blocked = engine
            .find_blocking_session_run("user-1", "sess-blocked")
            .await
            .unwrap();
        let free = engine
            .find_blocking_session_run("user-1", "sess-free")
            .await
            .unwrap();
        let done = engine
            .find_blocking_session_run("user-1", "sess-done")
            .await
            .unwrap();

        assert_eq!(blocked.unwrap().run_id, "running");
        assert!(free.is_none());
        assert!(done.is_none());
    }

    #[tokio::test]
    async fn check_control_status_uses_shared_durable_taxonomy() {
        let engine = test_engine();
        engine
            .start_run("paused", "user-1", "sess-paused")
            .await
            .unwrap();
        engine
            .persist_status(
                "user-1",
                "sess-paused",
                "paused",
                "paused",
                Some("user_resume"),
                None,
            )
            .await
            .unwrap();
        engine
            .start_run("waiting", "user-1", "sess-waiting")
            .await
            .unwrap();
        engine
            .persist_status(
                "user-1",
                "sess-waiting",
                "waiting",
                "waiting",
                Some("tool_approval"),
                None,
            )
            .await
            .unwrap();
        engine
            .start_run("cancelled", "user-1", "sess-cancelled")
            .await
            .unwrap();
        assert!(
            transition_typed_cancellation(
                &engine,
                "user-1",
                "sess-cancelled",
                "cancelled",
                &[STATUS_RUNNING],
                astra_turn_core::orchestration_types::CancellationOrigin::Runtime,
            )
            .await
        );

        assert_eq!(
            engine
                .check_control_status("user-1", "paused")
                .await
                .unwrap(),
            Some(RunControlStatus::Paused)
        );
        assert_eq!(
            engine
                .check_control_status("user-1", "waiting")
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            engine
                .check_control_status("user-1", "cancelled")
                .await
                .unwrap(),
            Some(RunControlStatus::Cancelled)
        );
    }

    #[tokio::test]
    async fn ancestor_controls_apply_transitively_without_copying_child_state() {
        let engine = test_engine();
        engine
            .start_run("root", "user-1", "session-1")
            .await
            .unwrap();
        engine
            .start_run_ext(
                "child",
                "user-1",
                "session-1",
                Some("root"),
                Some("delegation-1"),
                Some("worker"),
                None,
            )
            .await
            .unwrap();
        engine
            .start_run_ext(
                "grandchild",
                "user-1",
                "session-1",
                Some("child"),
                Some("delegation-2"),
                Some("reviewer"),
                None,
            )
            .await
            .unwrap();

        engine
            .persist_status(
                "user-1",
                "session-1",
                "root",
                STATUS_PAUSED,
                Some("user_resume"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            engine
                .check_control_status("user-1", "grandchild")
                .await
                .unwrap(),
            Some(RunControlStatus::Paused)
        );
        assert_eq!(
            engine
                .load_run("user-1", "grandchild")
                .await
                .unwrap()
                .unwrap()
                .status,
            STATUS_RUNNING,
            "ancestor control is derived at consumption time, not duplicated into child state"
        );

        assert!(
            transition_typed_cancellation(
                &engine,
                "user-1",
                "session-1",
                "root",
                &[STATUS_PAUSED],
                astra_turn_core::orchestration_types::CancellationOrigin::User,
            )
            .await
        );
        assert_eq!(
            engine
                .check_control_status("user-1", "grandchild")
                .await
                .unwrap(),
            Some(RunControlStatus::Cancelled)
        );
    }

    #[tokio::test]
    async fn cancellation_origin_query_finds_direct_child_user_request() {
        let engine = test_engine();
        engine
            .start_run("origin-root", "user-1", "session-1")
            .await
            .unwrap();
        engine
            .start_run_ext(
                "origin-child",
                "user-1",
                "session-1",
                Some("origin-root"),
                Some("delegation-1"),
                Some("worker"),
                None,
            )
            .await
            .unwrap();
        assert!(
            engine
                .request_run_cancellation("user-1", "origin-child")
                .await
                .unwrap()
        );

        assert_eq!(
            engine
                .cancellation_origin_in_lineage("user-1", "origin-child")
                .await
                .unwrap(),
            astra_turn_core::orchestration_types::CancellationOrigin::User,
            "a direct child DELETE marker is user-origin evidence even while its parent remains running"
        );
    }

    #[tokio::test]
    async fn cancellation_origin_query_finds_ancestor_user_request() {
        let engine = test_engine();
        engine
            .start_run("origin-root", "user-1", "session-1")
            .await
            .unwrap();
        engine
            .start_run_ext(
                "origin-child",
                "user-1",
                "session-1",
                Some("origin-root"),
                Some("delegation-1"),
                Some("worker"),
                None,
            )
            .await
            .unwrap();
        assert!(
            engine
                .request_run_cancellation("user-1", "origin-root")
                .await
                .unwrap()
        );

        assert_eq!(
            engine
                .cancellation_origin_in_lineage("user-1", "origin-child")
                .await
                .unwrap(),
            astra_turn_core::orchestration_types::CancellationOrigin::User,
            "a validated ancestor user marker must retain its origin for every descendant"
        );
    }

    #[tokio::test]
    async fn root_user_marker_crosses_intermediate_runtime_terminal_for_active_descendant() {
        let engine = test_engine();
        engine
            .start_run("origin-root", "user-1", "session-1")
            .await
            .unwrap();
        engine
            .start_run_ext(
                "origin-child",
                "user-1",
                "session-1",
                Some("origin-root"),
                Some("delegation-1"),
                Some("worker"),
                None,
            )
            .await
            .unwrap();
        engine
            .start_run_ext(
                "origin-grandchild",
                "user-1",
                "session-1",
                Some("origin-child"),
                Some("delegation-2"),
                Some("worker"),
                None,
            )
            .await
            .unwrap();
        assert!(
            transition_typed_cancellation(
                &engine,
                "user-1",
                "session-1",
                "origin-child",
                &[STATUS_RUNNING],
                astra_turn_core::orchestration_types::CancellationOrigin::Runtime,
            )
            .await
        );
        assert!(
            engine
                .request_run_cancellation("user-1", "origin-root")
                .await
                .unwrap()
        );

        assert_eq!(
            engine
                .latest_terminal_cancellation_origin("user-1", "origin-child")
                .await
                .unwrap(),
            Some(astra_turn_core::orchestration_types::CancellationOrigin::Runtime),
            "the intermediate run retains its exact immutable Runtime terminal"
        );
        assert_eq!(
            engine
                .check_control_status("user-1", "origin-grandchild")
                .await
                .unwrap(),
            Some(RunControlStatus::Cancelled),
            "the root User marker must still govern the active descendant"
        );
        assert_eq!(
            engine
                .cancellation_origin_in_lineage("user-1", "origin-grandchild")
                .await
                .unwrap(),
            astra_turn_core::orchestration_types::CancellationOrigin::User,
            "settlement origin must agree with the propagated control decision"
        );
    }

    #[tokio::test]
    async fn generic_status_apis_reject_cancelled_without_mutation() {
        let engine = test_engine();
        engine
            .start_run("ambiguous-cancel", "user-1", "session-1")
            .await
            .unwrap();
        let direct_error = engine
            .persist_status(
                "user-1",
                "session-1",
                "ambiguous-cancel",
                STATUS_CANCELLED,
                None,
                None,
            )
            .await
            .unwrap_err();
        assert!(direct_error.contains("cannot infer cancellation authority"));
        let cas_error = engine
            .persist_status_if_current(RunStatusCasRequest {
                user_id: "user-1",
                expected_session_id: "session-1",
                run_id: "ambiguous-cancel",
                expected_statuses: &[STATUS_RUNNING],
                status: STATUS_CANCELLED,
                waiting_for: None,
                error_message: None,
            })
            .await
            .unwrap_err();
        assert!(cas_error.contains("cannot infer cancellation authority"));
        let durable = engine
            .load_run("user-1", "ambiguous-cancel")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(durable.status, STATUS_RUNNING);
        assert_eq!(
            durable
                .events
                .iter()
                .filter(|event| {
                    astra_services::runs::extract_event_type(event) == "run_finished"
                })
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn check_control_status_propagates_only_typed_user_ancestor_cancellation() {
        for (case, origin, expected_control, expected_settlement_origin) in [
            (
                "user",
                astra_turn_core::orchestration_types::CancellationOrigin::User,
                Some(RunControlStatus::Cancelled),
                astra_turn_core::orchestration_types::CancellationOrigin::User,
            ),
            (
                "runtime",
                astra_turn_core::orchestration_types::CancellationOrigin::Runtime,
                None,
                astra_turn_core::orchestration_types::CancellationOrigin::Runtime,
            ),
            (
                "unverified",
                astra_turn_core::orchestration_types::CancellationOrigin::Unverified,
                None,
                astra_turn_core::orchestration_types::CancellationOrigin::Runtime,
            ),
        ] {
            let engine = test_engine();
            let root = format!("control-{case}-root");
            let child = format!("control-{case}-child");
            engine
                .start_run(&root, "user-1", "session-1")
                .await
                .unwrap();
            engine
                .start_run_ext(
                    &child,
                    "user-1",
                    "session-1",
                    Some(&root),
                    Some("delegation-1"),
                    Some("worker"),
                    None,
                )
                .await
                .unwrap();
            assert!(
                transition_typed_cancellation(
                    &engine,
                    "user-1",
                    "session-1",
                    &root,
                    &[STATUS_RUNNING],
                    origin,
                )
                .await
            );
            assert_eq!(
                engine.check_control_status("user-1", &child).await.unwrap(),
                expected_control,
                "unexpected descendant control for typed {case} ancestor"
            );
            assert_eq!(
                engine
                    .cancellation_origin_in_lineage("user-1", &child)
                    .await
                    .unwrap(),
                expected_settlement_origin,
                "Runtime/Unverified terminals must not cross lineage during settlement: {case}"
            );
            assert_eq!(
                engine.check_control_status("user-1", &root).await.unwrap(),
                Some(RunControlStatus::Cancelled),
                "a run's own terminal control remains direct regardless of origin"
            );
        }
    }

    #[tokio::test]
    async fn check_control_status_does_not_propagate_malformed_cancelled_ancestor() {
        let store = Arc::new(InMemoryRunStateStore::new());
        let engine = RunEngine::new(store.clone());
        engine
            .start_run("malformed-template", "user-1", "template-session")
            .await
            .unwrap();
        let mut malformed = engine
            .load_run("user-1", "malformed-template")
            .await
            .unwrap()
            .unwrap();
        malformed.run_id = "malformed-root".to_string();
        malformed.session_id = "session-1".to_string();
        malformed.status = STATUS_CANCELLED.to_string();
        malformed.root_run_id = Some("malformed-root".to_string());
        malformed.ancestor_path = Some("malformed-root".to_string());
        malformed.events.push(serde_json::json!({
            "event_type": "run_finished",
            "data": {
                "status": STATUS_CANCELLED,
                "cancelled": true
            }
        }));
        malformed.last_event_idx = malformed.events.len() as i64 - 1;
        store.insert_run(malformed).await.unwrap();
        engine
            .start_run_ext(
                "malformed-child",
                "user-1",
                "session-1",
                Some("malformed-root"),
                Some("delegation-1"),
                Some("worker"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            engine
                .check_control_status("user-1", "malformed-child")
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn cancellation_origin_query_fails_closed_on_malformed_or_missing_lineage() {
        let store = Arc::new(InMemoryRunStateStore::new());
        let engine = RunEngine::new(store.clone());
        engine
            .start_run("origin-root", "user-1", "session-1")
            .await
            .unwrap();
        engine
            .start_run_ext(
                "origin-child",
                "user-1",
                "session-1",
                Some("origin-root"),
                Some("delegation-1"),
                Some("worker"),
                None,
            )
            .await
            .unwrap();
        let template = engine
            .load_run("user-1", "origin-child")
            .await
            .unwrap()
            .unwrap();

        let mut malformed = template.clone();
        malformed.run_id = "malformed-child".to_string();
        malformed.status = STATUS_CANCELLED.to_string();
        malformed.ancestor_path = Some("origin-root/wrong-child".to_string());
        store.insert_run(malformed).await.unwrap();
        let malformed_error = engine
            .cancellation_origin_in_lineage("user-1", "malformed-child")
            .await
            .unwrap_err();
        assert!(malformed_error.contains("malformed ancestor_path"));

        let mut missing = template;
        missing.run_id = "missing-child".to_string();
        missing.status = STATUS_CANCELLED.to_string();
        missing.parent_run_id = Some("missing-parent".to_string());
        missing.root_run_id = Some("missing-parent".to_string());
        missing.ancestor_path = Some("missing-parent/missing-child".to_string());
        store.insert_run(missing).await.unwrap();
        let missing_error = engine
            .cancellation_origin_in_lineage("user-1", "missing-child")
            .await
            .unwrap_err();
        assert!(missing_error.contains("ancestor missing"));
    }

    #[tokio::test]
    async fn nonblocking_ancestor_pause_does_not_suspend_live_descendants() {
        let engine = test_engine();
        engine
            .start_run("root", "user-1", "session-1")
            .await
            .unwrap();
        engine
            .start_run_ext(
                "child",
                "user-1",
                "session-1",
                Some("root"),
                Some("delegation-1"),
                Some("worker"),
                None,
            )
            .await
            .unwrap();
        engine
            .persist_status("user-1", "session-1", "root", STATUS_PAUSED, None, None)
            .await
            .unwrap();

        assert_eq!(
            engine.check_control_status("user-1", "root").await.unwrap(),
            Some(RunControlStatus::Paused),
            "the paused run itself remains paused"
        );
        assert_eq!(
            engine
                .check_control_status("user-1", "child")
                .await
                .unwrap(),
            None,
            "budget/recovery pauses that release the session slot are not parent control commands"
        );
    }

    #[tokio::test]
    async fn run_control_poll_metrics_record_status_and_input_store_errors() {
        let registry = Arc::new(MetricsRegistry::new());
        let engine =
            RunEngine::new(Arc::new(FailingLoadRunStore)).with_metrics_registry(registry.clone());

        let status = engine.check_control_status("user-1", "run-input").await;
        assert_eq!(status.unwrap_err(), "load failed");
        let poll = engine.poll_user_intents("user-1", "run-input", 7).await;
        assert_eq!(poll.error.as_deref(), Some("load failed"));

        let rendered = registry.render_prometheus();
        assert!(
            rendered.contains(
                "astra_run_control_poll_attempts_total{operation=\"status\",outcome=\"error\"} 1"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "astra_run_control_poll_errors_total{class=\"store\",operation=\"status\"} 1"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "astra_run_control_poll_attempts_total{operation=\"user_intent_poll\",outcome=\"error\"} 1"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "astra_run_control_poll_errors_total{class=\"store\",operation=\"user_intent_poll\"} 1"
            ),
            "{rendered}"
        );
    }

    #[tokio::test]
    async fn run_control_poll_metrics_record_missing_and_ok_without_high_cardinality() {
        let registry = Arc::new(MetricsRegistry::new());
        let engine = test_engine().with_metrics_registry(registry.clone());

        let missing = engine.poll_user_intents("user-1", "missing-run", 3).await;
        assert!(missing.error.is_some());
        engine
            .start_run("run-input", "user-1", "sess-input")
            .await
            .unwrap();
        assert_eq!(
            engine
                .check_control_status("user-1", "run-input")
                .await
                .unwrap(),
            None
        );
        let ok = engine.poll_user_intents("user-1", "run-input", 0).await;
        assert_eq!(ok.error, None);

        let rendered = registry.render_prometheus();
        assert!(
            rendered.contains(
                "astra_run_control_poll_attempts_total{operation=\"user_intent_poll\",outcome=\"missing\"} 1"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "astra_run_control_poll_errors_total{class=\"missing\",operation=\"user_intent_poll\"} 1"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "astra_run_control_poll_attempts_total{operation=\"status\",outcome=\"ok\"} 1"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "astra_run_control_poll_attempts_total{operation=\"user_intent_poll\",outcome=\"ok\"} 1"
            ),
            "{rendered}"
        );
        assert!(
            !rendered.contains("user-1")
                && !rendered.contains("run-input")
                && !rendered.contains("missing-run"),
            "metrics must stay low-cardinality: {rendered}"
        );
    }

    #[tokio::test]
    async fn recovery_metrics_record_startup_actions_without_high_cardinality() {
        let registry = Arc::new(MetricsRegistry::new());
        let engine = test_engine().with_metrics_registry(registry.clone());
        engine
            .start_run("run-waiting-metric", "user-1", "sess-waiting")
            .await
            .unwrap();
        engine
            .persist_status(
                "user-1",
                "sess-waiting",
                "run-waiting-metric",
                STATUS_WAITING,
                Some("user_resume"),
                None,
            )
            .await
            .unwrap();
        engine
            .start_run("run-resume-metric", "user-1", "sess-resume")
            .await
            .unwrap();
        engine
            .persist_checkpoint(
                "user-1",
                "sess-resume",
                "run-resume-metric",
                r#"{"version":"checkpoint_v1","graceful":true,"last_batch_id":"shutdown-run-resume-metric"}"#,
            )
            .await
            .unwrap();
        engine
            .start_run("run-crash-metric", "user-1", "sess-crash")
            .await
            .unwrap();

        let recovered = engine.recover_active_runs().await.unwrap();
        assert_eq!(recovered.len(), 3);

        let rendered = registry.render_prometheus();
        assert!(
            rendered.contains("astra_run_recovery_scans_total{outcome=\"ok\"} 1"),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "astra_run_recovery_runs_total{action=\"session_continuation\",outcome=\"committed\"} 2"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "astra_run_recovery_runs_total{action=\"fail_crashed\",outcome=\"committed\"} 1"
            ),
            "{rendered}"
        );
        assert!(
            !rendered.contains("user-1")
                && !rendered.contains("run-waiting-metric")
                && !rendered.contains("run-resume-metric")
                && !rendered.contains("run-crash-metric"),
            "recovery metrics must stay low-cardinality: {rendered}"
        );
    }

    #[tokio::test]
    async fn recovery_metrics_record_scan_error() {
        let registry = Arc::new(MetricsRegistry::new());
        let engine =
            RunEngine::new(Arc::new(FailingLoadRunStore)).with_metrics_registry(registry.clone());

        let error = engine.recover_active_runs().await.unwrap_err();
        assert_eq!(error, "store unavailable");

        let rendered = registry.render_prometheus();
        assert!(
            rendered.contains("astra_run_recovery_scans_total{outcome=\"error\"} 1"),
            "{rendered}"
        );
        assert!(
            !rendered.contains("user-1") && !rendered.contains("run-"),
            "scan error metrics must stay low-cardinality: {rendered}"
        );
    }

    #[tokio::test]
    async fn poll_user_intents_keeps_cursor_when_after_index_exceeds_events() {
        let engine = test_engine();
        engine
            .start_run("run-input", "user-1", "sess-input")
            .await
            .unwrap();
        engine
            .append_event(
                "user-1",
                "sess-input",
                "run-input",
                serde_json::json!({
                    "event_type": "user_intent",
                    "data": {
                        "intent_id": "intent-ignored",
                        "delivery": "guide_current_run",
                        "input": { "content": "queued" }
                    },
                }),
            )
            .await
            .unwrap();

        let poll = engine.poll_user_intents("user-1", "run-input", 99).await;

        assert_eq!(poll.next_cursor, 99);
        assert!(poll.inputs.is_empty());
        assert_eq!(poll.error, None);
    }

    #[tokio::test]
    async fn poll_user_intents_preserves_submit_identity() {
        let engine = test_engine();
        engine
            .start_run("run-input-id", "user-1", "sess-input")
            .await
            .unwrap();
        engine
            .append_event(
                "user-1",
                "sess-input",
                "run-input-id",
                serde_json::json!({
                    "event_type": "user_intent",
                    "data": {
                        "intent_id": "intent-7",
                        "delivery": "guide_current_run",
                        "input": { "content": "queued" }
                    },
                }),
            )
            .await
            .unwrap();

        let poll = engine.poll_user_intents("user-1", "run-input-id", 0).await;

        assert_eq!(poll.inputs.len(), 1);
        assert_eq!(poll.inputs[0].intent_id, "intent-7");
        assert_eq!(
            poll.inputs[0].delivery,
            astra_turn_types::UserIntentDelivery::GuideCurrentRun
        );
        assert_eq!(
            poll.inputs[0].status,
            astra_turn_types::UserIntentStatus::AcceptedRemote
        );
        assert_eq!(poll.inputs[0].input["content"], "queued");
    }

    #[tokio::test]
    async fn poll_user_intents_replays_committed_apply_outbox_after_executor_crash() {
        let engine = test_engine();
        engine
            .start_run("run-apply-replay", "user-1", "sess-input")
            .await
            .unwrap();
        engine
            .append_event(
                "user-1",
                "sess-input",
                "run-apply-replay",
                serde_json::json!({
                    "event_type": "user_intent",
                    "data": {
                        "intent_id": "intent-replay",
                        "delivery": "guide_current_run",
                        "input": {
                            "content": "preserve structured context",
                            "astra_runtime_context": {"schema": "active_work_snapshot.v1"}
                        }
                    },
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            engine
                .mark_user_intents_applied(
                    "user-1",
                    "sess-input",
                    "run-apply-replay",
                    &[1],
                    UserIntentAdmissionAuthority::DurableOwnerGeneration(0),
                )
                .await
                .unwrap(),
            UserIntentApplyAck::Applied
        );

        let replay = engine
            .poll_user_intents("user-1", "run-apply-replay", 1)
            .await;
        assert_eq!(replay.inputs.len(), 1);
        assert_eq!(
            replay.inputs[0].status,
            astra_turn_types::UserIntentStatus::Applied
        );
        assert_eq!(replay.inputs[0].intent_id, "intent-replay");
        assert_eq!(
            replay.inputs[0].input["astra_runtime_context"]["schema"],
            "active_work_snapshot.v1"
        );
    }

    #[tokio::test]
    async fn poll_user_intents_isolates_poison_and_delivers_later_valid_event() {
        let engine = test_engine();
        engine
            .start_run("run-poison", "user-1", "sess-poison")
            .await
            .unwrap();
        engine
            .append_events_batch(
                "user-1",
                "sess-poison",
                "run-poison",
                &[
                    serde_json::json!({
                        "event_type": "user_intent",
                        "data": {
                            "intent_id": "intent-poison",
                            "delivery": "guide_current_run",
                            "input": {"unexpected": true}
                        }
                    }),
                    serde_json::json!({
                        "event_type": "user_intent",
                        "data": {
                            "intent_id": "intent-valid",
                            "delivery": "guide_current_run",
                            "input": {"content": "valid guidance"}
                        }
                    }),
                ],
            )
            .await
            .unwrap();

        let poll = engine.poll_user_intents("user-1", "run-poison", 0).await;

        assert_eq!(poll.error, None);
        assert_eq!(poll.next_cursor, 2);
        assert_eq!(poll.issues.len(), 1);
        assert_eq!(poll.issues[0].event_index, 1);
        assert_eq!(
            poll.issues[0].kind,
            UserIntentPollIssueKind::NoActionableContent
        );
        assert_eq!(poll.inputs.len(), 1);
        assert_eq!(poll.inputs[0].event_index, 2);
        assert_eq!(poll.inputs[0].intent_id, "intent-valid");
    }

    #[tokio::test]
    async fn poll_user_intents_skips_malformed_unrelated_tail_and_advances_cursor() {
        let engine = test_engine();
        engine
            .start_run("run-unrelated-tail", "user-1", "sess-unrelated-tail")
            .await
            .unwrap();
        engine
            .append_events_batch(
                "user-1",
                "sess-unrelated-tail",
                "run-unrelated-tail",
                &[
                    serde_json::json!({
                        "event_type": "user_intent",
                        "data": {
                            "intent_id": "intent-before-unrelated-tail",
                            "delivery": "guide_current_run",
                            "input": {"content": "deliver me"}
                        }
                    }),
                    serde_json::Value::String("malformed unrelated payload".to_string()),
                    serde_json::json!({
                        "event_type": "reasoning_delta",
                        "data": {"chunk": "unrelated tail"}
                    }),
                ],
            )
            .await
            .unwrap();

        let poll = engine
            .poll_user_intents("user-1", "run-unrelated-tail", 0)
            .await;
        assert_eq!(poll.error, None);
        assert!(poll.issues.is_empty());
        assert_eq!(poll.inputs.len(), 1);
        assert_eq!(poll.inputs[0].intent_id, "intent-before-unrelated-tail");
        assert_eq!(poll.next_cursor, 3);
    }

    #[tokio::test]
    async fn poll_user_intents_pages_without_losing_cross_page_disposition() {
        let engine = test_engine();
        engine
            .start_run("run-paged-intents", "user-1", "sess-paged-intents")
            .await
            .unwrap();
        let mut events = (0..USER_INTENT_CONTROL_DELTA_PAGE_SIZE)
            .map(|index| {
                let intent_id = if index + 1 == USER_INTENT_CONTROL_DELTA_PAGE_SIZE {
                    "intent-page-boundary".to_string()
                } else {
                    format!("intent-page-{index}")
                };
                serde_json::json!({
                    "event_type": "user_intent",
                    "idempotency_key": format!("user_intent:{intent_id}"),
                    "data": {
                        "intent_id": intent_id,
                        "delivery": "guide_current_run",
                        "input": {"content": format!("page input {index}")}
                    }
                })
            })
            .collect::<Vec<_>>();
        events.push(serde_json::json!({
            "event_type": "user_intent_applied",
            "idempotency_key": "user_intent_applied:intent-page-boundary",
            "data": {
                "intent_id": "intent-page-boundary",
                "delivery": "guide_current_run",
                "status": "applied",
                "event_index": USER_INTENT_CONTROL_DELTA_PAGE_SIZE,
                "content": "page input 255",
                "input": {"content": "page input 255"}
            }
        }));
        engine
            .append_events_batch("user-1", "sess-paged-intents", "run-paged-intents", &events)
            .await
            .unwrap();

        let first = engine
            .poll_user_intents("user-1", "run-paged-intents", 0)
            .await;
        assert_eq!(first.next_cursor, USER_INTENT_CONTROL_DELTA_PAGE_SIZE);
        assert!(first.snapshot_has_more);
        assert_eq!(
            first.snapshot_page_fact_count,
            USER_INTENT_CONTROL_DELTA_PAGE_SIZE
        );
        assert_eq!(first.inputs.len(), USER_INTENT_CONTROL_DELTA_PAGE_SIZE - 1);
        assert!(
            !first
                .inputs
                .iter()
                .any(|intent| intent.intent_id == "intent-page-boundary"),
            "lookahead must not emit AcceptedRemote for an already-applied source split by the page boundary"
        );

        let second = engine
            .poll_user_intents("user-1", "run-paged-intents", first.next_cursor)
            .await;
        assert_eq!(second.next_cursor, USER_INTENT_CONTROL_DELTA_PAGE_SIZE + 1);
        assert!(!second.snapshot_has_more);
        assert_eq!(second.snapshot_page_fact_count, 1);
        assert_eq!(second.inputs.len(), 1);
        assert_eq!(second.inputs[0].intent_id, "intent-page-boundary");
        assert_eq!(
            second.inputs[0].status,
            astra_turn_types::UserIntentStatus::Applied
        );
    }

    #[tokio::test]
    async fn user_intent_poll_and_apply_use_persisted_event_indices_with_gaps() {
        let engine = test_engine();
        engine
            .start_run("run-gapped", "user-1", "sess-gapped")
            .await
            .unwrap();
        engine
            .append_events_batch(
                "user-1",
                "sess-gapped",
                "run-gapped",
                &[
                    serde_json::json!({
                        "index": 7,
                        "event_type": "user_intent",
                        "data": {
                            "intent_id": "intent-7",
                            "delivery": "guide_current_run",
                            "input": {"content": "first"}
                        }
                    }),
                    serde_json::json!({
                        "index": 9,
                        "event_type": "user_intent",
                        "data": {
                            "intent_id": "intent-9",
                            "delivery": "guide_current_run",
                            "input": {"content": "second"}
                        }
                    }),
                ],
            )
            .await
            .unwrap();

        let first = engine.poll_user_intents("user-1", "run-gapped", 0).await;
        assert_eq!(first.next_cursor, 9);
        assert_eq!(
            first
                .inputs
                .iter()
                .map(|intent| intent.event_index)
                .collect::<Vec<_>>(),
            vec![7, 9]
        );

        engine
            .mark_user_intents_applied(
                "user-1",
                "sess-gapped",
                "run-gapped",
                &[9],
                UserIntentAdmissionAuthority::DurableOwnerGeneration(0),
            )
            .await
            .unwrap();
        engine
            .mark_user_intents_applied(
                "user-1",
                "sess-gapped",
                "run-gapped",
                &[9],
                UserIntentAdmissionAuthority::DurableOwnerGeneration(0),
            )
            .await
            .unwrap();

        let replay = engine.poll_user_intents("user-1", "run-gapped", 0).await;
        assert_eq!(replay.inputs.len(), 2);
        assert!(replay.inputs.iter().any(|intent| {
            intent.event_index == 7
                && intent.status == astra_turn_types::UserIntentStatus::AcceptedRemote
        }));
        assert!(replay.inputs.iter().any(|intent| {
            intent.event_index == 9 && intent.status == astra_turn_types::UserIntentStatus::Applied
        }));
        let run = engine
            .load_run("user-1", "run-gapped")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            run.events
                .iter()
                .filter(|event| {
                    event.get("event_type").and_then(serde_json::Value::as_str)
                        == Some("user_intent_applied")
                })
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn poll_user_intents_reports_store_load_errors() {
        let engine = RunEngine::new(Arc::new(FailingLoadRunStore));

        let poll = engine.poll_user_intents("user-1", "run-input", 7).await;

        assert_eq!(poll.next_cursor, 7);
        assert!(poll.inputs.is_empty());
        assert_eq!(poll.error.as_deref(), Some("load failed"));
    }

    #[tokio::test]
    async fn poll_user_intents_reports_missing_run_as_error() {
        let engine = test_engine();

        let poll = engine.poll_user_intents("user-1", "missing-run", 3).await;

        assert_eq!(poll.next_cursor, 3);
        assert!(poll.inputs.is_empty());
        assert!(poll.error.is_some());
    }

    #[tokio::test]
    async fn mark_user_intents_applied_preserves_execution_state_and_records_identity() {
        let engine = test_engine();
        engine
            .start_run("run-queued", "user-1", "sess-queued")
            .await
            .unwrap();
        engine
            .append_event(
                "user-1",
                "sess-queued",
                "run-queued",
                serde_json::json!({
                    "event_type": "user_intent",
                    "data": {
                        "intent_id": "intent-1",
                        "delivery": "guide_current_run",
                        "input": {"content": "focus the failing test"}
                    }
                }),
            )
            .await
            .unwrap();
        engine
            .persist_status(
                "user-1",
                "sess-queued",
                "run-queued",
                STATUS_WAITING,
                Some("edge_executor"),
                None,
            )
            .await
            .unwrap();

        let ack = engine
            .mark_user_intents_applied(
                "user-1",
                "sess-queued",
                "run-queued",
                &[1],
                UserIntentAdmissionAuthority::DurableOwnerGeneration(0),
            )
            .await
            .unwrap();
        assert_eq!(ack, UserIntentApplyAck::Applied);

        let run = engine
            .load_run("user-1", "run-queued")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.status, STATUS_WAITING);
        assert_eq!(run.waiting_for.as_deref(), Some("edge_executor"));
        assert_eq!(
            run.events
                .iter()
                .filter(
                    |event| event.get("event_type").and_then(serde_json::Value::as_str)
                        == Some("user_intent_applied")
                )
                .count(),
            1
        );
        let applied = run.events.last().unwrap();
        assert_eq!(applied["data"]["intent_id"], "intent-1");
        assert_eq!(applied["data"]["delivery"], "guide_current_run");
        assert_eq!(applied["data"]["status"], "applied");
        let poll = engine.poll_user_intents("user-1", "run-queued", 0).await;
        assert_eq!(poll.inputs.len(), 1);
        assert_eq!(poll.inputs[0].intent_id, "intent-1");
        assert_eq!(
            poll.inputs[0].status,
            astra_turn_types::UserIntentStatus::Applied,
            "the durable apply outbox must repair an executor crash before checkpoint"
        );
    }

    #[tokio::test]
    async fn fenced_pre_cutoff_intent_is_polled_applied_and_reopened() {
        let engine = test_engine();
        engine
            .start_run("run-fence-drain", "user-1", "sess-fence-drain")
            .await
            .unwrap();
        let intent = serde_json::json!({
            "event_type": "user_intent",
            "idempotency_key": "user_intent:intent-before-fence",
            "data": {
                "intent_id": "intent-before-fence",
                "delivery": "guide_current_run",
                "input": {"content": "apply before settling"}
            }
        });
        assert_eq!(
            engine
                .admit_run_guidance(AtomicRunGuidanceAdmissionRequest {
                    user_id: "user-1",
                    expected_session_id: "sess-fence-drain",
                    run_id: "run-fence-drain",
                    intent_id: "intent-before-fence",
                    event: &intent,
                    process_local_execution_live: true,
                })
                .await
                .unwrap(),
            AtomicRunGuidanceAdmission::Committed { event_index: 1 }
        );

        engine
            .fence_user_intent_submissions(
                "user-1",
                "sess-fence-drain",
                "run-fence-drain",
                UserIntentAdmissionAuthority::DurableOwnerGeneration(0),
            )
            .await
            .unwrap();
        let poll = engine
            .poll_user_intents("user-1", "run-fence-drain", 0)
            .await;
        assert_eq!(
            poll.inputs
                .iter()
                .map(|intent| intent.event_index)
                .collect::<Vec<_>>(),
            vec![1]
        );
        assert_eq!(
            engine
                .mark_user_intents_applied(
                    "user-1",
                    "sess-fence-drain",
                    "run-fence-drain",
                    &[1],
                    UserIntentAdmissionAuthority::DurableOwnerGeneration(0),
                )
                .await
                .unwrap(),
            UserIntentApplyAck::Applied
        );
        engine
            .reopen_user_intent_submissions(
                "user-1",
                "sess-fence-drain",
                "run-fence-drain",
                UserIntentAdmissionAuthority::DurableOwnerGeneration(0),
            )
            .await
            .unwrap();

        let after_reopen = serde_json::json!({
            "event_type": "user_intent",
            "idempotency_key": "user_intent:intent-after-reopen",
            "data": {
                "intent_id": "intent-after-reopen",
                "delivery": "guide_current_run",
                "input": {"content": "continue"}
            }
        });
        assert!(matches!(
            engine
                .admit_run_guidance(AtomicRunGuidanceAdmissionRequest {
                    user_id: "user-1",
                    expected_session_id: "sess-fence-drain",
                    run_id: "run-fence-drain",
                    intent_id: "intent-after-reopen",
                    event: &after_reopen,
                    process_local_execution_live: true,
                })
                .await
                .unwrap(),
            AtomicRunGuidanceAdmission::Committed { .. }
        ));

        let run = engine
            .load_run("user-1", "run-fence-drain")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            run.events
                .iter()
                .map(|event| event["event_type"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec![
                "run_started",
                "user_intent",
                "user_intent_settlement_fenced",
                "user_intent_applied",
                "user_intent_admission_reopened",
                "user_intent",
            ]
        );
        assert_eq!(run.events[2]["data"]["after_event_index"], 1);
    }

    #[tokio::test]
    async fn durable_user_intent_apply_rejects_process_local_authority() {
        let engine = test_engine();
        engine
            .start_run("run-apply-wrong-authority", "user-1", "sess-apply")
            .await
            .unwrap();
        engine
            .append_event(
                "user-1",
                "sess-apply",
                "run-apply-wrong-authority",
                serde_json::json!({
                    "event_type": "user_intent",
                    "idempotency_key": "user_intent:intent-wrong-authority",
                    "data": {
                        "intent_id": "intent-wrong-authority",
                        "delivery": "guide_current_run",
                        "input": {"content": "do not apply"}
                    }
                }),
            )
            .await
            .unwrap();

        let empty_error = engine
            .mark_user_intents_applied(
                "user-1",
                "sess-apply",
                "run-apply-wrong-authority",
                &[],
                UserIntentAdmissionAuthority::ProcessLocal,
            )
            .await
            .expect_err("an empty durable apply must still validate its authority type");
        assert!(empty_error.contains("exact owner generation"));

        let error = engine
            .mark_user_intents_applied(
                "user-1",
                "sess-apply",
                "run-apply-wrong-authority",
                &[1],
                UserIntentAdmissionAuthority::ProcessLocal,
            )
            .await
            .expect_err("durable provider must require an exact generation");

        assert!(error.contains("exact owner generation"));
        let run = engine
            .load_run("user-1", "run-apply-wrong-authority")
            .await
            .unwrap()
            .unwrap();
        assert!(!run.events.iter().any(|event| {
            event.get("event_type").and_then(serde_json::Value::as_str)
                == Some("user_intent_applied")
        }));
    }

    #[tokio::test]
    async fn mark_user_intents_applied_fails_closed_after_execution_is_paused() {
        let engine = test_engine();
        engine
            .start_run("run-paused-release", "user-1", "sess-paused")
            .await
            .unwrap();
        engine
            .append_event(
                "user-1",
                "sess-paused",
                "run-paused-release",
                serde_json::json!({
                    "event_type": "user_intent",
                    "data": {
                        "intent_id": "intent-paused",
                        "delivery": "guide_current_run",
                        "input": {"content": "use the focused test"}
                    }
                }),
            )
            .await
            .unwrap();
        engine
            .persist_status(
                "user-1",
                "sess-paused",
                "run-paused-release",
                STATUS_PAUSED,
                Some("user_resume"),
                None,
            )
            .await
            .unwrap();

        let error = engine
            .mark_user_intents_applied(
                "user-1",
                "sess-paused",
                "run-paused-release",
                &[1],
                UserIntentAdmissionAuthority::DurableOwnerGeneration(0),
            )
            .await
            .expect_err("paused execution must not append an applied fact");
        assert!(error.contains("paused"));

        let run = engine
            .load_run("user-1", "run-paused-release")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.status, STATUS_PAUSED);
        assert_eq!(run.waiting_for.as_deref(), Some("user_resume"));
        assert!(
            !run.events.iter().any(|event| {
                event.get("event_type").and_then(serde_json::Value::as_str)
                    == Some("user_intent_applied")
            }),
            "paused execution must leave the accepted intent unmodified"
        );
    }

    #[tokio::test]
    async fn mark_user_intents_applied_returns_ownership_on_cancelled_run_idempotently() {
        let engine = test_engine();
        engine
            .start_run("run-cancelled-release", "user-1", "sess-cancelled")
            .await
            .unwrap();
        engine
            .append_event(
                "user-1",
                "sess-cancelled",
                "run-cancelled-release",
                serde_json::json!({
                    "event_type": "user_intent",
                    "data": {
                        "intent_id": "intent-cancelled",
                        "delivery": "guide_current_run",
                        "input": {"content": "too late"}
                    }
                }),
            )
            .await
            .unwrap();
        assert!(
            transition_typed_cancellation(
                &engine,
                "user-1",
                "sess-cancelled",
                "run-cancelled-release",
                &[STATUS_RUNNING],
                astra_turn_core::orchestration_types::CancellationOrigin::User,
            )
            .await
        );
        let before = engine
            .load_run("user-1", "run-cancelled-release")
            .await
            .unwrap()
            .unwrap()
            .events
            .len();

        let ack = engine
            .mark_user_intents_applied(
                "user-1",
                "sess-cancelled",
                "run-cancelled-release",
                &[1],
                UserIntentAdmissionAuthority::DurableOwnerGeneration(0),
            )
            .await
            .unwrap();
        assert_eq!(ack, UserIntentApplyAck::RunTerminalReturned);

        let run = engine
            .load_run("user-1", "run-cancelled-release")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.status, STATUS_CANCELLED);
        assert_eq!(run.events.len(), before);
        let returned = run
            .events
            .iter()
            .find(|event| {
                event.get("event_type").and_then(serde_json::Value::as_str)
                    == Some("user_intent_returned")
            })
            .expect("terminal disposition must be durable");
        assert_eq!(returned["data"]["status"], "returned");
        assert_eq!(returned["data"]["content"], "too late");

        let retry = engine
            .mark_user_intents_applied(
                "user-1",
                "sess-cancelled",
                "run-cancelled-release",
                &[1],
                UserIntentAdmissionAuthority::DurableOwnerGeneration(0),
            )
            .await
            .unwrap();
        assert_eq!(retry, UserIntentApplyAck::RunTerminalReturned);
        let after_retry = engine
            .load_run("user-1", "run-cancelled-release")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after_retry.events.len(), before);
    }

    #[tokio::test]
    async fn mark_user_intents_applied_loses_cleanly_to_concurrent_terminal_transition() {
        let store = Arc::new(FlakyBatchTransitionStore::new(
            1,
            BatchTransitionFailureMode::ConcurrentCancelWins,
        ));
        let engine = RunEngine::new(store.clone());
        engine
            .start_run("run-apply-race", "user-1", "sess-apply-race")
            .await
            .unwrap();
        engine
            .append_event(
                "user-1",
                "sess-apply-race",
                "run-apply-race",
                serde_json::json!({
                    "event_type": "user_intent",
                    "idempotency_key": "user_intent:intent-race",
                    "data": {
                        "intent_id": "intent-race",
                        "delivery": "guide_current_run",
                        "input": {"content": "too late"}
                    }
                }),
            )
            .await
            .unwrap();

        let ack = engine
            .mark_user_intents_applied(
                "user-1",
                "sess-apply-race",
                "run-apply-race",
                &[1],
                UserIntentAdmissionAuthority::DurableOwnerGeneration(0),
            )
            .await
            .unwrap();
        assert_eq!(ack, UserIntentApplyAck::RunTerminalReturned);

        let run = engine
            .load_run("user-1", "run-apply-race")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.status, STATUS_CANCELLED);
        assert!(!run.events.iter().any(|event| {
            event.get("event_type").and_then(serde_json::Value::as_str)
                == Some("user_intent_applied")
        }));
        assert!(run.events.iter().any(|event| {
            event.get("event_type").and_then(serde_json::Value::as_str)
                == Some("user_intent_returned")
        }));
        assert_eq!(store.attempts(), 1);
    }

    #[tokio::test]
    async fn mark_user_intents_applied_reconciles_commit_then_timeout_idempotently() {
        let store = Arc::new(FlakyBatchTransitionStore::new(
            1,
            BatchTransitionFailureMode::FailAfterStoreWrite,
        ));
        let engine = RunEngine::new(store.clone());
        engine
            .start_run("run-apply-timeout", "user-1", "sess-apply-timeout")
            .await
            .unwrap();
        engine
            .append_event(
                "user-1",
                "sess-apply-timeout",
                "run-apply-timeout",
                serde_json::json!({
                    "event_type": "user_intent",
                    "idempotency_key": "user_intent:intent-timeout",
                    "data": {
                        "intent_id": "intent-timeout",
                        "delivery": "guide_current_run",
                        "input": {"content": "apply once"}
                    }
                }),
            )
            .await
            .unwrap();

        engine
            .mark_user_intents_applied(
                "user-1",
                "sess-apply-timeout",
                "run-apply-timeout",
                &[1],
                UserIntentAdmissionAuthority::DurableOwnerGeneration(0),
            )
            .await
            .unwrap();
        engine
            .mark_user_intents_applied(
                "user-1",
                "sess-apply-timeout",
                "run-apply-timeout",
                &[1],
                UserIntentAdmissionAuthority::DurableOwnerGeneration(0),
            )
            .await
            .unwrap();

        let run = engine
            .load_run("user-1", "run-apply-timeout")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            run.events
                .iter()
                .filter(|event| {
                    event.get("event_type").and_then(serde_json::Value::as_str)
                        == Some("user_intent_applied")
                })
                .count(),
            1
        );
        assert_eq!(
            store.attempts(),
            2,
            "an explicit idempotent retry performs one bounded store lookup without appending again"
        );
    }

    #[tokio::test]
    async fn persist_status_if_current_does_not_overwrite_unexpected_status() {
        let engine = test_engine();
        engine
            .start_run("run-cas", "user-1", "sess-cas")
            .await
            .unwrap();
        engine
            .persist_status(
                "user-1",
                "sess-cas",
                "run-cas",
                STATUS_PAUSED,
                Some("user_resume"),
                None,
            )
            .await
            .unwrap();

        let updated = engine
            .persist_status_if_current(RunStatusCasRequest {
                user_id: "user-1",
                expected_session_id: "sess-cas",
                run_id: "run-cas",
                expected_statuses: &[STATUS_RUNNING],
                status: STATUS_RUNNING,
                waiting_for: None,
                error_message: None,
            })
            .await
            .unwrap();

        let run = engine.load_run("user-1", "run-cas").await.unwrap().unwrap();
        assert!(!updated);
        assert_eq!(run.status, STATUS_PAUSED);
        assert_eq!(run.waiting_for.as_deref(), Some("user_resume"));
    }

    #[tokio::test]
    async fn missing_run_does_not_report_cancelled_control_status() {
        let engine = test_engine();
        assert_eq!(
            engine
                .check_control_status("user-1", "missing-run")
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn list_user_runs_cursor_pagination() {
        let engine = test_engine();
        for i in 0..5 {
            engine
                .start_run(&format!("run-{i}"), "user-1", &format!("sess-{i}"))
                .await
                .unwrap();
        }
        engine
            .start_run("run-other", "user-2", "sess-other")
            .await
            .unwrap();
        let first = engine
            .list_user_runs_cursor("user-1", 2, None)
            .await
            .unwrap();
        let runs = first.runs;
        assert_eq!(runs.len(), 2);
        assert!(first.next_cursor.is_some());
        assert_eq!(first.total, None);
        let runs2 = engine
            .list_user_runs_cursor("user-1", 10, first.next_cursor)
            .await
            .unwrap()
            .runs;
        assert_eq!(runs2.len(), 3);
    }

    #[tokio::test]
    async fn recover_active_runs_releases_orphaned_waiting_for_session_continuation() {
        let engine = test_engine();
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();
        engine.start_run("run-2", "user-1", "sess-2").await.unwrap();
        engine
            .persist_status(
                "user-1",
                "sess-1",
                "run-1",
                "waiting",
                Some("user_resume"),
                None,
            )
            .await
            .unwrap();
        engine
            .persist_status("user-1", "sess-2", "run-2", "completed", None, None)
            .await
            .unwrap();
        let active = engine.recover_active_runs().await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].run_id, "run-1");
        assert_eq!(active[0].status, STATUS_PAUSED);
        assert!(active[0].waiting_for.is_none());
        assert_eq!(
            active[0].events.last().unwrap()["data"]["resume_strategy"],
            "session_continuation"
        );
        engine
            .start_run("run-continuation", "user-1", "sess-1")
            .await
            .expect("startup recovery must release the session execution slot");
    }

    #[tokio::test]
    async fn recover_active_runtime_orphan_uses_crash_semantics_without_user_marker() {
        let engine = test_engine();
        engine
            .start_run("run-runtime-orphan", "user-1", "sess-runtime-orphan")
            .await
            .unwrap();

        let recovered = engine.recover_active_runs().await.unwrap();

        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].status, STATUS_FAILED);
        let terminal = recovered[0].events.last().unwrap();
        assert_eq!(terminal["data"]["source"], "crash_recovery");
        assert_eq!(terminal["data"]["error_code"], "crash_recovery");
        assert!(terminal["data"].get("cancellation_origin").is_none());
    }

    #[tokio::test]
    async fn recover_active_child_persists_ancestor_user_cancellation_origin() {
        let engine = test_engine();
        engine
            .start_run("recovery-root", "user-1", "session-1")
            .await
            .unwrap();
        engine
            .start_run_ext(
                "recovery-child",
                "user-1",
                "session-1",
                Some("recovery-root"),
                Some("delegation-1"),
                Some("worker"),
                None,
            )
            .await
            .unwrap();
        assert!(
            engine
                .request_run_cancellation("user-1", "recovery-root")
                .await
                .unwrap()
        );

        let recovered = engine.recover_active_runs().await.unwrap();
        let child = recovered
            .iter()
            .find(|run| run.run_id == "recovery-child")
            .expect("ancestor cancellation must recover child");
        assert_eq!(child.status, STATUS_CANCELLED);
        assert_eq!(
            child.events.last().unwrap()["data"]["cancellation_origin"],
            "user"
        );
    }

    #[tokio::test]
    async fn recover_active_runs_does_not_guess_when_cancellation_lookup_is_unavailable() {
        let store = Arc::new(
            FlakyBatchTransitionStore::new(0, BatchTransitionFailureMode::FailBeforeStoreWrite)
                .with_cancellation_lookup_failures(1),
        );
        let engine = RunEngine::new(store.clone());
        engine
            .start_run("run-uncertain-cancel", "user-1", "sess-uncertain-cancel")
            .await
            .unwrap();

        let recovered = engine.recover_active_runs().await.unwrap();

        assert!(recovered.is_empty());
        let durable = store
            .load_run("user-1", "run-uncertain-cancel")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(durable.status, STATUS_RUNNING);
        assert!(durable.events.iter().all(|event| {
            event
                .pointer("/data/error_code")
                .and_then(serde_json::Value::as_str)
                != Some("crash_recovery")
        }));

        let retried = engine.recover_active_runs().await.unwrap();
        assert_eq!(retried.len(), 1);
        assert_eq!(retried[0].status, STATUS_FAILED);
    }

    #[tokio::test]
    async fn recover_active_runs_drains_multiple_bounded_batches() {
        let engine = test_engine();
        let run_count = RUN_RECOVERY_CLAIM_BATCH_SIZE as usize + 9;
        for index in 0..run_count {
            engine
                .start_run(
                    &format!("run-recovery-batch-{index}"),
                    "user-recovery-batch",
                    &format!("session-recovery-batch-{index}"),
                )
                .await
                .unwrap();
        }

        let recovered = engine.recover_active_runs().await.unwrap();

        assert_eq!(recovered.len(), run_count);
        assert!(recovered.iter().all(|run| run.status == STATUS_FAILED));
        assert_eq!(
            recovered
                .iter()
                .map(|run| run.run_id.as_str())
                .collect::<HashSet<_>>()
                .len(),
            run_count,
            "a run must be classified at most once across recovery batches"
        );
    }

    #[tokio::test]
    async fn recover_active_runs_uses_bounded_store_claims() {
        let store = Arc::new(FlakyBatchTransitionStore::new(
            0,
            BatchTransitionFailureMode::FailBeforeStoreWrite,
        ));
        let engine = RunEngine::new(store.clone());
        engine
            .start_run("run-waiting", "user-1", "sess-waiting")
            .await
            .unwrap();
        engine
            .persist_status(
                "user-1",
                "sess-waiting",
                "run-waiting",
                STATUS_WAITING,
                Some("user_resume"),
                None,
            )
            .await
            .unwrap();
        engine
            .start_run("run-crashed", "user-1", "sess-crashed")
            .await
            .unwrap();

        let recovered = engine.recover_active_runs().await.unwrap();

        assert_eq!(
            store.recovery_claims(),
            2,
            "startup recovery must claim one work batch and then prove the queue is drained"
        );
        assert_eq!(
            store.waiting_queries(),
            0,
            "startup recovery must not separately query waiting runs outside the owner-scoped recovery path"
        );
        assert!(recovered.iter().any(|run| {
            run.run_id == "run-waiting" && run.status == STATUS_PAUSED && run.waiting_for.is_none()
        }));
        let crashed = engine
            .load_run("user-1", "run-crashed")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(crashed.status, STATUS_FAILED);
        assert_eq!(crashed.error_code.as_deref(), Some("crash_recovery"));
    }

    #[tokio::test]
    async fn recover_active_runs_releases_orphaned_blocking_pause() {
        let engine = test_engine();
        engine
            .start_run("run-paused", "user-1", "sess-paused")
            .await
            .unwrap();
        engine
            .persist_status(
                "user-1",
                "sess-paused",
                "run-paused",
                STATUS_PAUSED,
                Some("user_resume"),
                None,
            )
            .await
            .unwrap();

        let recovered = engine.recover_active_runs().await.unwrap();

        assert!(recovered.iter().any(|run| run.run_id == "run-paused"));
        let durable = engine
            .load_run("user-1", "run-paused")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(durable.status, STATUS_PAUSED);
        assert!(durable.waiting_for.is_none());
        assert!(durable.error_code.is_none());
        assert_eq!(
            durable.events.last().unwrap()["event_type"],
            "run_interrupted_after_restart"
        );
    }

    #[tokio::test]
    async fn recover_active_runs_releases_graceful_checkpoint_for_session_continuation() {
        let engine = test_engine();
        engine
            .start_run("run-resume", "user-1", "sess-resume")
            .await
            .unwrap();
        engine
            .persist_checkpoint(
                "user-1",
                "sess-resume",
                "run-resume",
                r#"{"version":"checkpoint_v1","graceful":true,"last_batch_id":"shutdown-run-resume"}"#,
            )
            .await
            .unwrap();

        let recovered = engine.recover_active_runs().await.unwrap();
        let resumed = recovered
            .into_iter()
            .find(|run| run.run_id == "run-resume")
            .expect("graceful checkpointed run should be recoverable");
        assert_eq!(resumed.status, STATUS_PAUSED);
        assert!(resumed.waiting_for.is_none());
        let durable = engine
            .load_run("user-1", "run-resume")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(durable.status, STATUS_PAUSED);
        assert!(durable.waiting_for.is_none());
        assert_eq!(
            durable.events.last().unwrap()["event_type"],
            "run_interrupted_after_restart"
        );
        assert_eq!(
            durable.events.last().unwrap()["data"]["checkpoint_available"],
            true
        );
    }

    #[tokio::test]
    async fn recover_active_runs_falls_back_to_embedded_legacy_checkpoint() {
        let store = Arc::new(InMemoryRunStateStore::new());
        let engine = RunEngine::new(store.clone());
        let now = chrono::Utc::now().to_rfc3339();
        store
            .insert_run(DurableRunRecord {
                run_id: "run-legacy".to_string(),
                user_id: "user-1".to_string(),
                session_id: "sess-legacy".to_string(),
                parent_run_id: None,
                root_run_id: Some("run-legacy".to_string()),
                ancestor_path: Some("run-legacy".to_string()),
                depth: 0,
                delegation_id: None,
                agent_id: None,
                retry_of: None,
                retry_scope: Some("node".to_string()),
                status: STATUS_RUNNING.to_string(),
                waiting_for: None,
                owner_pod_id: None,
                owner_lease_expires_at: None,
                run_generation: 0,
                last_event_idx: 0,
                checkpoint_version: Some("checkpoint_v1".to_string()),
                checkpoint_json: Some(
                    r#"{"version":"checkpoint_v1","graceful":true,"last_batch_id":"legacy-batch"}"#
                        .to_string(),
                ),
                error_code: None,
                error_message: None,
                retry_count: 0,
                total_prompt_tokens: 0,
                total_completion_tokens: 0,
                total_tool_calls: 0,
                agent_binding_id: None,
                agent_binding_name: None,
                agent_binding_schema_version: None,
                model_offering_id: None,
                resolved_model_name: None,
                runtime_profile: None,
                start_request_fingerprint: None,
                work_binding: None,
                events: vec![serde_json::json!({"event_type":"run_started","data":{}})],
                created_at: now.clone(),
                updated_at: now,
            })
            .await
            .unwrap();

        let recovered = engine.recover_active_runs().await.unwrap();
        let resumed = recovered
            .into_iter()
            .find(|run| run.run_id == "run-legacy")
            .expect("legacy checkpointed run should still resume");
        assert_eq!(resumed.status, STATUS_PAUSED);
        assert!(resumed.waiting_for.is_none());
    }

    #[tokio::test]
    async fn full_lifecycle_start_pause_resume_complete() {
        let engine = test_engine();
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();

        // Simulate pause
        engine
            .persist_status(
                "user-1",
                "sess-1",
                "run-1",
                "paused",
                Some("user_resume"),
                None,
            )
            .await
            .unwrap();
        engine
            .append_event(
                "user-1",
                "sess-1",
                "run-1",
                serde_json::json!({"event_type": "run_paused"}),
            )
            .await
            .unwrap();

        // Simulate resume
        engine
            .persist_status("user-1", "sess-1", "run-1", "running", None, None)
            .await
            .unwrap();
        engine
            .append_event(
                "user-1",
                "sess-1",
                "run-1",
                serde_json::json!({"event_type": "run_resumed"}),
            )
            .await
            .unwrap();

        // Simulate completion
        engine
            .persist_usage("user-1", "sess-1", "run-1", 2000, 800, 12)
            .await
            .unwrap();
        engine
            .persist_checkpoint(
                "user-1",
                "sess-1",
                "run-1",
                r#"{"phase":"final","final":true}"#,
            )
            .await
            .unwrap();
        engine
            .persist_status("user-1", "sess-1", "run-1", "completed", None, None)
            .await
            .unwrap();
        engine
            .append_event(
                "user-1",
                "sess-1",
                "run-1",
                serde_json::json!({"event_type": "run_finished", "data": {}}),
            )
            .await
            .unwrap();

        let run = engine.load_run("user-1", "run-1").await.unwrap().unwrap();
        assert_eq!(run.status, "completed");
        assert_eq!(run.total_prompt_tokens, 2000);
        assert_eq!(run.total_completion_tokens, 800);
        assert_eq!(run.total_tool_calls, 12);
        let checkpoint = engine
            .load_latest_checkpoint("user-1", "run-1", Some("phase"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(checkpoint.checkpoint_kind, "phase");
        assert_eq!(
            checkpoint.checkpoint_json,
            r#"{"phase":"final","final":true}"#
        );
        // run_started + run_paused + run_resumed + run_finished = 4
        assert_eq!(run.events.len(), 4);
        assert!(run.waiting_for.is_none());
    }

    #[tokio::test]
    async fn error_message_persists() {
        let engine = test_engine();
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();
        engine
            .persist_status(
                "user-1",
                "sess-1",
                "run-1",
                "failed",
                None,
                Some("OOM killed"),
            )
            .await
            .unwrap();
        let run = engine.load_run("user-1", "run-1").await.unwrap().unwrap();
        assert_eq!(run.status, "failed");
        assert_eq!(run.error_message.as_deref(), Some("OOM killed"));
    }

    /// P0-B: recover_active_runs must mark crashed running runs as failed.
    /// Simulates a process crash: run was in `running` state when the server died.
    #[tokio::test]
    async fn recover_active_runs_marks_crashed_running_as_failed() {
        let engine = test_engine();

        // Insert a run that was running when the process crashed
        engine
            .start_run("run-crash", "user-1", "sess-1")
            .await
            .unwrap();
        engine
            .persist_status("user-1", "sess-1", "run-crash", "running", None, None)
            .await
            .unwrap();
        engine
            .append_event(
                "user-1",
                "sess-1",
                "run-crash",
                serde_json::json!({
                    "event_type": "user_intent",
                    "idempotency_key": "user_intent:orphaned-guidance",
                    "data": {
                        "intent_id": "orphaned-guidance",
                        "delivery": "guide_current_run",
                        "input": {"content": "do not strand this"}
                    }
                }),
            )
            .await
            .unwrap();

        // Insert an orphaned waiting run. With no surviving consumer it must
        // release the session for a continuation run.
        engine
            .start_run("run-wait", "user-1", "sess-2")
            .await
            .unwrap();
        engine
            .persist_status("user-1", "sess-2", "run-wait", "waiting", None, None)
            .await
            .unwrap();
        engine
            .append_event(
                "user-1",
                "sess-2",
                "run-wait",
                serde_json::json!({
                    "event_type": "user_intent",
                    "idempotency_key": "user_intent:waiting-guidance",
                    "data": {
                        "intent_id": "waiting-guidance",
                        "delivery": "guide_current_run",
                        "input": {"content": "return on executor handoff"}
                    }
                }),
            )
            .await
            .unwrap();

        let recovered = engine.recover_active_runs().await.unwrap();

        // Both runs returned
        assert_eq!(
            recovered.len(),
            2,
            "both waiting and crashed-running returned"
        );

        // The crashed running run must now be marked failed in the store
        let crashed = engine
            .load_run("user-1", "run-crash")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            crashed.status, "failed",
            "crashed running run must be marked failed"
        );
        assert_eq!(
            crashed.error_message.as_deref(),
            Some("recovered from crash"),
            "error message must indicate crash recovery"
        );
        assert_eq!(crashed.error_code.as_deref(), Some("crash_recovery"));
        let crashed_event_types = crashed
            .events
            .iter()
            .filter_map(|event| event.get("event_type").and_then(serde_json::Value::as_str))
            .collect::<Vec<_>>();
        assert!(
            crashed_event_types.ends_with(&["user_intent_returned", "run_error", "run_finished"]),
            "crash recovery must return input ownership before its terminal event pair"
        );
        let returned = &crashed.events[crashed.events.len() - 3];
        let run_error = &crashed.events[crashed.events.len() - 2];
        let run_finished = crashed.events.last().unwrap();
        assert_eq!(run_error["data"]["error_code"], "crash_recovery");
        assert_eq!(
            run_finished["data"]["status"], "failed",
            "crash recovery run_finished must be self-describing across replay boundaries"
        );
        assert_eq!(run_finished["data"]["error_code"], "crash_recovery");
        assert_eq!(returned["data"]["intent_id"], "orphaned-guidance");
        assert_eq!(returned["data"]["status"], "returned");

        // The orphaned waiting run becomes a non-blocking paused continuation.
        let waiting = engine
            .load_run("user-1", "run-wait")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(waiting.status, STATUS_PAUSED);
        assert!(waiting.waiting_for.is_none());
        let waiting_types = waiting
            .events
            .iter()
            .filter_map(|event| event.get("event_type").and_then(serde_json::Value::as_str))
            .collect::<Vec<_>>();
        assert!(
            waiting_types.ends_with(&["user_intent_returned", "run_interrupted_after_restart"]),
            "executor handoff must return guidance before advertising session continuation: {waiting_types:?}"
        );
        assert_eq!(
            waiting.events.last().unwrap()["data"]["resume_strategy"],
            "session_continuation"
        );
    }

    #[tokio::test]
    async fn recover_active_runs_cas_miss_does_not_overwrite_completed() {
        let engine = test_engine();
        engine
            .start_run("run-race", "user-1", "sess-race")
            .await
            .unwrap();
        let stale_running = engine
            .load_run("user-1", "run-race")
            .await
            .unwrap()
            .unwrap();
        engine
            .persist_status(
                "user-1",
                "sess-race",
                "run-race",
                astra_core::STATUS_COMPLETED,
                None,
                None,
            )
            .await
            .unwrap();

        let recovered = engine.recover_active_run(stale_running).await;

        assert!(
            recovered.is_none(),
            "a CAS-missed completed run must not be reported as recovered"
        );
        let durable = engine
            .load_run("user-1", "run-race")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(durable.status, astra_core::STATUS_COMPLETED);
        assert!(durable.error_message.is_none());
        assert!(durable.error_code.is_none());
        assert!(
            durable.events.iter().all(|event| !matches!(
                event.get("event_type").and_then(serde_json::Value::as_str),
                Some("run_error" | "run_finished")
            )),
            "stale recovery must not append crash-recovery terminal events"
        );
    }
}
