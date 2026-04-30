use super::*;
use astra_services::session_journal::{self, JournalDirGuard, JournalEventType};

// ── chat_stream (SSE agentic loop) ────────────────────────────────────

/// Build a canonical SSE response for the mock chat-turn endpoint. Exposed
/// to sibling test modules (e.g. `resume_tests`) so they don't have to
/// duplicate the payload literal.
pub(super) fn sse_text_response(text: &str, session_id: &str) -> String {
    format!(
        "data: {{\"type\":\"session_info\",\"session_id\":\"{session_id}\"}}\n\n\
             data: {{\"type\":\"text_delta\",\"content\":\"{text}\"}}\n\n\
             data: {{\"type\":\"text_done\",\"full_text\":\"{text}\"}}\n\n\
             data: {{\"type\":\"usage\",\"input_tokens\":10,\"output_tokens\":5}}\n\n\
             data: {{\"type\":\"turn_complete\",\"has_tool_calls\":false}}\n\n\
             data: [DONE]\n\n"
    )
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
    let registry = tool_registry::ToolRegistry::new(edge_tools::all_tool_schemas());
    let selector = tool_selector::TfIdfSelector::new(registry);
    let mut pm = PermissionManager::new(true);
    let mut skill_qt = astra_skills::quality::SkillQualityTracker::new();
    let skill_search = astra_core::SkillSearchSettings::default();

    let result = stream_chat_sse(ChatTurnParams {
        api: &api,
        token: "fake-token",
        auth_profile: None,
        message: "hi",
        session_id: None,
        model: None,
        provider: None,
        explain: ExplainMode::Off,
        render_md: false,
        history: &[],
        perm_manager: &mut pm,
        verbose_mode: false,
        render_policy: crate::stream_render::RenderPolicy::Silent,
        selector: &selector,
        recent_tools: &[],
        tool_health_entries: &[],
        unified_skill_registry: astra_runtime::skills::empty_unified_registry(),
        plan_only_chat: false,
        is_plan_subtask: false,
        plan_subtask_id: None,
        delegation_engine: None,
        cancel_token: None,
        plan_assemble_line_release: None,
        stream_event_tx: None,
        approval_request_tx: None,
        mcp_manager: None,
        skill_search: &skill_search,
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
        runtime_continuity: None,
        turn_index: 0,
        evolution_service: None,
        pre_loaded_messages: None,
    })
    .await
    .unwrap();

    assert_eq!(result.session_id.as_deref(), Some("sess-step-adopt"));

    let adopted_path = temp
        .path()
        .join("sess-step-adopt")
        .join("step_events.jsonl");
    let ephemeral_path = temp.path().join("ephemeral").join("step_events.jsonl");
    let adopted_events = std::fs::read_to_string(&adopted_path)
        .expect("step events should persist under adopted session");

    assert!(!adopted_events.trim().is_empty());
    assert!(adopted_events.contains("\"step_id\":\"sess-step-adopt-turn-0-step-0\""));
    assert!(
        !ephemeral_path.exists(),
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
    let registry = tool_registry::ToolRegistry::new(edge_tools::all_tool_schemas());
    let selector = tool_selector::TfIdfSelector::new(registry);
    let mut pm = PermissionManager::new(true);
    let mut skill_qt = astra_skills::quality::SkillQualityTracker::new();
    let skill_search = astra_core::SkillSearchSettings::default();
    let result = stream_chat_sse(ChatTurnParams {
        api: &api,
        token: "fake-token",
        auth_profile: None,
        message: "hi",
        session_id: None,
        model: None,
        provider: None,
        explain: ExplainMode::Off,
        render_md: false,
        history: &[],
        perm_manager: &mut pm,
        verbose_mode: false,
        render_policy: crate::stream_render::RenderPolicy::Silent,
        selector: &selector,
        recent_tools: &[],
        tool_health_entries: &[],
        unified_skill_registry: astra_runtime::skills::empty_unified_registry(),
        plan_only_chat: false,
        is_plan_subtask: false,
        plan_subtask_id: None,
        delegation_engine: None,
        cancel_token: None,
        plan_assemble_line_release: None,
        stream_event_tx: None,
        approval_request_tx: None,
        mcp_manager: None,
        skill_search: &skill_search,
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
        runtime_continuity: None,
        turn_index: 0,
        evolution_service: None,
        pre_loaded_messages: None,
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
    let registry = tool_registry::ToolRegistry::new(edge_tools::all_tool_schemas());
    let selector = tool_selector::TfIdfSelector::new(registry);
    let mut pm = PermissionManager::new(true);
    let mut skill_qt = astra_skills::quality::SkillQualityTracker::new();
    let skill_search = astra_core::SkillSearchSettings::default();

    let result = stream_chat_sse(ChatTurnParams {
        api: &api,
        token: "fake-token",
        auth_profile: None,
        message: "hi",
        session_id: Some("sess-traced"),
        model: None,
        provider: None,
        explain: ExplainMode::Off,
        render_md: false,
        history: &[],
        perm_manager: &mut pm,
        verbose_mode: false,
        render_policy: crate::stream_render::RenderPolicy::Silent,
        selector: &selector,
        recent_tools: &[],
        tool_health_entries: &[],
        unified_skill_registry: astra_runtime::skills::empty_unified_registry(),
        plan_only_chat: false,
        is_plan_subtask: false,
        plan_subtask_id: None,
        delegation_engine: None,
        cancel_token: None,
        plan_assemble_line_release: None,
        stream_event_tx: None,
        approval_request_tx: None,
        mcp_manager: None,
        skill_search: &skill_search,
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
        runtime_continuity: None,
        turn_index: 0,
        evolution_service: None,
        pre_loaded_messages: None,
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
    let registry = tool_registry::ToolRegistry::new(edge_tools::all_tool_schemas());
    let selector = tool_selector::TfIdfSelector::new(registry);
    let transport = std::sync::Arc::new(astra_messaging::InProcessTransport::new());
    let tracker =
        std::sync::Arc::new(astra_runtime::server::delegation_engine::DelegationTracker::new());
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
    let skill_search = astra_core::SkillSearchSettings::default();

    for session_id in [None, Some("sess-override")] {
        let mut pm = PermissionManager::new(true);
        let mut skill_qt = astra_skills::quality::SkillQualityTracker::new();
        let result = stream_chat_sse(ChatTurnParams {
            api: &api,
            token: "fake-token",
            auth_profile: None,
            message: "hi",
            session_id,
            model: None,
            provider: None,
            explain: ExplainMode::Off,
            render_md: false,
            history: &[],
            perm_manager: &mut pm,
            verbose_mode: false,
            render_policy: crate::stream_render::RenderPolicy::Silent,
            selector: &selector,
            recent_tools: &[],
            tool_health_entries: &[],
            unified_skill_registry: astra_runtime::skills::empty_unified_registry(),
            plan_only_chat: false,
            is_plan_subtask: false,
            plan_subtask_id: None,
            delegation_engine: None,
            cancel_token: None,
            plan_assemble_line_release: None,
            stream_event_tx: None,
            approval_request_tx: None,
            mcp_manager: None,
            skill_search: &skill_search,
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
            runtime_continuity: None,
            turn_index: 0,
            evolution_service: None,
            pre_loaded_messages: None,
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
    let registry = tool_registry::ToolRegistry::new(edge_tools::all_tool_schemas());
    let selector = tool_selector::TfIdfSelector::new(registry);
    let transport = std::sync::Arc::new(astra_messaging::InProcessTransport::new());
    let tracker =
        std::sync::Arc::new(astra_runtime::server::delegation_engine::DelegationTracker::new());
    let router = std::sync::Arc::new(astra_messaging::AgentMailboxRouter::new(transport, tracker));
    let spawner = std::sync::Arc::new(astra_runtime::orchestration::DynamicAgentSpawner::new(
        router.clone(),
    ));
    let skill_search = astra_core::SkillSearchSettings::default();
    let mut pm = PermissionManager::new(true);
    let mut skill_qt = astra_skills::quality::SkillQualityTracker::new();

    let result = stream_chat_sse(ChatTurnParams {
        api: &api,
        token: "fake-token",
        auth_profile: None,
        message: "hi",
        session_id: None,
        model: None,
        provider: None,
        explain: ExplainMode::Off,
        render_md: false,
        history: &[],
        perm_manager: &mut pm,
        verbose_mode: false,
        render_policy: crate::stream_render::RenderPolicy::Silent,
        selector: &selector,
        recent_tools: &[],
        tool_health_entries: &[],
        unified_skill_registry: astra_runtime::skills::empty_unified_registry(),
        plan_only_chat: false,
        is_plan_subtask: false,
        plan_subtask_id: None,
        delegation_engine: None,
        cancel_token: None,
        plan_assemble_line_release: None,
        stream_event_tx: None,
        approval_request_tx: None,
        mcp_manager: None,
        skill_search: &skill_search,
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
        runtime_continuity: None,
        turn_index: 0,
        evolution_service: None,
        pre_loaded_messages: None,
    })
    .await
    .unwrap();

    assert_eq!(result.full_text, "Hello!");
    assert!(
        router.list_registered_agents().await.is_empty(),
        "ephemeral root mailbox should be unregistered after the turn"
    );
}

#[tokio::test]
async fn drain_root_mailbox_into_idle_queue_collects_pending_messages() {
    let transport = std::sync::Arc::new(astra_messaging::InProcessTransport::new());
    let tracker =
        std::sync::Arc::new(astra_runtime::server::delegation_engine::DelegationTracker::new());
    let router = std::sync::Arc::new(astra_messaging::AgentMailboxRouter::new(transport, tracker));
    let root_addr = astra_messaging::AgentAddress::new("root-run", "main");
    let worker_addr = astra_messaging::AgentAddress::new("worker-run", "worker");
    let root_mailbox = router.register(root_addr.clone(), None).await.unwrap();
    let worker_mailbox = router.register(worker_addr.clone(), None).await.unwrap();
    worker_mailbox
        .send(astra_messaging::AgentMessage::new(
            worker_addr,
            astra_messaging::MessageTarget::Direct { address: root_addr },
            astra_messaging::MessagePayload::Text {
                content: "done".to_string(),
                summary: Some("worker finished".to_string()),
            },
        ))
        .await
        .unwrap();

    let mut state = ReplState::default();
    state.root_mailbox = Some(root_mailbox);

    drain_root_mailbox_into_idle_queue(&mut state);

    assert_eq!(state.pending_idle_agent_messages.len(), 1);
    assert_eq!(state.pending_idle_agent_messages[0].from.agent_id, "worker");
    assert!(
        state
            .root_mailbox
            .as_mut()
            .and_then(|mailbox| mailbox.try_recv())
            .is_none()
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
    let registry = tool_registry::ToolRegistry::new(edge_tools::all_tool_schemas());
    let selector = tool_selector::TfIdfSelector::new(registry);
    let mut pm = PermissionManager::new(true);
    let mut skill_qt = astra_skills::quality::SkillQualityTracker::new();
    let skill_search = astra_core::SkillSearchSettings::default();
    let result = stream_chat_sse(ChatTurnParams {
        api: &api,
        token: "fake-token",
        auth_profile: None,
        message: "hi",
        session_id: None,
        model: None,
        provider: None,
        explain: ExplainMode::Off,
        render_md: false,
        history: &[],
        perm_manager: &mut pm,
        verbose_mode: false,
        render_policy: crate::stream_render::RenderPolicy::Silent,
        selector: &selector,
        recent_tools: &[],
        tool_health_entries: &[],
        unified_skill_registry: astra_runtime::skills::empty_unified_registry(),
        plan_only_chat: false,
        is_plan_subtask: false,
        plan_subtask_id: None,
        delegation_engine: None,
        cancel_token: None,
        plan_assemble_line_release: None,
        stream_event_tx: None,
        approval_request_tx: None,
        mcp_manager: None,
        skill_search: &skill_search,
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
        runtime_continuity: None,
        turn_index: 0,
        evolution_service: None,
        pre_loaded_messages: None,
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
                        "data: {\"type\":\"session_info\",\"session_id\":\"sess-tc\"}\n\n\
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
    let registry = tool_registry::ToolRegistry::new(edge_tools::all_tool_schemas());
    let selector = tool_selector::TfIdfSelector::new(registry);
    let mut pm = PermissionManager::new(true); // auto-approve
    let mut skill_qt = astra_skills::quality::SkillQualityTracker::new();
    let skill_search = astra_core::SkillSearchSettings::default();
    let result = stream_chat_sse(ChatTurnParams {
        api: &api,
        token: "fake-token",
        auth_profile: None,
        message: "run echo hi",
        session_id: None,
        model: None,
        provider: None,
        explain: ExplainMode::Off,
        render_md: false,
        history: &[],
        perm_manager: &mut pm,
        verbose_mode: false,
        render_policy: crate::stream_render::RenderPolicy::Silent,
        selector: &selector,
        recent_tools: &[],
        tool_health_entries: &[],
        unified_skill_registry: astra_runtime::skills::empty_unified_registry(),
        plan_only_chat: false,
        is_plan_subtask: false,
        plan_subtask_id: None,
        delegation_engine: None,
        cancel_token: None,
        plan_assemble_line_release: None,
        stream_event_tx: None,
        approval_request_tx: None,
        mcp_manager: None,
        skill_search: &skill_search,
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
        runtime_continuity: None,
        turn_index: 0,
        evolution_service: None,
        pre_loaded_messages: None,
    })
    .await
    .unwrap();
    assert_eq!(result.full_text, "Done!");
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
                move || {
                    let state = state.clone();
                    async move {
                        let n = state
                            .call_count
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let body = if n == 0 {
                            "data: {\"type\":\"session_info\",\"session_id\":\"sess-tx-e2e\"}\n\n\
                             data: {\"type\":\"tool_request\",\"request_id\":\"tr-tx-1\",\"tool\":\"bash\",\"args\":{\"command\":\"echo hi\",\"transaction_id\":\"tx-e2e\",\"rollback_on_failure\":true}}\n\n\
                             data: [DONE]\n\n"
                                .to_string()
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
    let registry = tool_registry::ToolRegistry::new(edge_tools::all_tool_schemas());
    let selector = tool_selector::TfIdfSelector::new(registry);
    let mut pm = PermissionManager::new(true);
    let mut skill_qt = astra_skills::quality::SkillQualityTracker::new();
    let skill_search = astra_core::SkillSearchSettings::default();
    let result = stream_chat_sse(ChatTurnParams {
        api: &api,
        token: "fake-token",
        auth_profile: None,
        message: "write inside a transaction",
        session_id: None,
        model: None,
        provider: None,
        explain: ExplainMode::Off,
        render_md: false,
        history: &[],
        perm_manager: &mut pm,
        verbose_mode: false,
        render_policy: crate::stream_render::RenderPolicy::Silent,
        selector: &selector,
        recent_tools: &[],
        tool_health_entries: &[],
        unified_skill_registry: astra_runtime::skills::empty_unified_registry(),
        plan_only_chat: false,
        is_plan_subtask: false,
        plan_subtask_id: None,
        delegation_engine: None,
        cancel_token: None,
        plan_assemble_line_release: None,
        stream_event_tx: None,
        approval_request_tx: None,
        mcp_manager: None,
        skill_search: &skill_search,
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
        runtime_continuity: None,
        turn_index: 0,
        evolution_service: None,
        pre_loaded_messages: None,
    })
    .await
    .unwrap();

    assert_eq!(result.full_text, "Done!");
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
                        state.turn_payloads.lock().await.push(body);
                        let n = state
                            .call_count
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let response = if n == 0 {
                            "data: {\"type\":\"session_info\",\"session_id\":\"sess-turn-identity\"}\n\n\
                             data: {\"type\":\"tool_request\",\"request_id\":\"tr-turn-1\",\"tool\":\"bash\",\"args\":{\"command\":\"echo hi\"}}\n\n\
                             data: [DONE]\n\n"
                                .to_string()
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
    let registry = tool_registry::ToolRegistry::new(edge_tools::all_tool_schemas());
    let selector = tool_selector::TfIdfSelector::new(registry);
    let mut pm = PermissionManager::new(true);
    let mut skill_qt = astra_skills::quality::SkillQualityTracker::new();
    let skill_search = astra_core::SkillSearchSettings::default();
    let result = stream_chat_sse(ChatTurnParams {
        api: &api,
        token: "fake-token",
        auth_profile: None,
        message: "review local changes",
        session_id: None,
        model: None,
        provider: None,
        explain: ExplainMode::Off,
        render_md: false,
        history: &[],
        perm_manager: &mut pm,
        verbose_mode: false,
        render_policy: crate::stream_render::RenderPolicy::Silent,
        selector: &selector,
        recent_tools: &[],
        tool_health_entries: &[],
        unified_skill_registry: astra_runtime::skills::empty_unified_registry(),
        plan_only_chat: false,
        is_plan_subtask: false,
        plan_subtask_id: None,
        delegation_engine: None,
        cancel_token: None,
        plan_assemble_line_release: None,
        stream_event_tx: None,
        approval_request_tx: None,
        mcp_manager: None,
        skill_search: &skill_search,
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
        runtime_continuity: None,
        turn_index: 0,
        evolution_service: None,
        pre_loaded_messages: None,
    })
    .await
    .unwrap();

    assert_eq!(result.full_text, "Done!");

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
