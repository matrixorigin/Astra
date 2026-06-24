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

use std::sync::Arc;

use astra_services::{
    DatabaseStateProjectionStore,
    runs::{
        CapabilityServerRefs, DurableRunCheckpointRecord, DurableRunDisplayProjectionRecord,
        DurableRunRecord, DurableRunStatusKind, RequestedTurnInteractionMode, RunStateStore,
        RuntimeProfileRequest, SelectedModelRequest, durable_run_status_kind,
    },
};

use astra_core::{
    STATUS_CANCELLED, STATUS_INPUT_QUEUED, STATUS_PAUSED, STATUS_RUNNING, STATUS_WAITING,
};

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
}

/// Optional run-start interaction context persisted into the durable
/// `run_started` event so replay/status surfaces can explain policy decisions.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunStartContext {
    pub interaction_mode: Option<RequestedTurnInteractionMode>,
    pub interactive_client: Option<bool>,
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

fn run_started_event_data(context: &RunStartContext) -> serde_json::Value {
    let mut data = serde_json::Map::new();
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
            match self.store.load_run(parent_run_id).await? {
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
        let selected_model_json = context
            .selected_model
            .as_ref()
            .and_then(|selected_model| serde_json::to_string(selected_model).ok());
        let selected_model_name = context
            .selected_model
            .as_ref()
            .map(|selected_model| selected_model.model.clone());
        let selected_model_gateway = context
            .selected_model
            .as_ref()
            .and_then(|selected_model| selected_model.gateway.clone());
        let capability_server_refs_json = context
            .capability_server_refs
            .as_ref()
            .and_then(|refs| serde_json::to_string(refs).ok());
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
        self.project_delegation_run_if_needed(run_id, STATUS_RUNNING, None)
            .await?;
        Ok(())
    }

    /// Persist a status change to the durable store.
    pub async fn persist_status(
        &self,
        run_id: &str,
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<bool, String> {
        let updated = self
            .store
            .update_run_status(run_id, status, waiting_for, error_message)
            .await?;
        if updated {
            let summary = error_message.or(waiting_for);
            self.project_delegation_run_if_needed(run_id, status, summary)
                .await?;
        }
        Ok(updated)
    }

    /// Persist a status change only if the durable row is still in one of the
    /// expected states. This prevents stale control-plane observations from
    /// overwriting a newer pause/cancel/terminal status.
    pub async fn persist_status_if_current(
        &self,
        run_id: &str,
        expected_statuses: &[&str],
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<bool, String> {
        let updated = self
            .store
            .update_run_status_if_current(
                run_id,
                expected_statuses,
                status,
                waiting_for,
                error_message,
            )
            .await?;
        if updated {
            let summary = error_message.or(waiting_for);
            self.project_delegation_run_if_needed(run_id, status, summary)
                .await?;
        }
        Ok(updated)
    }

    async fn project_delegation_run_if_needed(
        &self,
        run_id: &str,
        status: &str,
        last_summary_text: Option<&str>,
    ) -> Result<(), String> {
        let Some(projection_store) = self.projection_store.as_ref() else {
            return Ok(());
        };
        let Some(run) = self.store.load_run(run_id).await? else {
            return Ok(());
        };
        if run.parent_run_id.is_none() || run.delegation_id.is_none() {
            return Ok(());
        }
        projection_store
            .upsert_delegation_projection_for_run(
                run_id,
                status,
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
        run_id: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
        tool_calls: u32,
    ) -> Result<bool, String> {
        self.store
            .update_run_usage(run_id, prompt_tokens, completion_tokens, tool_calls)
            .await
    }

    /// Save a checkpoint for crash recovery.
    pub async fn persist_checkpoint(
        &self,
        run_id: &str,
        checkpoint_json: &str,
    ) -> Result<bool, String> {
        self.store.save_checkpoint(run_id, checkpoint_json).await
    }

    /// Load the newest typed checkpoint for a run.
    pub async fn load_latest_checkpoint(
        &self,
        run_id: &str,
        checkpoint_kind: Option<&str>,
    ) -> Result<Option<DurableRunCheckpointRecord>, String> {
        self.store
            .load_latest_checkpoint(run_id, checkpoint_kind)
            .await
    }

    /// Load the current durable display projection for a run.
    pub async fn load_run_projection(
        &self,
        run_id: &str,
    ) -> Result<Option<DurableRunDisplayProjectionRecord>, String> {
        self.store.load_run_projection(run_id).await
    }

    /// Append an event to the durable event log.
    pub async fn append_event(&self, run_id: &str, event: serde_json::Value) -> Result<(), String> {
        self.store.append_event(run_id, event).await
    }

    /// Append multiple events in a single batch.
    pub async fn append_events_batch(
        &self,
        run_id: &str,
        events: &[serde_json::Value],
    ) -> Result<(), String> {
        self.store.append_events_batch(run_id, events).await
    }

    /// Load a run from the durable store (cache miss or recovery path).
    pub async fn load_run(&self, run_id: &str) -> Result<Option<DurableRunRecord>, String> {
        self.store.load_run(run_id).await
    }

    /// Check whether the run has been cancelled or paused externally
    /// (e.g. by another pod in a horizontally-scaled deployment).
    /// Returns `Some("cancelled")` if cancelled, `Some("paused")` if paused,
    /// or `None` if the run is still active. Also returns `Some("cancelled")`
    /// when the run record cannot be found (e.g. was deleted by a different pod).
    pub async fn check_control_status(
        &self,
        run_id: &str,
    ) -> Result<Option<RunControlStatus>, String> {
        let record = self.store.load_run(run_id).await?;
        Ok(match record {
            None => None,
            Some(r) => match durable_run_status_kind(&r.status) {
                DurableRunStatusKind::Cancelled => Some(RunControlStatus::Cancelled),
                DurableRunStatusKind::Paused => Some(RunControlStatus::Paused),
                _ => None,
            },
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
        delegation_id: &str,
    ) -> Result<Vec<DurableRunRecord>, String> {
        self.store.find_sub_runs(delegation_id).await
    }

    /// Persist the verification-gate retry count for a run.
    pub async fn persist_retry_count(
        &self,
        run_id: &str,
        retry_count: u32,
    ) -> Result<bool, String> {
        self.store.update_retry_count(run_id, retry_count).await
    }

    /// Recover active runs after a crash/restart.
    ///
    /// - `waiting` runs: returned for the caller to resume.
    /// - `running` runs with graceful checkpoint_v1: moved to `waiting` and
    ///   annotated with `run_resumed_after_restart` for exactly-once replay.
    /// - other `running` runs: were in-flight when the process died; marked
    ///   `failed` with reason "recovered from crash".
    pub async fn recover_active_runs(&self) -> Result<Vec<DurableRunRecord>, String> {
        let waiting = self.store.find_waiting_runs().await?;
        let running = self.store.find_recoverable_running_runs().await?;

        let mut recovered_running = Vec::with_capacity(running.len());
        for mut run in running {
            if has_graceful_resume_checkpoint(self, &run).await {
                if let Err(e) = self
                    .store
                    .update_run_status(&run.run_id, STATUS_WAITING, Some("restart_resume"), None)
                    .await
                {
                    tracing::warn!(
                        target: "astra_runtime::run_engine",
                        run_id = %run.run_id,
                        error = %e,
                        "failed to mark graceful run waiting during recovery"
                    );
                }
                if let Err(e) = self
                    .store
                    .append_event(
                        &run.run_id,
                        serde_json::json!({
                            "event_type": "run_resumed_after_restart",
                            "data": {"checkpoint_version": "checkpoint_v1"}
                        }),
                    )
                    .await
                {
                    tracing::warn!(
                        target: "astra_runtime::run_engine",
                        run_id = %run.run_id,
                        error = %e,
                        "failed to append graceful restart event"
                    );
                }
                run.status = STATUS_WAITING.to_string();
                run.waiting_for = Some("restart_resume".to_string());
            } else {
                if let Err(e) = self
                    .store
                    .update_run_status(
                        &run.run_id,
                        astra_core::STATUS_FAILED,
                        None,
                        Some("recovered from crash"),
                    )
                    .await
                {
                    tracing::warn!(
                        target: "astra_runtime::run_engine",
                        run_id = %run.run_id,
                        error = %e,
                        "failed to mark crashed run as failed during recovery"
                    );
                }
                run.status = astra_core::STATUS_FAILED.to_string();
                run.error_message = Some("recovered from crash".to_string());
            }
            recovered_running.push(run);
        }

        // Return all: waiting (to resume) + recovered running runs.
        let mut all = waiting;
        all.extend(recovered_running);
        Ok(all)
    }

    /// List runs for a user (delegates to store).
    pub async fn list_user_runs(
        &self,
        user_id: &str,
        limit: u32,
        offset: u32,
    ) -> Result<(Vec<DurableRunRecord>, i64), String> {
        self.store.list_user_runs(user_id, limit, offset).await
    }

    /// Access the underlying store (for advanced queries).
    pub fn store(&self) -> &Arc<dyn RunStateStore> {
        &self.store
    }
}

use crate::turn::run_control::{
    QueuedRunInputEvent, RunControlStatus, RunInputProvider, RunQueuedInputPoll, RunStatusProvider,
};

#[async_trait::async_trait]
impl RunStatusProvider for RunEngine {
    #[allow(clippy::blocks_in_conditions)]
    async fn control_status(&self, run_id: &str) -> Result<Option<RunControlStatus>, String> {
        self.check_control_status(run_id).await
    }
}

#[async_trait::async_trait]
impl RunInputProvider for RunEngine {
    async fn poll_user_inputs(&self, run_id: &str, after_event_index: usize) -> RunQueuedInputPoll {
        let run = match self.store.load_run(run_id).await {
            Ok(Some(run)) => run,
            Ok(None) => {
                let error = format!("run not found while polling deferred input: {run_id}");
                return RunQueuedInputPoll {
                    next_cursor: after_event_index,
                    inputs: Vec::new(),
                    error: Some(error),
                };
            }
            Err(error) => {
                tracing::warn!(
                    run_id,
                    error = %error,
                    "failed to poll queued user inputs from run store"
                );
                return RunQueuedInputPoll {
                    next_cursor: after_event_index,
                    inputs: Vec::new(),
                    error: Some(error),
                };
            }
        };
        let released_indices = run
            .events
            .iter()
            .filter(|event| {
                event.get("event_type").and_then(serde_json::Value::as_str)
                    == Some("user_inputs_released")
            })
            .flat_map(|event| {
                event
                    .get("data")
                    .and_then(|data| data.get("event_indices"))
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(serde_json::Value::as_u64)
                    .map(|value| value as usize)
            })
            .collect::<std::collections::HashSet<_>>();
        let start_index = after_event_index.min(run.events.len());

        let inputs: Vec<QueuedRunInputEvent> = run
            .events
            .iter()
            .enumerate()
            .skip(start_index)
            .filter_map(|(event_index, event)| {
                let payload = event
                    .get("event_type")
                    .and_then(serde_json::Value::as_str)
                    .filter(|event_type| *event_type == "user_input")
                    .and_then(|_| event.get("data"))
                    .and_then(|data| data.get("input"))
                    .cloned()?;
                if released_indices.contains(&event_index) {
                    return None;
                }
                Some(QueuedRunInputEvent {
                    event_index,
                    input: payload,
                })
            })
            .collect();

        let mut error = None;
        if run.status == STATUS_INPUT_QUEUED && inputs.is_empty() && !released_indices.is_empty() {
            if let Err(update_error) = self
                .persist_status_if_current(
                    run_id,
                    &[STATUS_INPUT_QUEUED],
                    STATUS_RUNNING,
                    None,
                    None,
                )
                .await
            {
                tracing::warn!(
                    run_id,
                    error = %update_error,
                    "failed to auto-heal stale input-queued status after released inputs"
                );
                error = Some(update_error);
            }
        }

        RunQueuedInputPoll {
            next_cursor: after_event_index.max(run.events.len()),
            inputs,
            error,
        }
    }

    async fn mark_user_inputs_released(
        &self,
        run_id: &str,
        event_indices: &[usize],
    ) -> Result<(), String> {
        if event_indices.is_empty() {
            return Ok(());
        }
        let run =
            self.store.load_run(run_id).await?.ok_or_else(|| {
                format!("run not found while acknowledging deferred input: {run_id}")
            })?;
        match durable_run_status_kind(&run.status) {
            DurableRunStatusKind::Cancelled
            | DurableRunStatusKind::Completed
            | DurableRunStatusKind::Failed => return Ok(()),
            _ => {}
        }
        self.append_event(
            run_id,
            serde_json::json!({
                "event_type": "user_inputs_released",
                "data": { "event_indices": event_indices },
            }),
        )
        .await?;
        let current =
            self.store.load_run(run_id).await?.ok_or_else(|| {
                format!("run not found after acknowledging deferred input: {run_id}")
            })?;
        if current.status != STATUS_INPUT_QUEUED {
            return Ok(());
        }
        self.persist_status_if_current(run_id, &[STATUS_INPUT_QUEUED], STATUS_RUNNING, None, None)
            .await
            .map(|_| ())
    }
}

async fn has_graceful_resume_checkpoint(engine: &RunEngine, run: &DurableRunRecord) -> bool {
    if let Ok(Some(checkpoint)) = engine
        .load_latest_checkpoint(&run.run_id, Some("resume"))
        .await
    {
        return checkpoint_is_graceful_resume(
            &checkpoint.checkpoint_version,
            &checkpoint.checkpoint_json,
        );
    }
    checkpoint_is_graceful_resume(
        run.checkpoint_version.as_deref().unwrap_or_default(),
        run.checkpoint_json.as_deref().unwrap_or_default(),
    )
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

    async fn persist_status(
        &self,
        run_id: &str,
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<bool, String> {
        RunEngine::persist_status(self, run_id, status, waiting_for, error_message).await
    }

    async fn persist_usage(
        &self,
        run_id: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
        tool_calls: u32,
    ) -> Result<bool, String> {
        RunEngine::persist_usage(self, run_id, prompt_tokens, completion_tokens, tool_calls).await
    }

    async fn persist_checkpoint(
        &self,
        run_id: &str,
        checkpoint_json: &str,
    ) -> Result<bool, String> {
        RunEngine::persist_checkpoint(self, run_id, checkpoint_json).await
    }

    async fn append_event(&self, run_id: &str, event: serde_json::Value) -> Result<(), String> {
        RunEngine::append_event(self, run_id, event).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::runs::{DurableRunRecord, InMemoryRunStateStore, RunStateStore};

    fn test_engine() -> RunEngine {
        RunEngine::new(Arc::new(InMemoryRunStateStore::new()))
    }

    struct FailingLoadRunStore;

    #[async_trait::async_trait]
    impl RunStateStore for FailingLoadRunStore {
        async fn insert_run(&self, _record: DurableRunRecord) -> Result<(), String> {
            Err("store unavailable".into())
        }

        async fn load_run(&self, _run_id: &str) -> Result<Option<DurableRunRecord>, String> {
            Err("load failed".into())
        }

        async fn update_run_status(
            &self,
            _run_id: &str,
            _status: &str,
            _waiting_for: Option<&str>,
            _error_message: Option<&str>,
        ) -> Result<bool, String> {
            Err("store unavailable".into())
        }

        async fn update_run_status_if_current(
            &self,
            _run_id: &str,
            _expected_statuses: &[&str],
            _status: &str,
            _waiting_for: Option<&str>,
            _error_message: Option<&str>,
        ) -> Result<bool, String> {
            Err("store unavailable".into())
        }

        async fn update_run_usage(
            &self,
            _run_id: &str,
            _prompt_tokens: u64,
            _completion_tokens: u64,
            _tool_calls: u32,
        ) -> Result<bool, String> {
            Err("store unavailable".into())
        }

        async fn save_checkpoint(
            &self,
            _run_id: &str,
            _checkpoint_json: &str,
        ) -> Result<bool, String> {
            Err("store unavailable".into())
        }

        async fn load_latest_checkpoint(
            &self,
            _run_id: &str,
            _checkpoint_kind: Option<&str>,
        ) -> Result<Option<DurableRunCheckpointRecord>, String> {
            Err("store unavailable".into())
        }

        async fn load_run_projection(
            &self,
            _run_id: &str,
        ) -> Result<Option<DurableRunDisplayProjectionRecord>, String> {
            Err("store unavailable".into())
        }

        async fn append_events_batch(
            &self,
            _run_id: &str,
            _events: &[serde_json::Value],
        ) -> Result<(), String> {
            Err("store unavailable".into())
        }

        async fn list_user_runs(
            &self,
            _user_id: &str,
            _limit: u32,
            _offset: u32,
        ) -> Result<(Vec<DurableRunRecord>, i64), String> {
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
            _delegation_id: &str,
        ) -> Result<Vec<DurableRunRecord>, String> {
            Err("store unavailable".into())
        }

        async fn update_retry_count(
            &self,
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
        let run = engine.load_run("run-1").await.unwrap().unwrap();
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
                    execution_metadata: None,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let run = engine.load_run("run-ctx").await.unwrap().unwrap();
        assert_eq!(run.events[0]["event_type"], "run_started");
        assert_eq!(run.events[0]["data"]["interaction_mode"], "auto");
        assert_eq!(run.events[0]["data"]["suppressed_loop_nudges"], true);
        assert_eq!(run.events[0]["data"]["interactive_client"], true);
    }

    #[tokio::test]
    async fn start_run_with_context_persists_effective_agent_binding_runtime_profile_when_omitted()
    {
        let engine = test_engine();
        let request = astra_services::runs::ChatRequestData {
            message: "hello".to_string(),
            parts: Vec::new(),
            attachments: Vec::new(),
            session_id: None,
            full_llm_capture: false,
            agent_id: None,
            model: None,
            selected_model: None,
            agent_binding: Some(astra_services::runs::AgentBindingRuntimeRequest {
                id: "ab_018f05f5-c7dd-7f43-83e6-93d56d9d7391".to_string(),
                capability_server_refs: astra_services::runs::CapabilityServerRefs {
                    mcp: "tools".to_string(),
                    skills: "skills".to_string(),
                },
            }),
            runtime_auth: None,
            runtime_profile: None,
            llm_token_service: None,
            skill_search: None,
            allow_skills: None,
            allow_skill_sources: None,
            allow_tools: None,
            workspace_binding: None,
            executor_binding: None,
            runtime_mcp_bindings: Vec::new(),
            context: None,
            edge_executor_id: None,
            capabilities: Vec::new(),
            forward_headers: std::collections::HashMap::new(),
            execution_budget: None,
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

        let run = engine.load_run("run-agent-binding").await.unwrap().unwrap();
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
        let run = engine.load_run("run-prompt").await.unwrap().unwrap();
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
            .load_run("run-non-interactive")
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

        let run = engine.load_run("run-retry").await.unwrap().unwrap();
        assert_eq!(run.parent_run_id.as_deref(), Some("parent-1"));
        assert_eq!(run.delegation_id.as_deref(), Some("del-1"));
        assert_eq!(run.agent_id.as_deref(), Some("coder"));
        assert_eq!(run.retry_of.as_deref(), Some("run-original"));
    }

    #[tokio::test]
    async fn load_nonexistent_returns_none() {
        let engine = test_engine();
        assert!(engine.load_run("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn persist_status_updates() {
        let engine = test_engine();
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();
        let ok = engine
            .persist_status("run-1", "paused", Some("user_resume"), None)
            .await
            .unwrap();
        assert!(ok);
        let run = engine.load_run("run-1").await.unwrap().unwrap();
        assert_eq!(run.status, "paused");
        assert_eq!(run.waiting_for.as_deref(), Some("user_resume"));
    }

    #[tokio::test]
    async fn persist_status_nonexistent_returns_false() {
        let engine = test_engine();
        let ok = engine
            .persist_status("nope", "failed", None, Some("crash"))
            .await
            .unwrap();
        assert!(!ok);
    }

    #[tokio::test]
    async fn persist_usage_updates() {
        let engine = test_engine();
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();
        engine.persist_usage("run-1", 1000, 500, 7).await.unwrap();
        let run = engine.load_run("run-1").await.unwrap().unwrap();
        assert_eq!(run.total_prompt_tokens, 1000);
        assert_eq!(run.total_completion_tokens, 500);
        assert_eq!(run.total_tool_calls, 7);
    }

    #[tokio::test]
    async fn persist_checkpoint_saves() {
        let engine = test_engine();
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();
        let ck = r#"{"version":"checkpoint_v1","graceful":true,"messages":[],"turn":3}"#;
        engine.persist_checkpoint("run-1", ck).await.unwrap();
        let checkpoint = engine
            .load_latest_checkpoint("run-1", Some("resume"))
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
                "run-1",
                serde_json::json!({"event_type": "tool_call_start", "data": {"tool": "bash"}}),
            )
            .await
            .unwrap();
        engine.persist_usage("run-1", 11, 7, 3).await.unwrap();
        engine
            .persist_checkpoint(
                "run-1",
                r#"{"version":"checkpoint_v2","graceful":true,"last_batch_id":"batch-1"}"#,
            )
            .await
            .unwrap();
        engine
            .persist_status("run-1", "waiting", Some("user_input"), None)
            .await
            .unwrap();

        let projection = engine
            .load_run_projection("run-1")
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
        let projection = engine.load_run_projection("nope").await.unwrap();
        assert!(projection.is_none());
    }

    #[tokio::test]
    async fn append_event_accumulates() {
        let engine = test_engine();
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();
        engine
            .append_event(
                "run-1",
                serde_json::json!({"event_type": "tool_call_start"}),
            )
            .await
            .unwrap();
        engine
            .append_event("run-1", serde_json::json!({"event_type": "tool_result"}))
            .await
            .unwrap();
        let run = engine.load_run("run-1").await.unwrap().unwrap();
        assert_eq!(run.events.len(), 3); // run_started + 2 appended
    }

    #[tokio::test]
    async fn find_waiting_runs_filters_correctly() {
        let engine = test_engine();
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();
        engine.start_run("run-2", "user-1", "sess-2").await.unwrap();
        engine
            .persist_status("run-2", "waiting", Some("tool_approval"), None)
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
            .persist_status("paused-free", "paused", None, None)
            .await
            .unwrap();
        engine
            .start_run("completed", "user-1", "sess-done")
            .await
            .unwrap();
        engine
            .persist_status("completed", "completed", None, None)
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
            .persist_status("paused", "paused", Some("user_resume"), None)
            .await
            .unwrap();
        engine
            .start_run("waiting", "user-1", "sess-waiting")
            .await
            .unwrap();
        engine
            .persist_status("waiting", "waiting", Some("tool_approval"), None)
            .await
            .unwrap();
        engine
            .start_run("cancelled", "user-1", "sess-cancelled")
            .await
            .unwrap();
        engine
            .persist_status("cancelled", "cancelled", None, None)
            .await
            .unwrap();

        assert_eq!(
            engine.check_control_status("paused").await.unwrap(),
            Some(RunControlStatus::Paused)
        );
        assert_eq!(engine.check_control_status("waiting").await.unwrap(), None);
        assert_eq!(
            engine.check_control_status("cancelled").await.unwrap(),
            Some(RunControlStatus::Cancelled)
        );
    }

    #[tokio::test]
    async fn poll_user_inputs_keeps_cursor_when_after_index_exceeds_events() {
        let engine = test_engine();
        engine
            .start_run("run-input", "user-1", "sess-input")
            .await
            .unwrap();
        engine
            .append_event(
                "run-input",
                serde_json::json!({
                    "event_type": "user_input",
                    "data": { "input": { "content": "queued" } },
                }),
            )
            .await
            .unwrap();

        let poll = engine.poll_user_inputs("run-input", 99).await;

        assert_eq!(poll.next_cursor, 99);
        assert!(poll.inputs.is_empty());
        assert_eq!(poll.error, None);
    }

    #[tokio::test]
    async fn poll_user_inputs_reports_store_load_errors() {
        let engine = RunEngine::new(Arc::new(FailingLoadRunStore));

        let poll = engine.poll_user_inputs("run-input", 7).await;

        assert_eq!(poll.next_cursor, 7);
        assert!(poll.inputs.is_empty());
        assert_eq!(poll.error.as_deref(), Some("load failed"));
    }

    #[tokio::test]
    async fn poll_user_inputs_reports_missing_run_as_error() {
        let engine = test_engine();

        let poll = engine.poll_user_inputs("missing-run", 3).await;

        assert_eq!(poll.next_cursor, 3);
        assert!(poll.inputs.is_empty());
        assert_eq!(
            poll.error.as_deref(),
            Some("run not found while polling deferred input: missing-run")
        );
    }

    #[tokio::test]
    async fn mark_user_inputs_released_clears_input_queued_status() {
        let engine = test_engine();
        engine
            .start_run("run-queued", "user-1", "sess-queued")
            .await
            .unwrap();
        engine
            .persist_status("run-queued", STATUS_INPUT_QUEUED, Some("user_input"), None)
            .await
            .unwrap();

        engine
            .mark_user_inputs_released("run-queued", &[1])
            .await
            .unwrap();

        let run = engine.load_run("run-queued").await.unwrap().unwrap();
        assert_eq!(run.status, STATUS_RUNNING);
        assert_eq!(run.waiting_for, None);
        let poll = engine.poll_user_inputs("run-queued", 0).await;
        assert!(
            poll.inputs.is_empty(),
            "released inputs must not replay after crash recovery"
        );
    }

    #[tokio::test]
    async fn mark_user_inputs_released_does_not_overwrite_paused_status() {
        let engine = test_engine();
        engine
            .start_run("run-paused-release", "user-1", "sess-paused")
            .await
            .unwrap();
        engine
            .persist_status(
                "run-paused-release",
                STATUS_INPUT_QUEUED,
                Some("user_input"),
                None,
            )
            .await
            .unwrap();
        engine
            .persist_status(
                "run-paused-release",
                STATUS_PAUSED,
                Some("user_resume"),
                None,
            )
            .await
            .unwrap();

        engine
            .mark_user_inputs_released("run-paused-release", &[1])
            .await
            .unwrap();

        let run = engine
            .load_run("run-paused-release")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.status, STATUS_PAUSED);
        assert_eq!(run.waiting_for.as_deref(), Some("user_resume"));
    }

    #[tokio::test]
    async fn mark_user_inputs_released_does_not_append_on_cancelled_run() {
        let engine = test_engine();
        engine
            .start_run("run-cancelled-release", "user-1", "sess-cancelled")
            .await
            .unwrap();
        engine
            .persist_status("run-cancelled-release", STATUS_CANCELLED, None, None)
            .await
            .unwrap();
        let before = engine
            .load_run("run-cancelled-release")
            .await
            .unwrap()
            .unwrap()
            .events
            .len();

        engine
            .mark_user_inputs_released("run-cancelled-release", &[1])
            .await
            .unwrap();

        let run = engine
            .load_run("run-cancelled-release")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.status, STATUS_CANCELLED);
        assert_eq!(run.events.len(), before);
    }

    #[tokio::test]
    async fn persist_status_if_current_does_not_overwrite_unexpected_status() {
        let engine = test_engine();
        engine
            .start_run("run-cas", "user-1", "sess-cas")
            .await
            .unwrap();
        engine
            .persist_status("run-cas", STATUS_PAUSED, Some("user_resume"), None)
            .await
            .unwrap();

        let updated = engine
            .persist_status_if_current(
                "run-cas",
                &[STATUS_INPUT_QUEUED],
                STATUS_RUNNING,
                None,
                None,
            )
            .await
            .unwrap();

        let run = engine.load_run("run-cas").await.unwrap().unwrap();
        assert!(!updated);
        assert_eq!(run.status, STATUS_PAUSED);
        assert_eq!(run.waiting_for.as_deref(), Some("user_resume"));
    }

    #[tokio::test]
    async fn missing_run_does_not_report_cancelled_control_status() {
        let engine = test_engine();
        assert_eq!(
            engine.check_control_status("missing-run").await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn list_user_runs_pagination() {
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
        let (runs, total) = engine.list_user_runs("user-1", 2, 0).await.unwrap();
        assert_eq!(total, 5);
        assert_eq!(runs.len(), 2);
        let (runs2, _) = engine.list_user_runs("user-1", 10, 3).await.unwrap();
        assert_eq!(runs2.len(), 2);
    }

    #[tokio::test]
    async fn recover_active_runs_returns_waiting() {
        let engine = test_engine();
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();
        engine.start_run("run-2", "user-1", "sess-2").await.unwrap();
        engine
            .persist_status("run-1", "waiting", Some("user_resume"), None)
            .await
            .unwrap();
        engine
            .persist_status("run-2", "completed", None, None)
            .await
            .unwrap();
        let active = engine.recover_active_runs().await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].run_id, "run-1");
    }

    #[tokio::test]
    async fn recover_active_runs_promotes_graceful_resume_checkpoint_to_waiting() {
        let engine = test_engine();
        engine
            .start_run("run-resume", "user-1", "sess-resume")
            .await
            .unwrap();
        engine
            .persist_checkpoint(
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
        assert_eq!(resumed.status, "waiting");
        assert_eq!(resumed.waiting_for.as_deref(), Some("restart_resume"));
        let durable = engine.load_run("run-resume").await.unwrap().unwrap();
        assert_eq!(durable.status, "waiting");
        assert_eq!(durable.waiting_for.as_deref(), Some("restart_resume"));
        assert_eq!(
            durable.events.last().unwrap()["event_type"],
            "run_resumed_after_restart"
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
        assert_eq!(resumed.status, "waiting");
        assert_eq!(resumed.waiting_for.as_deref(), Some("restart_resume"));
    }

    #[tokio::test]
    async fn full_lifecycle_start_pause_resume_complete() {
        let engine = test_engine();
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();

        // Simulate pause
        engine
            .persist_status("run-1", "paused", Some("user_resume"), None)
            .await
            .unwrap();
        engine
            .append_event("run-1", serde_json::json!({"event_type": "run_paused"}))
            .await
            .unwrap();

        // Simulate resume
        engine
            .persist_status("run-1", "running", None, None)
            .await
            .unwrap();
        engine
            .append_event("run-1", serde_json::json!({"event_type": "run_resumed"}))
            .await
            .unwrap();

        // Simulate completion
        engine.persist_usage("run-1", 2000, 800, 12).await.unwrap();
        engine
            .persist_checkpoint("run-1", r#"{"phase":"final","final":true}"#)
            .await
            .unwrap();
        engine
            .persist_status("run-1", "completed", None, None)
            .await
            .unwrap();
        engine
            .append_event(
                "run-1",
                serde_json::json!({"event_type": "run_finished", "data": {}}),
            )
            .await
            .unwrap();

        let run = engine.load_run("run-1").await.unwrap().unwrap();
        assert_eq!(run.status, "completed");
        assert_eq!(run.total_prompt_tokens, 2000);
        assert_eq!(run.total_completion_tokens, 800);
        assert_eq!(run.total_tool_calls, 12);
        let checkpoint = engine
            .load_latest_checkpoint("run-1", Some("phase"))
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
            .persist_status("run-1", "failed", None, Some("OOM killed"))
            .await
            .unwrap();
        let run = engine.load_run("run-1").await.unwrap().unwrap();
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
            .persist_status("run-crash", "running", None, None)
            .await
            .unwrap();

        // Insert a waiting run (should be returned for resume, not failed)
        engine
            .start_run("run-wait", "user-1", "sess-2")
            .await
            .unwrap();
        engine
            .persist_status("run-wait", "waiting", None, None)
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
        let crashed = engine.load_run("run-crash").await.unwrap().unwrap();
        assert_eq!(
            crashed.status, "failed",
            "crashed running run must be marked failed"
        );
        assert_eq!(
            crashed.error_message.as_deref(),
            Some("recovered from crash"),
            "error message must indicate crash recovery"
        );

        // The waiting run must remain waiting
        let waiting = engine.load_run("run-wait").await.unwrap().unwrap();
        assert_eq!(
            waiting.status, "waiting",
            "waiting run must remain waiting for resume"
        );
    }

    #[tokio::test]
    async fn recover_active_runs_includes_input_queued_runs() {
        let engine = test_engine();
        engine
            .start_run("run-input-queued", "user-1", "sess-queued")
            .await
            .unwrap();
        engine
            .persist_status(
                "run-input-queued",
                STATUS_INPUT_QUEUED,
                Some("user_input"),
                None,
            )
            .await
            .unwrap();

        let recovered = engine.recover_active_runs().await.unwrap();

        assert!(
            recovered.iter().any(|run| run.run_id == "run-input-queued"),
            "input-queued runs must be part of active crash recovery"
        );
        let durable = engine.load_run("run-input-queued").await.unwrap().unwrap();
        assert_eq!(durable.status, "failed");
        assert_eq!(
            durable.error_message.as_deref(),
            Some("recovered from crash")
        );
    }
}
