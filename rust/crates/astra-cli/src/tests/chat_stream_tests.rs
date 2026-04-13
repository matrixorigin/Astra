use super::*;

// ── chat_stream (SSE agentic loop) ────────────────────────────────────

fn sse_text_response(text: &str, session_id: &str) -> String {
    format!(
        "data: {{\"type\":\"session_info\",\"session_id\":\"{session_id}\"}}\n\n\
             data: {{\"type\":\"text_delta\",\"content\":\"{text}\"}}\n\n\
             data: {{\"type\":\"text_done\",\"full_text\":\"{text}\"}}\n\n\
             data: {{\"type\":\"usage\",\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n\
             data: {{\"type\":\"turn_complete\",\"has_tool_calls\":false}}\n\n\
             data: [DONE]\n\n"
    )
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
    let mut skill_qt = astra_runtime::skills::quality::SkillQualityTracker::new();
    let skill_search = astra_core::SkillSearchSettings::default();
    let result = stream_chat_sse(ChatTurnParams {
        api: &api,
        token: "fake-token",
        message: "hi",
        session_id: None,
        model: None,
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
        database_snapshot_journal: None,
        git_stash_journal: None,
        turn_index: 0,
        evolution_service: None,
    })
    .await
    .unwrap();
    assert_eq!(result.full_text, "Hello!");
    assert_eq!(result.session_id.as_deref(), Some("sess-001"));
    assert_eq!(result.prompt_tokens, 10);
    assert_eq!(result.completion_tokens, 5);
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
    let transport = std::sync::Arc::new(astra_runtime::messaging::InProcessTransport::new());
    let tracker =
        std::sync::Arc::new(astra_runtime::server::delegation_engine::DelegationTracker::new());
    let router = std::sync::Arc::new(astra_runtime::messaging::AgentMailboxRouter::new(
        transport, tracker,
    ));
    let spawner = std::sync::Arc::new(astra_runtime::orchestration::DynamicAgentSpawner::new(
        router.clone(),
    ));
    let mut root_mailbox = Some(
        router
            .register(
                astra_runtime::messaging::AgentAddress::new("persisted-run", "main"),
                None,
            )
            .await
            .unwrap(),
    );
    let skill_search = astra_core::SkillSearchSettings::default();

    for session_id in [None, Some("sess-override")] {
        let mut pm = PermissionManager::new(true);
        let mut skill_qt = astra_runtime::skills::quality::SkillQualityTracker::new();
        let result = stream_chat_sse(ChatTurnParams {
            api: &api,
            token: "fake-token",
            message: "hi",
            session_id,
            model: None,
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
            database_snapshot_journal: None,
            git_stash_journal: None,
            turn_index: 0,
            evolution_service: None,
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
    let transport = std::sync::Arc::new(astra_runtime::messaging::InProcessTransport::new());
    let tracker =
        std::sync::Arc::new(astra_runtime::server::delegation_engine::DelegationTracker::new());
    let router = std::sync::Arc::new(astra_runtime::messaging::AgentMailboxRouter::new(
        transport, tracker,
    ));
    let spawner = std::sync::Arc::new(astra_runtime::orchestration::DynamicAgentSpawner::new(
        router.clone(),
    ));
    let skill_search = astra_core::SkillSearchSettings::default();
    let mut pm = PermissionManager::new(true);
    let mut skill_qt = astra_runtime::skills::quality::SkillQualityTracker::new();

    let result = stream_chat_sse(ChatTurnParams {
        api: &api,
        token: "fake-token",
        message: "hi",
        session_id: None,
        model: None,
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
        database_snapshot_journal: None,
        git_stash_journal: None,
        turn_index: 0,
        evolution_service: None,
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
    let transport = std::sync::Arc::new(astra_runtime::messaging::InProcessTransport::new());
    let tracker =
        std::sync::Arc::new(astra_runtime::server::delegation_engine::DelegationTracker::new());
    let router = std::sync::Arc::new(astra_runtime::messaging::AgentMailboxRouter::new(
        transport, tracker,
    ));
    let root_addr = astra_runtime::messaging::AgentAddress::new("root-run", "main");
    let worker_addr = astra_runtime::messaging::AgentAddress::new("worker-run", "worker");
    let root_mailbox = router.register(root_addr.clone(), None).await.unwrap();
    let worker_mailbox = router.register(worker_addr.clone(), None).await.unwrap();
    worker_mailbox
        .send(astra_runtime::messaging::AgentMessage::new(
            worker_addr,
            astra_runtime::messaging::MessageTarget::Direct { address: root_addr },
            astra_runtime::messaging::MessagePayload::Text {
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
    let mut skill_qt = astra_runtime::skills::quality::SkillQualityTracker::new();
    let skill_search = astra_core::SkillSearchSettings::default();
    let result = stream_chat_sse(ChatTurnParams {
        api: &api,
        token: "fake-token",
        message: "hi",
        session_id: None,
        model: None,
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
        database_snapshot_journal: None,
        git_stash_journal: None,
        turn_index: 0,
        evolution_service: None,
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
    let mut skill_qt = astra_runtime::skills::quality::SkillQualityTracker::new();
    let skill_search = astra_core::SkillSearchSettings::default();
    let result = stream_chat_sse(ChatTurnParams {
        api: &api,
        token: "fake-token",
        message: "run echo hi",
        session_id: None,
        model: None,
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
        database_snapshot_journal: None,
        git_stash_journal: None,
        turn_index: 0,
        evolution_service: None,
    })
    .await
    .unwrap();
    assert_eq!(result.full_text, "Done!");
    assert!(result.tool_calls_count > 0);
    assert!(call_count.load(std::sync::atomic::Ordering::SeqCst) >= 2);
}
