//! End-to-end tests for messaging integration with the agentic loop.
//!
//! Verifies:
//! 1. Preamble injects send_message tool schema when mailbox is present
//! 2. Turn-start drain formats pending messages as system messages
//! 3. send_message tool calls are intercepted and routed through mailbox
//! 4. Turn-end sends progress to parent via mailbox

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;

    use async_trait::async_trait;
    use serde_json::{Value, json};

    use crate::messaging::in_process::InProcessTransport;
    use crate::messaging::router::AgentMailboxRouter;
    use crate::messaging::types::*;
    use crate::orchestration::permission_sync::{
        InheritedPermissions, PermissionMode, PermissionRequest, PermissionResponse,
        PermissionSyncContext,
    };
    use crate::pipeline::step_protocol::InMemoryIdempotencyCache;
    use crate::pipeline::step_recorder::StepRecorder;
    use crate::semantic_dedup::SemanticDedup;
    use crate::server::delegation_engine::{DelegationTracker, SubRunRecord};
    use crate::turn::agentic_headless_round::{
        HeadlessStderrStyle, NoopHeadlessTerminal, run_agentic_headless_tool_round,
    };
    use crate::turn::agentic_loop_host::{
        AgenticLoopHost, AgenticLoopState, HostTurnResult, run_agentic_loop_with_host,
    };
    use crate::turn::chat_turn_heuristics::TaskExecutionProfile;
    use crate::turn::chat_turn_sse_dispatch::ChatTurnSseAccum;
    use crate::turn::sse_stream_host::EdgeToolExecResult;
    use crate::turn::turn_guard::TurnGuard;

    // ── Mock Host ───────────────────────────────────────────────────────────

    struct MockHost {
        turn_results: Vec<HostTurnResult>,
        current_turn: usize,
        valid_tools: HashSet<String>,
        emitted_lines: Vec<String>,
        injected_schemas: Vec<Value>,
    }

    impl MockHost {
        fn new(results: Vec<HostTurnResult>) -> Self {
            Self {
                turn_results: results,
                current_turn: 0,
                valid_tools: HashSet::new(),
                emitted_lines: Vec::new(),
                injected_schemas: Vec::new(),
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
        ) -> Result<HostTurnResult, String> {
            if self.turn_results.is_empty() {
                return Err("no more turns".to_string());
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
        }
    }

    fn server_tool_result(tool_calls: Vec<Value>) -> HostTurnResult {
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
            edge_tool_round: Vec::new(),
        }
    }

    // ── State builder ───────────────────────────────────────────────────────

    fn make_state() -> AgenticLoopState {
        AgenticLoopState {
            messages: Vec::new(),
            tool_results: Vec::new(),
            current_session_id: None,
            current_run_id: None,
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
            restricted_tools: HashSet::new(),
            step_recorder: StepRecorder::new("test-session", "test-task"),
            idempotency_cache: InMemoryIdempotencyCache::new(),
            semantic_dedup: SemanticDedup::new(0.95),
            turn_sigs: Vec::new(),
            turn_tool_names: Vec::new(),
            stall_events: Vec::new(),
            intent_tool_turns: Vec::new(),
            verdict_events: Vec::new(),
            last_heavy_checkpoint: None,
            tool_call_records: Vec::new(),
            forced_factual_retry: false,
            explain_turns: Vec::new(),
            first_ttft_ms: None,
            all_tools_used: HashSet::new(),
            first_selection_report: None,
            first_budget_pressure: 0.0,
            first_context_assembly_ms: None,
            first_memoria_ms: None,
            first_selector_ms: None,
            first_selector_strategy: None,
            first_selector_confidence: None,
            selector_tokens_in: 0,
            selector_tokens_out: 0,
            all_selected_skills: Vec::new(),
            message: "test query".to_string(),
            recent_tools: Vec::new(),
            task_profile: TaskExecutionProfile::default(),
            api: astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            api_token: String::new(),
            cancel_flag: None,
            cancel_token: None,
            delegation_engine: None,
            skill_registry_for_activation: None,
            skill_resolver: None,
            skill_executor: None,
            skill_model_override: None,
            skill_effort: None,
            skill_agent_type: None,
            skill_allowed_tools: None,
            skill_sandbox_policy: None,
            skill_quality_tracker: crate::skills::quality::SkillQualityTracker::new(),
            skill_improvement_tracker: crate::skills::improvement::ImprovementTracker::new(),
            pinned_skills: HashSet::new(),
            discovered_skills: HashSet::new(),
            skill_search: astra_core::SkillSearchSettings::default(),
            tool_event_hooks: crate::skills::hooks::ToolEventHookRegistry::default(),
            session_event_hooks: crate::skills::hooks::SessionEventHookRegistry::default(),
            stop_hooks: Vec::new(),
            stop_hook_runs: 0,
            teammate_idle_hooks: Vec::new(),
            teammate_idle_hook_runs: 0,
            workspace_root_hint: None,
            consecutive_same_error: 0,
            last_error_category: None,
            checkpoint_gate: None,
            data_snapshot_provider: None,
            last_composite_snapshot: None,
            last_measured_prompt_tokens: None,
            consecutive_context_window_errors: 0,
            max_turn_input_tokens: 0,
            budget_wrapup_injected: false,
            thinking_budget_tokens: None,
            skill_listing_message: None,
            invoked_skills: std::collections::HashMap::new(),
            recent_file_reads: Vec::new(),
            project_context: None,
            mailbox: None,
            ack_tracker: None,
            dead_letter_queue: None,
            messaging_metrics: None,
            progress_emitter: None,
            permission_context: None,
            permission_handler: None,
            observability_session: None,
            observability_hub: None,
        }
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    async fn setup_two_agents() -> (
        Arc<AgentMailboxRouter>,
        crate::messaging::router::AgentMailbox,
        crate::messaging::router::AgentMailbox,
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
        })
        .await;

        let child_mb = router
            .register(child_addr, Some("del-e2e".into()))
            .await
            .unwrap();

        (router, parent_mb, child_mb, dt)
    }

    // ── Tests ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn preamble_injects_send_message_schema_when_mailbox_present() {
        let (_router, _parent, child_mb, _dt) = setup_two_agents().await;

        let mut host = MockHost::new(vec![text_result("done")]);
        let mut state = make_state();
        state.mailbox = Some(child_mb);

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());

        // Preamble should have injected send_message schema.
        let has_send_msg = host.injected_schemas.iter().any(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                == Some("send_message")
        });
        assert!(has_send_msg, "send_message schema should be injected");
        assert!(
            host.valid_tools.contains("send_message"),
            "send_message should be in valid_tools"
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
        state.mailbox = Some(child_mb);

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());

        // The loop should have injected a system message with the drained mailbox content.
        let has_mailbox_msg = state.messages.iter().any(|m| {
            m.get("role").and_then(Value::as_str) == Some("system")
                && m.get("content")
                    .and_then(Value::as_str)
                    .is_some_and(|c| c.contains("📬") && c.contains("orchestrator"))
        });
        assert!(
            has_mailbox_msg,
            "should have system message with drained mailbox: {:?}",
            state
                .messages
                .iter()
                .filter(|m| m.get("role").and_then(Value::as_str) == Some("system"))
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn send_message_tool_call_intercepted_and_routed() {
        let (_router, mut parent_mb, child_mb, _dt) = setup_two_agents().await;

        // Turn 1: LLM calls send_message tool → should be intercepted
        // Turn 2: LLM produces final text
        // Note: arguments must be a JSON *string* (OpenAI tool call convention).
        let tool_calls = vec![json!({
            "id": "call-send-1",
            "type": "function",
            "function": {
                "name": "send_message",
                "arguments": r#"{"target": "parent", "content": "Auth module looks clean.", "message_type": "result"}"#
            }
        })];

        let mut host = MockHost::new(vec![
            server_tool_result(tool_calls),
            text_result("Reported to parent."),
        ])
        .with_valid_tools(&["send_message"]);

        let mut state = make_state();
        state.mailbox = Some(child_mb);

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());
        assert_eq!(state.final_text, "Reported to parent.");
        assert_eq!(host.current_turn, 2);

        // Parent should have received the message via mailbox.
        let received = parent_mb.try_recv();
        assert!(
            received.is_some(),
            "parent should have received send_message"
        );
        let msg = received.unwrap();
        assert_eq!(msg.from.agent_id, "worker");
        match &msg.payload {
            MessagePayload::Signal(AgentSignal::Completed { output }) => {
                assert_eq!(output, "Auth module looks clean.");
            }
            other => panic!("expected Completed signal, got: {other:?}"),
        }

        // The tool result should be in state.messages.
        let has_tool_result = state.messages.iter().any(|m| {
            m.get("role").and_then(Value::as_str) == Some("tool")
                && m.get("tool_call_id").and_then(Value::as_str) == Some("call-send-1")
        });
        assert!(
            has_tool_result,
            "intercepted tool call should produce a tool result message"
        );
    }

    #[tokio::test]
    async fn send_message_mixed_with_regular_tools() {
        let (_router, mut parent_mb, child_mb, _dt) = setup_two_agents().await;

        // Turn 1: LLM calls BOTH send_message and a regular tool.
        // send_message should be intercepted; the regular tool should pass through.
        let tool_calls = vec![
            json!({
                "id": "call-send-2",
                "type": "function",
                "function": {
                    "name": "send_message",
                    "arguments": r#"{"target": "parent", "content": "Starting work", "message_type": "text"}"#
                }
            }),
            json!({
                "id": "call-bash-1",
                "type": "function",
                "function": {
                    "name": "bash",
                    "arguments": r#"{"command": "echo hello"}"#
                }
            }),
        ];

        let edge_tools = vec![EdgeToolExecResult {
            request_id: "call-bash-1".into(),
            tool: "bash".into(),
            args: json!({"command": "echo hello"}),
            output: "hello".into(),
            status: "ok".into(),
            duration_ms: 5,
        }];

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
            },
            text_result("Done."),
        ])
        .with_valid_tools(&["send_message", "bash"]);

        let mut state = make_state();
        state.mailbox = Some(child_mb);

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());

        // Parent should have received the send_message.
        let received = parent_mb.try_recv();
        assert!(
            received.is_some(),
            "parent should have received text message"
        );
        match &received.unwrap().payload {
            MessagePayload::Text { content, .. } => {
                assert_eq!(content, "Starting work");
            }
            other => panic!("expected Text, got: {other:?}"),
        }

        // bash tool should still have been processed (2 turns = tool + final text).
        assert_eq!(host.current_turn, 2);
        assert!(state.all_tools_used.contains("bash"));
    }

    #[tokio::test]
    async fn progress_sent_to_parent_on_tool_turn() {
        let (_router, mut parent_mb, child_mb, _dt) = setup_two_agents().await;

        // Tool turn → should send progress to parent.
        let edge_tools = vec![EdgeToolExecResult {
            request_id: "call-read-1".into(),
            tool: "read_file".into(),
            args: json!({"path": "/tmp/x.txt"}),
            output: "content".into(),
            status: "ok".into(),
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
            },
            text_result("Read the file."),
        ])
        .with_valid_tools(&["read_file"]);

        let mut state = make_state();
        state.mailbox = Some(child_mb);

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());

        // Parent should have received a progress message from the tool turn.
        let received = parent_mb.try_recv();
        assert!(
            received.is_some(),
            "parent should have received progress from tool turn"
        );
        match &received.unwrap().payload {
            MessagePayload::Progress {
                status, tool_calls, ..
            } => {
                assert!(!status.is_empty());
                assert!(*tool_calls >= 1);
            }
            other => panic!("expected Progress, got: {other:?}"),
        }
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
        state.mailbox = Some(parent_mb);
        state.permission_context = Some(Arc::new(tokio::sync::RwLock::new(
            PermissionSyncContext::root(PermissionMode::Auto),
        )));

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
            "name": "bash",
            "arguments": r#"{"command": "echo hi"}"#
        })];

        let permission_context = Arc::new(tokio::sync::RwLock::new(PermissionSyncContext::new(
            InheritedPermissions {
                mode: PermissionMode::Prompt,
                allow_rules: vec![],
                deny_rules: vec![],
                ask_rules: vec![],
                allowed_tools: Some(HashSet::from(["view".to_string()])),
                is_background: false,
            },
        )));
        let mut messages = Vec::new();
        let mut tool_results = Vec::new();
        let valid_tool_names = HashSet::from(["bash".to_string()]);
        let mut restricted_tools = HashSet::new();
        let mut turn_guard = TurnGuard::new();
        let mut step_recorder = StepRecorder::new("test-session", "perm-headless");
        let mut idempotency_cache = InMemoryIdempotencyCache::new();
        let mut semantic_dedup = SemanticDedup::new(0.95);
        let mut tool_call_records = Vec::new();
        let tool_event_hooks = crate::skills::hooks::ToolEventHookRegistry::default();
        let mut term = NoopHeadlessTerminal;
        let edge_callback_outputs = std::collections::HashMap::new();
        let edge_tool_round: Vec<EdgeToolExecResult> = Vec::new();

        run_agentic_headless_tool_round(
            0,
            true,
            &astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            "",
            None,
            &tool_calls,
            &edge_tool_round,
            "",
            &edge_callback_outputs,
            &mut messages,
            &mut tool_results,
            &valid_tool_names,
            &mut restricted_tools,
            &mut turn_guard,
            &mut step_recorder,
            &mut idempotency_cache,
            &mut semantic_dedup,
            &mut tool_call_records,
            &tool_event_hooks,
            &mut term,
            None,
            Some(&permission_context),
        )
        .await;

        assert_eq!(tool_results.len(), 1);
        assert_eq!(tool_call_records.len(), 1);
        assert_eq!(
            tool_call_records[0].error.as_deref(),
            Some("blocked_tool: Tool 'bash' requires permission but no parent available"),
        );
    }

    /// Test: child requests permission via mailbox, parent approves, tool executes
    #[tokio::test]
    async fn child_permission_request_via_mailbox_approved() {
        use crate::orchestration::permission_sync::{
            PermissionRequestHandler, PermissionRule, PermissionUpdate,
        };

        let (router, parent_mb, mut child_mb, _dt) = setup_two_agents().await;

        // Parent has a handler that approves bash(git:*) requests
        let parent_ctx = Arc::new(tokio::sync::RwLock::new(PermissionSyncContext::root(
            PermissionMode::Prompt,
        )));
        let handler = PermissionRequestHandler::new(parent_ctx.clone());

        // Child has permission context that requires asking parent for bash
        let child_inherited = InheritedPermissions {
            mode: PermissionMode::Prompt,
            allow_rules: vec![],
            deny_rules: vec![],
            ask_rules: vec![PermissionRule::parse("bash(*)")],
            allowed_tools: None,
            is_background: false,
        };
        let child_permission_ctx = Arc::new(tokio::sync::RwLock::new(PermissionSyncContext::new(
            child_inherited,
        )));

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
                        "bash(git:*)",
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
            "id": "call-bash-git",
            "name": "bash",
            "arguments": r#"{"command": "git status"}"#
        })];

        let mut messages = Vec::new();
        let mut tool_results = Vec::new();
        let valid_tool_names = HashSet::from(["bash".to_string()]);
        let mut restricted_tools = HashSet::new();
        let mut turn_guard = TurnGuard::new();
        let mut step_recorder = StepRecorder::new("test-session", "perm-request");
        let mut idempotency_cache = InMemoryIdempotencyCache::new();
        let mut semantic_dedup = SemanticDedup::new(0.95);
        let mut tool_call_records = Vec::new();
        let tool_event_hooks = crate::skills::hooks::ToolEventHookRegistry::default();
        let mut term = NoopHeadlessTerminal;
        let edge_callback_outputs = std::collections::HashMap::new();
        let edge_tool_round: Vec<EdgeToolExecResult> = Vec::new();

        run_agentic_headless_tool_round(
            0,
            true,
            &astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            "",
            None,
            &tool_calls,
            &edge_tool_round,
            "",
            &edge_callback_outputs,
            &mut messages,
            &mut tool_results,
            &valid_tool_names,
            &mut restricted_tools,
            &mut turn_guard,
            &mut step_recorder,
            &mut idempotency_cache,
            &mut semantic_dedup,
            &mut tool_call_records,
            &tool_event_hooks,
            &mut term,
            Some(&mut child_mb),
            Some(&child_permission_ctx),
        )
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
    }

    /// Test: child requests permission but parent denies
    #[tokio::test]
    async fn child_permission_request_via_mailbox_denied() {
        use crate::orchestration::permission_sync::PermissionRequestHandler;

        let (router, parent_mb, mut child_mb, _dt) = setup_two_agents().await;

        // Parent has deny mode - rejects all requests
        let parent_ctx = Arc::new(tokio::sync::RwLock::new(PermissionSyncContext::root(
            PermissionMode::Deny,
        )));
        let handler = PermissionRequestHandler::new(parent_ctx.clone());

        // Child requires asking parent for all tools
        let child_inherited = InheritedPermissions {
            mode: PermissionMode::Prompt,
            allow_rules: vec![],
            deny_rules: vec![],
            ask_rules: vec![],
            allowed_tools: Some(HashSet::new()), // Empty = nothing allowed locally
            is_background: false,
        };
        let child_permission_ctx = Arc::new(tokio::sync::RwLock::new(PermissionSyncContext::new(
            child_inherited,
        )));

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
            "name": "bash",
            "arguments": r#"{"command": "rm -rf /"}"#
        })];

        let mut messages = Vec::new();
        let mut tool_results = Vec::new();
        let valid_tool_names = HashSet::from(["bash".to_string()]);
        let mut restricted_tools = HashSet::new();
        let mut turn_guard = TurnGuard::new();
        let mut step_recorder = StepRecorder::new("test-session", "perm-denied");
        let mut idempotency_cache = InMemoryIdempotencyCache::new();
        let mut semantic_dedup = SemanticDedup::new(0.95);
        let mut tool_call_records = Vec::new();
        let tool_event_hooks = crate::skills::hooks::ToolEventHookRegistry::default();
        let mut term = NoopHeadlessTerminal;
        let edge_callback_outputs = std::collections::HashMap::new();
        let edge_tool_round: Vec<EdgeToolExecResult> = Vec::new();

        run_agentic_headless_tool_round(
            0,
            true,
            &astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            "",
            None,
            &tool_calls,
            &edge_tool_round,
            "",
            &edge_callback_outputs,
            &mut messages,
            &mut tool_results,
            &valid_tool_names,
            &mut restricted_tools,
            &mut turn_guard,
            &mut step_recorder,
            &mut idempotency_cache,
            &mut semantic_dedup,
            &mut tool_call_records,
            &tool_event_hooks,
            &mut term,
            Some(&mut child_mb),
            Some(&child_permission_ctx),
        )
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
    }
}
