//! Concrete [`RunLifecycleService`] backed by [`ServerAgenticLoopHost`].
//!
//! This module replaces `UnconfiguredRunLifecycleService` with a real implementation
//! that runs multi-turn agentic loops on the server via the shared
//! [`run_agentic_loop_with_host`] cognitive pipeline.
//!
//! Run state is held in-memory (`DashMap`) for low-latency queries; events are
//! buffered per-run so `stream_run()` can replay from any offset.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use async_trait::async_trait;
use axum::Json;
use axum::http::StatusCode;
use serde_json::{Map, Value, json};
use tokio::sync::{Mutex as TokioMutex, RwLock};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use astra_core::{ErrorResponse, SharedPool, error_response};
use astra_services::EdgeContext;
use astra_services::runs::{
    CancelRunRecord, ChatRequestData, ChatRunRecord, ChatStreamRecord, RunLifecycleService,
    RunListRecord, RunMutationRecord, RunStatusRecord,
};

use crate::FernetTokenEncryptor;
use crate::MatrixOneSettings;
use crate::pipeline::step_recorder::StepRecorder;
use crate::turn::agentic_loop_host::{
    AgenticLoopOutcome, AgenticLoopState, CancellationState, MessagingState, SkillState,
    StopHookState, run_agentic_loop_with_host,
};

use astra_core::{
    STATUS_CANCELLED, STATUS_COMPLETED, STATUS_FAILED, STATUS_PAUSED, STATUS_RUNNING,
    STATUS_WAITING,
};

use super::run_engine::RunEngine;
use super::server_loop_host::ServerAgenticLoopHostBuilder;

// ─── Skill wiring for server paths ──────────────────────────────────────────

/// Build skill registry + resolver for server-side agentic loops.
///
/// Returns `(registry_for_activation, resolver)` using the global default
/// registry (Local + Bundled providers). Fork-context skills require a
/// `skill_executor` which is CLI-only; the server gets inline resolution only.
fn build_server_skill_resolver() -> (
    Option<Arc<crate::skills::UnifiedSkillRegistry>>,
    Option<Arc<dyn crate::turn::skill_tool::SkillResolver>>,
) {
    use crate::turn::skill_tool::SkillResolver as _;

    let registry = crate::skills::default_unified_registry().clone();
    if registry.is_empty() {
        return (None, None);
    }
    let inner = Arc::new(crate::skills::UnifiedSkillResolver::new(Arc::clone(
        &registry,
    )));
    let adapter = crate::skills::registry::LegacySkillResolverAdapter::new(inner);
    let skills = adapter.available_skills();
    let resolver: Option<Arc<dyn crate::turn::skill_tool::SkillResolver>> = if skills.is_empty() {
        None
    } else {
        Some(Arc::new(adapter))
    };
    (Some(registry), resolver)
}

fn skill_search_from_context(
    context: &std::collections::HashMap<String, serde_json::Value>,
) -> astra_core::SkillSearchSettings {
    context
        .get("skill_search")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

// ─── Run State ──────────────────────────────────────────────────────────────

/// Status of a single agentic run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

impl RunStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Running => STATUS_RUNNING,
            Self::Paused => STATUS_PAUSED,
            Self::Completed => STATUS_COMPLETED,
            Self::Failed => STATUS_FAILED,
            Self::Cancelled => STATUS_CANCELLED,
        }
    }
}

fn is_run_finished_event(event: &Value) -> bool {
    event.get("event_type").and_then(Value::as_str) == Some("run_finished")
}

fn merge_run_finished_event_data(target: &mut Value, source: &Value) {
    let source_data = source
        .get("data")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let Some(target_obj) = target.as_object_mut() else {
        return;
    };
    let target_data = target_obj
        .entry("data".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(target_data_obj) = target_data.as_object_mut() else {
        return;
    };
    for (key, value) in source_data {
        target_data_obj.insert(key, value);
    }
}

fn merge_cancelled_run_events(run: &mut RunState, mut finalized_events: Vec<Value>) {
    let terminal_event = finalized_events
        .last()
        .filter(|event| is_run_finished_event(event))
        .cloned();
    if terminal_event.is_some() {
        finalized_events.pop();
    }

    let insert_at = run
        .events
        .last()
        .filter(|event| is_run_finished_event(event))
        .map(|_| run.events.len().saturating_sub(1))
        .unwrap_or(run.events.len());
    run.events.splice(insert_at..insert_at, finalized_events);

    if let Some(terminal_event) = terminal_event {
        if let Some(existing_terminal) = run
            .events
            .last_mut()
            .filter(|event| is_run_finished_event(event))
        {
            merge_run_finished_event_data(existing_terminal, &terminal_event);
        } else {
            run.events.push(terminal_event);
        }
    }
}

fn terminal_events_for_persistence(events: &[Value]) -> Vec<Value> {
    events
        .iter()
        .filter(|event| {
            matches!(
                event.get("event_type").and_then(Value::as_str),
                Some("run_error" | "run_finished")
            )
        })
        .cloned()
        .collect()
}

/// Per-run state held in the lifecycle service.
struct RunState {
    run_id: String,
    session_id: String,
    user_id: String,
    status: RunStatus,
    events: Vec<Value>,
    cancel_flag: Arc<AtomicBool>,
    /// Cancelled together with `cancel_flag` on `cancel_run` for low-latency LLM abort.
    llm_cancel_token: Arc<CancellationToken>,
    #[allow(dead_code)]
    started_at: Instant,
    waiting_for: Option<String>,
}

// ─── Service ────────────────────────────────────────────────────────────────

/// Production [`RunLifecycleService`] that executes agentic loops via
/// [`ServerAgenticLoopHost`].
///
/// When a `RunEngine` is attached, all state changes are also persisted
/// to the durable store for crash recovery.
pub struct AgenticRunLifecycleService {
    /// In-memory run store (run_id → state). Hot cache for low-latency queries.
    /// Arc-wrapped so background tasks spawned by `create_run` can update events.
    runs: Arc<RwLock<HashMap<String, RunState>>>,
    /// LLM resolution dependencies.
    matrixone: MatrixOneSettings,
    encryptor: Arc<FernetTokenEncryptor>,
    shared_pool: Option<SharedPool>,
    /// Edge callback ledger shared with the API server.
    edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
    /// Optional durable run engine for persistence.
    run_engine: Option<RunEngine>,
    /// Optional delegation engine for multi-agent coordination.
    delegation_engine: Option<Arc<crate::server::delegation_engine::DelegationEngine>>,
}

impl AgenticRunLifecycleService {
    pub fn new(
        matrixone: MatrixOneSettings,
        encryptor: Arc<FernetTokenEncryptor>,
        edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
    ) -> Self {
        Self {
            runs: Arc::new(RwLock::new(HashMap::new())),
            matrixone,
            encryptor,
            shared_pool: None,
            edge_callback_ledger,
            run_engine: None,
            delegation_engine: None,
        }
    }

    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.shared_pool = Some(pool);
        self
    }

    pub fn with_run_engine(mut self, engine: RunEngine) -> Self {
        self.run_engine = Some(engine);
        self
    }

    pub fn with_delegation_engine(
        mut self,
        engine: Arc<crate::server::delegation_engine::DelegationEngine>,
    ) -> Self {
        self.delegation_engine = Some(engine);
        self
    }

    /// Clone the Arc handle to the runs map (for background tasks).
    fn runs_handle(&self) -> Arc<RwLock<HashMap<String, RunState>>> {
        Arc::clone(&self.runs)
    }

    fn build_tracked_run_state(
        run_id: String,
        session_id: String,
        user_id: String,
    ) -> (RunState, Arc<AtomicBool>, Arc<CancellationToken>) {
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let llm_cancel_token = Arc::new(CancellationToken::new());
        let run_state = RunState {
            run_id,
            session_id,
            user_id,
            status: RunStatus::Running,
            events: vec![json!({"event_type": "run_started", "data": {}})],
            cancel_flag: cancel_flag.clone(),
            llm_cancel_token: llm_cancel_token.clone(),
            started_at: Instant::now(),
            waiting_for: None,
        };
        (run_state, cancel_flag, llm_cancel_token)
    }

    fn configure_loop_state_runtime_controls(
        &self,
        loop_state: &mut AgenticLoopState,
        cancel_flag: &Arc<AtomicBool>,
        llm_cancel_token: &Arc<CancellationToken>,
    ) {
        loop_state.cancellation.flag = Some(cancel_flag.clone());
        loop_state.cancellation.token = Some(llm_cancel_token.clone());
        loop_state.delegation_engine = self.delegation_engine.clone();
    }

    async fn persist_run_start_if_configured(&self, run_id: &str, user_id: &str, session_id: &str) {
        if let Some(engine) = &self.run_engine {
            astra_core::log_persist!(
                engine.start_run(run_id, user_id, session_id).await,
                "run_lifecycle",
                run_id,
                "start_run"
            );
        }
    }

    async fn persist_terminal_events_if_configured(&self, run_id: &str, events: &[Value]) {
        let Some(engine) = &self.run_engine else {
            return;
        };
        for event in terminal_events_for_persistence(events) {
            astra_core::log_persist!(
                engine.append_event(run_id, event).await,
                "run_lifecycle",
                run_id,
                "append_terminal_event"
            );
        }
    }

    fn finalize_run_events(
        loop_outcome: Result<AgenticLoopOutcome, String>,
        mut events: Vec<Value>,
        loop_state: &AgenticLoopState,
    ) -> (Vec<Value>, RunStatus, Option<String>) {
        let usage = json!({
            "prompt_tokens": loop_state.total_prompt,
            "completion_tokens": loop_state.total_completion,
            "tool_call_count": loop_state.total_tool_calls,
        });
        let cancellation_requested = loop_state
            .cancellation
            .flag
            .as_ref()
            .is_some_and(|f| f.load(Ordering::Relaxed))
            || loop_state
                .cancellation
                .token
                .as_ref()
                .is_some_and(|t| t.is_cancelled());

        let (final_status, error_msg) = if cancellation_requested
            || matches!(&loop_outcome, Ok(AgenticLoopOutcome::Cancelled))
        {
            let mut data = usage;
            data["cancelled"] = Value::Bool(true);
            events.push(json!({
                "event_type": "run_finished",
                "data": data,
            }));
            (RunStatus::Cancelled, None)
        } else {
            match loop_outcome {
                Ok(AgenticLoopOutcome::Completed) => {
                    if !loop_state.final_text.is_empty() {
                        events.push(json!({
                        "event_type": "text_done",
                        "data": { "full_text": loop_state.final_text.clone() }
                        }));
                    }
                    events.push(json!({
                        "event_type": "run_finished",
                        "data": usage,
                    }));
                    (RunStatus::Completed, None)
                }
                Ok(AgenticLoopOutcome::Cancelled) => unreachable!("handled by cancellation gate"),
                Ok(AgenticLoopOutcome::Error(e)) => {
                    events.push(json!({
                        "event_type": "run_error",
                        "data": {"error": e.clone()}
                    }));
                    events.push(json!({
                        "event_type": "run_finished",
                        "data": usage.clone(),
                    }));
                    (RunStatus::Failed, Some(e))
                }
                Ok(AgenticLoopOutcome::Waiting(w)) => {
                    let msg = format!("waiting: {w}");
                    events.push(json!({
                        "event_type": "run_error",
                        "data": {"error": msg.clone()}
                    }));
                    events.push(json!({
                        "event_type": "run_finished",
                        "data": usage.clone(),
                    }));
                    (RunStatus::Failed, Some(msg))
                }
                Err(err) => {
                    events.push(json!({
                        "event_type": "run_error",
                        "data": {"error": err.clone()}
                    }));
                    events.push(json!({
                        "event_type": "run_finished",
                        "data": usage,
                    }));
                    (RunStatus::Failed, Some(err))
                }
            }
        };

        (events, final_status, error_msg)
    }

    /// Build a [`ServerAgenticLoopHost`] for a single run.
    fn build_host(
        &self,
        user_id: &str,
        session_id: &str,
        request: &ChatRequestData,
        edge_tools: Vec<Value>,
        edge_profile: Map<String, Value>,
    ) -> super::server_loop_host::ServerAgenticLoopHost {
        let mut builder = ServerAgenticLoopHostBuilder::new(
            self.matrixone.clone(),
            self.encryptor.clone(),
            user_id.to_string(),
            session_id.to_string(),
        )
        .with_model(request.model.clone())
        .with_edge_tools(edge_tools)
        .with_edge_profile(edge_profile)
        .with_edge_callback_ledger(self.edge_callback_ledger.clone());

        if let Some(pool) = &self.shared_pool {
            builder = builder.with_pool(pool.clone());
        }
        // Wire progress broadcaster from delegation engine for SSE agent tree events
        if let Some(ref de) = self.delegation_engine {
            if let Some(broadcaster) = de.progress_broadcaster() {
                builder = builder.with_progress_broadcaster(Arc::clone(broadcaster));
            }
        }
        builder.build()
    }

    /// Build the initial [`AgenticLoopState`] from a chat request.
    fn build_initial_state(
        &self,
        request: &ChatRequestData,
        session_id: &str,
        run_id: &str,
    ) -> AgenticLoopState {
        use crate::pipeline::step_protocol::InMemoryIdempotencyCache;
        use crate::semantic_dedup::SemanticDedup;
        use crate::turn::chat_turn_heuristics::infer_task_execution_profile;
        use crate::turn::stop_hooks_yaml::{
            detect_turn_hook_sets, is_plan_subtask_from_chat_context, project_root_for_stop_hooks,
        };

        let (skill_registry, skill_resolver) = build_server_skill_resolver();
        use crate::turn::turn_guard::TurnGuard;

        let user_message = json!({
            "role": "user",
            "content": request.message,
        });

        let max_turns = request.max_candidates.max(1) as usize;
        let task_profile = infer_task_execution_profile(&request.message);
        let edge_ctx = Self::extract_edge_context(request);
        let project_root_buf = project_root_for_stop_hooks(&edge_ctx);
        let hook_sets = project_root_buf
            .as_ref()
            .map(|root| {
                detect_turn_hook_sets(
                    root.as_path(),
                    task_profile,
                    is_plan_subtask_from_chat_context(&request.context),
                )
            })
            .unwrap_or_default();
        let workspace_root_hint = project_root_buf.map(|p| p.to_string_lossy().into_owned());
        let skill_search = request.skill_search.clone().unwrap_or_default();
        let (tool_event_hooks, session_event_hooks) = workspace_root_hint
            .as_ref()
            .map(|root| crate::skills::hooks::load_all_hooks(std::path::Path::new(root)))
            .unwrap_or_default();

        AgenticLoopState {
            messages: vec![user_message],
            tool_results: Vec::new(),
            current_session_id: Some(session_id.to_string()),
            current_run_id: Some(run_id.to_string()),
            final_text: String::new(),
            total_prompt: 0,
            total_completion: 0,
            total_cache_read: 0,
            total_cache_creation: 0,
            total_tool_calls: 0,
            has_any_usage: false,
            max_turns,
            remaining_turns: max_turns,
            turn_guard: TurnGuard::with_profile(task_profile),
            restricted_tools: std::collections::HashSet::new(),
            step_recorder: StepRecorder::new(session_id, run_id),
            idempotency_cache: InMemoryIdempotencyCache::new(),
            semantic_dedup: SemanticDedup::new(0.75),
            call_counts: HashMap::new(),
            max_identical_tool_calls: crate::runtime_config::RuntimeConfig::load()
                .tool_selection
                .effective_max_identical_calls(),
            stall: Default::default(),
            telemetry: Default::default(),
            skills: SkillState {
                registry_for_activation: skill_registry,
                resolver: skill_resolver,
                quality_tracker: crate::skills::quality::SkillQualityTracker::new(),
                improvement_tracker: crate::skills::improvement::ImprovementTracker::new(),
                search: skill_search,
                tool_event_hooks,
                session_event_hooks,
                ..Default::default()
            },
            hooks: StopHookState {
                stop_hooks: hook_sets.stop_hooks,
                teammate_idle_hooks: hook_sets.teammate_idle_hooks,
                workspace_root_hint,
                ..Default::default()
            },
            cancellation: Default::default(),
            messaging: Default::default(),
            error_recovery: Default::default(),
            message: request.message.clone(),
            recent_tools: Vec::new(),
            task_profile,
            api: astra_thin_client::ThinClient::new("http://127.0.0.1:1", None)
                .expect("valid dummy URL"),
            api_token: String::new(),
            delegation_engine: None,
            project_context: None,
            checkpoint_gate: None,
            evolution_service: None,
            rate_limit_cooldown: Default::default(),
            data_snapshot_provider: None,
            last_composite_snapshot: None,
            last_measured_prompt_tokens: None,
            consecutive_context_window_errors: 0,
            max_turn_input_tokens: astra_core::RuntimeLimits::global().max_turn_input_tokens,
            budget_wrapup_injected: false,
            thinking_budget_tokens: None,
            recent_file_reads: Vec::new(),
            permission_context: None,
            permission_handler: None,
            tactical_adapter: None,
            step_signal_collector: None,
            tool_budget_override: None,
            pending_reflection_signals: Vec::new(),
        }
    }

    /// Extract edge tools from the request context, or provide empty defaults.
    /// Parse the request context into a typed [`EdgeContext`].
    fn extract_edge_context(request: &ChatRequestData) -> EdgeContext {
        request
            .context
            .as_ref()
            .map(EdgeContext::from_context_map)
            .unwrap_or_default()
    }

    /// Extract edge tools from the request context, or provide empty defaults.
    fn extract_edge_tools(request: &ChatRequestData) -> Vec<Value> {
        Self::extract_edge_context(request).edge_tools
    }

    /// Extract edge profile from the request context, or provide empty defaults.
    fn extract_edge_profile(request: &ChatRequestData) -> Map<String, Value> {
        Self::extract_edge_context(request).edge_profile.to_map()
    }

    /// Collect run events into SSE-compatible format.
    fn format_run_events(events: &[Value], start_index: usize) -> Vec<Value> {
        events
            .iter()
            .enumerate()
            .map(|(i, ev)| {
                let mut out = ev.clone();
                if let Some(obj) = out.as_object_mut() {
                    obj.insert("index".to_string(), json!(start_index + i));
                }
                out
            })
            .collect()
    }

    fn status_record(run: &RunState) -> RunStatusRecord {
        RunStatusRecord {
            run_id: run.run_id.clone(),
            session_id: run.session_id.clone(),
            status: run.status.as_str().to_string(),
            waiting_for: run.waiting_for.clone(),
            events_count: run.events.len() as i64,
        }
    }

    #[cfg(test)]
    pub(crate) async fn test_llm_cancel_token_is_cancelled(&self, run_id: &str) -> Option<bool> {
        let runs = self.runs.read().await;
        runs.get(run_id).map(|r| r.llm_cancel_token.is_cancelled())
    }
}

#[async_trait]
impl RunLifecycleService for AgenticRunLifecycleService {
    /// Create a run (background mode): spawns the agentic loop in a task, returns immediately.
    async fn create_run(
        &self,
        user_id: String,
        request: ChatRequestData,
    ) -> Result<ChatRunRecord, (StatusCode, Json<ErrorResponse>)> {
        let run_id = Uuid::new_v4().to_string();
        let session_id = request
            .session_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        let (run_state, cancel_flag, llm_cancel_token) =
            Self::build_tracked_run_state(run_id.clone(), session_id.clone(), user_id.clone());
        self.runs.write().await.insert(run_id.clone(), run_state);

        // Persist to durable store if available
        self.persist_run_start_if_configured(&run_id, &user_id, &session_id)
            .await;

        // Spawn background agentic loop.
        let edge_tools = Self::extract_edge_tools(&request);
        let edge_profile = Self::extract_edge_profile(&request);
        let mut host = self.build_host(&user_id, &session_id, &request, edge_tools, edge_profile);
        let mut loop_state = self.build_initial_state(&request, &session_id, &run_id);
        self.configure_loop_state_runtime_controls(
            &mut loop_state,
            &cancel_flag,
            &llm_cancel_token,
        );

        // Clone handles we need inside the spawned task.
        let runs = self.runs_handle();
        let run_engine = self.run_engine.clone();
        let bg_run_id = run_id.clone();

        tokio::spawn(async move {
            let outcome = run_agentic_loop_with_host(&mut host, &mut loop_state).await;

            // Fire SessionEnd hooks (best-effort, non-blocking).
            crate::skills::hooks::fire_session_end(
                &loop_state.skills.session_event_hooks,
                loop_state.current_session_id.as_deref().unwrap_or(""),
            )
            .await;

            let (events, final_status, error_msg) =
                Self::finalize_run_events(outcome, host.take_emitted_events(), &loop_state);
            let terminal_events = terminal_events_for_persistence(&events);

            // Persist final status + usage to durable store.
            let status_str = final_status.as_str();
            let mut persist_terminal_state = true;

            // Update in-memory state with collected events and final status.
            if let Some(run) = runs.write().await.get_mut(&bg_run_id) {
                if run.status == RunStatus::Cancelled {
                    persist_terminal_state = false;
                    merge_cancelled_run_events(run, events);
                } else {
                    run.events.extend(events);
                    run.status = final_status;
                }
            }

            if let Some(engine) = &run_engine {
                if persist_terminal_state {
                    astra_core::log_persist!(
                        engine
                            .persist_status(&bg_run_id, status_str, None, error_msg.as_deref())
                            .await,
                        "run_lifecycle",
                        &bg_run_id,
                        "status"
                    );
                }
                astra_core::log_persist!(
                    engine
                        .persist_usage(
                            &bg_run_id,
                            loop_state.total_prompt,
                            loop_state.total_completion,
                            loop_state.total_tool_calls,
                        )
                        .await,
                    "run_lifecycle",
                    &bg_run_id,
                    "usage"
                );
                if persist_terminal_state {
                    for event in terminal_events {
                        astra_core::log_persist!(
                            engine.append_event(&bg_run_id, event).await,
                            "run_lifecycle",
                            &bg_run_id,
                            "append_terminal_event"
                        );
                    }
                }
            }
        });

        Ok(ChatRunRecord {
            session_id,
            run_id,
            status: STATUS_RUNNING.to_string(),
            explain: if request.explain {
                Some(json!({"mode": "background"}))
            } else {
                None
            },
        })
    }

    /// Stream chat (synchronous mode): runs the full agentic loop, returns all events.
    async fn stream_chat(
        &self,
        user_id: String,
        request: ChatRequestData,
    ) -> Result<ChatStreamRecord, (StatusCode, Json<ErrorResponse>)> {
        let run_id = Uuid::new_v4().to_string();
        let session_id = request
            .session_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        let edge_tools = Self::extract_edge_tools(&request);
        let edge_profile = Self::extract_edge_profile(&request);

        // If no edge tools are provided, return a minimal "no tools" response.
        // The client (CLI thin-client) is expected to provide edge_tools in context.
        let mut host = self.build_host(&user_id, &session_id, &request, edge_tools, edge_profile);
        let mut state = self.build_initial_state(&request, &session_id, &run_id);
        let (run_state, cancel_flag, llm_cancel_token) =
            Self::build_tracked_run_state(run_id.clone(), session_id.clone(), user_id.clone());
        self.runs.write().await.insert(run_id.clone(), run_state);
        self.persist_run_start_if_configured(&run_id, &user_id, &session_id)
            .await;
        self.configure_loop_state_runtime_controls(&mut state, &cancel_flag, &llm_cancel_token);

        let loop_result = run_agentic_loop_with_host(&mut host, &mut state).await;

        // Fire SessionEnd hooks (best-effort).
        crate::skills::hooks::fire_session_end(
            &state.skills.session_event_hooks,
            state.current_session_id.as_deref().unwrap_or(""),
        )
        .await;

        let (mut final_events, final_status, error_msg) =
            Self::finalize_run_events(loop_result, host.take_emitted_events(), &state);
        let mut all_events = vec![json!({"event_type": "run_started", "data": {}})];
        all_events.append(&mut final_events);

        if let Some(run) = self.runs.write().await.get_mut(&run_id) {
            run.events = all_events.clone();
            run.status = final_status.clone();
        }

        if let Some(engine) = &self.run_engine {
            astra_core::log_persist!(
                engine
                    .persist_status(&run_id, final_status.as_str(), None, error_msg.as_deref())
                    .await,
                "run_lifecycle",
                &run_id,
                "status"
            );
            astra_core::log_persist!(
                engine
                    .persist_usage(
                        &run_id,
                        state.total_prompt,
                        state.total_completion,
                        state.total_tool_calls,
                    )
                    .await,
                "run_lifecycle",
                &run_id,
                "usage"
            );
        }
        self.persist_terminal_events_if_configured(&run_id, &all_events[1..])
            .await;

        Ok(ChatStreamRecord {
            session_id,
            run_id,
            events: all_events,
        })
    }

    async fn get_run_status(
        &self,
        run_id: String,
        user_id: String,
    ) -> Result<RunStatusRecord, (StatusCode, Json<ErrorResponse>)> {
        let runs = self.runs.read().await;
        let run = runs
            .get(&run_id)
            .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "Run not found"))?;
        if run.user_id != user_id {
            return Err(error_response(StatusCode::FORBIDDEN, "Access denied"));
        }
        Ok(Self::status_record(run))
    }

    async fn stream_run(
        &self,
        run_id: String,
        user_id: String,
        last_index: u32,
    ) -> Result<Vec<Value>, (StatusCode, Json<ErrorResponse>)> {
        let runs = self.runs.read().await;
        let run = runs
            .get(&run_id)
            .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "Run not found"))?;
        if run.user_id != user_id {
            return Err(error_response(StatusCode::FORBIDDEN, "Access denied"));
        }
        let offset = last_index as usize;
        let events = if offset < run.events.len() {
            Self::format_run_events(&run.events[offset..], offset)
        } else {
            Vec::new()
        };
        Ok(events)
    }

    async fn cancel_run(
        &self,
        run_id: String,
        user_id: String,
    ) -> Result<CancelRunRecord, (StatusCode, Json<ErrorResponse>)> {
        let mut runs = self.runs.write().await;
        let run = runs
            .get_mut(&run_id)
            .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "Run not found"))?;
        if run.user_id != user_id {
            return Err(error_response(StatusCode::FORBIDDEN, "Access denied"));
        }
        if run.status == RunStatus::Running {
            run.cancel_flag.store(true, Ordering::SeqCst);
            run.llm_cancel_token.cancel();
            run.status = RunStatus::Cancelled;
            run.events.push(json!({
                "event_type": "run_finished",
                "data": {"cancelled": true}
            }));
            // Persist cancellation
            if let Some(engine) = &self.run_engine {
                astra_core::log_persist!(
                    engine
                        .persist_status(&run_id, STATUS_CANCELLED, None, None)
                        .await,
                    "run_lifecycle",
                    &run_id,
                    "cancel_status"
                );
                astra_core::log_persist!(
                    engine
                        .append_event(
                            &run_id,
                            json!({"event_type": "run_finished", "data": {"cancelled": true}}),
                        )
                        .await,
                    "run_lifecycle",
                    &run_id,
                    "cancel_event"
                );
            }
        }
        Ok(CancelRunRecord {
            run_id,
            status: run.status.as_str().to_string(),
        })
    }

    async fn list_runs(
        &self,
        user_id: String,
        limit: u32,
        offset: u32,
    ) -> Result<RunListRecord, (StatusCode, Json<ErrorResponse>)> {
        let runs = self.runs.read().await;
        let all: Vec<RunStatusRecord> = runs
            .values()
            .filter(|run| run.user_id == user_id)
            .map(Self::status_record)
            .collect();
        let total = all.len() as i64;
        let start = (offset as usize).min(all.len());
        let end = (start + limit as usize).min(all.len());
        let page = all[start..end].to_vec();
        Ok(RunListRecord {
            runs: page,
            total,
            limit,
            offset,
        })
    }

    async fn pause_run(
        &self,
        run_id: String,
        user_id: String,
    ) -> Result<RunMutationRecord, (StatusCode, Json<ErrorResponse>)> {
        let mut runs = self.runs.write().await;
        let run = runs
            .get_mut(&run_id)
            .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "Run not found"))?;
        if run.user_id != user_id {
            return Err(error_response(StatusCode::FORBIDDEN, "Access denied"));
        }
        if run.status != RunStatus::Running {
            return Err(error_response(
                StatusCode::CONFLICT,
                format!("Cannot pause run in '{}' state", run.status.as_str()),
            ));
        }
        let previous = run.status.as_str().to_string();
        run.status = RunStatus::Paused;
        run.waiting_for = Some("user_resume".to_string());
        run.events.push(json!({
            "event_type": "run_paused",
            "data": {}
        }));
        // Drop the write lock before async delegation calls.
        drop(runs);

        // Persist pause
        if let Some(engine) = &self.run_engine {
            astra_core::log_persist!(
                engine
                    .persist_status(&run_id, STATUS_PAUSED, Some("user_resume"), None)
                    .await,
                "run_lifecycle",
                &run_id,
                "pause_status"
            );
            astra_core::log_persist!(
                engine
                    .append_event(&run_id, json!({"event_type": "run_paused", "data": {}}))
                    .await,
                "run_lifecycle",
                &run_id,
                "pause_event"
            );
        }
        // Cascade: pause all delegated sub-runs of this parent.
        if let Some(de) = &self.delegation_engine {
            de.pause_children_of(&run_id).await;
        }
        Ok(RunMutationRecord {
            run_id,
            status: STATUS_PAUSED.to_string(),
            previous_status: previous,
        })
    }

    async fn resume_run(
        &self,
        run_id: String,
        user_id: String,
    ) -> Result<RunMutationRecord, (StatusCode, Json<ErrorResponse>)> {
        let mut runs = self.runs.write().await;
        let run = runs
            .get_mut(&run_id)
            .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "Run not found"))?;
        if run.user_id != user_id {
            return Err(error_response(StatusCode::FORBIDDEN, "Access denied"));
        }
        if run.status != RunStatus::Paused {
            return Err(error_response(
                StatusCode::CONFLICT,
                format!("Cannot resume run in '{}' state", run.status.as_str()),
            ));
        }
        let previous = run.status.as_str().to_string();
        run.status = RunStatus::Running;
        run.waiting_for = None;
        run.events.push(json!({
            "event_type": "run_resumed",
            "data": {}
        }));
        // Drop the write lock before async delegation calls.
        drop(runs);

        // Persist resume
        if let Some(engine) = &self.run_engine {
            astra_core::log_persist!(
                engine
                    .persist_status(&run_id, STATUS_RUNNING, None, None)
                    .await,
                "run_lifecycle",
                &run_id,
                "resume_status"
            );
            astra_core::log_persist!(
                engine
                    .append_event(&run_id, json!({"event_type": "run_resumed", "data": {}}))
                    .await,
                "run_lifecycle",
                &run_id,
                "resume_event"
            );
        }
        // Cascade: resume all delegated sub-runs of this parent.
        if let Some(de) = &self.delegation_engine {
            de.resume_children_of(&run_id).await;
        }
        Ok(RunMutationRecord {
            run_id,
            status: STATUS_RUNNING.to_string(),
            previous_status: previous,
        })
    }
}

// ─── Sub-Run Executor ───────────────────────────────────────────────────────

use crate::server::delegation_engine::{SubRunConfig, SubRunExecutor};

/// Production sub-run executor backed by [`ServerAgenticLoopHost`].
///
/// Creates a real agentic loop for each sub-run with the agent's system prompt,
/// model, and tool configuration.
pub struct ServerSubRunExecutor {
    matrixone: MatrixOneSettings,
    encryptor: Arc<FernetTokenEncryptor>,
    shared_pool: Option<SharedPool>,
    edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
}

impl ServerSubRunExecutor {
    pub fn new(
        matrixone: MatrixOneSettings,
        encryptor: Arc<FernetTokenEncryptor>,
        edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
    ) -> Self {
        Self {
            matrixone,
            encryptor,
            shared_pool: None,
            edge_callback_ledger,
        }
    }

    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.shared_pool = Some(pool);
        self
    }
}

#[async_trait]
impl SubRunExecutor for ServerSubRunExecutor {
    async fn execute(
        &self,
        config: SubRunConfig,
    ) -> Result<astra_services::coordination::AgentResult, String> {
        use crate::pipeline::step_protocol::InMemoryIdempotencyCache;
        use crate::semantic_dedup::SemanticDedup;
        use crate::turn::chat_turn_heuristics::infer_task_execution_profile;
        use crate::turn::stop_hooks_yaml::{
            detect_turn_hook_sets, is_plan_subtask_from_delegation_context,
            project_root_from_delegation_context,
        };
        use crate::turn::turn_guard::TurnGuard;

        // Build edge profile from agent's system prompt and metadata.
        let mut edge_profile = Map::new();
        if let Some(prompt) = &config.agent_profile.system_prompt {
            edge_profile.insert(
                "system_prompt_override".to_string(),
                Value::String(prompt.clone()),
            );
        }
        if let Some(model) = &config.agent_profile.model_override {
            edge_profile.insert("model".to_string(), Value::String(model.clone()));
        }
        edge_profile.insert(
            "agent_id".to_string(),
            Value::String(config.agent_profile.agent_id.clone()),
        );

        // Build the host with agent-specific configuration.
        let mut builder = ServerAgenticLoopHostBuilder::new(
            self.matrixone.clone(),
            self.encryptor.clone(),
            config.user_id.clone(),
            config.session_id.clone(),
        )
        .with_model(config.agent_profile.model_override.clone())
        .with_edge_profile(edge_profile)
        .with_edge_callback_ledger(self.edge_callback_ledger.clone());

        if let Some(pool) = &self.shared_pool {
            builder = builder.with_pool(pool.clone());
        }
        let mut host = builder.build();

        // Build the task prompt, incorporating previous output if pipeline.
        let full_task = if let Some(prev) = &config.previous_output {
            format!("{}\n\nPrevious agent output:\n{}", config.task, prev)
        } else {
            config.task.clone()
        };

        let user_message = json!({
            "role": "user",
            "content": full_task,
        });

        let task_profile = infer_task_execution_profile(&full_task);
        let project_root_buf = project_root_from_delegation_context(&config.context);
        let hook_sets = project_root_buf
            .as_ref()
            .map(|root| {
                detect_turn_hook_sets(
                    root.as_path(),
                    task_profile,
                    is_plan_subtask_from_delegation_context(&config.context),
                )
            })
            .unwrap_or_default();
        let workspace_root_hint = project_root_buf.map(|p| p.to_string_lossy().into_owned());
        let (tool_event_hooks, session_event_hooks) = workspace_root_hint
            .as_ref()
            .map(|root| crate::skills::hooks::load_all_hooks(std::path::Path::new(root)))
            .unwrap_or_default();

        let (skill_registry, skill_resolver) = build_server_skill_resolver();

        let mut loop_state = AgenticLoopState {
            messages: vec![user_message],
            tool_results: Vec::new(),
            current_session_id: Some(config.session_id.clone()),
            current_run_id: Some(config.run_id.clone()),
            final_text: String::new(),
            total_prompt: 0,
            total_completion: 0,
            total_cache_read: 0,
            total_cache_creation: 0,
            total_tool_calls: 0,
            has_any_usage: false,
            max_turns: 10,
            remaining_turns: 10,
            turn_guard: TurnGuard::new(),
            restricted_tools: std::collections::HashSet::new(),
            step_recorder: StepRecorder::new(&config.session_id, &config.run_id),
            idempotency_cache: InMemoryIdempotencyCache::new(),
            semantic_dedup: SemanticDedup::new(0.75),
            call_counts: HashMap::new(),
            max_identical_tool_calls: crate::runtime_config::RuntimeConfig::load()
                .tool_selection
                .effective_max_identical_calls(),
            stall: Default::default(),
            telemetry: Default::default(),
            skills: SkillState {
                registry_for_activation: skill_registry,
                resolver: skill_resolver,
                quality_tracker: crate::skills::quality::SkillQualityTracker::new(),
                improvement_tracker: crate::skills::improvement::ImprovementTracker::new(),
                search: skill_search_from_context(&config.context),
                tool_event_hooks,
                session_event_hooks,
                ..Default::default()
            },
            hooks: StopHookState {
                stop_hooks: hook_sets.stop_hooks,
                teammate_idle_hooks: hook_sets.teammate_idle_hooks,
                workspace_root_hint,
                ..Default::default()
            },
            cancellation: CancellationState {
                flag: config.pause_flag.clone(),
                token: None,
            },
            messaging: MessagingState {
                mailbox: config.mailbox,
                ..Default::default()
            },
            error_recovery: Default::default(),
            message: full_task,
            recent_tools: Vec::new(),
            task_profile,
            api: astra_thin_client::ThinClient::new("http://127.0.0.1:1", None)
                .expect("valid dummy URL"),
            api_token: String::new(),
            delegation_engine: None,
            project_context: None,
            checkpoint_gate: config.checkpoint_gate.clone(),
            evolution_service: None,
            rate_limit_cooldown: Default::default(),
            data_snapshot_provider: None,
            last_composite_snapshot: None,
            last_measured_prompt_tokens: None,
            consecutive_context_window_errors: 0,
            max_turn_input_tokens: astra_core::RuntimeLimits::global().max_turn_input_tokens,
            budget_wrapup_injected: false,
            thinking_budget_tokens: None,
            recent_file_reads: Vec::new(),
            permission_context: None,
            permission_handler: None,
            tactical_adapter: None,
            step_signal_collector: None,
            tool_budget_override: None,
            pending_reflection_signals: Vec::new(),
        };

        let outcome = run_agentic_loop_with_host(&mut host, &mut loop_state).await;

        // Fire SessionEnd hooks (best-effort).
        crate::skills::hooks::fire_session_end(
            &loop_state.skills.session_event_hooks,
            loop_state.current_session_id.as_deref().unwrap_or(""),
        )
        .await;

        match outcome {
            Ok(AgenticLoopOutcome::Completed) => Ok(astra_services::coordination::AgentResult {
                agent_id: config.agent_profile.agent_id,
                run_id: config.run_id,
                status: STATUS_COMPLETED.to_string(),
                output: if loop_state.final_text.is_empty() {
                    None
                } else {
                    Some(loop_state.final_text)
                },
                error: None,
                prompt_tokens: loop_state.total_prompt,
                completion_tokens: loop_state.total_completion,
                tool_calls: loop_state.total_tool_calls,
            }),
            Ok(AgenticLoopOutcome::Cancelled) => {
                // Cancelled via pause_flag — report as "paused" so the
                // delegation engine can distinguish from hard errors.
                Ok(astra_services::coordination::AgentResult {
                    agent_id: config.agent_profile.agent_id,
                    run_id: config.run_id,
                    status: STATUS_PAUSED.to_string(),
                    output: if loop_state.final_text.is_empty() {
                        None
                    } else {
                        Some(loop_state.final_text)
                    },
                    error: None,
                    prompt_tokens: loop_state.total_prompt,
                    completion_tokens: loop_state.total_completion,
                    tool_calls: loop_state.total_tool_calls,
                })
            }
            Ok(AgenticLoopOutcome::Waiting(reason)) => {
                Ok(astra_services::coordination::AgentResult {
                    agent_id: config.agent_profile.agent_id,
                    run_id: config.run_id,
                    status: STATUS_WAITING.to_string(),
                    output: Some(reason),
                    error: None,
                    prompt_tokens: loop_state.total_prompt,
                    completion_tokens: loop_state.total_completion,
                    tool_calls: loop_state.total_tool_calls,
                })
            }
            Ok(AgenticLoopOutcome::Error(err)) | Err(err) => {
                Ok(astra_services::coordination::AgentResult {
                    agent_id: config.agent_profile.agent_id,
                    run_id: config.run_id,
                    status: STATUS_FAILED.to_string(),
                    output: None,
                    error: Some(err),
                    prompt_tokens: loop_state.total_prompt,
                    completion_tokens: loop_state.total_completion,
                    tool_calls: loop_state.total_tool_calls,
                })
            }
        }
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Unwrap a `Result<T, (StatusCode, Json<ErrorResponse>)>` in tests.
    fn ok<T>(result: Result<T, (StatusCode, Json<ErrorResponse>)>) -> T {
        match result {
            Ok(v) => v,
            Err((status, body)) => panic!("expected Ok, got {status}: {}", body.0.detail),
        }
    }

    /// Unwrap the error side.
    fn err<T>(
        result: Result<T, (StatusCode, Json<ErrorResponse>)>,
    ) -> (StatusCode, Json<ErrorResponse>) {
        match result {
            Ok(_) => panic!("expected Err, got Ok"),
            Err(e) => e,
        }
    }

    fn test_settings() -> MatrixOneSettings {
        MatrixOneSettings {
            host: "localhost".to_string(),
            port: 6001,
            user: "test".to_string(),
            password: "test".to_string(),
            database: "test".to_string(),
        }
    }

    fn test_encryptor() -> Arc<FernetTokenEncryptor> {
        Arc::new(FernetTokenEncryptor::new("cJ8pxr3t6iJmSYqe6wD7vu2rN_C3ovGUxkC5H3NXFNY=").unwrap())
    }

    fn test_service() -> AgenticRunLifecycleService {
        AgenticRunLifecycleService::new(
            test_settings(),
            test_encryptor(),
            Arc::new(TokioMutex::new(HashMap::new())),
        )
    }

    fn test_request(message: &str) -> ChatRequestData {
        ChatRequestData {
            message: message.to_string(),
            session_id: None,
            agent_id: None,
            model: None,
            skill_search: None,
            context: None,
            max_candidates: 5,
            explain: false,
        }
    }

    #[test]
    fn finalize_run_events_appends_run_finished_for_failures() {
        let svc = test_service();
        let request = test_request("boom");
        let state = svc.build_initial_state(&request, "session-1", "run-1");

        let (events, status, error) = AgenticRunLifecycleService::finalize_run_events(
            Ok(AgenticLoopOutcome::Error("boom".into())),
            vec![],
            &state,
        );

        assert_eq!(status, RunStatus::Failed);
        assert_eq!(error.as_deref(), Some("boom"));
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["event_type"], "run_error");
        assert_eq!(events[1]["event_type"], "run_finished");
    }

    #[test]
    fn finalize_run_events_cancellation_beats_completed_outcome() {
        let svc = test_service();
        let request = test_request("done");
        let mut state = svc.build_initial_state(&request, "session-1", "run-1");
        let cancel_flag = Arc::new(AtomicBool::new(true));
        let cancel_token = Arc::new(CancellationToken::new());
        cancel_token.cancel();
        state.cancellation.flag = Some(cancel_flag);
        state.cancellation.token = Some(cancel_token);

        let (events, status, error) = AgenticRunLifecycleService::finalize_run_events(
            Ok(AgenticLoopOutcome::Completed),
            vec![],
            &state,
        );

        assert_eq!(status, RunStatus::Cancelled);
        assert!(error.is_none());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["event_type"], "run_finished");
        assert_eq!(events[0]["data"]["cancelled"], true);
    }

    #[test]
    fn merge_cancelled_run_events_preserves_order_and_usage() {
        let cancel_flag = Arc::new(AtomicBool::new(true));
        let cancel_token = Arc::new(CancellationToken::new());
        let mut run = RunState {
            run_id: "run-1".into(),
            session_id: "session-1".into(),
            user_id: "user-1".into(),
            status: RunStatus::Cancelled,
            events: vec![
                json!({"event_type": "run_started", "data": {}}),
                json!({"event_type": "run_finished", "data": {"cancelled": true}}),
            ],
            cancel_flag,
            llm_cancel_token: cancel_token,
            started_at: Instant::now(),
            waiting_for: None,
        };

        merge_cancelled_run_events(
            &mut run,
            vec![
                json!({"event_type": "text_delta", "data": {"chunk": "hi"}}),
                json!({"event_type": "run_finished", "data": {"cancelled": true, "prompt_tokens": 3}}),
            ],
        );

        assert_eq!(run.events.len(), 3);
        assert_eq!(run.events[1]["event_type"], "text_delta");
        assert_eq!(run.events[2]["event_type"], "run_finished");
        assert_eq!(run.events[2]["data"]["cancelled"], true);
        assert_eq!(run.events[2]["data"]["prompt_tokens"], 3);
    }

    #[test]
    fn terminal_events_for_persistence_keeps_only_terminal_lifecycle_events() {
        let events = vec![
            json!({"event_type": "text_delta", "data": {"chunk": "hi"}}),
            json!({"event_type": "run_error", "data": {"error": "boom"}}),
            json!({"event_type": "run_finished", "data": {"prompt_tokens": 1}}),
        ];

        let persisted = terminal_events_for_persistence(&events);
        assert_eq!(persisted.len(), 2);
        assert_eq!(persisted[0]["event_type"], "run_error");
        assert_eq!(persisted[1]["event_type"], "run_finished");
    }

    #[tokio::test]
    async fn create_run_returns_running_status() {
        let svc = test_service();
        let result = ok(svc.create_run("user-1".into(), test_request("hello")).await);
        assert_eq!(result.status, "running");
        assert!(!result.run_id.is_empty());
        assert!(!result.session_id.is_empty());
    }

    #[tokio::test]
    async fn create_run_uses_provided_session_id() {
        let svc = test_service();
        let mut req = test_request("hi");
        req.session_id = Some("custom-session".into());
        let result = ok(svc.create_run("user-1".into(), req).await);
        assert_eq!(result.session_id, "custom-session");
    }

    #[tokio::test]
    async fn create_run_explain_mode_returns_metadata() {
        let svc = test_service();
        let mut req = test_request("explain me");
        req.explain = true;
        let result = ok(svc.create_run("user-1".into(), req).await);
        assert!(result.explain.is_some());
        assert_eq!(result.explain.unwrap()["mode"], "background");
    }

    #[tokio::test]
    async fn stream_chat_tracks_run_for_status_and_replay() {
        let svc = test_service();
        let stream = ok(svc
            .stream_chat("user-1".into(), test_request("hello"))
            .await);

        let status = ok(svc
            .get_run_status(stream.run_id.clone(), "user-1".into())
            .await);
        let replay = ok(svc
            .stream_run(stream.run_id.clone(), "user-1".into(), 0)
            .await);

        assert_ne!(status.status, "running");
        assert_eq!(status.run_id, stream.run_id);
        assert_eq!(status.events_count as usize, stream.events.len());
        assert_eq!(replay.len(), stream.events.len());
        assert_eq!(replay[0]["event_type"], "run_started");
        assert_eq!(
            svc.test_llm_cancel_token_is_cancelled(&stream.run_id).await,
            Some(false)
        );
    }

    #[tokio::test]
    async fn get_run_status_returns_state() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);
        let status = ok(svc
            .get_run_status(run.run_id.clone(), "user-1".into())
            .await);
        assert_eq!(status.run_id, run.run_id);
        assert_eq!(status.status, "running");
        assert_eq!(status.events_count, 1);
    }

    #[tokio::test]
    async fn get_run_status_not_found() {
        let svc = test_service();
        let e = err(svc
            .get_run_status("nonexistent".into(), "user-1".into())
            .await);
        assert_eq!(e.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_run_status_forbidden_for_other_user() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);
        let e = err(svc.get_run_status(run.run_id, "user-2".into()).await);
        assert_eq!(e.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn cancel_run_sets_cancelled_status() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        let result = ok(svc.cancel_run(run.run_id.clone(), "user-1".into()).await);
        assert_eq!(result.status, "cancelled");
        let status = ok(svc.get_run_status(run.run_id, "user-1".into()).await);
        assert_eq!(status.status, "cancelled");
        assert!(status.events_count >= 1);
    }

    #[tokio::test]
    async fn cancel_run_cancels_llm_token_for_inflight_wake() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        assert_eq!(
            svc.test_llm_cancel_token_is_cancelled(&run.run_id).await,
            Some(false)
        );
        ok(svc.cancel_run(run.run_id.clone(), "user-1".into()).await);
        assert_eq!(
            svc.test_llm_cancel_token_is_cancelled(&run.run_id).await,
            Some(true)
        );
    }

    #[tokio::test]
    async fn cancel_run_idempotent_for_non_running() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        ok(svc.cancel_run(run.run_id.clone(), "user-1".into()).await);
        let result = ok(svc.cancel_run(run.run_id.clone(), "user-1".into()).await);
        assert_eq!(result.status, "cancelled");
    }

    #[tokio::test]
    async fn cancel_run_forbidden_for_other_user() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        let e = err(svc.cancel_run(run.run_id, "user-2".into()).await);
        assert_eq!(e.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn stream_run_returns_events_from_offset() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);
        let events = ok(svc.stream_run(run.run_id.clone(), "user-1".into(), 0).await);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["event_type"], "run_started");
        let events = ok(svc.stream_run(run.run_id, "user-1".into(), 1).await);
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn stream_run_not_found() {
        let svc = test_service();
        let e = err(svc
            .stream_run("nonexistent".into(), "user-1".into(), 0)
            .await);
        assert_eq!(e.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn list_runs_empty_initially() {
        let svc = test_service();
        let result = ok(svc.list_runs("user-1".into(), 10, 0).await);
        assert_eq!(result.total, 0);
        assert!(result.runs.is_empty());
    }

    #[tokio::test]
    async fn list_runs_filters_by_user() {
        let svc = test_service();
        ok(svc.create_run("user-1".into(), test_request("a")).await);
        ok(svc.create_run("user-2".into(), test_request("b")).await);
        ok(svc.create_run("user-1".into(), test_request("c")).await);
        let result = ok(svc.list_runs("user-1".into(), 10, 0).await);
        assert_eq!(result.total, 2);
        assert!(result.runs.iter().all(|r| r.status == "running"));
    }

    #[tokio::test]
    async fn list_runs_pagination() {
        let svc = test_service();
        for i in 0..5 {
            ok(svc
                .create_run("user-1".into(), test_request(&format!("msg {i}")))
                .await);
        }
        let page1 = ok(svc.list_runs("user-1".into(), 2, 0).await);
        assert_eq!(page1.runs.len(), 2);
        assert_eq!(page1.total, 5);
        let page2 = ok(svc.list_runs("user-1".into(), 2, 2).await);
        assert_eq!(page2.runs.len(), 2);
        let page3 = ok(svc.list_runs("user-1".into(), 2, 4).await);
        assert_eq!(page3.runs.len(), 1);
    }

    #[test]
    fn format_run_events_adds_index() {
        let events = vec![
            json!({"event_type": "run_started"}),
            json!({"event_type": "text_delta", "data": {"chunk": "hi"}}),
        ];
        let formatted = AgenticRunLifecycleService::format_run_events(&events, 0);
        assert_eq!(formatted[0]["index"], 0);
        assert_eq!(formatted[1]["index"], 1);
        assert_eq!(formatted[1]["event_type"], "text_delta");
    }

    #[test]
    fn format_run_events_preserves_global_offset() {
        let events = vec![
            json!({"event_type": "text_delta", "data": {"chunk": "a"}}),
            json!({"event_type": "text_delta", "data": {"chunk": "b"}}),
        ];
        let formatted = AgenticRunLifecycleService::format_run_events(&events, 5);
        assert_eq!(formatted[0]["index"], 5);
        assert_eq!(formatted[1]["index"], 6);
    }

    #[test]
    fn extract_edge_tools_from_context() {
        let mut ctx = serde_json::Map::new();
        ctx.insert(
            "edge_tools".to_string(),
            json!([{"function": {"name": "bash"}}]),
        );
        let req = ChatRequestData {
            message: "hi".into(),
            session_id: None,
            agent_id: None,
            model: None,
            skill_search: None,
            context: Some(ctx),
            max_candidates: 5,
            explain: false,
        };
        let tools = AgenticRunLifecycleService::extract_edge_tools(&req);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["function"]["name"], "bash");
    }

    #[test]
    fn extract_edge_tools_empty_when_no_context() {
        assert!(AgenticRunLifecycleService::extract_edge_tools(&test_request("hi")).is_empty());
    }

    #[test]
    fn extract_edge_profile_from_context() {
        let mut ctx = serde_json::Map::new();
        ctx.insert(
            "edge_profile".to_string(),
            json!({"cwd": "/tmp", "git_branch": "main"}),
        );
        let req = ChatRequestData {
            message: "hi".into(),
            session_id: None,
            agent_id: None,
            model: None,
            skill_search: None,
            context: Some(ctx),
            max_candidates: 5,
            explain: false,
        };
        let profile = AgenticRunLifecycleService::extract_edge_profile(&req);
        assert_eq!(profile["cwd"], "/tmp");
        assert_eq!(profile["git_branch"], "main");
    }

    #[test]
    fn build_initial_state_sets_user_message() {
        let svc = test_service();
        let req = test_request("write a test");
        let state = svc.build_initial_state(&req, "sess-1", "run-1");
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0]["role"], "user");
        assert_eq!(state.messages[0]["content"], "write a test");
        assert_eq!(state.current_session_id, Some("sess-1".to_string()));
        assert_eq!(state.current_run_id, Some("run-1".to_string()));
        assert_eq!(state.max_turns, 5);
        assert_eq!(state.remaining_turns, 5);
        assert_eq!(state.message, "write a test");
        assert!(state.cancellation.token.is_none());
    }

    #[test]
    fn build_initial_state_clamps_max_turns() {
        let svc = test_service();
        let mut req = test_request("go");
        req.max_candidates = 0;
        let state = svc.build_initial_state(&req, "s", "r");
        assert_eq!(state.max_turns, 1);
    }

    #[test]
    fn build_initial_state_loads_stop_hooks_from_edge_profile_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let mo = dir.path().join(".astra");
        std::fs::create_dir_all(&mo).unwrap();
        std::fs::write(
            mo.join("stop-hooks.yaml"),
            "version: 1\nauto_detect: false\nhooks:\n  - label: cloud_hook\n    command: true\n",
        )
        .unwrap();

        let svc = test_service();
        let mut req = test_request("implement a fix");
        req.context = Some(
            serde_json::json!({
                "edge_profile": { "cwd": dir.path().to_str().unwrap() }
            })
            .as_object()
            .unwrap()
            .clone(),
        );

        let state = svc.build_initial_state(&req, "s", "r");
        assert_eq!(state.hooks.stop_hooks.len(), 1);
        assert_eq!(state.hooks.stop_hooks[0].label, "cloud_hook");
        assert_eq!(
            state.hooks.workspace_root_hint.as_deref(),
            Some(dir.path().to_str().unwrap())
        );
    }

    #[test]
    fn run_status_as_str() {
        assert_eq!(RunStatus::Running.as_str(), "running");
        assert_eq!(RunStatus::Completed.as_str(), "completed");
        assert_eq!(RunStatus::Failed.as_str(), "failed");
        assert_eq!(RunStatus::Cancelled.as_str(), "cancelled");
        assert_eq!(RunStatus::Paused.as_str(), "paused");
    }

    #[tokio::test]
    async fn pause_run_transitions_running_to_paused() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        let result = ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
        assert_eq!(result.status, "paused");
        assert_eq!(result.previous_status, "running");
        let status = ok(svc.get_run_status(run.run_id, "user-1".into()).await);
        assert_eq!(status.status, "paused");
    }

    #[tokio::test]
    async fn pause_run_conflict_when_not_running() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        ok(svc.cancel_run(run.run_id.clone(), "user-1".into()).await);
        let e = err(svc.pause_run(run.run_id, "user-1".into()).await);
        assert_eq!(e.0, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn pause_run_forbidden_for_other_user() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        let e = err(svc.pause_run(run.run_id, "user-2".into()).await);
        assert_eq!(e.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn pause_run_not_found() {
        let svc = test_service();
        let e = err(svc.pause_run("nonexistent".into(), "user-1".into()).await);
        assert_eq!(e.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn resume_run_transitions_paused_to_running() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
        let result = ok(svc.resume_run(run.run_id.clone(), "user-1".into()).await);
        assert_eq!(result.status, "running");
        assert_eq!(result.previous_status, "paused");
        let status = ok(svc.get_run_status(run.run_id, "user-1".into()).await);
        assert_eq!(status.status, "running");
    }

    #[tokio::test]
    async fn resume_run_conflict_when_not_paused() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        let e = err(svc.resume_run(run.run_id, "user-1".into()).await);
        assert_eq!(e.0, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn resume_run_forbidden_for_other_user() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
        let e = err(svc.resume_run(run.run_id, "user-2".into()).await);
        assert_eq!(e.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn resume_run_not_found() {
        let svc = test_service();
        let e = err(svc.resume_run("nonexistent".into(), "user-1".into()).await);
        assert_eq!(e.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn pause_resume_round_trip_preserves_events() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
        ok(svc.resume_run(run.run_id.clone(), "user-1".into()).await);
        let status = ok(svc
            .get_run_status(run.run_id.clone(), "user-1".into())
            .await);
        assert_eq!(status.events_count, 3); // run_started + run_paused + run_resumed
        let events = ok(svc.stream_run(run.run_id, "user-1".into(), 0).await);
        assert_eq!(events[0]["event_type"], "run_started");
        assert_eq!(events[1]["event_type"], "run_paused");
        assert_eq!(events[2]["event_type"], "run_resumed");
    }

    #[tokio::test]
    async fn double_pause_is_conflict() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
        let e = err(svc.pause_run(run.run_id, "user-1".into()).await);
        assert_eq!(e.0, StatusCode::CONFLICT);
    }

    // ─── Durable persistence integration tests ─────────────────────────

    fn test_service_with_engine() -> AgenticRunLifecycleService {
        use crate::server::run_engine::RunEngine;
        use astra_services::runs::InMemoryRunStateStore;

        let engine = RunEngine::new(Arc::new(InMemoryRunStateStore::new()));
        AgenticRunLifecycleService::new(
            test_settings(),
            test_encryptor(),
            Arc::new(TokioMutex::new(HashMap::new())),
        )
        .with_run_engine(engine)
    }

    #[tokio::test]
    async fn durable_create_run_persists_to_store() {
        let svc = test_service_with_engine();
        let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);

        let engine = svc.run_engine.as_ref().unwrap();
        let durable = engine.load_run(&run.run_id).await.unwrap().unwrap();
        assert_eq!(durable.user_id, "user-1");
        assert_eq!(durable.status, "running");
        assert_eq!(durable.session_id, run.session_id);
    }

    #[tokio::test]
    async fn durable_create_run_eventually_persists_terminal_event() {
        let svc = test_service_with_engine();
        let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);

        let engine = svc.run_engine.as_ref().unwrap();
        let mut durable = None;
        for _ in 0..50 {
            let persisted = engine.load_run(&run.run_id).await.unwrap().unwrap();
            if persisted.status != "running" && persisted.events.len() >= 2 {
                durable = Some(persisted);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let durable = durable.expect("background run should persist terminal event");
        assert_eq!(durable.events.last().unwrap()["event_type"], "run_finished");
    }

    #[tokio::test]
    async fn durable_stream_chat_persists_final_state() {
        let svc = test_service_with_engine();
        let stream = ok(svc
            .stream_chat("user-1".into(), test_request("hello"))
            .await);

        let engine = svc.run_engine.as_ref().unwrap();
        let durable = engine.load_run(&stream.run_id).await.unwrap().unwrap();
        assert_eq!(durable.user_id, "user-1");
        assert_eq!(durable.session_id, stream.session_id);
        assert_ne!(durable.status, "running");
        assert!(durable.events.len() >= 2);
        assert_eq!(durable.events.last().unwrap()["event_type"], "run_finished");
    }

    #[tokio::test]
    async fn durable_cancel_persists_to_store() {
        let svc = test_service_with_engine();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        ok(svc.cancel_run(run.run_id.clone(), "user-1".into()).await);

        let engine = svc.run_engine.as_ref().unwrap();
        let mut durable = None;
        for _ in 0..50 {
            let persisted = engine.load_run(&run.run_id).await.unwrap().unwrap();
            if persisted.events.len() >= 2 {
                durable = Some(persisted);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let durable = durable.expect("cancelled run should persist terminal event");
        assert_eq!(durable.status, "cancelled");
        assert!(durable.events.len() >= 2); // run_started + run_finished
    }

    #[tokio::test]
    async fn durable_pause_resume_round_trip() {
        let svc = test_service_with_engine();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);

        ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
        let engine = svc.run_engine.as_ref().unwrap();
        let durable = engine.load_run(&run.run_id).await.unwrap().unwrap();
        assert_eq!(durable.status, "paused");
        assert_eq!(durable.waiting_for.as_deref(), Some("user_resume"));

        ok(svc.resume_run(run.run_id.clone(), "user-1".into()).await);
        let durable = engine.load_run(&run.run_id).await.unwrap().unwrap();
        assert_eq!(durable.status, "running");
        assert!(durable.waiting_for.is_none());
    }

    #[tokio::test]
    async fn no_engine_works_without_persistence() {
        // Service without engine should still work (backward compat)
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);
        ok(svc.cancel_run(run.run_id, "user-1".into()).await);
    }

    // ─── EdgeContext integration tests ──────────────────────────────────

    #[test]
    fn extract_edge_context_from_request_with_tools() {
        let mut ctx = serde_json::Map::new();
        ctx.insert(
            "edge_tools".to_string(),
            json!([{"function": {"name": "bash", "parameters": {}}}]),
        );
        ctx.insert(
            "edge_profile".to_string(),
            json!({"cwd": "/tmp", "git_branch": "main"}),
        );
        let req = ChatRequestData {
            context: Some(ctx),
            ..test_request("hello")
        };

        let edge_ctx = AgenticRunLifecycleService::extract_edge_context(&req);
        assert_eq!(edge_ctx.tool_count(), 1);
        assert_eq!(edge_ctx.tool_names(), vec!["bash"]);
        assert_eq!(edge_ctx.edge_profile.cwd.as_deref(), Some("/tmp"));
        assert_eq!(edge_ctx.edge_profile.git_branch.as_deref(), Some("main"));
    }

    #[test]
    fn extract_edge_context_from_empty_request() {
        let req = test_request("hello");
        let edge_ctx = AgenticRunLifecycleService::extract_edge_context(&req);
        assert!(!edge_ctx.has_tools());
        assert!(edge_ctx.edge_profile.cwd.is_none());
    }

    #[test]
    fn extract_edge_tools_backward_compat() {
        let mut ctx = serde_json::Map::new();
        ctx.insert(
            "edge_tools".to_string(),
            json!([
                {"function": {"name": "bash"}},
                {"function": {"name": "grep"}}
            ]),
        );
        let req = ChatRequestData {
            context: Some(ctx),
            ..test_request("hello")
        };
        let tools = AgenticRunLifecycleService::extract_edge_tools(&req);
        assert_eq!(tools.len(), 2);
    }

    #[test]
    fn extract_edge_profile_backward_compat() {
        let mut ctx = serde_json::Map::new();
        ctx.insert(
            "edge_profile".to_string(),
            json!({"cwd": "/workspace", "os": "linux"}),
        );
        let req = ChatRequestData {
            context: Some(ctx),
            ..test_request("hello")
        };
        let profile = AgenticRunLifecycleService::extract_edge_profile(&req);
        assert_eq!(profile.get("cwd").unwrap(), "/workspace");
        assert_eq!(profile.get("os").unwrap(), "linux");
    }

    // ─── Background spawning integration tests ──────────────────────────

    #[tokio::test]
    async fn create_run_spawns_background_task() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);
        assert_eq!(run.status, "running");

        // Deterministic wait: poll until the background task advances state.
        let status = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let status = ok(svc
                    .get_run_status(run.run_id.clone(), "user-1".into())
                    .await);
                if status.status != "running" || status.events_count > 1 {
                    break status;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("timeout waiting for background task to advance state");
        assert!(
            status.status != "running" || status.events_count > 1,
            "Expected background task to advance state, but status={} events={}",
            status.status,
            status.events_count
        );
    }

    #[tokio::test]
    async fn create_run_with_engine_persists_final_state() {
        let svc = test_service_with_engine();
        let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);

        // Deterministic wait: poll durable state until it leaves "running".
        let engine = svc.run_engine.as_ref().unwrap();
        let durable = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let durable = engine.load_run(&run.run_id).await.unwrap().unwrap();
                if durable.status != "running" {
                    break durable;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("timeout waiting for durable run status to finalize");
        assert_ne!(durable.status, "running");
    }

    // ─── DelegationTracker integration tests ────────────────────────────

    #[tokio::test]
    async fn delegation_tracker_get_children() {
        use crate::server::delegation_engine::{DelegationTracker, SubRunRecord, SubRunState};

        let tracker = DelegationTracker::new();
        tracker
            .record_sub_run(SubRunRecord {
                delegation_id: "d1".into(),
                run_id: "child-1".into(),
                parent_run_id: "parent-1".into(),
                agent_id: "agent-a".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;
        tracker
            .record_sub_run(SubRunRecord {
                delegation_id: "d1".into(),
                run_id: "child-2".into(),
                parent_run_id: "parent-1".into(),
                agent_id: "agent-b".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;
        tracker
            .record_sub_run(SubRunRecord {
                delegation_id: "d2".into(),
                run_id: "other-child".into(),
                parent_run_id: "parent-2".into(),
                agent_id: "agent-c".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;

        let mut children = tracker.get_children("parent-1").await;
        children.sort();
        assert_eq!(children, vec!["child-1", "child-2"]);

        let children = tracker.get_children("parent-2").await;
        assert_eq!(children, vec!["other-child"]);

        let children = tracker.get_children("nonexistent").await;
        assert!(children.is_empty());
    }

    // ─── Router route registration test ─────────────────────────────────

    #[test]
    fn router_includes_delegation_routes() {
        // Quick check that our delegation routes are registered.
        let source = include_str!("router_builder.rs");
        assert!(
            source.contains("/chat/runs/{run_id}/delegate"),
            "Missing delegation route"
        );
        assert!(
            source.contains("/chat/runs/{run_id}/delegations"),
            "Missing delegations list route"
        );
        assert!(
            source.contains("/chat/runs/{run_id}/delegations/pause"),
            "Missing delegations pause route"
        );
        assert!(
            source.contains("/chat/runs/{run_id}/delegations/resume"),
            "Missing delegations resume route"
        );
    }
}
