//! End-to-end tests for messaging integration with the agentic loop.
//!
//! Verifies:
//! 1. The legacy standalone send_message schema is never injected
//! 2. Turn-start drain formats pending messages as system messages
//! 3. Execution progress stays on the live projection instead of the mailbox

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;

    use async_trait::async_trait;
    use serde_json::{Map, Value, json};

    use crate::orchestration::permission_sync::{
        InheritedPermissions, PermissionMode, PermissionRequest, PermissionRequestMessaging,
        PermissionResponse, PermissionResponseMessaging, PermissionSyncContext,
    };
    use crate::server::delegation::engine::{DelegationTracker, SubRunRecord, SubRunState};
    use crate::turn::agentic::headless_round::{
        HeadlessStderrStyle, HeadlessToolRoundCtx, NoopHeadlessTerminal,
        run_agentic_headless_tool_round,
    };
    use crate::turn::agentic_loop::host::{
        AgenticLoopHost, AgenticLoopState, HostTurnResult, run_agentic_loop_with_host,
    };
    use astra_messaging::in_process::InProcessTransport;
    use astra_messaging::router::AgentMailboxRouter;
    use astra_messaging::types::*;
    use astra_pipeline::step_protocol::InMemoryIdempotencyCache;
    use astra_pipeline::step_recorder::StepRecorder;
    use astra_text_utils::semantic_dedup::SemanticDedup;
    use astra_turn_core::chat_turn_heuristics::TaskExecutionProfile;
    use astra_turn_core::chat_turn_sse_dispatch::ChatTurnSseAccum;
    use astra_turn_core::sse_stream_host::EdgeToolExecResult;
    use astra_turn_core::turn_guard::TurnGuard;

    fn edge_runtime_environment_fields() -> Map<String, Value> {
        let registry = astra_runtime_env::ToolRegistry::builtins();
        let advertisement = astra_runtime_env::RuntimeEnvironmentAdvertisement::new(
            astra_runtime_env::RunBinding::edge_developer("/workspace/project", &registry),
        );
        Map::from_iter([(
            "runtime_environment_advertisement".to_string(),
            serde_json::to_value(advertisement).expect("serialize advertisement"),
        )])
    }

    // ── Mock Host ───────────────────────────────────────────────────────────

    struct MockHost {
        turn_results: Vec<HostTurnResult>,
        current_turn: usize,
        valid_tools: HashSet<String>,
        emitted_lines: Vec<String>,
        injected_schemas: Vec<Value>,
        communication_events: Vec<astra_messaging::AgentCommunicationEvent>,
    }

    impl MockHost {
        fn new(results: Vec<HostTurnResult>) -> Self {
            Self {
                turn_results: results,
                current_turn: 0,
                valid_tools: HashSet::new(),
                emitted_lines: Vec::new(),
                injected_schemas: Vec::new(),
                communication_events: Vec::new(),
            }
        }

        fn with_valid_tools(mut self, tools: &[&str]) -> Self {
            self.valid_tools = tools.iter().map(|s| s.to_string()).collect();
            self
        }
    }

    #[async_trait]
    impl AgenticLoopHost for MockHost {
        async fn execute_turn(
            &mut self,
            _state: &mut AgenticLoopState,
        ) -> Result<HostTurnResult, astra_core::ClassifiedError> {
            if self.turn_results.is_empty() {
                return Err(astra_core::ClassifiedError::new(
                    astra_core::ErrorKind::BudgetExhausted,
                    "no more turns",
                ));
            }
            let result = self.turn_results.remove(0);
            self.current_turn += 1;
            Ok(result)
        }

        fn emit_headless_line(&mut self, _style: HeadlessStderrStyle, line: String) {
            self.emitted_lines.push(line);
        }

        fn is_quiet(&self) -> bool {
            true
        }

        fn valid_tool_names(&self) -> &HashSet<String> {
            &self.valid_tools
        }

        fn inject_tool_schema(&mut self, schema: Value) {
            if let Some(name) = schema
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
            {
                self.valid_tools.insert(name.to_string());
            }
            self.injected_schemas.push(schema);
        }

        fn on_agent_communication(&mut self, event: astra_messaging::AgentCommunicationEvent) {
            self.communication_events.push(event);
        }
    }

    // ── Result builders ─────────────────────────────────────────────────────

    fn text_result(text: &str) -> HostTurnResult {
        HostTurnResult {
            accum: ChatTurnSseAccum {
                full_text: text.to_string(),
                has_tool_calls: false,
                has_usage: true,
                prompt_tokens: 10,
                completion_tokens: 5,
                ..ChatTurnSseAccum::default()
            },
            ttft_ms: Some(10),
            edge_tool_round: Vec::new(),
            error_kind: None,
        }
    }

    // ── State builder ───────────────────────────────────────────────────────

    fn make_state() -> AgenticLoopState {
        AgenticLoopState {
            messages: Vec::new(),
            run_transcript_capture: None,
            volatile_pending: Vec::new(),
            recent_rounds: Vec::new(),
            tool_results: Vec::new(),
            current_session_id: None,
            current_run_id: None,
            current_run_owner_generation: None,
            inference_purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
            context_manifest_pool: None,
            context_manifest_user_id: None,
            context_manifest_model_name: None,
            runtime_manifest: None,
            recursion_depth: 0,
            final_text: String::new(),
            final_text_streamed: false,
            final_output_ready_notified: false,
            total_prompt: 0,
            total_completion: 0,
            total_cache_read: 0,
            total_cache_creation: 0,
            total_tool_calls: 0,
            total_observation_tool_calls: 0,
            tool_ledger_receipt: Default::default(),
            has_any_usage: false,
            max_turns: 10,
            remaining_turns: 10,
            agentic_turn_budget: TaskExecutionProfile::default().agentic_turn_budget,
            budget_is_explicit: false,
            budget_policy: None,
            current_round_index: 0,
            llm_rounds_completed: 0,
            last_request_message_count: None,
            turn_guard: TurnGuard::new(),
            restricted_tools: HashSet::new(),
            boosted_tools: HashSet::new(),
            widen_selection_pending: false,
            step_recorder: StepRecorder::new("test-user", "test-session", "test-task"),
            idempotency_cache: InMemoryIdempotencyCache::new(),
            semantic_dedup: SemanticDedup::new(0.95),
            call_counts: HashMap::new(),
            max_identical_tool_calls: astra_config::runtime_config::RuntimeConfig::load()
                .tool_policy
                .effective_max_identical_calls(),
            max_tools_per_turn: 15,
            repeated_cache_hit_suppression: 3,
            max_consecutive_empty_name: 3,
            stall: Default::default(),
            telemetry: Default::default(),
            skills: Default::default(),
            hooks: Default::default(),
            cancellation: Default::default(),
            messaging: Default::default(),
            user_intents: Default::default(),
            error_recovery: Default::default(),
            provider_adaptation: Default::default(),
            run_control: None,
            pipeline_session: None,
            message: "test query".to_string(),
            user_intent: "test query".to_string(),
            has_prior_assistant_turn: false,
            recent_tools: Vec::new(),
            activated_deferred_tool_names: Vec::new(),
            turn_intent: None,
            task_profile: TaskExecutionProfile::default(),
            last_finish_reason: None,
            last_turn_policy: crate::turn::agentic_loop::host::TurnInteractionPolicy::default(),
            api: astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            api_token: String::new(),
            delegation_engine: None,
            delegations_this_turn: 0,
            delegation_chain: Vec::new(),
            self_agent_id: "main".to_string(),
            project_context: None,
            checkpoint_gate: None,
            last_llm_context_manifest_trace: None,
            rate_limit_cooldown: Default::default(),
            data_snapshot_provider: None,
            last_composite_snapshot: None,
            last_measured_prompt_tokens: None,
            consecutive_context_window_errors: 0,
            compaction_effectiveness: Default::default(),
            pinned_tool_schema_tokens: 0,
            sticky_tool_schemas: Vec::new(),
            max_turn_input_tokens: 0,
            budget_wrapup_injected: false,
            context_compression_triggered: false,
            canonical_rewrite_state: Default::default(),
            provider_canonical_wal_base: None,
            provider_canonical_wal_head: None,
            budget_wrapup_ignored_rounds: 0,
            compact_tier_applied: astra_turn_core::compaction_types::CompactionTier::Normal,
            skill_produced_output: false,
            thinking: astra_turn_core::thinking_config::ThinkingConfig::Off,
            permission_context: None,
            permission_handler: None,
            tactical_adapter: None,
            step_signal_collector: None,
            tool_budget_override: None,
            recent_tactical_actions: Vec::new(),
            runtime_tool_executor: None,
            interruption: None,
            session_facts: Default::default(),
            memory_extraction_service: None,
            session_memory_state: Default::default(),
            compact_strategy: Default::default(),
            approval_overrides: None,
            confidence_trend: Default::default(),
            last_confidence_diagnosis: None,
            session_turn: 0,
            canonical_turn_chain_id: None,
            root_user_query_event_id: None,
            turn_event_buffer: None,
            harness: crate::turn::harness_adapter::HarnessSlot::empty(),
            observation_journal: Default::default(),
        }
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    async fn setup_two_agents() -> (
        Arc<AgentMailboxRouter>,
        astra_messaging::router::AgentMailbox,
        astra_messaging::router::AgentMailbox,
        Arc<DelegationTracker>,
    ) {
        let transport = Arc::new(InProcessTransport::new());
        let dt = Arc::new(DelegationTracker::new());
        let router = Arc::new(AgentMailboxRouter::new(transport, dt.clone()));

        let parent_addr = AgentAddress::new("run-parent", "orchestrator");
        let parent_mb = router.register(parent_addr, None).await.unwrap();

        let child_addr = AgentAddress::new("run-child-0", "worker");
        dt.record_sub_run(SubRunRecord {
            run_id: "run-child-0".into(),
            parent_run_id: "run-parent".into(),
            delegation_id: "del-e2e".into(),
            agent_id: "worker".into(),
            depth: 1,
            state: SubRunState::Created,
            retry_of: None,
        })
        .await;

        let child_mb = router
            .register(child_addr, Some("del-e2e".into()))
            .await
            .unwrap();

        (router, parent_mb, child_mb, dt)
    }

    // ── Tests ───────────────────────────────────────────────────────────────

    // send_message is now an action in the consolidated `agent` tool.
    // No separate schema injection is needed — the agent schema is always present.
    #[tokio::test]
    async fn preamble_no_longer_injects_send_message_schema() {
        let (_router, _parent, child_mb, _dt) = setup_two_agents().await;

        let mut host = MockHost::new(vec![text_result("done")]);
        let mut state = make_state();
        state.messaging.mailbox = Some(child_mb);

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());

        // send_message is no longer a separate injected schema — it's an
        // action in the always-present `agent` tool.
        let has_send_msg = host.injected_schemas.iter().any(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                == Some("send_message")
        });
        assert!(
            !has_send_msg,
            "send_message should NOT be separately injected (it's an agent action now)"
        );
    }

    #[tokio::test]
    async fn no_schema_injection_without_mailbox() {
        let mut host = MockHost::new(vec![text_result("done")]);
        let mut state = make_state();
        // mailbox is None

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());

        let has_send_msg = host.injected_schemas.iter().any(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                == Some("send_message")
        });
        assert!(!has_send_msg, "no send_message schema without mailbox");
    }

    #[tokio::test]
    async fn drain_injects_pending_messages_as_system_msg() {
        let (_router, parent_mb, mut child_mb, _dt) = setup_two_agents().await;

        // Parent sends a message to child BEFORE the loop starts.
        let msg = AgentMessage::new(
            parent_mb.address.clone(),
            MessageTarget::Direct {
                address: child_mb.address.clone(),
            },
            MessagePayload::Text {
                content: "Please focus on auth module.".into(),
                summary: None,
            },
        );
        parent_mb.send(msg).await.unwrap();

        // Verify child has a pending message.
        let pending = child_mb.try_recv();
        assert!(pending.is_some(), "child should have a pending message");

        // Put it back by sending again (try_recv consumed it).
        let msg2 = AgentMessage::new(
            parent_mb.address.clone(),
            MessageTarget::Direct {
                address: child_mb.address.clone(),
            },
            MessagePayload::Text {
                content: "Focus on auth module.".into(),
                summary: None,
            },
        );
        parent_mb.send(msg2).await.unwrap();

        let mut host = MockHost::new(vec![text_result("Working on auth.")]);
        let mut state = make_state();
        state.messaging.mailbox = Some(child_mb);

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());

        // Post-Task #45: drained mailbox rides the structured volatile
        // lane (Kind::Mailbox) instead of state.messages.
        let has_mailbox_msg = state.volatile_pending.iter().any(|inj| {
            inj.payload
                .as_str()
                .is_some_and(|text| text.contains("📬") && text.contains("orchestrator"))
        });
        assert!(
            has_mailbox_msg,
            "should have mailbox injection in volatile_pending: {:?}",
            state.volatile_pending,
        );
        assert_eq!(host.communication_events.len(), 1);
        assert_eq!(
            host.communication_events[0].payload_kind,
            astra_turn_types::AgentCommunicationPayloadKind::Text
        );
    }

    #[tokio::test]
    async fn mailbox_progress_is_consumed_without_polluting_model_or_durable_evidence() {
        let (_router, parent_mb, child_mb, _dt) = setup_two_agents().await;
        parent_mb
            .send(AgentMessage::new(
                parent_mb.address.clone(),
                MessageTarget::Direct {
                    address: child_mb.address.clone(),
                },
                MessagePayload::Progress {
                    turn_index: 4,
                    tool_calls: 3,
                    status: "working".into(),
                    detail: Some("inspecting storage".into()),
                },
            ))
            .await
            .unwrap();

        let mut host = MockHost::new(vec![text_result("Still working.")]);
        let mut state = make_state();
        state.messaging.mailbox = Some(child_mb);

        run_agentic_loop_with_host(&mut host, &mut state)
            .await
            .expect("progress must not disrupt the turn");

        assert!(
            state.volatile_pending.iter().all(|injection| !matches!(
                injection.kind,
                crate::turn::agentic_loop::host::VolatileKind::Mailbox
            )),
            "transient execution progress must not enter the model boundary"
        );
        assert!(
            host.communication_events.is_empty(),
            "transient execution progress must not become durable communication evidence"
        );
    }

    #[tokio::test]
    async fn tool_turn_progress_uses_live_projection_without_duplicate_mailbox_message() {
        let (_router, mut parent_mb, child_mb, _dt) = setup_two_agents().await;

        let progress = Arc::new(crate::orchestration::ProgressBroadcaster::default());
        let mut progress_rx = progress.subscribe();

        // Tool turn → should send progress to parent.
        let edge_tools = vec![EdgeToolExecResult {
            request_id: "call-read-1".into(),
            tool: "read_file".into(),
            args: json!({"path": "/tmp/x.txt"}),
            output: "content".into(),
            tool_result_fields: Some(edge_runtime_environment_fields()),
            status: "completed".into(),
            duration_ms: 5,
        }];

        let tool_calls = vec![json!({
            "id": "call-read-1",
            "type": "function",
            "function": {
                "name": "read_file",
                "arguments": r#"{"path": "/tmp/x.txt"}"#
            }
        })];

        let mut host = MockHost::new(vec![
            HostTurnResult {
                accum: ChatTurnSseAccum {
                    has_tool_calls: true,
                    has_usage: true,
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    tool_calls,
                    ..ChatTurnSseAccum::default()
                },
                ttft_ms: Some(10),
                edge_tool_round: edge_tools,
                error_kind: None,
            },
            text_result("Read the file."),
        ])
        .with_valid_tools(&["read_file"]);

        let mut state = make_state();
        state.messaging.mailbox = Some(child_mb);
        state.messaging.progress_emitter = Some(progress.for_agent_with_run_context(
            "worker".into(),
            "run-child-0".into(),
            "run-parent".into(),
            None,
        ));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());

        assert!(
            parent_mb.try_recv().is_none(),
            "live execution progress must not be duplicated into the durable mailbox"
        );
        let mut observed_turn_completion = false;
        while let Ok(event) = progress_rx.try_recv() {
            observed_turn_completion |= matches!(
                event.event_type,
                crate::orchestration::ProgressEventType::TurnCompleted { .. }
            );
        }
        assert!(
            observed_turn_completion,
            "the dedicated live lane must retain progress"
        );
    }

    #[tokio::test]
    async fn parent_loop_handles_permission_request_before_llm_injection() {
        let (_router, parent_mb, mut child_mb, _dt) = setup_two_agents().await;

        let request = PermissionRequest::new("bash", json!({"command": "echo hi"}))
            .to_message(&child_mb.address, &parent_mb.address)
            .with_correlation("perm-1");
        child_mb.send(request).await.unwrap();

        let mut host = MockHost::new(vec![text_result("Handled request.")]);
        let mut state = make_state();
        state.messaging.mailbox = Some(parent_mb);
        state.permission_context = Some(PermissionSyncContext::shared_root(PermissionMode::Auto));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());

        let response = child_mb
            .try_recv()
            .expect("child should receive permission response");
        match &response.payload {
            MessagePayload::Response { accepted, data, .. } => {
                assert!(*accepted);
                let parsed = PermissionResponse::from_message_payload(
                    data.as_ref().expect("response should include payload"),
                )
                .expect("response payload should parse");
                assert!(parsed.approved);
            }
            other => panic!("expected permission response, got {other:?}"),
        }

        let leaked_request = state.messages.iter().any(|m| {
            m.get("content")
                .and_then(Value::as_str)
                .is_some_and(|c| c.contains("ToolPermission"))
        });
        assert!(
            !leaked_request,
            "permission requests should be handled before LLM context injection"
        );
    }

    #[tokio::test]
    async fn child_tool_round_records_blocked_permission_denial() {
        let tool_calls = vec![json!({
            "id": "call-bash-perm",
            "type": "function",
            "function": {
                "name": "bash",
                "arguments": r#"{"command": "echo hi"}"#
            }
        })];

        let permission_context = PermissionSyncContext::shared(InheritedPermissions {
            mode: PermissionMode::Prompt,
            allow_rules: vec![],
            deny_rules: vec![],
            ask_rules: vec![],
            allowed_tools: Some(HashSet::from(["view".to_string()])),
            is_background: false,
            ..Default::default()
        });
        let mut messages = Vec::new();
        let mut tool_results = Vec::new();
        let valid_tool_names = HashSet::from(["bash".to_string()]);
        let mut restricted_tools = HashSet::new();
        let mut turn_guard = TurnGuard::new();
        let mut step_recorder = StepRecorder::new("test-user", "test-session", "perm-headless");
        let mut idempotency_cache = InMemoryIdempotencyCache::new();
        let mut semantic_dedup = SemanticDedup::new(0.95);
        let mut tool_call_records = Vec::new();
        let tool_event_hooks = crate::skills::hooks::ToolEventHookRegistry::default();
        let mut term = NoopHeadlessTerminal;
        let edge_callback_outputs = std::collections::HashMap::new();
        let edge_tool_round: Vec<EdgeToolExecResult> = Vec::new();

        run_agentic_headless_tool_round(HeadlessToolRoundCtx {
            turn_index: 0,
            session_turn: 1,
            quiet: true,
            api: &astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            token: "",
            current_user_id: None,
            current_session_id: None,
            current_run_id: None,
            current_turn_chain_id: None,
            durable_dispatch_admission: None,
            tool_calls: &tool_calls,
            edge_tool_round: &edge_tool_round,
            reasoning_content: "",
            reasoning_signature: "",
            edge_callback_outputs: &edge_callback_outputs,
            messages: &mut messages,
            tool_results: &mut tool_results,
            valid_tool_names: &valid_tool_names,
            deferred_tool_names: &std::collections::HashSet::new(),
            restricted_tools: &mut restricted_tools,
            turn_guard: &mut turn_guard,
            step_recorder: &mut step_recorder,
            idempotency_cache: &mut idempotency_cache,
            semantic_dedup: &mut semantic_dedup,
            call_counts: &mut std::collections::HashMap::new(),
            max_identical_calls: 2,
            max_tools_per_turn: 15,
            repeated_cache_hit_suppression: 3,
            max_consecutive_empty_name: 3,
            tool_call_records: &mut tool_call_records,
            tool_event_hooks: &tool_event_hooks,
            term: &mut term,
            mailbox: None,
            permission_context: Some(&permission_context),
            progress_emitter: None,
            pre_resolved_results: &[],
            runtime_tool_executor: None,
            turn_start: None,
            llm_round: 0,
            plan_mode_active: false,
        })
        .await;

        assert_eq!(tool_results.len(), 1);
        assert_eq!(tool_call_records.len(), 1);
        // The agent_type's `allowed_tools` is now treated as the
        // sub-agent's authorised surface — the parent's spawn action
        // declared what tools the child gets, so the gate denies
        // bash up-front with "not in allowlist" instead of trying to
        // ask a parent that was never registered (the pre-fix path,
        // which would deny with "no parent available"). Same outcome
        // — denial — but the new message is actionable: the child
        // agent's own allowlist is the rule.
        assert_eq!(
            tool_call_records[0].error.as_deref(),
            Some("blocked_tool: Tool 'bash' not in allowed tools list"),
        );
    }

    #[tokio::test]
    async fn plan_mode_blocks_mutating_tools_before_headless_protocol_fallback() {
        let tool_calls = vec![json!({
            "id": "call-write-plan",
            "type": "function",
            "function": {
                "name": "write_file",
                "arguments": r#"{"path":"tmp.txt","content":"hello"}"#
            }
        })];

        let mut messages = Vec::new();
        let mut tool_results = Vec::new();
        let valid_tool_names = HashSet::from(["write_file".to_string()]);
        let mut restricted_tools = HashSet::new();
        let mut turn_guard = TurnGuard::new();
        let mut step_recorder = StepRecorder::new("test-user", "test-session", "plan-mode-write");
        let mut idempotency_cache = InMemoryIdempotencyCache::new();
        let mut semantic_dedup = SemanticDedup::new(0.95);
        let mut tool_call_records = Vec::new();
        let tool_event_hooks = crate::skills::hooks::ToolEventHookRegistry::default();
        let mut term = NoopHeadlessTerminal;
        let edge_callback_outputs = std::collections::HashMap::new();
        let edge_tool_round: Vec<EdgeToolExecResult> = Vec::new();

        run_agentic_headless_tool_round(HeadlessToolRoundCtx {
            turn_index: 0,
            session_turn: 1,
            quiet: true,
            api: &astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            token: "",
            current_user_id: None,
            current_session_id: None,
            current_run_id: None,
            current_turn_chain_id: None,
            durable_dispatch_admission: None,
            tool_calls: &tool_calls,
            edge_tool_round: &edge_tool_round,
            reasoning_content: "",
            reasoning_signature: "",
            edge_callback_outputs: &edge_callback_outputs,
            messages: &mut messages,
            tool_results: &mut tool_results,
            valid_tool_names: &valid_tool_names,
            deferred_tool_names: &std::collections::HashSet::new(),
            restricted_tools: &mut restricted_tools,
            turn_guard: &mut turn_guard,
            step_recorder: &mut step_recorder,
            idempotency_cache: &mut idempotency_cache,
            semantic_dedup: &mut semantic_dedup,
            call_counts: &mut std::collections::HashMap::new(),
            max_identical_calls: 2,
            max_tools_per_turn: 15,
            repeated_cache_hit_suppression: 3,
            max_consecutive_empty_name: 3,
            tool_call_records: &mut tool_call_records,
            tool_event_hooks: &tool_event_hooks,
            term: &mut term,
            mailbox: None,
            permission_context: None,
            progress_emitter: None,
            pre_resolved_results: &[],
            runtime_tool_executor: None,
            turn_start: None,
            llm_round: 0,
            plan_mode_active: true,
        })
        .await;

        assert_eq!(tool_results.len(), 1);
        assert_eq!(tool_call_records.len(), 1);
        let record_error = tool_call_records[0].error.as_deref().unwrap_or("");
        assert!(
            record_error
                .contains("blocked_tool: tool 'write_file' is blocked while plan mode is active"),
            "unexpected journal error: {record_error}"
        );
        let tool_message = messages
            .iter()
            .find(|message| message["role"] == "tool")
            .expect("expected a tool result message");
        let body = tool_message["content"].as_str().unwrap_or("");
        assert!(
            body.contains("Permission denied for tool 'write_file'"),
            "unexpected tool body: {body}"
        );
        assert!(
            !body.contains("headless edge protocol"),
            "plan mode should short-circuit before protocol fallback: {body}"
        );
    }

    /// Provider tool batches are canonical authority input. Missing call ids
    /// fail the whole batch closed instead of inventing execution identity.
    #[tokio::test]
    async fn empty_tool_call_id_rejects_provider_batch_before_execution() {
        let tool_calls = vec![json!({
            "id": "",
            "type": "function",
            "function": {
                "name": "bash",
                "arguments": r#"{"command":"echo hi"}"#
            }
        })];

        let mut messages = Vec::new();
        let mut tool_results = Vec::new();
        let valid_tool_names = HashSet::from(["bash".to_string()]);
        let mut restricted_tools = HashSet::new();
        let mut turn_guard = TurnGuard::new();
        let mut step_recorder = StepRecorder::new("test-user", "test-session", "empty-id");
        let mut idempotency_cache = InMemoryIdempotencyCache::new();
        let mut semantic_dedup = SemanticDedup::new(0.95);
        let mut tool_call_records = Vec::new();
        let tool_event_hooks = crate::skills::hooks::ToolEventHookRegistry::default();
        let mut term = NoopHeadlessTerminal;
        let edge_callback_outputs = std::collections::HashMap::new();
        let edge_tool_round: Vec<EdgeToolExecResult> = Vec::new();

        let outcome = run_agentic_headless_tool_round(HeadlessToolRoundCtx {
            turn_index: 0,
            session_turn: 1,
            quiet: true,
            api: &astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            token: "",
            current_user_id: None,
            current_session_id: None,
            current_run_id: None,
            current_turn_chain_id: None,
            durable_dispatch_admission: None,
            tool_calls: &tool_calls,
            edge_tool_round: &edge_tool_round,
            reasoning_content: "",
            reasoning_signature: "",
            edge_callback_outputs: &edge_callback_outputs,
            messages: &mut messages,
            tool_results: &mut tool_results,
            valid_tool_names: &valid_tool_names,
            deferred_tool_names: &std::collections::HashSet::new(),
            restricted_tools: &mut restricted_tools,
            turn_guard: &mut turn_guard,
            step_recorder: &mut step_recorder,
            idempotency_cache: &mut idempotency_cache,
            semantic_dedup: &mut semantic_dedup,
            call_counts: &mut std::collections::HashMap::new(),
            max_identical_calls: 2,
            max_tools_per_turn: 15,
            repeated_cache_hit_suppression: 3,
            max_consecutive_empty_name: 3,
            tool_call_records: &mut tool_call_records,
            tool_event_hooks: &tool_event_hooks,
            term: &mut term,
            mailbox: None,
            permission_context: None,
            progress_emitter: None,
            pre_resolved_results: &[],
            runtime_tool_executor: None,
            turn_start: None,
            llm_round: 0,
            plan_mode_active: false,
        })
        .await;

        assert!(messages.is_empty());
        assert!(tool_results.is_empty());
        assert!(tool_call_records.is_empty());
        assert!(
            outcome
                .action_admission_error
                .as_deref()
                .is_some_and(|error| error.contains("tool call id is missing")),
            "unexpected admission result: {outcome:?}"
        );
    }

    /// Regression test for session 4a9c9697: skill + non-skill tool calls in
    /// the same turn caused tool results to appear BEFORE the assistant message
    /// in the conversation history. kimi-k2.5 (and other strict APIs) rejected
    /// this with 400 "tool_call_id is not found".
    ///
    /// pre_resolved_results ensures skill results are injected AFTER the
    /// assistant message: assistant(tool_calls) → tool(skill) → tool(executed).
    #[tokio::test]
    async fn pre_resolved_results_injected_after_assistant_message() {
        let tool_calls = vec![
            json!({
                "id": "call_skill",
                "type": "function",
                "function": { "name": "skill", "arguments": r#"{"skill_name":"review"}"# }
            }),
            json!({
                "id": "call_bash",
                "type": "function",
                "function": { "name": "bash", "arguments": r#"{"command":"echo hi"}"# }
            }),
        ];

        let mut messages = Vec::new();
        let mut tool_results = Vec::new();
        let valid_tool_names = HashSet::from(["bash".to_string(), "skill".to_string()]);
        let mut restricted_tools = HashSet::new();
        let mut turn_guard = TurnGuard::new();
        let mut step_recorder = StepRecorder::new("test-user", "test-session", "pre-resolved");
        let mut idempotency_cache = InMemoryIdempotencyCache::new();
        let mut semantic_dedup = SemanticDedup::new(0.95);
        let mut tool_call_records = Vec::new();
        let tool_event_hooks = crate::skills::hooks::ToolEventHookRegistry::default();
        let mut term = NoopHeadlessTerminal;
        let edge_callback_outputs = std::collections::HashMap::new();
        let edge_tool_round: Vec<EdgeToolExecResult> = Vec::new();

        // Simulate: skill interception resolved call_skill before headless round
        let pre_resolved = vec![(
            "call_skill".to_string(),
            "Skill instructions here".to_string(),
        )];

        run_agentic_headless_tool_round(HeadlessToolRoundCtx {
            turn_index: 0,
            session_turn: 1,
            quiet: true,
            api: &astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            token: "",
            current_user_id: None,
            current_session_id: None,
            current_run_id: None,
            current_turn_chain_id: None,
            durable_dispatch_admission: None,
            tool_calls: &tool_calls,
            edge_tool_round: &edge_tool_round,
            reasoning_content: "",
            reasoning_signature: "",
            edge_callback_outputs: &edge_callback_outputs,
            messages: &mut messages,
            tool_results: &mut tool_results,
            valid_tool_names: &valid_tool_names,
            deferred_tool_names: &std::collections::HashSet::new(),
            restricted_tools: &mut restricted_tools,
            turn_guard: &mut turn_guard,
            step_recorder: &mut step_recorder,
            idempotency_cache: &mut idempotency_cache,
            semantic_dedup: &mut semantic_dedup,
            call_counts: &mut std::collections::HashMap::new(),
            max_identical_calls: 2,
            max_tools_per_turn: 15,
            repeated_cache_hit_suppression: 3,
            max_consecutive_empty_name: 3,
            tool_call_records: &mut tool_call_records,
            tool_event_hooks: &tool_event_hooks,
            term: &mut term,
            mailbox: None,
            permission_context: None,
            progress_emitter: None,
            pre_resolved_results: &pre_resolved,
            runtime_tool_executor: None,
            turn_start: None,
            llm_round: 0,
            plan_mode_active: false,
        })
        .await;

        // messages[0] = assistant with tool_calls [call_skill, call_bash]
        // messages[1] = tool result for call_skill (pre-resolved)
        // messages[2] = tool result for call_bash (headless-executed)
        assert!(
            messages.len() >= 3,
            "expected assistant + tool results, got {}: {:#?}",
            messages.len(),
            messages
                .iter()
                .map(|m| {
                    let role = m["role"].as_str().unwrap_or("?");
                    let tcid = m.get("tool_call_id").and_then(|v| v.as_str()).unwrap_or("");
                    format!("{}({})", role, tcid)
                })
                .collect::<Vec<_>>()
        );
        assert_eq!(
            messages[0]["role"], "assistant",
            "first message must be assistant"
        );
        assert_eq!(
            messages[0]["tool_calls"].as_array().map(|a| a.len()),
            Some(2),
            "assistant must have both tool_calls"
        );

        // Pre-resolved skill result must come right after assistant
        assert_eq!(messages[1]["role"], "tool");
        assert_eq!(messages[1]["tool_call_id"], "call_skill");
        assert_eq!(messages[1]["content"], "Skill instructions here");

        // Headless-executed bash result comes after
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["tool_call_id"], "call_bash");
    }

    /// Regression test for session ccbd3a48: when skill defer intercepts ALL
    /// tool calls, effective_tool_calls becomes empty. If the headless round
    /// only sees the empty list, it falls back to the edge-only path and
    /// builds the assistant message with `edge-N` ids — but pre_resolved
    /// results use the original server-assigned ids. The mismatch causes
    /// kimi-k2.5 to reject with 400 "tool_call_id is not found".
    ///
    /// Fix: pass the full (pre-interception) tool_calls to the headless round
    /// so the assistant message always uses server-assigned ids.
    #[tokio::test]
    async fn all_tools_pre_resolved_still_uses_server_ids_in_assistant_message() {
        // Simulate: server returned 2 tool_calls, but ALL were intercepted
        // (e.g. skill + deferred). Edge round has matching results.
        let tool_calls = vec![
            json!({
                "id": "skill:0",
                "type": "function",
                "function": { "name": "skill", "arguments": r#"{"skill_name":"review"}"# }
            }),
            json!({
                "id": "read_file:1",
                "type": "function",
                "function": { "name": "read_file", "arguments": r#"{"path":"src/main.rs"}"# }
            }),
        ];

        let mut messages = Vec::new();
        let mut tool_results = Vec::new();
        let valid_tool_names = HashSet::from([
            "bash".to_string(),
            "skill".to_string(),
            "read_file".to_string(),
        ]);
        let mut restricted_tools = HashSet::new();
        let mut turn_guard = TurnGuard::new();
        let mut step_recorder = StepRecorder::new("test-user", "test-session", "all-pre-resolved");
        let mut idempotency_cache = InMemoryIdempotencyCache::new();
        let mut semantic_dedup = SemanticDedup::new(0.95);
        let mut tool_call_records = Vec::new();
        let tool_event_hooks = crate::skills::hooks::ToolEventHookRegistry::default();
        let mut term = NoopHeadlessTerminal;
        let edge_callback_outputs = std::collections::HashMap::new();
        let edge_tool_round: Vec<EdgeToolExecResult> = Vec::new();

        // ALL tool calls were pre-resolved by upstream (skill + defer)
        let pre_resolved = vec![
            ("skill:0".to_string(), "Skill instructions".to_string()),
            ("read_file:1".to_string(), "file contents".to_string()),
        ];

        run_agentic_headless_tool_round(HeadlessToolRoundCtx {
            turn_index: 0,
            session_turn: 1,
            quiet: true,
            api: &astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            token: "",
            current_user_id: None,
            current_session_id: None,
            current_run_id: None,
            current_turn_chain_id: None,
            durable_dispatch_admission: None,
            tool_calls: &tool_calls,
            edge_tool_round: &edge_tool_round,
            reasoning_content: "",
            reasoning_signature: "",
            edge_callback_outputs: &edge_callback_outputs,
            messages: &mut messages,
            tool_results: &mut tool_results,
            valid_tool_names: &valid_tool_names,
            deferred_tool_names: &std::collections::HashSet::new(),
            restricted_tools: &mut restricted_tools,
            turn_guard: &mut turn_guard,
            step_recorder: &mut step_recorder,
            idempotency_cache: &mut idempotency_cache,
            semantic_dedup: &mut semantic_dedup,
            call_counts: &mut std::collections::HashMap::new(),
            max_identical_calls: 2,
            max_tools_per_turn: 15,
            repeated_cache_hit_suppression: 3,
            max_consecutive_empty_name: 3,
            tool_call_records: &mut tool_call_records,
            tool_event_hooks: &tool_event_hooks,
            term: &mut term,
            mailbox: None,
            permission_context: None,
            progress_emitter: None,
            pre_resolved_results: &pre_resolved,
            runtime_tool_executor: None,
            turn_start: None,
            llm_round: 0,
            plan_mode_active: false,
        })
        .await;

        // Assistant message must use server-assigned ids, not edge-N
        assert_eq!(messages[0]["role"], "assistant");
        let tc_ids: Vec<&str> = messages[0]["tool_calls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tc| tc["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            tc_ids,
            vec!["skill:0", "read_file:1"],
            "assistant tool_calls must use server-assigned ids, not edge-N"
        );

        // Tool results must use matching ids
        assert_eq!(messages[1]["tool_call_id"], "skill:0");
        assert_eq!(messages[2]["tool_call_id"], "read_file:1");

        // No edge-N ids anywhere in messages
        for (i, m) in messages.iter().enumerate() {
            if let Some(tcid) = m.get("tool_call_id").and_then(Value::as_str) {
                assert!(
                    !tcid.starts_with("edge-"),
                    "message[{i}] has orphan edge id: {tcid}"
                );
            }
        }
    }

    /// Test: pre_resolved skill result + edge tool execution in the same round.
    /// Verifies that edge tools are correctly matched to server tool_calls
    /// while pre-resolved results are injected without duplication.
    #[tokio::test]
    async fn pre_resolved_mixed_with_edge_tool_execution() {
        let tool_calls = vec![
            json!({
                "id": "skill:0",
                "type": "function",
                "function": { "name": "skill", "arguments": r#"{"skill_name":"review"}"# }
            }),
            json!({
                "id": "grep:1",
                "type": "function",
                "function": { "name": "grep", "arguments": r#"{"pattern":"TODO"}"# }
            }),
        ];

        // Edge round has the grep result (executed at edge during SSE)
        let edge_tool_round = vec![EdgeToolExecResult {
            request_id: String::new(),
            tool: "grep".to_string(),
            args: json!({"pattern": "TODO"}),
            output: "src/main.rs:10: // TODO fix".to_string(),
            tool_result_fields: Some(edge_runtime_environment_fields()),
            status: "completed".to_string(),
            duration_ms: 50,
        }];

        let mut messages = Vec::new();
        let mut tool_results = Vec::new();
        let valid_tool_names = HashSet::from(["skill".to_string(), "grep".to_string()]);
        let mut restricted_tools = HashSet::new();
        let mut turn_guard = TurnGuard::new();
        let mut step_recorder = StepRecorder::new("test-user", "test-session", "mixed-edge");
        let mut idempotency_cache = InMemoryIdempotencyCache::new();
        let mut semantic_dedup = SemanticDedup::new(0.95);
        let mut tool_call_records = Vec::new();
        let tool_event_hooks = crate::skills::hooks::ToolEventHookRegistry::default();
        let mut term = NoopHeadlessTerminal;
        let edge_callback_outputs: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();

        // Skill was pre-resolved; grep will be matched from edge_tool_round
        let pre_resolved = vec![("skill:0".to_string(), "Skill instructions".to_string())];
        let permission_context = PermissionSyncContext::shared_root(PermissionMode::Auto);

        run_agentic_headless_tool_round(HeadlessToolRoundCtx {
            turn_index: 0,
            session_turn: 1,
            quiet: true,
            api: &astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            token: "",
            current_user_id: None,
            current_session_id: None,
            current_run_id: None,
            current_turn_chain_id: None,
            durable_dispatch_admission: None,
            tool_calls: &tool_calls,
            edge_tool_round: &edge_tool_round,
            reasoning_content: "",
            reasoning_signature: "",
            edge_callback_outputs: &edge_callback_outputs,
            messages: &mut messages,
            tool_results: &mut tool_results,
            valid_tool_names: &valid_tool_names,
            deferred_tool_names: &std::collections::HashSet::new(),
            restricted_tools: &mut restricted_tools,
            turn_guard: &mut turn_guard,
            step_recorder: &mut step_recorder,
            idempotency_cache: &mut idempotency_cache,
            semantic_dedup: &mut semantic_dedup,
            call_counts: &mut std::collections::HashMap::new(),
            max_identical_calls: 2,
            max_tools_per_turn: 15,
            repeated_cache_hit_suppression: 3,
            max_consecutive_empty_name: 3,
            tool_call_records: &mut tool_call_records,
            tool_event_hooks: &tool_event_hooks,
            term: &mut term,
            mailbox: None,
            permission_context: Some(&permission_context),
            progress_emitter: None,
            pre_resolved_results: &pre_resolved,
            runtime_tool_executor: None,
            turn_start: None,
            llm_round: 0,
            plan_mode_active: false,
        })
        .await;

        // assistant(skill:0, grep:1) → tool(skill:0, pre-resolved) → tool(grep:1, edge-executed)
        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(
            messages[0]["tool_calls"].as_array().unwrap().len(),
            2,
            "assistant must have both tool_calls"
        );

        assert_eq!(messages[1]["tool_call_id"], "skill:0");
        assert_eq!(messages[1]["content"], "Skill instructions");

        assert_eq!(messages[2]["tool_call_id"], "grep:1");
        assert!(
            messages[2]["content"].as_str().unwrap().contains("TODO"),
            "grep result should contain edge output"
        );

        // Exactly 3 messages: assistant + 2 tool results
        assert_eq!(messages.len(), 3, "no duplicate tool results");
    }

    #[tokio::test]
    async fn child_tool_round_aborts_after_three_consecutive_empty_tool_names() {
        let tool_calls = vec![
            json!({
                "id": "call-empty-1",
                "type": "function",
                "function": { "name": "", "arguments": {} }
            }),
            json!({
                "id": "call-empty-2",
                "type": "function",
                "function": { "name": "", "arguments": {} }
            }),
            json!({
                "id": "call-empty-3",
                "type": "function",
                "function": { "name": "", "arguments": {} }
            }),
            json!({
                "id": "call-after-burst",
                "type": "function",
                "function": {
                    "name": "bash",
                    "arguments": r#"{"command":"echo should-not-run"}"#
                }
            }),
        ];

        let mut messages = Vec::new();
        let mut tool_results = Vec::new();
        let valid_tool_names = HashSet::from(["bash".to_string()]);
        let mut restricted_tools = HashSet::new();
        let mut turn_guard = TurnGuard::new();
        let mut step_recorder = StepRecorder::new("test-user", "test-session", "empty-name-burst");
        let mut idempotency_cache = InMemoryIdempotencyCache::new();
        let mut semantic_dedup = SemanticDedup::new(0.95);
        let mut tool_call_records = Vec::new();
        let tool_event_hooks = crate::skills::hooks::ToolEventHookRegistry::default();
        let mut term = NoopHeadlessTerminal;
        let edge_callback_outputs = std::collections::HashMap::new();
        let edge_tool_round: Vec<EdgeToolExecResult> = Vec::new();

        let outcome = run_agentic_headless_tool_round(HeadlessToolRoundCtx {
            turn_index: 0,
            session_turn: 1,
            quiet: true,
            api: &astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            token: "",
            current_user_id: None,
            current_session_id: None,
            current_run_id: None,
            current_turn_chain_id: None,
            durable_dispatch_admission: None,
            tool_calls: &tool_calls,
            edge_tool_round: &edge_tool_round,
            reasoning_content: "",
            reasoning_signature: "",
            edge_callback_outputs: &edge_callback_outputs,
            messages: &mut messages,
            tool_results: &mut tool_results,
            valid_tool_names: &valid_tool_names,
            deferred_tool_names: &std::collections::HashSet::new(),
            restricted_tools: &mut restricted_tools,
            turn_guard: &mut turn_guard,
            step_recorder: &mut step_recorder,
            idempotency_cache: &mut idempotency_cache,
            semantic_dedup: &mut semantic_dedup,
            call_counts: &mut std::collections::HashMap::new(),
            max_identical_calls: 2,
            max_tools_per_turn: 15,
            repeated_cache_hit_suppression: 3,
            max_consecutive_empty_name: 3,
            tool_call_records: &mut tool_call_records,
            tool_event_hooks: &tool_event_hooks,
            term: &mut term,
            mailbox: None,
            permission_context: None,
            progress_emitter: None,
            pre_resolved_results: &[],
            runtime_tool_executor: None,
            turn_start: None,
            llm_round: 0,
            plan_mode_active: false,
        })
        .await;

        assert!(messages.is_empty());
        assert!(tool_results.is_empty());
        assert!(tool_call_records.is_empty());
        assert!(
            outcome
                .action_admission_error
                .as_deref()
                .is_some_and(|error| error.contains("tool name is missing")),
            "unexpected admission result: {outcome:?}"
        );
    }

    /// Test: child requests permission via mailbox, parent approves, tool executes
    #[tokio::test]
    async fn child_permission_request_via_mailbox_approved() {
        use crate::orchestration::permission_sync::{
            PermissionRequestHandler, PermissionRule, PermissionUpdate,
        };

        let (router, parent_mb, mut child_mb, _dt) = setup_two_agents().await;

        // Parent has a handler that approves bash requests
        let parent_ctx = PermissionSyncContext::shared_root(PermissionMode::Prompt);
        let handler = PermissionRequestHandler::new(parent_ctx.clone());

        // Child has permission context that requires asking parent for bash.
        // Use a bare ask rule so this test cannot be accidentally satisfied by
        // the read-only shortcut before it reaches the mailbox.
        let child_inherited = InheritedPermissions {
            mode: PermissionMode::Prompt,
            allow_rules: vec![],
            deny_rules: vec![],
            ask_rules: vec![PermissionRule::parse("bash")],
            allowed_tools: None,
            is_background: false,
            ..Default::default()
        };
        let child_permission_ctx = PermissionSyncContext::shared(child_inherited);

        // Spawn parent handler task
        let parent_router = router.clone();
        let parent_handler = tokio::spawn(async move {
            // Wait for permission request
            let msg = tokio::time::timeout(std::time::Duration::from_secs(5), parent_mb.recv())
                .await
                .expect("should receive within timeout")
                .expect("should have message");

            // Process and respond
            if let Some((correlation_id, mut response)) = handler.process_message(&msg).await {
                // Approve with suggested rule
                response.approved = true;
                response
                    .updates
                    .push(PermissionUpdate::allow(PermissionRule::parse(
                        r#"Bash(argv_prefix="touch")"#,
                    )));

                // Extract the target address from the Direct variant
                let target_addr = match &msg.to {
                    MessageTarget::Direct { address } => address.clone(),
                    _ => panic!("expected Direct target"),
                };

                let response_msg = response.to_message(&target_addr, &msg.from, &correlation_id);
                parent_router.send(response_msg).await.unwrap();
            }
        });

        // Child sends tool call that requires permission
        let tool_calls = vec![json!({
            "id": "call-bash-touch",
            "type": "function",
            "function": {
                "name": "bash",
                "arguments": r#"{"command": "touch astra-permission-approved-test"}"#
            }
        })];

        let mut messages = Vec::new();
        let mut tool_results = Vec::new();
        let valid_tool_names = HashSet::from(["bash".to_string()]);
        let mut restricted_tools = HashSet::new();
        let mut turn_guard = TurnGuard::new();
        let mut step_recorder = StepRecorder::new("test-user", "test-session", "perm-request");
        let mut idempotency_cache = InMemoryIdempotencyCache::new();
        let mut semantic_dedup = SemanticDedup::new(0.95);
        let mut tool_call_records = Vec::new();
        let tool_event_hooks = crate::skills::hooks::ToolEventHookRegistry::default();
        let mut term = NoopHeadlessTerminal;
        let edge_callback_outputs = std::collections::HashMap::new();
        let edge_tool_round: Vec<EdgeToolExecResult> = Vec::new();

        run_agentic_headless_tool_round(HeadlessToolRoundCtx {
            turn_index: 0,
            session_turn: 1,
            quiet: true,
            api: &astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            token: "",
            current_user_id: None,
            current_session_id: None,
            current_run_id: None,
            current_turn_chain_id: None,
            durable_dispatch_admission: None,
            tool_calls: &tool_calls,
            edge_tool_round: &edge_tool_round,
            reasoning_content: "",
            reasoning_signature: "",
            edge_callback_outputs: &edge_callback_outputs,
            messages: &mut messages,
            tool_results: &mut tool_results,
            valid_tool_names: &valid_tool_names,
            deferred_tool_names: &std::collections::HashSet::new(),
            restricted_tools: &mut restricted_tools,
            turn_guard: &mut turn_guard,
            step_recorder: &mut step_recorder,
            idempotency_cache: &mut idempotency_cache,
            semantic_dedup: &mut semantic_dedup,
            call_counts: &mut std::collections::HashMap::new(),
            max_identical_calls: 2,
            max_tools_per_turn: 15,
            repeated_cache_hit_suppression: 3,
            max_consecutive_empty_name: 3,
            tool_call_records: &mut tool_call_records,
            tool_event_hooks: &tool_event_hooks,
            term: &mut term,
            mailbox: Some(&mut child_mb),
            permission_context: Some(&child_permission_ctx),
            progress_emitter: None,
            pre_resolved_results: &[],
            runtime_tool_executor: None,
            turn_start: None,
            llm_round: 0,
            plan_mode_active: false,
        })
        .await;

        // Wait for parent handler to complete
        parent_handler.await.unwrap();

        // Tool should have been processed (not blocked)
        // Since bash is an edge tool and we're in test context, it will have an unknown_tool error
        // but importantly it should NOT have a permission denied error
        assert_eq!(tool_results.len(), 1);
        assert_eq!(tool_call_records.len(), 1);

        // Check that permission was NOT denied (the error should be something else like unknown_tool)
        let error = tool_call_records[0].error.as_deref();
        assert!(
            error.is_none() || !error.unwrap().contains("Permission denied"),
            "tool should not be blocked by permission: {:?}",
            error
        );

        let telemetry = child_permission_ctx.read().await.telemetry();
        assert_eq!(telemetry.permission_requests, 1);
        assert_eq!(telemetry.permission_requests_approved, 1);
    }

    /// Test: child requests permission but parent denies
    #[tokio::test]
    async fn child_permission_request_via_mailbox_denied() {
        use crate::orchestration::permission_sync::{PermissionRequestHandler, PermissionRule};

        let (router, parent_mb, mut child_mb, _dt) = setup_two_agents().await;

        // Parent has deny mode - rejects all requests
        let parent_ctx = PermissionSyncContext::shared_root(PermissionMode::Deny);
        let handler = PermissionRequestHandler::new(parent_ctx.clone());

        // Child requires asking parent for bash. The bare ask rule pins the
        // request-parent flow before any local shortcut can decide.
        let child_inherited = InheritedPermissions {
            mode: PermissionMode::Prompt,
            allow_rules: vec![],
            deny_rules: vec![],
            ask_rules: vec![PermissionRule::parse("bash")],
            allowed_tools: None,
            is_background: false,
            ..Default::default()
        };
        let child_permission_ctx = PermissionSyncContext::shared(child_inherited);

        // Spawn parent handler that denies
        let parent_router = router.clone();
        let parent_handler = tokio::spawn(async move {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(5), parent_mb.recv())
                .await
                .expect("should receive within timeout")
                .expect("should have message");

            if let Some((correlation_id, response)) = handler.process_message(&msg).await {
                // Response should already be denied due to Deny mode
                assert!(!response.approved);

                // Extract the target address from the Direct variant
                let target_addr = match &msg.to {
                    MessageTarget::Direct { address } => address.clone(),
                    _ => panic!("expected Direct target"),
                };

                let response_msg = response.to_message(&target_addr, &msg.from, &correlation_id);
                parent_router.send(response_msg).await.unwrap();
            }
        });

        let tool_calls = vec![json!({
            "id": "call-bash-denied",
            "type": "function",
            // Keep this command non-read-only: Prompt mode now auto-approves
            // read-only bash calls like `echo hi` locally, so they never reach
            // the parent mailbox this test is exercising.
            "function": {
                "name": "bash",
                "arguments": r#"{"command": "touch astra-permission-denied-test"}"#
            }
        })];

        let mut messages = Vec::new();
        let mut tool_results = Vec::new();
        let valid_tool_names = HashSet::from(["bash".to_string()]);
        let mut restricted_tools = HashSet::new();
        let mut turn_guard = TurnGuard::new();
        let mut step_recorder = StepRecorder::new("test-user", "test-session", "perm-denied");
        let mut idempotency_cache = InMemoryIdempotencyCache::new();
        let mut semantic_dedup = SemanticDedup::new(0.95);
        let mut tool_call_records = Vec::new();
        let tool_event_hooks = crate::skills::hooks::ToolEventHookRegistry::default();
        let mut term = NoopHeadlessTerminal;
        let edge_callback_outputs = std::collections::HashMap::new();
        let edge_tool_round: Vec<EdgeToolExecResult> = Vec::new();

        run_agentic_headless_tool_round(HeadlessToolRoundCtx {
            turn_index: 0,
            session_turn: 1,
            quiet: true,
            api: &astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            token: "",
            current_user_id: None,
            current_session_id: None,
            current_run_id: None,
            current_turn_chain_id: None,
            durable_dispatch_admission: None,
            tool_calls: &tool_calls,
            edge_tool_round: &edge_tool_round,
            reasoning_content: "",
            reasoning_signature: "",
            edge_callback_outputs: &edge_callback_outputs,
            messages: &mut messages,
            tool_results: &mut tool_results,
            valid_tool_names: &valid_tool_names,
            deferred_tool_names: &std::collections::HashSet::new(),
            restricted_tools: &mut restricted_tools,
            turn_guard: &mut turn_guard,
            step_recorder: &mut step_recorder,
            idempotency_cache: &mut idempotency_cache,
            semantic_dedup: &mut semantic_dedup,
            call_counts: &mut std::collections::HashMap::new(),
            max_identical_calls: 2,
            max_tools_per_turn: 15,
            repeated_cache_hit_suppression: 3,
            max_consecutive_empty_name: 3,
            tool_call_records: &mut tool_call_records,
            tool_event_hooks: &tool_event_hooks,
            term: &mut term,
            mailbox: Some(&mut child_mb),
            permission_context: Some(&child_permission_ctx),
            progress_emitter: None,
            pre_resolved_results: &[],
            runtime_tool_executor: None,
            turn_start: None,
            llm_round: 0,
            plan_mode_active: false,
        })
        .await;

        parent_handler.await.unwrap();

        // Tool should be blocked
        assert_eq!(tool_results.len(), 1);
        assert_eq!(tool_call_records.len(), 1);

        // Check that permission WAS denied
        let error = tool_call_records[0].error.as_deref();
        assert!(
            error.is_some() && error.unwrap().contains("blocked_tool"),
            "tool should be blocked by permission denial: {:?}",
            error
        );

        let telemetry = child_permission_ctx.read().await.telemetry();
        assert_eq!(telemetry.permission_requests, 1);
        assert_eq!(telemetry.permission_requests_approved, 0);
    }

    /// Empty tool names (model bug) must be rejected before dedup counting
    /// so they don't inflate call_counts and flood the context with 50+ stubs.
    #[tokio::test]
    async fn empty_tool_name_rejected_before_dedup() {
        // A malformed member rejects the provider batch atomically, before
        // dedup or per-call execution can create partial ledger evidence.
        let tool_calls: Vec<Value> = (0..5)
            .map(|i| {
                json!({
                    "id": format!("call-{i}"),
                    "type": "function",
                    "function": { "name": "", "arguments": "{}" }
                })
            })
            .collect();

        let mut messages = Vec::new();
        let mut tool_results = Vec::new();
        let valid_tool_names = HashSet::from(["bash".to_string()]);
        let mut restricted_tools = HashSet::new();
        let mut turn_guard = TurnGuard::new();
        let mut step_recorder = StepRecorder::new("test-user", "test-session", "empty-name");
        let mut idempotency_cache = InMemoryIdempotencyCache::new();
        let mut semantic_dedup = SemanticDedup::new(0.95);
        let mut tool_call_records = Vec::new();
        let tool_event_hooks = crate::skills::hooks::ToolEventHookRegistry::default();
        let mut term = NoopHeadlessTerminal;
        let edge_callback_outputs = HashMap::new();
        let edge_tool_round: Vec<EdgeToolExecResult> = Vec::new();

        let outcome = run_agentic_headless_tool_round(HeadlessToolRoundCtx {
            turn_index: 0,
            session_turn: 1,
            quiet: true,
            api: &astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            token: "",
            current_user_id: None,
            current_session_id: None,
            current_run_id: None,
            current_turn_chain_id: None,
            durable_dispatch_admission: None,
            tool_calls: &tool_calls,
            edge_tool_round: &edge_tool_round,
            reasoning_content: "",
            reasoning_signature: "",
            edge_callback_outputs: &edge_callback_outputs,
            messages: &mut messages,
            tool_results: &mut tool_results,
            valid_tool_names: &valid_tool_names,
            deferred_tool_names: &std::collections::HashSet::new(),
            restricted_tools: &mut restricted_tools,
            turn_guard: &mut turn_guard,
            step_recorder: &mut step_recorder,
            idempotency_cache: &mut idempotency_cache,
            semantic_dedup: &mut semantic_dedup,
            call_counts: &mut HashMap::new(),
            max_identical_calls: 2,
            max_tools_per_turn: 15,
            repeated_cache_hit_suppression: 3,
            max_consecutive_empty_name: 3,
            tool_call_records: &mut tool_call_records,
            tool_event_hooks: &tool_event_hooks,
            term: &mut term,
            mailbox: None,
            permission_context: None,
            progress_emitter: None,
            pre_resolved_results: &[],
            runtime_tool_executor: None,
            turn_start: None,
            llm_round: 0,
            plan_mode_active: false,
        })
        .await;

        assert!(messages.is_empty());
        assert!(tool_results.is_empty());
        assert!(tool_call_records.is_empty());
        assert!(
            outcome
                .action_admission_error
                .as_deref()
                .is_some_and(|error| error.contains("tool name is missing")),
            "unexpected admission result: {outcome:?}"
        );
    }
}
