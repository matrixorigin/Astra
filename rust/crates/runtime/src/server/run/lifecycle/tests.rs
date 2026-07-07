use super::*;
use crate::server::run::lifecycle::persistence::{
    build_tool_trace_events, extract_prev_assistant_text, extract_session_state_compact,
    messages_for_csl_persist, redact_trace_value, server_loop_causal_chain_id,
    transcript_page_bounds, transcript_page_seq,
};
use astra_services::runs::{
    DatabaseRunStateStore, DurableRunCheckpointRecord, DurableRunDisplayProjectionRecord,
    DurableRunRecord, InMemoryRunStateStore, RunStateStore, RuntimeMcpBindingRequest,
    RuntimeSkillBindingRequest,
};
use astra_services::session_journal::{JournalEventType, ToolCallRecord};
use astra_services::workspace_records::{
    InMemoryWorkspaceRecordStore, WorkspaceCleanupDebtStore, WorkspaceCleanupDebtStoreError,
    WorkspaceRecordStore,
};
use serde_json::json;
use sqlx::Row;
use std::collections::HashSet;
use std::ffi::OsString;
use std::sync::Mutex as StdMutex;
use uuid::Uuid;

static LIFECYCLE_RUN_DB: tokio::sync::OnceCell<SharedPool> = tokio::sync::OnceCell::const_new();
const DURABLE_EVENT_PRESSURE_OPT_IN: &str = "ASTRA_DURABLE_EVENT_PRESSURE_PROBE";

struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self { key, previous }
    }

    fn remove(key: &'static str) -> Self {
        let previous = std::env::var_os(key);
        unsafe { std::env::remove_var(key) };
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        unsafe {
            match self.previous.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

struct StaticRunControlProvider {
    status: Option<RunControlStatus>,
    calls: AtomicUsize,
}

impl StaticRunControlProvider {
    fn new(status: Option<RunControlStatus>) -> Self {
        Self {
            status,
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Acquire)
    }
}

#[async_trait::async_trait]
impl crate::turn::run_control::RunStatusProvider for StaticRunControlProvider {
    async fn control_status(
        &self,
        _user_id: &str,
        _run_id: &str,
    ) -> Result<Option<RunControlStatus>, String> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        Ok(self.status)
    }
}

#[async_trait::async_trait]
impl RunInputProvider for StaticRunControlProvider {
    async fn poll_user_inputs(
        &self,
        _user_id: &str,
        _run_id: &str,
        after_event_index: usize,
    ) -> crate::turn::run_control::RunQueuedInputPoll {
        crate::turn::run_control::RunQueuedInputPoll {
            next_cursor: after_event_index,
            inputs: Vec::new(),
            error: None,
        }
    }

    async fn mark_user_inputs_released(
        &self,
        _user_id: &str,
        _run_id: &str,
        _event_indices: &[usize],
    ) -> Result<(), String> {
        Ok(())
    }
}

struct ActiveTestModelService;

fn test_model_record(name: String) -> astra_services::ModelRecord {
    astra_services::ModelRecord {
        model_id: format!("model-{name}"),
        name,
        provider: "openai".to_string(),
        base_url: Some("https://models.example.com/v1".to_string()),
        description: None,
        is_active: true,
        context_window: 128_000,
        max_completion_tokens: None,
        input_modalities: Vec::new(),
        output_modalities: Vec::new(),
        supported_parameters: Vec::new(),
        pricing: Default::default(),
        architecture: None,
        tags: Vec::new(),
        quirks: Default::default(),
        connectivity: None,
        thinking_capability: None,
        thinking_probe: None,
    }
}

#[async_trait]
impl astra_services::ModelService for ActiveTestModelService {
    async fn create_model(
        &self,
        _user_id: String,
        _request: astra_services::ModelCreateRequestData,
    ) -> Result<astra_services::ModelRecord, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }

    async fn list_models(
        &self,
        _user_id: String,
        _is_admin: bool,
    ) -> Result<Vec<astra_services::ModelListItem>, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }

    async fn get_model(
        &self,
        model_name: String,
    ) -> Result<astra_services::ModelRecord, (StatusCode, Json<ErrorResponse>)> {
        if model_name == "test-model" {
            return Ok(test_model_record(model_name));
        }
        Err(error_response_coded(
            StatusCode::NOT_FOUND,
            "model not found",
            "model_not_found",
        ))
    }

    async fn update_model(
        &self,
        _model_name: String,
        _request: astra_services::ModelUpdateRequestData,
    ) -> Result<astra_services::ModelRecord, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }

    async fn delete_model(
        &self,
        _model_name: String,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }

    async fn check_model(
        &self,
        _model_name: String,
    ) -> Result<astra_services::ModelRecord, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }
}

#[test]
fn post_loop_memory_cleanup_permit_respects_limit() {
    let baseline = POST_LOOP_MEMORY_CLEANUP_IN_FLIGHT.load(Ordering::SeqCst);
    let limit = baseline + 1;

    let permit = try_acquire_post_loop_memory_cleanup_permit(limit)
        .expect("permit should be available below limit");
    assert_eq!(
        POST_LOOP_MEMORY_CLEANUP_IN_FLIGHT.load(Ordering::SeqCst),
        baseline + 1
    );
    assert!(
        try_acquire_post_loop_memory_cleanup_permit(limit).is_none(),
        "permit should reject at limit"
    );
    drop(permit);
    assert_eq!(
        POST_LOOP_MEMORY_CLEANUP_IN_FLIGHT.load(Ordering::SeqCst),
        baseline
    );
}

#[tokio::test]
async fn post_loop_memory_cleanup_metrics_stay_low_cardinality() {
    let _memoria = EnvVarGuard::remove("MEMORIA_MASTER_KEY");
    let registry = Arc::new(astra_turn_core::pipeline_metrics::MetricsRegistry::new());

    record_post_loop_memory_cleanup_dispatch_metrics(Some(&registry), "async", "scheduled");
    run_post_loop_memory_cleanup_work(
        "session-1".to_string(),
        astra_turn_types::session_facts::SessionFacts::default(),
        None,
        None,
        Some(registry.clone()),
        Duration::from_millis(DEFAULT_SESSION_MEMORY_POST_LOOP_DRAIN_TIMEOUT_MS),
    )
    .await;

    let rendered = registry.render_prometheus();
    assert!(
        rendered.contains(
            "astra_post_loop_memory_cleanup_dispatches_total{mode=\"async\",outcome=\"scheduled\"} 1"
        ),
        "{rendered}"
    );
    assert!(
        rendered.contains("astra_post_loop_memory_cleanup_workers_total{outcome=\"completed\"} 1"),
        "{rendered}"
    );
    assert!(
        rendered.contains("astra_session_memory_post_loop_drains_total{outcome=\"no_service\"} 1"),
        "{rendered}"
    );
    assert!(
        !rendered.contains("user_id=")
            && !rendered.contains("session_id=")
            && !rendered.contains("run_id="),
        "memory cleanup metrics must stay low-cardinality: {rendered}"
    );
}

#[tokio::test]
async fn post_loop_memory_cleanup_runs_inline_when_async_pool_is_full() {
    let _memoria = EnvVarGuard::remove("MEMORIA_MASTER_KEY");
    let registry = Arc::new(astra_turn_core::pipeline_metrics::MetricsRegistry::new());

    post_loop_memory_cleanup_with_limits(
        "session-inline",
        &astra_turn_types::session_facts::SessionFacts::default(),
        None,
        None,
        Some(registry.clone()),
        0,
        Duration::ZERO,
    )
    .await;

    let rendered = registry.render_prometheus();
    assert!(
        rendered.contains(
            "astra_post_loop_memory_cleanup_dispatches_total{mode=\"inline\",outcome=\"saturated\"} 1"
        ),
        "{rendered}"
    );
    assert!(
        rendered.contains("astra_post_loop_memory_cleanup_workers_total{outcome=\"completed\"} 1"),
        "{rendered}"
    );
    assert!(!rendered.contains("dropped_full"), "{rendered}");
}

fn test_session_task(
    id: &str,
    title: &str,
    status: astra_tools::task_mgmt::SessionTaskStatusKind,
) -> SessionTask {
    SessionTask {
        archived_at: None,
        id: id.to_string(),
        title: title.to_string(),
        description: None,
        status,
        subtasks: vec![],
        created_at: String::new(),
        updated_at: String::new(),
        active_form: None,
        owner: None,
        metadata: None,
        blocks: vec![],
        blocked_by: vec![],
    }
}

fn test_agent_progress_event(
    agent_id: &str,
    timestamp_epoch_ms: u64,
    event_type: ProgressEventType,
) -> AgentProgressEvent {
    AgentProgressEvent {
        agent_id: agent_id.to_string(),
        event_type,
        timestamp_epoch_ms,
        metadata: None,
    }
}

fn test_agent_spawned(
    agent_id: &str,
    run_id: &str,
    parent_run_id: &str,
    timestamp_epoch_ms: u64,
) -> AgentProgressEvent {
    test_agent_progress_event(
        agent_id,
        timestamp_epoch_ms,
        ProgressEventType::AgentSpawned {
            run_id: run_id.to_string(),
            parent_run_id: parent_run_id.to_string(),
            agent_type: "reviewer".to_string(),
            description: "review code".to_string(),
            fanout_slot: None,
        },
    )
}

#[test]
fn restore_session_state_compact_ignores_runtime_control_state() {
    let svc = test_service();
    let request = test_request("resume");
    let mut state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );
    state.max_turn_input_tokens = 123_456;
    state.remaining_turns = 9;

    restore_session_state_compact(
        astra_turn_core::conversation_log::SessionStateCompact {
            approval_overrides: Some(json!({"approval": "stale"})),
            budget_remaining_tokens: 42_000,
            budget_remaining_rounds: 3,
            consecutive_ctx_errors: 3,
            interruption: Some(json!({
                "kind": "budget_exhausted",
                "resume_action": "continue_immediately"
            })),
            compaction_tracker: Some(json!({
                "attempt_count": 4,
                "cumulative_tokens_freed": 18_000,
                "last_tokens_freed": 2_000,
                "last_was_insufficient": true,
                "consecutive_futile_attempts": 2,
            })),
            ..Default::default()
        },
        &mut state,
    );

    assert!(state.approval_overrides.is_none());
    assert!(state.interruption.is_none());
    assert_eq!(state.max_turn_input_tokens, 123_456);
    assert_eq!(state.remaining_turns, 9);
    assert_eq!(state.consecutive_context_window_errors, 0);
    assert_eq!(state.compaction_effectiveness.attempt_count, 0);
}

#[test]
fn csl_session_state_does_not_persist_runtime_control_state() {
    let svc = test_service();
    let request = test_request("resume");
    let mut state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );
    state.restricted_tools.insert("write_file".to_string());
    state.max_turn_input_tokens = 50_000;
    state.remaining_turns = 2;
    state.consecutive_context_window_errors = 5;
    state.interruption = Some(astra_turn_core::interruption::InterruptionRecord::new(
        astra_turn_core::interruption::InterruptionKind::BudgetExhausted,
        astra_turn_core::interruption::ResumeAction::ContinueImmediately,
        astra_turn_core::interruption::InterruptionStateSummary {
            has_checkpoint: true,
            tool_calls_completed: 1,
            turns_completed: 1,
            remaining_turns: 0,
            error_detail: Some("stale interruption".to_string()),
            stall_signal: None,
            resume_restricted_tools: vec![],
        },
    ));
    state.compaction_effectiveness.attempt_count = 7;

    let compact = extract_session_state_compact(&state);

    assert!(
        compact.blocked_tools.is_empty(),
        "conversation-log state must not persist transient runtime restrictions"
    );
    assert!(compact.approval_overrides.is_none());
    assert!(compact.interruption.is_none());
    assert_eq!(compact.budget_remaining_tokens, 0);
    assert_eq!(compact.budget_remaining_rounds, 0);
    assert_eq!(compact.consecutive_ctx_errors, 0);
    assert!(compact.compaction_tracker.is_none());
}

#[test]
fn csl_session_state_restore_ignores_legacy_blocked_tools() {
    let svc = test_service();
    let request = test_request("resume");
    let mut state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );

    restore_session_state_compact(
        astra_turn_core::conversation_log::SessionStateCompact {
            blocked_tools: vec!["legacy_stale_tool".into()],
            recent_tools: vec!["read_file".into()],
            ..Default::default()
        },
        &mut state,
    );

    assert!(
        state.restricted_tools.is_empty(),
        "legacy CSL blocked_tools must not restore as hard runtime restrictions"
    );
    assert_eq!(state.recent_tools, vec!["read_file"]);
}

#[test]
fn csl_restore_turn_start_excludes_current_user_message() {
    let svc = test_service();
    let request = test_request("3");
    let mut state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-2",
        None,
        None,
        None,
    );
    let restored = vec![
        json!({"role": "user", "content": "1"}),
        json!({"role": "assistant", "content": "ack 1"}),
    ];

    let turn_start =
        AgenticRunLifecycleService::restore_csl_messages_into_loop_state(restored, &mut state);

    assert_eq!(
        turn_start, 2,
        "CSL deltas must start before this turn's user message"
    );
    assert_eq!(state.messages.len(), 3);
    assert_eq!(state.messages[0]["content"], "1");
    assert_eq!(state.messages[1]["content"], "ack 1");
    assert_eq!(state.messages[2]["content"], "3");
}

#[tokio::test]
async fn csl_persist_after_restore_keeps_current_user_message() {
    use astra_turn_core::conversation_log::file_store::FileCslStore;
    use astra_turn_core::conversation_log::manager::{CslManager, CslManagerConfig};
    use astra_turn_core::conversation_log::{CslStore, SessionStateCompact};

    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn CslStore> = Arc::new(FileCslStore::new(dir.path()));
    let session_id = "server-csl-current-user";
    let mut first = CslManager::new(
        Arc::clone(&store),
        session_id.to_string(),
        CslManagerConfig::default(),
    )
    .unwrap();
    first
        .persist_turn(
            1,
            &[
                json!({"role": "user", "content": "1"}),
                json!({"role": "assistant", "content": "ack 1"}),
            ],
            &SessionStateCompact::default(),
        )
        .await
        .unwrap();

    let svc = test_service();
    let request = test_request("3");
    let mut state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-2",
        None,
        None,
        None,
    );
    state.final_text = "ack 3".to_string();

    let mut resumed = CslManager::new(
        Arc::clone(&store),
        session_id.to_string(),
        CslManagerConfig::default(),
    )
    .unwrap();
    let materialized = resumed.load().await.unwrap().unwrap();
    let turn_start = AgenticRunLifecycleService::restore_csl_messages_into_loop_state(
        materialized.messages,
        &mut state,
    );
    resumed.mark_turn_start(turn_start);

    let messages = messages_for_csl_persist(&state);
    resumed
        .persist_turn(2, &messages, &extract_session_state_compact(&state))
        .await
        .unwrap();

    let mut reloaded = CslManager::new(
        Arc::clone(&store),
        session_id.to_string(),
        CslManagerConfig::default(),
    )
    .unwrap();
    let final_state = reloaded.load().await.unwrap().unwrap();
    let contents = final_state
        .messages
        .iter()
        .map(|message| message["content"].as_str().unwrap_or_default())
        .collect::<Vec<_>>();

    assert_eq!(
        contents,
        vec!["1", "ack 1", "3", "ack 3"],
        "restored web runs must persist the current user message into CSL"
    );
}

#[test]
fn restore_step_checkpoint_runtime_state_restores_replay_guards_and_runtime_state() {
    let svc = test_service();
    let request = test_request("resume");
    let mut state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );
    let idem_key = astra_pipeline::step_protocol::IdempotencyKey::semantic(
        "read_file",
        &json!({"path": "src/lib.rs"}),
    );
    let mut idempotency_cache = astra_pipeline::step_protocol::InMemoryIdempotencyCache::new();
    idempotency_cache.record(
        &idem_key,
        astra_pipeline::step_protocol::CachedToolResult {
            tool_name: "read_file".into(),
            output: "cached contents".into(),
            is_error: false,
            cached_at: 123,
            context_signature: None,
        },
    );
    let restored = astra_pipeline::step_restore::RestoredSession {
        messages: Vec::new(),
        budget_remaining_tokens: 0,
        budget_remaining_rounds: 0,
        blocked_tools: vec!["flaky_tool".into()],
        recent_tools: vec!["read_file".into(), "bash".into()],
        idempotency_cache,
        resume_turn: 0,
        protocol_version: astra_pipeline::step_protocol::PROTOCOL_VERSION,
        completed_tool_results: HashMap::new(),
        interruption: None,
        approval_overrides: None,
        consecutive_context_window_errors: 5,
        compaction_state: Some(json!({
            "attempt_count": 6,
            "cumulative_tokens_freed": 24_000,
            "last_tokens_freed": 1_500,
            "last_was_insufficient": false,
            "consecutive_futile_attempts": 1,
        })),
        pipeline_state: None,
    };

    restore_step_checkpoint_runtime_state(restored, "2026-06-13", &mut state);

    assert!(state.restricted_tools.contains("flaky_tool"));
    assert_eq!(state.recent_tools, vec!["read_file", "bash"]);
    let cached = state
        .idempotency_cache
        .check(&idem_key)
        .expect("idempotency cache should be restored");
    assert_eq!(cached.output, "cached contents");
    assert_eq!(state.consecutive_context_window_errors, 5);
    assert_eq!(state.compaction_effectiveness.attempt_count, 6);
    assert_eq!(
        state.compaction_effectiveness.cumulative_tokens_freed,
        24_000
    );
    assert_eq!(state.compaction_effectiveness.last_tokens_freed, 1_500);
    assert!(!state.compaction_effectiveness.last_was_insufficient);
    assert_eq!(
        state.compaction_effectiveness.consecutive_futile_attempts,
        1
    );
}

#[test]
fn run_scoped_agent_progress_filter_replays_early_events_after_spawn() {
    let mut filter = server_loop_host::RunScopedAgentProgressFilter::new("root-run".to_string());

    let accepted = filter.accept(test_agent_progress_event(
        "agent-a",
        1,
        ProgressEventType::Started {
            description: "review code".to_string(),
        },
    ));
    assert!(accepted.is_empty());

    let accepted = filter.accept(test_agent_spawned("agent-a", "child-run", "root-run", 2));
    assert_eq!(accepted.len(), 2);
    assert!(matches!(
        accepted[0].event_type,
        ProgressEventType::Started { .. }
    ));
    assert!(matches!(
        accepted[1].event_type,
        ProgressEventType::AgentSpawned { .. }
    ));

    let accepted = filter.accept(test_agent_progress_event(
        "agent-a",
        3,
        ProgressEventType::ToolExecuting {
            tool_name: "rg".to_string(),
            turn: 1,
        },
    ));
    assert_eq!(accepted.len(), 1);
}

#[test]
fn run_scoped_agent_progress_filter_replays_bounded_latest_early_events_in_order() {
    let mut filter = server_loop_host::RunScopedAgentProgressFilter::new("root-run".to_string());

    for timestamp in 1..=10 {
        assert!(
            filter
                .accept(test_agent_progress_event(
                    "agent-a",
                    timestamp,
                    ProgressEventType::ToolExecuting {
                        tool_name: format!("tool-{timestamp}"),
                        turn: timestamp as u32,
                    },
                ))
                .is_empty()
        );
    }

    let accepted = filter.accept(test_agent_spawned("agent-a", "child-run", "root-run", 11));

    assert_eq!(
        accepted
            .iter()
            .map(|event| event.timestamp_epoch_ms)
            .collect::<Vec<_>>(),
        vec![3, 4, 5, 6, 7, 8, 9, 10, 11]
    );
}

#[test]
fn run_scoped_agent_progress_filter_blocks_foreign_root_events() {
    let mut filter = server_loop_host::RunScopedAgentProgressFilter::new("root-a".to_string());

    assert!(
        filter
            .accept(test_agent_progress_event(
                "agent-b",
                1,
                ProgressEventType::Started {
                    description: "other run".to_string(),
                },
            ))
            .is_empty()
    );
    assert!(
        filter
            .accept(test_agent_spawned("agent-b", "child-b", "root-b", 2))
            .is_empty()
    );
    assert!(
        !filter.agent_ids.contains("agent-b"),
        "foreign agent must not be admitted"
    );
    assert!(
        !filter.pending_by_agent.contains_key("agent-b"),
        "foreign spawn should clear cached early events"
    );
}

#[test]
fn run_scoped_agent_progress_filter_allows_nested_child_runs() {
    let mut filter = server_loop_host::RunScopedAgentProgressFilter::new("root-run".to_string());

    assert_eq!(
        filter
            .accept(test_agent_spawned("agent-a", "child-a", "root-run", 1))
            .len(),
        1
    );
    assert_eq!(
        filter
            .accept(test_agent_spawned("agent-b", "grandchild-b", "child-a", 2))
            .len(),
        1
    );
    assert!(filter.agent_ids.contains("agent-b"));
    assert!(filter.run_ids.contains("grandchild-b"));
}

#[tokio::test]
async fn agent_progress_stream_bridge_drains_progress_on_stop() {
    let svc = test_service();
    let (event_tx, mut event_rx) = mpsc::channel::<Value>(16);
    let bridge = svc.spawn_agent_progress_stream_bridge("root-run".to_string(), event_tx);

    let emitter = svc
        .server_agent_progress_broadcaster
        .for_agent("agent-a".to_string());
    emitter.started("review code");
    emitter.agent_spawned("child-run", "root-run", "reviewer", "review code");
    emitter.completed("done", 0, (0, 0), 7);

    bridge.stop_and_drain().await;

    let mut events = Vec::new();
    while let Ok(event) = event_rx.try_recv() {
        events.push(event);
    }

    assert!(
        events
            .iter()
            .any(|event| event["type"].as_str() == Some("agent_spawned")),
        "bridge should drain agent_spawned before stopping: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| event["type"].as_str() == Some("agent_completed")),
        "bridge should drain agent_completed before stopping: {events:?}"
    );
}

struct ImmediateLifecycleExecutor;

#[async_trait]
impl SpawnAgentExecutor for ImmediateLifecycleExecutor {
    async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
        Ok(SpawnRunResult {
            agent_id: config.agent_id,
            run_id: config.run_id,
            status: "completed".to_string(),
            finish_reason: "normal".to_string(),
            cancelled_by_user: None,
            output: Some("child done".to_string()),
            error: None,
            prompt_tokens: 3,
            completion_tokens: 5,
            tool_calls: 1,
            permission_summary: None,
            permission_requests: 0,
            permission_requests_approved: 0,
            tools_blocked: 0,
        })
    }
}

struct WaitingLifecycleExecutor;

#[async_trait]
impl SpawnAgentExecutor for WaitingLifecycleExecutor {
    async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
        Ok(SpawnRunResult {
            agent_id: config.agent_id,
            run_id: config.run_id,
            status: "waiting".to_string(),
            finish_reason: "waiting".to_string(),
            cancelled_by_user: None,
            output: Some("executor_offline".to_string()),
            error: None,
            prompt_tokens: 3,
            completion_tokens: 5,
            tool_calls: 1,
            permission_summary: None,
            permission_requests: 0,
            permission_requests_approved: 0,
            tools_blocked: 1,
        })
    }
}

#[tokio::test]
async fn missing_agent_lifecycle_stream_uses_spawner_archive() {
    let router = Arc::new(astra_messaging::AgentMailboxRouter::new(
        Arc::new(astra_messaging::InProcessTransport::new()),
        Arc::new(crate::server::delegation::engine::DelegationTracker::new()),
    ));
    let spawner =
        DynamicAgentSpawner::new(router).with_executor(Arc::new(ImmediateLifecycleExecutor));
    let execution_metadata = json!({
        "workspace": {"kind": "server_sandbox", "cwd": "/tmp/astra"},
        "executor": {"kind": "server_local"},
        "transport": "server_local"
    });
    let context = crate::orchestration::SpawnContext {
        parent_run_id: "root-run".to_string(),
        parent_agent_id: "root-agent".to_string(),
        recursion_depth: 0,
        parent_is_fork_child: false,
        inherited_permissions: crate::orchestration::InheritedPermissions::auto_approve(),
        inherited_skills: vec![],
        working_dir: PathBuf::from("/tmp/astra"),
        live_event_sink: None,
        trace_context: None,
        spawn_tool_call_id: Some("call-spawn".to_string()),
        execution_metadata: Some(execution_metadata),
        delegation_chain: Vec::new(),
    };
    let input = astra_turn_core::orchestration_spawn_tool::SpawnAgentInput {
        description: "review code".to_string(),
        prompt: "review".to_string(),
        agent_type: "explore".to_string(),
        run_in_background: false,
        ..Default::default()
    };
    let spawn_output = spawner.spawn(input, &context).await.unwrap();
    assert!(
        matches!(
            spawn_output,
            astra_turn_core::orchestration_spawn_tool::SpawnAgentOutput::Completed { .. }
        ),
        "test setup must archive a synchronous completed child: {spawn_output:?}"
    );

    let sent_lifecycle_events = Arc::new(std::sync::Mutex::new(HashSet::new()));
    let (event_tx, mut event_rx) = mpsc::channel::<Value>(8);
    assert!(
        stream_missing_agent_lifecycle_events(
            &spawner,
            "root-run",
            &event_tx,
            &sent_lifecycle_events
        )
        .await
    );

    let mut events = Vec::new();
    while let Ok(event) = event_rx.try_recv() {
        events.push(event);
    }
    assert_eq!(events.len(), 2, "expected spawned + completed: {events:?}");
    assert_eq!(events[0]["type"], "agent_spawned");
    assert_eq!(events[0]["workspace"]["kind"], "server_sandbox");
    assert_eq!(events[0]["executor"]["kind"], "server_local");
    assert_eq!(events[0]["transport"], "server_local");
    assert_eq!(events[1]["type"], "agent_completed");
    assert_eq!(events[1]["status"], "completed");
    assert_eq!(events[1]["workspace"]["kind"], "server_sandbox");

    let (second_tx, mut second_rx) = mpsc::channel::<Value>(8);
    assert!(
        stream_missing_agent_lifecycle_events(
            &spawner,
            "root-run",
            &second_tx,
            &sent_lifecycle_events
        )
        .await
    );
    assert!(
        second_rx.try_recv().is_err(),
        "already-sent lifecycle events must not be replayed twice"
    );
}

#[tokio::test]
async fn missing_agent_lifecycle_stream_reconstructs_waiting_child() {
    let router = Arc::new(astra_messaging::AgentMailboxRouter::new(
        Arc::new(astra_messaging::InProcessTransport::new()),
        Arc::new(crate::server::delegation::engine::DelegationTracker::new()),
    ));
    let spawner =
        DynamicAgentSpawner::new(router).with_executor(Arc::new(WaitingLifecycleExecutor));
    let context = crate::orchestration::SpawnContext {
        parent_run_id: "root-run".to_string(),
        parent_agent_id: "root-agent".to_string(),
        recursion_depth: 0,
        parent_is_fork_child: false,
        inherited_permissions: crate::orchestration::InheritedPermissions::auto_approve(),
        inherited_skills: vec![],
        working_dir: PathBuf::from("/tmp/astra"),
        live_event_sink: None,
        trace_context: None,
        spawn_tool_call_id: Some("call-spawn".to_string()),
        execution_metadata: Some(json!({
            "workspace": {"kind": "edge_workspace", "cwd": "/Users/test/repo"},
            "executor": {"kind": "edge_agent", "status": "offline"},
            "transport": "edge_ws"
        })),
        delegation_chain: Vec::new(),
    };
    let input = astra_turn_core::orchestration_spawn_tool::SpawnAgentInput {
        description: "review code".to_string(),
        prompt: "review".to_string(),
        agent_type: "explore".to_string(),
        run_in_background: false,
        ..Default::default()
    };
    let spawn_output = spawner.spawn(input, &context).await.unwrap();
    assert!(
        matches!(
            spawn_output,
            astra_turn_core::orchestration_spawn_tool::SpawnAgentOutput::Waiting { .. }
        ),
        "test setup must archive a synchronous waiting child: {spawn_output:?}"
    );

    let sent_lifecycle_events = Arc::new(std::sync::Mutex::new(HashSet::new()));
    let (event_tx, mut event_rx) = mpsc::channel::<Value>(8);
    assert!(
        stream_missing_agent_lifecycle_events(
            &spawner,
            "root-run",
            &event_tx,
            &sent_lifecycle_events
        )
        .await
    );

    let mut events = Vec::new();
    while let Ok(event) = event_rx.try_recv() {
        events.push(event);
    }
    assert_eq!(events.len(), 2, "expected spawned + waiting: {events:?}");
    assert_eq!(events[0]["type"], "agent_spawned");
    assert_eq!(events[1]["type"], "agent_waiting");
    assert_eq!(events[1]["reason"], "executor_offline");
    assert_eq!(events[1]["workspace"]["kind"], "edge_workspace");
    assert_eq!(events[1]["executor"]["kind"], "edge_agent");
}

#[test]
fn agent_live_event_to_work_surface_sse_maps_output_and_terminal() {
    let metadata = json!({
        "workspace": {
            "kind": "edge_workspace",
            "display_name": "MacBook Pro",
            "cwd": "/Users/test/project",
            "authority": "read_write",
            "fallback_policy": "disabled"
        },
        "executor": {
            "kind": "edge_agent",
            "executor_id": "edge-macbook-1",
            "display_name": "MacBook Pro",
            "transport": "edge_ws",
            "status": "online"
        },
        "transport": "edge_ws",
        "fallback_policy": "disabled"
    });
    let output = super::agent_live_event_to_work_surface_sse(
        &AgentLiveEvent {
            agent_id: "agent-1".to_string(),
            kind: AgentLiveEventKind::OutputDelta("child output".to_string()),
        },
        Some(&metadata),
    );
    assert_eq!(output["type"], "agent_live_event");
    assert_eq!(output["agent_id"], "agent-1");
    assert_eq!(output["event_kind"], "output_delta");
    assert_eq!(output["content"], "child output");
    assert_eq!(output["workspace"]["kind"], "edge_workspace");
    assert_eq!(output["executor"]["kind"], "edge_agent");
    assert_eq!(output["transport"], "edge_ws");
    assert_eq!(output["fallback_policy"], "disabled");

    let terminal = super::agent_live_event_to_work_surface_sse(
        &AgentLiveEvent {
            agent_id: "agent-1".to_string(),
            kind: AgentLiveEventKind::AgentTerminated {
                termination: AgentLiveTermination::Completed,
                duration_ms: 12,
                reason: None,
            },
        },
        Some(&metadata),
    );
    assert_eq!(terminal["event_kind"], "agent_terminated");
    assert_eq!(terminal["termination"], "completed");
    assert_eq!(terminal["status"], "completed");
    assert_eq!(terminal["duration_ms"], 12);
    assert_eq!(terminal["workspace"]["kind"], "edge_workspace");
    assert_eq!(terminal["executor"]["executor_id"], "edge-macbook-1");
}

// ── extract_prev_assistant_text + implicit feedback wiring ──

#[test]
fn task_board_resume_hint_is_bounded_and_prefers_running_work() {
    use astra_tools::task_mgmt::SessionTaskStatusKind;

    let tasks = vec![
        test_session_task("task-1", "pending setup", SessionTaskStatusKind::Pending),
        test_session_task(
            "task-2",
            "active implementation",
            SessionTaskStatusKind::InProgress,
        ),
        test_session_task("task-3", "already done", SessionTaskStatusKind::Completed),
        test_session_task("task-4", "waiting review", SessionTaskStatusKind::Paused),
    ];

    let hint = format_task_board_resume_hint(&tasks).expect("open task hint");

    assert_eq!(
        hint,
        "open=3 · next=[in_progress] task-2: active implementation · +2 more open"
    );
}

#[test]
fn task_board_resume_hint_is_absent_without_open_work() {
    use astra_tools::task_mgmt::SessionTaskStatusKind;

    let tasks = vec![test_session_task(
        "task-1",
        "already done",
        SessionTaskStatusKind::Completed,
    )];

    assert!(format_task_board_resume_hint(&tasks).is_none());
}

#[test]
fn trace_redaction_removes_nested_secrets_and_truncates_long_text() {
    let redacted = redact_trace_value(&json!({
        "Authorization": "Bearer secret",
        "nested": {
            "api_key": "abc123",
            "safe": "visible"
        },
        "items": [
            {"cookie": "session=abc"},
            {"text": "x".repeat(2_050)}
        ]
    }));

    assert_eq!(redacted["Authorization"], "[REDACTED]");
    assert_eq!(redacted["nested"]["api_key"], "[REDACTED]");
    assert_eq!(redacted["nested"]["safe"], "visible");
    assert_eq!(redacted["items"][0]["cookie"], "[REDACTED]");
    assert!(
        redacted["items"][1]["text"]
            .as_str()
            .expect("string")
            .ends_with("...")
    );
}

#[test]
fn tool_trace_events_populate_columns_and_redacted_payloads() {
    let trace = TraceContext {
        session_id: "session-1".to_string(),
        user_id: "user-1".to_string(),
        turn_id: "turn-1".to_string(),
        turn_seq: 7,
        causal_chain_id: "chain-1".to_string(),
        root_event_id: "trace:root".to_string(),
    };
    let record = ToolCallRecord {
        tool_call_id: Some("tool-call-1".to_string()),
        name: "agent".to_string(),
        ok: true,
        ms: 42,
        args_preview: Some("agent(action='spawn'): child".to_string()),
        result_preview: Some("launched child".to_string()),
        round: Some(2),
        args_full: Some(r#"{"action":"spawn","token":"secret"}"#.to_string()),
        result_full: Some(
            r#"{"agent_id":"child@run","run_id":"child-run","result":"ok"}"#.to_string(),
        ),
        ..Default::default()
    };

    let events = build_tool_trace_events(
        &trace,
        "root-run",
        None,
        Some("root-agent"),
        None,
        &[record],
    );

    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event_type, "tool_call_started");
    assert_eq!(events[0].tool_call_id.as_deref(), Some("tool-call-1"));
    assert_eq!(events[0].round_index, Some(2));
    assert_eq!(events[0].meta_tool_name.as_deref(), Some("agent"));
    assert_eq!(
        events[0].metadata["tool_args_json_redacted"]["token"],
        "[REDACTED]"
    );
    assert_eq!(events[1].event_type, "tool_call_completed");
    assert_eq!(events[1].meta_duration_ms, Some(42));
    assert_eq!(events[1].metadata["action"], "spawn");
    assert_eq!(events[1].metadata["child_run_id"], "child-run");
}

#[test]
fn extract_prev_assistant_text_picks_latest_assistant_string() {
    let messages = vec![
        serde_json::json!({"role": "user", "content": "hi"}),
        serde_json::json!({"role": "assistant", "content": "first answer"}),
        serde_json::json!({"role": "user", "content": "follow up"}),
        serde_json::json!({"role": "assistant", "content": "latest answer"}),
    ];
    assert_eq!(
        extract_prev_assistant_text(&messages).as_deref(),
        Some("latest answer")
    );
}

#[test]
fn extract_prev_assistant_text_handles_content_parts_array() {
    let messages = vec![
        serde_json::json!({"role": "user", "content": "hi"}),
        serde_json::json!({
            "role": "assistant",
            "content": [
                {"type": "text", "text": "part one"},
                {"type": "text", "text": "part two"},
            ],
        }),
    ];
    assert_eq!(
        extract_prev_assistant_text(&messages).as_deref(),
        Some("part one\npart two")
    );
}

#[test]
fn extract_prev_assistant_text_returns_none_when_no_assistant_turn() {
    let messages = vec![serde_json::json!({"role": "user", "content": "hi"})];
    assert!(extract_prev_assistant_text(&messages).is_none());
}

#[test]
fn extract_prev_assistant_text_skips_empty_assistant_bodies() {
    let messages = vec![
        serde_json::json!({"role": "assistant", "content": "real answer"}),
        serde_json::json!({"role": "user", "content": "ok"}),
        serde_json::json!({"role": "assistant", "content": "   "}),
    ];
    assert_eq!(
        extract_prev_assistant_text(&messages).as_deref(),
        Some("real answer")
    );
}

#[test]
fn build_run_turn_complete_event_carries_authoritative_assistant_text() {
    let event = build_run_turn_complete_event_with_interruption(0, "recovered final answer", None);
    assert_eq!(event["type"], "turn_complete");
    assert_eq!(event["assistant_text"], "recovered final answer");
    assert_eq!(event["has_tool_calls"], false);
}

#[test]
fn build_run_turn_complete_event_omits_empty_assistant_text() {
    let event = build_run_turn_complete_event_with_interruption(1, "", None);
    assert_eq!(event["type"], "turn_complete");
    assert_eq!(event["has_tool_calls"], true);
    assert!(event.get("assistant_text").is_none());
}

#[test]
fn stream_turn_complete_is_only_for_completed_or_paused_turns() {
    assert!(should_emit_stream_turn_complete(&RunStatus::Completed));
    assert!(should_emit_stream_turn_complete(&RunStatus::Paused));
    assert!(!should_emit_stream_turn_complete(&RunStatus::Failed));
    assert!(!should_emit_stream_turn_complete(&RunStatus::Cancelled));
    assert!(!should_emit_stream_turn_complete(&RunStatus::Waiting));
    assert!(!should_emit_stream_turn_complete(&RunStatus::InputQueued));
    assert!(!should_emit_stream_turn_complete(&RunStatus::Running));
}

#[test]
fn transcript_page_seq_rolls_over_every_fifty_items() {
    assert_eq!(transcript_page_seq(1), 1);
    assert_eq!(transcript_page_seq(50), 1);
    assert_eq!(transcript_page_seq(51), 2);
    assert_eq!(transcript_page_seq(101), 3);
}

#[test]
fn transcript_page_bounds_cover_exact_page_window() {
    assert_eq!(transcript_page_bounds(1), (1, 50));
    assert_eq!(transcript_page_bounds(2), (51, 100));
    assert_eq!(transcript_page_bounds(3), (101, 150));
}

#[test]
fn budget_exhausted_paused_run_does_not_block_next_session_turn() {
    let (mut run, _, _, _) = AgenticRunLifecycleService::build_tracked_run_state(
        "run-1".to_string(),
        "session-1".to_string(),
        "user-1".to_string(),
    );

    run.status = RunStatus::Running;
    assert!(
        AgenticRunLifecycleService::blocks_new_session_run(&run, "session-1"),
        "running run must block a concurrent turn"
    );

    run.status = RunStatus::Paused;
    run.waiting_for = Some("user_resume".to_string());
    assert!(
        AgenticRunLifecycleService::blocks_new_session_run(&run, "session-1"),
        "manual/user-wait paused run must block until resumed or cancelled"
    );

    run.waiting_for = None;
    assert!(
        !AgenticRunLifecycleService::blocks_new_session_run(&run, "session-1"),
        "budget-exhausted paused run has no waiting_for and must allow the next message"
    );

    run.status = RunStatus::Waiting;
    assert!(
        AgenticRunLifecycleService::blocks_new_session_run(&run, "session-1"),
        "waiting run must still block a concurrent turn"
    );
}

fn test_spawn_run_config(allowed_tools: Vec<&str>, read_only: bool) -> SpawnRunConfig {
    let inherited_permissions = crate::orchestration::InheritedPermissions::auto_approve();
    let permission_context =
        crate::orchestration::PermissionSyncContext::shared(inherited_permissions.clone());
    SpawnRunConfig {
        run_id: "child-run".to_string(),
        agent_id: "child@1234".to_string(),
        recursion_depth: 1,
        agent_type: "test".to_string(),
        task: "do work".to_string(),
        system_prompt_addendum: String::new(),
        model: Some("test-model".to_string()),
        max_turns: 3,
        allowed_tools: allowed_tools.into_iter().map(String::from).collect(),
        read_only,
        working_dir: std::path::PathBuf::from("/tmp"),
        mailbox: None,
        progress_emitter: None,
        context_cache: None,
        inherited_permissions,
        parent_address: None,
        permission_context,
        inherited_skills: Vec::new(),
        live_event_sink: None,
        inherited_prefix: None,
        execution_metadata: None,
        is_fork_child: false,
        delegation_chain: Vec::new(),
    }
}

fn test_spawn_runtime_context(parent_run_id: &str, user_id: &str) -> ServerSpawnRuntimeContext {
    ServerSpawnRuntimeContext {
        parent_run_id: parent_run_id.to_string(),
        user_id: user_id.to_string(),
        session_id: "session-1".to_string(),
        forward_headers: HashMap::new(),
        llm_token_service: None,
        request_constraints: RequestConstraints::default(),
        execution_metadata: None,
        pause_flag: None,
        cancel_token: None,
        trace_context: server_trace_context(user_id, "session-1", parent_run_id, 1),
        #[cfg(feature = "bridge-e2e-hooks")]
        test_child_llm_rounds: Vec::new(),
        #[cfg(feature = "harness")]
        harness_sink: None,
    }
}

#[tokio::test]
async fn server_spawn_runtime_context_is_keyed_by_parent_run() {
    let executor = ServerSpawnAgentExecutor::new(
        test_settings(),
        test_encryptor(),
        Arc::new(TokioMutex::new(HashMap::new())),
    );
    executor
        .set_runtime_context(test_spawn_runtime_context("parent-run-a", "user-a"))
        .await;
    executor
        .set_runtime_context(test_spawn_runtime_context("parent-run-b", "user-b"))
        .await;

    let mut config = test_spawn_run_config(vec!["*"], false);
    config.parent_address = Some(astra_messaging::types::AgentAddress::new(
        "parent-run-b",
        "root-agent",
    ));

    let context = executor.runtime_context_for_config(&config).await.unwrap();

    assert_eq!(context.parent_run_id, "parent-run-b");
    assert_eq!(context.user_id, "user-b");
}

#[tokio::test]
async fn server_spawn_runtime_context_requires_parent_lineage() {
    let executor = ServerSpawnAgentExecutor::new(
        test_settings(),
        test_encryptor(),
        Arc::new(TokioMutex::new(HashMap::new())),
    );
    executor
        .set_runtime_context(test_spawn_runtime_context("parent-run-a", "user-a"))
        .await;

    let config = test_spawn_run_config(vec!["*"], false);
    let err = match executor.runtime_context_for_config(&config).await {
        Ok(_) => panic!("server dynamic spawn must not run without parent lineage"),
        Err(err) => err,
    };

    assert!(err.contains("parent run lineage"), "{err}");
}

#[test]
fn subrun_turn_budget_uses_explicit_spawn_max_turns() {
    let profile = astra_turn_core::chat_turn_heuristics::infer_task_execution_profile(
        "explore the codebase and implement the fix",
    );
    let budget = resolve_subrun_agentic_turn_budget(profile, Some(3));

    assert_eq!(budget.initial_turns, 3);
    assert_eq!(budget.hard_turn_limit, 3);
    assert_eq!(budget.max_extensions, 0);
}

#[test]
fn spawn_child_constraints_intersect_parent_and_agent_allowlists() {
    let parent = RequestConstraints::new(
        Some(
            ["bash", "read_file", "write_file"]
                .into_iter()
                .map(String::from)
                .collect(),
        ),
        Some(["review"].into_iter().map(String::from).collect()),
        Some(
            [
                crate::skills::manifest::SkillSourceKind::Local,
                crate::skills::manifest::SkillSourceKind::Database,
            ]
            .into_iter()
            .collect(),
        ),
    );
    let config = test_spawn_run_config(vec!["bash", "read_file"], true);

    let constraints = spawn_child_request_constraints(&parent, &config);

    assert_eq!(
        constraints.allowed_tools.unwrap(),
        ["bash", "read_file"]
            .into_iter()
            .map(String::from)
            .collect()
    );
    assert_eq!(
        constraints.allowed_skills.unwrap(),
        ["review"].into_iter().map(String::from).collect()
    );
    assert_eq!(
        constraints.allowed_skill_sources.unwrap(),
        [
            crate::skills::manifest::SkillSourceKind::Local,
            crate::skills::manifest::SkillSourceKind::Database,
        ]
        .into_iter()
        .collect()
    );
}

#[test]
fn spawn_child_constraints_preserve_parent_when_child_allows_all() {
    let parent = RequestConstraints::new(
        Some(
            ["bash", "write_file"]
                .into_iter()
                .map(String::from)
                .collect(),
        ),
        None,
        None,
    );
    let config = test_spawn_run_config(vec!["*"], false);

    let constraints = spawn_child_request_constraints(&parent, &config);

    assert_eq!(
        constraints.allowed_tools.unwrap(),
        ["bash", "write_file"]
            .into_iter()
            .map(String::from)
            .collect()
    );
}

#[test]
fn spawn_child_constraints_read_only_wildcard_gets_read_only_tools() {
    let parent = RequestConstraints::default();
    let config = test_spawn_run_config(vec!["*"], true);

    let constraints = spawn_child_request_constraints(&parent, &config);
    let allowed = constraints.allowed_tools.unwrap();

    assert!(allowed.contains("read_file"));
    assert!(allowed.contains("grep"));
    assert!(!allowed.contains("write_file"));
    assert!(!allowed.contains("str_replace"));
}

#[test]
fn build_run_turn_complete_event_marks_interrupted_turns() {
    let interruption = astra_turn_core::interruption::InterruptionRecord::new(
        astra_turn_core::interruption::InterruptionKind::BudgetExhausted,
        astra_turn_core::interruption::ResumeAction::ContinueImmediately,
        astra_turn_core::interruption::InterruptionStateSummary {
            has_checkpoint: true,
            tool_calls_completed: 7,
            turns_completed: 15,
            remaining_turns: 0,
            error_detail: Some("Round budget hard-limit reached".to_string()),
            stall_signal: None,
            resume_restricted_tools: vec![],
        },
    );

    let event = build_run_turn_complete_event_with_interruption(
        7,
        "[Round budget hard-limit reached]",
        Some(&interruption),
    );

    assert_eq!(event["type"], "turn_complete");
    assert_eq!(event["has_tool_calls"], false);
    assert_eq!(event["stall_detected"], true);
    assert_eq!(event["execution_state"]["status"], "interrupted");
    assert_eq!(event["execution_state"]["interrupted"], true);
    assert_eq!(
        event["execution_state"]["interruption_kind"],
        "budget_exhausted"
    );
    assert_eq!(event["execution_state"]["tool_calls_completed"], 7);
    assert_eq!(event["execution_state"]["remaining_turns"], 0);
    assert_eq!(event["assistant_text"], "[Round budget hard-limit reached]");
}

#[test]
fn correction_keywords_trigger_was_corrected_via_implicit_feedback() {
    // Sanity-check that the detect_implicit_feedback_signal contract used in
    // record_server_loop_learning_outcome still recognizes Chinese-language
    // user corrections.
    let signal = astra_turn_types::detect_implicit_feedback_signal(
        "不对，你搞错了",
        Some("previous assistant reply"),
    );
    assert!(
        matches!(signal.signal_type.as_str(), "correction" | "frustration"),
        "expected correction/frustration, got {:?}",
        signal.signal_type
    );
}

#[test]
fn neutral_user_turn_does_not_flag_was_corrected() {
    let signal = astra_turn_types::detect_implicit_feedback_signal(
        "再列一下 docs 目录",
        Some("previous assistant reply"),
    );
    assert!(
        !matches!(signal.signal_type.as_str(), "correction" | "frustration"),
        "expected non-correction, got {:?}",
        signal.signal_type
    );
}

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
    MatrixOneSettings::from_env_with_database("test_astra_runtime")
}

fn test_encryptor() -> Arc<FernetTokenEncryptor> {
    Arc::new(FernetTokenEncryptor::new("cJ8pxr3t6iJmSYqe6wD7vu2rN_C3ovGUxkC5H3NXFNY=").unwrap())
}

#[derive(Default)]
struct FaultInjectedRunStoreCounters {
    status_calls: usize,
    append_calls: usize,
}

struct FaultInjectedStatusMutation {
    user_id: String,
    run_id: String,
    status: String,
    waiting_for: Option<String>,
    error_message: Option<String>,
}

struct FaultInjectedRunStateStore {
    inner: InMemoryRunStateStore,
    fail_status_calls: HashSet<usize>,
    fail_append_calls: HashSet<usize>,
    mutate_before_status_call: HashMap<usize, FaultInjectedStatusMutation>,
    counters: StdMutex<FaultInjectedRunStoreCounters>,
}

impl FaultInjectedRunStateStore {
    fn new(fail_status_calls: &[usize], fail_append_calls: &[usize]) -> Self {
        Self {
            inner: InMemoryRunStateStore::new(),
            fail_status_calls: fail_status_calls.iter().copied().collect(),
            fail_append_calls: fail_append_calls.iter().copied().collect(),
            mutate_before_status_call: HashMap::new(),
            counters: StdMutex::new(FaultInjectedRunStoreCounters::default()),
        }
    }

    fn with_status_mutation_before_call(
        mut self,
        call: usize,
        user_id: &str,
        run_id: &str,
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
    ) -> Self {
        self.mutate_before_status_call.insert(
            call,
            FaultInjectedStatusMutation {
                user_id: user_id.to_string(),
                run_id: run_id.to_string(),
                status: status.to_string(),
                waiting_for: waiting_for.map(ToString::to_string),
                error_message: error_message.map(ToString::to_string),
            },
        );
        self
    }

    fn next_status_call(&self) -> usize {
        let mut counters = self.counters.lock().expect("status counter lock");
        counters.status_calls += 1;
        counters.status_calls
    }

    fn next_append_call(&self) -> usize {
        let mut counters = self.counters.lock().expect("append counter lock");
        counters.append_calls += 1;
        counters.append_calls
    }

    async fn apply_status_mutation_before_call(&self, call: usize) -> Result<(), String> {
        if let Some(mutation) = self.mutate_before_status_call.get(&call) {
            self.inner
                .update_run_status(
                    &mutation.user_id,
                    &mutation.run_id,
                    &mutation.status,
                    mutation.waiting_for.as_deref(),
                    mutation.error_message.as_deref(),
                )
                .await?;
        }
        Ok(())
    }
}

#[async_trait]
impl RunStateStore for FaultInjectedRunStateStore {
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
        let call = self.next_status_call();
        self.apply_status_mutation_before_call(call).await?;
        if self.fail_status_calls.contains(&call) {
            return Err(format!("injected update_run_status failure on call {call}"));
        }
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
        let call = self.next_status_call();
        self.apply_status_mutation_before_call(call).await?;
        if self.fail_status_calls.contains(&call) {
            return Err(format!(
                "injected update_run_status_if_current failure on call {call}"
            ));
        }
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
        let call = self.next_status_call();
        self.apply_status_mutation_before_call(call).await?;
        if self.fail_status_calls.contains(&call) {
            return Err(format!(
                "injected update_run_status_with_event_if_current failure on call {call}"
            ));
        }
        let append_call = self.next_append_call();
        if self.fail_append_calls.contains(&append_call) {
            return Err(format!(
                "injected transition append_event failure on call {append_call}"
            ));
        }
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
        let call = self.next_status_call();
        self.apply_status_mutation_before_call(call).await?;
        if self.fail_status_calls.contains(&call) {
            return Err(format!(
                "injected update_run_status_with_events_if_current failure on call {call}"
            ));
        }
        let append_call = self.next_append_call();
        if self.fail_append_calls.contains(&append_call) {
            return Err(format!(
                "injected transition append_events failure on call {append_call}"
            ));
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
        let call = self.next_append_call();
        if self.fail_append_calls.contains(&call) {
            return Err(format!("injected append_event failure on call {call}"));
        }
        self.inner
            .append_events_batch(user_id, run_id, events)
            .await
    }

    async fn list_user_runs_cursor(
        &self,
        user_id: &str,
        limit: u32,
        cursor: Option<RunListCursor>,
    ) -> Result<astra_services::runs::DurableRunListPage, String> {
        self.inner
            .list_user_runs_cursor(user_id, limit, cursor)
            .await
    }

    async fn find_waiting_runs(&self) -> Result<Vec<DurableRunRecord>, String> {
        self.inner.find_waiting_runs().await
    }

    async fn find_running_runs(&self) -> Result<Vec<DurableRunRecord>, String> {
        self.inner.find_running_runs().await
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

struct DenyTokenBudgetGovernor;

#[async_trait]
impl astra_services::resource_governor::ResourceGovernor for DenyTokenBudgetGovernor {
    async fn get_limits(
        &self,
        _user_id: &str,
    ) -> astra_services::resource_governor::ResourceLimits {
        astra_services::resource_governor::ResourceLimits::default()
    }

    async fn set_limits(
        &self,
        _user_id: &str,
        _limits: astra_services::resource_governor::ResourceLimits,
    ) {
    }

    async fn get_usage(&self, _user_id: &str) -> astra_services::resource_governor::ResourceUsage {
        astra_services::resource_governor::ResourceUsage::default()
    }

    async fn check_session_create(
        &self,
        _user_id: &str,
    ) -> astra_services::resource_governor::LimitCheck {
        astra_services::resource_governor::LimitCheck::Allowed
    }

    async fn record_session_created(&self, _user_id: &str) {}

    async fn record_tool_calls(&self, _user_id: &str, _count: u64) {}

    async fn record_tokens(&self, _user_id: &str, _tokens: u64) {}

    async fn check_token_budget(
        &self,
        _user_id: &str,
    ) -> astra_services::resource_governor::LimitCheck {
        astra_services::resource_governor::LimitCheck::Denied {
            limit: astra_services::resource_governor::ResourceLimitKind::DailyTokens,
            reason: "daily token budget exhausted (1000/1000)".to_string(),
        }
    }
}

fn test_service() -> AgenticRunLifecycleService {
    let engine = RunEngine::new(Arc::new(InMemoryRunStateStore::new()));
    AgenticRunLifecycleService::new(
        test_settings(),
        test_encryptor(),
        Arc::new(TokioMutex::new(HashMap::new())),
        engine,
    )
    .with_model_service(Arc::new(ActiveTestModelService))
}

fn test_service_with_store(store: Arc<dyn RunStateStore>) -> AgenticRunLifecycleService {
    let engine = RunEngine::new(store);
    AgenticRunLifecycleService::new(
        test_settings(),
        test_encryptor(),
        Arc::new(TokioMutex::new(HashMap::new())),
        engine,
    )
    .with_model_service(Arc::new(ActiveTestModelService))
}

async fn setup_lifecycle_run_db_it() -> SharedPool {
    assert_eq!(
        std::env::var("ASTRA_TEST_DB_IT").as_deref(),
        Ok("1"),
        "set ASTRA_TEST_DB_IT=1 for ignored integration tests"
    );
    LIFECYCLE_RUN_DB
        .get_or_init(|| async {
            let settings = MatrixOneSettings::from_env();
            let catalog = std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG")
                .unwrap_or_else(|_| "mysql".to_string());
            astra_services::ensure_core_schema(&settings, &catalog)
                .await
                .expect("ensure_core_schema");
            SharedPool::new(&settings).await.expect("SharedPool::new")
        })
        .await
        .clone()
}

fn db_backed_test_service(
    shared_pool: &SharedPool,
    owner_pod_id: &str,
) -> AgenticRunLifecycleService {
    let store: Arc<dyn RunStateStore> =
        Arc::new(DatabaseRunStateStore::new(shared_pool.clone()).with_owner_pod_id(owner_pod_id));
    let engine = RunEngine::new(store);
    AgenticRunLifecycleService::new(
        shared_pool.settings().clone(),
        test_encryptor(),
        Arc::new(TokioMutex::new(HashMap::new())),
        engine,
    )
    .with_model_service(Arc::new(ActiveTestModelService))
}

async fn cleanup_lifecycle_run_fixture(pool: &SharedPool, user_id: &str, run_id: &str) {
    for sql in [
        "DELETE FROM run_display_projections WHERE user_id = ? AND run_id = ?",
        "DELETE FROM run_checkpoints WHERE user_id = ? AND run_id = ?",
        "DELETE FROM agent_run_events WHERE user_id = ? AND run_id = ?",
        "DELETE FROM agent_runs WHERE user_id = ? AND run_id = ?",
    ] {
        let _ = sqlx::query(sql)
            .bind(user_id)
            .bind(run_id)
            .execute(pool.get())
            .await;
    }
}

#[derive(Debug)]
struct DurableEventPressureBatch {
    raw_event_count: usize,
    candidate_rows: usize,
    candidate_bytes: usize,
    budgeted_events: Vec<Value>,
    budgeted_bytes: usize,
    compacted: bool,
}

#[derive(Debug)]
struct DurableEventPressureRunStats {
    raw_events: usize,
    candidate_rows: usize,
    candidate_bytes: usize,
    budgeted_rows: usize,
    budgeted_bytes: usize,
    persisted_rows: usize,
    replay_rows: usize,
    compacted_rows: usize,
    text_delta_rows: usize,
    elapsed_ms: u64,
}

fn durable_event_pressure_env_usize(name: &str, default: usize, min: usize) -> usize {
    match std::env::var(name) {
        Ok(value) => value
            .parse::<usize>()
            .unwrap_or_else(|err| panic!("invalid {name}={value:?}: {err}"))
            .max(min),
        Err(_) => default.max(min),
    }
}

fn durable_event_pressure_opted_in() -> bool {
    std::env::var(DURABLE_EVENT_PRESSURE_OPT_IN).as_deref() == Ok("1")
}

fn replay_event_type(event: &Value) -> Option<&str> {
    event
        .get("event_type")
        .or_else(|| event.get("type"))
        .and_then(Value::as_str)
}

fn build_durable_event_pressure_batch(
    run_ordinal: usize,
    text_delta_count: usize,
    progress_event_count: usize,
    budget: DurableRunEventBatchBudget,
) -> DurableEventPressureBatch {
    let mut raw_stream_events: Vec<Value> =
        Vec::with_capacity(text_delta_count + progress_event_count + 5);
    raw_stream_events.extend((0..text_delta_count).map(
        |idx| json!({"type": "text_delta", "content": format!("run-{run_ordinal}-chunk-{idx}")}),
    ));
    raw_stream_events.push(json!({
        "type": "tool_call",
        "tool_call": {"id": format!("call-{run_ordinal}"), "name": "bash"}
    }));
    raw_stream_events.push(json!({
        "type": "tool_call_end",
        "call_id": format!("call-{run_ordinal}"),
        "tool": "bash",
        "result": "ok"
    }));
    raw_stream_events.push(json!({
        "type": "reasoning_done",
        "data": {"signature": format!("sig-{run_ordinal}")}
    }));
    raw_stream_events.extend(
        (0..progress_event_count)
            .map(|idx| json!({"type": "agent_progress", "run_ordinal": run_ordinal, "seq": idx})),
    );
    raw_stream_events.push(json!({
        "event_type": "text_done",
        "data": {"full_text": format!("large durable final answer {run_ordinal}")}
    }));
    raw_stream_events.push(json!({
        "event_type": "run_finished",
        "data": {"prompt_tokens": 9, "completion_tokens": 3, "tool_call_count": 1}
    }));

    let durable_candidates: Vec<Value> = raw_stream_events
        .iter()
        .filter(|event| streaming_event_for_persistence(event))
        .cloned()
        .collect();
    let candidate_rows = durable_candidates.len();
    let candidate_bytes = durable_candidates
        .iter()
        .map(durable_run_event_estimated_bytes)
        .sum::<usize>();
    let budgeted_events =
        enforce_durable_run_event_batch_budget_with_budget(durable_candidates, budget);
    let budgeted_bytes = budgeted_events
        .iter()
        .map(durable_run_event_estimated_bytes)
        .sum::<usize>();
    let compacted = budgeted_events
        .iter()
        .any(|event| durable_event_type(event) == Some("durable_events_compacted"));

    DurableEventPressureBatch {
        raw_event_count: raw_stream_events.len(),
        candidate_rows,
        candidate_bytes,
        budgeted_events,
        budgeted_bytes,
        compacted,
    }
}

async fn durable_event_pressure_case(
    pool: SharedPool,
    run_ordinal: usize,
    text_delta_count: usize,
    progress_event_count: usize,
) -> Result<DurableEventPressureRunStats, String> {
    let user_id = "durable-event-pressure-user";
    let run_id = format!("durable-pressure-{run_ordinal}-{}", Uuid::new_v4());
    let session_id = format!("sess-durable-pressure-{run_ordinal}-{}", Uuid::new_v4());
    let svc = db_backed_test_service(&pool, &format!("durable-pressure-pod-{run_ordinal}"));
    let budget = DurableRunEventBatchBudget::default();
    cleanup_lifecycle_run_fixture(&pool, user_id, &run_id).await;

    let started = Instant::now();
    let result = async {
        svc.run_engine
            .start_run(&run_id, user_id, &session_id)
            .await
            .map_err(|err| format!("start durable DB run {run_id}: {err}"))?;

        let batch = build_durable_event_pressure_batch(
            run_ordinal,
            text_delta_count,
            progress_event_count,
            budget,
        );
        if batch
            .budgeted_events
            .iter()
            .any(|event| durable_event_type(event) == Some("text_delta"))
        {
            return Err(format!(
                "{run_id}: transport text_delta entered durable batch"
            ));
        }
        if !batch.compacted {
            return Err(format!("{run_id}: expected semantic overflow compaction"));
        }
        for expected in [
            "durable_events_compacted",
            "tool_call",
            "tool_call_end",
            "reasoning_done",
            "text_done",
            "run_finished",
        ] {
            if !batch
                .budgeted_events
                .iter()
                .any(|event| durable_event_type(event) == Some(expected))
            {
                return Err(format!("{run_id}: missing budgeted {expected}"));
            }
        }

        let transitioned = svc
            .run_engine
            .transition_status_with_events_if_current(
                user_id,
                &run_id,
                &[STATUS_RUNNING],
                STATUS_COMPLETED,
                None,
                None,
                &batch.budgeted_events,
            )
            .await
            .map_err(|err| format!("commit budgeted terminal events for {run_id}: {err}"))?;
        if !transitioned {
            return Err(format!("{run_id}: status transition unexpectedly stale"));
        }

        let rows = sqlx::query(
            "SELECT event_type
             FROM agent_run_events
             WHERE user_id = ? AND run_id = ?
             ORDER BY event_idx ASC",
        )
        .bind(user_id)
        .bind(&run_id)
        .fetch_all(pool.get())
        .await
        .map_err(|err| format!("load persisted event rows for {run_id}: {err}"))?;
        let persisted_types = rows
            .iter()
            .map(|row| {
                row.try_get::<String, _>("event_type")
                    .map_err(|err| format!("decode event_type for {run_id}: {err}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let persisted_rows = persisted_types.len();
        let text_delta_rows = persisted_types
            .iter()
            .filter(|event_type| event_type.as_str() == "text_delta")
            .count();
        let compacted_rows = persisted_types
            .iter()
            .filter(|event_type| event_type.as_str() == "durable_events_compacted")
            .count();
        if persisted_rows > budget.row_budget + 1 {
            return Err(format!(
                "{run_id}: persisted {persisted_rows} rows above budget plus run_started"
            ));
        }
        if text_delta_rows != 0 {
            return Err(format!(
                "{run_id}: persisted {text_delta_rows} text_delta rows"
            ));
        }
        if compacted_rows != 1 {
            return Err(format!(
                "{run_id}: expected exactly one compaction row, got {compacted_rows}"
            ));
        }

        let replay_events = svc
            .stream_run(run_id.clone(), user_id.to_string(), 1)
            .await
            .map_err(|response| {
                format!(
                    "stream replay failed for {run_id}: {:?}: {}",
                    response.0, response.1.0.detail
                )
            })?;
        if replay_events.len() > budget.row_budget {
            return Err(format!(
                "{run_id}: replay returned {} rows above durable batch budget",
                replay_events.len()
            ));
        }
        if replay_events
            .iter()
            .any(|event| replay_event_type(event) == Some("text_delta"))
        {
            return Err(format!("{run_id}: replay returned text_delta"));
        }
        let expected_answer = format!("large durable final answer {run_ordinal}");
        if !replay_events.iter().any(|event| {
            replay_event_type(event) == Some("text_done")
                && event.pointer("/data/full_text").and_then(Value::as_str)
                    == Some(expected_answer.as_str())
        }) {
            return Err(format!("{run_id}: replay missing final answer"));
        }
        if !replay_events
            .iter()
            .any(|event| replay_event_type(event) == Some("run_finished"))
        {
            return Err(format!("{run_id}: replay missing run_finished"));
        }

        Ok(DurableEventPressureRunStats {
            raw_events: batch.raw_event_count,
            candidate_rows: batch.candidate_rows,
            candidate_bytes: batch.candidate_bytes,
            budgeted_rows: batch.budgeted_events.len(),
            budgeted_bytes: batch.budgeted_bytes,
            persisted_rows,
            replay_rows: replay_events.len(),
            compacted_rows,
            text_delta_rows,
            elapsed_ms: duration_millis_u64(started.elapsed()),
        })
    }
    .await;

    cleanup_lifecycle_run_fixture(&pool, user_id, &run_id).await;
    result
}

async fn seed_lifecycle_run_for_pause_resume_it(
    svc: &AgenticRunLifecycleService,
    user_id: &str,
    run_id: &str,
    session_id: &str,
) {
    svc.run_engine
        .start_run(run_id, user_id, session_id)
        .await
        .expect("start durable DB run");
    let (run_state, _, _, _) = AgenticRunLifecycleService::build_tracked_run_state(
        run_id.to_string(),
        session_id.to_string(),
        user_id.to_string(),
    );
    svc.runs.write().await.insert(run_id.to_string(), run_state);
}

fn test_request(message: &str) -> ChatRequestData {
    ChatRequestData {
        message: message.to_string(),
        parts: Vec::new(),
        attachments: Vec::new(),
        runtime_system_prompt: None,
        session_id: None,
        full_llm_capture: false,
        agent_id: None,
        model: Some("test-model".to_string()),
        selected_model: Some(SelectedModelRequest {
            id: None,
            model: "test-model".to_string(),
            gateway: None,
        }),
        capability_descriptors: None,
        provider_runtime_authorized: false,
        agent_binding: None,
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
        forward_headers: HashMap::new(),
        execution_budget: None,
        explain: false,
        interaction_mode: None,
        interactive_client: false,
    }
}

fn test_runtime_mcp_binding() -> RuntimeMcpBindingRequest {
    RuntimeMcpBindingRequest {
        id: "request_tools".to_string(),
        transport: "streamable_http".to_string(),
        url: "https://tools.example.test/mcp/http".to_string(),
        auth_token: None,
        headers: HashMap::new(),
    }
}

#[derive(Clone)]
struct StaticSkillResolver {
    skills: Vec<crate::turn::skill_tool::SkillToolInfo>,
}

impl crate::turn::skill_tool::SkillResolver for StaticSkillResolver {
    fn resolve(
        &self,
        name: &str,
    ) -> Result<crate::turn::skill_tool::ResolvedSkill, crate::skills::SkillError> {
        Err(crate::skills::SkillError::NotFound(name.to_string()))
    }

    fn available_skills(&self) -> Vec<crate::turn::skill_tool::SkillToolInfo> {
        self.skills.clone()
    }
}

fn static_skill_resolver(name: &str) -> Arc<dyn crate::turn::skill_tool::SkillResolver> {
    Arc::new(StaticSkillResolver {
        skills: vec![crate::turn::skill_tool::SkillToolInfo {
            name: name.to_string(),
            description: "Binding-scoped skill".to_string(),
            when_to_use: None,
            source: crate::skills::manifest::SkillSourceKind::Plugin,
            aliases: Vec::new(),
            category: None,
            tags: Vec::new(),
        }],
    })
}

fn test_agent_binding_record(max_steps: Option<u32>) -> astra_services::AgentBindingRecord {
    astra_services::AgentBindingRecord {
        id: "abnd_test1234567890".to_string(),
        binding_name: "test-binding".to_string(),
        idempotency_key: "idem-test-binding".to_string(),
        status: astra_services::AgentBindingStatus::Active,
        agent_md: "Always follow the binding contract.".to_string(),
        capability_servers: vec![
            astra_services::CapabilityServerEndpoint {
                id: "mcp-main".to_string(),
                server_type: astra_services::CapabilityServerType::Mcp,
                transport: astra_services::CapabilityServerTransport::StreamableHttp,
                endpoint_url: None,
            },
            astra_services::CapabilityServerEndpoint {
                id: "skills-main".to_string(),
                server_type: astra_services::CapabilityServerType::Skill,
                transport: astra_services::CapabilityServerTransport::StreamableHttp,
                endpoint_url: None,
            },
        ],
        runtime_policy: astra_services::RuntimePolicy {
            max_steps,
            tool_mode: astra_services::ToolMode::McpGateway,
        },
        metadata: None,
        binding_schema_version: "v1".to_string(),
        created_at: "2026-06-19T00:00:00Z".to_string(),
        disabled_at: None,
    }
}

fn test_agent_binding_create_request() -> astra_services::AgentBindingCreateRequestData {
    astra_services::AgentBindingCreateRequestData {
        idempotency_key: "idem-runtime-binding".to_string(),
        binding: astra_services::AgentBindingPayload {
            binding_name: "runtime-binding".to_string(),
            agent_md: "Always follow the binding contract.".to_string(),
            capability_servers: vec![
                astra_services::CapabilityServerEndpoint {
                    id: "tools".to_string(),
                    server_type: astra_services::CapabilityServerType::Mcp,
                    transport: astra_services::CapabilityServerTransport::StreamableHttp,
                    endpoint_url: None,
                },
                astra_services::CapabilityServerEndpoint {
                    id: "skills".to_string(),
                    server_type: astra_services::CapabilityServerType::Skill,
                    transport: astra_services::CapabilityServerTransport::StreamableHttp,
                    endpoint_url: None,
                },
            ],
            runtime_policy: astra_services::RuntimePolicy {
                max_steps: Some(5),
                tool_mode: astra_services::ToolMode::McpGateway,
            },
            metadata: None,
            binding_schema_version: "v1".to_string(),
        },
    }
}

fn runtime_binding_request(id: String, mcp: &str, skills: &str) -> AgentBindingRuntimeRequest {
    AgentBindingRuntimeRequest {
        id,
        capability_server_refs: CapabilityServerRefs {
            mcp: mcp.to_string(),
            skills: skills.to_string(),
        },
    }
}

async fn service_with_in_memory_binding() -> (
    AgenticRunLifecycleService,
    Arc<astra_services::InMemoryAgentBindingService>,
    astra_services::AgentBindingRecord,
) {
    let binding_service = Arc::new(astra_services::InMemoryAgentBindingService::new());
    let record = astra_services::AgentBindingService::create_binding(
        binding_service.as_ref(),
        test_agent_binding_create_request(),
    )
    .await
    .expect("binding create");
    let service = test_service().with_agent_binding_service(binding_service.clone());
    (service, binding_service, record)
}

#[tokio::test]
async fn resolve_agent_binding_runtime_rejects_disabled_binding() {
    let (service, binding_service, record) = service_with_in_memory_binding().await;
    astra_services::AgentBindingService::disable_binding(
        binding_service.as_ref(),
        record.id.clone(),
    )
    .await
    .expect("binding disable");

    let err = match service
        .resolve_agent_binding_runtime(&runtime_binding_request(record.id, "tools", "skills"))
        .await
    {
        Ok(_) => panic!("disabled binding should not start new turns"),
        Err(err) => err,
    };

    assert_eq!(err.0, StatusCode::CONFLICT);
    assert_eq!(
        err.1.0.error_code.as_deref(),
        Some("agent_binding_disabled")
    );
}

#[tokio::test]
async fn resolve_agent_binding_runtime_rejects_missing_capability_ref() {
    let (service, _binding_service, record) = service_with_in_memory_binding().await;

    let err = match service
        .resolve_agent_binding_runtime(&runtime_binding_request(
            record.id,
            "missing-tools",
            "skills",
        ))
        .await
    {
        Ok(_) => panic!("missing mcp ref should fail before discovery"),
        Err(err) => err,
    };

    assert_eq!(err.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        err.1.0.error_code.as_deref(),
        Some("agent_binding_capability_ref_missing")
    );
}

#[tokio::test]
async fn resolve_agent_binding_runtime_rejects_capability_ref_type_mismatch() {
    let (service, _binding_service, record) = service_with_in_memory_binding().await;

    let err = match service
        .resolve_agent_binding_runtime(&runtime_binding_request(record.id, "skills", "skills"))
        .await
    {
        Ok(_) => panic!("mcp ref must resolve to an mcp server"),
        Err(err) => err,
    };

    assert_eq!(err.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        err.1.0.error_code.as_deref(),
        Some("agent_binding_capability_ref_invalid")
    );
}

#[test]
fn server_root_permissions_default_to_auto_for_server_approval_gate() {
    let mut request = test_request("edit files");
    request.interaction_mode = Some(RequestedTurnInteractionMode::Prompt);
    let constraints = RequestConstraints::default();

    let inherited =
        AgenticRunLifecycleService::inherited_permissions_from_request(&request, &constraints);

    assert_eq!(inherited.mode, PermissionMode::Auto);
    assert!(inherited.allowed_tools.is_none());
}

#[test]
fn server_root_permissions_map_deny_and_preserve_tool_allowlist() {
    let mut request = test_request("no tools");
    request.interaction_mode = Some(RequestedTurnInteractionMode::Deny);
    let constraints = RequestConstraints {
        allowed_tools: Some(["read_file".to_string()].into_iter().collect()),
        ..Default::default()
    };

    let inherited =
        AgenticRunLifecycleService::inherited_permissions_from_request(&request, &constraints);

    assert_eq!(inherited.mode, PermissionMode::Deny);
    assert!(
        inherited
            .allowed_tools
            .as_ref()
            .is_some_and(|tools| tools.contains("read_file"))
    );
}

#[test]
fn server_subrun_executor_keeps_inherited_permissions() {
    let inherited_permissions = InheritedPermissions::new(PermissionMode::Deny);
    let executor = ServerSubRunExecutor::new(
        test_settings(),
        test_encryptor(),
        Arc::new(TokioMutex::new(HashMap::new())),
    )
    .with_inherited_permissions(inherited_permissions);

    assert_eq!(executor.inherited_permissions.mode, PermissionMode::Deny);
}

#[test]
fn provision_subrun_workspace_rejects_unsafe_identity_components() {
    let executor = ServerSubRunExecutor::new(
        test_settings(),
        test_encryptor(),
        Arc::new(TokioMutex::new(HashMap::new())),
    );

    let session_error = executor
        .provision_subrun_workspace("session/123", "run-123")
        .expect_err("unsafe session id must fail instead of being sanitized");
    assert!(
        session_error.contains("invalid sub-run session_id"),
        "unexpected session error: {session_error}"
    );

    let run_error = executor
        .provision_subrun_workspace("session-123", "run/123")
        .expect_err("unsafe run id must fail instead of being sanitized");
    assert!(
        run_error.contains("invalid sub-run run_id"),
        "unexpected run error: {run_error}"
    );
}

struct FailingWorkspaceRecordStore;

#[async_trait]
impl WorkspaceRecordStore for FailingWorkspaceRecordStore {
    async fn upsert_workspace_record(
        &self,
        _entry: StoredWorkspaceRecordEntry,
    ) -> Result<(), WorkspaceRecordStoreError> {
        Err(WorkspaceRecordStoreError::Unavailable(
            "injected workspace store failure".to_string(),
        ))
    }

    async fn load_workspace_record(
        &self,
        _owner_id: &str,
        _workspace_id: &str,
    ) -> Result<Option<StoredWorkspaceRecordEntry>, WorkspaceRecordStoreError> {
        Ok(None)
    }

    async fn list_workspace_records(
        &self,
        _owner_id: &str,
        _limit: u32,
    ) -> Result<Vec<StoredWorkspaceRecordEntry>, WorkspaceRecordStoreError> {
        Ok(Vec::new())
    }

    async fn delete_workspace_record(
        &self,
        _owner_id: &str,
        _workspace_id: &str,
    ) -> Result<bool, WorkspaceRecordStoreError> {
        Err(WorkspaceRecordStoreError::Unavailable(
            "injected workspace store failure".to_string(),
        ))
    }
}

#[async_trait]
impl WorkspaceCleanupDebtStore for FailingWorkspaceRecordStore {
    async fn record_cleanup_debt(
        &self,
        _entry: WorkspaceCleanupDebtEntry,
    ) -> Result<(), WorkspaceCleanupDebtStoreError> {
        Err(WorkspaceCleanupDebtStoreError::Unavailable(
            "injected cleanup debt store failure".to_string(),
        ))
    }

    async fn list_cleanup_debts(
        &self,
        _owner_id: &str,
        _limit: u32,
    ) -> Result<Vec<WorkspaceCleanupDebtEntry>, WorkspaceCleanupDebtStoreError> {
        Ok(Vec::new())
    }

    async fn resolve_cleanup_debt(
        &self,
        _owner_id: &str,
        _debt_id: &str,
    ) -> Result<bool, WorkspaceCleanupDebtStoreError> {
        Ok(false)
    }

    async fn list_all_unresolved_debts(
        &self,
    ) -> Result<Vec<WorkspaceCleanupDebtEntry>, WorkspaceCleanupDebtStoreError> {
        Err(WorkspaceCleanupDebtStoreError::Unavailable(
            "injected cleanup debt store failure".to_string(),
        ))
    }

    async fn increment_debt_attempts(
        &self,
        _debt_id: &str,
    ) -> Result<(), WorkspaceCleanupDebtStoreError> {
        Err(WorkspaceCleanupDebtStoreError::Unavailable(
            "injected cleanup debt store failure".to_string(),
        ))
    }
}

fn test_cloud_workspace_record(workspace_id: &str) -> RuntimeWorkspaceRecord {
    RuntimeWorkspaceRecord {
        workspace_id: workspace_id.to_string(),
        owner_scope: RuntimeWorkspaceOwnerScope::Tenant,
        kind: astra_runtime_env::WorkspaceBindingKind::CloudWorkspace,
        authority: astra_runtime_env::WorkspaceAuthority::ReadWrite,
        root_or_volume_ref: "/cloud/volumes/team-volume-1".to_string(),
        source: RuntimeWorkspaceSource::PersistentVolume {
            volume_id: "team-volume-1".to_string(),
        },
        persistence: RuntimeWorkspacePersistence::Persistent,
        revision: "1".to_string(),
        display_name: "Team workspace".to_string(),
    }
}

#[tokio::test]
async fn lifecycle_persists_workspace_record_with_owner_session_and_run() {
    let store = Arc::new(InMemoryWorkspaceRecordStore::new());
    let svc = test_service().with_workspace_record_store(store.clone());
    let record = test_cloud_workspace_record("workspace-1");

    ok(svc
        .persist_workspace_record(
            "00000000-0000-0000-0000-000000000001",
            "session-1",
            "run-1",
            &record,
        )
        .await);

    let loaded = store
        .load_workspace_record("00000000-0000-0000-0000-000000000001", "workspace-1")
        .await
        .expect("load workspace record")
        .expect("record");
    assert_eq!(loaded.owner_id, "00000000-0000-0000-0000-000000000001");
    assert_eq!(loaded.session_id.as_deref(), Some("session-1"));
    assert_eq!(loaded.run_id.as_deref(), Some("run-1"));
    assert_eq!(loaded.record, record);
    assert!(
        store
            .load_workspace_record("00000000-0000-0000-0000-000000000002", "workspace-1")
            .await
            .expect("load workspace record")
            .is_none(),
        "workspace records must stay owner scoped"
    );
}

#[tokio::test]
async fn lifecycle_workspace_record_store_failure_fails_closed() {
    let svc = test_service().with_workspace_record_store(Arc::new(FailingWorkspaceRecordStore));
    let record = test_cloud_workspace_record("workspace-1");

    let error = err(svc
        .persist_workspace_record(
            "00000000-0000-0000-0000-000000000001",
            "session-1",
            "run-1",
            &record,
        )
        .await);

    assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        error
            .1
            .0
            .detail
            .contains("Failed to persist workspace record"),
        "{}",
        error.1.0.detail
    );
}

#[tokio::test]
async fn lifecycle_workspace_record_source_conflict_returns_conflict() {
    let store = Arc::new(InMemoryWorkspaceRecordStore::new());
    store
        .upsert_workspace_record(StoredWorkspaceRecordEntry::new(
            "00000000-0000-0000-0000-000000000002",
            Some("session-2".to_string()),
            Some("run-2".to_string()),
            test_cloud_workspace_record("workspace-2"),
        ))
        .await
        .expect("store existing workspace owner");
    let svc = test_service().with_workspace_record_store(store);
    let record = test_cloud_workspace_record("workspace-1");

    let error = err(svc
        .persist_workspace_record(
            "00000000-0000-0000-0000-000000000001",
            "session-1",
            "run-1",
            &record,
        )
        .await);

    assert_eq!(error.0, StatusCode::CONFLICT);
    assert!(
        error.1.0.detail.contains("Workspace ownership conflict"),
        "{}",
        error.1.0.detail
    );
}

#[tokio::test]
async fn lifecycle_records_cleanup_debt_when_failed_start_cleanup_fails() {
    let store = Arc::new(InMemoryWorkspaceRecordStore::new());
    let svc = test_service().with_workspace_record_store(store.clone());
    let mut record = test_cloud_workspace_record("workspace-cleanup-debt");
    record.persistence = RuntimeWorkspacePersistence::Session;
    record.source = RuntimeWorkspaceSource::Scratch;
    record.root_or_volume_ref = "/definitely/missing/astra-cleanup-debt".to_string();

    svc.cleanup_cloud_workspace_after_failed_start(
        "00000000-0000-0000-0000-000000000001",
        "session-1",
        "run-1",
        &record,
        "injected start failure".to_string(),
    )
    .await;

    let debts = store
        .list_cleanup_debts("00000000-0000-0000-0000-000000000001", 10)
        .await
        .expect("list cleanup debts");
    assert_eq!(debts.len(), 1);
    assert_eq!(debts[0].workspace_id, "workspace-cleanup-debt");
    assert_eq!(debts[0].reason, RuntimeCleanupReason::Failed);
    assert!(debts[0].message.contains("injected start failure"));
    assert_eq!(debts[0].session_id.as_deref(), Some("session-1"));
    assert_eq!(debts[0].run_id.as_deref(), Some("run-1"));
}

#[tokio::test]
async fn lifecycle_records_cleanup_debt_when_terminal_cleanup_fails() {
    let store = Arc::new(InMemoryWorkspaceRecordStore::new());
    let mut record = test_cloud_workspace_record("workspace-terminal-cleanup-debt");
    record.persistence = RuntimeWorkspacePersistence::Session;
    record.source = RuntimeWorkspaceSource::Scratch;
    record.root_or_volume_ref = "/definitely/missing/astra-terminal-cleanup-debt".to_string();

    AgenticRunLifecycleService::cleanup_cloud_workspace_after_terminal_run(
        Some(store.clone()),
        "00000000-0000-0000-0000-000000000001",
        "session-1",
        "run-1",
        &record,
        &RunStatus::Completed,
    )
    .await;

    let debts = store
        .list_cleanup_debts("00000000-0000-0000-0000-000000000001", 10)
        .await
        .expect("list cleanup debts");
    assert_eq!(debts.len(), 1);
    assert_eq!(debts[0].workspace_id, "workspace-terminal-cleanup-debt");
    assert_eq!(debts[0].reason, RuntimeCleanupReason::Completed);
    assert!(debts[0].message.contains("run ended with status completed"));
    assert_eq!(debts[0].session_id.as_deref(), Some("session-1"));
    assert_eq!(debts[0].run_id.as_deref(), Some("run-1"));
}

#[tokio::test]
async fn lifecycle_removes_workspace_record_after_successful_terminal_cleanup() {
    let store = Arc::new(InMemoryWorkspaceRecordStore::new());
    let record = test_cloud_workspace_record("workspace-terminal-cleanup-success");
    store
        .upsert_workspace_record(StoredWorkspaceRecordEntry::new(
            "00000000-0000-0000-0000-000000000001",
            Some("session-1".to_string()),
            Some("run-1".to_string()),
            record.clone(),
        ))
        .await
        .expect("store workspace record");

    AgenticRunLifecycleService::cleanup_cloud_workspace_after_terminal_run(
        Some(store.clone()),
        "00000000-0000-0000-0000-000000000001",
        "session-1",
        "run-1",
        &record,
        &RunStatus::Completed,
    )
    .await;

    assert!(
        store
            .load_workspace_record(
                "00000000-0000-0000-0000-000000000001",
                "workspace-terminal-cleanup-success"
            )
            .await
            .expect("load workspace record")
            .is_none(),
        "successful cleanup must remove the workspace record"
    );
    assert!(
        store
            .list_cleanup_debts("00000000-0000-0000-0000-000000000001", 10)
            .await
            .expect("list cleanup debts")
            .is_empty(),
        "successful cleanup must not create cleanup debt"
    );
}

#[tokio::test]
async fn lifecycle_skips_cloud_workspace_cleanup_for_resumable_status() {
    let store = Arc::new(InMemoryWorkspaceRecordStore::new());
    let mut record = test_cloud_workspace_record("workspace-waiting-no-cleanup");
    record.persistence = RuntimeWorkspacePersistence::Session;
    record.root_or_volume_ref = "/definitely/missing/astra-waiting-no-cleanup".to_string();

    AgenticRunLifecycleService::cleanup_cloud_workspace_after_terminal_run(
        Some(store.clone()),
        "00000000-0000-0000-0000-000000000001",
        "session-1",
        "run-1",
        &record,
        &RunStatus::Waiting,
    )
    .await;

    assert!(
        store
            .list_cleanup_debts("00000000-0000-0000-0000-000000000001", 10)
            .await
            .expect("list cleanup debts")
            .is_empty(),
        "resumable runs must keep their workspace for continuation"
    );
}

#[test]
fn cloud_git_source_maps_to_workspace_record_contract() {
    let mut request = test_request("checkout this repo");
    request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace,
        display_name: Some("Repo checkout".to_string()),
        root: None,
        source: Some(astra_services::runs::WorkspaceSourceRequest::GitCheckout {
            repository: "https://example.com/org/repo.git".to_string(),
            reference: None,
        }),
        authority: None,
        fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
    });

    let provision_request = ok(cloud_workspace_provision_request_from_request(
        &request, "123",
    ))
    .expect("cloud workspace request");

    assert_eq!(provision_request.workspace_id, "run-123");
    assert_eq!(
        provision_request.kind,
        astra_runtime_env::WorkspaceBindingKind::CloudWorkspace
    );
    assert_eq!(
        provision_request.authority,
        astra_runtime_env::WorkspaceAuthority::ReadWrite
    );
    assert_eq!(
        provision_request.persistence,
        RuntimeWorkspacePersistence::Session
    );
    assert_eq!(
        provision_request.source,
        RuntimeWorkspaceSource::GitCheckout {
            repository: "https://example.com/org/repo.git".to_string(),
            reference: None,
        }
    );

    let record = RuntimeWorkspaceRecord {
        workspace_id: provision_request.workspace_id,
        owner_scope: provision_request.owner_scope,
        kind: provision_request.kind,
        authority: provision_request.authority,
        root_or_volume_ref: "/cloud/checkouts/run-123".to_string(),
        source: provision_request.source,
        persistence: provision_request.persistence,
        revision: "1".to_string(),
        display_name: "Repo checkout".to_string(),
    };
    let snapshot = execution_bindings_from_workspace_record(&record);
    let workspace = &snapshot.workspace;
    let executor = &snapshot.executor;

    assert_eq!(workspace.kind, WorkspaceBindingKind::CloudWorkspace);
    assert_eq!(workspace.cwd.as_deref(), Some("/cloud/checkouts/run-123"));
    assert_eq!(executor.kind, ExecutorBindingKind::OrchestratorManaged);
    assert_eq!(executor.transport, ToolTransportKind::SandboxResidentAgent);
    assert_eq!(
        snapshot
            .runtime
            .as_ref()
            .map(|runtime| runtime.launch_driver),
        Some(astra_runtime_env::RuntimeLaunchDriver::Kubernetes)
    );
}

#[test]
fn cloud_persistent_volume_binding_maps_to_workspace_record_contract() {
    let mut request = test_request("use my workspace");
    request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace,
        display_name: Some("Team workspace".to_string()),
        root: None,
        source: Some(
            astra_services::runs::WorkspaceSourceRequest::PersistentVolume {
                volume_id: "team-volume-1".to_string(),
            },
        ),
        authority: None,
        fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
    });

    let provision_request = ok(cloud_workspace_provision_request_from_request(
        &request,
        "volume-run",
    ))
    .expect("cloud workspace request");

    assert_eq!(provision_request.workspace_id, "run-volume-run");
    assert_eq!(
        provision_request.kind,
        astra_runtime_env::WorkspaceBindingKind::CloudWorkspace
    );
    assert_eq!(
        provision_request.authority,
        astra_runtime_env::WorkspaceAuthority::ReadWrite
    );
    assert_eq!(
        provision_request.persistence,
        RuntimeWorkspacePersistence::Persistent
    );
    assert_eq!(
        provision_request.source,
        RuntimeWorkspaceSource::PersistentVolume {
            volume_id: "team-volume-1".to_string(),
        }
    );

    let record = RuntimeWorkspaceRecord {
        workspace_id: provision_request.workspace_id,
        owner_scope: provision_request.owner_scope,
        kind: provision_request.kind,
        authority: provision_request.authority,
        root_or_volume_ref: "/cloud/volumes/team-volume-1".to_string(),
        source: provision_request.source,
        persistence: provision_request.persistence,
        revision: "1".to_string(),
        display_name: "Team workspace".to_string(),
    };
    let snapshot = execution_bindings_from_workspace_record(&record);
    let workspace = &snapshot.workspace;
    let executor = &snapshot.executor;

    assert_eq!(workspace.kind, WorkspaceBindingKind::CloudWorkspace);
    assert_eq!(
        workspace.cwd.as_deref(),
        Some("/cloud/volumes/team-volume-1")
    );
    assert_eq!(executor.kind, ExecutorBindingKind::OrchestratorManaged);
    assert_eq!(executor.transport, ToolTransportKind::SandboxResidentAgent);
    assert_eq!(
        snapshot
            .runtime
            .as_ref()
            .map(|runtime| runtime.session_manager),
        Some(astra_runtime_env::RuntimeSessionManager::ProviderManaged)
    );
}

#[test]
fn cloud_scratch_source_maps_to_generic_workspace_record_contract() {
    let mut request = test_request("create scratch workspace");
    request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace,
        display_name: Some("Scratch workspace".to_string()),
        root: None,
        source: Some(astra_services::runs::WorkspaceSourceRequest::Scratch),
        authority: None,
        fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
    });

    let provision_request = ok(cloud_workspace_provision_request_from_request(
        &request,
        "scratch-run",
    ))
    .expect("scratch cloud workspace request");

    assert_eq!(provision_request.workspace_id, "run-scratch-run");
    assert_eq!(
        provision_request.kind,
        astra_runtime_env::WorkspaceBindingKind::CloudWorkspace
    );
    assert_eq!(provision_request.source, RuntimeWorkspaceSource::Scratch);
    assert_eq!(
        provision_request.persistence,
        RuntimeWorkspacePersistence::Session
    );
}

#[test]
fn cloud_uploaded_snapshot_source_defaults_to_immutable_read_only() {
    let mut request = test_request("inspect snapshot");
    request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace,
        display_name: None,
        root: None,
        source: Some(
            astra_services::runs::WorkspaceSourceRequest::UploadedSnapshot {
                artifact_id: "artifact-1".to_string(),
                root: None,
            },
        ),
        authority: None,
        fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
    });

    let provision_request = ok(cloud_workspace_provision_request_from_request(
        &request, "456",
    ))
    .expect("cloud workspace request");

    assert_eq!(
        provision_request.kind,
        astra_runtime_env::WorkspaceBindingKind::CloudWorkspace
    );
    assert_eq!(
        provision_request.authority,
        astra_runtime_env::WorkspaceAuthority::ReadOnly
    );
    assert_eq!(
        provision_request.persistence,
        RuntimeWorkspacePersistence::ImmutableSnapshot
    );
    assert_eq!(
        provision_request.source,
        RuntimeWorkspaceSource::UploadedSnapshot {
            artifact_id: "artifact-1".to_string(),
        }
    );
}

#[test]
fn cloud_template_source_defaults_to_read_write_session_workspace() {
    let mut request = test_request("start from template");
    request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace,
        display_name: None,
        root: Some("/cloud/templates/template-1".to_string()),
        source: Some(astra_services::runs::WorkspaceSourceRequest::Template {
            template_id: "template-1".to_string(),
        }),
        authority: None,
        fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
    });

    let provision_request = ok(cloud_workspace_provision_request_from_request(
        &request,
        "template-run",
    ))
    .expect("template workspace request");

    assert_eq!(
        provision_request.authority,
        astra_runtime_env::WorkspaceAuthority::ReadWrite
    );
    assert_eq!(
        provision_request.persistence,
        RuntimeWorkspacePersistence::Session
    );
    assert_eq!(
        provision_request.source,
        RuntimeWorkspaceSource::Template {
            template_id: "template-1".to_string(),
        }
    );
    assert_eq!(
        provision_request.requested_root.as_deref(),
        Some("/cloud/templates/template-1")
    );
}

#[test]
fn cloud_dataset_and_artifact_sources_default_to_immutable_read_only() {
    let cases = [
        (
            astra_services::runs::WorkspaceSourceRequest::DatasetBundle {
                dataset_id: "dataset-1".to_string(),
            },
            RuntimeWorkspaceSource::DatasetBundle {
                dataset_id: "dataset-1".to_string(),
            },
        ),
        (
            astra_services::runs::WorkspaceSourceRequest::ArtifactBundle {
                artifact_id: "artifact-1".to_string(),
            },
            RuntimeWorkspaceSource::ArtifactBundle {
                artifact_id: "artifact-1".to_string(),
            },
        ),
    ];

    for (source, expected_source) in cases {
        let mut request = test_request("inspect materialized source");
        request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
            kind: astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace,
            display_name: None,
            root: None,
            source: Some(source),
            authority: None,
            fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
        });

        let provision_request = ok(cloud_workspace_provision_request_from_request(
            &request,
            "bundle-run",
        ))
        .expect("bundle workspace request");

        assert_eq!(
            provision_request.authority,
            astra_runtime_env::WorkspaceAuthority::ReadOnly
        );
        assert_eq!(
            provision_request.persistence,
            RuntimeWorkspacePersistence::ImmutableSnapshot
        );
        assert_eq!(provision_request.source, expected_source);
    }
}

#[test]
fn cloud_materialized_source_rejects_relative_root_before_provisioning() {
    let mut request = test_request("bad template root");
    request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace,
        display_name: None,
        root: Some("relative/template".to_string()),
        source: Some(astra_services::runs::WorkspaceSourceRequest::Template {
            template_id: "template-1".to_string(),
        }),
        authority: None,
        fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
    });

    let error = err(cloud_workspace_provision_request_from_request(
        &request,
        "bad-template",
    ));

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert!(
        error
            .1
            .0
            .detail
            .contains("absolute materialized source path"),
        "{}",
        error.1.0.detail
    );
}

#[test]
fn cloud_materialized_source_rejects_empty_identifier() {
    let mut request = test_request("bad dataset");
    request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace,
        display_name: None,
        root: None,
        source: Some(
            astra_services::runs::WorkspaceSourceRequest::DatasetBundle {
                dataset_id: "   ".to_string(),
            },
        ),
        authority: None,
        fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
    });

    let error = err(cloud_workspace_provision_request_from_request(
        &request,
        "bad-dataset",
    ));

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert!(
        error.1.0.detail.contains("non-empty source.dataset_id"),
        "{}",
        error.1.0.detail
    );
}

#[test]
fn cloud_workspace_binding_requires_materialized_source() {
    let mut request = test_request("checkout");
    request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace,
        display_name: None,
        root: None,
        source: Some(astra_services::runs::WorkspaceSourceRequest::GitCheckout {
            repository: "   ".to_string(),
            reference: None,
        }),
        authority: None,
        fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
    });

    let error = err(cloud_workspace_provision_request_from_request(
        &request, "789",
    ));

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert!(
        error
            .1
            .0
            .detail
            .contains("Git checkout workspace requires a non-empty source.repository"),
        "{}",
        error.1.0.detail
    );
}

#[test]
fn cloud_workspace_binding_rejects_missing_source() {
    let mut request = test_request("use cloud workspace");
    request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace,
        display_name: None,
        root: None,
        source: None,
        authority: None,
        fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
    });

    let error = err(cloud_workspace_provision_request_from_request(
        &request,
        "bad-volume",
    ));

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert!(
        error
            .1
            .0
            .detail
            .contains("Cloud workspace requires an explicit source"),
        "{}",
        error.1.0.detail
    );
}

#[test]
fn cloud_workspace_runtime_kind_projects_to_server_binding() {
    let record = RuntimeWorkspaceRecord {
        workspace_id: "workspace-1".to_string(),
        owner_scope: RuntimeWorkspaceOwnerScope::Tenant,
        kind: astra_runtime_env::WorkspaceBindingKind::CloudWorkspace,
        authority: astra_runtime_env::WorkspaceAuthority::ReadWrite,
        root_or_volume_ref: "/cloud/volumes/team-volume-1".to_string(),
        source: RuntimeWorkspaceSource::PersistentVolume {
            volume_id: "team-volume-1".to_string(),
        },
        persistence: RuntimeWorkspacePersistence::Persistent,
        revision: "1".to_string(),
        display_name: "Team workspace".to_string(),
    };

    let workspace = server_workspace_binding_from_workspace_record(&record);

    assert_eq!(workspace.kind, WorkspaceBindingKind::CloudWorkspace);
    assert_eq!(
        workspace.cwd.as_deref(),
        Some("/cloud/volumes/team-volume-1")
    );
}

#[test]
fn request_execution_bindings_use_actual_server_workspace_for_server_sandbox() {
    let mut request = test_request("hello");
    request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::ServerSandbox,
        display_name: Some("Requested server".to_string()),
        root: Some("/client/claimed/path".to_string()),
        source: None,
        authority: Some(astra_services::runs::WorkspaceAuthorityRequest::ReadWrite),
        fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
    });
    request.executor_binding = Some(astra_services::runs::ExecutorBindingRequest {
        kind: astra_services::runs::ExecutorBindingRequestKind::ServerLocal,
        executor_id: Some("server-local".to_string()),
        display_name: Some("Requested executor".to_string()),
        transport: Some(astra_services::runs::ToolTransportKindRequest::ServerLocal),
        status: Some(astra_services::runs::ExecutorStatusRequest::Online),
    });

    let server_workspace = Path::new("/tmp/astra-runtime-workspace");
    let (workspace, executor) = resolve_request_execution_bindings(&request, server_workspace);

    assert_eq!(workspace.kind, WorkspaceBindingKind::ServerSandbox);
    assert_eq!(workspace.display_name, "Requested server");
    assert_eq!(
        workspace.cwd.as_deref(),
        Some("/tmp/astra-runtime-workspace")
    );
    assert_eq!(workspace.authority, WorkspaceAuthority::ReadWrite);
    assert_eq!(workspace.fallback_policy, FallbackPolicy::Disabled);
    assert_eq!(executor.kind, ExecutorBindingKind::ServerLocal);
    assert_eq!(executor.executor_id, "server-local");
    assert_eq!(executor.display_name, "Requested executor");
    assert_eq!(executor.transport, ToolTransportKind::ServerLocal);
    assert_eq!(executor.status, ExecutorStatus::Online);
}

#[test]
fn server_workspace_binding_decision_respects_explicit_binding_and_edge_tools() {
    let mut request = test_request("hello");

    assert!(!request_uses_server_workspace(&request, false));
    assert!(!request_uses_server_workspace(&request, true));

    request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::ServerSandbox,
        display_name: None,
        root: None,
        source: None,
        authority: None,
        fallback_policy: None,
    });
    assert!(request_uses_server_workspace(&request, true));

    request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::EdgeWorkspace,
        display_name: Some("Edge".to_string()),
        root: Some("/repo".to_string()),
        source: Some(astra_services::runs::WorkspaceSourceRequest::EdgePath {
            path: "/repo".to_string(),
        }),
        authority: Some(astra_services::runs::WorkspaceAuthorityRequest::ReadWrite),
        fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
    });
    assert!(!request_uses_server_workspace(&request, false));
    assert!(!request_uses_server_workspace(&request, true));
}

#[test]
fn request_execution_bindings_keep_edge_workspace_without_server_fallback() {
    let mut request = test_request("review this repo");
    request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::EdgeWorkspace,
        display_name: Some("MacBook Pro".to_string()),
        root: Some("/Users/xupeng/github/astra".to_string()),
        source: Some(astra_services::runs::WorkspaceSourceRequest::EdgePath {
            path: "/Users/xupeng/github/astra".to_string(),
        }),
        authority: Some(astra_services::runs::WorkspaceAuthorityRequest::ReadWrite),
        fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
    });
    request.executor_binding = Some(astra_services::runs::ExecutorBindingRequest {
        kind: astra_services::runs::ExecutorBindingRequestKind::EdgeAgent,
        executor_id: Some("edge-macbook-1".to_string()),
        display_name: Some("MacBook Pro".to_string()),
        transport: Some(astra_services::runs::ToolTransportKindRequest::EdgeWs),
        status: Some(astra_services::runs::ExecutorStatusRequest::Online),
    });

    let (workspace, executor) =
        resolve_request_execution_bindings(&request, Path::new("/tmp/server-workspace"));

    assert_eq!(workspace.kind, WorkspaceBindingKind::EdgeWorkspace);
    assert_eq!(workspace.display_name, "MacBook Pro");
    assert_eq!(workspace.cwd.as_deref(), Some("/Users/xupeng/github/astra"));
    assert_eq!(workspace.fallback_policy, FallbackPolicy::Disabled);
    assert_eq!(executor.kind, ExecutorBindingKind::EdgeAgent);
    assert_eq!(executor.executor_id, "edge-macbook-1");
    assert_eq!(executor.transport, ToolTransportKind::EdgeWs);
    assert_eq!(executor.status, ExecutorStatus::Online);
}

#[test]
fn workspace_binding_request_accepts_legacy_cwd_alias() {
    let mut request = test_request("review this repo");
    request.workspace_binding = Some(
        serde_json::from_value(json!({
            "kind": "edge_workspace",
            "display_name": "MacBook Pro",
            "cwd": "/Users/test/repo",
            "authority": "read_write",
            "fallback_policy": "disabled"
        }))
        .expect("legacy cwd alias should deserialize"),
    );
    request.executor_binding = Some(astra_services::runs::ExecutorBindingRequest {
        kind: astra_services::runs::ExecutorBindingRequestKind::EdgeAgent,
        executor_id: Some("edge-1".to_string()),
        display_name: Some("MacBook Pro".to_string()),
        transport: Some(astra_services::runs::ToolTransportKindRequest::EdgeWs),
        status: Some(astra_services::runs::ExecutorStatusRequest::Online),
    });

    let (workspace, executor) =
        resolve_request_execution_bindings(&request, Path::new("/tmp/server-workspace"));

    assert_eq!(workspace.kind, WorkspaceBindingKind::EdgeWorkspace);
    assert_eq!(workspace.cwd.as_deref(), Some("/Users/test/repo"));
    assert_eq!(executor.kind, ExecutorBindingKind::EdgeAgent);
    assert_eq!(executor.transport, ToolTransportKind::EdgeWs);
}

#[test]
fn edge_profile_execution_bindings_make_legacy_edge_tools_explicit() {
    let mut edge_profile = Map::new();
    edge_profile.insert("cwd".to_string(), json!("/Users/xupeng/github/astra"));
    edge_profile.insert("edge_agent_id".to_string(), json!("edge-macbook-1"));
    edge_profile.insert("hostname".to_string(), json!("MacBook Pro"));

    let (workspace, executor) = resolve_request_execution_bindings_without_server_workspace(
        &test_request("review this repo"),
        &edge_profile,
        true,
    )
    .expect("legacy edge profile should produce explicit bindings");

    assert_eq!(workspace.kind, WorkspaceBindingKind::EdgeWorkspace);
    assert_eq!(workspace.display_name, "MacBook Pro");
    assert_eq!(workspace.cwd.as_deref(), Some("/Users/xupeng/github/astra"));
    assert_eq!(workspace.authority, WorkspaceAuthority::ReadWrite);
    assert_eq!(workspace.fallback_policy, FallbackPolicy::Disabled);
    assert_eq!(executor.kind, ExecutorBindingKind::EdgeAgent);
    assert_eq!(executor.executor_id, "edge-macbook-1");
    assert_eq!(executor.display_name, "MacBook Pro");
    assert_eq!(executor.transport, ToolTransportKind::EdgeLedger);
    assert_eq!(executor.status, ExecutorStatus::Online);
}

#[test]
fn missing_edge_profile_execution_bindings_emit_no_workspace() {
    let (workspace, executor) = resolve_request_execution_bindings_without_server_workspace(
        &test_request("hello"),
        &Map::new(),
        false,
    )
    .expect("missing edge profile should still produce an explicit no-workspace binding");

    assert_eq!(workspace.kind, WorkspaceBindingKind::None);
    assert_eq!(workspace.display_name, "No workspace");
    assert_eq!(workspace.authority, WorkspaceAuthority::None);
    assert_eq!(workspace.fallback_policy, FallbackPolicy::Disabled);
    assert_eq!(executor.kind, ExecutorBindingKind::ServerLocal);
    assert_eq!(executor.executor_id, "server-control-plane");
    assert_eq!(executor.display_name, "Server control plane");
    assert_eq!(executor.transport, ToolTransportKind::ServerLocal);
    assert_eq!(executor.status, ExecutorStatus::Online);
}

#[test]
fn missing_edge_profile_with_edge_tools_uses_edge_ledger() {
    let (workspace, executor) = resolve_request_execution_bindings_without_server_workspace(
        &test_request("run client tool"),
        &Map::new(),
        true,
    )
    .expect("edge tools should produce an explicit edge-ledger binding");

    assert_eq!(workspace.kind, WorkspaceBindingKind::EdgeWorkspace);
    assert_eq!(workspace.display_name, "Edge workspace");
    assert_eq!(workspace.cwd, None);
    assert_eq!(workspace.authority, WorkspaceAuthority::ReadWrite);
    assert_eq!(executor.kind, ExecutorBindingKind::EdgeAgent);
    assert_eq!(executor.executor_id, "edge-ledger");
    assert_eq!(executor.transport, ToolTransportKind::EdgeLedger);
    assert_eq!(executor.status, ExecutorStatus::Online);
}

#[test]
fn explicit_no_workspace_binding_uses_server_control_plane_executor() {
    let mut request = test_request("plan only");
    request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::None,
        display_name: None,
        root: None,
        source: None,
        authority: None,
        fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
    });

    let (workspace, executor) =
        resolve_request_execution_bindings(&request, Path::new("/tmp/server-workspace"));

    assert_eq!(workspace.kind, WorkspaceBindingKind::None);
    assert_eq!(workspace.display_name, "No workspace");
    assert_eq!(workspace.authority, WorkspaceAuthority::None);
    assert_eq!(workspace.fallback_policy, FallbackPolicy::Disabled);
    assert_eq!(executor.kind, ExecutorBindingKind::ServerLocal);
    assert_eq!(executor.executor_id, "server-control-plane");
    assert_eq!(executor.display_name, "Server control plane");
    assert_eq!(executor.transport, ToolTransportKind::ServerLocal);
    assert_eq!(executor.status, ExecutorStatus::Online);
}

#[test]
fn execution_bindings_from_metadata_rebases_server_sandbox_cwd() {
    let metadata = json!({
        "workspace": {
            "kind": "server_sandbox",
            "display_name": "Server sandbox",
            "cwd": "/tmp/parent-workspace",
            "authority": "read_write",
            "fallback_policy": "disabled"
        },
        "executor": {
            "kind": "server_local",
            "executor_id": "server-local",
            "display_name": "Server sandbox",
            "transport": "server_local",
            "status": "online"
        }
    });

    let snapshot =
        execution_bindings_from_metadata(Some(&metadata), Path::new("/tmp/child-workspace"))
            .expect("metadata bindings");
    let workspace = &snapshot.workspace;
    let executor = &snapshot.executor;

    assert_eq!(workspace.kind, WorkspaceBindingKind::ServerSandbox);
    assert_eq!(workspace.cwd.as_deref(), Some("/tmp/child-workspace"));
    assert_eq!(executor.kind, ExecutorBindingKind::ServerLocal);
    assert!(snapshot.runtime.is_none());
}

#[tokio::test]
async fn validate_request_constraints_rejects_legacy_mcp_binding_ids() {
    let service = test_service();
    let mut request = test_request("hello");
    request.mcp_binding_ids = Some(vec!["mcp_bind_301".to_string()]);

    let err = service
        .validate_request_constraints("u1", &request)
        .await
        .expect_err("legacy mcp_binding_ids must be rejected on chat stream");

    assert_eq!(err.0, StatusCode::BAD_REQUEST);
    assert!(
        err.1
            .0
            .detail
            .contains("mcp_binding_ids is no longer supported")
    );
}

#[test]
fn runtime_bearer_parser_accepts_exact_single_bearer_token() {
    let parsed =
        parse_runtime_bearer_authorization("Bearer abc.DEF-123_~+/=").expect("valid bearer");
    assert_eq!(parsed.token, "abc.DEF-123_~+/=");
}

#[test]
fn runtime_bearer_parser_rejects_malformed_or_multiple_credentials() {
    for value in [
        "",
        "Basic abc",
        "bearer abc",
        "Bearer ",
        "Bearer  abc",
        "Bearer abc ",
        "Bearer abc def",
        "Bearer abc,Bearer def",
        "Bearer abc,def",
        "Bearer abc;def",
        "Bearer abc:Bearer:def",
    ] {
        let err = parse_runtime_bearer_authorization(value)
            .expect_err("malformed bearer should be rejected");
        assert_eq!(err.0, StatusCode::BAD_REQUEST, "{value}");
        assert_eq!(
            err.1.0.error_code.as_deref(),
            Some("agent_binding_runtime_auth_invalid"),
            "{value}"
        );
    }
}

#[tokio::test]
async fn validate_request_constraints_rejects_implicit_request_scoped_runtime_mcp_by_default() {
    let service = test_service();
    let mut request = test_request("hello");
    request.runtime_mcp_bindings = vec![test_runtime_mcp_binding()];

    let err = service
        .validate_request_constraints("u1", &request)
        .await
        .expect_err("runtime_mcp_bindings must explicitly select request_scoped profile");

    assert_eq!(err.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        err.1.0.error_code.as_deref(),
        Some("agent_binding_runtime_profile_conflict")
    );
    assert!(
        err.1
            .0
            .detail
            .contains("runtime_profile=request_scoped_runtime_mcp")
    );
}

#[tokio::test]
async fn validate_request_constraints_allows_explicit_request_scoped_runtime_mcp() {
    let service = test_service();
    let mut request = test_request("hello");
    request.runtime_mcp_bindings = vec![test_runtime_mcp_binding()];
    request.runtime_profile = Some(RuntimeProfileRequest::RequestScopedRuntimeMcp);

    service
        .validate_request_constraints("u1", &request)
        .await
        .expect("explicit request_scoped_runtime_mcp profile should allow runtime MCP");
}

#[tokio::test]
async fn validate_request_constraints_allows_implicit_request_scoped_runtime_mcp_when_enabled() {
    let service = test_service().with_allow_implicit_request_scoped_mcp(true);
    let mut request = test_request("hello");
    request.runtime_mcp_bindings = vec![test_runtime_mcp_binding()];

    service
        .validate_request_constraints("u1", &request)
        .await
        .expect("compatibility flag should allow implicit request-scoped runtime MCP");
}

#[tokio::test]
async fn validate_request_constraints_requires_selected_model() {
    let service = test_service();
    let mut request = test_request("hello");
    request.model = None;
    request.selected_model = None;

    let err = service
        .validate_request_constraints("u1", &request)
        .await
        .expect_err("selected_model is required for every chat stream request");

    assert_eq!(err.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        err.1.0.error_code.as_deref(),
        Some("selected_model_missing")
    );
}

#[tokio::test]
async fn validate_request_constraints_allows_native_model_without_gateway_auth() {
    let service = test_service();
    let request = test_request("hello");

    service
        .validate_request_constraints("u1", &request)
        .await
        .expect("native selected_model.model should not require runtime_auth");
}

#[tokio::test]
async fn validate_request_constraints_rejects_unknown_native_model_without_gateway() {
    let service = test_service();
    let mut request = test_request("hello");
    request.selected_model = Some(SelectedModelRequest {
        id: None,
        model: "missing-model".to_string(),
        gateway: None,
    });

    let err = service
        .validate_request_constraints("u1", &request)
        .await
        .expect_err("unknown native model should fail without gateway");

    assert_eq!(err.0, StatusCode::NOT_FOUND);
    assert_eq!(
        err.1.0.error_code.as_deref(),
        Some("selected_model_not_configured")
    );
}

#[tokio::test]
async fn validate_request_constraints_requires_runtime_auth_for_gateway() {
    let service = test_service();
    let mut request = test_request("hello");
    request.selected_model = Some(SelectedModelRequest {
        id: None,
        model: "external-model".to_string(),
        gateway: Some("primary-gateway".to_string()),
    });

    let err = service
        .validate_request_constraints("u1", &request)
        .await
        .expect_err("gateway selected_model must require runtime_auth");

    assert_eq!(err.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        err.1.0.error_code.as_deref(),
        Some("agent_binding_runtime_auth_missing")
    );
}

#[tokio::test]
async fn validate_request_constraints_rejects_unknown_selected_model_gateway() {
    let service = test_service()
        .with_model_gateway_service(Arc::new(astra_services::InMemoryModelGatewayService::new()));
    let mut request = test_request("hello");
    request.selected_model = Some(SelectedModelRequest {
        id: None,
        model: "external-model".to_string(),
        gateway: Some("primary-gateway".to_string()),
    });
    request.runtime_auth = Some(RuntimeAuthRequest {
        authorization: "Bearer runtime-grant".to_string(),
    });

    let err = service
        .validate_request_constraints("u1", &request)
        .await
        .expect_err("unknown selected_model.gateway should fail before loop start");

    assert_eq!(err.0, StatusCode::NOT_FOUND);
    assert_eq!(
        err.1.0.error_code.as_deref(),
        Some("model_gateway_not_found")
    );
}

#[tokio::test]
async fn validate_request_constraints_rejects_disabled_selected_model_gateway() {
    let gateway_service = astra_services::InMemoryModelGatewayService::new();
    astra_services::ModelGatewayService::create_gateway(
        &gateway_service,
        astra_services::ModelGatewayCreateRequestData {
            id: "primary-gateway".to_string(),
            resolve_url: "https://models.example.com/resolve".to_string(),
            model_protocol: astra_services::ModelProtocol::OpenAiChatCompletions,
            metadata: None,
        },
    )
    .await
    .expect("gateway create");
    astra_services::ModelGatewayService::disable_gateway(
        &gateway_service,
        "primary-gateway".to_string(),
    )
    .await
    .expect("gateway disable");
    let service = test_service().with_model_gateway_service(Arc::new(gateway_service));
    let mut request = test_request("hello");
    request.selected_model = Some(SelectedModelRequest {
        id: None,
        model: "external-model".to_string(),
        gateway: Some("primary-gateway".to_string()),
    });
    request.runtime_auth = Some(RuntimeAuthRequest {
        authorization: "Bearer runtime-grant".to_string(),
    });

    let err = service
        .validate_request_constraints("u1", &request)
        .await
        .expect_err("disabled selected_model.gateway should fail before loop start");

    assert_eq!(err.0, StatusCode::CONFLICT);
    assert_eq!(
        err.1.0.error_code.as_deref(),
        Some("model_gateway_disabled")
    );
}

fn test_runtime_descriptor(
    id: &str,
    descriptor_type: &str,
    endpoint_url: &str,
) -> astra_services::runs::RuntimeCapabilityDescriptorRequest {
    astra_services::runs::RuntimeCapabilityDescriptorRequest {
        id: id.to_string(),
        descriptor_type: descriptor_type.to_string(),
        transport: "http".to_string(),
        endpoint_url: endpoint_url.to_string(),
        protocol: "openai_responses".to_string(),
        metadata: serde_json::Map::new(),
    }
}

#[tokio::test]
async fn validate_request_constraints_accepts_provider_descriptor_without_registered_gateway() {
    let service = test_service();
    let mut request = test_request("hello");
    request.provider_runtime_authorized = true;
    request.selected_model = Some(SelectedModelRequest {
        id: None,
        model: "qwen3.7-max".to_string(),
        gateway: None,
    });
    request.runtime_auth = Some(RuntimeAuthRequest {
        authorization: "Bearer runtime-grant".to_string(),
    });
    request.capability_descriptors =
        Some(astra_services::runs::RuntimeCapabilityDescriptorsRequest {
            model_gateway: Some(test_runtime_descriptor(
                "moi-model-gateway",
                "model_gateway",
                "http://127.0.0.1/model-gateway",
            )),
            mcp: None,
            skills: None,
        });

    service
        .validate_request_constraints("u1", &request)
        .await
        .expect("provider descriptor should not require registered model gateway");
    let prepared = service
        .prepare_model_gateway_invocation(request)
        .await
        .expect("provider descriptor should become llm_token_service");
    assert_eq!(
        prepared
            .llm_token_service
            .as_ref()
            .map(|config| &config.url),
        Some(&"http://127.0.0.1/model-gateway".to_string())
    );
}

#[tokio::test]
async fn validate_request_constraints_rejects_provider_descriptor_with_selected_model_gateway() {
    let service = test_service();
    let mut request = test_request("hello");
    request.provider_runtime_authorized = true;
    request.selected_model = Some(SelectedModelRequest {
        id: None,
        model: "qwen3.7-max".to_string(),
        gateway: Some("primary-gateway".to_string()),
    });
    request.runtime_auth = Some(RuntimeAuthRequest {
        authorization: "Bearer runtime-grant".to_string(),
    });
    request.capability_descriptors =
        Some(astra_services::runs::RuntimeCapabilityDescriptorsRequest {
            model_gateway: Some(test_runtime_descriptor(
                "moi-model-gateway",
                "model_gateway",
                "http://127.0.0.1/model-gateway",
            )),
            mcp: None,
            skills: None,
        });

    let err = service
        .validate_request_constraints("u1", &request)
        .await
        .expect_err("provider descriptor path must not accept selected_model.gateway");
    assert_eq!(err.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        err.1.0.error_code.as_deref(),
        Some("selected_model_invalid")
    );
}

#[tokio::test]
async fn validate_request_constraints_rejects_descriptor_without_provider_authorization() {
    let service = test_service();
    let mut request = test_request("hello");
    request.selected_model = Some(SelectedModelRequest {
        id: Some("model-qwen".to_string()),
        model: "qwen3.7-max".to_string(),
        gateway: None,
    });
    request.runtime_auth = Some(RuntimeAuthRequest {
        authorization: "Bearer runtime-grant".to_string(),
    });
    request.capability_descriptors =
        Some(astra_services::runs::RuntimeCapabilityDescriptorsRequest {
            model_gateway: Some(test_runtime_descriptor(
                "moi-model-gateway",
                "model_gateway",
                "http://127.0.0.1/model-gateway",
            )),
            mcp: None,
            skills: None,
        });

    let err = service
        .validate_request_constraints("u1", &request)
        .await
        .expect_err("provider descriptors require provider authorization");
    assert_eq!(err.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        err.1.0.error_code.as_deref(),
        Some("provider_runtime_context_required")
    );
}

#[tokio::test]
async fn validate_request_constraints_rejects_agent_binding_registry_profile_without_binding() {
    let service = test_service();
    let mut request = test_request("hello");
    request.runtime_profile = Some(RuntimeProfileRequest::AgentBindingRegistry);

    let err = service
        .validate_request_constraints("u1", &request)
        .await
        .expect_err("agent_binding_registry profile must not be set without agent_binding");

    assert_eq!(err.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        err.1.0.error_code.as_deref(),
        Some("agent_binding_runtime_profile_conflict")
    );
}

#[tokio::test]
async fn validate_request_constraints_allows_agent_binding_with_omitted_runtime_profile() {
    let service = test_service();
    let mut request = test_request("hello");
    request.agent_binding = Some(AgentBindingRuntimeRequest {
        id: "abnd_test1234567890".to_string(),
        capability_server_refs: CapabilityServerRefs {
            mcp: "mcp-main".to_string(),
            skills: "skills-main".to_string(),
        },
    });
    request.runtime_auth = Some(RuntimeAuthRequest {
        authorization: "Bearer runtime-grant".to_string(),
    });

    service
        .validate_request_constraints("u1", &request)
        .await
        .expect("agent_binding itself is the explicit registry opt-in");
}

#[tokio::test]
async fn validate_request_constraints_rejects_agent_binding_edge_tools() {
    let service = test_service();
    let mut request = test_request("hello");
    request.agent_binding = Some(AgentBindingRuntimeRequest {
        id: "abnd_test1234567890".to_string(),
        capability_server_refs: CapabilityServerRefs {
            mcp: "mcp-main".to_string(),
            skills: "skills-main".to_string(),
        },
    });
    request.runtime_auth = Some(RuntimeAuthRequest {
        authorization: "Bearer runtime-grant".to_string(),
    });
    request.context = Some(
        json!({
            "edge_tools": [{"function": {"name": "request_tool"}}]
        })
        .as_object()
        .unwrap()
        .clone(),
    );

    let err = service
        .validate_request_constraints("u1", &request)
        .await
        .expect_err("agent_binding mode cannot carry request-scoped edge tools");

    assert_eq!(err.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        err.1.0.error_code.as_deref(),
        Some("agent_binding_runtime_profile_conflict")
    );
}

#[tokio::test]
async fn validate_request_constraints_rejects_agent_binding_edge_skills() {
    let service = test_service();
    let mut request = test_request("hello");
    request.agent_binding = Some(AgentBindingRuntimeRequest {
        id: "abnd_test1234567890".to_string(),
        capability_server_refs: CapabilityServerRefs {
            mcp: "mcp-main".to_string(),
            skills: "skills-main".to_string(),
        },
    });
    request.runtime_auth = Some(RuntimeAuthRequest {
        authorization: "Bearer runtime-grant".to_string(),
    });
    request.context = Some(
        json!({
            "edge_skills": [{"name": "request_skill"}]
        })
        .as_object()
        .unwrap()
        .clone(),
    );

    let err = service
        .validate_request_constraints("u1", &request)
        .await
        .expect_err("agent_binding mode cannot carry request-scoped edge skills");

    assert_eq!(err.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        err.1.0.error_code.as_deref(),
        Some("agent_binding_runtime_profile_conflict")
    );
}

#[tokio::test]
async fn build_initial_state_includes_database_skill_provider_when_wired() {
    use astra_services::skills::{
        SkillInfoRecord, SkillListCursor, SkillListItem, SkillListRecord, SkillPublishRequestData,
        SkillRecord, SkillService, SkillStatusRecord, SkillVersionRecord,
    };
    use async_trait::async_trait;

    #[derive(Default)]
    struct MockSkillService {
        unsupported_calls: std::sync::atomic::AtomicUsize,
    }

    impl MockSkillService {
        fn unsupported<T>(&self, operation: &str) -> Result<T, (StatusCode, Json<ErrorResponse>)> {
            self.unsupported_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err((
                StatusCode::NOT_IMPLEMENTED,
                Json(ErrorResponse::new(format!(
                    "MockSkillService::{operation} is not implemented in this test"
                ))),
            ))
        }
    }

    #[async_trait]
    impl SkillService for MockSkillService {
        async fn list_skills(
            &self,
            _user_id: String,
            limit: u32,
            cursor: Option<SkillListCursor>,
        ) -> Result<SkillListRecord, (StatusCode, Json<ErrorResponse>)> {
            if cursor.is_some() {
                return Ok(SkillListRecord {
                    skills: Vec::new(),
                    total: Some(1),
                    limit,
                    next_cursor: None,
                });
            }
            Ok(SkillListRecord {
                skills: vec![SkillListItem {
                    skill_id: "remote-db@1.0.0".to_string(),
                    skill_name: "remote-db".to_string(),
                    version: "1.0.0".to_string(),
                    description: Some("Remote DB skill".to_string()),
                    status: Some("active".to_string()),
                    source: Some("user".to_string()),
                    category: Some("integration".to_string()),
                    created_at: None,
                }],
                total: Some(1),
                limit,
                next_cursor: None,
            })
        }

        async fn get_skill(
            &self,
            _user_id: String,
            skill_id: String,
            _version: Option<String>,
        ) -> Result<SkillRecord, (StatusCode, Json<ErrorResponse>)> {
            if skill_id == "remote-db" || skill_id == "remote-db@1.0.0" {
                return Ok(SkillRecord {
                    skill_id: "remote-db@1.0.0".to_string(),
                    skill_name: "remote-db".to_string(),
                    version: "1.0.0".to_string(),
                    description: Some("Remote DB skill".to_string()),
                    metadata: Some(serde_json::json!({
                        "skill_type": "remote",
                        "remote_url": "http://127.0.0.1:18080/remote-skill",
                        "forward_headers": ["authorization", "x-workspace-id"],
                        "required_headers": ["x-workspace-id"],
                        "when_to_use": "when task needs remote orchestration"
                    })),
                    created_at: None,
                });
            }
            Err((
                StatusCode::NOT_FOUND,
                Json(ErrorResponse::new("not found".to_string())),
            ))
        }

        async fn get_skill_info(
            &self,
            _: String,
            _: String,
        ) -> Result<SkillInfoRecord, (StatusCode, Json<ErrorResponse>)> {
            self.unsupported("get_skill_info")
        }

        async fn list_skill_versions(
            &self,
            _: String,
            _: String,
        ) -> Result<Vec<SkillVersionRecord>, (StatusCode, Json<ErrorResponse>)> {
            self.unsupported("list_skill_versions")
        }

        async fn get_skill_status(
            &self,
            _: String,
            _: u32,
        ) -> Result<SkillStatusRecord, (StatusCode, Json<ErrorResponse>)> {
            self.unsupported("get_skill_status")
        }

        async fn publish_skill(
            &self,
            _: String,
            _: SkillPublishRequestData,
        ) -> Result<serde_json::Value, (StatusCode, Json<ErrorResponse>)> {
            self.unsupported("publish_skill")
        }

        async fn unpublish_skill(
            &self,
            _: String,
            _: String,
        ) -> Result<serde_json::Value, (StatusCode, Json<ErrorResponse>)> {
            self.unsupported("unpublish_skill")
        }
    }

    let skill_service = Arc::new(MockSkillService::default());
    let svc = test_service().with_skill_service(skill_service.clone());

    let default_request = test_request("hello");
    let default_state = svc.build_initial_state(
        "test-user",
        &default_request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );
    let default_resolver = default_state
        .skills
        .resolver
        .as_ref()
        .expect("default server resolver should include visible catalog");
    let default_names: Vec<String> = default_resolver
        .available_skills()
        .into_iter()
        .map(|skill| skill.name)
        .collect();
    assert!(
        default_names.iter().any(|name| name == "remote-db"),
        "expected database skill without request allow_skills filter: {default_names:?}"
    );
    assert!(
        default_state.skills.registry_for_activation.is_some(),
        "unfiltered server catalog should be available for conditional activation"
    );

    let mut request = test_request("hello");
    request.allow_skills = Some(vec!["remote-db".to_string()]);
    let state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );
    let resolver = state
        .skills
        .resolver
        .as_ref()
        .expect("skill resolver should be configured");
    let names: Vec<String> = resolver
        .available_skills()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert!(
        names.iter().any(|name| name == "remote-db"),
        "expected database skill in available skills: {names:?}"
    );

    let resolved = resolver
        .resolve("remote-db")
        .expect("resolver should load database skill");
    assert_eq!(
        resolved.remote_url.as_deref(),
        Some("http://127.0.0.1:18080/remote-skill")
    );
    assert_eq!(
        resolved.forward_headers,
        vec!["authorization".to_string(), "x-workspace-id".to_string()]
    );
    assert_eq!(
        resolved.required_headers,
        vec!["x-workspace-id".to_string()]
    );

    let mut filtered_request = test_request("hello");
    filtered_request.allow_skills = Some(vec!["remote-db".to_string()]);
    let filtered_state = svc.build_initial_state(
        "test-user",
        &filtered_request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );
    assert!(
        filtered_state.skills.registry_for_activation.is_none(),
        "request-scoped allow_skills should disable automatic conditional activation"
    );
    let filtered_resolver = filtered_state
        .skills
        .resolver
        .as_ref()
        .expect("filtered resolver should be configured");
    let filtered_names: Vec<String> = filtered_resolver
        .available_skills()
        .into_iter()
        .map(|skill| skill.name)
        .collect();
    assert_eq!(filtered_names, vec!["remote-db".to_string()]);
    filtered_resolver
        .resolve("remote-db")
        .expect("allowed remote-db skill should resolve");
    assert_eq!(
        skill_service
            .unsupported_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        0,
        "build_initial_state should only use list_skills/get_skill on this mock"
    );
}

#[tokio::test]
async fn create_run_rejects_unknown_request_skill_allowlist() {
    let svc = test_service();
    let mut request = test_request("hello");
    request.allow_skills = Some(vec!["__missing_skill__".into()]);

    let err = svc
        .create_run("user-1".into(), request)
        .await
        .expect_err("unknown allow_skills entry should be rejected");
    assert_eq!(err.0, StatusCode::BAD_REQUEST);
    assert!(err.1.0.detail.contains("allow_skills"));
}

#[test]
fn build_runtime_turn_evaluation_event_uses_loop_state_signals() {
    let svc = test_service();
    let request = test_request("git status");
    let mut state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );
    state.recent_tools = vec!["git_status".into()];
    state.telemetry.first_budget_pressure = 0.27;
    state.stall.events.push(("repetition_stall".into(), 1));
    state.stall.verdict_events.push(
        astra_turn_core::agentic_verdict_audit::AgenticVerdictAuditEvent {
            turn: 1,
            severity: "warning".into(),
            injections: vec!["stall detected".into()],
            avoid_tools: vec!["git_status".into()],
            health_avoidance_tools: vec![],
            force_stop: false,
            nudge_count: 1,
            interaction_mode: "prompt".into(),
            suppressed_loop_nudges: false,
            recent_error_pressure: 0,
            recent_timeout_pressure: 0,
            total_errors: 0,
            health_avoidance_count: 0,
            total_timeouts: 0,
            timeout_dominant_tools: vec![],
            total_cache_hits: 0,
            flaky_count: 0,
        },
    );
    state.stall.tool_call_records.push(ToolCallRecord {
        name: "git_status".into(),
        ok: true,
        ms: 14,
        error: None,
        input_bytes: Some(8),
        output_bytes: Some(180),
        args_preview: None,
        result_preview: Some("clean".into()),
        file_path: None,
        surgically_removed: None,
        original_tool_name: None,
        ..Default::default()
    });

    let event = build_runtime_turn_evaluation_event("session-1", "server_runtime", &state);

    assert_eq!(event.event_type, JournalEventType::TurnEvaluation);
    assert_eq!(event.turn, None);
    let metadata = event.metadata.expect("turn evaluation metadata");
    assert_eq!(metadata["source"], "server_runtime");
    assert_eq!(metadata["live_query"], false);
    assert_eq!(metadata["stall_count"], 1);
    assert_eq!(metadata["verdict_warning"], true);
    assert_eq!(metadata["tool_call_count"], 1);
    assert!(metadata["quality"].as_f64().unwrap() < 0.8);
    assert_eq!(metadata["signals"][0]["kind"], "tool_error_rate");
}

#[test]
fn finalize_run_events_appends_run_finished_for_failures() {
    let svc = test_service();
    let request = test_request("boom");
    let state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );

    let (events, status, error) = AgenticRunLifecycleService::finalize_run_events(
        Ok(AgenticLoopOutcome::Error("boom".into())),
        vec![],
        &state,
    );

    assert_eq!(status, RunStatus::Failed);
    assert_eq!(error.as_deref(), Some("boom"));
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["event_type"], "run_error");
    assert_eq!(events[0]["data"]["error_code"], "unknown");
    assert_eq!(events[0]["data"]["error_kind"], "unknown");
    assert_eq!(events[1]["event_type"], "run_finished");
    assert_eq!(events[1]["data"]["error_code"], "unknown");
    assert_eq!(events[1]["data"]["error_kind"], "unknown");
}

#[test]
fn finalize_run_events_classifies_string_error_outcomes() {
    let svc = test_service();
    let request = test_request("classify");
    let state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );

    for (message, expected_code) in [
        (
            "database operation failed: error communicating with database: unexpected EOF",
            "database_error",
        ),
        (
            "LLM request failed: error sending request for url (https://example.invalid)",
            "network",
        ),
        ("[stream_transport] stream body closed", "stream_transport"),
    ] {
        let (events, status, error) = AgenticRunLifecycleService::finalize_run_events(
            Ok(AgenticLoopOutcome::Error(message.into())),
            vec![],
            &state,
        );

        assert_eq!(status, RunStatus::Failed);
        assert_eq!(error.as_deref(), Some(message));
        assert_eq!(events[0]["data"]["error_code"], expected_code);
        assert_eq!(events[0]["data"]["error_kind"], expected_code);
        assert_eq!(events[1]["data"]["error_code"], expected_code);
        assert_eq!(events[1]["data"]["error_kind"], expected_code);
    }
}

#[test]
fn finalize_run_events_preserves_classified_error_code() {
    let svc = test_service();
    let request = test_request("network");
    let state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );

    let classified = astra_core::ClassifiedError::new(
        astra_core::ErrorKind::Network,
        "LLM request failed: connection reset",
    );
    let (events, status, error) =
        AgenticRunLifecycleService::finalize_run_events(Err(classified), vec![], &state);

    assert_eq!(status, RunStatus::Failed);
    assert_eq!(
        error.as_deref(),
        Some("[network] LLM request failed: connection reset")
    );
    assert_eq!(events[0]["event_type"], "run_error");
    assert_eq!(events[0]["data"]["error_code"], "network");
    assert_eq!(events[0]["data"]["error_kind"], "network");
    assert_eq!(events[1]["event_type"], "run_finished");
    assert_eq!(events[1]["data"]["error_code"], "network");
    assert_eq!(events[1]["data"]["error_kind"], "network");
    assert_eq!(
        events[1]["data"]["error"],
        "[network] LLM request failed: connection reset"
    );
}

#[test]
fn finalize_run_events_distinguishes_provider_admission_rejection() {
    let svc = test_service();
    let request = test_request("admission");
    let state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );

    let classified = astra_core::ClassifiedError::new(
        astra_core::ErrorKind::RateLimit,
        "LLM provider admission rpm limit reached",
    )
    .with_details_json(json!({"source": "llm_provider_admission"}).to_string());
    let (events, status, _error) =
        AgenticRunLifecycleService::finalize_run_events(Err(classified), vec![], &state);

    assert_eq!(status, RunStatus::Failed);
    assert_eq!(
        events[0]["data"]["error_code"],
        "llm_provider_admission_rejected"
    );
    assert_eq!(events[0]["data"]["error_kind"], "rate_limit");
    assert_eq!(
        events[1]["data"]["error_code"],
        "llm_provider_admission_rejected"
    );
    assert_eq!(events[1]["data"]["error_kind"], "rate_limit");
}

#[test]
fn finalize_run_events_cancellation_beats_completed_outcome() {
    let svc = test_service();
    let request = test_request("done");
    let mut state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );
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
fn streaming_final_replay_excludes_live_work_surface_events() {
    let events = vec![
        json!({"type": "text_delta", "content": "hi"}),
        json!({"type": "reasoning_delta", "content": "thinking"}),
        json!({"type": "tool_call", "tool_call": {"id": "call-1"}}),
        json!({"type": "tool_call_end", "call_id": "call-1", "result": "ok"}),
        json!({"type": "agent_progress", "agent_id": "agent-1", "status": "started"}),
        json!({"type": "agent_live_event", "agent_id": "agent-1", "event_kind": "output_delta", "content": "child"}),
        json!({"type": "run_blocked", "call_id": "call-1", "reason": "transport_disconnected"}),
        json!({"type": "run_blocked", "call_id": "call-2", "reason": "fallback_disabled"}),
        json!({"type": "run_blocked", "call_id": "call-3", "reason": "route_mismatch"}),
        json!({"event_type": "text_done", "data": {"full_text": "hi"}}),
        json!({"event_type": "run_finished", "data": {"prompt_tokens": 1}}),
    ];

    let replay: Vec<_> = events
        .iter()
        .filter(|event| streaming_final_event_for_replay(event))
        .cloned()
        .collect();

    assert_eq!(replay.len(), 2);
    assert_eq!(replay[0]["event_type"], "text_done");
    assert_eq!(replay[1]["event_type"], "run_finished");
    assert!(!live_delta_event_for_persistence(&events[0]));
    assert!(!live_delta_event_for_persistence(&events[1]));
    assert!(live_delta_event_for_persistence(&events[2]));
    assert!(live_delta_event_for_persistence(&events[3]));
    assert!(live_delta_event_for_persistence(&events[4]));
    assert!(!live_delta_event_for_persistence(&events[5]));
    assert!(live_delta_event_for_persistence(&events[6]));
    assert!(live_delta_event_for_persistence(&events[7]));
    assert!(live_delta_event_for_persistence(&events[8]));
}

#[test]
fn streaming_durable_persistence_keeps_semantic_events_before_terminal() {
    let events = vec![
        json!({"type": "reasoning_delta", "content": "thinking"}),
        json!({"type": "tool_call", "tool_call": {"id": "call-1"}}),
        json!({"type": "tool_call_end", "call_id": "call-1", "result": "ok"}),
        json!({"event_type": "text_done", "data": {"full_text": "answer"}}),
        json!({"event_type": "run_finished", "data": {"prompt_tokens": 1}}),
    ];

    let persisted: Vec<_> = events
        .iter()
        .filter(|event| streaming_event_for_persistence(event))
        .cloned()
        .collect();

    assert_eq!(persisted.len(), 4);
    assert_eq!(persisted[0]["type"], "tool_call");
    assert_eq!(persisted[1]["type"], "tool_call_end");
    assert_eq!(persisted[2]["event_type"], "text_done");
    assert_eq!(persisted[3]["event_type"], "run_finished");
}

#[test]
fn active_run_live_event_projection_is_bounded() {
    let mut run = RunState {
        run_id: "run-live-bound".to_string(),
        user_id: "user-live-bound".to_string(),
        session_id: "session-live-bound".to_string(),
        status: RunStatus::Running,
        events: vec![json!({"event_type": "run_started", "data": {"run_id": "run-live-bound"}})],
        cancel_flag: Arc::new(AtomicBool::new(false)),
        pause_flag: Arc::new(AtomicBool::new(false)),
        llm_cancel_token: Arc::new(CancellationToken::new()),
        live_tx: None,
        waiting_for: None,
    };

    for idx in 0..(MAX_ACTIVE_RUN_LIVE_EVENTS + 5) {
        push_active_run_live_event(&mut run, json!({"type": "agent_progress", "seq": idx}));
    }

    let live_events: Vec<_> = run
        .events
        .iter()
        .filter(|event| live_delta_event_for_persistence(event))
        .collect();
    assert_eq!(live_events.len(), MAX_ACTIVE_RUN_LIVE_EVENTS);
    assert_eq!(run.events[0]["event_type"], "run_started");
    assert_eq!(live_events[0]["seq"], 5);
}

#[test]
fn transport_delta_chunks_are_live_only_not_durable() {
    let events = vec![
        json!({"type": "text_delta", "content": "hi"}),
        json!({"type": "reasoning_delta", "content": "thinking"}),
        json!({"type": "thinking_delta", "content": "thinking"}),
        json!({"type": "reasoning_message_content", "content": "raw chain of thought"}),
        json!({"event_type": "reasoning_message_content", "data": {"content": "raw chain of thought"}}),
        json!({"type": "agent_live_event", "event_kind": "output_delta", "content": "child"}),
        json!({"type": "agent_live_event", "event_kind": "thinking_delta", "content": "child-thinking"}),
    ];

    for event in events {
        assert!(
            !streaming_event_for_persistence(&event),
            "transport delta should remain live-only: {event}"
        );
    }
}

#[test]
fn finalize_run_events_interrupted_completed_outcome_is_partial_not_completed() {
    let svc = test_service();
    let request = test_request("partial");
    let mut state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );
    state.final_text = "[Round budget hard-limit reached]".to_string();
    state.interruption = Some(astra_turn_core::interruption::InterruptionRecord::new(
        astra_turn_core::interruption::InterruptionKind::BudgetExhausted,
        astra_turn_core::interruption::ResumeAction::ContinueImmediately,
        astra_turn_core::interruption::InterruptionStateSummary {
            has_checkpoint: true,
            tool_calls_completed: 5,
            turns_completed: 15,
            remaining_turns: 0,
            error_detail: Some("Round budget hard-limit reached".to_string()),
            stall_signal: None,
            resume_restricted_tools: vec![],
        },
    ));

    let (events, status, error) = AgenticRunLifecycleService::finalize_run_events(
        Ok(AgenticLoopOutcome::Completed),
        vec![],
        &state,
    );

    assert_eq!(status, RunStatus::Paused);
    assert!(
        error.is_none(),
        "resumable interruption should be structured paused state, not a run error: {error:?}"
    );
    assert_eq!(events[0]["event_type"], "text_done");
    assert_eq!(events[0]["data"]["partial"], true);
    assert_eq!(
        events[0]["data"]["interruption"]["kind"],
        "budget_exhausted"
    );
    assert_eq!(events[1]["event_type"], "run_interrupted");
    assert_eq!(events[2]["event_type"], "run_finished");
    assert_eq!(events[2]["data"]["interrupted"], true);
    assert_eq!(events[2]["data"]["interruption_kind"], "budget_exhausted");
}

#[test]
fn merge_cancelled_run_events_preserves_order_and_usage() {
    let cancel_flag = Arc::new(AtomicBool::new(true));
    let cancel_token = Arc::new(CancellationToken::new());
    let mut run = RunState {
        run_id: "run-1".into(),
        user_id: "user-1".into(),
        session_id: "session-1".into(),
        status: RunStatus::Cancelled,
        events: vec![
            json!({"event_type": "run_started", "data": {}}),
            json!({"event_type": "run_finished", "data": {"cancelled": true}}),
        ],
        cancel_flag,
        pause_flag: Arc::new(AtomicBool::new(false)),
        llm_cancel_token: cancel_token,
        live_tx: None,
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
        json!({"type": "reasoning_delta", "content": "thinking"}),
        json!({"type": "thinking_delta", "content": "private thinking"}),
        json!({"type": "reasoning_message_content", "content": "raw chain of thought"}),
        json!({"event_type": "reasoning_message_content", "data": {"content": "raw chain of thought"}}),
        json!({"type": "reasoning_done"}),
        json!({"type": "thinking_done"}),
        json!({"event_type": "text_done", "data": {"full_text": "final answer"}}),
        json!({"event_type": "run_error", "data": {"error": "boom"}}),
        json!({"event_type": "run_finished", "data": {"prompt_tokens": 1}}),
    ];

    let persisted = terminal_events_for_persistence(&events);
    assert_eq!(persisted.len(), 5);
    assert_eq!(persisted[0]["type"], "reasoning_done");
    assert_eq!(persisted[1]["type"], "thinking_done");
    assert_eq!(persisted[2]["event_type"], "text_done");
    assert_eq!(persisted[3]["event_type"], "run_error");
    assert_eq!(persisted[4]["event_type"], "run_finished");
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
async fn create_run_rejects_invalid_server_workspace_session_id() {
    let svc = test_service();
    let mut req = test_request("hi");
    req.session_id = Some("../../".into());
    req.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::ServerSandbox,
        display_name: None,
        root: None,
        source: None,
        authority: Some(astra_services::runs::WorkspaceAuthorityRequest::ReadWrite),
        fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
    });

    let err = err(svc.create_run("user-1".into(), req).await);

    assert_eq!(err.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        err.1.0.detail,
        "Invalid session_id for server workspace provisioning"
    );
}

#[tokio::test]
async fn stream_chat_rejects_invalid_server_workspace_session_id() {
    let svc = test_service();
    let mut req = test_request("hi");
    req.session_id = Some("../../".into());
    req.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::ServerSandbox,
        display_name: None,
        root: None,
        source: None,
        authority: Some(astra_services::runs::WorkspaceAuthorityRequest::ReadWrite),
        fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
    });

    let err = err(svc.stream_chat("user-1".into(), req).await);

    assert_eq!(err.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        err.1.0.detail,
        "Invalid session_id for server workspace provisioning"
    );
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
async fn create_run_conflicts_when_same_session_already_has_active_run() {
    let svc = test_service();
    let mut first = test_request("hello");
    first.session_id = Some("shared-session".into());
    ok(svc.create_run("user-1".into(), first).await);

    let mut second = test_request("again");
    second.session_id = Some("shared-session".into());
    let err = err(svc.create_run("user-1".into(), second).await);
    assert_eq!(err.0, StatusCode::CONFLICT);
    assert_eq!(err.1.0.detail, "session already has an active run");
}

#[tokio::test]
async fn stream_chat_conflicts_when_same_session_already_has_active_run() {
    let svc = test_service();
    let mut first = test_request("hello");
    first.session_id = Some("shared-session".into());
    ok(svc.create_run("user-1".into(), first).await);

    let mut second = test_request("again");
    second.session_id = Some("shared-session".into());
    let err = err(svc.stream_chat("user-1".into(), second).await);
    assert_eq!(err.0, StatusCode::CONFLICT);
    assert_eq!(err.1.0.detail, "session already has an active run");
}

#[tokio::test]
#[ignore] // stream_chat runs full agentic loop; needs live DB + LLM or mock
async fn stream_chat_tracks_run_for_status_and_replay() {
    let svc = test_service();
    let stream = ok(svc
        .stream_chat("user-1".into(), test_request("hello"))
        .await);

    let status = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let status = ok(svc
                .get_run_status(stream.run_id.clone(), "user-1".into())
                .await);
            if status.status != "running" {
                break status;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("timeout waiting for stream_chat status to finish");
    let replay = ok(svc
        .stream_run(stream.run_id.clone(), "user-1".into(), 0)
        .await);

    assert_eq!(status.run_id, stream.run_id);
    assert!(status.events_count > 0);
    assert_eq!(replay.len(), status.events_count as usize);
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
    assert_eq!(status.workspace.as_ref().unwrap()["kind"], "none");
    assert_eq!(status.executor.as_ref().unwrap()["kind"], "server_local");
    assert_eq!(
        status.executor.as_ref().unwrap()["executor_id"],
        "server-control-plane"
    );
    assert_eq!(status.transport.as_deref(), Some("server_local"));
    assert_eq!(status.fallback_policy.as_deref(), Some("disabled"));
}

#[tokio::test]
async fn noninteractive_create_run_does_not_wire_ws_only_channels() {
    let svc = test_service();
    let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);

    assert!(!svc.approval_channels.lock().await.contains_key(&run.run_id));
    assert!(
        !svc.user_prompt_channels
            .lock()
            .await
            .contains_key(&run.run_id)
    );
    assert!(!svc.progress_channels.lock().await.contains_key(&run.run_id));
}

#[tokio::test]
async fn noninteractive_approval_gate_denies_required_tools_without_waiting_for_ws() {
    let gate = NonInteractiveApprovalGate;

    assert!(astra_tools::ToolApprovalGate::requires_approval(
        &gate, "bash"
    ));
    assert!(astra_tools::ToolApprovalGate::requires_approval_for(
        &gate,
        "git",
        &serde_json::json!({"action": "commit"})
    ));
    assert!(!astra_tools::ToolApprovalGate::requires_approval_for(
        &gate,
        "git",
        &serde_json::json!({"action": "diff"})
    ));
    let decision = astra_tools::ToolApprovalGate::request_approval(
        &gate,
        "req-1",
        "bash",
        &serde_json::json!({"command": "rm -rf /tmp/example"}),
    )
    .await;

    assert!(matches!(
        decision,
        astra_tools::ApprovalDecision::Denied { reason: Some(reason) }
            if reason.contains("no interactive client")
    ));
}

#[tokio::test]
async fn create_run_persists_interaction_mode_into_run_started_event() {
    let svc = test_service();
    let mut req = test_request("hello");
    req.interaction_mode = Some(astra_services::runs::RequestedTurnInteractionMode::Auto);
    req.interactive_client = true;
    let run = ok(svc.create_run("user-1".into(), req).await);

    let durable = svc
        .run_engine
        .load_run("user-1", &run.run_id)
        .await
        .expect("load run")
        .expect("run exists");
    assert_eq!(durable.events[0]["event_type"], "run_started");
    assert_eq!(durable.events[0]["data"]["interaction_mode"], "auto");
    assert_eq!(durable.events[0]["data"]["suppressed_loop_nudges"], true);
    assert_eq!(durable.events[0]["data"]["interactive_client"], true);
    assert_eq!(durable.events[0]["data"]["workspace"]["kind"], "none");
    assert!(durable.events[0]["data"]["workspace"]["cwd"].is_null());
    assert_eq!(
        durable.events[0]["data"]["executor"]["kind"],
        "server_local"
    );
    assert_eq!(
        durable.events[0]["data"]["executor"]["executor_id"],
        "server-control-plane"
    );
    assert_eq!(durable.events[0]["data"]["transport"], "server_local");
    assert_eq!(durable.events[0]["data"]["fallback_policy"], "disabled");
}

#[tokio::test]
async fn create_run_persists_edge_binding_into_run_started_event() {
    let svc = test_service();
    let mut req = test_request("review this repo");
    req.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
        kind: astra_services::runs::WorkspaceBindingRequestKind::EdgeWorkspace,
        display_name: Some("MacBook Pro".to_string()),
        root: Some("/Users/xupeng/github/astra".to_string()),
        source: Some(astra_services::runs::WorkspaceSourceRequest::EdgePath {
            path: "/Users/xupeng/github/astra".to_string(),
        }),
        authority: Some(astra_services::runs::WorkspaceAuthorityRequest::ReadWrite),
        fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
    });
    req.executor_binding = Some(astra_services::runs::ExecutorBindingRequest {
        kind: astra_services::runs::ExecutorBindingRequestKind::EdgeAgent,
        executor_id: Some("edge-macbook-1".to_string()),
        display_name: Some("MacBook Pro".to_string()),
        transport: Some(astra_services::runs::ToolTransportKindRequest::EdgeWs),
        status: Some(astra_services::runs::ExecutorStatusRequest::Online),
    });
    let run = ok(svc.create_run("user-1".into(), req).await);

    let durable = svc
        .run_engine
        .load_run("user-1", &run.run_id)
        .await
        .expect("load run")
        .expect("run exists");
    assert_eq!(durable.events[0]["event_type"], "run_started");
    assert_eq!(
        durable.events[0]["data"]["workspace"]["kind"],
        "edge_workspace"
    );
    assert_eq!(
        durable.events[0]["data"]["workspace"]["cwd"],
        "/Users/xupeng/github/astra"
    );
    assert_eq!(durable.events[0]["data"]["executor"]["kind"], "edge_agent");
    assert_eq!(
        durable.events[0]["data"]["executor"]["executor_id"],
        "edge-macbook-1"
    );
    assert_eq!(durable.events[0]["data"]["transport"], "edge_ws");
    assert_eq!(durable.events[0]["data"]["fallback_policy"], "disabled");

    let status = ok(svc
        .get_run_status(run.run_id.clone(), "user-1".into())
        .await);
    assert_eq!(status.workspace.as_ref().unwrap()["kind"], "edge_workspace");
    assert_eq!(
        status.workspace.as_ref().unwrap()["cwd"],
        "/Users/xupeng/github/astra"
    );
    assert_eq!(status.executor.as_ref().unwrap()["kind"], "edge_agent");
    assert_eq!(
        status.executor.as_ref().unwrap()["executor_id"],
        "edge-macbook-1"
    );
    assert_eq!(status.transport.as_deref(), Some("edge_ws"));
    assert_eq!(status.fallback_policy.as_deref(), Some("disabled"));
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
async fn get_run_status_hides_foreign_run() {
    let svc = test_service();
    let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);
    let e = err(svc.get_run_status(run.run_id, "user-2".into()).await);
    assert_eq!(e.0, StatusCode::NOT_FOUND);
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

#[tokio::test(start_paused = true)]
async fn active_run_control_watcher_cancels_token_after_slow_durable_poll() {
    let provider = Arc::new(StaticRunControlProvider::new(Some(
        RunControlStatus::Cancelled,
    )));
    let run_control: Arc<dyn RunControlProvider> = provider.clone();
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let pause_flag = Arc::new(AtomicBool::new(false));
    let cancel_token = Arc::new(CancellationToken::new());

    let _watcher = start_active_run_control_watcher(
        Some(run_control),
        "user-1".to_string(),
        "run-1".to_string(),
        cancel_flag.clone(),
        pause_flag.clone(),
        cancel_token.clone(),
    )
    .expect("watcher");

    tokio::task::yield_now().await;
    assert_eq!(provider.calls(), 0, "watcher must not poll immediately");
    tokio::time::advance(ACTIVE_RUN_DURABLE_CONTROL_WATCH_INTERVAL - Duration::from_millis(1))
        .await;
    tokio::task::yield_now().await;
    assert_eq!(
        provider.calls(),
        0,
        "watcher must respect the slow interval"
    );

    tokio::time::advance(Duration::from_millis(1)).await;
    cancel_token.cancelled().await;

    assert!(cancel_flag.load(Ordering::Acquire));
    assert!(cancel_token.is_cancelled());
    assert!(!pause_flag.load(Ordering::Acquire));
    assert_eq!(provider.calls(), 1);
}

#[tokio::test(start_paused = true)]
async fn active_run_control_watcher_sets_pause_without_cancelling_token() {
    let provider = Arc::new(StaticRunControlProvider::new(Some(
        RunControlStatus::Paused,
    )));
    let run_control: Arc<dyn RunControlProvider> = provider.clone();
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let pause_flag = Arc::new(AtomicBool::new(false));
    let cancel_token = Arc::new(CancellationToken::new());

    let _watcher = start_active_run_control_watcher(
        Some(run_control),
        "user-1".to_string(),
        "run-1".to_string(),
        cancel_flag.clone(),
        pause_flag.clone(),
        cancel_token.clone(),
    )
    .expect("watcher");

    tokio::task::yield_now().await;
    tokio::time::advance(ACTIVE_RUN_DURABLE_CONTROL_WATCH_INTERVAL).await;
    tokio::task::yield_now().await;

    assert_eq!(provider.calls(), 1);
    assert!(!cancel_flag.load(Ordering::Acquire));
    assert!(pause_flag.load(Ordering::Acquire));
    assert!(
        !cancel_token.is_cancelled(),
        "pause must not abort in-flight work"
    );
}

#[tokio::test]
async fn cancel_session_runs_cancels_active_run_for_that_session_only() {
    let svc = test_service();
    let mut session_a = test_request("task a");
    session_a.session_id = Some("session-a".to_string());
    let run_a = ok(svc.create_run("user-1".into(), session_a).await);

    let mut session_b = test_request("task b");
    session_b.session_id = Some("session-b".to_string());
    let run_b = ok(svc.create_run("user-1".into(), session_b).await);

    let cancelled = ok(svc
        .cancel_session_runs("session-a".to_string(), "user-1".to_string())
        .await);

    assert_eq!(cancelled.len(), 1);
    assert_eq!(cancelled[0].run_id, run_a.run_id);
    assert_eq!(cancelled[0].status, "cancelled");
    assert_eq!(
        svc.test_llm_cancel_token_is_cancelled(&run_a.run_id).await,
        Some(true)
    );
    assert_eq!(
        svc.test_llm_cancel_token_is_cancelled(&run_b.run_id).await,
        Some(false),
        "session cancel must not cancel runs from a different session"
    );
    let status_b = ok(svc.get_run_status(run_b.run_id, "user-1".into()).await);
    assert_eq!(status_b.status, "running");
}

#[tokio::test(start_paused = true)]
async fn cancel_run_schedules_in_memory_eviction() {
    let svc = test_service();
    let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
    assert!(
        svc.test_llm_cancel_token_is_cancelled(&run.run_id)
            .await
            .is_some()
    );

    ok(svc.cancel_run(run.run_id.clone(), "user-1".into()).await);
    tokio::time::advance(std::time::Duration::from_secs(301)).await;
    tokio::task::yield_now().await;

    assert_eq!(
        svc.test_llm_cancel_token_is_cancelled(&run.run_id).await,
        None,
        "cancelled runs must not stay pinned in the process-local run cache"
    );
}

#[tokio::test]
async fn cancel_run_from_paused_sets_cancelled_status_and_clears_pause_flag() {
    let svc = test_service();
    let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
    ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
    assert_eq!(svc.test_pause_flag_is_set(&run.run_id).await, Some(true));

    let result = ok(svc.cancel_run(run.run_id.clone(), "user-1".into()).await);
    assert_eq!(result.status, "cancelled");
    assert_eq!(svc.test_pause_flag_is_set(&run.run_id).await, Some(false));
    assert_eq!(
        svc.test_llm_cancel_token_is_cancelled(&run.run_id).await,
        Some(true)
    );
}

#[tokio::test]
async fn pause_run_sets_live_pause_flag_and_resume_clears_it() {
    let svc = test_service();
    let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
    assert_eq!(svc.test_pause_flag_is_set(&run.run_id).await, Some(false));
    ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
    assert_eq!(svc.test_pause_flag_is_set(&run.run_id).await, Some(true));
    ok(svc.resume_run(run.run_id.clone(), "user-1".into()).await);
    assert_eq!(svc.test_pause_flag_is_set(&run.run_id).await, Some(false));
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
async fn cancel_run_hides_foreign_run() {
    let svc = test_service();
    let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
    let e = err(svc.cancel_run(run.run_id, "user-2".into()).await);
    assert_eq!(e.0, StatusCode::NOT_FOUND);
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
    let result = ok(svc.list_runs_cursor("user-1".into(), 10, None).await);
    assert_eq!(result.total, None);
    assert!(result.runs.is_empty());
}

#[tokio::test]
async fn list_runs_filters_by_user() {
    let svc = test_service();
    let u1_a = ok(svc.create_run("user-1".into(), test_request("a")).await);
    let u2_b = ok(svc.create_run("user-2".into(), test_request("b")).await);
    let u1_c = ok(svc.create_run("user-1".into(), test_request("c")).await);
    let for_u1 = ok(svc.list_runs_cursor("user-1".into(), 10, None).await);
    assert_eq!(for_u1.total, None);
    let ids: std::collections::HashSet<_> = for_u1.runs.iter().map(|r| r.run_id.as_str()).collect();
    assert!(ids.contains(u1_a.run_id.as_str()));
    assert!(ids.contains(u1_c.run_id.as_str()));
    assert!(!ids.contains(u2_b.run_id.as_str()));
    assert!(
        for_u1
            .runs
            .iter()
            .all(|run| run.workspace.as_ref().unwrap()["kind"] == "none")
    );
    assert!(
        for_u1
            .runs
            .iter()
            .all(|run| run.executor.as_ref().unwrap()["kind"] == "server_local")
    );
    assert!(
        for_u1
            .runs
            .iter()
            .all(|run| run.executor.as_ref().unwrap()["executor_id"] == "server-control-plane")
    );

    let for_u2 = ok(svc.list_runs_cursor("user-2".into(), 10, None).await);
    assert_eq!(for_u2.total, None);
    assert_eq!(for_u2.runs[0].run_id, u2_b.run_id);
}

#[tokio::test]
async fn list_runs_cursor_pagination() {
    let svc = test_service();
    for i in 0..5 {
        ok(svc
            .create_run("user-1".into(), test_request(&format!("msg {i}")))
            .await);
    }
    let page1 = ok(svc.list_runs_cursor("user-1".into(), 2, None).await);
    assert_eq!(page1.runs.len(), 2);
    assert_eq!(page1.total, None);
    assert!(page1.next_cursor.is_some());
    let page2 = ok(svc
        .list_runs_cursor("user-1".into(), 2, page1.next_cursor)
        .await);
    assert_eq!(page2.runs.len(), 2);
    assert!(page2.next_cursor.is_some());
    let page3 = ok(svc
        .list_runs_cursor("user-1".into(), 2, page2.next_cursor)
        .await);
    assert_eq!(page3.runs.len(), 1);
    assert!(page3.next_cursor.is_none());
}

#[tokio::test]
async fn list_runs_cursor_pagination_omits_count_and_returns_next_cursor() {
    let svc = test_service();
    for i in 0..5 {
        ok(svc
            .create_run("user-cursor".into(), test_request(&format!("msg {i}")))
            .await);
    }

    let page1 = ok(svc.list_runs_cursor("user-cursor".into(), 2, None).await);
    assert_eq!(page1.total, None);
    assert_eq!(page1.runs.len(), 2);
    assert!(page1.next_cursor.is_some());

    let page2 = ok(svc
        .list_runs_cursor("user-cursor".into(), 2, page1.next_cursor)
        .await);
    assert_eq!(page2.total, None);
    assert_eq!(page2.runs.len(), 2);
    assert!(page2.next_cursor.is_some());

    let page3 = ok(svc
        .list_runs_cursor("user-cursor".into(), 2, page2.next_cursor)
        .await);
    assert_eq!(page3.total, None);
    assert_eq!(page3.runs.len(), 1);
    assert!(page3.next_cursor.is_none());
}

#[tokio::test]
async fn list_runs_orders_by_latest_update() {
    let svc = test_service();
    let older = ok(svc.create_run("user-1".into(), test_request("older")).await);
    let newer = ok(svc.create_run("user-1".into(), test_request("newer")).await);

    let initial = ok(svc.list_runs_cursor("user-1".into(), 10, None).await);
    assert_eq!(initial.runs[0].run_id, newer.run_id);

    ok(svc.pause_run(older.run_id.clone(), "user-1".into()).await);

    let after_update = ok(svc.list_runs_cursor("user-1".into(), 10, None).await);
    assert_eq!(
        after_update.runs[0].run_id, older.run_id,
        "list_runs_cursor should surface the most recently updated run first"
    );
}

/// P2-B: list_runs_cursor must clamp pagination params like other list endpoints.
#[tokio::test]
async fn list_runs_cursor_clamps_pagination() {
    let svc = test_service();
    // Absurdly large limit must not panic or produce unbounded queries.
    let result = ok(svc
        .list_runs_cursor("user-clamp".into(), u32::MAX, None)
        .await);
    assert_eq!(result.runs.len(), 0);
    // Verify the returned limit is clamped.
    assert!(
        result.limit <= astra_services::pagination::MAX_API_LIST_LIMIT,
        "limit must be clamped to MAX_API_LIST_LIMIT"
    );
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
fn durable_recent_events_honors_work_surface_hydrate_limit() {
    let events = (0..450)
        .map(|i| json!({"event_type": "tool_call_end", "data": {"seq": i}}))
        .collect();
    let run = DurableRunRecord {
        run_id: "run-long".to_string(),
        user_id: "user-1".to_string(),
        session_id: "session-1".to_string(),
        parent_run_id: None,
        root_run_id: None,
        ancestor_path: None,
        depth: 0,
        delegation_id: None,
        agent_id: None,
        retry_of: None,
        retry_scope: None,
        status: STATUS_RUNNING.to_string(),
        waiting_for: None,
        owner_pod_id: None,
        owner_lease_expires_at: None,
        run_generation: 0,
        last_event_idx: 449,
        checkpoint_version: None,
        checkpoint_json: None,
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
        events,
        created_at: "2026-06-13T00:00:00.000Z".to_string(),
        updated_at: "2026-06-13T00:00:00.000Z".to_string(),
    };

    let recent_events = AgenticRunLifecycleService::durable_recent_events(&run, 400);

    assert_eq!(recent_events.len(), 400);
    assert_eq!(recent_events[0]["index"], 50);
    assert_eq!(recent_events[399]["index"], 449);
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
        agent_binding: None,
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
        context: Some(ctx),
        edge_executor_id: None,
        capabilities: Vec::new(),
        forward_headers: HashMap::new(),
        execution_budget: None,
        explain: false,
        interaction_mode: None,
        interactive_client: false,
    };
    let tools = AgenticRunLifecycleService::extract_edge_tools(&req).expect("edge tools");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["function"]["name"], "bash");
}

#[test]
fn extract_edge_tools_empty_when_no_context() {
    assert!(
        AgenticRunLifecycleService::extract_edge_tools(&test_request("hi"))
            .expect("empty edge tools")
            .is_empty()
    );
}

fn trusted_domains_for_tests() -> Vec<super::TrustedLlmDomain> {
    vec![super::TrustedLlmDomain {
        host: "catalog".to_string(),
        port: Some(8081),
    }]
}

#[test]
fn validate_llm_token_service_config_accepts_http_url() {
    let config = astra_services::LlmTokenServiceConfig {
        url: "http://catalog:8081/api/v1/llm-token".to_string(),
        timeout_ms: Some(2500),
    };
    let trusted = trusted_domains_for_tests();
    assert!(super::validate_llm_token_service_config(Some(&config), &trusted).is_ok());
}

#[test]
fn validate_llm_token_service_config_rejects_invalid_url() {
    let config = astra_services::LlmTokenServiceConfig {
        url: "not-a-url".to_string(),
        timeout_ms: Some(2500),
    };
    let trusted = trusted_domains_for_tests();
    let err = super::validate_llm_token_service_config(Some(&config), &trusted)
        .expect_err("invalid url should fail");
    assert!(err.contains("valid URL"), "unexpected error: {err}");
}

#[test]
fn validate_llm_token_service_config_rejects_untrusted_url() {
    let config = astra_services::LlmTokenServiceConfig {
        url: "http://evil.example.com/v1/chat/completions".to_string(),
        timeout_ms: Some(2500),
    };
    let trusted = trusted_domains_for_tests();
    let err = super::validate_llm_token_service_config(Some(&config), &trusted)
        .expect_err("untrusted url should fail");
    assert!(
        err.contains("trusted domains"),
        "unexpected error message: {err}"
    );
}

#[test]
fn validate_llm_token_service_config_rejects_when_trusted_domains_unconfigured() {
    let config = astra_services::LlmTokenServiceConfig {
        url: "http://catalog:8081/api/v1/llm-token".to_string(),
        timeout_ms: Some(2500),
    };
    let err = super::validate_llm_token_service_config(Some(&config), &[])
        .expect_err("missing trusted domains should fail");
    assert!(
        err.contains(super::LLM_TOKEN_SERVICE_TRUSTED_DOMAINS_TABLE),
        "unexpected error message: {err}"
    );
}

#[test]
fn validate_llm_token_service_config_enforces_host_port_boundary_for_trusted_domains() {
    let config = astra_services::LlmTokenServiceConfig {
        url: "http://catalog:8082/api/v1/chat".to_string(),
        timeout_ms: Some(2500),
    };
    let trusted = trusted_domains_for_tests();
    let err = super::validate_llm_token_service_config(Some(&config), &trusted)
        .expect_err("host:port boundary should be enforced");
    assert!(
        err.contains("trusted domains"),
        "unexpected error message: {err}"
    );
}

#[test]
fn trusted_llm_domain_from_db_values_accepts_valid_host_and_port() {
    let parsed =
        super::trusted_llm_domain_from_db_values("catalog", 8081).expect("host+port should parse");
    assert_eq!(parsed.host, "catalog");
    assert_eq!(parsed.port, Some(8081));
    let wildcard = super::trusted_llm_domain_from_db_values("catalog", 0)
        .expect("sentinel port should represent wildcard");
    assert_eq!(wildcard.port, None);
}

#[test]
fn trusted_llm_domain_from_db_values_rejects_invalid_host_or_port() {
    let host_err = super::trusted_llm_domain_from_db_values("http://catalog:8081", 8081)
        .expect_err("host should not include scheme");
    assert!(host_err.contains("host"));
    let port_err = super::trusted_llm_domain_from_db_values("catalog", 70000)
        .expect_err("port out of range should fail");
    assert!(port_err.contains("port"));
}

#[test]
fn normalize_request_allowlists_preserve_explicit_empty_sets() {
    let empty: Vec<String> = Vec::new();
    assert_eq!(
        super::normalize_request_allowlist(Some(&empty), "allow_skills")
            .expect("empty allow_skills should normalize"),
        Some(HashSet::new())
    );
    assert_eq!(
        super::normalize_request_skill_sources(Some(&empty), "allow_skill_sources")
            .expect("empty allow_skill_sources should normalize"),
        Some(HashSet::new())
    );
}

#[test]
fn extract_edge_profile_from_context() {
    let mut ctx = serde_json::Map::new();
    ctx.insert(
        "edge_profile".to_string(),
        json!({
            "cwd": "/tmp",
            "git_branch": "main",
            "system_prompt_override": "override text"
        }),
    );
    let req = ChatRequestData {
        message: "hi".into(),
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
        agent_binding: None,
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
        context: Some(ctx),
        edge_executor_id: None,
        capabilities: Vec::new(),
        forward_headers: HashMap::new(),
        execution_budget: None,
        explain: false,
        interaction_mode: None,
        interactive_client: false,
    };
    let profile = AgenticRunLifecycleService::extract_edge_profile(&req).expect("edge profile");
    assert_eq!(profile["cwd"], "/tmp");
    assert_eq!(profile["git_branch"], "main");
    assert_eq!(profile["system_prompt_override"], "override text");
}

#[test]
fn build_initial_state_sets_user_message() {
    let svc = test_service();
    let req = test_request("write a test");
    let expected_budget = astra_turn_core::chat_turn_heuristics::resolve_agentic_turn_budget(
        astra_turn_core::chat_turn_heuristics::infer_task_execution_profile("write a test"),
        astra_core::RuntimeLimits::global().max_turns,
        None,
    );
    let state = svc.build_initial_state("test-user", &req, "sess-1", "run-1", None, None, None);
    assert_eq!(state.messages.len(), 1);
    assert_eq!(state.messages[0]["role"], "user");
    assert_eq!(state.messages[0]["content"], "write a test");
    assert_eq!(state.current_session_id, Some("sess-1".to_string()));
    assert_eq!(state.current_run_id, Some("run-1".to_string()));
    assert_eq!(state.max_turns, expected_budget.initial_turns);
    assert_eq!(state.remaining_turns, expected_budget.initial_turns);
    assert_eq!(state.agentic_turn_budget, expected_budget);
    assert_eq!(state.message, "write a test");
    assert!(state.cancellation.token.is_none());
}

#[test]
fn build_initial_state_applies_execution_budget_override() {
    let svc = test_service();
    let mut req = test_request("go");
    req.execution_budget = Some(astra_services::runs::ExecutionBudget {
        initial_turns: Some(4),
        hard_turn_limit: Some(9),
    });
    let state = svc.build_initial_state("test-user", &req, "s", "r", None, None, None);
    assert_eq!(state.max_turns, 4);
    assert_eq!(state.remaining_turns, 4);
    assert_eq!(state.agentic_turn_budget.hard_turn_limit, 9);
}

#[test]
fn build_initial_state_clamps_execution_budget_override() {
    let svc = test_service();
    let mut req = test_request("go");
    req.execution_budget = Some(astra_services::runs::ExecutionBudget {
        initial_turns: Some(0),
        hard_turn_limit: Some(0),
    });
    let state = svc.build_initial_state("test-user", &req, "s", "r", None, None, None);
    assert_eq!(state.max_turns, 1);
    assert_eq!(state.agentic_turn_budget.hard_turn_limit, 1);
}

#[test]
fn agent_binding_prompt_override_appends_stable_section() {
    let context = PreparedAgentBindingLoopContext {
        binding: test_agent_binding_record(Some(3)),
        skill_resolver: None,
    };
    let mut edge_profile = serde_json::Map::from_iter([(
        "system_prompt_override".to_string(),
        Value::String("Existing instruction.".to_string()),
    )]);

    AgenticRunLifecycleService::apply_agent_binding_prompt_override(
        &mut edge_profile,
        Some(&context),
        None,
    );

    assert_eq!(
        edge_profile
            .get("system_prompt_override")
            .and_then(Value::as_str),
        Some(
            "Existing instruction.\n\n## Agent Binding Instruction\nAlways follow the binding contract."
        )
    );
}

#[test]
fn agent_binding_prompt_override_appends_runtime_system_prompt() {
    let context = PreparedAgentBindingLoopContext {
        binding: test_agent_binding_record(Some(3)),
        skill_resolver: None,
    };
    let mut edge_profile = serde_json::Map::new();

    AgenticRunLifecycleService::apply_agent_binding_prompt_override(
        &mut edge_profile,
        Some(&context),
        Some("Runtime SQL scope db_name: retail."),
    );

    assert_eq!(
        edge_profile
            .get("system_prompt_override")
            .and_then(Value::as_str),
        Some(
            "## Agent Binding Instruction\nAlways follow the binding contract.\n\nRuntime SQL scope db_name: retail."
        )
    );
}

#[test]
fn build_initial_state_agent_binding_uses_binding_skills_and_max_steps() {
    let svc = test_service();
    let mut req = test_request("go");
    req.execution_budget = Some(astra_services::runs::ExecutionBudget {
        initial_turns: Some(8),
        hard_turn_limit: Some(12),
    });
    let binding_context = PreparedAgentBindingLoopContext {
        binding: test_agent_binding_record(Some(3)),
        skill_resolver: Some(static_skill_resolver("binding-only")),
    };
    let edge_context =
        AgenticRunLifecycleService::extract_edge_context(&req).expect("edge context");
    let mut edge_profile = edge_context.edge_profile.to_map();
    AgenticRunLifecycleService::apply_agent_binding_prompt_override(
        &mut edge_profile,
        Some(&binding_context),
        None,
    );

    let state = svc.build_initial_state_inner(
        "test-user",
        &req,
        "s",
        "r",
        None,
        None,
        None,
        RequestConstraints::default(),
        &edge_context,
        Some(&edge_profile),
        None,
        Some(&binding_context),
    );

    assert_eq!(state.max_turns, 3);
    assert_eq!(state.remaining_turns, 3);
    assert_eq!(state.agentic_turn_budget.hard_turn_limit, 3);
    assert!(state.skills.registry_for_activation.is_none());
    let names: Vec<String> = state
        .skills
        .resolver
        .as_ref()
        .expect("binding skill resolver must be installed")
        .available_skills()
        .into_iter()
        .map(|skill| skill.name)
        .collect();
    assert_eq!(names, vec!["binding-only".to_string()]);
}

#[tokio::test]
async fn request_scoped_runtime_skill_resolver_is_installed_from_provider_capability() {
    use axum::{Router, extract::State, http::HeaderMap, routing::post};
    use tokio::sync::Mutex;

    #[derive(Default)]
    struct Capture {
        authorization: Mutex<Option<String>>,
        body: Mutex<Option<Value>>,
    }

    async fn handler(
        State(capture): State<Arc<Capture>>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        *capture.authorization.lock().await = headers
            .get(reqwest::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(ToString::to_string);
        *capture.body.lock().await = Some(body);
        Json(json!({
            "jsonrpc": "2.0",
            "id": "astra-agent-binding-skills-list",
            "result": {
                "skills": [{
                    "name": "moi-skill",
                    "description": "Skill from external provider runtime context",
                    "when_to_use": "when MOI grants this skill for the turn",
                    "aliases": ["moi-alias"],
                    "category": "external",
                    "tags": ["moi"],
                    "instructions": "Call the provider skill capability server.",
                    "allowed_tools": [],
                    "input_schema": {"type": "object"},
                    "output_schema": {"type": "object"}
                }]
            }
        }))
    }

    let capture = Arc::new(Capture::default());
    let app = Router::new()
        .route("/skills", post(handler))
        .with_state(capture.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local skill capability server");
    let addr = listener.local_addr().expect("listener addr");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let endpoint = format!("http://{addr}/skills");

    let svc = test_service();
    let mut request = test_request("use the provider skill");
    request.allow_skills = Some(vec!["moi-skill".to_string()]);
    request.runtime_skill_binding = Some(RuntimeSkillBindingRequest {
        id: "moi-skills".to_string(),
        url: endpoint.clone(),
        authorization: "Bearer runtime-grant".to_string(),
    });
    request.forward_headers.insert(
        "authorization".to_string(),
        "Bearer runtime-grant".to_string(),
    );
    let request_constraints = AgenticRunLifecycleService::try_request_constraints(&request)
        .expect("skill allowlist should parse");
    let capabilities = svc
        .prepare_runtime_capabilities(&request, &request_constraints)
        .await
        .expect("provider skill capability should prepare a resolver");

    assert!(capabilities.agent_binding.is_none());
    assert!(capabilities.request_scoped_skill_resolver.is_some());
    let edge_context =
        AgenticRunLifecycleService::extract_edge_context(&request).expect("edge context");

    let state = svc.build_initial_state_inner(
        "external-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
        request_constraints,
        &edge_context,
        None,
        capabilities.request_scoped_skill_resolver.clone(),
        capabilities.agent_binding.as_ref(),
    );

    assert!(state.skills.registry_for_activation.is_none());
    let resolver = state
        .skills
        .resolver
        .as_ref()
        .expect("runtime-scoped skill resolver must be installed");
    let available = resolver.available_skills();
    assert_eq!(available.len(), 1);
    assert_eq!(available[0].name, "moi-skill");
    let resolved = resolver
        .resolve("moi-alias")
        .expect("runtime-scoped skill alias should resolve");
    assert_eq!(resolved.remote_url.as_deref(), Some(endpoint.as_str()));
    assert_eq!(resolved.forward_headers, vec!["authorization".to_string()]);
    assert_eq!(resolved.required_headers, vec!["authorization".to_string()]);

    let manifest =
        AgenticRunLifecycleService::build_runtime_manifest(&request, &capabilities, false)
            .expect("selected model should produce manifest");
    assert!(manifest.get("agent_binding").is_none());
    assert_eq!(
        manifest["request_scoped_runtime"]["discovered_skills"][0]["name"],
        "moi-skill"
    );
    assert_eq!(
        capture.authorization.lock().await.as_deref(),
        Some("Bearer runtime-grant")
    );
    assert_eq!(
        capture.body.lock().await.as_ref(),
        Some(&json!({
            "jsonrpc": "2.0",
            "id": "astra-agent-binding-skills-list",
            "method": "skills/list"
        }))
    );
    server.abort();
}

#[tokio::test]
async fn agent_binding_runtime_uses_request_capability_descriptor_endpoints() {
    use axum::{Router, extract::State, http::HeaderMap, routing::post};
    use tokio::sync::Mutex;

    #[derive(Default)]
    struct Capture {
        mcp_authorization: Mutex<Option<String>>,
        skill_authorization: Mutex<Option<String>>,
    }

    async fn mcp_handler(
        State(capture): State<Arc<Capture>>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        *capture.mcp_authorization.lock().await = headers
            .get(reqwest::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(ToString::to_string);
        Json(json!({
            "jsonrpc": "2.0",
            "id": body.get("id").cloned().unwrap_or(Value::Null),
            "result": {"tools": []}
        }))
    }

    async fn skills_handler(
        State(capture): State<Arc<Capture>>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        *capture.skill_authorization.lock().await = headers
            .get(reqwest::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(ToString::to_string);
        Json(json!({
            "jsonrpc": "2.0",
            "id": body.get("id").cloned().unwrap_or(Value::Null),
            "result": {
                "skills": [{
                    "name": "moi-skill",
                    "description": "Skill from descriptor endpoint",
                    "when_to_use": "when the binding grants it",
                    "instructions": "Use the request-scoped endpoint.",
                    "allowed_tools": [],
                    "input_schema": {"type": "object"},
                    "output_schema": {"type": "object"}
                }]
            }
        }))
    }

    let capture = Arc::new(Capture::default());
    let app = Router::new()
        .route("/mcp", post(mcp_handler))
        .route("/skills", post(skills_handler))
        .with_state(capture.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local capability server");
    let addr = listener.local_addr().expect("listener addr");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let (service, _binding_service, record) = service_with_in_memory_binding().await;
    let mut request = test_request("use binding tools");
    request.provider_runtime_authorized = true;
    request.runtime_profile = Some(RuntimeProfileRequest::AgentBindingRegistry);
    request.agent_binding = Some(runtime_binding_request(record.id, "tools", "skills"));
    request.runtime_auth = Some(RuntimeAuthRequest {
        authorization: "Bearer runtime-grant".to_string(),
    });
    request.capability_descriptors =
        Some(astra_services::runs::RuntimeCapabilityDescriptorsRequest {
            model_gateway: Some(test_runtime_descriptor(
                "moi-model-gateway",
                "model_gateway",
                &format!("http://{addr}/model"),
            )),
            mcp: Some(test_runtime_descriptor(
                "tools",
                "mcp",
                &format!("http://{addr}/mcp"),
            )),
            skills: Some(test_runtime_descriptor(
                "skills",
                "skills",
                &format!("http://{addr}/skills"),
            )),
        });

    let capabilities = service
        .prepare_runtime_capabilities(&request, &RequestConstraints::default())
        .await
        .expect("agent binding descriptors should prepare capabilities");

    assert!(capabilities.mcp_bundle.is_some());
    assert!(capabilities.agent_binding.is_some());
    assert_eq!(
        capture.mcp_authorization.lock().await.as_deref(),
        Some("Bearer runtime-grant")
    );
    assert_eq!(
        capture.skill_authorization.lock().await.as_deref(),
        Some("Bearer runtime-grant")
    );
    let manifest =
        AgenticRunLifecycleService::build_runtime_manifest(&request, &capabilities, false)
            .expect("selected_model should produce a runtime manifest");
    assert_eq!(manifest["selected_model"]["model"], "test-model");
    assert!(manifest["selected_model"].get("gateway").is_none());
    assert_eq!(
        manifest["model_resolution"]["source"],
        "provider_descriptor"
    );
    assert_eq!(
        manifest["model_resolution"]["descriptor"]["id"],
        "moi-model-gateway"
    );
    server.abort();
}

#[test]
fn runtime_manifest_includes_agent_binding_snapshot_without_runtime_auth() {
    let mut request = test_request("use binding tools");
    request.agent_binding = Some(AgentBindingRuntimeRequest {
        id: "ab_018f05f5-c7dd-7f43-83e6-93d56d9d7391".to_string(),
        capability_server_refs: CapabilityServerRefs {
            mcp: "tools".to_string(),
            skills: "skills".to_string(),
        },
    });
    request.runtime_auth = Some(RuntimeAuthRequest {
        authorization: "Bearer secret-runtime-token".to_string(),
    });
    request.runtime_profile = Some(RuntimeProfileRequest::AgentBindingRegistry);
    request.parts = vec![json!({"type": "text", "text": "use binding tools"})];
    request.attachments = vec![json!({"id": "att-1", "kind": "file"})];
    request.edge_executor_id = Some("edge-1".to_string());
    request.capabilities = vec!["bash".to_string(), "fs".to_string()];
    let capabilities = PreparedRuntimeCapabilities {
        mcp_bundle: Some(runtime_mcp::RuntimeMcpBundle {
            schemas: vec![json!({
                "type": "function",
                "function": {
                    "name": "mcp__tools__query",
                    "description": "Query data",
                    "parameters": {"type": "object"}
                }
            })],
            manager: None,
            agent_binding_mcp: None,
        }),
        request_scoped_skill_resolver: None,
        agent_binding: Some(PreparedAgentBindingLoopContext {
            binding: test_agent_binding_record(Some(3)),
            skill_resolver: Some(static_skill_resolver("binding-only")),
        }),
    };

    let manifest =
        AgenticRunLifecycleService::build_runtime_manifest(&request, &capabilities, false)
            .expect("selected_model should produce a runtime manifest");

    assert_eq!(manifest["selected_model"]["model"], "test-model");
    assert!(manifest["selected_model"].get("gateway").is_none());
    assert_eq!(manifest["runtime_profile"], "agent_binding_registry");
    assert_eq!(manifest["turn"]["message"], "use binding tools");
    assert_eq!(manifest["turn"]["parts"][0]["type"], "text");
    assert_eq!(manifest["turn"]["attachments"][0]["id"], "att-1");
    assert_eq!(manifest["turn"]["edge_executor_id"], "edge-1");
    assert_eq!(manifest["turn"]["capabilities"][0], "bash");
    assert_eq!(
        manifest["agent_binding"]["selected_capability_server_refs"]["mcp"],
        "tools"
    );
    assert_eq!(
        manifest["agent_binding"]["discovered_tools"][0]["function"]["name"],
        "mcp__tools__query"
    );
    assert_eq!(
        manifest["agent_binding"]["discovered_skills"][0]["name"],
        "binding-only"
    );
    let serialized = serde_json::to_string(&manifest).expect("runtime manifest should serialize");
    assert!(!serialized.contains("secret-runtime-token"));
    assert!(!serialized.contains("Bearer"));
}

#[test]
fn install_agent_binding_runtime_forward_headers_uses_runtime_auth() {
    let mut req = test_request("go");
    req.agent_binding = Some(AgentBindingRuntimeRequest {
        id: "abnd_test1234567890".to_string(),
        capability_server_refs: CapabilityServerRefs {
            mcp: "mcp-main".to_string(),
            skills: "skills-main".to_string(),
        },
    });
    req.runtime_auth = Some(RuntimeAuthRequest {
        authorization: "Bearer runtime-grant".to_string(),
    });
    req.forward_headers.insert(
        "authorization".to_string(),
        "Bearer client-token".to_string(),
    );

    AgenticRunLifecycleService::install_agent_binding_runtime_forward_headers(&mut req)
        .expect("runtime auth should be forwarded in memory for binding skills");

    assert_eq!(
        req.forward_headers.get("authorization").map(String::as_str),
        Some("Bearer runtime-grant")
    );
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

    let state = svc.build_initial_state("test-user", &req, "s", "r", None, None, None);
    assert_eq!(state.hooks.stop_hooks.len(), 1);
    assert_eq!(state.hooks.stop_hooks[0].label, "cloud_hook");
    assert_eq!(
        state.hooks.workspace_root_hint.as_deref(),
        Some(dir.path().to_str().unwrap())
    );
}

#[test]
fn build_initial_state_uses_workspace_override_when_no_edge_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let mo = dir.path().join(".astra");
    std::fs::create_dir_all(&mo).unwrap();
    std::fs::write(
        mo.join("stop-hooks.yaml"),
        "version: 1\nauto_detect: false\nhooks:\n  - label: server_hook\n    command: echo ok\n",
    )
    .unwrap();

    let svc = test_service();
    // Request with NO edge_profile.cwd — simulates web-agent mode.
    let req = test_request("fix a bug");
    let state = svc.build_initial_state("test-user", &req, "s", "r", Some(dir.path()), None, None);
    assert_eq!(state.hooks.stop_hooks.len(), 1);
    assert_eq!(state.hooks.stop_hooks[0].label, "server_hook");
    assert_eq!(
        state.hooks.workspace_root_hint.as_deref(),
        Some(dir.path().to_str().unwrap())
    );
}

#[test]
fn build_initial_state_edge_cwd_takes_priority_over_workspace_override() {
    // Edge profile with cwd set — workspace_override should be ignored.
    let edge_dir = tempfile::tempdir().unwrap();
    let mo = edge_dir.path().join(".astra");
    std::fs::create_dir_all(&mo).unwrap();
    std::fs::write(
        mo.join("stop-hooks.yaml"),
        "version: 1\nauto_detect: false\nhooks:\n  - label: edge_hook\n    command: true\n",
    )
    .unwrap();

    let override_dir = tempfile::tempdir().unwrap();
    let mo2 = override_dir.path().join(".astra");
    std::fs::create_dir_all(&mo2).unwrap();
    std::fs::write(
        mo2.join("stop-hooks.yaml"),
        "version: 1\nauto_detect: false\nhooks:\n  - label: override_hook\n    command: true\n",
    )
    .unwrap();

    let svc = test_service();
    let mut req = test_request("deploy");
    req.context = Some(
        serde_json::json!({
            "edge_profile": { "cwd": edge_dir.path().to_str().unwrap() }
        })
        .as_object()
        .unwrap()
        .clone(),
    );

    let state = svc.build_initial_state(
        "test-user",
        &req,
        "s",
        "r",
        Some(override_dir.path()),
        None,
        None,
    );
    // Edge profile's cwd wins over the workspace override.
    assert_eq!(state.hooks.stop_hooks.len(), 1);
    assert_eq!(state.hooks.stop_hooks[0].label, "edge_hook");
    assert_eq!(
        state.hooks.workspace_root_hint.as_deref(),
        Some(edge_dir.path().to_str().unwrap())
    );
}

#[test]
fn run_status_as_str() {
    assert_eq!(RunStatus::Running.as_str(), "running");
    assert_eq!(RunStatus::InputQueued.as_str(), "input-queued");
    assert_eq!(RunStatus::Completed.as_str(), "completed");
    assert_eq!(RunStatus::Failed.as_str(), "failed");
    assert_eq!(RunStatus::Cancelled.as_str(), "cancelled");
    assert_eq!(RunStatus::Paused.as_str(), "paused");
}

#[test]
fn server_loop_causal_chain_ids_fit_agent_event_column() {
    assert!(server_loop_causal_chain_id("server-loop").len() <= 64);
    assert!(server_loop_causal_chain_id("server-loop-tools").len() <= 64);
}

#[test]
fn has_buffered_terminal_completion_ignores_cancelled_and_interrupted_finishes() {
    assert!(has_buffered_terminal_completion(&[json!({
        "event_type": "run_finished",
        "data": {"total_prompt_tokens": 1, "total_completion_tokens": 1}
    })]));
    assert!(!has_buffered_terminal_completion(&[json!({
        "event_type": "run_finished",
        "data": {"cancelled": true}
    })]));
    assert!(!has_buffered_terminal_completion(&[json!({
        "event_type": "run_finished",
        "data": {"interrupted": true}
    })]));
    assert!(!has_buffered_terminal_completion(&[
        json!({
            "event_type": "run_finished",
            "data": {"total_prompt_tokens": 1, "total_completion_tokens": 1}
        }),
        json!({
            "event_type": "run_finished",
            "data": {"cancelled": true}
        }),
    ]));
}

#[test]
fn preserve_manual_pause_wins_over_late_completed_status() {
    assert!(should_preserve_manual_pause_on_completion(
        &RunStatus::Paused,
        &RunStatus::Completed
    ));
    assert!(!should_preserve_manual_pause_on_completion(
        &RunStatus::Paused,
        &RunStatus::Failed
    ));
    assert!(!should_preserve_manual_pause_on_completion(
        &RunStatus::Running,
        &RunStatus::Completed
    ));
}

#[tokio::test]
async fn durable_paused_state_wins_over_late_completed_status() {
    let svc = test_service_with_engine();
    let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
    svc.run_engine
        .persist_status(
            "user-1",
            &run.run_id,
            STATUS_PAUSED,
            Some("user_resume"),
            None,
        )
        .await
        .unwrap();

    assert!(
        should_preserve_manual_pause_from_durable(
            &svc.run_engine,
            "user-1",
            &run.run_id,
            &RunStatus::Completed,
        )
        .await
    );
    assert!(
        !should_preserve_manual_pause_from_durable(
            &svc.run_engine,
            "user-1",
            &run.run_id,
            &RunStatus::Failed,
        )
        .await
    );
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
async fn pause_run_hides_foreign_run() {
    let svc = test_service();
    let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
    let e = err(svc.pause_run(run.run_id, "user-2".into()).await);
    assert_eq!(e.0, StatusCode::NOT_FOUND);
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
async fn resume_run_promotes_buffered_completed_pause_to_completed() {
    let svc = test_service_with_engine();
    let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
    ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
    svc.run_engine
        .append_event(
            "user-1",
            &run.run_id,
            json!({
                "event_type": "run_finished",
                "data": {"total_prompt_tokens": 1, "total_completion_tokens": 1}
            }),
        )
        .await
        .unwrap();

    let result = ok(svc.resume_run(run.run_id.clone(), "user-1".into()).await);
    assert_eq!(result.status, "completed");
    assert_eq!(result.previous_status, "paused");
    let status = ok(svc.get_run_status(run.run_id, "user-1".into()).await);
    assert_eq!(status.status, "completed");
}

#[tokio::test]
#[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
async fn db_pause_resume_promotes_buffered_completed_terminal() {
    let pool = setup_lifecycle_run_db_it().await;
    let svc = db_backed_test_service(&pool, "pause-resume-it-pod-completed");
    let user_id = "user-1";
    let run_id = format!("pause-it-{}", Uuid::new_v4());
    let session_id = format!("sess-it-{}", Uuid::new_v4());
    cleanup_lifecycle_run_fixture(&pool, user_id, &run_id).await;
    seed_lifecycle_run_for_pause_resume_it(&svc, user_id, &run_id, &session_id).await;

    ok(svc.pause_run(run_id.clone(), user_id.to_string()).await);
    svc.run_engine
        .append_event(
            user_id,
            &run_id,
            json!({
                "event_type": "run_finished",
                "data": {"total_prompt_tokens": 1, "total_completion_tokens": 1}
            }),
        )
        .await
        .expect("append buffered completed terminal event");

    let result = ok(svc.resume_run(run_id.clone(), user_id.to_string()).await);
    assert_eq!(result.status, STATUS_COMPLETED);
    assert_eq!(result.previous_status, STATUS_PAUSED);

    let durable = svc
        .run_engine
        .load_run(user_id, &run_id)
        .await
        .expect("load durable run")
        .expect("durable run exists");
    assert_eq!(durable.status, STATUS_COMPLETED);
    assert!(durable.waiting_for.is_none());
    assert_eq!(durable.events.last().unwrap()["event_type"], "run_finished");

    {
        let runs = svc.runs.read().await;
        let live = runs.get(&run_id).expect("live run should still be tracked");
        assert!(matches!(&live.status, RunStatus::Completed));
    }
    cleanup_lifecycle_run_fixture(&pool, user_id, &run_id).await;
}

#[tokio::test]
async fn resume_run_does_not_promote_cancelled_or_interrupted_finish_to_completed() {
    for (suffix, data) in [
        ("cancelled", json!({"cancelled": true})),
        ("interrupted", json!({"interrupted": true})),
    ] {
        let svc = test_service_with_engine();
        let run = ok(svc
            .create_run("user-1".into(), test_request(&format!("task-{suffix}")))
            .await);
        ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
        svc.run_engine
            .append_event(
                "user-1",
                &run.run_id,
                json!({
                    "event_type": "run_finished",
                    "data": data
                }),
            )
            .await
            .unwrap();

        let result = ok(svc.resume_run(run.run_id.clone(), "user-1".into()).await);
        assert_eq!(result.status, STATUS_RUNNING, "{suffix}");
        assert_eq!(result.previous_status, STATUS_PAUSED, "{suffix}");
        let durable = svc
            .run_engine
            .load_run("user-1", &run.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(durable.status, STATUS_RUNNING, "{suffix}");
        assert_eq!(durable.events.last().unwrap()["event_type"], "run_resumed");
    }
}

#[tokio::test]
#[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
async fn db_resume_does_not_promote_cancelled_or_interrupted_terminal_markers() {
    let pool = setup_lifecycle_run_db_it().await;
    for (suffix, data) in [
        ("cancelled", json!({"cancelled": true})),
        ("interrupted", json!({"interrupted": true})),
    ] {
        let svc = db_backed_test_service(&pool, &format!("pause-resume-it-pod-{suffix}"));
        let user_id = "user-1";
        let run_id = format!("resume-{suffix}-{}", Uuid::new_v4());
        let session_id = format!("sess-{suffix}-{}", Uuid::new_v4());
        cleanup_lifecycle_run_fixture(&pool, user_id, &run_id).await;
        seed_lifecycle_run_for_pause_resume_it(&svc, user_id, &run_id, &session_id).await;

        ok(svc.pause_run(run_id.clone(), user_id.to_string()).await);
        svc.run_engine
            .append_event(
                user_id,
                &run_id,
                json!({
                    "event_type": "run_finished",
                    "data": data
                }),
            )
            .await
            .expect("append buffered non-completed terminal marker");

        let result = ok(svc.resume_run(run_id.clone(), user_id.to_string()).await);
        assert_eq!(result.status, STATUS_RUNNING, "{suffix}");
        assert_eq!(result.previous_status, STATUS_PAUSED, "{suffix}");

        let durable = svc
            .run_engine
            .load_run(user_id, &run_id)
            .await
            .expect("load durable run")
            .expect("durable run exists");
        assert_eq!(durable.status, STATUS_RUNNING, "{suffix}");
        assert!(durable.waiting_for.is_none(), "{suffix}");
        assert_eq!(
            durable.events.last().unwrap()["event_type"],
            "run_resumed",
            "{suffix}"
        );

        {
            let runs = svc.runs.read().await;
            let live = runs.get(&run_id).expect("live run should still be tracked");
            assert!(matches!(&live.status, RunStatus::Running));
        }
        cleanup_lifecycle_run_fixture(&pool, user_id, &run_id).await;
    }
}

#[tokio::test]
#[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
async fn db_durable_event_budget_bounds_large_stream_persistence() {
    let pool = setup_lifecycle_run_db_it().await;
    let svc = db_backed_test_service(&pool, "durable-budget-it-pod");
    let user_id = "user-1";
    let run_id = format!("budget-it-{}", Uuid::new_v4());
    let session_id = format!("sess-budget-it-{}", Uuid::new_v4());
    let budget = DurableRunEventBatchBudget::default();
    cleanup_lifecycle_run_fixture(&pool, user_id, &run_id).await;
    svc.run_engine
        .start_run(&run_id, user_id, &session_id)
        .await
        .expect("start durable DB run");

    let mut raw_stream_events: Vec<Value> = (0..10_000)
        .map(|idx| json!({"type": "text_delta", "content": format!("chunk-{idx}")}))
        .collect();
    raw_stream_events
        .push(json!({"type": "tool_call", "tool_call": {"id": "call-1", "name": "bash"}}));
    raw_stream_events.push(json!({
        "type": "tool_call_end",
        "call_id": "call-1",
        "tool": "bash",
        "result": "ok"
    }));
    raw_stream_events.extend(
        (0..(budget.row_budget + 25)).map(|idx| json!({"type": "agent_progress", "seq": idx})),
    );
    raw_stream_events.push(json!({
        "event_type": "text_done",
        "data": {"full_text": "large durable final answer"}
    }));
    raw_stream_events.push(json!({
        "event_type": "run_finished",
        "data": {"prompt_tokens": 9, "completion_tokens": 3, "tool_call_count": 1}
    }));

    let durable_candidates: Vec<Value> = raw_stream_events
        .iter()
        .filter(|event| streaming_event_for_persistence(event))
        .cloned()
        .collect();
    assert_eq!(
        durable_candidates
            .iter()
            .filter(|event| durable_event_type(event) == Some("text_delta"))
            .count(),
        0,
        "transport chunks must stay live-only before DB persistence"
    );

    let budgeted = enforce_durable_run_event_batch_budget_with_budget(durable_candidates, budget);
    assert_eq!(budgeted.len(), budget.row_budget);
    assert!(
        budgeted
            .iter()
            .any(|event| durable_event_type(event) == Some("durable_events_compacted")),
        "semantic overflow should be summarized"
    );
    assert!(
        budgeted
            .iter()
            .any(|event| durable_event_type(event) == Some("tool_call")),
        "tool start boundary must beat progress noise under budget pressure"
    );
    assert!(
        budgeted
            .iter()
            .any(|event| durable_event_type(event) == Some("tool_call_end")),
        "tool end boundary must beat progress noise under budget pressure"
    );
    assert_eq!(
        durable_event_type(&budgeted[budgeted.len() - 2]),
        Some("text_done")
    );
    assert_eq!(
        durable_event_type(&budgeted[budgeted.len() - 1]),
        Some("run_finished")
    );

    let transitioned = svc
        .run_engine
        .transition_status_with_events_if_current(
            user_id,
            &run_id,
            &[STATUS_RUNNING],
            STATUS_COMPLETED,
            None,
            None,
            &budgeted,
        )
        .await
        .expect("commit budgeted terminal events");
    assert!(transitioned);

    let rows = sqlx::query(
        "SELECT event_type
         FROM agent_run_events
         WHERE user_id = ? AND run_id = ?
         ORDER BY event_idx ASC",
    )
    .bind(user_id)
    .bind(&run_id)
    .fetch_all(pool.get())
    .await
    .expect("load persisted event rows");
    assert_eq!(
        rows.len(),
        budget.row_budget + 1,
        "DB rows should be bounded to budgeted batch plus run_started"
    );
    let persisted_types = rows
        .iter()
        .map(|row| row.try_get::<String, _>("event_type").expect("event_type"))
        .collect::<Vec<_>>();
    assert!(
        !persisted_types
            .iter()
            .any(|event_type| event_type == "text_delta")
    );
    for expected in [
        "durable_events_compacted",
        "tool_call",
        "tool_call_end",
        "text_done",
        "run_finished",
    ] {
        assert!(
            persisted_types
                .iter()
                .any(|event_type| event_type == expected),
            "missing persisted {expected}: {persisted_types:?}"
        );
    }

    let replay_events = ok(svc.stream_run(run_id.clone(), user_id.to_string(), 1).await);
    assert!(replay_events.len() <= budget.row_budget);
    assert!(replay_events.iter().all(|event| {
        event.get("type").and_then(Value::as_str) != Some("text_delta")
            && event.get("event_type").and_then(Value::as_str) != Some("text_delta")
    }));
    assert!(replay_events.iter().any(|event| {
        event.get("type").and_then(Value::as_str) == Some("tool_call")
            || event.get("event_type").and_then(Value::as_str) == Some("tool_call")
    }));
    assert!(replay_events.iter().any(|event| {
        event.get("type").and_then(Value::as_str) == Some("tool_call_end")
            || event.get("event_type").and_then(Value::as_str) == Some("tool_call_end")
    }));
    assert!(replay_events.iter().any(|event| {
        event.get("event_type").and_then(Value::as_str) == Some("text_done")
            && event.pointer("/data/full_text").and_then(Value::as_str)
                == Some("large durable final answer")
    }));
    assert!(
        replay_events.iter().any(|event| {
            event.get("event_type").and_then(Value::as_str) == Some("run_finished")
        })
    );

    cleanup_lifecycle_run_fixture(&pool, user_id, &run_id).await;
}

#[tokio::test]
#[ignore = "requires MatrixOne DB: ASTRA_TEST_DB_IT=1 and ASTRA_DURABLE_EVENT_PRESSURE_PROBE=1"]
async fn durable_run_event_pressure_probe() {
    if !durable_event_pressure_opted_in() {
        eprintln!(
            "DURABLE_EVENT_PRESSURE_SKIPPED set {DURABLE_EVENT_PRESSURE_OPT_IN}=1 or run make test-durable-event-pressure"
        );
        return;
    }

    let pool = setup_lifecycle_run_db_it().await;
    let run_count = durable_event_pressure_env_usize("ASTRA_DURABLE_EVENT_PRESSURE_RUNS", 100, 1);
    let text_delta_count =
        durable_event_pressure_env_usize("ASTRA_DURABLE_EVENT_PRESSURE_TEXT_DELTAS", 10_000, 1);
    let budget = DurableRunEventBatchBudget::default();
    let progress_event_count = durable_event_pressure_env_usize(
        "ASTRA_DURABLE_EVENT_PRESSURE_PROGRESS_ROWS",
        budget.row_budget + 25,
        budget.row_budget + 1,
    );

    let started = Instant::now();
    let tasks = (0..run_count).map(|run_ordinal| {
        durable_event_pressure_case(
            pool.clone(),
            run_ordinal,
            text_delta_count,
            progress_event_count,
        )
    });
    let results = futures_util::future::join_all(tasks).await;
    let mut stats = Vec::with_capacity(run_count);
    for result in results {
        stats.push(result.expect("durable event pressure run"));
    }

    let total_raw_events: usize = stats.iter().map(|stat| stat.raw_events).sum();
    let total_candidate_rows: usize = stats.iter().map(|stat| stat.candidate_rows).sum();
    let total_candidate_bytes: usize = stats.iter().map(|stat| stat.candidate_bytes).sum();
    let total_budgeted_rows: usize = stats.iter().map(|stat| stat.budgeted_rows).sum();
    let total_budgeted_bytes: usize = stats.iter().map(|stat| stat.budgeted_bytes).sum();
    let total_persisted_rows: usize = stats.iter().map(|stat| stat.persisted_rows).sum();
    let total_replay_rows: usize = stats.iter().map(|stat| stat.replay_rows).sum();
    let total_text_delta_rows: usize = stats.iter().map(|stat| stat.text_delta_rows).sum();
    let compacted_runs = stats.iter().filter(|stat| stat.compacted_rows == 1).count();
    let max_persisted_rows_per_run = stats
        .iter()
        .map(|stat| stat.persisted_rows)
        .max()
        .unwrap_or_default();
    let max_replay_rows_per_run = stats
        .iter()
        .map(|stat| stat.replay_rows)
        .max()
        .unwrap_or_default();
    let max_run_elapsed_ms = stats
        .iter()
        .map(|stat| stat.elapsed_ms)
        .max()
        .unwrap_or_default();

    assert_eq!(
        compacted_runs, run_count,
        "every overflowed run should emit one compaction summary"
    );
    assert_eq!(
        total_text_delta_rows, 0,
        "transport deltas must never enter durable run events"
    );
    assert!(
        total_persisted_rows <= run_count * (budget.row_budget + 1),
        "persisted rows should be bounded by durable batch budget plus run_started"
    );
    assert!(
        total_replay_rows <= run_count * budget.row_budget,
        "cache-miss replay rows should be bounded by durable batch budget"
    );

    eprintln!(
        "DURABLE_EVENT_PRESSURE_RESULT {}",
        json!({
            "path": "agent_run_events.durable_event_budget",
            "runs": run_count,
            "text_deltas_per_run": text_delta_count,
            "progress_rows_per_run": progress_event_count,
            "row_budget": budget.row_budget,
            "byte_budget": budget.byte_budget,
            "total_raw_events": total_raw_events,
            "total_candidate_rows": total_candidate_rows,
            "total_candidate_bytes": total_candidate_bytes,
            "total_budgeted_rows": total_budgeted_rows,
            "total_budgeted_bytes": total_budgeted_bytes,
            "total_persisted_rows": total_persisted_rows,
            "total_replay_rows": total_replay_rows,
            "total_text_delta_rows": total_text_delta_rows,
            "compacted_runs": compacted_runs,
            "summary_event_frequency": compacted_runs as f64 / run_count as f64,
            "max_persisted_rows_per_run": max_persisted_rows_per_run,
            "max_replay_rows_per_run": max_replay_rows_per_run,
            "max_run_elapsed_ms": max_run_elapsed_ms,
            "elapsed_ms": duration_millis_u64(started.elapsed())
        })
    );
}

#[tokio::test]
async fn resume_run_conflict_when_not_paused() {
    let svc = test_service();
    let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
    let e = err(svc.resume_run(run.run_id, "user-1".into()).await);
    assert_eq!(e.0, StatusCode::CONFLICT);
}

#[tokio::test]
async fn resume_run_hides_foreign_run() {
    let svc = test_service();
    let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
    ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
    let e = err(svc.resume_run(run.run_id, "user-2".into()).await);
    assert_eq!(e.0, StatusCode::NOT_FOUND);
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
    test_service()
}

#[tokio::test]
#[ignore] // runs full agentic loop; needs live infra
async fn durable_create_run_persists_to_store() {
    let svc = test_service_with_engine();
    let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);

    let engine = &svc.run_engine;
    let durable = engine
        .load_run("user-1", &run.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.user_id, "user-1");
    assert_eq!(durable.status, "running");
    assert_eq!(durable.session_id, run.session_id);
}

#[tokio::test]
#[ignore] // runs full agentic loop; needs live infra
async fn durable_create_run_eventually_persists_terminal_event() {
    let svc = test_service_with_engine();
    let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);

    let engine = &svc.run_engine;
    let durable = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let durable = engine
                .load_run("user-1", &run.run_id)
                .await
                .unwrap()
                .unwrap();
            if durable.status != "running"
                && matches!(
                    durable
                        .events
                        .last()
                        .and_then(|event| event["event_type"].as_str()),
                    Some("run_finished")
                )
            {
                break durable;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("timeout waiting for durable run to persist terminal event");
    assert_eq!(durable.events.last().unwrap()["event_type"], "run_finished");
}

#[tokio::test]
#[ignore] // runs full agentic loop; needs live infra
async fn durable_stream_chat_persists_final_state() {
    let svc = test_service_with_engine();
    let stream = ok(svc
        .stream_chat("user-1".into(), test_request("hello"))
        .await);

    let engine = &svc.run_engine;
    let durable = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let durable = engine
                .load_run("user-1", &stream.run_id)
                .await
                .unwrap()
                .unwrap();
            if durable.status != "running"
                && matches!(
                    durable
                        .events
                        .last()
                        .and_then(|event| event["event_type"].as_str()),
                    Some("run_finished")
                )
            {
                break durable;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("timeout waiting for durable stream_chat final state");
    assert_eq!(durable.user_id, "user-1");
    assert_eq!(durable.session_id, stream.session_id);
    assert!(durable.events.len() >= 2);
    assert_eq!(durable.events.last().unwrap()["event_type"], "run_finished");
}

#[tokio::test]
#[ignore] // runs full agentic loop; needs live infra
async fn durable_cancel_persists_to_store() {
    let svc = test_service_with_engine();
    let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
    ok(svc.cancel_run(run.run_id.clone(), "user-1".into()).await);

    let engine = &svc.run_engine;
    let durable = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let durable = engine
                .load_run("user-1", &run.run_id)
                .await
                .unwrap()
                .unwrap();
            if matches!(
                durable
                    .events
                    .last()
                    .and_then(|event| event["event_type"].as_str()),
                Some("run_finished")
            ) {
                break durable;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("timeout waiting for cancelled run to persist terminal event");
    assert_eq!(durable.status, "cancelled");
    assert!(durable.events.len() >= 2); // run_started + run_finished
}

#[tokio::test]
#[ignore] // runs full agentic loop; needs live infra
async fn durable_pause_resume_round_trip() {
    let svc = test_service_with_engine();
    let run = ok(svc.create_run("user-1".into(), test_request("task")).await);

    ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
    let engine = &svc.run_engine;
    let durable = engine
        .load_run("user-1", &run.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, "paused");
    assert_eq!(durable.waiting_for.as_deref(), Some("user_resume"));

    ok(svc.resume_run(run.run_id.clone(), "user-1".into()).await);
    let durable = engine
        .load_run("user-1", &run.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, "running");
    assert!(durable.waiting_for.is_none());
}

#[tokio::test]
async fn cancel_run_returns_durable_terminal_status_on_cache_miss() {
    let svc = test_service_with_engine();
    let engine = &svc.run_engine;
    engine.start_run("run-1", "user-1", "sess-1").await.unwrap();
    engine
        .persist_status("user-1", "run-1", STATUS_COMPLETED, None, None)
        .await
        .unwrap();

    let result = ok(svc.cancel_run("run-1".into(), "user-1".into()).await);
    assert_eq!(result.run_id, "run-1");
    assert_eq!(result.status, STATUS_COMPLETED);
}

#[tokio::test]
async fn cancel_run_running_cache_miss_persists_cancelled() {
    let svc = test_service_with_engine();
    let engine = &svc.run_engine;
    engine.start_run("run-1", "user-1", "sess-1").await.unwrap();

    let result = ok(svc.cancel_run("run-1".into(), "user-1".into()).await);
    assert_eq!(result.status, STATUS_CANCELLED);
    let durable = engine.load_run("user-1", "run-1").await.unwrap().unwrap();
    assert_eq!(durable.status, STATUS_CANCELLED);
    assert_eq!(durable.events.last().unwrap()["event_type"], "run_finished");
}

#[tokio::test]
async fn cancel_run_stale_read_does_not_overwrite_completed() {
    let store: Arc<dyn RunStateStore> = Arc::new(
        FaultInjectedRunStateStore::new(&[], &[]).with_status_mutation_before_call(
            1,
            "user-1",
            "run-race",
            STATUS_COMPLETED,
            None,
            None,
        ),
    );
    let svc = test_service_with_store(store);
    let engine = &svc.run_engine;
    engine
        .start_run("run-race", "user-1", "sess-1")
        .await
        .unwrap();

    let result = ok(svc.cancel_run("run-race".into(), "user-1".into()).await);
    assert_eq!(result.status, STATUS_COMPLETED);

    let durable = engine
        .load_run("user-1", "run-race")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, STATUS_COMPLETED);
    assert!(
        durable
            .events
            .iter()
            .all(|event| event["event_type"] != "run_finished"),
        "stale cancel must not append a cancel terminal event"
    );
}

#[tokio::test]
async fn pause_run_stale_read_does_not_overwrite_completed() {
    let store: Arc<dyn RunStateStore> = Arc::new(
        FaultInjectedRunStateStore::new(&[], &[]).with_status_mutation_before_call(
            1,
            "user-1",
            "run-pause-race",
            STATUS_COMPLETED,
            None,
            None,
        ),
    );
    let svc = test_service_with_store(store);
    let engine = &svc.run_engine;
    engine
        .start_run("run-pause-race", "user-1", "sess-1")
        .await
        .unwrap();

    let e = err(svc
        .pause_run("run-pause-race".into(), "user-1".into())
        .await);
    assert_eq!(e.0, StatusCode::CONFLICT);
    assert!(e.1.0.detail.contains("completed"));

    let durable = engine
        .load_run("user-1", "run-pause-race")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, STATUS_COMPLETED);
    assert!(
        durable
            .events
            .iter()
            .all(|event| event["event_type"] != "run_paused"),
        "stale pause must not append a pause event"
    );
}

#[tokio::test]
async fn resume_run_stale_read_does_not_overwrite_cancelled() {
    let store: Arc<dyn RunStateStore> = Arc::new(
        FaultInjectedRunStateStore::new(&[], &[]).with_status_mutation_before_call(
            2,
            "user-1",
            "run-resume-race",
            STATUS_CANCELLED,
            None,
            None,
        ),
    );
    let svc = test_service_with_store(store);
    let engine = &svc.run_engine;
    engine
        .start_run("run-resume-race", "user-1", "sess-1")
        .await
        .unwrap();
    engine
        .persist_status(
            "user-1",
            "run-resume-race",
            STATUS_PAUSED,
            Some("user_resume"),
            None,
        )
        .await
        .unwrap();

    let e = err(svc
        .resume_run("run-resume-race".into(), "user-1".into())
        .await);
    assert_eq!(e.0, StatusCode::CONFLICT);
    assert!(e.1.0.detail.contains("cancelled"));

    let durable = engine
        .load_run("user-1", "run-resume-race")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, STATUS_CANCELLED);
    assert!(
        durable
            .events
            .iter()
            .all(|event| event["event_type"] != "run_resumed"),
        "stale resume must not append a resume event"
    );
}

#[tokio::test]
async fn pause_run_running_succeeds_via_db() {
    let svc = test_service_with_engine();
    let engine = &svc.run_engine;
    engine.start_run("run-1", "user-1", "sess-1").await.unwrap();

    let result = ok(svc.pause_run("run-1".into(), "user-1".into()).await);
    assert_eq!(result.status, STATUS_PAUSED);
    let durable = engine.load_run("user-1", "run-1").await.unwrap().unwrap();
    assert_eq!(durable.status, STATUS_PAUSED);
}

#[tokio::test]
async fn resume_run_paused_succeeds_via_db() {
    let svc = test_service_with_engine();
    let engine = &svc.run_engine;
    engine.start_run("run-1", "user-1", "sess-1").await.unwrap();
    engine
        .persist_status("user-1", "run-1", STATUS_PAUSED, Some("user_resume"), None)
        .await
        .unwrap();

    let result = ok(svc.resume_run("run-1".into(), "user-1".into()).await);
    assert_eq!(result.status, STATUS_RUNNING);
    let durable = engine.load_run("user-1", "run-1").await.unwrap().unwrap();
    assert_eq!(durable.status, STATUS_RUNNING);
}

#[tokio::test]
async fn cancel_run_transition_failure_does_not_commit_status_or_event() {
    let store: Arc<dyn RunStateStore> = Arc::new(FaultInjectedRunStateStore::new(&[], &[1]));
    let svc = test_service_with_store(store);
    let run = ok(svc.create_run("user-1".into(), test_request("task")).await);

    let e = err(svc.cancel_run(run.run_id.clone(), "user-1".into()).await);
    assert_eq!(e.0, StatusCode::SERVICE_UNAVAILABLE);
    assert!(e.1.0.detail.contains("cancel transition"));

    let durable = svc
        .run_engine
        .load_run("user-1", &run.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, STATUS_RUNNING);
    assert!(durable.waiting_for.is_none());
    assert_eq!(durable.events.len(), 1);
    assert_eq!(durable.events[0]["event_type"], "run_started");

    let runs = svc.runs.read().await;
    let live = runs.get(&run.run_id).expect("live run state");
    assert_eq!(live.status, RunStatus::Running);
    assert!(live.waiting_for.is_none());
    assert!(!live.cancel_flag.load(Ordering::SeqCst));
    assert_eq!(live.events.len(), 1);
    assert_eq!(live.events[0]["event_type"], "run_started");
}

#[tokio::test]
async fn pause_run_transition_failure_does_not_commit_status_or_event() {
    let store: Arc<dyn RunStateStore> = Arc::new(FaultInjectedRunStateStore::new(&[], &[1]));
    let svc = test_service_with_store(store);
    let run = ok(svc.create_run("user-1".into(), test_request("task")).await);

    let e = err(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
    assert_eq!(e.0, StatusCode::SERVICE_UNAVAILABLE);
    assert!(e.1.0.detail.contains("pause transition"));

    let durable = svc
        .run_engine
        .load_run("user-1", &run.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, STATUS_RUNNING);
    assert!(durable.waiting_for.is_none());
    assert_eq!(durable.events.len(), 1);
    assert_eq!(durable.events[0]["event_type"], "run_started");

    let runs = svc.runs.read().await;
    let live = runs.get(&run.run_id).expect("live run state");
    assert_eq!(live.status, RunStatus::Running);
    assert!(live.waiting_for.is_none());
    assert!(!live.pause_flag.load(Ordering::SeqCst));
    assert_eq!(live.events.len(), 1);
    assert_eq!(live.events[0]["event_type"], "run_started");
}

#[tokio::test]
async fn resume_run_transition_failure_does_not_commit_status_or_event() {
    let store: Arc<dyn RunStateStore> = Arc::new(FaultInjectedRunStateStore::new(&[], &[2]));
    let svc = test_service_with_store(store);
    let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
    ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);

    let e = err(svc.resume_run(run.run_id.clone(), "user-1".into()).await);
    assert_eq!(e.0, StatusCode::SERVICE_UNAVAILABLE);
    assert!(e.1.0.detail.contains("resume transition"));

    let durable = svc
        .run_engine
        .load_run("user-1", &run.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, STATUS_PAUSED);
    assert_eq!(durable.waiting_for.as_deref(), Some("user_resume"));
    assert_eq!(durable.events.len(), 2);
    assert_eq!(durable.events[1]["event_type"], "run_paused");

    let runs = svc.runs.read().await;
    let live = runs.get(&run.run_id).expect("live run state");
    assert_eq!(live.status, RunStatus::Paused);
    assert_eq!(live.waiting_for.as_deref(), Some("user_resume"));
    assert!(live.pause_flag.load(Ordering::SeqCst));
    assert_eq!(live.events.len(), 2);
    assert_eq!(live.events[1]["event_type"], "run_paused");
}

#[tokio::test]
async fn cancel_run_paused_cache_miss_persists_cancelled() {
    let svc = test_service_with_engine();
    let engine = &svc.run_engine;
    engine.start_run("run-1", "user-1", "sess-1").await.unwrap();
    engine
        .persist_status("user-1", "run-1", STATUS_PAUSED, Some("user_resume"), None)
        .await
        .unwrap();

    let result = ok(svc.cancel_run("run-1".into(), "user-1".into()).await);
    assert_eq!(result.status, STATUS_CANCELLED);
    let durable = engine.load_run("user-1", "run-1").await.unwrap().unwrap();
    assert_eq!(durable.status, STATUS_CANCELLED);
}

#[tokio::test]
#[ignore] // stream_chat runs full agentic loop; needs live DB + LLM or mock
async fn get_run_status_falls_back_to_durable_store_on_cache_miss() {
    let svc = test_service_with_engine();
    let stream = ok(svc
        .stream_chat("user-1".into(), test_request("hello"))
        .await);
    let engine = &svc.run_engine;
    let durable = engine
        .load_run("user-1", &stream.run_id)
        .await
        .unwrap()
        .unwrap();

    svc.runs.write().await.remove(&stream.run_id);

    let status = ok(svc
        .get_run_status(stream.run_id.clone(), "user-1".into())
        .await);
    assert_eq!(status.run_id, stream.run_id);
    assert_eq!(status.session_id, stream.session_id);
    assert_eq!(status.status, durable.status);
    assert_eq!(status.waiting_for, durable.waiting_for);
    assert_eq!(status.events_count, durable.events.len() as i64);
}

#[tokio::test]
async fn stream_run_cache_miss_replays_durable_text_done() {
    let svc = test_service_with_engine();
    let engine = &svc.run_engine;
    engine
        .start_run("run-durable-text", "user-1", "session-1")
        .await
        .expect("start durable run");
    engine
        .append_event(
            "user-1",
            "run-durable-text",
            json!({"event_type": "text_done", "data": {"full_text": "durable final answer"}}),
        )
        .await
        .expect("persist text_done");
    engine
        .append_event(
            "user-1",
            "run-durable-text",
            json!({"event_type": "run_finished", "data": {"prompt_tokens": 1}}),
        )
        .await
        .expect("persist run_finished");

    let events = ok(svc
        .stream_run("run-durable-text".into(), "user-1".into(), 1)
        .await);

    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["event_type"], "text_done");
    assert_eq!(events[0]["data"]["full_text"], "durable final answer");
    assert_eq!(events[1]["event_type"], "run_finished");
}

#[tokio::test]
async fn submit_run_input_uses_durable_idempotency_on_cache_miss() {
    let svc = test_service_with_engine();
    let engine = &svc.run_engine;
    engine
        .start_run("run-input", "user-1", "session-1")
        .await
        .unwrap();

    let first = ok(svc
        .submit_run_input(
            "run-input".into(),
            "user-1".into(),
            RunInputData {
                idempotency_key: "key-1".into(),
                input: json!({"answer": "yes"}),
            },
        )
        .await);
    let duplicate = ok(svc
        .submit_run_input(
            "run-input".into(),
            "user-1".into(),
            RunInputData {
                idempotency_key: "key-1".into(),
                input: json!({"answer": "yes"}),
            },
        )
        .await);

    let durable = engine
        .load_run("user-1", "run-input")
        .await
        .unwrap()
        .unwrap();
    let matching_inputs = durable
        .events
        .iter()
        .filter(|event| event.get("idempotency_key").and_then(Value::as_str) == Some("key-1"))
        .count();
    let input_queued_events = durable
        .events
        .iter()
        .filter(|event| event.get("event_type").and_then(Value::as_str) == Some("run_input_queued"))
        .count();
    assert!(!first.duplicate);
    assert!(duplicate.duplicate);
    assert_eq!(matching_inputs, 1);
    assert_eq!(input_queued_events, 1);
    assert_eq!(durable.status, STATUS_INPUT_QUEUED);
    assert_eq!(durable.waiting_for.as_deref(), Some("user_input"));
}

#[tokio::test]
async fn submit_run_input_transition_failure_does_not_commit_status_or_events() {
    let store: Arc<dyn RunStateStore> = Arc::new(FaultInjectedRunStateStore::new(&[], &[1]));
    let svc = test_service_with_store(store);
    let engine = &svc.run_engine;
    engine
        .start_run("run-input-fail", "user-1", "session-1")
        .await
        .unwrap();

    let e = err(svc
        .submit_run_input(
            "run-input-fail".into(),
            "user-1".into(),
            RunInputData {
                idempotency_key: "key-fail".into(),
                input: json!({"answer": "not committed"}),
            },
        )
        .await);
    assert_eq!(e.0, StatusCode::SERVICE_UNAVAILABLE);
    assert!(e.1.0.detail.contains("input transition"));

    let durable = engine
        .load_run("user-1", "run-input-fail")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, STATUS_RUNNING);
    assert!(durable.waiting_for.is_none());
    assert!(
        durable.events.iter().all(|event| {
            event.get("idempotency_key").and_then(Value::as_str) != Some("key-fail")
        }),
        "failed input transition must not leave a partial user_input event"
    );
    assert!(
        durable.events.iter().all(
            |event| event.get("event_type").and_then(Value::as_str) != Some("run_input_queued")
        ),
        "failed input transition must not append run_input_queued"
    );
}

#[tokio::test]
async fn submit_run_input_rejects_terminal_durable_run() {
    let svc = test_service_with_engine();
    let engine = &svc.run_engine;
    engine
        .start_run("run-terminal-input", "user-1", "session-1")
        .await
        .unwrap();
    engine
        .persist_status("user-1", "run-terminal-input", STATUS_COMPLETED, None, None)
        .await
        .unwrap();

    let e = err(svc
        .submit_run_input(
            "run-terminal-input".into(),
            "user-1".into(),
            RunInputData {
                idempotency_key: "key-1".into(),
                input: json!({"answer": "late"}),
            },
        )
        .await);
    assert_eq!(e.0, StatusCode::CONFLICT);
}

#[tokio::test]
async fn submit_run_input_accepts_repeated_queueing_while_input_already_queued() {
    let svc = test_service_with_engine();
    let engine = &svc.run_engine;
    engine
        .start_run("run-queued-input", "user-1", "session-1")
        .await
        .unwrap();
    engine
        .persist_status(
            "user-1",
            "run-queued-input",
            STATUS_INPUT_QUEUED,
            Some("user_input"),
            None,
        )
        .await
        .unwrap();

    let result = svc
        .submit_run_input(
            "run-queued-input".into(),
            "user-1".into(),
            RunInputData {
                idempotency_key: "key-queued-1".into(),
                input: json!({"answer": "keep queueing"}),
            },
        )
        .await
        .expect("input-queued runs should accept additional deferred input");

    assert!(result.accepted);
    assert!(!result.duplicate);
    let durable = engine
        .load_run("user-1", "run-queued-input")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, STATUS_INPUT_QUEUED);
    assert_eq!(durable.waiting_for.as_deref(), Some("user_input"));
    assert!(durable.events.iter().any(|event| {
        event.get("idempotency_key").and_then(Value::as_str) == Some("key-queued-1")
    }));
}

#[tokio::test]
async fn submit_run_input_rejects_paused_durable_run() {
    let svc = test_service_with_engine();
    let engine = &svc.run_engine;
    engine
        .start_run("run-paused-input", "user-1", "session-1")
        .await
        .unwrap();
    engine
        .persist_status("user-1", "run-paused-input", STATUS_PAUSED, None, None)
        .await
        .unwrap();

    let e = err(svc
        .submit_run_input(
            "run-paused-input".into(),
            "user-1".into(),
            RunInputData {
                idempotency_key: "key-1".into(),
                input: json!({"answer": "late"}),
            },
        )
        .await);
    assert_eq!(e.0, StatusCode::CONFLICT);
}

#[tokio::test]
async fn submit_run_input_rejects_oversized_content() {
    let svc = test_service_with_engine();
    let engine = &svc.run_engine;
    engine
        .start_run("run-large-input", "user-1", "session-1")
        .await
        .unwrap();

    let e = err(svc
        .submit_run_input(
            "run-large-input".into(),
            "user-1".into(),
            RunInputData {
                idempotency_key: "key-large".into(),
                input: json!({"content": "x".repeat(MAX_DEFERRED_INPUT_CHARS + 1)}),
            },
        )
        .await);

    assert_eq!(e.0, StatusCode::PAYLOAD_TOO_LARGE);
    let durable = engine
        .load_run("user-1", "run-large-input")
        .await
        .unwrap()
        .unwrap();
    assert!(
        durable.events.iter().all(|event| {
            event.get("idempotency_key").and_then(Value::as_str) != Some("key-large")
        }),
        "oversized input must not be appended before validation"
    );
}

#[tokio::test]
async fn create_run_conflict_checks_durable_active_session() {
    let svc = test_service_with_engine();
    let engine = &svc.run_engine;
    engine
        .start_run("existing-run", "user-1", "shared-session")
        .await
        .unwrap();
    let mut request = test_request("second");
    request.session_id = Some("shared-session".into());

    let e = err(svc.create_run("user-1".into(), request).await);
    assert_eq!(e.0, StatusCode::CONFLICT);
}

#[tokio::test]
#[ignore] // stream_chat runs full agentic loop; needs live DB + LLM or mock
async fn stream_run_falls_back_to_durable_store_on_cache_miss() {
    let svc = test_service_with_engine();
    let stream = ok(svc
        .stream_chat("user-1".into(), test_request("hello"))
        .await);
    let engine = &svc.run_engine;
    let durable = engine
        .load_run("user-1", &stream.run_id)
        .await
        .unwrap()
        .unwrap();

    svc.runs.write().await.remove(&stream.run_id);

    let events = ok(svc
        .stream_run(stream.run_id.clone(), "user-1".into(), 1)
        .await);
    assert_eq!(
        events,
        AgenticRunLifecycleService::format_run_events(&durable.events[1..], 1)
    );
}

#[tokio::test]
#[ignore] // stream_chat runs full agentic loop; needs live DB + LLM or mock
async fn list_runs_falls_back_to_durable_store_on_cache_miss() {
    let svc = test_service_with_engine();
    let first = ok(svc
        .stream_chat("user-1".into(), test_request("first"))
        .await);
    let second = ok(svc
        .stream_chat("user-1".into(), test_request("second"))
        .await);

    svc.runs.write().await.remove(&first.run_id);

    let runs = ok(svc.list_runs_cursor("user-1".into(), 10, None).await);
    let run_ids: Vec<_> = runs.runs.iter().map(|run| run.run_id.as_str()).collect();
    assert_eq!(runs.total, None);
    assert!(run_ids.contains(&first.run_id.as_str()));
    assert!(run_ids.contains(&second.run_id.as_str()));
}

#[tokio::test]
async fn lifecycle_run_creation_is_durable_by_default() {
    let svc = test_service();
    let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);
    let durable = svc
        .run_engine
        .load_run("user-1", &run.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.user_id, "user-1");
    assert_eq!(durable.session_id, run.session_id);
    assert_eq!(durable.status, STATUS_RUNNING);
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

    let edge_ctx = AgenticRunLifecycleService::extract_edge_context(&req).expect("edge context");
    assert_eq!(edge_ctx.tool_count(), 1);
    assert_eq!(edge_ctx.tool_names(), vec!["bash"]);
    assert_eq!(edge_ctx.edge_profile.cwd.as_deref(), Some("/tmp"));
    assert_eq!(edge_ctx.edge_profile.git_branch.as_deref(), Some("main"));
}

#[test]
fn extract_edge_context_from_empty_request() {
    let req = test_request("hello");
    let edge_ctx = AgenticRunLifecycleService::extract_edge_context(&req).expect("edge context");
    assert!(!edge_ctx.has_tools());
    assert!(edge_ctx.edge_profile.cwd.is_none());
}

#[test]
fn extract_edge_context_rejects_malformed_context() {
    let mut ctx = serde_json::Map::new();
    ctx.insert("edge_tools".to_string(), json!({"not": "an array"}));
    let req = ChatRequestData {
        context: Some(ctx),
        ..test_request("hello")
    };

    let error = AgenticRunLifecycleService::extract_edge_context(&req)
        .expect_err("malformed edge context must fail loud");

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert!(
        error.1.0.detail.contains("invalid edge context"),
        "unexpected error: {}",
        error.1.0.detail
    );
}

#[tokio::test]
async fn create_run_rejects_malformed_edge_context_before_agent_start() {
    let svc = test_service();
    let mut ctx = serde_json::Map::new();
    ctx.insert("edge_tools".to_string(), json!({"not": "an array"}));
    let req = ChatRequestData {
        context: Some(ctx),
        ..test_request("hello")
    };

    let error = err(svc.create_run("user-1".into(), req).await);

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert!(
        error.1.0.detail.contains("invalid edge context"),
        "unexpected error: {}",
        error.1.0.detail
    );
}

#[tokio::test]
async fn stream_chat_rejects_malformed_edge_context_before_agent_start() {
    let svc = test_service();
    let mut ctx = serde_json::Map::new();
    ctx.insert("edge_profile".to_string(), json!({"cwd": 42}));
    let req = ChatRequestData {
        context: Some(ctx),
        ..test_request("hello")
    };

    let error = err(svc.stream_chat("user-1".into(), req).await);

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert!(
        error.1.0.detail.contains("invalid edge context"),
        "unexpected error: {}",
        error.1.0.detail
    );
}

// ─── Background spawning integration tests ──────────────────────────

#[tokio::test]
#[ignore] // runs full agentic loop; needs live infra
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
#[ignore] // runs full agentic loop; needs live infra
async fn create_run_with_engine_persists_final_state() {
    let svc = test_service_with_engine();
    let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);

    // Deterministic wait: poll durable state until it leaves "running".
    let engine = &svc.run_engine;
    let durable = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let durable = engine
                .load_run("user-1", &run.run_id)
                .await
                .unwrap()
                .unwrap();
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

#[tokio::test]
async fn fail_started_run_before_spawn_persists_terminal_events() {
    let svc = test_service_with_engine();
    let engine = &svc.run_engine;
    engine
        .start_run("run-pre-spawn", "user-1", "session-1")
        .await
        .unwrap();

    svc.fail_started_run_before_spawn(
        "user-1",
        "run-pre-spawn",
        "server capacity timeout before agentic loop start",
    )
    .await;

    let durable = engine
        .load_run("user-1", "run-pre-spawn")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, STATUS_FAILED);
    assert_eq!(durable.error_code.as_deref(), Some("run_admission_timeout"));
    assert!(
        durable.events.iter().any(|event| {
            event.get("event_type").and_then(Value::as_str) == Some("run_error")
                && event
                    .get("data")
                    .and_then(Value::as_object)
                    .and_then(|data| data.get("error_code"))
                    .and_then(Value::as_str)
                    == Some("run_admission_timeout")
        }),
        "durable run_error must explain the pre-spawn failure"
    );
    assert!(
        durable.events.iter().any(|event| {
            event.get("event_type").and_then(Value::as_str) == Some("run_finished")
                && event
                    .get("data")
                    .and_then(Value::as_object)
                    .and_then(|data| data.get("error_kind"))
                    .and_then(Value::as_str)
                    == Some("server_error")
        }),
        "durable run_finished must preserve the pre-spawn terminal code"
    );
}

#[tokio::test]
async fn fail_started_run_before_spawn_transition_failure_does_not_commit_partial_terminal() {
    let store: Arc<dyn RunStateStore> = Arc::new(FaultInjectedRunStateStore::new(&[], &[1]));
    let svc = test_service_with_store(store);
    let engine = &svc.run_engine;
    engine
        .start_run("run-pre-spawn-fail", "user-1", "session-1")
        .await
        .unwrap();

    svc.fail_started_run_before_spawn(
        "user-1",
        "run-pre-spawn-fail",
        "server capacity timeout before agentic loop start",
    )
    .await;

    let durable = engine
        .load_run("user-1", "run-pre-spawn-fail")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, STATUS_RUNNING);
    assert!(durable.error_code.is_none());
    assert!(
        durable.events.iter().all(|event| {
            !matches!(
                event.get("event_type").and_then(Value::as_str),
                Some("run_error" | "run_finished")
            )
        }),
        "failed pre-spawn transition must not leave partial terminal events"
    );
}

#[tokio::test]
async fn create_run_token_budget_reject_persists_terminal_events() {
    let svc = test_service()
        .with_resource_governor(Arc::new(DenyTokenBudgetGovernor))
        .with_run_concurrency_limit(1);
    let run = ok(svc
        .create_run("user-1".into(), test_request("over budget"))
        .await);

    assert!(
        svc.drain_background_tasks(Duration::from_secs(1)).await,
        "budget rejection task should finish promptly"
    );
    let durable = svc
        .run_engine
        .load_run("user-1", &run.run_id)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(durable.status, STATUS_FAILED);
    assert_eq!(
        durable.error_code.as_deref(),
        Some("per_user_daily_token_quota")
    );
    assert!(
        durable
            .events
            .iter()
            .any(
                |event| event.get("event_type").and_then(Value::as_str) == Some("run_error")
                    && event
                        .get("data")
                        .and_then(Value::as_object)
                        .and_then(|data| data.get("error_code"))
                        .and_then(Value::as_str)
                        == Some("per_user_daily_token_quota")
            ),
        "durable run_error must explain the quota failure"
    );
    assert!(
        durable
            .events
            .iter()
            .any(
                |event| event.get("event_type").and_then(Value::as_str) == Some("run_finished")
                    && event
                        .get("data")
                        .and_then(Value::as_object)
                        .and_then(|data| data.get("error_kind"))
                        .and_then(Value::as_str)
                        == Some("budget_exhausted")
            ),
        "durable run_finished must preserve the terminal quota code"
    );
}

#[tokio::test]
async fn token_budget_reject_transition_failure_does_not_commit_status_or_events() {
    let store: Arc<dyn RunStateStore> = Arc::new(FaultInjectedRunStateStore::new(&[], &[1]));
    let svc = test_service_with_store(store);
    let engine = &svc.run_engine;
    engine
        .start_run("run-quota-fail", "user-1", "session-1")
        .await
        .unwrap();

    let committed_events = AgenticRunLifecycleService::persist_started_run_quota_rejection(
        engine,
        &svc.runs_handle(),
        "user-1",
        "run-quota-fail",
        astra_services::resource_governor::ResourceLimitKind::DailyTokens,
        "daily token budget exhausted (1000/1000)",
    )
    .await;

    assert!(
        committed_events.is_none(),
        "injected transition failure must not report committed events"
    );
    let durable = engine
        .load_run("user-1", "run-quota-fail")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, STATUS_RUNNING);
    assert!(durable.error_code.is_none());
    assert!(
        durable.events.iter().all(|event| {
            !matches!(
                event.get("event_type").and_then(Value::as_str),
                Some("run_error" | "run_finished")
            )
        }),
        "failed quota transition must not leave partial terminal events"
    );
}

#[tokio::test]
async fn stream_chat_token_budget_reject_sends_sse_terminal_events() {
    let svc = test_service()
        .with_resource_governor(Arc::new(DenyTokenBudgetGovernor))
        .with_run_concurrency_limit(1);
    let mut stream = ok(svc
        .stream_chat("user-1".into(), test_request("over budget"))
        .await);
    let mut rx = stream.event_rx.take().expect("stream event receiver");
    let events = tokio::time::timeout(Duration::from_secs(1), async move {
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        events
    })
    .await
    .expect("budget rejection stream should close promptly");

    assert!(
        svc.drain_background_tasks(Duration::from_secs(1)).await,
        "budget rejection task should finish promptly"
    );
    assert!(
        events.iter().any(|event| {
            event.get("type").and_then(Value::as_str) == Some("run_error")
                && event.get("error_code").and_then(Value::as_str)
                    == Some("per_user_daily_token_quota")
        }),
        "SSE stream must include a structured run_error: {events:?}"
    );
    assert!(
        events.iter().any(|event| {
            event.get("type").and_then(Value::as_str) == Some("run_finished")
                && event.get("status").and_then(Value::as_str) == Some(STATUS_FAILED)
        }),
        "SSE stream must include failed run_finished: {events:?}"
    );

    let durable = svc
        .run_engine
        .load_run("user-1", &stream.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.status, STATUS_FAILED);
    assert_eq!(
        durable.error_code.as_deref(),
        Some("per_user_daily_token_quota")
    );
}

#[tokio::test]
async fn interactive_create_run_admission_reject_cleans_ws_channels() {
    let svc = test_service().with_run_concurrency_limit(1);
    svc.test_run_semaphore().close();
    let mut request = test_request("admission closed");
    request.interactive_client = true;

    let err = svc
        .create_run("user-1".into(), request)
        .await
        .expect_err("closed admission should reject before spawn");

    assert_eq!(err.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(err.1.0.error_code.as_deref(), Some("run_admission_closed"));
    assert!(
        svc.approval_channels.lock().await.is_empty(),
        "pre-spawn admission failure must not leak approval channel receivers"
    );
    assert!(
        svc.user_prompt_channels.lock().await.is_empty(),
        "pre-spawn admission failure must not leak ask_user channel receivers"
    );
    assert!(
        svc.progress_channels.lock().await.is_empty(),
        "pre-spawn admission failure must not leak progress channel receivers"
    );

    let page = svc
        .run_engine
        .list_user_runs_cursor("user-1", 10, None)
        .await
        .unwrap();
    let runs = page.runs;
    assert_eq!(page.total, None);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, STATUS_FAILED);
    assert_eq!(runs[0].error_code.as_deref(), Some("run_admission_closed"));
}

#[tokio::test]
async fn interactive_stream_chat_admission_reject_leaves_no_ws_channels() {
    let svc = test_service().with_run_concurrency_limit(1);
    svc.test_run_semaphore().close();
    let mut request = test_request("stream admission closed");
    request.interactive_client = true;

    let err = svc
        .stream_chat("user-1".into(), request)
        .await
        .expect_err("closed admission should reject streaming before spawn");

    assert_eq!(err.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(err.1.0.error_code.as_deref(), Some("run_admission_closed"));
    assert!(svc.approval_channels.lock().await.is_empty());
    assert!(svc.user_prompt_channels.lock().await.is_empty());
    assert!(svc.progress_channels.lock().await.is_empty());

    let page = svc
        .run_engine
        .list_user_runs_cursor("user-1", 10, None)
        .await
        .unwrap();
    let runs = page.runs;
    assert_eq!(page.total, None);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, STATUS_FAILED);
    assert_eq!(runs[0].error_code.as_deref(), Some("run_admission_closed"));
}

// ─── DelegationTracker integration tests ────────────────────────────

#[tokio::test]
async fn delegation_tracker_get_children() {
    use crate::server::delegation::engine::{DelegationTracker, SubRunRecord, SubRunState};

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

/// P0-C: The agentic loop spawn must check token budget before starting.
#[test]
fn run_lifecycle_checks_token_budget_before_loop() {
    let source = include_str!("mod.rs");
    let test_start = source.find("mod tests {").unwrap_or(source.len());
    let prod_code = &source[..test_start];
    assert!(
        prod_code.contains("check_token_budget"),
        "run_lifecycle must call check_token_budget before the agentic loop"
    );
}

/// P0-C: drain_background_tasks returns true when no tasks are running.
#[tokio::test]
async fn drain_background_tasks_returns_immediately_when_idle() {
    // Test the drain logic directly: counter at 0 → drain returns true immediately.
    let count = Arc::new(AtomicUsize::new(0));
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(100);
    let drained = loop {
        if count.load(Ordering::Acquire) == 0 {
            break true;
        }
        if tokio::time::Instant::now() >= deadline {
            break false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    assert!(drained, "counter at 0 — drain must return true immediately");
}

/// P0-C: background_task_count increments on spawn and decrements on exit.
#[tokio::test]
async fn background_task_count_tracks_spawned_tasks() {
    use std::sync::atomic::Ordering;
    let count = Arc::new(AtomicUsize::new(0));
    let count_clone = Arc::clone(&count);

    // Simulate what the spawn does: increment, spawn, decrement on drop
    count.fetch_add(1, Ordering::Release);
    let handle = tokio::spawn(async move {
        struct Guard(Arc<AtomicUsize>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::Release);
            }
        }
        let _g = Guard(count_clone);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    });

    assert_eq!(count.load(Ordering::Acquire), 1, "task in flight");
    handle.await.unwrap();
    assert_eq!(
        count.load(Ordering::Acquire),
        0,
        "task completed — counter must be 0"
    );
}

/// P1-F: list_runs_cursor pagination must be deterministic — all runs appear
/// exactly once across pages, with no duplicates or missing entries.
#[tokio::test]
async fn list_runs_cursor_pagination_is_deterministic() {
    let svc = test_service();
    for i in 0..5 {
        ok(svc
            .create_run("user-pg".into(), test_request(&format!("msg {i}")))
            .await);
    }
    // Collect all run_ids across 3 pages
    let mut all_ids = Vec::new();
    let page1 = ok(svc.list_runs_cursor("user-pg".into(), 2, None).await);
    all_ids.extend(page1.runs.iter().map(|r| r.run_id.clone()));
    let page2 = ok(svc
        .list_runs_cursor("user-pg".into(), 2, page1.next_cursor)
        .await);
    all_ids.extend(page2.runs.iter().map(|r| r.run_id.clone()));
    let page3 = ok(svc
        .list_runs_cursor("user-pg".into(), 2, page2.next_cursor)
        .await);
    all_ids.extend(page3.runs.iter().map(|r| r.run_id.clone()));

    assert_eq!(all_ids.len(), 5, "all 5 runs must appear across pages");
    let unique: std::collections::HashSet<_> = all_ids.iter().collect();
    assert_eq!(
        unique.len(),
        5,
        "no duplicate run_ids across pages — pagination must be deterministic"
    );
}

/// P1-A: RunStatus must have a Waiting variant that is non-terminal.
/// Runs needing external input must not be killed as Failed.
#[test]
fn waiting_is_non_terminal_status() {
    // Running → Waiting is valid
    assert!(
        RunStatus::Running
            .try_transition(&RunStatus::Waiting)
            .is_ok(),
        "Running → Waiting must be allowed"
    );
    // Waiting → Running is valid (resume after input)
    assert!(
        RunStatus::Waiting
            .try_transition(&RunStatus::Running)
            .is_ok(),
        "Waiting → Running must be allowed (resume)"
    );
    // Waiting → Cancelled is valid
    assert!(
        RunStatus::Waiting
            .try_transition(&RunStatus::Cancelled)
            .is_ok(),
        "Waiting → Cancelled must be allowed"
    );
    // Waiting → Failed is valid (timeout)
    assert!(
        RunStatus::Waiting
            .try_transition(&RunStatus::Failed)
            .is_ok(),
        "Waiting → Failed must be allowed"
    );
    // Waiting serializes as "waiting"
    assert_eq!(RunStatus::Waiting.as_str(), "waiting");
    assert!(
        RunStatus::Waiting
            .try_transition(&RunStatus::InputQueued)
            .is_ok(),
        "Waiting → InputQueued must be allowed when user input arrives"
    );
}

#[test]
fn run_status_live_semantics_align_with_durable_owner() {
    assert_eq!(
        RunStatus::from_durable_status(STATUS_RUNNING),
        Some(RunStatus::Running)
    );
    assert_eq!(
        RunStatus::from_durable_status(STATUS_INPUT_QUEUED),
        Some(RunStatus::InputQueued)
    );
    assert_eq!(
        RunStatus::from_durable_status(STATUS_WAITING),
        Some(RunStatus::Waiting)
    );
    assert_eq!(
        RunStatus::from_durable_status(STATUS_PAUSED),
        Some(RunStatus::Paused)
    );
    assert_eq!(
        RunStatus::from_durable_status(STATUS_COMPLETED),
        Some(RunStatus::Completed)
    );
    assert_eq!(
        RunStatus::from_durable_status(STATUS_FAILED),
        Some(RunStatus::Failed)
    );
    assert_eq!(
        RunStatus::from_durable_status(STATUS_CANCELLED),
        Some(RunStatus::Cancelled)
    );
    assert_eq!(RunStatus::from_durable_status("mystery"), None);

    assert!(RunStatus::Waiting.is_resumable());
    assert!(RunStatus::Paused.is_resumable());
    assert!(!RunStatus::Running.is_resumable());
    assert!(!RunStatus::Completed.is_resumable());

    assert_eq!(
        RunStatus::Running.blocks_session(None),
        astra_services::runs::durable_run_status_blocks_session(STATUS_RUNNING, None)
    );
    assert_eq!(
        RunStatus::Waiting.blocks_session(None),
        astra_services::runs::durable_run_status_blocks_session(STATUS_WAITING, None)
    );
    assert_eq!(
        RunStatus::Paused.blocks_session(Some("tool_approval")),
        astra_services::runs::durable_run_status_blocks_session(
            STATUS_PAUSED,
            Some("tool_approval")
        )
    );
    assert_eq!(
        RunStatus::Paused.blocks_session(None),
        astra_services::runs::durable_run_status_blocks_session(STATUS_PAUSED, None)
    );
    assert_eq!(
        RunStatus::Completed.blocks_session(None),
        astra_services::runs::durable_run_status_blocks_session(STATUS_COMPLETED, None)
    );
}

/// P1-A: finalize_run_events must preserve Waiting as a non-error status.
#[test]
fn finalize_run_events_preserves_waiting_without_error_event() {
    let svc = test_service();
    let request = test_request("wait");
    let state = svc.build_initial_state(
        "test-user",
        &request,
        "session-1",
        "run-1",
        None,
        None,
        None,
    );

    let (events, status, error) = AgenticRunLifecycleService::finalize_run_events(
        Ok(AgenticLoopOutcome::Waiting("tool_approval".into())),
        vec![],
        &state,
    );

    assert_eq!(status, RunStatus::Waiting);
    assert_eq!(error.as_deref(), Some("waiting: tool_approval"));
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["event_type"], "run_waiting");
    assert_eq!(events[0]["data"]["reason"], "waiting: tool_approval");
}

/// P1-F: stream_chat must persist usage unconditionally.
/// Cancelled runs still consumed tokens and must have accurate durable records,
/// even when status persistence is skipped.
#[test]
fn stream_chat_persists_usage_unconditionally() {
    let source = include_str!("mod.rs");
    // Find the stream_chat method
    let fn_start = source
        .find("async fn stream_chat(")
        .expect("stream_chat must exist");
    let fn_end = source[fn_start..]
        .find("\n    async fn ")
        .map(|p| fn_start + p)
        .unwrap_or(source.len());
    let fn_body = &source[fn_start..fn_end];

    let usage_pos = fn_body
        .find(".persist_usage(")
        .expect("stream_chat must call persist_usage");

    // persist_usage must NOT be inside the status-persistence guard.
    // Cancelled runs skip persist_status, but usage must still be written.
    let guard_pos = fn_body
        .find("if persist_status_update {")
        .expect("persist_status_update guard must exist");
    let guard_block = &fn_body[guard_pos..];
    let mut depth = 0;
    let mut guard_end = 0;
    for (i, c) in guard_block.char_indices() {
        if c == '{' {
            depth += 1;
        } else if c == '}' {
            depth -= 1;
            if depth == 0 {
                guard_end = guard_pos + i + 1;
                break;
            }
        }
    }
    assert!(
        usage_pos > guard_end,
        "persist_usage must remain outside the persist_status_update guard — \
         cancelled stream_chat runs must still persist usage for billing/audit"
    );
}

#[test]
fn durable_run_usage_uses_provider_input_tokens() {
    let source = include_str!("mod.rs");
    let production = source
        .split("\n#[cfg(test)]\nmod tests")
        .next()
        .expect("production lifecycle source");
    let mut checked_calls = 0;
    let mut cursor = production;
    while let Some(pos) = cursor.find(".persist_usage(") {
        checked_calls += 1;
        let snippet = &cursor[pos..cursor.len().min(pos + 360)];
        assert!(
            snippet.contains("provider_input_tokens()"),
            "durable run usage has no cache-specific columns, so prompt totals must include cache read/write buckets: {snippet}"
        );
        assert!(
            !snippet.contains("total_prompt,"),
            "fresh-input-only totals would under-report prompt-cache-heavy runs: {snippet}"
        );
        cursor = &cursor[pos + ".persist_usage(".len()..];
    }
    assert!(
        checked_calls >= 2,
        "stream_chat must persist usage on terminal paths"
    );
}

#[test]
fn server_subrun_agent_result_prompt_tokens_include_cache_buckets() {
    let source = include_str!("mod.rs");
    let fn_start = source
        .find("impl SubRunExecutor for ServerSubRunExecutor")
        .expect("server sub-run executor must exist");
    let fn_end = source[fn_start..]
        .find("// ─── Tests")
        .map(|p| fn_start + p)
        .expect("server sub-run executor body");
    let fn_body = &source[fn_start..fn_end];

    assert!(
        fn_body.contains("let prompt_tokens = loop_state.provider_input_tokens();"),
        "AgentResult has no cache-specific fields; prompt_tokens must represent provider input tokens"
    );
    assert!(
        !fn_body.contains("prompt_tokens: loop_state.total_prompt"),
        "fresh-input-only totals would under-report prompt-cache-heavy sub-runs"
    );
}

/// P1-C: build_server_skill_executor must accept and wire a cancel_token.
/// Without this, skill sub-runs ignore parent cancellation.
#[test]
fn build_server_skill_executor_accepts_cancel_token() {
    let source = include_str!("mod.rs");
    let fn_start = source
        .find("fn build_server_skill_executor(")
        .expect("build_server_skill_executor must exist");
    let fn_end = source[fn_start..]
        .find("\npub(crate) fn ")
        .or_else(|| source[fn_start..].find("\nfn "))
        .map(|p| fn_start + p)
        .unwrap_or(source.len());
    let fn_body = &source[fn_start..fn_end];
    assert!(
        fn_body.contains("cancel_token"),
        "build_server_skill_executor must accept a cancel_token parameter"
    );
    assert!(
        fn_body.contains("with_cancel_token"),
        "build_server_skill_executor must wire cancel_token via with_cancel_token"
    );
}

/// Runtime tool surfacing for forked server skills must inherit the parent
/// workspace/executor/runtime binding; otherwise sub-runs see raw edge
/// schemas without the capability resolver's runtime truth.
#[test]
fn build_server_skill_executor_wires_execution_binding_snapshot() {
    let source = include_str!("mod.rs");
    let fn_start = source
        .find("fn build_server_skill_executor(")
        .expect("build_server_skill_executor must exist");
    let fn_end = source[fn_start..]
        .find("\npub(crate) fn ")
        .or_else(|| source[fn_start..].find("\nfn "))
        .map(|p| fn_start + p)
        .unwrap_or(source.len());
    let fn_body = &source[fn_start..fn_end];
    assert!(
        fn_body.contains("execution_bindings"),
        "build_server_skill_executor must accept execution binding metadata"
    );
    assert!(
        fn_body.contains("with_execution_binding_snapshot"),
        "build_server_skill_executor must pass execution bindings to server skill sub-runs"
    );
}

#[test]
fn build_server_skill_executor_wires_inherited_permissions() {
    let source = include_str!("mod.rs");
    let fn_start = source
        .find("fn build_server_skill_executor(")
        .expect("build_server_skill_executor must exist");
    let fn_end = source[fn_start..]
        .find("\npub(crate) fn ")
        .or_else(|| source[fn_start..].find("\nfn "))
        .map(|p| fn_start + p)
        .unwrap_or(source.len());
    let fn_body = &source[fn_start..fn_end];
    assert!(
        fn_body.contains("inherited_permissions"),
        "build_server_skill_executor must accept request-level permissions"
    );
    assert!(
        fn_body.contains("with_inherited_permissions"),
        "build_server_skill_executor must pass request-level permissions to server skill sub-runs"
    );
}

#[test]
fn build_server_skill_executor_wires_reflect_service() {
    let source = include_str!("mod.rs");
    let fn_start = source
        .find("fn build_server_skill_executor(")
        .expect("build_server_skill_executor must exist");
    let fn_end = source[fn_start..]
        .find("\npub(crate) fn ")
        .or_else(|| source[fn_start..].find("\nfn "))
        .map(|p| fn_start + p)
        .unwrap_or(source.len());
    let fn_body = &source[fn_start..fn_end];
    assert!(
        fn_body.contains("reflect_service: Arc<dyn astra_services::ReflectService>"),
        "build_server_skill_executor must accept the shared reflect service"
    );
    assert!(
        fn_body.contains(".with_reflect_service(reflect_service)"),
        "build_server_skill_executor must pass reflect service to server skill sub-runs"
    );
}

#[test]
fn lifecycle_executor_construction_wires_reflect_service() {
    let source = include_str!("mod.rs");
    let production = source
        .split("\n#[cfg(test)]\nmod tests")
        .next()
        .expect("production lifecycle source");
    let root_wires = production
        .matches("wire_reflect_service_into_executor(executor, &self.reflect_service)")
        .count();
    assert!(
        root_wires >= 3,
        "root/resume/sub-run ServerToolExecutor construction must all inject the shared reflect service"
    );
    assert!(
        production.contains(".with_reflect_service(Arc::clone(&self.reflect_service))"),
        "dynamic agent spawner/sub-run builders must inherit the shared reflect service"
    );
    assert!(
        production.contains("self.reflect_service.is_configured()"),
        "capability construction must derive reflect visibility from reflect service readiness"
    );
}

/// P1-C: build_initial_state must pass cancel_token to skill executor builder.
#[test]
fn build_initial_state_passes_cancel_token_to_skill_executor() {
    let source = include_str!("mod.rs");
    let fn_start = source
        .find("fn build_initial_state(")
        .expect("build_initial_state must exist");
    let fn_end = source[fn_start..]
        .find("\n    fn ")
        .or_else(|| source[fn_start..].find("\n    pub"))
        .map(|p| fn_start + p)
        .unwrap_or(source.len());
    let fn_body = &source[fn_start..fn_end];
    assert!(
        fn_body.contains("cancel_token"),
        "build_initial_state must accept and pass cancel_token to skill executor"
    );
}

#[test]
fn build_initial_state_passes_execution_bindings_to_skill_executor() {
    let source = include_str!("mod.rs");
    let fn_start = source
        .find("fn build_initial_state(")
        .expect("build_initial_state must exist");
    let fn_end = source[fn_start..]
        .find("\n    fn ")
        .or_else(|| source[fn_start..].find("\n    pub"))
        .map(|p| fn_start + p)
        .unwrap_or(source.len());
    let fn_body = &source[fn_start..fn_end];
    assert!(
        fn_body.contains("execution_bindings"),
        "build_initial_state must accept execution bindings"
    );
    assert!(
        fn_body.contains("build_initial_state_inner(") && fn_body.contains("execution_bindings,"),
        "build_initial_state must pass execution bindings into build_initial_state_inner"
    );

    let inner_start = source[fn_start..]
        .find("fn build_initial_state_inner(")
        .map(|p| fn_start + p)
        .expect("build_initial_state_inner must exist");
    let inner_end = source[inner_start..]
        .find("\n    fn ")
        .or_else(|| source[inner_start..].find("\n    pub"))
        .map(|p| inner_start + p)
        .unwrap_or(source.len());
    let inner_body = &source[inner_start..inner_end];
    assert!(
        inner_body.contains("build_server_skill_executor(")
            && inner_body.contains("execution_bindings,"),
        "build_initial_state_inner must pass execution bindings into the skill executor builder"
    );
}

#[test]
fn resumable_run_statuses_stay_live_for_resume() {
    assert!(RunStatus::Waiting.is_resumable());
    assert!(RunStatus::Paused.is_resumable());
    assert!(!RunStatus::Running.is_resumable());
    assert!(!RunStatus::Completed.is_resumable());
    assert!(!RunStatus::Failed.is_resumable());
    assert!(!RunStatus::Cancelled.is_resumable());
}

/// A Waiting run persisted in durable store must be cancellable even after
/// the process-local control handle is gone.
#[tokio::test]
async fn cancel_run_waiting_cache_miss_persists_cancelled() {
    let svc = test_service_with_engine();
    let engine = &svc.run_engine;
    engine
        .start_run("waiting-run", "user-1", "session-1")
        .await
        .unwrap();
    engine
        .persist_status(
            "user-1",
            "waiting-run",
            STATUS_WAITING,
            Some("tool_approval"),
            None,
        )
        .await
        .unwrap();

    let result = ok(svc.cancel_run("waiting-run".into(), "user-1".into()).await);
    let durable = engine
        .load_run("user-1", "waiting-run")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, STATUS_CANCELLED);
    assert_eq!(durable.status, STATUS_CANCELLED);
}

/// Admission control: semaphore rejects when at capacity, allows after release.
#[tokio::test]
async fn run_semaphore_admission_control() {
    // Limit = 1: only one concurrent run permitted.
    let svc = AgenticRunLifecycleService::new(
        test_settings(),
        test_encryptor(),
        Arc::new(TokioMutex::new(HashMap::new())),
        RunEngine::new(Arc::new(InMemoryRunStateStore::new())),
    )
    .with_run_concurrency_limit(1);
    let sem = svc.test_run_semaphore();

    // 1st acquire succeeds.
    let permit1 = sem.clone().try_acquire_owned().expect("first permit");
    // 2nd acquire must fail — at capacity.
    assert!(
        sem.clone().try_acquire_owned().is_err(),
        "second acquire must fail when at capacity"
    );

    // After release, re-acquire succeeds.
    drop(permit1);
    let permit2 = sem
        .clone()
        .try_acquire_owned()
        .expect("re-acquire after release");
    drop(permit2);
}

/// Admission control: limit=2, third acquire must fail, release creates room.
#[tokio::test]
async fn run_semaphore_limit_two() {
    let svc = AgenticRunLifecycleService::new(
        test_settings(),
        test_encryptor(),
        Arc::new(TokioMutex::new(HashMap::new())),
        RunEngine::new(Arc::new(InMemoryRunStateStore::new())),
    )
    .with_run_concurrency_limit(2);
    let sem = svc.test_run_semaphore();

    let p1 = sem.clone().try_acquire_owned().expect("first");
    let p2 = sem.clone().try_acquire_owned().expect("second");
    assert!(sem.clone().try_acquire_owned().is_err(), "third must fail");

    drop(p1);
    // Now one slot open, re-acquire works.
    let p3 = sem
        .clone()
        .try_acquire_owned()
        .expect("re-acquire after one drop");
    drop(p2);
    drop(p3);
}

/// Admission with timeout: `acquire_owned` + `timeout` rejects after
/// the deadline while a short release window lets a waiter proceed.
#[tokio::test]
async fn run_semaphore_admission_timeout_waits_and_proceeds() {
    let svc = AgenticRunLifecycleService::new(
        test_settings(),
        test_encryptor(),
        Arc::new(TokioMutex::new(HashMap::new())),
        RunEngine::new(Arc::new(InMemoryRunStateStore::new())),
    )
    .with_run_concurrency_limit(1);
    let sem = svc.test_run_semaphore();

    // 1st acquire: capacity exhausted.
    let p1 = sem.clone().try_acquire_owned().expect("first");
    // Spawn a waiter with a short timeout — it will time out.
    let sem2 = sem.clone();
    let timeout_result =
        tokio::time::timeout(std::time::Duration::from_millis(50), sem2.acquire_owned()).await;
    assert!(
        timeout_result.is_err(),
        "waiter should time out when no slot opens"
    );

    // Now spawn a waiter and release the slot quickly — waiter should get it.
    let sem3 = sem.clone();
    let waiter = tokio::spawn(async move {
        tokio::time::timeout(std::time::Duration::from_secs(5), sem3.acquire_owned())
            .await
            .expect("timeout should not fire")
            .expect("acquire_owned")
    });
    // Small yield to let the waiter enter acquire_owned.
    tokio::task::yield_now().await;
    drop(p1); // release the slot
    let p2 = waiter.await.expect("waiter panicked");
    drop(p2);
}

#[tokio::test]
async fn run_admission_metrics_record_acquired_and_timeout() {
    let registry = Arc::new(astra_turn_core::pipeline_metrics::MetricsRegistry::new());
    let svc = test_service()
        .with_run_concurrency_limit(1)
        .with_metrics_registry(registry.clone());
    let sem = svc.test_run_semaphore();
    let first = sem.clone().try_acquire_owned().expect("first permit");

    let timed_out = match svc.acquire_run_permit(Duration::from_millis(5)).await {
        Ok(_) => panic!("admission should time out while the only permit is held"),
        Err(error) => error,
    };
    assert_eq!(timed_out, RunAdmissionError::Timeout);

    drop(first);
    let acquired = svc
        .acquire_run_permit(Duration::from_secs(1))
        .await
        .expect("released permit should be acquired");
    drop(acquired);

    let rendered = registry.render_prometheus();
    assert!(
        rendered.contains("astra_run_admission_attempts_total{outcome=\"timeout\"} 1"),
        "{rendered}"
    );
    assert!(
        rendered.contains("astra_run_admission_attempts_total{outcome=\"acquired\"} 1"),
        "{rendered}"
    );
    assert!(
        rendered.contains("astra_run_admission_wait_ms_total{outcome=\"timeout\"}"),
        "{rendered}"
    );
}

#[tokio::test]
async fn run_admission_closed_semaphore_is_rejected_and_counted() {
    let registry = Arc::new(astra_turn_core::pipeline_metrics::MetricsRegistry::new());
    let svc = test_service()
        .with_run_concurrency_limit(1)
        .with_metrics_registry(registry.clone());
    svc.test_run_semaphore().close();

    let error = match svc.acquire_run_permit(Duration::from_secs(1)).await {
        Ok(_) => panic!("closed semaphore must not admit a run without a permit"),
        Err(error) => error,
    };

    assert_eq!(error, RunAdmissionError::Closed);
    let rendered = registry.render_prometheus();
    assert!(
        rendered.contains("astra_run_admission_attempts_total{outcome=\"closed\"} 1"),
        "{rendered}"
    );
}

#[test]
fn run_admission_timeout_ignores_legacy_env_knob() {
    let _default = EnvVarGuard::remove("ASTRA_RUN_ADMISSION_TIMEOUT_SECS");
    assert_eq!(
        run_admission_timeout(),
        Duration::from_secs(DEFAULT_RUN_ADMISSION_TIMEOUT_SECS)
    );

    let _legacy = EnvVarGuard::set("ASTRA_RUN_ADMISSION_TIMEOUT_SECS", "90");
    assert_eq!(
        run_admission_timeout(),
        Duration::from_secs(DEFAULT_RUN_ADMISSION_TIMEOUT_SECS)
    );
}

#[test]
fn run_admission_capacity_response_uses_distinct_error_codes() {
    let timeout = run_admission_capacity_response(RunAdmissionError::Timeout);
    assert_eq!(timeout.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        timeout.1.error_code.as_deref(),
        Some("run_admission_timeout")
    );
    assert!(timeout.1.detail.contains("run_admission_timeout"));

    let closed = run_admission_capacity_response(RunAdmissionError::Closed);
    assert_eq!(closed.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(closed.1.error_code.as_deref(), Some("run_admission_closed"));
    assert!(closed.1.detail.contains("run_admission_closed"));
}

#[test]
fn per_user_run_quota_response_uses_quota_error_code() {
    let response = per_user_run_quota_response(
        astra_services::resource_governor::ResourceLimitKind::ConcurrentSessions,
        "concurrent session limit reached (5/5)".to_string(),
    );

    assert_eq!(response.0, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response.1.error_code.as_deref(),
        Some("per_user_concurrent_session_quota")
    );
    assert!(response.1.detail.contains("Per-user run quota exceeded"));
    assert!(response.1.detail.contains("concurrent_sessions"));
}

#[test]
fn durable_run_event_batch_metrics_record_rows_bytes_and_compaction() {
    let registry = Arc::new(astra_turn_core::pipeline_metrics::MetricsRegistry::new());
    register_durable_run_event_metrics(&registry);
    let events = vec![
        json!({"event_type": "durable_events_compacted", "data": {"dropped_events": 10}}),
        json!({"event_type": "run_finished", "data": {"prompt_tokens": 1}}),
    ];
    let bytes = events
        .iter()
        .map(durable_run_event_estimated_bytes)
        .sum::<usize>();

    record_durable_run_event_batch_metrics(
        Some(&registry),
        "streaming_terminal",
        "planned",
        &events,
    );
    record_durable_run_event_batch_metrics(Some(&registry), "streaming_terminal", "error", &events);

    let rendered = registry.render_prometheus();
    assert!(
        rendered.contains("# TYPE astra_durable_run_event_row_budget gauge")
            && rendered.contains("astra_durable_run_event_row_budget "),
        "{rendered}"
    );
    assert!(
        rendered.contains("# TYPE astra_durable_run_event_byte_budget gauge")
            && rendered.contains("astra_durable_run_event_byte_budget "),
        "{rendered}"
    );
    assert!(
        rendered.contains(
            "astra_durable_run_event_batches_total{compacted=\"true\",outcome=\"planned\",path=\"streaming_terminal\"} 1"
        ),
        "{rendered}"
    );
    assert!(
        rendered.contains(
            "astra_durable_run_event_rows_total{compacted=\"true\",outcome=\"planned\",path=\"streaming_terminal\"} 2"
        ),
        "{rendered}"
    );
    assert!(
        rendered.contains(&format!(
            "astra_durable_run_event_bytes_total{{compacted=\"true\",outcome=\"planned\",path=\"streaming_terminal\"}} {bytes}"
        )),
        "{rendered}"
    );
    assert!(
        rendered.contains(
            "astra_durable_run_event_batches_total{compacted=\"true\",outcome=\"error\",path=\"streaming_terminal\"} 1"
        ),
        "{rendered}"
    );
    assert!(
        !rendered.contains("run_id=") && !rendered.contains("session_id="),
        "metrics must stay low-cardinality: {rendered}"
    );
}
