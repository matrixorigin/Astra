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

use std::{collections::HashSet, sync::Arc, time::Duration};

use astra_services::{
    DatabaseStateProjectionStore,
    runs::{
        CapabilityServerRefs, DurableRunCheckpointRecord, DurableRunDisplayProjectionRecord,
        DurableRunInteractionKind, DurableRunInteractionResolveOutcome, DurableRunListPage,
        DurableRunRecord, DurableRunStatusKind, GuardedRunStatusTransition,
        GuardedRunStatusTransitionRequest, RUN_RECOVERY_CLAIM_BATCH_SIZE,
        RequestedTurnInteractionMode, RunListCursor, RunStateStore, RuntimeProfileRequest,
        SelectedModelRequest, TurnIntentExecutionPolicy, durable_run_status_kind,
    },
};
use astra_turn_core::pipeline_metrics::MetricsRegistry;

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
const USER_INTENT_APPLY_MAX_ATTEMPTS: usize = 8;
const USER_INTENT_APPLY_RETRY_BASE_DELAY_MS: u64 = 5;
const USER_INTENT_APPLY_RETRY_MAX_DELAY_MS: u64 = 80;
const RUN_RECOVERY_MAX_CONCURRENCY: usize = 8;
const OWNER_LEASE_RENEWAL_STATUSES: &[&str] = &[STATUS_RUNNING, STATUS_WAITING, STATUS_PAUSED];

#[derive(Clone, Debug, PartialEq)]
pub enum TerminalTransitionOutcome {
    Committed,
    /// Another durable transition won the CAS. The enclosed record is the
    /// authority callers must project instead of their stale local outcome.
    Superseded(Box<DurableRunRecord>),
}

fn crash_recovery_terminal_events() -> [serde_json::Value; 2] {
    [
        serde_json::json!({
            "event_type": "run_error",
            "data": {
                "error": "recovered from crash",
                "error_code": "crash_recovery",
                "error_kind": "crash_recovery",
            },
        }),
        serde_json::json!({
            "event_type": "run_finished",
            "data": {
                "status": STATUS_FAILED,
                "error": "recovered from crash",
                "error_code": "crash_recovery",
                "error_kind": "crash_recovery",
            },
        }),
    ]
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

/// Optional run-start interaction context persisted into the durable
/// `run_started` event so replay/status surfaces can explain policy decisions.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunStartContext {
    pub interaction_mode: Option<RequestedTurnInteractionMode>,
    pub interactive_client: Option<bool>,
    pub turn_intent_policy: TurnIntentExecutionPolicy,
    pub execution_metadata: Option<serde_json::Map<String, serde_json::Value>>,
    pub agent_binding_id: Option<String>,
    pub agent_binding_name: Option<String>,
    pub agent_binding_schema_version: Option<String>,
    pub selected_model: Option<SelectedModelRequest>,
    pub capability_server_refs: Option<CapabilityServerRefs>,
    pub runtime_profile: Option<RuntimeProfileRequest>,
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

fn effective_mode_label(context: &RunStartContext) -> Option<&'static str> {
    if let Some(mode) = context.interaction_mode {
        return Some(requested_mode_label(mode));
    }
    context
        .interactive_client
        .map(|interactive| if interactive { "prompt" } else { "headless" })
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

fn user_intent_apply_retry_delay(attempt: usize) -> Duration {
    let exponent = u32::try_from(attempt.saturating_sub(1)).unwrap_or(u32::MAX);
    let multiplier = 1_u64.checked_shl(exponent).unwrap_or(u64::MAX);
    Duration::from_millis(
        USER_INTENT_APPLY_RETRY_BASE_DELAY_MS
            .saturating_mul(multiplier)
            .min(USER_INTENT_APPLY_RETRY_MAX_DELAY_MS),
    )
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
    if let Some(mode_label) = effective_mode_label(context) {
        data.insert(
            "interaction_mode".to_string(),
            serde_json::Value::String(mode_label.to_string()),
        );
        data.insert(
            "suppressed_loop_nudges".to_string(),
            serde_json::Value::Bool(mode_label == "auto"),
        );
    }
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
    if let Some(selected_model) = context.selected_model.as_ref()
        && let Ok(value) = serde_json::to_value(selected_model)
    {
        data.insert("selected_model".to_string(), value);
    }
    if let Some(capability_server_refs) = context.capability_server_refs.as_ref()
        && let Ok(value) = serde_json::to_value(capability_server_refs)
    {
        data.insert("capability_server_refs".to_string(), value);
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

    /// Start renewing the store owner's active-run lease until the returned
    /// guard is dropped. Stores without shared owner leases return `None`.
    pub(crate) fn start_owner_lease_heartbeat(
        &self,
        user_id: String,
        run_id: String,
    ) -> Option<RunOwnerLeaseHeartbeat> {
        let interval = self
            .store
            .owner_lease_renewal_interval()?
            .max(Duration::from_millis(1));
        let engine = self.clone();
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();
        let join = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(interval) => {}
                    _ = &mut stop_rx => break,
                }

                match engine
                    .store
                    .renew_owner_lease(&user_id, &run_id, OWNER_LEASE_RENEWAL_STATUSES)
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => {
                        tracing::debug!(
                            target: "astra_runtime::run_engine",
                            run_id = %run_id,
                            "stopping run owner lease heartbeat after renewal returned false"
                        );
                        break;
                    }
                    Err(error) => {
                        tracing::warn!(
                            target: "astra_runtime::run_engine",
                            run_id = %run_id,
                            error = %error,
                            "failed to renew active run owner lease"
                        );
                    }
                }
            }

            if let Err(error) = engine.store.release_owner_lease(&user_id, &run_id).await {
                tracing::warn!(
                    target: "astra_runtime::run_engine",
                    run_id = %run_id,
                    error = %error,
                    "failed to release run owner lease after executor exit"
                );
            }
        });

        Some(RunOwnerLeaseHeartbeat {
            stop_tx: Some(stop_tx),
            _join: join,
        })
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
    ) -> Result<(), String> {
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
    ) -> Result<(), String> {
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
    ) -> Result<(), String> {
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

    /// Extended version of `start_run` with delegation metadata and interaction context.
    async fn start_run_ext_with_context(
        &self,
        run_id: &str,
        user_id: &str,
        session_id: &str,
        parent_run_id: Option<&str>,
        delegation_id: Option<&str>,
        agent_id: Option<&str>,
        retry_of: Option<&str>,
        context: RunStartContext,
    ) -> Result<(), String> {
        let now = chrono::Utc::now().to_rfc3339();
        let (root_run_id, ancestor_path, depth) = if let Some(parent_run_id) = parent_run_id {
            match self.store.load_run(user_id, parent_run_id).await? {
                Some(parent) => {
                    let parent_root = parent.root_run_id.unwrap_or(parent.run_id.clone());
                    let parent_path = parent.ancestor_path.unwrap_or(parent.run_id);
                    (
                        Some(parent_root),
                        Some(format!("{parent_path}/{run_id}")),
                        parent.depth.saturating_add(1),
                    )
                }
                None => (
                    Some(parent_run_id.to_string()),
                    Some(format!("{parent_run_id}/{run_id}")),
                    1,
                ),
            }
        } else {
            (Some(run_id.to_string()), Some(run_id.to_string()), 0)
        };
        let selected_model_json = context.selected_model.as_ref().and_then(|m| {
            serde_json::to_string(m)
                .inspect_err(|e| {
                    tracing::warn!(
                        target: "astra_runtime::engine",
                        run_id = %run_id,
                        error = %e,
                        "failed to serialize selected_model for durable run record"
                    );
                })
                .ok()
        });
        let selected_model_name = context
            .selected_model
            .as_ref()
            .map(|selected_model| selected_model.model.clone());
        let selected_model_gateway = context
            .selected_model
            .as_ref()
            .and_then(|selected_model| selected_model.gateway.clone());
        let capability_server_refs_json =
            context.capability_server_refs.as_ref().and_then(|refs| {
                serde_json::to_string(refs)
                    .inspect_err(|e| {
                        tracing::warn!(
                            target: "astra_runtime::engine",
                            run_id = %run_id,
                            error = %e,
                            "failed to serialize capability_server_refs for durable run record"
                        );
                    })
                    .ok()
            });
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
            selected_model_json,
            selected_model_name,
            selected_model_gateway,
            capability_server_refs_json,
            runtime_profile,
            events: vec![serde_json::json!({
                "event_type": "run_started",
                "data": run_started_data
            })],
            created_at: now.clone(),
            updated_at: now,
        };
        self.store.insert_run(record).await?;
        self.project_delegation_run_if_needed(user_id, run_id, None)
            .await?;
        Ok(())
    }

    /// Persist a status change to the durable store.
    pub async fn persist_status(
        &self,
        user_id: &str,
        run_id: &str,
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<bool, String> {
        let updated = self
            .store
            .update_run_status(user_id, run_id, status, waiting_for, error_message)
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

    /// Persist a status change only if the durable row is still in one of the
    /// expected states. This prevents stale control-plane observations from
    /// overwriting a newer pause/cancel/terminal status.
    pub async fn persist_status_if_current(
        &self,
        user_id: &str,
        run_id: &str,
        expected_statuses: &[&str],
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<bool, String> {
        let updated = self
            .store
            .update_run_status_if_current(
                user_id,
                run_id,
                expected_statuses,
                status,
                waiting_for,
                error_message,
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

    /// Persist an executor-produced delegation outcome without allowing a
    /// stale child completion to overwrite a concurrent pause or cancel.
    ///
    /// The durable run state is authoritative. Terminal outcomes are committed
    /// with their replay events in the same CAS; a lost CAS is reported as
    /// `Ok(false)` and the winning durable state remains untouched.
    pub async fn persist_delegation_outcome_status(
        &self,
        user_id: &str,
        run_id: &str,
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<bool, String> {
        let (canonical_status, expected_statuses, terminal) = match durable_run_status_kind(status)
        {
            DurableRunStatusKind::Running => (STATUS_RUNNING, vec![STATUS_RUNNING], false),
            DurableRunStatusKind::Waiting => {
                (STATUS_WAITING, vec![STATUS_RUNNING, STATUS_WAITING], false)
            }
            DurableRunStatusKind::Paused => {
                (STATUS_PAUSED, vec![STATUS_RUNNING, STATUS_PAUSED], false)
            }
            DurableRunStatusKind::Completed => {
                (STATUS_COMPLETED, vec![STATUS_RUNNING, STATUS_WAITING], true)
            }
            DurableRunStatusKind::Delegated => {
                (STATUS_DELEGATED, vec![STATUS_RUNNING, STATUS_WAITING], true)
            }
            DurableRunStatusKind::Failed => {
                (STATUS_FAILED, vec![STATUS_RUNNING, STATUS_WAITING], true)
            }
            DurableRunStatusKind::Cancelled => {
                (STATUS_CANCELLED, vec![STATUS_RUNNING, STATUS_WAITING], true)
            }
            // Verification failure is an AgentResult detail, not a
            // separate durable lifecycle state.
            DurableRunStatusKind::Other if status == "verification_failed" => {
                (STATUS_FAILED, vec![STATUS_RUNNING, STATUS_WAITING], true)
            }
            DurableRunStatusKind::Other => {
                return Err(format!(
                    "unsupported delegation outcome status '{status}' for run {run_id}"
                ));
            }
        };

        if !terminal {
            return self
                .persist_status_if_current(
                    user_id,
                    run_id,
                    &expected_statuses,
                    canonical_status,
                    waiting_for,
                    error_message,
                )
                .await;
        }

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
        events.push(serde_json::json!({
            "event_type": "run_finished",
            "data": {
                "status": canonical_status,
                "error": error_message,
            }
        }));

        match self
            .commit_terminal_status_with_events_if_current(
                user_id,
                run_id,
                &expected_statuses,
                canonical_status,
                waiting_for,
                error_message,
                &events,
            )
            .await?
        {
            TerminalTransitionOutcome::Committed => Ok(true),
            TerminalTransitionOutcome::Superseded(durable) => {
                tracing::info!(
                    target: "astra_runtime::delegation",
                    user_id,
                    run_id,
                    attempted_status = canonical_status,
                    durable_status = %durable.status,
                    "delegation outcome lost its status CAS; preserving durable authority"
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
        run_id: &str,
        expected_statuses: &[&str],
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
        event: serde_json::Value,
    ) -> Result<bool, String> {
        let updated = self
            .store
            .update_run_status_with_event_if_current(
                user_id,
                run_id,
                expected_statuses,
                status,
                waiting_for,
                error_message,
                event,
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
        event: serde_json::Value,
    ) -> Result<GuardedRunStatusTransition, String> {
        let outcome = self
            .store
            .update_run_status_with_event_if_current_unless_session_blocked(
                GuardedRunStatusTransitionRequest {
                    user_id,
                    run_id,
                    session_id,
                    expected_statuses,
                    status,
                    waiting_for,
                    error_message,
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
                run_id,
                expected_statuses,
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
        run_id: &str,
        expected_statuses: &[&str],
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
        events: &[serde_json::Value],
    ) -> Result<bool, String> {
        let mut saw_store_error = false;
        let mut last_error: Option<String> = None;
        for attempt in 1..=TERMINAL_TRANSITION_MAX_ATTEMPTS {
            match self
                .transition_status_with_events_if_current(
                    user_id,
                    run_id,
                    expected_statuses,
                    status,
                    waiting_for,
                    error_message,
                    events,
                )
                .await
            {
                Ok(true) => return Ok(true),
                Ok(false) if saw_store_error => {
                    return self
                        .reconcile_terminal_transition_after_store_error(
                            user_id,
                            run_id,
                            status,
                            waiting_for,
                            error_message,
                            events,
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
                run_id,
                expected_statuses,
                status,
                waiting_for,
                error_message,
                events,
            )
            .await?
        {
            return Ok(TerminalTransitionOutcome::Committed);
        }

        let durable = self
            .load_run(user_id, run_id)
            .await?
            .ok_or_else(|| format!("run {run_id} disappeared after terminal transition CAS"))?;
        Ok(TerminalTransitionOutcome::Superseded(Box::new(durable)))
    }

    async fn reconcile_terminal_transition_after_store_error(
        &self,
        user_id: &str,
        run_id: &str,
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
        events: &[serde_json::Value],
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
        if run.status != status {
            return Ok(false);
        }
        if !durable_run_contains_event_batch(&run, events) {
            self.store
                .append_events_batch(user_id, run_id, events)
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
        let Some(run) = self.store.load_run(user_id, run_id).await? else {
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
        run_id: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
        tool_calls: u32,
    ) -> Result<bool, String> {
        self.store
            .update_run_usage(
                user_id,
                run_id,
                prompt_tokens,
                completion_tokens,
                tool_calls,
            )
            .await
    }

    /// Save a checkpoint for crash recovery.
    pub async fn persist_checkpoint(
        &self,
        user_id: &str,
        run_id: &str,
        checkpoint_json: &str,
    ) -> Result<bool, String> {
        self.store
            .save_checkpoint(user_id, run_id, checkpoint_json)
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
        run_id: &str,
    ) -> Result<Option<DurableRunDisplayProjectionRecord>, String> {
        self.store.rebuild_run_projection(user_id, run_id).await
    }

    /// Append an event to the durable event log.
    pub async fn append_event(
        &self,
        user_id: &str,
        run_id: &str,
        event: serde_json::Value,
    ) -> Result<(), String> {
        self.store.append_event(user_id, run_id, event).await
    }

    /// Append multiple events in a single batch.
    pub async fn append_events_batch(
        &self,
        user_id: &str,
        run_id: &str,
        events: &[serde_json::Value],
    ) -> Result<(), String> {
        self.store
            .append_events_batch(user_id, run_id, events)
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
            match durable_run_status_kind(&ancestor.status) {
                DurableRunStatusKind::Cancelled => return Ok(Some(RunControlStatus::Cancelled)),
                DurableRunStatusKind::Paused => inherited_pause = true,
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
        run_id: &str,
        retry_count: u32,
    ) -> Result<bool, String> {
        self.store
            .update_retry_count(user_id, run_id, retry_count)
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
        run_id: &str,
        request_id: &str,
        kind: DurableRunInteractionKind,
        response_data: serde_json::Value,
    ) -> Result<DurableRunInteractionResolveOutcome, String> {
        self.store
            .resolve_run_interaction(user_id, run_id, request_id, kind, response_data)
            .await
    }

    /// Recover active runs after a crash/restart.
    ///
    /// A durable status is not proof that an execution task survived. The
    /// current shutdown checkpoint records detection metadata, not enough
    /// state to reconstruct an agent loop safely across processes.
    ///
    /// - orphaned `waiting` / blocking `paused` runs are moved to non-blocking
    ///   `paused` and direct the caller to continue the session with a new run;
    /// - `running` runs with a graceful checkpoint use the same honest
    ///   session-continuation fallback;
    /// - other `running` runs are marked failed because their
    ///   in-flight effects cannot be proven replay-safe.
    pub async fn recover_active_runs(&self) -> Result<Vec<DurableRunRecord>, String> {
        use futures_util::{StreamExt, stream};

        let mut recovered = Vec::new();
        let mut claimed_run_ids = HashSet::new();
        loop {
            let claimed = match self
                .store
                .claim_recoverable_active_runs(RUN_RECOVERY_CLAIM_BATCH_SIZE)
                .await
            {
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
        let checkpoint_available = has_graceful_resume_checkpoint(self, &run).await;
        let continue_via_session =
            matches!(run.status.as_str(), STATUS_WAITING | STATUS_PAUSED) || checkpoint_available;
        if continue_via_session {
            let event = restart_session_continuation_event(&expected_status, checkpoint_available);
            match self
                .store
                .update_run_status_with_event_if_current(
                    &run.user_id,
                    &run.run_id,
                    &[expected_status.as_str()],
                    STATUS_PAUSED,
                    None,
                    None,
                    event.clone(),
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
                    &run.run_id,
                    &[expected_status.as_str()],
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

    /// Access the underlying store (for advanced queries).
    pub fn store(&self) -> &Arc<dyn RunStateStore> {
        &self.store
    }
}

use crate::turn::run_control::{
    QueuedUserIntent, RunControlStatus, RunStatusProvider, UserIntentApplyAck, UserIntentPoll,
    UserIntentPollIssue, UserIntentPollIssueKind, UserIntentProvider,
};

fn durable_event_index(event: &serde_json::Value, fallback: usize) -> usize {
    event
        .get("index")
        .and_then(serde_json::Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
        .unwrap_or(fallback)
}

fn latest_durable_event_cursor(run: &DurableRunRecord) -> usize {
    let counter_cursor = usize::try_from(run.last_event_idx).unwrap_or(0);
    let observed_cursor = run
        .events
        .iter()
        .enumerate()
        .map(|(position, event)| durable_event_index(event, position))
        .max()
        .unwrap_or(0);
    counter_cursor.max(observed_cursor)
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

fn applied_user_intent_indices(run: &DurableRunRecord) -> std::collections::HashSet<usize> {
    run.events
        .iter()
        .filter(|event| {
            event.get("event_type").and_then(serde_json::Value::as_str)
                == Some("user_intent_applied")
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
}

#[async_trait::async_trait]
impl UserIntentProvider for RunEngine {
    async fn poll_user_intents(
        &self,
        user_id: &str,
        run_id: &str,
        after_event_index: usize,
    ) -> UserIntentPoll {
        let run = match self.store.load_run(user_id, run_id).await {
            Ok(Some(run)) => run,
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
                    inputs: Vec::new(),
                    issues: Vec::new(),
                    error: Some(error),
                };
            }
        };
        let applied_indices = applied_user_intent_indices(&run);
        let mut inputs = Vec::new();
        let mut issues = Vec::new();
        for (position, event) in run.events.iter().enumerate() {
            let event_index = durable_event_index(event, position);
            if event_index <= after_event_index
                || applied_indices.contains(&event_index)
                || event.get("event_type").and_then(serde_json::Value::as_str)
                    != Some("user_intent")
            {
                continue;
            }
            match parse_queued_user_intent(event_index, event) {
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
            next_cursor: after_event_index.max(latest_durable_event_cursor(&run)),
            inputs,
            issues,
            error: None,
        }
    }

    async fn mark_user_intents_applied(
        &self,
        user_id: &str,
        run_id: &str,
        event_indices: &[usize],
    ) -> Result<UserIntentApplyAck, String> {
        if event_indices.is_empty() {
            return Ok(UserIntentApplyAck::Applied);
        }
        let requested = event_indices
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        for attempt in 1..=USER_INTENT_APPLY_MAX_ATTEMPTS {
            let run =
                self.store.load_run(user_id, run_id).await?.ok_or_else(|| {
                    format!("run not found while applying user intents: {run_id}")
                })?;
            let already_applied = applied_user_intent_indices(&run);
            let pending = requested
                .iter()
                .copied()
                .filter(|event_index| !already_applied.contains(event_index))
                .collect::<Vec<_>>();
            if pending.is_empty() {
                record_control_poll_attempt(
                    self.metrics_registry.as_ref(),
                    "user_intent_apply",
                    "already_applied",
                );
                return Ok(UserIntentApplyAck::Applied);
            }
            if matches!(
                durable_run_status_kind(&run.status),
                DurableRunStatusKind::Cancelled
                    | DurableRunStatusKind::Completed
                    | DurableRunStatusKind::Delegated
                    | DurableRunStatusKind::Failed
            ) {
                record_control_poll_attempt(
                    self.metrics_registry.as_ref(),
                    "user_intent_apply",
                    "run_terminal",
                );
                return Ok(UserIntentApplyAck::RunTerminal);
            }

            let mut applied_events = Vec::with_capacity(pending.len());
            for event_index in &pending {
                let source = run
                    .events
                    .iter()
                    .enumerate()
                    .find(|(position, event)| durable_event_index(event, *position) == *event_index)
                    .map(|(_, event)| event)
                    .ok_or_else(|| {
                        format!("cannot apply user intent for unknown event index {event_index}")
                    })?;
                if source.get("event_type").and_then(serde_json::Value::as_str)
                    != Some("user_intent")
                {
                    return Err(format!("event index {event_index} is not a user intent"));
                }
                let data = source.get("data").ok_or_else(|| {
                    format!("user intent event {event_index} has no data payload")
                })?;
                let intent_id = data
                    .get("intent_id")
                    .and_then(serde_json::Value::as_str)
                    .filter(|intent_id| !intent_id.trim().is_empty())
                    .ok_or_else(|| format!("user intent event {event_index} has no intent_id"))?;
                let delivery = data
                    .get("delivery")
                    .cloned()
                    .ok_or_else(|| format!("user intent event {event_index} has no delivery"))?;
                applied_events.push(serde_json::json!({
                    "event_type": "user_intent_applied",
                    "idempotency_key": format!("user_intent_applied:{intent_id}"),
                    "data": {
                        "intent_id": intent_id,
                        "delivery": delivery,
                        "status": astra_turn_types::UserIntentStatus::Applied,
                        "event_index": event_index,
                    },
                }));
            }

            let expected_status = run.status.clone();
            match self
                .transition_status_with_events_if_current(
                    user_id,
                    run_id,
                    &[expected_status.as_str()],
                    &expected_status,
                    run.waiting_for.as_deref(),
                    run.error_message.as_deref(),
                    &applied_events,
                )
                .await
            {
                Ok(true) => {
                    record_control_poll_attempt(
                        self.metrics_registry.as_ref(),
                        "user_intent_apply",
                        "applied",
                    );
                    return Ok(UserIntentApplyAck::Applied);
                }
                Ok(false) if attempt < USER_INTENT_APPLY_MAX_ATTEMPTS => {
                    record_control_poll_attempt(
                        self.metrics_registry.as_ref(),
                        "user_intent_apply_retry",
                        "cas_conflict",
                    );
                    tracing::debug!(
                        target: "astra_runtime::run_control",
                        run_id,
                        attempt,
                        max_attempts = USER_INTENT_APPLY_MAX_ATTEMPTS,
                        "user intent apply CAS lost; retrying with a fresh durable snapshot"
                    );
                    tokio::time::sleep(user_intent_apply_retry_delay(attempt)).await;
                }
                Ok(false) => break,
                Err(error) => {
                    record_control_poll_error(
                        self.metrics_registry.as_ref(),
                        "user_intent_apply",
                        "store_error",
                    );
                    let after = self.store.load_run(user_id, run_id).await?;
                    if after.as_ref().is_some_and(|run| {
                        let applied = applied_user_intent_indices(run);
                        requested.iter().all(|index| applied.contains(index))
                    }) {
                        return Ok(UserIntentApplyAck::Applied);
                    }
                    if after.as_ref().is_some_and(|run| {
                        matches!(
                            durable_run_status_kind(&run.status),
                            DurableRunStatusKind::Cancelled
                                | DurableRunStatusKind::Completed
                                | DurableRunStatusKind::Delegated
                                | DurableRunStatusKind::Failed
                        )
                    }) {
                        return Ok(UserIntentApplyAck::RunTerminal);
                    }
                    return Err(error);
                }
            }
        }

        let run = self
            .store
            .load_run(user_id, run_id)
            .await?
            .ok_or_else(|| format!("run not found while applying user intents: {run_id}"))?;
        if matches!(
            durable_run_status_kind(&run.status),
            DurableRunStatusKind::Cancelled
                | DurableRunStatusKind::Completed
                | DurableRunStatusKind::Delegated
                | DurableRunStatusKind::Failed
        ) {
            record_control_poll_attempt(
                self.metrics_registry.as_ref(),
                "user_intent_apply",
                "run_terminal",
            );
            return Ok(UserIntentApplyAck::RunTerminal);
        }
        record_control_poll_error(
            self.metrics_registry.as_ref(),
            "user_intent_apply",
            "cas_exhausted",
        );
        tracing::warn!(
            target: "astra_runtime::run_control",
            run_id,
            status = %run.status,
            attempts = USER_INTENT_APPLY_MAX_ATTEMPTS,
            "user intent apply exhausted its CAS retry budget"
        );
        Err(format!(
            "user intent apply CAS exhausted for run {run_id} while status remained {}",
            run.status
        ))
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
    }

    async fn persist_status_if_current(
        &self,
        user_id: &str,
        run_id: &str,
        expected_statuses: &[&str],
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<bool, String> {
        RunEngine::persist_status_if_current(
            self,
            user_id,
            run_id,
            expected_statuses,
            status,
            waiting_for,
            error_message,
        )
        .await
    }

    async fn persist_usage(
        &self,
        user_id: &str,
        run_id: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
        tool_calls: u32,
    ) -> Result<bool, String> {
        RunEngine::persist_usage(
            self,
            user_id,
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
        run_id: &str,
        checkpoint_json: &str,
    ) -> Result<bool, String> {
        RunEngine::persist_checkpoint(self, user_id, run_id, checkpoint_json).await
    }

    async fn append_event(
        &self,
        user_id: &str,
        run_id: &str,
        event: serde_json::Value,
    ) -> Result<(), String> {
        RunEngine::append_event(self, user_id, run_id, event).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::runs::{DurableRunRecord, InMemoryRunStateStore, RunStateStore};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn test_engine() -> RunEngine {
        RunEngine::new(Arc::new(InMemoryRunStateStore::new()))
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
            engine
                .persist_status("user-1", run_id, winning_status, Some("control"), None)
                .await
                .unwrap();

            let committed = engine
                .persist_delegation_outcome_status("user-1", run_id, STATUS_COMPLETED, None, None)
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

    #[test]
    fn user_intent_apply_retry_delay_is_bounded_exponential_backoff() {
        assert_eq!(user_intent_apply_retry_delay(1), Duration::from_millis(5));
        assert_eq!(user_intent_apply_retry_delay(2), Duration::from_millis(10));
        assert_eq!(user_intent_apply_retry_delay(5), Duration::from_millis(80));
        assert_eq!(
            user_intent_apply_retry_delay(usize::MAX),
            Duration::from_millis(80)
        );
    }

    #[tokio::test]
    async fn owner_lease_heartbeat_is_disabled_when_store_has_no_interval() {
        assert!(
            test_engine()
                .start_owner_lease_heartbeat("user-1".to_string(), "run-1".to_string())
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
        let guard = engine
            .start_owner_lease_heartbeat("user-1".to_string(), "run-1".to_string())
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
    }

    #[derive(Clone, Copy)]
    enum BatchTransitionFailureMode {
        FailBeforeStoreWrite,
        FailAfterStoreWrite,
        FailAfterStatusWrite,
        ConcurrentCancelWins,
    }

    struct FlakyBatchTransitionStore {
        inner: InMemoryRunStateStore,
        fail_remaining: AtomicUsize,
        attempts: AtomicUsize,
        waiting_queries: AtomicUsize,
        recovery_claims: AtomicUsize,
        lease_renewal_interval: Option<Duration>,
        lease_renewals: AtomicUsize,
        lease_releases: AtomicUsize,
        mode: BatchTransitionFailureMode,
    }

    impl FlakyBatchTransitionStore {
        fn new(failures: usize, mode: BatchTransitionFailureMode) -> Self {
            Self {
                inner: InMemoryRunStateStore::new(),
                fail_remaining: AtomicUsize::new(failures),
                attempts: AtomicUsize::new(0),
                waiting_queries: AtomicUsize::new(0),
                recovery_claims: AtomicUsize::new(0),
                lease_renewal_interval: None,
                lease_renewals: AtomicUsize::new(0),
                lease_releases: AtomicUsize::new(0),
                mode,
            }
        }

        fn with_owner_lease_heartbeat(mut self, interval: Duration) -> Self {
            self.lease_renewal_interval = Some(interval);
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

        async fn load_run(
            &self,
            user_id: &str,
            run_id: &str,
        ) -> Result<Option<DurableRunRecord>, String> {
            self.inner.load_run(user_id, run_id).await
        }

        async fn update_run_status(
            &self,
            user_id: &str,
            run_id: &str,
            status: &str,
            waiting_for: Option<&str>,
            error_message: Option<&str>,
        ) -> Result<bool, String> {
            self.inner
                .update_run_status(user_id, run_id, status, waiting_for, error_message)
                .await
        }

        async fn update_run_status_if_current(
            &self,
            user_id: &str,
            run_id: &str,
            expected_statuses: &[&str],
            status: &str,
            waiting_for: Option<&str>,
            error_message: Option<&str>,
        ) -> Result<bool, String> {
            self.inner
                .update_run_status_if_current(
                    user_id,
                    run_id,
                    expected_statuses,
                    status,
                    waiting_for,
                    error_message,
                )
                .await
        }

        async fn update_run_status_with_event_if_current(
            &self,
            user_id: &str,
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
            run_id: &str,
            expected_statuses: &[&str],
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
                                run_id,
                                expected_statuses,
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
                            .update_run_status_if_current(
                                user_id,
                                run_id,
                                expected_statuses,
                                status,
                                waiting_for,
                                error_message,
                            )
                            .await?;
                        return Err("transient EOF after status-only commit".to_string());
                    }
                    BatchTransitionFailureMode::ConcurrentCancelWins => {
                        self.inner
                            .update_run_status_with_events_if_current(
                                user_id,
                                run_id,
                                expected_statuses,
                                STATUS_CANCELLED,
                                None,
                                Some("cancelled elsewhere"),
                                &[serde_json::json!({
                                    "event_type": "run_finished",
                                    "data": {"status": STATUS_CANCELLED}
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
                    run_id,
                    expected_statuses,
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
            run_id: &str,
            prompt_tokens: u64,
            completion_tokens: u64,
            tool_calls: u32,
        ) -> Result<bool, String> {
            self.inner
                .update_run_usage(
                    user_id,
                    run_id,
                    prompt_tokens,
                    completion_tokens,
                    tool_calls,
                )
                .await
        }

        async fn save_checkpoint(
            &self,
            user_id: &str,
            run_id: &str,
            checkpoint_json: &str,
        ) -> Result<bool, String> {
            self.inner
                .save_checkpoint(user_id, run_id, checkpoint_json)
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
            run_id: &str,
        ) -> Result<Option<DurableRunDisplayProjectionRecord>, String> {
            self.inner.rebuild_run_projection(user_id, run_id).await
        }

        async fn append_events_batch(
            &self,
            user_id: &str,
            run_id: &str,
            events: &[serde_json::Value],
        ) -> Result<(), String> {
            self.inner
                .append_events_batch(user_id, run_id, events)
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

        async fn renew_owner_lease(
            &self,
            _user_id: &str,
            _run_id: &str,
            _expected_statuses: &[&str],
        ) -> Result<bool, String> {
            self.lease_renewals.fetch_add(1, Ordering::SeqCst);
            Ok(true)
        }

        async fn release_owner_lease(&self, _user_id: &str, _run_id: &str) -> Result<bool, String> {
            self.lease_releases.fetch_add(1, Ordering::SeqCst);
            Ok(true)
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
            run_id: &str,
            retry_count: u32,
        ) -> Result<bool, String> {
            self.inner
                .update_retry_count(user_id, run_id, retry_count)
                .await
        }
    }

    struct FailingLoadRunStore;

    #[async_trait::async_trait]
    impl RunStateStore for FailingLoadRunStore {
        async fn insert_run(&self, _record: DurableRunRecord) -> Result<(), String> {
            Err("store unavailable".into())
        }

        async fn load_run(
            &self,
            _user_id: &str,
            _run_id: &str,
        ) -> Result<Option<DurableRunRecord>, String> {
            Err("load failed".into())
        }

        async fn update_run_status(
            &self,
            _user_id: &str,
            _run_id: &str,
            _status: &str,
            _waiting_for: Option<&str>,
            _error_message: Option<&str>,
        ) -> Result<bool, String> {
            Err("store unavailable".into())
        }

        async fn update_run_status_if_current(
            &self,
            _user_id: &str,
            _run_id: &str,
            _expected_statuses: &[&str],
            _status: &str,
            _waiting_for: Option<&str>,
            _error_message: Option<&str>,
        ) -> Result<bool, String> {
            Err("store unavailable".into())
        }

        async fn update_run_status_with_event_if_current(
            &self,
            _user_id: &str,
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
            _run_id: &str,
            _expected_statuses: &[&str],
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
            _run_id: &str,
            _prompt_tokens: u64,
            _completion_tokens: u64,
            _tool_calls: u32,
        ) -> Result<bool, String> {
            Err("store unavailable".into())
        }

        async fn save_checkpoint(
            &self,
            _user_id: &str,
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
            _run_id: &str,
        ) -> Result<Option<DurableRunDisplayProjectionRecord>, String> {
            Err("store unavailable".into())
        }

        async fn append_events_batch(
            &self,
            _user_id: &str,
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
                    interaction_mode: Some(RequestedTurnInteractionMode::Auto),
                    interactive_client: Some(true),
                    turn_intent_policy: TurnIntentExecutionPolicy::FixedDefault,
                    execution_metadata: None,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let run = engine.load_run("user-1", "run-ctx").await.unwrap().unwrap();
        assert_eq!(run.events[0]["event_type"], "run_started");
        assert_eq!(run.events[0]["data"]["interaction_mode"], "auto");
        assert_eq!(run.events[0]["data"]["suppressed_loop_nudges"], true);
        assert_eq!(run.events[0]["data"]["interactive_client"], true);
        assert_eq!(run.events[0]["data"]["turn_intent_policy"], "fixed_default");
    }

    #[tokio::test]
    async fn start_run_with_context_persists_effective_agent_binding_runtime_profile_when_omitted()
    {
        let engine = test_engine();
        let request = astra_services::runs::ChatRequestData {
            message: "hello".to_string(),
            user_intent: None,
            parts: Vec::new(),
            attachments: Vec::new(),
            runtime_system_prompt: None,
            session_id: None,
            full_llm_capture: false,
            agent_id: None,
            model: None,
            selected_model: None,
            capability_descriptors: None,
            provider_runtime_authorized: false,
            agent_binding: Some(astra_services::runs::AgentBindingRuntimeRequest {
                id: "ab_018f05f5-c7dd-7f43-83e6-93d56d9d7391".to_string(),
                capability_server_refs: astra_services::runs::CapabilityServerRefs {
                    mcp: "tools".to_string(),
                    skills: "skills".to_string(),
                },
            }),
            runtime_auth: None,
            runtime_skill_binding: None,
            runtime_profile: None,
            llm_token_service: None,
            skill_search: None,
            allow_skills: None,
            allow_skill_sources: None,
            allow_tools: None,
            workspace_binding: None,
            executor_binding: None,
            runtime_mcp_bindings: Vec::new(),
            mcp_binding_ids: None,
            context: None,
            edge_executor_id: None,
            capabilities: Vec::new(),
            forward_headers: std::collections::HashMap::new(),
            execution_budget: None,
            execution_policy: Default::default(),
            explain: false,
            interaction_mode: None,
            interactive_client: false,
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
    async fn start_run_with_context_uses_prompt_when_interactive_without_override() {
        let engine = test_engine();
        engine
            .start_run_with_context(
                "run-prompt",
                "user-1",
                "sess-1",
                RunStartContext {
                    interaction_mode: None,
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
        assert_eq!(run.events[0]["data"]["suppressed_loop_nudges"], false);
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
                    interaction_mode: Some(RequestedTurnInteractionMode::NonInteractive),
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
        assert_eq!(run.events[0]["data"]["suppressed_loop_nudges"], false);
    }

    #[tokio::test]
    async fn start_run_ext_persists_retry_linkage() {
        let engine = test_engine();
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
            .persist_status("user-1", "run-1", "paused", Some("user_resume"), None)
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
            .persist_status("user-1", "nope", "failed", None, Some("crash"))
            .await
            .unwrap();
        assert!(!ok);
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
                "run-terminal-retry",
                &[STATUS_RUNNING],
                STATUS_COMPLETED,
                None,
                None,
                &terminal_events,
            )
            .await
            .unwrap();

        assert_eq!(outcome, TerminalTransitionOutcome::Committed);
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
                "run-terminal-reconcile",
                &[STATUS_RUNNING],
                STATUS_COMPLETED,
                None,
                None,
                &terminal_events,
            )
            .await
            .unwrap();

        assert_eq!(outcome, TerminalTransitionOutcome::Committed);
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
                "run-terminal-repair",
                &[STATUS_RUNNING],
                STATUS_FAILED,
                None,
                Some("tool failed"),
                &terminal_events,
            )
            .await
            .unwrap();

        assert_eq!(outcome, TerminalTransitionOutcome::Committed);
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
            .persist_usage("user-1", "run-1", 1000, 500, 7)
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
            .persist_checkpoint("user-1", "run-1", ck)
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
                "run-1",
                serde_json::json!({"event_type": "tool_call_start", "data": {"tool": "bash"}}),
            )
            .await
            .unwrap();
        engine
            .persist_usage("user-1", "run-1", 11, 7, 3)
            .await
            .unwrap();
        engine
            .persist_checkpoint(
                "user-1",
                "run-1",
                r#"{"version":"checkpoint_v2","graceful":true,"last_batch_id":"batch-1"}"#,
            )
            .await
            .unwrap();
        engine
            .persist_status("user-1", "run-1", "waiting", Some("user_input"), None)
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
                "run-1",
                serde_json::json!({"event_type": "tool_call_start"}),
            )
            .await
            .unwrap();
        engine
            .append_event(
                "user-1",
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
            .persist_status("user-1", "run-2", "waiting", Some("tool_approval"), None)
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
            .persist_status("user-1", "paused-free", "paused", None, None)
            .await
            .unwrap();
        engine
            .start_run("completed", "user-1", "sess-done")
            .await
            .unwrap();
        engine
            .persist_status("user-1", "completed", "completed", None, None)
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
            .persist_status("user-1", "paused", "paused", Some("user_resume"), None)
            .await
            .unwrap();
        engine
            .start_run("waiting", "user-1", "sess-waiting")
            .await
            .unwrap();
        engine
            .persist_status("user-1", "waiting", "waiting", Some("tool_approval"), None)
            .await
            .unwrap();
        engine
            .start_run("cancelled", "user-1", "sess-cancelled")
            .await
            .unwrap();
        engine
            .persist_status("user-1", "cancelled", "cancelled", None, None)
            .await
            .unwrap();

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
            .persist_status("user-1", "root", STATUS_PAUSED, Some("user_resume"), None)
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

        engine
            .persist_status("user-1", "root", STATUS_CANCELLED, None, None)
            .await
            .unwrap();
        assert_eq!(
            engine
                .check_control_status("user-1", "grandchild")
                .await
                .unwrap(),
            Some(RunControlStatus::Cancelled)
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
    async fn poll_user_intents_isolates_poison_and_delivers_later_valid_event() {
        let engine = test_engine();
        engine
            .start_run("run-poison", "user-1", "sess-poison")
            .await
            .unwrap();
        engine
            .append_events_batch(
                "user-1",
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
    async fn user_intent_poll_and_apply_use_persisted_event_indices_with_gaps() {
        let engine = test_engine();
        engine
            .start_run("run-gapped", "user-1", "sess-gapped")
            .await
            .unwrap();
        engine
            .append_events_batch(
                "user-1",
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
            .mark_user_intents_applied("user-1", "run-gapped", &[9])
            .await
            .unwrap();
        engine
            .mark_user_intents_applied("user-1", "run-gapped", &[9])
            .await
            .unwrap();

        let replay = engine.poll_user_intents("user-1", "run-gapped", 0).await;
        assert_eq!(replay.inputs.len(), 1);
        assert_eq!(replay.inputs[0].event_index, 7);
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
                "run-queued",
                STATUS_WAITING,
                Some("edge_executor"),
                None,
            )
            .await
            .unwrap();

        let ack = engine
            .mark_user_intents_applied("user-1", "run-queued", &[1])
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
        assert!(
            poll.inputs.is_empty(),
            "applied intents must not replay after crash recovery"
        );
    }

    #[tokio::test]
    async fn mark_user_intents_applied_does_not_overwrite_paused_status() {
        let engine = test_engine();
        engine
            .start_run("run-paused-release", "user-1", "sess-paused")
            .await
            .unwrap();
        engine
            .append_event(
                "user-1",
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
                "run-paused-release",
                STATUS_PAUSED,
                Some("user_resume"),
                None,
            )
            .await
            .unwrap();

        let ack = engine
            .mark_user_intents_applied("user-1", "run-paused-release", &[1])
            .await
            .unwrap();
        assert_eq!(ack, UserIntentApplyAck::Applied);

        let run = engine
            .load_run("user-1", "run-paused-release")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.status, STATUS_PAUSED);
        assert_eq!(run.waiting_for.as_deref(), Some("user_resume"));
        assert!(
            run.events.iter().any(|event| {
                event.get("event_type").and_then(serde_json::Value::as_str)
                    == Some("user_intent_applied")
            }),
            "paused application must remain auditable without changing pause state"
        );
    }

    #[tokio::test]
    async fn mark_user_intents_applied_does_not_append_on_cancelled_run() {
        let engine = test_engine();
        engine
            .start_run("run-cancelled-release", "user-1", "sess-cancelled")
            .await
            .unwrap();
        engine
            .append_event(
                "user-1",
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
        engine
            .persist_status(
                "user-1",
                "run-cancelled-release",
                STATUS_CANCELLED,
                None,
                None,
            )
            .await
            .unwrap();
        let before = engine
            .load_run("user-1", "run-cancelled-release")
            .await
            .unwrap()
            .unwrap()
            .events
            .len();

        let ack = engine
            .mark_user_intents_applied("user-1", "run-cancelled-release", &[1])
            .await
            .unwrap();
        assert_eq!(ack, UserIntentApplyAck::RunTerminal);

        let run = engine
            .load_run("user-1", "run-cancelled-release")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.status, STATUS_CANCELLED);
        assert_eq!(run.events.len(), before);
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
            .mark_user_intents_applied("user-1", "run-apply-race", &[1])
            .await
            .unwrap();
        assert_eq!(ack, UserIntentApplyAck::RunTerminal);

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
            .mark_user_intents_applied("user-1", "run-apply-timeout", &[1])
            .await
            .unwrap();
        engine
            .mark_user_intents_applied("user-1", "run-apply-timeout", &[1])
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
        assert_eq!(store.attempts(), 1);
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
                "run-cas",
                STATUS_PAUSED,
                Some("user_resume"),
                None,
            )
            .await
            .unwrap();

        let updated = engine
            .persist_status_if_current(
                "user-1",
                "run-cas",
                &[STATUS_RUNNING],
                STATUS_RUNNING,
                None,
                None,
            )
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
            .persist_status("user-1", "run-1", "waiting", Some("user_resume"), None)
            .await
            .unwrap();
        engine
            .persist_status("user-1", "run-2", "completed", None, None)
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
                selected_model_json: None,
                selected_model_name: None,
                selected_model_gateway: None,
                capability_server_refs_json: None,
                runtime_profile: None,
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
            .persist_status("user-1", "run-1", "paused", Some("user_resume"), None)
            .await
            .unwrap();
        engine
            .append_event(
                "user-1",
                "run-1",
                serde_json::json!({"event_type": "run_paused"}),
            )
            .await
            .unwrap();

        // Simulate resume
        engine
            .persist_status("user-1", "run-1", "running", None, None)
            .await
            .unwrap();
        engine
            .append_event(
                "user-1",
                "run-1",
                serde_json::json!({"event_type": "run_resumed"}),
            )
            .await
            .unwrap();

        // Simulate completion
        engine
            .persist_usage("user-1", "run-1", 2000, 800, 12)
            .await
            .unwrap();
        engine
            .persist_checkpoint("user-1", "run-1", r#"{"phase":"final","final":true}"#)
            .await
            .unwrap();
        engine
            .persist_status("user-1", "run-1", "completed", None, None)
            .await
            .unwrap();
        engine
            .append_event(
                "user-1",
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
            .persist_status("user-1", "run-1", "failed", None, Some("OOM killed"))
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
            .persist_status("user-1", "run-crash", "running", None, None)
            .await
            .unwrap();

        // Insert an orphaned waiting run. With no surviving consumer it must
        // release the session for a continuation run.
        engine
            .start_run("run-wait", "user-1", "sess-2")
            .await
            .unwrap();
        engine
            .persist_status("user-1", "run-wait", "waiting", None, None)
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
            crashed_event_types.ends_with(&["run_error", "run_finished"]),
            "crash recovery failure must persist a complete terminal event pair"
        );
        let run_error = &crashed.events[crashed.events.len() - 2];
        let run_finished = crashed.events.last().unwrap();
        assert_eq!(run_error["data"]["error_code"], "crash_recovery");
        assert_eq!(
            run_finished["data"]["status"], "failed",
            "crash recovery run_finished must be self-describing across replay boundaries"
        );
        assert_eq!(run_finished["data"]["error_code"], "crash_recovery");

        // The orphaned waiting run becomes a non-blocking paused continuation.
        let waiting = engine
            .load_run("user-1", "run-wait")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(waiting.status, STATUS_PAUSED);
        assert!(waiting.waiting_for.is_none());
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
