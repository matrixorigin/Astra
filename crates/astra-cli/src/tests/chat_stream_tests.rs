use super::spawn_mock;
use crate::cli::chat_stream::{
    BasicCliChatContext, ChatTurnParams, DEFAULT_TURN_INDEX, stream_chat_sse,
};
use crate::cli::permission_manager::PermissionManager;
use crate::cli::session::session_state::ExplainMode;
use crate::edge_tools;
use astra_runtime::tool_registry;
use astra_services::session_journal::{self, JournalDirGuard, JournalEventType};
use axum::{Json, Router, routing::post};

// ── chat_stream (SSE agentic loop) ────────────────────────────────────

/// Build a canonical SSE response for the mock chat-turn endpoint. Exposed
/// to sibling test modules (e.g. `resume_tests`) so they don't have to
/// duplicate the payload literal.
pub(super) fn sse_text_response(text: &str, session_id: &str) -> String {
    format!(
        "data: {{\"type\":\"session_info\",\"session_id\":\"{session_id}\",\"run_id\":\"run-{session_id}\"}}\n\n\
             data: {{\"type\":\"text_delta\",\"content\":\"{text}\"}}\n\n\
             data: {{\"type\":\"text_done\",\"full_text\":\"{text}\"}}\n\n\
             data: {{\"type\":\"usage\",\"input_tokens\":10,\"output_tokens\":5}}\n\n\
             data: {{\"type\":\"turn_complete\",\"has_tool_calls\":false}}\n\n\
             data: [DONE]\n\n"
    )
}

#[tokio::test]
async fn stream_chat_sse_cannot_turn_active_fanout_into_a_completion_claim() {
    let captured_request = std::sync::Arc::new(std::sync::Mutex::new(None));
    let captured_request_for_route = captured_request.clone();
    let app = Router::new().route(
        "/chat/turn",
        post(move |Json(payload): Json<serde_json::Value>| {
            let captured_request = captured_request_for_route.clone();
            async move {
                *captured_request.lock().unwrap() = Some(payload);
                (
                    [("content-type", "text/event-stream")],
                    sse_text_response(
                        "All three agents completed. Here is the consolidated report.",
                        "sess-active-fanout",
                    ),
                )
            }
        }),
    );
    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
    let unified_skill_registry = astra_runtime::skills::empty_unified_registry().clone();
    let context = BasicCliChatContext {
        api: &api,
        auth_profile: None,
        message: "What is still running?",
        model_id: None,
        model: Some("test-model"),
        provider: None,
        explain: ExplainMode::Off,
        render_md: false,
        verbose_mode: false,
        render_policy: crate::cli::stream::stream_render::RenderPolicy::Silent,
        cli_context: None,
        unified_skill_registry: &unified_skill_registry,
        agent_spawner: None,
        root_agent_id: None,
        task_manager: None,
        task_notify_tx: None,
        bg_task_commands: None,
        bg_task_list_cache: None,
        bash_detach_slot: None,
        stream_event_tx: None,
        #[cfg(feature = "harness")]
        harness_sink: None,
        #[cfg(feature = "harness")]
        harness_trace: None,
        #[cfg(feature = "harness")]
        benchmark_profile: None,
    };
    let observations = vec![
        astra_core::work_unit::WorkUnitObservation::new(
            "review-group",
            "agent_fanout",
            astra_core::work_unit::WorkUnitStatus::Running,
            7,
            astra_core::work_unit::WorkUnitObservationMode::Current,
        )
        .unwrap()
        .with_wake_policy(astra_core::work_unit::WorkUnitWakePolicy::OnTerminal),
    ];
    let mut permission_manager = PermissionManager::new(true);
    let mut skill_quality_tracker = astra_skills::quality::SkillQualityTracker::new();
    let mut params = ChatTurnParams::basic_cli(
        &context,
        "fake-token",
        Some("sess-active-fanout"),
        &mut permission_manager,
        &mut skill_quality_tracker,
    );
    params.input_work_unit_observations = &observations;

    let result = stream_chat_sse(params).await.unwrap();

    assert!(
        result
            .full_text
            .contains("All three agents completed. Here is the consolidated report.")
    );
    assert!(
        result.full_text.contains(
            "Runtime state (authoritative): 1 asynchronous work unit(s) remain non-terminal"
        ),
        "runtime must deterministically qualify a false completion claim: {}",
        result.full_text
    );
    assert!(
        result
            .full_text
            .contains("This response is a partial snapshot, not a completion report")
    );

    let request = captured_request.lock().unwrap().clone().unwrap();
    let injections = request["edge_profile"]
        [astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_RUNTIME_VOLATILE_INJECTIONS]
        .as_array()
        .expect("typed runtime injection lane");
    let active_work = injections
        .iter()
        .find(|injection| injection["kind"] == "active_work_snapshot")
        .expect("active work snapshot reaches the model boundary");
    assert_eq!(active_work["delivery_class"], "required_context");
    assert_eq!(active_work["payload"]["authority"], "runtime_producer");
    assert_eq!(
        active_work["payload"]["work_unit_observations"][0]["id"],
        "review-group"
    );
    assert_eq!(
        active_work["payload"]["work_unit_observations"][0]["status"],
        "running"
    );
}

fn mock_mcp_server_binary() -> std::path::PathBuf {
    crate::mcp_client::ensure_mock_mcp_server_binary()
}

#[tokio::test(flavor = "current_thread")]
async fn stream_chat_sse_persists_first_turn_step_events_under_adopted_session_id() {
    let temp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(temp.path());
    let app = Router::new().route(
        "/chat/turn",
        post(|| async {
            (
                [("content-type", "text/event-stream")],
                sse_text_response("Hello!", "sess-step-adopt"),
            )
        }),
    );
    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
    let _registry = tool_registry::ToolRegistry::new(edge_tools::all_tool_schemas());
    let mut pm = PermissionManager::new(true);
    let mut skill_qt = astra_skills::quality::SkillQualityTracker::new();

    let result = stream_chat_sse(ChatTurnParams {
        api: &api,
        token: "fake-token",
        auth_profile: None,
        message: "hi",
        user_intent: "hi",
        input_runtime_required_texts: &[],
        input_runtime_volatile_texts: &[],
        input_work_unit_observations: &[],
        semantic_query_override: None,
        session_id: None,
        model_id: None,
        model: Some("test-model"),
        provider: None,
        explain: ExplainMode::Off,
        render_md: false,
        history: &[],
        perm_manager: &mut pm,
        verbose_mode: false,
        render_policy: crate::cli::stream::stream_render::RenderPolicy::Silent,
        cli_context: None,
        recent_tools: &[],
        activated_deferred_tool_names: None,
        resume_restricted_tools: &[],
        tool_health_entries: &[],
        session_lessons: &[],
        latest_skill_diagnosis: None,
        latest_turn_quality_feedback: None,
        unified_skill_registry: astra_runtime::skills::empty_unified_registry(),
        is_plan_subtask: false,
        plan_subtask_id: None,
        delegation_engine: None,
        cancel_token: None,
        run_control: None,
        incremental_state: None,
        plan_assemble_line_release: None,
        stream_event_tx: None,
        agent_live_event_sink: None,
        approval_request_tx: None,
        ask_user_request_tx: None,
        plan_review_request_tx: None,
        mcp_manager: None,
        skill_quality_tracker: &mut skill_qt,
        discovered_skills: None,
        messaging_metrics: None,
        agent_spawner: None,
        root_agent_id: None,
        root_mailbox_slot: None,
        observability_hub: None,
        observability_session: None,
        file_journal: None,
        file_state: None,
        database_snapshot_journal: None,
        git_stash_journal: None,
        git_commit_journal: None,
        git_worktree_journal: None,
        session_state_journal: None,
        task_manager: None,
        task_notify_tx: None,
        bg_task_commands: None,
        bg_task_list_cache: None,
        bash_detach_slot: None,
        turn_index: DEFAULT_TURN_INDEX,
        pipeline_state: None,
        compaction_state: None,
        consecutive_context_window_errors: 0,
        idempotency_cache: None,
        pre_loaded_messages: None,
        append_system_prompt: None,
        session_memory_extractor: None,
        #[cfg(feature = "harness")]
        harness_sink: None,
        #[cfg(feature = "harness")]
        harness_trace: None,
        #[cfg(feature = "harness")]
        benchmark_profile: None,
    })
    .await
    .unwrap();

    assert_eq!(result.session_id.as_deref(), Some("sess-step-adopt"));

    let cli_user_id = crate::cli::cli_config::cli_utils::cli_user_id();
    let store =
        astra_pipeline::step_checkpoint::FileBackedEventStore::new(&cli_user_id, "sess-step-adopt");
    let events = store.all_events();
    assert!(
        !events.is_empty(),
        "step events should persist under adopted session"
    );
    assert!(
        events
            .iter()
            .any(|e| e.step_id == "sess-step-adopt-turn-1-step-0"),
        "expected step_id sess-step-adopt-turn-1-step-0, found: {:?}",
        events.iter().map(|e| &e.step_id).collect::<Vec<_>>()
    );
    let ephemeral_store =
        astra_pipeline::step_checkpoint::FileBackedEventStore::new(&cli_user_id, "ephemeral");
    assert!(
        ephemeral_store.all_events().is_empty(),
        "new-session first turn must not persist step events under ephemeral/"
    );
}

#[tokio::test]
async fn stream_chat_sse_simple_text_response() {
    let app = Router::new().route(
        "/chat/turn",
        post(|| async {
            (
                [("content-type", "text/event-stream")],
                sse_text_response("Hello!", "sess-001"),
            )
        }),
    );
    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
    let _registry = tool_registry::ToolRegistry::new(edge_tools::all_tool_schemas());
    let mut pm = PermissionManager::new(true);
    let mut skill_qt = astra_skills::quality::SkillQualityTracker::new();
    let result = stream_chat_sse(ChatTurnParams {
        api: &api,
        token: "fake-token",
        auth_profile: None,
        message: "hi",
        user_intent: "hi",
        input_runtime_required_texts: &[],
        input_runtime_volatile_texts: &[],
        input_work_unit_observations: &[],
        semantic_query_override: None,
        session_id: None,
        model_id: None,
        model: Some("test-model"),
        provider: None,
        explain: ExplainMode::Off,
        render_md: false,
        history: &[],
        perm_manager: &mut pm,
        verbose_mode: false,
        render_policy: crate::cli::stream::stream_render::RenderPolicy::Silent,
        cli_context: None,
        recent_tools: &[],
        activated_deferred_tool_names: None,
        resume_restricted_tools: &[],
        tool_health_entries: &[],
        session_lessons: &[],
        latest_skill_diagnosis: None,
        latest_turn_quality_feedback: None,
        unified_skill_registry: astra_runtime::skills::empty_unified_registry(),
        is_plan_subtask: false,
        plan_subtask_id: None,
        delegation_engine: None,
        cancel_token: None,
        run_control: None,
        incremental_state: None,
        plan_assemble_line_release: None,
        stream_event_tx: None,
        agent_live_event_sink: None,
        approval_request_tx: None,
        ask_user_request_tx: None,
        plan_review_request_tx: None,
        mcp_manager: None,
        skill_quality_tracker: &mut skill_qt,
        discovered_skills: None,
        messaging_metrics: None,
        agent_spawner: None,
        root_agent_id: None,
        root_mailbox_slot: None,
        observability_hub: None,
        observability_session: None,
        file_journal: None,
        file_state: None,
        database_snapshot_journal: None,
        git_stash_journal: None,
        git_commit_journal: None,
        git_worktree_journal: None,
        session_state_journal: None,
        task_manager: None,
        task_notify_tx: None,
        bg_task_commands: None,
        bg_task_list_cache: None,
        bash_detach_slot: None,
        turn_index: DEFAULT_TURN_INDEX,
        pipeline_state: None,
        compaction_state: None,
        consecutive_context_window_errors: 0,
        idempotency_cache: None,
        pre_loaded_messages: None,
        append_system_prompt: None,
        session_memory_extractor: None,
        #[cfg(feature = "harness")]
        harness_sink: None,
        #[cfg(feature = "harness")]
        harness_trace: None,
        #[cfg(feature = "harness")]
        benchmark_profile: None,
    })
    .await
    .unwrap();
    assert_eq!(result.full_text, "Hello!");
    assert_eq!(result.session_id.as_deref(), Some("sess-001"));
    assert_eq!(result.prompt_tokens, 10);
    assert_eq!(result.completion_tokens, 5);
}

#[tokio::test]
async fn stream_chat_sse_preserves_existing_session_id_for_server_scoped_trace() {
    #[derive(Clone)]
    struct MockState {
        turn_payloads: std::sync::Arc<tokio::sync::Mutex<Vec<serde_json::Value>>>,
    }

    let state = MockState {
        turn_payloads: std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new())),
    };
    let app = Router::new().route(
        "/chat/turn",
        post({
            let state = state.clone();
            move |axum::Json(body): axum::Json<serde_json::Value>| {
                let state = state.clone();
                async move {
                    state.turn_payloads.lock().await.push(body);
                    (
                        [("content-type", "text/event-stream")],
                        sse_text_response("Hello!", "sess-traced"),
                    )
                }
            }
        }),
    );
    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
    let _registry = tool_registry::ToolRegistry::new(edge_tools::all_tool_schemas());
    let mut pm = PermissionManager::new(true);
    let mut skill_qt = astra_skills::quality::SkillQualityTracker::new();

    let result = stream_chat_sse(ChatTurnParams {
        api: &api,
        token: "fake-token",
        auth_profile: None,
        message: "hi",
        user_intent: "hi",
        input_runtime_required_texts: &[],
        input_runtime_volatile_texts: &[],
        input_work_unit_observations: &[],
        semantic_query_override: None,
        session_id: Some("sess-traced"),
        model_id: None,
        model: Some("test-model"),
        provider: None,
        explain: ExplainMode::Off,
        render_md: false,
        history: &[],
        perm_manager: &mut pm,
        verbose_mode: false,
        render_policy: crate::cli::stream::stream_render::RenderPolicy::Silent,
        cli_context: None,
        recent_tools: &[],
        activated_deferred_tool_names: None,
        resume_restricted_tools: &[],
        tool_health_entries: &[],
        session_lessons: &[],
        latest_skill_diagnosis: None,
        latest_turn_quality_feedback: None,
        unified_skill_registry: astra_runtime::skills::empty_unified_registry(),
        is_plan_subtask: false,
        plan_subtask_id: None,
        delegation_engine: None,
        cancel_token: None,
        run_control: None,
        incremental_state: None,
        plan_assemble_line_release: None,
        stream_event_tx: None,
        agent_live_event_sink: None,
        approval_request_tx: None,
        ask_user_request_tx: None,
        plan_review_request_tx: None,
        mcp_manager: None,
        skill_quality_tracker: &mut skill_qt,
        discovered_skills: None,
        messaging_metrics: None,
        agent_spawner: None,
        root_agent_id: None,
        root_mailbox_slot: None,
        observability_hub: None,
        observability_session: None,
        file_journal: None,
        file_state: None,
        database_snapshot_journal: None,
        git_stash_journal: None,
        git_commit_journal: None,
        git_worktree_journal: None,
        session_state_journal: None,
        task_manager: None,
        task_notify_tx: None,
        bg_task_commands: None,
        bg_task_list_cache: None,
        bash_detach_slot: None,
        turn_index: DEFAULT_TURN_INDEX,
        pipeline_state: None,
        compaction_state: None,
        consecutive_context_window_errors: 0,
        idempotency_cache: None,
        pre_loaded_messages: None,
        append_system_prompt: None,
        session_memory_extractor: None,
        #[cfg(feature = "harness")]
        harness_sink: None,
        #[cfg(feature = "harness")]
        harness_trace: None,
        #[cfg(feature = "harness")]
        benchmark_profile: None,
    })
    .await
    .unwrap();

    assert_eq!(result.session_id.as_deref(), Some("sess-traced"));

    let payloads = state.turn_payloads.lock().await;
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0]["session_id"], serde_json::json!("sess-traced"));
}

#[tokio::test]
async fn stream_chat_sse_reuses_persistent_root_mailbox_across_turns() {
    let app = Router::new().route(
        "/chat/turn",
        post(|| async {
            (
                [("content-type", "text/event-stream")],
                sse_text_response("Hello!", "sess-001"),
            )
        }),
    );
    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
    let _registry = tool_registry::ToolRegistry::new(edge_tools::all_tool_schemas());
    let transport = std::sync::Arc::new(astra_messaging::InProcessTransport::new());
    let tracker =
        std::sync::Arc::new(astra_runtime::server::delegation::engine::DelegationTracker::new());
    let router = std::sync::Arc::new(astra_messaging::AgentMailboxRouter::new(transport, tracker));
    let spawner = std::sync::Arc::new(astra_runtime::orchestration::DynamicAgentSpawner::new(
        router.clone(),
    ));
    let mut root_mailbox = Some(
        router
            .register(
                astra_messaging::AgentAddress::new("persisted-run", "main"),
                None,
            )
            .await
            .unwrap(),
    );

    for session_id in [None, Some("sess-override")] {
        let mut pm = PermissionManager::new(true);
        let mut skill_qt = astra_skills::quality::SkillQualityTracker::new();
        let result = stream_chat_sse(ChatTurnParams {
            api: &api,
            token: "fake-token",
            auth_profile: None,
            message: "hi",
            user_intent: "hi",
            input_runtime_required_texts: &[],
            input_runtime_volatile_texts: &[],
            input_work_unit_observations: &[],
            semantic_query_override: None,
            session_id,
            model_id: None,
            model: Some("test-model"),
            provider: None,
            explain: ExplainMode::Off,
            render_md: false,
            history: &[],
            perm_manager: &mut pm,
            verbose_mode: false,
            render_policy: crate::cli::stream::stream_render::RenderPolicy::Silent,
            cli_context: None,
            recent_tools: &[],
            activated_deferred_tool_names: None,
            resume_restricted_tools: &[],
            tool_health_entries: &[],
            session_lessons: &[],
            latest_skill_diagnosis: None,
            latest_turn_quality_feedback: None,
            unified_skill_registry: astra_runtime::skills::empty_unified_registry(),
            is_plan_subtask: false,
            plan_subtask_id: None,
            delegation_engine: None,
            cancel_token: None,
            run_control: None,
            incremental_state: None,
            plan_assemble_line_release: None,
            stream_event_tx: None,
            agent_live_event_sink: None,
            approval_request_tx: None,
            ask_user_request_tx: None,
            plan_review_request_tx: None,
            mcp_manager: None,
            skill_quality_tracker: &mut skill_qt,
            discovered_skills: None,
            messaging_metrics: None,
            agent_spawner: Some(spawner.clone()),
            root_agent_id: Some("main"),
            root_mailbox_slot: Some(&mut root_mailbox),
            observability_hub: None,
            observability_session: None,
            file_journal: None,
            file_state: None,
            database_snapshot_journal: None,
            git_stash_journal: None,
            git_commit_journal: None,
            git_worktree_journal: None,
            session_state_journal: None,
            task_manager: None,
            task_notify_tx: None,
            bg_task_commands: None,
            bg_task_list_cache: None,
            bash_detach_slot: None,
            turn_index: DEFAULT_TURN_INDEX,
            pipeline_state: None,
            compaction_state: None,
            consecutive_context_window_errors: 0,
            idempotency_cache: None,
            pre_loaded_messages: None,
            append_system_prompt: None,
            session_memory_extractor: None,
            #[cfg(feature = "harness")]
            harness_sink: None,
            #[cfg(feature = "harness")]
            harness_trace: None,
            #[cfg(feature = "harness")]
            benchmark_profile: None,
        })
        .await
        .unwrap();
        assert_eq!(result.full_text, "Hello!");
        assert_eq!(
            root_mailbox
                .as_ref()
                .map(|mailbox| mailbox.address.run_id.as_str()),
            Some("persisted-run")
        );
    }
}

#[tokio::test]
async fn stream_chat_sse_executes_bound_agent_spawn_to_child_request() {
    let mock = crate::cli::mock_llm::MockLlmServer::start(
        crate::cli::mock_llm::MockScenario::AgentThenComplete,
    )
    .await
    .expect("start scripted parent and child LLM server");
    let api = astra_thin_client::ThinClient::new(&mock.base_url, None).unwrap();
    let unified_skill_registry = astra_runtime::skills::empty_unified_registry().clone();
    let spawner = crate::cli::agent_runtime::build_one_shot_spawner(
        &api,
        "fake-token".to_string(),
        unified_skill_registry.clone(),
        None,
        Some("mock-model".to_string()),
    )
    .await;
    let mut pm = PermissionManager::new(true);
    let mut skill_qt = astra_skills::quality::SkillQualityTracker::new();

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        stream_chat_sse(ChatTurnParams {
            api: &api,
            token: "fake-token",
            auth_profile: None,
            message: "delegate_one_child_and_keep_it_observable",
            user_intent: "delegate_one_child_and_keep_it_observable",
            input_runtime_required_texts: &[],
            input_runtime_volatile_texts: &[],
            input_work_unit_observations: &[],
            semantic_query_override: None,
            session_id: None,
            model_id: None,
            model: Some("mock-model"),
            provider: None,
            explain: ExplainMode::Off,
            render_md: false,
            history: &[],
            perm_manager: &mut pm,
            verbose_mode: false,
            render_policy: crate::cli::stream::stream_render::RenderPolicy::Silent,
            cli_context: None,
            recent_tools: &[],
            activated_deferred_tool_names: None,
            resume_restricted_tools: &[],
            tool_health_entries: &[],
            session_lessons: &[],
            latest_skill_diagnosis: None,
            latest_turn_quality_feedback: None,
            unified_skill_registry: &unified_skill_registry,
            is_plan_subtask: false,
            plan_subtask_id: None,
            delegation_engine: None,
            cancel_token: None,
            run_control: None,
            incremental_state: None,
            plan_assemble_line_release: None,
            stream_event_tx: None,
            agent_live_event_sink: None,
            approval_request_tx: None,
            ask_user_request_tx: None,
            plan_review_request_tx: None,
            mcp_manager: None,
            skill_quality_tracker: &mut skill_qt,
            discovered_skills: None,
            messaging_metrics: None,
            agent_spawner: Some(spawner),
            root_agent_id: Some("main"),
            root_mailbox_slot: None,
            observability_hub: None,
            observability_session: None,
            file_journal: None,
            file_state: None,
            database_snapshot_journal: None,
            git_stash_journal: None,
            git_commit_journal: None,
            git_worktree_journal: None,
            session_state_journal: None,
            task_manager: None,
            task_notify_tx: None,
            bg_task_commands: None,
            bg_task_list_cache: None,
            bash_detach_slot: None,
            turn_index: DEFAULT_TURN_INDEX,
            pipeline_state: None,
            compaction_state: None,
            consecutive_context_window_errors: 0,
            idempotency_cache: None,
            pre_loaded_messages: None,
            append_system_prompt: None,
            session_memory_extractor: None,
            #[cfg(feature = "harness")]
            harness_sink: None,
            #[cfg(feature = "harness")]
            harness_trace: None,
            #[cfg(feature = "harness")]
            benchmark_profile: None,
        }),
    )
    .await
    .expect("agent spawn turn should not hang")
    .expect("agent spawn turn should complete");

    assert!(
        result
            .full_text
            .contains("Parent synthesized the child evidence"),
        "parent should continue after one child result: {:?}",
        result.full_text
    );
    let child_requests = mock
        .received_requests()
        .into_iter()
        .filter(|request| {
            request
                .get("agent_type")
                .and_then(serde_json::Value::as_str)
                == Some("general-purpose")
                && request
                    .get("messages")
                    .and_then(serde_json::Value::as_array)
                    .and_then(|messages| {
                        messages.iter().rev().find(|message| {
                            message.get("role").and_then(serde_json::Value::as_str) == Some("user")
                        })
                    })
                    .and_then(|message| message.get("content"))
                    .and_then(serde_json::Value::as_str)
                    == Some(crate::cli::mock_llm::AGENT_JOURNEY_CHILD_TASK)
        })
        .count();
    assert_eq!(child_requests, 1);
}

#[tokio::test]
async fn stream_chat_sse_unregisters_ephemeral_root_mailbox() {
    let app = Router::new().route(
        "/chat/turn",
        post(|| async {
            (
                [("content-type", "text/event-stream")],
                sse_text_response("Hello!", "sess-001"),
            )
        }),
    );
    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
    let _registry = tool_registry::ToolRegistry::new(edge_tools::all_tool_schemas());
    let transport = std::sync::Arc::new(astra_messaging::InProcessTransport::new());
    let tracker =
        std::sync::Arc::new(astra_runtime::server::delegation::engine::DelegationTracker::new());
    let router = std::sync::Arc::new(astra_messaging::AgentMailboxRouter::new(transport, tracker));
    let spawner = std::sync::Arc::new(astra_runtime::orchestration::DynamicAgentSpawner::new(
        router.clone(),
    ));
    let mut pm = PermissionManager::new(true);
    let mut skill_qt = astra_skills::quality::SkillQualityTracker::new();

    let result = stream_chat_sse(ChatTurnParams {
        api: &api,
        token: "fake-token",
        auth_profile: None,
        message: "hi",
        user_intent: "hi",
        input_runtime_required_texts: &[],
        input_runtime_volatile_texts: &[],
        input_work_unit_observations: &[],
        semantic_query_override: None,
        session_id: None,
        model_id: None,
        model: Some("test-model"),
        provider: None,
        explain: ExplainMode::Off,
        render_md: false,
        history: &[],
        perm_manager: &mut pm,
        verbose_mode: false,
        render_policy: crate::cli::stream::stream_render::RenderPolicy::Silent,
        cli_context: None,
        recent_tools: &[],
        activated_deferred_tool_names: None,
        resume_restricted_tools: &[],
        tool_health_entries: &[],
        session_lessons: &[],
        latest_skill_diagnosis: None,
        latest_turn_quality_feedback: None,
        unified_skill_registry: astra_runtime::skills::empty_unified_registry(),
        is_plan_subtask: false,
        plan_subtask_id: None,
        delegation_engine: None,
        cancel_token: None,
        run_control: None,
        incremental_state: None,
        plan_assemble_line_release: None,
        stream_event_tx: None,
        agent_live_event_sink: None,
        approval_request_tx: None,
        ask_user_request_tx: None,
        plan_review_request_tx: None,
        mcp_manager: None,
        skill_quality_tracker: &mut skill_qt,
        discovered_skills: None,
        messaging_metrics: None,
        agent_spawner: Some(spawner),
        root_agent_id: Some("bg-root"),
        root_mailbox_slot: None,
        observability_hub: None,
        observability_session: None,
        file_journal: None,
        file_state: None,
        database_snapshot_journal: None,
        git_stash_journal: None,
        git_commit_journal: None,
        git_worktree_journal: None,
        session_state_journal: None,
        task_manager: None,
        task_notify_tx: None,
        bg_task_commands: None,
        bg_task_list_cache: None,
        bash_detach_slot: None,
        turn_index: DEFAULT_TURN_INDEX,
        pipeline_state: None,
        compaction_state: None,
        consecutive_context_window_errors: 0,
        idempotency_cache: None,
        pre_loaded_messages: None,
        append_system_prompt: None,
        session_memory_extractor: None,
        #[cfg(feature = "harness")]
        harness_sink: None,
        #[cfg(feature = "harness")]
        harness_trace: None,
        #[cfg(feature = "harness")]
        benchmark_profile: None,
    })
    .await
    .unwrap();

    assert_eq!(result.full_text, "Hello!");
    assert!(
        !router.is_run_registered("persisted-run").await,
        "ephemeral root mailbox should be unregistered after the turn"
    );
}

#[tokio::test]
async fn stream_chat_sse_api_error_propagated() {
    let app = Router::new().route(
        "/chat/turn",
        post(|| async {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({"detail": "model overloaded"})),
            )
        }),
    );
    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
    let _registry = tool_registry::ToolRegistry::new(edge_tools::all_tool_schemas());
    let mut pm = PermissionManager::new(true);
    let mut skill_qt = astra_skills::quality::SkillQualityTracker::new();
    let result = stream_chat_sse(ChatTurnParams {
        api: &api,
        token: "fake-token",
        auth_profile: None,
        message: "hi",
        user_intent: "hi",
        input_runtime_required_texts: &[],
        input_runtime_volatile_texts: &[],
        input_work_unit_observations: &[],
        semantic_query_override: None,
        session_id: None,
        model_id: None,
        model: Some("test-model"),
        provider: None,
        explain: ExplainMode::Off,
        render_md: false,
        history: &[],
        perm_manager: &mut pm,
        verbose_mode: false,
        render_policy: crate::cli::stream::stream_render::RenderPolicy::Silent,
        cli_context: None,
        recent_tools: &[],
        activated_deferred_tool_names: None,
        resume_restricted_tools: &[],
        tool_health_entries: &[],
        session_lessons: &[],
        latest_skill_diagnosis: None,
        latest_turn_quality_feedback: None,
        unified_skill_registry: astra_runtime::skills::empty_unified_registry(),
        is_plan_subtask: false,
        plan_subtask_id: None,
        delegation_engine: None,
        cancel_token: None,
        run_control: None,
        incremental_state: None,
        plan_assemble_line_release: None,
        stream_event_tx: None,
        agent_live_event_sink: None,
        approval_request_tx: None,
        ask_user_request_tx: None,
        plan_review_request_tx: None,
        mcp_manager: None,
        skill_quality_tracker: &mut skill_qt,
        discovered_skills: None,
        messaging_metrics: None,
        agent_spawner: None,
        root_agent_id: None,
        root_mailbox_slot: None,
        observability_hub: None,
        observability_session: None,
        file_journal: None,
        file_state: None,
        database_snapshot_journal: None,
        git_stash_journal: None,
        git_commit_journal: None,
        git_worktree_journal: None,
        session_state_journal: None,
        task_manager: None,
        task_notify_tx: None,
        bg_task_commands: None,
        bg_task_list_cache: None,
        bash_detach_slot: None,
        turn_index: DEFAULT_TURN_INDEX,
        pipeline_state: None,
        compaction_state: None,
        consecutive_context_window_errors: 0,
        idempotency_cache: None,
        pre_loaded_messages: None,
        append_system_prompt: None,
        session_memory_extractor: None,
        #[cfg(feature = "harness")]
        harness_sink: None,
        #[cfg(feature = "harness")]
        harness_trace: None,
        #[cfg(feature = "harness")]
        benchmark_profile: None,
    })
    .await;
    assert!(result.is_err());
    let failure = result.unwrap_err();
    assert!(failure.error.contains("500"), "got: {}", failure.error);
}

#[tokio::test]
async fn stream_chat_sse_with_tool_call_loop() {
    // Mock server: first call returns a tool call, second call returns text.
    let call_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let cc = call_count.clone();
    let app = Router::new().route(
            "/chat/turn",
            post(move || {
                let cc = cc.clone();
                async move {
                    let n = cc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let body = if n == 0 {
                        // First turn: return a tool call for bash
                        "data: {\"type\":\"session_info\",\"session_id\":\"sess-tc\",\"run_id\":\"run-sess-tc\"}\n\n\
                         data: {\"type\":\"tool_call\",\"id\":\"tc-1\",\"name\":\"bash\",\"arguments\":{\"command\":\"echo hi\"}}\n\n\
                         data: {\"type\":\"turn_complete\",\"has_tool_calls\":true}\n\n\
                         data: [DONE]\n\n"
                            .to_string()
                    } else {
                        // Second turn: return text
                        sse_text_response("Done!", "sess-tc")
                    };
                    (
                        [("content-type", "text/event-stream")],
                        body,
                    )
                }
            }),
        );
    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
    let _registry = tool_registry::ToolRegistry::new(edge_tools::all_tool_schemas());
    let mut pm = PermissionManager::new(true); // auto-approve
    let mut skill_qt = astra_skills::quality::SkillQualityTracker::new();
    let result = stream_chat_sse(ChatTurnParams {
        api: &api,
        token: "fake-token",
        auth_profile: None,
        message: "run echo hi",
        user_intent: "run echo hi",
        input_runtime_required_texts: &[],
        input_runtime_volatile_texts: &[],
        input_work_unit_observations: &[],
        semantic_query_override: None,
        session_id: None,
        model_id: None,
        model: Some("test-model"),
        provider: None,
        explain: ExplainMode::Off,
        render_md: false,
        history: &[],
        perm_manager: &mut pm,
        verbose_mode: false,
        render_policy: crate::cli::stream::stream_render::RenderPolicy::Silent,
        cli_context: None,
        recent_tools: &[],
        activated_deferred_tool_names: None,
        resume_restricted_tools: &[],
        tool_health_entries: &[],
        session_lessons: &[],
        latest_skill_diagnosis: None,
        latest_turn_quality_feedback: None,
        unified_skill_registry: astra_runtime::skills::empty_unified_registry(),
        is_plan_subtask: false,
        plan_subtask_id: None,
        delegation_engine: None,
        cancel_token: None,
        run_control: None,
        incremental_state: None,
        plan_assemble_line_release: None,
        stream_event_tx: None,
        agent_live_event_sink: None,
        approval_request_tx: None,
        ask_user_request_tx: None,
        plan_review_request_tx: None,
        mcp_manager: None,
        skill_quality_tracker: &mut skill_qt,
        discovered_skills: None,
        messaging_metrics: None,
        agent_spawner: None,
        root_agent_id: None,
        root_mailbox_slot: None,
        observability_hub: None,
        observability_session: None,
        file_journal: None,
        file_state: None,
        database_snapshot_journal: None,
        git_stash_journal: None,
        git_commit_journal: None,
        git_worktree_journal: None,
        session_state_journal: None,
        task_manager: None,
        task_notify_tx: None,
        bg_task_commands: None,
        bg_task_list_cache: None,
        bash_detach_slot: None,
        turn_index: DEFAULT_TURN_INDEX,
        pipeline_state: None,
        compaction_state: None,
        consecutive_context_window_errors: 0,
        idempotency_cache: None,
        pre_loaded_messages: None,
        append_system_prompt: None,
        session_memory_extractor: None,
        #[cfg(feature = "harness")]
        harness_sink: None,
        #[cfg(feature = "harness")]
        harness_trace: None,
        #[cfg(feature = "harness")]
        benchmark_profile: None,
    })
    .await
    .unwrap();
    assert!(
        result.full_text.starts_with("Done!"),
        "unexpected full_text: {:?}",
        result.full_text
    );
    assert!(result.tool_calls_count > 0);
    assert!(call_count.load(std::sync::atomic::Ordering::SeqCst) >= 2);
}

#[tokio::test(flavor = "current_thread")]
async fn stream_chat_sse_journals_transaction_boundaries_end_to_end() {
    let temp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(temp.path());
    #[derive(Clone)]
    struct StreamingMockState {
        call_count: std::sync::Arc<std::sync::atomic::AtomicU32>,
        tool_results: std::sync::Arc<tokio::sync::Mutex<Vec<serde_json::Value>>>,
    }

    let state = StreamingMockState {
        call_count: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
        tool_results: std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new())),
    };
    let app = Router::new()
        .route(
            "/chat/turn",
            post({
                let state = state.clone();
                move |axum::Json(request): axum::Json<serde_json::Value>| {
                    let state = state.clone();
                    async move {
                        let n = state
                            .call_count
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let body = if n == 0 {
                            let turn_chain_id = request
                                .get("turn_chain_id")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("chain-tx-e2e");
                            format!(
                                "data: {{\"type\":\"session_info\",\"session_id\":\"sess-tx-e2e\",\"run_id\":\"run-sess-tx-e2e\"}}\n\n\
                                 data: {}\n\n\
                                 data: [DONE]\n\n",
                                serde_json::json!({
                                    "type": "tool_request",
                                    "session_id": "sess-tx-e2e",
                                    "run_id": "run-sess-tx-e2e",
                                    "turn_chain_id": turn_chain_id,
                                    "request_id": "tr-tx-1",
                                    "tool": "bash",
                                    "args": {
                                        "command": "echo hi",
                                        "transaction_id": "tx-e2e",
                                        "rollback_on_failure": true
                                    }
                                })
                            )
                        } else {
                            sse_text_response("Done!", "sess-tx-e2e")
                        };
                        ([("content-type", "text/event-stream")], body)
                    }
                }
            }),
        )
        .route(
            "/tools/result",
            post({
                let state = state.clone();
                move |axum::Json(body): axum::Json<serde_json::Value>| {
                    let state = state.clone();
                    async move {
                        state.tool_results.lock().await.push(body);
                        axum::Json(serde_json::json!({ "ok": true }))
                    }
                }
            }),
        );
    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
    let _registry = tool_registry::ToolRegistry::new(edge_tools::all_tool_schemas());
    let mut pm = PermissionManager::new(true);
    let mut skill_qt = astra_skills::quality::SkillQualityTracker::new();
    let result = stream_chat_sse(ChatTurnParams {
        api: &api,
        token: "fake-token",
        auth_profile: None,
        message: "write inside a transaction",
        user_intent: "write inside a transaction",
        input_runtime_required_texts: &[],
        input_runtime_volatile_texts: &[],
        input_work_unit_observations: &[],
        semantic_query_override: None,
        session_id: None,
        model_id: None,
        model: Some("test-model"),
        provider: None,
        explain: ExplainMode::Off,
        render_md: false,
        history: &[],
        perm_manager: &mut pm,
        verbose_mode: false,
        render_policy: crate::cli::stream::stream_render::RenderPolicy::Silent,
        cli_context: None,
        recent_tools: &[],
        activated_deferred_tool_names: None,
        resume_restricted_tools: &[],
        tool_health_entries: &[],
        session_lessons: &[],
        latest_skill_diagnosis: None,
        latest_turn_quality_feedback: None,
        unified_skill_registry: astra_runtime::skills::empty_unified_registry(),
        is_plan_subtask: false,
        plan_subtask_id: None,
        delegation_engine: None,
        cancel_token: None,
        run_control: None,
        incremental_state: None,
        plan_assemble_line_release: None,
        stream_event_tx: None,
        agent_live_event_sink: None,
        approval_request_tx: None,
        ask_user_request_tx: None,
        plan_review_request_tx: None,
        mcp_manager: None,
        skill_quality_tracker: &mut skill_qt,
        discovered_skills: None,
        messaging_metrics: None,
        agent_spawner: None,
        root_agent_id: None,
        root_mailbox_slot: None,
        observability_hub: None,
        observability_session: None,
        file_journal: None,
        file_state: None,
        database_snapshot_journal: None,
        git_stash_journal: None,
        git_commit_journal: None,
        git_worktree_journal: None,
        session_state_journal: None,
        task_manager: None,
        task_notify_tx: None,
        bg_task_commands: None,
        bg_task_list_cache: None,
        bash_detach_slot: None,
        turn_index: DEFAULT_TURN_INDEX,
        pipeline_state: None,
        compaction_state: None,
        consecutive_context_window_errors: 0,
        idempotency_cache: None,
        pre_loaded_messages: None,
        append_system_prompt: None,
        session_memory_extractor: None,
        #[cfg(feature = "harness")]
        harness_sink: None,
        #[cfg(feature = "harness")]
        harness_trace: None,
        #[cfg(feature = "harness")]
        benchmark_profile: None,
    })
    .await
    .unwrap();

    assert!(
        result.full_text.starts_with("Done!"),
        "unexpected full_text: {:?}",
        result.full_text
    );
    assert!(result.tool_calls_count > 0);

    let tool_results = state.tool_results.lock().await;
    assert_eq!(tool_results.len(), 1);
    assert_eq!(tool_results[0]["request_id"].as_str(), Some("tr-tx-1"));
    drop(tool_results);

    let boundary_events: Vec<_> = session_journal::read_journal("sess-tx-e2e")
        .unwrap()
        .into_iter()
        .filter(|event| {
            matches!(
                event.event_type,
                JournalEventType::ExecutionBoundaryOpened
                    | JournalEventType::ExecutionBoundaryCommitted
            )
        })
        .collect();
    assert_eq!(boundary_events.len(), 2);
    assert_eq!(
        boundary_events[0].event_type,
        JournalEventType::ExecutionBoundaryOpened
    );
    assert_eq!(
        boundary_events[1].event_type,
        JournalEventType::ExecutionBoundaryCommitted
    );

    let opened = boundary_events[0]
        .metadata
        .as_ref()
        .and_then(|meta| meta.get("execution_boundary"))
        .expect("opened boundary metadata");
    assert_eq!(opened["kind"].as_str(), Some("tool_batch"));
    assert_eq!(opened["transaction_id"].as_str(), Some("tx-e2e"));

    let committed = boundary_events[1]
        .metadata
        .as_ref()
        .and_then(|meta| meta.get("execution_boundary"))
        .expect("committed boundary metadata");
    assert_eq!(committed["kind"].as_str(), Some("tool_batch"));
    assert_eq!(committed["transaction_id"].as_str(), Some("tx-e2e"));
}

#[tokio::test(flavor = "current_thread")]
async fn stream_chat_sse_reuses_authoritative_turn_identity_across_chat_turn_retries() {
    #[derive(Clone)]
    struct StreamingMockState {
        call_count: std::sync::Arc<std::sync::atomic::AtomicU32>,
        turn_payloads: std::sync::Arc<tokio::sync::Mutex<Vec<serde_json::Value>>>,
        tool_results: std::sync::Arc<tokio::sync::Mutex<Vec<serde_json::Value>>>,
    }

    let state = StreamingMockState {
        call_count: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
        turn_payloads: std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new())),
        tool_results: std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new())),
    };
    let app = Router::new()
        .route(
            "/chat/turn",
            post({
                let state = state.clone();
                move |axum::Json(body): axum::Json<serde_json::Value>| {
                    let state = state.clone();
                    async move {
                        let turn_chain_id = body
                            .get("turn_chain_id")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("chain-turn-identity")
                            .to_string();
                        state.turn_payloads.lock().await.push(body);
                        let n = state
                            .call_count
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let response = if n == 0 {
                            format!(
                                "data: {{\"type\":\"session_info\",\"session_id\":\"sess-turn-identity\",\"run_id\":\"run-sess-turn-identity\"}}\n\n\
                                 data: {}\n\n\
                                 data: [DONE]\n\n",
                                serde_json::json!({
                                    "type": "tool_request",
                                    "session_id": "sess-turn-identity",
                                    "run_id": "run-sess-turn-identity",
                                    "turn_chain_id": turn_chain_id,
                                    "request_id": "tr-turn-1",
                                    "tool": "bash",
                                    "args": {"command": "echo hi"}
                                })
                            )
                        } else {
                            sse_text_response("Done!", "sess-turn-identity")
                        };
                        ([("content-type", "text/event-stream")], response)
                    }
                }
            }),
        )
        .route(
            "/tools/result",
            post({
                let state = state.clone();
                move |axum::Json(body): axum::Json<serde_json::Value>| {
                    let state = state.clone();
                    async move {
                        state.tool_results.lock().await.push(body);
                        axum::Json(serde_json::json!({ "ok": true }))
                    }
                }
            }),
        );
    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
    let _registry = tool_registry::ToolRegistry::new(edge_tools::all_tool_schemas());
    let mut pm = PermissionManager::new(true);
    let mut skill_qt = astra_skills::quality::SkillQualityTracker::new();
    let result = stream_chat_sse(ChatTurnParams {
        api: &api,
        token: "fake-token",
        auth_profile: None,
        message: "review local changes",
        user_intent: "review local changes",
        input_runtime_required_texts: &[],
        input_runtime_volatile_texts: &[],
        input_work_unit_observations: &[],
        semantic_query_override: None,
        session_id: None,
        model_id: None,
        model: Some("test-model"),
        provider: None,
        explain: ExplainMode::Off,
        render_md: false,
        history: &[],
        perm_manager: &mut pm,
        verbose_mode: false,
        render_policy: crate::cli::stream::stream_render::RenderPolicy::Silent,
        cli_context: None,
        recent_tools: &[],
        activated_deferred_tool_names: None,
        resume_restricted_tools: &[],
        tool_health_entries: &[],
        session_lessons: &[],
        latest_skill_diagnosis: None,
        latest_turn_quality_feedback: None,
        unified_skill_registry: astra_runtime::skills::empty_unified_registry(),
        is_plan_subtask: false,
        plan_subtask_id: None,
        delegation_engine: None,
        cancel_token: None,
        run_control: None,
        incremental_state: None,
        plan_assemble_line_release: None,
        stream_event_tx: None,
        agent_live_event_sink: None,
        approval_request_tx: None,
        ask_user_request_tx: None,
        plan_review_request_tx: None,
        mcp_manager: None,
        skill_quality_tracker: &mut skill_qt,
        discovered_skills: None,
        messaging_metrics: None,
        agent_spawner: None,
        root_agent_id: None,
        root_mailbox_slot: None,
        observability_hub: None,
        observability_session: None,
        file_journal: None,
        file_state: None,
        database_snapshot_journal: None,
        git_stash_journal: None,
        git_commit_journal: None,
        git_worktree_journal: None,
        session_state_journal: None,
        task_manager: None,
        task_notify_tx: None,
        bg_task_commands: None,
        bg_task_list_cache: None,
        bash_detach_slot: None,
        turn_index: DEFAULT_TURN_INDEX,
        pipeline_state: None,
        compaction_state: None,
        consecutive_context_window_errors: 0,
        idempotency_cache: None,
        pre_loaded_messages: None,
        append_system_prompt: None,
        session_memory_extractor: None,
        #[cfg(feature = "harness")]
        harness_sink: None,
        #[cfg(feature = "harness")]
        harness_trace: None,
        #[cfg(feature = "harness")]
        benchmark_profile: None,
    })
    .await
    .unwrap();

    assert!(
        result.full_text.starts_with("Done!"),
        "unexpected full_text: {:?}",
        result.full_text
    );

    let payloads = state.turn_payloads.lock().await;
    assert_eq!(payloads.len(), 2, "expected two /chat/turn payloads");
    assert_eq!(payloads[0]["session_turn"], serde_json::json!(1));
    assert_eq!(payloads[1]["session_turn"], serde_json::json!(1));
    assert_eq!(payloads[0]["turn_chain_id"], payloads[1]["turn_chain_id"]);
    assert_eq!(
        payloads[0]["user_query_event_id"],
        payloads[1]["user_query_event_id"]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn stream_chat_sse_resynchronizes_stale_bridge_session_turn_once() {
    #[derive(Clone)]
    struct StreamingMockState {
        call_count: std::sync::Arc<std::sync::atomic::AtomicU32>,
        turn_payloads: std::sync::Arc<tokio::sync::Mutex<Vec<serde_json::Value>>>,
    }

    let state = StreamingMockState {
        call_count: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
        turn_payloads: std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new())),
    };
    let app = Router::new().route(
        "/chat/turn",
        post({
            let state = state.clone();
            move |axum::Json(body): axum::Json<serde_json::Value>| {
                let state = state.clone();
                async move {
                    state.turn_payloads.lock().await.push(body);
                    let n = state
                        .call_count
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let response = if n == 0 {
                        format!(
                            "data: {}\n\ndata: [DONE]\n\n",
                            serde_json::json!({
                                "type": "error",
                                "message": "explicit bridge session_turn 1 is stale for session sess-stale; expected at least 2",
                                "error_code": "bridge_session_turn_stale",
                                "metadata": {
                                    "session_id": "sess-stale",
                                    "actual_session_turn": 1,
                                    "expected_session_turn": 2,
                                    "turn_chain_id": "root-chain",
                                    "user_query_event_id": "root-query"
                                }
                            })
                        )
                    } else {
                        sse_text_response("Recovered!", "sess-stale")
                    };
                    ([("content-type", "text/event-stream")], response)
                }
            }
        }),
    );
    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
    let _registry = tool_registry::ToolRegistry::new(edge_tools::all_tool_schemas());
    let mut pm = PermissionManager::new(true);
    let mut skill_qt = astra_skills::quality::SkillQualityTracker::new();
    let result = stream_chat_sse(ChatTurnParams {
        api: &api,
        token: "fake-token",
        auth_profile: None,
        message: "continue after interrupted turn",
        user_intent: "continue after interrupted turn",
        input_runtime_required_texts: &[],
        input_runtime_volatile_texts: &[],
        input_work_unit_observations: &[],
        semantic_query_override: None,
        session_id: Some("sess-stale"),
        model_id: None,
        model: Some("test-model"),
        provider: None,
        explain: ExplainMode::Off,
        render_md: false,
        history: &[],
        perm_manager: &mut pm,
        verbose_mode: false,
        render_policy: crate::cli::stream::stream_render::RenderPolicy::Silent,
        cli_context: None,
        recent_tools: &[],
        activated_deferred_tool_names: None,
        resume_restricted_tools: &[],
        tool_health_entries: &[],
        session_lessons: &[],
        latest_skill_diagnosis: None,
        latest_turn_quality_feedback: None,
        unified_skill_registry: astra_runtime::skills::empty_unified_registry(),
        is_plan_subtask: false,
        plan_subtask_id: None,
        delegation_engine: None,
        cancel_token: None,
        run_control: None,
        incremental_state: None,
        plan_assemble_line_release: None,
        stream_event_tx: None,
        agent_live_event_sink: None,
        approval_request_tx: None,
        ask_user_request_tx: None,
        plan_review_request_tx: None,
        mcp_manager: None,
        skill_quality_tracker: &mut skill_qt,
        discovered_skills: None,
        messaging_metrics: None,
        agent_spawner: None,
        root_agent_id: None,
        root_mailbox_slot: None,
        observability_hub: None,
        observability_session: None,
        file_journal: None,
        file_state: None,
        database_snapshot_journal: None,
        git_stash_journal: None,
        git_commit_journal: None,
        git_worktree_journal: None,
        session_state_journal: None,
        task_manager: None,
        task_notify_tx: None,
        bg_task_commands: None,
        bg_task_list_cache: None,
        bash_detach_slot: None,
        turn_index: DEFAULT_TURN_INDEX,
        pipeline_state: None,
        compaction_state: None,
        consecutive_context_window_errors: 0,
        idempotency_cache: None,
        pre_loaded_messages: None,
        append_system_prompt: None,
        session_memory_extractor: None,
        #[cfg(feature = "harness")]
        harness_sink: None,
        #[cfg(feature = "harness")]
        harness_trace: None,
        #[cfg(feature = "harness")]
        benchmark_profile: None,
    })
    .await
    .unwrap();

    assert_eq!(result.full_text, "Recovered!");

    let payloads = state.turn_payloads.lock().await;
    assert_eq!(payloads.len(), 2, "expected stale conflict plus one retry");
    assert_eq!(payloads[0]["session_turn"], serde_json::json!(1));
    assert_eq!(payloads[1]["session_turn"], serde_json::json!(2));
    assert_eq!(payloads[0]["turn_chain_id"], payloads[1]["turn_chain_id"]);
    assert_eq!(
        payloads[0]["user_query_event_id"],
        payloads[1]["user_query_event_id"]
    );
}

// ── Phase 3C: Chat stream MCP integration tests ──────────────────────────────

#[tokio::test]
async fn stream_chat_sse_dispatches_mcp_tool_call() {
    let mock_server_bin = mock_mcp_server_binary();

    // Connect McpClientManager to mock server via stdio
    let mut manager = crate::mcp_client::McpClientManager::new();
    let config = crate::mcp_client::McpServerConfig {
        name: "mock".to_string(),
        transport: crate::mcp_client::Transport::Stdio {
            command: vec![mock_server_bin.to_string_lossy().to_string()],
            args: vec![],
            env: std::collections::HashMap::new(),
        },
        description: String::new(),
        enabled: true,
        retry: crate::mcp_client::RetryConfig::default(),
    };
    manager
        .connect(config)
        .await
        .expect("connect to mock MCP server");

    // Verify tools were discovered (echo, add, get_time)
    let tools = manager.all_tools();
    assert!(!tools.is_empty(), "mock MCP server should expose tools");
    let tool_names: Vec<String> = tools.iter().map(|t| t.1.name.to_string()).collect();
    assert!(
        tool_names.iter().any(|n| n.contains("echo")),
        "expected echo tool, got: {:?}",
        tool_names
    );

    // Tool name as it will appear in SSE: mcp_{server}_{tool}
    let mcp_tool_name = crate::mcp_client::sanitize_tool_name("mcp_mock_echo");

    // HTTP mock: first call returns MCP tool_call, second returns text
    let call_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let cc = call_count.clone();
    let tool_name_clone = mcp_tool_name.clone();
    let app = axum::Router::new().route(
        "/chat/turn",
        axum::routing::post(move || {
            let cc = cc.clone();
            let tn = tool_name_clone.clone();
            async move {
                let n = cc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let body = if n == 0 {
                    format!(
                        "data: {{\"type\":\"session_info\",\"session_id\":\"sess-mcp\",\"run_id\":\"run-sess-mcp\"}}\n\n\
                         data: {{\"type\":\"tool_call\",\"id\":\"mcp-1\",\"name\":\"{}\",\"arguments\":{{\"message\":\"hello from test\"}}}}\n\n\
                         data: {{\"type\":\"turn_complete\",\"has_tool_calls\":true}}\n\n\
                         data: [DONE]\n\n",
                        tn
                    )
                } else {
                    sse_text_response("MCP done!", "sess-mcp")
                };
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    body,
                )
            }
        }),
    );
    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();

    let mcp_arc = std::sync::Arc::new(tokio::sync::RwLock::new(manager));
    let mut pm = PermissionManager::new(true);
    let mut skill_qt = astra_skills::quality::SkillQualityTracker::new();

    let result = stream_chat_sse(ChatTurnParams {
        api: &api,
        token: "fake-token",
        auth_profile: None,
        message: "call echo",
        user_intent: "call echo",
        input_runtime_required_texts: &[],
        input_runtime_volatile_texts: &[],
        input_work_unit_observations: &[],
        semantic_query_override: None,
        session_id: None,
        model_id: None,
        model: Some("test-model"),
        provider: None,
        explain: ExplainMode::Off,
        render_md: false,
        history: &[],
        perm_manager: &mut pm,
        verbose_mode: false,
        render_policy: crate::cli::stream::stream_render::RenderPolicy::Silent,
        cli_context: None,
        recent_tools: &[],
        activated_deferred_tool_names: None,
        resume_restricted_tools: &[],
        tool_health_entries: &[],
        session_lessons: &[],
        latest_skill_diagnosis: None,
        latest_turn_quality_feedback: None,
        unified_skill_registry: astra_runtime::skills::empty_unified_registry(),
        is_plan_subtask: false,
        plan_subtask_id: None,
        delegation_engine: None,
        cancel_token: None,
        run_control: None,
        incremental_state: None,
        plan_assemble_line_release: None,
        stream_event_tx: None,
        agent_live_event_sink: None,
        approval_request_tx: None,
        ask_user_request_tx: None,
        plan_review_request_tx: None,
        mcp_manager: Some(mcp_arc),
        skill_quality_tracker: &mut skill_qt,
        discovered_skills: None,
        messaging_metrics: None,
        agent_spawner: None,
        root_agent_id: None,
        root_mailbox_slot: None,
        observability_hub: None,
        observability_session: None,
        file_journal: None,
        file_state: None,
        database_snapshot_journal: None,
        git_stash_journal: None,
        git_commit_journal: None,
        git_worktree_journal: None,
        session_state_journal: None,
        task_manager: None,
        task_notify_tx: None,
        bg_task_commands: None,
        bg_task_list_cache: None,
        bash_detach_slot: None,
        turn_index: DEFAULT_TURN_INDEX,
        pipeline_state: None,
        compaction_state: None,
        consecutive_context_window_errors: 0,
        idempotency_cache: None,
        pre_loaded_messages: None,
        append_system_prompt: None,
        session_memory_extractor: None,
        #[cfg(feature = "harness")]
        harness_sink: None,
        #[cfg(feature = "harness")]
        harness_trace: None,
        #[cfg(feature = "harness")]
        benchmark_profile: None,
    })
    .await
    .unwrap();

    assert!(
        result.full_text.starts_with("MCP done!"),
        "unexpected full_text: {:?}",
        result.full_text
    );
    assert!(
        result.tool_calls_count > 0,
        "expected at least one MCP tool call"
    );
    assert!(
        call_count.load(std::sync::atomic::Ordering::SeqCst) >= 2,
        "expected at least 2 HTTP rounds (tool_call + final text)"
    );
}
