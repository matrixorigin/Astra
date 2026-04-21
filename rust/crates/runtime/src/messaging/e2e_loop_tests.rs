//! End-to-end tests for messaging integration with the agentic loop.
//!
//! Verifies:
//! 1. Preamble injects send_message tool schema when mailbox is present
//! 2. Turn-start drain formats pending messages as system messages
//! 3. send_message tool calls are intercepted and routed through mailbox
//! 4. Turn-end sends progress to parent via mailbox

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;

    use async_trait::async_trait;
    use serde_json::{Value, json};

    use crate::messaging::in_process::InProcessTransport;
    use crate::messaging::router::AgentMailboxRouter;
    use crate::messaging::types::*;
    use crate::orchestration::permission_sync::{
        InheritedPermissions, PermissionMode, PermissionRequest, PermissionRequestMessaging,
        PermissionResponse, PermissionResponseMessaging, PermissionSyncContext,
    };
    use crate::pipeline::step_protocol::InMemoryIdempotencyCache;
    use crate::pipeline::step_recorder::StepRecorder;
    use crate::semantic_dedup::SemanticDedup;
    use crate::server::delegation_engine::{DelegationTracker, SubRunRecord, SubRunState};
    use crate::turn::agentic_headless_round::{
        HeadlessStderrStyle, HeadlessToolRoundCtx, NoopHeadlessTerminal,
        run_agentic_headless_tool_round,
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
            error_kind: None,
        }
    }

    // ── State builder ───────────────────────────────────────────────────────

    fn make_state() -> AgenticLoopState {
        AgenticLoopState {
            messages: Vec::new(),
            tool_results: Vec::new(),
            current_session_id: None,
            current_run_id: None,
            recursion_depth: 0,
            final_text: String::new(),
            total_prompt: 0,
            total_completion: 0,
            total_cache_read: 0,
            total_cache_creation: 0,
            total_tool_calls: 0,
            total_evidence_tool_calls: 0,
            has_any_usage: false,
            max_turns: 10,
            remaining_turns: 10,
            current_round_index: 0,
            turn_guard: TurnGuard::new(),
            restricted_tools: HashSet::new(),
            boosted_tools: HashSet::new(),
            widen_selection_pending: false,
            step_recorder: StepRecorder::new("test-session", "test-task"),
            idempotency_cache: InMemoryIdempotencyCache::new(),
            semantic_dedup: SemanticDedup::new(0.95),
            call_counts: HashMap::new(),
            max_identical_tool_calls: crate::runtime_config::RuntimeConfig::load()
                .tool_selection
                .effective_max_identical_calls(),
            max_tools_per_turn: 15,
            stall: Default::default(),
            telemetry: Default::default(),
            skills: Default::default(),
            hooks: Default::default(),
            cancellation: Default::default(),
            messaging: Default::default(),
            error_recovery: Default::default(),
            message: "test query".to_string(),
            recent_tools: Vec::new(),
            task_profile: TaskExecutionProfile::default(),
            last_turn_policy: crate::turn::agentic_loop_host::TurnInteractionPolicy::default(),
            api: astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
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
            compaction_effectiveness: Default::default(),
            max_turn_input_tokens: 0,
            budget_wrapup_injected: false,
            skill_produced_output: false,
            max_cumulative_tokens: 0,
            thinking_budget_tokens: None,
            recent_file_reads: Vec::new(),
            permission_context: None,
            permission_handler: None,
            tactical_adapter: None,
            step_signal_collector: None,
            tool_budget_override: None,
            pending_reflection_signals: Vec::new(),
            recent_tactical_actions: Vec::new(),
            server_tool_executor: None,
            interruption: None,
            session_facts: Default::default(),
            approval_overrides: None,
            confidence_trend: Default::default(),
            last_confidence_diagnosis: None,
            session_turn: 0,
            prefetch_injected: false,
            turn_event_buffer: None,
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

    #[tokio::test]
    async fn preamble_injects_send_message_schema_when_mailbox_present() {
        let (_router, _parent, child_mb, _dt) = setup_two_agents().await;

        let mut host = MockHost::new(vec![text_result("done")]);
        let mut state = make_state();
        state.messaging.mailbox = Some(child_mb);

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
        state.messaging.mailbox = Some(child_mb);

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
        state.messaging.mailbox = Some(child_mb);

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
            tool_result_fields: None,
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
                error_kind: None,
            },
            text_result("Done."),
        ])
        .with_valid_tools(&["send_message", "bash"]);

        let mut state = make_state();
        state.messaging.mailbox = Some(child_mb);

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
        assert!(state.telemetry.all_tools_used.contains("bash"));
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
            tool_result_fields: None,
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
                error_kind: None,
            },
            text_result("Read the file."),
        ])
        .with_valid_tools(&["read_file"]);

        let mut state = make_state();
        state.messaging.mailbox = Some(child_mb);

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
        state.messaging.mailbox = Some(parent_mb);
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

        run_agentic_headless_tool_round(HeadlessToolRoundCtx {
            turn_index: 0,
            quiet: true,
            api: &astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            token: "",
            current_session_id: None,
            tool_calls: &tool_calls,
            edge_tool_round: &edge_tool_round,
            reasoning_content: "",
            edge_callback_outputs: &edge_callback_outputs,
            messages: &mut messages,
            tool_results: &mut tool_results,
            valid_tool_names: &valid_tool_names,
            restricted_tools: &mut restricted_tools,
            turn_guard: &mut turn_guard,
            step_recorder: &mut step_recorder,
            idempotency_cache: &mut idempotency_cache,
            semantic_dedup: &mut semantic_dedup,
            call_counts: &mut std::collections::HashMap::new(),
            max_identical_calls: 2,
            max_tools_per_turn: 15,
            tool_call_records: &mut tool_call_records,
            tool_event_hooks: &tool_event_hooks,
            term: &mut term,
            mailbox: None,
            permission_context: Some(&permission_context),
            progress_emitter: None,
            pre_resolved_results: &[],
            server_tool_executor: None,
            turn_start: None,
            llm_round: 0,
        })
        .await;

        assert_eq!(tool_results.len(), 1);
        assert_eq!(tool_call_records.len(), 1);
        assert_eq!(
            tool_call_records[0].error.as_deref(),
            Some("blocked_tool: Tool 'bash' requires permission but no parent available"),
        );
    }

    /// Regression test for session 46fd8ed8: kimi-k2.5 returned tool_calls
    /// with empty id → assistant message had id="" while tool result had a
    /// UUID → API returned 400 "tool_call_id not found".
    ///
    /// After the fix, normalize_tool_call_for_accum generates a synthetic UUID
    /// for empty ids, so the assistant message and tool result share the same id.
    #[tokio::test]
    async fn empty_tool_call_id_gets_synthetic_uuid_and_ids_match() {
        // Tool call with empty id but valid name — simulates kimi-k2.5 behavior
        let tool_calls = vec![json!({
            "id": "",
            "name": "bash",
            "arguments": r#"{"command":"echo hi"}"#
        })];

        let mut messages = Vec::new();
        let mut tool_results = Vec::new();
        let valid_tool_names = HashSet::from(["bash".to_string()]);
        let mut restricted_tools = HashSet::new();
        let mut turn_guard = TurnGuard::new();
        let mut step_recorder = StepRecorder::new("test-session", "empty-id");
        let mut idempotency_cache = InMemoryIdempotencyCache::new();
        let mut semantic_dedup = SemanticDedup::new(0.95);
        let mut tool_call_records = Vec::new();
        let tool_event_hooks = crate::skills::hooks::ToolEventHookRegistry::default();
        let mut term = NoopHeadlessTerminal;
        let edge_callback_outputs = std::collections::HashMap::new();
        let edge_tool_round: Vec<EdgeToolExecResult> = Vec::new();

        let _ = run_agentic_headless_tool_round(HeadlessToolRoundCtx {
            turn_index: 0,
            quiet: true,
            api: &astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            token: "",
            current_session_id: None,
            tool_calls: &tool_calls,
            edge_tool_round: &edge_tool_round,
            reasoning_content: "",
            edge_callback_outputs: &edge_callback_outputs,
            messages: &mut messages,
            tool_results: &mut tool_results,
            valid_tool_names: &valid_tool_names,
            restricted_tools: &mut restricted_tools,
            turn_guard: &mut turn_guard,
            step_recorder: &mut step_recorder,
            idempotency_cache: &mut idempotency_cache,
            semantic_dedup: &mut semantic_dedup,
            call_counts: &mut std::collections::HashMap::new(),
            max_identical_calls: 2,
            max_tools_per_turn: 15,
            tool_call_records: &mut tool_call_records,
            tool_event_hooks: &tool_event_hooks,
            term: &mut term,
            mailbox: None,
            permission_context: None,
            progress_emitter: None,
            pre_resolved_results: &[],
            server_tool_executor: None,
            turn_start: None,
            llm_round: 0,
        })
        .await;

        // messages[0] = assistant message with tool_calls
        // messages[1] = tool result message with tool_call_id
        assert!(
            messages.len() >= 2,
            "expected assistant + tool result messages"
        );

        let assistant_tc_id = messages[0]["tool_calls"][0]["id"]
            .as_str()
            .expect("assistant tool_call must have id");
        let tool_result_id = messages[1]["tool_call_id"]
            .as_str()
            .expect("tool result must have tool_call_id");

        assert!(
            !assistant_tc_id.is_empty(),
            "assistant tool_call id must not be empty"
        );
        assert_eq!(
            assistant_tc_id, tool_result_id,
            "assistant tool_call id and tool result tool_call_id must match"
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
        let mut step_recorder = StepRecorder::new("test-session", "pre-resolved");
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
            quiet: true,
            api: &astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            token: "",
            current_session_id: None,
            tool_calls: &tool_calls,
            edge_tool_round: &edge_tool_round,
            reasoning_content: "",
            edge_callback_outputs: &edge_callback_outputs,
            messages: &mut messages,
            tool_results: &mut tool_results,
            valid_tool_names: &valid_tool_names,
            restricted_tools: &mut restricted_tools,
            turn_guard: &mut turn_guard,
            step_recorder: &mut step_recorder,
            idempotency_cache: &mut idempotency_cache,
            semantic_dedup: &mut semantic_dedup,
            call_counts: &mut std::collections::HashMap::new(),
            max_identical_calls: 2,
            max_tools_per_turn: 15,
            tool_call_records: &mut tool_call_records,
            tool_event_hooks: &tool_event_hooks,
            term: &mut term,
            mailbox: None,
            permission_context: None,
            progress_emitter: None,
            pre_resolved_results: &pre_resolved,
            server_tool_executor: None,
            turn_start: None,
            llm_round: 0,
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
        let mut step_recorder = StepRecorder::new("test-session", "all-pre-resolved");
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
            quiet: true,
            api: &astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            token: "",
            current_session_id: None,
            tool_calls: &tool_calls,
            edge_tool_round: &edge_tool_round,
            reasoning_content: "",
            edge_callback_outputs: &edge_callback_outputs,
            messages: &mut messages,
            tool_results: &mut tool_results,
            valid_tool_names: &valid_tool_names,
            restricted_tools: &mut restricted_tools,
            turn_guard: &mut turn_guard,
            step_recorder: &mut step_recorder,
            idempotency_cache: &mut idempotency_cache,
            semantic_dedup: &mut semantic_dedup,
            call_counts: &mut std::collections::HashMap::new(),
            max_identical_calls: 2,
            max_tools_per_turn: 15,
            tool_call_records: &mut tool_call_records,
            tool_event_hooks: &tool_event_hooks,
            term: &mut term,
            mailbox: None,
            permission_context: None,
            progress_emitter: None,
            pre_resolved_results: &pre_resolved,
            server_tool_executor: None,
            turn_start: None,
            llm_round: 0,
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
            tool_result_fields: None,
            status: "ok".to_string(),
            duration_ms: 50,
        }];

        let mut messages = Vec::new();
        let mut tool_results = Vec::new();
        let valid_tool_names = HashSet::from(["skill".to_string(), "grep".to_string()]);
        let mut restricted_tools = HashSet::new();
        let mut turn_guard = TurnGuard::new();
        let mut step_recorder = StepRecorder::new("test-session", "mixed-edge");
        let mut idempotency_cache = InMemoryIdempotencyCache::new();
        let mut semantic_dedup = SemanticDedup::new(0.95);
        let mut tool_call_records = Vec::new();
        let tool_event_hooks = crate::skills::hooks::ToolEventHookRegistry::default();
        let mut term = NoopHeadlessTerminal;
        let edge_callback_outputs: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();

        // Skill was pre-resolved; grep will be matched from edge_tool_round
        let pre_resolved = vec![("skill:0".to_string(), "Skill instructions".to_string())];

        run_agentic_headless_tool_round(HeadlessToolRoundCtx {
            turn_index: 0,
            quiet: true,
            api: &astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            token: "",
            current_session_id: None,
            tool_calls: &tool_calls,
            edge_tool_round: &edge_tool_round,
            reasoning_content: "",
            edge_callback_outputs: &edge_callback_outputs,
            messages: &mut messages,
            tool_results: &mut tool_results,
            valid_tool_names: &valid_tool_names,
            restricted_tools: &mut restricted_tools,
            turn_guard: &mut turn_guard,
            step_recorder: &mut step_recorder,
            idempotency_cache: &mut idempotency_cache,
            semantic_dedup: &mut semantic_dedup,
            call_counts: &mut std::collections::HashMap::new(),
            max_identical_calls: 2,
            max_tools_per_turn: 15,
            tool_call_records: &mut tool_call_records,
            tool_event_hooks: &tool_event_hooks,
            term: &mut term,
            mailbox: None,
            permission_context: None,
            progress_emitter: None,
            pre_resolved_results: &pre_resolved,
            server_tool_executor: None,
            turn_start: None,
            llm_round: 0,
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
                "name": "",
                "arguments": {}
            }),
            json!({
                "id": "call-empty-2",
                "name": "",
                "arguments": {}
            }),
            json!({
                "id": "call-empty-3",
                "name": "",
                "arguments": {}
            }),
            json!({
                "id": "call-after-burst",
                "name": "bash",
                "arguments": r#"{"command":"echo should-not-run"}"#
            }),
        ];

        let mut messages = Vec::new();
        let mut tool_results = Vec::new();
        let valid_tool_names = HashSet::from(["bash".to_string()]);
        let mut restricted_tools = HashSet::new();
        let mut turn_guard = TurnGuard::new();
        let mut step_recorder = StepRecorder::new("test-session", "empty-name-burst");
        let mut idempotency_cache = InMemoryIdempotencyCache::new();
        let mut semantic_dedup = SemanticDedup::new(0.95);
        let mut tool_call_records = Vec::new();
        let tool_event_hooks = crate::skills::hooks::ToolEventHookRegistry::default();
        let mut term = NoopHeadlessTerminal;
        let edge_callback_outputs = std::collections::HashMap::new();
        let edge_tool_round: Vec<EdgeToolExecResult> = Vec::new();

        run_agentic_headless_tool_round(HeadlessToolRoundCtx {
            turn_index: 0,
            quiet: true,
            api: &astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            token: "",
            current_session_id: None,
            tool_calls: &tool_calls,
            edge_tool_round: &edge_tool_round,
            reasoning_content: "",
            edge_callback_outputs: &edge_callback_outputs,
            messages: &mut messages,
            tool_results: &mut tool_results,
            valid_tool_names: &valid_tool_names,
            restricted_tools: &mut restricted_tools,
            turn_guard: &mut turn_guard,
            step_recorder: &mut step_recorder,
            idempotency_cache: &mut idempotency_cache,
            semantic_dedup: &mut semantic_dedup,
            call_counts: &mut std::collections::HashMap::new(),
            max_identical_calls: 2,
            max_tools_per_turn: 15,
            tool_call_records: &mut tool_call_records,
            tool_event_hooks: &tool_event_hooks,
            term: &mut term,
            mailbox: None,
            permission_context: None,
            progress_emitter: None,
            pre_resolved_results: &[],
            server_tool_executor: None,
            turn_start: None,
            llm_round: 0,
        })
        .await;

        assert_eq!(tool_results.len(), 3);
        assert_eq!(tool_call_records.len(), 3);
        assert!(
            tool_call_records.iter().all(|r| r.name.is_empty()),
            "expected only malformed empty-name calls to be recorded before abort"
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

        run_agentic_headless_tool_round(HeadlessToolRoundCtx {
            turn_index: 0,
            quiet: true,
            api: &astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            token: "",
            current_session_id: None,
            tool_calls: &tool_calls,
            edge_tool_round: &edge_tool_round,
            reasoning_content: "",
            edge_callback_outputs: &edge_callback_outputs,
            messages: &mut messages,
            tool_results: &mut tool_results,
            valid_tool_names: &valid_tool_names,
            restricted_tools: &mut restricted_tools,
            turn_guard: &mut turn_guard,
            step_recorder: &mut step_recorder,
            idempotency_cache: &mut idempotency_cache,
            semantic_dedup: &mut semantic_dedup,
            call_counts: &mut std::collections::HashMap::new(),
            max_identical_calls: 2,
            max_tools_per_turn: 15,
            tool_call_records: &mut tool_call_records,
            tool_event_hooks: &tool_event_hooks,
            term: &mut term,
            mailbox: Some(&mut child_mb),
            permission_context: Some(&child_permission_ctx),
            progress_emitter: None,
            pre_resolved_results: &[],
            server_tool_executor: None,
            turn_start: None,
            llm_round: 0,
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

        run_agentic_headless_tool_round(HeadlessToolRoundCtx {
            turn_index: 0,
            quiet: true,
            api: &astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            token: "",
            current_session_id: None,
            tool_calls: &tool_calls,
            edge_tool_round: &edge_tool_round,
            reasoning_content: "",
            edge_callback_outputs: &edge_callback_outputs,
            messages: &mut messages,
            tool_results: &mut tool_results,
            valid_tool_names: &valid_tool_names,
            restricted_tools: &mut restricted_tools,
            turn_guard: &mut turn_guard,
            step_recorder: &mut step_recorder,
            idempotency_cache: &mut idempotency_cache,
            semantic_dedup: &mut semantic_dedup,
            call_counts: &mut std::collections::HashMap::new(),
            max_identical_calls: 2,
            max_tools_per_turn: 15,
            tool_call_records: &mut tool_call_records,
            tool_event_hooks: &tool_event_hooks,
            term: &mut term,
            mailbox: Some(&mut child_mb),
            permission_context: Some(&child_permission_ctx),
            progress_emitter: None,
            pre_resolved_results: &[],
            server_tool_executor: None,
            turn_start: None,
            llm_round: 0,
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
    }

    /// Empty tool names (model bug) must be rejected before dedup counting
    /// so they don't inflate call_counts and flood the context with 50+ stubs.
    #[tokio::test]
    async fn empty_tool_name_rejected_before_dedup() {
        // 5 tool calls with empty name — the round should stop after the
        // malformed-call abort threshold, and none should hit the dedup path.
        let tool_calls: Vec<Value> = (0..5)
            .map(|i| {
                json!({
                    "id": format!("call-{i}"),
                    "name": "",
                    "arguments": "{}"
                })
            })
            .collect();

        let mut messages = Vec::new();
        let mut tool_results = Vec::new();
        let valid_tool_names = HashSet::from(["bash".to_string()]);
        let mut restricted_tools = HashSet::new();
        let mut turn_guard = TurnGuard::new();
        let mut step_recorder = StepRecorder::new("test-session", "empty-name");
        let mut idempotency_cache = InMemoryIdempotencyCache::new();
        let mut semantic_dedup = SemanticDedup::new(0.95);
        let mut tool_call_records = Vec::new();
        let tool_event_hooks = crate::skills::hooks::ToolEventHookRegistry::default();
        let mut term = NoopHeadlessTerminal;
        let edge_callback_outputs = HashMap::new();
        let edge_tool_round: Vec<EdgeToolExecResult> = Vec::new();

        run_agentic_headless_tool_round(HeadlessToolRoundCtx {
            turn_index: 0,
            quiet: true,
            api: &astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            token: "",
            current_session_id: None,
            tool_calls: &tool_calls,
            edge_tool_round: &edge_tool_round,
            reasoning_content: "",
            edge_callback_outputs: &edge_callback_outputs,
            messages: &mut messages,
            tool_results: &mut tool_results,
            valid_tool_names: &valid_tool_names,
            restricted_tools: &mut restricted_tools,
            turn_guard: &mut turn_guard,
            step_recorder: &mut step_recorder,
            idempotency_cache: &mut idempotency_cache,
            semantic_dedup: &mut semantic_dedup,
            call_counts: &mut HashMap::new(),
            max_identical_calls: 2,
            max_tools_per_turn: 15,
            tool_call_records: &mut tool_call_records,
            tool_event_hooks: &tool_event_hooks,
            term: &mut term,
            mailbox: None,
            permission_context: None,
            progress_emitter: None,
            pre_resolved_results: &[],
            server_tool_executor: None,
            turn_start: None,
            llm_round: 0,
        })
        .await;

        // Only the malformed empty-name calls up to the abort threshold should
        // be recorded, and each should still produce an immediate tool result.
        assert_eq!(tool_call_records.len(), 3);
        assert_eq!(tool_results.len(), 3);
        // Every record should be unknown_tool, not duplicate_within_turn.
        for rec in &tool_call_records {
            assert!(
                rec.error
                    .as_deref()
                    .is_some_and(|e| e.starts_with("unknown_tool")),
                "expected unknown_tool error, got: {:?}",
                rec.error
            );
        }
    }
}
